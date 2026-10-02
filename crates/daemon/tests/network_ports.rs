//! Changing a network port after it is made: the name other machines see, its UDP port and who
//! may join (FR-015, R-078).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::{EndpointKind, InvitationPolicy, NetworkSession};
use midi_harbor_core::failure::FailureReason;
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::state::ConnectionPhase;
use midi_harbor_daemon::discovery::DiscoveredPeer;
use midi_harbor_daemon::identity::PortIdentity;
use midi_harbor_daemon::{Daemon, DaemonError, NetworkPortChange};
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::MidiPlatform;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

/// Starts a daemon standing in for one machine, over a scratch directory of its own.
async fn machine(label: &str) -> Arc<Daemon> {
    let root = common::scratch("midi-harbor-network-ports")
        .join(format!("{label}-{}", uuid::Uuid::new_v4()));
    Daemon::start(
        common::quiet(root),
        Arc::new(FakeMidiPlatform::new()) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory")
}

/// Returns a network port's stored settings.
async fn settings(daemon: &Arc<Daemon>, id: EndpointId) -> NetworkSession {
    daemon
        .read(
            |config, _| match config.endpoint(id).map(|e| e.kind.clone()) {
                Some(EndpointKind::NetworkSession(session)) => session,
                _ => panic!("not a network port"),
            },
        )
        .await
}

/// Returns an even UDP port that is free, with the one above it free too for the data port.
///
/// Chosen at random below the ports systems hand out for port zero, which start at 32768 on Linux
/// and 49152 on Windows. Windows hands those out in order, so a pair the system had just chosen
/// and released here was often the next one given to a daemon starting in another test, and the
/// move to it was refused.
fn free_pair() -> u16 {
    for _ in 0..200 {
        // Even ports from 20000 to 29998.
        let port = 20_000 + 2 * rand::random_range(0..5_000u16);
        let free = |port: u16| {
            UdpSocket::bind(("0.0.0.0", port)).is_ok() && UdpSocket::bind(("::", port)).is_ok()
        };
        if free(port) && free(port + 1) {
            return port;
        }
    }
    panic!("no free pair of UDP ports");
}

/// Waits for a network port to be connected, reporting whether it was.
async fn connected(daemon: &Arc<Daemon>, id: EndpointId) -> bool {
    for _ in 0..100 {
        if daemon
            .session_status(id)
            .await
            .is_some_and(|status| status.state.phase() == ConnectionPhase::Connected)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Builds Stage on a near machine connected to Front of House on a far one.
async fn joined() -> (Arc<Daemon>, EndpointId, Arc<Daemon>, EndpointId) {
    let far = machine("far").await;
    let front = far
        .create_network_session("Front of House", 0, InvitationPolicy::AcceptAll)
        .await
        .expect("the far network port is created");
    let port = far
        .session_status(front.id)
        .await
        .expect("the far network port is listening")
        .control_port;
    let near = machine("near").await;
    let stage = near
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near network port is created");
    near.connect_peer(stage.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("the near network port invites the far one");
    assert!(
        connected(&near, stage.id).await,
        "a far network port accepting everyone lets the near one in"
    );
    (near, stage.id, far, front.id)
}

/// Proves that a new Bonjour name is applied to a running network port without dropping the
/// machine connected to it, that a change naming only the Bonjour name leaves every other setting
/// as it was, and that a machine joining afterwards is told the new name.
///
/// The far port accepts everyone, so a change that reset the unnamed settings to their defaults
/// would show as the invitation policy falling back to asking first.
#[tokio::test]
async fn a_new_bonjour_name_leaves_the_connected_machine_connected() {
    let (near, stage, far, front) = joined().await;
    let peer = far
        .session_status(front)
        .await
        .expect("the far network port is running")
        .peer_address;
    let before = settings(&far, front).await;

    far.update_network_port(
        front,
        NetworkPortChange {
            local_name: Some("Main Stage".to_owned()),
            ..NetworkPortChange::default()
        },
    )
    .await
    .expect("the far network port takes the new Bonjour name");

    let after = settings(&far, front).await;
    assert_eq!(
        after.local_name.as_str(),
        "Main Stage",
        "the new Bonjour name is stored"
    );
    assert_eq!(
        after.invitation_policy,
        InvitationPolicy::AcceptAll,
        "a change that says nothing about the invitation policy leaves it alone"
    );
    assert_eq!(
        after.control_port, before.control_port,
        "a change that says nothing about the UDP port leaves it alone"
    );
    assert_eq!(
        after.automatic_port, before.automatic_port,
        "a change that says nothing about the automatic port leaves it alone"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let status = far
        .session_status(front)
        .await
        .expect("the far network port is still running");
    assert_eq!(
        status.state.phase(),
        ConnectionPhase::Connected,
        "a new Bonjour name does not restart the network port"
    );
    assert_eq!(status.peer_address, peer, "the machine was dropped");
    assert!(
        connected(&near, stage).await,
        "the near machine stays connected across the far port's rename"
    );

    // A machine joining now is told the new name.
    let port = status.control_port;
    let late = machine("late").await;
    let booth = late
        .create_network_session("Booth", 0, InvitationPolicy::Prompt)
        .await
        .expect("a third network port is created");
    late.connect_peer(booth.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("the third network port invites the far one");
    let mut told = None;
    for _ in 0..100 {
        told = late
            .session_status(booth.id)
            .await
            .and_then(|status| status.peer_name);
        if told.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        told.as_deref(),
        Some("Main Stage"),
        "a machine joining after the rename is told the new Bonjour name"
    );
}

/// Proves that a new UDP port moves the listener, releases the old port and connects again to
/// the machine the network port had connected to, so moving a port does not cost the connection.
#[tokio::test]
async fn a_new_udp_port_moves_the_listener_and_reconnects_its_machine() {
    let (near, stage, _far, _front) = joined().await;
    let old = near
        .session_status(stage)
        .await
        .expect("the near network port is running")
        .control_port;
    let new = free_pair();

    near.update_network_port(
        stage,
        NetworkPortChange {
            control_port: Some(new),
            ..NetworkPortChange::default()
        },
    )
    .await
    .expect("the near network port moves to a free UDP port");

    assert_eq!(
        settings(&near, stage).await.control_port,
        new,
        "the new UDP port is stored so a restart comes back on it"
    );
    let status = near
        .session_status(stage)
        .await
        .expect("the near network port is running");
    assert_eq!(
        status.control_port, new,
        "the running network port listens on the new UDP port"
    );
    assert!(
        UdpSocket::bind(("0.0.0.0", old)).is_ok(),
        "the old UDP port is still held"
    );
    assert!(
        connected(&near, stage).await,
        "the machine it had connected to was not connected to again"
    );
}

/// Proves that a change whose UDP port cannot be bound is refused whole, before the running
/// network port is touched.
///
/// Stopping it and starting it again on the old port would leave a window in which a daemon
/// starting beside it could take that port (R-082), so the listener's state must not even have
/// restarted.
#[tokio::test]
async fn a_udp_port_that_cannot_be_bound_leaves_the_network_port_where_it_was() {
    let daemon = machine("taken").await;
    let stage = daemon
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the network port is created");
    let old = settings(&daemon, stage.id).await.control_port;
    let listening_since = daemon
        .session_status(stage.id)
        .await
        .expect("the network port is running")
        .state
        .since();
    let taken = free_pair();
    let _holder = (
        UdpSocket::bind(("0.0.0.0", taken)).expect("the test holds the UDP port it will ask for"),
        UdpSocket::bind(("::", taken)).ok(),
    );

    let refused = daemon
        .update_network_port(
            stage.id,
            NetworkPortChange {
                control_port: Some(taken),
                local_name: Some("Main Stage".to_owned()),
                ..NetworkPortChange::default()
            },
        )
        .await;

    assert!(refused.is_err(), "a UDP port in use was accepted");
    let kept = settings(&daemon, stage.id).await;
    assert_eq!(
        kept.control_port, old,
        "the refused UDP port was stored anyway"
    );
    assert_eq!(
        kept.local_name.as_str(),
        "Stage",
        "the refused change was half applied"
    );
    assert_eq!(
        daemon
            .session_status(stage.id)
            .await
            .expect("the network port is still running")
            .control_port,
        old,
        "the network port was left down"
    );
    assert_eq!(
        daemon
            .session_status(stage.id)
            .await
            .expect("the network port is still running")
            .state
            .since(),
        listening_since,
        "the network port was stopped and started again"
    );
}

/// What is tried against Stage, on a daemon where Booth is another network port.
#[derive(Debug)]
enum Attempt {
    /// Moves Stage to the UDP port Booth listens on.
    MoveToBoothsPort,
    /// Moves Stage to an even UDP port nothing holds.
    MoveToFreePort,
    /// Changes Stage as given.
    Change(NetworkPortChange),
    /// Makes another network port.
    Create {
        name: &'static str,
        local_name: Option<&'static str>,
        control_port: u16,
    },
}

/// Proves that a network port cannot take a Bonjour name or UDP port another network port holds,
/// nor an odd UDP port, while a name or port nobody holds is taken.
///
/// Two network ports advertising one name look like one machine to every peer, and two on one UDP
/// port cannot both listen. RTP-MIDI's data port is the one above the control port, so an odd
/// control port would be moved up one without saying; it is refused instead, naming the even
/// ports either side.
#[tokio::test]
async fn a_name_or_udp_port_another_network_port_holds_is_refused() {
    struct Case {
        name: &'static str,
        attempt: Attempt,
        refused: bool,
        /// Text the refusal must contain, so the user can tell what to change.
        naming: Option<&'static str>,
    }
    let cases = [
        Case {
            name: "a UDP port another network port listens on",
            attempt: Attempt::MoveToBoothsPort,
            refused: true,
            naming: Some("Booth"),
        },
        Case {
            name: "a Bonjour name another network port advertises",
            attempt: Attempt::Change(NetworkPortChange {
                local_name: Some("Booth".to_owned()),
                ..NetworkPortChange::default()
            }),
            refused: true,
            naming: None,
        },
        Case {
            name: "an odd UDP port",
            attempt: Attempt::Change(NetworkPortChange {
                control_port: Some(5005),
                ..NetworkPortChange::default()
            }),
            refused: true,
            naming: None,
        },
        Case {
            name: "a new network port on an odd UDP port",
            attempt: Attempt::Create {
                name: "Odd",
                local_name: None,
                control_port: 5005,
            },
            refused: true,
            naming: None,
        },
        Case {
            name: "a new network port advertising Stage",
            attempt: Attempt::Create {
                name: "Studio",
                local_name: Some("Stage"),
                control_port: 0,
            },
            refused: true,
            naming: None,
        },
        Case {
            name: "a Bonjour name nobody advertises",
            attempt: Attempt::Change(NetworkPortChange {
                local_name: Some("Main Stage".to_owned()),
                ..NetworkPortChange::default()
            }),
            refused: false,
            naming: None,
        },
        Case {
            name: "an even UDP port nothing holds",
            attempt: Attempt::MoveToFreePort,
            refused: false,
            naming: None,
        },
        Case {
            name: "a new network port advertising a name of its own",
            attempt: Attempt::Create {
                name: "Studio",
                local_name: Some("Studio"),
                control_port: 0,
            },
            refused: false,
            naming: None,
        },
    ];

    for case in cases {
        let daemon = machine("held").await;
        let stage = daemon
            .create_network_session("Stage", 0, InvitationPolicy::Prompt)
            .await
            .expect("Stage is created");
        let booth = daemon
            .create_network_session("Booth", 0, InvitationPolicy::Prompt)
            .await
            .expect("Booth is created beside Stage");
        let before = settings(&daemon, stage.id).await;

        let result = match case.attempt {
            Attempt::MoveToBoothsPort => {
                let port = settings(&daemon, booth.id).await.control_port;
                let change = NetworkPortChange {
                    control_port: Some(port),
                    ..NetworkPortChange::default()
                };
                daemon.update_network_port(stage.id, change).await.map(drop)
            }
            Attempt::MoveToFreePort => {
                let change = NetworkPortChange {
                    control_port: Some(free_pair()),
                    ..NetworkPortChange::default()
                };
                daemon.update_network_port(stage.id, change).await.map(drop)
            }
            Attempt::Change(change) => daemon.update_network_port(stage.id, change).await.map(drop),
            Attempt::Create {
                name,
                local_name,
                control_port,
            } => daemon
                .create_network_port(
                    name,
                    local_name,
                    control_port,
                    InvitationPolicy::Prompt,
                    true,
                )
                .await
                .map(drop),
        };

        assert_eq!(
            result.is_err(),
            case.refused,
            "{}: refused should be {}, got {result:?}",
            case.name,
            case.refused
        );
        if let (Some(text), Err(error)) = (case.naming, &result) {
            assert!(
                error.to_string().contains(text),
                "{}: the refusal does not say which network port holds it: {error}",
                case.name
            );
        }
        if case.refused {
            let after = settings(&daemon, stage.id).await;
            assert_eq!(
                after.local_name, before.local_name,
                "{}: a refused change still renamed Stage",
                case.name
            );
            assert_eq!(
                after.control_port, before.control_port,
                "{}: a refused change still moved Stage",
                case.name
            );
        }
    }
}

/// Proves that a new invitation policy reaches the running network port: a machine held waiting
/// under "ask first" is let in once the network port accepts anyone, without restarting it.
#[tokio::test]
async fn a_new_invitation_policy_applies_to_the_running_network_port() {
    let far = machine("policy-far").await;
    let front = far
        .create_network_session("Front of House", 0, InvitationPolicy::Prompt)
        .await
        .expect("the far network port is created");
    let port = far
        .session_status(front.id)
        .await
        .expect("the far network port is listening")
        .control_port;
    let near = machine("policy-near").await;
    let stage = near
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near network port is created");
    near.connect_peer(stage.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("the near network port invites the far one");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_ne!(
        far.session_status(front.id)
            .await
            .expect("the far network port is running")
            .state
            .phase(),
        ConnectionPhase::Connected,
        "let in without being asked"
    );

    far.set_invitation_policy(front.id, InvitationPolicy::AcceptAll)
        .await
        .expect("the far network port's policy changes to accepting everyone");

    // The near side invites again on its own backoff, so this waits out a few of its tries.
    let mut let_in = false;
    for _ in 0..3 {
        if connected(&near, stage.id).await {
            let_in = true;
            break;
        }
    }
    assert!(
        let_in,
        "the new policy did not reach the running network port"
    );
}

/// Waits for a network port's machines to satisfy a condition, returning the last seen.
async fn machines_until(
    daemon: &Arc<Daemon>,
    id: EndpointId,
    wanted: impl Fn(&[midi_harbor_daemon::session::Machine]) -> bool,
) -> Vec<midi_harbor_daemon::session::Machine> {
    let mut seen = Vec::new();
    for _ in 0..200 {
        seen = daemon
            .session_status(id)
            .await
            .map(|status| status.machines)
            .unwrap_or_default();
        if wanted(&seen) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    seen
}

/// Proves that a second machine invited beside the peer is carried and remembered beside it,
/// that disconnecting the peer leaves the second machine joined and remembered as the peer in its
/// place, and that a machine no longer taking part cannot be disconnected again.
///
/// The peer disconnected by the user is forgotten, so it is not connected to again on the next
/// start, and it leaves the remembered machines with the connection that put it there.
///
/// Regression: the machine stayed remembered at the address it was connected at, so the network
/// port went on offering an old connection after the far side had stopped listening there.
#[tokio::test]
async fn a_second_machine_joins_beside_the_first_and_each_can_be_disconnected_alone() {
    let (near, stage, far, front) = joined().await;
    let far_port = far
        .session_status(front)
        .await
        .expect("the far network port is running")
        .control_port;
    let booth_machine = machine("booth").await;
    let booth = booth_machine
        .create_network_session("Booth", 0, InvitationPolicy::AcceptAll)
        .await
        .expect("a third network port is created");
    let booth_port = booth_machine
        .session_status(booth.id)
        .await
        .expect("the third network port is listening")
        .control_port;
    let (to_front, to_booth) = (
        SocketAddr::from(([127, 0, 0, 1], far_port)),
        SocketAddr::from(([127, 0, 0, 1], booth_port)),
    );
    assert!(
        settings(&near, stage).await.peer.is_some(),
        "the machine connected first is remembered as the peer"
    );

    near.invite_machine(stage, to_booth)
        .await
        .expect("the near network port invites a second machine");
    let both = machines_until(&near, stage, |machines| {
        machines.len() == 2 && machines.iter().all(|machine| machine.joined)
    })
    .await;
    assert_eq!(
        both.iter()
            .map(|machine| machine.address)
            .collect::<Vec<_>>(),
        vec![to_front, to_booth],
        "both machines are joined, the peer first: {both:?}"
    );

    let remembered = settings(&near, stage).await;
    assert_eq!(
        remembered.other_peers.len(),
        1,
        "the second machine is not remembered beside the first"
    );
    let second = remembered.other_peers[0];

    // The peer goes, and is forgotten. The second machine stays, remembered as the peer.
    near.disconnect_machine(stage, to_front)
        .await
        .expect("the near network port disconnects the first machine");
    let remembered = settings(&near, stage).await;
    assert_eq!(
        remembered.peer,
        Some(second),
        "the machine that took the peer's place is not remembered as the peer"
    );
    assert!(
        remembered.other_peers.is_empty(),
        "the machine promoted to peer is still remembered beside it: {remembered:?}"
    );
    let known: Vec<Vec<String>> = near
        .read(|config, _| {
            config
                .peers
                .iter()
                .map(|known| known.addresses.clone())
                .collect()
        })
        .await;
    assert_eq!(
        known,
        vec![vec![to_booth.to_string()]],
        "only the machine still connected is remembered, not the one disconnected"
    );
    let left = machines_until(&near, stage, |machines| {
        machines.len() == 1 && machines[0].joined
    })
    .await;
    assert_eq!(
        left.len(),
        1,
        "only the second machine is left joined: {left:?}"
    );
    assert_eq!(
        left[0].address, to_booth,
        "disconnecting the first machine left the second one carried"
    );

    assert!(
        near.disconnect_machine(stage, to_front).await.is_err(),
        "a machine no longer taking part was disconnected again"
    );
}

/// Starts a far machine with a network port accepting everyone, returning it, its network port
/// and the address to reach that network port at.
async fn accepting_machine(label: &str, name: &str) -> (Arc<Daemon>, EndpointId, SocketAddr) {
    let daemon = machine(label).await;
    let port = daemon
        .create_network_session(name, 0, InvitationPolicy::AcceptAll)
        .await
        .expect("the far network port is created");
    let control_port = daemon
        .session_status(port.id)
        .await
        .expect("the far network port is listening")
        .control_port;
    (
        daemon,
        port.id,
        SocketAddr::from(([127, 0, 0, 1], control_port)),
    )
}

/// Reports whether every machine listed is carrying MIDI and they are exactly `expected`.
fn all_joined(machines: &[midi_harbor_daemon::session::Machine], expected: &[SocketAddr]) -> bool {
    machines.iter().all(|machine| machine.joined)
        && machines
            .iter()
            .map(|machine| machine.address)
            .collect::<Vec<_>>()
            == expected
}

/// Proves that a machine invited straight after the peer is connected is remembered beside the
/// peer rather than in its place.
///
/// Regression: the session's status was read to tell whether the machine became the peer, before
/// the session had acted on the connect ahead of it. The machine was remembered as the peer, the
/// peer was forgotten, and after a restart only the machine came back. On one thread the session
/// cannot act between the two calls, so the old reading fails every time.
#[tokio::test]
async fn a_machine_invited_straight_after_connecting_is_remembered_beside_the_peer() {
    let (_front_machine, _, to_front) = accepting_machine("straight-front", "Front of House").await;
    let (_booth_machine, _, to_booth) = accepting_machine("straight-booth", "Booth").await;
    let near = machine("straight-near").await;
    let stage = near
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near network port is created")
        .id;
    near.connect_peer(stage, to_front)
        .await
        .expect("the near network port connects to Front of House");
    near.invite_machine(stage, to_booth)
        .await
        .expect("the near network port invites Booth beside it");

    let remembered = settings(&near, stage).await;
    let peer = remembered
        .peer
        .expect("the machine connected first is remembered as the peer");
    assert_eq!(
        addresses_of(&near, peer).await,
        vec![to_front.to_string()],
        "the machine invited took the peer's place: {remembered:?}"
    );
    assert_eq!(
        remembered.other_peers.len(),
        1,
        "the invited machine is remembered beside the peer: {remembered:?}"
    );
    assert_eq!(
        addresses_of(&near, remembered.other_peers[0]).await,
        vec![to_booth.to_string()],
        "the machine remembered beside the peer is the one invited"
    );
}

/// Returns the addresses a remembered machine is reached at.
async fn addresses_of(daemon: &Arc<Daemon>, peer: midi_harbor_core::ids::PeerId) -> Vec<String> {
    daemon
        .read(move |config, _| {
            config
                .peers
                .iter()
                .find(|known| known.id == peer)
                .map(|known| known.addresses.clone())
                .unwrap_or_default()
        })
        .await
}

/// Proves that every machine connected comes back after a restart, and that one the user
/// disconnected does not (FR-015i).
///
/// Regression: only the first machine was remembered, so a second one connected beside it had to
/// be connected again by hand after every restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_machine_connected_comes_back_after_a_restart_until_it_is_disconnected() {
    let (_front_machine, _, to_front) = accepting_machine("restart-front", "Front of House").await;
    let (_booth_machine, _, to_booth) = accepting_machine("restart-booth", "Booth").await;
    let root = common::scratch("midi-harbor-network-ports")
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
    let stage = first
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near network port is created")
        .id;
    first
        .connect_peer(stage, to_front)
        .await
        .expect("the near network port connects to Front of House");
    first
        .invite_machine(stage, to_booth)
        .await
        .expect("the near network port invites Booth beside it");
    let both = machines_until(&first, stage, |machines| {
        all_joined(machines, &[to_front, to_booth])
    })
    .await;
    assert!(
        all_joined(&both, &[to_front, to_booth]),
        "both machines join before the restart: {both:?}"
    );

    // Both come back after a restart.
    first.end_sessions().await;
    let second = start()
        .await
        .expect("the near daemon starts again over the same directory");
    let back = machines_until(&second, stage, |machines| {
        all_joined(machines, &[to_front, to_booth])
    })
    .await;
    assert!(
        all_joined(&back, &[to_front, to_booth]),
        "only some machines came back: {back:?}"
    );

    // The second machine, disconnected, is forgotten; the first is not.
    second
        .disconnect_machine(stage, to_booth)
        .await
        .expect("the near network port disconnects Booth");
    let remembered = settings(&second, stage).await;
    assert!(
        remembered.peer.is_some(),
        "disconnecting the second machine forgot the peer too: {remembered:?}"
    );
    assert!(
        remembered.other_peers.is_empty(),
        "the machine the user disconnected is still remembered: {remembered:?}"
    );

    second.end_sessions().await;
    let third = start()
        .await
        .expect("the near daemon starts a third time over the same directory");
    assert!(
        connected(&third, stage).await,
        "the first did not come back"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    let machines = machines_until(&third, stage, |machines| machines.len() == 1).await;
    assert_eq!(
        machines
            .iter()
            .map(|machine| machine.address)
            .collect::<Vec<_>>(),
        vec![to_front],
        "a machine the user disconnected came back after a restart"
    );
}

/// Proves that a machine connected beside the peer, whose network port goes away, stays listed
/// and waiting without disturbing the peer, and is connected again without being asked when it
/// returns on the same UDP port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_connected_beside_the_first_comes_back_when_its_link_returns() {
    let (_front_machine, _, to_front) = accepting_machine("drop-front", "Front of House").await;
    let (booth_machine, booth, to_booth) = accepting_machine("drop-booth", "Booth").await;
    let near = machine("drop-near").await;
    let stage = near
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near network port is created")
        .id;
    near.connect_peer(stage, to_front)
        .await
        .expect("the near network port connects to Front of House");
    near.invite_machine(stage, to_booth)
        .await
        .expect("the near network port invites Booth beside it");
    let both = machines_until(&near, stage, |machines| {
        all_joined(machines, &[to_front, to_booth])
    })
    .await;
    assert!(
        all_joined(&both, &[to_front, to_booth]),
        "both machines join before Booth goes away: {both:?}"
    );

    // The second machine's network port goes away.
    booth_machine
        .set_enabled(booth, false)
        .await
        .expect("Booth's network port is switched off");
    let waiting = machines_until(&near, stage, |machines| {
        machines
            .iter()
            .any(|machine| machine.address == to_booth && !machine.joined)
    })
    .await;
    assert!(
        waiting
            .iter()
            .any(|machine| machine.address == to_booth && !machine.joined),
        "the second machine was let go: {waiting:?}"
    );
    assert!(
        connected(&near, stage).await,
        "the first machine was dropped"
    );

    // It returns on the same UDP port.
    booth_machine
        .set_enabled(booth, true)
        .await
        .expect("Booth's network port is switched on again");
    let back = machines_until(&near, stage, |machines| {
        all_joined(machines, &[to_front, to_booth])
    })
    .await;
    assert!(
        all_joined(&back, &[to_front, to_booth]),
        "the second machine did not come back: {back:?}"
    );
}

/// Proves that a machine invited while nothing is connected becomes the remembered peer, so it
/// is connected to again after a restart like a machine connected to directly.
#[tokio::test]
async fn inviting_a_machine_with_nothing_connected_makes_it_the_remembered_peer() {
    let (_far, _, to_far) = accepting_machine("alone-far", "Front of House").await;
    let near = machine("alone-near").await;
    let stage = near
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near network port is created");

    near.invite_machine(stage.id, to_far)
        .await
        .expect("the near network port invites Front of House");

    assert!(
        connected(&near, stage.id).await,
        "a machine accepting everyone lets the invited one in"
    );
    assert!(
        settings(&near, stage.id).await.peer.is_some(),
        "the only machine is not connected again after a restart"
    );
}

/// Builds a far machine asking about invitations and a near one inviting it, returning the far
/// daemon, its network port, and the near side's network port with the far port to invite.
async fn asking(label: &str) -> (Arc<Daemon>, EndpointId, Arc<Daemon>, EndpointId, SocketAddr) {
    let far = machine(&format!("{label}-far")).await;
    let front = far
        .create_network_session("Front of House", 0, InvitationPolicy::Prompt)
        .await
        .expect("the far network port is created");
    let port = far
        .session_status(front.id)
        .await
        .expect("the far network port is listening")
        .control_port;
    let near = machine(&format!("{label}-near")).await;
    let stage = near
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near network port is created");
    (
        far,
        front.id,
        near,
        stage.id,
        SocketAddr::from(([127, 0, 0, 1], port)),
    )
}

/// Waits for an invitation to be waiting on the user, returning its identifier.
async fn asked(daemon: &Arc<Daemon>) -> Option<String> {
    for _ in 0..100 {
        if let Some(invitation) = daemon.pending_invitations().await.first() {
            return Some(invitation.id.clone());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// How the far machine came to let the near one in.
#[derive(Clone, Copy, Debug)]
enum Grant {
    /// The user accepted its invitation once, for this run.
    AnsweredOnce,
    /// It was added as a known machine and trusted.
    Trusted,
    /// The user accepted its invitation once, and then it was added as a trusted machine named
    /// "Stage machine".
    AnsweredOnceAndTrusted,
}

/// How the far machine took that permission away.
#[derive(Clone, Copy, Debug)]
enum Withdraw {
    /// Trust was switched off for its address.
    TrustSwitchedOff,
    /// It was added again as a known machine named "Stage machine", untrusted.
    AddedAgainUntrusted,
    /// The known machine "Stage machine" was forgotten.
    Forgotten,
}

/// Proves that once permission to join is withdrawn, however it was given and however it was
/// withdrawn, the machine's next invitation is asked about rather than let in.
///
/// Each row connects, disconnects with `disconnect_peer`, withdraws permission and connects
/// again, so it also proves that a session disconnected and connected again sends a fresh
/// invitation, and that a machine answered once is connected. Regression: an invitation accepted
/// once let the machine in until the daemon restarted, whatever trust later said, and a machine
/// added again untrusted kept the trust it had.
#[tokio::test]
async fn once_trust_is_withdrawn_the_next_invitation_is_asked_about() {
    struct Case {
        name: &'static str,
        grant: Grant,
        withdraw: Withdraw,
    }
    let cases = [
        Case {
            name: "answered once, then added untrusted",
            grant: Grant::AnsweredOnce,
            withdraw: Withdraw::AddedAgainUntrusted,
        },
        Case {
            name: "answered once and trusted, then trust switched off",
            grant: Grant::AnsweredOnceAndTrusted,
            withdraw: Withdraw::TrustSwitchedOff,
        },
        Case {
            name: "answered once and trusted, then forgotten",
            grant: Grant::AnsweredOnceAndTrusted,
            withdraw: Withdraw::Forgotten,
        },
        Case {
            name: "trusted, then trust switched off",
            grant: Grant::Trusted,
            withdraw: Withdraw::TrustSwitchedOff,
        },
        Case {
            name: "trusted, then added again untrusted",
            grant: Grant::Trusted,
            withdraw: Withdraw::AddedAgainUntrusted,
        },
    ];

    for case in cases {
        let (far, front, near, stage, to_far) = asking("trust").await;

        // Grant permission, and connect under it.
        match case.grant {
            Grant::AnsweredOnce | Grant::AnsweredOnceAndTrusted => {
                near.connect_peer(stage, to_far)
                    .await
                    .expect("the near network port invites the far one");
                let invitation = asked(&far).await.unwrap_or_else(|| {
                    panic!("{}: an unknown machine was not asked about", case.name)
                });
                far.respond_to_invitation(&invitation, true, false)
                    .await
                    .expect("the far machine accepts the invitation once");
                if matches!(case.grant, Grant::AnsweredOnceAndTrusted) {
                    far.add_known_machine("127.0.0.1", Some("Stage machine".to_owned()), true)
                        .await
                        .expect("the far machine remembers the near one as trusted");
                }
            }
            Grant::Trusted => {
                far.add_known_machine("127.0.0.1", None, true)
                    .await
                    .expect("the far machine trusts the near one");
            }
        }
        near.connect_peer(stage, to_far)
            .await
            .expect("the near network port invites the far one again");
        assert!(
            connected(&near, stage).await,
            "{}: a machine given permission was not let in",
            case.name
        );

        // Withdraw it, and invite again.
        near.disconnect_peer(stage)
            .await
            .expect("the near network port leaves the far one");
        match case.withdraw {
            Withdraw::TrustSwitchedOff => {
                let known = far
                    .set_peer_trusted("127.0.0.1", false)
                    .await
                    .expect("the far machine switches trust off");
                assert!(
                    !known.trusted,
                    "{}: switching trust off reports the machine as still trusted",
                    case.name
                );
            }
            Withdraw::AddedAgainUntrusted => {
                let again = far
                    .add_known_machine("127.0.0.1", Some("Stage machine".to_owned()), false)
                    .await
                    .expect("the far machine adds the near one again, untrusted");
                assert!(
                    !again.trusted,
                    "{}: the second add left it trusted",
                    case.name
                );
                assert_eq!(
                    again.name, "Stage machine",
                    "{}: adding a known machine again takes the name given",
                    case.name
                );
            }
            Withdraw::Forgotten => {
                far.remove_peer("Stage machine")
                    .await
                    .expect("the far machine forgets the near one");
            }
        }
        near.connect_peer(stage, to_far)
            .await
            .expect("the near network port invites the far one a third time");

        assert!(
            asked(&far).await.is_some(),
            "{}: a machine whose permission was withdrawn was let in without asking",
            case.name
        );
        assert_ne!(
            far.session_status(front)
                .await
                .expect("the far network port is running")
                .state
                .phase(),
            ConnectionPhase::Connected,
            "{}: the far network port connected while the invitation waited on the user",
            case.name
        );
    }
}

/// Proves that deleting a network port drops the invitations waiting on it, so the user is not
/// asked about joining something that no longer exists.
#[tokio::test]
async fn deleting_a_network_port_drops_the_invitations_waiting_on_it() {
    let (far, front, near, stage, to_far) = asking("deleted").await;
    near.connect_peer(stage, to_far)
        .await
        .expect("the near network port invites the far one");
    assert!(
        asked(&far).await.is_some(),
        "a far network port that asks first holds the invitation for the user"
    );

    far.delete_network_port(front)
        .await
        .expect("the far network port is deleted");

    assert!(
        far.pending_invitations().await.is_empty(),
        "an invitation to a deleted network port is still waiting"
    );
}

/// Returns how many endpoints hold a name.
async fn holding(daemon: &Arc<Daemon>, name: &str) -> usize {
    daemon
        .read(|config, _| {
            config
                .endpoints
                .iter()
                .filter(|e| e.name.as_str() == name)
                .count()
        })
        .await
}

/// A name claimed on a daemon holding the port Keys, the network port Stage, and the network port
/// Booth without its automatic port.
#[derive(Debug)]
enum Claim {
    /// Makes a port.
    Port(&'static str),
    /// Makes a network port, with or without its automatic port.
    NetworkPort {
        name: &'static str,
        automatic_port: bool,
    },
    /// Renames an endpoint.
    Rename {
        from: &'static str,
        to: &'static str,
    },
    /// Imports a configuration, merged with the one held.
    Import(&'static str),
}

/// What a claim should come to.
#[derive(Debug)]
enum Outcome {
    /// It is taken.
    Accepted,
    /// It is refused as a name already in use, naming that name.
    Taken(&'static str),
    /// It is refused with a message containing this text.
    Reported(&'static str),
}

/// Proves that ports and network ports share one namespace, whichever is made, renamed or
/// imported second, while an endpoint keeping its own name is not a clash with itself.
///
/// Other applications see a network port as a port of its name, so the two would show as
/// identical ports. A network port without its automatic port holds its name too, because
/// switching the automatic port on later would bring the clash back. A merging import matches
/// endpoints by kind and name, so it would add the network port beside the port of its name. A
/// rename that keeps the name is what an edit saved unchanged sends.
#[tokio::test]
async fn a_port_and_a_network_port_cannot_share_a_name() {
    struct Case {
        name: &'static str,
        claim: Claim,
        want: Outcome,
    }
    let cases = [
        Case {
            name: "a port under a network port's name",
            claim: Claim::Port("Stage"),
            want: Outcome::Taken("Stage"),
        },
        Case {
            name: "a port under a network port's name with spaces around it",
            claim: Claim::Port("  Stage "),
            want: Outcome::Taken("Stage"),
        },
        Case {
            name: "a port under the name of a network port without its automatic port",
            claim: Claim::Port("Booth"),
            want: Outcome::Taken("Booth"),
        },
        Case {
            name: "a network port under a port's name",
            claim: Claim::NetworkPort {
                name: "Keys",
                automatic_port: true,
            },
            want: Outcome::Taken("Keys"),
        },
        Case {
            name: "a network port without its automatic port under a port's name",
            claim: Claim::NetworkPort {
                name: "Keys",
                automatic_port: false,
            },
            want: Outcome::Taken("Keys"),
        },
        Case {
            name: "a port renamed to a network port's name",
            claim: Claim::Rename {
                from: "Keys",
                to: "Stage",
            },
            want: Outcome::Taken("Stage"),
        },
        Case {
            name: "a network port renamed to a port's name",
            claim: Claim::Rename {
                from: "Stage",
                to: "Keys",
            },
            want: Outcome::Taken("Keys"),
        },
        Case {
            name: "a network port imported under a port's name",
            claim: Claim::Import(
                "endpoints:\n\
                 - name: Keys\n\
                 \x20 kind: network_port\n\
                 \x20 control_port: 0\n",
            ),
            want: Outcome::Reported("both named 'Keys'"),
        },
        Case {
            name: "a port renamed to its own name",
            claim: Claim::Rename {
                from: "Keys",
                to: "Keys",
            },
            want: Outcome::Accepted,
        },
        Case {
            name: "a network port renamed to its own name",
            claim: Claim::Rename {
                from: "Stage",
                to: "Stage",
            },
            want: Outcome::Accepted,
        },
    ];

    for case in cases {
        let daemon = machine("names").await;
        daemon
            .create_virtual_port("Keys", 1, 1)
            .await
            .expect("the port Keys is created");
        daemon
            .create_network_session("Stage", 0, InvitationPolicy::Prompt)
            .await
            .expect("the network port Stage is created");
        daemon
            .create_network_port("Booth", None, 0, InvitationPolicy::Prompt, false)
            .await
            .expect("the network port Booth is created without its automatic port");

        let result = match case.claim {
            Claim::Port(name) => daemon.create_virtual_port(name, 1, 1).await.map(drop),
            Claim::NetworkPort {
                name,
                automatic_port,
            } => daemon
                .create_network_port(name, None, 0, InvitationPolicy::Prompt, automatic_port)
                .await
                .map(drop),
            Claim::Rename { from, to } => {
                let id = daemon
                    .resolve(from)
                    .await
                    .expect("the endpoint to rename exists");
                daemon.rename_endpoint(id, to, true).await.map(drop)
            }
            Claim::Import(text) => daemon.import_configuration(text, false).await.map(drop),
        };

        let as_wanted = match (&case.want, &result) {
            (Outcome::Accepted, Ok(())) => true,
            (
                Outcome::Taken(name),
                Err(DaemonError::Failure(FailureReason::NameConflict { name: taken })),
            ) => taken == name,
            (Outcome::Reported(text), Err(error)) => error.to_string().contains(text),
            _ => false,
        };
        assert!(
            as_wanted,
            "{}: expected {:?}, got {result:?}",
            case.name, case.want
        );
        for held in ["Keys", "Stage", "Booth"] {
            assert_eq!(
                holding(&daemon, held).await,
                1,
                "{}: {held} is no longer held by exactly one endpoint",
                case.name
            );
        }
    }
}

/// Proves that a hand-written configuration holding a port and a network port of one name keeps
/// both and records the clash in the history as an error.
///
/// Dropping either would lose part of the user's setup, and saying nothing would leave two
/// identical ports with no explanation.
#[tokio::test]
async fn a_hand_written_port_and_network_port_of_one_name_are_kept_and_reported() {
    let root = common::scratch("midi-harbor-network-ports")
        .join(format!("hand-written-{}", uuid::Uuid::new_v4()));
    let paths = midi_harbor_core::paths::Paths::rooted_at(root);
    std::fs::create_dir_all(paths.config_dir()).expect("the configuration directory is created");
    std::fs::write(
        paths.config_file(),
        "preferences:\n\
         \x20 advertise_sessions: false\n\
         endpoints:\n\
         - name: Stage\n\
         \x20 kind: virtual_port\n\
         - name: Stage\n\
         \x20 kind: network_port\n\
         \x20 control_port: 0\n",
    )
    .expect("the hand-written configuration is written");

    let daemon = Daemon::start(
        paths,
        Arc::new(FakeMidiPlatform::new()) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over the hand-written configuration");

    assert_eq!(
        holding(&daemon, "Stage").await,
        2,
        "both endpoints named Stage are kept"
    );
    let reported = daemon.events(None, 100).await.into_iter().any(|event| {
        event.severity == midi_harbor_core::events::Severity::Error
            && event.detail.contains("both named 'Stage'")
    });
    assert!(reported, "the clash was not recorded in the history");
}

/// Builds an advertisement of a session as discovery reports one, since a test runner cannot be
/// relied on to carry multicast.
fn advertisement(
    name: &str,
    address: SocketAddr,
    identity: Option<PortIdentity>,
) -> DiscoveredPeer {
    DiscoveredPeer {
        id: midi_harbor_core::ids::PeerId::new(),
        name: name.to_owned(),
        fullname: format!("{name}._apple-midi._udp.local."),
        addresses: vec![address.ip()],
        port: address.port(),
        is_self: false,
        identity,
    }
}

/// Starts a near daemon over a hand-written configuration whose network port Stage connects to
/// the machine written as `peer`, the lines of one entry under `peers`. Returns the daemon, Stage
/// and the paths, to read the configuration back from disk.
async fn stage_connecting_to(
    label: &str,
    peer: &str,
) -> (Arc<Daemon>, EndpointId, midi_harbor_core::paths::Paths) {
    let root = common::scratch("midi-harbor-network-ports")
        .join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let paths = midi_harbor_core::paths::Paths::rooted_at(root);
    std::fs::create_dir_all(paths.config_dir()).expect("the configuration directory is created");
    std::fs::write(
        paths.config_file(),
        format!(
            "preferences:\n\
             \x20 advertise_sessions: false\n\
             endpoints:\n\
             - name: Stage\n\
             \x20 kind: network_port\n\
             \x20 control_port: 0\n\
             \x20 peer: 6f1d4c1e-3b0a-4a52-9d57-0c2f5a8e7b11\n\
             peers:\n\
             - id: 6f1d4c1e-3b0a-4a52-9d57-0c2f5a8e7b11\n\
             \x20 name: Front of House\n\
             {peer}"
        ),
    )
    .expect("the configuration is written");
    let near = Daemon::start(
        paths.clone(),
        Arc::new(FakeMidiPlatform::new()) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the near daemon starts over the configuration");
    let stage = near
        .read(|config, _| {
            config
                .endpoints
                .iter()
                .find(|endpoint| endpoint.name.as_str() == "Stage")
                .map(|endpoint| endpoint.id)
        })
        .await
        .expect("Stage is loaded from the configuration");
    (near, stage, paths)
}

/// Returns the one remembered machine, read back from disk.
fn stored_machine(paths: &midi_harbor_core::paths::Paths) -> midi_harbor_core::config::PeerConfig {
    midi_harbor_core::config::load(paths)
        .expect("the configuration is read back from disk")
        .config
        .peers
        .into_iter()
        .next()
        .expect("one machine is remembered")
}

/// Proves that a machine whose session is advertised on another port is connected to there once
/// its link is down, with the new address stored for the next start, and that a machine
/// carrying MIDI is left where it is (R-105).
///
/// A session that comes back on a port the system chose again was invited at its old port for
/// good, although it was advertised on the new one. An advertisement naming a connected machine
/// elsewhere may be another machine that took the name, so it moves nothing.
#[tokio::test]
async fn a_machine_whose_session_moved_is_followed_to_where_it_is_advertised() {
    let (_front_machine, _, to_front) = accepting_machine("moved-front", "Front of House").await;
    let (_booth_machine, _, to_booth) = accepting_machine("moved-booth", "Booth").await;
    // Where Front of House was when Stage connected to it. Nothing listens there now.
    let gone = free_pair();
    let (near, stage, paths) = stage_connecting_to(
        "moved",
        &format!(
            "\x20 addresses: [\"127.0.0.1:{gone}\"]\n\
             \x20 advertised_as: Front of House\n"
        ),
    )
    .await;

    // Its link down, the machine is followed to the port its session is advertised on.
    near.follow_advertised(&[advertisement("Front of House", to_front, None)])
        .await;
    assert!(
        connected(&near, stage).await,
        "Stage did not connect to the session where it is advertised now"
    );
    assert_eq!(
        stored_machine(&paths).addresses,
        vec![to_front.to_string()],
        "the new address is not stored, so a restart would invite the old one"
    );

    // Carrying MIDI, it is left where it is.
    near.follow_advertised(&[advertisement("Front of House", to_booth, None)])
        .await;
    let machines = machines_until(&near, stage, |machines| all_joined(machines, &[to_front])).await;
    assert!(
        all_joined(&machines, &[to_front]),
        "a connected machine was moved by an advertisement: {machines:?}"
    );
    assert_eq!(
        stored_machine(&paths).addresses,
        vec![to_front.to_string()],
        "a connected machine's stored address was changed by an advertisement"
    );
}

/// Proves that a trusted machine is followed to another host only by the session that proves
/// it is the network port connected to before, and that the trust goes with it (R-106).
///
/// Trust is held by host, and an advertisement can claim any key. A machine claiming Front of
/// House's key from another host is asked to sign a challenge with it; Booth cannot, and is not
/// followed. The IPv6 loopback address stands for the host Front of House left, and the IPv4 one
/// for the host it is on now.
#[tokio::test]
async fn a_trusted_machine_is_followed_to_another_host_once_it_proves_which_port_it_is() {
    let (front_machine, front, to_front) =
        accepting_machine("proved-front", "Front of House").await;
    let (_booth_machine, _, to_booth) = accepting_machine("proved-booth", "Booth").await;
    let front_of_house = PortIdentity::new(&front_machine.identity_key(), front)
        .expect("Front of House has a key and an identifier");
    let gone = free_pair();
    let held = format!("[::1]:{gone}");
    let (near, stage, paths) = stage_connecting_to(
        "proved",
        &format!(
            "\x20 addresses: [\"{held}\"]\n\
             \x20 trusted: true\n\
             \x20 advertised_as: Front of House\n\
             \x20 key: {}\n\
             \x20 port_id: {front}\n",
            front_machine.identity_key()
        ),
    )
    .await;

    // Booth advertises itself as Front of House's port. It cannot prove it.
    near.follow_advertised(&[advertisement(
        "Front of House",
        to_booth,
        Some(front_of_house),
    )])
    .await;
    assert_eq!(
        stored_machine(&paths).addresses,
        vec![held],
        "a machine that did not prove the key was followed, and trusted"
    );

    // Front of House, renamed and on another host, proves it.
    near.follow_advertised(&[advertisement("Main Stage", to_front, Some(front_of_house))])
        .await;
    assert!(
        connected(&near, stage).await,
        "Stage did not follow the port that proved itself"
    );
    let stored = stored_machine(&paths);
    assert_eq!(
        (stored.addresses, stored.advertised_as, stored.trusted),
        (
            vec![to_front.to_string()],
            Some("Main Stage".to_owned()),
            true
        ),
        "the proved port's new address and name are not stored with its trust"
    );
}

/// Proves that the key and identifier a session advertises are kept with the machine only once
/// the port at the address connected to proves them (R-106).
///
/// An advertisement can name any address, so a forged one at a trusted machine's address would
/// otherwise plant a key for its forger to prove later from anywhere.
#[tokio::test]
async fn a_machines_key_is_kept_only_once_proved_where_it_is_connected() {
    let (front_machine, front, to_front) =
        accepting_machine("learnt-front", "Front of House").await;
    let (booth_machine, _, _) = accepting_machine("learnt-booth", "Booth").await;
    let front_of_house = PortIdentity::new(&front_machine.identity_key(), front)
        .expect("Front of House has a key and an identifier");
    let forged = PortIdentity::new(&booth_machine.identity_key(), front).expect("Booth has a key");
    let (near, stage, paths) =
        stage_connecting_to("learnt", &format!("\x20 addresses: [\"{to_front}\"]\n")).await;
    assert!(
        connected(&near, stage).await,
        "Stage connects to Front of House where it is remembered"
    );

    near.follow_advertised(&[advertisement("Front of House", to_front, Some(forged))])
        .await;
    let stored = stored_machine(&paths);
    assert_eq!(
        (stored.advertised_as.as_deref(), stored.key, stored.port_id),
        (Some("Front of House"), None, None),
        "the session's name is kept, and a key the port did not prove is not"
    );

    near.follow_advertised(&[advertisement(
        "Front of House",
        to_front,
        Some(front_of_house),
    )])
    .await;
    let stored = stored_machine(&paths);
    assert_eq!(
        (stored.key, stored.port_id),
        (Some(front_machine.identity_key()), Some(front)),
        "the key and identifier the port proved are not kept"
    );
}
