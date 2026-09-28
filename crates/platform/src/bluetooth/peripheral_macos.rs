//! Advertising this machine as a BLE MIDI peripheral, over CoreBluetooth.
//!
//! The same shape as the Linux half — a thread with its own runtime, a command queue, outcomes as
//! events — over a different library, because `btleplug` is central-only on every platform.
//!
//! CoreBluetooth delivers everything through a delegate, which this crate forwards as a channel,
//! so reading a central's writes and notifying it are both ordinary tasks. The ring stays the
//! boundary.

use super::{BluetoothEvent, CHARACTERISTIC_UUID, SERVICE_UUID};
use crate::error::PlatformError;
use ble_peripheral_rust::gatt::characteristic::Characteristic;
use ble_peripheral_rust::gatt::peripheral_event::{
    PeripheralEvent, RequestResponse, WriteRequestResponse,
};
use ble_peripheral_rust::gatt::properties::{AttributePermission, CharacteristicProperty};
use ble_peripheral_rust::gatt::service::Service;
use ble_peripheral_rust::{Peripheral, PeripheralImpl};
use midi_harbor_blemidi::codec::{Decoder, Encoder, Event, MAX_PACKET};
use midi_harbor_core::capability::UnavailableReason;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

/// How many delegate events may queue before the oldest caller waits.
///
/// Generous, because the only producer is CoreBluetooth's delegate and blocking it would stall
/// the radio rather than merely delaying a log line.
const EVENT_QUEUE: usize = 256;

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
    /// Stop advertising.
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
    /// Whether CoreBluetooth has said anything yet. Until it has, the adapter's state is a guess.
    settled: std::sync::atomic::AtomicBool,
}

impl Shared {
    /// Records an event for the next drain.
    fn report(&self, event: BluetoothEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }

    /// Records why the role cannot be used, reporting the change once.
    ///
    /// CoreBluetooth's first answer settles a guess rather than changing anything, so it is
    /// reported only if it says the radio is unusable. Reporting the guess and then the answer
    /// put "the adapter is switched off" and "became available" in the history at every start,
    /// on a machine whose adapter was on throughout.
    fn set_unavailable(&self, reason: Option<UnavailableReason>) {
        let first = !self.settled.swap(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut held) = self.unavailable.lock() {
            if *held == reason && !first {
                return;
            }
            *held = reason.clone();
        }
        if first && reason.is_none() {
            return;
        }
        self.report(BluetoothEvent::AdapterChanged(reason));
    }

    /// Records the state assumed until CoreBluetooth says otherwise, without reporting it.
    fn assume_unavailable(&self, reason: UnavailableReason) {
        if let Ok(mut held) = self.unavailable.lock() {
            *held = Some(reason);
        }
    }
}

/// The peripheral half of the Bluetooth seam on macOS.
pub struct CoreBluetoothPeripheral {
    commands: mpsc::UnboundedSender<Command>,
    shared: Arc<Shared>,
}

