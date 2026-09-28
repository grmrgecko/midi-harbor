# Feature Specification: App Store Submission

**Created**: 2026-09-28

**Status**: Implemented; built and checked on 2026-09-28 with the owner's certificates and
profile, not yet uploaded

**Input**: User request: "How do I build for the app store?", then the certificates and the
provisioning profile, and "Okay, added."

014 built the sandboxed App Store variant, signed ad hoc to run on the building Mac, and left the
package App Store Connect takes to the owner. `make appstore` now builds that package when the
owner's provisioning profile is in `.signing/`.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Building a submission (Priority: P1)

The owner runs `make appstore` and gets `Midi-Harbor-<version>.pkg`, which Transporter uploads
to App Store Connect as a new build of Midi Harbor.

**Why this priority**: it is the whole request.

**Independent Test**: With the certificates and profile in place, run `make appstore` and check
the signatures, entitlements, embedded profile, architectures and Info.plist of what it makes.

**Acceptance Scenarios**:

1. **Given** the profile and both certificates, **When** `make appstore` runs, **Then** it makes
   a universal app signed with Apple Distribution, the profile embedded and its identifiers in
   the app's entitlements, and a package signed with Mac Installer Distribution.
2. **Given** two builds of one version, **When** both are uploaded, **Then** the second is
   accepted, its build number being higher.
3. **Given** a profile for another App ID, **When** `make appstore` runs, **Then** it stops,
   naming both.
4. **Given** no profile, **When** `make appstore` runs, **Then** it builds the app to run on this
   Mac, as before.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-S01**: `make appstore` MUST build an installer package App Store Connect accepts, from the
  profile and certificates alone, with nothing else to set.
- **FR-S02**: Each build MUST carry a build number higher than any before it.
- **FR-S03**: The build MUST refuse a profile whose App ID is not the bundle's.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-S01**: A new build reaches App Store Connect with one command and one upload.

## Assumptions

- Uploading stays with Transporter: an API key upload needs the App Store Connect key that
  notarization is also waiting for, and can be added with it.
- The build number is the build's time in UTC to the minute; two submissions within one minute
  are not expected.
