//! Pairing once, and never having to think about it again.
//!
//! A Bluetooth device is not like a network peer, which announces itself and can be probed. It
//! simply stops, and the only sign that it is back is an advertisement. So the promise here is
//! narrow and specific: connect it once, and every time it is switched on afterwards it comes
//! back on its own, with its routes intact and without asking the user anything (FR-020).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::capability::{CapabilityName, UnavailableReason};
use midi_harbor_core::endpoint::{Endpoint, EndpointKind};
use midi_harbor_core::failure::FailureReason;
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::paths::Paths;
use midi_harbor_core::state::ConnectionPhase;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::bluetooth::{
    BluetoothPlatform, BluetoothRole, DiscoveredPeripheral, PeripheralId,
};
use midi_harbor_platform::fake::{FakeBluetoothPlatform, FakeMidiPlatform, Outgoing};
use midi_harbor_platform::midi::MidiPlatform;
use midi_harbor_platform::sysevents::SystemEvents;
use std::sync::Arc;
use std::time::Duration;

/// How long to allow a background watcher to notice something.
///
/// The Bluetooth watcher runs on a five-hundred-millisecond tick, so anything shorter than a
/// couple of ticks is a race rather than a test.
const SETTLE: Duration = Duration::from_millis(1_500);

/// Builds a daemon over fake platforms with nothing configured.
async fn daemon(
    label: &str,
) -> (
    Arc<Daemon>,
    Arc<FakeBluetoothPlatform>,
    Arc<FakeMidiPlatform>,
) {
    let root =
        common::scratch("midi-harbor-bluetooth").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let midi = Arc::new(FakeMidiPlatform::new());
    let radio = Arc::new(FakeBluetoothPlatform::new());
    let system = Arc::new(midi_harbor_platform::fake::FakeSystemEvents::new());

    let daemon = Daemon::start_with_bluetooth(
        Paths::rooted_at(root),
        Arc::clone(&midi) as Arc<dyn MidiPlatform>,
        Arc::clone(&system) as Arc<dyn SystemEvents>,
        Arc::clone(&radio) as Arc<dyn BluetoothPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");

    (daemon, radio, midi)
}

/// Returns a peripheral as a scan reports it, addressed by its name.
fn peripheral(name: &str) -> DiscoveredPeripheral {
    DiscoveredPeripheral {
        id: PeripheralId::new(format!("addr:{name}")),
        name: Some(name.to_owned()),
        rssi: Some(-52),
        paired: false,
    }
}

/// Returns a note on channel one at a fixed velocity.
fn note_on(note: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        channel: Channel::new(0).expect("channel one is in range"),
        note,
        velocity: 100,
    }
}

/// Finds a device the way a user does, by scanning for it, and connects it.
async fn scanned_and_connected(
    daemon: &Arc<Daemon>,
    radio: &FakeBluetoothPlatform,
    device: &DiscoveredPeripheral,
) -> Endpoint {
    daemon
        .start_bluetooth_scan(None)
        .await
        .expect("the scan starts");
    radio.bring_into_range(device.clone());
    tokio::time::sleep(SETTLE).await;
    daemon
        .connect_bluetooth(device.id.as_str())
        .await
        .expect("the device in range connects")
}

/// Starts advertising under "Studio Mac" and returns the advertised endpoint.
async fn advertising(daemon: &Arc<Daemon>) -> Endpoint {
    daemon
        .set_peripheral_advertising(true, Some("Studio Mac".to_owned()))
        .await
        .expect("advertising starts")
        .expect("advertising offers an endpoint")
}

/// Counts the Bluetooth endpoints in the configuration.
async fn bluetooth_endpoints(daemon: &Arc<Daemon>) -> usize {
    daemon
        .read(|config, _| {
            config
                .endpoints
                .iter()
                .filter(|endpoint| matches!(endpoint.kind, EndpointKind::BluetoothDevice(_)))
                .count()
        })
        .await
}

