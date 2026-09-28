//! Midi Harbor.
//!
//! One executable that selects its role from its arguments: the background engine, the graphical
//! interface, a command-line action, or managing its own service registration. There is no
//! separate daemon binary.

use clap::Parser;
use midi_harbor_cli::{Cli, Command, ExitCode};
use midi_harbor_daemon::log_file::{LOG_CAP, RollingFile};

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    // A supervisor leaves the log to the daemon it runs: two processes rolling one file over
    // would each rename it out from under the other.
    let log_file = match &cli.command {
        Some(Command::Daemon {
            log_file,
            supervise: false,
        }) => log_file.as_deref(),
        _ => None,
    };
    init_logging(cli.verbose, cli.quiet, log_file);

    let code = match &cli.command {
        Some(Command::Daemon {
            supervise: true, ..
        }) => supervise(),
        Some(Command::Daemon { .. }) => run_daemon(cli.socket.clone()),
        Some(Command::Gui) => run_gui(true, cli.socket.clone()),
        // No arguments means the graphical interface, or help when this build has none.
        None => run_gui(false, cli.socket.clone()),
        Some(_) => runtime().block_on(midi_harbor_cli::run::dispatch(cli)),
    };
    std::process::ExitCode::from(u8::try_from(code.code()).unwrap_or(1))
}

/// Builds the async runtime used by every role.
fn runtime() -> tokio::runtime::Runtime {
    match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start the async runtime: {error}");
            std::process::exit(ExitCode::Failure.code());
        }
    }
}

/// Runs the engine in the foreground, registering nothing with the system.
fn run_daemon(socket: Option<std::path::PathBuf>) -> ExitCode {
    runtime().block_on(async {
        let paths = match midi_harbor_core::paths::Paths::resolve() {
            Ok(paths) => paths.with_socket(socket),
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::Failure;
            }
        };

        // Checked before anything opens: a second daemon would create its own ports and
        // sessions alongside the first's before finding the socket taken.
        if midi_harbor_daemon::already_serving(&paths).await {
            eprintln!(
                "a daemon is already running at {}",
                paths.socket_file().display()
            );
            // Not a second socket: one would still read this user's configuration and open the
            // same ports and sessions.
            eprintln!("stop it with 'midi-harbor service stop', or Ctrl-C where it runs");
            return ExitCode::Failure;
        }

        let replacing = std::env::var_os(REPLACED_BECAUSE).is_some();
        // When the process being replaced found the service gone, which the warning gives as the
        // time it stopped; this one starts some seconds later, and up to half a minute.
        let lost_at = std::env::var(MIDI_SERVER_LOST_AT)
            .ok()
            .and_then(|at| at.parse::<jiff::Timestamp>().ok());
        let midi = match open_midi(replacing).await {
            Ok(midi) => midi,
            Err(error) => {
                eprintln!("could not open the platform MIDI backend: {error}");
                return ExitCode::Failure;
            }
        };
        // A radio that will not open is not fatal: the capability query reports both roles as
        // unavailable and everything else runs, which is what Principle IV asks for.
        let bluetooth = match midi_harbor_platform::bluetooth_backend() {
            Ok(bluetooth) => bluetooth,
            Err(error) => {
                eprintln!("bluetooth is unavailable: {error}");
                std::sync::Arc::new(midi_harbor_platform::fake::FakeBluetoothPlatform::new())
            }
        };
        let system = std::sync::Arc::new(midi_harbor_platform::CombinedSystemEvents::start());
        let daemon =
            match midi_harbor_daemon::Daemon::start_with_bluetooth(paths, midi, system, bluetooth)
                .await
            {
                Ok(daemon) => daemon,
                Err(error) => {
                    eprintln!("could not start the daemon: {error}");
                    return ExitCode::Failure;
                }
            };

        if replacing {
            daemon.note_replaced_after_midi_server_lost(lost_at).await;
            // Said on the desktop too, since nobody may be looking at a window or a status when
            // it matters. The sandboxed build's app is the one showing its window, so its
            // helper leaves this to the banner there.
            if !midi_harbor_core::paths::sandboxed() {
                midi_harbor_platform::notify::post(
                    "The MIDI service stopped and was restarted",
                    "Midi Harbor recovered, but other apps may have lost their MIDI connection. \
                     Relaunch any app that stops sending or receiving MIDI.",
                );
            }
        }

        let served = std::sync::Arc::clone(&daemon);
        match midi_harbor_daemon::run(served).await {
            Ok(midi_harbor_daemon::Stopped::Asked) => ExitCode::Success,
            Ok(midi_harbor_daemon::Stopped::Replace) => {
                replace_daemon(daemon.midi_server_lost_at().await)
            }
            Err(error) => {
                eprintln!("the daemon stopped: {error}");
                ExitCode::Failure
            }
        }
    })
}

/// Runs the daemon as a child with the same arguments, less `--supervise`, starting it again
/// whenever it fails.
fn supervise() -> ExitCode {
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(error) => {
            eprintln!("could not find this program to supervise it: {error}");
            return ExitCode::Failure;
        }
    };
    let arguments: Vec<std::ffi::OsString> = std::env::args_os()
        .skip(1)
        .filter(|argument| argument != midi_harbor_service::taskscheduler::SUPERVISE_FLAG)
        .collect();
    if midi_harbor_service::supervisor::supervise(&program, &arguments) {
        ExitCode::Success
    } else {
        ExitCode::Failure
    }
}

