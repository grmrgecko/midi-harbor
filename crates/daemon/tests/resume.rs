//! Noticing that the machine came back.
//!
//! The backoff reaches thirty seconds, so a session that was retrying when the lid closed waits
//! up to that long after it opens. None of this is required for recovery (the backoff gets there
//! on its own, and the platform sources that report a wake are unreliable by nature), but the
//! difference is between resuming when the machine wakes and resuming half a minute later.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::events::EventKind;
use midi_harbor_core::paths::Paths;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::{FakeMidiPlatform, FakeSystemEvents};
use midi_harbor_platform::midi::MidiPlatform;
use midi_harbor_platform::sysevents::{SystemEvent, SystemEvents};
use std::sync::Arc;
use std::time::Duration;

/// Builds a daemon whose machine events a test drives.
async fn daemon(label: &str) -> (Arc<Daemon>, Arc<FakeSystemEvents>) {
    let root =
        common::scratch("midi-harbor-resume").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let system = Arc::new(FakeSystemEvents::new());

    let daemon = Daemon::start_with(
        Paths::rooted_at(root),
        platform as Arc<dyn MidiPlatform>,
        Arc::clone(&system) as Arc<dyn SystemEvents>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, system)
}

/// Reports whether the daemon records a resume within `within`, polling its event history.
async fn saw_resume(daemon: &Arc<Daemon>, within: Duration) -> bool {
    let started = std::time::Instant::now();
    loop {
        let recorded = daemon
            .events(None, 100)
            .await
            .iter()
            .any(|event| event.kind == EventKind::SystemResumed);
        if recorded || started.elapsed() >= within {
            return recorded;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Proves that a wake and an address change each count as a reason to reconnect, and that a
/// coming suspend or a quiet machine do not.
///
/// A wake and an address change are the two ways a connection dies without anything on the wire
/// saying so, so each records `SystemResumed` and nudges every session. `Suspending` arrives
/// before the machine goes away, when reconnecting is pointless. A quiet machine is the control:
/// the watcher must not invent a resume on its own. A resume is awaited for up to six seconds; its
/// absence is judged after one and a half, which is the watcher's one-second pass plus half a
/// pass of margin.
#[tokio::test]
async fn only_a_wake_or_an_address_change_is_a_reason_to_reconnect() {
    struct Case {
        name: &'static str,
        event: fn(&FakeSystemEvents),
        want: bool,
    }
    let cases = [
        Case {
            name: "wake",
            event: FakeSystemEvents::sleep_and_wake,
            want: true,
        },
        Case {
            name: "network",
            event: |system| system.emit(SystemEvent::NetworkChanged),
            want: true,
        },
        Case {
            name: "suspend",
            event: |system| system.emit(SystemEvent::Suspending),
            want: false,
        },
        Case {
            name: "quiet",
            event: |_| {},
            want: false,
        },
    ];

    for case in cases {
        let (daemon, system) = daemon(case.name).await;
        (case.event)(&system);

        let within = if case.want {
            Duration::from_secs(6)
        } else {
            Duration::from_millis(1_500)
        };
        assert_eq!(
            saw_resume(&daemon, within).await,
            case.want,
            "{}: a resume is recorded exactly when the event is a reason to reconnect",
            case.name
        );
    }
}
