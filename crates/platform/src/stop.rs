//! Asking a daemon to stop on Windows, which has no terminate signal to send it.
//!
//! The daemon waits on a named event, and `service stop` sets it. Ending the process instead,
//! which is all Task Scheduler offers, would skip releasing held notes and saying goodbye to
//! peers, and those notes would sound on other machines until someone noticed.
//!
//! The event is named after the daemon's pipe, whose name is random and readable only by the
//! user, so nobody else can stop the daemon or listen in place of it, and its default access
//! gives it to the user who made it. It lives in the global namespace, because the daemon runs in
//! the desktop session and `service stop` may come from another, such as an ssh login; in the
//! session namespace the request was never seen (research R-087).

use crate::error::PlatformError;
use std::sync::Arc;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, OpenProcess, PROCESS_TERMINATE,
    SetEvent, TerminateProcess, WaitForSingleObject,
};

/// Returns the event name that stops the daemon serving on `pipe`.
///
/// Only the random part of the pipe's name is reused, prefixed for the global namespace.
pub fn event_name(pipe: &str) -> String {
    let suffix = pipe.rsplit('\\').next().unwrap_or(pipe);
    format!("Global\\{suffix}-stop")
}

/// A kernel handle this process owns: an event, a process, or a snapshot.
struct Event(HANDLE);

// SAFETY: a Windows kernel handle may be used and closed from any thread.
unsafe impl Send for Event {}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this process and is closed exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Creates the stop event for `pipe` and returns what is notified once it is set.
///
/// A thread waits on the event for the life of the process, which is the life of the daemon.
pub fn listen(pipe: &str) -> Result<Arc<tokio::sync::Notify>, PlatformError> {
    let name = crate::dll::wide(&event_name(pipe));
    // SAFETY: `name` is NUL-terminated and outlives the call; default security gives the event to
    // this user, and an auto-reset event starts unset.
    let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(PlatformError::Os {
            operation: "create the stop event",
            detail: std::io::Error::last_os_error().to_string(),
        });
    }
    let event = Event(handle);

    let stopped = Arc::new(tokio::sync::Notify::new());
    let notify = Arc::clone(&stopped);
    std::thread::Builder::new()
        .name("harbor-stop".to_owned())
        .spawn(move || {
            // Moved whole, so the handle is closed only when this thread ends.
            let event = event;
            // SAFETY: the handle stays open for as long as this thread owns `event`.
            let waited = unsafe { WaitForSingleObject(event.0, INFINITE) };
            if waited == 0 {
                // Stored as a permit, so a stop set before anyone waits is not lost.
                notify.notify_one();
            }
        })
        .map_err(|error| PlatformError::Os {
            operation: "start the stop watcher",
            detail: error.to_string(),
        })?;
    Ok(stopped)
}

/// Asks the daemon serving on `pipe` to stop, returning whether one was there to ask.
pub fn request(pipe: &str) -> Result<bool, PlatformError> {
    let name = crate::dll::wide(&event_name(pipe));
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if handle.is_null() {
        return Ok(false);
    }
    let event = Event(handle);
    // SAFETY: the handle was opened with the right to set it and is still open.
    if unsafe { SetEvent(event.0) } == 0 {
        return Err(PlatformError::Os {
            operation: "set the stop event",
            detail: std::io::Error::last_os_error().to_string(),
        });
    }
    Ok(true)
}

/// `ERROR_PIPE_BUSY`: every instance of the pipe is taken until the server makes another.
const ERROR_PIPE_BUSY: i32 = 231;

/// Ends the daemon serving on `pipe` at once, and the supervisor that would start it again,
/// returning whether there was a daemon to end.
///
/// The last resort for a daemon that did not stop when asked: nothing is silenced. Ending the
/// logon task is not enough on its own, because it ends only the console host the task started,
/// and left the supervisor and the daemon running.
pub fn force(pipe: &str) -> Result<bool, PlatformError> {
    // Ask the pipe which process serves it. Every instance can be taken for a moment while the
    // daemon makes the next, which is waited out rather than reported.
    let mut attempts = 0;
    let client = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe)
        {
            Ok(client) => break client,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 40 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                return Err(PlatformError::Os {
                    operation: "reach the daemon's pipe",
                    detail: error.to_string(),
                });
            }
        }
    };
    let mut daemon: u32 = 0;
    // SAFETY: the handle is an open pipe client for the duration of the call, and `daemon` is
    // writable.
    let found = unsafe {
        use std::os::windows::io::AsRawHandle;
        GetNamedPipeServerProcessId(client.as_raw_handle(), &mut daemon)
    };
    drop(client);
    if found == 0 {
        return Err(PlatformError::Os {
            operation: "find the daemon's process",
            detail: std::io::Error::last_os_error().to_string(),
        });
    }

    // The supervisor goes first, so it cannot start another daemon when this one ends. It is the
    // daemon's parent, and only a parent running the same program is taken for it.
    let processes = processes()?;
    let entry = |pid: u32| processes.iter().find(|(id, _, _)| *id == pid);
    if let Some((_, parent, program)) = entry(daemon)
        && let Some((supervisor, _, parent_program)) = entry(*parent)
        && parent_program.eq_ignore_ascii_case(program)
    {
        terminate(*supervisor)?;
    }
    terminate(daemon)?;
    Ok(true)
}

