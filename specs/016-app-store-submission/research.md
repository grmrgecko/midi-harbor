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
