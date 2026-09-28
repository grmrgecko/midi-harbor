//! Virtual MIDI ports on Windows, through Windows MIDI Services.
//!
//! A port is a virtual device: an endpoint this process owns, which other applications see and
//! connect to, and which is torn down when this process disconnects from it, as a CoreMIDI
//! virtual endpoint is when its process ends. Each connector is a function block on a group of
//! its own, named as the port's connectors are elsewhere, which Windows publishes to MIDI 1.0
//! applications as one WinMM port each. The service Windows shipped before its late-2026 update
//! names those ports after their groups instead (research R-093).
//!
//! Windows MIDI Services has two forms of its API (R-093). Windows carries `Windows.Devices.Midi2`
//! itself from its late-2026 update; before that, the App SDK, `Microsoft.Windows.Devices.Midi2`,
//! works with the service Windows already has once the user installs its runtime. The two differ
//! in detail rather than in shape, so each has its own module over its own generated bindings.
//! The in-box API is used whenever Windows registers it, so a machine moves onto it with the
//! update that brings it, whether or not the runtime is still installed.
//!
//! The service before that update never finishes closing a virtual device, and answers nothing
//! more until it is restarted. Every call into it is therefore made on a thread of its own and
//! waited for a bounded time, so a stalled service costs this process its virtual ports and
//! nothing else.
//!
//! MIDI crosses the boundary as Universal MIDI Packets, through `ump`. What other applications
//! send arrives on a callback that reads the packet into words it owns, translates it, and pushes
//! it into the connector's ring, allocating nothing.

mod appsdk;
#[rustfmt::skip]
mod appsdk_bindings;
mod inbox;
#[rustfmt::skip]
mod inbox_bindings;

use super::ump;
use crate::error::PlatformError;
use midi_harbor_core::capability::UnavailableReason;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;
use midi_harbor_core::stream::{Chunk, Scanner};
use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tracing::{error, info};

/// What the user needs for virtual ports, named when they are unavailable.
pub const COMPONENT: &str = "Windows MIDI Services, which Windows 11 includes from its late-2026 \
                             update, or before then its App SDK runtime,";

/// The session every port's connection belongs to, as the service lists it.
const SESSION_NAME: &str = "Midi Harbor";

/// What the service is told made the ports.
const MANUFACTURER: &str = "Midi Harbor";

/// The send result bit that says a message went, in both forms of the API.
const SENT: u32 = 0x8000_0000;

/// How long a call into the service is waited for before it is left to finish alone.
const SERVICE_WAIT: Duration = Duration::from_secs(5);

/// How long opening a session is waited for, which includes the service starting and loading its
/// transports: over five seconds on a freshly restarted service (R-093).
const START_WAIT: Duration = Duration::from_secs(30);

/// A call into the service that has not returned.
const RUNNING: u8 = 0;

/// A call into the service that has returned.
const RETURNED: u8 = 1;

/// A call into the service that outlived its wait and has not returned.
const ABANDONED: u8 = 2;

/// How many calls into the service outlived their wait and have not returned.
static STALLED: AtomicUsize = AtomicUsize::new(0);

/// What the user needs when the service stopped answering, named in place of `COMPONENT`.
const RESPONSIVE: &str = "a responsive Windows MIDI Service (restart midisrv)";

/// Reports why virtual ports cannot be created on this machine, when they cannot.
///
/// Asking starts the service, which can take seconds, and asks nothing of a service that has
/// stopped answering.
pub fn unavailable() -> Option<UnavailableReason> {
    let component = if STALLED.load(Ordering::Acquire) > 0 {
        RESPONSIVE
    } else {
        let usable = bounded("check for Windows MIDI Services", START_WAIT, || {
            (inbox::present() && inbox::usable()) || appsdk::usable()
        });
        match usable {
            Some(true) => return None,
            Some(false) => COMPONENT,
            None => RESPONSIVE,
        }
    };
    Some(UnavailableReason::MissingSystemComponent {
        component: component.to_owned(),
    })
}

/// The connection to Windows MIDI Services every port is made through.
pub enum Service {
    /// Through the API Windows carries.
    Inbox(inbox::Service),
    /// Through the App SDK runtime.
    AppSdk(appsdk::Service),
}

impl Service {
    /// Opens a session through whichever form of the API this machine has, the in-box one first.
    pub fn connect() -> Result<Arc<Self>, PlatformError> {
        refuse_while_stalled("open a Windows MIDI Services session")?;
        let service = bounded("open a Windows MIDI Services session", START_WAIT, || {
            if inbox::present() {
                inbox::Service::connect().map(Self::Inbox)
            } else {
                appsdk::Service::connect().map(Self::AppSdk)
            }
        })
        .ok_or_else(|| not_answering("open a Windows MIDI Services session"))??;
        let api = match service {
            Self::Inbox(_) => "Windows.Devices.Midi2",
            Self::AppSdk(_) => "Microsoft.Windows.Devices.Midi2",
        };
        info!(api, "windows midi services session opened");
        Ok(Arc::new(service))
    }
}

