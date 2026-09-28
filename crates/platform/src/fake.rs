//! In-memory platform backends with programmable failures.
//!
//! These exist because the seams in this crate already exist for real reasons, not to make
//! something mockable. They are what lets every resilience behaviour be tested by inducing the
//! actual failure — a device removed, a permission denied, a machine suspended — on a CI runner
//! with no hardware at all.

use crate::bluetooth::{
    BluetoothEvent, BluetoothPlatform, BluetoothRole, DiscoveredPeripheral, LinkHandle,
    PeripheralId,
};
use crate::error::PlatformError;
use crate::midi::{
    ConnectorIds, DiscoveredDevice, MidiPlatform, MidiPlatformEvent, PortHandle, VirtualPortSpec,
};
use crate::sysevents::{SystemEvent, SystemEvents};
use midi_harbor_core::capability::UnavailableReason;
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;
use midi_harbor_core::stream::{Chunk, Scanner};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One thing sent out through an endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outgoing {
    /// A channel, system common or real-time message.
    Message(MidiMessage),
    /// A whole system-exclusive message, framing included.
    SysEx(Vec<u8>),
}

/// A failure the fake should produce instead of succeeding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Injected {
    /// Refuse to create any more ports.
    ResourceLimit,
    /// Report the named device as held by another application.
    Claimed(String),
    /// Refuse for want of a permission.
    PermissionDenied(String),
}

/// The mutable interior of the fake, kept behind one lock so state stays consistent.
#[derive(Default)]
struct Inner {
    ports: HashMap<u64, VirtualPortSpec>,
    /// What was sent out through each open endpoint, in order, so delivery and the order of it
    /// can both be asserted.
    outgoing: HashMap<u64, Vec<Outgoing>>,
    /// Sinks handed over at creation, kept so a test can push MIDI in as another application
    /// would.
    sinks: HashMap<u64, RtProducer>,
    /// One scanner per port, since running status and an open dump both span reads.
    scanners: HashMap<u64, Scanner>,
    /// Sinks for a virtual port's MIDI In connectors past the first, by port and connector.
    connector_sinks: HashMap<(u64, u8), RtProducer>,
    /// What was sent out through a virtual port's MIDI Out connectors past the first.
    connector_outgoing: HashMap<(u64, u8), Vec<Outgoing>>,
    open_devices: HashMap<u64, DeviceFingerprint>,
    attached: Vec<DiscoveredDevice>,
    events: Vec<MidiPlatformEvent>,
    next_handle: u64,
    next_unique_id: u32,
    injected: Option<Injected>,
    /// How long each open takes, as a backend waiting on its platform would.
    open_delay: std::time::Duration,
}

/// A MIDI platform that exists only in memory.
#[derive(Default)]
pub struct FakeMidiPlatform {
    inner: Mutex<Inner>,
}

impl FakeMidiPlatform {
    /// Creates an empty platform with no hardware attached.
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next operation fail in the given way, or clears any pending failure.
    pub fn inject(&self, failure: Option<Injected>) {
        self.with_inner(|inner| inner.injected = failure);
    }

    /// Makes every later port creation and device open take this long before it answers.
    pub fn delay_opens(&self, delay: std::time::Duration) {
        self.with_inner(|inner| inner.open_delay = delay);
    }

    /// Waits out the configured open delay, outside the lock so the rest of the fake still
    /// answers meanwhile.
    fn pause_for_open(&self) {
        let delay = self.with_inner(|inner| inner.open_delay);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }

    /// Simulates hardware being plugged in, emitting the arrival event a backend would.
    pub fn attach(&self, device: DiscoveredDevice) {
        self.with_inner(|inner| {
            inner.attached.push(device.clone());
            inner.events.push(MidiPlatformEvent::DeviceAdded(device));
            inner.events.push(MidiPlatformEvent::SetupChanged);
        });
    }

