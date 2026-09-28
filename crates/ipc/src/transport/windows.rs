//! Carrying the contract over a named pipe.
//!
//! The pipe's name is random, and the daemon writes it to a file where a socket would be on the
//! other platforms. Only someone who can read that file can find the pipe, so the user's own
//! profile directory isolates one user's daemon the way a socket's mode does elsewhere, and no
//! other process can claim the name before the daemon does. The pipe refuses clients on other
//! machines, and Windows' default access for a pipe lets other local users open it only for
//! reading, which is not enough to send a request.

use super::TransportError;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint, Uri};

/// The start of every pipe name the daemon creates.
///
/// A file whose contents do not start with this is not followed, so a planted file cannot send a
/// client to some other path.
const PIPE_PREFIX: &str = r"\\.\pipe\midi-harbor-";

/// How many accepted connections may wait for the server to take them.
const BACKLOG: usize = 16;

/// How long a client waits between tries while every pipe instance is busy.
const BUSY_RETRY: Duration = Duration::from_millis(50);

/// How many times a client tries a busy pipe before giving up, two seconds in all.
const BUSY_ATTEMPTS: u32 = 40;

/// `ERROR_FILE_NOT_FOUND`: no pipe of that name exists.
const ERROR_FILE_NOT_FOUND: i32 = 2;

/// `ERROR_PIPE_BUSY`: the pipe exists, and every instance is taken until the server makes another.
const ERROR_PIPE_BUSY: i32 = 231;

/// One client's connection to the daemon.
pub struct Connection(NamedPipeServer);

impl tonic::transport::server::Connected for Connection {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(context, buffer)
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(context)
    }
}

/// Creates the daemon's pipe and records its name at `path`, replacing a record left by a
/// previous run.
///
/// A record outliving its process is normal after a crash, so an existing one is replaced rather
/// than treated as another daemon: liveness is decided by whether the pipe it names answers.
pub fn bind(path: &Path) -> Result<ReceiverStream<io::Result<Connection>>, TransportError> {
    // A pipe something answers on belongs to a running daemon, and replacing the record would
    // leave two daemons running with only the second reachable.
    if answers(path) {
        return Err(TransportError::InUse(path.to_path_buf()));
    }

    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|source| TransportError::Io {
            operation: "create",
            path: parent.to_path_buf(),
            source,
        })?;
    }

    // Create the pipe before recording it, so the name is never readable before it is taken.
    let name = format!("{PIPE_PREFIX}{}", uuid::Uuid::new_v4().simple());
    let first = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(&name)
        .map_err(|source| TransportError::Io {
            operation: "create a pipe for",
            path: path.to_path_buf(),
            source,
        })?;
    std::fs::write(path, &name).map_err(|source| TransportError::Io {
        operation: "write",
        path: path.to_path_buf(),
        source,
    })?;

    // Each instance serves one client, so a new one is made as each is taken.
    let (sender, receiver) = tokio::sync::mpsc::channel(BACKLOG);
    tokio::spawn(async move {
        let mut waiting = first;
        loop {
            if let Err(error) = waiting.connect().await {
                if sender.send(Err(error)).await.is_err() {
                    return;
                }
                continue;
            }
            let next = match ServerOptions::new()
                .reject_remote_clients(true)
                .create(&name)
            {
                Ok(next) => next,
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            };
            let connected = std::mem::replace(&mut waiting, next);
            if sender.send(Ok(Connection(connected))).await.is_err() {
                return;
            }
        }
    });
    Ok(ReceiverStream::new(receiver))
}

/// Connects a client channel to the daemon's pipe.
///
/// The URI is required by the HTTP stack but never used, because the connector ignores it and
/// opens the pipe directly.
pub async fn connect(path: &Path) -> Result<Channel, TransportError> {
    let name = pipe_name(path).ok_or_else(|| TransportError::NotRunning(path.to_path_buf()))?;
    let missing = path.to_path_buf();

    let channel = Endpoint::from_static("http://[::1]:50051")
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let name = name.clone();
            async move {
                let mut attempts = 0;
                loop {
                    match ClientOptions::new().open(&name) {
                        Ok(client) => return Ok(hyper_util::rt::TokioIo::new(client)),
                        Err(error)
                            if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                                && attempts < BUSY_ATTEMPTS =>
                        {
                            attempts += 1;
                            tokio::time::sleep(BUSY_RETRY).await;
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }))
        .await
        .map_err(|error| {
            // No such pipe is the named-pipe form of nothing listening.
            if gone(&error) {
                TransportError::NotRunning(missing)
            } else {
                TransportError::Connect(error)
            }
        })?;
    Ok(channel)
}

/// Reports whether a daemon is actually serving, as opposed to a stale record existing.
pub async fn probe(path: &Path) -> bool {
    answers(path)
}

/// Reports whether the pipe recorded at `path` exists and accepts clients, without a runtime.
pub fn answers(path: &Path) -> bool {
    let Some(name) = pipe_name(path) else {
        return false;
    };
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&name)
    {
        Ok(_) => true,
        // Busy means every instance is taken by other clients, which only a live daemon has.
        Err(error) => error.raw_os_error() == Some(ERROR_PIPE_BUSY),
    }
}

