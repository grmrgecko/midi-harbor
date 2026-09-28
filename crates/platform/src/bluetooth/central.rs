//! Connecting out to BLE MIDI devices, over `btleplug`.
//!
//! `btleplug` is asynchronous and this seam is not, so the backend owns a thread running its own
//! runtime and is driven by a command queue. Nothing here blocks the caller waiting for a radio:
//! a connection is requested, a handle is issued immediately, and the link coming up or failing
//! arrives later as an event. The daemon's connection state machine already works that way, and
//! blocking a task on a radio that may take seconds to answer would stall everything sharing its
//! thread.

use super::{BluetoothEvent, DiscoveredPeripheral, LinkHandle, PeripheralId, SERVICE_UUID};
use crate::error::PlatformError;
use btleplug::api::{
    Central as _, CentralEvent, CentralState, Characteristic, Manager as _, Peripheral as _,
    ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use midi_harbor_blemidi::codec::{CHARACTERISTIC_UUID, Decoder, Encoder, Event, MAX_PACKET};
use midi_harbor_core::capability::UnavailableReason;
use midi_harbor_core::failure::FailureReason;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};

/// How long to wait for a link to come up before giving up on it.
///
/// A peripheral that has gone out of range does not refuse a connection, it simply never answers,
/// so without this the attempt would sit there for as long as the radio allowed.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One instruction for the backend thread.
enum Command {
    /// Begin looking for peripherals advertising the MIDI service.
    StartScan,
    /// Stop looking.
    StopScan,
    /// Open a link, reporting the outcome as an event under the handle already issued.
    Connect {
        handle: u64,
        id: PeripheralId,
        sink: Option<RtProducer>,
    },
    /// Close a link.
    Disconnect { handle: u64 },
    /// Send messages over a link.
    Send {
        handle: u64,
        messages: Vec<MidiMessage>,
    },
    /// Send one whole system-exclusive message over a link.
    SendSysEx { handle: u64, bytes: Vec<u8> },
    /// Stop the backend thread.
    Shutdown,
}

/// State the seam reads without going through the backend thread.
#[derive(Default)]
struct Shared {
    /// Events waiting to be drained.
    events: Mutex<Vec<BluetoothEvent>>,
    /// Peripherals currently advertising the MIDI service.
    in_range: Mutex<Vec<DiscoveredPeripheral>>,
    /// Links believed to be open, so a send to a dead handle fails rather than vanishing.
    links: Mutex<HashMap<u64, PeripheralId>>,
    /// Why the central role is unusable, when it is.
    unavailable: Mutex<Option<UnavailableReason>>,
    /// The next handle to issue.
    next_handle: AtomicU64,
}

impl Shared {
    /// Records an event for the next drain.
    fn report(&self, event: BluetoothEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }

    /// Reports whether the role is unusable right now.
    fn is_unavailable(&self) -> bool {
        self.unavailable.lock().is_ok_and(|held| held.is_some())
    }

    /// Records why the role cannot be used, reporting the change once.
    fn set_unavailable(&self, reason: Option<UnavailableReason>) {
        if let Ok(mut held) = self.unavailable.lock() {
            if *held == reason {
                return;
            }
            *held = reason.clone();
        }
        self.report(BluetoothEvent::AdapterChanged(reason));
    }
}

/// The central half of the Bluetooth seam.
pub struct BtleplugCentral {
    commands: mpsc::UnboundedSender<Command>,
    shared: Arc<Shared>,
}

