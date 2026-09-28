//! The Windows backend: a Task Scheduler task that starts the daemon at logon.
//!
//! A Windows service would run in session 0, away from the user's MIDI devices and teVirtualMIDI
//! ports, and installing one needs an administrator. A task in the user's own name needs neither.
//! Task Scheduler does not restart a program that fails, so the task runs the daemon under its
//! own supervisor (see `supervisor`), and it runs it through a headless console host so no
//! window opens at logon.

use crate::ServiceSpec;
#[cfg(windows)]
use crate::{ServiceError, ServiceManager, ServiceStatus, is_stale, run};
use std::path::{Path, PathBuf};

/// The task's name in Task Scheduler's library.
pub const TASK_NAME: &str = "Midi Harbor";

/// The flag that runs the daemon under its supervisor.
pub const SUPERVISE_FLAG: &str = "--supervise";

/// Escapes text for inclusion in a task definition's XML.
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Undoes `escape_xml`.
fn unescape_xml(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Quotes one argument the way the C runtime's command-line parser reads it back.
///
/// Backslashes are literal except before a quote, so a run of them before a quote, or before the
/// closing quote, is doubled. A path ending in a backslash would otherwise escape its own closing
/// quote.
pub fn quote_argument(argument: &str) -> String {
    let needs_quotes =
        argument.is_empty() || argument.chars().any(|c| c == ' ' || c == '\t' || c == '"');
    if !needs_quotes {
        return argument.to_owned();
    }
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(other);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

/// Reads the first argument back out of a command line written by `quote_argument`.
///
/// Halving a run of backslashes rounds down on purpose: before a quote, the odd one left over is
/// what escapes it.
#[allow(clippy::integer_division)]
fn first_argument(command_line: &str) -> Option<String> {
    let line = command_line.trim_start();
    let Some(rest) = line.strip_prefix('"') else {
        return line.split_whitespace().next().map(str::to_owned);
    };
    let mut argument = String::new();
    let mut backslashes = 0usize;
    for character in rest.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' if backslashes % 2 == 1 => {
                argument.extend(std::iter::repeat_n('\\', backslashes / 2));
                argument.push('"');
                backslashes = 0;
            }
            '"' => {
                argument.extend(std::iter::repeat_n('\\', backslashes / 2));
                return Some(argument);
            }
            other => {
                argument.extend(std::iter::repeat_n('\\', backslashes));
                argument.push(other);
                backslashes = 0;
            }
        }
    }
    None
}

/// Returns the specification with the daemon run under its supervisor and told where to log.
///
/// Task Scheduler discards a program's output, so the daemon keeps its own log and rolls it over.
pub fn supervised(spec: &ServiceSpec, log_file: &Path) -> ServiceSpec {
    let mut spec = spec.clone();
    spec.arguments.push(SUPERVISE_FLAG.to_owned());
    spec.arguments.push("--log-file".to_owned());
    spec.arguments.push(log_file.display().to_string());
    spec
}

/// Renders the task definition for a specification, running as `user`.
///
/// `console_host` is the system's `conhost.exe`. Running the daemon through it with
/// `--headless` gives the daemon the console it expects without showing a window. The priority
/// is Task Scheduler's normal one, 4; its default, 7, runs a task below normal priority, which is
/// wrong for a process carrying MIDI.
pub fn render_task(spec: &ServiceSpec, user: &str, console_host: &Path) -> String {
    let mut arguments = format!(
        "--headless {}",
        quote_argument(&spec.executable.display().to_string())
    );
    for argument in &spec.arguments {
        arguments.push(' ');
        arguments.push_str(&quote_argument(argument));
    }
    let user = escape_xml(user);
    let triggers = if spec.start_at_login {
        format!(
            "    <LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>{user}</UserId>\n    </LogonTrigger>\n"
        )
    } else {
        String::new()
    };

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
         \x20 <RegistrationInfo>\n\
         \x20   <Description>Midi Harbor MIDI connectivity daemon</Description>\n\
         \x20 </RegistrationInfo>\n\
         \x20 <Triggers>\n{triggers}  </Triggers>\n\
         \x20 <Principals>\n\
         \x20   <Principal id=\"Author\">\n\
         \x20     <UserId>{user}</UserId>\n\
         \x20     <LogonType>InteractiveToken</LogonType>\n\
         \x20     <RunLevel>LeastPrivilege</RunLevel>\n\
         \x20   </Principal>\n\
         \x20 </Principals>\n\
         \x20 <Settings>\n\
         \x20   <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
         \x20   <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
         \x20   <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
         \x20   <AllowHardTerminate>true</AllowHardTerminate>\n\
         \x20   <StartWhenAvailable>false</StartWhenAvailable>\n\
         \x20   <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\n\
         \x20   <IdleSettings>\n\
         \x20     <StopOnIdleEnd>false</StopOnIdleEnd>\n\
         \x20     <RestartOnIdle>false</RestartOnIdle>\n\
         \x20   </IdleSettings>\n\
         \x20   <AllowStartOnDemand>true</AllowStartOnDemand>\n\
         \x20   <Enabled>true</Enabled>\n\
         \x20   <Hidden>false</Hidden>\n\
         \x20   <RunOnlyIfIdle>false</RunOnlyIfIdle>\n\
         \x20   <WakeToRun>false</WakeToRun>\n\
         \x20   <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n\
         \x20   <Priority>4</Priority>\n\
         \x20 </Settings>\n\
         \x20 <Actions Context=\"Author\">\n\
         \x20   <Exec>\n\
         \x20     <Command>{command}</Command>\n\
         \x20     <Arguments>{arguments}</Arguments>\n\
         \x20   </Exec>\n\
         \x20 </Actions>\n\
         </Task>\n",
        command = escape_xml(&console_host.display().to_string()),
        arguments = escape_xml(&arguments),
    )
}

