//! The macOS MIDI backend.
//!
//! CoreMIDI delivers notifications only to a thread running a CoreFoundation run loop, and
//! delivers nothing at all, silently, without one. This backend therefore owns a dedicated
//! thread: it creates the client, runs the loop for the process lifetime, and serves requests
//! sent to it over a channel. Every CoreMIDI object stays on that thread.

use crate::error::PlatformError;
use crate::midi::{
    ConnectorIds, DiscoveredDevice, MidiPlatform, MidiPlatformEvent, PortHandle, VirtualPortSpec,
};
use coremidi::{
    AnyObject, Client, Destinations, Notification, Object, Sources, VirtualDestination,
    VirtualSource,
};
use midi_harbor_core::endpoint::Direction;
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;
use midi_harbor_core::stream::{Chunk, Scanner};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, error, warn};

/// How long each run-loop turn lasts when nothing asks for the thread sooner.
///
/// Every request wakes the run loop, so this bounds only how often the thread looks for requests
/// it was not woken for. Before requests woke it, this was the wait every send paid: 50 ms per
/// backend a message crossed, against a one-millisecond budget (SC-008).
const RUN_LOOP_SLICE: f64 = 0.05;

/// How often the run-loop thread checks that `MIDIServer` still answers.
///
/// Nothing reports the server dying, so this bounds how long the daemon goes without MIDI before
/// it notices. A check is one round trip to the server, and two seconds keeps that negligible.
const SERVER_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// The backend thread's run loop, held so a request can wake it.
///
/// Core Foundation documents `CFRunLoopStop` as callable from any thread, and it is the only call
/// made through this. The reference is retained for as long as it is held.
struct RunLoop(core_foundation_sys::runloop::CFRunLoopRef);

// SAFETY: the reference is only ever passed to `CFRunLoopStop` and `CFRelease`, both of which
// Core Foundation documents as safe from any thread, and it is retained while held.
unsafe impl Send for RunLoop {}
// SAFETY: as for `Send`; nothing reads or writes through the reference.
unsafe impl Sync for RunLoop {}

impl RunLoop {
    /// Wakes the run loop so its thread serves requests now rather than after its turn.
    fn wake(&self) {
        // SAFETY: the run loop is retained, so the reference is live. Stopping a run loop that is
        // not running makes its next run return at once, so a request cannot be missed.
        unsafe { core_foundation_sys::runloop::CFRunLoopStop(self.0) };
    }
}

impl Drop for RunLoop {
    fn drop(&mut self) {
        // SAFETY: balances the retain taken when the run loop was handed over.
        unsafe { core_foundation_sys::base::CFRelease(self.0.cast()) };
    }
}

/// The CoreMIDI property holding an endpoint's persistent identifier.
const UNIQUE_ID_PROPERTY: &str = "uniqueID";

/// The property CoreMIDI sets on an endpoint that is configured but not currently available.
const OFFLINE_PROPERTY: &str = "offline";

/// CoreMIDI's `kMIDIPropertyDriverOwner`, which an endpoint inherits from its device.
const DRIVER_PROPERTY: &str = "driver";

/// CoreMIDI's `kMIDIPropertyManufacturer`, which an endpoint inherits from its device.
const MANUFACTURER_PROPERTY: &str = "manufacturer";

/// CoreMIDI's `kMIDIPropertyModel`, which an endpoint inherits from its device.
const MODEL_PROPERTY: &str = "model";

/// Apple's drivers whose ports are software rather than hardware: IAC buses and network sessions.
const SOFTWARE_DRIVERS: [&str; 2] = [
    "com.apple.AppleMIDIIACDriver",
    "com.apple.AppleMIDIRTPDriver",
];

/// A request for the run-loop thread to carry out.
enum Request {
    CreatePort {
        spec: VirtualPortSpec,
        sinks: Vec<RtProducer>,
        reply: Sender<Result<(PortHandle, ConnectorIds), PlatformError>>,
    },
    Send {
        handle: PortHandle,
        connector: u8,
        messages: Vec<MidiMessage>,
        reply: Sender<Result<(), PlatformError>>,
    },
    SendSysEx {
        handle: PortHandle,
        connector: u8,
        bytes: Vec<u8>,
        reply: Sender<Result<(), PlatformError>>,
    },
    DestroyPort {
        handle: PortHandle,
        reply: Sender<Result<(), PlatformError>>,
    },
    ListDevices {
        reply: Sender<Vec<DiscoveredDevice>>,
    },
    OpenDevice {
        fingerprint: DeviceFingerprint,
        sink: Option<RtProducer>,
        reply: Sender<Result<PortHandle, PlatformError>>,
    },
    CloseDevice {
        handle: PortHandle,
        reply: Sender<Result<(), PlatformError>>,
    },
}