impl BtleplugCentral {
    /// Starts the backend thread and its runtime.
    ///
    /// Returns successfully even when there is no adapter: the capability query is how absence is
    /// reported, and a daemon that refused to start for want of a radio would take MIDI down with
    /// it.
    pub fn start() -> Result<Self, PlatformError> {
        let (commands, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);

        std::thread::Builder::new()
            .name("midi-harbor-ble".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        error!(error = %err, "failed to start the bluetooth runtime");
                        thread_shared.set_unavailable(Some(UnavailableReason::NotBuilt));
                        return;
                    }
                };
                runtime.block_on(run(receiver, thread_shared));
            })
            .map_err(|err| PlatformError::Os {
                operation: "starting the bluetooth backend thread",
                detail: err.to_string(),
            })?;

        Ok(Self { commands, shared })
    }

    /// Reports why the central role cannot be used, or `None` when it can.
    pub fn unavailable(&self) -> Option<UnavailableReason> {
        self.shared
            .unavailable
            .lock()
            .ok()
            .and_then(|held| held.clone())
    }

    /// Returns the peripherals currently advertising the MIDI service.
    pub fn in_range(&self) -> Vec<DiscoveredPeripheral> {
        self.shared
            .in_range
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default()
    }

    /// Queues one command, treating a stopped backend as an unavailable adapter.
    fn queue(&self, command: Command) -> Result<(), PlatformError> {
        self.commands
            .send(command)
            .map_err(|_| PlatformError::AdapterUnavailable)
    }

    /// Begins looking for peripherals.
    pub fn start_scan(&self) -> Result<(), PlatformError> {
        self.queue(Command::StartScan)
    }

    /// Stops looking.
    pub fn stop_scan(&self) -> Result<(), PlatformError> {
        self.queue(Command::StopScan)
    }

    /// Requests a link, issuing its handle straight away.
    pub fn connect(
        &self,
        id: &PeripheralId,
        sink: Option<RtProducer>,
    ) -> Result<LinkHandle, PlatformError> {
        // Refuse here rather than in the backend, so asking for something that was never seen is
        // an error the caller can act on instead of a silent failure event.
        let known = self
            .shared
            .in_range
            .lock()
            .map(|held| held.iter().any(|found| &found.id == id))
            .unwrap_or(false);
        if !known {
            return Err(PlatformError::NotFound(id.to_string()));
        }

        let handle = self.shared.next_handle.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut links) = self.shared.links.lock() {
            links.insert(handle, id.clone());
        }
        self.queue(Command::Connect {
            handle,
            id: id.clone(),
            sink,
        })?;
        Ok(LinkHandle::from_raw(handle))
    }

    /// Closes a link.
    pub fn disconnect(&self, link: LinkHandle) -> Result<(), PlatformError> {
        self.known(link)?;
        self.queue(Command::Disconnect { handle: link.get() })
    }

    /// Sends messages over a link.
    pub fn send(&self, link: LinkHandle, messages: &[MidiMessage]) -> Result<(), PlatformError> {
        self.known(link)?;
        self.queue(Command::Send {
            handle: link.get(),
            messages: messages.to_vec(),
        })
    }

    /// Sends one whole system-exclusive message over a link.
    pub fn send_sysex(&self, link: LinkHandle, bytes: &[u8]) -> Result<(), PlatformError> {
        self.known(link)?;
        self.queue(Command::SendSysEx {
            handle: link.get(),
            bytes: bytes.to_vec(),
        })
    }

    /// Takes the events observed since the last call.
    pub fn drain_events(&self) -> Vec<BluetoothEvent> {
        match self.shared.events.lock() {
            Ok(mut events) => std::mem::take(&mut events),
            Err(poisoned) => std::mem::take(&mut poisoned.into_inner()),
        }
    }

    /// Fails for a handle this backend never issued or has already dropped.
    fn known(&self, link: LinkHandle) -> Result<(), PlatformError> {
        let open = self
            .shared
            .links
            .lock()
            .map(|links| links.contains_key(&link.get()))
            .unwrap_or(false);
        if open {
            Ok(())
        } else {
            Err(PlatformError::NotFound(link.to_string()))
        }
    }
}

