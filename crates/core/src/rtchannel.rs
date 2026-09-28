//! Carrying MIDI across the real-time boundary.
//!
//! A platform read callback runs in a real-time context: it may not allocate, lock, or block. So
//! it does nothing but stamp what arrived and push it into a pre-allocated ring for an ordinary
//! task to drain.
//!
//! These live here rather than beside the router so that the platform layer can hold a producer
//! without depending on the daemon.

use crate::ids::EndpointId;
use crate::midi::MidiMessage;
use crate::rtevent::{RtEvent, RtPayload};
use crate::stream::SysExEnd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Events one endpoint may buffer before the oldest are dropped.
///
/// At a few thousand messages a second this is roughly a second of headroom, which is far longer
/// than any drain delay should ever be. Larger would hide a problem rather than solve it.
pub const EVENT_CAPACITY: usize = 4096;

/// System-exclusive bytes one endpoint may buffer.
///
/// Enough for several large dumps in flight. A sample dump that exceeds it is truncated and
/// counted, which is honest; silently growing the buffer would not be.
pub const SYSEX_CAPACITY: usize = 65_536;

/// Wakes whoever drains the rings when something is pushed into one.
///
/// Draining on a timer made every message wait for the next tick, which cost as much as the
/// whole latency budget, and woke every endpoint's drain hundreds of times a second while nothing
/// played. Ringing is one atomic swap, and a wake only on the first ring since the listener last
/// waited, so a burst of messages costs one wake. `Thread::unpark` takes no lock and allocates
/// nothing, which is what lets a real-time callback ring it.
#[derive(Debug, Default)]
pub struct Doorbell {
    rung: AtomicBool,
    listener: OnceLock<std::thread::Thread>,
}

impl Doorbell {
    /// Creates a doorbell no one is listening to yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Says something arrived. Safe from a real-time context.
    pub fn ring(&self) {
        if !self.rung.swap(true, Ordering::AcqRel)
            && let Some(listener) = self.listener.get()
        {
            listener.unpark();
        }
    }

    /// Blocks until the doorbell rings or `timeout` passes, reporting whether it rang.
    ///
    /// The first thread to wait becomes the only one ever woken. A ring that came before the wait
    /// is not lost: it is seen at once.
    pub fn wait(&self, timeout: Duration) -> bool {
        let _ = self.listener.set(std::thread::current());
        if self.rung.swap(false, Ordering::AcqRel) {
            return true;
        }
        std::thread::park_timeout(timeout);
        self.rung.swap(false, Ordering::AcqRel)
    }
}

/// The producing half, held by a platform callback.
///
/// Every method here is safe to call from a real-time context: no allocation, no locking, no
/// blocking, and no arithmetic that can panic.
pub struct RtProducer {
    events: rtrb::Producer<RtEvent>,
    sysex: rtrb::Producer<u8>,
    dropped: Arc<AtomicU64>,
    malformed: Arc<AtomicU64>,
    source: EndpointId,
    doorbell: Option<Arc<Doorbell>>,
}

impl RtProducer {
    /// Wakes the drain, if one is listening, after something was accepted.
    fn announce(&self) {
        if let Some(doorbell) = &self.doorbell {
            doorbell.ring();
        }
    }

