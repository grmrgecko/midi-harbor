//! The system event seam: sleep, wake, and network changes.

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
#[cfg(not(windows))]
use std::time::Instant;
use std::time::{Duration, SystemTime};

/// How long a native backend holds a suspend for the daemon before letting it go regardless.
///
/// The daemon acts on a suspend as soon as it is signalled, and releasing notes and ending
/// sessions takes milliseconds, so this is ample; logind allows five seconds by default and macOS
/// thirty. Windows allows about two, and its backend holds for less than this.
pub const READY_BOUND: Duration = Duration::from_secs(3);

/// Something the operating system reported about the machine's condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemEvent {
    /// The machine is about to suspend.
    Suspending,
    /// The machine has resumed.
    Resumed,
    /// A network interface or address changed.
    NetworkChanged,
}

/// Reports machine-level changes that make reconnecting worth attempting immediately.
///
/// These signals are an optimisation and never a correctness mechanism. The platform sources are
/// known to miss events — a machine can suspend without the API firing, and logind does not emit
/// a resume signal when returning from hibernation — so recovery must not depend on them. They
/// exist to turn a thirty-second timeout into an instant reconnection, nothing more.
pub trait SystemEvents: Send + Sync {
    /// Takes any events observed since the last call, oldest first.
    fn drain_events(&self) -> Vec<SystemEvent>;

    /// Tells the platform the daemon has done what it does before a suspend, so the suspend may
    /// go ahead.
    ///
    /// A backend that holds a suspend for this lets it go on its own after a short bound as
    /// well: a stalled daemon must not keep a machine awake. A backend that cannot hold a
    /// suspend has nothing to do.
    fn ready_for_sleep(&self) {}

    /// Returns what is signalled when an event cannot wait for the next look, which is a suspend
    /// about to happen.
    ///
    /// On Linux, NetworkManager takes the network down within milliseconds of the same logind
    /// signal, so the goodbye and the notes released before sleep go out only if the daemon acts
    /// at once rather than at its next look (R-072). A backend that has no such event returns
    /// nothing, and is looked at on the interval alone.
    fn urgent(&self) -> Option<Arc<tokio::sync::Notify>> {
        None
    }
}

/// How often the machine is looked at.
///
/// Public because it is part of how long a recovery takes, which is a number the product
/// promises rather than an implementation detail.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// How far the two clocks may drift apart between samples before a suspend is assumed.
///
/// Well above anything a clock correction produces between samples and well below the shortest
/// suspend worth reacting to. A false positive costs one reconnection attempt that was going to
/// happen anyway; a missed one costs nothing but the wait this exists to avoid.
const GAP_TOLERANCE: Duration = Duration::from_secs(5);

/// One reading of the machine's two clocks.
#[derive(Clone, Copy, Debug)]
pub struct ClockSample {
    /// A clock that stops while the machine is suspended.
    pub monotonic: Duration,
    /// A clock that keeps running while the machine is suspended.
    pub wall: Duration,
}

/// A clock that counts only time the machine spends awake.
///
/// On macOS and Linux that is `Instant`. On Windows `Instant` is `QueryPerformanceCounter`, which
/// Windows does not promise to stop during sleep, so the unbiased interrupt time is read instead:
/// Windows documents it as leaving out time spent in sleep and hibernation. A clock that counted
/// through sleep would never fall behind the wall clock, and no suspend would ever be noticed.
pub struct AwakeClock {
    #[cfg(not(windows))]
    started: Instant,
    #[cfg(windows)]
    started: Duration,
}

impl AwakeClock {
    /// Starts counting from now.
    pub fn start() -> Self {
        #[cfg(not(windows))]
        {
            Self {
                started: Instant::now(),
            }
        }
        #[cfg(windows)]
        {
            Self {
                started: unbiased_interrupt_time(),
            }
        }
    }

    /// Returns how long the machine has been awake since the clock started.
    pub fn elapsed(&self) -> Duration {
        #[cfg(not(windows))]
        {
            self.started.elapsed()
        }
        #[cfg(windows)]
        {
            unbiased_interrupt_time().saturating_sub(self.started)
        }
    }
}

/// Reads Windows' interrupt time less the time spent asleep.
#[cfg(windows)]
fn unbiased_interrupt_time() -> Duration {
    let mut hundreds_of_nanoseconds: u64 = 0;
    // SAFETY: the function writes one u64 through the pointer, which is valid for the call.
    unsafe {
        windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime(
            &mut hundreds_of_nanoseconds,
        )
    };
    // The count is in units of 100 ns.
    Duration::from_nanos(hundreds_of_nanoseconds.saturating_mul(100))
}