/// Waits until the link is open or closed as asked, giving up after `limit`.
async fn link_becomes(daemon: &Arc<Daemon>, id: EndpointId, open: bool, limit: Duration) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < limit {
        if daemon.bluetooth_link_open(id).await == open {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Reports whether what reached a port would stop the note: an all-notes-off (controller 123) or
/// the note's own note off.
fn releases(sent: &[MidiMessage], note: u8) -> bool {
    sent.iter().any(|message| {
        matches!(
            message,
            MidiMessage::ControlChange {
                controller: 123,
                ..
            }
        ) || matches!(message, MidiMessage::NoteOff { note: off, .. } if *off == note)
    })
}

/// Proves that connecting the same device twice leaves one configuration entry and one link.
///
/// The lesson from R-040, applied before it can happen again: anything that writes an entry per
/// sighting grows the user's configuration file for as long as the device is switched on. A second
/// link delivers everything the device sends a second time, and the first, no longer held, can
/// never be closed.
#[tokio::test]
async fn connecting_a_device_twice_leaves_one_entry() {
    let (daemon, radio, _midi) = daemon("repeat").await;
    let device = peripheral("Acme BLE");
    let first = scanned_and_connected(&daemon, &radio, &device).await;
    let second = daemon
        .connect_bluetooth(device.id.as_str())
        .await
        .expect("the device connects a second time");

    assert_eq!(first.id, second.id, "a second connect made a second entry");
    assert_eq!(
        bluetooth_endpoints(&daemon).await,
        1,
        "one device connected twice is one entry in the configuration"
    );
    assert_eq!(
        radio.links_to(&device.id),
        1,
        "a second connect opened a second link"
    );
}

/// Proves that a device the radio has just lost is refused as not found, while a device the radio
/// still hears connects.
///
/// The daemon's list of what is in range trails the radio's. A device lost in between was refused
/// by the radio and reported as removed, though it had never been connected. The device still in
/// range pins the positive side: the refusal is about the lost device, not about connecting.
#[tokio::test]
async fn a_device_the_radio_has_just_lost_is_reported_as_not_in_range() {
    let (daemon, radio, _midi) = daemon("just-lost").await;
    let lost = peripheral("Acme BLE");
    let heard = peripheral("Other BLE");
    daemon
        .start_bluetooth_scan(None)
        .await
        .expect("the scan starts");
    radio.bring_into_range(lost.clone());
    radio.bring_into_range(heard.clone());
    tokio::time::sleep(SETTLE).await;

    radio.take_out_of_range(&lost.id);
    let refused = daemon.connect_bluetooth(lost.id.as_str()).await;
    assert!(
        matches!(&refused, Err(midi_harbor_daemon::DaemonError::NotFound(address)) if address == lost.id.as_str()),
        "a device the radio no longer hears is not found rather than removed, got {refused:?}"
    );
    let connected = daemon.connect_bluetooth(heard.id.as_str()).await;
    assert!(
        connected.is_ok(),
        "a device the radio still hears connects, got {connected:?}"
    );
}

/// Proves FR-020 end to end: a remembered device that drops reconnects on its own when it
/// returns, as the same endpoint, and the history records the drop and the return (FR-046,
/// SC-013).
///
/// Nothing polls a Bluetooth device and nothing can: hearing its advertisement is the only signal
/// it is back, so hearing one has to be what reconnects it. A Bluetooth link once changed state
/// without a word in the history, so a user could not tell afterwards when it dropped or that it
/// came back. Three events are expected: connected, lost, back.
#[tokio::test]
async fn a_remembered_device_reconnects_when_it_returns() {
    let (daemon, radio, _midi) = daemon("return").await;
    let device = peripheral("Acme BLE");
    let endpoint = scanned_and_connected(&daemon, &radio, &device).await;
    tokio::time::sleep(SETTLE).await;

    // Switched off, and the link goes with it.
    radio.take_out_of_range(&device.id);
    assert!(
        link_becomes(&daemon, endpoint.id, false, Duration::from_secs(5)).await,
        "the link outlived the device"
    );

    // Switched on again, with nobody asked anything.
    radio.bring_into_range(device.clone());
    assert!(
        link_becomes(&daemon, endpoint.id, true, Duration::from_secs(10)).await,
        "a remembered device did not come back on its own"
    );
    assert_eq!(
        bluetooth_endpoints(&daemon).await,
        1,
        "the device came back as a new entry rather than as itself"
    );

    // The link is held a moment before the watcher marks it connected.
    tokio::time::sleep(SETTLE).await;
    let recorded: Vec<String> = daemon
        .events(None, 100)
        .await
        .into_iter()
        .filter(|event| event.endpoint == Some(endpoint.id))
        .map(|event| event.detail)
        .collect();
    let name = endpoint.name.as_str();
    assert_eq!(
        recorded,
        vec![
            format!("{name} connected"),
            format!("{name} lost its link; it reconnects when the device is back in range"),
            format!("{name} is back and connected"),
        ],
        "the history must say when the device dropped and that it came back"
    );
}

/// Proves that a forgotten device does not reconnect when it returns.
///
/// The other half of FR-020: forgetting has to mean something, or a user who wants to stop a
/// device reconnecting has no way to say so.
#[tokio::test]
async fn a_forgotten_device_stays_forgotten_when_it_returns() {
    let (daemon, radio, _midi) = daemon("forget").await;
    let device = peripheral("Acme BLE");
    let endpoint = scanned_and_connected(&daemon, &radio, &device).await;

    daemon
        .forget_bluetooth(endpoint.id)
        .await
        .expect("the device is forgotten");
    radio.take_out_of_range(&device.id);
    tokio::time::sleep(SETTLE).await;
    radio.bring_into_range(device);
    tokio::time::sleep(SETTLE).await;

    assert_eq!(
        bluetooth_endpoints(&daemon).await,
        0,
        "a forgotten device came back by itself"
    );
}

/// Proves that a malformed packet is counted on the device that sent it, and the packet after it
/// still arrives.
///
/// A bad packet was only logged at debug, so a device sending garbage looked like one playing
/// nothing, and nothing in the interface said which.
#[tokio::test]
async fn a_malformed_packet_is_counted_on_the_device_that_sent_it() {
    let (daemon, radio, _midi) = daemon("malformed").await;
    let device = peripheral("Acme BLE");
    let endpoint = scanned_and_connected(&daemon, &radio, &device).await;

    let link = radio
        .link_for(&device.id)
        .expect("the fake issued a link handle");
    assert!(
        radio.peripheral_sends_malformed(link),
        "the link accepts the malformed packet"
    );
    assert!(
        radio.peripheral_sends(link, note_on(60), 1_000),
        "the link accepts the note after it"
    );
    tokio::time::sleep(SETTLE).await;

    let counters = daemon
        .counters(endpoint.id)
        .await
        .expect("the device has counters");
    assert_eq!(
        counters.packets_malformed, 1,
        "one malformed packet was sent, so one is counted"
    );
    assert_eq!(
        counters.messages_received, 1,
        "the next packet still arrives"
    );
    assert_eq!(
        counters.messages_dropped, 0,
        "a malformed packet is not a dropped message"
    );
}

/// Proves that messages the input ring could not hold are counted as dropped, one for each the
/// ring refused.
///
/// A full ring was logged and never counted, so the endpoint's dropped count stayed at zero while
/// it lost MIDI. Twice the ring's capacity is offered. The test runtime has one thread, so nothing
/// drains the ring while it fills.
#[tokio::test]
async fn messages_the_input_ring_could_not_hold_are_counted_as_dropped() {
    let (daemon, radio, _midi) = daemon("overflow").await;
    let device = peripheral("Acme BLE");
    let endpoint = scanned_and_connected(&daemon, &radio, &device).await;

    let link = radio
        .link_for(&device.id)
        .expect("the fake issued a link handle");
    let refused = (0..midi_harbor_core::rtchannel::EVENT_CAPACITY * 2)
        .filter(|_| !radio.peripheral_sends(link, note_on(60), 1_000))
        .count() as u64;
    assert!(refused > 0, "the ring never filled");
    tokio::time::sleep(SETTLE).await;

    let counters = daemon
        .counters(endpoint.id)
        .await
        .expect("the device has counters");
    assert_eq!(
        counters.messages_dropped, refused,
        "every message the ring refused is counted as dropped"
    );
    assert_eq!(
        counters.packets_malformed, 0,
        "an overflow is not a malformed packet"
    );
}

/// Proves that a connected device and the advertised port can both be watched.
///
/// Neither has a platform handle, which is what watching once required, so `monitor` refused both
/// as not running while they carried MIDI.
#[tokio::test]
async fn a_connected_device_and_the_advertised_port_can_be_watched() {
    let (daemon, radio, _midi) = daemon("watch").await;
    let device = peripheral("Acme BLE");
    let endpoint = scanned_and_connected(&daemon, &radio, &device).await;

    let mut watching = daemon
        .watch_endpoint(endpoint.id)
        .await
        .expect("a connected device can be watched");
    let link = radio
        .link_for(&device.id)
        .expect("the fake issued a link handle");
    assert!(
        radio.peripheral_sends(link, note_on(60), 1_000),
        "the link accepts the note"
    );
    let observed = tokio::time::timeout(SETTLE, watching.recv())
        .await
        .expect("the note is seen in time")
        .expect("the watch delivers what it saw");
    assert!(
        matches!(
            observed.seen,
            midi_harbor_daemon::state::Seen::Message(message) if message == note_on(60)
        ),
        "the watch shows the note the device sent"
    );

    let advertised = advertising(&daemon).await;
    assert!(
        daemon.watch_endpoint(advertised.id).await.is_some(),
        "the advertised port can be watched"
    );
}

/// Proves that MIDI routed to a connected device reaches the radio, and that a note held on it is
/// released when its source is switched off.
///
/// Silencing sent only through platform ports, and a Bluetooth link has none, so switching off
/// the keyboard left the note ringing on the Bluetooth synth.
#[tokio::test]
async fn a_note_held_on_a_bluetooth_synth_is_released_when_its_source_goes() {
    let (daemon, radio, midi) = daemon("silence").await;
    let device = peripheral("Acme BLE");
    let endpoint = scanned_and_connected(&daemon, &radio, &device).await;
    let keyboard = daemon
        .create_virtual_port("Keyboard", 1, 1)
        .await
        .expect("the Keyboard port is created");
    daemon
        .create_route("Keyboard", endpoint.name.as_str())
        .await
        .expect("the route from Keyboard to the device is created");
    let handle = midi
        .port_handle("Keyboard")
        .expect("the Keyboard port has a platform handle");
    assert!(
        midi.feed(handle, &[note_on(64)]),
        "the keyboard port accepts the note"
    );
    tokio::time::sleep(SETTLE).await;
    let link = radio
        .link_for(&device.id)
        .expect("the fake issued a link handle");
    assert_eq!(
        radio.sent(link),
        vec![Outgoing::Message(note_on(64))],
        "the routed note reaches the radio"
    );

    daemon
        .set_enabled(keyboard.id, false)
        .await
        .expect("the keyboard is switched off");

    let messages: Vec<MidiMessage> = radio
        .sent(link)
        .into_iter()
        .filter_map(|sent| match sent {
            Outgoing::Message(message) => Some(message),
            Outgoing::SysEx(_) => None,
        })
        .collect();
    assert!(
        releases(&messages, 64),
        "the note is still held: {:?}",
        radio.sent(link)
    );
}

/// Proves that the advertised endpoint and advertising are one switch, and that switching
/// advertising on again offers the same single port.
///
/// The endpoint list and the advertising control were separate switches for one thing, so the
/// list could show the endpoint on while nothing advertised.
#[tokio::test]
async fn the_advertised_endpoint_and_advertising_are_one_switch() {
    let (daemon, radio, _midi) = daemon("one-switch").await;
    let endpoint = advertising(&daemon).await;
    assert_eq!(
        radio.advertised_name(),
        Some("Studio Mac".to_owned()),
        "the radio advertises under the name asked for"
    );

    // Switching the endpoint off stops advertising, and records it.
    let off = daemon
        .set_enabled(endpoint.id, false)
        .await
        .expect("the advertised endpoint is switched off");
    assert!(!off.enabled, "the endpoint reads as off once switched off");
    assert_eq!(radio.advertised_name(), None, "still advertising");
    let preference = daemon
        .read(|config, _| config.preferences.bluetooth_advertising)
        .await;
    assert!(
        !preference,
        "the advertising preference follows the endpoint's switch"
    );

    // Switching it on advertises again, under its own name.
    daemon
        .set_enabled(endpoint.id, true)
        .await
        .expect("the advertised endpoint is switched on");
    assert_eq!(
        radio.advertised_name(),
        Some("Studio Mac".to_owned()),
        "switching the endpoint on advertises under its own name"
    );

    // Stopping advertising switches the endpoint off.
    daemon
        .set_peripheral_advertising(false, None)
        .await
        .expect("advertising stops");
    let enabled = daemon
        .read(|config, _| config.endpoint(endpoint.id).map(|e| e.enabled))
        .await;
    assert_eq!(enabled, Some(false), "the endpoint still reads as on");

    // Advertising again offers the same port rather than a new one.
    let again = advertising(&daemon).await;
    assert_eq!(
        again.id, endpoint.id,
        "switching advertising on again made a new port"
    );
    assert_eq!(
        bluetooth_endpoints(&daemon).await,
        1,
        "advertising offers one port however often it is switched on"
    );
}

/// Proves FR-021 and FR-053: every way Bluetooth can be unavailable is reported as itself, and
/// only the refused role is affected.
///
/// "Bluetooth is unavailable" is three different problems with three different answers (buy an
/// adapter, switch it on, grant a permission), and a user told only the first of those has no way
/// to reach the other two. The capability query names each. The refusal names a permission, which
/// is the user's to grant, and otherwise says only that Bluetooth is not available (R-077).
#[tokio::test]
async fn every_way_bluetooth_can_be_unavailable_says_which_one_it_is() {
    let cases = [
        (UnavailableReason::NoAdapter, "bluetooth is not available"),
        (UnavailableReason::AdapterOff, "bluetooth is not available"),
        (
            UnavailableReason::PermissionDenied {
                what: "Bluetooth".to_owned(),
            },
            "Bluetooth permission was not granted",
        ),
    ];

    for (reason, expected) in cases {
        let (daemon, radio, midi) = daemon("unavailable").await;
        radio.refuse(BluetoothRole::Peripheral, reason.clone());

        // The capability query is where the distinction has to survive, because the failure
        // reason is a closed set shared with every other endpoint kind.
        let reported = daemon
            .capabilities()
            .all()
            .iter()
            .find(|capability| capability.name == CapabilityName::BluetoothPeripheral)
            .and_then(|capability| capability.reason.clone())
            .expect("the peripheral role reports a reason");
        assert_eq!(
            reported.to_string(),
            reason.to_string(),
            "{reason}: the capability query lost the distinction"
        );

        // The request fails too, rather than only the query disagreeing with it.
        let refused = daemon
            .set_peripheral_advertising(true, None)
            .await
            .expect_err("advertising is refused");
        assert!(
            refused.to_string().contains(expected),
            "{reason}: the refusal said {refused}, not {expected}"
        );

        // The other role is untouched, and so is everything that is not Bluetooth.
        daemon
            .start_bluetooth_scan(Some(Duration::from_millis(50)))
            .await
            .expect("scanning still works");
        daemon
            .create_virtual_port("Synth", 1, 1)
            .await
            .expect("a virtual port is still created");
        assert!(
            midi.port_handle("Synth").is_some(),
            "{reason}: a Bluetooth refusal must not stop virtual ports opening"
        );
    }
}

/// Proves that advertising resumes after a restart because the configuration file says to.
///
/// The configuration has offered a `bluetooth_advertising` preference since the schema was
/// written, and nothing read it: a user could set it by hand, restart, and find the machine silent
/// with no indication why.
#[tokio::test]
async fn advertising_survives_a_restart_because_the_file_decides_it() {
    let root =
        common::scratch("midi-harbor-bluetooth").join(format!("restart-{}", uuid::Uuid::new_v4()));
    let midi = Arc::new(FakeMidiPlatform::new());
    let system = Arc::new(midi_harbor_platform::fake::FakeSystemEvents::new());

    // A radio each, because a restart gives the new daemon a radio that has forgotten everything,
    // and because two daemons sharing one would let the first answer for the second.
    let before = Arc::new(FakeBluetoothPlatform::new());
    let after = Arc::new(FakeBluetoothPlatform::new());

    let start = |radio: &Arc<FakeBluetoothPlatform>| {
        Daemon::start_with_bluetooth(
            Paths::rooted_at(root.clone()),
            Arc::clone(&midi) as Arc<dyn MidiPlatform>,
            Arc::clone(&system) as Arc<dyn SystemEvents>,
            Arc::clone(radio) as Arc<dyn BluetoothPlatform>,
        )
    };

    let first = start(&before)
        .await
        .expect("the first daemon starts over a scratch directory");
    advertising(&first).await;

    let second = start(&after)
        .await
        .expect("the second daemon starts over the same directory");
    tokio::time::sleep(SETTLE).await;

    assert_eq!(
        after.advertised_name(),
        Some("Studio Mac".to_owned()),
        "the machine came back silent despite the configuration saying to advertise"
    );
    assert_eq!(
        bluetooth_endpoints(&second).await,
        1,
        "the restarted daemon reuses the advertised endpoint rather than adding one"
    );
}

/// Proves that the advertised port sends and counts nothing while no device is on it, and
/// delivers system exclusive whole once one subscribes.
///
/// A message counts as sent only once a device has received it. One dump is fed after a device
/// subscribes, so one message is counted.
#[tokio::test]
async fn nothing_is_counted_as_sent_to_an_advertised_port_nobody_is_on() {
    let (daemon, radio, midi) = daemon("advertised-unheard").await;
    let endpoint = advertising(&daemon).await;
    daemon
        .create_virtual_port("Keyboard", 1, 1)
        .await
        .expect("the Keyboard port is created");
    daemon
        .create_route("Keyboard", endpoint.name.as_str())
        .await
        .expect("the route from Keyboard to the advertised port is created");
    let handle = midi
        .port_handle("Keyboard")
        .expect("the Keyboard port has a platform handle");
    let sent = |daemon: Arc<Daemon>| async move {
        daemon
            .counters(endpoint.id)
            .await
            .map_or(0, |counters| counters.messages_sent)
    };

    // Play into the port while nobody is on it.
    assert!(
        midi.feed(handle, &[note_on(60)]),
        "the keyboard port accepts the note"
    );
    assert!(
        midi.feed_bytes(handle, &[0xF0, 0x7D, 0x01, 0xF7]),
        "the keyboard port accepts the dump"
    );
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        sent(Arc::clone(&daemon)).await,
        0,
        "nothing is counted as sent while no device is on the port"
    );
    assert!(
        radio.notified().is_empty(),
        "nothing is notified while no device is on the port"
    );

    // A device subscribes, and a dump is sent to it.
    radio.central_subscribes("phone");
    tokio::time::sleep(SETTLE).await;
    let dump = [0xF0, 0x7D, 0x01, 0x02, 0xF7];
    assert!(
        midi.feed_bytes(handle, &dump),
        "the keyboard port accepts the dump"
    );
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        radio.notified(),
        vec![Outgoing::SysEx(dump.to_vec())],
        "the dump routed to the advertised port reaches the radio whole"
    );
    assert_eq!(
        sent(Arc::clone(&daemon)).await,
        1,
        "one dump reached a device, so one message is counted"
    );
}

