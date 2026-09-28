//! Applying a changed configuration to a running daemon.
//!
//! Reloading the file and importing a setup both end here, and both have to honour FR-050: only
//! the connections whose configuration actually changed are disturbed. Each endpoint in the new
//! document is compared with the one running, and what differs decides how much is done to it:
//! nothing, a change made in place, a switch on or off, or closing and reopening it.

use crate::state::{Change, Daemon, DaemonError};
use midi_harbor_core::config::{self, ConfigError, Configuration};
use midi_harbor_core::endpoint::{Endpoint, EndpointKind};
use midi_harbor_core::events::{EventKind, Severity};
use midi_harbor_core::ids::EndpointId;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// What applying a configuration did, by endpoint name.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Endpoints that did not exist before.
    pub added: Vec<String>,
    /// Endpoints that no longer exist.
    pub removed: Vec<String>,
    /// Endpoints closed and opened again because something they depend on changed.
    pub restarted: Vec<String>,
    /// Routes that did not exist before.
    pub routes_added: usize,
    /// Settings accepted but only used from the next start, named so the caller can say so.
    pub pending_restart: Vec<String>,
}

/// How much applying a change to one endpoint disturbs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disturbance {
    /// Nothing that is stored differs.
    None,
    /// Only something that can change while it runs, such as a label or an invitation policy.
    InPlace,
    /// Switched on or off, and nothing else that would need a restart.
    Toggled,
    /// Something it was opened with differs, so it is closed and opened again.
    Restart,
}

impl Daemon {
    /// Returns the configuration as the document `save` would write.
    pub async fn export_configuration(&self) -> Result<String, DaemonError> {
        let inner = self.inner.read().await;
        Ok(config::to_text(&inner.config)?)
    }

    /// Reads the configuration file again and applies what changed in it.
    ///
    /// A file that cannot be read is refused and left where it is, with everything still
    /// running. At startup the daemon falls back to defaults instead, because it has nothing to
    /// lose; here, falling back would tear down a working setup over a typo.
    pub async fn reload_configuration(self: &Arc<Self>) -> Result<Applied, DaemonError> {
        let path = self.paths.config_file();
        let text = std::fs::read_to_string(&path).map_err(|source| {
            DaemonError::Config(ConfigError::Io {
                operation: "read",
                path: path.clone(),
                source,
            })
        })?;
        let incoming = config::parse(&text)?;
        self.apply(incoming).await
    }

    /// Applies a setup exported from this machine or another.
    ///
    /// Merging adds what this machine lacks and changes nothing it has. Replacing makes the setup
    /// match the document. Either way an endpoint that matches one here by kind and name keeps
    /// this machine's identity for it, so a setup moved between machines lines up with what is
    /// already running instead of restarting it, and this machine keeps its own name.
    pub async fn import_configuration(
        self: &Arc<Self>,
        text: &str,
        replace: bool,
    ) -> Result<Applied, DaemonError> {
        let offered = config::parse(text)?;
        let current = self.inner.read().await.config.clone();
        let incoming = if replace {
            replaced(&current, offered)
        } else {
            merged(&current, offered)
        };
        // A merge matches endpoints by kind and name, so a network port offered under a virtual
        // port's name here, or the other way round, would be added beside it.
        config::check_names(&incoming)?;
        self.apply(incoming).await
    }

