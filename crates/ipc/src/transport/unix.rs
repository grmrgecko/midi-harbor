//! Carrying the contract over a Unix domain socket.
//!
//! A local socket gets per-user isolation from a file permission instead of an authentication
//! scheme.

use super::TransportError;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tokio::net::{UnixListener, UnixStream};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint, Uri};

/// Mode for the socket itself, so only the owning user can reach the daemon.
const SOCKET_MODE: u32 = 0o600;
/// Mode for the directory holding it.
const DIRECTORY_MODE: u32 = 0o700;

/// Longest usable socket path, from the `sun_path` field of `sockaddr_un`.
///
/// A hard kernel limit, not a convention: 104 bytes on macOS and the BSDs, 108 on Linux, with one
/// reserved for the terminator. Exceeding it produces an error that says nothing useful about
/// what went wrong, so the length is checked before binding.
#[cfg(target_os = "linux")]
const MAX_SOCKET_PATH: usize = 107;

/// Longest usable socket path on this platform.
#[cfg(not(target_os = "linux"))]
const MAX_SOCKET_PATH: usize = 103;

/// Binds the daemon's listening socket, replacing a stale one left by a previous run.
///
/// A socket file outliving its process is normal after a crash, so an existing path is removed
/// rather than treated as another daemon: liveness is decided by whether a connection succeeds,
/// which `probe` does.
pub fn bind(path: &Path) -> Result<UnixListenerStream, TransportError> {
    check_length(path)?;

    // Only a directory made here is made private. One that already exists belongs to someone
    // else's plan: a socket named with `--socket` in `/tmp` once had the daemon try to close
    // `/tmp` to every other user, and fail to start where it was not allowed to. The socket's
    // own mode is what keeps other users out.
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|source| TransportError::Io {
            operation: "create",
            path: parent.to_path_buf(),
            source,
        })?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(DIRECTORY_MODE)).map_err(
            |source| TransportError::Io {
                operation: "secure",
                path: parent.to_path_buf(),
                source,
            },
        )?;
    }

    // A socket something answers on belongs to a running daemon. Removing it anyway left two
    // daemons running, each with its own ports and sessions, and only the second reachable.
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(TransportError::InUse(path.to_path_buf()));
    }

    // Remove a leftover socket so binding does not fail on a path nothing is listening to.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(TransportError::Io {
                operation: "clear",
                path: path.to_path_buf(),
                source,
            });
        }
    }

    let listener = UnixListener::bind(path).map_err(|source| TransportError::Io {
        operation: "bind",
        path: path.to_path_buf(),
        source,
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE)).map_err(
        |source| TransportError::Io {
            operation: "secure",
            path: path.to_path_buf(),
            source,
        },
    )?;

    Ok(UnixListenerStream::new(listener))
}

