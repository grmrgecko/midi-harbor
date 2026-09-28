//! The macOS backend: a launchd user agent.

use crate::{
    SERVICE_LABEL, ServiceError, ServiceManager, ServiceSpec, ServiceStatus, is_stale, run,
    write_definition,
};
use std::path::{Path, PathBuf};

/// Escapes text for inclusion in a plist string element.
///
/// Paths can contain ampersands and angle brackets, and an unescaped one produces a plist launchd
/// silently refuses to load.
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Renders the launch agent property list for a specification.
///
/// `KeepAlive` is what restarts the daemon after a crash, and `RunAtLoad` is what starts it at
/// login. Both are required by the resilience rules: a daemon that stays dead after a crash would
/// take every connection with it.
pub fn render_plist(spec: &ServiceSpec) -> String {
    let mut arguments = String::new();
    arguments.push_str(&format!(
        "\t\t<string>{}</string>\n",
        escape_xml(&spec.executable.display().to_string())
    ));
    for argument in &spec.arguments {
        arguments.push_str(&format!("\t\t<string>{}</string>\n", escape_xml(argument)));
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n{arguments}\t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<{run_at_load}/>\n\
         \t<key>KeepAlive</key>\n\
         \t<true/>\n\
         \t<key>ProcessType</key>\n\
         \t<string>Interactive</string>\n\
         </dict>\n\
         </plist>\n",
        label = SERVICE_LABEL,
        arguments = arguments,
        run_at_load = if spec.start_at_login { "true" } else { "false" },
    )
}

/// Returns the specification with the daemon told to write its log to `log_file`.
///
/// launchd discards a daemon's output unless the agent names a file, and never trims one it
/// does, so the daemon keeps its own log and rolls it over.
pub fn with_log_file(spec: &ServiceSpec, log_file: &Path) -> ServiceSpec {
    let mut spec = spec.clone();
    spec.arguments.push("--log-file".to_owned());
    spec.arguments.push(log_file.display().to_string());
    spec
}

/// Reads the executable path out of a rendered plist.
///
/// Used to detect a registration pointing at a binary that has since moved.
pub fn registered_executable(plist: &str) -> Option<PathBuf> {
    let array = plist.split("<array>").nth(1)?;
    let first = array.split("<string>").nth(1)?;
    let path = first.split("</string>").next()?;
    Some(PathBuf::from(unescape_xml(path)))
}

