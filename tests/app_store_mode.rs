//! The App Store mode through the real binary, and the supervisor the App Store app runs its
//! daemon under (feature 014).
//!
//! The mode keys on `APP_SANDBOX_CONTAINER_ID`, which the App Sandbox sets before `main` and
//! nothing else does (research R-094), so setting it here, with `HOME` and `TMPDIR` pointed at a
//! scratch directory as the sandbox points them at the container, is the mode as the binary sees
//! it, without signing anything.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Returns an empty scratch directory for one test, kept short so a socket inside it stays well
/// within macOS's 103-byte limit.
fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mh-as-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("tmp")).unwrap();
    dir
}

/// Builds a command for the binary as the App Sandbox would start it: the variable set, and
/// `HOME` and `TMPDIR` inside a container, here the scratch directory.
fn sandboxed(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_midi-harbor"));
    command
        .env("APP_SANDBOX_CONTAINER_ID", "com.mrgeckosmedia.MidiHarbor")
        .env("HOME", dir.join("home"))
        // The sandbox's TMPDIR ends in a slash, as the container's did (R-094).
        .env("TMPDIR", format!("{}/", dir.join("tmp").display()));
    command
}

/// Runs a sandboxed command to completion.
fn run(dir: &Path, arguments: &[&str]) -> Output {
    sandboxed(dir)
        .args(arguments)
        .output()
        .expect("the binary runs")
}