    /// Pushes a message, counting it as dropped if the ring is full.
    ///
    /// Returns whether it was accepted, though callers on the real-time path usually ignore that:
    /// there is nothing useful they could do about a full ring.
    pub fn push(&mut self, message: MidiMessage, timestamp: u64) -> bool {
        let event = RtEvent::message(self.source, timestamp, message);
        match self.events.push(event) {
            Ok(()) => {
                self.announce();
                true
            }
            Err(_) => {
                // Counting rather than growing. A buffer that grows under load allocates on the
                // real-time path, which is the thing this whole structure exists to avoid.
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Pushes a run of system-exclusive bytes.
    ///
    /// The bytes go into their own ring and the event records only how many, so the event itself
    /// stays fixed-size. Both rings are written from this thread, so their order is preserved
    /// without synchronisation.
    pub fn push_sysex(&mut self, bytes: &[u8], end: SysExEnd, timestamp: u64) -> bool {
        let Ok(len) = u16::try_from(bytes.len()) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if self.sysex.slots() < bytes.len() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }

        // Write the bytes first, so a consumer that sees the event always finds them present.
        for byte in bytes {
            if self.sysex.push(*byte).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        }

        let event = RtEvent {
            source: self.source,
            timestamp,
            payload: RtPayload::SysEx { len, end },
        };
        if self.events.push(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.announce();
        true
    }

    /// Returns how many events this endpoint has dropped for want of space.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Counts one packet from the device that could not be decoded and was discarded.
    pub fn record_malformed(&self) {
        self.malformed.fetch_add(1, Ordering::Relaxed);
    }
}

/// The consuming half, drained by an ordinary task.
pub struct RtConsumer {
    events: rtrb::Consumer<RtEvent>,
    sysex: rtrb::Consumer<u8>,
    dropped: Arc<AtomicU64>,
    malformed: Arc<AtomicU64>,
    source: EndpointId,
    /// Which of the endpoint's MIDI In connectors this ring carries, counting from zero.
    connector: u8,
}

/// One drained event, with any system-exclusive bytes gathered up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drained {
    /// A message that travelled inline.
    Message {
        /// When it arrived.
        timestamp: u64,
        /// The message.
        message: MidiMessage,
    },
    /// A run of system-exclusive bytes.
    SysEx {
        /// When it arrived.
        timestamp: u64,
        /// The bytes, including the framing at either end of the message.
        bytes: Vec<u8>,
        /// Whether the message is finished, and whether it finished properly.
        end: SysExEnd,
    },
}

impl RtConsumer {
    /// Returns which endpoint this consumer drains.
    pub fn source(&self) -> EndpointId {
        self.source
    }

    /// Returns which of the endpoint's MIDI In connectors this consumer drains, counting from
    /// zero.
    ///
    /// Held by the ring rather than by each event: every connector has a ring of its own, so a
    /// system-exclusive dump arriving on one is never interleaved with one arriving on another.
    pub fn connector(&self) -> u8 {
        self.connector
    }

    /// Returns how many events were dropped for want of space.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Returns how many packets from the device could not be decoded.
    pub fn malformed(&self) -> u64 {
        self.malformed.load(Ordering::Relaxed)
    }

    /// Takes everything waiting, up to `limit` events.
    ///
    /// Bounded so one very busy endpoint cannot starve the others sharing this task.
    pub fn drain(&mut self, limit: usize) -> Vec<Drained> {
        let mut taken = Vec::new();

        while taken.len() < limit {
            let Ok(event) = self.events.pop() else {
                break;
            };
            match event.payload {
                RtPayload::Message(message) => {
                    taken.push(Drained::Message {
                        timestamp: event.timestamp,
                        message,
                    });
                }
                RtPayload::SysEx { len, end } => {
                    let mut bytes = Vec::with_capacity(usize::from(len));
                    for _ in 0..len {
                        match self.sysex.pop() {
                            Ok(byte) => bytes.push(byte),
                            // The producer writes bytes before the event, so a short read means
                            // the rings have diverged and nothing further can be trusted.
                            Err(_) => break,
                        }
                    }
                    taken.push(Drained::SysEx {
                        timestamp: event.timestamp,
                        bytes,
                        end,
                    });
                }
            }
        }
        taken
    }
}

/// Creates the two halves of one endpoint's data path.
///
/// Both rings are allocated here, once, so nothing on the real-time path ever allocates.
pub fn channel(source: EndpointId) -> (RtProducer, RtConsumer) {
    build(source, 0, None)
}

/// Creates the two halves of one endpoint's data path, ringing `doorbell` whenever something is
/// pushed.
pub fn channel_with_doorbell(
    source: EndpointId,
    doorbell: Arc<Doorbell>,
) -> (RtProducer, RtConsumer) {
    build(source, 0, Some(doorbell))
}

/// Creates the two halves of the data path for one of an endpoint's MIDI In connectors, counting
/// from zero, ringing `doorbell` whenever something is pushed.
pub fn connector_channel(
    source: EndpointId,
    connector: u8,
    doorbell: Arc<Doorbell>,
) -> (RtProducer, RtConsumer) {
    build(source, connector, Some(doorbell))
}

/// Allocates both rings for one endpoint connector.
fn build(
    source: EndpointId,
    connector: u8,
    doorbell: Option<Arc<Doorbell>>,
) -> (RtProducer, RtConsumer) {
    let (event_tx, event_rx) = rtrb::RingBuffer::new(EVENT_CAPACITY);
    let (sysex_tx, sysex_rx) = rtrb::RingBuffer::new(SYSEX_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    let malformed = Arc::new(AtomicU64::new(0));

    (
        RtProducer {
            events: event_tx,
            sysex: sysex_tx,
            dropped: Arc::clone(&dropped),
            malformed: Arc::clone(&malformed),
            source,
            doorbell,
        },
        RtConsumer {
            events: event_rx,
            sysex: sysex_rx,
            dropped,
            malformed,
            source,
            connector,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi::Channel;

    fn note() -> MidiMessage {
        MidiMessage::NoteOn {
            channel: Channel::new(0).expect("channel 0"),
            note: 60,
            velocity: 1,
        }
    }

    /// A push that lands before the drain waits is seen at once rather than lost.
    ///
    /// The daemon's listener falls back to looking once a second, so a lost wake costs up to a
    /// second of latency without failing any routing test; this and the next test are the only
    /// checks that the doorbell wakes at once.
    #[test]
    fn a_push_before_the_wait_is_seen_rather_than_lost() {
        let doorbell = Arc::new(Doorbell::new());
        let (mut producer, _consumer) =
            channel_with_doorbell(EndpointId::new(), Arc::clone(&doorbell));
        assert!(
            !doorbell.wait(Duration::from_millis(20)),
            "with nothing pushed the wait must end by timing out"
        );

        assert!(producer.push(note(), 0), "an empty ring must accept a note");
        let started = std::time::Instant::now();
        assert!(
            doorbell.wait(Duration::from_secs(5)),
            "a push before the wait must not be lost"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a push before the wait must be seen at once, not after the timeout"
        );
    }

    /// A push from another thread wakes a drain that is already waiting.
    ///
    /// This is the path a real-time callback takes. The pushing thread sleeps first only so the
    /// waiter is parked when the push lands; the assertion is on the wake, not on the timing.
    #[test]
    fn a_push_from_another_thread_wakes_the_waiting_drain() {
        let doorbell = Arc::new(Doorbell::new());
        let (mut producer, _consumer) =
            channel_with_doorbell(EndpointId::new(), Arc::clone(&doorbell));
        assert!(
            !doorbell.wait(Duration::from_millis(1)),
            "with nothing pushed the wait must end by timing out"
        );

        let pusher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            producer.push(note(), 0);
        });
        let started = std::time::Instant::now();
        assert!(
            doorbell.wait(Duration::from_secs(5)),
            "a push from another thread must wake the waiter"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the waiter must be woken by the push, not by the timeout"
        );
        pusher.join().expect("the pushing thread finishes");
    }
}
