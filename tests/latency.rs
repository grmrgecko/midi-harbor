//! How long a message takes to cross the daemon between two virtual ports (SC-008).
//!
//! Ignored by default because it needs the machine's real MIDI system. Run it with:
//!
//! ```text
//! cargo test --release --test latency -- --ignored --nocapture
//! ```
//!
//! A daemon runs in this process on the real backend, with two ports and a route between them. A
//! second backend instance stands in for another application: it sends into one port and listens
//! on the other, the way a DAW and a synth would, and times each message across.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::paths::Paths;
use midi_harbor_core::rtchannel::{self, Drained};
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::midi::{DiscoveredDevice, MidiPlatform};
use std::time::{Duration, Instant};

/// Messages timed, after the warm-up.
const SAMPLES: usize = 2_000;

/// Messages sent and discarded first, so opening costs are not counted.
const WARM_UP: usize = 50;

/// SC-008: the mean a message may take across a virtual port.
const MEAN_BUDGET: Duration = Duration::from_millis(1);

/// SC-008: the 99th percentile a message may take across a virtual port.
const P99_BUDGET: Duration = Duration::from_millis(3);

/// How long to wait for one message before calling it lost.
const GIVE_UP: Duration = Duration::from_millis(250);

/// Locks SC-008: a message crosses the daemon between two virtual ports in under 1 ms on
/// average and under 3 ms at the 99th percentile, on the machine's real MIDI system.
///
/// Measured end to end as another application sees it, so the platform's own delivery counts
/// against the budget as it does for a user.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the machine's real MIDI system"]
async fn a_message_crosses_a_virtual_port_within_budget() {
    // The daemon, on the real backend, in a directory of its own.
    let root = std::env::temp_dir().join(format!("mh-latency-{}", std::process::id()));
    let daemon = Daemon::start(
        Paths::rooted_at(&root),
        midi_harbor_platform::midi_backend().expect("the real backend"),
    )
    .await
    .expect("a daemon");
    daemon
        .create_virtual_port("MH Latency In", 1, 1)
        .await
        .expect("a port");
    daemon
        .create_virtual_port("MH Latency Out", 1, 1)
        .await
        .expect("a port");
    daemon
        .create_route("MH Latency In", "MH Latency Out")
        .await
        .expect("a route");

    // Another application, as far as the daemon can tell.
    let app = midi_harbor_platform::midi_backend().expect("a second client");
    let into = app
        .open_device(&appeared(app.as_ref(), "MH Latency In").fingerprint)
        .expect("to send into the port");
    let (heard, mut listening) = rtchannel::channel(EndpointId::new());
    app.open_device_with_sink(
        &appeared(app.as_ref(), "MH Latency Out").fingerprint,
        Some(heard),
    )
    .expect("to listen on the port");

    let timings = tokio::task::spawn_blocking(move || measure(app.as_ref(), into, &mut listening))
        .await
        .expect("the measurement");

    let _ = std::fs::remove_dir_all(&root);
    report_and_check(timings);
}