/// Waits until something answers on `socket`, or `limit` passes.
fn answers_within(socket: &Path, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if UnixStream::connect(socket).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Proves that in the App Store build every `service` command is refused with exit 4, naming
/// "Start at login", while the direct build still reaches launchd (contracts/cli.md, FR-A03).
///
/// The sandbox refuses launchctl, and an agent plist written inside the container would never be
/// seen by launchd, so installing would appear to work and start nothing.
#[test]
fn the_app_store_build_refuses_service_commands() {
    let dir = scratch("service");
    for command in [&["service", "install"][..], &["service", "status"]] {
        let output = run(&dir, command);
        assert_eq!(
            output.status.code(),
            Some(4),
            "{command:?} must exit 4, unavailable, in the App Store build: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("\"Start at login\""),
            "{command:?} must point at Start at login instead: {output:?}"
        );
    }

    let direct = Command::new(env!("CARGO_BIN_EXE_midi-harbor"))
        .args(["service", "status"])
        .env("HOME", dir.join("home"))
        .env_remove("APP_SANDBOX_CONTAINER_ID")
        .output()
        .expect("the binary runs");
    assert_ne!(
        direct.status.code(),
        Some(4),
        "without the sandbox, service status must still ask launchd: {direct:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Proves that the sandboxed daemon listens on `$TMPDIR/daemon.sock`, where a sandboxed command
/// line finds it with no `--socket`, reports service installation unavailable because this build
/// does not include it, and stops on `service stop`, asked over its socket since no service
/// manager runs it (R-096, R-097, FR-039c).
///
/// With the unsandboxed layout the socket path in a container passes macOS's 103-byte limit for
/// account names over 18 characters; without the directory, over 30. The App Store app's own
/// Quit stops a daemon it found running the same way, so the command line can do what the window
/// does.
#[test]
fn the_sandboxed_daemon_serves_from_the_container_tmp() {
    let dir = scratch("daemon");
    let socket = dir.join("tmp").join("daemon.sock");
    let daemon = sandboxed(&dir)
        .arg("daemon")
        .spawn()
        .expect("the daemon starts");
    assert!(
        answers_within(&socket, Duration::from_secs(20)),
        "the sandboxed daemon must listen at {}",
        socket.display()
    );

    let output = run(&dir, &["capabilities", "--json"]);
    let stopped = run(&dir, &["service", "stop"]);
    let mut daemon = daemon;
    let exited = daemon.wait().expect("the daemon is waited for");
    assert!(
        stopped.status.success() && exited.success(),
        "service stop must stop the sandboxed daemon through its graceful shutdown: \
         {stopped:?}, the daemon exited {exited:?}"
    );
    assert!(
        output.status.success(),
        "a sandboxed command line must reach the daemon with no --socket: {output:?}"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("capabilities prints JSON");
    let service = report
        .as_array()
        .and_then(|all| {
            all.iter()
                .find(|capability| capability["name"] == "service installation")
        })
        .unwrap_or_else(|| panic!("the report must answer for the service manager: {report}"));
    assert_eq!(
        service["available"], false,
        "the App Store build cannot install a service: {service}"
    );
    assert_eq!(
        service["reason"], "this build does not include it",
        "the reason must be that this build does not include it: {service}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Returns the pids of the daemons serving `socket`, found by the scratch path in their
/// arguments, which no other process has.
fn daemons_on(socket: &Path) -> Vec<u32> {
    let listed = Command::new("pgrep")
        .args(["-f", &format!("{} daemon", socket.display())])
        .output()
        .expect("pgrep runs");
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// Waits until `pid` has exited, or `limit` passes.
fn gone_within(pid: u32, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success());
        if !alive {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Returns arguments that run the real binary as a daemon on `socket`, through env(1) so it has a
/// home of its own and never reads the user's configuration.
fn daemon_arguments(dir: &Path, socket: &Path) -> Vec<std::ffi::OsString> {
    vec![
        format!("HOME={}", dir.join("home").display()).into(),
        env!("CARGO_BIN_EXE_midi-harbor").into(),
        "--socket".into(),
        socket.as_os_str().to_owned(),
        "daemon".into(),
    ]
}

/// Proves that a supervisor told to stop ends its daemon through SIGTERM, within the daemon's own
/// grace rather than by the kill after it, and does not start it again (FR-A02, FR-A08, R-095).
///
/// The App Store app stops its daemon this way when the user quits. A supervisor that treated
/// the stopped daemon as failed would start it again two seconds later, and the user's quit
/// would leave MIDI running with no app to stop it.
#[test]
fn a_stopped_supervisor_leaves_no_daemon_running() {
    let dir = scratch("supervised");
    let socket = dir.join("tmp").join("daemon.sock");
    let supervisor = midi_harbor_service::supervisor::Supervisor::start(
        Path::new("/usr/bin/env"),
        &daemon_arguments(&dir, &socket),
    );
    assert!(
        answers_within(&socket, Duration::from_secs(20)),
        "the supervised daemon must serve its socket"
    );
    let started = daemons_on(&socket);
    assert_eq!(started.len(), 1, "one daemon must serve: {started:?}");

    // A graceful stop takes about two seconds, the daemon's own grace for open requests; the
    // kill after five would also leave no daemon, but would skip releasing held notes.
    let stopping = Instant::now();
    supervisor.stop(Duration::from_secs(5));
    assert!(
        stopping.elapsed() < Duration::from_secs(4),
        "the daemon must stop on SIGTERM, not be killed after the grace: took {:?}",
        stopping.elapsed()
    );
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        UnixStream::connect(&socket).is_err() && daemons_on(&socket).is_empty(),
        "no daemon may run after the supervisor was stopped, nor two seconds later"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Proves the App Store app's hold on its daemon: it starts one when none serves, uses one that
/// already serves rather than starting a second, gets a crashed one back, and stops one it found
/// running as surely as one it started (FR-A02, FR-A08, FR-A14).
///
/// A second daemon would open the same ports and sessions beside the first. A window relaunched
/// after a crash finds the daemon it left, and Quit must still stop it, since in the App Store
/// build nothing else will. The sandbox forbids signalling a daemon an earlier instance of the
/// app started, so that one is asked to stop over its socket (R-095).
#[cfg(feature = "gui")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_owns_its_daemon_through_crashes_of_either() {
    use midi_harbor_gui::app_store::DaemonOwner;

    let dir = scratch("owner");
    let socket = dir.join("tmp").join("daemon.sock");
    let arguments = daemon_arguments(&dir, &socket);
    let env = Path::new("/usr/bin/env");

    // Nothing serves, so the app starts its daemon.
    let first = DaemonOwner::start(env, &arguments, &socket)
        .await
        .expect("a daemon starts and serves");
    assert!(
        first.started(),
        "with nothing serving, the app must start a daemon"
    );
    let [started] = daemons_on(&socket)[..] else {
        panic!("one daemon must serve: {:?}", daemons_on(&socket));
    };

    // A daemon that crashes is started again. The killed one answers until it has gone.
    let _ = Command::new("kill")
        .args(["-9", &started.to_string()])
        .status();
    assert!(
        gone_within(started, Duration::from_secs(5)),
        "the killed daemon never exited"
    );
    assert!(
        answers_within(&socket, Duration::from_secs(10)),
        "a daemon that crashed must be started again after the supervisor's two seconds"
    );
    let [restarted] = daemons_on(&socket)[..] else {
        panic!(
            "one daemon must serve after the restart: {:?}",
            daemons_on(&socket)
        );
    };
    assert_ne!(
        restarted, started,
        "the daemon serving must be a new process"
    );

    // A second app, as after a window crash, uses the daemon serving and starts none.
    let second = DaemonOwner::start(env, &arguments, &socket)
        .await
        .expect("the running daemon is found");
    assert!(
        !second.started(),
        "a daemon already serving must be used, not a second started"
    );
    assert_eq!(
        daemons_on(&socket),
        vec![restarted],
        "attaching must leave the same daemon serving, and only it"
    );

    // Quitting that app stops the daemon it found, which ends the first app's supervision too.
    second.stop().await;
    assert!(
        gone_within(restarted, Duration::from_secs(5)),
        "the daemon the app found must stop when it quits"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        daemons_on(&socket).is_empty(),
        "a daemon asked to stop must not be started again by the first app's supervisor"
    );
    first.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}