/// Proves that after a restart, Bluetooth endpoints report a definite state rather than unknown:
/// the advertised one disabled, a remembered device out of range disconnected.
///
/// Bluetooth endpoints had no state until the radio reported something, so after a restart each
/// read "unknown": the advertised one while advertising was off, and a remembered device that was
/// simply out of range.
#[tokio::test]
async fn after_a_restart_bluetooth_endpoints_say_what_they_are_doing() {
    let root =
        common::scratch("midi-harbor-bluetooth").join(format!("seeded-{}", uuid::Uuid::new_v4()));
    let midi = Arc::new(FakeMidiPlatform::new());
    let system = Arc::new(midi_harbor_platform::fake::FakeSystemEvents::new());
    let start = |radio: Arc<FakeBluetoothPlatform>| {
        Daemon::start_with_bluetooth(
            Paths::rooted_at(root.clone()),
            Arc::clone(&midi) as Arc<dyn MidiPlatform>,
            Arc::clone(&system) as Arc<dyn SystemEvents>,
            radio as Arc<dyn BluetoothPlatform>,
        )
    };

    let radio = Arc::new(FakeBluetoothPlatform::new());
    let first = start(Arc::clone(&radio))
        .await
        .expect("the first daemon starts over a scratch directory");
    let device = peripheral("Acme BLE");
    let remembered = scanned_and_connected(&first, &radio, &device).await;
    let advertised = advertising(&first).await;
    first
        .set_peripheral_advertising(false, None)
        .await
        .expect("advertising stops");

    // A new daemon, with a radio that hears nothing.
    let second = start(Arc::new(FakeBluetoothPlatform::new()))
        .await
        .expect("the second daemon starts over the same directory");
    let phase = |id| {
        let second = Arc::clone(&second);
        async move {
            second
                .read(|_, runtime| runtime.get(&id).map(|r| r.state.phase()))
                .await
        }
    };
    assert_eq!(
        phase(advertised.id).await,
        Some(ConnectionPhase::Disabled),
        "the advertised endpoint with advertising off reads as disabled"
    );
    assert_eq!(
        phase(remembered.id).await,
        Some(ConnectionPhase::Disconnected),
        "a remembered device out of range reads as disconnected"
    );
}

