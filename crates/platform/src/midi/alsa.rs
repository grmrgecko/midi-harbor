//! The Linux MIDI backend.
//!
//! The ALSA sequencer, like CoreMIDI, delivers events to whoever is polling its descriptor, so
//! this backend owns a dedicated thread for the process lifetime and serves requests sent to it
//! over a channel. Every sequencer handle stays on that thread.
//!
//! ALSA represents MIDI as structured events rather than bytes. They are converted to and from
//! bytes at the boundary so that the rest of the system sees the same MIDI on both platforms,
//! and so the parser that already treats peer input as hostile is the only one in the project.

use crate::error::PlatformError;
use crate::midi::{
    ConnectorIds, DiscoveredDevice, MidiPlatform, MidiPlatformEvent, PortHandle, VirtualPortSpec,
};
use alsa::poll::Descriptors;
use alsa::seq::{
    Addr, ClientIter, Event, EventType, MidiEvent, PortCap, PortIter, PortSubscribe,
    PortSubscribeIter, PortType, QuerySubsType, Seq,
};
use midi_harbor_core::endpoint::Direction;
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;
use midi_harbor_core::stream::{Chunk, Scanner};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use tracing::{debug, warn};

/// How long the thread waits for anything before looking anyway.
///
/// Sequencer input and requests both wake it at once, so this bounds nothing on the data path.
/// The thread once slept this long between looks instead, which every message paid on the way in
/// and again on the way out.
const POLL_TIMEOUT_MS: i32 = 50;

/// Buffer size for converting between sequencer events and MIDI bytes.
///
/// Large enough for a system-exclusive dump of reasonable size; anything larger is split across
/// several events by the sequencer itself.
const CODEC_BUFFER: u32 = 4096;

/// The ALSA client and port that announce changes to the sequencer.
const ANNOUNCE_ADDR: Addr = Addr { client: 0, port: 1 };

/// A request for the sequencer thread to carry out.
enum Request {
    CreatePort {
        spec: VirtualPortSpec,
        sinks: Vec<RtProducer>,
        reply: Sender<Result<(PortHandle, ConnectorIds), PlatformError>>,
    },
    DestroyPort {
        handle: PortHandle,
        reply: Sender<Result<(), PlatformError>>,
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

/// One sequencer port: a connector of a virtual port, or the bridge to a device.
struct SeqPort {
    /// The sequencer port number.
    port: i32,
    /// Where MIDI arriving on it goes.
    sink: Option<RtProducer>,
    /// Splits what arrives into messages, carrying running status and any open dump between
    /// events. One per port, because that state belongs to the sender on the other end of it.
    scanner: Scanner,
}

impl SeqPort {
    /// Wraps a sequencer port with where its MIDI goes.
    fn new(port: i32, sink: Option<RtProducer>) -> Self {
        Self {
            port,
            sink,
            scanner: Scanner::new(),
        }
    }
}

/// One endpoint this backend owns.
struct OwnedPort {
    /// Its sequencer ports. A virtual port has one per connector, numbered from zero: the n-th
    /// carries MIDI In n where it has one, and MIDI Out n where it has one. A device has one.
    ports: Vec<SeqPort>,
    /// Subscriptions made on behalf of this port, as sender and destination pairs.
    ///
    /// Recorded because ALSA keeps a subscription alive until it is removed or one side goes
    /// away, and a bridge port that is deleted without unsubscribing leaves the device believing
    /// it still has a reader.
    links: Vec<(Addr, Addr)>,
}

/// Creates and owns ALSA sequencer ports on a dedicated thread.
pub struct AlsaPlatform {
    requests: Sender<Request>,
    /// Set by the sequencer thread when a client or port comes or goes. A flag rather than a
    /// queue, because the read path may not lock or allocate, and one re-enumeration answers any
    /// number of announcements.
    setup_changed: Arc<AtomicBool>,
    /// Written to after each request, so the thread wakes to serve it rather than waiting out
    /// its poll.
    waker: UnixStream,
}

impl AlsaPlatform {
    /// Starts the backend thread and waits for the sequencer to open.
    pub fn start() -> Result<Self, PlatformError> {
        let (requests, inbox) = mpsc::channel();
        let setup_changed = Arc::new(AtomicBool::new(false));
        let (ready, started) = mpsc::channel();
        let (waker, woken) = UnixStream::pair().map_err(|error| PlatformError::Os {
            operation: "create the alsa thread's wake-up pipe",
            detail: error.to_string(),
        })?;
        // Neither end may block: a full pipe already means the thread will wake, and the
        // thread empties it without waiting for more.
        for end in [&waker, &woken] {
            end.set_nonblocking(true)
                .map_err(|error| PlatformError::Os {
                    operation: "configure the alsa thread's wake-up pipe",
                    detail: error.to_string(),
                })?;
        }

        let thread_changed = Arc::clone(&setup_changed);
        std::thread::Builder::new()
            .name("alsa-seq".to_owned())
            .spawn(move || sequencer_thread(inbox, thread_changed, &ready, &woken))
            .map_err(|error| PlatformError::Os {
                operation: "start the alsa thread",
                detail: error.to_string(),
            })?;

        // Failing here rather than on first use means a missing sequencer is reported at startup.
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                requests,
                setup_changed,
                waker,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(PlatformError::Os {
                operation: "open the alsa sequencer",
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
                operation: "reach the alsa thread",
                detail: "the backend thread has stopped".to_owned(),
            })?;
        // A full pipe already has a wake-up waiting in it, so a failed write loses nothing.
        let _ = (&self.waker).write(&[1]);
        answer
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| PlatformError::Os {
                operation: "wait for the alsa thread",
                detail: "the backend thread did not answer".to_owned(),
            })
    }
}

impl MidiPlatform for AlsaPlatform {
    fn create_virtual_port(
        &self,
        spec: &VirtualPortSpec,
        sinks: Vec<RtProducer>,
    ) -> Result<(PortHandle, ConnectorIds), PlatformError> {
        let spec = spec.clone();
        self.ask(|reply| Request::CreatePort { spec, sinks, reply })?
    }

