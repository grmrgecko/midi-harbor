//! Where configuration, runtime state and the daemon socket live on each platform.

use std::path::{Path, PathBuf};

/// Directory name used for configuration and runtime state on every platform.
///
/// The project's own name, unspaced. A space would buy nothing and cost characters in the socket
/// path, which is bounded by `sun_path`.
pub const APP_DIR: &str = "midi-harbor";

/// Reverse-DNS identifier of the application bundle, which the launchd label extends.
pub const BUNDLE_ID: &str = "com.mrgeckosmedia.MidiHarbor";
/// File name of the configuration document.
pub const CONFIG_FILE: &str = "config.yaml";
/// File name of the daemon's listening socket.
pub const SOCKET_FILE: &str = "daemon.sock";

/// Names the variable the App Sandbox sets in every sandboxed process, before `main` runs.
pub const SANDBOX_VARIABLE: &str = "APP_SANDBOX_CONTAINER_ID";

/// Reports whether this process runs in the macOS App Sandbox, which only the App Store build
/// does.
///
/// The sandbox sets `APP_SANDBOX_CONTAINER_ID` itself, and redirects `HOME` and `TMPDIR` into the
/// app's container (research R-094). Every other platform returns false.
pub fn sandboxed() -> bool {
    cfg!(target_os = "macos") && std::env::var_os(SANDBOX_VARIABLE).is_some()
}

/// Why a standard directory could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// The operating system reported no home directory for this user.
    #[error("could not determine the user's home directory")]
    NoHome,
}

/// Resolves the standard locations Midi Harbor uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    config_dir: PathBuf,
    runtime_dir: PathBuf,
    /// A socket named with `--socket`, which replaces the standard one and nothing else.
    socket: Option<PathBuf>,
}

impl Paths {
    /// Resolves the platform's standard locations for the current user.
    ///
    /// Configuration goes where each platform expects a user-editable document:
    /// `~/Library/Application Support/midi-harbor` on macOS, and `$XDG_CONFIG_HOME/midi-harbor`
    /// on Linux, which honours the variable when it is set and falls back to `~/.config`.
    pub fn resolve() -> Result<Self, PathError> {
        let base = directories::BaseDirs::new().ok_or(PathError::NoHome)?;
        let config_dir = base.config_dir().join(APP_DIR);
        Ok(Self {
            config_dir,
            runtime_dir: Self::runtime_dir_in(&base),
            socket: None,
        })
    }

    /// Returns the directory transient runtime state belongs in.
    ///
    /// The socket is not configuration and must not outlive a boot. Linux has
    /// `XDG_RUNTIME_DIR` for exactly this; macOS has no equivalent, so the per-user temporary
    /// directory is the closest correct thing.
    fn runtime_root(base: &directories::BaseDirs) -> PathBuf {
        if let Some(dir) = base.runtime_dir() {
            return dir.to_path_buf();
        }
        std::env::temp_dir()
    }

    /// Returns the runtime directory: the app's own directory under the runtime root, or in the
    /// sandbox the container's `tmp` itself.
    ///
    /// The container's `tmp` is already private to Midi Harbor, and its path is long: with the
    /// directory the socket reaches macOS's 103-byte limit for account names over 15 characters,
    /// and without it for names over 27 (research R-096).
    fn runtime_dir_in(base: &directories::BaseDirs) -> PathBuf {
        if sandboxed() {
            return std::env::temp_dir();
        }
        Self::runtime_root(base).join(APP_DIR)
    }

    /// Builds paths rooted at an explicit directory, for tests.
    pub fn rooted_at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            config_dir: root.join("config"),
            runtime_dir: root.join("run"),
            socket: None,
        }
    }

    /// Uses `socket` in place of the standard socket, leaving the configuration where it was.
    ///
    /// This is what `--socket` means for every role, the daemon included: a second daemon on its
    /// own socket still reads the user's configuration, and one that silently listened on the
    /// standard socket instead would collide with the daemon already there.
    #[must_use]
    pub fn with_socket(mut self, socket: Option<PathBuf>) -> Self {
        if socket.is_some() {
            self.socket = socket;
        }
        self
    }

    /// Returns the directory holding the configuration document.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Returns the configuration document's full path.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE)
    }

    /// Returns the directory holding runtime state.
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Returns the daemon socket's full path.
    pub fn socket_file(&self) -> PathBuf {
        self.socket
            .clone()
            .unwrap_or_else(|| self.runtime_dir.join(SOCKET_FILE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configuration lives where the platform keeps a user's editable documents, and the
    /// runtime directory holding the socket has no space in it.
    ///
    /// macOS keeps them in `~/Library/Application Support`, as the `directories` crate reports
    /// it; elsewhere the platform's own directory holds a `midi-harbor` folder. The socket path is
    /// bounded by `sun_path`, 104 bytes on macOS, so characters spent on presentation are
    /// characters taken from the temporary directory the system hands us.
    #[test]
    fn paths_resolve_where_the_platform_expects_them() {
        let paths = Paths::resolve().expect("this user has a home directory");
        assert!(
            paths.config_dir().is_absolute() && paths.runtime_dir().is_absolute(),
            "resolved paths must not depend on the working directory: {paths:?}"
        );
        assert!(
            !paths.runtime_dir().display().to_string().contains(' '),
            "the runtime directory must leave room in sun_path: {paths:?}"
        );

        let shown = paths.config_dir().display().to_string();
        let expected = if cfg!(target_os = "macos") {
            "Library/Application Support/midi-harbor"
        } else {
            APP_DIR
        };
        assert!(
            shown.ends_with(expected),
            "the configuration must be in the platform's place for user documents: {shown}"
        );
    }
}
