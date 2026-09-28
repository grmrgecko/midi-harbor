//! Notes are stopped before a port stops being able to stop them.
//!
//! A note on with no matching note off is the failure a musician hears, and the one they cannot
//! fix from the application that caused it: the note is sounding on a synth nothing is talking to
//! any more. Deleting or disabling a port is exactly the moment that happens.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::Direction;
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::midi::{Channel, MidiMessage, silence_channel};
use midi_harbor_core::paths::Paths;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::DiscoveredDevice;
use midi_harbor_platform::midi::{MidiPlatform, PortHandle};
use std::sync::Arc;
use std::time::Duration;

/// Starts a daemon over a fake platform in its own scratch directory.
async fn started(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root =
        common::scratch("midi-harbor-silencing").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        Paths::rooted_at(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
}

/// Builds a daemon over a fake platform with one route from Keyboard to Synth.
async fn routed(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let (daemon, platform) = started(label).await;
    for name in ["Keyboard", "Synth"] {
        daemon
            .create_virtual_port(name, 1, 1)
            .await
            .expect("the virtual port is created");
    }
    daemon
        .create_route("Keyboard", "Synth")
        .await
        .expect("the route from Keyboard to Synth is created");
    (daemon, platform)
}

/// Returns the platform handle of a named port.
fn handle_for(platform: &FakeMidiPlatform, name: &str) -> PortHandle {
    platform
        .port_handle(name)
        .unwrap_or_else(|| panic!("no platform handle for {name}"))
}

/// Returns a channel by its zero-based index.
fn channel(index: u8) -> Channel {
    Channel::new(index).expect("the channel index is in range")
}

/// Returns a note on at a fixed velocity.
fn note_on(index: u8, note: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        channel: channel(index),
        note,
        velocity: 100,
    }
}

/// Returns a note off with zero release velocity.
fn note_off(index: u8, note: u8) -> MidiMessage {
    MidiMessage::NoteOff {
        channel: channel(index),
        note,
        velocity: 0,
    }
}

/// Returns what silencing one held note on a channel nobody else plays sends: the pedal release,
/// the note's own note off, then all-notes-off and all-sound-off, four messages in all.
fn silenced(index: u8, note: u8) -> Vec<MidiMessage> {
    let [pedal, notes_off, sound_off] = silence_channel(channel(index));
    vec![pedal, note_off(index, note), notes_off, sound_off]
}

