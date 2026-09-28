//! Advertising this machine as a BLE MIDI peripheral, over BlueZ.
//!
//! `btleplug` is explicitly central-only — its own documentation sends peripheral users elsewhere
//! — so this half is BlueZ's own binding. It follows the same shape as the central: a thread with
//! its own runtime, driven by a command queue, reporting outcomes as events rather than blocking
//! a caller on the radio.
//!
//! BlueZ hands a GATT characteristic's traffic over as a pair of pipes rather than callbacks, so
//! reading from a connected central and notifying it are both ordinary tasks. The ring is still
//! the boundary, and pushing into it is all that is done with the bytes.

use super::{BluetoothEvent, CHARACTERISTIC_UUID, SERVICE_UUID};
use crate::error::PlatformError;
use bluer::adv::Advertisement;
use bluer::gatt::CharacteristicReader;
use bluer::gatt::local::{
    Application, Characteristic, CharacteristicControl, CharacteristicControlEvent,
    CharacteristicNotify, CharacteristicNotifyMethod, CharacteristicWrite,
    CharacteristicWriteMethod, Service, characteristic_control,
};
use midi_harbor_blemidi::codec::{Decoder, Encoder, Event, MAX_PACKET};
use midi_harbor_core::capability::UnavailableReason;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tracing::{debug, error, info};

/// The largest packet BLE MIDI framing allows, used until a central negotiates something smaller.
const DEFAULT_MTU: usize = MAX_PACKET;

/// How often subscriptions are checked for a central that has gone.
///
/// BlueZ closes a notification session when its central leaves, but nothing is told unless it
/// looks. Found only when a write failed, a central that left stayed "connected" for as long as
/// nothing was sent to it.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// One instruction for the backend thread.
enum Command {
    /// Begin advertising under a name, sending anything received into the sink.
    Advertise {
        name: String,
        sink: Option<RtProducer>,
    },
    /// Send messages to every subscribed central.
    Notify { messages: Vec<MidiMessage> },
    /// Send one whole system-exclusive message to every subscribed central.
    NotifySysEx { bytes: Vec<u8> },
    /// Stop advertising and drop any central still connected.
    Stop,
    /// Stop the backend thread.
    Shutdown,
}

/// State the seam reads without going through the backend thread.
#[derive(Default)]
struct Shared {
    /// Events waiting to be drained.
    events: Mutex<Vec<BluetoothEvent>>,
    /// Why the peripheral role is unusable, when it is.
    unavailable: Mutex<Option<UnavailableReason>>,
    /// The name being advertised, when advertising.
    advertised: Mutex<Option<String>>,
}

impl Shared {
    /// Records an event for the next drain.
    fn report(&self, event: BluetoothEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
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

/// The peripheral half of the Bluetooth seam on Linux.
pub struct BluerPeripheral {
    commands: mpsc::UnboundedSender<Command>,
    shared: Arc<Shared>,
}

impl BluerPeripheral {
    /// Starts the backend thread and its runtime.
    pub fn start() -> Result<Self, PlatformError> {
        let (commands, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);

        std::thread::Builder::new()
            .name("midi-harbor-ble-adv".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        error!(error = %err, "failed to start the bluetooth advertising runtime");
                        thread_shared.set_unavailable(Some(UnavailableReason::NotBuilt));
                        return;
                    }
                };
                runtime.block_on(run(receiver, thread_shared));
            })
            .map_err(|err| PlatformError::Os {
                operation: "starting the bluetooth advertising thread",
                detail: err.to_string(),
            })?;