/// Proves that a link that keeps dropping is reported unstable and still reconnects the next
/// time the device is heard.
///
/// The edge case the spec names: a link that flaps is spaced out rather than reconnected as fast
/// as it drops, and the user is told why it keeps going quiet. Spacing it out must not lose it,
/// though: the last sighting below is the only one, and it still has to reconnect. The device
/// drops and returns `UNSTABLE_AFTER` times, then drops once more.
#[tokio::test]
async fn a_device_that_keeps_dropping_is_called_unstable_and_still_comes_back() {
    let (daemon, radio, _midi) = daemon("flapping").await;
    let device = peripheral("Loose Cable");
    let endpoint = scanned_and_connected(&daemon, &radio, &device).await;

    for _ in 0..midi_harbor_core::state::UNSTABLE_AFTER {
        radio.take_out_of_range(&device.id);
        assert!(
            link_becomes(&daemon, endpoint.id, false, Duration::from_secs(5)).await,
            "the link outlived the device"
        );
        radio.bring_into_range(device.clone());
        assert!(
            link_becomes(&daemon, endpoint.id, true, Duration::from_secs(15)).await,
            "a flapping device stopped coming back"
        );
    }
    radio.take_out_of_range(&device.id);
    assert!(
        link_becomes(&daemon, endpoint.id, false, Duration::from_secs(5)).await,
        "the link outlived the device"
    );

    let unstable = daemon
        .read(|_, runtime| {
            runtime
                .get(&endpoint.id)
                .map(|r| r.state.is_unstable(jiff::Timestamp::now()))
        })
        .await;
    assert_eq!(unstable, Some(true), "the flapping went unreported");

    radio.bring_into_range(device.clone());
    assert!(
        link_becomes(&daemon, endpoint.id, true, Duration::from_secs(15)).await,
        "heard once during its backoff, and never reconnected"
    );
}

