//! Embeds the icon in the Windows executable. Explorer, the Start menu and shortcuts show it, and
//! so does the taskbar, since the window sets no icon of its own.
//!
//! The resource compiler is required rather than optional, so a release build without one fails
//! instead of shipping an executable with the generic icon. Cross-compiling uses mingw-w64's
//! `windres`, and a Windows build with MSVC uses the Windows SDK's `rc.exe`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=packaging/windows/midi-harbor.rc");
    println!("cargo:rerun-if-changed=packaging/windows/midi-harbor.ico");
    if std::env::var("CARGO_CFG_TARGET_OS")? != "windows" {
        return Ok(());
    }
    embed_resource::compile("packaging/windows/midi-harbor.rc", embed_resource::NONE)
        .manifest_required()
        .map_err(|err| format!("could not embed the Windows icon: {err}"))?;
    Ok(())
}
