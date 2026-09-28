//! The Dock icon, which the App Store build shows only while its window is open.

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use tracing::warn;

/// Shows or hides the app's Dock icon, bringing the app forward when showing it.
///
/// The variant's Info.plist sets `LSUIElement`, so the app starts as an accessory with no Dock
/// icon, and a launch at login with the window closed never shows one (research R-098).
pub fn set_dock_visible(visible: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        warn!("the Dock icon can only be changed on the main thread");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    if visible {
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    } else {
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}