/// Proves FR-020 after the user's scan has ended: a device that drops still reconnects, and the
/// radio stops listening again once nothing is waiting.
///
/// A device is heard only while the radio listens, and nothing listened once the user's scan
/// ended: a device that dropped afterwards never reconnected, while the log said it would when it
/// returned.
#[tokio::test]
async fn a_device_lost_after_the_scan_ended_still_comes_back() {
    let (daemon, radio, _midi) = daemon("rescan").await;
    let device = peripheral("Acme BLE");
    daemon
        .start_bluetooth_scan(Some(Duration::from_millis(100)))
        .await
        .expect("the scan starts");
    radio.bring_into_range(device.clone());
    tokio::time::sleep(SETTLE).await;
    let endpoint = daemon
        .connect_bluetooth(device.id.as_str())
        .await
        .expect("the device in range connects");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !radio.is_scanning(),
        "the scan should have ended while everything was connected"
    );

    radio.take_out_of_range(&device.id);
    assert!(
        link_becomes(&daemon, endpoint.id, false, Duration::from_secs(5)).await,
        "the link outlived the device"
    );
    radio.bring_into_range(device.clone());
    assert!(
        link_becomes(&daemon, endpoint.id, true, Duration::from_secs(10)).await,
        "a device that dropped after the scan ended never came back"
    );

    // Listening stops again once nothing is waiting.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !radio.is_scanning(),
        "the radio went on scanning for nothing"
    );
}

