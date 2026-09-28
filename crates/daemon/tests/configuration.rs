//! A configuration file the daemon cannot use, met at startup.
//!
//! People edit the file by hand, so a daemon will one day start on a file it cannot parse, or one
//! written by a newer build. Neither may cost the user their setup (FR-051).

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use midi_harbor_core::config::{CURRENT_SCHEMA, ConfigError};
use midi_harbor_core::events::{EventKind, Severity};
use midi_harbor_core::paths::Paths;
use midi_harbor_daemon::{Daemon, DaemonError};
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::MidiPlatform;
use std::sync::Arc;

/// Returns paths in a directory of their own whose configuration file holds `text`.
fn written(label: &str, text: &str) -> Paths {
    let root = common::scratch("midi-harbor-configuration")
        .join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let paths = Paths::rooted_at(root);
    std::fs::create_dir_all(paths.config_dir()).expect("the configuration directory is made");
    std::fs::write(paths.config_file(), text).expect("the configuration file is written");
    paths
}

/// An unparseable file is moved aside intact under a timestamped name, the daemon starts from
/// defaults, and the move is reported as an error naming where the file went, so the user can
/// recover the setup by hand rather than finding it gone (FR-051).
#[tokio::test]
async fn an_unreadable_file_is_moved_aside_whole_and_reported() {
    // A tab indenting a mapping is a YAML error, the kind a hand edit makes.
    let unreadable = "endpoints:\n\t- name: Keys\n";
    let paths = written("unreadable", unreadable);

    let daemon = Daemon::start(
        paths.clone(),
        Arc::new(FakeMidiPlatform::new()) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts from defaults rather than refusing an unreadable file");

    let aside: Vec<_> = std::fs::read_dir(paths.config_dir())
        .expect("the configuration directory is listed")
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.starts_with("corrupt-"))
        })
        .collect();
    assert_eq!(
        aside.len(),
        1,
        "the unreadable file is kept exactly once, under a timestamped name"
    );
    assert_eq!(
        std::fs::read_to_string(&aside[0]).expect("the preserved file is read"),
        unreadable,
        "the preserved file holds the user's text byte for byte"
    );

    let events = daemon.events(None, 100).await;
    let reported = events.iter().find(|event| {
        event.kind == EventKind::ConfigurationChanged && event.severity == Severity::Error
    });
    let reported = reported.expect("the move is reported as an error event");
    assert!(
        reported.detail.contains(&aside[0].display().to_string()),
        "the event names where the file went, so the user can find it: {}",
        reported.detail
    );
}

/// A file from a newer build is refused, and left exactly as it was, rather than read with its
/// unknown fields dropped and then written back without them.
#[tokio::test]
async fn a_file_from_a_newer_build_is_refused_and_left_untouched() {
    let newer = format!("schema_version: {}\nendpoints: []\n", CURRENT_SCHEMA + 1);
    let paths = written("newer", &newer);

    let started = Daemon::start(
        paths.clone(),
        Arc::new(FakeMidiPlatform::new()) as Arc<dyn MidiPlatform>,
    )
    .await;

    assert!(
        matches!(
            started,
            Err(DaemonError::Config(ConfigError::SchemaTooNew { found })) if found == CURRENT_SCHEMA + 1
        ),
        "a newer schema refuses to start and names the version it found"
    );
    assert_eq!(
        std::fs::read_to_string(paths.config_file()).expect("the file is still where it was"),
        newer,
        "the newer file is neither moved nor rewritten"
    );
}
