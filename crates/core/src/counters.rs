//! Per-connection traffic counters.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Sentinel stored in `last_activity` when nothing has been seen yet.
const NO_ACTIVITY: i64 = i64::MIN;

/// Live traffic counts for one endpoint or route.
///
/// Every field is an atomic because these are incremented from the real-time path, where taking a
/// lock or allocating is forbidden. Reads are approximate under concurrent writes, which is
/// correct for a display: exact cross-field consistency is not worth stalling MIDI for.
#[derive(Debug, Default)]
pub struct TrafficCounters {
    messages_sent: AtomicU64,
    messages_received: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
    messages_lost: AtomicU64,
    messages_recovered: AtomicU64,
    messages_dropped: AtomicU64,
    packets_malformed: AtomicU64,
    last_activity: AtomicI64,
    last_received: AtomicI64,
    last_sent: AtomicI64,
}

impl TrafficCounters {
    /// Creates counters with every value at zero and no activity recorded.
    pub fn new() -> Self {
        Self {
            last_activity: AtomicI64::new(NO_ACTIVITY),
            last_received: AtomicI64::new(NO_ACTIVITY),
            last_sent: AtomicI64::new(NO_ACTIVITY),
            ..Self::default()
        }
    }

    /// Records a message sent, with its encoded size.
    pub fn record_sent(&self, bytes: u64, at: Timestamp) {
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        let nanos = self.touch(at);
        self.last_sent.store(nanos, Ordering::Relaxed);
    }

    /// Records a message received, with its encoded size.
    pub fn record_received(&self, bytes: u64, at: Timestamp) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
        let nanos = self.touch(at);
        self.last_received.store(nanos, Ordering::Relaxed);
    }

    /// Records a message we discarded ourselves because a buffer was full.
    ///
    /// Deliberately separate from `messages_lost`: this is our own backpressure, not the
    /// network's packet loss, and conflating the two would hide a capacity problem behind a
    /// network excuse.
    pub fn record_dropped(&self, count: u64) {
        self.messages_dropped.fetch_add(count, Ordering::Relaxed);
    }

    /// Records packets from the device that could not be decoded and were discarded whole.
    ///
    /// Separate from both loss and drops: the device sent something wrong, which neither the
    /// network nor our own capacity explains.
    pub fn record_malformed(&self, count: u64) {
        self.packets_malformed.fetch_add(count, Ordering::Relaxed);
    }

    /// Returns an immediate copy for display or serialisation.
    pub fn snapshot(&self) -> CounterSnapshot {
        let time = |stored: &AtomicI64| match stored.load(Ordering::Relaxed) {
            NO_ACTIVITY => None,
            nanos => Timestamp::from_nanosecond(i128::from(nanos)).ok(),
        };
        CounterSnapshot {
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            messages_received: self.messages_received.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            messages_lost: self.messages_lost.load(Ordering::Relaxed),
            messages_recovered: self.messages_recovered.load(Ordering::Relaxed),
            messages_dropped: self.messages_dropped.load(Ordering::Relaxed),
            packets_malformed: self.packets_malformed.load(Ordering::Relaxed),
            last_activity: time(&self.last_activity),
            last_received: time(&self.last_received),
            last_sent: time(&self.last_sent),
        }
    }

    /// Stores the time of the most recent traffic, saturating rather than failing on odd clocks,
    /// and returns it as stored for the direction's own time.
    fn touch(&self, at: Timestamp) -> i64 {
        let nanos = i64::try_from(at.as_nanosecond()).unwrap_or(i64::MAX);
        self.last_activity.store(nanos, Ordering::Relaxed);
        nanos
    }
}

/// An immediate copy of a set of counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterSnapshot {
    /// Messages handed to the transport.
    pub messages_sent: u64,
    /// Messages taken from the transport.
    pub messages_received: u64,
    /// Encoded bytes sent.
    pub bytes_sent: u64,
    /// Encoded bytes received.
    pub bytes_received: u64,
    /// Messages the network lost.
    pub messages_lost: u64,
    /// Messages rebuilt from the recovery journal.
    pub messages_recovered: u64,
    /// Messages we discarded because a buffer was full.
    pub messages_dropped: u64,
    /// Packets from the device that could not be decoded.
    #[serde(default)]
    pub packets_malformed: u64,
    /// When traffic was last seen.
    pub last_activity: Option<Timestamp>,
    /// When a message was last received.
    #[serde(default)]
    pub last_received: Option<Timestamp>,
    /// When a message was last sent.
    #[serde(default)]
    pub last_sent: Option<Timestamp>,
}