/// Reads the pipe name recorded at `path`, refusing anything that is not one of ours.
pub fn pipe_name(path: &Path) -> Option<String> {
    let recorded = std::fs::read_to_string(path).ok()?;
    let name = recorded.trim();
    (name.starts_with(PIPE_PREFIX)
        && name
            .get(PIPE_PREFIX.len()..)
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_hexdigit())))
    .then(|| name.to_owned())
}

/// Reports whether a connection failed because the pipe does not exist.
fn gone(error: &tonic::transport::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<io::Error>()
            && io.raw_os_error() == Some(ERROR_FILE_NOT_FOUND)
        {
            return true;
        }
        source = current.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Builds a record path in a directory of its own.
    fn temp_record(label: &str) -> PathBuf {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        std::env::temp_dir()
            .join("mh-pipes")
            .join(format!("{label}-{unique}"))
            .join("daemon.sock")
    }

    /// Locks how `bind` treats an existing record: one naming a pipe that is gone, as a crash
    /// leaves, is replaced, while one naming a pipe a daemon answers on is refused and left as it
    /// was.
    ///
    /// Replacing a live record would leave two daemons running with only the second reachable.
    #[tokio::test]
    async fn a_stale_record_is_replaced_and_a_live_one_is_refused() {
        let stale = temp_record("stale");
        std::fs::create_dir_all(stale.parent().expect("a record path has a directory"))
            .expect("the record's directory must be made");
        std::fs::write(&stale, format!("{PIPE_PREFIX}0123456789abcdef"))
            .expect("the stale record must be written");
        let _replacement = bind(&stale).expect("a stale record must not block a restart");
        assert!(probe(&stale).await, "the replacement pipe must answer");

        let live = temp_record("live");
        let _first = bind(&live).expect("the first daemon's pipe must be created");
        let recorded =
            std::fs::read_to_string(&live).expect("the first daemon must record its pipe");
        assert!(
            matches!(bind(&live), Err(TransportError::InUse(_))),
            "a pipe a daemon answers on must not be taken over"
        );
        assert_eq!(
            std::fs::read_to_string(&live).expect("the record must still be there"),
            recorded,
            "the first daemon's record must be left as it was"
        );
        assert!(probe(&live).await, "the first daemon must keep its pipe");
    }

    /// Locks what a client makes of a record that leads to no daemon: no record, a record naming
    /// one of our pipes that no longer exists (`ERROR_FILE_NOT_FOUND`, 2), and a record naming
    /// anything that is not one of our pipes all read as no daemon running.
    ///
    /// The last is the guard research R-087 relies on: a planted record must not send a client
    /// to some other path.
    #[tokio::test]
    async fn a_record_that_leads_to_no_daemon_reads_as_not_running() {
        /// Writes whatever a case needs beside the record, and returns the record's contents.
        type Record = fn(&Path) -> Option<String>;
        let cases: [(&str, Record); 3] = [
            ("no record", |_| None),
            ("a record naming a gone pipe", |_| {
                Some(format!("{PIPE_PREFIX}0123456789abcdef"))
            }),
            ("a record naming a file", |directory| {
                let target = directory.join("target.txt");
                std::fs::write(&target, b"not a pipe").expect("the target must be written");
                Some(target.display().to_string())
            }),
        ];
        for (name, record) in cases {
            let path = temp_record("absent");
            let directory = path.parent().expect("a record path has a directory");
            std::fs::create_dir_all(directory).expect("the record's directory must be made");
            if let Some(contents) = record(directory) {
                std::fs::write(&path, contents).expect("the record must be written");
            }
            assert!(!probe(&path).await, "{name}: nothing must answer");
            assert!(
                matches!(connect(&path).await, Err(TransportError::NotRunning(_))),
                "{name}: a client must be told no daemon is running"
            );
        }
    }
}
