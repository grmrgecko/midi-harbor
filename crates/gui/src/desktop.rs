//! Giving an AppImage its place among the user's applications (research R-109).
//!
//! A Wayland desktop finds a window's icon by its application ID, looking for a desktop entry of
//! that name among the installed ones. A package installs the entry. An AppImage carries one
//! inside, where no desktop looks, so its window showed the generic icon and it was missing from
//! the application menu. Run from an AppImage, the window installs the entry and the icon for
//! this user, pointing at the AppImage file.

use std::path::{Path, PathBuf};

/// The desktop entry's name without its extension, which is the window's application ID.
const ID: &str = "com.mrgeckosmedia.MidiHarbor";

/// The icon theme every desktop falls back to, under a data directory.
const THEME: &str = "icons/hicolor";

/// Installs this AppImage's desktop entry and icon for the user, when the program runs from one.
///
/// A failure costs the icon and nothing else, so it is logged and the window opens regardless.
pub fn integrate() {
    let Some((appimage, appdir)) = midi_harbor_service::running_appimage() else {
        return;
    };
    let Some(data_home) = data_home() else {
        return;
    };
    match install(&appimage, &appdir, &data_home, &data_dirs()) {
        Ok(Written { entry, icon }) => {
            if icon {
                announce_icon(&data_home.join(THEME));
            }
            if entry || icon {
                tracing::info!(appimage = %appimage.display(), "installed the desktop entry");
            }
        }
        Err(error) => tracing::debug!(error = %error, "could not install the desktop entry"),
    }
}

/// Which of the two files an installation wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Written {
    /// The desktop entry.
    entry: bool,
    /// The icon.
    icon: bool,
}

/// Installs the entry and icon under `data_home`, and reports which were written.
///
/// Nothing is installed where a package already provides the entry in `data_dirs`: one under
/// the user's directory takes precedence, and would point the package's menu item at the
/// AppImage.
fn install(
    appimage: &Path,
    appdir: &Path,
    data_home: &Path,
    data_dirs: &[PathBuf],
) -> std::io::Result<Written> {
    let entry_name = format!("{ID}.desktop");
    let icon_name = format!("{ID}.svg");
    if data_dirs
        .iter()
        .any(|dir| dir.join("applications").join(&entry_name).exists())
    {
        return Ok(Written::default());
    }
    let Some(target) = appimage.to_str() else {
        return Ok(Written::default());
    };

    // Write each only when it differs, so an AppImage that has not moved touches nothing.
    let carried = appdir.join("usr/share");
    let entry = entry_for(
        &std::fs::read_to_string(carried.join("applications").join(&entry_name))?,
        target,
    );
    let icon = std::fs::read(carried.join("icons/hicolor/scalable/apps").join(&icon_name))?;
    let wrote_entry = write_if_changed(
        &data_home.join("applications").join(&entry_name),
        entry.as_bytes(),
    )?;
    let wrote_icon = write_if_changed(
        &data_home.join(THEME).join("scalable/apps").join(&icon_name),
        &icon,
    )?;
    Ok(Written {
        entry: wrote_entry,
        icon: wrote_icon,
    })
}

/// Tells desktops already running that the icon theme under `theme` has a new icon.
///
/// A shell reads the theme's directories when it starts and does not look again. Plasma, running
/// since before the icon's directory existed, found the desktop entry and drew a blank where
/// the icon belonged. KDE reloads its icons on the signal sent here, and loaders that compare
/// times reload when the theme's directory is newer than what they read. Neither is needed for
/// the icon to be there at the next login, so a failure is not reported.
fn announce_icon(theme: &Path) {
    if let Ok(directory) = std::fs::File::open(theme) {
        let _ = directory.set_modified(std::time::SystemTime::now());
    }
    // The signal KDE's own programs send after changing icons. Group 0 is the desktop's.
    let _ = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--type=signal",
            "/KIconLoader",
            "org.kde.KIconLoader.iconChanged",
            "int32:0",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Writes `contents` to `path` unless the file already holds them, and reports whether it wrote.
fn write_if_changed(path: &Path, contents: &[u8]) -> std::io::Result<bool> {
    if std::fs::read(path).is_ok_and(|held| held == contents) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(true)
}

/// Rewrites the desktop entry an AppImage carries so that it starts the AppImage file.
///
/// `TryExec` names the file too, so a desktop leaves the entry out of its menu once the AppImage
/// has been deleted, which is the only way an AppImage is ever removed.
fn entry_for(carried: &str, appimage: &str) -> String {
    let mut entry = String::with_capacity(carried.len() + appimage.len() * 2);
    for line in carried.lines() {
        if line.starts_with("TryExec=") {
            continue;
        }
        match line.strip_prefix("Exec=") {
            Some(command) => {
                // The carried entry runs `midi-harbor`, found on PATH by a package install.
                let arguments = command
                    .split_once(' ')
                    .map_or("", |(_, arguments)| arguments);
                entry.push_str(&format!("Exec={} {arguments}\n", quoted(appimage)));
                entry.push_str(&format!("TryExec={}\n", appimage.replace('\\', "\\\\")));
            }
            None => {
                entry.push_str(line);
                entry.push('\n');
            }
        }
    }
    entry
}

/// Quotes a path as one argument of an `Exec` key.
///
/// The Desktop Entry Specification has an argument holding a space or a reserved character
/// written in double quotes, with `"`, `` ` ``, `$` and `\` escaped by a backslash. The value is
/// then a string, where a backslash is itself written twice, and a literal percent sign is `%%`.
fn quoted(path: &str) -> String {
    let mut argument = String::with_capacity(path.len() + 2);
    argument.push('"');
    for character in path.chars() {
        match character {
            '"' | '`' | '$' => {
                argument.push_str("\\\\");
                argument.push(character);
            }
            '\\' => argument.push_str("\\\\\\\\"),
            '%' => argument.push_str("%%"),
            other => argument.push(other),
        }
    }
    argument.push('"');
    argument
}

/// Returns the directory the user's own data files live under.
fn data_home() -> Option<PathBuf> {
    absolute(std::env::var_os("XDG_DATA_HOME"))
        .or_else(|| absolute(std::env::var_os("HOME")).map(|home| home.join(".local/share")))
}

/// Returns the directories installed data files are searched in, after the user's own.
fn data_dirs() -> Vec<PathBuf> {
    match std::env::var_os("XDG_DATA_DIRS").filter(|dirs| !dirs.is_empty()) {
        Some(dirs) => std::env::split_paths(&dirs).collect(),
        // The XDG Base Directory Specification's default.
        None => vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ],
    }
}