    /// Simulates the platform's MIDI service dying, emitting the event the backend's probe would.
    pub fn lose_server(&self) {
        self.with_inner(|inner| inner.events.push(MidiPlatformEvent::ServerLost));
    }

    /// Simulates another application letting go of a device it held exclusively.
    ///
    /// Emits nothing, because neither CoreMIDI nor ALSA announces this: the device was attached
    /// throughout, and only trying again reveals that it is free.
    pub fn release(&self, name: &str) {
        self.with_inner(|inner| {
            for device in inner
                .attached
                .iter_mut()
                .filter(|device| device.fingerprint.name == name)
            {
                device.claimed_by = None;
            }
        });
    }

    /// Simulates hardware being unplugged, emitting the removal event a backend would.
    pub fn detach(&self, name: &str) {
        self.with_inner(|inner| {
            let Some(index) = inner
                .attached
                .iter()
                .position(|d| d.fingerprint.name == name)
            else {
                return;
            };
            let device = inner.attached.remove(index);
            inner.open_devices.retain(|_, held| held.name != name);
            inner
                .events
                .push(MidiPlatformEvent::DeviceRemoved(device.fingerprint));
            inner.events.push(MidiPlatformEvent::SetupChanged);
        });
    }

    /// Returns the handle backing a named port, so a test can drive or inspect it.
    pub fn port_handle(&self, name: &str) -> Option<PortHandle> {
        self.with_inner(|inner| {
            inner
                .ports
                .iter()
                .find(|(_, spec)| spec.name == name)
                .map(|(handle, _)| PortHandle::from_raw(*handle))
        })
    }

    /// Returns the handle backing opened hardware, so a test can drive it.
    pub fn device_handle(&self, name: &str) -> Option<PortHandle> {
        self.with_inner(|inner| {
            inner
                .open_devices
                .iter()
                .find(|(_, fingerprint)| fingerprint.name == name)
                .map(|(handle, _)| PortHandle::from_raw(*handle))
        })
    }

    /// Returns everything sent out through an endpoint, in the order it was sent.
    pub fn outgoing(&self, handle: PortHandle) -> Vec<Outgoing> {
        self.with_inner(|inner| {
            inner
                .outgoing
                .get(&handle.get())
                .cloned()
                .unwrap_or_default()
        })
    }

    /// Returns the messages sent out through an endpoint, for asserting delivery.
    pub fn sent(&self, handle: PortHandle) -> Vec<MidiMessage> {
        self.outgoing(handle)
            .into_iter()
            .filter_map(|item| match item {
                Outgoing::Message(message) => Some(message),
                Outgoing::SysEx(_) => None,
            })
            .collect()
    }

    /// Returns the most recent thing sent out through an endpoint, without copying the rest.
    ///
    /// For timing: a measurement that copied everything sent so far on every look would hold the
    /// lock the daemon needs to deliver the very message being timed.
    pub fn last_sent(&self, handle: PortHandle) -> Option<Outgoing> {
        self.with_inner(|inner| {
            inner
                .outgoing
                .get(&handle.get())
                .and_then(|sent| sent.last().cloned())
        })
    }

    /// Returns the system-exclusive messages sent out through an endpoint.
    pub fn sent_sysex(&self, handle: PortHandle) -> Vec<Vec<u8>> {
        self.outgoing(handle)
            .into_iter()
            .filter_map(|item| match item {
                Outgoing::SysEx(bytes) => Some(bytes),
                Outgoing::Message(_) => None,
            })
            .collect()
    }

    /// Pushes raw bytes into a port as another application would, scanning them as a backend does.
    ///
    /// Takes bytes rather than messages so a test can send a dump, and splits them exactly where
    /// the caller splits them, so a message spanning two reads is a case a test can construct.
    pub fn feed_bytes(&self, handle: PortHandle, bytes: &[u8]) -> bool {
        self.with_inner(|inner| {
            let Some(sink) = inner.sinks.get_mut(&handle.get()) else {
                return false;
            };
            let scanner = inner.scanners.entry(handle.get()).or_default();
            let mut accepted = true;
            scanner.scan(bytes, &mut |chunk| match chunk {
                Chunk::Message(message) => accepted &= sink.push(message, 0),
                Chunk::SysEx { bytes, end } => accepted &= sink.push_sysex(bytes, end, 0),
            });
            accepted
        })
    }

