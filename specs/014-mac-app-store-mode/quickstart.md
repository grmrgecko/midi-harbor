# Quickstart: validating Mac App Store mode

Run on a Mac with macOS 13 or newer, a desktop session, and both Rust macOS targets. Scenarios 1
to 4 need nothing from the owner. Scenario 5 adds a login item and needs a logout, and scenario 6
kills the MIDI server, which interrupts every MIDI application on the Mac; ask before either.

## Automated

```bash
make test-integration     # tests/app_store_mode.rs and the supervisor stop, with the rest
```

These run the real binary with `APP_SANDBOX_CONTAINER_ID` set: service commands refused with exit
4, service installation reported unavailable, the socket at `$TMPDIR/daemon.sock`, and a
supervised daemon stopped with its held notes released.

## Build the variant

```bash
packaging/macos/build.sh --app-store
codesign --verify --strict "target/package/macos-app-store/Midi Harbor.app"
codesign -d --entitlements - "target/package/macos-app-store/Midi Harbor.app/Contents/MacOS/midi-harbor-daemon"
```

Expected: the bundle verifies, and each executable's entitlements match
[contracts/bundle.md](contracts/bundle.md).

## 1. Connections keep running from the menu bar (User Story 1)

1. Open the variant from Finder. The window opens, the Dock icon shows, and a Midi Harbor item is
   in the menu bar. `pgrep -fl midi-harbor-daemon` shows one daemon.
2. Create a virtual port "AS Keys" and a network port "AS Stage", connected to another machine
   (the Arch VM at 192.0.2.41 with a scratch daemon, by address), and route one to the other.
3. Close the window with its close button. Expected: no Dock icon, the menu bar item remains, and
   a note sent into "AS Keys" from any MIDI application arrives on the far machine, watched there
   with `midi-harbor monitor`.
4. Choose Open Midi Harbor from the menu bar item. Expected: within a second the window shows
   current state, and the Dock icon is back.
5. Choose Quit Midi Harbor. Expected: `pgrep` finds no daemon, "AS Keys" is gone from other
   applications, and a note held across the quit was released on the far machine.

## 2. ⌘Q and ⌥⌘Q (User Story 2)

1. With the window focused, open the app menu. Expected: the items and shortcuts in
   [contracts/menus.md](contracts/menus.md).
2. ⌘Q. Expected: as step 1.3 above.
3. Reopen, ⌥⌘Q. Expected: as step 1.5.

## 3. A crashed window, and a second launch (FR-A11, FR-A14)

1. With the app running and a note routed, `kill -9` the app's own process (not the daemon).
   Expected: the daemon keeps running and MIDI keeps flowing.
2. Open the app from Finder. Expected: one daemon still, the window shows its state, and Quit
   stops it.
3. With the app running, open it again from Finder. Expected: the running app's window comes
   forward and no second process starts.

## 4. What the sandbox allows (User Story 4)

1. Scan for Bluetooth devices from the window. Expected: macOS asks once for Bluetooth for Midi
   Harbor, and devices in range are listed.
2. With a USB MIDI device attached, `device list` from the variant's executable. Expected: the
   device with its maker, model and serial. Repeat with a build whose app entitlements lack
   `device.usb`; if nothing differs, drop that entitlement (R-100).
3. `capabilities` from the variant's executable. Expected: every capability as the direct build
   reports it, except service installation (contracts/cli.md).

## 5. Start at login (User Story 3; with the owner)

1. In Settings, turn on Start at login. Expected: on, or waiting with where to approve it in
   System Settings.
2. Quit with the window open, log out and back in. Expected: the window opens and the ports are
   back.
3. Close the window, quit from the menu bar item, log out and in. Expected: only the menu bar item,
   no Dock icon, and the ports are back.
4. Turn Start at login off. Expected: System Settings no longer lists Midi Harbor.

## 6. The MIDI server dies (with the owner)

1. With the app running and a virtual port routed, `killall MIDIServer`. Expected: the window
   shows the daemon unreachable for a few seconds, then everything back, with the history saying
   why the daemon restarted.

## Clean up

Delete the test ports through the variant's own executable (the container is not writable from
outside it), remove `target/package/macos-app-store`, and turn Start at login off if it was
turned on.
