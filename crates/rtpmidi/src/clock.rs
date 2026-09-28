//! Clock synchronisation, and the liveness signal derived from it.
//!
//! The three-message exchange estimates how far apart the two machines' clocks are and how long a
//! round trip takes. Its second job matters more: a peer that stops answering is how a silent
//! disconnection is noticed. Nothing else on an idle session would reveal that the far side is
//! gone, so this is the authoritative liveness check and the platform's sleep and network signals
//! are only an optimisation on top of it.

use crate::control::ControlPacket;
use std::time::Duration;

/// Wire timestamps count hundred-microsecond units.
pub const TICKS_PER_SECOND: u64 = 10_000;

/// How long to wait between exchanges once a session has settled.
///
/// Apple's implementation requires an exchange at least once every 60 seconds, so this leaves
/// room for several to be lost before a peer would consider us gone.
pub const STEADY_INTERVAL: Duration = Duration::from_secs(10);

/// How long to wait between exchanges while an estimate is still being established.
pub const INITIAL_INTERVAL: Duration = Duration::from_millis(250);

/// How many exchanges to run at the short interval before settling down.
///
/// Apple's Network MIDI, when invited, carries no MIDI in either direction until about six
/// exchanges have completed, counting its own. At the steady interval that took twenty seconds,
/// so eight run at the short interval, as Apple's own initiator runs a burst early on (R-068).
const INITIAL_EXCHANGES: u32 = 8;

/// How long without a completed exchange before the peer is considered gone.
///
/// Three missed steady-interval exchanges plus margin. Long enough not to tear down a session
/// over one lost packet, short enough that a dead peer is noticed well inside the ten seconds
/// the recovery budget allows.
pub const LIVENESS_TIMEOUT: Duration = Duration::from_secs(35);

/// Converts a duration into wire timestamp units, saturating rather than wrapping.
pub fn ticks_from(duration: Duration) -> u64 {
    duration
        .as_micros()
        .saturating_div(100)
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Converts wire timestamp units into a duration.
pub fn duration_from(ticks: u64) -> Duration {
    Duration::from_micros(ticks.saturating_mul(100))
}

/// What a received clock packet asked the receiver to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockAction {
    /// Send this packet to continue the exchange.
    Reply(ControlPacket),
    /// The exchange finished and the estimate was updated.
    Completed,
    /// The packet did not belong to a clock exchange, or carried a count this side cannot answer.
    Ignored,
}

/// Tracks the estimated relationship between this machine's clock and a peer's.
#[derive(Debug, Clone, Default)]
pub struct ClockSync {
    offset_ticks: i64,
    round_trip_ticks: u64,
    last_completed: Option<u64>,
    completed_exchanges: u32,
}

impl ClockSync {
    /// Creates an estimate with nothing measured yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns how far the peer's clock runs ahead of this one, in wire units.
    pub fn offset_ticks(&self) -> i64 {
        self.offset_ticks
    }

    /// Returns the most recent round-trip measurement.
    pub fn round_trip(&self) -> Duration {
        duration_from(self.round_trip_ticks)
    }

    /// Reports whether any exchange has completed, so the estimate means something.
    pub fn is_established(&self) -> bool {
        self.last_completed.is_some()
    }

    /// Returns how long since the last completed exchange.
    pub fn since_last_exchange(&self, now_ticks: u64) -> Option<Duration> {
        let last = self.last_completed?;
        Some(duration_from(now_ticks.saturating_sub(last)))
    }

    /// Reports whether the peer is still answering.
    ///
    /// A session that has never completed an exchange is not yet considered dead: it is still
    /// starting up, and tearing it down here would prevent it ever establishing.
    pub fn is_alive(&self, now_ticks: u64) -> bool {
        match self.since_last_exchange(now_ticks) {
            Some(elapsed) => elapsed < LIVENESS_TIMEOUT,
            None => true,
        }
    }

    /// Returns how long to wait before starting the next exchange.
    ///
    /// Short while the estimate is being established, then long enough to stay well inside the
    /// peer's own timeout without spending bandwidth on an idle session.
    pub fn next_interval(&self) -> Duration {
        if self.completed_exchanges < INITIAL_EXCHANGES {
            INITIAL_INTERVAL
        } else {
            STEADY_INTERVAL
        }
    }

    /// Builds the first packet of an exchange.
    pub fn begin(&self, ssrc: u32, now_ticks: u64) -> ControlPacket {
        ControlPacket::ClockSync {
            ssrc,
            count: 0,
            timestamps: [now_ticks, 0, 0],
        }
    }