    fn destroy_virtual_port(&self, handle: PortHandle) -> Result<(), PlatformError> {
        self.ask(|reply| Request::DestroyPort { handle, reply })?
    }

    fn send(&self, handle: PortHandle, messages: &[MidiMessage]) -> Result<(), PlatformError> {
        self.send_to(handle, 0, messages)
    }

    fn send_sysex(&self, handle: PortHandle, bytes: &[u8]) -> Result<(), PlatformError> {
        self.send_sysex_to(handle, 0, bytes)
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

    fn open_device(&self, fingerprint: &DeviceFingerprint) -> Result<PortHandle, PlatformError> {
        self.open_device_with_sink(fingerprint, None)
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
        if self.setup_changed.swap(false, Ordering::AcqRel) {
            vec![MidiPlatformEvent::SetupChanged]
        } else {
            Vec::new()
        }
    }
}

/// Owns the sequencer and pumps its input.
fn sequencer_thread(
    inbox: Receiver<Request>,
    setup_changed: Arc<AtomicBool>,
    ready: &Sender<Result<(), PlatformError>>,
    woken: &UnixStream,
) {
    let seq = match Seq::open(None, None, true) {
        Ok(seq) => seq,
        Err(error) => {
            let _ = ready.send(Err(PlatformError::Os {
                operation: "open the alsa sequencer",
                detail: error.to_string(),
            }));
            return;
        }
    };

    let name = CString::new("Midi Harbor").unwrap_or_default();
    if let Err(error) = seq.set_client_name(&name) {
        let _ = ready.send(Err(PlatformError::Os {
            operation: "name the alsa client",
            detail: error.to_string(),
        }));
        return;
    }

    // Subscribing to the announce port is how hot-plug is noticed, the same role the notify
    // callback plays on macOS.
    let announce = subscribe_to_announcements(&seq);
    if announce.is_err() {
        warn!("could not subscribe to alsa announcements; hot-plug will not be detected");
    }

    let mut codec = match MidiEvent::new(CODEC_BUFFER) {
        Ok(codec) => codec,
        Err(error) => {
            let _ = ready.send(Err(PlatformError::Os {
                operation: "create the alsa midi codec",
                detail: error.to_string(),
            }));
            return;
        }
    };
    // Running status is decoded rather than relied upon, so every message carries its status.
    codec.enable_running_status(false);

    // What the thread waits on: the sequencer having input for us, or a request arriving.
    let mut watched = match (&seq, Some(alsa::Direction::Capture)).get() {
        Ok(descriptors) => descriptors,
        Err(error) => {
            let _ = ready.send(Err(PlatformError::Os {
                operation: "watch the alsa sequencer",
                detail: error.to_string(),
            }));
            return;
        }
    };
    watched.push(alsa::poll::pollfd {
        fd: woken.as_raw_fd(),
        events: alsa::poll::Flags::IN.bits(),
        revents: 0,
    });

    let _ = ready.send(Ok(()));
    let mut owned: HashMap<u64, OwnedPort> = HashMap::new();
    let mut next_handle = 0u64;

    loop {
        // Wait for input or a request, whichever comes first. A failed poll only means looking
        // again after the timeout would have.
        for descriptor in &mut watched {
            descriptor.revents = 0;
        }
        if alsa::poll::poll(&mut watched, POLL_TIMEOUT_MS).is_err() {
            std::thread::sleep(Duration::from_millis(1));
        }
        // Empty the wake-up pipe; the requests themselves are in the channel.
        let mut discard = [0u8; 64];
        while matches!((&*woken).read(&mut discard), Ok(read) if read > 0) {}

        // Serve requests first, then drain whatever the sequencer has for us.
        while let Ok(request) = inbox.try_recv() {
            match request {
                Request::CreatePort { spec, sinks, reply } => {
                    next_handle = next_handle.saturating_add(1);
                    let handle = PortHandle::from_raw(next_handle);
                    let answer = create_ports(&seq, &spec, sinks).map(|ports| {
                        let _ = owned.insert(
                            handle.get(),
                            OwnedPort {
                                ports,
                                links: Vec::new(),
                            },
                        );
                        // ALSA has no persistent per-port identifier the way CoreMIDI does, so
                        // there is nothing to pin: identity comes from the client and port name.
                        (handle, ConnectorIds::default())
                    });
                    let _ = reply.send(answer);
                }
                Request::DestroyPort { handle, reply } => {
                    let answer = match owned.remove(&handle.get()) {
                        Some(owned_port) => owned_port.ports.iter().try_for_each(|port| {
                            seq.delete_port(port.port)
                                .map_err(|error| PlatformError::Os {
                                    operation: "delete an alsa port",
                                    detail: error.to_string(),
                                })
                        }),
                        None => Err(PlatformError::NotFound(handle.to_string())),
                    };
                    let _ = reply.send(answer);
                }
                Request::Send {
                    handle,
                    connector,
                    messages,
                    reply,
                } => {
                    let answer = seq_port(&owned, handle, connector)
                        .and_then(|port| send_messages(&seq, &mut codec, port, &messages));
                    let _ = reply.send(answer);
                }
                Request::SendSysEx {
                    handle,
                    connector,
                    bytes,
                    reply,
                } => {
                    let answer = seq_port(&owned, handle, connector)
                        .and_then(|port| send_sysex(&seq, port, &bytes));
                    let _ = reply.send(answer);
                }
                Request::ListDevices { reply } => {
                    let _ = reply.send(enumerate_devices(&seq));
                }
                Request::OpenDevice {
                    fingerprint,
                    sink,
                    reply,
                } => {
                    next_handle = next_handle.saturating_add(1);
                    let handle = PortHandle::from_raw(next_handle);
                    let answer = open_device(&seq, &fingerprint).map(|(port, links)| {
                        let _ = owned.insert(
                            handle.get(),
                            OwnedPort {
                                ports: vec![SeqPort::new(port, sink)],
                                links,
                            },
                        );
                        handle
                    });
                    let _ = reply.send(answer);
                }
                Request::CloseDevice { handle, reply } => {
                    let answer = match owned.remove(&handle.get()) {
                        Some(owned_port) => close_device(&seq, &owned_port),
                        None => Err(PlatformError::NotFound(handle.to_string())),
                    };
                    let _ = reply.send(answer);
                }
            }
        }

        drain_input(&seq, &codec, &mut owned, &setup_changed);
    }
}

/// Creates a virtual port's sequencer ports, one per connector, that other clients can connect
/// to.
///
/// The n-th port carries MIDI In n, which other applications write to, where the port has one,
/// and MIDI Out n, which they read from, where it has one. With one of each, as most ports have,
/// that is a single port under the port's own name; with more, they are numbered.
fn create_ports(
    seq: &Seq,
    spec: &VirtualPortSpec,
    sinks: Vec<RtProducer>,
) -> Result<Vec<SeqPort>, PlatformError> {
    let count = spec.inputs.max(spec.outputs);
    let mut sinks = sinks.into_iter();
    let mut ports: Vec<SeqPort> = Vec::with_capacity(usize::from(count));
    for index in 0..count {
        let name = midi_harbor_core::endpoint::connector_name(&spec.name, count, index);
        let name = CString::new(name).map_err(|_| PlatformError::Os {
            operation: "name an alsa port",
            detail: "the name contains a null byte".to_owned(),
        })?;
        let mut caps = PortCap::empty();
        let takes_input = index < spec.inputs;
        if takes_input {
            caps |= PortCap::WRITE | PortCap::SUBS_WRITE;
        }
        if index < spec.outputs {
            caps |= PortCap::READ | PortCap::SUBS_READ;
        }
        let created = seq
            .create_simple_port(&name, caps, PortType::MIDI_GENERIC | PortType::APPLICATION)
            .map_err(|error| port_creation_failed(seq, "create an alsa port", &error));
        match created {
            Ok(port) => {
                let sink = if takes_input { sinks.next() } else { None };
                ports.push(SeqPort::new(port, sink));
            }
            Err(error) => {
                // A port made only partly would offer applications some of its connectors.
                for made in &ports {
                    let _ = seq.delete_port(made.port);
                }
                return Err(error);
            }
        }
    }
    Ok(ports)
}

/// Finds the sequencer port behind one of an endpoint's MIDI Out connectors, counting from zero.
fn seq_port(
    owned: &HashMap<u64, OwnedPort>,
    handle: PortHandle,
    connector: u8,
) -> Result<i32, PlatformError> {
    let Some(owned_port) = owned.get(&handle.get()) else {
        return Err(PlatformError::NotFound(handle.to_string()));
    };
    owned_port
        .ports
        .get(usize::from(connector))
        .map(|port| port.port)
        .ok_or_else(|| {
            PlatformError::NotFound(format!("{handle} MIDI Out {}", u16::from(connector) + 1))
        })
}

/// The most ports one sequencer client may own: the kernel's `SNDRV_SEQ_MAX_PORTS`.
const MAX_CLIENT_PORTS: usize = 254;

/// Explains a failure to create a port.
///
/// ALSA reports a client that has used up its ports as `EINVAL`, the same error as a bad
/// argument, so the port count is what tells them apart. Without this the limit reached the user
/// as a protocol error quoting "Invalid argument (22)", which says nothing they can act on.
fn port_creation_failed(seq: &Seq, operation: &'static str, error: &alsa::Error) -> PlatformError {
    let full = seq
        .client_id()
        .is_ok_and(|client| PortIter::new(seq, client).count() >= MAX_CLIENT_PORTS);
    if full {
        PlatformError::ResourceLimit
    } else {
        PlatformError::Os {
            operation,
            detail: error.to_string(),
        }
    }
}

/// Capabilities of a port only this client subscribes: the announcements port, and the bridge
/// port each opened device gets.
///
/// Without the subscribe permissions, which ALSA asks only of the end the subscriber does not
/// own, no other client can connect to it, and enumeration here skips it. Given them, a second
/// Midi Harbor on the machine took every bridge port for a device and opened it with a bridge of
/// its own, which the first then opened in turn, until both ran out of ports.
const PRIVATE_PORT: PortCap = PortCap::READ
    .union(PortCap::WRITE)
    .union(PortCap::NO_EXPORT);

/// Opens an attached device by bridging a port of our own to it.
///
/// ALSA has no notion of opening a device: a client subscribes one port to another, and MIDI
/// flows along the subscription. So a bridge port is created here and subscribed in whichever
/// directions the device supports, which is what makes the device's MIDI reach our ring buffer
/// and our MIDI reach the device.
fn open_device(
    seq: &Seq,
    fingerprint: &DeviceFingerprint,
) -> Result<(i32, Vec<(Addr, Addr)>), PlatformError> {
    // Locate the device.
    let (addr, readable, writable) = find_device(seq, fingerprint)
        .ok_or_else(|| PlatformError::NotFound(fingerprint.name.clone()))?;

    // Create the bridge port. It both reads and writes, because which directions are actually
    // used is decided by the subscriptions below rather than by the port's own capabilities.
    let name = CString::new(fingerprint.name.as_str()).map_err(|_| PlatformError::Os {
        operation: "name an alsa bridge port",
        detail: "the name contains a null byte".to_owned(),
    })?;
    let our_port = seq
        .create_simple_port(
            &name,
            PRIVATE_PORT,
            PortType::MIDI_GENERIC | PortType::APPLICATION,
        )
        .map_err(|error| port_creation_failed(seq, "create an alsa bridge port", &error))?;

    let our_client = seq.client_id().map_err(|error| PlatformError::Os {
        operation: "read the alsa client id",
        detail: error.to_string(),
    })?;
    let ours = Addr {
        client: our_client,
        port: our_port,
    };

    // Subscribe in each direction the device offers.
    let mut links = Vec::new();
    if readable && let Err(error) = link(seq, addr, ours) {
        // The bridge port is removed rather than left behind, since a port that carries nothing
        // would still appear to other applications as something to connect to.
        let _ = seq.delete_port(our_port);
        return Err(error);
    } else if readable {
        links.push((addr, ours));
    }
    if writable && let Err(error) = link(seq, ours, addr) {
        for (sender, dest) in &links {
            let _ = seq.unsubscribe_port(*sender, *dest);
        }
        let _ = seq.delete_port(our_port);
        return Err(error);
    } else if writable {
        links.push((ours, addr));
    }

    if links.is_empty() {
        let _ = seq.delete_port(our_port);
        return Err(PlatformError::NotFound(fingerprint.name.clone()));
    }

    debug!(device = %fingerprint.name, port = our_port, "opened an alsa device");
    Ok((our_port, links))
}

/// Removes a bridge port's subscriptions and the port itself.
fn close_device(seq: &Seq, owned: &OwnedPort) -> Result<(), PlatformError> {
    // Unsubscribing first means the device stops sending before the port it sends to disappears.
    for (sender, dest) in &owned.links {
        let _ = seq.unsubscribe_port(*sender, *dest);
    }
    owned.ports.iter().try_for_each(|port| {
        seq.delete_port(port.port)
            .map_err(|error| PlatformError::Os {
                operation: "delete an alsa bridge port",
                detail: error.to_string(),
            })
    })
}

/// Subscribes one port to another so MIDI flows from sender to destination.
fn link(seq: &Seq, sender: Addr, dest: Addr) -> Result<(), PlatformError> {
    let subscribe = PortSubscribe::empty().map_err(|error| PlatformError::Os {
        operation: "prepare an alsa subscription",
        detail: error.to_string(),
    })?;
    subscribe.set_sender(sender);
    subscribe.set_dest(dest);
    seq.subscribe_port(&subscribe).map_err(|error| {
        // Busy means another client holds the device's port exclusively. That is not a fault
        // to report as a protocol error but a claim that ends when the other application lets
        // go, which the retry loop notices.
        if error.errno() == EBUSY {
            return PlatformError::Claimed {
                by: exclusive_holder(seq, sender, dest),
            };
        }
        PlatformError::Os {
            operation: "subscribe an alsa port",
            detail: error.to_string(),
        }
    })
}

/// Linux's `EBUSY`, which ALSA returns for a port another client holds exclusively.
const EBUSY: i32 = 16;

/// Names the client holding an exclusive subscription on whichever end of a link is ours to
/// share, if the sequencer will say.
fn exclusive_holder(seq: &Seq, sender: Addr, dest: Addr) -> Option<String> {
    let our_client = seq.client_id().ok()?;
    // The device is whichever end is not ours. Reading from it, the holder is another reader;
    // writing to it, another writer.
    let reading = sender.client != our_client;
    let (device, kind) = if reading {
        (sender, QuerySubsType::READ)
    } else {
        (dest, QuerySubsType::WRITE)
    };
    PortSubscribeIter::new(seq, device, kind)
        .filter(PortSubscribe::get_exclusive)
        .map(|held| {
            if reading {
                held.get_dest()
            } else {
                held.get_sender()
            }
        })
        .find(|holder| holder.client != our_client)
        .and_then(|holder| {
            seq.get_any_client_info(holder.client)
                .ok()
                .and_then(|info| info.get_name().ok().map(str::to_owned))
        })
}

/// Finds the sequencer port a fingerprint refers to, and which directions it offers.
///
/// The topology path is tried first because it names the exact client and port. A device that has
/// been replugged comes back at a different address, so the name is the fallback: it is how the
/// same physical device is recognised across a reconnect.
fn find_device(seq: &Seq, fingerprint: &DeviceFingerprint) -> Option<(Addr, bool, bool)> {
    let our_client = seq.client_id().unwrap_or(-1);
    let (mut by_position, mut by_name) = (None, None);

    for client in ClientIter::new(seq) {
        if client.get_client() == our_client || client.get_client() == 0 {
            continue;
        }
        let client_name = client.get_name().unwrap_or("unknown").to_owned();
        let usb = usb_identity(&client);
        for port in PortIter::new(seq, client.get_client()) {
            let caps = port.get_capability();
            let readable = caps.contains(PortCap::READ) && caps.contains(PortCap::SUBS_READ);
            let writable = caps.contains(PortCap::WRITE) && caps.contains(PortCap::SUBS_WRITE);
            if !readable && !writable {
                continue;
            }

            let addr = Addr {
                client: client.get_client(),
                port: port.get_port(),
            };
            // Built exactly as enumeration builds it, so a stored fingerprint finds its port by
            // the same fields it was recorded with. A serial settles it wherever the device is.
            let here = fingerprint_of(
                &client_name,
                addr.client,
                addr.port,
                port.get_name().unwrap_or_default().to_owned(),
                usb.as_ref(),
            );
            if fingerprint.usb_serial.is_some() && here.usb_serial == fingerprint.usb_serial {
                return Some((addr, readable, writable));
            }
            if by_position.is_none()
                && fingerprint.topology_path.is_some()
                && here.topology_path == fingerprint.topology_path
            {
                by_position = Some((addr, readable, writable));
            }
            if by_name.is_none() && here.name == fingerprint.name {
                by_name = Some((addr, readable, writable));
            }
        }
    }
    by_position.or(by_name)
}

/// Reads the USB identity of the sound card a client belongs to, if it belongs to one.
///
/// Software clients have no card and hardware that is not USB has no USB identity; both leave
/// the sequencer's own numbering as the only position there is.
fn usb_identity(client: &alsa::seq::ClientInfo) -> Option<super::usb::UsbIdentity> {
    super::usb::identity_of_card(std::path::Path::new("/sys"), card_of(client)?)
}

/// Returns the sound card a client belongs to, if it belongs to one.
///
/// Belonging to a card is what makes a client hardware. The client number does not say it:
/// Midi Through is a kernel client numbered among the cards, and has no card.
fn card_of(client: &alsa::seq::ClientInfo) -> Option<i32> {
    client.get_card().ok().filter(|card| *card >= 0)
}

/// Builds the fingerprint for one port of one client.
///
/// ALSA's client number changes whenever the order things appear changes, which is why it only
/// stands in for a position when there is nothing better. A USB device's socket is fixed for as
/// long as it stays plugged there, and its serial number wherever it is plugged.
fn fingerprint_of(
    client_name: &str,
    client: i32,
    port: i32,
    name: String,
    usb: Option<&super::usb::UsbIdentity>,
) -> DeviceFingerprint {
    let Some(usb) = usb else {
        return DeviceFingerprint {
            manufacturer: Some(client_name.to_owned()),
            name,
            topology_path: Some(format!("alsa:{client}:{port}")),
            ..DeviceFingerprint::default()
        };
    };
    DeviceFingerprint {
        // One serial covers every port of an interface, and a serial match is taken as certain,
        // so without the port the second input of an interface would be taken for its first.
        usb_serial: usb.serial.as_ref().map(|serial| format!("{serial}#{port}")),
        manufacturer: usb
            .manufacturer
            .clone()
            .or_else(|| Some(client_name.to_owned())),
        model: usb.product.clone(),
        name,
        topology_path: Some(match &usb.socket {
            Some(socket) => format!("usb-{socket}:{port}"),
            None => format!("alsa:{client}:{port}"),
        }),
        ..DeviceFingerprint::default()
    }
}

/// Subscribes to the sequencer's announce port, so client and port changes are noticed.
fn subscribe_to_announcements(seq: &Seq) -> Result<(), PlatformError> {
    let name = CString::new("announcements").unwrap_or_default();
    let port = seq
        .create_simple_port(&name, PRIVATE_PORT, PortType::APPLICATION)
        .map_err(|error| PlatformError::Os {
            operation: "create the announce port",
            detail: error.to_string(),
        })?;

    let subscribe = PortSubscribe::empty().map_err(|error| PlatformError::Os {
        operation: "prepare an alsa subscription",
        detail: error.to_string(),
    })?;
    subscribe.set_sender(ANNOUNCE_ADDR);
    subscribe.set_dest(Addr {
        client: seq.client_id().unwrap_or(0),
        port,
    });

    seq.subscribe_port(&subscribe)
        .map_err(|error| PlatformError::Os {
            operation: "subscribe to alsa announcements",
            detail: error.to_string(),
        })
}

/// Reads everything waiting on the sequencer and pushes it to the right sink.
fn drain_input(
    seq: &Seq,
    codec: &MidiEvent,
    owned: &mut HashMap<u64, OwnedPort>,
    setup_changed: &AtomicBool,
) {
    let mut input = seq.input();
    let mut buffer = [0u8; 256];

    while input.event_input_pending(true).unwrap_or(0) > 0 {
        let Ok(mut event) = input.event_input() else {
            return;
        };

        // A client or port appearing or disappearing is the sequencer's hot-plug signal.
        if is_announcement(&event) {
            setup_changed.store(true, Ordering::Release);
            continue;
        }

        // The event names the port it arrived on, which is how it reaches the right sink.
        let destination = event.get_dest().port;

        // A dump arrives as its own event carrying the bytes directly. Taken as they are rather
        // than through the codec, whose buffer is sized for ordinary messages and would refuse
        // anything larger.
        let bytes: &[u8] = if event.get_type() == EventType::Sysex {
            event.get_ext().unwrap_or_default()
        } else {
            let Ok(written) = codec.decode(&mut buffer, &mut event) else {
                continue;
            };
            buffer.get(..written).unwrap_or_default()
        };
        if bytes.is_empty() {
            continue;
        }

        // A scan rather than a lookup table, so the read path allocates nothing.
        let Some(port) = owned
            .values_mut()
            .flat_map(|candidate| candidate.ports.iter_mut())
            .find(|candidate| candidate.port == destination)
        else {
            continue;
        };
        let Some(sink) = port.sink.as_mut() else {
            continue;
        };
        push_bytes(&mut port.scanner, sink, bytes);
    }
}

/// Reports whether an event is the sequencer telling us the client or port list changed.
fn is_announcement(event: &alsa::seq::Event<'_>) -> bool {
    use alsa::seq::EventType;
    matches!(
        event.get_type(),
        EventType::ClientStart
            | EventType::ClientExit
            | EventType::ClientChange
            | EventType::PortStart
            | EventType::PortExit
            | EventType::PortChange
    )
}

/// Splits MIDI bytes into messages and pushes each one.
///
/// Runs on the sequencer thread, which is the real-time path: no allocation, no locking, and no
/// arithmetic that can panic.
fn push_bytes(scanner: &mut Scanner, sink: &mut RtProducer, bytes: &[u8]) {
    scanner.scan(bytes, &mut |chunk| match chunk {
        Chunk::Message(message) => {
            let _ = sink.push(message, 0);
        }
        Chunk::SysEx { bytes, end } => {
            let _ = sink.push_sysex(bytes, end, 0);
        }
    });
}

/// Sends one whole system-exclusive message out through an owned port.
///
/// Built as a variable-length event directly rather than through the codec, so a dump of any size
/// travels in one piece instead of meeting the codec's fixed buffer.
fn send_sysex(seq: &Seq, port: i32, bytes: &[u8]) -> Result<(), PlatformError> {
    if bytes.is_empty() {
        return Ok(());
    }

    let mut event = Event::new_ext(EventType::Sysex, bytes);
    event.set_source(port);
    event.set_subs();
    event.set_direct();
    seq.event_output(&mut event)
        .map_err(|error| PlatformError::Os {
            operation: "send a system-exclusive event",
            detail: error.to_string(),
        })?;

    seq.drain_output()
        .map(|_| ())
        .map_err(|error| PlatformError::Os {
            operation: "flush a system-exclusive event",
            detail: error.to_string(),
        })
}

/// Sends messages out through an owned port.
fn send_messages(
    seq: &Seq,
    codec: &mut MidiEvent,
    port: i32,
    messages: &[MidiMessage],
) -> Result<(), PlatformError> {
    let mut bytes = Vec::with_capacity(messages.len() * 3);
    let mut buffer = [0u8; 3];
    for message in messages {
        let written = message.encode(&mut buffer);
        if let Some(encoded) = buffer.get(..written) {
            bytes.extend_from_slice(encoded);
        }
    }
    if bytes.is_empty() {
        return Ok(());
    }

    // The codec turns bytes back into sequencer events, one message at a time.
    let mut offset = 0;
    while offset < bytes.len() {
        let Some(rest) = bytes.get(offset..) else {
            break;
        };
        let (consumed, event) = codec.encode(rest).map_err(|error| PlatformError::Os {
            operation: "encode a sequencer event",
            detail: error.to_string(),
        })?;
        if consumed == 0 {
            break;
        }
        offset = offset.saturating_add(consumed);

        let Some(mut event) = event else {
            continue;
        };
        event.set_source(port);
        event.set_subs();
        event.set_direct();
        seq.event_output(&mut event)
            .map_err(|error| PlatformError::Os {
                operation: "send a sequencer event",
                detail: error.to_string(),
            })?;
    }

    seq.drain_output()
        .map(|_| ())
        .map_err(|error| PlatformError::Os {
            operation: "flush sequencer output",
            detail: error.to_string(),
        })
}

/// Lists the sequencer ports other clients expose.
fn enumerate_devices(seq: &Seq) -> Vec<DiscoveredDevice> {
    let mut devices = Vec::new();
    let our_client = seq.client_id().unwrap_or(-1);

    for client in ClientIter::new(seq) {
        // Our own ports and the sequencer's internal client are not devices to route to.
        if client.get_client() == our_client || client.get_client() == 0 {
            continue;
        }
        let client_name = client.get_name().unwrap_or("unknown").to_owned();
        // Hardware clients belong to a sound card, and a USB card knows its serial number and
        // socket. Software clients have no card and keep the sequencer's own numbering.
        let usb = usb_identity(&client);

        for port in PortIter::new(seq, client.get_client()) {
            let caps = port.get_capability();
            let readable = caps.contains(PortCap::READ) && caps.contains(PortCap::SUBS_READ);
            let writable = caps.contains(PortCap::WRITE) && caps.contains(PortCap::SUBS_WRITE);
            if !readable && !writable {
                continue;
            }

            let direction = match (readable, writable) {
                (true, true) => Direction::Bidirectional,
                (true, false) => Direction::Input,
                _ => Direction::Output,
            };
            let port_name = port.get_name().unwrap_or("unknown").to_owned();

            devices.push(DiscoveredDevice {
                fingerprint: fingerprint_of(
                    &client_name,
                    client.get_client(),
                    port.get_port(),
                    port_name,
                    usb.as_ref(),
                ),
                direction,
                claimed_by: None,
                software: card_of(&client).is_none(),
            });
        }
    }
    debug!(count = devices.len(), "enumerated alsa ports");
    devices
}

#[cfg(test)]
mod tests {
    //! The ALSA backend against the machine's real sequencer, as the CoreMIDI backend is tested
    //! against CoreMIDI. A second backend instance stands in for another application.

