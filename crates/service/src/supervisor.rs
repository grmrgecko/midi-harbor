//! Keeping the daemon running where the service manager will not.
//!
//! launchd and systemd start the daemon again when it crashes. Task Scheduler does not: its
//! restart setting covers a task that fails to start, and a program that exits with an error is
//! left stopped (research R-083). On Windows the task therefore runs `midi-harbor daemon
//! --supervise`, which runs the daemon as a child and starts another when one fails. The App
//! Store app supervises its bundled daemon the same way, through [`Supervisor`], which it can stop.

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// How long to wait before the first restart, matching the systemd unit's `RestartSec`.
pub const RESTART_DELAY: Duration = Duration::from_secs(2);

/// The longest wait between restarts of a daemon that keeps failing.
///
/// A daemon that fails at once, every time, has a problem only the user can fix, and restarting
/// it every two seconds would fill its log while finding nothing new.
pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(60);

/// How long a daemon must run before its failure is treated as a new one rather than part of a
/// run of failures.
pub const STEADY: Duration = Duration::from_secs(60);

/// Returns how long to wait before starting the daemon again.
///
/// `previous` is the wait before the run that just failed, and `ran_for` how long that run
/// lasted. The wait doubles while failures come quickly, and falls back to the first once a run
/// has lasted.
pub fn next_delay(previous: Option<Duration>, ran_for: Duration) -> Duration {
    match previous {
        Some(previous) if ran_for < STEADY => previous.saturating_mul(2).min(MAX_RESTART_DELAY),
        _ => RESTART_DELAY,
    }
}

/// Runs `program` with `arguments` until it exits successfully, starting it again whenever it
/// does not.
///
/// A successful exit is how the daemon reports that it was asked to stop, so it ends supervision
/// rather than being overridden. Returns false only when the program cannot be started at all.
pub fn supervise(program: &Path, arguments: &[OsString]) -> bool {
    let mut delay = None;
    loop {
        let started = Instant::now();
        let status = std::process::Command::new(program).args(arguments).status();
        let ran_for = started.elapsed();
        match status {
            Ok(status) if status.success() => {
                info!("the daemon stopped; supervision ends");
                return true;
            }
            Ok(status) => {
                warn!(%status, seconds = ran_for.as_secs(), "the daemon exited with a failure");
            }
            Err(error) => {
                error!(error = %error, program = %program.display(), "could not start the daemon");
                return false;
            }
        }
        let wait = next_delay(delay, ran_for);
        info!(seconds = wait.as_secs(), "starting the daemon again");
        std::thread::sleep(wait);
        delay = Some(wait);
    }
}

/// A daemon supervised on a thread of its own, which can be told to stop.
///
/// The App Store app has no service manager to run its daemon (research R-095), so it supervises
/// the bundled helper itself with the same pacing as [`supervise`], and stops it when the user
/// quits Midi Harbor entirely.
#[cfg(unix)]
pub struct Supervisor {
    shared: std::sync::Arc<Shared>,
    finished: std::sync::mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// What the supervising thread and [`Supervisor::stop`] share.
#[cfg(unix)]
struct Shared {
    /// The running child's pid, and whether supervision has been told to stop. One lock, so a
    /// child is never started after a stop has been asked for.
    state: std::sync::Mutex<SupervisedState>,
    /// Wakes a supervisor waiting out a restart delay when it is told to stop.
    stopped: std::sync::Condvar,
}

/// The supervised child, as the lock in [`Shared`] guards it.
#[cfg(unix)]
#[derive(Default)]
struct SupervisedState {
    child: Option<u32>,
    stopping: bool,
}

#[cfg(unix)]
impl Supervisor {
    /// Starts supervising `program` run with `arguments`.
    pub fn start(program: &Path, arguments: &[OsString]) -> Self {
        let shared = std::sync::Arc::new(Shared {
            state: std::sync::Mutex::new(SupervisedState::default()),
            stopped: std::sync::Condvar::new(),
        });
        let (done, finished) = std::sync::mpsc::channel();
        let thread = {
            let shared = std::sync::Arc::clone(&shared);
            let program = program.to_path_buf();
            let arguments = arguments.to_vec();
            std::thread::spawn(move || {
                run_supervised(&shared, &program, &arguments);
                let _ = done.send(());
            })
        };
        Self {
            shared,
            finished,
            thread: Some(thread),
        }
    }

    /// Stops the daemon: asks it to stop with SIGTERM, which it answers by releasing held notes
    /// and ending its sessions, and kills it if it has not exited within `grace`.
    ///
    /// Returns once supervision has ended. No child is started after this is called.
    pub fn stop(mut self, grace: Duration) {
        let child = match self.shared.state.lock() {
            Ok(mut state) => {
                state.stopping = true;
                state.child
            }
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.stopping = true;
                state.child
            }
        };
        self.shared.stopped.notify_all();
        if let Some(pid) = child {
            info!(pid, "stopping the daemon");
            signal(pid, rustix::process::Signal::TERM);
        }

