//! The menu bar menus (specs/015-mac-menus/contracts/menus.md) and the App Store build's menu bar
//! item (specs/014-mac-app-store-mode/contracts/menus.md).

use super::ShellError;
use super::delegate::shell;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSImage, NSMenu, NSMenuItem,
    NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWorkspace,
};
use objc2_foundation::{NSData, NSPoint, NSSize, NSString, NSURL};
use std::cell::RefCell;
use tracing::warn;

/// The menu bar item's glyph, the anchor from the app icon drawn at twice its 18-point size.
const MENU_BAR_ICON: &[u8] = include_bytes!("../../../../packaging/macos/MenuBarIcon.png");

/// The web pages the Help menu opens, by the tag of the item standing for each.
const LINKS: [&str; 2] = [
    "https://github.com/grmrgecko/midi-harbor/blob/main/docs/README.md",
    "https://github.com/grmrgecko/midi-harbor/issues",
];

/// The Edit menu: each item's title, its key, and that key's code on the ANSI layout.
const EDIT_ITEMS: [(&str, &str, u16); 4] = [
    ("Cut", "x", 7),
    ("Copy", "c", 8),
    ("Paste", "v", 9),
    ("Select All", "a", 0),
];

thread_local! {
    /// The menu bar item, which disappears when released.
    static STATUS_ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
}

/// Where a menu item's action goes.
enum Target {
    /// The app's own menu target, for its window actions.
    Shell,
    /// The responder chain, for the standard actions AppKit answers itself.
    Standard,
}

