//! The connection lifecycle every endpoint shares.
//!
//! One state machine covers virtual ports, physical devices, network sessions and Bluetooth links.
//! Its defining property is that no input drives an enabled endpoint into a state it cannot leave:
//! a connection the user has not switched off is always either working or on its way back.

use crate::backoff::{Backoff, BackoffPolicy};
use crate::failure::FailureReason;
use crate::time::{self, Clock};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Where a connection currently sits in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPhase {
    /// The user switched this endpoint off. The only phase that stays put on its own.
    Disabled,
    /// Enabled but not yet started.
    Disconnected,
    /// An attempt is in flight.
    Connecting,
    /// Carrying MIDI.
    Connected,
    /// Down for a reason the user cannot act on, waiting out a backoff delay.
    Retrying,
    /// Down for a reason the user can act on. Still retried, and re-evaluated on every attempt.
    Unavailable,
}

impl ConnectionPhase {
    /// Reports whether MIDI can flow in this phase.
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Connected)
    }

    /// Reports whether the machine will leave this phase without further input from the user.
    ///
    /// True for everything except `Disabled`, which is the user's own decision. `Unavailable` is
    /// included deliberately: it is surfaced to the user but never abandoned.
    pub fn recovers_on_its_own(&self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

/// Something that happened to a connection.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The user switched the endpoint on.
    Enable,
    /// The user switched the endpoint off.
    Disable,
    /// An attempt has started.
    Attempting,
    /// The attempt succeeded.
    Established,
    /// The attempt failed.
    Failed(FailureReason),
    /// An established connection went down.
    Lost(FailureReason),
    /// The backoff delay expired.
    RetryDue,
    /// The platform hinted that conditions changed — a wake, or a network interface change.
    ///
    /// Only ever an optimisation. Recovery does not depend on this arriving, because the platform
    /// sources that produce it are unreliable.
    Nudge,
}

/// What the supervisor must do as a result of a transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Open the connection now.
    StartConnect,
    /// Wait this long, then deliver `Event::RetryDue`.
    ScheduleRetry(Duration),
    /// Abandon any in-flight attempt and any pending retry.
    CancelPending,
    /// Silence every note this endpoint left sounding, before anything else.
    SilenceNotes,
    /// Resend controller, program and pitch state, because the peer missed what happened.
    RestoreState,
}

/// How long a connection must stay up before it counts as recovered.
///
/// A link that drops sooner than this is flapping, and resetting the backoff on each brief
/// connection is what let a flapping link reconnect four times a second indefinitely.
pub const STABLE_AFTER: Duration = Duration::from_secs(10);

/// How many brief connections in a row mark a link as unstable.
pub const UNSTABLE_AFTER: u32 = 3;

/// The lifecycle position of one endpoint, with the history needed to explain it.
#[derive(Debug, Clone)]
pub struct ConnectionState {
    phase: ConnectionPhase,
    since: Timestamp,
    last_error: Option<FailureReason>,
    next_retry: Option<Timestamp>,
    backoff: Backoff,
    /// Tracks whether this connection has been up before, so recovery is distinguished from a
    /// first connection. Only a recovery needs state restoration.
    was_connected: bool,
    /// When the current connection was established, while it is up.
    connected_at: Option<Timestamp>,
    /// Connections in a row that dropped before `STABLE_AFTER`.
    flaps: u32,
}

impl ConnectionState {
    /// Creates a state for an endpoint that starts switched off.
    pub fn disabled(now: Timestamp) -> Self {
        Self::with_policy(ConnectionPhase::Disabled, now, BackoffPolicy::default())
    }

    /// Creates a state for an endpoint that starts switched on but not yet connected.
    pub fn enabled(now: Timestamp) -> Self {
        Self::with_policy(ConnectionPhase::Disconnected, now, BackoffPolicy::default())
    }

    /// Creates a state in a given phase with a specific retry policy.
    pub fn with_policy(phase: ConnectionPhase, now: Timestamp, policy: BackoffPolicy) -> Self {
        Self {
            phase,
            since: now,
            last_error: None,
            next_retry: None,
            backoff: Backoff::new(policy),
            was_connected: false,
            connected_at: None,
            flaps: 0,
        }
    }