    /// Pushes MIDI into a port as another application would, for driving the data path.
    pub fn feed(&self, handle: PortHandle, messages: &[MidiMessage]) -> bool {
        self.with_inner(|inner| {
            let Some(sink) = inner.sinks.get_mut(&handle.get()) else {
                return false;
            };
            messages.iter().all(|message| sink.push(*message, 0))
        })
    }

    /// Pushes MIDI into one of a port's MIDI In connectors, counting from zero, as another
    /// application would.
    pub fn feed_connector(
        &self,
        handle: PortHandle,
        connector: u8,
        messages: &[MidiMessage],
    ) -> bool {
        if connector == 0 {
            return self.feed(handle, messages);
        }
        self.with_inner(|inner| {
            let Some(sink) = inner.connector_sinks.get_mut(&(handle.get(), connector)) else {
                return false;
            };
            messages.iter().all(|message| sink.push(*message, 0))
        })
    }

    /// Returns the messages sent out through one of a port's MIDI Out connectors, counting from
    /// zero.
    pub fn sent_through(&self, handle: PortHandle, connector: u8) -> Vec<MidiMessage> {
        if connector == 0 {
            return self.sent(handle);
        }
        self.with_inner(|inner| {
            inner
                .connector_outgoing
                .get(&(handle.get(), connector))
                .map(|sent| {
                    sent.iter()
                        .filter_map(|item| match item {
                            Outgoing::Message(message) => Some(*message),
                            Outgoing::SysEx(_) => None,
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// Returns the connector counts a port was created with.
    pub fn connectors(&self, handle: PortHandle) -> Option<(u8, u8)> {
        self.with_inner(|inner| {
            inner
                .ports
                .get(&handle.get())
                .map(|spec| (spec.inputs, spec.outputs))
        })
    }

    /// Returns the names of the virtual ports that currently exist.
    pub fn port_names(&self) -> Vec<String> {
        let mut names = self.with_inner(|inner| {
            inner
                .ports
                .values()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        });
        names.sort();
        names
    }

    /// Records something sent through a MIDI Out connector past a port's first, failing for a
    /// connector the port does not have.
    fn with_outgoing(
        &self,
        handle: PortHandle,
        connector: u8,
        record: impl FnOnce(&mut Vec<Outgoing>),
    ) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            let has = inner
                .ports
                .get(&handle.get())
                .is_some_and(|spec| connector < spec.outputs);
            if !has {
                return Err(PlatformError::NotFound(format!(
                    "{handle} MIDI Out {}",
                    u16::from(connector) + 1
                )));
            }
            record(
                inner
                    .connector_outgoing
                    .entry((handle.get(), connector))
                    .or_default(),
            );
            Ok(())
        })
    }

    /// Runs `f` against the interior, recovering rather than propagating a poisoned lock.
    fn with_inner<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        match self.inner.lock() {
            Ok(mut guard) => f(&mut guard),
            // A poisoned lock means a test panicked mid-operation. Continuing on the recovered
            // state keeps the original failure readable instead of masking it with a second one.
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }

    /// Converts a pending injected failure into an error, consuming it.
    fn take_injected(inner: &mut Inner) -> Option<PlatformError> {
        match inner.injected.take()? {
            Injected::ResourceLimit => Some(PlatformError::ResourceLimit),
            Injected::Claimed(by) => Some(PlatformError::Claimed { by: Some(by) }),
            Injected::PermissionDenied(what) => Some(PlatformError::PermissionDenied { what }),
        }
    }
}

impl MidiPlatform for FakeMidiPlatform {
    fn create_virtual_port(
        &self,
        spec: &VirtualPortSpec,
        sinks: Vec<RtProducer>,
    ) -> Result<(PortHandle, ConnectorIds), PlatformError> {
        self.pause_for_open();
        self.with_inner(|inner| {
            if let Some(error) = Self::take_injected(inner) {
                return Err(error);
            }
            if inner
                .ports
                .values()
                .any(|existing| existing.name == spec.name)
            {
                return Err(PlatformError::NameConflict(spec.name.clone()));
            }

            // Honour pinned identifiers so the fake exercises the same restart path as the real
            // backends, where reusing the stored values is what keeps routes bound.
            let mut assign = |count: u8, pinned: &[u32]| -> Vec<u32> {
                (0..count)
                    .map(|index| match pinned.get(usize::from(index)) {
                        Some(id) => *id,
                        None => {
                            inner.next_unique_id = inner.next_unique_id.saturating_add(1);
                            inner.next_unique_id
                        }
                    })
                    .collect()
            };
            let ids = ConnectorIds {
                inputs: assign(spec.inputs, &spec.pinned_inputs),
                outputs: assign(spec.outputs, &spec.pinned_outputs),
            };

            inner.next_handle = inner.next_handle.saturating_add(1);
            let handle = PortHandle::from_raw(inner.next_handle);
            inner.ports.insert(handle.get(), spec.clone());
            for (index, sink) in sinks.into_iter().enumerate().take(usize::from(spec.inputs)) {
                match u8::try_from(index) {
                    Ok(0) => {
                        let _ = inner.sinks.insert(handle.get(), sink);
                    }
                    Ok(connector) => {
                        let _ = inner
                            .connector_sinks
                            .insert((handle.get(), connector), sink);
                    }
                    Err(_) => {}
                }
            }
            Ok((handle, ids))
        })
    }

    fn destroy_virtual_port(&self, handle: PortHandle) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            let _ = inner.sinks.remove(&handle.get());
            inner
                .connector_sinks
                .retain(|(port, _), _| *port != handle.get());
            match inner.ports.remove(&handle.get()) {
                Some(_) => Ok(()),
                None => Err(PlatformError::NotFound(handle.to_string())),
            }
        })
    }

    fn send(&self, handle: PortHandle, messages: &[MidiMessage]) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            if !inner.ports.contains_key(&handle.get())
                && !inner.open_devices.contains_key(&handle.get())
            {
                return Err(PlatformError::NotFound(handle.to_string()));
            }
            inner
                .outgoing
                .entry(handle.get())
                .or_default()
                .extend(messages.iter().copied().map(Outgoing::Message));
            Ok(())
        })
    }

    fn send_sysex(&self, handle: PortHandle, bytes: &[u8]) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            if !inner.ports.contains_key(&handle.get())
                && !inner.open_devices.contains_key(&handle.get())
            {
                return Err(PlatformError::NotFound(handle.to_string()));
            }
            inner
                .outgoing
                .entry(handle.get())
                .or_default()
                .push(Outgoing::SysEx(bytes.to_vec()));
            Ok(())
        })
    }

    fn send_to(
        &self,
        handle: PortHandle,
        connector: u8,
        messages: &[MidiMessage],
    ) -> Result<(), PlatformError> {
        if connector == 0 {
            return self.send(handle, messages);
        }
        self.with_outgoing(handle, connector, |sent| {
            sent.extend(messages.iter().copied().map(Outgoing::Message));
        })
    }

    fn send_sysex_to(
        &self,
        handle: PortHandle,
        connector: u8,
        bytes: &[u8],
    ) -> Result<(), PlatformError> {
        if connector == 0 {
            return self.send_sysex(handle, bytes);
        }
        self.with_outgoing(handle, connector, |sent| {
            sent.push(Outgoing::SysEx(bytes.to_vec()));
        })
    }

    fn open_device(&self, fingerprint: &DeviceFingerprint) -> Result<PortHandle, PlatformError> {
        self.with_inner(|inner| {
            if let Some(error) = Self::take_injected(inner) {
                return Err(error);
            }
            let Some(device) = inner
                .attached
                .iter()
                .find(|d| d.fingerprint.name == fingerprint.name)
                .cloned()
            else {
                return Err(PlatformError::NotFound(fingerprint.name.clone()));
            };
            if let Some(holder) = device.claimed_by {
                return Err(PlatformError::Claimed { by: Some(holder) });
            }

            inner.next_handle = inner.next_handle.saturating_add(1);
            let handle = PortHandle::from_raw(inner.next_handle);
            inner.open_devices.insert(handle.get(), device.fingerprint);
            Ok(handle)
        })
    }

    fn open_device_with_sink(
        &self,
        fingerprint: &DeviceFingerprint,
        sink: Option<RtProducer>,
    ) -> Result<PortHandle, PlatformError> {
        // Overridden rather than left to the default, which drops the sink: without this a test
        // can open hardware but never make it send anything, which is the blind spot that let a
        // backend ship with no device input at all.
        self.pause_for_open();
        let handle = self.open_device(fingerprint)?;
        self.with_inner(|inner| {
            if let Some(sink) = sink {
                let _ = inner.sinks.insert(handle.get(), sink);
            }
        });
        Ok(handle)
    }

    fn close_device(&self, handle: PortHandle) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            let _ = inner.sinks.remove(&handle.get());
            let _ = inner.scanners.remove(&handle.get());
            match inner.open_devices.remove(&handle.get()) {
                Some(_) => Ok(()),
                None => Err(PlatformError::NotFound(handle.to_string())),
            }
        })
    }

    fn list_devices(&self) -> Result<Vec<DiscoveredDevice>, PlatformError> {
        self.with_inner(|inner| {
            if let Some(error) = Self::take_injected(inner) {
                return Err(error);
            }
            Ok(inner.attached.clone())
        })
    }

    fn drain_events(&self) -> Vec<MidiPlatformEvent> {
        self.with_inner(|inner| std::mem::take(&mut inner.events))
    }
}