    /// Handles a clock packet from a peer, returning what to do next.
    ///
    /// `ssrc` identifies this endpoint, so a reply carries our own source rather than echoing the
    /// peer's.
    pub fn handle(&mut self, packet: &ControlPacket, ssrc: u32, now_ticks: u64) -> ClockAction {
        let ControlPacket::ClockSync {
            count, timestamps, ..
        } = packet
        else {
            return ClockAction::Ignored;
        };

        match count {
            // The peer opened an exchange, so answer with our own time.
            0 => {
                let first = timestamps.first().copied().unwrap_or(0);
                ClockAction::Reply(ControlPacket::ClockSync {
                    ssrc,
                    count: 1,
                    timestamps: [first, now_ticks, 0],
                })
            }
            // The peer answered ours, so close the exchange and record the estimate.
            1 => {
                let (t0, t1) = (
                    timestamps.first().copied().unwrap_or(0),
                    timestamps.get(1).copied().unwrap_or(0),
                );
                // We opened the exchange, so every timestamp but the peer's is ours.
                let (round_trip, peer_offset) = measure(t0, t1, now_ticks);
                self.record(round_trip, peer_offset, now_ticks);
                ClockAction::Reply(ControlPacket::ClockSync {
                    ssrc,
                    count: 2,
                    timestamps: [t0, t1, now_ticks],
                })
            }
            // The initiator closed the exchange, so we can measure it from this side too.
            2 => {
                let (t0, t1, t2) = (
                    timestamps.first().copied().unwrap_or(0),
                    timestamps.get(1).copied().unwrap_or(0),
                    timestamps.get(2).copied().unwrap_or(0),
                );
                // The peer opened it, so `t0` and `t2` are on the peer's clock and only `t1` is
                // ours. The exchange is dated by our own clock, because that is the clock liveness
                // is judged against. Dating it by the peer's made the responder's liveness depend
                // on how long before or after us the peer started: behind by more than the
                // timeout, and it dropped a working session moments after accepting it; ahead,
                // and it would never notice a dead one.
                let (round_trip, our_offset) = measure(t0, t1, t2);
                self.record(round_trip, our_offset.saturating_neg(), now_ticks);
                ClockAction::Completed
            }
            _ => ClockAction::Ignored,
        }
    }

    /// Records one completed exchange.
    ///
    /// `peer_offset` is how far the peer's clock runs ahead of ours, and `completed_at` is when
    /// the exchange completed, on our own clock.
    fn record(&mut self, round_trip: u64, peer_offset: i64, completed_at: u64) {
        self.round_trip_ticks = round_trip;
        self.offset_ticks = peer_offset;
        self.last_completed = Some(completed_at);
        self.completed_exchanges = self.completed_exchanges.saturating_add(1);
    }

    /// Converts a timestamp from the peer's clock into this machine's.
    pub fn to_local(&self, peer_ticks: u64) -> u64 {
        let local = i64::try_from(peer_ticks)
            .unwrap_or(i64::MAX)
            .saturating_sub(self.offset_ticks);
        u64::try_from(local).unwrap_or(0)
    }
}