/// Detects a suspend by watching the wall clock run away from the monotonic one.
///
/// Every platform's awake clock stops while suspended and the wall clock keeps going, so
/// the difference between the two is how long the machine was away. This finds a suspend that
/// no platform reported, which research R-010 records as the normal case rather than the
/// exception.
#[derive(Debug, Default)]
pub struct ClockGap {
    previous: Option<ClockSample>,
}

impl ClockGap {
    /// Creates a detector with no reading yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a reading, reporting how long the machine was away when it was away.
    ///
    /// Reports nothing for the first reading, because a gap is a difference between two.
    pub fn observe(&mut self, sample: ClockSample) -> Option<Duration> {
        let previous = self.previous.replace(sample)?;

        // Saturating on both: a clock that goes backwards is a correction, not a suspend.
        let monotonic = sample.monotonic.saturating_sub(previous.monotonic);
        let wall = sample.wall.saturating_sub(previous.wall);
        let gap = wall.saturating_sub(monotonic);

        (gap >= GAP_TOLERANCE).then_some(gap)
    }
}

/// Reports a change in the addresses this machine can be reached at.
///
/// An address appearing or disappearing is how a network comes back without anything announcing
/// it: a cable is plugged in, a laptop joins a different network, a lease is renewed on a
/// different address. A session bound to the old address will never recover by waiting.
#[derive(Debug, Default)]
pub struct AddressWatch {
    previous: Option<BTreeSet<IpAddr>>,
}

impl AddressWatch {
    /// Creates a watch with no reading yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the current addresses, reporting whether they differ from the last reading.
    ///
    /// Reports nothing for the first reading, because a change is a difference between two.
    pub fn observe(&mut self, addresses: BTreeSet<IpAddr>) -> bool {
        let changed = self
            .previous
            .as_ref()
            .is_some_and(|previous| *previous != addresses);
        self.previous = Some(addresses);
        changed
    }
}

/// Returns the addresses this machine currently answers on, excluding loopback.
///
/// Loopback is left out because it never changes, so including it would only ever dilute the
/// comparison. An empty set is a meaningful answer: it means there is no network to reach.
pub fn routable_addresses() -> Option<BTreeSet<IpAddr>> {
    let interfaces = if_addrs::get_if_addrs().ok()?;
    Some(
        interfaces
            .into_iter()
            .filter(|interface| !interface.is_loopback())
            .map(|interface| interface.addr.ip())
            .collect(),
    )
}

/// Watches the machine by looking at it, rather than by being told about it.
///
/// Deliberately not a substitute for the platform's own notifications, which report a suspend
/// before it happens and an interface change the instant it happens. It is what runs on both
/// platforms with no privileges and no bindings, and R-010 records that the platform sources miss
/// suspends anyway, so it is worth having underneath them rather than instead of them.
pub struct PolledSystemEvents {
    observed: Arc<Mutex<Vec<SystemEvent>>>,
}

impl PolledSystemEvents {
    /// Starts watching the clocks and the machine's addresses on a background thread.
    pub fn start() -> Self {
        let observed: Arc<Mutex<Vec<SystemEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&observed);

        // A thread rather than a task, so the sampling interval does not depend on how busy the
        // runtime is. A stalled runtime is exactly when a resume goes unnoticed.
        let started = AwakeClock::start();
        std::thread::Builder::new()
            .name("harbor-machine".to_owned())
            .spawn(move || {
                let mut clock = ClockGap::new();
                let mut addresses = AddressWatch::new();
                loop {
                    std::thread::sleep(SAMPLE_INTERVAL);
                    let mut found = Vec::new();

                    if let Ok(wall) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)
                        && let Some(gap) = clock.observe(ClockSample {
                            monotonic: started.elapsed(),
                            wall,
                        })
                    {
                        tracing::info!(
                            away_seconds = gap.as_secs(),
                            "the machine was suspended; reconnecting rather than waiting out a backoff"
                        );
                        // The pair is reported because that is the shape of the event a consumer
                        // handles: something stopped, then something started again.
                        found.push(SystemEvent::Suspending);
                        found.push(SystemEvent::Resumed);
                    }

                    if let Some(current) = routable_addresses() {
                        let count = current.len();
                        if addresses.observe(current) {
                            tracing::info!(
                                addresses = count,
                                "this machine's addresses changed; reconnecting rather than waiting"
                            );
                            found.push(SystemEvent::NetworkChanged);
                        }
                    }

                    if found.is_empty() {
                        continue;
                    }
                    if let Ok(mut guard) = sink.lock() {
                        guard.extend(found);
                    }
                }
            })
            .ok();

        Self { observed }
    }
}