        Ok(Self { commands, shared })
    }

    /// Reports why the peripheral role cannot be used, or `None` when it can.
    pub fn unavailable(&self) -> Option<UnavailableReason> {
        self.shared
            .unavailable
            .lock()
            .ok()
            .and_then(|held| held.clone())
    }

    /// Returns the name being advertised, if any.
    pub fn advertised_name(&self) -> Option<String> {
        self.shared
            .advertised
            .lock()
            .ok()
            .and_then(|held| held.clone())
    }

    /// Begins advertising under `name`.
    pub fn advertise(&self, name: &str, sink: Option<RtProducer>) -> Result<(), PlatformError> {
        if let Some(reason) = self.unavailable() {
            return Err(match reason {
                UnavailableReason::PermissionDenied { what } => {
                    PlatformError::PermissionDenied { what }
                }
                _ => PlatformError::AdapterUnavailable,
            });
        }
        self.queue(Command::Advertise {
            name: name.to_owned(),
            sink,
        })
    }

    /// Sends messages to every subscribed central.
    pub fn notify(&self, messages: &[MidiMessage]) -> Result<(), PlatformError> {
        self.advertising()?;
        self.queue(Command::Notify {
            messages: messages.to_vec(),
        })
    }

    /// Sends one whole system-exclusive message to every subscribed central.
    pub fn notify_sysex(&self, bytes: &[u8]) -> Result<(), PlatformError> {
        self.advertising()?;
        self.queue(Command::NotifySysEx {
            bytes: bytes.to_vec(),
        })
    }

    /// Stops advertising.
    pub fn stop_advertising(&self) -> Result<(), PlatformError> {
        self.queue(Command::Stop)
    }

    /// Takes the events observed since the last call.
    pub fn drain_events(&self) -> Vec<BluetoothEvent> {
        match self.shared.events.lock() {
            Ok(mut events) => std::mem::take(&mut events),
            Err(poisoned) => std::mem::take(&mut poisoned.into_inner()),
        }
    }

    /// Fails when nothing is advertised, so a send into nothing is reported rather than dropped.
    fn advertising(&self) -> Result<(), PlatformError> {
        if self.advertised_name().is_some() {
            Ok(())
        } else {
            Err(PlatformError::NotFound("no advertised port".to_owned()))
        }
    }

    /// Queues one command, treating a stopped backend as an unavailable adapter.
    fn queue(&self, command: Command) -> Result<(), PlatformError> {
        self.commands
            .send(command)
            .map_err(|_| PlatformError::AdapterUnavailable)
    }
}

impl Drop for BluerPeripheral {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

/// What BlueZ hands back while an advertisement and its service are registered.
///
/// Dropping either handle withdraws the registration, so they are held for as long as the port is
/// advertised and never merely created.
struct Registration {
    _advertisement: bluer::adv::AdvertisementHandle,
    _application: bluer::gatt::local::ApplicationHandle,
}

/// Where notifications are written, once a central subscribes.
type Writers = Arc<tokio::sync::Mutex<Vec<bluer::gatt::CharacteristicWriter>>>;

/// The backend thread's main loop.
async fn run(mut commands: mpsc::UnboundedReceiver<Command>, shared: Arc<Shared>) {
    let adapter = match open_adapter(&shared).await {
        Some(adapter) => adapter,
        None => {
            while let Some(command) = commands.recv().await {
                if matches!(command, Command::Shutdown) {
                    return;
                }
            }
            return;
        }
    };

    let started = Instant::now();
    let writers: Writers = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    // Held rather than read: dropping it is what withdraws the advertisement and the service.
    let mut registration: Option<Registration> = None;
    let mut encoder = Encoder::new(DEFAULT_MTU);
    let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let command = tokio::select! {
            command = commands.recv() => command,
            _ = sweep.tick() => {
                sweep_closed(&writers, &shared).await;
                continue;
            }
        };
        let Some(command) = command else {
            return;
        };
        match command {
            Command::Advertise { name, sink } => {
                // Withdraw any existing registration first, because BlueZ refuses a second
                // application on the same path.
                drop(registration.take());
                writers.lock().await.clear();
                match register(
                    &adapter,
                    &name,
                    sink,
                    Arc::clone(&writers),
                    Arc::clone(&shared),
                )
                .await
                {
                    Ok(held) => {
                        registration = Some(held);
                        if let Ok(mut advertised) = shared.advertised.lock() {
                            *advertised = Some(name.clone());
                        }
                        info!(name = %name, "advertising as a bluetooth midi peripheral");
                    }
                    Err(err) => {
                        error!(name = %name, error = %err, "failed to advertise over bluetooth");
                        shared.set_unavailable(Some(UnavailableReason::AdapterOff));
                    }
                }
            }
            Command::Notify { messages } => {
                let at = started.elapsed().as_millis() as u64;
                let mut packets = Vec::new();
                for message in &messages {
                    encoder.push(at, message, &mut |packet| packets.push(packet.to_vec()));
                }
                encoder.flush(&mut |packet| packets.push(packet.to_vec()));
                write_all(&writers, &packets, &shared).await;
            }
            Command::NotifySysEx { bytes } => {
                let at = started.elapsed().as_millis() as u64;
                let mut packets = Vec::new();
                match encoder.push_sysex(at, &bytes, &mut |packet| packets.push(packet.to_vec())) {
                    Ok(()) => {
                        encoder.flush(&mut |packet| packets.push(packet.to_vec()));
                        write_all(&writers, &packets, &shared).await;
                    }
                    Err(err) => {
                        error!(error = %err, "refused to send a malformed dump over bluetooth");
                    }
                }
            }
            Command::Stop => {
                drop(registration.take());
                writers.lock().await.clear();
                if let Ok(mut advertised) = shared.advertised.lock() {
                    *advertised = None;
                }
                info!("stopped advertising as a bluetooth midi peripheral");
            }
            Command::Shutdown => return,
        }
    }
}