/// The endpoints owned by the run-loop thread.
#[derive(Default)]
struct Owned {
    /// Each virtual port's MIDI Out connectors, which applications receive from, in order.
    sources: HashMap<u64, Vec<VirtualSource>>,
    /// Each virtual port's MIDI In connectors, which applications send to, in order.
    destinations: HashMap<u64, Vec<VirtualDestination>>,
    /// Input ports connected to attached hardware, one per opened device.
    device_inputs: HashMap<u64, coremidi::InputPort>,
    /// Where each opened device sends its MIDI.
    device_outputs: HashMap<u64, (coremidi::OutputPort, coremidi::Destination)>,
    next_handle: u64,
}

/// Creates and owns CoreMIDI endpoints on a dedicated run-loop thread.
pub struct CoreMidiPlatform {
    requests: Sender<Request>,
    events: Arc<Mutex<Vec<MidiPlatformEvent>>>,
    run_loop: RunLoop,
}

impl CoreMidiPlatform {
    /// Starts the backend thread and waits for its client to come up.
    pub fn start() -> Result<Self, PlatformError> {
        let (requests, inbox) = mpsc::channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ready, started) = mpsc::channel();

        let thread_events = Arc::clone(&events);
        std::thread::Builder::new()
            .name("coremidi".to_owned())
            .spawn(move || run_loop_thread(inbox, thread_events, ready))
            .map_err(|error| PlatformError::Os {
                operation: "start the coremidi thread",
                detail: error.to_string(),
            })?;

        // Fail here rather than on first use, so a client problem is reported at startup.
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(run_loop)) => Ok(Self {
                requests,
                events,
                run_loop,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(PlatformError::Os {
                operation: "start the coremidi client",
                detail: "the backend thread did not report readiness".to_owned(),
            }),
        }
    }

    /// Sends a request to the backend thread and waits for its answer.
    fn ask<T>(&self, build: impl FnOnce(Sender<T>) -> Request) -> Result<T, PlatformError> {
        let (reply, answer) = mpsc::channel();
        self.requests
            .send(build(reply))
            .map_err(|_| PlatformError::Os {
                operation: "reach the coremidi thread",
                detail: "the backend thread has stopped".to_owned(),
            })?;
        self.run_loop.wake();
        answer
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| PlatformError::Os {
                operation: "wait for the coremidi thread",
                detail: "the backend thread did not answer".to_owned(),
            })
    }
}

impl MidiPlatform for CoreMidiPlatform {
    fn create_virtual_port(
        &self,
        spec: &VirtualPortSpec,
        sinks: Vec<RtProducer>,
    ) -> Result<(PortHandle, ConnectorIds), PlatformError> {
        let spec = spec.clone();
        self.ask(|reply| Request::CreatePort { spec, sinks, reply })?
    }

    fn send(&self, handle: PortHandle, messages: &[MidiMessage]) -> Result<(), PlatformError> {
        self.send_to(handle, 0, messages)
    }

    fn send_to(
        &self,
        handle: PortHandle,
        connector: u8,
        messages: &[MidiMessage],
    ) -> Result<(), PlatformError> {
        let messages = messages.to_vec();
        self.ask(|reply| Request::Send {
            handle,
            connector,
            messages,
            reply,
        })?
    }

    fn send_sysex_to(
        &self,
        handle: PortHandle,
        connector: u8,
        bytes: &[u8],
    ) -> Result<(), PlatformError> {
        let bytes = bytes.to_vec();
        self.ask(|reply| Request::SendSysEx {
            handle,
            connector,
            bytes,
            reply,
        })?
    }

    fn send_sysex(&self, handle: PortHandle, bytes: &[u8]) -> Result<(), PlatformError> {
        self.send_sysex_to(handle, 0, bytes)
    }

    fn destroy_virtual_port(&self, handle: PortHandle) -> Result<(), PlatformError> {
        self.ask(|reply| Request::DestroyPort { handle, reply })?
    }

    fn open_device(&self, fingerprint: &DeviceFingerprint) -> Result<PortHandle, PlatformError> {
        let fingerprint = fingerprint.clone();
        self.ask(|reply| Request::OpenDevice {
            fingerprint,
            sink: None,
            reply,
        })?
    }

    fn open_device_with_sink(
        &self,
        fingerprint: &DeviceFingerprint,
        sink: Option<RtProducer>,
    ) -> Result<PortHandle, PlatformError> {
        let fingerprint = fingerprint.clone();
        self.ask(|reply| Request::OpenDevice {
            fingerprint,
            sink,
            reply,
        })?
    }

    fn close_device(&self, handle: PortHandle) -> Result<(), PlatformError> {
        self.ask(|reply| Request::CloseDevice { handle, reply })?
    }

    fn list_devices(&self) -> Result<Vec<DiscoveredDevice>, PlatformError> {
        self.ask(|reply| Request::ListDevices { reply })
    }

    fn drain_events(&self) -> Vec<MidiPlatformEvent> {
        match self.events.lock() {
            Ok(mut guard) => std::mem::take(&mut guard),
            Err(poisoned) => std::mem::take(&mut poisoned.into_inner()),
        }
    }
}

