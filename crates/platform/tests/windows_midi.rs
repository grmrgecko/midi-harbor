//! Virtual ports and devices through the real Windows backend.
//!
//! Ignored by default because they need Windows MIDI Services: the API Windows carries from its
//! late-2026 update, or before then the App SDK runtime. A second backend plays the part of
//! another application: it finds the first's ports through WinMM as any program would, and opens
//! them.
//!
//! Run them from the desktop session, as a scheduled task with an interactive logon for example:
//! the service does not answer a virtual device created from an SSH session (research R-093).
//! The service Windows shipped before its late-2026 update stops answering once a virtual device
//! is closed, so there each test runs alone and the Windows MIDI Service is restarted between
//! them; the backend gives up on the service rather than wait, so the next test fails at once.
//!
//! Every test shares the same two backends. Dropping a backend closes its ports at once, and a
//! port created within moments of another closing can take its slot and be listed under the old
//! name, or not at all (research R-086), which the next test would trip over. Run:
//!
//! ```text
//! cargo test -p midi-harbor-platform --test windows_midi -- --ignored --test-threads 1
//! ```

#![cfg(windows)]
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use midi_harbor_core::endpoint::Direction;
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::rtchannel::{self, Drained, RtConsumer};
use midi_harbor_core::stream::SysExEnd;
use midi_harbor_platform::MidiPlatform;
use midi_harbor_platform::PlatformError;
use midi_harbor_platform::midi::{MidiPlatformEvent, VirtualPortSpec};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// The backend under test, and a second one standing in for another application.
fn backends() -> (Arc<dyn MidiPlatform>, Arc<dyn MidiPlatform>) {
    static BACKENDS: OnceLock<(Arc<dyn MidiPlatform>, Arc<dyn MidiPlatform>)> = OnceLock::new();
    let (ours, theirs) = BACKENDS.get_or_init(|| {
        (
            midi_harbor_platform::midi_backend().expect("the real backend"),
            midi_harbor_platform::midi_backend().expect("a second backend"),
        )
    });
    (Arc::clone(ours), Arc::clone(theirs))
}

/// Waits for ports this test destroyed to close and for WinMM to catch up, so the next test does
/// not meet a list still changing.
///
/// Churning ports this fast is something only these tests do; the backend's own handling of a
/// moving list is what the waits inside each test exercise.
fn settle() {
    std::thread::sleep(Duration::from_millis(1500));
}

/// A port name no other test or program is using.
fn unique(label: &str) -> String {
    format!("Harbor {label} {}", std::process::id())
}

fn note(number: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        channel: Channel::new(0).unwrap(),
        note: number,
        velocity: 100,
    }
}

/// How long WinMM may take to list a port the driver created, or to drop one it closed.
///
/// Usually well under a second, but a closed port was once still listed fifteen seconds later.
const WINMM_SETTLE: Duration = Duration::from_secs(15);

