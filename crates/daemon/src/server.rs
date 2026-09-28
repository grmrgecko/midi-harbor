//! Serving the contract on the daemon's socket.

use crate::service::HarborService;
use crate::state::Daemon;
use midi_harbor_ipc::HarborServer;
use midi_harbor_ipc::transport;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// Reports whether a daemon already answers on the socket these paths name.
///
/// Asked before starting anything, because a second daemon creates its own ports and sessions
/// before it reaches the socket, and those collide with the first's.
pub async fn already_serving(paths: &midi_harbor_core::paths::Paths) -> bool {
    transport::probe(&paths.socket_file()).await
}

/// How long stopping waits for clients to finish before closing their connections.
///
/// Long enough for a request already being answered, such as a route being created, to finish.
/// A stream a window is watching never finishes on its own, so waiting for it is pointless.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Why the daemon stopped serving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// The process was asked to stop.
    Asked,
    /// This process can no longer reach the MIDI service, and a new one has to take its place.
    Replace,
}

/// Runs the daemon until the process is asked to stop, or has to be replaced.
///
/// Returns only then, so callers can run this in the foreground under any supervisor.
pub async fn run(daemon: Arc<Daemon>) -> Result<Stopped, Box<dyn std::error::Error>> {
    let socket = daemon.paths().socket_file();
    let incoming = transport::bind(&socket)?;
    info!(socket = %socket.display(), "daemon listening");
    let asked = stop_requests(&socket)?;

    // Learn why serving has to end, and when.
    let (stopped_tx, stopped_rx) = tokio::sync::watch::channel(None);
    let replaced = Arc::clone(&daemon);
    tokio::spawn(async move {
        let why = tokio::select! {
            () = shutdown(asked) => Stopped::Asked,
            () = replaced.restart_requested() => Stopped::Replace,
            () = replaced.stop_requested() => Stopped::Asked,
        };
        let _ = stopped_tx.send(Some(why));
    });

    // Serve until then. Stopping waits for open connections to close, and a client watching a
    // stream never closes its own, so an open window kept the daemon running after it was asked
    // to stop, until the service manager killed it without silencing anything.
    let mut graceful = stopped_rx.clone();
    let mut forced = stopped_rx.clone();
    let service = HarborService::new(Arc::clone(&daemon));
    let serving = tonic::transport::Server::builder()
        .add_service(HarborServer::new(service))
        .serve_with_incoming_shutdown(incoming, async move {
            let _ = graceful.wait_for(Option::is_some).await;
        });
    let served = tokio::select! {
        served = serving => served,
        () = async move {
            let _ = forced.wait_for(Option::is_some).await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => {
            info!("clients are still connected; closing their connections");
            Ok(())
        }
    };

    // Both of these happen however this was reached. Notes held when the daemon stops would
    // sound until something else stops them, and nothing else will: the only thing that knew
    // they were playing is this process. Returning early on a serve error would skip that.
    daemon.silence_all().await;
    daemon.end_sessions().await;
    daemon.release_platform();

    // Leaving the socket behind would make the next start look like a daemon is already running.
    let _ = std::fs::remove_file(&socket);
    served?;

    let stopped = (*stopped_rx.borrow()).unwrap_or(Stopped::Asked);
    info!("daemon stopped");
    Ok(stopped)
}

/// Starts listening for requests to stop other than signals, where the platform needs one.
///
/// Windows has no terminate signal for `service stop` to send, so the daemon waits on an event
/// named after its pipe instead. Unix has the signal, and nothing else.
fn stop_requests(
    socket: &std::path::Path,
) -> Result<Option<Arc<tokio::sync::Notify>>, Box<dyn std::error::Error>> {
    #[cfg(windows)]
    {
        let pipe = transport::pipe_name(socket).ok_or("the daemon's pipe was not recorded")?;
        Ok(Some(midi_harbor_platform::stop::listen(&pipe)?))
    }
    #[cfg(not(windows))]
    {
        let _ = socket;
        Ok(None)
    }
}

/// Resolves when a request from `stop_requests` arrives, and never if there is nothing to ask.
async fn requested(asked: Option<Arc<tokio::sync::Notify>>) {
    match asked {
        Some(asked) => asked.notified().await,
        None => std::future::pending().await,
    }
}

/// Resolves when the process is asked to stop.
#[cfg(unix)]
async fn shutdown(asked: Option<Arc<tokio::sync::Notify>>) {
    let interrupt = tokio::signal::ctrl_c();
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(signal) => signal,
        // Without a terminate handler the daemon still stops on interrupt, which is enough to
        // avoid leaving it unstoppable.
        Err(_) => {
            let _ = interrupt.await;
            return;
        }
    };
    tokio::select! {
        _ = interrupt => {}
        _ = term.recv() => {}
        () = requested(asked) => {}
    }
}

/// Resolves when the process is asked to stop: by `service stop`, by Ctrl-C, or by its console
/// closing or the user signing out.
#[cfg(windows)]
async fn shutdown(asked: Option<Arc<tokio::sync::Notify>>) {
    use tokio::signal::windows;

    let interrupt = tokio::signal::ctrl_c();
    let (Ok(mut close), Ok(mut logoff), Ok(mut system)) = (
        windows::ctrl_close(),
        windows::ctrl_logoff(),
        windows::ctrl_shutdown(),
    ) else {
        tokio::select! {
            _ = interrupt => {}
            () = requested(asked) => {}
        }
        return;
    };
    tokio::select! {
        _ = interrupt => {}
        () = requested(asked) => {}
        _ = close.recv() => {}
        _ = logoff.recv() => {}
        _ = system.recv() => {}
    }
}
