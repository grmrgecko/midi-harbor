//! Desktop notifications, for news a person needs even with no Midi Harbor window open.

/// Shows a notification with `title` and `body`, where this platform can.
///
/// On macOS it goes through `osascript`, which a bare executable run by launchd can use where the
/// notification API needs an app bundle. The text travels as arguments, never spliced into the
/// script, so nothing in it needs escaping. It runs on a thread of its own and is waited for
/// there, so the caller is not held up and no child is left unreaped. Elsewhere it does nothing:
/// only CoreMIDI has a service that can die under the daemon.
pub fn post(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("/usr/bin/osascript");
        command
            .args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
            ])
            .arg(title)
            .arg(body)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        std::thread::spawn(move || match command.status() {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::warn!(%status, "could not show a notification"),
            Err(error) => tracing::warn!(error = %error, "could not show a notification"),
        });
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, body);
    }
}
