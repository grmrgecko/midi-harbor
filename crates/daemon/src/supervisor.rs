//! Retrying virtual ports and hardware that failed to open.
//!
//! Network sessions have their own supervisors, and those have always retried. A virtual port or
//! a device that failed to open was given a retry time by the state machine and then left alone,
//! so `status` announced a retry that never came: a port refused at startup by an endpoint limit
//! stayed shut after the limit cleared, and a device held by another application stayed shut
//! after that application quit. This delivers those retries.

use crate::dataplane;
use crate::state::{Change, Daemon, OpenedPort, record_platform_ids};
use midi_harbor_core::config;
use midi_harbor_core::endpoint::{Endpoint, EndpointKind};
use midi_harbor_core::events::{EventKind, Severity};
use midi_harbor_core::failure::FailureReason;
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::state::{ConnectionPhase, ConnectionState, Event};
use midi_harbor_platform::midi::ConnectorIds;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// How often waiting endpoints are checked for a retry that has come due.
///
/// The shortest backoff is 250 ms, so checking more often than that would find nothing new.
pub const RETRY_INTERVAL: Duration = Duration::from_millis(250);

impl Daemon {
    /// Retries each waiting virtual port and device when its backoff expires.
    pub(crate) fn watch_retries(self: &Arc<Self>) {
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(RETRY_INTERVAL);
            loop {
                ticker.tick().await;
                daemon.retry_due().await;
            }
        });
    }

    /// Makes one attempt at every endpoint whose retry has come due.
    async fn retry_due(self: &Arc<Self>) {
        let now = self.clock.now();
        let due: Vec<EndpointId> = {
            let inner = self.inner.read().await;
            inner
                .config
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.enabled && retried_here(endpoint))
                .filter(|endpoint| {
                    inner
                        .runtime
                        .get(&endpoint.id)
                        .is_some_and(|runtime| runtime.handle.is_none() && due(&runtime.state, now))
                })
                .map(|endpoint| endpoint.id)
                .collect()
        };
        // Each attempt on its own task, so one endpoint slow to answer does not hold up the
        // others waiting behind it, and a fault in one attempt ends that attempt alone. Claiming
        // an attempt moves the endpoint out of the waiting phases, so the next pass cannot start
        // a second one beside it.
        for id in due {
            let daemon = Arc::clone(self);
            tokio::spawn(async move { daemon.retry(id).await });
        }
    }

    /// Attempts to open one endpoint again, carrying its backoff forward whatever happens.
    ///
    /// The platform is asked without the daemon's lock held. Every route needs that lock to find
    /// its destinations, so holding it through an open stopped all MIDI for as long as the
    /// platform took to answer about one endpoint that was not working anyway (FR-029).
    async fn retry(self: &Arc<Self>, id: EndpointId) {
        // Claim the attempt. Checked again under the lock, because the endpoint may have been
        // disabled, deleted or opened some other way since the list was taken. Moving it to
        // connecting is what stops the next pass retrying it again while this one is out.
        let endpoint = {
            let mut inner = self.inner.write().await;
            let now = self.clock.now();
            let Some(endpoint) = inner.config.endpoint(id).cloned() else {
                return;
            };
            let Some(runtime) = inner.runtime.get_mut(&id) else {
                return;
            };
            if !endpoint.enabled
                || !retried_here(&endpoint)
                || runtime.handle.is_some()
                || !due(&runtime.state, now)
            {
                return;
            }
            let _ = runtime
                .state
                .apply_now(Event::RetryDue, self.clock.as_ref());
            endpoint
        };

        // Open it, with nothing held, on a thread that may block. A backend waits for its
        // platform thread to answer, and waiting on a runtime worker held up whatever task was
        // queued behind it there, however many other workers were idle.
        let daemon = Arc::clone(self);
        let wanted = endpoint.clone();
        let opened = tokio::task::spawn_blocking(move || match &wanted.kind {
            EndpointKind::VirtualPort(_) => daemon.open_virtual_port(&wanted),
            EndpointKind::PhysicalDevice(device) => {
                let (producer, consumer) = dataplane::channel(id);
                daemon
                    .midi
                    .open_device_with_sink(&device.fingerprint, Some(producer))
                    .map(|handle| OpenedPort {
                        handle,
                        ids: ConnectorIds::default(),
                        consumers: vec![consumer],
                    })
                    .map_err(|error| error.as_failure_reason())
            }
            _ => Err(FailureReason::ConfigInvalid {
                detail: "not an endpoint this loop opens".to_owned(),
            }),
        })
        .await;
        let opened: Result<OpenedPort, FailureReason> = match opened {
            Ok(opened) => opened,
            // Only a panic in the backend or the runtime shutting down gets here. Recorded as a
            // failure so the endpoint is retried rather than left connecting for good.
            Err(error) => {
                error!(endpoint = %endpoint.name, error = %error, "retry did not finish");
                Err(FailureReason::ProtocolError {
                    detail: "the platform did not answer".to_owned(),
                })
            }
        };

        // Record the outcome, unless the endpoint changed while the platform was answering.
        // Switching it off or deleting it removes its runtime, and switching it back on starts
        // a new one, so this attempt's is found only if nothing else touched it.
        let mut inner = self.inner.write().await;
        let now = self.clock.now();
        let Some(runtime) = inner.runtime.get_mut(&id).filter(|runtime| {
            runtime.handle.is_none() && runtime.state.phase() == ConnectionPhase::Connecting
        }) else {
            // Switched off, deleted or opened elsewhere meanwhile: what was opened is not wanted.
            drop(inner);
            if let Ok(OpenedPort { handle, .. }) = opened {
                let _ = match endpoint.kind {
                    EndpointKind::PhysicalDevice(_) => self.midi.close_device(handle),
                    _ => self.midi.destroy_virtual_port(handle),
                };
            }
            return;
        };

        match opened {
            Ok(OpenedPort {
                handle,
                ids,
                consumers,
            }) => {
                let attempts = runtime.state.attempt();
                let _ = runtime
                    .state
                    .apply_now(Event::Attempting, self.clock.as_ref());
                let _ = runtime
                    .state
                    .apply_now(Event::Established, self.clock.as_ref());
                runtime.handle = Some(handle);

                // The identifier matters as much here as on a first open: without it, other
                // applications see a new port after the next restart.
                if record_platform_ids(&mut inner.config, id, &ids)
                    && let Err(error) = config::save(&self.paths, &inner.config)
                {
                    warn!(error = %error, "could not persist a platform identifier; the port may change identity on restart");
                }
                info!(endpoint = %endpoint.name, attempts, "opened after retrying");
                let _ = inner.events.record(midi_harbor_core::events::event(
                    EventKind::EndpointStateChanged,
                    Severity::Info,
                    now,
                    format!(
                        "{} connected after {}",
                        endpoint.name,
                        crate::state::failed_attempts(attempts)
                    ),
                ));
                drop(inner);
                for consumer in consumers {
                    self.start_dispatch(consumer);
                }
            }
            Err(reason) => {
                // Quiet on purpose: the first failure is in the history already, and one entry per
                // backoff step for as long as a device stays claimed says nothing new.
                debug!(endpoint = %endpoint.name, error = %reason, "retry failed");
                let _ = runtime
                    .state
                    .apply_now(Event::Failed(reason), self.clock.as_ref());
                drop(inner);
            }
        }
        let _ = self.changes.send(Change::EndpointChanged(id));
    }
}

/// Reports whether this loop is the one responsible for retrying the endpoint.
///
/// Network sessions and Bluetooth links retry through their own supervisors, and hardware that is
/// not attached is waiting for the device to return, not for a backoff to expire.
fn retried_here(endpoint: &Endpoint) -> bool {
    match &endpoint.kind {
        EndpointKind::VirtualPort(_) => true,
        EndpointKind::PhysicalDevice(device) => device.present,
        _ => false,
    }
}

/// Reports whether an endpoint is waiting and its retry time has passed.
fn due(state: &ConnectionState, now: jiff::Timestamp) -> bool {
    matches!(
        state.phase(),
        ConnectionPhase::Retrying | ConnectionPhase::Unavailable
    ) && state.next_retry().is_some_and(|at| at <= now)
}
