//! Switching a network session off and on.
//!
//! A session runs in its own supervisor, which reconciling never reached. Disabling one changed
//! the configuration and nothing else: it went on listening, advertising and carrying MIDI while
//! the configuration said it was off. Enabling one that had started disabled did nothing until
//! the daemon restarted.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::InvitationPolicy;
use midi_harbor_core::paths::Paths;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::MidiPlatform;
use std::net::{Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

/// Starts a daemon over a scratch directory of its own.
async fn daemon(label: &str) -> Arc<Daemon> {
    let root =
        common::scratch("midi-harbor-sessions").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    Daemon::start(
        common::quiet(root),
        Arc::new(FakeMidiPlatform::new()) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory")
}

/// Reports whether anything still holds the port, which is how a stopped session is told from
/// one that only says it stopped.
fn port_is_held(port: u16) -> bool {
    UdpSocket::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))).is_err()
        && UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port))).is_err()
}

/// Waits up to two seconds for a condition, reporting whether it held.
async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..40 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Proves that disabling a session stops its supervisor and releases its UDP port, rather than
/// only marking it off in the configuration while it goes on listening.
#[tokio::test]
async fn a_disabled_session_stops_listening() {
    let daemon = daemon("disable").await;
    let session = daemon
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the session is created");
    let port = daemon
        .session_status(session.id)
        .await
        .expect("the session is running")
        .control_port;
    assert!(
        port_is_held(port),
        "the session is not listening to begin with"
    );

    daemon
        .set_enabled(session.id, false)
        .await
        .expect("the session is disabled");

    assert!(
        daemon.session_status(session.id).await.is_none(),
        "the supervisor still runs"
    );
    assert!(
        eventually(|| !port_is_held(port)).await,
        "a disabled session still holds its port"
    );
}

/// Proves that a session switched off and on again starts listening again, on the UDP port the
/// system chose when it was made, and keeps that port in the configuration so a restarted daemon
/// comes back on it as well.
///
/// A peer that connected by address retries that address. A session that came back on a
/// different port left it retrying a port with nothing behind it, for good.
#[tokio::test]
async fn a_session_keeps_the_port_the_system_chose() {
    let daemon = daemon("pinned").await;
    let session = daemon
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the session is created");
    let chosen = daemon
        .session_status(session.id)
        .await
        .expect("the session is running")
        .control_port;

    daemon
        .set_enabled(session.id, false)
        .await
        .expect("the session is disabled");
    let enabled = daemon
        .set_enabled(session.id, true)
        .await
        .expect("the session is enabled again");

    let status = daemon
        .session_status(session.id)
        .await
        .expect("enabling a session started nothing");
    assert_eq!(
        status.control_port, chosen,
        "the session came back on a different UDP port"
    );
    assert!(
        port_is_held(status.control_port),
        "the session enabled again says it runs but does not listen"
    );
    let midi_harbor_core::endpoint::EndpointKind::NetworkSession(config) = enabled.kind else {
        panic!("the endpoint enabled is not a session");
    };
    assert_eq!(
        config.control_port, chosen,
        "the chosen UDP port is not kept in the configuration"
    );
}

