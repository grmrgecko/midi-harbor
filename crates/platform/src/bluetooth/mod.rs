//! The Bluetooth platform seam.
//!
//! Two roles live behind one trait. As a central this machine connects out to a BLE MIDI device;
//! as a peripheral it advertises itself so a phone or tablet can connect in. They are separate
//! capabilities because they fail separately: a Linux adapter may refuse to advertise while
//! scanning perfectly well, and macOS grants the peripheral role only to a bundled application.
//!
//! Peripheral identity is opaque above this seam. BlueZ names a device by its hardware address;
//! CoreBluetooth refuses to reveal one and issues a UUID that means something only on this Mac.
//! Nothing above may parse either.

#[cfg(target_os = "macos")]
pub mod authorization;
pub mod central;
pub mod native;
#[cfg(target_os = "linux")]
pub mod peripheral_linux;
#[cfg(target_os = "macos")]
pub mod peripheral_macos;

use crate::error::PlatformError;

/// How many bytes of name an advertisement can carry beside the MIDI service, where that is
/// limited.
///
/// macOS puts the whole advertisement in one 31-byte packet and never uses the scan response.
/// The flags take 3 bytes, the transmit power macOS always adds 3, and the 128-bit MIDI service
/// 18, which leaves 31 - 3 - 3 - 18 = 7, and a name's own header takes 2 of those. A longer name
/// is dropped without an error (research R-058). BlueZ sends a name of any length in the scan
/// response.
#[cfg(target_os = "macos")]
pub const ADVERTISED_NAME_ROOM: Option<usize> = Some(31 - 3 - 3 - 18 - 2);

/// How many bytes of name an advertisement can carry beside the MIDI service, where that is
/// limited.
#[cfg(not(target_os = "macos"))]
pub const ADVERTISED_NAME_ROOM: Option<usize> = None;

/// Reports whether an advertisement carrying this name actually sends it.
pub fn advertised_name_fits(name: &str) -> bool {
    ADVERTISED_NAME_ROOM.is_none_or(|room| name.len() <= room)
}
use midi_harbor_core::capability::UnavailableReason;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;
use std::fmt;

pub use midi_harbor_blemidi::codec::{CHARACTERISTIC_UUID, SERVICE_UUID};

/// A backend's handle on one open link.
///
/// Valid only for the lifetime of the backend that issued it, like `PortHandle`, and for the same
/// reason: a link does not survive a disconnection, while the endpoint that names it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinkHandle(u64);

impl LinkHandle {
    /// Creates a handle from a backend-assigned value.
    pub fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// Returns the backend-assigned value.
    pub fn get(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for LinkHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "link#{}", self.0)
    }
}

/// How the platform names one peripheral.
///
/// Stable enough to reconnect to and to store in the configuration, but meaningful only to the
/// backend that issued it and only on this machine.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeripheralId(String);

impl PeripheralId {
    /// Creates an identifier from whatever the backend uses to name a device.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the backend's own form of the identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PeripheralId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which side of a Bluetooth link this machine is playing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BluetoothRole {
    /// Connecting out to a device that advertises itself.
    Central,
    /// Advertising so a device can connect in.
    Peripheral,
}

impl fmt::Display for BluetoothRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Central => f.write_str("central"),
            Self::Peripheral => f.write_str("peripheral"),
        }
    }
}

/// A BLE MIDI peripheral the adapter can currently see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredPeripheral {
    /// What the backend calls it.
    pub id: PeripheralId,
    /// The advertised name, when it advertises one.
    pub name: Option<String>,
    /// Signal strength in dBm, which is the only distance information BLE offers.
    pub rssi: Option<i16>,
    /// Whether the operating system already has a bond with it.
    pub paired: bool,
}

/// Something the platform reported about the Bluetooth environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BluetoothEvent {
    /// A peripheral advertising the MIDI service came into range.
    PeripheralFound(DiscoveredPeripheral),
    /// One stopped advertising, or went out of range.
    PeripheralLost(PeripheralId),
    /// A link came up.
    Connected(PeripheralId),
    /// A link went down, whether by request or by the device walking away.
    Disconnected(PeripheralId),
    /// A link that was asked for never came up, and why.
    ///
    /// Apart from `Disconnected`, which a link that was up and carrying MIDI reports: one that
    /// never came up has nothing to silence, and its reason is what the user needs to read.
    LinkFailed {
        /// The device the link was asked for.
        id: PeripheralId,
        /// The closest reason the closed set allows; the radio's own is in the log.
        reason: midi_harbor_core::failure::FailureReason,
    },
    /// A central connected to us and subscribed, so it is now listening.
    CentralSubscribed(String),
    /// It stopped listening.
    CentralUnsubscribed(String),
    /// The adapter itself became usable or stopped being so.
    ///
    /// Carries the reason when unusable, because "Bluetooth is off" and "no adapter is present"
    /// call for different things from the person reading it.
    AdapterChanged(Option<UnavailableReason>),
}

