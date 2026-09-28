//! The console a Windows program is given whether it wants one or not.
//!
//! The executable is a console program, so the command line works in any terminal. Started from
//! Explorer or a shortcut to open the interface, Windows gives it a console of its own, and that
//! empty window stayed open beside the interface for as long as it ran.

/// Lets go of the console if this process is the only one using it, which closes its window.
///
/// A console shared with a terminal the user started the program from is kept, so output and
/// errors still reach them. Does nothing on other platforms.
pub fn release_own() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};

        let mut attached = [0u32; 2];
        // SAFETY: the buffer holds two process identifiers, which is the count passed; a larger
        // count is reported without being written.
        let count = unsafe { GetConsoleProcessList(attached.as_mut_ptr(), 2) };
        if count == 1 {
            // SAFETY: detaching from a console is always allowed; nothing here writes to it
            // afterwards except through handles that then fail quietly.
            unsafe { FreeConsole() };
        }
    }
}
