//! A physical keyboard on one machine, playing a synth on another over a network session (SC-010c).
//!
//! Two daemons in one process stand in for the two machines, joined by a real RTP-MIDI session
//! over loopback. Unplugging the keyboard mid-note must stop the note on the far side: the keyboard
//! that would send the note off is gone, and the only thing that knew the note was held is the
//! route on this side.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::{Direction, InvitationPolicy};
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::state::ConnectionPhase;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::{DiscoveredDevice, MidiPlatform};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Starts a daemon standing in for one machine, over a scratch directory of its own, returning
/// it with its fake platform.
async fn machine(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root =
        common::scratch("midi-harbor-repeater").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        common::quiet(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
}

/// Returns the hardware keyboard, as the platform reports it when plugged in.
fn keystation() -> DiscoveredDevice {
    DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            name: "Keystation".to_owned(),
            unique_id: Some(0x4B53),
            ..DeviceFingerprint::default()
        },
        direction: Direction::Bidirectional,
        claimed_by: None,
        software: false,
    }
}

/// Returns a note on channel 1 at velocity 100.
fn note_on(note: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        channel: Channel::new(0).expect("channel 0 is valid"),
        note,
        velocity: 100,
    }
}

/// Waits for a condition, giving up after `limit`.
async fn within(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < limit {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Reports whether anything received would stop a note held on channel 1.
fn released(received: &[MidiMessage], note: u8) -> bool {
    received.iter().any(|message| match message {
        MidiMessage::NoteOff { note: n, .. } => *n == note,
        MidiMessage::NoteOn {
            note: n, velocity, ..
        } => *n == note && *velocity == 0,
        // All notes off, or all sound off.
        MidiMessage::ControlChange { controller, .. } => *controller == 123 || *controller == 120,
        _ => false,
    })
}

/// Proves that unplugging a keyboard mid-note stops the note on the synth of another machine it
/// was playing over a network session, and that the keyboard plugged back in plays over the same
/// session again (SC-010c, Principle I). The release must come from the near route, as the
/// module documentation explains.
#[tokio::test]
async fn unplugging_the_keyboard_stops_its_note_on_the_other_machine() {
    // The far machine: a session that accepts the near one, feeding a synth.
    let (far, far_platform) = machine("far").await;
    let synth = far
        .create_virtual_port("Synth", 1, 1)
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

    // The near machine: a keyboard routed into a session connected to the far one.
    let (near, near_platform) = machine("near").await;
    let outgoing = near
        .create_network_session("Stage Out", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near session is created");
    near_platform.attach(keystation());
    near.refresh_devices().await;
    near.create_route("Keystation", "Stage Out")
        .await
        .expect("the route from Keystation to Stage Out is created");
    near.connect_peer(outgoing.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("the near session invites the far one");
    let mut connected = false;
    for _ in 0..100 {
        let phase = near
            .session_status(outgoing.id)
            .await
            .map(|s| s.state.phase());
        if phase == Some(ConnectionPhase::Connected) {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(connected, "the two machines never connected");
    let synth_handle = far_platform
        .port_handle(synth.name.as_str())
        .expect("Synth has a platform port");

    // A note is held on the far synth.
    let keys = near_platform
        .device_handle("Keystation")
        .expect("the plugged-in keyboard has a platform handle");
    assert!(
        near_platform.feed(keys, &[note_on(60)]),
        "the fake platform takes the note played on the keyboard"
    );
    assert!(
        within(Duration::from_secs(5), || far_platform
            .sent(synth_handle)
            .contains(&note_on(60)))
        .await,
        "the note never crossed the network"
    );

    // The keyboard is unplugged before it can release the note.
    near_platform.detach("Keystation");
    near.refresh_devices().await;
    assert!(
        within(Duration::from_secs(5), || released(
            &far_platform.sent(synth_handle),
            60
        ))
        .await,
        "the note is still sounding on the other machine: {:?}",
        far_platform.sent(synth_handle)
    );

    // Plugged back in, it plays again, over the same session.
    near_platform.attach(keystation());
    near.refresh_devices().await;
    let keys = near_platform
        .device_handle("Keystation")
        .expect("the replugged keyboard has a platform handle");
    assert!(
        near_platform.feed(keys, &[note_on(62)]),
        "the fake platform takes the note played on the replugged keyboard"
    );
    assert!(
        within(Duration::from_secs(5), || far_platform
            .sent(synth_handle)
            .contains(&note_on(62)))
        .await,
        "the replugged keyboard does not reach the other machine"
    );
}