        // Wait for it, and make sure of it once the grace has passed.
        if self.finished.recv_timeout(grace).is_err() {
            if let Some(pid) = child {
                warn!(
                    pid,
                    seconds = grace.as_secs(),
                    "the daemon did not stop in time; killing it"
                );
                signal(pid, rustix::process::Signal::KILL);
            }
            let _ = self.finished.recv();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Sends `signal` to `pid`, ignoring a process that has already gone.
#[cfg(unix)]
fn signal(pid: u32, signal: rustix::process::Signal) {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    let _ = rustix::process::kill_process(pid, signal);
}

/// Runs the restart loop until the daemon exits successfully, cannot be started, or supervision
/// is told to stop.
#[cfg(unix)]
fn run_supervised(shared: &Shared, program: &Path, arguments: &[OsString]) {
    let mut delay = None;
    loop {
        // Start the daemon, unless a stop was asked for first.
        let started = Instant::now();
        let mut child = {
            let Ok(mut state) = shared.state.lock() else {
                return;
            };
            if state.stopping {
                return;
            }
            match std::process::Command::new(program).args(arguments).spawn() {
                Ok(child) => {
                    state.child = Some(child.id());
                    info!(pid = child.id(), "daemon started");
                    child
                }
                Err(error) => {
                    error!(error = %error, program = %program.display(), "could not start the daemon");
                    return;
                }
            }
        };

        // Wait for it to exit.
        let status = child.wait();
        let ran_for = started.elapsed();
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        state.child = None;
        if state.stopping {
            info!("the daemon stopped; supervision ends");
            return;
        }
        match status {
            Ok(status) if status.success() => {
                info!("the daemon stopped; supervision ends");
                return;
            }
            Ok(status) => {
                warn!(%status, seconds = ran_for.as_secs(), "the daemon exited with a failure");
            }
            Err(error) => {
                error!(error = %error, "could not wait for the daemon");
                return;
            }
        }

        // Wait before starting it again, waking early for a stop.
        let wait = next_delay(delay, ran_for);
        info!(seconds = wait.as_secs(), "starting the daemon again");
        let Ok((state, _)) = shared
            .stopped
            .wait_timeout_while(state, wait, |state| !state.stopping)
        else {
            return;
        };
        if state.stopping {
            return;
        }
        delay = Some(wait);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the restart pacing: the first restart after two seconds, matching the systemd
    /// unit's `RestartSec=2`, doubling while failures come within `STEADY` of starting, capped at
    /// sixty seconds, and back to two once a run has lasted `STEADY`.
    ///
    /// Task Scheduler restarts nothing (research R-083), so this is the only restart a Windows
    /// daemon gets. The doubling runs 2, 4, 8, 16, 32 and would reach 64, which the cap holds
    /// at 60.
    #[test]
    fn restarts_back_off_while_failures_are_quick_and_reset_once_a_run_lasts() {
        let quick = Duration::from_secs(1);
        let cases = [
            ("first failure", None, quick, RESTART_DELAY),
            (
                "second quick failure",
                Some(RESTART_DELAY),
                quick,
                Duration::from_secs(4),
            ),
            (
                "fifth quick failure",
                Some(Duration::from_secs(16)),
                quick,
                Duration::from_secs(32),
            ),
            (
                "doubling past the cap",
                Some(Duration::from_secs(32)),
                quick,
                MAX_RESTART_DELAY,
            ),
            (
                "already at the cap",
                Some(MAX_RESTART_DELAY),
                quick,
                MAX_RESTART_DELAY,
            ),
            (
                "a run one second short of steady",
                Some(MAX_RESTART_DELAY),
                STEADY - Duration::from_secs(1),
                MAX_RESTART_DELAY,
            ),
            (
                "a run that reached steady",
                Some(MAX_RESTART_DELAY),
                STEADY,
                RESTART_DELAY,
            ),
        ];
        for (name, previous, ran_for, want) in cases {
            assert_eq!(
                next_delay(previous, ran_for),
                want,
                "{name}: the wait before the next restart is wrong"
            );
        }
    }

    /// Locks that supervision ends on a successful exit and on a program that cannot start,
    /// rather than looping.
    ///
    /// `service stop` on Windows asks the daemon to exit and then waits for its pipe to go quiet
    /// (see `taskscheduler`), so a supervisor that restarted a daemon that exited cleanly would
    /// bring it back two seconds after every stop.
    #[test]
    fn supervision_ends_on_a_clean_exit_and_on_a_program_that_cannot_start() {
        #[cfg(unix)]
        let clean: (&str, Vec<OsString>) = ("/usr/bin/true", Vec::new());
        #[cfg(windows)]
        let clean: (&str, Vec<OsString>) = ("cmd.exe", vec!["/c".into(), "exit 0".into()]);
        let cases = [
            ("a clean exit", clean.0, clean.1, true),
            (
                "a missing program",
                "/nonexistent/midi-harbor",
                Vec::new(),
                false,
            ),
        ];
        for (name, program, arguments, want) in cases {
            assert_eq!(
                supervise(Path::new(program), &arguments),
                want,
                "{name}: supervision must end with this result rather than restart"
            );
        }
    }
}
