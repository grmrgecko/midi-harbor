//! The Linux backend: a systemd user unit.

use crate::{
    SYSTEMD_UNIT, ServiceError, ServiceManager, ServiceSpec, ServiceStatus, is_stale, run,
    write_definition,
};
use std::path::PathBuf;

/// Quotes a value for a systemd unit directive.
///
/// Paths containing spaces need quoting or systemd reads them as several arguments.
fn quote(value: &str) -> String {
    if value.contains(' ') || value.contains('"') {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

/// Renders the systemd user unit for a specification.
///
/// `Restart=always` is what brings the daemon back after a crash, and `WantedBy=default.target`
/// is what starts it at login. Both are required by the resilience rules.
///
/// `ExecStop` stops the daemon alone and waits for it before systemd signals the rest of the
/// unit. Run from an AppImage, the rest is the runtime serving the daemon's own executable, and
/// signalled together the mount goes while the daemon is still releasing its notes (R-104).
pub fn render_unit(spec: &ServiceSpec) -> String {
    let mut command = quote(&spec.executable.display().to_string());
    for argument in &spec.arguments {
        command.push(' ');
        command.push_str(&quote(argument));
    }

    let install = if spec.start_at_login {
        "\n[Install]\nWantedBy=default.target\n"
    } else {
        "\n"
    };

    format!(
        "[Unit]\n\
         Description=Midi Harbor MIDI connectivity daemon\n\
         Documentation=https://github.com/grmrgecko/midi-harbor\n\
         After=network.target sound.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={command}\n\
         ExecStop=/bin/sh -c 'kill -TERM $MAINPID && while kill -0 $MAINPID 2>/dev/null; do sleep 0.1; done'\n\
         Restart=always\n\
         RestartSec=2\n\
         \n{install}"
    )
}

/// Reads the executable path out of a rendered unit.
pub fn registered_executable(unit: &str) -> Option<PathBuf> {
    let line = unit.lines().find(|line| line.starts_with("ExecStart="))?;
    let command = line.strip_prefix("ExecStart=")?;

    // The executable is the first argument, which may be quoted if it contains spaces.
    if let Some(rest) = command.strip_prefix('"') {
        let path = rest.split('"').next()?;
        return Some(PathBuf::from(
            path.replace("\\\"", "\"").replace("\\\\", "\\"),
        ));
    }
    Some(PathBuf::from(command.split_whitespace().next()?))
}

/// Reports whether this system has a usable systemd user session.
///
/// A distribution without systemd, or a container with no user session bus, is a supported
/// situation: the daemon simply runs in the foreground instead.
pub fn is_available() -> bool {
    run("systemctl", &["--user", "show-environment"]).is_ok()
}

/// Manages the daemon as a systemd user unit.
pub struct Systemd {
    unit_path: PathBuf,
}

impl Systemd {
    /// Resolves the unit's location for the current user.
    pub fn new() -> Result<Self, ServiceError> {
        let base = directories::BaseDirs::new().ok_or(ServiceError::NoHome)?;
        let unit_path = base.config_dir().join("systemd/user").join(SYSTEMD_UNIT);
        Ok(Self { unit_path })
    }
}

impl ServiceManager for Systemd {
    fn install(&self, spec: &ServiceSpec) -> Result<PathBuf, ServiceError> {
        write_definition(&self.unit_path, &render_unit(spec))?;

        // systemd caches unit files, so an updated definition is invisible until a reload.
        run("systemctl", &["--user", "daemon-reload"])?;
        if spec.start_at_login {
            run("systemctl", &["--user", "enable", SYSTEMD_UNIT])?;
        }
        Ok(self.unit_path.clone())
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        // Disabling a unit that is not enabled is not an error worth surfacing.
        let _ = run("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]);
        match std::fs::remove_file(&self.unit_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ServiceError::Io {
                    operation: "remove",
                    path: self.unit_path.clone(),
                    source,
                });
            }
        }
        let _ = run("systemctl", &["--user", "daemon-reload"]);
        Ok(())
    }

    fn start(&self) -> Result<(), ServiceError> {
        run("systemctl", &["--user", "start", SYSTEMD_UNIT]).map(|_| ())
    }

    fn stop(&self) -> Result<(), ServiceError> {
        run("systemctl", &["--user", "stop", SYSTEMD_UNIT]).map(|_| ())
    }

    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let Ok(unit) = std::fs::read_to_string(&self.unit_path) else {
            return Ok(ServiceStatus::default());
        };
        let registered = registered_executable(&unit);

        // is-active exits non-zero when inactive, which is an answer rather than a failure.
        let running = run("systemctl", &["--user", "is-active", SYSTEMD_UNIT])
            .map(|output| output.trim() == "active")
            .unwrap_or(false);

        Ok(ServiceStatus {
            installed: true,
            running,
            definition_path: Some(self.unit_path.clone()),
            stale: is_stale(registered.as_deref()),
            registered_executable: registered,
        })
    }

    fn name(&self) -> &'static str {
        "systemd"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the unit directives systemd acts on: `Restart=always` in every case, so a crashed
    /// daemon comes back; `After=network.target sound.target`, so it starts once ALSA is up; an
    /// `ExecStop` that signals the main process alone and waits for it; and an `[Install]` section
    /// with `WantedBy=default.target` only when it should start at login.
    ///
    /// The directives are as systemd.service(5) and systemd.unit(5) define them. Started before
    /// ALSA is up, every endpoint would fail its first attempt. Without the `ExecStop`, systemd
    /// signals every process in the unit at once, and a daemon run from an AppImage died of
    /// SIGBUS as it stopped, its executable unmounted under it (R-104).
    #[test]
    fn the_unit_restarts_after_a_crash_and_starts_at_login_only_when_asked() {
        let cases = [
            ("start at login", true, true),
            ("start by hand", false, false),
        ];
        for (name, start_at_login, want_install) in cases {
            let unit = render_unit(&ServiceSpec {
                executable: PathBuf::from("/usr/local/bin/midi-harbor"),
                arguments: vec!["daemon".to_owned()],
                start_at_login,
            });
            assert!(
                unit.contains("\nRestart=always\n"),
                "{name}: a daemon that stays dead after a crash takes every connection with it"
            );
            assert!(
                unit.contains("\nAfter=network.target sound.target\n"),
                "{name}: the daemon must wait for the network and the sound stack"
            );
            assert!(
                unit.contains(
                    "\nExecStop=/bin/sh -c 'kill -TERM $MAINPID && while kill -0 $MAINPID \
                     2>/dev/null; do sleep 0.1; done'\n"
                ),
                "{name}: the daemon must be stopped and waited for before the rest of the unit"
            );
            assert_eq!(
                unit.contains("\n[Install]\nWantedBy=default.target\n"),
                want_install,
                "{name}: the install section must follow the start-at-login setting"
            );
        }
    }

    /// Locks `ExecStart` quoting and reading the executable back from it: a path with a space is
    /// double-quoted, as systemd.service(5) requires to keep it one argument, and a plain path is
    /// left bare.
    ///
    /// Reading it back is how `service status` finds a registration whose binary has moved.
    #[test]
    fn exec_start_keeps_the_executable_one_argument_and_reads_back() {
        let cases = [
            (
                "a plain path",
                "/usr/local/bin/midi-harbor",
                "ExecStart=/usr/local/bin/midi-harbor daemon\n",
            ),
            (
                "a path with a space",
                "/opt/Midi Harbor/midi-harbor",
                "ExecStart=\"/opt/Midi Harbor/midi-harbor\" daemon\n",
            ),
        ];
        for (name, path, want_line) in cases {
            let unit = render_unit(&ServiceSpec {
                executable: PathBuf::from(path),
                arguments: vec!["daemon".to_owned()],
                start_at_login: true,
            });
            assert!(
                unit.contains(want_line),
                "{name}: systemd must read the executable as one argument: {unit}"
            );
            assert_eq!(
                registered_executable(&unit),
                Some(PathBuf::from(path)),
                "{name}: the registered executable must read back as it was written"
            );
        }
    }
}