/// The mutable interior of the Bluetooth fake.
#[derive(Default)]
struct BluetoothInner {
    /// Peripherals currently in range, whether or not a scan is running.
    in_range: Vec<DiscoveredPeripheral>,
    /// Links this machine has open as a central.
    links: HashMap<u64, PeripheralId>,
    /// What was sent over each link, in order.
    outgoing: HashMap<u64, Vec<Outgoing>>,
    /// Sinks for what each connected peripheral sends back.
    sinks: HashMap<u64, RtProducer>,
    /// What was sent to subscribed centrals while advertising.
    notified: Vec<Outgoing>,
    /// The sink for what a connected central sends in.
    advertised_sink: Option<RtProducer>,
    /// The name being advertised, when advertising.
    advertised: Option<String>,
    /// Whether a scan is running, so a test can assert one was started and stopped.
    scanning: bool,
    /// Why the next link asked for will fail, once accepted, as a real radio's does.
    next_link_fails: Option<midi_harbor_core::failure::FailureReason>,
    events: Vec<BluetoothEvent>,
    next_handle: u64,
    /// Reasons a role is refused, which is how permission and adapter failures are induced.
    refused: HashMap<&'static str, UnavailableReason>,
}

/// A Bluetooth platform that exists only in memory.
///
/// The radio is the one seam that cannot be exercised on a build machine at all, and in practice
/// not reliably on a development machine either: two adapters a room apart hear each other at the
/// noise floor. Everything above this seam is therefore tested here.
#[derive(Default)]
pub struct FakeBluetoothPlatform {
    inner: Mutex<BluetoothInner>,
}