/// Waits for a port the daemon made to appear to another application, as a DAW would find it.
fn appeared(app: &dyn MidiPlatform, name: &str) -> DiscoveredDevice {
    for _ in 0..50 {
        if let Some(found) = app
            .list_devices()
            .unwrap()
            .into_iter()
            .find(|device| device.fingerprint.name == name)
        {
            return found;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{name} never appeared to another application");
}

/// Returns the timing a fraction of the way through timings sorted from fastest to slowest.
fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    let at = ((sorted.len() as f64 * fraction) as usize).min(sorted.len() - 1);
    sorted[at]
}

/// Sends messages one at a time and times each until it is heard on the far side.
fn measure(
    app: &dyn MidiPlatform,
    into: midi_harbor_platform::midi::PortHandle,
    listening: &mut rtchannel::RtConsumer,
) -> Vec<Duration> {
    let channel = Channel::new(0).unwrap();
    let mut timings = Vec::with_capacity(SAMPLES);
    // A small generator for the gap between messages. A fixed gap would land at the same point
    // in the daemon's own cycle every time, and measure that point rather than the average.
    let mut seed: u32 = 0x9E37_79B9;

    for index in 0..WARM_UP + SAMPLES {
        let note = u8::try_from(index % 128).unwrap();
        let message = MidiMessage::NoteOn {
            channel,
            note,
            velocity: 64,
        };
        let sent = Instant::now();
        app.send(into, &[message]).expect("sent");

        let heard = loop {
            let found = listening.drain(64).into_iter().any(
                |drained| matches!(drained, Drained::Message { message: m, .. } if m == message),
            );
            if found {
                break Some(sent.elapsed());
            }
            if sent.elapsed() > GIVE_UP {
                break None;
            }
            std::hint::spin_loop();
        };
        let taken = heard.unwrap_or_else(|| panic!("message {index} was never heard"));
        if index >= WARM_UP {
            timings.push(taken);
        }

        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        std::thread::sleep(Duration::from_micros(1_000 + u64::from(seed % 4_000)));
    }
    timings
}

/// Prints the distribution and holds it to SC-008.
fn report_and_check(mut timings: Vec<Duration>) {
    timings.sort();
    let total: Duration = timings.iter().sum();
    let mean = total / u32::try_from(timings.len()).unwrap();
    let (p50, p99, max) = (
        percentile(&timings, 0.50),
        percentile(&timings, 0.99),
        percentile(&timings, 1.0),
    );
    println!(
        "{} messages: mean {mean:?}, p50 {p50:?}, p99 {p99:?}, max {max:?}",
        timings.len()
    );
    assert!(
        mean < MEAN_BUDGET,
        "mean {mean:?} is over SC-008's {MEAN_BUDGET:?}"
    );
    assert!(
        p99 < P99_BUDGET,
        "p99 {p99:?} is over SC-008's {P99_BUDGET:?}"
    );
}

/// Locks SC-009: half the 99th-percentile round trip over a network session to another machine
/// stays under 5 ms.
///
/// Ignored, and needs a machine to echo. On the far machine, run a daemon with a session that
/// accepts invitations routed into a port, that port routed back into the session, and the
/// port's output connected to its own input (on Linux, `aconnect` the port to itself). Then:
///
/// ```text
/// HARBOR_ECHO=192.0.2.13:5104 cargo test --release --test latency round_trip -- --ignored --nocapture
/// ```
///
/// Half the round trip bounds the one-way time without needing the two clocks to agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a machine to echo; set HARBOR_ECHO"]
async fn a_round_trip_over_a_network_session() {
    let Ok(echo) = std::env::var("HARBOR_ECHO") else {
        panic!("set HARBOR_ECHO to the echoing machine's session, such as 192.0.2.13:5104");
    };
    let echo: std::net::SocketAddr = echo.parse().expect("HARBOR_ECHO as host:port");

    let root = std::env::temp_dir().join(format!("mh-echo-{}", std::process::id()));
    let daemon = Daemon::start(
        Paths::rooted_at(&root),
        midi_harbor_platform::midi_backend().expect("the real backend"),
    )
    .await
    .expect("a daemon");
    for name in ["MH Echo Out", "MH Echo Back"] {
        daemon
            .create_virtual_port(name, 1, 1)
            .await
            .expect("a port");
    }
    let session = daemon
        .create_network_session(
            "MH Echo",
            0,
            midi_harbor_core::endpoint::InvitationPolicy::Prompt,
        )
        .await
        .expect("a session");
    daemon
        .create_route("MH Echo Out", "MH Echo")
        .await
        .expect("out");
    daemon
        .create_route("MH Echo", "MH Echo Back")
        .await
        .expect("back");
    daemon
        .connect_peer(session.id, echo)
        .await
        .expect("connect");
    let mut connected = false;
    for _ in 0..100 {
        let phase = daemon
            .session_status(session.id)
            .await
            .map(|status| status.state.phase());
        if phase == Some(midi_harbor_core::state::ConnectionPhase::Connected) {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        connected,
        "the session must connect to the echoing machine at {echo}"
    );

    let app = midi_harbor_platform::midi_backend().expect("a second client");
    let into = app
        .open_device(&appeared(app.as_ref(), "MH Echo Out").fingerprint)
        .expect("to send");
    let (heard, mut listening) = rtchannel::channel(EndpointId::new());
    app.open_device_with_sink(
        &appeared(app.as_ref(), "MH Echo Back").fingerprint,
        Some(heard),
    )
    .expect("to listen");

    let round_trips =
        tokio::task::spawn_blocking(move || measure(app.as_ref(), into, &mut listening))
            .await
            .expect("the measurement");
    let _ = std::fs::remove_dir_all(&root);

    let mut sorted = round_trips;
    sorted.sort();
    let (p50, p99, max) = (
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.99),
        percentile(&sorted, 1.0),
    );
    println!(
        "{} round trips to {echo}: p50 {p50:?}, p99 {p99:?}, max {max:?}; one way at most p99 {:?}",
        sorted.len(),
        p99 / 2
    );
    assert!(
        p99 / 2 < Duration::from_millis(5),
        "half the p99 round trip, {:?}, is over SC-009's 5 ms",
        p99 / 2
    );
}
