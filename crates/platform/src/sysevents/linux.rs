//! logind's word on sleep and wake, over the system bus.
//!
//! logind announces a suspend with `PrepareForSleep(true)` and the return with
//! `PrepareForSleep(false)`. It waits for a process holding a delay lock before suspending, up
//! to its own `InhibitDelayMaxSec`, so holding one is what gives the daemon time to release notes
//! held on other machines. logind does not always announce the return from hibernation, which is
//! why the polled watcher runs underneath this rather than being replaced by it (R-010).

use super::{READY_BOUND, SystemEvent, SystemEvents};
use dbus::arg::OwnedFd;
use dbus::blocking::{Connection, Proxy};
use dbus::message::MatchRule;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// logind's bus name, which is also the only sender whose sleep signal is believed.
const LOGIND: &str = "org.freedesktop.login1";

/// logind's manager object and interface.
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";

/// How long a call to logind may take before it is abandoned.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Sleep and wake as logind reports them.
pub struct LogindSystemEvents {
    observed: Arc<Mutex<Vec<SystemEvent>>>,
    ready: Sender<()>,
    urgent: Arc<tokio::sync::Notify>,
}

impl LogindSystemEvents {
    /// Connects to the system bus and starts listening, or returns nothing where there is no
    /// logind to listen to, such as in a container.
    pub fn start() -> Option<Self> {
        let connection = match Connection::new_system() {
            Ok(connection) => connection,
            Err(error) => {
                tracing::info!(error = %error, "no system bus; sleep is noticed only after waking");
                return None;
            }
        };
        let observed: Arc<Mutex<Vec<SystemEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let (ready, readied) = mpsc::channel();
        let urgent = Arc::new(tokio::sync::Notify::new());

        // Held from the start, so the first suspend is delayed like every later one.
        let lock: Arc<Mutex<Option<OwnedFd>>> = Arc::new(Mutex::new(inhibit(&connection)));

        let rule = MatchRule::new_signal(MANAGER, "PrepareForSleep").with_sender(LOGIND);
        let sink = Arc::clone(&observed);
        let held = Arc::clone(&lock);
        let signal = Arc::clone(&urgent);
        let readied = Mutex::new(readied);
        let matched = connection.add_match(
            rule,
            move |(starting,): (bool,), connection: &Connection, _: &dbus::Message| {
                if starting {
                    before_sleep(&sink, &signal, &readied, &held);
                } else {
                    push(&sink, SystemEvent::Resumed);
                    // Taken again for the next suspend, which the one just released no longer
                    // covers.
                    if let Ok(mut held) = held.lock() {
                        *held = inhibit(connection);
                    }
                }
                true
            },
        );
        if let Err(error) = matched {
            tracing::error!(error = %error, "failed to subscribe to logind's sleep signal");
            return None;
        }

        let spawned = std::thread::Builder::new()
            .name("harbor-logind".to_owned())
            .spawn(move || {
                loop {
                    if let Err(error) = connection.process(Duration::from_secs(1)) {
                        tracing::error!(error = %error, "lost the system bus; sleep is noticed only after waking");
                        return;
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::error!(error = %error, "failed to start the logind watcher");
            return None;
        }
        tracing::info!("watching logind for sleep and wake");
        Some(Self {
            observed,
            ready,
            urgent,
        })
    }
}

impl SystemEvents for LogindSystemEvents {
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

/// Reports the suspend and holds it until the daemon is ready or the bound passes, then lets it
/// go.
fn before_sleep(
    sink: &Mutex<Vec<SystemEvent>>,
    urgent: &tokio::sync::Notify,
    readied: &Mutex<Receiver<()>>,
    held: &Mutex<Option<OwnedFd>>,
) {
    if let Ok(readied) = readied.lock() {
        // A ready left over from an earlier suspend says nothing about this one.
        while readied.try_recv().is_ok() {}
        push(sink, SystemEvent::Suspending);
        urgent.notify_one();
        if readied.recv_timeout(READY_BOUND).is_err() {
            tracing::info!("the daemon was not ready in time; letting the suspend go ahead");
        }
    } else {
        push(sink, SystemEvent::Suspending);
        urgent.notify_one();
    }
    // Closing the descriptor is what releases the lock.
    if let Ok(mut held) = held.lock() {
        drop(held.take());
    }
}

/// Takes a delay lock on sleep, or nothing if logind refuses one.
fn inhibit(connection: &Connection) -> Option<OwnedFd> {
    let manager = Proxy::new(LOGIND, MANAGER_PATH, CALL_TIMEOUT, connection);
    let taken: Result<(OwnedFd,), dbus::Error> = manager.method_call(
        MANAGER,
        "Inhibit",
        (
            "sleep",
            "Midi Harbor",
            "Releasing notes held on other machines",
            "delay",
        ),
    );
    match taken {
        Ok((lock,)) => Some(lock),
        Err(error) => {
            tracing::info!(error = %error, "logind would not delay sleep; held notes may ring on after a suspend");
            None
        }
    }
}

fn push(sink: &Mutex<Vec<SystemEvent>>, event: SystemEvent) {
    match sink.lock() {
        Ok(mut guard) => guard.push(event),
        Err(poisoned) => poisoned.into_inner().push(event),
    }
}