impl FakeBluetoothPlatform {
    /// Creates an adapter with nothing in range.
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next link asked for fail after it has been accepted, for the given reason.
    ///
    /// A real radio accepts the request and answers later, when the device does not reply or
    /// refuses, so the failure arrives as an event rather than as an error from `connect`.
    pub fn fail_next_link(&self, reason: midi_harbor_core::failure::FailureReason) {
        self.with_inner(|inner| inner.next_link_fails = Some(reason));
    }

    /// Refuses a role for the given reason, as an adapter without permission would.
    pub fn refuse(&self, role: BluetoothRole, reason: UnavailableReason) {
        self.with_inner(|inner| {
            inner.refused.insert(role_key(role), reason.clone());
            inner
                .events
                .push(BluetoothEvent::AdapterChanged(Some(reason)));
        });
    }

    /// Simulates a peripheral coming into range and advertising the MIDI service.
    ///
    /// Reported only while scanning, as a real radio reports it: nothing hears an advertisement
    /// it is not listening for. Reporting it regardless let a daemon that never scanned for a
    /// lost device appear to reconnect it.
    pub fn bring_into_range(&self, peripheral: DiscoveredPeripheral) {
        self.with_inner(|inner| {
            inner.in_range.retain(|known| known.id != peripheral.id);
            inner.in_range.push(peripheral.clone());
            if inner.scanning {
                inner
                    .events
                    .push(BluetoothEvent::PeripheralFound(peripheral));
            }
        });
    }