/// Returns a path from the environment when it is set and absolute, as the specification
/// requires of every one of its directories.
fn absolute(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the entry installed for an AppImage starts the AppImage file, written as the
    /// Desktop Entry Specification's `Exec` key reads it: quoted, with a dollar sign escaped and
    /// a percent sign doubled, the arguments kept, and `TryExec` naming the file so the entry
    /// disappears with it. Every other line is carried over as it was.
    #[test]
    fn the_installed_entry_starts_the_appimage_file() {
        let carried = "[Desktop Entry]\nName=Midi Harbor\nExec=midi-harbor gui\nIcon=x\n";
        let cases = [
            (
                "a plain path",
                "/home/user/Applications/Midi-Harbor.AppImage",
                "Exec=\"/home/user/Applications/Midi-Harbor.AppImage\" gui\n\
                 TryExec=/home/user/Applications/Midi-Harbor.AppImage\n",
            ),
            (
                "a path with a space, a dollar and a percent sign",
                "/home/user/My Apps/$5 100%.AppImage",
                "Exec=\"/home/user/My Apps/\\\\$5 100%%.AppImage\" gui\n\
                 TryExec=/home/user/My Apps/$5 100%.AppImage\n",
            ),
        ];
        for (name, appimage, want) in cases {
            assert_eq!(
                entry_for(carried, appimage),
                format!("[Desktop Entry]\nName=Midi Harbor\n{want}Icon=x\n"),
                "{name}: the entry is not what a desktop reads as this file"
            );
        }
    }

    /// Proves an AppImage's entry and icon are installed under the user's data directory, are
    /// left untouched on the next run, and are not installed at all where a package already
    /// provides the entry, whose menu item the user's copy would otherwise take over.
    #[test]
    fn an_appimage_installs_its_entry_once_and_never_over_a_package() {
        let root = std::env::temp_dir().join(format!("mh-desktop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let appdir = root.join("AppDir");
        let entry = format!("{ID}.desktop");
        let icon = format!("{ID}.svg");
        for (dir, name, contents) in [
            (
                "applications",
                &entry,
                "[Desktop Entry]\nExec=midi-harbor gui\n",
            ),
            ("icons/hicolor/scalable/apps", &icon, "<svg/>"),
        ] {
            let dir = appdir.join("usr/share").join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), contents).unwrap();
        }
        let appimage = root.join("Midi-Harbor.AppImage");
        let home = root.join("home");
        let installed = home.join("applications").join(&entry);

        assert_eq!(
            install(&appimage, &appdir, &home, &[]).unwrap(),
            Written {
                entry: true,
                icon: true
            },
            "the first run must install the entry and the icon"
        );
        assert!(
            std::fs::read_to_string(&installed)
                .unwrap()
                .contains(&format!("Exec=\"{}\" gui", appimage.display())),
            "the installed entry must start the AppImage file"
        );
        assert!(
            home.join("icons/hicolor/scalable/apps")
                .join(&icon)
                .exists(),
            "the icon the entry names must be installed with it"
        );
        assert_eq!(
            install(&appimage, &appdir, &home, &[]).unwrap(),
            Written::default(),
            "a second run of the same AppImage must write nothing"
        );

        // A package's entry in the system directories is left to stand.
        let system = root.join("system");
        std::fs::create_dir_all(system.join("applications")).unwrap();
        std::fs::write(system.join("applications").join(&entry), "").unwrap();
        let other_home = root.join("other-home");
        assert_eq!(
            install(&appimage, &appdir, &other_home, &[system]).unwrap(),
            Written::default(),
            "an entry a package provides must not be shadowed"
        );
        assert!(
            !other_home.exists(),
            "nothing may be written beside a package's entry"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
