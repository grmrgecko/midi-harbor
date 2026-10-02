//! Advertising a service through the platform's own mDNS responder.
//!
//! Each platform already runs a responder that answers other machines' queries for as long as a
//! service is registered: Bonjour on macOS, Avahi on Linux, and the DNS Client service on Windows.
//! Registering through it is what keeps a session visible, which a responder of our own did not
//! manage (research R-022).

#[cfg(windows)]
mod windows;
#[cfg(not(windows))]
mod zeroconf;

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;

/// The DNS-SD service type RTP-MIDI sessions are advertised under, without its domain.
pub const SERVICE: &str = "_apple-midi._udp";

/// Registers `name` on `port` with the platform responder and keeps it published until
/// `running` is cleared. `properties` are published with it as its TXT record.
///
/// Blocks for the life of the advertisement, so the caller gives it a thread. `ready` receives
/// one answer once the responder has accepted or refused the name, so a rejected registration is
/// reported rather than leaving a session that believes it is advertised.
pub fn advertise(
    name: &str,
    port: u16,
    properties: &[(String, String)],
    running: &AtomicBool,
    ready: &Sender<Result<(), String>>,
) {
    #[cfg(windows)]
    windows::advertise(name, port, properties, running, ready);
    #[cfg(not(windows))]
    zeroconf::advertise(name, port, properties, running, ready);
}

/// Reports whether this machine has a responder to register with.
///
/// Only Windows can lack the interface outright: it arrived in Windows 10 build 18362, and on an
/// older system the registration functions are absent from `dnsapi.dll`. Whether Avahi is running
/// on Linux is the capability query's question, since that can change while the daemon runs.
pub fn present() -> bool {
    #[cfg(windows)]
    {
        windows::present()
    }
    #[cfg(not(windows))]
    {
        true
    }
}
