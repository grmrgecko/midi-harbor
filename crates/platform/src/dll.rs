//! Loading a Windows DLL and its functions at run time.
//!
//! Used for interfaces that may be missing from the machine: a driver the user has not
//! installed, or a function older Windows releases lack. Linking them the ordinary way would stop
//! the whole program from starting on such a machine, where it should start and report the one
//! capability as unavailable.

use std::ffi::CStr;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// An untyped function exported by a DLL, to be cast to its documented signature.
pub type RawFunction = unsafe extern "system" fn() -> isize;

/// A DLL loaded into this process, which stays loaded for the life of the process.
///
/// Never unloaded, because the functions taken from it are kept in statics and called from
/// callbacks the system may still be running.
#[derive(Clone, Copy)]
pub struct Library(HMODULE);

// SAFETY: an HMODULE is a process-wide value that any thread may pass to GetProcAddress, and the
// library is never freed, so the handle cannot dangle.
unsafe impl Send for Library {}
// SAFETY: as above; the handle is only ever read.
unsafe impl Sync for Library {}

impl Library {
    /// Loads a DLL by name from the system's search path, or returns nothing if it is absent.
    pub fn open(name: &str) -> Option<Self> {
        let wide = wide(name);
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        (!module.is_null()).then_some(Self(module))
    }

    /// Returns an exported function, or nothing if this version of the DLL lacks it.
    pub fn function(&self, name: &CStr) -> Option<RawFunction> {
        // SAFETY: the module handle is valid for the life of the process, and `name` is a
        // NUL-terminated ASCII string.
        unsafe { GetProcAddress(self.0, name.as_ptr().cast()) }
    }
}

/// Encodes a string as NUL-terminated UTF-16, the form every wide Windows function takes.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decodes a UTF-16 buffer up to its first NUL, replacing anything that is not valid.
pub fn narrow(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(buffer.get(..end).unwrap_or(buffer))
}