/// Reverses `escape_xml`, the ampersand last so an escaped entity is not unescaped twice.
fn unescape_xml(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Manages the daemon as a launchd user agent.
pub struct Launchd {
    plist_path: PathBuf,
    log_file: PathBuf,
    domain: String,
}

impl Launchd {
    /// Resolves the agent's location for the current user.
    pub fn new() -> Result<Self, ServiceError> {
        let home = directories::BaseDirs::new().ok_or(ServiceError::NoHome)?;
        let plist_path = home
            .home_dir()
            .join("Library/LaunchAgents")
            .join(format!("{SERVICE_LABEL}.plist"));
        let log_file = home.home_dir().join("Library/Logs/midi-harbor/daemon.log");
        // Modern launchctl addresses services by domain rather than by file.
        let domain = format!("gui/{}", rustix::process::getuid().as_raw());
        Ok(Self {
            plist_path,
            log_file,
            domain,
        })
    }

    /// Returns the service target used by launchctl subcommands.
    fn target(&self) -> String {
        format!("{}/{}", self.domain, SERVICE_LABEL)
    }

    /// Removes any existing registration, ignoring the error when none exists.
    fn bootout(&self) {
        let _ = run("launchctl", &["bootout", &self.target()]);
    }
}

impl ServiceManager for Launchd {
    fn install(&self, spec: &ServiceSpec) -> Result<PathBuf, ServiceError> {
        // Remove first so re-running install updates the registration in place rather than
        // leaving launchd holding a definition we just overwrote.
        self.bootout();
        let spec = with_log_file(spec, &self.log_file);
        write_definition(&self.plist_path, &render_plist(&spec))?;

        let plist = self.plist_path.display().to_string();
        run("launchctl", &["bootstrap", &self.domain, &plist])?;
        Ok(self.plist_path.clone())
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        self.bootout();
        match std::fs::remove_file(&self.plist_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(ServiceError::Io {
                operation: "remove",
                path: self.plist_path.clone(),
                source,
            }),
        }
    }

    fn start(&self) -> Result<(), ServiceError> {
        run("launchctl", &["kickstart", &self.target()]).map(|_| ())
    }

    fn stop(&self) -> Result<(), ServiceError> {
        run("launchctl", &["kill", "SIGTERM", &self.target()]).map(|_| ())
    }

    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let Ok(plist) = std::fs::read_to_string(&self.plist_path) else {
            return Ok(ServiceStatus::default());
        };
        let registered = registered_executable(&plist);

        // launchctl print exits non-zero when the service is not loaded, which is a normal answer
        // rather than a failure.
        let running = run("launchctl", &["print", &self.target()])
            .map(|output| output.contains("state = running"))
            .unwrap_or(false);

        Ok(ServiceStatus {
            installed: true,
            running,
            definition_path: Some(self.plist_path.clone()),
            stale: is_stale(registered.as_deref()),
            registered_executable: registered,
        })
    }

    fn name(&self) -> &'static str {
        "launchd"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the specification `service install` writes for an application bundle.
    fn spec() -> ServiceSpec {
        ServiceSpec {
            executable: PathBuf::from("/Applications/Midi Harbor.app/Contents/MacOS/midi-harbor"),
            arguments: vec!["daemon".to_owned()],
            start_at_login: true,
        }
    }

    /// Locks the launchctl service target to `gui/<uid>/<label>`, with the uid `id -u` prints.
    ///
    /// launchctl's `bootstrap`, `kickstart`, `kill` and `print` find a per-user agent only in the
    /// user's GUI domain; the integration test in `tests/service_lifecycle.rs` runs against a
    /// stand-in launchctl that ignores the target, so only this pins it.
    #[test]
    fn the_agent_is_addressed_in_this_users_gui_domain() {
        let printed = std::process::Command::new("id")
            .arg("-u")
            .output()
            .expect("id must run to report this user's uid");
        let uid = String::from_utf8(printed.stdout).expect("id must print its uid as UTF-8");
        let agent = Launchd::new().expect("the test user must have a home directory");
        assert_eq!(
            agent.target(),
            format!("gui/{}/{SERVICE_LABEL}", uid.trim()),
            "launchctl finds a per-user agent only under gui/<uid>"
        );
    }

    /// Locks the plist keys launchd acts on: `KeepAlive` true in every case, so launchd restarts a
    /// crashed daemon, and `RunAtLoad` following `start_at_login`, so the agent starts at login
    /// only when asked to.
    ///
    /// The keys and their `<true/>` and `<false/>` values are as launchd.plist(5) defines them.
    #[test]
    fn the_agent_restarts_after_a_crash_and_starts_at_login_only_when_asked() {
        let cases = [
            ("start at login", true, "<key>RunAtLoad</key>\n\t<true/>"),
            ("start by hand", false, "<key>RunAtLoad</key>\n\t<false/>"),
        ];
        for (name, start_at_login, run_at_load) in cases {
            let plist = render_plist(&ServiceSpec {
                start_at_login,
                ..spec()
            });
            assert!(
                plist.contains(&format!(
                    "<key>Label</key>\n\t<string>{SERVICE_LABEL}</string>"
                )),
                "{name}: launchd addresses the agent by its label"
            );
            assert!(
                plist.contains("<key>KeepAlive</key>\n\t<true/>"),
                "{name}: a daemon that stays dead after a crash takes every connection with it"
            );
            assert!(
                plist.contains(run_at_load),
                "{name}: RunAtLoad must follow the start-at-login setting"
            );
        }
    }

    /// Locks `ProgramArguments` as launchd passes it to the daemon: the executable, the `daemon`
    /// subcommand, then `--log-file` and the log's path.
    ///
    /// launchd discards a program's output unless the plist names a file, and never trims one it
    /// does, so the daemon is told where to keep and roll its own log.
    #[test]
    fn the_daemon_runs_as_its_subcommand_and_is_told_where_to_log() {
        let log = Path::new("/Users/me/Library/Logs/midi-harbor/daemon.log");
        let plist = render_plist(&with_log_file(&spec(), log));
        assert!(
            plist.contains(
                "<key>ProgramArguments</key>\n\t<array>\n\
                 \t\t<string>/Applications/Midi Harbor.app/Contents/MacOS/midi-harbor</string>\n\
                 \t\t<string>daemon</string>\n\
                 \t\t<string>--log-file</string>\n\
                 \t\t<string>/Users/me/Library/Logs/midi-harbor/daemon.log</string>\n\
                 \t</array>"
            ),
            "the program arguments must be the executable, the subcommand and the log file, in order: {plist}"
        );
    }

    /// Locks that a path holding XML's special characters reaches launchd unchanged, read back
    /// through `plutil`, which parses property lists with the same CoreFoundation parser
    /// launchd uses, and that `service status` reads the same path back from the file.
    ///
    /// An unescaped ampersand or angle bracket makes a plist launchd silently refuses to load, so
    /// the agent would never start and nothing would say why. A path read back still escaped
    /// made `service status` call a current registration stale.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_path_needing_escaping_reaches_launchd_unchanged() {
        use std::io::Write;

        let cases = [
            ("an ampersand", "/Users/a&b/midi-harbor"),
            ("angle brackets", "/Users/me/Midi <Harbor>/midi-harbor"),
            ("quotes", "/Users/me/\"Midi\" 'Harbor'/midi-harbor"),
        ];
        for (name, path) in cases {
            let plist = render_plist(&ServiceSpec {
                executable: PathBuf::from(path),
                ..spec()
            });
            let mut plutil = std::process::Command::new("plutil")
                .args(["-extract", "ProgramArguments.0", "raw", "-o", "-", "-"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("plutil ships with macOS and must run");
            plutil
                .stdin
                .take()
                .expect("plutil's input must be piped")
                .write_all(plist.as_bytes())
                .expect("plutil must accept the plist on its input");
            let read = plutil
                .wait_with_output()
                .expect("plutil must finish reading the plist");
            assert!(
                read.status.success(),
                "{name}: plutil refused the plist, as launchd would: {}",
                String::from_utf8_lossy(&read.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&read.stdout).trim_end_matches('\n'),
                path,
                "{name}: launchd must be given the executable's path unchanged"
            );
            assert_eq!(
                registered_executable(&plist),
                Some(PathBuf::from(path)),
                "{name}: the registered path must read back as it was written"
            );
        }
    }
}