/// The machine watched both ways: the platform's own notices, and the clocks and addresses
/// underneath them.
///
/// The platform says a suspend is coming, which nothing else can; the polled watcher catches the
/// suspends and network changes the platform misses. Each covers the other's gap.
pub struct CombinedSystemEvents {
    native: Option<Box<dyn SystemEvents>>,
    polled: PolledSystemEvents,
}

impl CombinedSystemEvents {
    /// Starts the polled watcher, and the platform's own source where there is one.
    pub fn start() -> Self {
        #[cfg(target_os = "linux")]
        let native = linux::LogindSystemEvents::start()
            .map(|native| Box::new(native) as Box<dyn SystemEvents>);
        #[cfg(target_os = "macos")]
        let native = macos::PowerSystemEvents::start()
            .map(|native| Box::new(native) as Box<dyn SystemEvents>);
        #[cfg(windows)]
        let native = windows::PowerSystemEvents::start()
            .map(|native| Box::new(native) as Box<dyn SystemEvents>);
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let native = None;

        Self {
            native,
            polled: PolledSystemEvents::start(),
        }
    }
}

impl SystemEvents for CombinedSystemEvents {
    fn drain_events(&self) -> Vec<SystemEvent> {
        // The platform's first: its suspend comes before the machine goes, the polled pair only
        // once it is back, and the daemon acts on a suspend only if nothing after it says the
        // machine returned.
        let mut events = self
            .native
            .as_ref()
            .map(|native| native.drain_events())
            .unwrap_or_default();
        events.extend(self.polled.drain_events());
        events
    }

    fn ready_for_sleep(&self) {
        if let Some(native) = &self.native {
            native.ready_for_sleep();
        }
    }

    fn urgent(&self) -> Option<Arc<tokio::sync::Notify>> {
        self.native.as_ref().and_then(|native| native.urgent())
    }
}

impl SystemEvents for PolledSystemEvents {
    fn drain_events(&self) -> Vec<SystemEvent> {
        match self.observed.lock() {
            Ok(mut guard) => std::mem::take(&mut guard),
            Err(poisoned) => std::mem::take(&mut poisoned.into_inner()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Locks that the awake clock runs at real speed while the machine is awake: 200 ms of sleep
    /// reads as at least 150 ms and no more than the real time plus 50 ms.
    ///
    /// On Windows it reads `QueryUnbiasedInterruptTime`, which counts in units of 100 ns; a wrong
    /// unit would make every reading off by a power of ten, and the gap detector would see a
    /// suspend at every sample or never.
    #[test]
    fn the_awake_clock_keeps_time_with_the_machine() {
        // A real clock read against real time; nothing here advances a state machine.
        let clock = AwakeClock::start();
        let started = Instant::now();
        std::thread::sleep(Duration::from_millis(200));
        let awake = clock.elapsed();
        let real = started.elapsed();
        assert!(
            awake >= Duration::from_millis(150) && awake <= real + Duration::from_millis(50),
            "the awake clock must keep time with the machine: awake {awake:?} over {real:?}"
        );
    }

    /// Locks suspend detection from two clock readings: a suspend is the time the wall clock ran
    /// ahead of the awake clock, reported from `GAP_TOLERANCE`, five seconds, up. The awake clock
    /// stops while the machine sleeps and the wall clock does not (research R-010).
    ///
    /// One second awake against 7,201 seconds on the wall is 7,200 seconds away. A gap of 4
    /// seconds is a clock correction, not a suspend, and a wall clock set back saturates to no
    /// gap rather than underflowing into an enormous one.
    #[test]
    fn a_suspend_is_the_time_the_wall_clock_ran_ahead_of_the_awake_clock() {
        let start = 1_700_000_000;
        let cases = [
            ("clocks running together", 1, start + 1, None),
            ("a gap just under the tolerance", 1, start + 5, None),
            ("a gap at the tolerance", 1, start + 6, Some(5)),
            ("two hours away", 1, start + 7_201, Some(7_200)),
            ("a wall clock set back", 1, start - 1_000, None),
        ];
        for (name, awake, wall, want) in cases {
            let mut gap = ClockGap::new();
            let first = gap.observe(ClockSample {
                monotonic: Duration::from_secs(10),
                wall: Duration::from_secs(start),
            });
            assert_eq!(first, None, "{name}: one reading cannot show a gap");
            let away = gap.observe(ClockSample {
                monotonic: Duration::from_secs(10 + awake),
                wall: Duration::from_secs(wall),
            });
            assert_eq!(
                away,
                want.map(Duration::from_secs),
                "{name}: the time away is wrong"
            );
        }
    }
}
