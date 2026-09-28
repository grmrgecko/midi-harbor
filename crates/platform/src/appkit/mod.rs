//! The app's place in macOS: its menus (feature 015), and for the App Store build its menu bar
//! item, its Dock icon, quitting through its app delegate, and its login item (feature 014).
//!
//! AppKit must be called on the main thread, so every function here does nothing, and logs why,
//! when called from any other. What the user chooses in a menu reaches the window as a
//! [`ShellAction`] on the channel [`take_actions`] hands over, so nothing in AppKit reaches into
//! the window's state.

mod delegate;
mod dock;
pub mod login_item;
mod menus;

pub use delegate::{launched_at_login, reply_to_terminate};
pub use dock::set_dock_visible;
pub use menus::install_menus;

use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Something the user asked of the app through AppKit rather than through its window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellAction {
    /// Show the window: from the menu bar item, a second launch, or a click on the Dock icon.
    Open,
    /// Close the window: Command-W, and in App Store mode Command-Q, which closes it to the menu
    /// bar and leaves everything running.
    CloseWindow,
    /// Zoom the window, or return it to its size before.
    Zoom,
    /// Quit entirely. macOS is waiting, through `NSTerminateLater`, for
    /// [`reply_to_terminate`] once the daemon has stopped.
    Quit,
    /// Show the Settings page, from Settings… in the app menu.
    ShowSettings,
    /// Show a page, by its position in the list [`install_menus`] was given.
    ShowPage(usize),
    /// Open the dialog for a new virtual port.
    NewVirtualPort,
    /// Open the dialog for a new network port.
    NewNetworkPort,
    /// Open the dialog for a new route.
    NewRoute,
    /// Export the diagnostic report.
    ExportDiagnostics,
}

/// Why the app could not be set up in the menu bar.
#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    /// AppKit was called from a thread other than the main one.
    #[error("the menu bar can only be set up on the main thread")]
    NotMainThread,
}

/// The sending half of the action channel, which AppKit's callbacks use.
static SENDER: OnceLock<UnboundedSender<ShellAction>> = OnceLock::new();
/// The receiving half, until the window takes it.
static RECEIVER: Mutex<Option<UnboundedReceiver<ShellAction>>> = Mutex::new(None);

/// Sets up the target of the menus, before the event loop starts. For the App Store build it is
/// also made the app delegate, so quitting, reopening and the launch event come to this app.
///
/// winit 0.31 leaves the delegate free, listening for launch and termination through
/// notifications instead (research R-098). The direct build leaves it free too, so quitting
/// there stays winit's.
pub fn install(store: bool) -> Result<(), ShellError> {
    let (sender, receiver) = unbounded_channel();
    if SENDER.set(sender).is_ok() {
        match RECEIVER.lock() {
            Ok(mut slot) => *slot = Some(receiver),
            Err(poisoned) => *poisoned.into_inner() = Some(receiver),
        }
    }
    delegate::install(store)
}

/// Hands over the channel of actions chosen through AppKit, once.
pub fn take_actions() -> Option<UnboundedReceiver<ShellAction>> {
    match RECEIVER.lock() {
        Ok(mut slot) => slot.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
}

/// Sends an action to the window, dropping it if the window has gone.
fn send(action: ShellAction) {
    if let Some(sender) = SENDER.get() {
        let _ = sender.send(action);
    }
}