/// Connects out to BLE MIDI devices, and advertises this machine as one.
///
/// Like `MidiPlatform`, implementations own a backend thread: CoreBluetooth delivers nothing
/// without a run loop, and BlueZ is reached over D-Bus, so the seam is a trait over a running
/// backend rather than a set of free functions.
pub trait BluetoothPlatform: Send + Sync {
    /// Reports why a role cannot be used right now, or `None` when it can.
    ///
    /// Answered per role rather than per adapter because the two fail independently, and a user
    /// told only "Bluetooth is unavailable" cannot tell which half they have lost.
    fn unavailable(&self, role: BluetoothRole) -> Option<UnavailableReason>;

    /// Begins looking for peripherals advertising the MIDI service.
    ///
    /// Scanning is expensive in power and in airtime, so it is started and stopped explicitly
    /// rather than run for as long as the daemon does.
    fn start_scan(&self) -> Result<(), PlatformError>;

    /// Stops looking.
    fn stop_scan(&self) -> Result<(), PlatformError>;

    /// Opens a link to a peripheral and subscribes to what it sends.
    ///
    /// `sink` is written from the platform's notification callback, which is a real-time context,
    /// so the backend decodes the packet and pushes, and does nothing else.
    fn connect(
        &self,
        id: &PeripheralId,
        sink: Option<RtProducer>,
    ) -> Result<LinkHandle, PlatformError>;

    /// Closes a link.
    fn disconnect(&self, link: LinkHandle) -> Result<(), PlatformError>;

    /// Sends messages over an open link.
    fn send(&self, link: LinkHandle, messages: &[MidiMessage]) -> Result<(), PlatformError>;

    /// Sends one whole system-exclusive message over an open link.
    ///
    /// Separate from `send` for the same reason as on the MIDI seam: a dump is unbounded and
    /// carries its own framing. The backend spreads it over as many packets as the link's MTU
    /// needs without reframing it.
    fn send_sysex(&self, link: LinkHandle, bytes: &[u8]) -> Result<(), PlatformError>;

    /// Advertises this machine as a BLE MIDI peripheral under `name`.
    ///
    /// `sink` receives what a connected central sends in, under the same real-time constraints as
    /// `connect`.
    fn advertise(&self, name: &str, sink: Option<RtProducer>) -> Result<(), PlatformError>;

    /// Sends messages to every central currently subscribed to our advertised port.
    fn notify(&self, messages: &[MidiMessage]) -> Result<(), PlatformError>;

    /// Sends one whole system-exclusive message to every central subscribed to our advertised
    /// port, spread over as many packets as it needs, as `send_sysex` does over a link.
    fn notify_sysex(&self, bytes: &[u8]) -> Result<(), PlatformError>;

    /// Stops advertising and drops any central still connected.
    fn stop_advertising(&self) -> Result<(), PlatformError>;

    /// Takes any platform events observed since the last call, oldest first.
    ///
    /// Draining rather than streaming keeps the seam free of a runtime dependency, exactly as on
    /// the MIDI seam.
    fn drain_events(&self) -> Vec<BluetoothEvent>;
}

#[cfg(all(test, target_os = "macos"))]
mod name_tests {
    use super::*;

    /// Locks the name room macOS leaves beside the MIDI service: 31 - 3 - 3 - 18 - 2 = 5 bytes,
    /// counted in UTF-8 bytes rather than characters.
    ///
    /// Measured: "HM" went out beside the service, and "Harbor Mac" was dropped without an error
    /// (research R-058). "Café!" is five characters but six bytes.
    #[test]
    fn macos_sends_a_name_of_five_bytes_beside_the_midi_service() {
        let cases = [
            ("two bytes", "HM", true),
            ("five bytes", "Stage", true),
            ("ten bytes", "Harbor Mac", false),
            ("five characters in six bytes", "Café!", false),
        ];
        for (name, advertised, want) in cases {
            assert_eq!(
                advertised_name_fits(advertised),
                want,
                "{name}: macOS drops a longer name without an error"
            );
        }
    }
}