/// Owns the CoreMIDI client and pumps its run loop.
fn run_loop_thread(
    inbox: Receiver<Request>,
    events: Arc<Mutex<Vec<MidiPlatformEvent>>>,
    ready: Sender<Result<RunLoop, PlatformError>>,
) {
    let notify_events = Arc::clone(&events);
    let client =
        match Client::new_with_notifications("Midi Harbor", move |notification: &Notification| {
            record_notification(&notify_events, notification);
        }) {
            Ok(client) => client,
            Err(status) => {
                let _ = ready.send(Err(PlatformError::Os {
                    operation: "create the coremidi client",
                    detail: format!("OSStatus {status}"),
                }));
                return;
            }
        };
    // SAFETY: `CFRunLoopGetCurrent` returns this thread's run loop, and the retain keeps it
    // alive for the platform that holds it.
    let run_loop = unsafe {
        let current = core_foundation_sys::runloop::CFRunLoopGetCurrent();
        core_foundation_sys::base::CFRetain(current.cast());
        RunLoop(current)
    };
    let _ = ready.send(Ok(run_loop));

    let mut owned = Owned::default();
    let mut probed_at = std::time::Instant::now();
    let mut server_lost = false;
    loop {
        // Serve every pending request, then give the run loop a turn so CoreMIDI can deliver
        // notifications. Without this turn no notification ever arrives.
        while let Ok(request) = inbox.try_recv() {
            match request {
                Request::CreatePort { spec, sinks, reply } => {
                    let _ = reply.send(create_port(&client, &mut owned, &spec, sinks));
                }
                Request::Send {
                    handle,
                    connector,
                    messages,
                    reply,
                } => {
                    let _ = reply.send(send_messages(&owned, handle, connector, &messages));
                }
                Request::SendSysEx {
                    handle,
                    connector,
                    bytes,
                    reply,
                } => {
                    let _ = reply.send(send_sysex(&owned, handle, connector, &bytes));
                }
                Request::DestroyPort { handle, reply } => {
                    let existed = owned.sources.remove(&handle.get()).is_some()
                        | owned.destinations.remove(&handle.get()).is_some();
                    let answer = if existed {
                        Ok(())
                    } else {
                        Err(PlatformError::NotFound(handle.to_string()))
                    };
                    let _ = reply.send(answer);
                }
                Request::ListDevices { reply } => {
                    let _ = reply.send(enumerate_devices());
                }
                Request::OpenDevice {
                    fingerprint,
                    sink,
                    reply,
                } => {
                    let _ = reply.send(open_device(&client, &mut owned, &fingerprint, sink));
                }
                Request::CloseDevice { handle, reply } => {
                    let existed = owned.device_inputs.remove(&handle.get()).is_some()
                        | owned.device_outputs.remove(&handle.get()).is_some();
                    let answer = if existed {
                        Ok(())
                    } else {
                        Err(PlatformError::NotFound(handle.to_string()))
                    };
                    let _ = reply.send(answer);
                }
            }
        }

        // Check the server still answers. Creating a port is the check because it reaches the
        // server and fails once it has gone, where cached reads such as the number of sources
        // go on answering with what they last saw. The port is private and dropped at once, so
        // no other application sees it.
        if !server_lost && probed_at.elapsed() >= SERVER_PROBE_INTERVAL {
            probed_at = std::time::Instant::now();
            if let Err(status) = client.output_port("Midi Harbor probe") {
                server_lost = true;
                error!(status, "the MIDI server stopped answering");
                if let Ok(mut guard) = events.lock() {
                    guard.push(MidiPlatformEvent::ServerLost);
                }
            }
        }

        unsafe {
            // SAFETY: CFRunLoopRunInMode takes a static mode constant and a duration, and runs
            // the calling thread's run loop. This thread owns that run loop and the CoreMIDI
            // client bound to it, so there is no cross-thread access to guard.
            core_foundation_sys::runloop::CFRunLoopRunInMode(
                core_foundation_sys::runloop::kCFRunLoopDefaultMode,
                RUN_LOOP_SLICE,
                0,
            );
        }
    }
}

