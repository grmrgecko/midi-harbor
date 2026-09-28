# Tasks: App Store Submission

Tasks by their numbers in the project-wide sequence, which continues across every spec.

- [x] T245 Build the App Store package with `make appstore`, per FR-S01 to FR-S03 — done: with `.signing/app-store.provisionprofile`, `bundle.sh --helper` embeds the profile, adds its App ID and team to the app's entitlements after checking the App ID is the bundle's, signs both executables with the keychain's Apple Distribution identity, sets the build number to the time in UTC and `ITSAppUsesNonExemptEncryption` to false, and wraps the app in a package signed with Mac Installer Distribution; without a profile it builds as before. Checked on the owner's Mac (R-103); not unit tested, since it is a build script.
