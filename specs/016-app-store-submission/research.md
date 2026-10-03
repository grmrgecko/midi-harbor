# Research: App Store Submission

The investigation behind this spec, under its number in the project-wide research log.

---

## R-103: What App Store Connect needs beyond the sandboxed app

**Status**: **VERIFIED** (2026-09-28) locally; not yet uploaded. Built as T245.

**The App ID needs no capabilities.** Every entitlement the build uses (the sandbox, network
client and server, Bluetooth, USB, and the helper's inherit) is a sandbox entitlement that
signing grants. The login item uses `SMAppService`, and the helper shares the app's container by
inheriting its sandbox, so App Groups are not needed either. The profile Apple generated for the
bare App ID carries `com.apple.application-identifier`, `com.apple.developer.team-identifier` and
`keychain-access-groups` (`<team>.*`). The first two must be in the app's signed entitlements to
match the profile; keychain access groups are not used, so they are left out.

**The certificates came untrusted.** Xcode created the Apple Distribution and Mac Installer
Distribution certificates, but `security find-identity` called both `CSSMERR_TP_NOT_TRUSTED`:
they are issued by Apple's WWDR G3 intermediate, which this Mac lacked. The Developer ID
certificate, issued by another intermediate, was unaffected. Installing
`AppleWWDRCAG3.cer` made both valid. The keychain names the installer certificate
"3rd Party Mac Developer Installer", its older name, which is what the build looks for.

**The helper keeps only the sandbox and inherit.** A helper signed to inherit its parent's
sandbox must carry exactly those two entitlements, so the profile's identifiers go on the app
alone.

**The build number is the build's time.** App Store Connect refuses a build number it has seen
for the app, so `CFBundleVersion` is the time in UTC, `%Y%m%d%H%M`, twelve digits. The version
shown, `CFBundleShortVersionString`, stays `VERSION`. A count of commits was rejected, since the
owner rewrites history before publishing and the count would fall.

**Encryption.** The macOS build links no TLS; `cargo tree` finds only `sha2` and `chacha20`,
used for hashing and random numbers, which are exempt. `ITSAppUsesNonExemptEncryption` is false,
so App Store Connect does not ask at every upload.

**Extended attributes are stripped before signing.** The first upload was refused with error
91109: `Contents/embedded.provisionprofile` carried `com.apple.quarantine`, which the browser set
on the downloaded profile and `cp` kept. `xattr -cr` on the app before signing removes it and
anything else a copied file brings. `com.apple.provenance` stays on every file, since macOS
sets it on whatever an app writes and it cannot be cleared; App Store Connect's check names only
quarantine.

**Evidence**, `make appstore` on the development Mac:

- `lipo -archs`: `x86_64 arm64` for both executables, once `x86_64-apple-darwin` was installed.
- `codesign -dvv`: both signed by "Apple Distribution: James Coleman (8XMLMKNPUT)" with the
  hardened runtime. The app's entitlements are the sandbox list plus
  `8XMLMKNPUT.com.mrgeckosmedia.MidiHarbor` and `8XMLMKNPUT`. The helper's are the sandbox and
  inherit.
- `Contents/embedded.provisionprofile` present; `codesign --verify --deep --strict` passes.
- `pkgutil --check-signature`: signed by "3rd Party Mac Developer Installer: James Coleman
  (8XMLMKNPUT)", which it reports as a development certificate, as it does every App Store
  installer certificate.

**Not checked**: an upload after the quarantine fix; the expanded package carries no attribute but
`com.apple.provenance`.

---

## R-110: What App Review's automated check refused

**Status**: **VERIFIED** (2026-10-03) locally; resubmitted the same day, verdict pending. Built as T255.

The first submission came back with two automated messages before any person reviewed it.

**The private API was winit's.** The check named `_CGSSetWindowBackgroundBlurRadius`. Nothing in
this project calls it: winit's macOS backend does, in `set_blur`, with `CGSMainConnectionID`
beside it, and libcosmic's iced pins `pop-os/winit` at `9567503`. No window here asks for blur,
but the linker keeps the import whether or not the call is reached, and the check reads imports.
winit 0.30.13 and the fork's other revisions in the cargo cache carry the same call, so no
update removes it.

**Decision**: `grmrgecko/winit`, branch `no-private-blur`, is `9567503` with the two declarations
removed and `set_blur` doing nothing on macOS. The root `Cargo.toml` patches every winit crate to
it, since patching `winit-appkit` alone would leave two copies of `winit-core`. It moves with the
libcosmic revision. The direct-download build takes the same patch, having no use for blur
either. `bundle.sh` now refuses to sign an App Store bundle whose executables import any `_CGS`
followed by a capital, the prefix of the window server's private calls; the public
`CGShieldingWindowLevel` does not match.

**The server entitlement stays.** The second message said `com.apple.security.network.server` had
no matching functionality. It has: each network port binds UDP ports and takes session
invitations from other machines (R-097). The helper does the listening and carries only the
sandbox and inherit, which is the likely reason the check saw none, though Apple does not say.
The answer is a reply and a note in App Review Information describing RTP-MIDI and how to see a
port from Audio MIDI Setup on another Mac, not a change to the build.

**Evidence**, `make appstore` on the development Mac:

- Before, `nm -u` on the full binary listed `_CGSMainConnectionID` and
  `_CGSSetWindowBackgroundBlurRadius` for both architectures; the headless binary listed neither.
- After, `nm -u -arch arm64` and `-arch x86_64` list no `_CGS` private symbol in either
  executable, and `strings` finds no `CGSSetWindowBackgroundBlurRadius` in the app.
- `codesign --verify --deep --strict` passes on the bundle.

**Not checked**: App Review's verdict on the rebuilt package, and its answer to the reply.