/// Measures one exchange from its three timestamps.
///
/// `t0` and `t2` are the initiator's send and receive times; `t1` is the responder's. Returns the
/// round trip the initiator observed, and how far the responder's clock sits from the midpoint of
/// that span.
fn measure(t0: u64, t1: u64, t2: u64) -> (u64, i64) {
    // A peer whose timestamps run backwards is misbehaving, not a reason to produce a wild
    // estimate, so the span saturates at zero.
    let round_trip = t2.saturating_sub(t0);
    // Halving the observed span is exactly what the midpoint is; the divisor is a constant and
    // cannot be zero.
    #[allow(clippy::integer_division)]
    let half_span = round_trip / 2;
    let midpoint = t0.saturating_add(half_span);
    let responder_offset = i64::try_from(t1)
        .unwrap_or(i64::MAX)
        .saturating_sub(i64::try_from(midpoint).unwrap_or(i64::MAX));
    (round_trip, responder_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    const US: u32 = 0x1111_1111;
    const THEM: u32 = 0x2222_2222;

    /// Where [`exchange`] starts the initiator's clock, in wire ticks.
    const START: u64 = 1_000_000;

    /// Runs a full exchange between two sides and returns both estimates.
    ///
    /// `peer_ahead` is how far the responder's clock runs ahead of the initiator's, and `one_way`
    /// is the transit time in wire units, so the expected offset and round trip are known exactly.
    /// The initiator completes at `START + 2 * one_way` on its own clock and the responder at
    /// `START + 3 * one_way + peer_ahead` on its own.
    fn exchange(peer_ahead: i64, one_way: u64) -> (ClockSync, ClockSync) {
        let (mut initiator, mut responder) = (ClockSync::new(), ClockSync::new());
        let mut local = START;
        let peer_clock =
            |local: u64| u64::try_from(i64::try_from(local).unwrap() + peer_ahead).unwrap();

        // Our side opens the exchange.
        let first = initiator.begin(US, local);

        // It arrives one transit later, on the peer's clock.
        local += one_way;
        let ClockAction::Reply(second) = responder.handle(&first, THEM, peer_clock(local)) else {
            panic!("a responder must answer an opening packet");
        };

        // The answer comes back to us.
        local += one_way;
        let ClockAction::Reply(third) = initiator.handle(&second, US, local) else {
            panic!("an initiator must close the exchange it opened");
        };

        // The closing packet reaches the peer.
        local += one_way;
        assert_eq!(
            responder.handle(&third, THEM, peer_clock(local)),
            ClockAction::Completed,
            "the closing packet must complete the responder's side of the exchange"
        );

        (initiator, responder)
    }

    /// Both sides of one exchange measure the same offset with opposite signs, and the same
    /// round trip, as Apple's three-message exchange defines them.
    ///
    /// Transit is symmetric here, so the midpoint estimate lands on the real offset exactly: with
    /// one-way transit `d`, the responder's timestamp `t1 = t0 + d + ahead` sits `ahead` past the
    /// midpoint `t0 + (2d) / 2`, and the round trip is `2d`. Converting a peer timestamp subtracts
    /// the offset, so `START + ahead` on the peer's clock is `START` on ours.
    #[test]
    fn an_exchange_measures_offset_and_round_trip_from_both_sides() {
        let cases = [
            ("clocks that agree", 0_i64, 50_u64),
            ("a peer three seconds ahead", 30_000, 10),
            ("a peer four and a half seconds behind", -45_000, 10),
        ];
        for (name, ahead, one_way) in cases {
            let (initiator, responder) = exchange(ahead, one_way);
            let peer_start = u64::try_from(i64::try_from(START).unwrap() + ahead).unwrap();

            assert_eq!(
                initiator.offset_ticks(),
                ahead,
                "{name}: the initiator must see the peer's clock this far ahead"
            );
            assert_eq!(
                responder.offset_ticks(),
                -ahead,
                "{name}: the responder must see the initiator's clock with the opposite sign"
            );
            for (side, sync) in [("initiator", &initiator), ("responder", &responder)] {
                assert_eq!(
                    sync.round_trip(),
                    duration_from(2 * one_way),
                    "{name}: the {side}'s round trip is two transits"
                );
            }
            assert_eq!(
                initiator.to_local(peer_start),
                START,
                "{name}: a peer timestamp must convert to the same instant on the initiator"
            );
            assert_eq!(
                responder.to_local(START),
                peer_start,
                "{name}: an initiator timestamp must convert to the same instant on the responder"
            );
        }
    }

    /// Liveness is judged on each side's own clock, and a peer is declared gone exactly when
    /// `LIVENESS_TIMEOUT` has passed since the last completed exchange.
    ///
    /// The responder rows are regressions. Dating the responder's exchange by the initiator's
    /// timestamps put it a minute in the past when the responder's clock ran a minute ahead, so
    /// a session was dropped moments after being accepted, and a minute in the future when it ran
    /// behind, so a dead peer was never noticed. A minute is `60 * 10_000 = 600_000` ticks, and
    /// with a one-way transit of 10 the initiator completes at `START + 20` and the responder at
    /// `START + 30 + ahead`.
    #[test]
    fn a_peer_is_declared_gone_exactly_at_the_liveness_timeout() {
        let minute = i64::try_from(ticks_from(Duration::from_secs(60))).unwrap();
        let cases = [
            ("the initiator", 0_i64, true, START + 20),
            (
                "a responder a minute ahead of its peer",
                minute,
                false,
                START + 30 + minute.unsigned_abs(),
            ),
            (
                "a responder a minute behind its peer",
                -minute,
                false,
                START + 30 - minute.unsigned_abs(),
            ),
        ];
        let timeout = ticks_from(LIVENESS_TIMEOUT);
        for (name, ahead, is_initiator, completed) in cases {
            let (initiator, responder) = exchange(ahead, 10);
            let sync = if is_initiator { initiator } else { responder };

            assert!(
                sync.is_alive(completed),
                "{name}: a peer just heard from must be alive"
            );
            assert!(
                sync.is_alive(completed + timeout - 1),
                "{name}: a peer must be alive until the timeout has fully passed"
            );
            assert!(
                !sync.is_alive(completed + timeout),
                "{name}: a peer silent for the whole timeout must be declared gone"
            );
        }
    }

    /// Apple's Network MIDI gets the six completed exchanges it waits for within three seconds.
    ///
    /// Invited, Apple carries no MIDI in either direction until about six exchanges have
    /// completed (R-068). At the 10-second steady interval that took twenty seconds; eight at
    /// the 250 ms initial interval take `8 * 250 ms = 2 s`.
    #[test]
    fn apple_gets_the_exchanges_it_waits_for_within_seconds() {
        let mut sync = ClockSync::new();
        let mut elapsed = Duration::ZERO;
        let mut exchanges = 0;
        while sync.next_interval() < STEADY_INTERVAL {
            elapsed += sync.next_interval();
            sync.record(0, 0, 0);
            exchanges += 1;
        }
        assert!(
            exchanges >= 6,
            "only {exchanges} exchanges ran before settling, and Apple waits for six"
        );
        assert!(
            elapsed <= Duration::from_secs(3),
            "settling took {elapsed:?}, so Apple would carry no MIDI for that long"
        );
    }

    /// The liveness timeout sits between several missed steady exchanges and Apple's limit.
    ///
    /// Apple's implementation requires an exchange at least once every 60 seconds. Below that, a
    /// timeout of more than three steady intervals (`3 * 10 s = 30 s`) survives a few lost
    /// packets without tearing down a working session.
    #[test]
    fn the_steady_interval_leaves_room_for_lost_exchanges() {
        assert!(
            LIVENESS_TIMEOUT > STEADY_INTERVAL * 3,
            "three lost exchanges must not be enough to declare a peer gone"
        );
        assert!(
            LIVENESS_TIMEOUT < Duration::from_secs(60),
            "a peer must be noticed gone before Apple's own 60-second limit"
        );
    }

    /// Timestamps a peer controls produce a bounded estimate without panicking or wrapping.
    ///
    /// A peer can put anything in these fields. Receive time before send time cannot happen
    /// honestly, so the round trip `t2 - t0` saturates at zero rather than wrapping to nearly
    /// `u64::MAX`, and a span that really is `u64::MAX` stays there.
    #[test]
    fn hostile_timestamps_saturate_rather_than_wrap() {
        let cases = [
            ("receive before send", 1_000, 500, 200, 0),
            ("the widest honest span", 0, u64::MAX, u64::MAX, u64::MAX),
            ("send at the end of time", u64::MAX, 0, 0, 0),
            ("a responder at the end of time", u64::MAX, u64::MAX, 0, 0),
            ("receive at the end of time", 0, 0, u64::MAX, u64::MAX),
        ];
        for (name, t0, t1, t2, want_round_trip) in cases {
            let mut sync = ClockSync::new();
            let answer = ControlPacket::ClockSync {
                ssrc: THEM,
                count: 1,
                timestamps: [t0, t1, 0],
            };
            let _ = sync.handle(&answer, US, t2);

            assert_eq!(
                sync.round_trip(),
                duration_from(want_round_trip),
                "{name}: the round trip must saturate rather than wrap"
            );
            // Conversion and liveness must stay in range whatever the estimate turned out to be.
            let _ = sync.to_local(0);
            let _ = sync.to_local(u64::MAX);
            let _ = sync.is_alive(u64::MAX);
        }
    }

    /// A responder answers an opening packet with count 1, the initiator's time echoed first, its
    /// own time second, and its own SSRC.
    ///
    /// That is the layout of Apple's `CK` exchange: the initiator measures the round trip from
    /// the first timestamp and the offset from the second, and matches the reply by source.
    #[test]
    fn the_responder_answers_an_opening_packet_with_its_own_time() {
        let mut sync = ClockSync::new();
        let opening = ControlPacket::ClockSync {
            ssrc: THEM,
            count: 0,
            timestamps: [77, 0, 0],
        };

        assert_eq!(
            sync.handle(&opening, US, 999),
            ClockAction::Reply(ControlPacket::ClockSync {
                ssrc: US,
                count: 1,
                timestamps: [77, 999, 0],
            }),
            "a reply carries our source and both times the initiator needs"
        );
    }
}
