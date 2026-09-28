//! Failures a platform backend can report.

use midi_harbor_core::failure::FailureReason;

/// Something a platform backend could not do.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// The operating system refused the operation.
    #[error("{operation} failed: {detail}")]
    Os {
        /// What was attempted.
        operation: &'static str,
        /// What the system reported.
        detail: String,
    },
    /// The named endpoint or device is not known to the backend.
    #[error("{0} is not available")]
    NotFound(String),
    /// Another application holds the device exclusively.
    #[error("device is held by another application")]
    Claimed {
        /// Names the holder when the platform reports it.
        by: Option<String>,
    },
    /// The operating system has not granted a required permission.
    #[error("{what} permission was not granted")]
    PermissionDenied {
        /// What the user needs to grant.
        what: String,
    },
    /// Bluetooth cannot be used right now: no adapter, one switched off, or no system service.
    #[error("bluetooth is not available")]
    AdapterUnavailable,
    /// The system will not create any more endpoints.
    #[error("the system endpoint limit was reached")]
    ResourceLimit,
    /// The name is already taken at the platform level.
    #[error("the name {0} is already in use")]
    NameConflict(String),
    /// The backend is not implemented for this platform.
    #[error("{0} is not supported on this platform")]
    Unsupported(&'static str),
}

impl PlatformError {
    /// Maps a backend failure onto the domain reason the state machine and interface use.
    ///
    /// Keeping this mapping in one place means a new platform error cannot reach the user as an
    /// opaque string; it has to be classified as something the interface can explain.
    pub fn as_failure_reason(&self) -> FailureReason {
        match self {
            Self::Os { detail, .. } => FailureReason::ProtocolError {
                detail: detail.clone(),
            },
            Self::NotFound(_) => FailureReason::DeviceRemoved,
            Self::Claimed { by } => FailureReason::DeviceClaimed { by: by.clone() },
            Self::PermissionDenied { what } => {
                FailureReason::PermissionDenied { what: what.clone() }
            }
            Self::AdapterUnavailable => FailureReason::AdapterUnavailable,
            Self::ResourceLimit => FailureReason::ResourceLimit,
            Self::NameConflict(name) => FailureReason::NameConflict { name: name.clone() },
            Self::Unsupported(what) => FailureReason::ConfigInvalid {
                detail: format!("{what} is not supported here"),
            },
        }
    }
}
