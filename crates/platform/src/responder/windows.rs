//! Registration through the DNS Client service, which is Windows' own mDNS responder.
//!
//! The functions are looked up at run time rather than linked, because older Windows 10 releases
//! ship a `dnsapi.dll` without them, and a program linked against a missing function does not
//! start at all.

use super::SERVICE;
use crate::dll::{Library, RawFunction, wide};
use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;
use tracing::{debug, warn};
use windows_sys::Win32::Foundation::DNS_REQUEST_PENDING;
use windows_sys::Win32::NetworkManagement::Dns::{
    DNS_QUERY_REQUEST_VERSION1, DNS_SERVICE_CANCEL, DNS_SERVICE_INSTANCE,
    DNS_SERVICE_REGISTER_REQUEST, IP6_ADDRESS,
};
use windows_sys::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
use windows_sys::core::PCWSTR;

/// How long the responder has to accept a name before the session goes ahead regardless.
const ACCEPT_WAIT: Duration = Duration::from_secs(4);

/// How long withdrawing may take before the thread stops waiting for the responder.
const WITHDRAW_WAIT: Duration = Duration::from_secs(2);

/// How often the thread looks to see whether it has been asked to withdraw.
const WATCH_INTERVAL: Duration = Duration::from_millis(200);

type RegisterFn =
    unsafe extern "system" fn(*const DNS_SERVICE_REGISTER_REQUEST, *mut DNS_SERVICE_CANCEL) -> u32;
type ConstructFn = unsafe extern "system" fn(
    PCWSTR,
    PCWSTR,
    *const u32,
    *const IP6_ADDRESS,
    u16,
    u16,
    u16,
    u32,
    *const PCWSTR,
    *const PCWSTR,
) -> *mut DNS_SERVICE_INSTANCE;
type FreeFn = unsafe extern "system" fn(*const DNS_SERVICE_INSTANCE);

/// The four DNS-SD functions this uses.
struct Api {
    register: RegisterFn,
    deregister: RegisterFn,
    construct: ConstructFn,
    free: FreeFn,
}

/// Returns the DNS-SD functions, loading them on first use.
fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        let library = Library::open("dnsapi.dll")?;
        let register = library.function(c"DnsServiceRegister")?;
        let deregister = library.function(c"DnsServiceDeRegister")?;
        let construct = library.function(c"DnsServiceConstructInstance")?;
        let free = library.function(c"DnsServiceFreeInstance")?;
        // SAFETY: each pointer is the named export of dnsapi.dll, and each type above is that
        // function's documented signature, so calling through the cast pointer is calling the
        // function as declared.
        unsafe {
            Some(Api {
                register: std::mem::transmute::<RawFunction, RegisterFn>(register),
                deregister: std::mem::transmute::<RawFunction, RegisterFn>(deregister),
                construct: std::mem::transmute::<RawFunction, ConstructFn>(construct),
                free: std::mem::transmute::<RawFunction, FreeFn>(free),
            })
        }
    })
    .as_ref()
}

/// Reports whether this Windows has the DNS-SD registration functions.
pub(super) fn present() -> bool {
    api().is_some()
}