/// Names the environment variable a daemon sets when it replaces itself, so the new one can say
/// why it started.
const REPLACED_BECAUSE: &str = "MIDI_HARBOR_REPLACED_BECAUSE";

/// Names the environment variable carrying when the replaced daemon found the MIDI service gone.
const MIDI_SERVER_LOST_AT: &str = "MIDI_HARBOR_MIDI_SERVER_LOST_AT";

/// Opens the platform MIDI backend, waiting for it when this daemon replaced one that lost it.
///
/// The service that died is started again by the system, and may not answer the moment the new
/// process asks. A daemon started by hand fails at once, so a real misconfiguration is not
/// hidden behind a wait.
async fn open_midi(
    replacing: bool,
) -> Result<std::sync::Arc<dyn midi_harbor_platform::midi::MidiPlatform>, String> {
    // Thirty one-second attempts, the longest the server was seen taking to come back plus room.
    let attempts = if replacing { 30 } else { 1 };
    let mut last = String::new();
    for _ in 0..attempts {
        match midi_harbor_platform::midi_backend() {
            Ok(midi) => return Ok(midi),
            Err(error) => last = error.to_string(),
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    Err(last)
}

/// Replaces this process with a new daemon run the same way, and returns only if that fails.
///
/// A process that has lost the MIDI service cannot reach it again (R-079), but a new one can,
/// and everything it needs is in the configuration. Keeping the process identifier means launchd
/// and systemd see the same service throughout.
#[cfg(unix)]
fn replace_daemon(lost_at: Option<jiff::Timestamp>) -> ExitCode {
    use std::os::unix::process::CommandExt;

    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(error) => {
            eprintln!("could not find this program to restart it: {error}");
            return ExitCode::Failure;
        }
    };
    let mut command = std::process::Command::new(program);
    command
        .args(std::env::args_os().skip(1))
        .env(REPLACED_BECAUSE, "midi-server");
    if let Some(at) = lost_at {
        command.env(MIDI_SERVER_LOST_AT, at.to_string());
    }
    let error = command.exec();
    eprintln!("could not restart the daemon: {error}");
    ExitCode::Failure
}

/// Starts a new daemon run the same way, for this process to exit in favour of.
///
/// Windows cannot replace a running process's image, so the new daemon is a separate process.
/// Nothing on Windows asks for this yet; it is here so a backend that does has the same path.
#[cfg(windows)]
fn replace_daemon(lost_at: Option<jiff::Timestamp>) -> ExitCode {
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(error) => {
            eprintln!("could not find this program to restart it: {error}");
            return ExitCode::Failure;
        }
    };
    let mut command = std::process::Command::new(program);
    command
        .args(std::env::args_os().skip(1))
        .env(REPLACED_BECAUSE, "midi-server");
    if let Some(at) = lost_at {
        command.env(MIDI_SERVER_LOST_AT, at.to_string());
    }
    match command.spawn() {
        Ok(_) => ExitCode::Success,
        Err(error) => {
            eprintln!("could not restart the daemon: {error}");
            ExitCode::Failure
        }
    }
}

/// Opens the graphical interface, or explains its absence on a headless build.
///
/// `explicit` distinguishes `midi-harbor gui` from a bare invocation: asking for the interface by
/// name deserves an error, while running with no arguments deserves help.
#[cfg(feature = "gui")]
fn run_gui(_explicit: bool, socket: Option<std::path::PathBuf>) -> ExitCode {
    midi_harbor_platform::console::release_own();
    match midi_harbor_gui::run(socket) {
        Ok(()) => ExitCode::Success,
        Err(error) => {
            eprintln!("could not open the graphical interface: {error}");
            ExitCode::Failure
        }
    }
}

/// Explains that this build has no graphical interface.
#[cfg(not(feature = "gui"))]
fn run_gui(explicit: bool, _socket: Option<std::path::PathBuf>) -> ExitCode {
    use clap::CommandFactory;

    if explicit {
        eprintln!(
            "this build does not include the graphical interface; \
             install the full build, or use the commands below"
        );
    }

    let mut command = Cli::command();
    let _ = command.print_help();
    // Said once: asked for by name, the explanation above already covers it.
    if !explicit {
        println!();
        println!("this build does not include the graphical interface");
    }

    if explicit {
        ExitCode::Usage
    } else {
        ExitCode::Success
    }
}

/// Sets up logging at the verbosity the user asked for, to standard error or to a log file.
fn init_logging(verbose: u8, quiet: bool, log_file: Option<&std::path::Path>) {
    let level = match (quiet, verbose) {
        (true, _) => "error",
        (_, 0) => "info",
        (_, 1) => "debug",
        _ => "trace",
    };
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| format!("midi_harbor={level}"));

    // Logging is best effort: a subscriber that cannot install, or a log file that cannot open,
    // must not stop the daemon running.
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if let Some(path) = log_file {
        match RollingFile::open(path, LOG_CAP) {
            Ok(file) => {
                let _ = builder
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
                    .try_init();
                return;
            }
            Err(error) => {
                eprintln!(
                    "could not open the log file {}, logging here instead: {error}",
                    path.display()
                );
            }
        }
    }
    let _ = builder.with_writer(std::io::stderr).try_init();
}
