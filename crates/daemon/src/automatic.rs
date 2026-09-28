//! The automatic port a network port shows to other applications on this computer (FR-015h).
//!
//! It is the network port's own, the way macOS presents its network sessions: MIDI another
//! application sends it goes out over the network, and MIDI arriving over the network comes out
//! of it as well as going wherever the network port is routed. It is not an endpoint in its own
//! right, so no route names it and nothing lists it apart from the network port.

use crate::dataplane;
use crate::state::{Daemon, Runtime};
use midi_harbor_core::config;
use midi_harbor_core::endpoint::{Endpoint, EndpointKind};
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::state::{ConnectionState, Event};
use midi_harbor_platform::midi::{PortHandle, VirtualPortSpec};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Returns the identifier the automatic port of a network port carries in the data path.
///
/// Derived rather than stored, so it is the same on every run and needs no place in the file.
pub fn port_id(session: EndpointId) -> EndpointId {
    session.derived("automatic port")
}

/// A network port's automatic port while it is open.
pub struct AutomaticPort {
    /// The network port it belongs to.
    pub session: EndpointId,
    /// Its platform handle, counters and the notes sounding on it.
    pub runtime: Runtime,
}

impl Daemon {
    /// Opens a network port's automatic port, when it has one switched on and it is not open.
    pub(crate) async fn open_automatic_port(self: &Arc<Self>, session: EndpointId) {
        let id = port_id(session);

        // Decide whether it is wanted.
        let wanted = {
            let inner = self.inner.read().await;
            if inner.automatic_ports.contains_key(&id) {
                return;
            }
            inner
                .config
                .endpoint(session)
                .and_then(|endpoint| match &endpoint.kind {
                    EndpointKind::NetworkSession(held)
                        if held.automatic_port && endpoint.enabled =>
                    {
                        Some(VirtualPortSpec {
                            name: endpoint.name.as_str().to_owned(),
                            inputs: 1,
                            outputs: 1,
                            pinned_inputs: held.port_input_id.into_iter().collect(),
                            pinned_outputs: held.port_output_id.into_iter().collect(),
                        })
                    }
                    _ => None,
                })
        };
        let Some(spec) = wanted else {
            return;
        };

        // Create it on the platform, with nothing held.
        let (producer, consumer) = dataplane::connector_channel(id, 0);
        let (handle, ids) = match self.midi.create_virtual_port(&spec, vec![producer]) {
            Ok(created) => created,
            Err(error) => {
                warn!(port = %spec.name, error = %error, "could not open a network port's automatic port");
                return;
            }
        };

        // Keep it, unless another start opened one meanwhile.
        let mut inner = self.inner.write().await;
        if inner.automatic_ports.contains_key(&id) {
            drop(inner);
            let _ = self.midi.destroy_virtual_port(handle);
            return;
        }
        let now = self.clock.now();
        let mut state = ConnectionState::enabled(now);
        let _ = state.apply_now(Event::Attempting, self.clock.as_ref());
        let _ = state.apply_now(Event::Established, self.clock.as_ref());
        let _ = inner.automatic_ports.insert(
            id,
            AutomaticPort {
                session,
                runtime: Runtime::new(state, Some(handle)),
            },
        );
        // The identifiers are kept so other applications see the same port after a restart.
        if remember_ids(
            &mut inner.config.endpoints,
            session,
            &ids.inputs,
            &ids.outputs,
        ) && let Err(error) = config::save(&self.paths, &inner.config)
        {
            warn!(error = %error, "could not persist an automatic port's identifiers; it may change identity on restart");
        }
        drop(inner);
        self.start_dispatch(consumer);
        info!(port = %spec.name, "automatic port opened");
    }

    /// Closes a network port's automatic port, stopping what it has sounding first.
    pub(crate) async fn close_automatic_port(&self, session: EndpointId) {
        let id = port_id(session);
        // Silenced while it is still open, since afterwards nothing reaches the applications
        // listening to it.
        self.silence_endpoint(id).await;
        let Some(port) = self.inner.write().await.automatic_ports.remove(&id) else {
            return;
        };
        if let Some(handle) = port.runtime.handle
            && let Err(error) = self.midi.destroy_virtual_port(handle)
        {
            debug!(error = %error, "could not remove an automatic port");
        }
        info!(session = %session, "automatic port closed");
    }

    /// Opens or closes a network port's automatic port to match its configuration, and opens it
    /// again under a new name after a rename.
    pub(crate) async fn settle_automatic_port(
        self: &Arc<Self>,
        endpoint: &Endpoint,
        renamed: bool,
    ) {
        let EndpointKind::NetworkSession(session) = &endpoint.kind else {
            return;
        };
        let running = self.sessions.read().await.contains_key(&endpoint.id);
        if renamed || !session.automatic_port || !running {
            self.close_automatic_port(endpoint.id).await;
        }
        if session.automatic_port && running {
            self.open_automatic_port(endpoint.id).await;
        }
    }

    /// Returns the network port an automatic port belongs to, when `id` is one.
    pub(crate) async fn automatic_port_owner(&self, id: EndpointId) -> Option<EndpointId> {
        let inner = self.inner.read().await;
        inner.automatic_ports.get(&id).map(|port| port.session)
    }

    /// Returns a network port's open automatic port, with its platform handle.
    pub(crate) async fn automatic_port_of(
        &self,
        session: EndpointId,
    ) -> Option<(EndpointId, PortHandle)> {
        let id = port_id(session);
        let inner = self.inner.read().await;
        inner
            .automatic_ports
            .get(&id)
            .and_then(|port| port.runtime.handle)
            .map(|handle| (id, handle))
    }
}

/// Stores the platform identifiers an automatic port was given, reporting whether any changed.
fn remember_ids(
    endpoints: &mut [Endpoint],
    session: EndpointId,
    inputs: &[u32],
    outputs: &[u32],
) -> bool {
    let Some(Endpoint {
        kind: EndpointKind::NetworkSession(held),
        ..
    }) = endpoints.iter_mut().find(|endpoint| endpoint.id == session)
    else {
        return false;
    };
    let (input, output) = (inputs.first().copied(), outputs.first().copied());
    let mut changed = false;
    if input.is_some() && held.port_input_id != input {
        held.port_input_id = input;
        changed = true;
    }
    if output.is_some() && held.port_output_id != output {
        held.port_output_id = output;
        changed = true;
    }
    changed
}
