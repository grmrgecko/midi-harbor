//! The target of the app's menus, which the App Store build also makes its app delegate:
//! quitting only once the daemon has stopped, a second launch showing the window, and telling a
//! launch at login from any other.

use super::menus::{open_link, resend_edit_key};
use super::{ShellAction, ShellError, send};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationDelegate, NSApplicationTerminateReply, NSMenuItem,
};
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSNotification};
use std::cell::RefCell;
use std::sync::OnceLock;
use tracing::warn;

/// The launch event's code, `kAEOpenApplication`, 'oapp'.
const OPEN_APPLICATION: u32 = u32::from_be_bytes(*b"oapp");
/// The keyword of the launch event's property data, `keyAEPropData`, 'prdt'.
const PROPERTY_DATA: u32 = u32::from_be_bytes(*b"prdt");
/// What the property data holds for a launch at login, `keyAELaunchedAsLogInItem`, 'lgit'.
const LAUNCHED_AS_LOGIN_ITEM: u32 = u32::from_be_bytes(*b"lgit");

/// Whether this launch was at login, decided once from the launch event.
static AT_LOGIN: OnceLock<bool> = OnceLock::new();

thread_local! {
    /// The delegate, kept alive here since `NSApplication` holds its delegate weakly.
    static SHELL: RefCell<Option<Retained<Shell>>> = const { RefCell::new(None) };
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements, and `Shell` implements no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MidiHarborShell"]
    pub(super) struct Shell;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for Shell {}

    // SAFETY: each method's signature matches the protocol's, as objc2-app-kit declares it.
    unsafe impl NSApplicationDelegate for Shell {
        /// Holds the quit until the window has stopped the daemon, so held notes are released
        /// whatever asked to quit: a menu, a logout, a restart or a shutdown.
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            send(ShellAction::Quit);
            NSApplicationTerminateReply::TerminateLater
        }

        /// Shows the window when the app is opened again while running, from Finder or by a
        /// click on its Dock icon (FR-A11).
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_reopen(&self, _sender: &NSApplication, _visible: bool) -> bool {
            send(ShellAction::Open);
            true
        }

        /// Reads the launch event while it is still the current one (research R-099).
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            let _ = launched_at_login();
        }
    }

    impl Shell {
        /// Shows the window, from the menu bar item.
        #[unsafe(method(openWindow:))]
        fn open_window(&self, _sender: Option<&AnyObject>) {
            send(ShellAction::Open);
        }

        /// Closes the window, from Command-W, or to the menu bar from Command-Q in App Store
        /// mode.
        #[unsafe(method(closeWindow:))]
        fn close_window(&self, _sender: Option<&AnyObject>) {
            send(ShellAction::CloseWindow);
        }

        /// Zooms the window.
        #[unsafe(method(zoomWindow:))]
        fn zoom_window(&self, _sender: Option<&AnyObject>) {
            send(ShellAction::Zoom);
        }

        /// Shows the Settings page.
        #[unsafe(method(showSettings:))]
        fn show_settings(&self, _sender: Option<&AnyObject>) {
            send(ShellAction::ShowSettings);
        }

        /// Shows the page the View menu item stands for, by the item's tag.
        #[unsafe(method(showPage:))]
        fn show_page(&self, sender: Option<&AnyObject>) {
            let tag = sender
                .and_then(|sender| sender.downcast_ref::<NSMenuItem>())
                .map(|item| item.tag());
            if let Some(Ok(position)) = tag.map(usize::try_from) {
                send(ShellAction::ShowPage(position));
            }
        }

        /// Opens the dialog for a new virtual port.
        #[unsafe(method(newVirtualPort:))]
        fn new_virtual_port(&self, _sender: Option<&AnyObject>) {
            send(ShellAction::NewVirtualPort);
        }

        /// Opens the dialog for a new network port.
        #[unsafe(method(newNetworkPort:))]
        fn new_network_port(&self, _sender: Option<&AnyObject>) {
            send(ShellAction::NewNetworkPort);
        }

        /// Opens the dialog for a new route.
        #[unsafe(method(newRoute:))]
        fn new_route(&self, _sender: Option<&AnyObject>) {
            send(ShellAction::NewRoute);
        }

        /// Exports the diagnostic report.
        #[unsafe(method(exportDiagnostics:))]
        fn export_diagnostics(&self, _sender: Option<&AnyObject>) {
            send(ShellAction::ExportDiagnostics);
        }

        /// Hands an Edit menu item's shortcut to the window, whose text fields act on it.
        #[unsafe(method(editKey:))]
        fn edit_key(&self, sender: Option<&AnyObject>) {
            if let Some(item) = sender.and_then(|sender| sender.downcast_ref::<NSMenuItem>()) {
                resend_edit_key(MainThreadMarker::from(self), item);
            }
        }

        /// Opens the web page a Help menu item stands for, by the item's tag.
        #[unsafe(method(openLink:))]
        fn open_link(&self, sender: Option<&AnyObject>) {
            if let Some(item) = sender.and_then(|sender| sender.downcast_ref::<NSMenuItem>()) {
                open_link(item.tag());
            }
        }
    }
);

impl Shell {
    /// Creates the delegate.
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: `init` is `NSObject`'s designated initialiser, called once on a fresh
        // allocation.
        unsafe { msg_send![super(this), init] }
    }
}

/// Creates the menus' target, and sets it on the application as its delegate for the App Store
/// build.
pub(super) fn install(store: bool) -> Result<(), ShellError> {
    let mtm = MainThreadMarker::new().ok_or(ShellError::NotMainThread)?;
    let shell = Shell::new(mtm);
    if store {
        NSApplication::sharedApplication(mtm).setDelegate(Some(ProtocolObject::from_ref(&*shell)));
    }
    SHELL.with(|slot| *slot.borrow_mut() = Some(shell));
    Ok(())
}

/// Returns the menus' target, for menu items to send their actions to.
pub(super) fn shell() -> Option<Retained<Shell>> {
    SHELL.with(|slot| slot.borrow().clone())
}

/// Reports whether macOS started this app as a login item.
///
/// Decided from the launch Apple event, whose property data is `keyAELaunchedAsLogInItem` for a
/// launch at login. The event is current only while launching finishes, so the first answer is
/// kept; the delegate asks then.
pub fn launched_at_login() -> bool {
    *AT_LOGIN.get_or_init(|| {
        let Some(event) = NSAppleEventManager::sharedAppleEventManager().currentAppleEvent() else {
            return false;
        };
        // SAFETY: `eventID` and `paramDescriptorForKeyword:` are `NSAppleEventDescriptor`
        // methods taking and returning the types given here, `AEEventID` and `AEKeyword` being
        // `u32` four-character codes. Called by message rather than through objc2-foundation,
        // which gates them behind the CoreServices bindings.
        let (id, data): (u32, Option<Retained<NSAppleEventDescriptor>>) = unsafe {
            (
                msg_send![&*event, eventID],
                msg_send![&*event, paramDescriptorForKeyword: PROPERTY_DATA],
            )
        };
        id == OPEN_APPLICATION
            && data.is_some_and(|data| data.enumCodeValue() == LAUNCHED_AS_LOGIN_ITEM)
    })
}

/// Answers the quit the delegate held, once the daemon has stopped.
pub fn reply_to_terminate() {
    let Some(mtm) = MainThreadMarker::new() else {
        warn!("the quit can only be answered on the main thread");
        return;
    };
    NSApplication::sharedApplication(mtm).replyToApplicationShouldTerminate(true);
}
