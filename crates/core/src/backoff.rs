//! Retry pacing for connections that are down.

use std::time::Duration;

/// Base delay before the first retry.
const DEFAULT_BASE: Duration = Duration::from_millis(250);
/// Each successive attempt multiplies the delay by this factor.
const DEFAULT_FACTOR: f64 = 2.0;
/// Ceiling on the delay, so a long outage still retries about twice a minute.
const DEFAULT_MAX: Duration = Duration::from_secs(30);

/// Ceiling for a connection whose retry costs a handful of packets.
///
/// This number is what SC-003 is made of. Nothing reports that an interruption ended when this
/// machine's own address never changed — an upstream router rebooting looks like silence — so the
/// ceiling is the longest a session can fail to notice that the network came back. Ten seconds is
/// the promise, and the handshake needs some of it.
const RESPONSIVE_MAX: Duration = Duration::from_secs(5);

/// How quickly retries back off, and how far.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackoffPolicy {
    /// Delay before the first retry.
    pub base: Duration,
    /// Multiplier applied per attempt.
    pub factor: f64,
    /// Upper bound on the computed delay.
    pub max: Duration,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            base: DEFAULT_BASE,
            factor: DEFAULT_FACTOR,
            max: DEFAULT_MAX,
        }
    }
}

impl BackoffPolicy {
    /// Pacing for a connection that is cheap to retry and expensive to leave down.
    ///
    /// A network session's retry is three small packets. Waiting half a minute between them
    /// saves nothing worth having and is most of the time a user spends wondering why their
    /// keyboard stopped working.
    pub fn responsive() -> Self {
        Self {
            max: RESPONSIVE_MAX,
            ..Self::default()
        }
    }
}

/// Tracks how many times a connection has been retried and how long to wait next.
///
/// The delay grows exponentially and is then capped, but it never stops being produced: there is
/// no attempt count at which this yields "give up". Principle I forbids a terminal failure state
/// for a connection the user has left enabled.
#[derive(Debug, Clone)]
pub struct Backoff {
    policy: BackoffPolicy,
    attempt: u32,
}

impl Backoff {
    /// Creates a backoff with the given policy and no attempts recorded.
    pub fn new(policy: BackoffPolicy) -> Self {
        Self { policy, attempt: 0 }
    }