impl Drop for BtleplugCentral {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

/// One open link's state, owned by the backend thread.
struct Link {
    peripheral: Peripheral,
    characteristic: Characteristic,
    /// Per-link, because the packet being built and its header's timestamp bits are per-link.
    encoder: Encoder,
}

/// The backend thread's main loop.
async fn run(mut commands: mpsc::UnboundedReceiver<Command>, shared: Arc<Shared>) {
    let adapter = match open_adapter(&shared).await {
        Some(adapter) => adapter,
        None => {
            // Drain commands so callers get an ordinary refusal rather than a queue that grows.
            while let Some(command) = commands.recv().await {
                if matches!(command, Command::Shutdown) {
                    return;
                }
            }
            return;
        }
    };

    // Watch discovery on its own task, because a scan runs for as long as the user is looking.
    let watcher_shared = Arc::clone(&shared);
    let watcher_adapter = adapter.clone();
    tokio::spawn(async move { watch(watcher_adapter, watcher_shared).await });

    let started = Instant::now();
    let mut links: HashMap<u64, Link> = HashMap::new();

    while let Some(command) = commands.recv().await {
        match command {
            Command::StartScan => {
                if let Err(err) = adapter.start_scan(scan_filter()).await {
                    error!(error = %err, "failed to start the bluetooth scan");
                }
            }
            Command::StopScan => {
                if let Err(err) = adapter.stop_scan().await {
                    debug!(error = %err, "failed to stop the bluetooth scan");
                }
            }
            Command::Connect { handle, id, sink } => {
                match open_link(&adapter, &id, sink, Arc::clone(&shared)).await {
                    Ok(link) => {
                        links.insert(handle, link);
                        shared.report(BluetoothEvent::Connected(id));
                    }
                    Err(reason) => {
                        error!(peripheral = %id, error = %reason, "failed to open the bluetooth link");
                        if let Ok(mut open) = shared.links.lock() {
                            open.remove(&handle);
                        }
                        shared.report(BluetoothEvent::LinkFailed { id, reason });
                    }
                }
            }
            Command::Disconnect { handle } => {
                if let Some(link) = links.remove(&handle)
                    && let Err(err) = link.peripheral.disconnect().await
                {
                    debug!(error = %err, "failed to close the bluetooth link");
                }
                let id = shared
                    .links
                    .lock()
                    .ok()
                    .and_then(|mut open| open.remove(&handle));
                if let Some(id) = id {
                    shared.report(BluetoothEvent::Disconnected(id));
                }
            }
            Command::Send { handle, messages } => {
                let at = started.elapsed().as_millis() as u64;
                if let Some(link) = links.get_mut(&handle) {
                    let mut packets = Vec::new();
                    for message in &messages {
                        link.encoder
                            .push(at, message, &mut |packet| packets.push(packet.to_vec()));
                    }
                    link.encoder
                        .flush(&mut |packet| packets.push(packet.to_vec()));
                    write_all(link, &packets, &shared, handle).await;
                }
            }
            Command::SendSysEx { handle, bytes } => {
                let at = started.elapsed().as_millis() as u64;
                if let Some(link) = links.get_mut(&handle) {
                    let mut packets = Vec::new();
                    match link
                        .encoder
                        .push_sysex(at, &bytes, &mut |packet| packets.push(packet.to_vec()))
                    {
                        Ok(()) => {
                            link.encoder
                                .flush(&mut |packet| packets.push(packet.to_vec()));
                            write_all(link, &packets, &shared, handle).await;
                        }
                        Err(err) => {
                            error!(error = %err, "refused to send a malformed dump over bluetooth");
                        }
                    }
                }
            }
            Command::Shutdown => {
                for (_, link) in links.drain() {
                    let _ = link.peripheral.disconnect().await;
                }
                return;
            }
        }
    }
}

/// Opens the first adapter, recording why the role is unusable when there is none.
async fn open_adapter(shared: &Arc<Shared>) -> Option<Adapter> {
    let manager = match Manager::new().await {
        Ok(manager) => manager,
        Err(err) => {
            // On Linux this is BlueZ being absent or not running, which is a missing component
            // rather than missing hardware, and the two call for different things from the user.
            debug!(error = %err, "no bluetooth manager");
            shared.set_unavailable(Some(UnavailableReason::MissingSystemComponent {
                component: "the system Bluetooth service".to_owned(),
            }));
            return None;
        }
    };
    let adapter = match manager.adapters().await {
        Ok(adapters) => adapters.into_iter().next(),
        Err(err) => {
            debug!(error = %err, "failed to list bluetooth adapters");
            None
        }
    };
    match adapter {
        Some(adapter) => {
            // On macOS there is always an adapter, refused or not, and a refused one scans and
            // hears nothing. Said here, the capability query names the permission instead of
            // reporting a radio that is ready.
            let refusal = refusal();
            if refusal.is_some() {
                warn!("bluetooth permission was refused; grant it in System Settings");
            }
            let state = adapter.adapter_state().await.unwrap_or_else(|err| {
                debug!(error = %err, "failed to read the bluetooth adapter state");
                CentralState::Unknown
            });
            let reason = availability(state, refusal.clone()).unwrap_or(refusal);
            match &reason {
                None => info!("bluetooth central ready"),
                Some(UnavailableReason::AdapterOff) => info!("bluetooth adapter is off"),
                Some(_) => {}
            }
            shared.set_unavailable(reason);
            Some(adapter)
        }
        None => {
            shared.set_unavailable(Some(why_no_adapter()));
            None
        }
    }
}

/// Explains an adapter state, or returns `None` when the state says nothing either way.
///
/// `Some(None)` is a radio that is usable. An unknown state leaves the last answer standing:
/// CoreBluetooth reports a refused permission and a radio still starting up as unknown, and
/// neither is news about the power switch.
fn availability(
    state: CentralState,
    refusal: Option<UnavailableReason>,
) -> Option<Option<UnavailableReason>> {
    match state {
        CentralState::PoweredOn => Some(refusal),
        CentralState::PoweredOff => Some(refusal.or(Some(UnavailableReason::AdapterOff))),
        CentralState::Unknown => None,
    }
}

/// Returns the reason for a refused Bluetooth permission, when this process has been refused.
#[cfg(target_os = "macos")]
fn refusal() -> Option<UnavailableReason> {
    super::authorization::refused().then(super::authorization::permission_denied)
}

/// Returns nothing, because only macOS asks the user before a process may use the radio.
#[cfg(not(target_os = "macos"))]
fn refusal() -> Option<UnavailableReason> {
    None
}

/// Tells hardware that is absent apart from a stack that is not running.
///
/// `btleplug` reports both as an empty adapter list, but they ask different things of the user:
/// one needs an adapter plugged in, the other needs BlueZ started. The kernel lists every
/// controller it has bound under `/sys/class/bluetooth` whether or not anything is managing it,
/// so an entry there with no adapter here means the hardware is present and unmanaged.
#[cfg(target_os = "linux")]
fn why_no_adapter() -> UnavailableReason {
    let controllers = std::fs::read_dir("/sys/class/bluetooth")
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);
    if controllers > 0 {
        UnavailableReason::MissingSystemComponent {
            component: "BlueZ".to_owned(),
        }
    } else {
        UnavailableReason::NoAdapter
    }
}

