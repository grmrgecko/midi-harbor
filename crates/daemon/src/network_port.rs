//! Changing a network port after it is made: the name other machines see, its UDP port, who may
//! join, and its automatic port (FR-015, R-078).

use crate::state::{Change, Daemon, DaemonError};
use midi_harbor_core::config;
use midi_harbor_core::endpoint::{
    Endpoint, EndpointKind, EndpointName, InvitationPolicy, NetworkSession,
};
use midi_harbor_core::failure::FailureReason;
use midi_harbor_core::ids::EndpointId;
use std::sync::Arc;
use tracing::{info, warn};

/// What to change about a network port. A field left `None` is left as it is.
#[derive(Clone, Debug, Default)]
pub struct NetworkPortChange {
    /// The name other machines see it by, its Bonjour name.
    pub local_name: Option<String>,
    /// The UDP port it listens on; zero lets the system choose.
    pub control_port: Option<u16>,
    /// How it treats invitations.
    pub policy: Option<InvitationPolicy>,
    /// Whether other applications see it as a MIDI port of its name.
    pub automatic_port: Option<bool>,
}

/// Refuses a UDP port a network port cannot listen on. Zero lets the system choose.
///
/// The data port is the next one up, and binding moves an odd control port up one because some
/// implementations refuse an odd data port, so only an even port is taken as asked.
pub(crate) fn check_control_port(port: u16) -> Result<(), DaemonError> {
    if !port.is_multiple_of(2) {
        return Err(FailureReason::ConfigInvalid {
            detail: format!(
                "UDP port {port} is odd, and RTP-MIDI needs an even one with the data port above \
                 it; choose {} or {}",
                port.saturating_sub(1),
                port.saturating_add(1)
            ),
        }
        .into());
    }
    Ok(())
}

impl Daemon {
    /// Changes a network port's settings and applies them to it while it runs.
    ///
    /// A new name reaches the machines that connect from then on, and those already connected
    /// stay connected. A new UDP port means listening again, so the network port restarts: a
    /// machine it connected to is connected to again, and a machine that connected to it has to
    /// be told the new port.
    pub async fn update_network_port(
        self: &Arc<Self>,
        id: EndpointId,
        change: NetworkPortChange,
    ) -> Result<Endpoint, DaemonError> {
        // Validate the change.
        let local_name = change
            .local_name
            .as_deref()
            .map(EndpointName::new)
            .transpose()?;
        if let Some(port) = change.control_port {
            check_control_port(port)?;
            // A running port moved to a pair that is taken is refused before it is touched. One
            // another network port has is left to the check below, which names it.
            let running = self.sessions.read().await.get(&id).map(Arc::clone);
            let configured = self
                .read(|config, _| {
                    config.endpoints.iter().any(|other| {
                        other.id != id
                            && matches!(&other.kind, EndpointKind::NetworkSession(session)
                                if session.control_port == port)
                    })
                })
                .await;
            if let Some(session) = running
                && port != 0
                && port != session.control_port()
                && !configured
                && !crate::net::SessionSockets::pair_free(port).await
            {
                return Err(FailureReason::ConfigInvalid {
                    detail: format!("UDP port {port} or the one above it is in use"),
                }
                .into());
            }
        }

        // Record it.
        let (before, updated) = {
            let mut inner = self.inner.write().await;
            for other in inner.config.endpoints.iter().filter(|e| e.id != id) {
                let EndpointKind::NetworkSession(session) = &other.kind else {
                    continue;
                };
                // Two network ports advertising one name look like one machine to every peer.
                if local_name.as_ref() == Some(&session.local_name) {
                    return Err(FailureReason::NameConflict {
                        name: session.local_name.as_str().to_owned(),
                    }
                    .into());
                }
                if let Some(port) = change.control_port
                    && port != 0
                    && port == session.control_port
                {
                    return Err(FailureReason::ConfigInvalid {
                        detail: format!("UDP port {port} is already used by '{}'", other.name),
                    }
                    .into());
                }
            }
            let Some(endpoint) = inner.config.endpoints.iter_mut().find(|e| e.id == id) else {
                return Err(DaemonError::NotFound(id.to_string()));
            };
            let EndpointKind::NetworkSession(held) = &mut endpoint.kind else {
                return Err(FailureReason::ConfigInvalid {
                    detail: format!("{} is not a network port", endpoint.name),
                }
                .into());
            };
            let before = held.clone();
            if let Some(name) = local_name {
                held.local_name = name;
            }
            if let Some(port) = change.control_port {
                held.control_port = port;
            }
            if let Some(policy) = change.policy {
                held.invitation_policy = policy;
            }
            if let Some(on) = change.automatic_port {
                held.automatic_port = on;
            }
            let updated = endpoint.clone();
            config::save(&self.paths, &inner.config)?;
            (before, updated)
        };
        let EndpointKind::NetworkSession(after) = &updated.kind else {
            return Ok(updated);
        };

        // Apply it to the running session.
        let running = self.sessions.read().await.get(&id).map(Arc::clone);
        if let Some(session) = running {
            if after.control_port != before.control_port {
                info!(endpoint = %updated.name, port = after.control_port, "network port moving to a new UDP port");
                self.stop_session(id, before.local_name.as_str()).await;
                // Binding moves to a nearby pair when the one asked for is taken, which keeps a
                // network port up after a restart but here would put it somewhere the user did
                // not choose.
                let started = self.start_session(&updated).await;
                let listening = self
                    .session_status(id)
                    .await
                    .map(|status| status.control_port);
                let refused = match started {
                    Err(error) => Some(error),
                    Ok(()) if after.control_port != 0 && listening != Some(after.control_port) => {
                        Some(
                            FailureReason::ConfigInvalid {
                                detail: format!(
                                    "UDP port {} or the one above it is in use",
                                    after.control_port
                                ),
                            }
                            .into(),
                        )
                    }
                    Ok(()) => None,
                };
                if let Some(error) = refused {
                    // Put it back as it was, so a port that could not be bound leaves the network
                    // port neither down, nor somewhere else, nor half changed.
                    warn!(endpoint = %updated.name, error = %error, "could not listen on the new UDP port; keeping the old settings");
                    self.stop_session(id, after.local_name.as_str()).await;
                    self.restore(id, before).await;
                    return Err(error);
                }
            } else if after.local_name != before.local_name {
                let _ = session.rename(after.local_name.as_str().to_owned()).await;
                if let Some(discovery) = &self.discovery {
                    discovery.withdraw(before.local_name.as_str());
                    let announce = self
                        .read(|config, _| config.preferences.advertise_sessions)
                        .await;
                    if announce
                        && let Err(error) = discovery.advertise(
                            after.local_name.as_str(),
                            session.control_port(),
                            id,
                        )
                    {
                        warn!(endpoint = %updated.name, error = %error, "could not advertise the new name");
                    }
                }
            }
        }
        if change.policy.is_some() {
            self.push_invitation_policy().await;
        }
        if change.automatic_port.is_some() {
            self.settle_automatic_port(&updated, false).await;
        }
        let _ = self.changes.send(Change::EndpointChanged(id));
        Ok(updated)
    }