/// Waits for a device named `name` to be listed by `backend` in `direction`.
///
/// WinMM adds a new port a moment after the driver creates it, and lists its input and output
/// halves separately, so one half can be listed before the other.
fn wait_for_device(
    backend: &Arc<dyn MidiPlatform>,
    name: &str,
    direction: Direction,
) -> Option<midi_harbor_platform::midi::DiscoveredDevice> {
    let started = Instant::now();
    while started.elapsed() < WINMM_SETTLE {
        if let Some(device) = backend
            .list_devices()
            .expect("devices")
            .into_iter()
            .find(|device| device.fingerprint.name == name && device.direction == direction)
        {
            return Some(device);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let listed: Vec<String> = backend
        .list_devices()
        .expect("devices")
        .into_iter()
        .map(|device| format!("{:?}", device.fingerprint))
        .collect();
    eprintln!("{name} never appeared; listed: {listed:#?}");
    None
}

/// Sends with `send` until `wanted` arrives on `listening`, returning how many sends it took.
///
/// Sent again rather than once, and the count reported, because a message sent in the first
/// moments after another application opens a port can be lost in the driver (research R-086).
fn sent_until_heard(send: &dyn Fn(), listening: &mut RtConsumer, wanted: &Drained) -> Option<u32> {
    for attempt in 1..=10 {
        send();
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(200) {
            if listening.drain(64).iter().any(|drained| drained == wanted) {
                if attempt > 1 {
                    eprintln!("heard after {attempt} sends");
                }
                return Some(attempt);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    None
}

/// Locks that a port created through Windows MIDI Services is listed by WinMM to another
/// application as one software device in both directions, and carries MIDI each way once opened.
///
/// WinMM lists the halves of a port separately; were they not paired, another application would
/// see two devices for one port.
#[test]
#[ignore = "needs Windows MIDI Services"]
fn a_created_port_carries_midi_both_ways_to_another_application() {
    let (ours, theirs) = backends();
    let name = unique("Both Ways");

    let (into_ours, mut ours_heard) = rtchannel::channel(EndpointId::new());
    let (port, _) = ours
        .create_virtual_port(&VirtualPortSpec::simple(&name), vec![into_ours])
        .expect("the port is created");

    // The other application sees one device, both ways, belonging to software.
    let device = wait_for_device(&theirs, &name, Direction::Bidirectional)
        .expect("the port must be visible through WinMM");
    assert_eq!(
        device.direction,
        Direction::Bidirectional,
        "the port's two halves must be one device"
    );
    assert!(device.software, "an application's port is not hardware");

    let (into_theirs, mut theirs_heard) = rtchannel::channel(EndpointId::new());
    let opened = theirs
        .open_device_with_sink(&device.fingerprint, Some(into_theirs))
        .expect("the other application opens it");

    // What they send arrives on our MIDI In.
    let arrived = Drained::Message {
        timestamp: 0,
        message: note(60),
    };
    let send = || theirs.send(opened, &[note(60)]).expect("they send");
    assert!(
        sent_until_heard(&send, &mut ours_heard, &arrived).is_some(),
        "their note never reached us"
    );

    // What we send out reaches them.
    let arrived = Drained::Message {
        timestamp: 0,
        message: note(62),
    };
    let send = || ours.send(port, &[note(62)]).expect("we send");
    assert!(
        sent_until_heard(&send, &mut theirs_heard, &arrived).is_some(),
        "our note never reached them"
    );

    theirs.close_device(opened).expect("they close it");
    ours.destroy_virtual_port(port)
        .expect("the port is destroyed");
    settle();
}

/// Locks that a system-exclusive dump longer than one WinMM input buffer crosses a port whole in
/// each direction, carried as UMP type 3 packets and rejoined.
///
/// 6,003 bytes: the framing, a manufacturer byte and 6,000 data bytes, so it arrives in pieces.
#[test]
#[ignore = "needs Windows MIDI Services"]
fn system_exclusive_crosses_a_port_whole() {
    let (ours, theirs) = backends();
    let name = unique("Dump");

    let (into_ours, mut ours_heard) = rtchannel::channel(EndpointId::new());
    let (port, _) = ours
        .create_virtual_port(&VirtualPortSpec::simple(&name), vec![into_ours])
        .expect("the port is created");
    let device =
        wait_for_device(&theirs, &name, Direction::Bidirectional).expect("the port is visible");
    let (into_theirs, mut theirs_heard) = rtchannel::channel(EndpointId::new());
    let opened = theirs
        .open_device_with_sink(&device.fingerprint, Some(into_theirs))
        .unwrap();

    // A note first each way, so the dump is not what meets a port just opened.
    let warm = Drained::Message {
        timestamp: 0,
        message: note(61),
    };
    let send = || theirs.send(opened, &[note(61)]).expect("they send");
    assert!(
        sent_until_heard(&send, &mut ours_heard, &warm).is_some(),
        "their warming note never reached us"
    );
    let send = || ours.send(port, &[note(61)]).expect("we send");
    assert!(
        sent_until_heard(&send, &mut theirs_heard, &warm).is_some(),
        "our warming note never reached them"
    );

    // Longer than one WinMM input buffer, so it arrives in pieces and is joined.
    let mut dump = vec![0xF0, 0x7D];
    dump.extend((0..6000u32).map(|i| (i % 128) as u8));
    dump.push(0xF7);

    let collect = |listening: &mut RtConsumer| -> Vec<u8> {
        let started = Instant::now();
        let mut bytes = Vec::new();
        while started.elapsed() < Duration::from_secs(5) {
            for drained in listening.drain(64) {
                if let Drained::SysEx {
                    bytes: piece, end, ..
                } = drained
                {
                    bytes.extend(piece);
                    if end != SysExEnd::Open {
                        return bytes;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        bytes
    };

    theirs.send_sysex(opened, &dump).expect("they send a dump");
    let arrived = collect(&mut ours_heard);
    assert!(
        arrived == dump,
        "their dump arrived as {} bytes",
        arrived.len()
    );

    ours.send_sysex(port, &dump).expect("we send a dump");
    let arrived = collect(&mut theirs_heard);
    assert!(
        arrived == dump,
        "our dump arrived as {} bytes",
        arrived.len()
    );

    theirs.close_device(opened).unwrap();
    ours.destroy_virtual_port(port).unwrap();
    settle();
}

/// Locks that a port this backend created is not offered back to it as a device.
///
/// WinMM lists every port on the machine, ours among them, and routing to our own port through
/// WinMM would loop.
#[test]
#[ignore = "needs Windows MIDI Services"]
fn our_own_ports_are_not_offered_back_to_us_as_devices() {
    let (ours, theirs) = backends();
    let name = unique("Own");
    let (port, _) = ours
        .create_virtual_port(&VirtualPortSpec::simple(&name), Vec::new())
        .expect("the port is created");
    // Seen by another backend first, so its absence from ours is not just WinMM being slow.
    wait_for_device(&theirs, &name, Direction::Bidirectional)
        .expect("the port is visible to another application");
    let listed = ours
        .list_devices()
        .unwrap()
        .into_iter()
        .any(|device| device.fingerprint.name == name);
    assert!(!listed, "our own port was offered as a device");
    ours.destroy_virtual_port(port).unwrap();
    settle();
}

/// Locks that a port with several connectors is listed as one WinMM port per group, each in the
/// direction its connectors give it, under either naming Windows MIDI Services uses.
///
/// The current service names a port after its connector's function block; the one Windows
/// shipped before its late-2026 update names it after its group, "Gr 2" for the second
/// (research R-093).
#[test]
#[ignore = "needs Windows MIDI Services"]
fn several_connectors_are_offered_as_numbered_ports_in_the_right_directions() {
    let (ours, theirs) = backends();
    let name = unique("Split");
    let spec = VirtualPortSpec {
        inputs: 2,
        outputs: 1,
        ..VirtualPortSpec::simple(&name)
    };
    let (port, _) = ours
        .create_virtual_port(&spec, Vec::new())
        .expect("the port is created");

    // The first carries MIDI In 1 and the only MIDI Out; the second only takes MIDI in, which
    // another application sees as an output to send to. Both are asked of one listing.
    let by_connector = [
        (format!("{name} 1"), Direction::Bidirectional),
        (format!("{name} 2"), Direction::Output),
    ];
    let by_group = [
        (name.clone(), Direction::Bidirectional),
        (format!("{name} Gr 2"), Direction::Output),
    ];
    let started = Instant::now();
    let mut listed = Vec::new();
    while started.elapsed() < WINMM_SETTLE {
        listed = theirs
            .list_devices()
            .unwrap()
            .into_iter()
            .filter(|device| device.fingerprint.name.starts_with(&name))
            .map(|device| (device.fingerprint.name, device.direction))
            .collect();
        listed.sort_by(|a, b| a.0.cmp(&b.0));
        if listed == by_connector || listed == by_group {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        listed == by_connector || listed == by_group,
        "the port must be listed under one naming or the other, got {listed:?}"
    );
    assert!(
        ours.send_to(port, 1, &[note(64)]).is_err(),
        "there is no second MIDI Out"
    );
    ours.destroy_virtual_port(port).unwrap();
    settle();
}

/// Locks that creating a port under a name another port has is refused as a name conflict.
///
/// Windows MIDI Services makes a device's identifier from its name and refuses a second device
/// of the same name without saying why, so the backend must refuse it first, as a conflict the
/// interface can explain.
#[test]
#[ignore = "needs Windows MIDI Services"]
fn a_name_another_port_has_is_refused_as_a_conflict() {
    let (ours, _) = backends();
    let name = unique("Taken");
    let (port, _) = ours
        .create_virtual_port(&VirtualPortSpec::simple(&name), Vec::new())
        .expect("the port is created");
    let second = ours.create_virtual_port(&VirtualPortSpec::simple(&name), Vec::new());
    assert!(
        matches!(second, Err(PlatformError::NameConflict(ref taken)) if *taken == name),
        "a second port of the same name must be refused as a conflict, got {second:?}"
    );
    ours.destroy_virtual_port(port).unwrap();
    settle();
}

/// Locks that a port another application creates is announced as a setup change, so the
/// daemon looks again at once rather than on its next poll.
#[test]
#[ignore = "needs Windows MIDI Services"]
fn another_application_creating_a_port_is_announced() {
    let (ours, theirs) = backends();
    let _ = ours.drain_events();

    let (port, _) = theirs
        .create_virtual_port(&VirtualPortSpec::simple(unique("Arrival")), Vec::new())
        .expect("the port is created");
    let started = Instant::now();
    let mut announced = false;
    while started.elapsed() < WINMM_SETTLE {
        if ours
            .drain_events()
            .contains(&MidiPlatformEvent::SetupChanged)
        {
            announced = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    theirs.destroy_virtual_port(port).unwrap();
    settle();
    assert!(
        announced,
        "a port another application created was never announced"
    );
}

/// Locks that a port renamed by destroying and creating it is listed under its new name within
/// three seconds, and that the old port is not offered back to us meanwhile.
///
/// Closed first, the old port's slot went to the new one and WinMM went on showing the old name
/// in every process, on some renames and not others (research R-086), so several are done in a
/// row.
#[test]
#[ignore = "needs Windows MIDI Services"]
fn a_port_renamed_by_destroying_and_creating_it_shows_its_new_name_at_once() {
    let (ours, theirs) = backends();
    let mut name = unique("Rename 0");
    let (mut port, _) = ours
        .create_virtual_port(&VirtualPortSpec::simple(&name), Vec::new())
        .expect("the port is created");
    wait_for_device(&theirs, &name, Direction::Bidirectional).expect("the port is visible");

    for round in 1..=8 {
        let before = name;
        name = unique(&format!("Rename {round}"));
        ours.destroy_virtual_port(port)
            .expect("the port is destroyed");
        port = ours
            .create_virtual_port(&VirtualPortSpec::simple(&name), Vec::new())
            .expect("the renamed port is created")
            .0;

        let started = Instant::now();
        let mut seen = false;
        while !seen && started.elapsed() < Duration::from_secs(3) {
            seen = theirs
                .list_devices()
                .unwrap()
                .iter()
                .any(|device| device.fingerprint.name == name);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(seen, "rename {round}: the new name did not appear");

        // The old port is ours, so it is not offered back to us while WinMM lingers over it.
        let offered_back = ours
            .list_devices()
            .unwrap()
            .iter()
            .any(|device| device.fingerprint.name == before);
        assert!(
            !offered_back,
            "rename {round}: the port we destroyed was offered back as a device"
        );
    }
    ours.destroy_virtual_port(port).unwrap();
    settle();
}
