//! What this build, on this machine, can actually do.

use crate::bluetooth::{BluetoothPlatform, BluetoothRole};
use midi_harbor_core::capability::{Capability, CapabilityName, CapabilitySet, UnavailableReason};

/// Reports the capabilities available, asking `bluetooth` about the radio when one is running.
///
/// Every capability is answered for, so the interface can present something unavailable as
/// unavailable rather than letting the user try it and meet a failure. An unanswered capability
/// would be a silent gap, which Principle IV forbids.
///
/// Bluetooth is the only capability that cannot be decided from the build alone: whether an
/// adapter exists, is switched on, and has been granted permission is known only to a started
/// backend. Without one, both Bluetooth roles report themselves unavailable, which is what a
/// build with no backend should say.
pub fn query_with(bluetooth: Option<&dyn BluetoothPlatform>) -> CapabilitySet {
    CapabilitySet::new(
        CapabilityName::all()
            .into_iter()
            .map(|name| assess(name, bluetooth))
            .collect(),
    )
}

/// Assesses one capability on the current platform.
fn assess(name: CapabilityName, bluetooth: Option<&dyn BluetoothPlatform>) -> Capability {
    match name {
        CapabilityName::VirtualPorts => match virtual_ports_missing() {
            Some(reason) => Capability::unavailable(name, reason),
            None => Capability::available(name),
        },

        // Devices and sessions are available wherever the project builds. Whether the service
        // can be installed is the service manager's question, which the daemon answers.
        CapabilityName::PhysicalDevices
        | CapabilityName::NetworkSessions
        | CapabilityName::ServiceManager => Capability::available(name),

        CapabilityName::MdnsResponder => match responder_missing() {
            Some(reason) => Capability::unavailable(name, reason),
            None => Capability::available(name),
        },

        CapabilityName::BluetoothCentral => role(name, bluetooth, BluetoothRole::Central),
        CapabilityName::BluetoothPeripheral => role(name, bluetooth, BluetoothRole::Peripheral),
    }
}

/// Reports why virtual ports cannot be created, when they cannot.
///
/// Windows creates them through Windows MIDI Services, whose app API Windows 11 includes from its
/// November 2026 update.
#[cfg(windows)]
fn virtual_ports_missing() -> Option<UnavailableReason> {
    crate::midi::wms::unavailable()
}

/// Reports why virtual ports cannot be created; CoreMIDI and ALSA always can.
#[cfg(not(windows))]
fn virtual_ports_missing() -> Option<UnavailableReason> {
    None
}

/// Where avahi-daemon listens for the clients that ask it to advertise.
#[cfg(target_os = "linux")]
const AVAHI_SOCKET: &str = "/run/avahi-daemon/socket";

/// Reports why sessions cannot be advertised, when they cannot.
///
/// Advertising goes through the system's responder. Linux has one only while avahi-daemon runs,
/// and without it this said "available" while every session went unannounced.
#[cfg(target_os = "linux")]
fn responder_missing() -> Option<UnavailableReason> {
    (!std::path::Path::new(AVAHI_SOCKET).exists()).then(|| {
        UnavailableReason::MissingSystemComponent {
            component: "avahi-daemon".to_owned(),
        }
    })
}

/// Reports why sessions cannot be advertised, when they cannot.
///
/// Windows has had a responder since the DNS Client service learned DNS-SD, and a release from
/// before then lacks the functions to register with it.
#[cfg(windows)]
fn responder_missing() -> Option<UnavailableReason> {
    (!crate::responder::present()).then(|| UnavailableReason::MissingSystemComponent {
        component: "DNS-SD in the DNS Client service".to_owned(),
    })
}

/// Reports why sessions cannot be advertised; the macOS responder is part of the system.
#[cfg(target_os = "macos")]
fn responder_missing() -> Option<UnavailableReason> {
    None
}

/// Reports one Bluetooth role, which only a started backend can answer for.
fn role(
    name: CapabilityName,
    bluetooth: Option<&dyn BluetoothPlatform>,
    role: BluetoothRole,
) -> Capability {
    match bluetooth {
        Some(backend) => match backend.unavailable(role) {
            Some(reason) => Capability::unavailable(name, reason),
            None => Capability::available(name),
        },
        None => Capability::unavailable(name, UnavailableReason::NotBuilt),
    }
}
