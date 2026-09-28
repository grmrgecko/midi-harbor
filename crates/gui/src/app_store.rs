//! The App Store mode (feature 014): owning the daemon the app bundles, and remembering whether
//! the window was open when Midi Harbor last quit.
//!
//! The sandboxed App Store build has no service manager, so the app runs its daemon itself: a
//! headless copy of the program beside its own executable, signed to inherit the app's sandbox
//! (research R-095). The window stays a client of that daemon over the contract, as it is of one
//! launchd runs.

use midi_harbor_service::supervisor::Supervisor;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{info, warn};

/// The helper's file name, beside the app's own executable (contracts/bundle.md).
pub const HELPER: &str = "midi-harbor-daemon";

/// How long a daemon asked to stop may take before it is killed. It needs about two seconds to
/// let open requests finish; a kill skips releasing held notes, so the wait is generous.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long starting waits for a new daemon to answer on its socket.
const START_WAIT: Duration = Duration::from_secs(30);

/// The daemon the app is responsible for.
pub enum DaemonOwner {
    /// The app started it and supervises it.
    Started(Supervisor),
    /// It was already serving, left by a window that crashed; the app stops it on Quit all the
    /// same, since in App Store mode nothing else would.
    Attached {
        /// Where it serves, which is how the app finds its process.
        socket: PathBuf,
    },
}

impl DaemonOwner {
    /// Uses the daemon serving `socket`, or starts `program` with `arguments` under supervision
    /// and waits for it to serve.
    ///
    /// A second daemon would open the same ports and sessions beside the first, so one that
    /// answers is always used rather than replaced.
    pub async fn start(
        program: &Path,
        arguments: &[OsString],
        socket: &Path,
    ) -> Result<Self, String> {
        if midi_harbor_ipc::transport::probe(socket).await {
            info!(socket = %socket.display(), "using the daemon already running");
            return Ok(Self::Attached {
                socket: socket.to_path_buf(),
            });
        }

        // Start one, and wait for it to serve.
        info!(program = %program.display(), "starting the daemon");
        let supervisor = Supervisor::start(program, arguments);
        let started = std::time::Instant::now();
        while started.elapsed() < START_WAIT {
            if midi_harbor_ipc::transport::probe(socket).await {
                return Ok(Self::Started(supervisor));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(format!(
            "the daemon did not start listening at {} within {} seconds",
            socket.display(),
            START_WAIT.as_secs()
        ))
    }

    /// Reports whether the app started this daemon, rather than finding it running.
    pub fn started(&self) -> bool {
        matches!(self, Self::Started(_))
    }

    /// Stops the daemon through its graceful shutdown, and returns once it has gone.
    ///
    /// One the app started is sent SIGTERM by its supervisor. One it found running was started by
    /// an earlier instance of the app, and the sandbox forbids signalling it, so it is asked over
    /// its socket (research R-095).
    pub async fn stop(self) {
        match self {
            Self::Started(supervisor) => {
                let _ = tokio::task::spawn_blocking(move || supervisor.stop(STOP_GRACE)).await;
            }
            Self::Attached { socket } => {
                info!(socket = %socket.display(), "stopping the daemon found running");
                let asked = match crate::client::Client::connect(Some(socket.clone())).await {
                    Ok(client) => client.stop_daemon().await,
                    Err(error) => Err(error),
                };
                if let Err(error) = asked {
                    warn!(error = %error, "could not ask the daemon to stop");
                    return;
                }
                let asked_at = std::time::Instant::now();
                while asked_at.elapsed() < STOP_GRACE {
                    if !midi_harbor_ipc::transport::probe(&socket).await {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                warn!("the daemon was asked to stop and is still answering");
            }
        }
    }
}

/// Returns the helper's path, beside this executable.
pub fn helper() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not find this program to start its daemon: {error}"))?;
    let directory = executable
        .parent()
        .ok_or_else(|| format!("{} has no directory", executable.display()))?;
    Ok(directory.join(HELPER))
}

/// What the app remembers between runs, in `window.yaml` beside the configuration
/// (data-model.md). The daemon owns `config.yaml` and rewrites it from what it holds, so this
/// lives apart from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowState {
    /// Whether the window was open when Midi Harbor last quit entirely.
    #[serde(default = "open_by_default")]
    pub open: bool,
    /// Whether Start at login has been offered, which happens once.
    #[serde(default)]
    pub login_offered: bool,
}

/// Returns what a missing field means: open, so the window shows rather than the app hiding.
fn open_by_default() -> bool {
    true
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            open: true,
            login_offered: false,
        }
    }
}

/// The file's name in the configuration directory.
const WINDOW_FILE: &str = "window.yaml";

