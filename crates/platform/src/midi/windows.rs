//! The Windows MIDI backend.
//!
//! Hardware and other applications' ports go through WinMM, and this process's own virtual ports
//! through Windows MIDI Services (see `wms`). WinMM announces nothing about devices arriving or
//! leaving without a window to send the message to, so a thread lists the endpoints once a second
//! and reports a change, which is what the daemon re-enumerates on.

use super::winmm::{self, Input, OpenFailure, Output};
use super::winmm_identity::{self, WinmmEndpoint};
use super::wms::{Service, VirtualPort};
use crate::error::PlatformError;
use crate::midi::{
    ConnectorIds, DiscoveredDevice, MidiPlatform, MidiPlatformEvent, PortHandle, VirtualPortSpec,
};
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tracing::debug;
use windows_sys::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

/// How many times opening a device is tried while WinMM's list keeps moving under it.
const OPEN_ATTEMPTS: u32 = 5;

/// How long to let WinMM's list settle before trying an open again.
const OPEN_RETRY: Duration = Duration::from_millis(100);

/// How often the endpoint lists are compared for a device arriving or leaving.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// What one handle stands for.
enum Owned {
    /// A virtual port, until it is closed.
    Virtual(Option<VirtualPort>),
    /// Hardware or another application's port, opened in whichever directions it offers.
    Device {
        input: Option<Input>,
        output: Option<Output>,
    },
}

impl Owned {
    /// Closes everything it holds.
    fn close(&mut self) {
        match self {
            Self::Virtual(port) => {
                if let Some(port) = port.take() {
                    port.close();
                }
            }
            Self::Device { input, output } => {
                if let Some(input) = input.take() {
                    input.close();
                }
                if let Some(output) = output.take() {
                    output.close();
                }
            }
        }
    }
}

/// One handle's entry.
///
/// The endpoint has its own lock, so a system-exclusive dump going out to one device does not
/// hold up messages to every other endpoint.
struct Entry {
    /// Every name WinMM may list this process's virtual port under, empty for a device.
    names: Vec<String>,
    owned: Arc<Mutex<Owned>>,
}

/// Creates virtual ports through Windows MIDI Services and opens hardware through WinMM.
pub struct WindowsMidiPlatform {
    entries: Mutex<HashMap<u64, Entry>>,
    next: AtomicU64,
    setup_changed: Arc<AtomicBool>,
    /// The session virtual ports are made through, opened when the first one is.
    service: Mutex<Option<Arc<Service>>>,
}

impl WindowsMidiPlatform {
    /// Starts the backend and the thread that watches for devices arriving and leaving.
    pub fn start() -> Result<Self, PlatformError> {
        keep_multithreaded_apartment()?;
        let setup_changed = Arc::new(AtomicBool::new(false));
        let watched = Arc::downgrade(&setup_changed);
        std::thread::Builder::new()
            .name("winmm-watch".to_owned())
            .spawn(move || watch(&watched))
            .map_err(|error| PlatformError::Os {
                operation: "start the MIDI device watcher",
                detail: error.to_string(),
            })?;
        Ok(Self {
            entries: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            setup_changed,
            service: Mutex::new(None),
        })
    }