impl CoreBluetoothPeripheral {
    /// Starts the backend thread and its runtime.
    pub fn start() -> Result<Self, PlatformError> {
        let (commands, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);

        // Unknown until CoreBluetooth reports the adapter's state, which it does asynchronously.
        thread_shared.assume_unavailable(UnavailableReason::AdapterOff);

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

impl Drop for CoreBluetoothPeripheral {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

/// The backend thread's main loop.
async fn run(mut commands: mpsc::UnboundedReceiver<Command>, shared: Arc<Shared>) {
    let (delegate, delegate_events) = mpsc::channel(EVENT_QUEUE);
    let mut peripheral = match Peripheral::new(delegate).await {
        Ok(peripheral) => peripheral,
        Err(err) => {
            error!(error = %err, "failed to open the bluetooth peripheral manager");
            shared.set_unavailable(Some(UnavailableReason::NoAdapter));
            drain(&mut commands).await;
            return;
        }
    };

    // Everything a central does arrives on the delegate channel, including the adapter's state,
    // so following it is how the role learns it is usable at all.
    let sink = Arc::new(tokio::sync::Mutex::new(None));
    let listener_sink = Arc::clone(&sink);
    let listener_shared = Arc::clone(&shared);
    tokio::spawn(async move { follow(delegate_events, listener_sink, listener_shared).await });

    let started = Instant::now();
    let mut encoder = Encoder::new(MAX_PACKET);
    let mut served = false;

    while let Some(command) = commands.recv().await {
        match command {
            Command::Advertise { name, sink: feed } => {
                *sink.lock().await = feed;

                // CoreBluetooth keeps a service for the life of the manager, so adding it twice
                // is an error rather than a replacement.
                if !served {
                    if let Err(err) = peripheral.add_service(&midi_service()).await {
                        error!(error = %err, "failed to publish the bluetooth midi service");
                        shared.set_unavailable(Some(UnavailableReason::AdapterOff));
                        continue;
                    }
                    served = true;
                }

                match peripheral.start_advertising(&name, &[SERVICE_UUID]).await {
                    Ok(()) => {
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
                notify_all(&mut peripheral, &packets).await;
            }
            Command::NotifySysEx { bytes } => {
                let at = started.elapsed().as_millis() as u64;
                let mut packets = Vec::new();
                match encoder.push_sysex(at, &bytes, &mut |packet| packets.push(packet.to_vec())) {
                    Ok(()) => {
                        encoder.flush(&mut |packet| packets.push(packet.to_vec()));
                        notify_all(&mut peripheral, &packets).await;
                    }
                    Err(err) => {
                        error!(error = %err, "refused to send a malformed dump over bluetooth");
                    }
                }
            }
            Command::Stop => {
                if let Err(err) = peripheral.stop_advertising().await {
                    debug!(error = %err, "failed to stop advertising over bluetooth");
                }
                *sink.lock().await = None;
                if let Ok(mut advertised) = shared.advertised.lock() {
                    *advertised = None;
                }
                info!("stopped advertising as a bluetooth midi peripheral");
            }
            Command::Shutdown => {
                let _ = peripheral.stop_advertising().await;
                return;
            }
        }
    }
}

/// The one service every BLE MIDI device carries.
fn midi_service() -> Service {
    Service {
        uuid: SERVICE_UUID,
        primary: true,
        characteristics: vec![Characteristic {
            uuid: CHARACTERISTIC_UUID,
            properties: vec![
                CharacteristicProperty::Read,
                CharacteristicProperty::WriteWithoutResponse,
                CharacteristicProperty::Notify,
            ],
            permissions: vec![
                AttributePermission::Readable,
                AttributePermission::Writeable,
            ],
            ..Default::default()
        }],
    }
}

/// Follows the delegate channel for as long as the backend runs.
async fn follow(
    mut events: mpsc::Receiver<PeripheralEvent>,
    sink: Arc<tokio::sync::Mutex<Option<RtProducer>>>,
    shared: Arc<Shared>,
) {
    let mut decoder = Decoder::new();

    while let Some(event) = events.recv().await {
        match event {
            PeripheralEvent::StateUpdate { is_powered } => {
                // This crate reports only whether the manager is powered, which is false both for
                // a radio switched off and for a process refused permission, so the authorization
                // is asked for separately.
                shared.set_unavailable((!is_powered).then(super::authorization::unpowered_reason));
            }
            PeripheralEvent::CharacteristicSubscriptionUpdate {
                request,
                subscribed,
            } => {
                shared.report(if subscribed {
                    BluetoothEvent::CentralSubscribed(request.client)
                } else {
                    BluetoothEvent::CentralUnsubscribed(request.client)
                });
            }
            PeripheralEvent::WriteRequest {
                value, responder, ..
            } => {
                {
                    let mut held = sink.lock().await;
                    if let Some(sink) = held.as_mut() {
                        let decoded = decoder.decode(&value, &mut |event| match event {
                            Event::Message { at, message } => {
                                sink.push(message, at);
                            }
                            Event::SysEx { at, bytes, end } => {
                                sink.push_sysex(bytes, end, at);
                            }
                        });
                        if let Err(err) = decoded {
                            // One bad write is the central's fault, not a reason to drop it.
                            sink.record_malformed();
                            debug!(error = %err, "discarded a malformed bluetooth packet");
                        }
                    }
                }
                // Answered either way: a central left waiting on a response stops sending.
                let _ = responder.send(WriteRequestResponse {
                    response: RequestResponse::Success,
                });
            }
            PeripheralEvent::ReadRequest { responder, .. } => {
                // BLE MIDI carries nothing readable — the characteristic is declared readable
                // only because the specification says so — so an empty answer is the whole truth.
                let _ = responder.send(
                    ble_peripheral_rust::gatt::peripheral_event::ReadRequestResponse {
                        value: Vec::new(),
                        response: RequestResponse::Success,
                    },
                );
            }
        }
    }
}

/// Sends every packet to whoever is subscribed.
async fn notify_all(peripheral: &mut Peripheral, packets: &[Vec<u8>]) {
    for packet in packets {
        if let Err(err) = peripheral
            .update_characteristic(CHARACTERISTIC_UUID, packet.clone())
            .await
        {
            debug!(error = %err, "failed to notify a subscribed central");
            return;
        }
    }
}

/// Consumes commands without acting on them, for a backend that never came up.
///
/// Draining rather than dropping the receiver keeps callers getting an ordinary refusal instead
/// of a queue that grows for as long as the daemon runs.
async fn drain(commands: &mut mpsc::UnboundedReceiver<Command>) {
    while let Some(command) = commands.recv().await {
        if matches!(command, Command::Shutdown) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks which of CoreBluetooth's answers are reported as adapter changes: the first answer
    /// only when it says the radio is unusable, since it settles a guess rather than changing
    /// anything, and each later change once.
    ///
    /// Reporting the guess and then the answer put "the adapter is switched off" and "became
    /// available" in the history at every start, on a machine whose adapter was on throughout.
    #[test]
    fn only_real_changes_in_the_adapter_are_reported() {
        let off = || Some(UnavailableReason::AdapterOff);
        let cases = [
            ("found on at startup", vec![None], Vec::new()),
            ("found off at startup", vec![off()], vec![off()]),
            (
                "switched off, repeated, then on",
                vec![None, off(), off(), None],
                vec![off(), None],
            ),
        ];
        for (name, answers, want) in cases {
            let shared = Shared::default();
            shared.assume_unavailable(UnavailableReason::AdapterOff);
            for answer in answers {
                shared.set_unavailable(answer);
            }
            let reported: Vec<Option<UnavailableReason>> = shared
                .events
                .lock()
                .expect("the event list must not be poisoned")
                .drain(..)
                .filter_map(|event| match event {
                    BluetoothEvent::AdapterChanged(reason) => Some(reason),
                    _ => None,
                })
                .collect();
            assert_eq!(
                reported, want,
                "{name}: the history must hold each real change once and nothing else"
            );
        }
    }
}