/// Creates one virtual port's connectors on the run-loop thread.
///
/// Named from the routes' side, as the model names them: a MIDI In connector is one applications
/// send into, which CoreMIDI calls a destination, and a MIDI Out connector is one applications
/// receive from, a source. Every connector's identifier is pinned, so each keeps its identity
/// across restarts and the applications bound to it find it again.
fn create_port(
    client: &Client,
    owned: &mut Owned,
    spec: &VirtualPortSpec,
    sinks: Vec<RtProducer>,
) -> Result<(PortHandle, ConnectorIds), PlatformError> {
    owned.next_handle = owned.next_handle.saturating_add(1);
    let handle = PortHandle::from_raw(owned.next_handle);
    let mut ids = ConnectorIds::default();

    // MIDI Out first, so a port from before connectors pins its identity to the same endpoint.
    let mut sources = Vec::with_capacity(usize::from(spec.outputs));
    for index in 0..spec.outputs {
        let source = client
            .virtual_source(&spec.output_name(index))
            .map_err(|status| PlatformError::Os {
                operation: "create a virtual source",
                detail: format!("OSStatus {status}"),
            })?;
        let pinned = spec.pinned_outputs.get(usize::from(index)).copied();
        if let Some(id) = pin_or_read_unique_id(&source, pinned) {
            ids.outputs.push(id);
        }
        sources.push(source);
    }

    let mut sinks = sinks.into_iter();
    let mut destinations = Vec::with_capacity(usize::from(spec.inputs));
    for index in 0..spec.inputs {
        // This closure is the real-time path: CoreMIDI calls it on its own high-priority thread
        // whenever another application sends into the connector. It may not allocate, lock or
        // block, so it does nothing but decode and push, into a ring this connector alone owns.
        let mut sink = sinks.next();
        let mut scanner = Scanner::new();
        let destination = client
            .virtual_destination(&spec.input_name(index), move |packets| {
                let Some(sink) = sink.as_mut() else {
                    return;
                };
                for packet in packets.iter() {
                    push_packet(&mut scanner, sink, packet.timestamp(), packet.data());
                }
            })
            .map_err(|status| PlatformError::Os {
                operation: "create a virtual destination",
                detail: format!("OSStatus {status}"),
            })?;
        let pinned = spec.pinned_inputs.get(usize::from(index)).copied();
        if let Some(id) = pin_or_read_unique_id(&destination, pinned) {
            ids.inputs.push(id);
        }
        destinations.push(destination);
    }

    owned.sources.insert(handle.get(), sources);
    owned.destinations.insert(handle.get(), destinations);
    debug!(port = %spec.name, handle = %handle, inputs = spec.inputs, outputs = spec.outputs, "virtual endpoint created");
    Ok((handle, ids))
}

/// Decodes one packet's bytes and pushes what it finds.
///
/// Runs in a real-time callback: no allocation, no locking, no blocking, and no arithmetic that
/// can panic. The scanner outlives the packet because running status and an unfinished dump both
/// carry from one packet to the next — CoreMIDI splits a large dump across packets by design.
fn push_packet(scanner: &mut Scanner, sink: &mut RtProducer, timestamp: u64, data: &[u8]) {
    scanner.scan(data, &mut |chunk| match chunk {
        Chunk::Message(message) => {
            let _ = sink.push(message, timestamp);
        }
        Chunk::SysEx { bytes, end } => {
            let _ = sink.push_sysex(bytes, end, timestamp);
        }
    });
}

/// Opens attached hardware, connecting its input and preparing to send to its output.
///
/// A device with only one side is still opened: a keyboard that only sends and a sound module
/// that only receives are both ordinary, and refusing either would make them unroutable.
fn open_device(
    client: &Client,
    owned: &mut Owned,
    fingerprint: &DeviceFingerprint,
    sink: Option<RtProducer>,
) -> Result<PortHandle, PlatformError> {
    owned.next_handle = owned.next_handle.saturating_add(1);
    let handle = PortHandle::from_raw(owned.next_handle);
    let mut opened_anything = false;

    // Receiving from the device needs an input port connected to its source.
    if let Some(source) = find_source(fingerprint) {
        let mut sink = sink;
        let mut scanner = Scanner::new();
        let input = client
            .input_port("harbor-in", move |packets| {
                let Some(sink) = sink.as_mut() else {
                    return;
                };
                for packet in packets.iter() {
                    push_packet(&mut scanner, sink, packet.timestamp(), packet.data());
                }
            })
            .map_err(|status| PlatformError::Os {
                operation: "create an input port",
                detail: format!("OSStatus {status}"),
            })?;
        input
            .connect_source(&source)
            .map_err(|status| PlatformError::Os {
                operation: "connect to a device",
                detail: format!("OSStatus {status}"),
            })?;
        owned.device_inputs.insert(handle.get(), input);
        opened_anything = true;
    }

    // Sending to it needs an output port and the destination to aim at.
    if let Some(destination) = find_destination(fingerprint) {
        let output = client
            .output_port("harbor-out")
            .map_err(|status| PlatformError::Os {
                operation: "create an output port",
                detail: format!("OSStatus {status}"),
            })?;
        owned
            .device_outputs
            .insert(handle.get(), (output, destination));
        opened_anything = true;
    }

    if !opened_anything {
        return Err(PlatformError::NotFound(fingerprint.name.clone()));
    }
    debug!(device = %fingerprint.name, handle = %handle, "opened a device");
    Ok(handle)
}

/// Finds the source endpoint matching a fingerprint.
fn find_source(fingerprint: &DeviceFingerprint) -> Option<coremidi::Source> {
    Sources
        .into_iter()
        .find(|source| matches(source.unique_id(), source.display_name(), fingerprint))
}

