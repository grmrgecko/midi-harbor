//! A session carrying more than one machine (the simultaneous-invitations edge case).
//!
//! A session held one peer and refused anyone else who invited it, so of two machines inviting
//! at once, one was turned away. Machines let in beside the peer are guests: each carries the
//! same MIDI, and one leaving does not disturb the others.

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
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::MidiPlatform;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// One machine: its daemon, its fake platform and its session.
struct Machine {
    daemon: Arc<Daemon>,
    platform: Arc<FakeMidiPlatform>,
    session: EndpointId,
}

/// A machine with a session routed both ways to a virtual port called "Keys".
async fn machine(label: &str, policy: InvitationPolicy) -> Machine {
    let root =
        common::scratch("midi-harbor-guests").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        common::quiet(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    daemon
        .create_virtual_port("Keys", 1, 1)
        .await
        .expect("the port Keys is created");
    let session = daemon
        .create_network_session(label, 0, policy)
        .await
        .expect("the machine's session is created")
        .id;
    daemon
        .create_route("Keys", label)
        .await
        .expect("the route from Keys to the session is created");
    daemon
        .create_route(label, "Keys")
        .await
        .expect("the route from the session to Keys is created");
    Machine {
        daemon,
        platform,
        session,
    }
}

/// Returns a note on channel 1 at velocity 100.
fn note(number: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        channel: Channel::new(0).expect("channel 0 is valid"),
        note: number,
        velocity: 100,
    }
}

/// Waits up to five seconds for a condition, reporting whether it held.
async fn until(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Waits up to five seconds for a machine's session to be connected, reporting whether it was.
async fn connected(machine: &Machine) -> bool {
    for _ in 0..100 {
        let phase = machine
            .daemon
            .session_status(machine.session)
            .await
            .map(|status| status.state.phase());
        if phase == Some(ConnectionPhase::Connected) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

impl Machine {
    /// Plays a note into this machine's Keys.
    fn play(&self, number: u8) {
        let keys = self
            .platform
            .port_handle("Keys")
            .expect("Keys has a platform port");
        assert!(
            self.platform.feed(keys, &[note(number)]),
            "the fake platform takes the note fed into Keys"
        );
    }

    /// Waits for a note to come out of this machine's Keys, reporting whether it did.
    async fn hears(&self, number: u8) -> bool {
        let keys = self
            .platform
            .port_handle("Keys")
            .expect("Keys has a platform port");
        until(|| self.platform.sent(keys).contains(&note(number))).await
    }
}

/// Proves that two machines inviting one session are both let in and carried both ways, and that
/// the peer leaving hands the session to the remaining guest, which goes on being carried with the
/// session still reading as connected.
///
/// Regression: a session held one peer and refused anyone else who invited it, so of two
/// machines inviting at once, one was turned away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_machines_inviting_one_session_are_both_carried() {
    let hub = machine("Hub", InvitationPolicy::AcceptAll).await;
    let port = hub
        .daemon
        .session_status(hub.session)
        .await
        .expect("the hub's session is running")
        .control_port;
    let hub_address = SocketAddr::from(([127, 0, 0, 1], port));
    let first = machine("First", InvitationPolicy::Prompt).await;
    let second = machine("Second", InvitationPolicy::Prompt).await;
    // The first is the hub's peer, so the second is let in beside it as a guest.
    first
        .daemon
        .connect_peer(first.session, hub_address)
        .await
        .expect("the first machine invites the hub");
    assert!(connected(&first).await, "the first machine never connected");
    second
        .daemon
        .connect_peer(second.session, hub_address)
        .await
        .expect("the second machine invites the hub");
    assert!(
        connected(&second).await,
        "the second machine was turned away"
    );

    // The hub says who it carries beside its peer.
    let guests = |hub: Arc<Daemon>, id| async move {
        hub.session_status(id)
            .await
            .map(|status| status.guests)
            .unwrap_or_default()
    };
    assert!(
        until_async(
            || guests(Arc::clone(&hub.daemon), hub.session),
            |listed| *listed == vec!["Second".to_owned()]
        )
        .await,
        "the hub did not list the second machine"
    );

    // Both reach the hub, and the hub reaches both.
    first.play(60);
    second.play(62);
    assert!(
        hub.hears(60).await,
        "the first machine's note never arrived"
    );
    assert!(
        hub.hears(62).await,
        "the second machine's note never arrived"
    );
    hub.play(64);
    assert!(
        first.hears(64).await,
        "the hub's note never reached the first"
    );
    assert!(
        second.hears(64).await,
        "the hub's note never reached the second"
    );

    // The peer leaving hands the session to the guest, which goes on being carried.
    first
        .daemon
        .disconnect_peer(first.session)
        .await
        .expect("the first machine leaves the hub");
    tokio::time::sleep(Duration::from_millis(500)).await;
    hub.play(66);
    assert!(
        second.hears(66).await,
        "the remaining machine stopped being carried when the other left"
    );
    second.play(68);
    assert!(
        hub.hears(68).await,
        "the remaining machine's note never reached the hub after the other left"
    );
    let hub_phase = hub
        .daemon
        .session_status(hub.session)
        .await
        .map(|status| status.state.phase());
    assert_eq!(
        hub_phase,
        Some(ConnectionPhase::Connected),
        "the hub reads as listening while it carries a machine"
    );
    assert!(
        guests(Arc::clone(&hub.daemon), hub.session)
            .await
            .is_empty(),
        "the machine that took the peer's place is still listed beside it"
    );
}

/// Polls an asynchronous reading for up to five seconds until it satisfies `wanted`.
async fn until_async<T, F, Fut>(mut read: F, wanted: impl Fn(&T) -> bool) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    for _ in 0..100 {
        if wanted(&read().await) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}