/// Reads the daemon's executable path out of a task definition.
///
/// The task's command is the console host, so the daemon is the first argument after
/// `--headless`.
pub fn registered_executable(definition: &str) -> Option<PathBuf> {
    let arguments = definition
        .split("<Arguments>")
        .nth(1)?
        .split("</Arguments>")
        .next()?;
    let arguments = unescape_xml(arguments);
    let rest = arguments.trim_start().strip_prefix("--headless")?;
    first_argument(rest).map(PathBuf::from)
}

/// Encodes a task definition the way Task Scheduler reads a file: UTF-16 with a byte order mark,
/// as its own XML declaration says.
pub fn encode_definition(definition: &str) -> Vec<u8> {
    std::iter::once(0xFEFF_u16)
        .chain(definition.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// Reports a status from what Task Scheduler and the daemon's pipe say.
#[cfg(windows)]
fn status_from(definition: Option<&str>, definition_path: &Path, running: bool) -> ServiceStatus {
    let Some(definition) = definition else {
        return ServiceStatus::default();
    };
    let registered = registered_executable(definition);
    ServiceStatus {
        installed: true,
        running,
        definition_path: Some(definition_path.to_path_buf()),
        stale: is_stale(registered.as_deref()),
        registered_executable: registered,
    }
}

/// How long `stop` waits for the daemon to finish after asking it to.
#[cfg(windows)]
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Manages the daemon as a Task Scheduler task.
#[cfg(windows)]
pub struct TaskScheduler {
    definition_path: PathBuf,
    log_file: PathBuf,
    socket: PathBuf,
}

#[cfg(windows)]
impl TaskScheduler {
    /// Resolves where the task's definition and the daemon's log live for the current user.
    ///
    /// Both go in the local, not the roaming, application data directory: the task names an
    /// executable on this machine.
    pub fn new() -> Result<Self, ServiceError> {
        let base = directories::BaseDirs::new().ok_or(ServiceError::NoHome)?;
        let local = base.data_local_dir().join(midi_harbor_core::paths::APP_DIR);
        let socket = midi_harbor_core::paths::Paths::resolve()
            .map_err(|_| ServiceError::NoHome)?
            .socket_file();
        Ok(Self {
            definition_path: local.join("midi-harbor-task.xml"),
            log_file: local.join("logs").join("daemon.log"),
            socket,
        })
    }

    /// Returns the user the task runs as, in the form Task Scheduler accepts.
    fn user() -> Result<String, ServiceError> {
        Ok(run("whoami", &[])?.trim().to_owned())
    }

    /// Returns the system's console host.
    fn console_host() -> PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        PathBuf::from(root).join("System32").join("conhost.exe")
    }

    /// Asks the daemon to stop and waits for it, returning whether it has.
    fn ask_to_stop(&self) -> bool {
        let Some(pipe) = midi_harbor_ipc::transport::pipe_name(&self.socket) else {
            return true;
        };
        if !matches!(midi_harbor_platform::stop::request(&pipe), Ok(true)) {
            return !midi_harbor_ipc::transport::answers(&self.socket);
        }
        let started = std::time::Instant::now();
        while started.elapsed() < STOP_WAIT {
            if !midi_harbor_ipc::transport::answers(&self.socket) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }
}

#[cfg(windows)]
impl ServiceManager for TaskScheduler {
    fn install(&self, spec: &ServiceSpec) -> Result<PathBuf, ServiceError> {
        let spec = supervised(spec, &self.log_file);
        let definition = render_task(&spec, &Self::user()?, &Self::console_host());
        if let Some(parent) = self.definition_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ServiceError::Io {
                operation: "create",
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(&self.definition_path, encode_definition(&definition)).map_err(
            |source| ServiceError::Io {
                operation: "write",
                path: self.definition_path.clone(),
                source,
            },
        )?;
        let path = self.definition_path.display().to_string();
        run(
            "schtasks",
            &["/Create", "/TN", TASK_NAME, "/XML", &path, "/F"],
        )?;
        Ok(self.definition_path.clone())
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        // A daemon left running would keep its ports open with nothing to start it again.
        let _ = self.stop();
        let _ = run("schtasks", &["/Delete", "/TN", TASK_NAME, "/F"]);
        match std::fs::remove_file(&self.definition_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(ServiceError::Io {
                operation: "remove",
                path: self.definition_path.clone(),
                source,
            }),
        }
    }

    fn start(&self) -> Result<(), ServiceError> {
        run("schtasks", &["/Run", "/TN", TASK_NAME]).map(|_| ())
    }

    fn stop(&self) -> Result<(), ServiceError> {
        if self.ask_to_stop() {
            return Ok(());
        }
        // Ending the daemon skips releasing held notes, so it is the last resort for one that did
        // not answer. Ending the task alone ends only the console host it started, and left the
        // supervisor and the daemon running, so both are ended by process.
        tracing::warn!("the daemon did not stop when asked; ending it");
        if let Some(pipe) = midi_harbor_ipc::transport::pipe_name(&self.socket) {
            midi_harbor_platform::stop::force(&pipe).map_err(|error| ServiceError::Command {
                command: "end the daemon".to_owned(),
                detail: error.to_string(),
            })?;
        }
        let _ = run("schtasks", &["/End", "/TN", TASK_NAME]);
        if midi_harbor_ipc::transport::answers(&self.socket) {
            return Err(ServiceError::Command {
                command: "end the daemon".to_owned(),
                detail: "it is still running; end midi-harbor.exe in Task Manager".to_owned(),
            });
        }
        Ok(())
    }

    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let definition = run("schtasks", &["/Query", "/TN", TASK_NAME, "/XML"]).ok();
        let running = midi_harbor_ipc::transport::answers(&self.socket);
        Ok(status_from(
            definition.as_deref(),
            &self.definition_path,
            running,
        ))
    }

    fn name(&self) -> &'static str {
        "Task Scheduler"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the specification `service install` writes for an installed build.
    fn spec() -> ServiceSpec {
        ServiceSpec {
            executable: PathBuf::from(r"C:\Program Files\Midi Harbor\midi-harbor.exe"),
            arguments: vec!["daemon".to_owned()],
            start_at_login: true,
        }
    }

    /// Returns the console host as Windows installs it.
    fn console() -> PathBuf {
        PathBuf::from(r"C:\Windows\System32\conhost.exe")
    }

    /// Locks the task's action: `conhost.exe --headless` runs the daemon under its supervisor,
    /// with its log file named.
    ///
    /// Task Scheduler does not restart a program that fails (research R-083), so the supervisor
    /// does; it discards a program's output, so the daemon keeps its own log; and a console
    /// program it starts directly opens a window at logon, which `--headless` suppresses.
    #[test]
    fn the_daemon_runs_supervised_through_a_headless_console() {
        let spec = supervised(
            &spec(),
            Path::new(r"C:\Users\A\AppData\Local\midi-harbor\logs\daemon.log"),
        );
        let task = render_task(&spec, r"host\a", &console());
        assert!(
            task.contains(r"<Command>C:\Windows\System32\conhost.exe</Command>"),
            "the task must start the console host rather than the daemon: {task}"
        );
        assert!(
            task.contains(
                r#"<Arguments>--headless &quot;C:\Program Files\Midi Harbor\midi-harbor.exe&quot; daemon --supervise --log-file C:\Users\A\AppData\Local\midi-harbor\logs\daemon.log</Arguments>"#
            ),
            "the console host must run the supervised daemon with its log file: {task}"
        );
    }

    /// Locks the task's principal and trigger: it runs as the user with an interactive token at
    /// least privilege, and has a `LogonTrigger` for that user only when it should start at
    /// login.
    ///
    /// The element names are Task Scheduler's task schema, version 1.2. A task that needed
    /// elevation, or ran outside the user's session, could not reach the user's MIDI devices.
    #[test]
    fn the_task_runs_as_the_user_and_starts_at_logon_only_when_asked() {
        let cases = [("start at login", true, 2), ("start by hand", false, 1)];
        for (name, start_at_login, want_user_ids) in cases {
            let task = render_task(
                &ServiceSpec {
                    start_at_login,
                    ..spec()
                },
                r"host\a",
                &console(),
            );
            assert_eq!(
                task.contains("<LogonTrigger>"),
                start_at_login,
                "{name}: the logon trigger must follow the start-at-login setting"
            );
            assert_eq!(
                task.matches(r"<UserId>host\a</UserId>").count(),
                want_user_ids,
                "{name}: the principal, and the trigger when there is one, must name the user"
            );
            assert!(
                task.contains("<LogonType>InteractiveToken</LogonType>")
                    && task.contains("<RunLevel>LeastPrivilege</RunLevel>"),
                "{name}: the task must run in the user's session without elevation"
            );
        }
    }

    /// Locks the settings that would otherwise stop or slow a daemon carrying MIDI: priority 4,
    /// no execution time limit, and no stopping or refusing to start on battery.
    ///
    /// Task Scheduler's defaults are priority 7, below normal; a three-day limit; and both
    /// battery rules on, which would stop MIDI whenever a laptop is unplugged.
    #[test]
    fn the_task_runs_at_normal_priority_without_limits() {
        let task = render_task(&spec(), r"host\a", &console());
        for setting in [
            "<Priority>4</Priority>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
        ] {
            assert!(
                task.contains(setting),
                "{setting} must override Task Scheduler's default"
            );
        }
    }

    /// Locks argument quoting against the rules the Microsoft C runtime parses a command line
    /// by ("Parsing C command-line arguments"): quotes around an argument with a space, tab or
    /// quote, or an empty one; backslashes doubled before a quote, including the closing one.
    /// Each path is also read back out of a rendered task, through its XML escaping, as
    /// `service status` does to find a registration whose binary has moved.
    #[test]
    fn the_executable_is_quoted_as_windows_reads_it_and_reads_back() {
        let cases = [
            (
                "a plain path",
                r"C:\tools\midi-harbor.exe",
                r"C:\tools\midi-harbor.exe",
            ),
            (
                "a path with a space",
                r"C:\Program Files\Midi Harbor\midi-harbor.exe",
                r#""C:\Program Files\Midi Harbor\midi-harbor.exe""#,
            ),
            ("a trailing backslash", r"C:\a b\", r#""C:\a b\\""#),
            ("an embedded quote", r#"say "hi""#, r#""say \"hi\"""#),
            ("a backslash before a quote", r#"x\"y z"#, r#""x\\\"y z""#),
            ("an empty argument", "", r#""""#),
            (
                "an ampersand",
                r"C:\a&b\midi-harbor.exe",
                r"C:\a&b\midi-harbor.exe",
            ),
        ];
        for (name, path, want_quoted) in cases {
            assert_eq!(
                quote_argument(path),
                want_quoted,
                "{name}: the C runtime must read the quoted form back as one argument"
            );
            let task = render_task(
                &ServiceSpec {
                    executable: PathBuf::from(path),
                    ..spec()
                },
                r"host\a",
                &console(),
            );
            assert_eq!(
                registered_executable(&task),
                Some(PathBuf::from(path)),
                "{name}: the registered executable must read back as it was written"
            );
        }
    }

    /// Locks the definition file's encoding: UTF-16 little-endian with a byte order mark, as the
    /// task's own `encoding="UTF-16"` declaration says and as `schtasks /Create /XML` reads it.
    #[test]
    fn the_definition_file_is_utf16_with_a_byte_order_mark() {
        assert_eq!(
            encode_definition("<a/>"),
            [0xFF, 0xFE, b'<', 0, b'a', 0, b'/', 0, b'>', 0],
            "schtasks must find the byte order mark its declared encoding promises"
        );
    }
}
