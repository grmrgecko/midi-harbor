//! Bluetooth links, and the memory that brings them back.
//!
//! A Bluetooth device is closer to attached hardware than to a network session: the user pairs
//! with it once and expects it to work every time it is switched on afterwards, without being
//! told about it again. So a device connected here is written into the configuration like a piece
//! of hardware, and coming back into range is enough to reconnect it (FR-020).
//!
//! What it does not share with hardware is identity. A fingerprint describes what a device *is*,
//! and two identical keyboards need position to tell them apart; a Bluetooth address is issued by
//! the platform and is already unique, so it is the whole of the identity and nothing else is
//! matched on.

use crate::dataplane;
use crate::state::{Change, Daemon, DaemonError, Runtime};
use midi_harbor_core::capability::UnavailableReason;
use midi_harbor_core::config;
use midi_harbor_core::endpoint::{BleRole, BluetoothDevice, Endpoint, EndpointKind, EndpointName};
use midi_harbor_core::events::{self, EventKind, Severity};
use midi_harbor_core::failure::FailureReason;
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::state::ConnectionState;
use midi_harbor_platform::bluetooth::{
    BluetoothEvent, BluetoothRole, DiscoveredPeripheral, PeripheralId,
};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// How often the Bluetooth backend is asked what it has seen.
///
/// The same cadence as the MIDI environment watcher, for the same reason: a device appearing
/// should reach the interface in well under the time it takes a person to look at it.
pub const BLUETOOTH_EVENT_INTERVAL: Duration = Duration::from_millis(500);

/// How long a scan runs when the caller does not say.
///
/// Scanning costs power and airtime on every device in range, so it stops on its own. A user who
/// wants longer asks for longer.
pub const DEFAULT_SCAN: Duration = Duration::from_secs(30);

/// Why the radio is listening.
///
/// Two reasons share one radio: a scan the user asked for, which ends on its own, and devices
/// waiting to reconnect, which can only be heard while it listens. The Bluetooth watcher reads
/// both each tick and starts or stops the scan when the answer changes.
#[derive(Debug, Default)]
pub struct ScanPlan {
    /// Whether the radio is scanning now.
    running: bool,
    /// When the scan the user asked for ends.
    user_until: Option<std::time::Instant>,
}

impl Daemon {
    /// Begins looking for BLE MIDI peripherals, stopping on its own after `duration`.
    ///
    /// Stops on its own because a scan left running is a battery cost the user never asked for
    /// and cannot see. Devices waiting to reconnect keep the radio listening after it ends.
    pub async fn start_bluetooth_scan(
        self: &Arc<Self>,
        duration: Option<Duration>,
    ) -> Result<(), DaemonError> {
        self.bluetooth_role_available(BluetoothRole::Central)?;
        let mut plan = self.bt_scan.lock().await;
        plan.user_until = Some(std::time::Instant::now() + duration.unwrap_or(DEFAULT_SCAN));
        if !plan.running {
            self.bluetooth.start_scan().map_err(platform_failure)?;
            plan.running = true;
        }
        Ok(())
    }

    /// Stops the scan the user asked for. The radio goes on listening while a device waits to
    /// reconnect.
    pub async fn stop_bluetooth_scan(self: &Arc<Self>) -> Result<(), DaemonError> {
        self.bt_scan.lock().await.user_until = None;
        self.keep_listening().await;
        Ok(())
    }

    /// Starts or stops scanning so the radio listens exactly while something needs it.
    ///
    /// A device is heard only while the radio listens, and hearing it is the only sign it is
    /// back. Scanning only when the user asked meant a device that dropped afterwards never
    /// reconnected.
    pub(crate) async fn keep_listening(self: &Arc<Self>) {
        let waiting = self.bluetooth_devices_waiting().await;
        let mut plan = self.bt_scan.lock().await;
        let asked = plan
            .user_until
            .is_some_and(|until| std::time::Instant::now() < until);
        if !asked {
            plan.user_until = None;
        }
        let wanted =
            (asked || waiting > 0) && self.bluetooth.unavailable(BluetoothRole::Central).is_none();

        if wanted && !plan.running {
            match self.bluetooth.start_scan() {
                Ok(()) => {
                    plan.running = true;
                    if waiting > 0 && !asked {
                        info!(waiting, "listening for bluetooth devices to come back");
                    }
                }
                Err(error) => debug!(error = %error, "could not start the bluetooth scan"),
            }
        } else if !wanted && plan.running {
            plan.running = false;
            if let Err(error) = self.bluetooth.stop_scan() {
                debug!(error = %error, "could not stop the bluetooth scan");
            }
        }
    }

    /// Counts the remembered devices this machine connects to that are not connected now.
    async fn bluetooth_devices_waiting(&self) -> usize {
        let remembered: Vec<EndpointId> = {
            let inner = self.inner.read().await;
            inner
                .config
                .endpoints
                .iter()
                .filter(|endpoint| {
                    endpoint.enabled
                        && matches!(
                            &endpoint.kind,
                            EndpointKind::BluetoothDevice(device)
                                if device.paired && device.role == BleRole::Central
                        )
                })
                .map(|endpoint| endpoint.id)
                .collect()
        };
        let links = self.bt_links.read().await;
        remembered
            .iter()
            .filter(|id| !links.contains_key(id))
            .count()
    }