    /// Reports a link to a peripheral as closed with no link open, as a real radio does after a
    /// link has already failed or been closed by this side.
    pub fn report_disconnected(&self, id: &PeripheralId) {
        self.with_inner(|inner| inner.events.push(BluetoothEvent::Disconnected(id.clone())));
    }

    /// Simulates a peripheral walking out of range, dropping any link to it.
    pub fn take_out_of_range(&self, id: &PeripheralId) {
        self.with_inner(|inner| {
            inner.in_range.retain(|known| &known.id != id);
            let lost: Vec<u64> = inner
                .links
                .iter()
                .filter(|(_, held)| *held == id)
                .map(|(handle, _)| *handle)
                .collect();
            for handle in lost {
                inner.links.remove(&handle);
                inner.sinks.remove(&handle);
                inner.events.push(BluetoothEvent::Disconnected(id.clone()));
            }
            inner
                .events
                .push(BluetoothEvent::PeripheralLost(id.clone()));
        });
    }

    /// Simulates a central connecting to our advertised port and subscribing.
    pub fn central_subscribes(&self, who: &str) {
        self.with_inner(|inner| {
            inner
                .events
                .push(BluetoothEvent::CentralSubscribed(who.to_owned()));
        });
    }

    /// Simulates a central that was subscribed to the advertised port leaving it.
    pub fn central_leaves(&self, who: &str) {
        self.with_inner(|inner| {
            inner
                .events
                .push(BluetoothEvent::CentralUnsubscribed(who.to_owned()));
        });
    }

    /// Returns the link handle issued for a peripheral, so a test can play its part.
    pub fn link_for(&self, id: &PeripheralId) -> Option<LinkHandle> {
        self.with_inner(|inner| {
            inner
                .links
                .iter()
                .find(|(_, held)| *held == id)
                .map(|(handle, _)| LinkHandle::from_raw(*handle))
        })
    }

    /// Returns how many links are open to a peripheral.
    ///
    /// More than one is a fault: each delivers what the device sends, so every message arrives
    /// once per link.
    pub fn links_to(&self, id: &PeripheralId) -> usize {
        self.with_inner(|inner| inner.links.values().filter(|held| *held == id).count())
    }

    /// Returns what was sent over a link, in order.
    pub fn sent(&self, link: LinkHandle) -> Vec<Outgoing> {
        self.with_inner(|inner| inner.outgoing.get(&link.get()).cloned().unwrap_or_default())
    }

    /// Returns what was sent to subscribed centrals, in order.
    pub fn notified(&self) -> Vec<Outgoing> {
        self.with_inner(|inner| inner.notified.clone())
    }

