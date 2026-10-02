//! Telling the user that the daemon is not the build the window is, and updating it when they
//! ask (research R-108).
//!
//! The window and the daemon are one program, installed together. A daemon keeps running the
//! copy it was started from, so after an update, or with a second copy of Midi Harbor installed
//! another way, the window can reach a daemon that is not the build it came with. Every build
//! carries an identifier and the daemon reports its own. When they differ the window says so,
//! and offers to register its own copy as the service and restart the daemon from it.
//!
//! Nothing is replaced until the user asks. A restart drops every connection for a few seconds,
//! which is theirs to time, and two windows of different builds cannot then replace each
//! other's daemon in turn.

use midi_harbor_service::{ServiceSpec, ServiceStatus};

/// How the daemon's version stands against the window's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relation {
    /// The daemon is an older version, or too old to say which.
    Older {
        /// The version the daemon reports, empty when it reports none.
        version: String,
    },
    /// The daemon is this version, built or installed separately.
    Same,
    /// The daemon is a newer version. This window can still take its place, but an older
    /// daemon may not read the configuration the newer one wrote.
    Newer {
        /// The version the daemon reports.
        version: String,
    },
}

/// Why the window cannot replace the daemon itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// The window was pointed at the daemon with `--socket`, which the service does not serve.
    OtherSocket,
    /// The service is not running the daemon, so there is no registration that says how it was
    /// started.
    ByHand,
}

/// What the window says about a daemon that is another build than itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// How the daemon's version stands against the window's.
    pub relation: Relation,
    /// Why the window cannot replace it, when it cannot.
    pub blocked: Option<Blocked>,
}

impl Notice {
    /// Works out what to say about a daemon of another build, reporting `daemon_version`, from
    /// a window of `own_version`. `other_socket` says the window was started with `--socket`,
    /// and `service` is what the service manager reports, when there is one to ask.
    pub fn about(
        daemon_version: &str,
        own_version: &str,
        other_socket: bool,
        service: Option<&ServiceStatus>,
    ) -> Self {
        let relation = match (numbers(daemon_version), numbers(own_version)) {
            (Some(daemon), Some(own)) if daemon > own => Relation::Newer {
                version: daemon_version.to_owned(),
            },
            (Some(daemon), Some(own)) if daemon == own => Relation::Same,
            _ => Relation::Older {
                version: daemon_version.to_owned(),
            },
        };
        let by_service = service.is_some_and(|service| service.installed && service.running);
        let blocked = if other_socket {
            Some(Blocked::OtherSocket)
        } else if by_service {
            None
        } else {
            Some(Blocked::ByHand)
        };
        Self { relation, blocked }
    }

    /// Returns the notice's heading.
    pub fn heading(&self) -> &'static str {
        match self.relation {
            Relation::Older { .. } => "The daemon is outdated",
            Relation::Same => "The daemon is a different build",
            Relation::Newer { .. } => "The daemon is newer than this window",
        }
    }

    /// Says what was found and what updating does, or what to do where the window cannot.
    pub fn explanation(&self, own_version: &str) -> String {
        let found = match &self.relation {
            Relation::Older { version } if version.is_empty() => {
                format!("It is running an older Midi Harbor, and this is {own_version}.")
            }
            Relation::Older { version } | Relation::Newer { version } => {
                format!("It is running Midi Harbor {version}, and this is {own_version}.")
            }
            Relation::Same => {
                format!("It was started from another copy of Midi Harbor {own_version}.")
            }
        };
        let next = match (self.blocked, &self.relation) {
            (Some(Blocked::OtherSocket), _) => {
                "This window was pointed at it with --socket, so restart it from this copy to \
                 use this version."
            }
            (Some(Blocked::ByHand), _) => {
                "The background service did not start it, so stop it and start it from this \
                 copy to use this version."
            }
            (None, Relation::Newer { .. }) => {
                "Open that version instead, or put this older one in its place. Connections \
                 drop for a few seconds and come back."
            }
            (None, _) => {
                "Updating restarts it from this copy. Connections drop for a few seconds and \
                 come back."
            }
        };
        format!("{found} {next}")
    }

    /// Returns the label of the button that replaces the daemon, or nothing when the window
    /// cannot.
    pub fn action(&self) -> Option<&'static str> {
        if self.blocked.is_some() {
            return None;
        }
        Some(match self.relation {
            Relation::Newer { .. } => "Use this version",
            _ => "Update now",
        })
    }
}

