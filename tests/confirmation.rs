//! Destructive commands refuse to act without `--yes`.
//!
//! `port delete` once accepted `--yes` and ignored it, so a delete went ahead unasked while its
//! help described a confirmation. The refusal comes before the daemon is reached, which is what
//! lets this run with no daemon at all: refused is exit 7, and reaching for the daemon is exit 3.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

/// Runs the binary against a socket nothing listens on, returning its exit code.
fn run(arguments: &[&str]) -> i32 {
    let socket = std::env::temp_dir().join(format!("mh-no-daemon-{}.sock", std::process::id()));
    Command::new(env!("CARGO_BIN_EXE_midi-harbor"))
        .arg("--socket")
        .arg(&socket)
        .args(arguments)
        .output()
        .expect("the binary runs")
        .status
        .code()
        .expect("an exit code")
}

/// Locks that a destructive command without `--yes` exits 7 before reaching the daemon, and
/// with it goes on to the daemon, which is absent here and so exits 3.
///
/// The row with `--yes` is what proves the flag is read: `port delete` once accepted it and
/// ignored it, deleting unasked.
#[test]
fn a_destructive_command_needs_yes() {
    let cases: [(&[&str], i32); 3] = [
        (&["port", "delete", "Bus 1"], 7),
        (&["port", "delete", "Bus 1", "--yes"], 3),
        (
            &["config", "import", "/nonexistent.yaml", "--mode", "replace"],
            7,
        ),
    ];
    for (arguments, want) in cases {
        assert_eq!(
            run(arguments),
            want,
            "{arguments:?} must exit {want}: 7 refuses for want of --yes, 3 reached for the daemon"
        );
    }
}