    /// Returns how many attempts have been recorded.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Clears the attempt count, called once a connection succeeds.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Records an attempt and returns how long to wait before the next one.
    ///
    /// `jitter` is a fraction in `[0.0, 1.0]` scaling the computed delay, which spreads retries
    /// out when many connections fail together. Callers in production supply a random value; tests
    /// supply a fixed one so the result is deterministic.
    pub fn next_delay(&mut self, jitter: f64) -> Duration {
        // Compute the uncapped exponential delay for this attempt.
        let exponent = f64::from(self.attempt);
        let base = self.policy.base.as_secs_f64();
        let factor = if self.policy.factor.is_finite() && self.policy.factor >= 1.0 {
            self.policy.factor
        } else {
            DEFAULT_FACTOR
        };
        let grown = base * factor.powf(exponent);

        // Cap it, then apply full jitter over the capped value.
        let capped = grown.min(self.policy.max.as_secs_f64());
        let clamped_jitter = if jitter.is_finite() {
            jitter.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let delayed = capped * clamped_jitter;

        self.attempt = self.attempt.saturating_add(1);

        // A zero delay would spin, so hold a floor of one jittered millisecond.
        if !delayed.is_finite() || delayed <= 0.0 {
            return Duration::from_millis(1);
        }
        Duration::from_secs_f64(delayed).max(Duration::from_millis(1))
    }

    /// Records an attempt and returns a randomly jittered delay.
    pub fn next_delay_random(&mut self) -> Duration {
        self.next_delay(rand::random::<f64>())
    }

    /// Returns the uncapped-then-capped delay for the current attempt without jitter, for display.
    pub fn peek_ceiling(&self) -> Duration {
        let exponent = f64::from(self.attempt);
        let grown = self.policy.base.as_secs_f64() * self.policy.factor.powf(exponent);
        let capped = grown.min(self.policy.max.as_secs_f64());
        if capped.is_finite() && capped > 0.0 {
            Duration::from_secs_f64(capped)
        } else {
            self.policy.max
        }
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(BackoffPolicy::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The delay doubles from 250 ms and then holds at the 30 s ceiling for as long as retries
    /// continue.
    ///
    /// With full jitter (1.0) the delay is the ceiling itself: 0.25 s * 2^n for attempt n, so
    /// 0.25, 0.5, 1, 2, 4, 8 and 16 s, then 0.25 s * 2^7 = 32 s is capped at 30 s. Principle I
    /// forbids a terminal failure, so the ten-thousandth attempt still yields the ceiling rather
    /// than a signal to stop.
    #[test]
    fn the_delay_doubles_from_the_base_and_holds_at_the_ceiling_forever() {
        let mut backoff = Backoff::default();
        let want: Vec<Duration> = [250, 500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]
            .into_iter()
            .map(Duration::from_millis)
            .collect();
        let got: Vec<Duration> = want.iter().map(|_| backoff.next_delay(1.0)).collect();
        assert_eq!(
            got, want,
            "the delay must double from the base and stop at the ceiling"
        );

        for _ in want.len()..10_000 {
            let _ = backoff.next_delay(1.0);
        }
        assert_eq!(
            backoff.next_delay(1.0),
            DEFAULT_MAX,
            "no attempt count may turn the ceiling into giving up"
        );
    }

    /// Jitter scales the capped delay, and no jitter or policy value yields a zero, negative or
    /// unbounded delay.
    ///
    /// A zero delay would spin the retry loop, so the floor is one millisecond. A factor that is
    /// not a finite number of at least one falls back to doubling, and jitter that is not finite
    /// counts as full, so a hostile or mistyped policy still paces retries.
    #[test]
    fn jitter_and_hostile_policies_stay_between_the_floor_and_the_ceiling() {
        struct Case {
            name: &'static str,
            policy: BackoffPolicy,
            earlier_attempts: u32,
            jitter: f64,
            want: Duration,
        }
        let cases = [
            Case {
                name: "half jitter on the first attempt halves 250 ms",
                policy: BackoffPolicy::default(),
                earlier_attempts: 0,
                jitter: 0.5,
                want: Duration::from_millis(125),
            },
            Case {
                name: "zero jitter holds the one millisecond floor",
                policy: BackoffPolicy::default(),
                earlier_attempts: 2,
                jitter: 0.0,
                want: Duration::from_millis(1),
            },
            Case {
                name: "negative jitter clamps to zero and holds the floor",
                policy: BackoffPolicy::default(),
                earlier_attempts: 2,
                jitter: -3.0,
                want: Duration::from_millis(1),
            },
            Case {
                name: "infinite jitter counts as full, 250 ms * 2^2 = 1 s",
                policy: BackoffPolicy::default(),
                earlier_attempts: 2,
                jitter: f64::INFINITY,
                want: Duration::from_secs(1),
            },
            Case {
                name: "a NaN factor falls back to doubling, 250 ms * 2^2 = 1 s",
                policy: BackoffPolicy {
                    factor: f64::NAN,
                    ..BackoffPolicy::default()
                },
                earlier_attempts: 2,
                jitter: 1.0,
                want: Duration::from_secs(1),
            },
            Case {
                name: "a zero base holds the floor rather than spinning",
                policy: BackoffPolicy {
                    base: Duration::ZERO,
                    ..BackoffPolicy::default()
                },
                earlier_attempts: 0,
                jitter: 1.0,
                want: Duration::from_millis(1),
            },
        ];
        for case in cases {
            let mut backoff = Backoff::new(case.policy);
            for _ in 0..case.earlier_attempts {
                let _ = backoff.next_delay(1.0);
            }
            assert_eq!(
                backoff.next_delay(case.jitter),
                case.want,
                "{}: the delay must stay between the floor and the ceiling",
                case.name
            );
        }
    }
}