impl WindowState {
    /// Reads the state kept in `config_dir`. A missing or unreadable file reads as open, so a
    /// first launch, or one after a crash, shows the window.
    pub fn load(config_dir: &Path) -> Self {
        std::fs::read_to_string(config_dir.join(WINDOW_FILE))
            .ok()
            .and_then(|text| serde_yaml_ng::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes the state to `config_dir` atomically: a temporary file, flushed, then renamed over
    /// the old one.
    pub fn save(&self, config_dir: &Path) -> Result<(), String> {
        let target = config_dir.join(WINDOW_FILE);
        let temporary = config_dir.join(format!("{WINDOW_FILE}.tmp-{}", std::process::id()));
        let encoded = serde_yaml_ng::to_string(self)
            .map_err(|error| format!("could not encode the window's state: {error}"))?;
        let written = std::fs::create_dir_all(config_dir)
            .and_then(|()| std::fs::File::create(&temporary))
            .and_then(|mut file| {
                file.write_all(encoded.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&temporary, &target));
        written.map_err(|error| format!("could not save {}: {error}", target.display()))
    }
}

/// What the window keeps about App Store mode while it runs.
pub struct StoreState {
    /// The daemon the app owns, until Quit takes it to stop it.
    pub owner: std::sync::Arc<std::sync::Mutex<Option<DaemonOwner>>>,
    /// The socket the app's daemon serves, where the window connects.
    pub socket: PathBuf,
    /// Why the daemon could not be started, when it could not.
    pub failed: Option<String>,
    /// Whether the window is showing, which is what is remembered at quit.
    pub visible: bool,
    /// Whether a quit is under way, so a second one waits for the first.
    pub quitting: bool,
    /// What the Start at login switch shows.
    pub login: midi_harbor_platform::appkit::login_item::LoginItem,
    /// Why the last change to Start at login failed.
    pub login_error: Option<String>,
    /// What is remembered between runs.
    pub remembered: WindowState,
    /// Where the remembered state is kept.
    config_dir: PathBuf,
}

impl StoreState {
    /// Sets up App Store mode when this process runs sandboxed, and returns nothing otherwise.
    ///
    /// The window starts hidden only when macOS started the app at login and the window was
    /// closed when Midi Harbor last quit; started any other way, it shows (FR-A10).
    pub fn begin(socket: Option<&PathBuf>) -> Option<Self> {
        if !midi_harbor_core::paths::sandboxed() {
            return None;
        }
        let paths = midi_harbor_core::paths::Paths::resolve()
            .ok()?
            .with_socket(socket.cloned());
        let remembered = WindowState::load(paths.config_dir());
        let visible = !midi_harbor_platform::appkit::launched_at_login() || remembered.open;
        Some(Self {
            owner: std::sync::Arc::new(std::sync::Mutex::new(None)),
            socket: paths.socket_file(),
            failed: None,
            visible,
            quitting: false,
            login: midi_harbor_platform::appkit::login_item::status(),
            login_error: None,
            remembered,
            config_dir: paths.config_dir().to_path_buf(),
        })
    }

    /// Reports whether Start at login is still to be offered, which happens once.
    pub fn offers_login(&self) -> bool {
        !self.remembered.login_offered
            && self.login == midi_harbor_platform::appkit::login_item::LoginItem::Off
    }

    /// Records that Start at login was offered, so it is not offered again.
    pub fn login_offered(&mut self) {
        self.remembered.login_offered = true;
        if let Err(error) = self.remembered.save(&self.config_dir) {
            warn!(error = %error, "could not remember that Start at login was offered");
        }
    }

    /// Records whether the window is open as Midi Harbor quits.
    pub fn remember_at_quit(&mut self) {
        self.remembered.open = self.visible;
        if let Err(error) = self.remembered.save(&self.config_dir) {
            warn!(error = %error, "could not remember whether the window was open");
        }
    }
}

/// Starts or finds the daemon, keeping it in `owner`.
pub async fn own_daemon(
    owner: std::sync::Arc<std::sync::Mutex<Option<DaemonOwner>>>,
    socket: PathBuf,
) -> Result<(), String> {
    let program = helper()?;
    let arguments: Vec<OsString> = vec![
        "--socket".into(),
        socket.clone().into_os_string(),
        "daemon".into(),
    ];
    let owned = DaemonOwner::start(&program, &arguments, &socket).await?;
    match owner.lock() {
        Ok(mut slot) => *slot = Some(owned),
        Err(poisoned) => *poisoned.into_inner() = Some(owned),
    }
    Ok(())
}

/// Stops the daemon in `owner`, if there is one.
pub async fn stop_daemon(owner: std::sync::Arc<std::sync::Mutex<Option<DaemonOwner>>>) {
    let owned = match owner.lock() {
        Ok(mut slot) => slot.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    };
    if let Some(owned) = owned {
        owned.stop().await;
    }
}