    use super::*;

    /// Builds a port with one MIDI In and one MIDI Out.
    fn spec(name: String) -> VirtualPortSpec {
        VirtualPortSpec::simple(name)
    }

    /// Locks that the kernel's Midi Through client, from `snd-seq-dummy`, is listed as
    /// software.
    ///
    /// Midi Through is a kernel client, numbered among the sound cards, and was listed as
    /// physical hardware beside them.
    #[test]
    fn midi_through_is_not_hardware() {
        let platform = AlsaPlatform::start().expect("alsa backend");
        let through = platform
            .list_devices()
            .expect("device list")
            .into_iter()
            .find(|device| device.fingerprint.name.starts_with("Midi Through"))
            .expect("Midi Through; load it with 'modprobe snd-seq-dummy'");
        assert!(through.software, "Midi Through was listed as hardware");
    }

    /// Locks that a port this backend creates is listed by another sequencer client, and is gone
    /// from that client's list once destroyed.
    #[test]
    fn a_created_port_appears_to_another_application() {
        let platform = AlsaPlatform::start().expect("alsa backend");
        let other = AlsaPlatform::start().expect("a second client");
        let spec = spec(format!("Harbor Test {}", std::process::id()));

        let (handle, _) = platform
            .create_virtual_port(&spec, Vec::new())
            .expect("create");
        let seen = other.list_devices().expect("device list");
        assert!(
            seen.iter()
                .any(|device| device.fingerprint.name == spec.name),
            "the created port should be visible to other applications"
        );

        platform.destroy_virtual_port(handle).expect("destroy");
        let seen = other.list_devices().expect("device list");
        assert!(
            !seen
                .iter()
                .any(|device| device.fingerprint.name == spec.name),
            "a destroyed port should be gone for other applications too"
        );
    }

