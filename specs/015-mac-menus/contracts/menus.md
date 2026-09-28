# Contract: the menus on macOS

Both builds. What differs in App Store mode is marked; its menu bar item is in
[014's contract](../../014-mac-app-store-mode/contracts/menus.md).

**Midi Harbor**

| Item | Shortcut | Does |
|---|---|---|
| About Midi Harbor | | The standard About panel |
| — | | |
| Settings… | ⌘, | Shows the Settings page |
| — | | |
| Services | | The standard Services menu |
| — | | |
| Close to Menu Bar | ⌘Q | App Store mode only: hides the window and the Dock icon |
| Hide Midi Harbor | ⌘H | The standard Hide |
| Hide Others | ⌥⌘H | The standard Hide Others |
| Show All | | The standard Show All |
| — | | |
| Quit Midi Harbor | ⌘Q, or ⌥⌘Q in App Store mode | Quits; in App Store mode stops the daemon first |

**File**

| Item | Shortcut | Does |
|---|---|---|
| New Virtual Port… | ⌘N | Shows Endpoints and opens the new virtual port dialog |
| New Network Port… | ⌥⌘N | Shows Endpoints and opens the new network port dialog |
| New Route… | ⇧⌘N | Shows Routes and opens the new route dialog |
| — | | |
| Export Diagnostic Report | | Shows Settings, where the report says where it was saved |
| — | | |
| Close Window | ⌘W | Ends the window, or closes it to the menu bar in App Store mode |

The New items do nothing while no daemon answers.

**Edit**

| Item | Shortcut |
|---|---|
| Cut | ⌘X |
| Copy | ⌘C |
| Paste | ⌘V |
| Select All | ⌘A |

macOS adds its dictation and emoji items below them.

**View**

Each page in the order the sidebar lists them, on ⌘1 to ⌘6: Endpoints, Routes, Bluetooth,
Activity, Monitor, Settings. macOS adds Enter Full Screen.

**Window**

| Item | Shortcut | Does |
|---|---|---|
| Minimize | ⌘M | The standard Minimize |
| Zoom | | Maximizes the window, or restores it |
| — | | |
| Bring All to Front | | The standard Bring All to Front |

macOS adds its tiling items and the window itself.

**Help**

| Item | Opens |
|---|---|
| Midi Harbor Help | `https://github.com/grmrgecko/midi-harbor/blob/main/docs/README.md` |
| Report an Issue | `https://github.com/grmrgecko/midi-harbor/issues` |

macOS adds its search field.