    /// Returns the name being advertised, if any.
    pub fn advertised_name(&self) -> Option<String> {
        self.with_inner(|inner| inner.advertised.clone())
    }

    /// Reports whether a scan is running.
    pub fn is_scanning(&self) -> bool {
        self.with_inner(|inner| inner.scanning)
    }

    /// Pushes MIDI in as a connected peripheral would send it.
    pub fn peripheral_sends(&self, link: LinkHandle, message: MidiMessage, timestamp: u64) -> bool {
        self.with_inner(|inner| {
            inner
                .sinks
                .get_mut(&link.get())
                .is_some_and(|sink| sink.push(message, timestamp))
        })
    }

    /// Delivers a packet that could not be decoded, as a connected peripheral might send one.
    pub fn peripheral_sends_malformed(&self, link: LinkHandle) -> bool {
        self.with_inner(|inner| {
            inner
                .sinks
                .get(&link.get())
                .map(RtProducer::record_malformed)
                .is_some()
        })
    }

    /// Pushes MIDI in as a connected central would send it to our advertised port.
    pub fn central_sends(&self, message: MidiMessage, timestamp: u64) -> bool {
        self.with_inner(|inner| {
            inner
                .advertised_sink
                .as_mut()
                .is_some_and(|sink| sink.push(message, timestamp))
        })
    }

    /// Runs `body` against the interior, recovering rather than propagating a poisoned lock.
    fn with_inner<T>(&self, body: impl FnOnce(&mut BluetoothInner) -> T) -> T {
        match self.inner.lock() {
            Ok(mut guard) => body(&mut guard),
            Err(poisoned) => body(&mut poisoned.into_inner()),
        }
    }
}

/// The key a refused role is stored under.
fn role_key(role: BluetoothRole) -> &'static str {
    match role {
        BluetoothRole::Central => "central",
        BluetoothRole::Peripheral => "peripheral",
    }
}

impl BluetoothPlatform for FakeBluetoothPlatform {
    fn unavailable(&self, role: BluetoothRole) -> Option<UnavailableReason> {
        self.with_inner(|inner| inner.refused.get(role_key(role)).cloned())
    }