    /// Returns the Windows MIDI Services session, opening it if this is the first port.
    fn service(&self) -> Result<Arc<Service>, PlatformError> {
        let mut service = match self.service.lock() {
            Ok(service) => service,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(open) = service.as_ref() {
            return Ok(Arc::clone(open));
        }
        let opened = Service::connect()?;
        *service = Some(Arc::clone(&opened));
        Ok(opened)
    }

    /// Forgets the session after a port could not be made through it, so the next port opens a
    /// fresh one: a session does not survive the service restarting. Ports made through it keep
    /// it until they close.
    fn forget_service(&self, failed: &Arc<Service>) {
        let mut service = match self.service.lock() {
            Ok(service) => service,
            Err(poisoned) => poisoned.into_inner(),
        };
        if service
            .as_ref()
            .is_some_and(|open| Arc::ptr_eq(open, failed))
        {
            *service = None;
        }
    }

    /// Stores what a new handle stands for and returns the handle.
    fn insert(&self, names: Vec<String>, owned: Owned) -> PortHandle {
        let handle = self.next.fetch_add(1, Ordering::Relaxed);
        let entry = Entry {
            names,
            owned: Arc::new(Mutex::new(owned)),
        };
        match self.entries.lock() {
            Ok(mut entries) => {
                entries.insert(handle, entry);
            }
            Err(poisoned) => {
                poisoned.into_inner().insert(handle, entry);
            }
        }
        PortHandle::from_raw(handle)
    }

    /// Returns what a handle stands for, without holding the table while it is used.
    fn get(&self, handle: PortHandle) -> Result<Arc<Mutex<Owned>>, PlatformError> {
        let entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        entries
            .get(&handle.get())
            .map(|entry| Arc::clone(&entry.owned))
            .ok_or_else(|| PlatformError::NotFound(handle.to_string()))
    }

    /// Takes a handle out of the table and closes what it stood for, once no send is using it.
    fn remove(&self, handle: PortHandle) -> Result<(), PlatformError> {
        let entry = match self.entries.lock() {
            Ok(mut entries) => entries.remove(&handle.get()),
            Err(poisoned) => poisoned.into_inner().remove(&handle.get()),
        };
        let entry = entry.ok_or_else(|| PlatformError::NotFound(handle.to_string()))?;
        match entry.owned.lock() {
            Ok(mut owned) => owned.close(),
            Err(poisoned) => poisoned.into_inner().close(),
        }
        Ok(())
    }

    /// Returns every name this process's virtual ports show other applications.
    fn own_names(&self) -> Vec<String> {
        let entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        entries
            .values()
            .flat_map(|entry| entry.names.iter().cloned())
            .collect()
    }
}

/// Keeps a thread in the process's multithreaded apartment for the life of the process.
///
/// Windows MIDI Services is a WinRT API, and a thread that has not joined an apartment may use one
/// only while some thread holds the multithreaded apartment open; the daemon calls in from its
/// runtime's threads, which join none.
fn keep_multithreaded_apartment() -> Result<(), PlatformError> {
    static STARTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let started = STARTED.get_or_init(|| {
        let (joined, confirmed) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("winrt-apartment".to_owned())
            .spawn(move || {
                // SAFETY: joins this thread to the multithreaded apartment; the thread then parks
                // for the life of the process without leaving it.
                let result =
                    unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32) };
                let _ = joined.send(result >= 0);
                loop {
                    std::thread::park();
                }
            });
        spawned.is_ok() && confirmed.recv().unwrap_or(false)
    });
    if *started {
        Ok(())
    } else {
        Err(PlatformError::Os {
            operation: "join the multithreaded apartment",
            detail: "the apartment thread did not start".to_owned(),
        })
    }
}

impl MidiPlatform for WindowsMidiPlatform {
    fn create_virtual_port(
        &self,
        spec: &VirtualPortSpec,
        sinks: Vec<RtProducer>,
    ) -> Result<(PortHandle, ConnectorIds), PlatformError> {
        let count = spec.inputs.max(spec.outputs);
        let names: Vec<String> = (0..count)
            .map(|index| midi_harbor_core::endpoint::connector_name(&spec.name, count, index))
            .collect();
        // The service refuses a second device of the same name without saying why, since the
        // name decides its identifier, so a name one of this process's ports shows is refused
        // here first. WinMM's own list is not asked: it can go on listing a port for seconds
        // after it closed, and a port closed and made again under its name would be refused.
        let taken = winmm_identity::cut(&spec.name, winmm_identity::NAME_UNITS);
        if self.own_names().contains(&taken) {
            return Err(PlatformError::NameConflict(spec.name.clone()));
        }
        let service = self.service()?;
        let port = VirtualPort::create(
            &service,
            &spec.name,
            &names,
            spec.inputs,
            spec.outputs,
            sinks,
        )
        .inspect_err(|_| self.forget_service(&service))?;
        // Windows MIDI Services assigns no identifier an application could pin.
        Ok((
            self.insert(
                winmm_identity::virtual_port_names(&spec.name, &names),
                Owned::Virtual(Some(port)),
            ),
            ConnectorIds::default(),
        ))
    }