    /// Returns the current phase.
    pub fn phase(&self) -> ConnectionPhase {
        self.phase
    }

    /// Returns when the connection entered its current phase.
    pub fn since(&self) -> Timestamp {
        self.since
    }

    /// Returns how long the connection has held its current phase.
    pub fn time_in_phase(&self, now: Timestamp) -> Duration {
        time::elapsed(self.since, now)
    }

    /// Returns the most recent failure, which persists after recovery so users can see what
    /// happened while they were not watching.
    pub fn last_error(&self) -> Option<&FailureReason> {
        self.last_error.as_ref()
    }

    /// Returns when the next attempt is due, if one is scheduled.
    pub fn next_retry(&self) -> Option<Timestamp> {
        self.next_retry
    }

    /// Returns how many attempts have been made since the last connection that stayed up.
    pub fn attempt(&self) -> u32 {
        self.backoff.attempt()
    }

    /// Reports whether the link keeps connecting and dropping (the rapid connect/disconnect edge
    /// case).
    ///
    /// Unstable after `UNSTABLE_AFTER` connections in a row each lasted less than
    /// `STABLE_AFTER`, and stable again once one lasts that long. The backoff already keeps such
    /// a link from reconnecting as fast as it drops. This is what tells the user why it keeps
    /// going quiet.
    pub fn is_unstable(&self, now: Timestamp) -> bool {
        let settled = self
            .connected_at
            .is_some_and(|at| time::elapsed(at, now) >= STABLE_AFTER);
        self.flaps >= UNSTABLE_AFTER && !settled
    }

    /// Applies an event and returns what the supervisor must do about it.
    ///
    /// `jitter` is a fraction in `[0.0, 1.0]` used to spread retries out; production passes a
    /// random value and tests pass a fixed one.
    pub fn apply(&mut self, event: Event, clock: &dyn Clock, jitter: f64) -> Vec<Effect> {
        let now = clock.now();
        match event {
            Event::Disable => self.on_disable(now),
            Event::Enable => self.on_enable(now),
            Event::Attempting => self.on_attempting(now),
            Event::Established => self.on_established(now),
            Event::Failed(reason) => self.on_down(now, reason, false, jitter),
            Event::Lost(reason) => self.on_down(now, reason, true, jitter),
            Event::RetryDue | Event::Nudge => self.on_retry(now, matches!(event, Event::Nudge)),
        }
    }

    /// Applies an event with random jitter, for production callers.
    pub fn apply_now(&mut self, event: Event, clock: &dyn Clock) -> Vec<Effect> {
        self.apply(event, clock, rand::random::<f64>())
    }

    /// Moves to a new phase, recording when, and reports whether the phase actually changed.
    fn enter(&mut self, phase: ConnectionPhase, now: Timestamp) -> bool {
        if self.phase == phase {
            return false;
        }
        self.phase = phase;
        self.since = now;
        true
    }

    fn on_disable(&mut self, now: Timestamp) -> Vec<Effect> {
        if self.phase == ConnectionPhase::Disabled {
            return Vec::new();
        }
        // Silence before tearing down, so nothing is left sounding on the far side.
        let mut effects = Vec::new();
        if self.phase == ConnectionPhase::Connected {
            effects.push(Effect::SilenceNotes);
        }
        effects.push(Effect::CancelPending);
        self.next_retry = None;
        self.backoff.reset();
        self.connected_at = None;
        self.flaps = 0;
        self.enter(ConnectionPhase::Disabled, now);
        effects
    }

    fn on_enable(&mut self, now: Timestamp) -> Vec<Effect> {
        if self.phase != ConnectionPhase::Disabled {
            return Vec::new();
        }
        self.backoff.reset();
        self.last_error = None;
        self.enter(ConnectionPhase::Disconnected, now);
        vec![Effect::StartConnect]
    }

    fn on_attempting(&mut self, now: Timestamp) -> Vec<Effect> {
        if self.phase == ConnectionPhase::Disabled || self.phase == ConnectionPhase::Connected {
            return Vec::new();
        }
        self.next_retry = None;
        self.enter(ConnectionPhase::Connecting, now);
        Vec::new()
    }

