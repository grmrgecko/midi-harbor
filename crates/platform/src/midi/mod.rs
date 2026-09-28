//! The MIDI platform seam.

#[cfg(target_os = "linux")]
pub mod alsa;
pub mod apple_setup;
#[cfg(target_os = "macos")]
pub mod coremidi;
pub mod ump;
pub mod usb;
#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub mod winmm;
pub mod winmm_identity;
#[cfg(windows)]
pub mod wms;

use crate::error::PlatformError;
use midi_harbor_core::endpoint::Direction;
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;
use std::fmt;

/// A backend's handle on one open platform endpoint.
///
/// Valid only for the lifetime of the backend that issued it. Deliberately not the domain's
/// `EndpointId`: handles are lost on restart and on replug, which is exactly why routes reference
/// the domain identity instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortHandle(u64);

impl PortHandle {
    /// Creates a handle from a backend-assigned value.
    pub fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// Returns the backend-assigned value.
    pub fn get(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for PortHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "port#{}", self.0)
    }
}

/// What to create when opening a virtual port.
///
/// A port has MIDI In connectors, which other applications send to, and MIDI Out connectors,
/// which they receive from, at least one of each. Several of one kind are numbered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualPortSpec {
    /// The name other applications will see.
    pub name: String,
    /// How many MIDI In connectors to create.
    pub inputs: u8,
    /// How many MIDI Out connectors to create.
    pub outputs: u8,
    /// The platform identifiers to pin on each MIDI In connector, in order, so each keeps its
    /// identity across restarts. Empty on first creation, after which the assigned values are
    /// stored and reused.
    pub pinned_inputs: Vec<u32>,
    /// The platform identifiers to pin on each MIDI Out connector, in order.
    pub pinned_outputs: Vec<u32>,
}

impl VirtualPortSpec {
    /// Creates a spec for a port with one connector of each kind and nothing pinned.
    pub fn simple(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            inputs: 1,
            outputs: 1,
            pinned_inputs: Vec::new(),
            pinned_outputs: Vec::new(),
        }
    }

    /// Returns the name of one MIDI In connector, as other applications see it.
    pub fn input_name(&self, index: u8) -> String {
        midi_harbor_core::endpoint::connector_name(&self.name, self.inputs, index)
    }

    /// Returns the name of one MIDI Out connector, as other applications see it.
    pub fn output_name(&self, index: u8) -> String {
        midi_harbor_core::endpoint::connector_name(&self.name, self.outputs, index)
    }
}

/// The platform identifiers a virtual port's connectors were given, to store for next time.
///
/// Empty where the platform has no such identifier, as ALSA does not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectorIds {
    /// Each MIDI In connector's identifier, in order.
    pub inputs: Vec<u32>,
    /// Each MIDI Out connector's identifier, in order.
    pub outputs: Vec<u32>,
}

/// MIDI hardware the backend can see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredDevice {
    /// The values used to recognise this hardware again after it is replugged.
    pub fingerprint: DeviceFingerprint,
    /// Which directions the hardware offers.
    pub direction: Direction,
    /// Names the application holding it exclusively, when one does.
    pub claimed_by: Option<String>,
    /// Whether this is another application's port rather than hardware.
    ///
    /// Hardware is remembered while it is unplugged, so its routes resume when it returns. An
    /// application's port is remembered only while a route names it, or every tool that ever
    /// opened one would stay in the configuration for good.
    pub software: bool,
}

/// Something the platform reported about the MIDI environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MidiPlatformEvent {
    /// Hardware or another application's endpoint appeared.
    DeviceAdded(DiscoveredDevice),
    /// It went away.
    DeviceRemoved(DeviceFingerprint),
    /// The environment changed in a way that warrants re-enumerating.
    SetupChanged,
    /// The platform's MIDI service stopped answering, and this process can no longer reach it.
    ///
    /// Apple's `MIDIServer` can crash, and when it does every CoreMIDI call in this process fails
    /// from then on, including creating a new client, even after launchd has started it again
    /// (R-079). Only a new process recovers, and nothing announces the loss, so the backend has to
    /// ask.
    ServerLost,
}