/// Finds the destination endpoint matching a fingerprint.
///
/// A device's source and destination each have a unique identifier of their own, and the
/// fingerprint carries its source's. Matching destinations against that identifier found nothing
/// for any device with both, so routing MIDI to hardware failed with only a debug line to say so.
/// The two belong to one entity, which is how the destination is found. Endpoints without an
/// entity, such as another application's virtual ports, fall back to their shared name.
fn find_destination(fingerprint: &DeviceFingerprint) -> Option<coremidi::Destination> {
    if let Some(sibling) = fingerprint.unique_id.and_then(sibling_destination_id) {
        return Destinations
            .into_iter()
            .find(|destination| destination.unique_id() == Some(sibling));
    }
    Destinations.into_iter().find(|destination| {
        matches(
            destination.unique_id(),
            destination.display_name(),
            fingerprint,
        ) || destination.display_name().as_deref() == Some(fingerprint.name.as_str())
    })
}

/// Returns the unique identifier of the destination sharing an entity with the endpoint `id`
/// names, when it has one.
///
/// When `id` already names a destination, as it does for hardware that only receives, the entity
/// yields that same destination.
fn sibling_destination_id(id: u32) -> Option<u32> {
    use coremidi_sys::{
        MIDIEndpointGetEntity, MIDIEntityGetDestination, MIDIEntityGetNumberOfDestinations,
        MIDIObjectFindByUniqueID, MIDIObjectGetIntegerProperty, kMIDIPropertyUniqueID,
    };

    // CoreMIDI's identifiers are signed; this crate stores them as the same bits unsigned.
    let wanted = i32::from_ne_bytes(id.to_ne_bytes());
    let mut endpoint = 0;
    let mut kind = 0;
    // SAFETY: both out-pointers are to initialised locals that outlive the call, and CoreMIDI
    // writes at most one object reference and one type code through them.
    let found = unsafe { MIDIObjectFindByUniqueID(wanted, &raw mut endpoint, &raw mut kind) };
    if found != 0 || endpoint == 0 {
        return None;
    }

    let mut entity = 0;
    // SAFETY: `endpoint` was returned by CoreMIDI a moment ago, and the out-pointer is to an
    // initialised local. A reference that is not an endpoint produces an error status, not a
    // crash.
    let status = unsafe { MIDIEndpointGetEntity(endpoint, &raw mut entity) };
    if status != 0 || entity == 0 {
        return None;
    }
    // SAFETY: `entity` was returned by CoreMIDI for this endpoint just above.
    if unsafe { MIDIEntityGetNumberOfDestinations(entity) } == 0 {
        return None;
    }
    // SAFETY: the entity has at least one destination, so index zero is in range.
    let destination = unsafe { MIDIEntityGetDestination(entity, 0) };
    if destination == 0 {
        return None;
    }

    let mut value = 0;
    // SAFETY: `destination` came from CoreMIDI just above, the property key is a static CoreMIDI
    // constant, and the out-pointer is to an initialised local.
    let status =
        unsafe { MIDIObjectGetIntegerProperty(destination, kMIDIPropertyUniqueID, &raw mut value) };
    (status == 0).then(|| u32::from_ne_bytes(value.to_ne_bytes()))
}

/// Reports whether an endpoint's identity matches a stored fingerprint.
///
/// The unique identifier settles it when both sides have one; otherwise the name decides, which
/// is all the platform offers for some hardware.
fn matches(unique_id: Option<u32>, name: Option<String>, fingerprint: &DeviceFingerprint) -> bool {
    if let (Some(theirs), Some(ours)) = (unique_id, fingerprint.unique_id) {
        return theirs == ours;
    }
    name.as_deref() == Some(fingerprint.name.as_str())
}

/// Sends messages out through an owned endpoint.
fn send_messages(
    owned: &Owned,
    handle: PortHandle,
    connector: u8,
    messages: &[MidiMessage],
) -> Result<(), PlatformError> {
    // An opened device sends through its output port; a virtual port sends as its own source.
    if let Some((output, destination)) = owned.device_outputs.get(&handle.get()) {
        let packets = build_packets(messages);
        if packets.is_empty() {
            return Ok(());
        }
        return output
            .send(destination, &coremidi::PacketBuffer::new(0, &packets))
            .map_err(|status| PlatformError::Os {
                operation: "send midi to a device",
                detail: format!("OSStatus {status}"),
            });
    }

    let source = owned_source(owned, handle, connector)?;

    let bytes = build_packets(messages);
    if bytes.is_empty() {
        return Ok(());
    }

    let packets = coremidi::PacketBuffer::new(0, &bytes);
    source
        .received(&packets)
        .map_err(|status| PlatformError::Os {
            operation: "send midi",
            detail: format!("OSStatus {status}"),
        })
}

/// Finds one of a virtual port's MIDI Out connectors, counting from zero.
fn owned_source(
    owned: &Owned,
    handle: PortHandle,
    connector: u8,
) -> Result<&VirtualSource, PlatformError> {
    owned
        .sources
        .get(&handle.get())
        .and_then(|sources| sources.get(usize::from(connector)))
        .ok_or_else(|| {
            PlatformError::NotFound(format!("{handle} MIDI Out {}", u16::from(connector) + 1))
        })
}