/// Returns the reason an endpoint's connection last failed, if it has.
async fn last_error(daemon: &Arc<Daemon>, id: EndpointId) -> Option<FailureReason> {
    daemon
        .read(|_, runtime| {
            runtime
                .get(&id)
                .and_then(|runtime| runtime.state.last_error().cloned())
        })
        .await
}

/// Proves that a link that never comes up reports the device's own reason, and keeps it when the
/// radio reports the link closed afterwards.
///
/// A connection the device refused was reported as "device was removed", which sent the user
/// looking for a device that was right there. A radio also reports the link closed after it has
/// already failed; taken as a fresh loss, that replaced the real reason with "device was removed"
/// and logged that the device would reconnect, which is how a refused connection read on Linux.
#[tokio::test]
async fn a_link_that_never_comes_up_says_why() {
    let (daemon, radio, _midi) = daemon("failed-link").await;
    let device = peripheral("Acme BLE");
    daemon
        .start_bluetooth_scan(None)
        .await
        .expect("the scan starts");
    radio.bring_into_range(device.clone());
    tokio::time::sleep(SETTLE).await;

    radio.fail_next_link(FailureReason::PeerRejected);
    let endpoint = daemon
        .connect_bluetooth(device.id.as_str())
        .await
        .expect("the connect request is accepted");

    // Wait for the failure to be recorded.
    let mut reason = None;
    for _ in 0..40 {
        reason = last_error(&daemon, endpoint.id).await;
        if reason.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        reason,
        Some(FailureReason::PeerRejected),
        "the link's own reason was lost"
    );
    assert!(
        !daemon.bluetooth_link_open(endpoint.id).await,
        "a link that failed to come up reads as open"
    );

    // The radio reports the failed link closed.
    radio.report_disconnected(&device.id);
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        last_error(&daemon, endpoint.id).await,
        Some(FailureReason::PeerRejected),
        "a late report of the closed link replaced the real reason"
    );
}

