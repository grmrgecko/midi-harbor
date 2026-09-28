//! Connecting to the daemon.

use midi_harbor_core::paths::Paths;
use midi_harbor_ipc::pb::{GetServerInfoRequest, ServerInfo};
use midi_harbor_ipc::transport::{self, TransportError};
use midi_harbor_ipc::{HarborClient, check_compatibility};
use std::path::PathBuf;
use tonic::transport::Channel;

/// What the user should be told when the daemon cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No daemon is listening.
    ///
    /// Reported with the two ways to fix it, because "connection refused" sends a user looking at
    /// permissions when the answer is that nothing is running.
    #[error(
        "the daemon is not running\n\
         run 'midi-harbor service install --start' to install it and start it at login,\n\
         or 'midi-harbor daemon' to run it in the foreground"
    )]
    NotRunning,
    /// The client and daemon cannot talk to each other.
    #[error("{0}")]
    Version(String),
    /// The socket could not be reached.
    #[error("{0}")]
    Transport(#[from] TransportError),
    /// The daemon refused the first call.
    #[error("the daemon did not respond: {0}")]
    Rpc(#[from] tonic::Status),
}

/// A connected client, already checked for version compatibility.
pub struct Client {
    inner: HarborClient<Channel>,
    server: ServerInfo,
}

impl Client {
    /// Connects to the daemon and verifies that both sides speak the same contract.
    ///
    /// The version check happens before anything else, so a mismatch is reported as a mismatch
    /// rather than surfacing later as a call that mysteriously does not exist.
    pub async fn connect(socket: Option<PathBuf>) -> Result<Self, ClientError> {
        let socket = match socket {
            Some(path) => path,
            None => Paths::resolve()
                .map_err(|_| ClientError::NotRunning)?
                .socket_file(),
        };

        if !transport::probe(&socket).await {
            return Err(ClientError::NotRunning);
        }
        let channel = transport::connect(&socket).await?;
        let mut inner = HarborClient::new(channel);

        let server = inner
            .get_server_info(GetServerInfoRequest {})
            .await?
            .into_inner();
        if let Err(mismatch) = check_compatibility(&server) {
            return Err(ClientError::Version(mismatch.guidance()));
        }
        Ok(Self { inner, server })
    }

    /// Returns the generated client for making calls.
    pub fn rpc(&mut self) -> &mut HarborClient<Channel> {
        &mut self.inner
    }

    /// Returns what the daemon reported about itself.
    pub fn server(&self) -> &ServerInfo {
        &self.server
    }
}