/// Bytes of one system-exclusive message per CoreMIDI packet.
///
/// A dump is the one message CoreMIDI allows to span packets, and a packet's payload is bounded.
/// Sending in fixed pieces keeps well under that bound without reframing the message, which a
/// receiver would notice.
const SYSEX_CHUNK: usize = 256;

/// Sends one whole system-exclusive message out through an open endpoint.
fn send_sysex(
    owned: &Owned,
    handle: PortHandle,
    connector: u8,
    bytes: &[u8],
) -> Result<(), PlatformError> {
    if bytes.is_empty() {
        return Ok(());
    }

    // An opened device sends through its output port; a virtual port sends as its own source.
    if let Some((output, destination)) = owned.device_outputs.get(&handle.get()) {
        for chunk in bytes.chunks(SYSEX_CHUNK) {
            output
                .send(destination, &coremidi::PacketBuffer::new(0, chunk))
                .map_err(|status| PlatformError::Os {
                    operation: "send system-exclusive to a device",
                    detail: format!("OSStatus {status}"),
                })?;
        }
        return Ok(());
    }

    let source = owned_source(owned, handle, connector)?;
    for chunk in bytes.chunks(SYSEX_CHUNK) {
        source
            .received(&coremidi::PacketBuffer::new(0, chunk))
            .map_err(|status| PlatformError::Os {
                operation: "send system-exclusive",
                detail: format!("OSStatus {status}"),
            })?;
    }
    Ok(())
}

/// Reports whether CoreMIDI says this endpoint is not currently available.
///
/// An interface that is switched off, or an IAC bus that has been disabled, keeps its endpoint and
/// is marked offline rather than removed. Treating it as attached would list hardware that is not
/// there and let routes to it report that they are fine.
fn is_offline(object: &Object) -> bool {
    object
        .get_property_integer(OFFLINE_PROPERTY)
        .is_ok_and(|value| value != 0)
}

/// Pins an endpoint's persistent identifier, or reads the one the system assigned.
///
/// Pinning is what makes an endpoint keep its identity across restarts, so other applications
/// recognise it as the same port rather than a new one.
fn pin_or_read_unique_id(object: &Object, pinned: Option<u32>) -> Option<u32> {
    if let Some(wanted) = pinned {
        let as_signed = i32::from_ne_bytes(wanted.to_ne_bytes());
        if let Err(status) = object.set_property_integer(UNIQUE_ID_PROPERTY, as_signed) {
            // A refused identifier means another endpoint already holds it. Keeping the
            // system-assigned one is better than failing to create the port at all.
            warn!(
                status,
                "could not pin the endpoint identifier; using the assigned one"
            );
        }
    }
    object.unique_id()
}

/// Reports whether an endpoint is software rather than hardware.
///
/// An endpoint with no driver was created by an application. Of those with one, only Apple's
/// IAC and network drivers make ports out of software.
fn is_software(object: &Object) -> bool {
    object
        .get_property_string(DRIVER_PROPERTY)
        .map_or(true, |owner| is_software_driver(&owner))
}

/// Reports whether a driver makes ports out of software.
fn is_software_driver(owner: &str) -> bool {
    SOFTWARE_DRIVERS.contains(&owner)
}

/// Lists the MIDI hardware and other applications' endpoints currently present.
fn enumerate_devices() -> Vec<DiscoveredDevice> {
    let mut devices: HashMap<String, DiscoveredDevice> = HashMap::new();

    for source in Sources {
        if is_offline(&source) {
            continue;
        }
        let Some(name) = source.display_name() else {
            continue;
        };
        let entry = devices
            .entry(name.clone())
            .or_insert_with(|| DiscoveredDevice {
                fingerprint: DeviceFingerprint {
                    unique_id: source.unique_id(),
                    name,
                    ..described(&source)
                },
                direction: Direction::Input,
                claimed_by: None,
                software: is_software(&source),
            });
        entry.direction = Direction::Input;
    }

    for destination in Destinations {
        if is_offline(&destination) {
            continue;
        }
        let Some(name) = destination.display_name() else {
            continue;
        };
        match devices.get_mut(&name) {
            // Present on both sides, so the hardware carries MIDI in both directions.
            Some(existing) => existing.direction = Direction::Bidirectional,
            None => {
                devices.insert(
                    name.clone(),
                    DiscoveredDevice {
                        fingerprint: DeviceFingerprint {
                            unique_id: destination.unique_id(),
                            name,
                            ..described(&destination)
                        },
                        direction: Direction::Output,
                        claimed_by: None,
                        software: is_software(&destination),
                    },
                );
            }
        }
    }

    devices.into_values().collect()
}