/// One virtual port, with the session it was made through.
pub struct VirtualPort {
    service: Arc<Service>,
    port: Port,
}

/// A port as whichever form of the API made it.
enum Port {
    /// Made through the API Windows carries.
    Inbox(inbox::VirtualPort),
    /// Made through the App SDK runtime.
    AppSdk(appsdk::VirtualPort),
}

impl VirtualPort {
    /// Creates a port named `name` with the given connectors, and publishes it.
    ///
    /// Connector `n` is on group `n`: it takes MIDI in when `n < inputs`, delivering it into
    /// sink `n`, and sends when `n < outputs`. Sixteen groups make sixteen connectors at most.
    pub fn create(
        service: &Arc<Service>,
        name: &str,
        names: &[String],
        inputs: u8,
        outputs: u8,
        sinks: Vec<RtProducer>,
    ) -> Result<Self, PlatformError> {
        if inputs.max(outputs) > ump::GROUPS {
            return Err(PlatformError::ResourceLimit);
        }
        refuse_while_stalled("create a virtual port")?;
        let receivers = Receivers::new(inputs, outputs, sinks);
        let service = Arc::clone(service);
        let name = name.to_owned();
        let names = names.to_vec();
        bounded("create a virtual port", SERVICE_WAIT, move || {
            let port = match &*service {
                Service::Inbox(inbox) => Port::Inbox(inbox::VirtualPort::create(
                    inbox, &name, &names, inputs, outputs, receivers,
                )?),
                Service::AppSdk(appsdk) => Port::AppSdk(appsdk::VirtualPort::create(
                    appsdk, &name, &names, inputs, outputs, receivers,
                )?),
            };
            Ok(Self { service, port })
        })
        .ok_or_else(|| not_answering("create a virtual port"))?
    }

    /// Reports whether connector `connector` sends to other applications.
    pub fn sends(&self, connector: u8) -> bool {
        match &self.port {
            Port::Inbox(port) => port.sends(connector),
            Port::AppSdk(port) => port.sends(connector),
        }
    }

    /// Sends messages to the applications receiving from connector `connector`.
    pub fn send(&self, connector: u8, messages: &[MidiMessage]) -> Result<(), PlatformError> {
        let mut buffer = [0u8; 3];
        for message in messages {
            let written = message.encode(&mut buffer);
            let Some(word) = buffer
                .get(..written)
                .and_then(|bytes| ump::encode_message(bytes, connector))
            else {
                continue;
            };
            match &self.port {
                Port::Inbox(port) => port.send_word(word)?,
                Port::AppSdk(port) => port.send_word(word)?,
            }
        }
        Ok(())
    }

    /// Sends one whole system-exclusive message through connector `connector`.
    pub fn send_sysex(&self, connector: u8, bytes: &[u8]) -> Result<(), PlatformError> {
        let mut outcome = Ok(());
        ump::encode_sysex(bytes, connector, &mut |first, second| {
            if outcome.is_ok() {
                outcome = match &self.port {
                    Port::Inbox(port) => port.send_words(first, second),
                    Port::AppSdk(port) => port.send_words(first, second),
                };
            }
        });
        outcome
    }

    /// Withdraws the port from other applications, waiting for the service no longer than
    /// `SERVICE_WAIT`.
    pub fn close(self) {
        let Self { service, port } = self;
        let _ = bounded("close a virtual port", SERVICE_WAIT, move || {
            match (port, &*service) {
                (Port::Inbox(port), Service::Inbox(service)) => port.close(service),
                (Port::AppSdk(port), Service::AppSdk(service)) => port.close(service),
                // A port is only ever made through the service it keeps.
                _ => {}
            }
        });
    }
}

/// Returns the direction each connector's function block takes: whether other applications send
/// to it, and whether they receive from it.
fn connector_directions(inputs: u8, outputs: u8) -> impl Iterator<Item = (u8, bool, bool)> {
    (0..inputs.max(outputs)).map(move |index| (index, index < inputs, index < outputs))
}

/// Turns a send result into an error when the message did not go.
fn check_sent(result: u32) -> Result<(), PlatformError> {
    if result & SENT != 0 {
        Ok(())
    } else {
        Err(PlatformError::Os {
            operation: "send through a virtual port",
            detail: format!("Windows MIDI Services refused the message (result {result:#x})"),
        })
    }
}