    fn destroy_virtual_port(&self, handle: PortHandle) -> Result<(), PlatformError> {
        self.remove(handle)
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
        let owned = self.get(handle)?;
        let owned = match owned.lock() {
            Ok(owned) => owned,
            Err(poisoned) => poisoned.into_inner(),
        };
        match &*owned {
            Owned::Virtual(Some(port)) if port.sends(connector) => port.send(connector, messages),
            Owned::Virtual(_) => Err(no_output(handle, connector)),
            Owned::Device {
                output: Some(output),
                ..
            } if connector == 0 => output.send(messages),
            Owned::Device { .. } => Err(no_output(handle, connector)),
        }
    }

    fn send_sysex_to(
        &self,
        handle: PortHandle,
        connector: u8,
        bytes: &[u8],
    ) -> Result<(), PlatformError> {
        let owned = self.get(handle)?;
        let owned = match owned.lock() {
            Ok(owned) => owned,
            Err(poisoned) => poisoned.into_inner(),
        };
        match &*owned {
            Owned::Virtual(Some(port)) if port.sends(connector) => {
                port.send_sysex(connector, bytes)
            }
            Owned::Virtual(_) => Err(no_output(handle, connector)),
            Owned::Device {
                output: Some(output),
                ..
            } if connector == 0 => output.send_sysex(bytes),
            Owned::Device { .. } => Err(no_output(handle, connector)),
        }
    }

    fn open_device(&self, fingerprint: &DeviceFingerprint) -> Result<PortHandle, PlatformError> {
        self.open_device_with_sink(fingerprint, None)
    }

    fn open_device_with_sink(
        &self,
        fingerprint: &DeviceFingerprint,
        sink: Option<RtProducer>,
    ) -> Result<PortHandle, PlatformError> {
        // WinMM opens by position, and positions shift when anything arrives or leaves, so what
        // was opened is checked against what was meant, and looked for again if it moved.
        let mut sink = sink;
        let mut attempts = 0;
        let (input, output) = loop {
            attempts += 1;
            let (inputs, outputs) = winmm::enumerate();
            let wanted_input = find(&inputs, fingerprint).cloned();
            let wanted_output = find(&outputs, fingerprint).cloned();
            if wanted_input.is_none() && wanted_output.is_none() {
                return Err(PlatformError::NotFound(fingerprint.name.clone()));
            }

            // Another application holding the device is an answer, and anything else may be the
            // list moving under the open, so it is tried again.
            let retry = |error: PlatformError| match error {
                PlatformError::Claimed { .. } => Err(error),
                _ if attempts >= OPEN_ATTEMPTS => Err(error),
                _ => {
                    debug!(device = %fingerprint.name, error = %error, "opening failed; trying again");
                    std::thread::sleep(OPEN_RETRY);
                    Ok(())
                }
            };
            let output = match wanted_output
                .as_ref()
                .map(|endpoint| Output::open(endpoint.index))
                .transpose()
            {
                Ok(output) => output,
                Err(error) => {
                    retry(error)?;
                    continue;
                }
            };
            let input = match wanted_input.as_ref() {
                None => None,
                Some(endpoint) => match Input::open(endpoint.index, sink.take()) {
                    Ok(input) => Some(input),
                    Err(failed) => {
                        let OpenFailure {
                            error,
                            sink: returned,
                        } = *failed;
                        sink = returned;
                        if let Some(output) = output {
                            output.close();
                        }
                        retry(error)?;
                        continue;
                    }
                },
            };

            // Asked of the handles rather than of the positions tried, since the list may have
            // moved between reading it and opening.
            let still =
                |wanted: &Option<WinmmEndpoint>, position: Option<Option<u32>>, input| match (
                    wanted, position,
                ) {
                    (Some(endpoint), Some(Some(position))) => winmm::endpoint_at(position, input)
                        .is_some_and(|now| {
                            now.name == endpoint.name && now.interface == endpoint.interface
                        }),
                    (None, None) => true,
                    _ => false,
                };
            if still(&wanted_input, input.as_ref().map(Input::position), true)
                && still(&wanted_output, output.as_ref().map(Output::position), false)
            {
                break (input, output);
            }
            // The sink went to the input just closed; it comes back so the next try has it.
            if let Some(input) = input {
                sink = input.close_returning_sink();
            }
            if let Some(output) = output {
                output.close();
            }
            if attempts >= OPEN_ATTEMPTS {
                return Err(PlatformError::NotFound(fingerprint.name.clone()));
            }
            debug!(device = %fingerprint.name, "the device list moved while opening; trying again");
            std::thread::sleep(OPEN_RETRY);
        };
        debug!(device = %fingerprint.name, "opened a MIDI device");
        Ok(self.insert(Vec::new(), Owned::Device { input, output }))
    }

