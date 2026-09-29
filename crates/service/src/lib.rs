//! Installing Midi Harbor as a per-user service that starts at login.
//!
//! The daemon must outlive any window and come back after a crash, which is the operating
//! system's job rather than ours where the system will do it. This crate is the seam between that
//! intent and the three service managers that implement it: launchd, systemd, and Task Scheduler,
//! which starts the daemon but leaves restarting it to `supervisor`.
//!
//! Generating the service definition is kept separate from invoking the service manager, so the
//! exact plist, unit file and task definition contents are testable without a launchd, systemd
//! or Task Scheduler session.

#[cfg(unix)]
pub mod launchd;
pub mod supervisor;
pub mod systemd;
pub mod taskscheduler;

use std::path::{Path, PathBuf};

/// Label and unit name for the daemon on macOS and Linux.
pub const SERVICE_LABEL: &str = "com.mrgeckosmedia.MidiHarbor.daemon";

/// File name of the systemd user unit.
pub const SYSTEMD_UNIT: &str = "midi-harbor.service";

/// Why a service operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// A file could not be read or written.
    #[error("could not {operation} {path}: {source}")]
    Io {
        /// What was attempted.
        operation: &'static str,
        /// Which file it concerned.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The service manager reported a failure.
    #[error("{command} failed: {detail}")]
    Command {
        /// Which command was run.
        command: String,
        /// What it reported.
        detail: String,
    },
    /// The service manager could not be run at all.
    #[error("could not run {command}: {source}")]
    Unrunnable {
        /// Which command could not be started.
        command: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// No supported service manager is present.
    ///
    /// Reported rather than worked around, because the honest answer is to run the daemon in the
    /// foreground under whatever supervisor the user already has.
    #[error(
        "no supported user service manager is available on this system; \
         run 'midi-harbor daemon' under your own supervisor instead"
    )]
    NoServiceManager,
    /// The user's home directory could not be determined.
    #[error("could not determine the user's home directory")]
    NoHome,
    /// This is the sandboxed App Store build, which cannot register a launchd agent and is started
    /// by the app itself.
    #[error(
        "the App Store build is started by Midi Harbor itself; \
         turn on \"Start at login\" in its settings"
    )]
    AppStore,
}

/// What to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    /// The executable to run.
    ///
    /// Stored as an absolute path, so moving the binary is detectable as a stale registration
    /// rather than a service that silently stops working.
    pub executable: PathBuf,
    /// Arguments passed to it.
    pub arguments: Vec<String>,
    /// Whether to start it at login.
    pub start_at_login: bool,
}

impl ServiceSpec {
    /// Builds a specification running the current executable as the daemon.
    ///
    /// From an AppImage it runs the AppImage file, since the running executable is inside a
    /// mount that is gone once this process exits.
    pub fn for_current_executable() -> Result<Self, ServiceError> {
        let running = std::env::current_exe().map_err(|source| ServiceError::Io {
            operation: "locate",
            path: PathBuf::from("the running executable"),
            source,
        })?;
        // AppImages exist only on Linux.
        #[cfg(target_os = "linux")]
        let executable = appimage_file(
            &running,
            std::env::var_os("APPIMAGE"),
            std::env::var_os("APPDIR"),
        )
        .unwrap_or(running);
        #[cfg(not(target_os = "linux"))]
        let executable = running;
        Ok(Self {
            executable,
            arguments: vec!["daemon".to_owned()],
            start_at_login: true,
        })
    }
}

/// What a service manager reports about the installed service.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServiceStatus {
    /// Whether a definition exists.
    pub installed: bool,
    /// Whether it is running now.
    pub running: bool,
    /// Where the definition lives.
    pub definition_path: Option<PathBuf>,
    /// The executable the definition points at.
    pub registered_executable: Option<PathBuf>,
    /// Set when the registration points at an executable that is no longer there.
    ///
    /// The service manager reports this as simply not running, which sends a user looking in the
    /// wrong place, so it is detected and named explicitly.
    pub stale: bool,
}

impl ServiceStatus {
    /// Describes the service's state in one line.
    pub fn describe(&self) -> String {
        match (self.installed, self.running, self.stale) {
            (false, _, _) => "not installed".to_owned(),
            (true, _, true) => match &self.registered_executable {
                Some(path) => format!("installed but stale: {} is missing", path.display()),
                None => "installed but stale: the registered executable is missing".to_owned(),
            },
            (true, true, false) => "installed and running".to_owned(),
            (true, false, false) => "installed but not running".to_owned(),
        }
    }
}

/// Installs and controls the daemon as a per-user service.
///
/// Every operation is per-user and needs no elevation. Installing when a registration already
/// exists updates it in place rather than creating a second one.
pub trait ServiceManager: Send + Sync {
    /// Writes the service definition and registers it, replacing any existing registration.
    fn install(&self, spec: &ServiceSpec) -> Result<PathBuf, ServiceError>;

    /// Deregisters the service, leaving the user's configuration untouched.
    fn uninstall(&self) -> Result<(), ServiceError>;

