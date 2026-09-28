//! Bringing a recovered link back to the controller state it was last sent (FR-027).
//!
//! A link that drops loses whatever changed while it was down, and a fresh session starts with an
//! empty recovery journal. The state machine asked for a restoration on every recovery and
//! nothing acted on it, so a volume moved during an outage stayed stale at the receiver.

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

/// Starts a daemon standing in for one machine, over a scratch directory of its own, returning
/// it with its fake platform.
async fn machine(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root = common::scratch("midi-harbor-restoration")
        .join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        common::quiet(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
}

/// Returns a channel volume change (controller 7) on channel 1.
fn volume(value: u8) -> MidiMessage {
    MidiMessage::ControlChange {
        channel: Channel::new(0).expect("channel 0 is valid"),
        controller: 7,
        value,
    }
}

/// Waits up to `within` for a condition, reporting whether it held.
async fn until(mut condition: impl FnMut() -> bool, within: Duration) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < within {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    condition()
}

/// Returns a session's connection phase, or `None` when it is not running.
async fn phase(
    daemon: &Arc<Daemon>,
    id: midi_harbor_core::ids::EndpointId,
) -> Option<ConnectionPhase> {
    daemon
        .session_status(id)
        .await
        .map(|status| status.state.phase())
}

/// Proves that a controller value sent while a network link was down reaches the peer once the
/// link recovers (FR-027, Principle I: controller state is resynchronised across reconnects).
///
/// The far session is switched off to drop the link, the fader moves to 30 while it is down, and
/// the far synth must receive 30 once the session is back. The recovery wait allows fifteen
/// seconds because the near side reconnects on its own backoff.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_control_moved_while_the_link_was_down_reaches_the_peer_when_it_returns() {
    // The far machine: a session feeding a synth.
    let (far, far_platform) = machine("far").await;
    far.create_virtual_port("Synth", 1, 1)
        .await
        .expect("the far port Synth is created");
    let incoming = far
        .create_network_session("Stage In", 0, InvitationPolicy::AcceptAll)
        .await
        .expect("the far session is created");
    far.create_route("Stage In", "Synth")
        .await
        .expect("the route from Stage In to Synth is created");
    let port = far
        .session_status(incoming.id)
        .await
        .expect("the far session is running")
        .control_port;

    // This machine: a controller routed into a session connected to the far one.
    let (near, near_platform) = machine("near").await;
    near.create_virtual_port("Faders", 1, 1)
        .await
        .expect("the near port Faders is created");
    let outgoing = near
        .create_network_session("Stage Out", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near session is created");
    near.create_route("Faders", "Stage Out")
        .await
        .expect("the route from Faders to Stage Out is created");
    near.connect_peer(outgoing.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("the near session invites the far one");
    let mut up = false;
    for _ in 0..100 {
        if phase(&near, outgoing.id).await == Some(ConnectionPhase::Connected) {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(up, "never connected");

    let faders = near_platform
        .port_handle("Faders")
        .expect("Faders has a platform port");
    let synth = far_platform
        .port_handle("Synth")
        .expect("Synth has a platform port");
    assert!(
        near_platform.feed(faders, &[volume(90)]),
        "the fake platform takes the first value fed into Faders"
    );
    assert!(
        until(
            || far_platform.sent(synth).contains(&volume(90)),
            Duration::from_secs(3)
        )
        .await,
        "the first value never arrived"
    );

    // The far side goes away, and the fader moves while it is gone.
    far.set_enabled(incoming.id, false)
        .await
        .expect("the far session is switched off");
    let mut down = false;
    for _ in 0..100 {
        if phase(&near, outgoing.id).await != Some(ConnectionPhase::Connected) {
            down = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(down, "the link never dropped");
    assert!(
        near_platform.feed(faders, &[volume(30)]),
        "the fake platform takes the value fed while the link is down"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !far_platform.sent(synth).contains(&volume(30)),
        "the value reached the peer while the link was down, so its later arrival proves nothing"
    );

    // It comes back, and the value it missed is sent to it.
    far.set_enabled(incoming.id, true)
        .await
        .expect("the far session is switched on again");
    assert!(
        until(
            || far_platform.sent(synth).contains(&volume(30)),
            Duration::from_secs(15)
        )
        .await,
        "the peer was left at the value it had before the outage: {:?}",
        far_platform.sent(synth)
    );
}