/// Registers a service with the DNS Client service and keeps it published until `running` is
/// cleared.
pub(super) fn advertise(
    name: &str,
    port: u16,
    properties: &[(String, String)],
    running: &AtomicBool,
    ready: &Sender<Result<(), String>>,
) {
    let Some(api) = api() else {
        let _ = ready.send(Err(
            "this version of Windows has no DNS-SD registration interface".to_owned(),
        ));
        return;
    };

    // Describe the service.
    let instance_name = wide(&format!("{}.{SERVICE}.local", instance_label(name)));
    let host_name = wide(&format!("{}.local", computer_name()));
    // The TXT record, as two arrays of strings the same length.
    let keys: Vec<Vec<u16>> = properties.iter().map(|(key, _)| wide(key)).collect();
    let values: Vec<Vec<u16>> = properties.iter().map(|(_, value)| wide(value)).collect();
    let key_pointers: Vec<PCWSTR> = keys.iter().map(|key| key.as_ptr()).collect();
    let value_pointers: Vec<PCWSTR> = values.iter().map(|value| value.as_ptr()).collect();
    let property_count = u32::try_from(properties.len()).unwrap_or(0);
    // SAFETY: every string is NUL-terminated and outlives the call, which copies them. Null
    // addresses ask the responder to answer with the host's own. The key and value arrays each
    // hold `property_count` pointers to those strings, or zero is passed for an unrepresentable
    // count and neither is read.
    let instance = unsafe {
        (api.construct)(
            instance_name.as_ptr(),
            host_name.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            port,
            0,
            0,
            property_count,
            key_pointers.as_ptr(),
            value_pointers.as_ptr(),
        )
    };
    if instance.is_null() {
        let _ = ready.send(Err("the service description was refused".to_owned()));
        return;
    }

    // Register it. The context outlives every callback that can refer to it: it is freed only
    // once the withdrawal has been confirmed, and leaked if that confirmation never comes.
    let (answered, answers) = mpsc::channel::<u32>();
    let context = Box::into_raw(Box::new(answered));
    let request = DNS_SERVICE_REGISTER_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1,
        InterfaceIndex: 0,
        pServiceInstance: instance,
        pRegisterCompletionCallback: Some(completed),
        pQueryContext: context.cast::<c_void>(),
        hCredentials: std::ptr::null_mut(),
        unicastEnabled: 0,
    };
    let mut cancel = DNS_SERVICE_CANCEL {
        reserved: std::ptr::null_mut(),
    };
    // SAFETY: the request, the instance it points to, and the context stay alive until the
    // registration is withdrawn below.
    let status = unsafe { (api.register)(&request, &mut cancel) };
    if i64::from(status) != i64::from(DNS_REQUEST_PENDING) {
        // SAFETY: the registration was refused, so no callback holds either pointer.
        unsafe {
            (api.free)(instance);
            drop(Box::from_raw(context));
        }
        let _ = ready.send(Err(format!(
            "the DNS Client service refused the registration with status {status}"
        )));
        return;
    }

    // Wait for the responder to accept the name, so a refusal is reported.
    match answers.recv_timeout(ACCEPT_WAIT) {
        Ok(0) => {
            debug!(name, "the DNS Client service accepted the registration");
            let _ = ready.send(Ok(()));
        }
        Err(_) => {
            debug!(
                name,
                "the DNS Client service has not answered the registration yet"
            );
            let _ = ready.send(Ok(()));
        }
        Ok(status) => {
            // A refused registration has finished, so the context is no longer referred to.
            // SAFETY: as above.
            unsafe {
                (api.free)(instance);
                drop(Box::from_raw(context));
            }
            let _ = ready.send(Err(format!(
                "the DNS Client service refused the name with status {status}"
            )));
            return;
        }
    }

    while running.load(Ordering::Relaxed) {
        std::thread::sleep(WATCH_INTERVAL);
    }

    // Withdraw it, waiting for the responder to confirm before anything it may still read is
    // freed.
    // SAFETY: the request is still the one registered, and everything it points to is alive.
    let status = unsafe { (api.deregister)(&request, std::ptr::null_mut()) };
    let confirmed = i64::from(status) == i64::from(DNS_REQUEST_PENDING)
        && answers.recv_timeout(WITHDRAW_WAIT).is_ok();
    if confirmed {
        // SAFETY: the withdrawal has completed, so no further callback will run for this
        // registration and nothing refers to the instance or the context.
        unsafe {
            (api.free)(instance);
            drop(Box::from_raw(context));
        }
        debug!(name, "withdrew advertisement");
    } else {
        // Freeing either while a callback may still arrive would hand it freed memory, so both
        // are left for the process to reclaim when it exits.
        warn!(
            name,
            status, "the DNS Client service did not confirm withdrawing the session"
        );
    }
}

/// Receives the DNS Client service's answer to a registration or a withdrawal.
unsafe extern "system" fn completed(
    status: u32,
    context: *const c_void,
    instance: *const DNS_SERVICE_INSTANCE,
) {
    // The instance handed back is a copy the caller owns.
    if !instance.is_null()
        && let Some(api) = api()
    {
        // SAFETY: the service allocated this copy for us and nothing else refers to it.
        unsafe { (api.free)(instance) };
    }
    // SAFETY: `context` is the `Sender` boxed in `advertise`, which stays alive until an answer
    // for the withdrawal has been received.
    if let Some(answered) = unsafe { context.cast::<Sender<u32>>().as_ref() } {
        let _ = answered.send(status);
    }
}

/// Returns the instance name as the DNS Client service must be given it.
///
/// The service splits an instance name at every dot and takes a backslash literally, so the
/// escaped form DNS-SD defines went out as two labels, `Win Harbor\` and `Test`, which no browser
/// accepted as an instance (research R-084). A hyphen stands in for the dot. It is ASCII because
/// the service lowercases any name holding a character that is not, which a lookalike dot would
/// have done to the whole name. This daemon recognises its own records by address and port, so
/// the change does not make it mistake itself for a peer.
fn instance_label(name: &str) -> String {
    name.replace('.', "-")
}

/// Returns this machine's host name, as the responder answers for it.
fn computer_name() -> String {
    let mut buffer = [0u16; 256];
    let mut length = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    // SAFETY: `length` holds the buffer's capacity in UTF-16 units, which bounds the write.
    let ok =
        unsafe { GetComputerNameExW(ComputerNameDnsHostname, buffer.as_mut_ptr(), &mut length) };
    if ok == 0 {
        return std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows".to_owned());
    }
    crate::dll::narrow(&buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the instance label given to the DNS Client service: a dot becomes an ASCII hyphen,
    /// and a name without one is unchanged.
    ///
    /// The service splits an instance name at every dot and takes a backslash literally, so
    /// "Studio.A" went out as two labels no browser accepted as an instance (research R-084).
    #[test]
    fn a_dot_in_a_name_does_not_split_it() {
        let cases = [
            ("a name with a dot", "Studio.A", "Studio-A"),
            ("a plain name", "Plain name", "Plain name"),
        ];
        for (name, instance, want) in cases {
            assert_eq!(
                instance_label(instance),
                want,
                "{name}: the DNS Client service must see one label"
            );
        }
    }
}
