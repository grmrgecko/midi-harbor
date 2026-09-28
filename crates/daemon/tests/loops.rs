//! Loops across machines, caught where they close (FR-033).
//!
//! A loop within one machine shows in its route graph. One across machines does not: each
//! machine's routes look sensible, and a note goes round for as long as they run. RTP-MIDI
//! carries nothing to say where a message began, so a route from one session to another watches
//! for MIDI it sent out moments ago coming straight back.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::InvitationPolicy;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::state::ConnectionPhase;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::MidiPlatform;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Starts a daemon whose sessions are not announced, standing in for one machine.
async fn machine(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root =
        common::scratch("midi-harbor-loops").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        common::quiet(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
}

/// Creates a session that accepts anyone, returning its port.
async fn listening(daemon: &Arc<Daemon>, name: &str) -> u16 {
    let session = daemon
        .create_network_session(name, 0, InvitationPolicy::AcceptAll)
        .await
        .unwrap_or_else(|error| panic!("the session {name} is created: {error}"));
    daemon
        .session_status(session.id)
        .await
        .unwrap_or_else(|| panic!("the session {name} reports its status"))
        .control_port
}

/// Creates a session and connects it to a port on this machine, waiting until it is up.
async fn connected(daemon: &Arc<Daemon>, name: &str, port: u16) {
    let session = daemon
        .create_network_session(name, 0, InvitationPolicy::Prompt)
        .await
        .unwrap_or_else(|error| panic!("the session {name} is created: {error}"));
    daemon
        .connect_peer(session.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .unwrap_or_else(|error| panic!("the session {name} invites port {port}: {error}"));
    // Up to SC-003's ten seconds: under the whole suite's load, five was once not enough.
    for _ in 0..200 {
        let phase = daemon
            .session_status(session.id)
            .await
            .map(|status| status.state.phase());
        if phase == Some(ConnectionPhase::Connected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{name} never connected");
}

/// Reports whether the route from `from` to `to` exists and is switched on.
async fn route_enabled(daemon: &Arc<Daemon>, from: &str, to: &str) -> bool {
    daemon
        .read(|config, _| {
            config
                .routes
                .iter()
                .find(|route| route.from == from && route.to == to)
                .is_some_and(|route| route.enabled)
        })
        .await
}

/// Returns a note-on for `value` on the first channel.
fn note(value: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        channel: Channel::new(0).expect("channel 0 is a valid channel"),
        note: value,
        velocity: 100,
    }
}

/// Proves that a loop across two machines is switched off at the route that closes it, leaves
/// the keyboard's own route on, and is explained in the history (FR-033).
///
/// Machine A plays a keyboard into A1, which reaches B1 on machine B. B routes B1 into B2, which
/// reaches A2 back on A, and A routes A2 into A1. Every route is sensible on its own, so only
/// MIDI coming straight back reveals the loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loop_across_two_machines_is_switched_off_where_it_closes() {
    let (a, a_platform) = machine("a").await;
    let (b, _) = machine("b").await;
    let b1 = listening(&b, "B1").await;
    let a2 = listening(&a, "A2").await;
    a.create_virtual_port("Keys", 1, 1)
        .await
        .expect("machine A's Keys port is created");
    connected(&a, "A1", b1).await;
    connected(&b, "B2", a2).await;
    a.create_route("Keys", "A1")
        .await
        .expect("the route from Keys to A1 is created");
    a.create_route("A2", "A1")
        .await
        .expect("the route from A2 to A1 is created");
    b.create_route("B1", "B2")
        .await
        .expect("the route from B1 to B2 is created");

    let keys = a_platform
        .port_handle("Keys")
        .expect("machine A's Keys port is open");
    assert!(
        a_platform.feed(keys, &[note(60)]),
        "the fake accepts MIDI fed into an open port"
    );

    let mut stopped = false;
    for _ in 0..60 {
        if !route_enabled(&a, "A2", "A1").await {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        stopped,
        "the route closing the loop was left carrying it round"
    );
    assert!(
        route_enabled(&a, "Keys", "A1").await,
        "the keyboard's own route was switched off"
    );

    // The reason is recorded just after the route is switched off, so it is waited for too: on
    // the slower Windows machine the history was read in between.
    let mut explained = false;
    for _ in 0..60 {
        explained = a
            .events(None, 200)
            .await
            .iter()
            .any(|event| event.detail.contains("closed a loop across machines"));
        if explained {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(explained, "the loop was not explained in the history");
}

/// Proves that a machine relaying one session on to another is not taken for a loop, and that
/// the relay carries nearly every note.
///
/// Machine A relays B's session on to C. B plays the same note over and over, fast, which is what
/// a relay carries all the time and what a naive echo check would mistake for MIDI coming back.
/// Fifty notes are played and at least forty must arrive, so a relay switched off partway through
/// fails the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relaying_one_machine_to_another_is_not_a_loop() {
    let (a, _) = machine("relay").await;
    let (b, b_platform) = machine("player").await;
    let (c, c_platform) = machine("listener").await;
    let into_a = listening(&a, "From B").await;
    let into_c = listening(&c, "From A").await;
    c.create_virtual_port("Synth", 1, 1)
        .await
        .expect("machine C's Synth port is created");
    c.create_route("From A", "Synth")
        .await
        .expect("the route from From A to Synth is created");
    connected(&a, "To C", into_c).await;
    a.create_route("From B", "To C")
        .await
        .expect("the relaying route from From B to To C is created");
    b.create_virtual_port("Keys", 1, 1)
        .await
        .expect("machine B's Keys port is created");
    connected(&b, "To A", into_a).await;
    b.create_route("Keys", "To A")
        .await
        .expect("the route from Keys to To A is created");

    let keys = b_platform
        .port_handle("Keys")
        .expect("machine B's Keys port is open");
    for _ in 0..50 {
        assert!(
            b_platform.feed(keys, &[note(36)]),
            "the fake accepts MIDI fed into an open port"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(
        route_enabled(&a, "From B", "To C").await,
        "a relay was taken for a loop"
    );
    let synth = c_platform
        .port_handle("Synth")
        .expect("machine C's Synth port is open");
    let heard = c_platform
        .sent(synth)
        .iter()
        .filter(|m| **m == note(36))
        .count();
    assert!(heard >= 40, "only {heard} of 50 notes reached the far end");
}