    fn on_established(&mut self, now: Timestamp) -> Vec<Effect> {
        if self.phase == ConnectionPhase::Disabled {
            return Vec::new();
        }
        // Only a reconnection needs state restoration; a first connection has nothing to restore.
        let recovering = self.was_connected;
        // The backoff is not reset here. A connection earns that by staying up, which is
        // decided when it drops.
        self.next_retry = None;
        self.was_connected = true;
        self.connected_at = Some(now);
        self.enter(ConnectionPhase::Connected, now);
        if recovering {
            vec![Effect::RestoreState]
        } else {
            Vec::new()
        }
    }

    fn on_down(
        &mut self,
        now: Timestamp,
        reason: FailureReason,
        was_up: bool,
        jitter: f64,
    ) -> Vec<Effect> {
        if self.phase == ConnectionPhase::Disabled {
            return Vec::new();
        }
        let mut effects = Vec::new();
        // Losing an established link can leave notes held on the far side.
        if was_up && self.phase == ConnectionPhase::Connected {
            effects.push(Effect::SilenceNotes);
        }

        // A connection that stayed up has recovered, and the next outage starts from the short
        // delay. One that dropped quickly carries its backoff forward, so a flapping link slows
        // down instead of reconnecting as fast as it fails.
        if let Some(at) = self.connected_at.take() {
            if time::elapsed(at, now) >= STABLE_AFTER {
                self.backoff.reset();
                self.flaps = 0;
            } else {
                self.flaps = self.flaps.saturating_add(1);
            }
        }

        // A reason the user must act on is surfaced; one they cannot act on retries quietly.
        // Both keep retrying, because the user may clear the condition at any moment.
        let phase = if reason.needs_user_action() {
            ConnectionPhase::Unavailable
        } else {
            ConnectionPhase::Retrying
        };
        self.last_error = Some(reason);
        self.enter(phase, now);

        let delay = self.backoff.next_delay(jitter);
        self.next_retry = Some(time::saturating_add(now, delay));
        effects.push(Effect::ScheduleRetry(delay));
        effects
    }