/// Installs the menu bar menus over winit's default, and for the App Store build adds the menu
/// bar item. `pages` names the window's pages in order, for the View menu.
///
/// Called once the event loop runs: winit builds its default menu as launching finishes, and
/// this replaces it (research R-098).
pub fn install_menus(store: bool, pages: &[&str]) -> Result<(), ShellError> {
    let mtm = MainThreadMarker::new().ok_or(ShellError::NotMainThread)?;
    let app = NSApplication::sharedApplication(mtm);
    let command = NSEventModifierFlags::Command;
    let option = NSEventModifierFlags::Option;
    let shift = NSEventModifierFlags::Shift;

    // The app menu. The App Store build closes to the menu bar on Command-Q and quits on
    // Command-Option-Q, since Command-Shift-Q is macOS's Log Out (research R-098).
    let services = NSMenu::new(mtm);
    let services_item = item(mtm, "Services", None, "", command, Target::Standard);
    services_item.setSubmenu(Some(&services));
    let mut app_items = vec![
        Some(item(
            mtm,
            "About Midi Harbor",
            Some(sel!(orderFrontStandardAboutPanel:)),
            "",
            command,
            Target::Standard,
        )),
        None,
        Some(item(
            mtm,
            "Settings…",
            Some(sel!(showSettings:)),
            ",",
            command,
            Target::Shell,
        )),
        None,
        Some(services_item),
        None,
    ];
    if store {
        app_items.push(Some(item(
            mtm,
            "Close to Menu Bar",
            Some(sel!(closeWindow:)),
            "q",
            command,
            Target::Shell,
        )));
    }
    app_items.extend([
        Some(item(
            mtm,
            "Hide Midi Harbor",
            Some(sel!(hide:)),
            "h",
            command,
            Target::Standard,
        )),
        Some(item(
            mtm,
            "Hide Others",
            Some(sel!(hideOtherApplications:)),
            "h",
            command | option,
            Target::Standard,
        )),
        Some(item(
            mtm,
            "Show All",
            Some(sel!(unhideAllApplications:)),
            "",
            command,
            Target::Standard,
        )),
        None,
        Some(item(
            mtm,
            "Quit Midi Harbor",
            Some(sel!(terminate:)),
            "q",
            if store { command | option } else { command },
            Target::Standard,
        )),
    ]);
    let app_menu = menu(mtm, "Midi Harbor", &app_items);

    // The File menu. The window draws its own title bar, so AppKit's own close and zoom are
    // unavailable to it, and the window closes and zooms itself (research R-102).
    let file_menu = menu(
        mtm,
        "File",
        &[
            Some(item(
                mtm,
                "New Virtual Port…",
                Some(sel!(newVirtualPort:)),
                "n",
                command,
                Target::Shell,
            )),
            Some(item(
                mtm,
                "New Network Port…",
                Some(sel!(newNetworkPort:)),
                "n",
                command | option,
                Target::Shell,
            )),
            Some(item(
                mtm,
                "New Route…",
                Some(sel!(newRoute:)),
                "n",
                command | shift,
                Target::Shell,
            )),
            None,
            Some(item(
                mtm,
                "Export Diagnostic Report",
                Some(sel!(exportDiagnostics:)),
                "",
                command,
                Target::Shell,
            )),
            None,
            Some(item(
                mtm,
                "Close Window",
                Some(sel!(closeWindow:)),
                "w",
                command,
                Target::Shell,
            )),
        ],
    );

    // The Edit menu, whose items hand their shortcut to the window's text fields (research
    // R-102). macOS adds its own dictation and emoji items to a menu titled Edit.
    let edit_items: Vec<_> = EDIT_ITEMS
        .iter()
        .map(|(title, key, _)| {
            Some(item(
                mtm,
                title,
                Some(sel!(editKey:)),
                key,
                command,
                Target::Shell,
            ))
        })
        .collect();
    let edit_menu = menu(mtm, "Edit", &edit_items);

    // The View menu, a page on each of Command-1 onwards.
    let page_items: Vec<_> = pages
        .iter()
        .enumerate()
        .map(|(position, title)| {
            let key = if position < 9 {
                (position + 1).to_string()
            } else {
                String::new()
            };
            let entry = item(
                mtm,
                title,
                Some(sel!(showPage:)),
                &key,
                command,
                Target::Shell,
            );
            entry.setTag(isize::try_from(position).unwrap_or(isize::MAX));
            Some(entry)
        })
        .collect();
    let view_menu = menu(mtm, "View", &page_items);

    // The Window menu, to which AppKit adds the window itself.
    let window_menu = menu(
        mtm,
        "Window",
        &[
            Some(item(
                mtm,
                "Minimize",
                Some(sel!(performMiniaturize:)),
                "m",
                command,
                Target::Standard,
            )),
            Some(item(
                mtm,
                "Zoom",
                Some(sel!(zoomWindow:)),
                "",
                command,
                Target::Shell,
            )),
            None,
            Some(item(
                mtm,
                "Bring All to Front",
                Some(sel!(arrangeInFront:)),
                "",
                command,
                Target::Standard,
            )),
        ],
    );

    // The Help menu, to which AppKit adds its search field.
    let help_items: Vec<_> = ["Midi Harbor Help", "Report an Issue"]
        .iter()
        .enumerate()
        .map(|(tag, title)| {
            let entry = item(
                mtm,
                title,
                Some(sel!(openLink:)),
                "",
                command,
                Target::Shell,
            );
            entry.setTag(isize::try_from(tag).unwrap_or(isize::MAX));
            Some(entry)
        })
        .collect();
    let help_menu = menu(mtm, "Help", &help_items);

    let bar = NSMenu::new(mtm);
    for submenu in [
        &app_menu,
        &file_menu,
        &edit_menu,
        &view_menu,
        &window_menu,
        &help_menu,
    ] {
        let holder = NSMenuItem::new(mtm);
        holder.setSubmenu(Some(submenu));
        bar.addItem(&holder);
    }
    app.setMainMenu(Some(&bar));
    app.setServicesMenu(Some(&services));
    app.setWindowsMenu(Some(&window_menu));
    app.setHelpMenu(Some(&help_menu));

    if store {
        install_status_item(mtm);
    }
    Ok(())
}

/// Adds the App Store build's menu bar item.
fn install_status_item(mtm: MainThreadMarker) {
    let command = NSEventModifierFlags::Command;
    let status = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = status.button(mtm) {
        let data = NSData::with_bytes(MENU_BAR_ICON);
        if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
            image.setSize(NSSize::new(18.0, 18.0));
            image.setTemplate(true);
            button.setImage(Some(&image));
        }
        button.setToolTip(Some(&NSString::from_str("Midi Harbor")));
    }
    let status_menu = menu(
        mtm,
        "Midi Harbor",
        &[
            Some(item(
                mtm,
                "Open Midi Harbor",
                Some(sel!(openWindow:)),
                "",
                command,
                Target::Shell,
            )),
            None,
            Some(item(
                mtm,
                "Quit Midi Harbor",
                Some(sel!(terminate:)),
                "",
                command,
                Target::Standard,
            )),
        ],
    );
    status.setMenu(Some(&status_menu));
    STATUS_ITEM.with(|slot| *slot.borrow_mut() = Some(status));
}