/// Proves that the advertised port reads connected only while a device is subscribed to it.
///
/// It once read connected the moment advertising began, stayed connected after the device left,
/// and read disabled when advertising was switched off and on again.
#[tokio::test]
async fn the_advertised_port_reads_connected_only_while_a_device_is() {
    let (daemon, radio, _midi) = daemon("advertised-state").await;
    let phase = |daemon: Arc<Daemon>| async move {
        let id = daemon
            .read(|config, _| {
                config
                    .endpoints
                    .iter()
                    .find(|endpoint| {
                        matches!(&endpoint.kind, EndpointKind::BluetoothDevice(device)
                            if device.role == midi_harbor_core::endpoint::BleRole::Peripheral)
                    })
                    .map(|endpoint| endpoint.id)
            })
            .await
            .expect("the advertised endpoint is configured");
        tokio::time::sleep(SETTLE).await;
        daemon
            .read(move |_, runtime| runtime.get(&id).map(|runtime| runtime.state.phase()))
            .await
    };

    daemon
        .set_peripheral_advertising(true, Some("Stage".to_owned()))
        .await
        .expect("advertising starts");
    assert_eq!(
        phase(Arc::clone(&daemon)).await,
        Some(ConnectionPhase::Disconnected),
        "advertising with nobody subscribed reads as disconnected"
    );

    radio.central_subscribes("a phone");
    assert_eq!(
        phase(Arc::clone(&daemon)).await,
        Some(ConnectionPhase::Connected),
        "a subscribed device makes the port read as connected"
    );

    radio.central_leaves("a phone");
    assert_eq!(
        phase(Arc::clone(&daemon)).await,
        Some(ConnectionPhase::Disconnected),
        "the port reads as disconnected once the device leaves"
    );

    daemon
        .set_peripheral_advertising(false, None)
        .await
        .expect("advertising stops");
    daemon
        .set_peripheral_advertising(true, None)
        .await
        .expect("advertising starts again");
    assert_eq!(
        phase(Arc::clone(&daemon)).await,
        Some(ConnectionPhase::Disconnected),
        "advertising switched off and on again reads as disconnected, not disabled"
    );
}

/// Proves that MIDI from a connected device reaches its route, and that a note it was playing is
/// released downstream when the device is lost.
///
/// Losing the link stopped only what was being sent to the device, so a keyboard that dropped
/// mid-note left the note sounding on the synth it was routed to.
#[tokio::test]
async fn a_note_a_lost_device_was_playing_is_released_downstream() {
    let (daemon, radio, midi) = daemon("lost-source").await;
    let device = peripheral("Acme BLE");
    let endpoint = scanned_and_connected(&daemon, &radio, &device).await;
    daemon
        .create_virtual_port("Synth", 1, 1)
        .await
        .expect("the Synth port is created");
    daemon
        .create_route(endpoint.name.as_str(), "Synth")
        .await
        .expect("the route from the device to Synth is created");
    let link = radio
        .link_for(&device.id)
        .expect("the fake issued a link handle");
    assert!(
        radio.peripheral_sends(link, note_on(60), 1_000),
        "the link accepts the note"
    );
    tokio::time::sleep(SETTLE).await;
    let synth = midi
        .port_handle("Synth")
        .expect("the Synth port has a platform handle");
    assert_eq!(
        midi.sent(synth),
        vec![note_on(60)],
        "the device's note reaches its route"
    );

    radio.take_out_of_range(&device.id);
    assert!(
        link_becomes(&daemon, endpoint.id, false, Duration::from_secs(5)).await,
        "the link outlived the device"
    );
    tokio::time::sleep(SETTLE).await;
    assert!(
        releases(&midi.sent(synth), 60),
        "the note is still held: {:?}",
        midi.sent(synth)
    );
}

/// Proves that a note a device on the advertised port was playing is released downstream when
/// the device leaves.
///
/// Nothing was silenced when the last device left the advertised port.
#[tokio::test]
async fn a_note_a_departed_central_was_playing_is_released_downstream() {
    let (daemon, radio, midi) = daemon("central-left").await;
    let advertised = advertising(&daemon).await;
    daemon
        .create_virtual_port("Synth", 1, 1)
        .await
        .expect("the Synth port is created");
    daemon
        .create_route(advertised.name.as_str(), "Synth")
        .await
        .expect("the route from the advertised port to Synth is created");
    radio.central_subscribes("iPad");
    tokio::time::sleep(SETTLE).await;
    assert!(
        radio.central_sends(note_on(62), 1_000),
        "the advertised port accepts the note"
    );
    tokio::time::sleep(SETTLE).await;
    let synth = midi
        .port_handle("Synth")
        .expect("the Synth port has a platform handle");
    assert_eq!(
        midi.sent(synth),
        vec![note_on(62)],
        "the subscribed device's note reaches its route"
    );

    radio.central_leaves("iPad");
    tokio::time::sleep(SETTLE).await;
    assert!(
        releases(&midi.sent(synth), 62),
        "the note is still held: {:?}",
        midi.sent(synth)
    );
}

