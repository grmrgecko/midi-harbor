//! The contract between the Midi Harbor daemon and its clients.
//!
//! Types here are generated from `proto/midiharbor/v1/harbor.proto` by the build script. The
//! proto file is the contract; nothing in the generated module is hand-written.

pub mod status;
pub mod transport;
pub mod version;

/// Types and service stubs generated from the daemon contract.
#[allow(
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used
)]
pub mod pb {
    tonic::include_proto!("midiharbor.v1");
}

pub use pb::harbor_client::HarborClient;
pub use pb::harbor_server::{Harbor, HarborServer};
pub use status::{GUIDANCE_METADATA_KEY, IntoStatus, REASON_METADATA_KEY};
pub use version::{PROTOCOL_MAJOR, PROTOCOL_MINOR, VersionMismatch, check_compatibility};
