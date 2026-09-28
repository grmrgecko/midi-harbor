//! One endpoint failing or retrying while the others carry on (FR-029).
//!
//! A retry held the daemon's lock for as long as the platform took to open the endpoint, and
//! every route in the daemon needs that lock to find its destinations. A device another
//! application held, retried every few seconds, stopped all MIDI for as long as each attempt took.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::Direction;
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::paths::Paths;
use midi_harbor_core::state::ConnectionPhase;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::{FakeMidiPlatform, Outgoing};
use midi_harbor_platform::midi::{DiscoveredDevice, MidiPlatform};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long each attempt at the stuck device takes to answer.
const SLOW_OPEN: Duration = Duration::from_secs(2);

/// How long a message on the healthy route may take while the stuck device is retried.
const HEALTHY_BUDGET: Duration = Duration::from_millis(250);

/// Builds a daemon over a fake platform, rooted in its own temporary directory.
async fn daemon() -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root = common::scratch("midi-harbor-isolation").join(uuid::Uuid::new_v4().to_string());
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        Paths::rooted_at(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
}

/// Proves that while a held device is retried, each attempt taking two seconds to answer, a
/// healthy route keeps delivering every message within 250 ms (FR-029).
///
/// A retry held the daemon's lock for as long as the platform took to open the endpoint, and
/// every route needs that lock to find its destinations. Messages are sent for two attempts'
/// worth of time, four seconds, so the measurement overlaps more than one attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_retry_does_not_stall_other_routes() {
    let (daemon, platform) = daemon().await;

    // A healthy route between two ports.
    for name in ["Keys", "Synth"] {
        daemon
            .create_virtual_port(name, 1, 1)
            .await
            .unwrap_or_else(|error| panic!("the port {name} is created: {error}"));
    }
    daemon
        .create_route("Keys", "Synth")
        .await
        .expect("the route from Keys to Synth is created");

    // A device another application holds, so it fails to open and goes on retrying.
    platform.attach(DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            name: "Held Keys".to_owned(),
            unique_id: Some(0x4E1D),
            ..DeviceFingerprint::default()
        },
        direction: Direction::Bidirectional,
        claimed_by: Some("Another App".to_owned()),
        software: false,
    });
    let mut phase = None;
    for _ in 0..60 {
        phase = daemon
            .read(|config, runtime| {
                config
                    .endpoints
                    .iter()
                    .find(|endpoint| endpoint.name.as_str() == "Held Keys")
                    .and_then(|endpoint| runtime.get(&endpoint.id))
                    .map(|runtime| runtime.state.phase())
            })
            .await;
        if phase.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Claimed is Unavailable rather than Retrying, and the retry loop re-evaluates both.
    assert!(
        matches!(
            phase,
            Some(ConnectionPhase::Retrying | ConnectionPhase::Unavailable)
        ),
        "a held device must be listed and waiting to be retried, got {phase:?}"
    );

    // From now on every attempt at it takes a while to answer.
    platform.delay_opens(SLOW_OPEN);

    let keys = platform.port_handle("Keys").expect("the port Keys is open");
    let synth = platform
        .port_handle("Synth")
        .expect("the port Synth is open");
    let channel = Channel::new(0).expect("channel 0 is a valid channel");
    let mut slowest = Duration::ZERO;
    let started = Instant::now();
    let mut value = 0u8;
    while started.elapsed() < SLOW_OPEN * 2 {
        let message = MidiMessage::ControlChange {
            channel,
            controller: 1,
            value,
        };
        let sent = Instant::now();
        assert!(
            platform.feed(keys, &[message]),
            "the fake accepts MIDI fed into an open port"
        );
        while platform.last_sent(synth) != Some(Outgoing::Message(message)) {
            assert!(
                sent.elapsed() < SLOW_OPEN * 2,
                "message {value} never arrived"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        slowest = slowest.max(sent.elapsed());
        value = (value + 1) % 128;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        slowest < HEALTHY_BUDGET,
        "the healthy route took {slowest:?} while another endpoint was being retried"
    );
}

/// Proves that a device switched off while a retry is opening it is closed again when that
/// retry answers, rather than left open and connected.
///
/// The attempt answers after the user's switch, so what it opened has to be let go. The wait is
/// the two seconds the open takes plus half a second of margin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_switched_off_mid_retry_is_not_left_open() {
    let (daemon, platform) = daemon().await;
    platform.attach(DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            name: "Held Keys".to_owned(),
            ..DeviceFingerprint::default()
        },
        direction: Direction::Bidirectional,
        claimed_by: Some("Another App".to_owned()),
        software: false,
    });
    let mut held = None;
    for _ in 0..60 {
        held = daemon
            .read(|config, runtime| {
                config
                    .endpoints
                    .iter()
                    .find(|endpoint| endpoint.name.as_str() == "Held Keys")
                    .filter(|endpoint| runtime.contains_key(&endpoint.id))
                    .map(|endpoint| endpoint.id)
            })
            .await;
        if held.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let held = held.expect("the device was never listed");

    // The next attempt succeeds, slowly, and the user switches the device off while it is out.
    platform.delay_opens(SLOW_OPEN);
    platform.release("Held Keys");
    // Allowing for however far the backoff has grown while the device was held.
    let mut attempting = false;
    for _ in 0..500 {
        let phase = daemon
            .read(|_, runtime| runtime.get(&held).map(|r| r.state.phase()))
            .await;
        if phase == Some(ConnectionPhase::Connecting) {
            attempting = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(attempting, "no retry began");
    daemon
        .set_enabled(held, false)
        .await
        .expect("the device is switched off mid-retry");

    tokio::time::sleep(SLOW_OPEN + Duration::from_millis(500)).await;
    assert!(
        platform.device_handle("Held Keys").is_none(),
        "a device switched off was left open by a retry that finished afterwards"
    );
    let phase = daemon
        .read(|_, runtime| runtime.get(&held).map(|r| r.state.phase()))
        .await;
    assert_ne!(
        phase,
        Some(ConnectionPhase::Connected),
        "a device switched off must not read as connected once the late retry answers"
    );
}

/// Proves that devices waiting to be retried are opened side by side rather than one after
/// another.
///
/// Retried one after another, three devices slow to answer took three times as long to come back
/// as one did. Side by side, they take one backoff step plus one open: the bound is two opens
/// plus half a second, three seconds and a half, against the four and a half three opens in a
/// row would take.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn devices_are_retried_side_by_side() {
    const OPEN: Duration = Duration::from_millis(1_500);
    let (daemon, platform) = daemon().await;
    let names = ["Held One", "Held Two", "Held Three"];
    for (index, name) in names.iter().enumerate() {
        platform.attach(DiscoveredDevice {
            fingerprint: DeviceFingerprint {
                name: (*name).to_owned(),
                unique_id: Some(0x5100 + u32::try_from(index).expect("three fit in a u32")),
                ..DeviceFingerprint::default()
            },
            direction: Direction::Bidirectional,
            claimed_by: Some("Another App".to_owned()),
            software: false,
        });
    }
    let listed = |daemon: Arc<Daemon>| async move {
        daemon
            .read(|config, runtime| {
                names
                    .iter()
                    .filter(|name| {
                        config
                            .endpoints
                            .iter()
                            .find(|endpoint| endpoint.name.as_str() == **name)
                            .is_some_and(|endpoint| runtime.contains_key(&endpoint.id))
                    })
                    .count()
            })
            .await
    };
    for _ in 0..60 {
        if listed(Arc::clone(&daemon)).await == names.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    platform.delay_opens(OPEN);
    for name in names {
        platform.release(name);
    }
    let released = Instant::now();
    while names
        .iter()
        .any(|name| platform.device_handle(name).is_none())
    {
        assert!(
            released.elapsed() < OPEN * 5,
            "the devices never all came back"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let took = released.elapsed();
    assert!(
        took < OPEN * 2 + Duration::from_millis(500),
        "three devices took {took:?} to come back, against {OPEN:?} for one"
    );
}