/// Proves FR-027: a device that drops and comes back is sent the controller state routed to it
/// before it dropped.
///
/// A synth switched off and on has lost what it was set to, and nothing resent it. The volume
/// (controller 7) set before the drop must reach the new link.
#[tokio::test]
async fn a_device_that_comes_back_is_sent_its_controller_state_again() {
    let (daemon, radio, midi) = daemon("restore").await;
    let device = peripheral("Acme Synth");
    let endpoint = scanned_and_connected(&daemon, &radio, &device).await;
    daemon
        .create_virtual_port("Faders", 1, 1)
        .await
        .expect("the Faders port is created");
    daemon
        .create_route("Faders", endpoint.name.as_str())
        .await
        .expect("the route from Faders to the device is created");
    let volume = MidiMessage::ControlChange {
        channel: Channel::new(0).expect("channel one is in range"),
        controller: 7,
        value: 90,
    };
    let faders = midi
        .port_handle("Faders")
        .expect("the Faders port has a platform handle");
    assert!(
        midi.feed(faders, &[volume]),
        "the faders port accepts the volume"
    );
    tokio::time::sleep(SETTLE).await;

    radio.take_out_of_range(&device.id);
    assert!(
        link_becomes(&daemon, endpoint.id, false, Duration::from_secs(5)).await,
        "the link outlived the device"
    );
    radio.bring_into_range(device.clone());
    assert!(
        link_becomes(&daemon, endpoint.id, true, Duration::from_secs(10)).await,
        "a remembered device did not come back on its own"
    );
    tokio::time::sleep(SETTLE).await;

    let link = radio
        .link_for(&device.id)
        .expect("the fake issued a handle for the new link");
    assert!(
        radio.sent(link).contains(&Outgoing::Message(volume)),
        "the device came back without its volume: {:?}",
        radio.sent(link)
    );
}

/// Proves FR-019: notes a device played apart are delivered apart, in order.
///
/// Timestamps were decoded and dropped, so two notes played 10 ms apart but carried in one radio
/// packet reached the synth together. The notes carry timestamps 1000 and 1010, ten milliseconds
/// apart; at least five must separate their delivery, allowing half for scheduling jitter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notes_a_device_played_apart_are_delivered_apart() {
    let (daemon, radio, midi) = daemon("timing").await;
    let device = peripheral("Acme BLE");
    let endpoint = scanned_and_connected(&daemon, &radio, &device).await;
    daemon
        .create_virtual_port("Synth", 1, 1)
        .await
        .expect("the Synth port is created");
    daemon
        .create_route(endpoint.name.as_str(), "Synth")
        .await
        .expect("the route from the device to Synth is created");
    tokio::time::sleep(SETTLE).await;
    let link = radio
        .link_for(&device.id)
        .expect("the fake issued a link handle");
    let synth = midi
        .port_handle("Synth")
        .expect("the Synth port has a platform handle");

    assert!(
        radio.peripheral_sends(link, note_on(60), 1_000),
        "the link accepts the first note"
    );
    assert!(
        radio.peripheral_sends(link, note_on(64), 1_010),
        "the link accepts the second note"
    );

    let started = std::time::Instant::now();
    let mut first = None;
    let mut second = None;
    while started.elapsed() < Duration::from_secs(2) && second.is_none() {
        let sent = midi.sent(synth);
        if first.is_none() && sent.contains(&note_on(60)) {
            first = Some(std::time::Instant::now());
        }
        if sent.contains(&note_on(64)) {
            second = Some(std::time::Instant::now());
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    let (first, second) = (
        first.expect("the first note arrives"),
        second.expect("the second note arrives"),
    );
    let gap = second.duration_since(first);
    assert!(
        gap >= Duration::from_millis(5),
        "played 10 ms apart, delivered {gap:?} apart"
    );
    assert_eq!(
        midi.sent(synth),
        vec![note_on(60), note_on(64)],
        "both notes arrive once, in the order they were played"
    );
}

/// Proves that a changed machine name is advertised after a reload, without a restart.
///
/// The name was read once at startup, so a reload reported it as waiting on a restart and
/// advertising went on using the old one.
#[tokio::test]
async fn a_changed_machine_name_is_advertised_without_a_restart() {
    let (daemon, _radio, _midi) = daemon("machine-name").await;
    let path = daemon.paths().config_file();
    let text = std::fs::read_to_string(&path).expect("the daemon wrote its configuration");
    let mut stored = midi_harbor_core::config::parse(&text).expect("the configuration parses");
    stored.preferences.machine_name = Some("Stage Rig".to_owned());
    std::fs::write(
        &path,
        midi_harbor_core::config::to_text(&stored).expect("the configuration serializes"),
    )
    .expect("the edited configuration is written");

    let applied = daemon
        .reload_configuration()
        .await
        .expect("the configuration reloads");
    assert!(
        applied.pending_restart.is_empty(),
        "still waiting on a restart: {:?}",
        applied.pending_restart
    );
    let advertised = daemon
        .set_peripheral_advertising(true, None)
        .await
        .expect("advertising starts")
        .expect("advertising offers an endpoint");
    assert_eq!(
        advertised.name.as_str(),
        "Stage Rig",
        "advertising uses the name the reload applied"
    );
}
