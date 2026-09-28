# Feature Specification: macOS Menus

**Created**: 2026-09-28

**Status**: Implemented; checked in the direct build's window on 2026-09-28 against a scratch
daemon, every menu driven through System Events

**Input**: User request: "The menu options on MacOS are small, normally you'd have a `File`,
`Edit`, `Window`, and `Help` menu."

The direct-download build showed winit's default menu: the app menu alone. The App Store build
had its own app and Window menus (014). Both builds now show the menus a Mac app is expected to
have, with the window's own commands in them.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Working from the menu bar (Priority: P1)

A Mac user presses ⌘N and the dialog for a new virtual port opens over the Endpoints page. They
paste a name copied from elsewhere with ⌘V, or with Edit > Paste. ⌘2 shows the Routes page and
⌘, the Settings page. Help > Midi Harbor Help opens the documentation.

**Why this priority**: it is the whole request.

**Independent Test**: Open the window, read its menus through the accessibility interface, and
choose each item, by click and by shortcut, checking what the window does.

**Acceptance Scenarios**:

1. **Given** the window is open, **When** the menu bar is read, **Then** it holds the app menu,
   File, Edit, View, Window and Help, as contracts/menus.md lists them.
2. **Given** a text field has the focus, **When** Cut, Copy, Paste or Select All is chosen, by
   click or by shortcut, **Then** the field acts as it does on the key alone, and typing
   afterwards is not read as a shortcut.
3. **Given** no daemon answers, **When** a New item is chosen, **Then** nothing opens, since the
   dialog could not create anything.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-U01**: The window on macOS MUST show an app menu, File, Edit, View, Window and Help, in
  both builds, with the shortcuts contracts/menus.md gives.
- **FR-U02**: The Edit menu's items MUST act on the focused text field whether chosen by click
  or by shortcut.
- **FR-U03**: The App Store build MUST keep what 014 gave its menus: Command-Q closing to the
  menu bar and Command-Option-Q quitting.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-U01**: Every command the menus list works from the menu bar, by click and by shortcut.

## Assumptions

- A View menu lists the pages, on ⌘1 onwards, beside the four menus the owner named, as Mac
  apps put navigation there.
- Undo and Redo are left out: the text fields keep no history.
- Help opens the project's documentation and issue list on GitHub in the default browser.
