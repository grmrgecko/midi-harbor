//! A daemon that loses the platform's MIDI service makes way for a new one.
//!
//! When Apple's `MIDIServer` dies, nothing in the process that was using it can reach it again,
//! not even a new client (R-079). The daemon's answer is to stop serving and be replaced, and the
//! replacement says why it started.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::Direction;
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_daemon::{Daemon, Stopped};
use midi_harbor_ipc::{HarborClient, pb, transport};
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::{DiscoveredDevice, MidiPlatform};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Starts a daemon serving on its own short socket path, returning what `run` resolves to.
async fn serving(
    label: &str,
) -> (
    Arc<Daemon>,
    Arc<FakeMidiPlatform>,
    PathBuf,
    tokio::task::JoinHandle<Result<Stopped, String>>,
) {
    let root = common::scratch("midi-harbor-server-lost")
        .join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let socket = common::scratch("mh-lost").join(format!("{}.sock", &unique[..8]));
    let parent = socket
        .parent()
        .expect("the socket path sits in a scratch directory");
    std::fs::create_dir_all(parent).expect("the socket's scratch directory is created");

    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        common::quiet(root).with_socket(Some(socket.clone())),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    let served = Arc::clone(&daemon);
    let running = tokio::spawn(async move {
        midi_harbor_daemon::run(served)
            .await
            .map_err(|error| error.to_string())
    });
    for _ in 0..100 {
        if transport::probe(&socket).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (daemon, platform, socket, running)
}

/// Proves that losing the MIDI server stops the daemon with the reason `Replace` and removes its
/// socket, even with a window watching its streams, while an ordinary change to the MIDI
/// environment leaves it serving (R-079).
///
/// The socket goes with it, or the replacement would find it and think a daemon is running. An
/// open window keeps streams open that never finish on their own; stopping waited for them, so
/// a daemon asked to stop with the window open kept running until it was killed. A device
/// arriving is refreshed like any change; the wait of a second and a half gives the watcher time
/// to act on it.
#[tokio::test(flavor = "multi_thread")]
async fn losing_the_midi_server_stops_the_daemon_to_be_replaced() {
    for (name, watching) in [("no client", false), ("a window watching", true)] {
        let (daemon, platform, socket, running) = serving("lost").await;
        assert!(
            transport::probe(&socket).await,
            "{name}: the daemon must be serving before the loss"
        );

        let mut streams = None;
        if watching {
            let mut client = HarborClient::new(
                transport::connect(&socket)
                    .await
                    .expect("the window connects to the daemon's socket"),
            );
            let state = client
                .watch_state(pb::WatchStateRequest {})
                .await
                .expect("the window opens a state stream");
            let events = client
                .watch_events(pb::WatchEventsRequest { after_id: None })
                .await
                .expect("the window opens an event stream");
            streams = Some((client, state, events));
        }

        platform.attach(DiscoveredDevice {
            fingerprint: DeviceFingerprint {
                unique_id: Some(9001),
                name: "Pad Controller Port A".to_owned(),
                ..DeviceFingerprint::default()
            },
            direction: Direction::Bidirectional,
            claimed_by: None,
            software: false,
        });
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            !running.is_finished(),
            "{name}: a device arriving stopped the daemon"
        );
        assert!(
            transport::probe(&socket).await,
            "{name}: the daemon must still serve after a device arrived"
        );

        platform.lose_server();
        let stopped = tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .expect("the daemon stops within ten seconds of the MIDI server being lost")
            .expect("the serving task completes")
            .expect("the daemon stops cleanly");
        assert_eq!(
            stopped,
            Stopped::Replace,
            "{name}: a daemon that lost the MIDI server must ask to be replaced"
        );
        assert!(
            daemon.midi_server_lost_at().await.is_some(),
            "{name}: a daemon that lost the MIDI server must say when, for its replacement's warning"
        );
        assert!(!socket.exists(), "{name}: the socket was left behind");
        drop(streams);
    }
}
