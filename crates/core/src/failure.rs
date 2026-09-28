//! The closed set of reasons a connection can fail.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Why a connection is not currently usable.
///
/// Deliberately a closed enum rather than a string: the interface renders specific guidance from
/// it, the IPC layer maps it onto stable error codes, and the command line derives exit codes from
/// it. Adding a variant is a contract change, not an implementation detail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    /// No route to the peer, or no network at all.
    NetworkUnreachable,
    /// The peer stopped answering within the liveness window.
    PeerTimeout,
    /// The peer explicitly refused the session.
    PeerRejected,
    /// The hardware backing this endpoint was removed.
    DeviceRemoved,
    /// Another application holds the device exclusively.
    DeviceClaimed {
        /// Names the holder when the platform reports it.
        by: Option<String>,
    },
    /// The operating system has not granted the permission this endpoint needs.
    PermissionDenied {
        /// Names the permission, so the interface can tell the user what to grant.
        what: String,
    },
    /// Bluetooth cannot be used on this machine right now.
    ///
    /// Said without advice, by the owner's decision (R-077): the cause may be a missing adapter,
    /// one switched off, or a system service that is not running, and the capability query is
    /// where each is named.
    AdapterUnavailable,
    /// Another endpoint already uses this name.
    NameConflict {
        /// The name that collided.
        name: String,
    },
    /// The operating system refused to create more endpoints.
    ResourceLimit,
    /// The peer sent something that violates the protocol.
    ProtocolError {
        /// Describes what was malformed.
        detail: String,
    },
    /// The stored configuration for this endpoint cannot be applied.
    ConfigInvalid {
        /// Describes what is wrong with it.
        detail: String,
    },
}

impl FailureReason {
    /// Returns one value of every variant, for checks that must cover them all.
    ///
    /// Exit codes, IPC codes and guidance are each decided per variant, and a variant missing
    /// from one of those checks would quietly take whatever default applies. The match below
    /// stops compiling when a variant is added, which is what keeps this list complete.
    pub fn one_of_each() -> [FailureReason; 11] {
        let reasons = [
            Self::NetworkUnreachable,
            Self::PeerTimeout,
            Self::PeerRejected,
            Self::DeviceRemoved,
            Self::DeviceClaimed { by: None },
            Self::PermissionDenied {
                what: "bluetooth".to_owned(),
            },
            Self::AdapterUnavailable,
            Self::NameConflict {
                name: "Bus".to_owned(),
            },
            Self::ResourceLimit,
            Self::ProtocolError {
                detail: "bad header".to_owned(),
            },
            Self::ConfigInvalid {
                detail: "port 0".to_owned(),
            },
        ];
        for reason in &reasons {
            // A new variant belongs in the list above as well as here.
            match reason {
                Self::NetworkUnreachable
                | Self::PeerTimeout
                | Self::PeerRejected
                | Self::DeviceRemoved
                | Self::DeviceClaimed { .. }
                | Self::PermissionDenied { .. }
                | Self::AdapterUnavailable
                | Self::NameConflict { .. }
                | Self::ResourceLimit
                | Self::ProtocolError { .. }
                | Self::ConfigInvalid { .. } => {}
            }
        }
        reasons
    }

    /// Reports whether clearing this failure requires the user to do something.
    ///
    /// This is the distinction FR-028 draws, and it decides which phase the connection reports:
    /// a failure the user cannot act on retries silently in `Retrying`, while one they can act on
    /// is surfaced as `Unavailable`, with guidance wherever there is one fix to name.
    ///
    /// Neither is terminal. Retrying continues in both cases, because the user may fix the
    /// condition at any moment — switching an adapter on or closing the application holding a
    /// device must reconnect without them touching Midi Harbor.
    pub fn needs_user_action(&self) -> bool {
        match self {
            Self::NetworkUnreachable
            | Self::PeerTimeout
            | Self::DeviceRemoved
            | Self::ProtocolError { .. } => false,
            Self::PeerRejected
            | Self::DeviceClaimed { .. }
            | Self::PermissionDenied { .. }
            | Self::AdapterUnavailable
            | Self::NameConflict { .. }
            | Self::ResourceLimit
            | Self::ConfigInvalid { .. } => true,
        }
    }

