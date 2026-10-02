//! The service commands against the platform's service manager, end to end (T044, FR-039h).
//!
//! The binary runs with `PATH` holding nothing but a stand-in for `launchctl` or `systemctl`, so
//! the real one cannot be reached and nothing is registered with the machine. The stand-in keeps
//! its state in files and starts the daemon itself, as the real one would, so `service start`
//! has a daemon to wait for. Starting a service that is running does nothing, as with the real
//! ones. Everything else a user would have is the real code: the definition
//! written under a scratch home, the status read back from it, and the configuration beside it.

#![cfg(any(target_os = "macos", target_os = "linux"))]
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A stand-in `launchctl`, answering the subcommands the launchd backend uses.
#[cfg(target_os = "macos")]
const TOOL: (&str, &str) = (
    "launchctl",
    r#"#!/bin/sh
echo "$*" >> "$FAKE_STATE/calls"
running() { [ -f "$FAKE_STATE/pid" ] && kill -0 "$(/bin/cat "$FAKE_STATE/pid")" 2>/dev/null; }
halt() {
  if running; then
    kill "$(/bin/cat "$FAKE_STATE/pid")"
    while running; do /bin/sleep 0.1; done
  fi
  /bin/rm -f "$FAKE_STATE/pid"
}
case "$1" in
  bootstrap) : > "$FAKE_STATE/loaded" ;;
  bootout) [ -f "$FAKE_STATE/loaded" ] || { echo "Boot-out failed: 3: No such process" >&2; exit 3; }
           halt; /bin/rm -f "$FAKE_STATE/loaded" ;;
  kickstart) [ -f "$FAKE_STATE/loaded" ] || exit 113
             running && exit 0
             "$FAKE_DAEMON" daemon > "$FAKE_STATE/daemon.out" 2>&1 &
             echo $! > "$FAKE_STATE/pid" ;;
  kill) halt ;;
  print) [ -f "$FAKE_STATE/loaded" ] || exit 113
         if running; then echo "state = running"; else echo "state = not running"; fi ;;
  *) exit 64 ;;
esac
"#,
);

/// A stand-in `systemctl`, answering the `--user` subcommands the systemd backend uses.
#[cfg(target_os = "linux")]
const TOOL: (&str, &str) = (
    "systemctl",
    r#"#!/bin/sh
echo "$*" >> "$FAKE_STATE/calls"
running() { [ -f "$FAKE_STATE/pid" ] && kill -0 "$(/bin/cat "$FAKE_STATE/pid")" 2>/dev/null; }
halt() {
  if running; then
    kill "$(/bin/cat "$FAKE_STATE/pid")"
    while running; do /bin/sleep 0.1; done
  fi
  /bin/rm -f "$FAKE_STATE/pid"
}
[ "$1" = "--user" ] || exit 64
shift
case "$1" in
  show-environment) echo "HOME=$HOME" ;;
  daemon-reload) ;;
  enable) : > "$FAKE_STATE/enabled" ;;
  disable) halt; /bin/rm -f "$FAKE_STATE/enabled" ;;
  start) running && exit 0
         "$FAKE_DAEMON" daemon > "$FAKE_STATE/daemon.out" 2>&1 &
         echo $! > "$FAKE_STATE/pid" ;;
  stop) halt ;;
  is-active) if running; then echo active; else echo inactive; exit 3; fi ;;
  *) exit 64 ;;
esac
"#,
);

/// Where the service definition is written, under a given home.
fn definition_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents")
    } else {
        home.join(".config/systemd/user")
    }
}

/// Where the configuration lives, under a given home.
fn config_file(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/midi-harbor/config.yaml")
    } else {
        home.join(".config/midi-harbor/config.yaml")
    }
}

