//! Platform seams for MIDI, Bluetooth, and system events.
//!
//! Every `cfg(target_os)` in the project lives under this crate. Core logic talks to the traits
//! defined here, and the in-memory fakes let the whole system be exercised on a machine with no
//! MIDI hardware, no network peer and no Bluetooth radio.

#[cfg(target_os = "macos")]
pub mod appkit;
pub mod bluetooth;
pub mod capability;
pub mod console;
#[cfg(windows)]
pub mod dll;
pub mod error;
pub mod fake;
pub mod midi;
pub mod notify;
pub mod responder;
pub mod socket;
#[cfg(windows)]
pub mod stop;
pub mod sysevents;

pub use bluetooth::{
    BluetoothEvent, BluetoothPlatform, BluetoothRole, DiscoveredPeripheral, LinkHandle,
    PeripheralId,
};
pub use error::PlatformError;
pub use midi::{MidiPlatform, MidiPlatformEvent, PortHandle};
pub use sysevents::{
    AddressWatch, ClockGap, ClockSample, CombinedSystemEvents, PolledSystemEvents, SystemEvent,
    SystemEvents,
};

use std::sync::Arc;

/// Returns the MIDI backend for this platform.
///
/// Falls back to the in-memory backend where no native one exists, so the daemon still runs and
/// the capability query reports honestly what is missing, rather than refusing to start.
pub fn midi_backend() -> Result<Arc<dyn MidiPlatform>, PlatformError> {
    #[cfg(target_os = "macos")]
    {
        Ok(Arc::new(midi::coremidi::CoreMidiPlatform::start()?))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Arc::new(midi::alsa::AlsaPlatform::start()?))
    }
    #[cfg(windows)]
    {
        Ok(Arc::new(midi::windows::WindowsMidiPlatform::start()?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        Ok(Arc::new(fake::FakeMidiPlatform::new()))
    }
}

/// Returns the Bluetooth backend for this platform.
///
/// Like `midi_backend`, this succeeds even where the hardware is missing: absence is reported
/// through the capability query, and a daemon that refused to start for want of a radio would
/// take MIDI down with it.
pub fn bluetooth_backend() -> Result<Arc<dyn BluetoothPlatform>, PlatformError> {
    Ok(Arc::new(bluetooth::native::NativeBluetooth::start()?))
}
