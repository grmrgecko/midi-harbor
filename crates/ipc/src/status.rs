//! Mapping domain failures onto gRPC statuses.
//!
//! One mapping serves the whole product: `FailureReason` becomes a stable slug, the slug becomes
//! trailing metadata on the status, and the command line derives its exit code from that slug.
//! Clients switch on the slug and never on the message text.

use midi_harbor_core::failure::FailureReason;
use tonic::{Code, Status};

/// Trailing metadata key carrying the stable machine-readable failure slug.
pub const REASON_METADATA_KEY: &str = "harbor-reason";

/// Trailing metadata key carrying what the user can do about a failure, as UTF-8 text.
///
/// Binary rather than ASCII metadata, because guidance names things the user chose, and those
/// need not be ASCII. Present only when there is something to do.
pub const GUIDANCE_METADATA_KEY: &str = "harbor-guidance-bin";

/// Converts a domain failure into a gRPC status that carries its slug.
pub trait IntoStatus {
    /// Returns the status a client should receive for this failure.
    fn into_status(self) -> Status;
}

impl IntoStatus for FailureReason {
    fn into_status(self) -> Status {
        let code = match &self {
            // Something the user asked for is not there.
            FailureReason::DeviceRemoved => Code::NotFound,
            // The request conflicts with what already exists.
            FailureReason::NameConflict { .. } => Code::AlreadyExists,
            // The system cannot serve this right now, but might later.
            FailureReason::NetworkUnreachable
            | FailureReason::PeerTimeout
            | FailureReason::AdapterUnavailable
            | FailureReason::DeviceClaimed { .. } => Code::Unavailable,
            // The user lacks something only they can grant.
            FailureReason::PermissionDenied { .. } => Code::PermissionDenied,
            // The request cannot be served in the current state.
            FailureReason::PeerRejected | FailureReason::ResourceLimit => Code::FailedPrecondition,
            // The input itself is wrong.
            FailureReason::ProtocolError { .. } | FailureReason::ConfigInvalid { .. } => {
                Code::InvalidArgument
            }
        };

        let slug = self.code();
        let guidance = self.guidance();
        let mut status = Status::new(code, self.to_string());
        // FR-028: a failure the user can act on says how. Without this the CLI had only the
        // message, which says what went wrong and never what to do.
        if let Some(guidance) = guidance {
            status.metadata_mut().insert_bin(
                GUIDANCE_METADATA_KEY,
                tonic::metadata::MetadataValue::from_bytes(guidance.as_bytes()),
            );
        }
        // A slug that cannot be encoded would be a programming error in the slug itself, since
        // they are fixed ASCII. Skipping the metadata is better than failing the whole response.
        if let Ok(value) = slug.parse() {
            status.metadata_mut().insert(REASON_METADATA_KEY, value);
        }
        status
    }
}

/// Returns what the user can do about the failure a status reports, when there is something.
pub fn reason_guidance(status: &Status) -> Option<String> {
    let value = status.metadata().get_bin(GUIDANCE_METADATA_KEY)?;
    let bytes = value.to_bytes().ok()?;
    String::from_utf8(bytes.to_vec()).ok()
}

/// Returns the stable failure slug carried by a status, when it has one.
///
/// Absent for statuses the daemon did not originate, such as a transport failure.
pub fn reason_slug(status: &Status) -> Option<&str> {
    status.metadata().get(REASON_METADATA_KEY)?.to_str().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the contract a client switches on for every failure reason: the gRPC status code,
    /// the `harbor-reason` slug in the trailing metadata, and the guidance in binary metadata
    /// exactly when the reason has some.
    ///
    /// The CLI's exit codes derive from the code and slug, so changing either is a contract
    /// change. The name "Flügel" is not ASCII, which gRPC's ASCII metadata cannot carry; binary
    /// `-bin` metadata carries it intact, per the gRPC over HTTP/2 specification.
    #[test]
    fn every_reason_reaches_the_client_as_its_code_slug_and_guidance() {
        let cases = [
            (
                FailureReason::NetworkUnreachable,
                Code::Unavailable,
                "network_unreachable",
            ),
            (
                FailureReason::PeerTimeout,
                Code::Unavailable,
                "peer_timeout",
            ),
            (
                FailureReason::PeerRejected,
                Code::FailedPrecondition,
                "peer_rejected",
            ),
            (
                FailureReason::DeviceRemoved,
                Code::NotFound,
                "device_removed",
            ),
            (
                FailureReason::DeviceClaimed {
                    by: Some("Logic".to_owned()),
                },
                Code::Unavailable,
                "device_claimed",
            ),
            (
                FailureReason::PermissionDenied {
                    what: "bluetooth".to_owned(),
                },
                Code::PermissionDenied,
                "permission_denied",
            ),
            (
                FailureReason::AdapterUnavailable,
                Code::Unavailable,
                "adapter_unavailable",
            ),
            (
                FailureReason::NameConflict {
                    name: "Flügel".to_owned(),
                },
                Code::AlreadyExists,
                "name_conflict",
            ),
            (
                FailureReason::ResourceLimit,
                Code::FailedPrecondition,
                "resource_limit",
            ),
            (
                FailureReason::ProtocolError {
                    detail: "bad header".to_owned(),
                },
                Code::InvalidArgument,
                "protocol_error",
            ),
            (
                FailureReason::ConfigInvalid {
                    detail: "port 0".to_owned(),
                },
                Code::InvalidArgument,
                "config_invalid",
            ),
        ];
        assert_eq!(
            cases.len(),
            FailureReason::one_of_each().len(),
            "every failure reason must have a row in the contract"
        );
        for (reason, want_code, want_slug) in cases {
            let guidance = reason.guidance();
            let status = reason.into_status();
            assert_eq!(
                status.code(),
                want_code,
                "{want_slug}: clients switch on the status code"
            );
            assert_eq!(
                reason_slug(&status),
                Some(want_slug),
                "{want_slug}: clients switch on the slug and never on the message"
            );
            assert_eq!(
                reason_guidance(&status),
                guidance,
                "{want_slug}: the guidance must reach the client intact, and only when there is some"
            );
        }
    }
}
