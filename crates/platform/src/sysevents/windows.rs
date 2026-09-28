//! Windows' word on sleep and wake, from the power manager.
//!
//! A registration made with `PowerRegisterSuspendResumeNotification` is called with
//! `PBT_APMSUSPEND` before the machine suspends, and the suspend waits for the call to return, so
//! holding it briefly is what gives the daemon time to release notes held on other machines. A
//! process has about two seconds before Windows stops waiting. The polled watcher runs
//! underneath, since no platform reports every suspend (R-010).

use super::{SystemEvent, SystemEvents};
use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_sys::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, PowerRegisterSuspendResumeNotification,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
};

/// How long a suspend is held for the daemon.
///
/// Windows allows about two seconds for a process to handle the notification, less than
/// `READY_BOUND`, so the hold ends just inside that rather than at the shared bound.
const HOLD: Duration = Duration::from_millis(1900);

/// What the power callback shares with the rest of the backend.
struct Shared {
    observed: Arc<Mutex<Vec<SystemEvent>>>,
    urgent: Arc<tokio::sync::Notify>,
    readied: Mutex<Receiver<()>>,
}

/// Sleep and wake as the Windows power manager reports them.
pub struct PowerSystemEvents {
    observed: Arc<Mutex<Vec<SystemEvent>>>,
    ready: Sender<()>,
    urgent: Arc<tokio::sync::Notify>,
}

impl PowerSystemEvents {
    /// Registers for suspend and resume notifications, or returns nothing if Windows refuses.
    pub fn start() -> Option<Self> {
        let observed: Arc<Mutex<Vec<SystemEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let (ready, readied) = mpsc::channel();
        let urgent = Arc::new(tokio::sync::Notify::new());

        // Lives as long as the process: the callback may run at any time until then, and the
        // registration is never withdrawn.
        let shared: &'static Shared = Box::leak(Box::new(Shared {
            observed: Arc::clone(&observed),
            urgent: Arc::clone(&urgent),
            readied: Mutex::new(readied),
        }));
        let parameters: &'static DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS =
            Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(power_changed),
                Context: std::ptr::from_ref(shared).cast_mut().cast::<c_void>(),
            }));
        let mut registration: *mut c_void = std::ptr::null_mut();
        // SAFETY: with DEVICE_NOTIFY_CALLBACK the recipient is a pointer to subscription
        // parameters, which are leaked above and so outlive the registration, as does the
        // context they carry.
        let result = unsafe {
            PowerRegisterSuspendResumeNotification(
                DEVICE_NOTIFY_CALLBACK,
                std::ptr::from_ref(parameters).cast_mut().cast::<c_void>(),
                &mut registration,
            )
        };
        if result != 0 {
            tracing::error!(
                error = %std::io::Error::from_raw_os_error(result as i32),
                "failed to register for suspend notifications; sleep is noticed only after waking"
            );
            return None;
        }
        tracing::info!("watching the power manager for sleep and wake");
        Some(Self {
            observed,
            ready,
            urgent,
        })
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

/// Receives a power notification.
///
/// Called on a thread of the power manager's, not on the MIDI data path, so it may lock and wait.
unsafe extern "system" fn power_changed(
    context: *const c_void,
    kind: u32,
    _setting: *const c_void,
) -> u32 {
    // SAFETY: the context is the leaked `Shared` given at registration, which lives for the
    // process.
    let Some(shared) = (unsafe { context.cast::<Shared>().as_ref() }) else {
        return 0;
    };
    match kind {
        PBT_APMSUSPEND => before_sleep(shared),
        // Automatic resume comes on every wake; resume-suspend follows it when a user is present.
        // Both are reported, and a second resume costs nothing.
        PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => push(shared, SystemEvent::Resumed),
        _ => {}
    }
    0
}

/// Reports the suspend and holds it until the daemon is ready or the bound passes.
fn before_sleep(shared: &Shared) {
    if let Ok(readied) = shared.readied.lock() {
        // A ready left over from an earlier suspend says nothing about this one.
        while readied.try_recv().is_ok() {}
        push(shared, SystemEvent::Suspending);
        shared.urgent.notify_one();
        if readied.recv_timeout(HOLD).is_err() {
            tracing::info!("the daemon was not ready in time; letting the suspend go ahead");
        }
    } else {
        push(shared, SystemEvent::Suspending);
        shared.urgent.notify_one();
    }
}

fn push(shared: &Shared, event: SystemEvent) {
    match shared.observed.lock() {
        Ok(mut guard) => guard.push(event),
        Err(poisoned) => poisoned.into_inner().push(event),
    }
}