    /// Puts a network port's settings back as they were, and starts it with them.
    async fn restore(self: &Arc<Self>, id: EndpointId, settings: NetworkSession) {
        let restored = {
            let mut inner = self.inner.write().await;
            let Some(endpoint) = inner.config.endpoints.iter_mut().find(|e| e.id == id) else {
                return;
            };
            if let EndpointKind::NetworkSession(held) = &mut endpoint.kind {
                *held = settings;
            }
            let restored = endpoint.clone();
            if let Err(error) = config::save(&self.paths, &inner.config) {
                warn!(error = %error, "could not persist a network port's restored settings");
            }
            restored
        };
        if let Err(error) = self.start_session(&restored).await {
            warn!(endpoint = %restored.name, error = %error, "could not listen on the old UDP port either");
        }
    }

    /// Deletes a network port, reporting the routes it leaves broken.
    ///
    /// Everything it was carrying is stopped first: the notes it played here, and those it sent
    /// its machines, which have no way to stop them once it has gone. Its routes stay, broken,
    /// and mend if a network port of its name returns.
    pub async fn delete_network_port(
        self: &Arc<Self>,
        id: EndpointId,
    ) -> Result<Vec<String>, DaemonError> {
        let local_name = match self.read(|config, _| config.endpoint(id).cloned()).await {
            Some(Endpoint {
                kind: EndpointKind::NetworkSession(session),
                ..
            }) => session.local_name,
            Some(endpoint) => {
                return Err(FailureReason::ConfigInvalid {
                    detail: format!("{} is not a network port", endpoint.name),
                }
                .into());
            }
            None => return Err(DaemonError::NotFound(id.to_string())),
        };

        // Stop it while it can still be heard.
        self.silence_routes_from(id).await;
        self.stop_session(id, local_name.as_str()).await;
        // An invitation waiting on it could only be answered for a network port that is gone.
        self.invitations
            .write()
            .await
            .retain(|_, invitation| invitation.session != id);

        // Forget it.
        let (name, orphaned) = {
            let mut inner = self.inner.write().await;
            let Some(index) = inner.config.endpoints.iter().position(|e| e.id == id) else {
                return Err(DaemonError::NotFound(id.to_string()));
            };
            let endpoint = inner.config.endpoints.remove(index);
            let orphaned: Vec<String> = inner
                .config
                .routes
                .iter()
                .filter(|route| route.touches(&endpoint))
                .map(|route| format!("{} -> {}", route.from, route.to))
                .collect();
            config::save(&self.paths, &inner.config)?;
            (endpoint.name.to_string(), orphaned)
        };
        let _ = self.changes.send(Change::EndpointRemoved(id));
        info!(endpoint = %name, "network port deleted");
        Ok(orphaned)
    }

    /// Switches a network port's automatic port on or off, and keeps the choice.
    pub async fn set_automatic_port(
        self: &Arc<Self>,
        id: EndpointId,
        on: bool,
    ) -> Result<Endpoint, DaemonError> {
        self.update_network_port(
            id,
            NetworkPortChange {
                automatic_port: Some(on),
                ..NetworkPortChange::default()
            },
        )
        .await
    }

    /// Changes how a network port treats invitations.
    pub async fn set_invitation_policy(
        self: &Arc<Self>,
        id: EndpointId,
        policy: InvitationPolicy,
    ) -> Result<Endpoint, DaemonError> {
        self.update_network_port(
            id,
            NetworkPortChange {
                policy: Some(policy),
                ..NetworkPortChange::default()
            },
        )
        .await
    }
}
