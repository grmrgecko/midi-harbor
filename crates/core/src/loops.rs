//! Spotting MIDI that has come back around a loop across machines.
//!
//! A loop within one machine is visible in its route graph. One across machines is not: machine
//! A sends a keyboard to B over a session, B routes that session back to A over another, and A
//! routes that one to the first. Each machine's routes look sensible, and a note goes round for
//! as long as the machines run. RTP-MIDI carries plain MIDI, with nothing to say where a message
//! began, so the loop has to be recognised from what it does.
//!
//! Routing is direct (R-019), so MIDI arriving on a session can leave on another session only
//! through a route from one to the other, and every loop across machines passes through such a
//! route on some machine. A message is an echo when it is about to leave through a session it
//! left moments ago from a different source. A route that relays one peer to another sees its
//! own earlier forwards come from the same source, so relaying repeated notes is not an echo.

use crate::ids::EndpointId;
use crate::midi::MidiMessage;
use crate::time;
use jiff::Timestamp;
use std::time::Duration;

/// How recently a message must have left for its return to count as an echo.
///
/// Well above a round trip across two machines on a local network, a few milliseconds, and well
/// below the gap between two notes a person plays.
pub const ECHO_WINDOW: Duration = Duration::from_millis(250);

/// How many echoes within `TRIP_WINDOW` mean a loop.
///
/// A loop echoes every message on every round, hundreds of times a second. Two players striking
/// the same note on two machines at once cannot reach this many in a quarter of a second.
pub const TRIP_ECHOES: u32 = 16;

/// The window `TRIP_ECHOES` must fall within.
pub const TRIP_WINDOW: Duration = Duration::from_millis(250);

/// How many recent sends a session remembers.
///
/// Fixed, so recording never allocates. At 512 messages a second, 250 ms of sends fit.
const CAPACITY: usize = 128;

/// One message sent out through a session, and where it came from.
#[derive(Debug, Clone, Copy)]
struct Sent {
    message: MidiMessage,
    source: EndpointId,
    at: Timestamp,
}

/// The messages a session sent out recently.
#[derive(Debug, Clone)]
pub struct SentLog {
    entries: [Option<Sent>; CAPACITY],
    next: usize,
}

impl Default for SentLog {
    fn default() -> Self {
        Self {
            entries: [None; CAPACITY],
            next: 0,
        }
    }
}

impl SentLog {
    /// Creates a log with nothing in it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a message sent out through the session, overwriting the oldest when full.
    pub fn record(&mut self, message: MidiMessage, source: EndpointId, at: Timestamp) {
        if let Some(slot) = self.entries.get_mut(self.next) {
            *slot = Some(Sent {
                message,
                source,
                at,
            });
        }
        self.next = (self.next + 1) % CAPACITY;
    }

    /// Reports whether a message arriving from `via` is one this session sent moments ago from
    /// somewhere else.
    pub fn came_back(&self, message: &MidiMessage, via: EndpointId, now: Timestamp) -> bool {
        self.entries.iter().flatten().any(|sent| {
            sent.message == *message
                && sent.source != via
                && time::elapsed(sent.at, now) <= ECHO_WINDOW
        })
    }
}

/// Counts the echoes on one route, and says when they amount to a loop.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoopWatch {
    first: Option<Timestamp>,
    echoes: u32,
}

impl LoopWatch {
    /// Creates a watch that has seen no echoes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one echo, reporting true exactly once when the echoes within `TRIP_WINDOW` reach
    /// `TRIP_ECHOES`, so a loop is acted on once rather than for every message it carries.
    pub fn echo(&mut self, now: Timestamp) -> bool {
        match self.first {
            Some(first) if time::elapsed(first, now) <= TRIP_WINDOW => {
                self.echoes = self.echoes.saturating_add(1);
            }
            _ => {
                self.first = Some(now);
                self.echoes = 1;
            }
        }
        self.echoes == TRIP_ECHOES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: i64) -> Timestamp {
        Timestamp::from_millisecond(1_790_000_000_000 + ms).expect("a timestamp")
    }

    /// A loop is reported once, and players who happen to repeat each other never trip it.
    ///
    /// A loop echoes every message on every round: one echo a millisecond for 100 ms reaches the
    /// 16 echoes within 250 ms at the sixteenth and is reported that once, so the route is
    /// switched off once rather than for every message the loop carries. Two players striking
    /// the same note eight times a second echo every 125 ms, so any 250 ms window holds at most
    /// three echoes (0, 125 and 250 ms) and never reaches 16.
    #[test]
    fn a_loop_trips_once_and_coincidences_never_do() {
        let cases = [
            ("a loop echoing every millisecond", 100, 1, 1),
            ("two players eight beats a second", 40, 125, 0),
        ];
        for (case, echoes, every_ms, want) in cases {
            let mut watch = LoopWatch::new();
            let trips = (0..echoes)
                .filter(|index: &i64| watch.echo(at(index * every_ms)))
                .count();
            assert_eq!(
                trips, want,
                "{case}: a loop must be acted on once and coincidence never"
            );
        }
    }
}