/// Reports absent hardware, where there is no second explanation to offer.
#[cfg(not(target_os = "linux"))]
fn why_no_adapter() -> UnavailableReason {
    UnavailableReason::NoAdapter
}

/// Follows discovery and the adapter's power for as long as the backend runs.
async fn watch(adapter: Adapter, shared: Arc<Shared>) {
    let mut events = match adapter.events().await {
        Ok(events) => events,
        Err(err) => {
            error!(error = %err, "failed to follow bluetooth discovery");
            return;
        }
    };

    while let Some(event) = events.next().await {
        match event {
            CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => {
                let Ok(peripheral) = adapter.peripheral(&id).await else {
                    continue;
                };
                let Some(found) = describe(&peripheral).await else {
                    continue;
                };
                // Reported when first heard, and again when its name arrives or changes. The name
                // usually comes in the scan response, after the advertisement that first reports
                // the device, so reporting only the first sighting listed a named device as
                // nameless for as long as it stayed in range.
                let changed = shared
                    .in_range
                    .lock()
                    .map(|mut known| {
                        let news = is_news(known.iter().find(|entry| entry.id == found.id), &found);
                        known.retain(|entry| entry.id != found.id);
                        known.push(found.clone());
                        news
                    })
                    .unwrap_or(false);
                if changed {
                    shared.report(BluetoothEvent::PeripheralFound(found));
                }
            }
            CentralEvent::DeviceDisconnected(id) => {
                let id = PeripheralId::new(id.to_string());
                // Forgotten, so that hearing it again is news. A device is reported only the
                // first time it is seen, and one that dropped and came back was never reported
                // again, so nothing reconnected it.
                if let Ok(mut known) = shared.in_range.lock() {
                    known.retain(|entry| entry.id != id);
                }
                shared.report(BluetoothEvent::Disconnected(id));
            }
            CentralEvent::StateUpdate(state) => {
                let Some(reason) = availability(state, refusal()) else {
                    continue;
                };
                // A radio switched off hears nothing, so what it heard before is no longer in
                // range. The daemon stops and restarts the scan from the capability itself.
                if reason.is_some()
                    && let Ok(mut known) = shared.in_range.lock()
                {
                    known.clear();
                }
                match &reason {
                    None => info!("bluetooth adapter switched on"),
                    Some(reason) => info!(reason = %reason, "bluetooth adapter became unusable"),
                }
                shared.set_unavailable(reason);
            }
            _ => {}
        }
    }
}

/// Reports whether a sighting is worth passing on: a device not heard before, or one whose name
/// has arrived or changed.
fn is_news(previous: Option<&DiscoveredPeripheral>, found: &DiscoveredPeripheral) -> bool {
    previous.is_none_or(|entry| entry.name != found.name)
}

