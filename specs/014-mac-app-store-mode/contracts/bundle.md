# Contract: the App Store variant of the bundle

Built by `packaging/macos/build.sh --app-store` on a Mac, into `target/package/macos-app-store/`.

```text
Midi Harbor.app/Contents/
├── Info.plist                  as the direct build's, with LSMinimumSystemVersion 13.0 and LSUIElement true
├── MacOS/
│   ├── midi-harbor             the full build, signed with app-store.entitlements
│   └── midi-harbor-daemon      the headless build, signed with helper.entitlements
└── Resources/AppIcon.icns
```

Both executables are universal when both Rust targets are installed.

**`packaging/macos/app-store.entitlements`**

```text
com.apple.security.app-sandbox          true
com.apple.security.network.client       true
com.apple.security.network.server       true
com.apple.security.device.bluetooth     true
com.apple.security.device.usb           true
```

**`packaging/macos/helper.entitlements`**

```text
com.apple.security.app-sandbox          true
com.apple.security.inherit              true
```

The helper is signed first and the bundle after, each with the hardened runtime, ad hoc unless
`MIDI_HARBOR_SIGNING_IDENTITY` names a certificate. `codesign --verify --strict` passes on the
bundle, and `codesign -d --entitlements -` on each executable shows exactly the lists above.

The bundle identifier stays `com.mrgeckosmedia.MidiHarbor`. A disk image is not made for this
variant: the App Store takes an installer package built with the owner's certificates.