    /// Makes the running setup match `incoming`, disturbing only what differs.
    async fn apply(self: &Arc<Self>, mut incoming: Configuration) -> Result<Applied, DaemonError> {
        let current = self.inner.read().await.config.clone();
        let mut applied = Applied::default();

        // Decide what happens to every endpoint before touching any of them.
        let mut taken_down: Vec<Endpoint> = Vec::new();
        let mut changed: Vec<EndpointId> = Vec::new();
        for old in &current.endpoints {
            match incoming.endpoints.iter().find(|new| new.id == old.id) {
                None => {
                    applied.removed.push(old.name.to_string());
                    taken_down.push(old.clone());
                }
                Some(new) => match disturbance(old, new) {
                    Disturbance::None => {}
                    Disturbance::InPlace => changed.push(old.id),
                    Disturbance::Toggled => {
                        changed.push(old.id);
                        taken_down.push(old.clone());
                    }
                    Disturbance::Restart => {
                        applied.restarted.push(new.name.to_string());
                        changed.push(old.id);
                        taken_down.push(old.clone());
                    }
                },
            }
        }
        let existing: HashSet<EndpointId> = current.endpoints.iter().map(|e| e.id).collect();
        let added: Vec<EndpointId> = incoming
            .endpoints
            .iter()
            .filter(|endpoint| !existing.contains(&endpoint.id))
            .map(|endpoint| {
                applied.added.push(endpoint.name.to_string());
                endpoint.id
            })
            .collect();
        applied.routes_added = incoming
            .routes
            .iter()
            .filter(|route| !current.routes.iter().any(|held| held.same_ends(route)))
            .count();

        // Tear down while the old routes still exist, because silencing what an endpoint was
        // sending finds the destinations through them.
        for endpoint in &taken_down {
            self.take_down(endpoint).await;
        }

        // Swap the document in. What the daemon observes about hardware and radios is not
        // stored, so it is carried over. The device refresh below would work it out again, but
        // until then every device would read as absent to anyone looking, and the retry loop
        // skips absent hardware.
        carry_observations(&current, &mut incoming);
        let advertising_changed =
            current.preferences.bluetooth_advertising != incoming.preferences.bluetooth_advertising;
        let announcing_changed =
            current.preferences.advertise_sessions != incoming.preferences.advertise_sessions;
        {
            let mut inner = self.inner.write().await;
            inner.config = incoming.clone();
            crate::state::sync_route_counters(&mut inner);
            config::save(&self.paths, &inner.config)?;
        }

        // Bring up whatever should be running and is not.
        self.reconcile().await;
        self.start_configured_sessions().await;
        self.push_invitation_policy().await;
        self.refresh_devices().await;
        if advertising_changed {
            let enabled = incoming.preferences.bluetooth_advertising;
            if let Err(error) = self.set_peripheral_advertising(enabled, None).await {
                warn!(error = %error, "could not apply the bluetooth advertising preference");
            }
        }
        if announcing_changed {
            self.apply_session_advertising(incoming.preferences.advertise_sessions)
                .await;
        }

        // Tell clients, and keep a record of what the change did.
        for endpoint in &current.endpoints {
            if !incoming.endpoints.iter().any(|new| new.id == endpoint.id) {
                let _ = self.changes.send(Change::EndpointRemoved(endpoint.id));
            }
        }
        for id in added {
            let _ = self.changes.send(Change::EndpointAdded(id));
        }
        for id in changed {
            let _ = self.changes.send(Change::EndpointChanged(id));
        }
        let summary = format!(
            "configuration applied: {} added, {} removed, {} restarted, {} new routes",
            applied.added.len(),
            applied.removed.len(),
            applied.restarted.len(),
            applied.routes_added
        );
        info!(
            added = applied.added.len(),
            removed = applied.removed.len(),
            restarted = applied.restarted.len(),
            "configuration applied"
        );
        {
            let mut inner = self.inner.write().await;
            let now = self.clock.now();
            let _ = inner.events.record(midi_harbor_core::events::event(
                EventKind::ConfigurationChanged,
                Severity::Info,
                now,
                summary,
            ));
        }
        Ok(applied)
    }

    /// Stops an endpoint so the new configuration can decide whether and how it runs again.
    ///
    /// Silences in both directions first: what it was playing, and what it was sending
    /// elsewhere, since both lose their only source of note offs when it stops.
    pub(crate) async fn take_down(self: &Arc<Self>, endpoint: &Endpoint) {
        let id = endpoint.id;
        self.silence_endpoint(id).await;
        self.silence_routes_from(id).await;

        match &endpoint.kind {
            EndpointKind::NetworkSession(session) => {
                self.stop_session(id, session.local_name.as_str()).await;
            }
            EndpointKind::BluetoothDevice(_) => {
                if let Some(link) = self.bt_links.write().await.remove(&id)
                    && let Err(error) = self.bluetooth.disconnect(link)
                {
                    debug!(endpoint = %endpoint.name, error = %error, "could not close the bluetooth link");
                }
            }
            EndpointKind::VirtualPort(_) | EndpointKind::PhysicalDevice(_) => {}
        }

        // Removing the runtime entry is what lets reconcile open it again from scratch.
        let mut inner = self.inner.write().await;
        if let Some(runtime) = inner.runtime.remove(&id)
            && let Some(handle) = runtime.handle
        {
            let closed = match &endpoint.kind {
                EndpointKind::PhysicalDevice(_) => self.midi.close_device(handle),
                EndpointKind::VirtualPort(_) => self.midi.destroy_virtual_port(handle),
                _ => Ok(()),
            };
            if let Err(error) = closed {
                warn!(endpoint = %endpoint.name, error = %error, "could not close endpoint");
            }
        }
    }
}

/// Decides how much a change to one endpoint disturbs it.
fn disturbance(old: &Endpoint, new: &Endpoint) -> Disturbance {
    let (old, new) = (stored(old), stored(new));
    if old == new {
        return Disturbance::None;
    }

    // Undo the changes that can be made in place, and see whether anything else is left.
    let mut rest = new.clone();
    rest.enabled = old.enabled;
    if !renames_on_the_platform(&old) {
        rest.name = old.name.clone();
    }
    if let (EndpointKind::NetworkSession(was), EndpointKind::NetworkSession(now)) =
        (&old.kind, &mut rest.kind)
    {
        now.invitation_policy = was.invitation_policy;
    }

    if rest != old {
        Disturbance::Restart
    } else if old.enabled != new.enabled {
        Disturbance::Toggled
    } else {
        Disturbance::InPlace
    }
}

