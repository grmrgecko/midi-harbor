# Packaging

## Releases

A release is built with [GoReleaser] from one machine with Docker, macOS or Linux, inside an
image that adds Rust to GoReleaser's cross-compiling image. It builds:

| Artifact | Platforms |
|---|---|
| `tar.gz` archive | Linux x86_64 and arm64 |
| `tar.gz` archive, headless only | Linux and macOS, x86_64 and arm64 |
| `zip` archive | Windows x86_64 |
| `.deb` and `.rpm`, `midi-harbor` and `midi-harbor-headless` | Linux x86_64 and arm64 |
| `.dmg` holding `Midi Harbor.app`, the license and the docs | macOS, one universal binary |
| `checksums.txt` | all of them |

The Linux builds link against Debian 12's libraries and need glibc 2.35 or newer, so they run on
Debian 12, Ubuntu 22.04, Fedora 36, RHEL 10 and anything newer. The `headless` builds use
`--no-default-features` and contain none of the GUI toolkit. The two packages install the same
binary, so each conflicts with the other. On the Mac the graphical interface ships only in the
app, so the only macOS archives are the headless ones.

Set up once, and again after changing the libraries Midi Harbor links against or the Rust
version:

```bash
make build-docker-image    # packaging/cross/Dockerfile
make build-sysroot         # packaging/sysroot/, see its README
```

A release builds the sysroots by itself when they are missing, as they are after `make clean`.

Then:

```bash
make snapshot              # everything into dist/, publishing nothing
make release               # the commit tagged v<VERSION>, to GitHub
```

The version is in `VERSION` at the root, and nowhere else: the binary reads it as it compiles,
and every package, archive and the app take it from there. To release, change `VERSION`, commit,
tag the commit `v` and the version, push the tag, and run `make release`, which refuses a commit
without that tag. A snapshot is versioned `<VERSION>-SNAPSHOT-<commit>`.

`make release` reads `GITHUB_TOKEN` from `.release-env`, which git ignores. The packages name
their maintainer from `MIDI_HARBOR_MAINTAINER`, or from git's `user.name` and `user.email` when
that is not set.

[GoReleaser]: https://goreleaser.com

## Icon

`Icon.svg` is the icon. The Linux packages install it as it is, and `packaging/icons.sh` renders it
into `packaging/macos/AppIcon.icns` and `packaging/windows/midi-harbor.ico`, which are checked in
so building needs none of its tools. Run the script on a Mac after changing the icon. The App
Store build's menu bar item uses `packaging/macos/MenuBarIcon.svg`, the anchor alone, which the
script renders to `MenuBarIcon.png`; keep it in step with `Icon.svg` by hand.

The Windows executable carries the icon as a resource, compiled by `windres` from mingw-w64, or
`rc.exe` when building with MSVC. A Windows build fails without one.

## macOS app and disk image

GoReleaser makes app bundles and disk images only in its paid edition, so
`packaging/macos/bundle.sh` makes them instead. GoReleaser runs it on the universal binary it
builds, which puts `Midi Harbor.app` in `dist/macos/` and `Midi-Harbor-<version>.dmg` in
`dist/` beside the other artifacts, and the release publishes the disk image. The image holds the
app, a link to Applications, and the license and docs the archives carry. In Docker the app and
disk image are signed with rcodesign.