    /// Returns the stable slug used as an IPC error code and to derive command-line exit codes.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NetworkUnreachable => "network_unreachable",
            Self::PeerTimeout => "peer_timeout",
            Self::PeerRejected => "peer_rejected",
            Self::DeviceRemoved => "device_removed",
            Self::DeviceClaimed { .. } => "device_claimed",
            Self::PermissionDenied { .. } => "permission_denied",
            Self::AdapterUnavailable => "adapter_unavailable",
            Self::NameConflict { .. } => "name_conflict",
            Self::ResourceLimit => "resource_limit",
            Self::ProtocolError { .. } => "protocol_error",
            Self::ConfigInvalid { .. } => "config_invalid",
        }
    }

    /// Returns what the user can do about this failure, or `None` when there is nothing to do but
    /// wait for the automatic retry.
    pub fn guidance(&self) -> Option<String> {
        match self {
            Self::NetworkUnreachable | Self::PeerTimeout | Self::DeviceRemoved => None,
            Self::PeerRejected => Some(
                "the peer refused the session; accept it there, or check its invitation policy"
                    .to_owned(),
            ),
            Self::DeviceClaimed { by } => Some(match by {
                Some(app) => format!(
                    "close {app} or release the device in it, then it reconnects automatically"
                ),
                None => "another application holds this device exclusively".to_owned(),
            }),
            Self::PermissionDenied { what } => Some(format!(
                "grant {what} permission in system settings, then retry"
            )),
            Self::AdapterUnavailable => None,
            Self::NameConflict { name } => Some(format!("choose a name other than '{name}'")),
            Self::ResourceLimit => Some(
                "the operating system will not create more endpoints; remove one first".to_owned(),
            ),
            Self::ProtocolError { .. } => None,
            Self::ConfigInvalid { detail } => Some(format!("correct the configuration: {detail}")),
        }
    }
}

impl std::error::Error for FailureReason {}

impl fmt::Display for FailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NetworkUnreachable => write!(f, "network unreachable"),
            Self::PeerTimeout => write!(f, "peer not responding"),
            Self::PeerRejected => write!(f, "peer refused the session"),
            Self::DeviceRemoved => write!(f, "device was removed"),
            Self::DeviceClaimed { by: Some(app) } => write!(f, "device is in use by {app}"),
            Self::DeviceClaimed { by: None } => {
                write!(f, "device is in use by another application")
            }
            Self::PermissionDenied { what } => write!(f, "{what} permission was not granted"),
            Self::AdapterUnavailable => write!(f, "bluetooth is not available"),
            Self::NameConflict { name } => write!(f, "the name '{name}' is already in use"),
            Self::ResourceLimit => write!(f, "the system endpoint limit was reached"),
            Self::ProtocolError { detail } => write!(f, "protocol error: {detail}"),
            Self::ConfigInvalid { detail } => write!(f, "configuration is invalid: {detail}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failure the user must act on comes with guidance, and one they cannot act on asks
    /// nothing of them (FR-028).
    ///
    /// Bluetooth being unavailable is the one exception, by the owner's decision (R-077): it is
    /// surfaced as needing the user, but "switch the adapter on" was wrong for a machine with no
    /// adapter and for one where BlueZ was not running, and the refusal cannot tell which.
    #[test]
    fn guidance_is_offered_exactly_when_the_user_must_act() {
        for reason in FailureReason::one_of_each() {
            let want_guidance =
                reason.needs_user_action() && reason != FailureReason::AdapterUnavailable;
            assert_eq!(
                reason.guidance().is_some(),
                want_guidance,
                "{reason}: guidance must be offered exactly when there is one fix to name"
            );
        }
        assert!(
            FailureReason::AdapterUnavailable.needs_user_action(),
            "unavailable Bluetooth must still be surfaced rather than retried quietly"
        );
    }

    /// Every reason has its own code.
    ///
    /// The code is the IPC error code clients switch on and the key the command line derives its
    /// exit code from, so two reasons sharing one would make them indistinguishable to both.
    #[test]
    fn every_reason_has_its_own_code() {
        let codes = FailureReason::one_of_each().map(|reason| reason.code());
        let mut distinct = codes.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            codes.len(),
            "no two reasons may share a code: {codes:?}"
        );
    }
}
