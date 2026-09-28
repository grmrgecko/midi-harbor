# Tasks: macOS Menus

Tasks by their numbers in the project-wide sequence, which continues across every spec.

- [x] T244 Give the window on macOS an app menu, File, Edit, View, Window and Help in both builds, per FR-U01 to FR-U03 — done: `appkit::install_menus` builds them from the page titles the window passes, the target object exists in both builds and is the app delegate only in App Store mode, and the window handles the new `ShellAction`s; Edit hands its shortcut to the window (R-102). Checked live in the direct build (R-102); not unit tested, since it is AppKit wiring with no format of its own.
