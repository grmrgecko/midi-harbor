//! Exit codes, derived from the daemon's stable failure slugs.

use midi_harbor_core::failure::FailureReason;
use midi_harbor_ipc::status::reason_slug;
use tonic::{Code, Status};

/// The process exit codes this program uses.
///
/// Stable and scriptable: a caller can branch on these without parsing output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    /// The command succeeded.
    Success = 0,
    /// Something failed that no other code describes.
    Failure = 1,
    /// The command line was wrong, or the graphical interface is not in this build.
    Usage = 2,
    /// The daemon is not running or cannot be reached.
    DaemonUnreachable = 3,
    /// The capability is not available on this machine.
    Unavailable = 4,
    /// No such endpoint, route, peer or device.
    NotFound = 5,
    /// The name is already in use, or the route already exists.
    Conflict = 6,
    /// The action needs confirmation that was not given.
    ConfirmationRequired = 7,
    /// The client and daemon speak different major protocol versions.
    VersionMismatch = 8,
}

impl ExitCode {
    /// Returns the numeric code to exit with.
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// Maps a daemon status onto the exit code a script should see.
///
/// The stable slug is preferred over the gRPC code, because several conditions share a code but
/// need different exits: a name conflict and a missing endpoint are both client errors, and a
/// caller acts differently on each.
pub fn from_status(status: &Status) -> ExitCode {
    // A slug this client does not know comes from a newer daemon, and the gRPC code is still a
    // better guide than the generic failure.
    let known = reason_slug(status).and_then(|slug| {
        FailureReason::one_of_each()
            .into_iter()
            .find(|reason| reason.code() == slug)
    });
    if let Some(reason) = known {
        return for_reason(&reason);
    }
    match status.code() {
        Code::NotFound => ExitCode::NotFound,
        Code::AlreadyExists => ExitCode::Conflict,
        Code::FailedPrecondition => ExitCode::ConfirmationRequired,
        Code::Unavailable => ExitCode::DaemonUnreachable,
        Code::Unimplemented => ExitCode::VersionMismatch,
        Code::InvalidArgument => ExitCode::Usage,
        Code::PermissionDenied => ExitCode::Unavailable,
        _ => ExitCode::Failure,
    }
}

/// The exit code for each failure reason.
///
/// No wildcard, so a new reason cannot fall through to the generic failure unnoticed. The two
/// that do exit `1` do so by decision: a peer declining a session and a peer or the operating
/// system breaking protocol are nothing a script on this machine can correct.
fn for_reason(reason: &FailureReason) -> ExitCode {
    match reason {
        FailureReason::NameConflict { .. } => ExitCode::Conflict,
        FailureReason::DeviceRemoved => ExitCode::NotFound,
        FailureReason::AdapterUnavailable
        | FailureReason::PermissionDenied { .. }
        | FailureReason::NetworkUnreachable
        | FailureReason::PeerTimeout
        | FailureReason::DeviceClaimed { .. }
        | FailureReason::ResourceLimit => ExitCode::Unavailable,
        // Raised when a request does not fit the endpoint it names, such as forgetting a port.
        FailureReason::ConfigInvalid { .. } => ExitCode::Usage,
        FailureReason::PeerRejected | FailureReason::ProtocolError { .. } => ExitCode::Failure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi_harbor_ipc::status::{IntoStatus, REASON_METADATA_KEY};

    /// Locks the exit code each failure reason produces, as the number a script switches on.
    ///
    /// Scripts branch on these numbers, so changing one is a contract change. The stable slug
    /// decides, not the gRPC code: a name conflict and a missing endpoint are both client errors,
    /// yet a script retries one and gives up on the other. Only a peer declining and a protocol
    /// error exit 1, by decision (T121), since nothing on this machine can correct them. Every
    /// reason must have a row, so a new one cannot reach exit 1 by default.
    #[test]
    fn every_failure_reason_exits_with_its_contracted_code() {
        let contract = [
            ("peer_rejected", 1),
            ("protocol_error", 1),
            ("config_invalid", 2),
            ("adapter_unavailable", 4),
            ("permission_denied", 4),
            ("network_unreachable", 4),
            ("peer_timeout", 4),
            ("device_claimed", 4),
            ("resource_limit", 4),
            ("device_removed", 5),
            ("name_conflict", 6),
        ];
        for reason in FailureReason::one_of_each() {
            let want = contract
                .iter()
                .find(|(slug, _)| *slug == reason.code())
                .map(|(_, code)| *code);
            let got = from_status(&reason.clone().into_status()).code();
            assert_eq!(
                Some(got),
                want,
                "{} must exit {want:?}, and a new reason needs a row here",
                reason.code()
            );
        }
    }

    /// Locks the exit code for a status that carries no slug this client knows.
    ///
    /// A transport failure never reaches the daemon, so it has no slug and must still say the
    /// daemon is unreachable (3). A refusal for want of `--yes` exits 7 so a script can retry with
    /// it. A slug from a newer daemon is unknown here, and its gRPC code is a better guide than
    /// the generic failure.
    #[test]
    fn a_status_without_a_known_slug_exits_by_its_grpc_code() {
        let mut newer = Status::new(Code::NotFound, "gone");
        newer.metadata_mut().insert(
            REASON_METADATA_KEY,
            "something_new".parse().expect("a valid metadata value"),
        );
        let cases = [
            (
                "connection refused",
                Status::new(Code::Unavailable, "connection refused"),
                3,
            ),
            (
                "missing confirmation",
                Status::new(Code::FailedPrecondition, "confirm"),
                7,
            ),
            ("slug from a newer daemon", newer, 5),
            (
                "older protocol",
                Status::new(Code::Unimplemented, "no such call"),
                8,
            ),
            ("unclassified", Status::new(Code::Internal, "broken"), 1),
        ];
        for (name, status, want) in cases {
            assert_eq!(
                from_status(&status).code(),
                want,
                "the {name} status must exit {want}"
            );
        }
    }
}
