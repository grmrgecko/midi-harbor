//! Domain types, connection state machine, routing and configuration.
//!
//! This crate carries no I/O and no platform code, so every rule it encodes can be tested on a
//! machine with no MIDI hardware, no network peer and no Bluetooth radio.

pub mod backoff;
pub mod capability;
pub mod config;
pub mod controls;
pub mod counters;
pub mod devicetime;
pub mod endpoint;
pub mod events;
pub mod failure;
pub mod fingerprint;
pub mod ids;
pub mod loops;
pub mod midi;
pub mod paths;
pub mod router;
pub mod rtchannel;
pub mod rtevent;
pub mod sounding;
pub mod state;
pub mod stream;
pub mod time;

/// Midi Harbor's version, from the `VERSION` file at the root of the repository, which every
/// build step reads. The crates' own versions stay 0.0.0, since they are never published.
pub const VERSION: &str = include_str!("../../../VERSION").trim_ascii();

pub use backoff::{Backoff, BackoffPolicy};
pub use capability::{Capability, CapabilityName, CapabilitySet, UnavailableReason};
pub use config::Configuration;
pub use counters::{CounterSnapshot, TrafficCounters};
pub use endpoint::{Direction, Endpoint, EndpointKind, EndpointName};
pub use events::{EventKind, EventLog, Severity};
pub use failure::FailureReason;
pub use fingerprint::{DeviceFingerprint, MatchConfidence};
pub use ids::{EndpointId, EventId, PeerId, RouteId};
pub use midi::{Channel, MidiMessage};
pub use paths::Paths;
pub use router::{ResolvedRoute, RouteValidity, Router};
pub use rtchannel::{Drained, RtConsumer, RtProducer};
pub use rtevent::{RtEvent, RtPayload};
pub use sounding::Sounding;
pub use state::{ConnectionPhase, ConnectionState, Effect};
pub use stream::{Chunk, Scanner, SysExEnd};
pub use time::{Clock, SystemClock, TestClock};
