//! Contract version checking.
//!
//! The version lives in the proto package name, so a breaking change means a new package and an
//! old client meets `UNIMPLEMENTED` rather than undefined behaviour. That alone does not say
//! *which* side is old, which is why clients call `GetServerInfo` first.

use crate::pb::ServerInfo;

/// Major version of the contract this build speaks, matching the proto package `midiharbor.v1`.
pub const PROTOCOL_MAJOR: u32 = 1;

/// Minor version, raised for additive changes: new calls, new fields, new enum values.
///
/// 1.1 added `StopDaemon`. A 1.0 daemon answers it with `UNIMPLEMENTED`, and every earlier call
/// is unchanged. 1.2 added `StatusSummary.midi_server_replaced_at`, the last-received and
/// last-sent times in `TrafficCounters`, and a network port's `automatic_port_counters`, which an
/// older daemon leaves unset, and `SendTestNote` and `DismissMidiServerWarning`, which it answers
/// with `UNIMPLEMENTED`.
pub const PROTOCOL_MINOR: u32 = 2;

/// A client and daemon that cannot talk to each other.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "this client speaks protocol {client_major}, the daemon speaks {daemon_major}; update {stale}"
)]
pub struct VersionMismatch {
    /// The major version this client speaks.
    pub client_major: u32,
    /// The major version the daemon speaks.
    pub daemon_major: u32,
    /// The daemon's build version, for the message.
    pub daemon_version: String,
    /// Which component the user must update.
    pub stale: Stale,
}

/// Which side of a version mismatch is behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stale {
    /// The client is older and must be updated.
    Client,
    /// The daemon is older and must be updated.
    Daemon,
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client => f.write_str("the client"),
            Self::Daemon => f.write_str("the daemon"),
        }
    }
}

impl VersionMismatch {
    /// Returns guidance naming both versions and the component to update.
    ///
    /// Clients render this instead of a generic connection error, because "could not connect"
    /// sends a user looking at sockets and permissions when the real answer is an update.
    pub fn guidance(&self) -> String {
        match self.stale {
            Stale::Client => format!(
                "the daemon is running {} and speaks protocol {}; this client speaks {}. update the client",
                self.daemon_version, self.daemon_major, self.client_major
            ),
            Stale::Daemon => format!(
                "the daemon is running {} and speaks protocol {}; this client speaks {}. restart the daemon so it picks up the new build",
                self.daemon_version, self.daemon_major, self.client_major
            ),
        }
    }
}

/// Checks whether this client can talk to the daemon that returned `info`.
///
/// A differing major version is refused. A differing minor version is fine in either direction:
/// unknown fields are ignored on the wire, and an unknown call returns `UNIMPLEMENTED` which the
/// caller handles where it is used.
pub fn check_compatibility(info: &ServerInfo) -> Result<(), VersionMismatch> {
    if info.protocol_major == PROTOCOL_MAJOR {
        return Ok(());
    }
    let stale = if info.protocol_major > PROTOCOL_MAJOR {
        Stale::Client
    } else {
        Stale::Daemon
    };
    Err(VersionMismatch {
        client_major: PROTOCOL_MAJOR,
        daemon_major: info.protocol_major,
        daemon_version: info.daemon_version.clone(),
        stale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the compatibility rule of the versioned contract: the same major version is
    /// compatible whatever the minor, in either direction, and a different major is refused
    /// naming the side that is behind.
    ///
    /// Proto3 ignores unknown fields, so a minor difference is safe; a major difference is a new
    /// package, `midiharbor.v2`, which an old peer cannot call.
    #[test]
    fn only_a_different_major_version_is_refused_and_the_stale_side_is_named() {
        let cases = [
            ("the same version", PROTOCOL_MAJOR, PROTOCOL_MINOR, None),
            ("a newer minor", PROTOCOL_MAJOR, PROTOCOL_MINOR + 7, None),
            ("the oldest minor", PROTOCOL_MAJOR, 0, None),
            ("a newer major", PROTOCOL_MAJOR + 1, 0, Some(Stale::Client)),
            ("an older major", PROTOCOL_MAJOR - 1, 0, Some(Stale::Daemon)),
        ];
        for (name, major, minor, want_stale) in cases {
            let info = ServerInfo {
                daemon_version: "0.1.0".to_owned(),
                protocol_major: major,
                protocol_minor: minor,
                started_at: None,
                config_path: String::new(),
                socket_path: String::new(),
            };
            assert_eq!(
                check_compatibility(&info)
                    .err()
                    .map(|mismatch| mismatch.stale),
                want_stale,
                "{name}: the wrong side was told to update, or a compatible daemon was refused"
            );
        }
    }
}