To build them on a Mac without Docker, run `packaging/macos/build.sh`, which writes to
`target/package/macos/`. It signs as [Signing and notarizing](#signing-and-notarizing) describes,
and builds a universal binary when both Rust targets are installed (`rustup target add
x86_64-apple-darwin aarch64-apple-darwin`), and a binary for the building Mac otherwise.

`make appstore`, which runs `packaging/macos/build.sh --app-store`, builds the Mac App Store
variant instead, into `target/package/macos-app-store/`: sandboxed, requiring macOS 13, starting
without a Dock icon, and carrying the headless build as `Contents/MacOS/midi-harbor-daemon`, the
daemon the app starts. The app is signed with `packaging/macos/app-store.entitlements` and the
helper with `packaging/macos/helper.entitlements`, which lets it take the app's sandbox. It makes
no disk image.

With a provisioning profile in `.signing/app-store.provisionprofile` it is built for submission,
as `Midi-Harbor-<version>.pkg` beside the app:

- the profile is embedded as `Contents/embedded.provisionprofile`, and the App ID and team it
  names are added to the app's entitlements; a profile for another App ID is refused;
- both executables are signed with the keychain's Apple Distribution identity, or the one
  `MIDI_HARBOR_SIGNING_IDENTITY` names;
- the build number, `CFBundleVersion`, is the build's time in UTC (`202609281937`), since every
  upload needs a higher one, while the version shown stays `VERSION`;
- `ITSAppUsesNonExemptEncryption` is false: hashing and random numbers are all the encryption
  Midi Harbor has;
- the installer package is signed with the keychain's Mac Installer Distribution identity, which
  the keychain names "3rd Party Mac Developer Installer".

Upload the package with Apple's Transporter app, then choose the build in App Store Connect for
TestFlight or review. An App Store build does not run until the App Store installs it. Without
the profile, the app is signed ad hoc, or with `MIDI_HARBOR_SIGNING_IDENTITY`, to run on this Mac.

Setting up for submission, once:

1. In Xcode, **Settings → Accounts → Manage Certificates**, add an **Apple Distribution** and a
   **Mac Installer Distribution** certificate. If the keychain calls them untrusted
   (`security find-identity -v` leaves them out), install Apple's
   [WWDR G3 intermediate](https://www.apple.com/certificateauthority/AppleWWDRCAG3.cer).
2. In the developer portal, **Identifiers**, register an explicit macOS App ID for
   `com.mrgeckosmedia.MidiHarbor`, with no capabilities: every entitlement the build uses is a
   sandbox one that signing grants.
3. In **Profiles**, generate a **Mac App Store Connect** distribution profile for that App ID and
   the Apple Distribution certificate, and save it as `.signing/app-store.provisionprofile`. It
   lasts a year, and is made again when it or the certificate is renewed.
4. In App Store Connect, add the app with that bundle ID.

The app is signed with the hardened runtime either way, as notarization requires. Bluetooth
permission is granted to the signed app, which is why the daemon should run from inside it:
`service install`, run from the app's own binary, registers that binary with launchd.

### Signing and notarizing

The script signs with a Developer ID certificate and notarizes when it finds the credentials, and
signs ad hoc otherwise, so a build without them still works. In Docker it looks in `.signing/`,
which git ignores:

| File | What it is |
|---|---|
| `developer-id.p12` | the Developer ID Application certificate and its private key |
| `developer-id.p12.password` | the password the `.p12` was exported with |
| `notary-api-key.json` | an App Store Connect API key, which notarization uses |

With the certificate, the app is signed with the hardened runtime and the disk image is signed.
With the API key as well, each is submitted to Apple's notary service and has its ticket stapled,
the app before it goes on the image so a copy dragged out of it carries its own. That runs on
every build that finds them, `make snapshot` included, and adds a few minutes.

To make them, once:

1. In Xcode, **Settings → Accounts → Manage Certificates**, add a **Developer ID Application**
   certificate; only the team's Account Holder can. In Keychain Access, export it with its
   private key from **My Certificates** as `.signing/developer-id.p12`, and write the password to
   `.signing/developer-id.p12.password`.
   Keychain Access exports the older `.p12` encryption rcodesign reads; one made by OpenSSL 3
   needs `-legacy`.
2. In App Store Connect, **Users and Access → Integrations → App Store Connect API**, generate a
   team key with the **Developer** role. Download `AuthKey_<key ID>.p8`, which Apple offers once,
   and note the issuer ID above the list of keys. Encode it for rcodesign:

   ```bash
   docker run --rm -v "$PWD:/src" -w /src --entrypoint rcodesign midi-harbor-cross:latest \
       encode-app-store-connect-api-key -o .signing/notary-api-key.json \
       <issuer ID> <key ID> .signing/AuthKey_<key ID>.p8
   ```

`packaging/macos/build.sh`, on a Mac, signs with the identity `MIDI_HARBOR_SIGNING_IDENTITY` names,
or else the first Developer ID Application identity in the keychain, and notarizes with
`notarytool` when `.signing/notary-api-key.json` exists. The `.p12` is not needed there.
