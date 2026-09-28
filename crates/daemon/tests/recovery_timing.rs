//! The two numbers that define self-healing.
//!
//! SC-003 and SC-004 are the product's central claim: MIDI resumes within ten seconds of an
//! interruption ending, and within fifteen of a wake. Everything else in this system is in
//! service of those.
//!
//! These are simulations over the real constants and the real backoff, not over a real network.
//! What they guard is the part of recovery that is scheduling (when the next attempt happens),
//! because that is the part a change to a constant can quietly blow, and the part that was
//! blowing it. Measured end-to-end figures live in research R-034.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use midi_harbor_core::backoff::{Backoff, BackoffPolicy};
use midi_harbor_daemon::session::TICK_INTERVAL;
use midi_harbor_daemon::state::SYSTEM_EVENT_INTERVAL;
use midi_harbor_platform::sysevents::SAMPLE_INTERVAL;
use midi_harbor_rtpmidi::clock::LIVENESS_TIMEOUT;
use midi_harbor_rtpmidi::session::{INVITE_TIMEOUT, PROBE_TIMEOUT};
use std::time::Duration;

/// SC-003: MIDI resumes within ten seconds of an interruption ending.
const RESUME_BUDGET: Duration = Duration::from_secs(10);

/// SC-004: sessions resume within fifteen seconds of the machine waking.
const WAKE_BUDGET: Duration = Duration::from_secs(15);

/// How long an invitation takes to be answered, at worst.
///
/// One lost invitation is assumed rather than the happy path, because a budget that only holds
/// when nothing goes wrong is not a budget.
const HANDSHAKE: Duration = INVITE_TIMEOUT;

/// Interruptions to induce, per SC-003's "at least 99 of 100".
const TRIALS: usize = 100;

/// How long each simulated interruption lasts.
///
/// Longer than the liveness timeout on purpose: a shorter outage never reaches the retry loop at
/// all, because the session does not notice it and the MIDI simply resumes. The interesting case
/// is the one where the session gave up and is waiting.
const OUTAGE: Duration = Duration::from_secs(300);

/// Returns when each retry would be attempted through one simulated outage.
///
/// Uses the real backoff with real jitter, so the schedule is the one a session would follow.
fn retry_schedule(policy: BackoffPolicy) -> Vec<Duration> {
    let mut backoff = Backoff::new(policy);
    let mut attempts = Vec::new();
    let mut at = Duration::ZERO;

    while at < OUTAGE {
        at = at.saturating_add(backoff.next_delay_random());
        attempts.push(at);
    }
    attempts
}

/// Proves SC-003: at least 99 of 100 interruptions are recovered within ten seconds of ending.
///
/// Nothing reports that an interruption ended when this machine's own address never changed (an
/// upstream router rebooting looks exactly like silence), so the only thing that finds out is the
/// next scheduled retry. The worst case is the responsive retry ceiling of 5 s, plus a lost
/// invitation's 2 s `INVITE_TIMEOUT`, plus one 250 ms session tick: 7.25 s, inside the 10 s budget.
/// The default ceiling of 30 s plus the same 2 s handshake is 32 s, which is why sessions carry a
/// policy of their own; if the ceiling is ever raised back towards the default, the ceiling
/// assertion says why before the simulation does.
#[test]
fn an_interruption_that_ends_is_recovered_within_ten_seconds() {
    // Bound the worst case from the ceiling.
    let ceiling = BackoffPolicy::responsive().max;
    let worst = ceiling
        .saturating_add(HANDSHAKE)
        .saturating_add(TICK_INTERVAL);
    assert!(
        worst <= RESUME_BUDGET,
        "a retry ceiling of {ceiling:?} leaves {worst:?} in the worst case, over the {RESUME_BUDGET:?} budget"
    );
    assert!(
        BackoffPolicy::default().max.saturating_add(HANDSHAKE) > RESUME_BUDGET,
        "the default ceiling is supposed to be the one that cannot meet this, or sessions need no policy of their own"
    );

    // Simulate interruptions over the real backoff with real jitter.
    let mut breaches = Vec::new();
    for trial in 0..TRIALS {
        let schedule = retry_schedule(BackoffPolicy::responsive());

        // The network returns at a point of its own choosing, which lands inside whichever retry
        // interval happens to cover it.
        let returns_at = OUTAGE.mul_f64(rand::random::<f64>());
        let next_attempt = schedule
            .iter()
            .find(|at| **at >= returns_at)
            .copied()
            .expect("the session never stops retrying, so an attempt follows the network's return");

        let recovery = (next_attempt - returns_at)
            .saturating_add(HANDSHAKE)
            .saturating_add(TICK_INTERVAL);
        if recovery > RESUME_BUDGET {
            breaches.push((trial, recovery));
        }
    }
    assert!(
        breaches.len() <= 1,
        "{} of {TRIALS} interruptions took longer than {RESUME_BUDGET:?} to recover: {breaches:?}",
        breaches.len()
    );
}

/// Proves SC-004: a session suspended with the machine is recovered within fifteen seconds of the
/// wake.
///
/// After a suspend the session is still Connected, because nothing ran while the machine was
/// away. Recovery is: notice the machine came back (the 1 s clock `SAMPLE_INTERVAL` plus the 1 s
/// `SYSTEM_EVENT_INTERVAL` watcher pass), ask the peer whether the link survived and give up on
/// the answer (2 s `PROBE_TIMEOUT`), wait for the next 250 ms tick, then reconnect through a lost
/// invitation (2 s). That is 6.25 s, inside the 15 s budget. The ordinary 35 s `LIVENESS_TIMEOUT`
/// would miss it, which is the reason the probe exists: every build before the probe waited for
/// it, because a suspended session looks exactly like a working one until it expires.
#[test]
fn a_wake_is_recovered_within_fifteen_seconds() {
    let notice = SAMPLE_INTERVAL.saturating_add(SYSTEM_EVENT_INTERVAL);
    let worst = notice
        .saturating_add(PROBE_TIMEOUT)
        .saturating_add(TICK_INTERVAL)
        .saturating_add(HANDSHAKE);

    assert!(
        worst <= WAKE_BUDGET,
        "waking takes {worst:?} in the worst case, over the {WAKE_BUDGET:?} budget"
    );
    assert!(
        LIVENESS_TIMEOUT > WAKE_BUDGET,
        "if the liveness timeout fits inside the budget, the probe is no longer load-bearing and this reasoning needs revisiting"
    );
}