/// Returns the object a notification names, whatever kind it is.
fn object_of(object: &AnyObject) -> &Object {
    match object {
        AnyObject::Other(o) => o,
        AnyObject::Device(d) | AnyObject::ExternalDevice(d) => d,
        AnyObject::Entity(e) | AnyObject::ExternalEntity(e) => e,
        AnyObject::Source(s) | AnyObject::ExternalSource(s) => s,
        AnyObject::Destination(d) | AnyObject::ExternalDestination(d) => d,
    }
}

/// Translates a CoreMIDI notification into a platform event.
fn record_notification(events: &Arc<Mutex<Vec<MidiPlatformEvent>>>, notification: &Notification) {
    let event = match notification {
        Notification::ObjectAdded(info) => MidiPlatformEvent::DeviceAdded(DiscoveredDevice {
            fingerprint: fingerprint_of(&info.child),
            direction: Direction::Bidirectional,
            claimed_by: None,
            software: is_software(object_of(&info.child)),
        }),
        Notification::ObjectRemoved(info) => {
            MidiPlatformEvent::DeviceRemoved(fingerprint_of(&info.child))
        }
        Notification::SetupChanged => MidiPlatformEvent::SetupChanged,
        // Property changes and IO errors do not change what is available to route.
        _ => return,
    };

    if let Ok(mut guard) = events.lock() {
        guard.push(event);
    }
}

/// Builds a fingerprint for an object mentioned in a notification.
///
/// Notifications identify an object by reference, so the name and identifier are read back from
/// it. A removed object may already be gone, in which case only a placeholder name is available.
fn fingerprint_of(object: &AnyObject) -> DeviceFingerprint {
    let inner = object_of(object);
    DeviceFingerprint {
        unique_id: inner.unique_id(),
        name: inner
            .display_name()
            .or_else(|| inner.name())
            .unwrap_or_else(|| "unknown".to_owned()),
        ..DeviceFingerprint::default()
    }
}