    /// Locks that the ports one instance opens to bridge a device, and its announcements port,
    /// are not listed as devices by another instance.
    ///
    /// Each instance saw the other's bridge ports as devices and opened them with bridges of its
    /// own, which the other then opened, until both ran out of ports.
    #[test]
    fn another_instance_does_not_take_our_own_ports_for_devices() {
        let platform = AlsaPlatform::start().expect("alsa backend");
        let other = AlsaPlatform::start().expect("a second instance");
        let through = platform
            .list_devices()
            .expect("device list")
            .into_iter()
            .find(|device| device.fingerprint.name.starts_with("Midi Through"))
            .expect("Midi Through; load it with 'modprobe snd-seq-dummy'");
        let handle = platform.open_device(&through.fingerprint).expect("open");

        let seen = other.list_devices().expect("device list");
        let named = |name: &str| {
            seen.iter()
                .filter(|device| device.fingerprint.name == name)
                .count()
        };
        assert_eq!(
            named(&through.fingerprint.name),
            1,
            "the bridge port was listed beside the device it bridges"
        );
        assert_eq!(
            named("announcements"),
            0,
            "the announcements port was listed"
        );

        platform.close_device(handle).expect("close");
    }

    /// Locks that a destroyed and recreated port is opened again by the fingerprint another
    /// client remembered of the first.
    ///
    /// ALSA has no per-port identifier to pin, so what survives a restart is the client and port
    /// name. An application that remembered the port has to find it again by that.
    #[test]
    fn a_recreated_port_is_found_again_by_what_was_remembered_of_it() {
        let platform = AlsaPlatform::start().expect("alsa backend");
        let other = AlsaPlatform::start().expect("a second client");
        let spec = spec(format!("Harbor Pin {}", std::process::id()));

        let (first, _) = platform
            .create_virtual_port(&spec, Vec::new())
            .expect("create");
        let remembered = other
            .list_devices()
            .expect("device list")
            .into_iter()
            .find(|device| device.fingerprint.name == spec.name)
            .expect("the port is listed")
            .fingerprint;
        platform.destroy_virtual_port(first).expect("destroy");

        let (second, _) = platform
            .create_virtual_port(&spec, Vec::new())
            .expect("recreate");
        let reopened = other
            .open_device(&remembered)
            .expect("the recreated port is found by what was remembered of the first");
        other.close_device(reopened).expect("close");
        platform.destroy_virtual_port(second).expect("destroy");
    }

