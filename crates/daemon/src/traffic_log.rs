//! A line in the log for each endpoint whose traffic moved.
//!
//! The history lives in memory and goes with the process, and the data path never logs, so
//! nothing on disk said whether a message arrived at a given minute. These lines do, read from
//! the counters by a normal task: the data path only increments atomics, as before.

use crate::state::Daemon;
use midi_harbor_core::counters::CounterSnapshot;
use midi_harbor_core::ids::EndpointId;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

/// How often the counters are compared with the last look.
const LOOK_INTERVAL: Duration = Duration::from_secs(10);

/// The least time between two lines for an endpoint whose traffic keeps moving.
///
/// A cue arriving on a quiet endpoint is logged at the next look; a clock or a controller
/// sweeping all the time is logged once a minute, not every ten seconds.
const BUSY_INTERVAL: Duration = Duration::from_secs(60);

/// One endpoint's traffic as last seen and last logged.
struct Watched {
    /// The counts at the last look.
    seen: (u64, u64, u64),
    /// Whether they had moved at the last look.
    moving: bool,
    /// When a line was last written for it.
    logged: Option<Instant>,
}

/// Returns the counts that decide whether traffic moved: received, sent and dropped.
fn counts(snapshot: &CounterSnapshot) -> (u64, u64, u64) {
    (
        snapshot.messages_received,
        snapshot.messages_sent,
        snapshot.messages_dropped,
    )
}

/// Formats a time the way the rest of the log does, or a dash for none.
fn when(at: Option<jiff::Timestamp>) -> String {
    at.map_or_else(|| "-".to_owned(), |at| at.to_string())
}

impl Daemon {
    /// Logs each endpoint's traffic when it moved, at most once a minute while it keeps moving.
    pub(crate) fn watch_traffic_log(self: &Arc<Self>) {
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(LOOK_INTERVAL);
            let mut watched: HashMap<EndpointId, Watched> = HashMap::new();
            loop {
                ticker.tick().await;
                let now = Instant::now();
                let current = daemon.traffic_by_name().await;

                // Forget what is no longer running, so a port opened again starts afresh.
                watched.retain(|id, _| current.iter().any(|(held, _, _)| held == id));

                for (id, name, snapshot) in current {
                    let seen = counts(&snapshot);
                    let Some(entry) = watched.get_mut(&id) else {
                        // The first look sets the baseline: traffic before it is not news.
                        watched.insert(
                            id,
                            Watched {
                                seen,
                                moving: false,
                                logged: None,
                            },
                        );
                        continue;
                    };

                    // Decide whether this look earns a line.
                    let moved = seen != entry.seen;
                    let was_quiet = !entry.moving;
                    let due = entry
                        .logged
                        .is_none_or(|at| now.duration_since(at) >= BUSY_INTERVAL);
                    entry.seen = seen;
                    entry.moving = moved;
                    if !moved || !(was_quiet || due) {
                        continue;
                    }

                    entry.logged = Some(now);
                    info!(
                        endpoint = %name,
                        received = snapshot.messages_received,
                        sent = snapshot.messages_sent,
                        dropped = snapshot.messages_dropped,
                        last_received = %when(snapshot.last_received),
                        last_sent = %when(snapshot.last_sent),
                        "traffic"
                    );
                }
            }
        });
    }

    /// Returns the counters of every running endpoint and automatic port, with the name each is
    /// logged under.
    async fn traffic_by_name(&self) -> Vec<(EndpointId, String, CounterSnapshot)> {
        let inner = self.inner.read().await;
        let name = |id: &EndpointId| {
            inner
                .config
                .endpoint(*id)
                .map(|endpoint| endpoint.name.as_str().to_owned())
        };
        let endpoints = inner
            .runtime
            .iter()
            .chain(inner.session_runtime.iter())
            .filter_map(|(id, runtime)| Some((*id, name(id)?, runtime.counters.snapshot())));
        let automatic = inner.automatic_ports.iter().filter_map(|(id, port)| {
            let owner = name(&port.session)?;
            Some((
                *id,
                format!("{owner} (automatic port)"),
                port.runtime.counters.snapshot(),
            ))
        });
        endpoints.chain(automatic).collect()
    }
}