    /// Returns the peripherals currently in range, whether or not they are known.
    pub async fn bluetooth_in_range(&self) -> Vec<DiscoveredPeripheral> {
        self.bt_seen.read().await.clone()
    }

    /// Returns the address of every configured Bluetooth device, and the endpoint that holds it.
    ///
    /// Used to mark a discovered peripheral as already known, so the interface offers to connect
    /// to a stranger and to disconnect from a device the user already paired.
    pub async fn bluetooth_endpoints(&self) -> std::collections::HashMap<String, EndpointId> {
        let inner = self.inner.read().await;
        inner
            .config
            .endpoints
            .iter()
            .filter_map(|endpoint| match &endpoint.kind {
                EndpointKind::BluetoothDevice(device) if !device.address.is_empty() => {
                    Some((device.address.clone(), endpoint.id))
                }
                _ => None,
            })
            .collect()
    }

    /// Reports what this machine can do, asking the radio rather than assuming.
    ///
    /// Whether Bluetooth works is not decidable from the build: it depends on an adapter being
    /// present, switched on and permitted, which only a started backend knows. Answering without
    /// asking one is how this reported "not included in this build" on a machine whose radio was
    /// working.
    ///
    /// The daemon settles the two the platform cannot: whether a service manager exists to install
    /// into, and whether discovery started. Both were once reported available regardless.
    pub fn capabilities(&self) -> midi_harbor_core::capability::CapabilitySet {
        settle_capabilities(
            &midi_harbor_platform::capability::query_with(Some(self.bluetooth.as_ref())),
            self.service_unavailable.as_ref(),
            self.discovery_started(),
        )
    }

    /// Reports whether a link to this endpoint is open.
    ///
    /// The configuration entry outlives the link by design, so "configured" and "connected" are
    /// different questions and the interface has to be able to ask the second one.
    pub async fn bluetooth_link_open(&self, id: EndpointId) -> bool {
        self.bt_links.read().await.contains_key(&id)
    }

    /// Connects to a peripheral by address, remembering it so it returns on its own next time.
    pub async fn connect_bluetooth(
        self: &Arc<Self>,
        address: &str,
    ) -> Result<Endpoint, DaemonError> {
        self.bluetooth_role_available(BluetoothRole::Central)?;

        // Find it among what the radio can actually see, so connecting to something that walked
        // away fails here rather than after a ten-second wait.
        let found = self
            .bluetooth_in_range()
            .await
            .into_iter()
            .find(|peripheral| peripheral.id.as_str() == address)
            .ok_or_else(|| DaemonError::NotFound(address.to_owned()))?;

        let endpoint = self.remember_bluetooth(&found).await?;
        // The radio may have lost it since it was last reported. That is the same answer as never
        // having heard it, and reported as a removed device it sent the user looking for one that
        // had never been connected.
        match self.open_bluetooth_link(&endpoint).await {
            Err(DaemonError::Failure(FailureReason::DeviceRemoved)) => {
                Err(DaemonError::NotFound(address.to_owned()))
            }
            opened => opened.map(|()| endpoint),
        }
    }

    /// Closes a link without forgetting the device.
    ///
    /// The configuration entry stays, because the user disconnecting a keyboard for the evening
    /// is not the same as telling the system to stop knowing about it.
    pub async fn disconnect_bluetooth(
        self: &Arc<Self>,
        id: EndpointId,
    ) -> Result<Endpoint, DaemonError> {
        self.silence_endpoint(id).await;

        if let Some(link) = self.bt_links.write().await.remove(&id)
            && let Err(error) = self.bluetooth.disconnect(link)
        {
            debug!(endpoint = %id, error = %error, "could not close the bluetooth link");
        }

        let mut inner = self.inner.write().await;
        if let Some(runtime) = inner.runtime.get_mut(&id) {
            runtime.handle = None;
            let _ = runtime
                .state
                .apply_now(midi_harbor_core::state::Event::Disable, self.clock.as_ref());
        }
        let endpoint = inner
            .config
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == id)
            .cloned()
            .ok_or_else(|| DaemonError::NotFound(id.to_string()))?;
        drop(inner);

