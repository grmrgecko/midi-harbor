# Research: macOS Menus

The investigation behind this spec, under its number in the project-wide research log.

---

## R-102: Menus over a window winit draws

**Status**: **VERIFIED** (2026-09-28), in the direct build. Built as T244.

The menus replace winit's default as the App Store build's did (R-098), now in both builds: the
menus' target object is created in both, and made the app delegate only in App Store mode, so
quitting the direct build stays winit's. What the menus do in the window arrives as a
`ShellAction` on the same channel.

**AppKit's own close and zoom are unavailable.** libcosmic draws its own title bar, so the
window has no close or zoom button, and AppKit disables `performClose:` and `performZoom:` for
it. Close Window and Zoom go to the app instead, which closes the window through iced, as its
own close button does, or toggles it maximized.

**The Edit menu has no responder to send to.** winit's view implements none of `cut:`, `copy:`,
`paste:` or `selectAll:`. libcosmic's text field acts on the keys alone, reading a Command
shortcut from the key event's logical key, or its physical key for a non-Latin one
(`text_input/input.rs`, libcosmic 87ab817). A menu item takes its key equivalent before the
window sees it, so an item bound to ⌘C would swallow the shortcut the field needs. The items
instead hand the shortcut on to the key window's first responder:

- Typed, the event that matched the item is passed to `keyDown:` as it came.
- Clicked, the key is sent pressed and released with Command, then a modifier change with the
  modifiers actually held. winit reads the modifiers from each key event
  (`winit-appkit/src/view.rs`, `update_modifiers`), so without the last event the window would
  go on believing Command was down, and the next letter typed would be a shortcut. A modifier
  change with key code 0 carries no key, only the modifiers.

The clicked key uses its ANSI key code. On a layout that moves A, such as AZERTY, a clicked
Select All reaches the field as the letter at that position; the shortcut typed is unaffected.

**Evidence**, the direct build against a scratch daemon, driven through System Events:

- The menu bar read back as contracts/menus.md, every item enabled while the window was key.
- View > Routes and ⌘, showed their pages. File > New Virtual Port… opened its dialog over
  Endpoints.
- In the dialog's name field, a clicked Select All then Copy put "Bus Alpha" on the clipboard.
  A letter typed next replaced the selection rather than acting as a shortcut. ⌘A, ⌘C, ⌘X,
  a clicked Paste and ⌘V each did what the key does alone.
- Zoom filled the screen. Close Window ended the window's process.

**Not checked**: the App Store build, whose menus are built by the same code with its three
differences, and Help, which opens the browser.
