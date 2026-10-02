//! Installing and controlling the background service.

use crate::commands::ServiceCommand;
use crate::exit::ExitCode;
use crate::output::Format;
use midi_harbor_service::{self as service, ServiceSpec};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long a freshly started daemon may take to answer before starting is called a failure.
const STARTUP_WAIT: Duration = Duration::from_secs(10);

/// How often the socket is tried while waiting.
const STARTUP_POLL: Duration = Duration::from_millis(50);

/// Stops the daemon by asking it over its socket, where no service manager runs it.
async fn stop_by_request(format: &Format, socket: Option<std::path::PathBuf>) -> ExitCode {
    let mut client = match crate::client::Client::connect(socket).await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::DaemonUnreachable;
        }
    };
    match client
        .rpc()
        .stop_daemon(midi_harbor_ipc::pb::StopDaemonRequest {})
        .await
    {
        Ok(_) => {
            format.line("stopped");
            ExitCode::Success
        }
        Err(status) => {
            eprintln!("could not stop the daemon: {}", status.message());
            ExitCode::Failure
        }
    }
}

/// Runs a service subcommand.
pub async fn run(
    command: &ServiceCommand,
    format: &Format,
    socket: Option<std::path::PathBuf>,
) -> ExitCode {
    // A machine without a supported service manager is a supported situation, not a failure of
    // this program, so it is reported with the alternative rather than as an error.
    let manager = match service::detect() {
        Ok(manager) => manager,
        // The App Store app runs its daemon itself, so there is nothing to install or start, but
        // it can still be stopped, which the app's own Quit does the same way.
        Err(service::ServiceError::AppStore) if matches!(command, ServiceCommand::Stop) => {
            return stop_by_request(format, socket).await;
        }
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::Unavailable;
        }
    };

    match command {
        ServiceCommand::Install { start } => {
            let spec = match ServiceSpec::for_current_executable() {
                Ok(spec) => spec,
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::Failure;
                }
            };

            // Re-running install updates the registration in place, so say which happened.
            let existed = manager
                .status()
                .map(|status| status.installed)
                .unwrap_or(false);
            // With --start a daemon already running is stopped first, so the one started is this
            // copy. Starting a service that is running does nothing, and left an updated program
            // registered beside the old daemon still running.
            let installed = if *start {
                manager.replace(&spec)
            } else {
                manager.install(&spec)
            };
            let path = match installed {
                Ok(path) => path,
                Err(error) => {
                    eprintln!("could not install the service: {error}");
                    return ExitCode::Failure;
                }
            };

            let verb = if existed { "updated" } else { "installed" };
            format.line(format!(
                "{verb} the {} service at {}",
                manager.name(),
                path.display()
            ));
            format.line("it will start automatically at login");

            if *start {
                return report_started(format);
            }
            ExitCode::Success
        }

        ServiceCommand::Uninstall => match manager.uninstall() {
            Ok(()) => {
                format.line("the service was removed; your configuration was left untouched");
                ExitCode::Success
            }
            Err(error) => {
                eprintln!("could not remove the service: {error}");
                ExitCode::Failure
            }
        },

        ServiceCommand::Start => match manager.start() {
            Ok(()) => report_started(format),
            Err(error) => {
                eprintln!("could not start the service: {error}");
                ExitCode::Failure
            }
        },

        ServiceCommand::Stop => match manager.stop() {
            Ok(()) => {
                format.line("stopped");
                ExitCode::Success
            }
            Err(error) => {
                eprintln!("could not stop the service: {error}");
                ExitCode::Failure
            }
        },

        ServiceCommand::Status => match manager.status() {
            Ok(status) => {
                // The version and uptime come from the daemon itself, when one answers: the
                // service manager knows only that a process is registered and alive (US2/AC2).
                let daemon = match crate::client::Client::connect(socket).await {
                    Ok(client) => Some(client.server().clone()),
                    Err(_) => None,
                };
                let started = daemon
                    .as_ref()
                    .and_then(|info| info.started_at.as_ref())
                    .and_then(|at| jiff::Timestamp::new(at.seconds, at.nanos).ok());
                let uptime = started.map(|at| {
                    jiff::Timestamp::now()
                        .as_second()
                        .saturating_sub(at.as_second())
                });
                if format.json {
                    format.emit(&serde_json::json!({
                        "manager": manager.name(),
                        "installed": status.installed,
                        "running": status.running,
                        "stale": status.stale,
                        "definition_path": status.definition_path,
                        "registered_executable": status.registered_executable,
                        "daemon_version": daemon.as_ref().map(|info| info.daemon_version.clone()),
                        "same_build": daemon.as_ref().map(|info| info.build_id == midi_harbor_core::BUILD_ID),
                        "started_at": started.map(|at| at.to_string()),
                        "uptime_seconds": uptime,
                    }));
                } else {
                    format.line(format!("{}: {}", manager.name(), status.describe()));
                    if let Some(info) = &daemon {
                        let mut line = format!("daemon: version {}", info.daemon_version);
                        if let Some(seconds) = uptime {
                            line.push_str(&format!(", up {}", uptime_text(seconds)));
                        }
                        format.line(line);
                        // A daemon left running by another copy does not have what this one
                        // has, and nothing else here would say so.
                        if info.build_id != midi_harbor_core::BUILD_ID {
                            format.line(
                                "daemon: another build than this program; run 'midi-harbor \
                                 service install --start' to replace it with this one",
                            );
                        }
                    }
                    if let Some(path) = &status.definition_path {
                        format.line(format!("definition: {}", path.display()));
                    }
                }
                // A stale registration is a problem the user must fix, so it must not exit zero.
                if status.stale {
                    ExitCode::Failure
                } else {
                    ExitCode::Success
                }
            }
            Err(error) => {
                eprintln!("could not read the service status: {error}");
                ExitCode::Failure
            }
        },
    }
}

/// Reports a start once the daemon answers, which is what "started" has to mean.
///
/// The service manager returns as soon as it has launched the process, before the daemon has
/// opened its socket. Saying "started" then sent a script's very next command to a daemon not
/// yet listening: `service install --start` followed by `port create`, the quickstart's first
/// two commands, failed with "the daemon is not running".
fn report_started(format: &Format) -> ExitCode {
    let socket = match midi_harbor_core::paths::Paths::resolve() {
        Ok(paths) => paths.socket_file(),
        Err(error) => {
            eprintln!("started, but could not find where the daemon listens: {error}");
            return ExitCode::Failure;
        }
    };
    if answers_within(&socket, STARTUP_WAIT) {
        format.line("started");
        ExitCode::Success
    } else {
        eprintln!(
            "the service was started but the daemon did not answer within {} seconds",
            STARTUP_WAIT.as_secs()
        );
        eprintln!("see 'midi-harbor service status', or run 'midi-harbor daemon' to see why");
        ExitCode::DaemonUnreachable
    }
}

/// Renders how long the daemon has been up, to the two largest units that say something.
#[allow(clippy::integer_division)]
fn uptime_text(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
    );
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

/// Waits up to `bound` for something to accept connections on the socket.
fn answers_within(socket: &Path, bound: Duration) -> bool {
    let started = Instant::now();
    loop {
        if midi_harbor_ipc::transport::answers(socket) {
            return true;
        }
        if started.elapsed() >= bound {
            return false;
        }
        std::thread::sleep(STARTUP_POLL);
    }
}