        let _ = self.changes.send(Change::EndpointChanged(id));
        Ok(endpoint)
    }

    /// Forgets a device, so it no longer reconnects when it comes back into range.
    pub async fn forget_bluetooth(
        self: &Arc<Self>,
        id: EndpointId,
    ) -> Result<Vec<String>, DaemonError> {
        let _ = self.disconnect_bluetooth(id).await;

        let mut inner = self.inner.write().await;
        let Some(index) = inner.config.endpoints.iter().position(|e| e.id == id) else {
            return Err(DaemonError::NotFound(id.to_string()));
        };
        let is_bluetooth = inner
            .config
            .endpoints
            .get(index)
            .is_some_and(|endpoint| matches!(endpoint.kind, EndpointKind::BluetoothDevice(_)));
        if !is_bluetooth {
            return Err(DaemonError::Failure(FailureReason::ConfigInvalid {
                detail: "only Bluetooth devices can be forgotten this way".to_owned(),
            }));
        }
        let endpoint = inner.config.endpoints.remove(index);
        inner.runtime.remove(&id);

        // Routes naming it are reported rather than removed, for the same reason as hardware: the
        // user may be about to pair a replacement under the same name.
        let orphaned: Vec<String> = inner
            .config
            .routes
            .iter()
            .filter(|route| route.touches(&endpoint))
            .map(|route| format!("{} -> {}", route.from, route.to))
            .collect();

        config::save(&self.paths, &inner.config)?;
        drop(inner);

        info!(device = %endpoint.name.as_str(), "forgot a bluetooth device");
        let _ = self.changes.send(Change::EndpointRemoved(id));
        Ok(orphaned)
    }

    /// Advertises this machine as a BLE MIDI peripheral, or stops doing so.
    pub async fn set_peripheral_advertising(
        self: &Arc<Self>,
        enabled: bool,
        name: Option<String>,
    ) -> Result<Option<Endpoint>, DaemonError> {
        self.remember_advertising_choice(enabled).await?;

        if !enabled {
            self.bluetooth
                .stop_advertising()
                .map_err(platform_failure)?;
            let previous = self.bt_advertised.write().await.take();
            self.bt_centrals
                .store(0, std::sync::atomic::Ordering::SeqCst);
            if let Some(id) = previous {
                let mut inner = self.inner.write().await;
                if let Some(runtime) = inner.runtime.get_mut(&id) {
                    runtime.handle = None;
                    let _ = runtime
                        .state
                        .apply_now(midi_harbor_core::state::Event::Disable, self.clock.as_ref());
                }
                drop(inner);
                let _ = self.changes.send(Change::EndpointChanged(id));
            }
            return Ok(None);
        }

        self.bluetooth_role_available(BluetoothRole::Peripheral)?;

        // The name comes from the endpoint, not from the request. A user who named their port
        // once should not find it renamed to the machine's hostname after a restart, and a user
        // who names it again is renaming the port rather than advertising a second one.
        let endpoint = self.remember_advertised(name.as_deref()).await?;
        let advertised = endpoint.name.as_str().to_owned();

        let (producer, consumer) = dataplane::channel(endpoint.id);
        self.bluetooth
            .advertise(&advertised, Some(producer))
            .map_err(platform_failure)?;

        // Waiting for a device to connect, which is not the same as one being connected. It
        // once read connected the moment advertising began, and stayed disabled if advertising
        // had been switched off and on.
        self.bt_centrals
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.mark_advertising_waiting(endpoint.id).await;
        *self.bt_advertised.write().await = Some(endpoint.id);
        self.start_dispatch(consumer);

        info!(name = %advertised, "advertising as a bluetooth midi peripheral");
        if !midi_harbor_platform::bluetooth::advertised_name_fits(&advertised) {
            warn!(
                name = %advertised,
                "the name is too long to fit beside the midi service; devices will show this machine's own name"
            );
        }
        Ok(Some(endpoint))
    }

    /// Records whether this machine should advertise, so the choice survives a restart.
    ///
    /// The advertised endpoint's own switch is kept in step, because it and the preference are
    /// one choice. Left alone, the endpoint list showed it switched on while nothing advertised.
    async fn remember_advertising_choice(&self, enabled: bool) -> Result<(), DaemonError> {
        let mut inner = self.inner.write().await;
        let mut changed = inner.config.preferences.bluetooth_advertising != enabled;
        inner.config.preferences.bluetooth_advertising = enabled;
        for endpoint in &mut inner.config.endpoints {
            let advertised = matches!(
                &endpoint.kind,
                EndpointKind::BluetoothDevice(device) if device.role == BleRole::Peripheral
            );
            if advertised && endpoint.enabled != enabled {
                endpoint.enabled = enabled;
                changed = true;
            }
        }
        if changed {
            config::save(&self.paths, &inner.config)?;
        }
        Ok(())
    }

    /// Gives every Bluetooth endpoint a state to report from the moment the daemon starts.
    ///
    /// A Bluetooth endpoint gains runtime state only when the radio reports something about it,
    /// so after a restart every one read "unknown" in a warning colour, including the advertised
    /// endpoint while advertising was simply off.
    pub(crate) async fn seed_bluetooth_states(&self) {
        let now = self.clock.as_ref().now();
        let mut inner = self.inner.write().await;
        let advertising = inner.config.preferences.bluetooth_advertising;
        let unseeded: Vec<(EndpointId, bool)> = inner
            .config
            .endpoints
            .iter()
            .filter(|endpoint| !inner.runtime.contains_key(&endpoint.id))
            .filter_map(|endpoint| match &endpoint.kind {
                EndpointKind::BluetoothDevice(device) => {
                    let wanted =
                        endpoint.enabled && (device.role != BleRole::Peripheral || advertising);
                    Some((endpoint.id, wanted))
                }
                _ => None,
            })
            .collect();
        for (id, wanted) in unseeded {
            // Wanted but not yet heard is disconnected: a remembered device reconnects when it
            // is next heard, and advertising resumes once the radio is ready.
            let state = if wanted {
                ConnectionState::enabled(now)
            } else {
                ConnectionState::disabled(now)
            };
            inner.runtime.insert(id, Runtime::new(state, None));
        }
    }

    /// Starts advertising if the configuration says to, once the radio can.
    ///
    /// Deferred rather than done at startup: CoreBluetooth reports the adapter's state
    /// asynchronously, so a daemon that tried once at boot would decide the radio was unavailable
    /// a moment before it became available.
    async fn resume_advertising_if_configured(self: &Arc<Self>) {
        let wanted = {
            let inner = self.inner.read().await;
            inner.config.preferences.bluetooth_advertising
        };
        if !wanted || self.bt_advertised.read().await.is_some() {
            return;
        }
        if self
            .bluetooth
            .unavailable(BluetoothRole::Peripheral)
            .is_some()
        {
            return;
        }
        if let Err(error) = self.set_peripheral_advertising(true, None).await {
            warn!(error = %error, "could not resume advertising over bluetooth");
        }
    }

    /// Watches the radio for devices arriving, leaving, and connecting to us.
    ///
    /// Reconnection lives here rather than in a retry loop because a Bluetooth device gives no
    /// warning and answers no probe: the only signal that it is back is its advertisement, so
    /// hearing one *is* the retry.
    pub(crate) fn watch_bluetooth(self: &Arc<Self>) {
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(BLUETOOTH_EVENT_INTERVAL);
            loop {
                ticker.tick().await;
                for event in daemon.bluetooth.drain_events() {
                    daemon.handle_bluetooth_event(event).await;
                }
                // Checked every tick rather than only after an event, because the radio becoming
                // usable is not something every backend announces, and a machine configured to
                // advertise must not stay silent waiting for a message that never comes.
                daemon.resume_advertising_if_configured().await;
                daemon.keep_listening().await;
            }
        });
    }

    /// Acts on one thing the radio reported.
    async fn handle_bluetooth_event(self: &Arc<Self>, event: BluetoothEvent) {
        match event {
            BluetoothEvent::PeripheralFound(found) => {
                {
                    let mut seen = self.bt_seen.write().await;
                    seen.retain(|known| known.id != found.id);
                    seen.push(found.clone());
                }
                self.reconnect_if_known(&found).await;
            }
            BluetoothEvent::PeripheralLost(id) => {
                self.bt_seen.write().await.retain(|known| known.id != id);
            }
            BluetoothEvent::Connected(id) => {
                if let Some(endpoint) = self.bluetooth_endpoint_for(&id).await {
                    self.mark_bluetooth_connected(endpoint).await;
                }
            }
            BluetoothEvent::Disconnected(id) => {
                if let Some(endpoint) = self.bluetooth_endpoint_for(&id).await {
                    self.mark_bluetooth_lost(endpoint).await;
                }
            }
            BluetoothEvent::LinkFailed { id, reason } => {
                if let Some(endpoint) = self.bluetooth_endpoint_for(&id).await {
                    self.mark_bluetooth_failed(endpoint, reason).await;
                }
            }
            BluetoothEvent::CentralSubscribed(who) => {
                info!(central = %who, "a device connected to our bluetooth port");
                self.bt_centrals
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some(id) = *self.bt_advertised.read().await {
                    self.mark_bluetooth_connected(id).await;
                }
            }
            BluetoothEvent::CentralUnsubscribed(who) => {
                info!(central = %who, "a device left our bluetooth port");
                let left = self
                    .bt_centrals
                    .fetch_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |count| Some(count.saturating_sub(1)),
                    )
                    .map_or(0, |previous| previous.saturating_sub(1));
                if left == 0
                    && let Some(id) = *self.bt_advertised.read().await
                {
                    // The last device to leave can no longer release what it was playing.
                    self.silence_routes_from(id).await;
                    self.mark_advertising_waiting(id).await;
                }
            }
            BluetoothEvent::AdapterChanged(reason) => {
                let now = self.clock.as_ref().now();
                let detail = match &reason {
                    Some(reason) => format!("bluetooth became unavailable: {reason}"),
                    None => "bluetooth became available".to_owned(),
                };
                let mut inner = self.inner.write().await;
                let _ = inner.events.record(events::event(
                    EventKind::CapabilitiesChanged,
                    if reason.is_some() {
                        Severity::Warning
                    } else {
                        Severity::Info
                    },
                    now,
                    detail,
                ));
            }
        }
    }

    /// Reconnects a remembered device that has just come back into range.
    async fn reconnect_if_known(self: &Arc<Self>, found: &DiscoveredPeripheral) {
        let endpoint = {
            let inner = self.inner.read().await;
            inner
                .config
                .endpoints
                .iter()
                .find(|endpoint| {
                    endpoint.enabled
                        && matches!(
                            &endpoint.kind,
                            EndpointKind::BluetoothDevice(device)
                                if device.paired && device.address == found.id.as_str()
                        )
                })
                .cloned()
        };
        let Some(endpoint) = endpoint else {
            return;
        };
        if self.bt_links.read().await.contains_key(&endpoint.id) {
            return;
        }

        // Heard again before its backoff ran out. A device that connects and drops at once would
        // otherwise be reconnected every time it advertised, which is what the backoff exists to
        // stop. It is not ignored, though: the radio may not report it again, so the reconnect is
        // scheduled for when the backoff ends rather than left to the next sighting.
        let now = self.clock.now();
        let wait = {
            let inner = self.inner.read().await;
            inner
                .runtime
                .get(&endpoint.id)
                .and_then(|runtime| runtime.state.next_retry())
                .map(|at| midi_harbor_core::time::elapsed(now, at))
                .filter(|wait| !wait.is_zero())
        };
        if let Some(wait) = wait {
            let first = self.bt_deferred.write().await.insert(endpoint.id);
            if first {
                debug!(device = %endpoint.name.as_str(), ?wait, "heard during its backoff; reconnecting when it ends");
                let daemon = Arc::clone(self);
                let id = endpoint.id;
                tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    daemon.bt_deferred.write().await.remove(&id);
                    // Checked again, because the user may have switched it off or forgotten it,
                    // or it may have connected some other way, while this waited.
                    let still_wanted = {
                        let inner = daemon.inner.read().await;
                        inner.config.endpoint(id).filter(|e| e.enabled).cloned()
                    };
                    let Some(endpoint) = still_wanted else {
                        return;
                    };
                    if daemon.bt_links.read().await.contains_key(&id) {
                        return;
                    }
                    if let Err(error) = daemon.open_bluetooth_link(&endpoint).await {
                        warn!(device = %endpoint.name.as_str(), error = %error, "could not reconnect it");
                    }
                });
            }
            return;
        }

        info!(device = %endpoint.name.as_str(), "a remembered bluetooth device returned");
        if let Err(error) = self.open_bluetooth_link(&endpoint).await {
            warn!(device = %endpoint.name.as_str(), error = %error, "could not reconnect it");
        }
    }

    /// Opens the link for a configured endpoint and starts carrying its MIDI.
    async fn open_bluetooth_link(self: &Arc<Self>, endpoint: &Endpoint) -> Result<(), DaemonError> {
        let EndpointKind::BluetoothDevice(device) = &endpoint.kind else {
            return Err(DaemonError::Failure(FailureReason::ConfigInvalid {
                detail: "that endpoint is not a Bluetooth device".to_owned(),
            }));
        };

        // A link already open or opening is left alone. Connecting while the device reconnected
        // by itself opened a second link over the first: every message it sent arrived twice, and
        // the first link, no longer held, could not be closed. The check and the insert share one
        // lock so the two paths cannot both pass it.
        let mut links = self.bt_links.write().await;
        if links.contains_key(&endpoint.id) {
            return Ok(());
        }
        let (producer, consumer) = dataplane::channel(endpoint.id);
        let link = self
            .bluetooth
            .connect(&PeripheralId::new(device.address.clone()), Some(producer))
            .map_err(platform_failure)?;
        links.insert(endpoint.id, link);
        drop(links);

        self.mark_bluetooth_connecting(endpoint.id).await;
        self.start_dispatch(consumer);
        Ok(())
    }

    /// Stores a newly connected device, or returns the entry it already has.
    async fn remember_bluetooth(
        self: &Arc<Self>,
        found: &DiscoveredPeripheral,
    ) -> Result<Endpoint, DaemonError> {
        let mut inner = self.inner.write().await;

        if let Some(existing) = inner.config.endpoints.iter().find(|endpoint| {
            matches!(
                &endpoint.kind,
                EndpointKind::BluetoothDevice(device) if device.address == found.id.as_str()
            )
        }) {
            return Ok(existing.clone());
        }

        // A device that advertises no name still has to be callable, and its address is the only
        // other thing it has.
        let proposed = found
            .name
            .clone()
            .unwrap_or_else(|| format!("Bluetooth {}", found.id));
        let name = unique_name(&inner.config, &proposed);

        let endpoint = Endpoint::new(
            EndpointName::new(name)?,
            EndpointKind::BluetoothDevice(BluetoothDevice {
                address: found.id.to_string(),
                role: BleRole::Central,
                // Pairing here means the user asked for this device by name, which is what makes
                // it come back on its own later.
                paired: true,
                rssi: found.rssi,
            }),
        );
        let id = endpoint.id;
        inner.config.add_endpoint(endpoint.clone());
        config::save(&self.paths, &inner.config)?;
        drop(inner);

        let _ = self.changes.send(Change::EndpointAdded(id));
        Ok(endpoint)
    }

    /// Stores the endpoint standing for this machine's own advertised port.
    ///
    /// There is only ever one: this machine has one radio, so a second advertised port would be
    /// a second name for the same thing.
    async fn remember_advertised(
        self: &Arc<Self>,
        name: Option<&str>,
    ) -> Result<Endpoint, DaemonError> {
        let existing = {
            let inner = self.inner.read().await;
            inner
                .config
                .endpoints
                .iter()
                .find(|endpoint| {
                    matches!(
                        &endpoint.kind,
                        EndpointKind::BluetoothDevice(device) if device.role == BleRole::Peripheral
                    )
                })
                .cloned()
        };

        if let Some(existing) = existing {
            // Naming it again renames the one port, rewriting the routes that name it, rather
            // than leaving a port whose name no longer matches what is on the air.
            return match name {
                Some(name) if name != existing.name.as_str() => {
                    self.rename_endpoint(existing.id, name, true).await
                }
                _ => Ok(existing),
            };
        }

        let mut inner = self.inner.write().await;
        let proposed = name.unwrap_or(self.machine_name(&inner.config));
        let endpoint = Endpoint::new(
            EndpointName::new(unique_name(&inner.config, proposed))?,
            EndpointKind::BluetoothDevice(BluetoothDevice {
                // Our own port has no address of its own: it is this machine, whatever address
                // the radio happens to present to whoever connects.
                address: String::new(),
                role: BleRole::Peripheral,
                paired: false,
                rssi: None,
            }),
        );
        let id = endpoint.id;
        inner.config.add_endpoint(endpoint.clone());
        config::save(&self.paths, &inner.config)?;
        drop(inner);

        let _ = self.changes.send(Change::EndpointAdded(id));
        Ok(endpoint)
    }

    /// Finds the endpoint that names a peripheral address.
    async fn bluetooth_endpoint_for(&self, id: &PeripheralId) -> Option<EndpointId> {
        let inner = self.inner.read().await;
        inner
            .config
            .endpoints
            .iter()
            .find(|endpoint| {
                matches!(
                    &endpoint.kind,
                    EndpointKind::BluetoothDevice(device) if device.address == id.as_str()
                )
            })
            .map(|endpoint| endpoint.id)
    }

    /// Records a link as attempting, before the radio has answered.
    async fn mark_bluetooth_connecting(self: &Arc<Self>, id: EndpointId) {
        let now = self.clock.as_ref().now();
        {
            let mut inner = self.inner.write().await;
            let runtime = inner
                .runtime
                .entry(id)
                .or_insert_with(|| Runtime::new(ConnectionState::enabled(now), None));
            let _ = runtime.state.apply_now(
                midi_harbor_core::state::Event::Attempting,
                self.clock.as_ref(),
            );
        }
        let _ = self.changes.send(Change::EndpointChanged(id));
    }

    /// Records a link as carrying MIDI, bringing a recovered one back to its controller state.
    async fn mark_bluetooth_connected(self: &Arc<Self>, id: EndpointId) {
        let now = self.clock.as_ref().now();
        let (handle, restoration) = {
            let mut inner = self.inner.write().await;
            let runtime = inner
                .runtime
                .entry(id)
                .or_insert_with(|| Runtime::new(ConnectionState::enabled(now), None));
            let effects = runtime.state.apply_now(
                midi_harbor_core::state::Event::Established,
                self.clock.as_ref(),
            );
            let recovered = effects.contains(&midi_harbor_core::state::Effect::RestoreState);
            // A device that was switched off and on has lost what it was set to (FR-027).
            let restoration = if recovered {
                runtime
                    .controls
                    .lock()
                    .map(|controls| controls.restore())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let handle = runtime.handle;
            let name = endpoint_name(&inner, id);
            let detail = if recovered {
                format!("{name} is back and connected")
            } else {
                format!("{name} connected")
            };
            record_link_event(&mut inner, id, now, Severity::Info, detail);
            (handle, restoration)
        };
        if !restoration.is_empty() {
            info!(
                endpoint = %id,
                messages = restoration.len(),
                "restoring controller state on a recovered bluetooth link"
            );
            if self.deliver(id, handle, &restoration).await {
                let at = self.clock.now();
                self.observe(id, &restoration, true, at).await;
            }
        }
        let _ = self.changes.send(Change::EndpointChanged(id));
    }

    /// Records the advertised port as waiting for a device to connect.
    ///
    /// Set directly rather than through an event: the connection machine has no step back to
    /// waiting that is not a failure, and a device leaving the port is not one, nor a reason to
    /// retry anything.
    async fn mark_advertising_waiting(self: &Arc<Self>, id: EndpointId) {
        let now = self.clock.as_ref().now();
        {
            let mut inner = self.inner.write().await;
            let runtime = inner
                .runtime
                .entry(id)
                .or_insert_with(|| Runtime::new(ConnectionState::enabled(now), None));
            runtime.state = ConnectionState::enabled(now);
            let name = endpoint_name(&inner, id);
            record_link_event(
                &mut inner,
                id,
                now,
                Severity::Info,
                format!("the last device left {name}; it is advertising again"),
            );
        }
        let _ = self.changes.send(Change::EndpointChanged(id));
    }

    /// Records a link as lost, leaving the configuration entry in place.
    async fn mark_bluetooth_lost(self: &Arc<Self>, id: EndpointId) {
        // A radio reports a link closed after this side closed it, or after it failed to come up.
        // With no link held there is nothing to lose. Taken as a loss, it replaced the real
        // reason with "device was removed" and logged a reconnect the user had not asked for.
        if !self.bt_links.read().await.contains_key(&id) {
            debug!(endpoint = %id, "the radio reported a link already closed");
            return;
        }
        // Nothing can release what was playing on the device, nor what it was playing into its
        // routes, exactly as for unplugged hardware.
        self.silence_endpoint(id).await;
        self.silence_routes_from(id).await;
        self.bt_links.write().await.remove(&id);

        let now = self.clock.as_ref().now();
        {
            let mut inner = self.inner.write().await;
            let runtime = inner
                .runtime
                .entry(id)
                .or_insert_with(|| Runtime::new(ConnectionState::enabled(now), None));
            runtime.handle = None;
            let _ = runtime.state.apply_now(
                midi_harbor_core::state::Event::Lost(FailureReason::DeviceRemoved),
                self.clock.as_ref(),
            );
            let name = endpoint_name(&inner, id);
            record_link_event(
                &mut inner,
                id,
                now,
                Severity::Warning,
                format!("{name} lost its link; it reconnects when the device is back in range"),
            );
        }
        info!(endpoint = %id, "bluetooth link lost; it will reconnect when the device returns");
        let _ = self.changes.send(Change::EndpointChanged(id));
    }

    /// Records a link that was asked for and never came up, with the reason the radio gave.
    ///
    /// Not a loss: nothing was carried, so nothing needs silencing, and the reason is the radio's
    /// rather than "device was removed", which every failure once read as.
    async fn mark_bluetooth_failed(self: &Arc<Self>, id: EndpointId, reason: FailureReason) {
        self.bt_links.write().await.remove(&id);
        let now = self.clock.as_ref().now();
        {
            let mut inner = self.inner.write().await;
            let runtime = inner
                .runtime
                .entry(id)
                .or_insert_with(|| Runtime::new(ConnectionState::enabled(now), None));
            runtime.handle = None;
            // Only the first failure of a streak: a device out of range fails every time it is
            // tried, and the first already says what is wrong.
            let first = runtime.state.attempt() == 0;
            let _ = runtime.state.apply_now(
                midi_harbor_core::state::Event::Attempting,
                self.clock.as_ref(),
            );
            let _ = runtime.state.apply_now(
                midi_harbor_core::state::Event::Failed(reason.clone()),
                self.clock.as_ref(),
            );
            if first {
                let name = endpoint_name(&inner, id);
                record_link_event(
                    &mut inner,
                    id,
                    now,
                    Severity::Warning,
                    format!("{name} could not connect: {reason}"),
                );
            }
        }
        info!(endpoint = %id, reason = %reason, "bluetooth link did not come up; it will be tried again when the device is heard");
        let _ = self.changes.send(Change::EndpointChanged(id));
    }

    /// Refuses a request for a role the radio cannot play, naming why.
    ///
    /// `FailureReason` is closed and maps onto IPC codes, so the platform's reason is narrowed
    /// rather than carried whole: a permission the user can grant is worth its own variant,
    /// because telling someone to switch on a radio that is already on sends them nowhere. The
    /// rest collapse into one, and the distinction between them survives in the capability query,
    /// which is where FR-053 asks for it.
    fn bluetooth_role_available(&self, role: BluetoothRole) -> Result<(), DaemonError> {
        let Some(reason) = self.bluetooth.unavailable(role) else {
            return Ok(());
        };
        debug!(role = %role, reason = %reason, "refused a bluetooth request");
        Err(DaemonError::Failure(match reason {
            UnavailableReason::PermissionDenied { what } => {
                FailureReason::PermissionDenied { what }
            }
            _ => FailureReason::AdapterUnavailable,
        }))
    }
}

