//! Gives every build an identifier of its own, so a window can tell whether the daemon it
//! reached is the build it came with (research R-108).
//!
//! Packaging sets `MIDI_HARBOR_BUILD_ID` once for everything that goes into one package, so the
//! app and the daemon it carries agree even when they are built separately. Without it a new
//! identifier is made whenever this crate or the version changes, which is as often as a build
//! outside packaging can be told apart.

fn main() {
    println!("cargo:rerun-if-env-changed=MIDI_HARBOR_BUILD_ID");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../../VERSION");
    let id = std::env::var("MIDI_HARBOR_BUILD_ID")
        .ok()
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    println!("cargo:rustc-env=MIDI_HARBOR_BUILD_ID={}", id.trim());
}