/// Reports whether an endpoint's name is what other software sees it by.
///
/// A virtual port's name is the port's name on the platform, so renaming one means creating it
/// again, under the same platform identifier so connections to it survive. Everywhere else the
/// name is a label this program keeps: a session advertises its local name, and hardware and
/// radios have names of their own.
fn renames_on_the_platform(endpoint: &Endpoint) -> bool {
    matches!(endpoint.kind, EndpointKind::VirtualPort(_))
}

/// Returns the endpoint with everything observed rather than stored set to its default.
fn stored(endpoint: &Endpoint) -> Endpoint {
    let mut endpoint = endpoint.clone();
    match &mut endpoint.kind {
        EndpointKind::PhysicalDevice(device) => {
            device.present = false;
            device.confidence = midi_harbor_core::fingerprint::MatchConfidence::None;
            device.claimed_by = None;
        }
        EndpointKind::BluetoothDevice(device) => device.rssi = None,
        EndpointKind::VirtualPort(_) | EndpointKind::NetworkSession(_) => {}
    }
    endpoint
}

/// Copies what the daemon observes about each endpoint across to the incoming document.
fn carry_observations(current: &Configuration, incoming: &mut Configuration) {
    for endpoint in &mut incoming.endpoints {
        let Some(held) = current.endpoints.iter().find(|held| held.id == endpoint.id) else {
            continue;
        };
        match (&mut endpoint.kind, &held.kind) {
            (EndpointKind::PhysicalDevice(new), EndpointKind::PhysicalDevice(old)) => {
                new.present = old.present;
                new.confidence = old.confidence;
                new.claimed_by.clone_from(&old.claimed_by);
            }
            (EndpointKind::BluetoothDevice(new), EndpointKind::BluetoothDevice(old)) => {
                new.rssi = old.rssi;
            }
            _ => {}
        }
    }
}

/// Builds the setup a replacing import produces.
fn replaced(current: &Configuration, offered: Configuration) -> Configuration {
    let mut next = offered;
    // Two machines advertising under one name cannot be told apart on the network.
    next.preferences
        .machine_name
        .clone_from(&current.preferences.machine_name);
    for endpoint in &mut next.endpoints {
        adopt_local_identity(current, endpoint);
    }
    // Hardware attached here is found, not configured, so a file from another machine cannot
    // know about it. Dropping it would only have it rediscovered moments later under a new
    // identity, after being reported as removed.
    for held in &current.endpoints {
        let attached_here =
            matches!(&held.kind, EndpointKind::PhysicalDevice(device) if device.present);
        let named = next.endpoints.iter().any(|endpoint| {
            endpoint.id == held.id
                || (endpoint.kind.slug() == held.kind.slug() && endpoint.name == held.name)
        });
        if attached_here && !named {
            next.endpoints.push(held.clone());
        }
    }
    next
}

/// Builds the setup a merging import produces.
fn merged(current: &Configuration, offered: Configuration) -> Configuration {
    let mut next = current.clone();
    for mut endpoint in offered.endpoints {
        let here = next
            .endpoints
            .iter()
            .any(|held| held.kind.slug() == endpoint.kind.slug() && held.name == endpoint.name);
        if here {
            continue;
        }
        if next.endpoints.iter().any(|held| held.id == endpoint.id) {
            endpoint.id = EndpointId::new();
        }
        forget_foreign_identity(&mut endpoint);
        next.endpoints.push(endpoint);
    }
    for route in offered.routes {
        let here = next.routes.iter().any(|held| held.same_ends(&route));
        if !here {
            next.routes.push(route);
        }
    }
    for peer in offered.peers {
        let here = next
            .peers
            .iter()
            .any(|held| held.id == peer.id || held.name == peer.name);
        if !here {
            next.peers.push(peer);
        }
    }
    next
}

/// Gives an imported endpoint this machine's identity for the matching one, if there is one.
fn adopt_local_identity(current: &Configuration, endpoint: &mut Endpoint) {
    if current.endpoints.iter().any(|held| held.id == endpoint.id) {
        return;
    }
    let Some(local) = current
        .endpoints
        .iter()
        .find(|held| held.kind.slug() == endpoint.kind.slug() && held.name == endpoint.name)
    else {
        forget_foreign_identity(endpoint);
        return;
    };
    endpoint.id = local.id;
    if let (EndpointKind::VirtualPort(new), EndpointKind::VirtualPort(old)) =
        (&mut endpoint.kind, &local.kind)
    {
        new.input_ids.clone_from(&old.input_ids);
        new.output_ids.clone_from(&old.output_ids);
    }
}

/// Drops what only meant something on the machine an endpoint came from.
///
/// A virtual port's platform identifier was assigned by that machine's MIDI system. Pinning it
/// here could collide with a port that already holds it.
fn forget_foreign_identity(endpoint: &mut Endpoint) {
    if let EndpointKind::VirtualPort(port) = &mut endpoint.kind {
        port.platform_unique_id = None;
        port.input_ids.clear();
        port.output_ids.clear();
    }
}