/// Turns a platform refusal into a domain failure, so callers switch on a reason rather than text.
fn platform_failure(error: midi_harbor_platform::PlatformError) -> DaemonError {
    DaemonError::Failure(error.as_failure_reason())
}

/// Returns a name no other endpoint is using.
///
/// Two devices advertising the same name is ordinary — a pair of identical controllers ship with
/// one — and routes name their endpoints, so a duplicate would make a route ambiguous.
fn unique_name(config: &midi_harbor_core::config::Configuration, proposed: &str) -> String {
    let taken = |candidate: &str| {
        config
            .endpoints
            .iter()
            .any(|endpoint| endpoint.name.as_str() == candidate)
    };
    if !taken(proposed) {
        return proposed.to_owned();
    }
    for suffix in 2..=u16::MAX {
        let candidate = format!("{proposed} {suffix}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    proposed.to_owned()
}

/// Settles what only the daemon knows on top of what the platform probed: whether a service
/// manager exists to install into, and whether discovery started.
fn settle_capabilities(
    probed: &midi_harbor_core::capability::CapabilitySet,
    service_unavailable: Option<&UnavailableReason>,
    discovery_started: bool,
) -> midi_harbor_core::capability::CapabilitySet {
    use midi_harbor_core::capability::{Capability, CapabilityName, CapabilitySet};
    let discovery_missing =
        (!discovery_started).then(|| UnavailableReason::MissingSystemComponent {
            component: "multicast DNS".to_owned(),
        });
    CapabilitySet::new(
        probed
            .all()
            .iter()
            .map(|capability| {
                let overridden = match capability.name {
                    CapabilityName::ServiceManager => service_unavailable.cloned(),
                    // The platform's own reason, such as a missing responder, is the more useful.
                    CapabilityName::MdnsResponder if capability.available => {
                        discovery_missing.clone()
                    }
                    _ => None,
                };
                match overridden {
                    Some(reason) => Capability::unavailable(capability.name, reason),
                    None => capability.clone(),
                }
            })
            .collect(),
    )
}

/// Returns an endpoint's name for the history, or its identifier if it has none.
fn endpoint_name(inner: &crate::state::Inner, id: EndpointId) -> String {
    inner
        .config
        .endpoints
        .iter()
        .find(|endpoint| endpoint.id == id)
        .map_or_else(|| id.to_string(), |endpoint| endpoint.name.to_string())
}

/// Records a Bluetooth link changing state in the history, against its endpoint.
///
/// A link that dropped and came back was otherwise visible only in the log, so the history could
/// not say when a Bluetooth device dropped or why (FR-046, SC-013).
fn record_link_event(
    inner: &mut crate::state::Inner,
    id: EndpointId,
    at: jiff::Timestamp,
    severity: Severity,
    detail: String,
) {
    let mut event = events::event(EventKind::EndpointStateChanged, severity, at, detail);
    event.endpoint = Some(id);
    let _ = inner.events.record(event);
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use midi_harbor_core::capability::{Capability, CapabilityName, CapabilitySet};

    /// Returns a probe in which every capability is available, except the responder when a reason
    /// for its absence is given.
    fn probed(responder_missing: Option<&UnavailableReason>) -> CapabilitySet {
        CapabilitySet::new(
            CapabilityName::all()
                .into_iter()
                .map(|name| match (name, responder_missing) {
                    (CapabilityName::MdnsResponder, Some(reason)) => {
                        Capability::unavailable(name, reason.clone())
                    }
                    _ => Capability::available(name),
                })
                .collect(),
        )
    }

    /// Returns the reason a component is named as missing.
    fn missing(component: &str) -> UnavailableReason {
        UnavailableReason::MissingSystemComponent {
            component: component.to_owned(),
        }
    }

    /// Proves the daemon overrides the platform's probe only for the two capabilities it alone
    /// knows (a service manager to install into, and whether discovery started), and that a
    /// reason the platform already gave for a missing responder wins over the daemon's generic
    /// one, since "avahi-daemon" tells the user what to install and "multicast DNS" does not.
    /// Both overrides exist because each capability was once reported available regardless.
    #[test]
    fn the_daemon_settles_only_what_the_platform_cannot_probe() {
        struct Case {
            name: &'static str,
            responder_missing: Option<UnavailableReason>,
            service_unavailable: Option<UnavailableReason>,
            discovery_started: bool,
            want_service: Option<UnavailableReason>,
            want_responder: Option<UnavailableReason>,
        }
        let cases = [
            Case {
                name: "a healthy machine is left as probed",
                responder_missing: None,
                service_unavailable: None,
                discovery_started: true,
                want_service: None,
                want_responder: None,
            },
            Case {
                name: "a machine with no service manager cannot install the service",
                responder_missing: None,
                service_unavailable: Some(missing("systemd")),
                discovery_started: true,
                want_service: Some(missing("systemd")),
                want_responder: None,
            },
            Case {
                name: "discovery that never started is not available",
                responder_missing: None,
                service_unavailable: None,
                discovery_started: false,
                want_service: None,
                want_responder: Some(missing("multicast DNS")),
            },
            Case {
                name: "the platform's own reason for a missing responder is kept",
                responder_missing: Some(missing("avahi-daemon")),
                service_unavailable: None,
                discovery_started: false,
                want_service: None,
                want_responder: Some(missing("avahi-daemon")),
            },
        ];

        for case in cases {
            let settled = settle_capabilities(
                &probed(case.responder_missing.as_ref()),
                case.service_unavailable.as_ref(),
                case.discovery_started,
            );
            let reason = |name| {
                settled
                    .get(name)
                    .unwrap_or_else(|| panic!("{}: {name:?} was dropped from the set", case.name))
                    .reason
                    .clone()
            };
            assert_eq!(
                reason(CapabilityName::ServiceManager),
                case.want_service,
                "{}: the service manager reason is wrong",
                case.name
            );
            assert_eq!(
                reason(CapabilityName::MdnsResponder),
                case.want_responder,
                "{}: the responder reason is wrong",
                case.name
            );
            for name in CapabilityName::all() {
                if !matches!(
                    name,
                    CapabilityName::ServiceManager | CapabilityName::MdnsResponder
                ) {
                    assert!(
                        settled.is_available(name),
                        "{}: {name:?} was probed available and nothing the daemon knows changes it",
                        case.name
                    );
                }
            }
        }
    }
}