/// Proves that a session losing its peer and getting it back records both in the history, and
/// that a peer switching its session off is recorded as a disconnection.
///
/// Regression: sessions logged their connecting and losing connection but never recorded either,
/// so the history, which is what a user reads, showed a session that had dropped as never having
/// done anything at all. Connecting twice (once at the start, once after the far side returns)
/// makes two connection records.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_losing_its_peer_and_getting_it_back_is_in_the_history() {
    let far = daemon("history-far").await;
    let far_session = far
        .create_network_session("Stage In", 0, InvitationPolicy::AcceptAll)
        .await
        .expect("the far session is created");
    let port = far
        .session_status(far_session.id)
        .await
        .expect("the far session is running")
        .control_port;
    let near = daemon("history-near").await;
    let near_session = near
        .create_network_session("Stage Out", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near session is created");
    near.connect_peer(near_session.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("the near session invites the far one");

    let recorded = |daemon: Arc<Daemon>, text: &'static str, times: usize| async move {
        for _ in 0..200 {
            let found = daemon
                .events(None, 200)
                .await
                .iter()
                .filter(|event| {
                    event.endpoint == Some(near_session.id) && event.detail.contains(text)
                })
                .count();
            if found >= times {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    };

    assert!(
        recorded(Arc::clone(&near), "Stage Out connected to", 1).await,
        "connecting was not recorded"
    );
    // Switching the far session off ends the session from its side, which is what the history
    // says: a peer that said goodbye is not one that stopped answering.
    far.set_enabled(far_session.id, false)
        .await
        .expect("the far session is switched off");
    assert!(
        recorded(Arc::clone(&near), "disconnected from Stage Out", 1).await,
        "losing the connection was not recorded"
    );
    far.set_enabled(far_session.id, true)
        .await
        .expect("the far session is switched on again");
    assert!(
        recorded(Arc::clone(&near), "Stage Out connected to", 2).await,
        "reconnecting was not recorded"
    );
}

/// Proves that a daemon on its way out ends its sessions at the far end, so a peer does not keep
/// a dead session listed.
///
/// Regression: stopping the daemon silenced notes but left every session open at the far end, so
/// Apple's Network MIDI kept the old session listed and sent its MIDI there (R-068). The far
/// daemon gets a runtime of its own, torn down afterwards as a process exit tears one down,
/// because a session left running on a live runtime says goodbye by itself once dropped.
#[test]
fn a_daemon_on_its_way_out_tells_its_peers() {
    let far_runtime = tokio::runtime::Runtime::new().expect("the far machine's runtime starts");
    let near_runtime = tokio::runtime::Runtime::new().expect("the near machine's runtime starts");

    let (far, port) = far_runtime.block_on(async {
        let far = daemon("exit-far").await;
        let session = far
            .create_network_session("Stage In", 0, InvitationPolicy::AcceptAll)
            .await
            .expect("the far session is created");
        let port = far
            .session_status(session.id)
            .await
            .expect("the far session is running")
            .control_port;
        (far, port)
    });
    let (near, near_session) = near_runtime.block_on(async {
        let near = daemon("exit-near").await;
        let session = near
            .create_network_session("Stage Out", 0, InvitationPolicy::Prompt)
            .await
            .expect("the near session is created");
        near.connect_peer(session.id, SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .expect("the near session invites the far one");
        (near, session.id)
    });

    let recorded = |text: &'static str| {
        near_runtime.block_on(async {
            for _ in 0..100 {
                if near.events(None, 200).await.iter().any(|event| {
                    event.endpoint == Some(near_session) && event.detail.contains(text)
                }) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            false
        })
    };
    assert!(recorded("Stage Out connected to"), "never connected");

    far_runtime.block_on(far.end_sessions());
    far_runtime.shutdown_background();
    assert!(
        recorded("disconnected from Stage Out"),
        "the peer was not told"
    );
}

/// Proves that what a session carries is counted on both sides and can be watched while it
/// runs.
///
/// Regression: a session had no runtime entry, so nothing it carried was counted and monitoring
/// it said it was not running while it carried MIDI. One note is fed, so each side counts at
/// least one message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_a_session_carries_is_counted_and_can_be_watched() {
    use midi_harbor_core::midi::{Channel, MidiMessage};

    let far_platform = Arc::new(FakeMidiPlatform::new());
    let far = Daemon::start(
        common::quiet(
            common::scratch("midi-harbor-sessions")
                .join(format!("counted-far-{}", uuid::Uuid::new_v4())),
        ),
        Arc::clone(&far_platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the far daemon starts over a scratch directory");
    far.create_virtual_port("Synth", 1, 1)
        .await
        .expect("the far port Synth is created");
    let far_session = far
        .create_network_session("Stage In", 0, InvitationPolicy::AcceptAll)
        .await
        .expect("the far session is created");
    far.create_route("Stage In", "Synth")
        .await
        .expect("the route from Stage In to Synth is created");
    let port = far
        .session_status(far_session.id)
        .await
        .expect("the far session is running")
        .control_port;

    let near_platform = Arc::new(FakeMidiPlatform::new());
    let near = Daemon::start(
        common::quiet(
            common::scratch("midi-harbor-sessions")
                .join(format!("counted-near-{}", uuid::Uuid::new_v4())),
        ),
        Arc::clone(&near_platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the near daemon starts over a scratch directory");
    near.create_virtual_port("Keys", 1, 1)
        .await
        .expect("the near port Keys is created");
    let near_session = near
        .create_network_session("Stage Out", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near session is created");
    near.create_route("Keys", "Stage Out")
        .await
        .expect("the route from Keys to Stage Out is created");
    near.connect_peer(near_session.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("the near session invites the far one");
    assert!(
        connected_within(&near, near_session.id).await,
        "a far session accepting everyone lets the near one in"
    );

    let mut watching = far
        .watch_endpoint(far_session.id)
        .await
        .expect("a running session can be watched");
    let note = MidiMessage::NoteOn {
        channel: Channel::new(0).expect("channel 0 is valid"),
        note: 60,
        velocity: 100,
    };
    assert!(
        near_platform.feed(
            near_platform
                .port_handle("Keys")
                .expect("Keys has a platform port"),
            &[note]
        ),
        "the fake platform takes the note fed into Keys"
    );

    let seen = tokio::time::timeout(Duration::from_secs(2), watching.recv())
        .await
        .expect("the note should have been seen")
        .expect("the feed stays open");
    assert!(
        matches!(seen.seen, midi_harbor_daemon::state::Seen::Message(m) if m == note),
        "the far session's watcher saw something other than the note sent: {:?}",
        seen.seen
    );
    assert!(
        !seen.outbound,
        "a note arriving from the network is seen as inbound"
    );

    let received = far
        .counters(far_session.id)
        .await
        .expect("the far session has counters")
        .messages_received;
    let sent = near
        .counters(near_session.id)
        .await
        .expect("the near session has counters")
        .messages_sent;
    assert!(received >= 1, "the far session counted {received} received");
    assert!(sent >= 1, "the near session counted {sent} sent");
}

/// Proves that announcing sessions can be switched off and on in the configuration file and
/// applies on reload without restarting any session.
///
/// A network where announcing is unwelcome, and every test run, needs sessions that work without
/// being seen.
#[tokio::test]
async fn announcing_sessions_can_be_switched_off_and_on() {
    let root =
        common::scratch("midi-harbor-sessions").join(format!("quiet-{}", uuid::Uuid::new_v4()));
    let paths = Paths::rooted_at(&root);
    std::fs::create_dir_all(paths.config_dir()).expect("the configuration directory is created");
    std::fs::write(
        paths.config_file(),
        "preferences:\n  advertise_sessions: false\nendpoints:\n  - name: Quiet Stage\n    kind: network_session\n",
    )
    .expect("the configuration with announcing off is written");
    let daemon = Daemon::start(
        paths.clone(),
        Arc::new(FakeMidiPlatform::new()) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over the written configuration");
    let id = daemon
        .resolve("Quiet Stage")
        .await
        .expect("the configured session exists");
    let port = daemon
        .session_status(id)
        .await
        .expect("the session is running")
        .control_port;
    assert!(
        port_is_held(port),
        "a session that is not announced still listens"
    );
    assert!(
        !daemon
            .announced_sessions()
            .contains(&"Quiet Stage".to_owned()),
        "announced with announcing off"
    );

    let text =
        std::fs::read_to_string(paths.config_file()).expect("the configuration file is readable");
    std::fs::write(
        paths.config_file(),
        text.replace("advertise_sessions: false", "advertise_sessions: true"),
    )
    .expect("the configuration with announcing on is written");
    daemon
        .reload_configuration()
        .await
        .expect("the configuration reloads");
    assert!(
        daemon
            .announced_sessions()
            .contains(&"Quiet Stage".to_owned()),
        "switching announcing on in the file does not announce the session"
    );
    assert_eq!(
        daemon
            .session_status(id)
            .await
            .expect("the session is running")
            .control_port,
        port,
        "the session restarted to be announced"
    );

    let text =
        std::fs::read_to_string(paths.config_file()).expect("the configuration file is readable");
    std::fs::write(
        paths.config_file(),
        text.replace("advertise_sessions: true", "advertise_sessions: false"),
    )
    .expect("the configuration with announcing off is written again");
    daemon
        .reload_configuration()
        .await
        .expect("the configuration reloads");
    assert!(
        !daemon
            .announced_sessions()
            .contains(&"Quiet Stage".to_owned()),
        "switching announcing off in the file leaves the session announced"
    );
}

/// Waits up to five seconds for a session to be connected, reporting whether it was.
async fn connected_within(daemon: &Arc<Daemon>, id: midi_harbor_core::ids::EndpointId) -> bool {
    for _ in 0..100 {
        let phase = daemon
            .session_status(id)
            .await
            .map(|status| status.state.phase());
        if phase == Some(midi_harbor_core::state::ConnectionPhase::Connected) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Proves that a session comes back from a restart connected to its peer when it was left
/// connected, and stays disconnected when the user disconnected it (FR-015).
///
/// Regression: the peer was held only in the running supervisor, so a restart or a reboot brought
/// the session back listening and nothing connected it again. The disconnected row only means
/// something beside the connected one, which shows a restart can reconnect at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_comes_back_connected_after_a_restart_only_if_it_was_left_connected() {
    struct Case {
        name: &'static str,
        disconnect_first: bool,
        want_connected: bool,
    }
    let cases = [
        Case {
            name: "left connected",
            disconnect_first: false,
            want_connected: true,
        },
        Case {
            name: "disconnected by the user",
            disconnect_first: true,
            want_connected: false,
        },
    ];

    for case in cases {
        let far = daemon("restart-far").await;
        let far_session = far
            .create_network_session("Stage In", 0, InvitationPolicy::AcceptAll)
            .await
            .expect("the far session is created");
        let port = far
            .session_status(far_session.id)
            .await
            .expect("the far session is running")
            .control_port;

        let root = common::scratch("midi-harbor-sessions")
            .join(format!("restart-near-{}", uuid::Uuid::new_v4()));
        // Quieted once: `quiet` rewrites the configuration file, which a restart must find as it
        // was left.
        let paths = common::quiet(root);
        let start = || {
            Daemon::start(
                paths.clone(),
                Arc::new(FakeMidiPlatform::new()) as Arc<dyn MidiPlatform>,
            )
        };
        let first = start()
            .await
            .expect("the near daemon starts over a scratch directory");
        let near = first
            .create_network_session("Stage Out", 0, InvitationPolicy::Prompt)
            .await
            .expect("the near session is created");
        first
            .connect_peer(near.id, SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .expect("the near session invites the far one");
        assert!(
            connected_within(&first, near.id).await,
            "{}: the near session never connected before the restart",
            case.name
        );
        if case.disconnect_first {
            first
                .disconnect_peer(near.id)
                .await
                .expect("the near session disconnects from the far one");
        }

        first.end_sessions().await;
        let second = start()
            .await
            .expect("the near daemon starts again over the same directory");
        assert_eq!(
            connected_within(&second, near.id).await,
            case.want_connected,
            "{}: after a restart the session should be connected only if it was left connected",
            case.name
        );
    }
}