/// Creates and owns platform MIDI endpoints, and reports changes to the MIDI environment.
///
/// Implementations own a backend thread. On macOS that thread must run a CoreFoundation run loop,
/// because CoreMIDI delivers no notifications without one and fails silently in its absence; on
/// Linux it polls the ALSA sequencer descriptor. This is why the seam is a trait over a running
/// backend rather than a set of free functions.
pub trait MidiPlatform: Send + Sync {
    /// Creates a virtual port other applications can see, returning its handle and the platform
    /// identifiers to store for next time.
    ///
    /// `sinks` receive MIDI that other applications send into each MIDI In connector, one per
    /// connector in order; a connector with no sink carries nothing in. Each is written from the
    /// platform's own callback thread, which is a real-time context, so the backend must do
    /// nothing with it but stamp and push.
    fn create_virtual_port(
        &self,
        spec: &VirtualPortSpec,
        sinks: Vec<RtProducer>,
    ) -> Result<(PortHandle, ConnectorIds), PlatformError>;

    /// Sends messages out through an open endpoint.
    ///
    /// Called from an ordinary task rather than a real-time context, so it may allocate. The
    /// backend is responsible for not blocking longer than a send needs.
    fn send(&self, handle: PortHandle, messages: &[MidiMessage]) -> Result<(), PlatformError>;

    /// Sends one whole system-exclusive message out through an open endpoint.
    ///
    /// Separate from `send` because a dump is unbounded and carries its own framing, so it
    /// cannot travel as a fixed-size `MidiMessage`. The bytes include `0xF0` and `0xF7`; a
    /// backend that has to split them across platform packets must not reframe them.
    fn send_sysex(&self, handle: PortHandle, bytes: &[u8]) -> Result<(), PlatformError>;

    /// Sends messages out through one of an endpoint's MIDI Out connectors, counting from zero.
    ///
    /// Only a virtual port has more than one; for anything else the first is the endpoint itself.
    fn send_to(
        &self,
        handle: PortHandle,
        connector: u8,
        messages: &[MidiMessage],
    ) -> Result<(), PlatformError> {
        if connector == 0 {
            self.send(handle, messages)
        } else {
            Err(PlatformError::NotFound(format!(
                "{handle} MIDI Out {}",
                u16::from(connector) + 1
            )))
        }
    }

    /// Sends one whole system-exclusive message out through one of an endpoint's MIDI Out
    /// connectors, counting from zero.
    fn send_sysex_to(
        &self,
        handle: PortHandle,
        connector: u8,
        bytes: &[u8],
    ) -> Result<(), PlatformError> {
        if connector == 0 {
            self.send_sysex(handle, bytes)
        } else {
            Err(PlatformError::NotFound(format!(
                "{handle} MIDI Out {}",
                u16::from(connector) + 1
            )))
        }
    }

    /// Destroys a virtual port.
    fn destroy_virtual_port(&self, handle: PortHandle) -> Result<(), PlatformError>;

    /// Opens attached hardware for use, returning its handle.
    fn open_device(&self, fingerprint: &DeviceFingerprint) -> Result<PortHandle, PlatformError>;

    /// Opens attached hardware and routes what it sends into `sink`.
    ///
    /// Defaults to opening without a sink, so a backend that cannot yet receive still allows the
    /// device to be opened for output rather than failing outright.
    fn open_device_with_sink(
        &self,
        fingerprint: &DeviceFingerprint,
        sink: Option<RtProducer>,
    ) -> Result<PortHandle, PlatformError> {
        let _ = sink;
        self.open_device(fingerprint)
    }

    /// Closes a previously opened device.
    fn close_device(&self, handle: PortHandle) -> Result<(), PlatformError>;

    /// Lists the hardware currently attached.
    fn list_devices(&self) -> Result<Vec<DiscoveredDevice>, PlatformError>;

    /// Closes every port and device this backend holds, before the process exits.
    ///
    /// The daemon's handles are otherwise left for the operating system to close as the process
    /// ends. On Windows a daemon once stopped cleanly and then never finished exiting, one thread
    /// left and the process past killing, which is what a driver waiting in its own cleanup looks
    /// like (research R-090); closing everything first keeps that cleanup out of the process's
    /// teardown. Backends whose handles need nothing of the sort do nothing.
    fn shutdown(&self) {}

    /// Takes any platform events observed since the last call, oldest first.
    ///
    /// Draining rather than streaming keeps the seam free of a runtime dependency, so the same
    /// trait serves the async daemon and synchronous tests.
    fn drain_events(&self) -> Vec<MidiPlatformEvent>;
}