    fn start_scan(&self) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            inner.scanning = true;
            // Everything already in range is reported again, because a scan that started late
            // must not miss a device that was advertising before it.
            let found: Vec<BluetoothEvent> = inner
                .in_range
                .iter()
                .cloned()
                .map(BluetoothEvent::PeripheralFound)
                .collect();
            inner.events.extend(found);
            Ok(())
        })
    }

    fn stop_scan(&self) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            inner.scanning = false;
            Ok(())
        })
    }

    fn connect(
        &self,
        id: &PeripheralId,
        sink: Option<RtProducer>,
    ) -> Result<LinkHandle, PlatformError> {
        self.with_inner(|inner| {
            if !inner.in_range.iter().any(|known| &known.id == id) {
                return Err(PlatformError::NotFound(id.to_string()));
            }
            inner.next_handle = inner.next_handle.saturating_add(1);
            let handle = inner.next_handle;
            if let Some(reason) = inner.next_link_fails.take() {
                inner.events.push(BluetoothEvent::LinkFailed {
                    id: id.clone(),
                    reason,
                });
                return Ok(LinkHandle::from_raw(handle));
            }
            inner.links.insert(handle, id.clone());
            if let Some(sink) = sink {
                inner.sinks.insert(handle, sink);
            }
            inner.events.push(BluetoothEvent::Connected(id.clone()));
            Ok(LinkHandle::from_raw(handle))
        })
    }

    fn disconnect(&self, link: LinkHandle) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            let Some(id) = inner.links.remove(&link.get()) else {
                return Err(PlatformError::NotFound(link.to_string()));
            };
            inner.sinks.remove(&link.get());
            inner.events.push(BluetoothEvent::Disconnected(id));
            Ok(())
        })
    }

    fn send(&self, link: LinkHandle, messages: &[MidiMessage]) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            if !inner.links.contains_key(&link.get()) {
                return Err(PlatformError::NotFound(link.to_string()));
            }
            let out = inner.outgoing.entry(link.get()).or_default();
            out.extend(messages.iter().copied().map(Outgoing::Message));
            Ok(())
        })
    }

    fn send_sysex(&self, link: LinkHandle, bytes: &[u8]) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            if !inner.links.contains_key(&link.get()) {
                return Err(PlatformError::NotFound(link.to_string()));
            }
            inner
                .outgoing
                .entry(link.get())
                .or_default()
                .push(Outgoing::SysEx(bytes.to_vec()));
            Ok(())
        })
    }

    fn advertise(&self, name: &str, sink: Option<RtProducer>) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            if let Some(reason) = inner.refused.get(role_key(BluetoothRole::Peripheral)) {
                return Err(match reason {
                    UnavailableReason::PermissionDenied { what } => {
                        PlatformError::PermissionDenied { what: what.clone() }
                    }
                    _ => PlatformError::AdapterUnavailable,
                });
            }
            inner.advertised = Some(name.to_owned());
            inner.advertised_sink = sink;
            Ok(())
        })
    }

    fn notify(&self, messages: &[MidiMessage]) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            if inner.advertised.is_none() {
                return Err(PlatformError::NotFound("no advertised port".to_owned()));
            }
            inner
                .notified
                .extend(messages.iter().copied().map(Outgoing::Message));
            Ok(())
        })
    }

    fn notify_sysex(&self, bytes: &[u8]) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            if inner.advertised.is_none() {
                return Err(PlatformError::NotFound("no advertised port".to_owned()));
            }
            inner.notified.push(Outgoing::SysEx(bytes.to_vec()));
            Ok(())
        })
    }

    fn stop_advertising(&self) -> Result<(), PlatformError> {
        self.with_inner(|inner| {
            inner.advertised = None;
            inner.advertised_sink = None;
            Ok(())
        })
    }

    fn drain_events(&self) -> Vec<BluetoothEvent> {
        self.with_inner(|inner| std::mem::take(&mut inner.events))
    }
}

/// A system event source driven by tests rather than by the operating system.
#[derive(Default)]
pub struct FakeSystemEvents {
    events: Mutex<Vec<SystemEvent>>,
    /// How many times the daemon has said a suspend may go ahead.
    readied: std::sync::atomic::AtomicUsize,
    /// Signalled on a suspend, as the native backends do.
    urgent: Arc<tokio::sync::Notify>,
}

impl FakeSystemEvents {
    /// Creates a source that has reported nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues an event as the operating system would report it.
    pub fn emit(&self, event: SystemEvent) {
        if let Ok(mut guard) = self.events.lock() {
            guard.push(event);
        }
        if event == SystemEvent::Suspending {
            self.urgent.notify_one();
        }
    }

    /// Simulates a full suspend and resume cycle, reported together as the polled watcher reports
    /// a sleep it only noticed on waking.
    ///
    /// Both events are queued under one lock, so the daemon cannot drain the suspend alone and
    /// take it for a sleep still to come.
    pub fn sleep_and_wake(&self) {
        if let Ok(mut guard) = self.events.lock() {
            guard.push(SystemEvent::Suspending);
            guard.push(SystemEvent::Resumed);
        }
        self.urgent.notify_one();
    }

    /// Returns how many times the daemon has said a suspend may go ahead.
    pub fn times_readied(&self) -> usize {
        self.readied.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl SystemEvents for FakeSystemEvents {
    fn drain_events(&self) -> Vec<SystemEvent> {
        match self.events.lock() {
            Ok(mut guard) => std::mem::take(&mut guard),
            Err(poisoned) => std::mem::take(&mut poisoned.into_inner()),
        }
    }

    fn ready_for_sleep(&self) {
        let _ = self
            .readied
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn urgent(&self) -> Option<Arc<tokio::sync::Notify>> {
        Some(Arc::clone(&self.urgent))
    }
}
