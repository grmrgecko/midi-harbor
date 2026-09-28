# Contract: menus and shortcuts in App Store mode

What the user can click and press. Only App Store mode shows the menu bar item.

## Menu bar menu

Shown while the app is active, which is while its window is open. Both builds share the menus in
[015's contract](../../015-mac-menus/contracts/menus.md), which began here. App Store mode differs
in three items:

| Item | Shortcut | Does |
|---|---|---|
| Close to Menu Bar | ⌘Q | Hides the window and the Dock icon; everything keeps running |
| Quit Midi Harbor | ⌥⌘Q | Stops the daemon, then exits |
| File > Close Window | ⌘W | Same as Close to Menu Bar |

The window's close button does the same as Close to Menu Bar.

⇧⌘Q is not used: it is macOS's Log Out, in the Apple menu, and it takes precedence over the app's
menu (research R-098).

## Menu bar item

Always shown in App Store mode. The anchor from the app icon, alone, drawn as a template image so
it follows the menu bar's appearance (`packaging/macos/MenuBarIcon.svg`). Clicking it opens:

| Item | Does |
|---|---|
| Open Midi Harbor | Shows the window and the Dock icon, and brings the window forward |
| — | |
| Quit Midi Harbor | Stops the daemon, then exits |

## Quitting

Every way of quitting entirely ends the same way: Quit Midi Harbor from either menu, ⌥⌘Q, and a
logout, restart or shutdown. The window's open state is recorded first (data-model.md), then the
daemon is asked to stop, releasing held notes, and the app exits once it has. A daemon the app
started is sent SIGTERM and killed after 5 s if it has not stopped; one it found running, left by a
window that crashed, is asked through the contract's `StopDaemon`, since the sandbox forbids
signalling it, and the app exits after 5 s whether or not it has gone.

## Settings

App Store mode adds **Start at login** to the Settings page, a switch with the states in
data-model.md, and removes the service offer the direct build shows when no daemon runs.
