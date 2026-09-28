//! The bounded history of what happened to each connection.

use crate::ids::{EndpointId, EventId, RouteId};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// How many events the daemon keeps before discarding the oldest.
pub const DEFAULT_CAPACITY: usize = 10_000;

/// How much attention an event deserves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Normal lifecycle.
    Info,
    /// Something degraded but recovered, or will.
    Warning,
    /// Something failed and needs attention.
    Error,
}

/// What kind of thing happened.
///
/// Stable machine-readable names, so clients and log processors can match on them without parsing
/// prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// An endpoint moved between lifecycle phases.
    EndpointStateChanged,
    /// An endpoint appeared, through creation or discovery.
    EndpointAdded,
    /// An endpoint went away.
    EndpointRemoved,
    /// A route became valid, broken, or part of a loop.
    RouteValidityChanged,
    /// Notes were silenced on an endpoint.
    NotesSilenced,
    /// Controller and program state were resent after a recovery.
    StateRestored,
    /// A peer invited this machine to a session.
    InvitationReceived,
    /// The set of available capabilities changed.
    CapabilitiesChanged,
    /// The machine suspended and came back, or its network changed underneath us.
    SystemResumed,
    /// Configuration was loaded, reloaded or repaired.
    ConfigurationChanged,
    /// The daemon started.
    DaemonStarted,
    /// The daemon is shutting down.
    DaemonStopping,
    /// The daemon replaced one that lost the platform's MIDI service, which other applications
    /// may have lost with it.
    MidiServerReplaced,
}

impl EventKind {
    /// Returns the stable wire name for this kind.
    ///
    /// Written out rather than derived from the Rust identifier, because the name travels in the
    /// IPC contract: deriving it would let a variant rename change what clients match on without
    /// anything in the contract appearing to move.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::EndpointStateChanged => "endpoint_state_changed",
            EventKind::EndpointAdded => "endpoint_added",
            EventKind::EndpointRemoved => "endpoint_removed",
            EventKind::RouteValidityChanged => "route_validity_changed",
            EventKind::NotesSilenced => "notes_silenced",
            EventKind::StateRestored => "state_restored",
            EventKind::InvitationReceived => "invitation_received",
            EventKind::CapabilitiesChanged => "capabilities_changed",
            EventKind::SystemResumed => "system_resumed",
            EventKind::ConfigurationChanged => "configuration_changed",
            EventKind::DaemonStarted => "daemon_started",
            EventKind::DaemonStopping => "daemon_stopping",
            EventKind::MidiServerReplaced => "midi_server_replaced",
        }
    }
}

/// One timestamped record of something that happened.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Monotonic within a daemon run.
    pub id: EventId,
    /// When it happened.
    pub at: Timestamp,
    /// Which endpoint it concerns, when it concerns one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<EndpointId>,
    /// Which route it concerns, when it concerns one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteId>,
    /// How much attention it deserves.
    pub severity: Severity,
    /// What kind of thing happened.
    pub kind: EventKind,
    /// A human-readable description.
    pub detail: String,
}

/// A fixed-size history of recent events.
///
/// Lives in the daemon rather than in a client, which is what lets a user see a failure that
/// happened and recovered while no window was open. Bounded so an endpoint flapping for a week
/// cannot exhaust memory.
#[derive(Debug)]
pub struct EventLog {
    entries: VecDeque<Event>,
    capacity: usize,
    next_id: EventId,
}

impl EventLog {
    /// Creates a log holding at most `capacity` events. A zero capacity is raised to one, because
    /// a log that silently keeps nothing would be worse than a small one.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
            next_id: EventId::from_raw(1),
        }
    }

    /// Records an event and returns the identifier it was given.
    pub fn record(&mut self, mut event: Event) -> EventId {
        let id = self.next_id;
        event.id = id;
        self.next_id = self.next_id.next();

        if self.entries.len() >= self.capacity {
            let _ = self.entries.pop_front();
        }
        self.entries.push_back(event);
        id
    }

    /// Returns events newer than `after`, oldest first, capped at `limit`.
    pub fn since(&self, after: Option<EventId>, limit: usize) -> Vec<&Event> {
        self.entries
            .iter()
            .filter(|event| after.is_none_or(|cutoff| event.id > cutoff))
            .take(limit)
            .collect()
    }
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

/// Builds an event, leaving the identifier for the log to assign.
pub fn event(
    kind: EventKind,
    severity: Severity,
    at: Timestamp,
    detail: impl Into<String>,
) -> Event {
    Event {
        id: EventId::from_raw(0),
        at,
        endpoint: None,
        route: None,
        severity,
        kind,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log holds at most its capacity, discarding the oldest, and a zero capacity is raised
    /// to one.
    ///
    /// Bounded so an endpoint flapping for a week cannot exhaust the daemon's memory. Ten events
    /// into a log of three keep identifiers 8, 9 and 10; into a log asked for zero, which keeps
    /// one rather than silently nothing, they keep 10 alone.
    #[test]
    fn the_log_keeps_only_the_newest_events_up_to_its_capacity() {
        let cases = [
            ("a log of three keeps the last three", 3, vec![8, 9, 10]),
            ("a log asked for zero keeps the last one", 0, vec![10]),
        ];
        for (case, capacity, want) in cases {
            let mut log = EventLog::new(capacity);
            for _ in 0..10 {
                let _ = log.record(event(
                    EventKind::EndpointStateChanged,
                    Severity::Info,
                    Timestamp::UNIX_EPOCH,
                    "connected",
                ));
            }
            let kept: Vec<u64> = log.since(None, 100).iter().map(|e| e.id.get()).collect();
            assert_eq!(
                kept, want,
                "{case}: the oldest events must go first and the log must never grow past its bound"
            );
        }
    }

    /// Every event kind's wire name is the snake_case name serde writes for it.
    ///
    /// The name travels in the gRPC contract, written out by hand so a variant rename cannot move
    /// it; the serialised form travels in JSON. Two spellings of one event would let a client
    /// match one and miss the other.
    #[test]
    fn every_kind_has_one_snake_case_wire_name() {
        for kind in [
            EventKind::EndpointStateChanged,
            EventKind::EndpointAdded,
            EventKind::EndpointRemoved,
            EventKind::RouteValidityChanged,
            EventKind::NotesSilenced,
            EventKind::StateRestored,
            EventKind::InvitationReceived,
            EventKind::CapabilitiesChanged,
            EventKind::SystemResumed,
            EventKind::ConfigurationChanged,
            EventKind::DaemonStarted,
            EventKind::DaemonStopping,
            EventKind::MidiServerReplaced,
        ] {
            let name = kind.as_str();
            assert!(
                !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{kind:?}: the wire name {name:?} must be snake_case, not a Rust identifier's shape"
            );
            let serialised = serde_json::to_string(&kind).expect("an event kind serialises");
            assert_eq!(
                serialised.trim_matches('"'),
                name,
                "{kind:?}: the wire name must match the serialised name"
            );
        }
    }
}