/// A scratch machine: a home, a runtime directory, and a `PATH` holding only the stand-in.
///
/// Dropping it stops any daemon the stand-in started, so a failed assertion leaves nothing
/// running.
struct Machine {
    root: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mh-service-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "bin", "state", "run"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let tool = root.join("bin").join(TOOL.0);
        std::fs::write(&tool, TOOL.1).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Runs the binary, returning its exit code and standard output.
    fn run(&self, arguments: &[&str]) -> (i32, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_midi-harbor"))
            .args(arguments)
            .env_clear()
            // Only the stand-in is reachable, so the real service manager cannot be touched.
            .env("PATH", self.root.join("bin"))
            .env("HOME", self.home())
            .env("TMPDIR", self.root.join("run"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("FAKE_STATE", self.root.join("state"))
            .env("FAKE_DAEMON", env!("CARGO_BIN_EXE_midi-harbor"))
            .output()
            .expect("the binary runs");
        (
            output.status.code().expect("an exit code"),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    }

    /// Returns the process the stand-in is running as the daemon.
    fn daemon_pid(&self) -> String {
        std::fs::read_to_string(self.root.join("state/pid"))
            .expect("the stand-in recorded the daemon it started")
            .trim()
            .to_owned()
    }

    fn status(&self) -> serde_json::Value {
        let (code, out) = self.run(&["--json", "service", "status"]);
        assert_eq!(code, 0, "service status must succeed: {out}");
        serde_json::from_str(&out).expect("service status writes one JSON document")
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(self.root.join("state/pid")) {
            let _ = Command::new("/bin/kill").arg(pid.trim()).status();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Locks the service lifecycle a user drives: install registers the daemon stopped, a second
/// install updates the one registration, start waits until the daemon answers, installing with
/// `--start` while it runs replaces the running daemon, stop ends it, and uninstall removes the
/// registration and leaves the configuration byte for byte as it was.
///
/// Losing a user's ports and routes to an uninstall, or leaving two registrations to fight over
/// one socket, are the failures this guards. So is an update that registers the new program and
/// leaves the old daemon running: starting a service that is already running does nothing
/// (R-108).
#[test]
fn the_service_installs_starts_stops_and_uninstalls_in_place() {
    let machine = Machine::new();
    let config = config_file(&machine.home());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "preferences:\n  advertise_sessions: false\n").unwrap();

    // Install registers it, stopped.
    let (code, out) = machine.run(&["service", "install"]);
    assert_eq!(code, 0, "install must succeed: {out}");
    let status = machine.status();
    assert_eq!(
        status["installed"], true,
        "install must register the service: {status}"
    );
    assert_eq!(
        status["running"], false,
        "install alone must not start it: {status}"
    );
    assert_eq!(
        status["stale"], false,
        "a fresh registration must not be stale: {status}"
    );
    assert_eq!(
        status["registered_executable"].as_str().map(PathBuf::from),
        Some(PathBuf::from(env!("CARGO_BIN_EXE_midi-harbor"))),
        "the registration must run the executable that installed it"
    );

    // Installing again updates the one registration rather than adding a second.
    let (code, out) = machine.run(&["service", "install"]);
    assert_eq!(code, 0, "a second install must succeed: {out}");
    let definitions = std::fs::read_dir(definition_dir(&machine.home()))
        .unwrap()
        .count();
    assert_eq!(
        definitions, 1,
        "a second install must update the one definition, not add one"
    );

    // Start runs the daemon and waits for it to answer; stop ends it.
    let (code, out) = machine.run(&["service", "start"]);
    assert_eq!(code, 0, "start must wait until the daemon answers: {out}");
    assert_eq!(
        machine.status()["running"],
        true,
        "the daemon must be running after start"
    );

    // Installing with --start while it runs replaces the daemon with the program that asked.
    let old = machine.daemon_pid();
    let (code, out) = machine.run(&["service", "install", "--start"]);
    assert_eq!(code, 0, "install --start over a running daemon: {out}");
    let status = machine.status();
    assert_eq!(
        (status["running"].clone(), status["same_build"].clone()),
        (true.into(), true.into()),
        "the daemon must be running, and the build that installed it: {status}"
    );
    assert_ne!(
        machine.daemon_pid(),
        old,
        "the daemon running before the install was left running"
    );

    let (code, out) = machine.run(&["service", "stop"]);
    assert_eq!(code, 0, "stop must succeed: {out}");
    assert_eq!(
        machine.status()["running"],
        false,
        "the daemon must be stopped after stop"
    );

    // Uninstall removes the registration and leaves the configuration exactly as it was. The
    // daemon rewrote it while it ran, so the comparison is with what was there just before.
    let before = std::fs::read_to_string(&config).unwrap();
    let (code, out) = machine.run(&["service", "uninstall"]);
    assert_eq!(code, 0, "uninstall must succeed: {out}");
    assert_eq!(
        machine.status()["installed"],
        false,
        "uninstall must remove the registration"
    );
    assert_eq!(
        std::fs::read_dir(definition_dir(&machine.home()))
            .unwrap()
            .count(),
        0,
        "uninstall must leave no definition behind"
    );
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        before,
        "uninstall must leave the user's configuration untouched"
    );
}