    /// Locks how a port's connectors appear to other sequencer clients: the n-th ALSA port
    /// carries MIDI In n and MIDI Out n where the port has them, numbered from 1, with no
    /// unnumbered port beside them.
    ///
    /// With one of each it would be a single port under the port's own name.
    #[test]
    fn several_connectors_are_offered_to_applications_as_numbered_ports() {
        let platform = AlsaPlatform::start().expect("alsa backend");
        let other = AlsaPlatform::start().expect("a second client");
        let name = format!("Harbor Keys {}", std::process::id());
        let spec = VirtualPortSpec {
            outputs: 2,
            ..spec(name.clone())
        };
        let (handle, _) = platform
            .create_virtual_port(&spec, Vec::new())
            .expect("create");

        let seen = other.list_devices().expect("device list");
        let find = |wanted: String| {
            seen.iter()
                .find(|device| device.fingerprint.name == wanted)
                .map(|device| device.direction)
        };
        // Seen from the other application, a MIDI Out it can read from is where MIDI arrives
        // from, which the device list calls an input.
        assert_eq!(
            find(format!("{name} 1")),
            Some(Direction::Bidirectional),
            "the first port must carry MIDI In 1 and MIDI Out 1"
        );
        assert_eq!(
            find(format!("{name} 2")),
            Some(Direction::Input),
            "the second port must carry only MIDI Out 2"
        );
        assert_eq!(
            find(name),
            None,
            "a numbered port was also listed unnumbered"
        );

        platform.destroy_virtual_port(handle).expect("destroy");
    }
}