/// Reads a version's numbers, leaving out anything after them such as `-rc1`.
///
/// Compared as numbers, since 0.10.0 is newer than 0.9.3 and older as text.
fn numbers(version: &str) -> Option<Vec<u64>> {
    version
        .split(['-', '+'])
        .next()?
        .split('.')
        .map(|part| part.parse().ok())
        .collect()
}

/// Asks the service manager what is registered, or returns nothing where there is none to ask.
pub async fn service_status() -> Option<ServiceStatus> {
    tokio::task::spawn_blocking(|| midi_harbor_service::detect().ok()?.status().ok())
        .await
        .ok()
        .flatten()
}

/// Registers this copy as the service in place of whatever is registered, and restarts the
/// daemon from it.
pub async fn replace() -> Result<(), String> {
    let done = tokio::task::spawn_blocking(|| {
        let manager = midi_harbor_service::detect().map_err(|error| error.to_string())?;
        let spec = ServiceSpec::for_current_executable().map_err(|error| error.to_string())?;
        manager
            .replace(&spec)
            .map(|_| ())
            .map_err(|error| format!("could not update the daemon: {error}"))
    })
    .await;
    done.map_err(|error| format!("updating the daemon did not finish: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves what the window says about a daemon of another build, and when it offers to
    /// replace it: only a daemon the service is running, reached on the service's own socket.
    ///
    /// 0.10.0 is newer than 0.9.3 by its numbers and older as text. A daemon older than
    /// protocol 1.3 reports no build, and may report a version; one that reports neither is
    /// outdated all the same.
    #[test]
    fn a_daemon_of_another_build_is_named_and_offered_only_where_the_service_runs_it() {
        let running = ServiceStatus {
            installed: true,
            running: true,
            ..ServiceStatus::default()
        };
        let stopped = ServiceStatus {
            running: false,
            ..running.clone()
        };
        let older = |version: &str| Relation::Older {
            version: version.to_owned(),
        };
        let cases = [
            (
                "an older daemon",
                ("0.9.3", "0.10.0", false, Some(&running)),
                (older("0.9.3"), None, Some("Update now")),
            ),
            (
                "a daemon that reports no version",
                ("", "0.10.0", false, Some(&running)),
                (older(""), None, Some("Update now")),
            ),
            (
                "another build of this version",
                ("0.10.0", "0.10.0", false, Some(&running)),
                (Relation::Same, None, Some("Update now")),
            ),
            (
                "a newer daemon",
                ("0.10.0", "0.9.3", false, Some(&running)),
                (
                    Relation::Newer {
                        version: "0.10.0".to_owned(),
                    },
                    None,
                    Some("Use this version"),
                ),
            ),
            (
                "a daemon the service is not running",
                ("0.9.3", "0.10.0", false, Some(&stopped)),
                (older("0.9.3"), Some(Blocked::ByHand), None),
            ),
            (
                "no service manager",
                ("0.9.3", "0.10.0", false, None),
                (older("0.9.3"), Some(Blocked::ByHand), None),
            ),
            (
                "a window pointed at another socket",
                ("0.9.3", "0.10.0", true, Some(&running)),
                (older("0.9.3"), Some(Blocked::OtherSocket), None),
            ),
        ];
        for (name, (daemon, own, other_socket, service), (relation, blocked, action)) in cases {
            let notice = Notice::about(daemon, own, other_socket, service);
            assert_eq!(
                (notice.relation.clone(), notice.blocked, notice.action()),
                (relation, blocked, action),
                "{name}: said or offered wrongly"
            );
        }
    }
}
