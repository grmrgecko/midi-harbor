//! Releasing held notes and ending sessions before the machine goes to sleep (FR-026).
//!
//! A machine that sleeps with a note held on another machine leaves it sounding there until the
//! far side's liveness check gives up, which is more than half a minute. A platform that says a
//! suspend is coming gives the daemon the chance to release it first, and to tell the far side
//! the session is over so it waits rather than chasing a sleeping machine.

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
use midi_harbor_platform::fake::{FakeMidiPlatform, FakeSystemEvents};
use midi_harbor_platform::midi::MidiPlatform;
use midi_harbor_platform::sysevents::{SystemEvent, SystemEvents};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// NOTE is the key held down through each sleep.
const NOTE: u8 = 60;

/// Starts a daemon over a scratch directory, with machine events the test drives.
async fn machine(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>, Arc<FakeSystemEvents>) {
    let root =
        common::scratch("midi-harbor-suspend").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let system = Arc::new(FakeSystemEvents::new());
    let daemon = Daemon::start_with(
        common::quiet(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
        Arc::clone(&system) as Arc<dyn SystemEvents>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform, system)
}

/// Reports whether anything received would stop the note.
fn released(received: &[MidiMessage]) -> bool {
    received.iter().any(|message| match message {
        MidiMessage::NoteOff { note, .. } => *note == NOTE,
        MidiMessage::NoteOn { note, velocity, .. } => *note == NOTE && *velocity == 0,
        MidiMessage::ControlChange { controller, .. } => *controller == 123,
        _ => false,
    })
}

/// Returns the note-on that starts the held note.
fn held() -> MidiMessage {
    MidiMessage::NoteOn {
        channel: Channel::new(0).expect("channel 0 is a valid MIDI channel"),
        note: NOTE,
        velocity: 100,
    }
}

/// Reports whether `condition` holds within `within`, polling it.
async fn until(mut condition: impl FnMut() -> bool, within: Duration) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < within {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    condition()
}

/// Returns the phase a session is in, if the daemon knows it.
async fn phase(daemon: &Daemon, id: EndpointId) -> Option<ConnectionPhase> {
    daemon
        .session_status(id)
        .await
        .map(|status| status.state.phase())
}

/// Reports whether a session reaches `wanted` within `within`, polling its phase.
async fn reaches(
    daemon: &Daemon,
    id: EndpointId,
    wanted: ConnectionPhase,
    within: Duration,
) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < within {
        if phase(daemon, id).await == Some(wanted) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Proves that a note held across a network session is released on the far machine for a sleep
/// still to come, and not for one already over (FR-026).
///
/// A coming sleep must release the note and only then tell the platform the suspend may go ahead,
/// or it sounds on the far machine until that side's liveness check gives up. The polled watcher
/// reports a suspend only once the machine is back, paired with its resume; releasing then could
/// cut a note someone had just started to play, and the platform, already past the sleep, is not
/// told anything. A release is awaited for up to three seconds; its absence is judged after one
/// and a half, the watcher's one-second pass plus half a pass of margin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_note_is_released_only_for_a_sleep_still_to_come() {
    struct Case {
        name: &'static str,
        event: fn(&FakeSystemEvents),
        want_released: bool,
        want_readied: usize,
    }
    let cases = [
        Case {
            name: "coming",
            event: |system| system.emit(SystemEvent::Suspending),
            want_released: true,
            want_readied: 1,
        },
        Case {
            name: "over",
            event: FakeSystemEvents::sleep_and_wake,
            want_released: false,
            want_readied: 0,
        },
    ];

    for case in cases {
        // Set up the far machine: a session feeding a synth.
        let (far, far_platform, _) = machine(&format!("{}-far", case.name)).await;
        far.create_virtual_port("Synth", 1, 1)
            .await
            .expect("the Synth port is created");
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
            .expect("the far session reports its status")
            .control_port;

        // Set up this machine: a keyboard routed into a session connected to the far one.
        let (near, near_platform, system) = machine(&format!("{}-near", case.name)).await;
        near.create_virtual_port("Keys", 1, 1)
            .await
            .expect("the Keys port is created");
        let outgoing = near
            .create_network_session("Stage Out", 0, InvitationPolicy::Prompt)
            .await
            .expect("the near session is created");
        near.create_route("Keys", "Stage Out")
            .await
            .expect("the route from Keys to Stage Out is created");
        near.connect_peer(outgoing.id, SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .expect("the near session is pointed at the far one");
        assert!(
            reaches(
                &near,
                outgoing.id,
                ConnectionPhase::Connected,
                Duration::from_secs(5)
            )
            .await,
            "{}: the sessions never connected over loopback",
            case.name
        );

        // Hold the note on the far synth.
        let keys = near_platform
            .port_handle("Keys")
            .expect("the Keys port has a platform handle");
        let synth = far_platform
            .port_handle("Synth")
            .expect("the Synth port has a platform handle");
        assert!(
            near_platform.feed(keys, &[held()]),
            "{}: the keyboard port accepts the note",
            case.name
        );
        assert!(
            until(
                || far_platform.sent(synth).contains(&held()),
                Duration::from_secs(2)
            )
            .await,
            "{}: the note never reached the far synth",
            case.name
        );

        // Report the sleep and judge the release.
        (case.event)(&system);
        let within = if case.want_released {
            Duration::from_secs(3)
        } else {
            Duration::from_millis(1_500)
        };
        assert_eq!(
            until(|| released(&far_platform.sent(synth)), within).await,
            case.want_released,
            "{}: the note is released on the far machine exactly when the sleep is still to come",
            case.name
        );
        assert!(
            until(
                || system.times_readied() == case.want_readied,
                Duration::from_secs(1)
            )
            .await,
            "{}: the platform is told the suspend may go ahead once per sleep still to come, \
             and was told {} times",
            case.name,
            system.times_readied()
        );
    }
}

/// Proves that a session ended for sleep stays ended until the machine wakes, whichever side
/// made the connection, and then reconnects at once (R-070, R-072).
///
/// Left to find out for itself, a far machine that was invited invited again when its liveness
/// check gave up, and one that had made the connection invited again at once and every
/// thirty-five seconds after; each invitation woke the sleeping Mac for most of a minute (R-070).
/// So the sleeping machine tells the far side before the suspend goes ahead, and the far side
/// then waits rather than chasing. NetworkManager also takes the network down as the machine
/// goes, which reads as an address change, and acting on it reconnected the session just ended
/// for sleep (R-072); the "network down" row reports one change with the suspend and another
/// 1.2 s later, as the address settles. After two seconds left alone, waking must reconnect
/// within three, well inside the thirty-five a liveness timeout would take.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_ended_for_sleep_stays_ended_until_the_machine_wakes() {
    struct Case {
        name: &'static str,
        sleeper_invites: bool,
        network_drops: bool,
    }
    let cases = [
        Case {
            name: "inviting",
            sleeper_invites: true,
            network_drops: false,
        },
        Case {
            name: "invited",
            sleeper_invites: false,
            network_drops: false,
        },
        Case {
            name: "network down",
            sleeper_invites: true,
            network_drops: true,
        },
    ];

    for case in cases {
        // Connect the machine that will sleep and the far one, in the row's direction.
        let (near, _, system) = machine(&format!("{}-near", case.name)).await;
        let (far, _, _) = machine(&format!("{}-far", case.name)).await;
        let near_policy = if case.sleeper_invites {
            InvitationPolicy::Prompt
        } else {
            InvitationPolicy::AcceptAll
        };
        let sleeper = near
            .create_network_session("Laptop", 0, near_policy)
            .await
            .expect("the sleeping machine's session is created");
        let desk = far
            .create_network_session("Desk", 0, InvitationPolicy::AcceptAll)
            .await
            .expect("the far machine's session is created");
        let ((inviter, from), (invited, to)) = if case.sleeper_invites {
            ((&near, sleeper.id), (&far, desk.id))
        } else {
            ((&far, desk.id), (&near, sleeper.id))
        };
        let port = invited
            .session_status(to)
            .await
            .expect("the invited session reports its status")
            .control_port;
        inviter
            .connect_peer(from, SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .expect("the inviting session is pointed at the other");
        assert!(
            reaches(
                &near,
                sleeper.id,
                ConnectionPhase::Connected,
                Duration::from_secs(5)
            )
            .await,
            "{}: the sessions never connected over loopback",
            case.name
        );

        // Go to sleep: the far side is told before the platform may suspend.
        system.emit(SystemEvent::Suspending);
        if case.network_drops {
            system.emit(SystemEvent::NetworkChanged);
        }
        assert!(
            until(|| system.times_readied() == 1, Duration::from_secs(3)).await,
            "{}: the platform was never told the suspend could go ahead",
            case.name
        );
        assert!(
            reaches(
                &far,
                desk.id,
                ConnectionPhase::Disconnected,
                Duration::from_secs(2)
            )
            .await,
            "{}: the far session was not told the machine is going to sleep",
            case.name
        );

        // Stay asleep: nothing reconnects, whatever the network does meanwhile.
        if case.network_drops {
            tokio::time::sleep(Duration::from_millis(1_200)).await;
            system.emit(SystemEvent::NetworkChanged);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            phase(&far, desk.id).await,
            Some(ConnectionPhase::Disconnected),
            "{}: the far session went after a machine that said it was going to sleep",
            case.name
        );
        assert_ne!(
            phase(&near, sleeper.id).await,
            Some(ConnectionPhase::Connected),
            "{}: the session reconnected on the way to sleep",
            case.name
        );

        // Wake: the session comes back without waiting for a liveness timeout.
        system.emit(SystemEvent::Resumed);
        assert!(
            reaches(
                &near,
                sleeper.id,
                ConnectionPhase::Connected,
                Duration::from_secs(3)
            )
            .await,
            "{}: the machine that slept did not reconnect on waking",
            case.name
        );
    }
}

/// Proves that a coming sleep is acted on at once rather than on the watcher's next pass (R-072).
///
/// NetworkManager has the network down within tens of milliseconds of the sleep signal, so
/// waiting for the next once-a-second look sent the goodbye into no network. Each of three rounds
/// must be readied within 150 ms, well under the one-second pass, so a round that waited for the
/// pass fails; three rounds keep one lucky alignment with the pass from passing the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_coming_sleep_is_acted_on_at_once() {
    let (_, _, system) = machine("prompt").await;
    for round in 1..=3 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        system.emit(SystemEvent::Suspending);
        assert!(
            until(
                || system.times_readied() == round,
                Duration::from_millis(150)
            )
            .await,
            "round {round}: the suspend waited for the watcher's next pass"
        );
        system.emit(SystemEvent::Resumed);
    }
}
