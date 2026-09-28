//! IOKit's word on sleep and wake.
//!
//! Registering for system power notifications makes a suspend wait for this process to answer
//! `kIOMessageSystemWillSleep`, which is what gives the daemon time to release notes held on
//! other machines. Every such message has to be answered, promptly: one left unanswered holds
//! the machine awake for thirty seconds. The polled watcher runs underneath, since IOKit misses
//! suspends too (R-010).

use super::{READY_BOUND, SystemEvent, SystemEvents};
use core_foundation_sys::runloop::{
    CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRun, CFRunLoopSourceRef,
    kCFRunLoopDefaultMode,
};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// IOKit's name for a Mach port it hands out.
type IoObject = u32;

/// An opaque notification port.
type NotificationPort = *mut c_void;

/// The machine is asking whether it may sleep when idle.
const CAN_SYSTEM_SLEEP: u32 = 0xE000_0270;
/// The machine is going to sleep, and waits for an answer.
const SYSTEM_WILL_SLEEP: u32 = 0xE000_0280;
/// The machine has woken and everything is powered again.
const SYSTEM_HAS_POWERED_ON: u32 = 0xE000_0300;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port: *mut NotificationPort,
        callback: extern "C" fn(*mut c_void, IoObject, u32, *mut c_void),
        notifier: *mut IoObject,
    ) -> IoObject;
    fn IONotificationPortGetRunLoopSource(port: NotificationPort) -> CFRunLoopSourceRef;
    fn IOAllowPowerChange(kernel_port: IoObject, notification: isize) -> i32;
}

/// What the power callback shares with the rest of the backend.
struct Shared {
    observed: Arc<Mutex<Vec<SystemEvent>>>,
    urgent: Arc<tokio::sync::Notify>,
    readied: Mutex<Receiver<()>>,
    /// The connection to the power domain, which every answer goes through. Set before the
    /// run loop starts, so before any callback can read it.
    root: AtomicU32,
}

/// Sleep and wake as IOKit reports them.
pub struct PowerSystemEvents {
    observed: Arc<Mutex<Vec<SystemEvent>>>,
    ready: Sender<()>,
    urgent: Arc<tokio::sync::Notify>,
}

impl PowerSystemEvents {
    /// Registers for power notifications on a thread of its own, or returns nothing if IOKit
    /// refuses.
    pub fn start() -> Option<Self> {
        let observed: Arc<Mutex<Vec<SystemEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let (ready, readied) = mpsc::channel();
        let urgent = Arc::new(tokio::sync::Notify::new());
        let shared = Shared {
            observed: Arc::clone(&observed),
            urgent: Arc::clone(&urgent),
            readied: Mutex::new(readied),
            root: AtomicU32::new(0),
        };
        let (registered, answer) = mpsc::channel();

        let spawned = std::thread::Builder::new()
            .name("harbor-power".to_owned())
            .spawn(move || {
                // Lives as long as the process: the callback may run at any time until then.
                let shared: &'static Shared = Box::leak(Box::new(shared));
                let mut port: NotificationPort = std::ptr::null_mut();
                let mut notifier: IoObject = 0;
                // SAFETY: `shared` is leaked, so the reference passed as the callback's context
                // stays valid for every call. `port` and `notifier` are live locals IOKit writes
                // into before returning.
                let root = unsafe {
                    IORegisterForSystemPower(
                        std::ptr::from_ref(shared).cast_mut().cast(),
                        &mut port,
                        on_power,
                        &mut notifier,
                    )
                };
                if root == 0 || port.is_null() {
                    let _ = registered.send(false);
                    return;
                }
                shared.root.store(root, Ordering::SeqCst);

                // SAFETY: `port` is the notification port IOKit just returned, and its source is
                // added to this thread's own run loop, which then runs on this thread for good.
                unsafe {
                    let source = IONotificationPortGetRunLoopSource(port);
                    CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
                }
                let _ = registered.send(true);
                // SAFETY: runs this thread's run loop, which is what delivers the callbacks.
                unsafe { CFRunLoopRun() };
            });
        if let Err(error) = spawned {
            tracing::error!(error = %error, "failed to start the power watcher");
            return None;
        }
        if answer.recv().unwrap_or(false) {
            tracing::info!("watching IOKit for sleep and wake");
            Some(Self {
                observed,
                ready,
                urgent,
            })
        } else {
            tracing::error!(
                "failed to register for power notifications; sleep is noticed only after waking"
            );
            None
        }
    }
}

impl SystemEvents for PowerSystemEvents {
    fn drain_events(&self) -> Vec<SystemEvent> {
        match self.observed.lock() {
            Ok(mut guard) => std::mem::take(&mut guard),
            Err(poisoned) => std::mem::take(&mut poisoned.into_inner()),
        }
    }

    fn ready_for_sleep(&self) {
        let _ = self.ready.send(());
    }

    fn urgent(&self) -> Option<Arc<tokio::sync::Notify>> {
        Some(Arc::clone(&self.urgent))
    }
}

/// Answers IOKit's power messages. Must not panic: it is called from C.
extern "C" fn on_power(
    context: *mut c_void,
    _service: IoObject,
    message: u32,
    argument: *mut c_void,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: the context is the leaked `Shared` registered in `start`, never freed.
    let shared = unsafe { &*context.cast::<Shared>() };
    let root = shared.root.load(Ordering::SeqCst);
    // The message's identifier travels as the argument's address.
    let notification = argument as isize;

    match message {
        // An idle sleep is not this process's to refuse.
        CAN_SYSTEM_SLEEP => {
            // SAFETY: `root` is the power connection registered in `start`.
            let _ = unsafe { IOAllowPowerChange(root, notification) };
        }
        SYSTEM_WILL_SLEEP => {
            if let Ok(readied) = shared.readied.lock() {
                // A ready left over from an earlier suspend says nothing about this one.
                while readied.try_recv().is_ok() {}
                push(&shared.observed, SystemEvent::Suspending);
                shared.urgent.notify_one();
                if readied.recv_timeout(READY_BOUND).is_err() {
                    tracing::info!(
                        "the daemon was not ready in time; letting the suspend go ahead"
                    );
                }
            } else {
                push(&shared.observed, SystemEvent::Suspending);
                shared.urgent.notify_one();
            }
            // SAFETY: as above. Answered on every path, since an unanswered message holds the
            // machine awake.
            let _ = unsafe { IOAllowPowerChange(root, notification) };
        }
        SYSTEM_HAS_POWERED_ON => push(&shared.observed, SystemEvent::Resumed),
        _ => {}
    }
}

fn push(sink: &Mutex<Vec<SystemEvent>>, event: SystemEvent) {
    match sink.lock() {
        Ok(mut guard) => guard.push(event),
        Err(poisoned) => poisoned.into_inner().push(event),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks that IOKit accepts the registration for power notifications through
    /// `IORegisterForSystemPower`, and that nothing is reported before anything happens.
    ///
    /// Registering is all a test can do without putting the machine to sleep, and it is the step
    /// that fails if the hand-written IOKit declarations in this file are wrong.
    #[test]
    fn registers_for_power_notifications() {
        let events = PowerSystemEvents::start();
        assert!(
            events.is_some_and(|events| events.drain_events().is_empty()),
            "IOKit must accept the registration, and report nothing before a power change"
        );
    }
}
