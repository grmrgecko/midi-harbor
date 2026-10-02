//! The engine that owns every endpoint and connection.
//!
//! Everything MIDI-related lives here and nowhere else. Clients reach it over the gRPC contract,
//! and no client's lifetime is tied to any resource it owns: opening, closing or crashing a
//! window must not disturb a connection.

pub mod automatic;
pub mod bluetooth;
pub mod dataplane;
pub mod devices;
pub mod discovery;
pub mod identity;
pub mod log_file;
pub mod net;
pub mod network_port;
pub mod reconcile;
pub mod server;
pub mod service;
pub mod session;
pub mod state;
pub mod supervisor;
mod traffic_log;

pub use network_port::NetworkPortChange;
pub use server::{Stopped, already_serving, run};
pub use state::{Daemon, DaemonError, RouteRequest};