/// Opens the adapter, recording why the role is unusable when it cannot.
async fn open_adapter(shared: &Arc<Shared>) -> Option<bluer::Adapter> {
    let session = match bluer::Session::new().await {
        Ok(session) => session,
        Err(err) => {
            debug!(error = %err, "no bluetooth session");
            shared.set_unavailable(Some(UnavailableReason::MissingSystemComponent {
                component: "BlueZ".to_owned(),
            }));
            return None;
        }
    };
    let adapter = match session.default_adapter().await {
        Ok(adapter) => adapter,
        Err(err) => {
            debug!(error = %err, "no bluetooth adapter to advertise from");
            shared.set_unavailable(Some(UnavailableReason::NoAdapter));
            return None;
        }
    };
    if let Err(err) = adapter.set_powered(true).await {
        debug!(error = %err, "failed to power the bluetooth adapter");
        shared.set_unavailable(Some(UnavailableReason::AdapterOff));
        return None;
    }
    shared.set_unavailable(None);
    Some(adapter)
}

/// Registers the advertisement and the GATT service, and starts reading what a central sends.
async fn register(
    adapter: &bluer::Adapter,
    name: &str,
    sink: Option<RtProducer>,
    writers: Writers,
    shared: Arc<Shared>,
) -> bluer::Result<Registration> {
    // Advertise the service, so a central browsing for MIDI finds us.
    let advertisement = Advertisement {
        service_uuids: [SERVICE_UUID].into_iter().collect(),
        discoverable: Some(true),
        local_name: Some(name.to_owned()),
        ..Default::default()
    };
    let advertisement = adapter.advertise(advertisement).await?;

    // Serve the one characteristic every BLE MIDI device carries. BlueZ delivers subscriptions
    // and incoming writes over a control stream rather than through callbacks, so the handle is
    // created first and the stream followed for as long as the registration lasts.
    let (control, control_handle) = characteristic_control();
    let application = Application {
        services: vec![Service {
            uuid: SERVICE_UUID,
            primary: true,
            characteristics: vec![Characteristic {
                uuid: CHARACTERISTIC_UUID,
                write: Some(CharacteristicWrite {
                    write_without_response: true,
                    method: CharacteristicWriteMethod::Io,
                    ..Default::default()
                }),
                notify: Some(CharacteristicNotify {
                    notify: true,
                    method: CharacteristicNotifyMethod::Io,
                    ..Default::default()
                }),
                control_handle,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let application = adapter.serve_gatt_application(application).await?;

    let sink = Arc::new(tokio::sync::Mutex::new(sink));
    tokio::spawn(async move { follow(control, sink, writers, shared).await });

    Ok(Registration {
        _advertisement: advertisement,
        _application: application,
    })
}

/// Follows one characteristic's control stream for as long as the port is advertised.
///
/// This is where a central connecting is noticed at all: BlueZ reports a subscription by handing
/// over a writer, and an incoming write by handing over a reader.
async fn follow(
    mut control: CharacteristicControl,
    sink: Arc<tokio::sync::Mutex<Option<RtProducer>>>,
    writers: Writers,
    shared: Arc<Shared>,
) {
    while let Some(event) = control.next().await {
        match event {
            CharacteristicControlEvent::Notify(writer) => {
                let central = writer.device_address().to_string();
                writers.lock().await.push(writer);
                shared.report(BluetoothEvent::CentralSubscribed(central));
            }
            CharacteristicControlEvent::Write(request) => match request.accept() {
                Ok(reader) => {
                    let sink = Arc::clone(&sink);
                    tokio::spawn(async move { read(reader, sink).await });
                }
                Err(err) => {
                    debug!(error = %err, "refused an incoming bluetooth write");
                }
            },
        }
    }
}

/// Drops the subscriptions whose central has gone, reporting each.
async fn sweep_closed(writers: &Writers, shared: &Arc<Shared>) {
    let mut held = writers.lock().await;
    let mut index = 0;
    while let Some(writer) = held.get(index) {
        // An error reading the session's state is treated as the session being over: a writer
        // that cannot be asked cannot be written to either.
        if writer.is_closed().unwrap_or(true) {
            let writer = held.remove(index);
            debug!(central = %writer.device_address(), "a central stopped listening");
            shared.report(BluetoothEvent::CentralUnsubscribed(
                writer.device_address().to_string(),
            ));
        } else {
            index += 1;
        }
    }
}

/// Writes every packet to every subscribed central, dropping those that have gone.
async fn write_all(writers: &Writers, packets: &[Vec<u8>], shared: &Arc<Shared>) {
    let mut held = writers.lock().await;
    let mut gone = Vec::new();

    for (index, writer) in held.iter_mut().enumerate() {
        for packet in packets {
            if let Err(err) = writer.write_all(packet).await {
                debug!(error = %err, "failed to notify a subscribed central");
                gone.push(index);
                break;
            }
        }
    }

    // Remove from the back, so earlier indices stay valid.
    for index in gone.into_iter().rev() {
        if index < held.len() {
            let writer = held.remove(index);
            shared.report(BluetoothEvent::CentralUnsubscribed(
                writer.device_address().to_string(),
            ));
        }
    }
}

/// Reads one central's writes into the endpoint's ring for as long as it stays connected.
async fn read(mut reader: CharacteristicReader, sink: Arc<tokio::sync::Mutex<Option<RtProducer>>>) {
    let mut decoder = Decoder::new();
    let mut buffer = vec![0_u8; reader.mtu()];

    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        let Some(packet) = buffer.get(..read) else {
            return;
        };
        let mut held = sink.lock().await;
        let Some(sink) = held.as_mut() else {
            continue;
        };
        let decoded = decoder.decode(packet, &mut |event| match event {
            Event::Message { at, message } => {
                sink.push(message, at);
            }
            Event::SysEx { at, bytes, end } => {
                sink.push_sysex(bytes, end, at);
            }
        });
        if let Err(err) = decoded {
            // One bad write is the central's fault, not a reason to drop it: a phone that stops
            // working because of a single malformed packet is worse than one that drops a note.
            sink.record_malformed();
            debug!(error = %err, "discarded a malformed bluetooth packet");
        }
    }
}
