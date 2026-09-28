//! Clients coming and going leave the daemon exactly as it was.
//!
//! The daemon exists so connections outlive the programs that manage them (FR-037). A client that
//! connects, reads, opens a stream and vanishes mid-stream is ordinary, and fifty of them must not
//! reopen a port, move a phase or lose a record (SC-014, SC-013).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::InvitationPolicy;
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::state::ConnectionPhase;
use midi_harbor_daemon::Daemon;
use midi_harbor_ipc::{HarborClient, pb, transport};
use midi_harbor_platform::fake::{FakeMidiPlatform, Injected};
use midi_harbor_platform::midi::MidiPlatform;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How many clients come and go.
const CYCLES: usize = 50;

/// Starts a daemon serving on its own socket, with two routed ports and a network session.
///
/// The socket gets a short path of its own, because a Unix socket path is limited to about a
/// hundred bytes and one under the scratch root would not fit.
async fn serving(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>, PathBuf) {
    let root =
        common::scratch("midi-harbor-clients").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let socket = common::scratch("mh-clients").join(format!("{}.sock", &unique[..8]));
    std::fs::create_dir_all(
        socket
            .parent()
            .expect("the socket path sits in a scratch directory"),
    )
    .expect("the socket's scratch directory is created");

    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        common::quiet(root).with_socket(Some(socket.clone())),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    let served = Arc::clone(&daemon);
    tokio::spawn(async move {
        let _ = midi_harbor_daemon::run(served)
            .await
            .map_err(|error| error.to_string());
    });
    for _ in 0..100 {
        if transport::probe(&socket).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (daemon, platform, socket)
}

/// Everything about the endpoints a client could disturb: phase, when it began, and whether
/// the platform handle is the same one.
async fn condition(
    daemon: &Arc<Daemon>,
) -> Vec<(EndpointId, ConnectionPhase, String, Option<u64>)> {
    let mut seen = daemon
        .read(|config, runtime| {
            config
                .endpoints
                .iter()
                .filter_map(|endpoint| {
                    let runtime = runtime.get(&endpoint.id)?;
                    Some((
                        endpoint.id,
                        runtime.state.phase(),
                        runtime.state.since().to_string(),
                        runtime.handle.map(|handle| handle.get()),
                    ))
                })
                .collect::<Vec<_>>()
        })
        .await;
    seen.sort_by_key(|entry| entry.0.to_string());
    seen
}

/// One client doing what clients do, then disappearing, sometimes in the middle of a stream.
async fn come_and_go(socket: &Path, cycle: usize, synth: &str) {
    let channel = transport::connect(socket)
        .await
        .expect("the client connects to the daemon's socket");
    let mut client = HarborClient::new(channel);
    let _ = client
        .get_status(pb::GetStatusRequest {})
        .await
        .expect("the daemon answers a status request");
    let _ = client
        .list_endpoints(pb::ListEndpointsRequest::default())
        .await
        .expect("the daemon lists its endpoints");

    // Open a stream and walk away from it without reading, in a different way each time.
    match cycle % 4 {
        0 => {
            let _stream = client
                .watch_state(pb::WatchStateRequest {})
                .await
                .expect("the state stream opens");
        }
        1 => {
            let _stream = client
                .watch_events(pb::WatchEventsRequest { after_id: None })
                .await
                .expect("the event stream opens");
        }
        2 => {
            let _stream = client
                .monitor_endpoint(pb::MonitorEndpointRequest {
                    endpoint_id: synth.to_owned(),
                })
                .await
                .expect("the monitor stream opens");
        }
        _ => {
            let _stream = client
                .watch_traffic(pb::WatchTrafficRequest::default())
                .await
                .expect("the traffic stream opens");
        }
    }
}

/// Returns a note on channel 1 at velocity 100.
fn note(n: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        channel: Channel::new(0).expect("channel 0 is valid"),
        note: n,
        velocity: 100,
    }
}

