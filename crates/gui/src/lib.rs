//! libcosmic graphical interface over the daemon IPC contract.
//!
//! The window is a client like any other: it owns no MIDI state and holds nothing the daemon does
//! not, so closing it changes nothing about what is connected.

mod app;
#[cfg(target_os = "macos")]
pub mod app_store;
mod client;
mod dialogs;
pub mod format;
mod onboarding;
mod parts;
mod update;
mod view;

use cosmic::app::Settings;
use cosmic::iced::Size;

/// Width the window opens at: the nav bar, a list, and the docked details panel side by side.
const WINDOW_WIDTH: f32 = 1280.0;
/// Height the window opens at.
const WINDOW_HEIGHT: f32 = 820.0;

/// Why the interface could not be shown.
#[derive(Debug, thiserror::Error)]
pub enum GuiError {
    /// The window system refused the window.
    #[error("could not open a window: {0}")]
    Window(#[from] cosmic::iced::Error),
}

/// Opens the window and runs until the user closes it.
///
/// Returns once the window is closed. A daemon that is not running is not an error here: the
/// window opens and says so, because telling a user to start a daemon in a terminal is the one
/// thing a graphical interface exists to avoid.
pub fn run(socket: Option<std::path::PathBuf>) -> Result<(), GuiError> {
    let settings = Settings::default().size(Size::new(WINDOW_WIDTH, WINDOW_HEIGHT));
    #[cfg(target_os = "macos")]
    let settings = macos(settings);
    cosmic::app::run::<app::App>(settings, socket)?;
    Ok(())
}

/// Sets up the menus' target, and the App Store build, which lives in the menu bar: closing the
/// window leaves it running, and its delegate has to be in place before the event loop starts to
/// hear about quitting and the launch event (feature 014, research R-098).
#[cfg(target_os = "macos")]
fn macos(settings: Settings) -> Settings {
    let store = midi_harbor_core::paths::sandboxed();
    if let Err(error) = midi_harbor_platform::appkit::install(store) {
        tracing::error!(error = %error, "failed to set up the menus");
    }
    if store {
        settings.exit_on_close(false)
    } else {
        settings
    }
}
