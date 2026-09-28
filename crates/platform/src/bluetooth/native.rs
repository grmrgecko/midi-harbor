//! The Bluetooth backend this build actually uses.
//!
//! The two roles come from different libraries — `btleplug` is explicitly central-only and its own
//! documentation sends peripheral users elsewhere — so they are assembled here into the one seam
//! the daemon talks to. A role with no backend yet reports itself unavailable rather than failing
//! when it is used.

use super::central::BtleplugCentral;
#[cfg(target_os = "linux")]
use super::peripheral_linux::BluerPeripheral;
#[cfg(target_os = "macos")]
use super::peripheral_macos::CoreBluetoothPeripheral;
use super::{
    BluetoothEvent, BluetoothPlatform, BluetoothRole, DiscoveredPeripheral, LinkHandle,
    PeripheralId,
};
use crate::error::PlatformError;
use midi_harbor_core::capability::UnavailableReason;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;

/// The peripheral backend this platform has, if any.
#[cfg(target_os = "linux")]
type Peripheral = BluerPeripheral;

/// The peripheral backend this platform has, if any.
#[cfg(target_os = "macos")]
type Peripheral = CoreBluetoothPeripheral;

/// The peripheral backend this platform has, if any.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
type Peripheral = Unbuilt;

/// The platform's Bluetooth backend.
pub struct NativeBluetooth {
    central: BtleplugCentral,
    peripheral: Peripheral,
}

impl NativeBluetooth {
    /// Starts the backends this platform has.
    pub fn start() -> Result<Self, PlatformError> {
        Ok(Self {
            central: BtleplugCentral::start()?,
            peripheral: Peripheral::start()?,
        })
    }

    /// Returns the peripherals currently advertising the MIDI service.
    pub fn in_range(&self) -> Vec<DiscoveredPeripheral> {
        self.central.in_range()
    }
}

impl BluetoothPlatform for NativeBluetooth {
    fn unavailable(&self, role: BluetoothRole) -> Option<UnavailableReason> {
        match role {
            BluetoothRole::Central => self.central.unavailable(),
            BluetoothRole::Peripheral => self.peripheral.unavailable(),
        }
    }

    fn start_scan(&self) -> Result<(), PlatformError> {
        self.central.start_scan()
    }

    fn stop_scan(&self) -> Result<(), PlatformError> {
        self.central.stop_scan()
    }

    fn connect(
        &self,
        id: &PeripheralId,
        sink: Option<RtProducer>,
    ) -> Result<LinkHandle, PlatformError> {
        self.central.connect(id, sink)
    }

    fn disconnect(&self, link: LinkHandle) -> Result<(), PlatformError> {
        self.central.disconnect(link)
    }

    fn send(&self, link: LinkHandle, messages: &[MidiMessage]) -> Result<(), PlatformError> {
        self.central.send(link, messages)
    }

    fn send_sysex(&self, link: LinkHandle, bytes: &[u8]) -> Result<(), PlatformError> {
        self.central.send_sysex(link, bytes)
    }

    fn advertise(&self, name: &str, sink: Option<RtProducer>) -> Result<(), PlatformError> {
        self.peripheral.advertise(name, sink)
    }

    fn notify(&self, messages: &[MidiMessage]) -> Result<(), PlatformError> {
        self.peripheral.notify(messages)
    }

    fn notify_sysex(&self, bytes: &[u8]) -> Result<(), PlatformError> {
        self.peripheral.notify_sysex(bytes)
    }

    fn stop_advertising(&self) -> Result<(), PlatformError> {
        self.peripheral.stop_advertising()
    }

    fn drain_events(&self) -> Vec<BluetoothEvent> {
        let mut events = self.central.drain_events();
        events.extend(self.peripheral.drain_events());
        events
    }
}

/// The peripheral role on a platform whose backend is not written yet.
///
/// Present so the seam has one shape everywhere: the role reports itself unavailable and every
/// request refuses, rather than the type disappearing and taking the method with it.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub struct Unbuilt;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl Unbuilt {
    /// Starts nothing.
    fn start() -> Result<Self, PlatformError> {
        Ok(Self)
    }

    /// Reports the role as absent from this build.
    fn unavailable(&self) -> Option<UnavailableReason> {
        Some(UnavailableReason::NotBuilt)
    }

    /// Refuses to advertise.
    fn advertise(&self, _name: &str, _sink: Option<RtProducer>) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(
            "advertising as a Bluetooth MIDI peripheral",
        ))
    }

    /// Refuses to notify, since nothing can be subscribed.
    fn notify(&self, _messages: &[MidiMessage]) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(
            "advertising as a Bluetooth MIDI peripheral",
        ))
    }

    /// Refuses to notify, since nothing can be subscribed.
    fn notify_sysex(&self, _bytes: &[u8]) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(
            "advertising as a Bluetooth MIDI peripheral",
        ))
    }

    /// Succeeds, because nothing was advertised to stop.
    fn stop_advertising(&self) -> Result<(), PlatformError> {
        Ok(())
    }

    /// Reports nothing, because nothing is running.
    fn drain_events(&self) -> Vec<BluetoothEvent> {
        Vec::new()
    }
}