/// Proves that fifty clients connecting, reading, opening a stream and vanishing leave every
/// endpoint's phase, start time and platform handle as they were, the session on its UDP port,
/// and the route carrying MIDI (FR-037, SC-014).
///
/// The fifty cycles walk away from each of the four streams in turn, so each is abandoned
/// twelve or thirteen times.
#[tokio::test]
async fn fifty_clients_coming_and_going_change_nothing() {
    let (daemon, platform, socket) = serving("churn").await;
    for name in ["Keyboard", "Synth"] {
        daemon
            .create_virtual_port(name, 1, 1)
            .await
            .expect("the port is created");
    }
    daemon
        .create_route("Keyboard", "Synth")
        .await
        .expect("the route from Keyboard to Synth is created");
    let session = daemon
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the session is created");
    let session_port = daemon
        .session_status(session.id)
        .await
        .expect("the session is running")
        .control_port;
    let synth = daemon
        .resolve("Synth")
        .await
        .expect("Synth exists")
        .to_string();
    let before = condition(&daemon).await;

    for cycle in 0..CYCLES {
        come_and_go(&socket, cycle, &synth).await;
    }
    // Let the server notice the last client has gone.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        condition(&daemon).await,
        before,
        "a client disturbed an endpoint"
    );
    assert_eq!(
        daemon
            .session_status(session.id)
            .await
            .expect("the session is still running")
            .control_port,
        session_port,
        "a client restarted the session"
    );
    // And it still carries MIDI, which is the point of it staying up.
    let keyboard = platform
        .port_handle("Keyboard")
        .expect("Keyboard has a platform port");
    let synth_handle = platform
        .port_handle("Synth")
        .expect("Synth has a platform port");
    assert!(
        platform.feed(keyboard, &[note(64)]),
        "the fake platform takes the note fed into Keyboard"
    );
    let mut carried = false;
    for _ in 0..40 {
        if platform.sent(synth_handle).contains(&note(64)) {
            carried = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(carried, "the route stopped carrying MIDI");
}

/// Proves that a failure and its recovery stay in the daemon's history after fifty clients have
/// come and gone (SC-013).
///
/// The history is the daemon's, not a client's: a failure that happened while nobody was
/// watching is exactly the one the user needs to read about afterwards.
#[tokio::test]
async fn failures_and_recoveries_are_still_there_after_every_client_has_gone() {
    let (daemon, platform, socket) = serving("history").await;
    let port = daemon
        .create_virtual_port("Synth", 1, 1)
        .await
        .expect("the port Synth is created");
    // Refused on the way back on, so the retry loop has something to recover.
    daemon
        .set_enabled(port.id, false)
        .await
        .expect("Synth is switched off");
    platform.inject(Some(Injected::ResourceLimit));
    daemon
        .set_enabled(port.id, true)
        .await
        .expect("Synth is switched on again");
    // Wait for the retry loop to recover it, which records the recovery.
    for _ in 0..60 {
        let connected = daemon
            .read(|_, runtime| runtime.get(&port.id).map(|r| r.state.phase()))
            .await
            == Some(ConnectionPhase::Connected);
        if connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    for cycle in 0..CYCLES {
        come_and_go(&socket, cycle, &port.id.to_string()).await;
    }

    let channel = transport::connect(&socket)
        .await
        .expect("a last client connects to the daemon's socket");
    let mut client = HarborClient::new(channel);
    let events = client
        .list_events(pb::ListEventsRequest {
            limit: 500,
            ..Default::default()
        })
        .await
        .expect("the daemon lists its history")
        .into_inner()
        .events;
    let details: Vec<&str> = events.iter().map(|event| event.detail.as_str()).collect();
    assert!(
        details
            .iter()
            .any(|detail| detail.starts_with("Synth could not open")),
        "the failure is gone from the history: {details:?}"
    );
    assert!(
        details
            .iter()
            .any(|detail| detail.ends_with("Synth connected after 1 failed attempt")),
        "the recovery is gone from the history: {details:?}"
    );
}

/// Proves that a session's state reads the same on the state stream, in a listing, and in the
/// reply to a change.
///
/// Regression: a client keeps a cache fed by WatchState and resyncs with ListEndpoints. The
/// stream built sessions without the state their supervisor holds, so a client patching its
/// cache from it saw every session lose its state.
#[tokio::test]
async fn a_session_reads_the_same_on_the_stream_as_in_a_listing() {
    let (daemon, _platform, socket) = serving("stream-state").await;
    let session = daemon
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the session is created");
    let mut client = HarborClient::new(
        transport::connect(&socket)
            .await
            .expect("the client connects to the daemon's socket"),
    );
    let mut stream = client
        .watch_state(pb::WatchStateRequest {})
        .await
        .expect("the state stream opens")
        .into_inner();

    // Change something, so the session appears on the stream.
    let reply = client
        .set_endpoint_enabled(pb::SetEndpointEnabledRequest {
            id: session.id.to_string(),
            enabled: true,
        })
        .await
        .expect("the daemon enables the session")
        .into_inner();
    let streamed = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .expect("an event in time")
            .expect("the stream delivers without error")
            .expect("the stream stays open");
        if let Some(pb::state_event::Change::EndpointChanged(endpoint)) = event.change
            && endpoint.id == session.id.to_string()
        {
            break endpoint;
        }
    };
    let listed = client
        .list_endpoints(pb::ListEndpointsRequest::default())
        .await
        .expect("the daemon lists its endpoints")
        .into_inner()
        .endpoints
        .into_iter()
        .find(|endpoint| endpoint.id == session.id.to_string())
        .expect("the session is listed");

    let phase = |endpoint: &pb::Endpoint| endpoint.state.as_ref().map(|state| state.phase);
    assert!(phase(&listed).is_some(), "the listing has no state either");
    assert_eq!(
        phase(&streamed),
        phase(&listed),
        "the stream disagrees with the listing"
    );
    assert_eq!(
        phase(&reply),
        phase(&listed),
        "the reply disagrees with the listing"
    );
}

/// Starts a daemon with one network port accepting everyone, returning its UDP port.
async fn accepting(label: &str, name: &str) -> (Arc<Daemon>, u16) {
    let root =
        common::scratch("midi-harbor-clients").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let daemon = Daemon::start(
        common::quiet(root),
        Arc::new(FakeMidiPlatform::new()) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the far daemon starts over a scratch directory");
    let port = daemon
        .create_network_session(name, 0, InvitationPolicy::AcceptAll)
        .await
        .expect("the far network port is created");
    let control = daemon
        .session_status(port.id)
        .await
        .expect("the far network port is listening")
        .control_port;
    (daemon, control)
}

/// Proves that the gRPC contract connects a second machine alongside the peer, lists both with
/// their names and how they joined, disconnects one by address, and refuses an address that is
/// not one with InvalidArgument, the code a client switches on.
#[tokio::test]
async fn a_client_connects_a_second_machine_alongside_and_disconnects_one() {
    let (daemon, _platform, socket) = serving("machines").await;
    let stage = daemon
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the network port Stage is created");
    let (_front, front_port) = accepting("front", "Front of House").await;
    let (_booth, booth_port) = accepting("booth", "Booth").await;
    let mut client = HarborClient::new(
        transport::connect(&socket)
            .await
            .expect("the client connects to the daemon's socket"),
    );
    let connect = |address: String, alongside: bool| pb::ConnectPeerRequest {
        session_endpoint_id: stage.id.to_string(),
        peer_id: address,
        alongside,
    };
    let (front, booth) = (
        format!("127.0.0.1:{front_port}"),
        format!("127.0.0.1:{booth_port}"),
    );

    client
        .connect_peer(connect(front.clone(), false))
        .await
        .expect("the client connects Stage to Front of House");
    client
        .connect_peer(connect(booth.clone(), true))
        .await
        .expect("the client connects Booth alongside");

    // Both are listed, joined, the peer first.
    let mut machines = Vec::new();
    for _ in 0..200 {
        machines = client
            .list_endpoints(pb::ListEndpointsRequest::default())
            .await
            .expect("the daemon lists its endpoints")
            .into_inner()
            .endpoints
            .into_iter()
            .find(|endpoint| endpoint.id == stage.id.to_string())
            .and_then(|endpoint| match endpoint.detail {
                Some(pb::endpoint::Detail::NetworkSession(detail)) => Some(detail.machines),
                _ => None,
            })
            .unwrap_or_default();
        if machines.len() == 2 && machines.iter().all(|machine| machine.joined) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        machines
            .iter()
            .map(|machine| machine.address.clone())
            .collect::<Vec<_>>(),
        vec![front.clone(), booth.clone()],
        "both machines are listed, the peer first: {machines:?}"
    );
    assert_eq!(
        machines[1].name.as_deref(),
        Some("Booth"),
        "the machine alongside is listed under the name it advertises"
    );
    assert!(
        machines.iter().all(|machine| machine.invited),
        "machines this side connected to are listed as invited, not as guests: {machines:?}"
    );

    let reply = client
        .disconnect_machine(pb::DisconnectMachineRequest {
            session_endpoint_id: stage.id.to_string(),
            address: booth.clone(),
        })
        .await
        .expect("the client disconnects Booth")
        .into_inner();
    let left = match reply.detail {
        Some(pb::endpoint::Detail::NetworkSession(detail)) => detail.machines,
        _ => Vec::new(),
    };
    assert_eq!(
        left.iter()
            .map(|machine| machine.address.clone())
            .collect::<Vec<_>>(),
        vec![front],
        "disconnecting Booth leaves only the peer, as the reply reports"
    );

    let refused = client
        .disconnect_machine(pb::DisconnectMachineRequest {
            session_endpoint_id: stage.id.to_string(),
            address: "not an address".to_owned(),
        })
        .await
        .expect_err("a nonsense address was accepted");
    assert_eq!(
        refused.code(),
        tonic::Code::InvalidArgument,
        "a malformed address is the client's mistake, and says so in its code"
    );
}

/// Proves that the gRPC contract adds a machine untrusted, switches its trust on and off by
/// identifier or by name, reports the change both in its reply and in the listing, and treats an
/// unset trust as trusted.
///
/// An unset trust keeps what adding a machine meant before the field existed: it is let in
/// without asking.
#[tokio::test]
async fn a_client_adds_a_machine_untrusted_and_then_trusts_it() {
    let (_daemon, _platform, socket) = serving("trust").await;
    let mut client = HarborClient::new(
        transport::connect(&socket)
            .await
            .expect("the client connects to the daemon's socket"),
    );
    let trusted_now = |client: HarborClient<_>| async move {
        let mut client = client;
        client
            .list_peers(pb::ListPeersRequest::default())
            .await
            .expect("the daemon lists its known machines")
            .into_inner()
            .peers
            .into_iter()
            .find(|peer| peer.advertised_name == "Studio PC")
            .map(|peer| peer.trusted)
    };

    let added = client
        .add_manual_peer(pb::AddManualPeerRequest {
            address: "192.0.2.13".to_owned(),
            port: 5004,
            name: Some("Studio PC".to_owned()),
            trusted: Some(false),
        })
        .await
        .expect("the client adds Studio PC untrusted")
        .into_inner();
    assert!(
        !added.trusted,
        "a machine added untrusted is reported trusted"
    );
    assert_eq!(
        trusted_now(client.clone()).await,
        Some(false),
        "a machine added untrusted is listed as trusted"
    );

    let changed = client
        .set_peer_trusted(pb::SetPeerTrustedRequest {
            peer_id: added.id.clone(),
            trusted: true,
        })
        .await
        .expect("the client trusts Studio PC by identifier")
        .into_inner();
    assert!(changed.trusted, "trusting a machine is not reported");
    assert_eq!(
        trusted_now(client.clone()).await,
        Some(true),
        "trusting a machine is not listed"
    );

    let untrusted = client
        .set_peer_trusted(pb::SetPeerTrustedRequest {
            peer_id: "Studio PC".to_owned(),
            trusted: false,
        })
        .await
        .expect("the client takes Studio PC's trust away by name")
        .into_inner();
    assert!(!untrusted.trusted, "taking trust away is not reported");
    assert_eq!(
        trusted_now(client.clone()).await,
        Some(false),
        "taking trust away is not listed"
    );

    let unset = client
        .add_manual_peer(pb::AddManualPeerRequest {
            address: "192.0.2.14".to_owned(),
            port: 0,
            name: None,
            trusted: None,
        })
        .await
        .expect("the client adds a machine without saying whether to trust it")
        .into_inner();
    assert!(
        unset.trusted,
        "a machine added without saying is not trusted, as adding one always meant"
    );
}

/// Proves that the contract reports a network port's automatic port's traffic beside the
/// network port's own, with when each way last carried a message.
///
/// Asking whether a cue reached this computer took two answers the contract did not give: what
/// the network carried, and what the automatic port passed between it and the applications here.
/// A note an application sends into Stage's automatic port is received there and sent over the
/// network, so the automatic port has received at least as many as the network port sent. Nothing
/// travels the other way, so the network port's last-received time and the automatic port's
/// last-sent time stay empty, which the single last-activity time could not say.
#[tokio::test]
async fn a_network_port_reports_its_automatic_ports_traffic_and_when_each_way_carried() {
    let (daemon, platform, socket) = serving("automatic-traffic").await;
    let stage = daemon
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the network port Stage is created");
    let (_front, front_port) = accepting("front-traffic", "Front of House").await;
    daemon
        .connect_peer(
            stage.id,
            std::net::SocketAddr::from(([127, 0, 0, 1], front_port)),
        )
        .await
        .expect("Stage invites Front of House");
    let automatic = platform
        .port_handle("Stage")
        .expect("Stage has an automatic port");
    let mut client = HarborClient::new(
        transport::connect(&socket)
            .await
            .expect("the client connects to the daemon's socket"),
    );

    // Send until the network carries one, since the connection may still be settling.
    let note = MidiMessage::NoteOn {
        channel: Channel::new(0).expect("channel 0 is valid"),
        note: 60,
        velocity: 100,
    };
    let mut listed = None;
    for _ in 0..200 {
        platform.feed(automatic, &[note]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let endpoint = client
            .list_endpoints(pb::ListEndpointsRequest::default())
            .await
            .expect("the daemon lists its endpoints")
            .into_inner()
            .endpoints
            .into_iter()
            .find(|endpoint| endpoint.id == stage.id.to_string())
            .expect("Stage is listed");
        if endpoint
            .counters
            .as_ref()
            .is_some_and(|counters| counters.messages_sent > 0)
        {
            listed = Some(endpoint);
            break;
        }
    }
    let listed = listed.expect("the network carried a note sent into Stage's automatic port");

    let network = listed.counters.expect("Stage reports its counters");
    let Some(pb::endpoint::Detail::NetworkSession(detail)) = listed.detail else {
        panic!("Stage is listed as a network port");
    };
    let automatic = detail
        .automatic_port_counters
        .expect("Stage reports its automatic port's counters while it is open");
    assert!(
        automatic.messages_received >= network.messages_sent,
        "every note the network carried was received from an application first: {} received, \
         {} sent",
        automatic.messages_received,
        network.messages_sent
    );
    assert!(
        automatic.last_received.is_some() && network.last_sent.is_some(),
        "the automatic port must say when it last received and the network port when it last sent"
    );
    assert!(
        network.last_received.is_none() && automatic.last_sent.is_none(),
        "nothing came back from Front of House, so neither last time the other way may be set"
    );
}

/// Proves that a test note sent through the contract leaves the port as a note-on, shows to a
/// monitor as leaving, and is followed by its note-off without the client asking.
///
/// The note-off comes from the daemon so a window closed, or a command stopped, in the middle of
/// the note cannot leave it sounding on whatever listens to the port.
#[tokio::test]
async fn a_test_note_leaves_the_port_and_its_note_off_follows() {
    let (daemon, platform, socket) = serving("test-note").await;
    daemon
        .create_virtual_port("Cue Bus", 1, 1)
        .await
        .expect("the port Cue Bus is created");
    let port = platform
        .port_handle("Cue Bus")
        .expect("Cue Bus has a platform handle");
    let mut client = HarborClient::new(
        transport::connect(&socket)
            .await
            .expect("the client connects to the daemon's socket"),
    );
    let mut watching = client
        .monitor_endpoint(pb::MonitorEndpointRequest {
            endpoint_id: "Cue Bus".to_owned(),
        })
        .await
        .expect("the client watches Cue Bus")
        .into_inner();

    client
        .send_test_note(pb::SendTestNoteRequest {
            endpoint_id: "Cue Bus".to_owned(),
            channel: 3,
            note: 64,
            velocity: 90,
            length_ms: Some(50),
        })
        .await
        .expect("the daemon sends a test note out of Cue Bus");
    // A client gone before the note-off must not stop it being sent.
    drop(client);

    let channel = Channel::new(2).expect("channel 3 is valid");
    let on = MidiMessage::NoteOn {
        channel,
        note: 64,
        velocity: 90,
    };
    let off = MidiMessage::NoteOff {
        channel,
        note: 64,
        velocity: 0,
    };
    let mut sent = Vec::new();
    for _ in 0..100 {
        sent = platform.sent(port);
        if sent.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        sent,
        vec![on, off],
        "the note-on must leave Cue Bus, followed by its note-off and nothing else"
    );
    let seen = tokio::time::timeout(Duration::from_secs(2), watching.message())
        .await
        .expect("the monitor reports the note within two seconds")
        .expect("the monitor stream stays open")
        .expect("the monitor reports a message");
    assert!(
        seen.outbound && seen.data == vec![0x92, 64, 90],
        "a monitor must show the test note leaving Cue Bus: {seen:?}"
    );
}

/// Proves which test notes the contract refuses, and with which code: numbers outside MIDI's
/// ranges as InvalidArgument, each beside the last value accepted, and an endpoint nothing can be
/// sent out of, or one switched off, as FailedPrecondition.
///
/// MIDI 1.0 has channels 1 to 16 and notes and velocities 0 to 127, and a note-on at velocity 0
/// is a note-off, so the lowest velocity accepted is 1. A MIDI In-only device takes nothing from
/// the computer.
#[tokio::test]
async fn a_test_note_is_refused_outside_midis_ranges_and_where_nothing_can_be_sent() {
    let (daemon, platform, socket) = serving("test-note-refused").await;
    daemon
        .create_virtual_port("Cue Bus", 1, 1)
        .await
        .expect("the port Cue Bus is created");
    let off = daemon
        .create_virtual_port("Spare Bus", 1, 1)
        .await
        .expect("the port Spare Bus is created");
    daemon
        .set_enabled(off.id, false)
        .await
        .expect("Spare Bus is switched off");
    platform.attach(midi_harbor_platform::midi::DiscoveredDevice {
        fingerprint: midi_harbor_core::fingerprint::DeviceFingerprint {
            unique_id: Some(4242),
            name: "Pad Controller".to_owned(),
            ..Default::default()
        },
        direction: midi_harbor_core::endpoint::Direction::Input,
        claimed_by: None,
        software: false,
    });
    daemon.refresh_devices().await;
    let mut client = HarborClient::new(
        transport::connect(&socket)
            .await
            .expect("the client connects to the daemon's socket"),
    );

    struct Case {
        name: &'static str,
        endpoint: &'static str,
        channel: u32,
        note: u32,
        velocity: u32,
        length_ms: Option<u32>,
        want: tonic::Code,
    }
    let case = |name, endpoint, channel, note, velocity, length_ms, want| Case {
        name,
        endpoint,
        channel,
        note,
        velocity,
        length_ms,
        want,
    };
    use tonic::Code::{FailedPrecondition, InvalidArgument, Ok};
    let cases = [
        case(
            "the highest of each",
            "Cue Bus",
            16,
            127,
            127,
            Some(10_000),
            Ok,
        ),
        case("the lowest of each", "Cue Bus", 1, 0, 1, Some(1), Ok),
        case("channel 0", "Cue Bus", 0, 60, 100, None, InvalidArgument),
        case("channel 17", "Cue Bus", 17, 60, 100, None, InvalidArgument),
        case("note 128", "Cue Bus", 1, 128, 100, None, InvalidArgument),
        case("velocity 0", "Cue Bus", 1, 60, 0, None, InvalidArgument),
        case("velocity 128", "Cue Bus", 1, 60, 128, None, InvalidArgument),
        case("no length", "Cue Bus", 1, 60, 100, Some(0), InvalidArgument),
        case(
            "a length over ten seconds",
            "Cue Bus",
            1,
            60,
            100,
            Some(10_001),
            InvalidArgument,
        ),
        case(
            "a MIDI In-only device",
            "Pad Controller",
            1,
            60,
            100,
            None,
            FailedPrecondition,
        ),
        case(
            "a port switched off",
            "Spare Bus",
            1,
            60,
            100,
            None,
            FailedPrecondition,
        ),
    ];
    for case in cases {
        let code = match client
            .send_test_note(pb::SendTestNoteRequest {
                endpoint_id: case.endpoint.to_owned(),
                channel: case.channel,
                note: case.note,
                velocity: case.velocity,
                length_ms: case.length_ms,
            })
            .await
        {
            std::result::Result::Ok(_) => Ok,
            Err(status) => status.code(),
        };
        assert_eq!(code, case.want, "{}: the wrong answer", case.name);
    }
}