/// Returns a fingerprint holding the maker and model an endpoint reports, when it reports them.
///
/// Empty strings count as not reported: a driver that leaves them blank has said nothing.
fn described(object: &Object) -> DeviceFingerprint {
    let property = |name: &str| {
        object
            .get_property_string(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    DeviceFingerprint {
        manufacturer: property(MANUFACTURER_PROPERTY),
        model: property(MODEL_PROPERTY),
        ..DeviceFingerprint::default()
    }
}

/// Encodes messages into a contiguous run of MIDI bytes.
fn build_packets(messages: &[MidiMessage]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(messages.len() * 3);
    let mut buffer = [0u8; 3];
    for message in messages {
        let written = message.encode(&mut buffer);
        if let Some(encoded) = buffer.get(..written) {
            bytes.extend_from_slice(encoded);
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks which CoreMIDI driver owners make software ports: Apple's IAC Driver and network
    /// driver, spelled as `kMIDIPropertyDriverOwner` reports them, and not its Bluetooth or USB
    /// drivers, whose ports are hardware.
    #[test]
    fn apples_iac_and_network_drivers_make_software_ports() {
        let cases = [
            ("the IAC Driver", "com.apple.AppleMIDIIACDriver", true),
            ("the network driver", "com.apple.AppleMIDIRTPDriver", true),
            (
                "the Bluetooth driver",
                "com.apple.AppleMIDIBluetoothDriver",
                false,
            ),
            ("the USB driver", "com.apple.AppleMIDIUSBDriver", false),
        ];
        for (name, owner, want) in cases {
            assert_eq!(
                is_software_driver(owner),
                want,
                "{name}: its ports must be listed as software or hardware as they are"
            );
        }
    }

    /// Which end of the MIDI flow a probe endpoint is.
    enum Probe {
        Source,
        Destination,
    }

    /// Locks that enumeration reads `kMIDIPropertyManufacturer` and `kMIDIPropertyModel` from
    /// CoreMIDI, for a device found by its source or only by its destination, and treats a blank
    /// value as none.
    ///
    /// Enumeration once filled in only the name and identifier, so the window had no maker or
    /// model to show for any device on a Mac.
    #[test]
    fn a_device_is_listed_with_the_maker_and_model_it_reports() {
        let client = coremidi::Client::new(&format!("Harbor Maker Test {}", std::process::id()))
            .expect("CoreMIDI must accept a client");
        let cases = [
            (
                "a source with a maker and a blank model",
                Probe::Source,
                Some("Harbor Instruments"),
                "  ",
                (Some("Harbor Instruments"), None),
            ),
            (
                "a destination only, with a model",
                Probe::Destination,
                None,
                "Probe One",
                (None, Some("Probe One")),
            ),
        ];
        for (index, (name, probe, manufacturer, model, want)) in cases.into_iter().enumerate() {
            let endpoint = format!("Harbor Maker Probe {} {index}", std::process::id());
            let set = |object: &coremidi::Object| {
                if let Some(manufacturer) = manufacturer {
                    object
                        .set_property_string(MANUFACTURER_PROPERTY, manufacturer)
                        .expect("CoreMIDI must accept a maker");
                }
                object
                    .set_property_string(MODEL_PROPERTY, model)
                    .expect("CoreMIDI must accept a model");
            };
            // Held until the end of the row, so the probe stays listed while it is looked for.
            let _held: Box<dyn std::any::Any> = match probe {
                Probe::Source => {
                    let source = client
                        .virtual_source(&endpoint)
                        .expect("CoreMIDI must create a source");
                    set(&source);
                    Box::new(source)
                }
                Probe::Destination => {
                    let destination = client
                        .virtual_destination(&endpoint, |_| {})
                        .expect("CoreMIDI must create a destination");
                    set(&destination);
                    Box::new(destination)
                }
            };

            // CoreMIDI lists a new endpoint a moment after creating it.
            let mut found = None;
            for _ in 0..50 {
                found = enumerate_devices()
                    .into_iter()
                    .find(|device| device.fingerprint.name == endpoint);
                if found.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let device = found.expect("the probe must be listed within a second");
            assert_eq!(
                (
                    device.fingerprint.manufacturer.as_deref(),
                    device.fingerprint.model.as_deref()
                ),
                want,
                "{name}: the maker and model must be what CoreMIDI reports, with blank as none"
            );
        }
    }

    /// Reports whether another application sees an endpoint of this name, as a source it can
    /// receive from and as a destination it can send into.
    fn seen_by_others(name: &str) -> (bool, bool) {
        (
            coremidi::Sources
                .into_iter()
                .any(|source| source.display_name().as_deref() == Some(name)),
            coremidi::Destinations
                .into_iter()
                .any(|destination| destination.display_name().as_deref() == Some(name)),
        )
    }

    /// Returns what other applications see of an endpoint once it matches `expected`, or after
    /// two seconds.
    ///
    /// CoreMIDI registers a client's endpoints one after another, so the last of several can
    /// appear a moment after the call that made it returns.
    fn settled(name: &str, expected: (bool, bool)) -> (bool, bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let seen = seen_by_others(name);
            if seen == expected || std::time::Instant::now() > deadline {
                return seen;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Locks how a port's connectors appear to other applications through CoreMIDI: a MIDI In
    /// connector as a destination they send into, a MIDI Out connector as a source they receive
    /// from, several of one kind numbered from 1 and a single one under the port's own name.
    ///
    /// The two ends were once swapped, and a one-way port on macOS carried nothing in either
    /// direction.
    #[test]
    fn each_connector_is_offered_to_applications_as_the_end_it_is_for() {
        let platform = CoreMidiPlatform::start().expect("the CoreMIDI backend must start");
        let name = format!("Harbor Keys {}", std::process::id());
        let spec = VirtualPortSpec {
            outputs: 2,
            ..VirtualPortSpec::simple(name.clone())
        };
        let (handle, ids) = platform
            .create_virtual_port(&spec, Vec::new())
            .expect("CoreMIDI must create the port");
        assert_eq!(
            (ids.inputs.len(), ids.outputs.len()),
            (1, 2),
            "every connector must report a persistent identifier"
        );

        // Each as (source seen, destination seen).
        let cases = [
            ("its one MIDI In", name.clone(), (false, true)),
            ("MIDI Out 1", format!("{name} 1"), (true, false)),
            ("MIDI Out 2", format!("{name} 2"), (true, false)),
        ];
        for (connector, endpoint, want) in cases {
            assert_eq!(
                settled(&endpoint, want),
                want,
                "{connector}: other applications must see it as the end it is for"
            );
        }

        platform
            .destroy_virtual_port(handle)
            .expect("CoreMIDI must destroy the port");
    }

    /// Locks that CoreMIDI honours a pinned `kMIDIPropertyUniqueID` on every connector of a
    /// recreated port, not only the first.
    ///
    /// The identifiers are how other applications remember a port across a daemon restart;
    /// losing any one unbinds whatever used that connector.
    #[test]
    fn a_pinned_identifier_is_honoured_so_identity_survives_a_restart() {
        let platform = CoreMidiPlatform::start().expect("the CoreMIDI backend must start");
        let name = format!("Harbor Pin {}", std::process::id());
        let mut spec = VirtualPortSpec {
            inputs: 2,
            ..VirtualPortSpec::simple(name)
        };

        let (first, assigned) = platform
            .create_virtual_port(&spec, Vec::new())
            .expect("CoreMIDI must create the port");
        platform
            .destroy_virtual_port(first)
            .expect("CoreMIDI must destroy the port");

        spec.pinned_inputs = assigned.inputs.clone();
        spec.pinned_outputs = assigned.outputs.clone();
        let (second, again) = platform
            .create_virtual_port(&spec, Vec::new())
            .expect("CoreMIDI must recreate the port");
        assert_eq!(
            assigned, again,
            "every connector must keep its identity across recreation"
        );
        platform
            .destroy_virtual_port(second)
            .expect("CoreMIDI must destroy the recreated port");
    }
}
