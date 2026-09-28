//! How long a message takes to cross a network session (SC-009).
//!
//! Ignored by default because it is a measurement, not a check that belongs in every run:
//!
//! ```text
//! cargo test --release -p midi-harbor-daemon --test network_latency -- --ignored --nocapture
//! ```
//!
//! Two daemons in one process are joined by a real RTP-MIDI session over loopback. A port on one
//! is routed into the session, and the session on the other is routed to a port there. The
//! platform at each end is the in-memory one, so what is timed is the daemon and the session:
//! handing a message to the supervisor, packing it, the datagram, reading it, and routing it on.
//! The network between two machines adds its own time on top.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::InvitationPolicy;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::state::ConnectionPhase;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::{FakeMidiPlatform, Outgoing};
use midi_harbor_platform::midi::{MidiPlatform, PortHandle};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Messages timed, after the warm-up.
const SAMPLES: usize = 2_000;

/// Messages sent and discarded first, while the clock exchange settles.
const WARM_UP: usize = 100;

/// SC-009: the 99th percentile a message may take across a network session.
const P99_BUDGET: Duration = Duration::from_millis(5);

/// Starts a daemon over a scratch directory, with sessions kept off the network.
async fn machine(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root = common::scratch("midi-harbor-network-latency")
        .join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        common::quiet(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
}

/// Returns how long each of `SAMPLES` messages took from `keys` on one platform to `synth` on
/// the other, after `WARM_UP` messages that are not timed.
///
/// Runs on a blocking thread so the wait for each delivery does not hold a runtime worker. The
/// gaps between messages are uneven (1 ms plus up to 4 ms from a xorshift sequence), so the
/// result is not tied to one point in either side's cycle.
async fn time_deliveries(
    from: Arc<FakeMidiPlatform>,
    keys: PortHandle,
    to: Arc<FakeMidiPlatform>,
    synth: PortHandle,
) -> Vec<Duration> {
    tokio::task::spawn_blocking(move || {
        let channel = Channel::new(0).expect("channel 0 is a valid MIDI channel");
        let mut timings = Vec::with_capacity(SAMPLES);
        let mut seed: u32 = 0x2545_F491;
        for index in 0..WARM_UP + SAMPLES {
            let message = MidiMessage::ControlChange {
                channel,
                controller: 20,
                value: u8::try_from(index % 128).expect("a value modulo 128 fits a data byte"),
            };
            let sent = Instant::now();
            assert!(
                from.feed(keys, &[message]),
                "message {index} was refused by the sending port"
            );
            loop {
                if to.last_sent(synth) == Some(Outgoing::Message(message)) {
                    break;
                }
                assert!(
                    sent.elapsed() < Duration::from_secs(1),
                    "message {index} was never delivered"
                );
                std::thread::sleep(Duration::from_micros(20));
            }
            if index >= WARM_UP {
                timings.push(sent.elapsed());
            }
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            std::thread::sleep(Duration::from_micros(1_000 + u64::from(seed % 4_000)));
        }
        timings
    })
    .await
    .expect("the timing thread finishes without panicking")
}

/// Returns the timing at `fraction` of the way through `sorted`, clamped to the last one.
fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    sorted[((sorted.len() as f64 * fraction) as usize).min(sorted.len() - 1)]
}

/// Proves SC-009: the 99th percentile of messages crosses a loopback RTP-MIDI session within
/// 5 ms.
///
/// Times 2,000 messages after 100 of warm-up while the clock exchange settles. Loopback removes
/// the network, so the figure is the daemon's and the session's share; `the_same_route_without_a_session_for_comparison`
/// separates the two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement; run it deliberately"]
async fn a_message_crosses_a_network_session_within_budget() {
    // Set up the far side: a session accepting the near one, feeding a synth.
    let (far, far_platform) = machine("far").await;
    far.create_virtual_port("Synth", 1, 1)
        .await
        .expect("the Synth port is created");
    let incoming = far
        .create_network_session("Stage In", 0, InvitationPolicy::AcceptAll)
        .await
        .expect("the far session is created");
    far.create_route("Stage In", "Synth")
        .await
        .expect("the route from Stage In to Synth is created");
    let port = far
        .session_status(incoming.id)
        .await
        .expect("the far session reports its status")
        .control_port;

    // Set up the near side: a keyboard port routed into a session connected to the far one.
    let (near, near_platform) = machine("near").await;
    near.create_virtual_port("Keys", 1, 1)
        .await
        .expect("the Keys port is created");
    let outgoing = near
        .create_network_session("Stage Out", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near session is created");
    near.create_route("Keys", "Stage Out")
        .await
        .expect("the route from Keys to Stage Out is created");
    near.connect_peer(outgoing.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("the near session is pointed at the far one");
    for _ in 0..100 {
        let phase = near
            .session_status(outgoing.id)
            .await
            .map(|status| status.state.phase());
        if phase == Some(ConnectionPhase::Connected) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Time the messages.
    let keys = near_platform
        .port_handle("Keys")
        .expect("the Keys port has a platform handle");
    let synth = far_platform
        .port_handle("Synth")
        .expect("the Synth port has a platform handle");
    let mut sorted = time_deliveries(near_platform, keys, far_platform, synth).await;
    sorted.sort();

    // Report and judge them.
    let total: Duration = sorted.iter().sum();
    let mean = total / u32::try_from(sorted.len()).expect("the sample count fits a u32");
    let (p50, p99, max) = (
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.99),
        percentile(&sorted, 1.0),
    );
    println!(
        "{} messages over a loopback session: mean {mean:?}, p50 {p50:?}, p99 {p99:?}, max {max:?}",
        sorted.len()
    );
    assert!(
        p99 < P99_BUDGET,
        "the 99th percentile of {p99:?} is over SC-009's {P99_BUDGET:?} budget"
    );
}

/// Times messages from one port to another on a single daemon, with no session in between.
///
/// The control for the measurement above: whatever this shows is the daemon's own share, and the
/// difference is the session's. It prints rather than asserts, since SC-009 budgets the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement; run it deliberately"]
async fn the_same_route_without_a_session_for_comparison() {
    let (daemon, platform) = machine("local").await;
    for name in ["Keys", "Synth"] {
        daemon
            .create_virtual_port(name, 1, 1)
            .await
            .expect("the virtual port is created");
    }
    daemon
        .create_route("Keys", "Synth")
        .await
        .expect("the route from Keys to Synth is created");
    let keys = platform
        .port_handle("Keys")
        .expect("the Keys port has a platform handle");
    let synth = platform
        .port_handle("Synth")
        .expect("the Synth port has a platform handle");

    let mut sorted = time_deliveries(Arc::clone(&platform), keys, platform, synth).await;
    sorted.sort();
    println!(
        "{} messages through one daemon: p50 {:?}, p99 {:?}, max {:?}",
        sorted.len(),
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.99),
        percentile(&sorted, 1.0)
    );
}