    fn close_device(&self, handle: PortHandle) -> Result<(), PlatformError> {
        self.remove(handle)
    }

    fn list_devices(&self) -> Result<Vec<DiscoveredDevice>, PlatformError> {
        let own = self.own_names();
        let (inputs, outputs) = winmm::enumerate();
        Ok(winmm_identity::pair(&inputs, &outputs, &|name| {
            own.iter().any(|mine| mine == name)
        }))
    }

    fn shutdown(&self) {
        let entries = match self.entries.lock() {
            Ok(mut entries) => std::mem::take(&mut *entries),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        let closed = entries.len();
        for entry in entries.into_values() {
            match entry.owned.lock() {
                Ok(mut owned) => owned.close(),
                Err(poisoned) => poisoned.into_inner().close(),
            }
        }
        if closed > 0 {
            debug!(closed, "closed every MIDI port and device");
        }
    }

    fn drain_events(&self) -> Vec<MidiPlatformEvent> {
        if self.setup_changed.swap(false, Ordering::AcqRel) {
            vec![MidiPlatformEvent::SetupChanged]
        } else {
            Vec::new()
        }
    }
}

impl Drop for WindowsMidiPlatform {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Explains a send to a connector that does not exist.
fn no_output(handle: PortHandle, connector: u8) -> PlatformError {
    PlatformError::NotFound(format!("{handle} MIDI Out {}", u16::from(connector) + 1))
}

/// Finds the endpoint a fingerprint describes, preferring the one in the same place.
fn find<'a>(
    endpoints: &'a [WinmmEndpoint],
    fingerprint: &DeviceFingerprint,
) -> Option<&'a WinmmEndpoint> {
    endpoints
        .iter()
        .find(|endpoint| {
            endpoint.name == fingerprint.name
                && fingerprint.topology_path.as_deref() == Some(endpoint.interface.as_str())
        })
        .or_else(|| {
            endpoints
                .iter()
                .find(|endpoint| winmm_identity::matches(fingerprint, endpoint))
        })
}

/// Compares the endpoint lists once a second and flags a change, for as long as the backend
/// lives.
fn watch(changed: &Weak<AtomicBool>) {
    let mut previous = signature();
    loop {
        std::thread::sleep(WATCH_INTERVAL);
        let Some(changed) = changed.upgrade() else {
            return;
        };
        let current = signature();
        if current != previous {
            changed.store(true, Ordering::Release);
            previous = current;
        }
    }
}

/// Everything about the endpoint lists that a device arriving or leaving changes.
fn signature() -> Vec<(bool, String, String)> {
    let (inputs, outputs) = winmm::enumerate();
    inputs
        .into_iter()
        .map(|endpoint| (true, endpoint.name, endpoint.interface))
        .chain(
            outputs
                .into_iter()
                .map(|endpoint| (false, endpoint.name, endpoint.interface)),
        )
        .collect()
}