/// Describes a peripheral, skipping anything that is not offering MIDI.
async fn describe(peripheral: &Peripheral) -> Option<DiscoveredPeripheral> {
    let properties = peripheral.properties().await.ok().flatten()?;
    if !properties.services.contains(&SERVICE_UUID) {
        return None;
    }
    Some(DiscoveredPeripheral {
        id: PeripheralId::new(peripheral.id().to_string()),
        name: properties.local_name,
        rssi: properties.rssi,
        paired: false,
    })
}

/// Connects, discovers the MIDI characteristic, and starts reading what the device sends.
///
/// Fails with the closest reason the closed set allows. They were once all "device was removed",
/// which sent a user whose device was right there looking for one that had walked away.
async fn open_link(
    adapter: &Adapter,
    id: &PeripheralId,
    sink: Option<RtProducer>,
    shared: Arc<Shared>,
) -> Result<Link, FailureReason> {
    // Find the peripheral.
    let peripheral = adapter
        .peripherals()
        .await
        .map_err(|_| FailureReason::AdapterUnavailable)?
        .into_iter()
        .find(|candidate| candidate.id().to_string() == id.as_str())
        .ok_or(FailureReason::DeviceRemoved)?;

    // Connect, giving up rather than waiting on a device that has walked away.
    tokio::time::timeout(CONNECT_TIMEOUT, peripheral.connect())
        .await
        .map_err(|_| {
            warn!(peripheral = %id, "bluetooth device did not answer the connection");
            FailureReason::PeerTimeout
        })?
        .map_err(|err| {
            // The radio's own reason, such as a pairing one side has forgotten, does not fit the
            // closed set, so it is kept here where it can be read.
            warn!(peripheral = %id, error = %err, "bluetooth connect refused");
            FailureReason::PeerRejected
        })?;

    // Find the characteristic every BLE MIDI device carries.
    peripheral.discover_services().await.map_err(|err| {
        step_failure(
            shared.is_unavailable(),
            format!("the device did not list its services: {err}"),
        )
    })?;
    let characteristic = peripheral
        .characteristics()
        .into_iter()
        .find(|candidate| candidate.uuid == CHARACTERISTIC_UUID)
        .ok_or_else(|| FailureReason::ConfigInvalid {
            detail: "the device offers no MIDI characteristic".to_owned(),
        })?;

    // Read what it sends, for as long as the link lasts.
    peripheral.subscribe(&characteristic).await.map_err(|err| {
        step_failure(
            shared.is_unavailable(),
            format!("the device refused to send MIDI notifications: {err}"),
        )
    })?;
    if let Some(sink) = sink {
        let reader = peripheral.clone();
        let id = id.clone();
        tokio::spawn(async move { read(reader, id, sink, shared).await });
    }

    info!(peripheral = %id, "bluetooth link connected");
    Ok(Link {
        peripheral,
        characteristic,
        encoder: Encoder::new(MAX_PACKET),
    })
}

/// Names why a step failed on a device that had already accepted the connection.
///
/// A radio that has since gone off is the adapter's doing. Otherwise the radio works and the
/// device did not complete what every BLE MIDI device must, and saying the adapter was
/// unavailable sent the user to switch on a radio that was already on.
fn step_failure(adapter_unusable: bool, detail: String) -> FailureReason {
    if adapter_unusable {
        FailureReason::AdapterUnavailable
    } else {
        FailureReason::ProtocolError { detail }
    }
}

/// Decodes notifications into the endpoint's ring for as long as the link lasts.
///
/// This is an ordinary task rather than a platform callback: `btleplug` has already copied the
/// packet into a buffer it owns by the time it reaches here, so there is no real-time context to
/// protect. The ring is still the boundary, and pushing into it is all this does with the bytes.
async fn read(peripheral: Peripheral, id: PeripheralId, mut sink: RtProducer, shared: Arc<Shared>) {
    let Ok(mut notifications) = peripheral.notifications().await else {
        return;
    };
    let mut decoder = Decoder::new();

    while let Some(notification) = notifications.next().await {
        let decoded = decoder.decode(&notification.value, &mut |event| match event {
            Event::Message { at, message } => {
                sink.push(message, at);
            }
            Event::SysEx { at, bytes, end } => {
                sink.push_sysex(bytes, end, at);
            }
        });
        if let Err(err) = decoded {
            // A malformed packet is the device's fault, not a reason to drop the link: the next
            // one may be fine, and a keyboard that stops working because of one bad write is
            // worse than one that drops a note. Counted, so the endpoint shows it happening.
            sink.record_malformed();
            debug!(peripheral = %id, error = %err, "discarded a malformed bluetooth packet");
        }
    }

    info!(peripheral = %id, "bluetooth link closed");
    shared.report(BluetoothEvent::Disconnected(id));
}