    fn on_retry(&mut self, now: Timestamp, nudged: bool) -> Vec<Effect> {
        match self.phase {
            // Unavailable is included on purpose: it is re-evaluated, never abandoned.
            ConnectionPhase::Retrying
            | ConnectionPhase::Unavailable
            | ConnectionPhase::Disconnected => {}
            _ => return Vec::new(),
        }
        // A platform hint means conditions genuinely changed, so start again from the short delay
        // rather than making the user wait out a backoff that is no longer relevant.
        if nudged {
            self.backoff.reset();
        }
        self.next_retry = None;
        self.enter(ConnectionPhase::Connecting, now);
        vec![Effect::StartConnect]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::TestClock;

    /// Drops a connection that has been up for `lasted` and returns the delay chosen for the
    /// retry, then brings it back up either by waiting that delay or, when `nudged`, at once.
    fn flap(
        state: &mut ConnectionState,
        clock: &TestClock,
        lasted: Duration,
        nudged: bool,
    ) -> Duration {
        clock.advance(lasted);
        let _ = state.apply(Event::Lost(FailureReason::PeerTimeout), clock, 1.0);
        let delay = time::elapsed(
            clock.now(),
            state.next_retry().expect("a retry is scheduled"),
        );
        if nudged {
            let _ = state.apply(Event::Nudge, clock, 1.0);
        } else {
            clock.advance(delay);
            let _ = state.apply(Event::RetryDue, clock, 1.0);
        }
        let _ = state.apply(Event::Established, clock, 1.0);
        delay
    }

    /// Each event moves the connection to the phase it means and asks the supervisor for exactly
    /// the effects it needs, in order.
    ///
    /// Only a reconnection restores controller state; a first connection has nothing to restore.
    /// Losing an established link silences its notes before anything else, so nothing is left
    /// sounding on the far side. A failure the user must act on is surfaced as unavailable but
    /// still retried (Principle I), since they may fix it at any moment. Switching off is the
    /// user's decision and is final until they switch on again. The first retry delay with full
    /// jitter is the 250 ms base.
    #[test]
    fn each_event_asks_for_exactly_the_effects_it_needs() {
        let first_retry = Effect::ScheduleRetry(Duration::from_millis(250));
        let connect = || vec![Event::Attempting, Event::Established];
        let lost = || Event::Lost(FailureReason::PeerTimeout);
        let refused = || Event::Failed(FailureReason::AdapterUnavailable);
        let ignored = |event: Event| {
            (
                "a disabled endpoint ignores everything but being switched on",
                false,
                vec![],
                event,
                vec![],
                ConnectionPhase::Disabled,
            )
        };
        let cases = [
            (
                "a first connection restores nothing",
                true,
                vec![Event::Attempting],
                Event::Established,
                vec![],
                ConnectionPhase::Connected,
            ),
            (
                "a reconnection restores controller state",
                true,
                [connect(), vec![lost(), Event::RetryDue]].concat(),
                Event::Established,
                vec![Effect::RestoreState],
                ConnectionPhase::Connected,
            ),
            (
                "losing a link silences it before scheduling a retry",
                true,
                connect(),
                lost(),
                vec![Effect::SilenceNotes, first_retry.clone()],
                ConnectionPhase::Retrying,
            ),
            (
                "a failure the user must act on is surfaced and still retried",
                true,
                vec![Event::Attempting],
                refused(),
                vec![first_retry],
                ConnectionPhase::Unavailable,
            ),
            (
                "an unavailable endpoint is tried again when its retry falls due",
                true,
                vec![Event::Attempting, refused()],
                Event::RetryDue,
                vec![Effect::StartConnect],
                ConnectionPhase::Connecting,
            ),
            (
                "switching off a connected endpoint silences it and cancels what is pending",
                true,
                connect(),
                Event::Disable,
                vec![Effect::SilenceNotes, Effect::CancelPending],
                ConnectionPhase::Disabled,
            ),
            ignored(Event::Attempting),
            ignored(Event::Established),
            ignored(refused()),
            ignored(lost()),
            ignored(Event::RetryDue),
            ignored(Event::Nudge),
            (
                "switching on a disabled endpoint connects it",
                false,
                vec![],
                Event::Enable,
                vec![Effect::StartConnect],
                ConnectionPhase::Disconnected,
            ),
        ];
        for (case, enabled, before, event, want, phase) in cases {
            let clock = TestClock::at_epoch();
            let mut state = if enabled {
                ConnectionState::enabled(clock.now())
            } else {
                ConnectionState::disabled(clock.now())
            };
            for earlier in before {
                let _ = state.apply(earlier, &clock, 1.0);
            }
            let applied = format!("{event:?}");
            assert_eq!(
                state.apply(event, &clock, 1.0),
                want,
                "{case}: {applied} must ask for exactly these effects"
            );
            assert_eq!(
                state.phase(),
                phase,
                "{case}: {applied} must leave the connection in this phase"
            );
        }
    }

    /// The retry delay resets only after a connection holds for `STABLE_AFTER`, or when the
    /// platform says conditions changed.
    ///
    /// Resetting on every brief connection kept the delay at its shortest forever, so a link that
    /// came up and dropped at once was retried four times a second. Brief connections instead
    /// carry the backoff forward: 250 ms, then 500 ms, then 1 s. One that holds 10 s has
    /// recovered and starts again from 250 ms. A wake or network change (a nudge) starts again
    /// from 250 ms too, because the backoff it would wait out is no longer relevant.
    #[test]
    fn the_backoff_resets_only_when_a_connection_holds_or_conditions_change() {
        let ms = Duration::from_millis;
        let steps = [
            ("a first brief connection drops", ms(1_000), false, ms(250)),
            ("a second brief connection drops", ms(1_000), false, ms(500)),
            (
                "a third brief connection drops",
                ms(1_000),
                false,
                ms(1_000),
            ),
            ("a connection that held drops", STABLE_AFTER, false, ms(250)),
            (
                "a brief connection drops, then a nudge",
                ms(200),
                true,
                ms(500),
            ),
            (
                "a brief connection after the nudge drops",
                ms(200),
                false,
                ms(250),
            ),
        ];
        let clock = TestClock::at_epoch();
        let mut state = ConnectionState::enabled(clock.now());
        let _ = state.apply(Event::Attempting, &clock, 1.0);
        let _ = state.apply(Event::Established, &clock, 1.0);
        for (step, lasted, nudged, want) in steps {
            assert_eq!(
                flap(&mut state, &clock, lasted, nudged),
                want,
                "{step}: the delay must grow across brief connections and reset only when earned"
            );
        }
    }

    /// A link is called unstable after `UNSTABLE_AFTER` brief connections in a row, and stable
    /// again as soon as one holds for `STABLE_AFTER`.
    ///
    /// The backoff already keeps such a link from reconnecting as fast as it drops; this is what
    /// tells the user why it keeps going quiet. A connection that has held is stable even before
    /// it next drops, and a drop after it starts the count again.
    #[test]
    fn a_flapping_link_says_so_until_a_connection_holds() {
        let clock = TestClock::at_epoch();
        let mut state = ConnectionState::enabled(clock.now());
        let _ = state.apply(Event::Attempting, &clock, 1.0);
        let _ = state.apply(Event::Established, &clock, 1.0);
        for flaps in 1..UNSTABLE_AFTER {
            let _ = flap(&mut state, &clock, Duration::from_secs(1), false);
            assert!(
                !state.is_unstable(clock.now()),
                "{flaps} brief connections must not yet call the link unstable"
            );
        }
        let _ = flap(&mut state, &clock, Duration::from_secs(1), false);
        assert!(
            state.is_unstable(clock.now()),
            "{UNSTABLE_AFTER} brief connections in a row must call the link unstable"
        );

        clock.advance(STABLE_AFTER);
        assert!(
            !state.is_unstable(clock.now()),
            "a connection that has held must read as stable before it next drops"
        );
        let _ = flap(&mut state, &clock, Duration::ZERO, false);
        assert!(
            !state.is_unstable(clock.now()),
            "a drop after a connection held must start the count again"
        );
    }

    /// No sequence of events leaves an enabled endpoint stranded: it is connected, on its way
    /// there, or has a retry scheduled.
    ///
    /// Principle I stated as a property. Five hundred rounds cycle attempts, failures of every
    /// kind the user may or may not be able to act on, retries, connections and losses, with the
    /// clock moving 100 ms between each, and after every one the phase must be one the machine
    /// leaves on its own.
    #[test]
    fn no_event_sequence_strands_an_enabled_endpoint() {
        let clock = TestClock::at_epoch();
        let reasons = [
            FailureReason::NetworkUnreachable,
            FailureReason::PeerTimeout,
            FailureReason::PeerRejected,
            FailureReason::DeviceRemoved,
            FailureReason::AdapterUnavailable,
            FailureReason::PermissionDenied {
                what: "bluetooth".to_owned(),
            },
            FailureReason::ResourceLimit,
            FailureReason::ConfigInvalid {
                detail: "bad port".to_owned(),
            },
        ];
        let mut state = ConnectionState::enabled(clock.now());

        for round in 0..500usize {
            let reason = reasons[round % reasons.len()].clone();
            let event = match round % 5 {
                0 => Event::Attempting,
                1 => Event::Failed(reason),
                2 => Event::RetryDue,
                3 => Event::Established,
                _ => Event::Lost(FailureReason::PeerTimeout),
            };
            let _ = state.apply(event, &clock, 0.5);
            clock.advance(Duration::from_millis(100));

            assert!(
                state.phase().recovers_on_its_own(),
                "round {round}: {:?} is a phase an enabled endpoint cannot leave",
                state.phase()
            );
            if matches!(
                state.phase(),
                ConnectionPhase::Retrying | ConnectionPhase::Unavailable
            ) {
                assert!(
                    state.next_retry().is_some(),
                    "round {round}: down in {:?} with no retry scheduled leaves it stranded",
                    state.phase()
                );
            }
        }
    }
}
