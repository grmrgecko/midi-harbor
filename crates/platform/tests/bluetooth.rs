//! The native Bluetooth backend against a real radio.
//!
//! Ignored by default because a build machine has no Bluetooth adapter. The rules the seam
//! promises are proved through the daemon, against the in-memory backend, in
//! `crates/daemon/tests/bluetooth.rs`; these check the native backend's answers on the air.

#![allow(clippy::expect_used)]

use midi_harbor_platform::bluetooth::{BluetoothPlatform, BluetoothRole};

/// Locks that the native backend reports the central role usable on a machine with a radio, and
/// scans; what it hears is printed for a person to judge.
///
/// Ignored by default because a build machine has no Bluetooth, and a test that fails for want of
/// hardware teaches nothing. Run it where there is a radio with:
///
/// ```text
/// cargo test -p midi-harbor-platform --test bluetooth -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs a Bluetooth adapter"]
fn the_native_backend_reports_what_the_radio_can_actually_do() {
    use midi_harbor_platform::bluetooth::native::NativeBluetooth;
    use std::time::Duration;

    let adapter = NativeBluetooth::start().expect("the backend starts even with no radio");

    // Opening the adapter is asynchronous, so the first answer may precede it.
    std::thread::sleep(Duration::from_secs(2));
    let central = adapter.unavailable(BluetoothRole::Central);
    println!("central: {central:?}");
    println!(
        "peripheral: {:?}",
        adapter.unavailable(BluetoothRole::Peripheral)
    );

    assert_eq!(
        central, None,
        "the central role must be usable on a machine with a radio"
    );

    adapter.start_scan().expect("a scan starts");
    std::thread::sleep(Duration::from_secs(10));
    adapter.stop_scan().expect("a scan stops");

    for found in adapter.in_range() {
        println!(
            "found {} name={:?} rssi={:?}",
            found.id, found.name, found.rssi
        );
    }
    for event in adapter.drain_events() {
        println!("event {event:?}");
    }
}

/// Locks that the native backend reports the peripheral role usable and starts advertising; what
/// reaches the air is checked by a person.
///
/// Ignored for the same reason as the test above. Watch what goes out with `btmon` while it runs:
/// the advertising data must carry the BLE MIDI service UUID, or no central will ever find us.
#[test]
#[ignore = "needs a Bluetooth adapter"]
fn the_advertised_port_reaches_the_air() {
    use midi_harbor_platform::bluetooth::native::NativeBluetooth;
    use std::time::Duration;

    let adapter = NativeBluetooth::start().expect("the backend starts even with no radio");
    std::thread::sleep(Duration::from_secs(2));

    let peripheral = adapter.unavailable(BluetoothRole::Peripheral);
    println!("peripheral: {peripheral:?}");
    assert_eq!(
        peripheral, None,
        "the peripheral role must be usable for this build to advertise"
    );

    adapter
        .advertise("Midi Harbor Test", None)
        .expect("advertising starts");
    std::thread::sleep(Duration::from_secs(12));

    for event in adapter.drain_events() {
        println!("event {event:?}");
    }

    adapter.stop_advertising().expect("advertising stops");
    std::thread::sleep(Duration::from_secs(2));
}