/// Writes every packet of one message, dropping the link's registration if the device has gone.
async fn write_all(link: &Link, packets: &[Vec<u8>], shared: &Arc<Shared>, handle: u64) {
    for packet in packets {
        if let Err(err) = link
            .peripheral
            .write(&link.characteristic, packet, WriteType::WithoutResponse)
            .await
        {
            debug!(error = %err, "failed to write a bluetooth packet");
            let id = shared
                .links
                .lock()
                .ok()
                .and_then(|mut open| open.remove(&handle));
            if let Some(id) = id {
                shared.report(BluetoothEvent::Disconnected(id));
            }
            return;
        }
    }
}

/// Narrows the scan to devices offering MIDI.
///
/// On macOS the filter is applied by the operating system, which is the only way to see a service
/// UUID that CoreBluetooth has pushed into the advertisement's overflow area. On Linux it is
/// applied here as well, in `describe`, because BlueZ reports everything it hears.
fn scan_filter() -> ScanFilter {
    ScanFilter {
        services: vec![SERVICE_UUID],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks how an adapter state reads as availability: powered off is off, powered on is
    /// ready, a refused permission outranks either, and an unknown state leaves the last answer
    /// standing.
    ///
    /// CoreBluetooth reports a refused permission as a manager that is not powered, and reports
    /// both a refusal and a radio still starting up as unknown. Switching the radio on does
    /// nothing for a refused process, so the guidance stays on the permission. The power was
    /// once read only at startup, so a radio switched off later was reported ready.
    #[test]
    fn an_adapter_state_reads_as_availability_with_a_refusal_first() {
        let refused = UnavailableReason::PermissionDenied {
            what: "Bluetooth".to_owned(),
        };
        let cases = [
            (
                "powered off",
                CentralState::PoweredOff,
                None,
                Some(Some(UnavailableReason::AdapterOff)),
            ),
            ("powered on", CentralState::PoweredOn, None, Some(None)),
            (
                "powered off and refused",
                CentralState::PoweredOff,
                Some(refused.clone()),
                Some(Some(refused.clone())),
            ),
            (
                "powered on and refused",
                CentralState::PoweredOn,
                Some(refused.clone()),
                Some(Some(refused)),
            ),
            ("unknown", CentralState::Unknown, None, None),
        ];
        for (name, state, refusal, want) in cases {
            assert_eq!(
                availability(state, refusal),
                want,
                "{name}: the user must be sent to the switch or the permission that is at fault"
            );
        }
    }

    /// Locks which sightings are passed on: a device not heard before, and one whose name has
    /// arrived or changed, but not the same device heard again.
    ///
    /// A BLE device's name usually comes in the scan response, after the advertisement that first
    /// reports it. Passing on only the first sighting listed a named device as nameless for as long as it stayed in range, and
    /// passing on every advertisement would flood the daemon.
    #[test]
    fn a_sighting_is_news_only_when_the_device_or_its_name_is_new() {
        let sighting = |name: Option<&str>| DiscoveredPeripheral {
            id: PeripheralId::new("hci0/dev_E8_48_B8_C8_20_00".to_owned()),
            name: name.map(str::to_owned),
            rssi: Some(-60),
            paired: false,
        };
        let cases = [
            ("a first sighting", None, sighting(None), true),
            (
                "a name arriving",
                Some(sighting(None)),
                sighting(Some("Harbor Linux")),
                true,
            ),
            (
                "a name changing",
                Some(sighting(Some("Old"))),
                sighting(Some("New")),
                true,
            ),
            (
                "the same device again",
                Some(sighting(Some("Harbor Linux"))),
                sighting(Some("Harbor Linux")),
                false,
            ),
        ];
        for (name, previous, found, want) in cases {
            assert_eq!(
                is_news(previous.as_ref(), &found),
                want,
                "{name}: only a new device or a new name is worth passing on"
            );
        }
    }
}