/// Waits for a condition, so the test does not depend on dispatch timing.
async fn eventually(mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Plays a message from Keyboard and waits for it to arrive at Synth.
async fn play(platform: &FakeMidiPlatform, message: MidiMessage) {
    let synth = handle_for(platform, "Synth");
    let before = platform.sent(synth).len();
    assert!(
        platform.feed(handle_for(platform, "Keyboard"), &[message]),
        "the keyboard port accepts the message fed into it"
    );
    assert!(
        eventually(|| platform.sent(synth).len() > before).await,
        "the note never reached the destination"
    );
}

/// What a test takes away from the Keyboard to Synth route.
#[derive(Clone, Copy, Debug)]
enum Withdraw {
    /// Switches off the named port.
    DisablePort(&'static str),
    /// Deletes the named port.
    DeletePort(&'static str),
    /// Switches off the only route.
    DisableRoute,
    /// Deletes the only route.
    DeleteRoute,
    /// Silences everything, as the daemon does when it stops.
    StopDaemon,
}

/// Applies one withdrawal to a daemon built by `routed`.
async fn withdraw(daemon: &Arc<Daemon>, action: Withdraw) {
    match action {
        Withdraw::DisablePort(name) => {
            let id = daemon.resolve(name).await.expect("the port resolves");
            daemon
                .set_enabled(id, false)
                .await
                .expect("the port is switched off");
        }
        Withdraw::DeletePort(name) => {
            let id = daemon.resolve(name).await.expect("the port resolves");
            daemon
                .delete_virtual_port(id)
                .await
                .expect("the port is deleted");
        }
        Withdraw::DisableRoute => {
            let route = daemon
                .router()
                .await
                .routes()
                .first()
                .expect("the daemon has the one route")
                .id;
            daemon
                .set_route_enabled(&route.to_string(), false)
                .await
                .expect("the route is switched off");
        }
        Withdraw::DeleteRoute => {
            let route = daemon
                .router()
                .await
                .routes()
                .first()
                .expect("the daemon has the one route")
                .id;
            daemon
                .delete_route(&route.to_string())
                .await
                .expect("the route is deleted");
        }
        Withdraw::StopDaemon => daemon.silence_all().await,
    }
}

/// Proves that a note left sounding at a destination is stopped whenever anything carrying it
/// goes away: the destination, the source, the route between them, or the daemon itself.
///
/// Each row plays one note from Keyboard to Synth and withdraws one thing. Synth then receives
/// the four messages of `silenced`, and only for the channel the note was on. A reset on a channel
/// nothing played would still clear sustain and cut sound for whatever else drives the port.
#[tokio::test]
async fn a_note_left_sounding_is_stopped_whenever_what_carries_it_goes_away() {
    struct Case {
        name: &'static str,
        action: Withdraw,
        channel: u8,
        note: u8,
        why: &'static str,
    }
    let cases = [
        Case {
            name: "switching off the destination",
            action: Withdraw::DisablePort("Synth"),
            channel: 0,
            note: 60,
            why: "the port is closed afterwards, so nothing can release the note later",
        },
        Case {
            name: "deleting the destination",
            action: Withdraw::DeletePort("Synth"),
            channel: 0,
            note: 60,
            why: "the port is gone afterwards, so nothing can release the note later",
        },
        Case {
            name: "switching off the source",
            action: Withdraw::DisablePort("Keyboard"),
            channel: 0,
            note: 60,
            why: "the note off would come from the source, whose routes are now suspended",
        },
        Case {
            name: "deleting the source",
            action: Withdraw::DeletePort("Keyboard"),
            channel: 0,
            note: 60,
            why: "the note off would come from the source, which no longer exists",
        },
        Case {
            name: "switching off the route",
            action: Withdraw::DisableRoute,
            channel: 0,
            note: 60,
            why: "the note off would travel the route, which no longer carries anything",
        },
        Case {
            name: "stopping the daemon",
            action: Withdraw::StopDaemon,
            channel: 0,
            note: 60,
            why: "the only thing that knew the note was playing is this process",
        },
        Case {
            name: "switching off the destination after a drum note on channel ten",
            action: Withdraw::DisablePort("Synth"),
            channel: 9,
            note: 38,
            why: "only the channel that was played is reset, never all sixteen",
        },
    ];

    for case in cases {
        let (daemon, platform) = routed(&case.name.replace(' ', "-")).await;
        let synth = handle_for(&platform, "Synth");
        play(&platform, note_on(case.channel, case.note)).await;
        let before = platform.sent(synth).len();

        withdraw(&daemon, case.action).await;

        let want = silenced(case.channel, case.note);
        eventually(|| platform.sent(synth).len() >= before + want.len()).await;
        assert_eq!(
            platform.sent(synth).split_off(before),
            want,
            "{}: the held note must be stopped because {}",
            case.name,
            case.why
        );
    }
}

/// Proves that silence is earned rather than sent on principle: withdrawing a port or route with
/// no note left sounding sends nothing at all to the destination.
///
/// Resetting a port nobody was playing clears sustain and cuts sound for whatever else is
/// connected to it.
#[tokio::test]
async fn nothing_is_sent_where_nothing_is_left_sounding() {
    struct Case {
        name: &'static str,
        played: Vec<MidiMessage>,
        action: Withdraw,
    }
    let cases = [
        Case {
            name: "an idle port switched off",
            played: vec![],
            action: Withdraw::DisablePort("Synth"),
        },
        Case {
            name: "a port whose only note was released, switched off",
            played: vec![note_on(0, 60), note_off(0, 60)],
            action: Withdraw::DisablePort("Synth"),
        },
        Case {
            name: "an idle route deleted",
            played: vec![],
            action: Withdraw::DeleteRoute,
        },
    ];

    for case in cases {
        let (daemon, platform) = routed(&case.name.replace(' ', "-")).await;
        let synth = handle_for(&platform, "Synth");
        for message in &case.played {
            play(&platform, *message).await;
        }
        let before = platform.sent(synth);

        withdraw(&daemon, case.action).await;

        assert_eq!(
            platform.sent(synth),
            before,
            "{}: the destination was disturbed although nothing was sounding",
            case.name
        );
    }
}

/// Proves that silencing leaves a `NotesSilenced` event naming the port, because a synth going
/// quiet on its own is alarming unless something says why.
#[tokio::test]
async fn silencing_is_recorded_so_it_can_be_explained_afterwards() {
    let (daemon, platform) = routed("recorded").await;
    play(&platform, note_on(0, 60)).await;

    let id = daemon.resolve("Synth").await.expect("the synth resolves");
    daemon
        .set_enabled(id, false)
        .await
        .expect("the synth is switched off");

    let events = daemon.events(None, 100).await;
    let silenced = events
        .iter()
        .find(|event| event.kind == midi_harbor_core::events::EventKind::NotesSilenced)
        .expect("the silencing was not recorded");
    assert_eq!(
        silenced.endpoint,
        Some(id),
        "the event names the port that was silenced"
    );
    assert!(
        silenced.detail.contains("Synth"),
        "the event detail names the port by name, got {}",
        silenced.detail
    );
}

/// Proves that a port disabled and enabled again starts with nothing recorded as sounding.
///
/// The record has to be cleared with the silence, or the next disable resets channels that
/// nothing has played since.
#[tokio::test]
async fn a_port_disabled_and_enabled_again_starts_quiet() {
    let (daemon, platform) = routed("reopened").await;
    play(&platform, note_on(0, 60)).await;

    let id = daemon.resolve("Synth").await.expect("the synth resolves");
    daemon
        .set_enabled(id, false)
        .await
        .expect("the synth is switched off");
    daemon
        .set_enabled(id, true)
        .await
        .expect("the synth is switched on again");

    // The port was rebuilt, so this is the handle of the reopened one.
    let reopened = handle_for(&platform, "Synth");
    daemon
        .set_enabled(id, false)
        .await
        .expect("the synth is switched off again");

    assert!(
        platform.sent(reopened).is_empty(),
        "a reopened port was reset for notes played before it was closed"
    );
}

/// Builds a daemon with a Keystation keyboard attached and the virtual ports the unplugging
/// cases route between: Pads as a second source, Synth and Sampler as destinations, and Rack with
/// two MIDI Outs.
async fn with_hardware(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let (daemon, platform) = started(label).await;
    platform.attach(DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            name: "Keystation".to_owned(),
            ..DeviceFingerprint::default()
        },
        direction: Direction::Bidirectional,
        claimed_by: None,
        software: false,
    });
    daemon.refresh_devices().await;
    for (name, outs) in [("Synth", 1), ("Pads", 1), ("Sampler", 1), ("Rack", 2)] {
        daemon
            .create_virtual_port(name, 1, outs)
            .await
            .expect("the virtual port is created");
    }
    (daemon, platform)
}

/// Plays a note from the Keystation and waits for it to leave through one MIDI Out of a port.
async fn play_from_hardware(platform: &FakeMidiPlatform, to: &str, out: u8, message: MidiMessage) {
    // Fed until it lands rather than once: the daemon re-enumerates hardware on its own
    // schedule, so the handle the fake reports can be a moment behind the one it has just opened.
    let destination = handle_for(platform, to);
    let before = platform.sent_through(destination, out).len();
    for _ in 0..40 {
        if let Some(source) = platform.device_handle("Keystation") {
            let _ = platform.feed(source, &[message]);
        }
        if platform.sent_through(destination, out).len() > before {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the note never reached {to}");
}

/// Proves FR-015f: unplugging a keyboard stops the notes it was holding, and only those.
///
/// Nobody can release them otherwise: the keyboard that would send the note off is in a bag.
/// Each row routes the Keystation to one MIDI Out and, in most rows, a second source (Pads) to
/// the same or another place. A destination can be fed by several sources, and cutting off the
/// others because one went away would turn one silent instrument into all of them. Only notes
/// leaving through the same MIDI Out share a channel with the unplugged source's route.
#[tokio::test]
async fn unplugging_a_keyboard_stops_its_own_notes_and_nobody_elses() {
    /// Where a source's note goes: its channel, its note, the port and the MIDI Out.
    type Play = (u8, u8, &'static str, u8);
    struct Case {
        name: &'static str,
        keystation: Play,
        pads: Option<Play>,
        want: Vec<MidiMessage>,
        why: &'static str,
    }
    let cases = [
        Case {
            name: "the only source",
            keystation: (0, 60, "Synth", 0),
            pads: None,
            want: silenced(0, 60),
            why: "the channel is the keyboard's alone, so it gets the full reset",
        },
        Case {
            name: "another source on another channel into the same destination",
            keystation: (0, 60, "Synth", 0),
            pads: Some((5, 72, "Synth", 0)),
            want: silenced(0, 60),
            why: "channel six keeps its note, so only channel one is reset",
        },
        Case {
            name: "another source on the same channel into the same destination",
            keystation: (9, 36, "Synth", 0),
            pads: Some((9, 38, "Synth", 0)),
            want: vec![note_off(9, 36)],
            why: "the pedal release and broad resets on a shared channel stopped the other \
                  controller's held note along with the unplugged one's",
        },
        Case {
            name: "another source on the same channel into another destination",
            keystation: (9, 36, "Synth", 0),
            pads: Some((9, 38, "Sampler", 0)),
            want: silenced(9, 36),
            why: "a note sounding at another destination does not hold back the reset",
        },
        Case {
            name: "another source on the same channel through another MIDI Out",
            keystation: (9, 36, "Rack", 0),
            pads: Some((9, 38, "Rack", 1)),
            want: silenced(9, 36),
            why: "each MIDI Out is its own cable to its own instrument, so a note through one \
                  does not share a channel with a route through another",
        },
    ];

    for case in cases {
        let (daemon, platform) = with_hardware(&case.name.replace(' ', "-")).await;
        let (channel, note, to, out) = case.keystation;
        daemon
            .create_route_through("Keystation", 0, to, out)
            .await
            .expect("the route from the Keystation is created");
        if let Some((_, _, pads_to, pads_out)) = case.pads {
            daemon
                .create_route_through("Pads", 0, pads_to, pads_out)
                .await
                .expect("the route from Pads is created");
        }

        // Play the Keystation's note, then the other source's.
        play_from_hardware(&platform, to, out, note_on(channel, note)).await;
        if let Some((pads_channel, pads_note, pads_to, pads_out)) = case.pads {
            let destination = handle_for(&platform, pads_to);
            let before = platform.sent_through(destination, pads_out).len();
            assert!(
                platform.feed(
                    handle_for(&platform, "Pads"),
                    &[note_on(pads_channel, pads_note)]
                ),
                "{}: the pads port accepts the note fed into it",
                case.name
            );
            assert!(
                eventually(|| platform.sent_through(destination, pads_out).len() > before).await,
                "{}: the other source's note never reached its destination",
                case.name
            );
        }

        // Unplug the Keystation and read what its destination was sent since.
        let destination = handle_for(&platform, to);
        let before = platform.sent_through(destination, out).len();
        platform.detach("Keystation");
        daemon.refresh_devices().await;

        assert_eq!(
            platform.sent_through(destination, out).split_off(before),
            case.want,
            "{}: unplugging the keyboard must stop exactly its own notes because {}",
            case.name,
            case.why
        );
    }
}
