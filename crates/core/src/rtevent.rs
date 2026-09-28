//! The fixed-size event that crosses the real-time boundary.
//!
//! A MIDI read callback runs in a real-time context: it may not allocate, lock, or block. So the
//! only thing it does is stamp what arrived and push it into a ring buffer for an ordinary task
//! to deal with. Everything here is `Copy` and fixed-size for that reason.
//!
//! System-exclusive data is unbounded and cannot travel inline. It goes through a separate byte
//! ring alongside this one: the event records how many bytes to read, and because both rings are
//! written by the same thread, ordering between them is preserved without any synchronisation.

use crate::ids::EndpointId;
use crate::midi::MidiMessage;
use crate::stream::SysExEnd;

/// What a real-time event carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtPayload {
    /// A channel or system message, small enough to travel inline.
    Message(MidiMessage),
    /// A run of system-exclusive bytes waiting in the byte ring.
    SysEx {
        /// How many bytes to read from the byte ring.
        len: u16,
        /// Whether the message is finished, and whether it finished properly.
        end: SysExEnd,
    },
}

/// One thing that happened on an endpoint, as seen from the real-time path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtEvent {
    /// Where it arrived, so the router knows what to do with it without a lookup by name.
    pub source: EndpointId,
    /// When it arrived, in the platform's own units.
    ///
    /// Taken in the callback rather than when the event is drained, because the delay before
    /// draining is exactly the jitter this timestamp exists to remove.
    pub timestamp: u64,
    /// What it carries.
    pub payload: RtPayload,
}

impl RtEvent {
    /// Creates an event carrying a single message.
    pub fn message(source: EndpointId, timestamp: u64, message: MidiMessage) -> Self {
        Self {
            source,
            timestamp,
            payload: RtPayload::Message(message),
        }
    }
}
