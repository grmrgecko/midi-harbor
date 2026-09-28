//! Delivering a Bluetooth device's messages when its own clock says they were played (FR-019).
//!
//! A Bluetooth MIDI device collects what is played into packets sent once per connection
//! interval, 7.5 ms or more, and stamps each message with the millisecond it was played. Delivered
//! as they arrive, a chord played over a few milliseconds lands all at once and a run played
//! evenly lands in bunches. Holding each message until its timestamp, measured against the latest
//! a packet has run, gives back the spacing the player made, at the cost of that much latency.

use std::time::Duration;

/// The longest any message is held. A connection interval is 7.5 to 15 ms on most devices, and
/// holding longer than about one of them trades more latency than the spacing is worth.
pub const MAX_WAIT: Duration = Duration::from_millis(10);

/// How fast the anchor relaxes after a late packet: by 1/64 of the device time that passes, a
/// right shift of six, so one delayed packet does not keep every later message waiting.
const RELAX_SHIFT: u32 = 6;

/// Relates a device's millisecond clock to this machine's, and says how long to hold a message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceTiming {
    /// Local minus device time for the latest arrival seen, relaxed over time, in microseconds so
    /// relaxation too small to show in milliseconds still accumulates. Holding a message until
    /// its device time plus this delivers it in step with that arrival.
    anchor_us: Option<i64>,
    /// The device time last seen, from which relaxation is measured.
    last_device: Option<u64>,
}

impl DeviceTiming {
    /// Creates a relation with nothing observed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns how long to hold a message stamped `device_ms` that arrived at `local_ms`.
    ///
    /// Both are milliseconds on their own clocks, each counting forward. A message that arrived
    /// as late as any before it is not held at all; one that arrived early, relative to the
    /// others, is held by how early it came, up to `MAX_WAIT`.
    pub fn hold(&mut self, device_ms: u64, local_ms: u64) -> Duration {
        let micros = |millis: u64| {
            i64::try_from(millis)
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000)
        };
        let lateness = micros(local_ms).saturating_sub(micros(device_ms));

        // Relax the anchor by a share of the device time that has passed since the last message.
        let relaxed = match (self.anchor_us, self.last_device) {
            (Some(anchor), Some(last)) => {
                let passed = micros(device_ms.saturating_sub(last));
                anchor.saturating_sub(passed >> RELAX_SHIFT)
            }
            (anchor, _) => anchor.unwrap_or(lateness),
        };
        self.last_device = Some(device_ms);

        // Never earlier than this arrival, which has already happened, and never so far ahead
        // that the hold exceeds the bound.
        let bound = i64::try_from(MAX_WAIT.as_micros()).unwrap_or(i64::MAX);
        let anchor = relaxed.max(lateness).min(lateness.saturating_add(bound));
        self.anchor_us = Some(anchor);

        Duration::from_micros(u64::try_from(anchor.saturating_sub(lateness)).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    /// Reports whether a hold is within a tenth of a millisecond of what was expected, which
    /// relaxation between notes of one packet may take off.
    fn about(held: Duration, expected: Duration) -> bool {
        held.abs_diff(expected) <= Duration::from_micros(100)
    }

    /// Each message is held until its device timestamp, measured against the latest arrival, and
    /// never longer than `MAX_WAIT` (FR-019).
    ///
    /// Each arrival is (device ms, local ms, wanted hold). A packet stamped 1000 ms arriving at
    /// 20 ms sets the anchor; the next packet, one 10 ms connection interval later, arrives on
    /// time, so its first note is held 0 ms and a note played 3 ms after it is held 3 ms. Only
    /// differences matter, so a device clock millions of milliseconds away relates the same
    /// way. A note stamped 40 ms after one it arrived with would be held 40 ms, so the 10 ms
    /// bound applies.
    #[test]
    fn a_message_is_held_until_its_device_time_within_the_bound() {
        type Arrivals = &'static [(u64, u64, u64)];
        let cases: [(&str, Arrivals); 3] = [
            (
                "an on-time packet holds only its later notes",
                &[(1_000, 20, 0), (1_010, 30, 0), (1_013, 30, 3)],
            ),
            (
                "clocks far apart still relate by their differences",
                &[(5, 9_000_000, 0), (9, 9_000_000, 4)],
            ),
            (
                "nothing is held longer than the bound",
                &[(1_000, 50, 0), (1_040, 50, 10)],
            ),
        ];
        for (case, arrivals) in cases {
            let mut timing = DeviceTiming::new();
            for (device, local, want) in arrivals {
                let held = timing.hold(*device, *local);
                assert!(
                    about(held, ms(*want)),
                    "{case}: stamped {device} and arriving at {local}, held {held:?} rather than \
                     {want} ms, so the spacing the player made is lost"
                );
            }
        }
    }

    /// One late packet raises the anchor, and the anchor relaxes until on-time packets are not
    /// held at all.
    ///
    /// A packet 8 ms late sets the anchor 8 ms later than on-time packets need. It relaxes by
    /// 1/64 of the device time that passes, so each 10 ms packet takes 10 / 64 = 0.156 ms off,
    /// and 8 ms is gone after about 52 packets; 200 are ample. Without relaxation one delayed
    /// packet would add 8 ms of latency to everything after it.
    #[test]
    fn one_late_packet_does_not_keep_everything_after_it_waiting() {
        let mut timing = DeviceTiming::new();
        let _ = timing.hold(1_000, 20);
        assert_eq!(
            timing.hold(1_010, 38),
            ms(0),
            "a late packet has already waited and must not be held further"
        );
        let early = timing.hold(1_020, 40);
        assert!(
            early > ms(0),
            "the packet after a late one must still wait while the anchor is raised"
        );

        let (mut device, mut local, mut held) = (1_020, 40, early);
        for _ in 0..200 {
            device += 10;
            local += 10;
            held = timing.hold(device, local);
        }
        assert_eq!(
            held,
            ms(0),
            "the anchor must relax until on-time packets are not held"
        );
    }
}