/// Hands an Edit menu item's shortcut to the key window's first responder, as if typed.
///
/// A menu item takes its key equivalent before the window sees it, and the window's text fields
/// act only on keys (research R-102). A shortcut that was typed is handed on as it came. A click
/// on the item is sent as the key pressed and released with Command, followed by the modifiers
/// actually held, so the window does not go on believing Command is down.
pub(super) fn resend_edit_key(mtm: MainThreadMarker, item: &NSMenuItem) {
    let app = NSApplication::sharedApplication(mtm);
    let Some(responder) = app.keyWindow().and_then(|window| window.firstResponder()) else {
        return;
    };

    // Typed: the event that matched the item.
    if let Some(event) = app.currentEvent()
        && event.r#type() == NSEventType::KeyDown
    {
        responder.keyDown(&event);
        return;
    }

    // Clicked: the key as typed on the ANSI layout.
    let key = item.keyEquivalent().to_string();
    let Some(&(_, _, code)) = EDIT_ITEMS.iter().find(|(_, each, _)| *each == key) else {
        return;
    };
    let window = app.keyWindow().map_or(0, |window| window.windowNumber());
    let command = NSEventModifierFlags::Command;
    let held = NSEvent::modifierFlags_class();
    let press = key_event(NSEventType::KeyDown, command, window, &key, code);
    let release = key_event(NSEventType::KeyUp, command, window, &key, code);
    // Key code 0 in a modifier change carries no key, only the modifiers held.
    let restore = key_event(NSEventType::FlagsChanged, held, window, "", 0);
    if let (Some(press), Some(release), Some(restore)) = (press, release, restore) {
        responder.keyDown(&press);
        responder.keyUp(&release);
        responder.flagsChanged(&restore);
    }
}

/// Builds a key event for the window numbered `window`.
fn key_event(
    kind: NSEventType,
    modifiers: NSEventModifierFlags,
    window: isize,
    characters: &str,
    code: u16,
) -> Option<Retained<NSEvent>> {
    let characters = NSString::from_str(characters);
    NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
        kind,
        NSPoint::new(0.0, 0.0),
        modifiers,
        0.0,
        window,
        None,
        &characters,
        &characters,
        false,
        code,
    )
}

/// Opens the web page with the tag `tag` in the default browser.
pub(super) fn open_link(tag: isize) {
    let link = usize::try_from(tag).ok().and_then(|tag| LINKS.get(tag));
    let Some(url) = link.and_then(|link| NSURL::URLWithString(&NSString::from_str(link))) else {
        return;
    };
    if !NSWorkspace::sharedWorkspace().openURL(&url) {
        warn!(url = link.unwrap_or(&""), "failed to open a help page");
    }
}

/// Builds a menu from items, `None` standing for a separator.
fn menu(
    mtm: MainThreadMarker,
    title: &str,
    items: &[Option<Retained<NSMenuItem>>],
) -> Retained<NSMenu> {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
    for entry in items {
        match entry {
            Some(item) => menu.addItem(item),
            None => menu.addItem(&NSMenuItem::separatorItem(mtm)),
        }
    }
    menu
}

/// Builds a menu item with a key equivalent held with `modifiers`, sending `action` to `target`.
fn item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<Sel>,
    key: &str,
    modifiers: NSEventModifierFlags,
    target: Target,
) -> Retained<NSMenuItem> {
    // SAFETY: the action is a selector the target responds to: the menu target's own methods, or
    // a standard action the responder chain answers.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        )
    };
    item.setKeyEquivalentModifierMask(modifiers);
    if let Target::Shell = target
        && let Some(shell) = shell()
    {
        let object: &AnyObject = &shell;
        // SAFETY: the target outlives the menu, held for the life of the process in the
        // delegate module, so the item's weak target never dangles.
        unsafe { item.setTarget(Some(object)) };
    }
    item
}