/// Lists every process as its identifier, its parent's, and its program's file name.
fn processes() -> Result<Vec<(u32, u32, String)>, PlatformError> {
    // SAFETY: takes a snapshot of the system's processes; the handle is closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(PlatformError::Os {
            operation: "list processes",
            detail: std::io::Error::last_os_error().to_string(),
        });
    }
    let snapshot = Event(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..PROCESSENTRY32W::default()
    };
    let mut listed = Vec::new();
    // SAFETY: the snapshot is open and `entry` is a writable PROCESSENTRY32W whose size field is
    // set, as both functions require.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while more {
        listed.push((
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            crate::dll::narrow(&entry.szExeFile),
        ));
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    Ok(listed)
}

/// Ends one process.
fn terminate(pid: u32) -> Result<(), PlatformError> {
    // SAFETY: opens the process for termination only; the handle is closed when `process` drops.
    let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if process.is_null() {
        return Err(PlatformError::Os {
            operation: "open the daemon's process",
            detail: std::io::Error::last_os_error().to_string(),
        });
    }
    let process = Event(process);
    // SAFETY: the handle is open with the right to terminate.
    if unsafe { TerminateProcess(process.0, 1) } == 0 {
        return Err(PlatformError::Os {
            operation: "end the daemon's process",
            detail: std::io::Error::last_os_error().to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Locks the stop event's name: the pipe's random part in the `Global\` kernel object
    /// namespace.
    ///
    /// The daemon runs in the desktop session and `service stop` may come from another, such as
    /// an ssh login; in the session namespace the request was never seen (research R-087).
    #[test]
    fn the_event_is_named_after_the_pipe_in_the_global_namespace() {
        assert_eq!(
            event_name(r"\\.\pipe\midi-harbor-0123abcd"),
            r"Global\midi-harbor-0123abcd-stop",
            "the stop event must be reachable from every session"
        );
    }

    /// Locks both answers of `request`: false when no daemon listens on the pipe, and true, with
    /// the listener told to stop, when one does.
    ///
    /// `service stop` falls back to ending the process only when asking found nobody, so a
    /// request that reported a listener it never reached would leave the daemon running.
    #[tokio::test]
    async fn a_stop_request_reaches_a_listening_daemon_and_reports_when_none_listens() {
        let pipe = format!(r"\\.\pipe\midi-harbor-{}", std::process::id());
        assert!(
            !request(&pipe).expect("asking with nobody listening must not fail"),
            "nobody listens before the event is created"
        );
        let stopped = listen(&pipe).expect("the stop event must be created");
        assert!(
            request(&pipe).expect("asking a listening daemon must not fail"),
            "a listening daemon's event must be found"
        );
        tokio::time::timeout(Duration::from_secs(5), stopped.notified())
            .await
            .expect("the listener must be told to stop");
    }

    /// Names the environment variable that gives a copy of this test binary its part.
    const ROLE: &str = "MIDI_HARBOR_STOP_TEST_ROLE";

    /// Runs this test binary again as `role`, running only the helper below.
    fn spawn(role: &str, pipe: &str) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "stop::tests::helper",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env(ROLE, role)
            .env("MIDI_HARBOR_STOP_TEST_PIPE", pipe)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Plays a part when run as a copy, and does nothing in an ordinary run.
    ///
    /// Not a test of its own: `forcing_ends_the_daemon_and_the_supervisor_above_it` runs it.
    ///
    /// As the supervisor it runs a daemon and waits for it; as the daemon it serves a pipe until
    /// it is ended.
    #[test]
    fn helper() {
        let (Ok(role), Ok(pipe)) = (
            std::env::var(ROLE),
            std::env::var("MIDI_HARBOR_STOP_TEST_PIPE"),
        ) else {
            return;
        };
        if role == "supervisor" {
            let _ = spawn("daemon", &pipe).wait();
            std::thread::sleep(Duration::from_secs(60));
        } else {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                // Always one instance waiting, as the daemon keeps one.
                let mut served = Vec::new();
                loop {
                    let server = tokio::net::windows::named_pipe::ServerOptions::new()
                        .create(&pipe)
                        .unwrap();
                    let _ = server.connect().await;
                    served.push(server);
                }
            });
        }
    }

    /// Locks that `force` ends the daemon serving a pipe and the supervisor that started it,
    /// found as the daemon's parent running the same program.
    ///
    /// Ending the logon task ends only the console host it started, and left the supervisor and
    /// the daemon running; the supervisor, left alone, would start another daemon.
    #[test]
    fn forcing_ends_the_daemon_and_the_supervisor_above_it() {
        let pipe = format!(r"\\.\pipe\midi-harbor-force-{}", std::process::id());
        let mut supervisor = spawn("supervisor", &pipe);
        let started = std::time::Instant::now();
        while std::fs::metadata(&pipe).is_err() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }

        let forced = force(&pipe);
        let ended = std::time::Instant::now();
        let mut exited = None;
        while exited.is_none() && ended.elapsed() < Duration::from_secs(5) {
            exited = supervisor.try_wait().unwrap();
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = supervisor.kill();
        let _ = supervisor.wait();

        assert!(
            forced.expect("forcing must not fail"),
            "the daemon serving the pipe must be found"
        );
        assert!(
            exited.is_some(),
            "the supervisor must be ended with the daemon"
        );
        assert!(
            !force(&pipe).expect("forcing again must not fail"),
            "the daemon must be ended"
        );
    }
}