/// Runs one call into the service on a thread of its own and waits for it no longer than `wait`,
/// returning nothing if it is still waiting.
///
/// The service Windows ships before its late-2026 update never answers a virtual device's
/// disconnection (Microsoft's issue #1236, R-093), and from then on answers no creation either.
/// Waiting on it would leave the daemon unable to change any port, or to exit. A call that
/// outlives its wait is left running rather than abandoned, and counted in `STALLED` until it
/// returns, which it does once the service is restarted. What it returns then is dropped.
fn bounded<T: Send + 'static>(
    operation: &'static str,
    wait: Duration,
    call: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let state = Arc::new(AtomicU8::new(RUNNING));
    let (done, finished) = std::sync::mpsc::sync_channel(1);
    let running = Arc::clone(&state);
    let spawned = std::thread::Builder::new()
        .name("wms-call".to_owned())
        .spawn(move || {
            let result = call();
            if running.swap(RETURNED, Ordering::AcqRel) == ABANDONED {
                STALLED.fetch_sub(1, Ordering::AcqRel);
                info!(
                    operation,
                    "windows midi services answered a call it had stalled on"
                );
            }
            let _ = done.send(result);
        });
    if let Err(err) = spawned {
        error!(operation, error = %err, "failed to start a windows midi services call");
        return None;
    }
    if let Ok(result) = finished.recv_timeout(wait) {
        return Some(result);
    }

    // Count the call as stalled before marking it, so its return never finds the count at zero.
    STALLED.fetch_add(1, Ordering::AcqRel);
    if state
        .compare_exchange(RUNNING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        STALLED.fetch_sub(1, Ordering::AcqRel);
        return finished.recv().ok();
    }
    error!(
        operation,
        "windows midi services did not answer; restart the Windows MIDI Service to recover"
    );
    None
}

/// Refuses a call while an earlier one still waits on the service, which answers none until it
/// is restarted.
fn refuse_while_stalled(operation: &'static str) -> Result<(), PlatformError> {
    if STALLED.load(Ordering::Acquire) == 0 {
        Ok(())
    } else {
        Err(not_answering(operation))
    }
}

/// Explains a call the service did not answer.
fn not_answering(operation: &'static str) -> PlatformError {
    PlatformError::Os {
        operation,
        detail: "Windows MIDI Services stopped answering; restart the Windows MIDI Service \
                 (midisrv) to recover"
            .to_owned(),
    }
}

/// Explains a port the service would not create, and at which step.
fn creation_refused(name: &str, step: &str) -> PlatformError {
    PlatformError::Os {
        operation: "create a virtual port",
        detail: format!("Windows MIDI Services did not create '{name}': {step}"),
    }
}

/// One connector's receiving state.
struct Receiver {
    scanner: Scanner,
    sink: Option<RtProducer>,
}

/// What the receive callback owns, shared with it through an `Arc`.
///
/// `busy` is taken for the length of one callback. The service is not documented to deliver one
/// connection's messages on one thread, so a second callback arriving while the first runs gives
/// up its packet and counts it rather than waiting: the real-time path may not block.
struct Receivers {
    busy: AtomicBool,
    connectors: UnsafeCell<Vec<Receiver>>,
    dropped: AtomicU64,
}

// SAFETY: `connectors` is touched only by the callback that holds `busy`, which the atomic swap
// grants to one thread at a time; everything else is atomic.
unsafe impl Sync for Receivers {}
// SAFETY: as above; the scanners and producers move between threads only with `busy` held.
unsafe impl Send for Receivers {}

impl Receivers {
    /// Gives each connector that takes MIDI in its sink, in order.
    fn new(inputs: u8, outputs: u8, sinks: Vec<RtProducer>) -> Self {
        let mut sinks = sinks.into_iter();
        Self {
            busy: AtomicBool::new(false),
            connectors: UnsafeCell::new(
                connector_directions(inputs, outputs)
                    .map(|(_, receives, _)| Receiver {
                        scanner: Scanner::new(),
                        sink: if receives { sinks.next() } else { None },
                    })
                    .collect(),
            ),
            dropped: AtomicU64::new(0),
        }
    }

    /// Delivers one packet another application sent into its connector's ring.
    ///
    /// A real-time context: it translates the packet on the stack and scans the bytes into the
    /// ring. It allocates, locks and logs nothing.
    fn deliver(&self, packet: &[u32]) {
        if self.busy.swap(true, Ordering::Acquire) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let group = packet.first().map_or(0, |first| ump::group_of(*first));
        let mut bytes = [0u8; 8];
        let written = ump::decode(packet, &mut bytes);
        // SAFETY: `busy` is held, so no other callback is touching the connectors.
        let connectors = unsafe { &mut *self.connectors.get() };
        if let (
            Some(Receiver {
                scanner,
                sink: Some(sink),
            }),
            Some(bytes),
        ) = (connectors.get_mut(usize::from(group)), bytes.get(..written))
        {
            scanner.scan(bytes, &mut |chunk| match chunk {
                Chunk::Message(message) => {
                    let _ = sink.push(message, 0);
                }
                Chunk::SysEx { bytes, end } => {
                    let _ = sink.push_sysex(bytes, end, 0);
                }
            });
        }
        self.busy.store(false, Ordering::Release);
    }
}
