//! Carrying the contract between processes on one machine.
//!
//! Never TCP. gRPC's usual transport would put the whole control plane on the network. macOS and
//! Linux use a Unix domain socket; Windows uses a named pipe that refuses remote clients, found
//! through a file at the same path a socket would have, so the rest of the program deals in one
//! kind of location everywhere.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{answers, bind, connect, probe};
#[cfg(windows)]
pub use windows::{Connection, answers, bind, connect, pipe_name, probe};

use std::io;
use std::path::PathBuf;

/// Why the socket could not be prepared or reached.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The socket or its directory could not be created.
    #[error("could not {operation} {path}: {source}")]
    Io {
        /// What was attempted.
        operation: &'static str,
        /// Which path it concerned.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// No daemon is listening.
    #[error("no daemon is listening at {0}")]
    NotRunning(PathBuf),
    /// Another daemon is already listening, and answers.
    #[error("a daemon is already listening at {0}")]
    InUse(PathBuf),
    /// The socket path exceeds what the operating system allows.
    #[error(
        "socket path is {length} bytes, which exceeds the {limit} the operating system allows: {path}"
    )]
    PathTooLong {
        /// The offending path.
        path: PathBuf,
        /// How long it is.
        length: usize,
        /// The platform limit.
        limit: usize,
    },
    /// The channel could not be established for some other reason.
    #[error("could not connect to the daemon: {0}")]
    Connect(#[from] tonic::transport::Error),
}