/// Connects a client channel to the daemon's socket.
///
/// The URI is required by the HTTP stack but never used, because the connector ignores it and
/// dials the socket path directly.
pub async fn connect(path: &Path) -> Result<Channel, TransportError> {
    check_length(path)?;
    if !path.exists() {
        return Err(TransportError::NotRunning(path.to_path_buf()));
    }
    let socket = path.to_path_buf();

    let channel = Endpoint::from_static("http://[::1]:50051")
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move {
                let stream = UnixStream::connect(socket).await?;
                Ok::<_, io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await?;
    Ok(channel)
}

/// Rejects a socket path the operating system cannot represent, with a message that explains it.
fn check_length(path: &Path) -> Result<(), TransportError> {
    let length = path.as_os_str().as_encoded_bytes().len();
    if length > MAX_SOCKET_PATH {
        return Err(TransportError::PathTooLong {
            path: path.to_path_buf(),
            length,
            limit: MAX_SOCKET_PATH,
        });
    }
    Ok(())
}

/// Reports whether a daemon is actually listening, as opposed to a stale socket file existing.
pub async fn probe(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    UnixStream::connect(path).await.is_ok()
}

/// Reports whether something accepts connections on the socket, without a runtime.
pub fn answers(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Builds a socket path short enough to bind, since `sun_path` is tight on macOS.
    ///
    /// Every path shares one root, emptied on first use in each run, so directories from earlier
    /// runs do not pile up in the temporary directory.
    fn temp_socket(label: &str) -> PathBuf {
        static EMPTIED: std::sync::Once = std::sync::Once::new();
        let shared = std::env::temp_dir().join("mh-sockets");
        EMPTIED.call_once(|| {
            let _ = std::fs::remove_dir_all(&shared);
        });
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let short = unique.get(..8).unwrap_or("00000000");
        shared.join(format!("{label}-{short}")).join("d.sock")
    }

    /// Returns a file's permission bits.
    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("the path must exist to have a mode")
            .permissions()
            .mode()
            & 0o777
    }

    /// Locks the file modes that stand in for authentication: the socket is 0600, and a
    /// directory the daemon makes for it is 0700, while one that already existed keeps its mode.
    ///
    /// The file permission is what decides who can reach the daemon (AGENTS.md, the daemon
    /// contract). A socket named with `--socket` in `/tmp` once had the daemon try to close
    /// `/tmp` to every other user.
    #[tokio::test]
    async fn the_socket_is_private_and_a_directory_it_did_not_make_is_left_alone() {
        let cases = [
            ("a directory made for the socket", None, DIRECTORY_MODE),
            (
                "a shared directory that already existed",
                Some(0o755),
                0o755,
            ),
        ];
        for (name, existing, want_directory) in cases {
            let path = temp_socket("mode");
            let parent = path.parent().expect("a socket path has a directory");
            if let Some(existing) = existing {
                std::fs::create_dir_all(parent).expect("the shared directory must be made");
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(existing))
                    .expect("the shared directory's mode must be set");
            }
            let _listener = bind(&path).expect("the socket must bind");
            assert_eq!(
                mode(&path),
                SOCKET_MODE,
                "{name}: the socket must not be reachable by other users"
            );
            assert_eq!(
                mode(parent),
                want_directory,
                "{name}: only a directory the daemon made may be made private"
            );
        }
    }

    /// Locks how `bind` treats what it finds at the socket path: a leftover file, as a crash
    /// leaves, is replaced and does not probe as a daemon, while a socket something answers on
    /// is refused and keeps answering.
    ///
    /// Taking over a live socket once left two daemons running, each with its own ports and
    /// sessions, and only the second reachable.
    #[tokio::test]
    async fn a_leftover_socket_file_is_replaced_and_a_live_one_is_refused() {
        let leftover = temp_socket("leftover");
        std::fs::create_dir_all(leftover.parent().expect("a socket path has a directory"))
            .expect("the socket's directory must be made");
        std::fs::write(&leftover, b"stale").expect("the leftover file must be written");
        assert!(
            !probe(&leftover).await,
            "a leftover file must not be taken for a running daemon"
        );
        let _replacement = bind(&leftover).expect("a leftover file must not block a restart");
        assert!(
            probe(&leftover).await,
            "the replacement socket must accept connections"
        );

        let live = temp_socket("live");
        let _first = bind(&live).expect("the first daemon's socket must bind");
        assert!(
            matches!(bind(&live), Err(TransportError::InUse(_))),
            "a socket a daemon answers on must not be taken over"
        );
        assert!(probe(&live).await, "the first daemon must keep its socket");
    }

    /// Locks the socket path limit at the kernel's: a path of exactly `MAX_SOCKET_PATH` bytes
    /// binds, and one byte more is refused with an explanation before the kernel sees it.
    ///
    /// `sun_path` in `sockaddr_un` holds 104 bytes on macOS and the BSDs and 108 on Linux,
    /// less one for the terminator. The kernel's own refusal only says the path is too long for
    /// `SUN_LEN`, which tells a user nothing.
    #[tokio::test]
    async fn a_path_at_the_kernel_limit_binds_and_one_byte_longer_is_explained() {
        let base = temp_socket("limit");
        let directory = base.parent().expect("a socket path has a directory");
        let used = directory.as_os_str().len() + "/".len() + ".sock".len();
        let fill = MAX_SOCKET_PATH
            .checked_sub(used)
            .expect("the temporary directory must leave room for a socket name");
        let at_limit = directory.join(format!("{}.sock", "s".repeat(fill)));
        let over_limit = directory.join(format!("{}.sock", "s".repeat(fill + 1)));
        assert_eq!(
            at_limit.as_os_str().len(),
            MAX_SOCKET_PATH,
            "the path must sit exactly at the limit for the test to mean anything"
        );

        let _listener = bind(&at_limit).expect("the kernel must accept a path at the limit");
        match bind(&over_limit) {
            Err(TransportError::PathTooLong { length, limit, .. }) => assert_eq!(
                (length, limit),
                (MAX_SOCKET_PATH + 1, MAX_SOCKET_PATH),
                "the explanation must give the path's length and the limit"
            ),
            other => panic!("a path over the limit must be explained, got {other:?}"),
        }
    }
}
