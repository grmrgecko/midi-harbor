//! Socket options the standard library does not offer.

/// Makes a socket's port its own until it closes, where the platform would otherwise share it.
///
/// On Windows, a dual-stack UDP socket did not stop another socket binding its port over IPv4 or
/// IPv6, and bound a port an IPv4 socket already held, so a second program could take a session's
/// datagrams (research R-088). `SO_EXCLUSIVEADDRUSE` refuses both. Set before binding. Linux and
/// macOS refuse a second bind without being asked, so there this does nothing.
pub fn claim_exclusively(socket: &socket2::Socket) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            SO_EXCLUSIVEADDRUSE, SOCKET, SOL_SOCKET, setsockopt,
        };

        let enabled: i32 = 1;
        // SAFETY: the socket is open for the duration of the call, and the option value is an
        // `i32` whose size is passed with it.
        let result = unsafe {
            setsockopt(
                socket.as_raw_socket() as SOCKET,
                SOL_SOCKET,
                SO_EXCLUSIVEADDRUSE,
                std::ptr::from_ref(&enabled).cast::<u8>(),
                std::mem::size_of::<i32>() as i32,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = socket;
        Ok(())
    }
}