    /// Starts the service now.
    fn start(&self) -> Result<(), ServiceError>;

    /// Stops the service now.
    fn stop(&self) -> Result<(), ServiceError>;

    /// Reports whether the service is installed, running, and whether its registration is stale.
    fn status(&self) -> Result<ServiceStatus, ServiceError>;

    /// Names the service manager, for messages.
    fn name(&self) -> &'static str;
}

/// Returns the service manager for this platform.
pub fn detect() -> Result<Box<dyn ServiceManager>, ServiceError> {
    #[cfg(target_os = "macos")]
    {
        // The sandbox refuses launchctl and would put the agent's plist inside the container,
        // where launchd never looks.
        if midi_harbor_core::paths::sandboxed() {
            return Err(ServiceError::AppStore);
        }
        Ok(Box::new(launchd::Launchd::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        if systemd::is_available() {
            Ok(Box::new(systemd::Systemd::new()?))
        } else {
            Err(ServiceError::NoServiceManager)
        }
    }
    #[cfg(windows)]
    {
        Ok(Box::new(taskscheduler::TaskScheduler::new()?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        Err(ServiceError::NoServiceManager)
    }
}

/// Returns the AppImage file the running executable was started from, if it was.
///
/// The AppImage runtime sets `APPIMAGE` to the file and `APPDIR` to where it mounted it, and
/// every process started from the application inherits both. So the executable must be inside
/// `APPDIR`, or the variables belong to another AppImage that started this program.
#[cfg(target_os = "linux")]
fn appimage_file(
    running: &Path,
    appimage: Option<std::ffi::OsString>,
    appdir: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let appdir = PathBuf::from(appdir?);
    if appdir.as_os_str().is_empty() || !running.starts_with(&appdir) {
        return None;
    }
    let appimage = PathBuf::from(appimage?);
    appimage.is_absolute().then_some(appimage)
}

/// Reports whether the executable a registration points at still exists.
pub(crate) fn is_stale(registered: Option<&Path>) -> bool {
    registered.is_some_and(|path| !path.exists())
}

/// Writes a service definition, creating its directory.
pub(crate) fn write_definition(path: &Path, contents: &str) -> Result<(), ServiceError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ServiceError::Io {
            operation: "create",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, contents).map_err(|source| ServiceError::Io {
        operation: "write",
        path: path.to_path_buf(),
        source,
    })
}

/// Runs a service manager command, turning a non-zero exit into a reportable failure.
pub(crate) fn run(command: &str, args: &[&str]) -> Result<String, ServiceError> {
    let output = std::process::Command::new(command)
        .args(args)
        .output()
        .map_err(|source| ServiceError::Unrunnable {
            command: command.to_owned(),
            source,
        })?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(ServiceError::Command {
        command: format!("{command} {}", args.join(" ")),
        detail: if detail.is_empty() {
            "no detail reported".to_owned()
        } else {
            detail
        },
    })
}

// AppImages exist only on Linux, and these paths are not absolute on Windows.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Locks which executable a service registration names when the program runs from an
    /// AppImage.
    ///
    /// The AppImage runtime (AppImage/type2-runtime, runtime.c) mounts the image at
    /// `/tmp/.mount_<name><random>`, sets `APPDIR` to that mount and `APPIMAGE` to the image
    /// file, and replaces itself with the application. The mount is gone once the application
    /// exits, so a unit naming the executable inside it fails at the next login. Children inherit
    /// both variables, so a program started from another AppImage, such as a terminal, sees that
    /// AppImage's values while running from somewhere else entirely.
    #[test]
    fn a_registration_names_the_appimage_file_only_when_running_from_its_mount() {
        let mount = "/tmp/.mount_MidiHaAbC123";
        let inside = "/tmp/.mount_MidiHaAbC123/usr/bin/midi-harbor";
        let cases = [
            (
                "running from the AppImage's mount",
                inside,
                Some("/home/user/Apps/Midi-Harbor-0.1.0-x86_64.AppImage"),
                Some(mount),
                Some("/home/user/Apps/Midi-Harbor-0.1.0-x86_64.AppImage"),
            ),
            (
                "installed binary started from another AppImage's terminal",
                "/usr/bin/midi-harbor",
                Some("/home/user/Apps/Terminal.AppImage"),
                Some("/tmp/.mount_TerminXyZ789"),
                None,
            ),
            (
                "installed binary with no AppImage involved",
                "/usr/bin/midi-harbor",
                None,
                None,
                None,
            ),
            (
                "an empty APPDIR, which every path starts with",
                inside,
                Some("/home/user/Apps/Midi-Harbor-0.1.0-x86_64.AppImage"),
                Some(""),
                None,
            ),
        ];
        for (name, running, appimage, appdir, want) in cases {
            assert_eq!(
                appimage_file(
                    Path::new(running),
                    appimage.map(Into::into),
                    appdir.map(Into::into),
                ),
                want.map(PathBuf::from),
                "{name}: the registration must name a file that outlives this process"
            );
        }
    }
}
