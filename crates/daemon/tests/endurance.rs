//! A long run with faults, holding the daemon to zero unrecovered disconnections (SC-007).
//!
//! Ignored by default because it runs for as long as it is told to:
//!
//! ```text
//! HARBOR_SOAK_SECS=86400 cargo test --release -p midi-harbor-daemon --test endurance -- --ignored --nocapture
//! ```
//!
//! Two daemons in one process are joined by a loopback session, with a keyboard on one routed
//! through the session to a synth on the other, and MIDI flowing throughout. Faults are injected
//! in turn: the far session disappears and returns, the keyboard is unplugged and replugged, and
//! the synth port is switched off and on. After each one, a message has to make it all the way
//! across within SC-003's ten seconds, or the run fails.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::{Direction, InvitationPolicy};
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::{FakeMidiPlatform, Outgoing};
use midi_harbor_platform::midi::{DiscoveredDevice, MidiPlatform};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// SC-003: how long a recovery may take.
const RECOVERY_BUDGET: Duration = Duration::from_secs(10);

/// How long a fault lasts before it is undone.
const OUTAGE: Duration = Duration::from_secs(2);

/// How long the run lasts when `HARBOR_SOAK_SECS` does not say.
const DEFAULT_SOAK: Duration = Duration::from_secs(120);

/// Starts a daemon over a scratch directory, with sessions kept off the network.
async fn machine(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root =
        common::scratch("midi-harbor-endurance").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        common::quiet(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
}

/// Returns the hardware keyboard that is unplugged and plugged back in.
fn keyboard() -> DiscoveredDevice {
    DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            name: "Soak Keys".to_owned(),
            unique_id: Some(0x50AC),
            ..DeviceFingerprint::default()
        },
        direction: Direction::Bidirectional,
        claimed_by: None,
        software: false,
    }
}

/// The two machines and what joins them.
struct Rig {
    near: Arc<Daemon>,
    near_platform: Arc<FakeMidiPlatform>,
    far: Arc<Daemon>,
    far_platform: Arc<FakeMidiPlatform>,
    far_session: midi_harbor_core::ids::EndpointId,
    near_session: midi_harbor_core::ids::EndpointId,
    synth: midi_harbor_core::ids::EndpointId,
    /// Set while a probe is out, so the background traffic does not land after it and hide it.
    probing: Arc<AtomicBool>,
}

impl Rig {
    /// Describes both machines as they stand, so a fault that was not recovered can be explained
    /// from the run that hit it: the soak runs with logging off, and a day is too long to
    /// reproduce on demand.
    async fn diagnose(&self) -> String {
        let mut report = String::new();
        for (side, daemon, session) in [
            ("near", &self.near, self.near_session),
            ("far", &self.far, self.far_session),
        ] {
            let status = daemon.session_status(session).await.map(|status| {
                format!(
                    "{:?} attempt {} peer {:?}",
                    status.state.phase(),
                    status.state.attempt(),
                    status.peer_address
                )
            });
            report.push_str(&format!("{side} session: {status:?}\n"));
            for route in daemon.router().await.routes() {
                report.push_str(&format!(
                    "{side} route {} -> {}: {:?}, enabled {}\n",
                    route.from, route.to, route.validity, route.enabled
                ));
            }
            let history = daemon.events(None, usize::MAX).await;
            for event in history.iter().rev().take(30).rev() {
                report.push_str(&format!(
                    "{side} event {:?} {:?}: {}\n",
                    event.at, event.kind, event.detail
                ));
            }
        }
        report.push_str(&format!(
            "keyboard handle: {:?}\n",
            self.near_platform.device_handle("Soak Keys")
        ));
        report
    }

    /// Sends a probe from the keyboard and waits for it at the synth, returning how long that
    /// took, or nothing if it never arrived within the budget.
    async fn probe(&self, value: u8) -> Option<Duration> {
        let message = MidiMessage::ControlChange {
            channel: Channel::new(15).expect("channel 15 is a valid MIDI channel"),
            controller: 119,
            value,
        };
        self.probing.store(true, Ordering::Relaxed);
        let arrived = self.await_probe(message).await;
        self.probing.store(false, Ordering::Relaxed);
        arrived
    }

    /// Feeds `message` until it arrives at the synth or `RECOVERY_BUDGET` runs out, returning how
    /// long it took.
    async fn await_probe(&self, message: MidiMessage) -> Option<Duration> {
        let started = Instant::now();
        while started.elapsed() < RECOVERY_BUDGET {
            // Resent until it arrives, because a message sent mid-outage is lost by design.
            if let Some(keys) = self.near_platform.device_handle("Soak Keys") {
                let _ = self.near_platform.feed(keys, &[message]);
            }
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let arrived = self
                    .far_platform
                    .port_handle("Soak Synth")
                    .and_then(|synth| self.far_platform.last_sent(synth));
                if arrived == Some(Outgoing::Message(message)) {
                    return Some(started.elapsed());
                }
            }
        }
        None
    }
}

/// Proves SC-007: across a long run with faults, every disconnection is recovered, each within
/// SC-003's ten seconds.
///
/// Three faults take turns, each lasting two seconds before it is undone: the far session is
/// switched off and on (the near session has to notice and reconnect by itself), the keyboard is
/// unplugged and replugged, and the synth port is switched off and on. After each, a probe must
/// cross the whole path from keyboard to synth within the budget while background notes play
/// throughout, or the run fails with a description of both machines.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "runs for as long as HARBOR_SOAK_SECS says"]
async fn a_long_run_with_faults_never_stays_down() {
    let soak = std::env::var("HARBOR_SOAK_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .map_or(DEFAULT_SOAK, Duration::from_secs);

    // Set up the far side: a session accepting the near one, feeding a synth.
    let (far, far_platform) = machine("far").await;
    let synth = far
        .create_virtual_port("Soak Synth", 1, 1)
        .await
        .expect("the Soak Synth port is created");
    let far_session = far
        .create_network_session("Soak In", 0, InvitationPolicy::AcceptAll)
        .await
        .expect("the far session is created");
    far.create_route("Soak In", "Soak Synth")
        .await
        .expect("the route from Soak In to Soak Synth is created");
    let port = far
        .session_status(far_session.id)
        .await
        .expect("the far session reports its status")
        .control_port;

    // Set up the near side: a keyboard routed into a session connected to the far one.
    let (near, near_platform) = machine("near").await;
    let near_session = near
        .create_network_session("Soak Out", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near session is created");
    near_platform.attach(keyboard());
    near.refresh_devices().await;
    near.create_route("Soak Keys", "Soak Out")
        .await
        .expect("the route from Soak Keys to Soak Out is created");
    near.connect_peer(near_session.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("the near session is pointed at the far one");

    let rig = Rig {
        near: Arc::clone(&near),
        near_platform: Arc::clone(&near_platform),
        far: Arc::clone(&far),
        far_platform,
        far_session: far_session.id,
        near_session: near_session.id,
        synth: synth.id,
        probing: Arc::new(AtomicBool::new(false)),
    };
    assert!(
        rig.probe(0).await.is_some(),
        "the rig never carried MIDI before any fault was injected"
    );

    // Play background traffic, as a player would, for the whole run.
    let playing = Arc::new(AtomicBool::new(true));
    let played = Arc::new(AtomicU64::new(0));
    let traffic = {
        let playing = Arc::clone(&playing);
        let played = Arc::clone(&played);
        let probing = Arc::clone(&rig.probing);
        let platform = Arc::clone(&near_platform);
        tokio::spawn(async move {
            let channel = Channel::new(0).expect("channel 0 is a valid MIDI channel");
            let mut note = 36u8;
            while playing.load(Ordering::Relaxed) {
                if !probing.load(Ordering::Relaxed)
                    && let Some(keys) = platform.device_handle("Soak Keys")
                {
                    let on = MidiMessage::NoteOn {
                        channel,
                        note,
                        velocity: 80,
                    };
                    let off = MidiMessage::NoteOff {
                        channel,
                        note,
                        velocity: 0,
                    };
                    if platform.feed(keys, &[on, off]) {
                        played.fetch_add(2, Ordering::Relaxed);
                    }
                }
                note = if note >= 84 { 36 } else { note + 1 };
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };

    // Inject faults, in turn, until the time is up.
    let started = Instant::now();
    let (mut cycles, mut slowest) = (0u64, Duration::ZERO);
    let mut value = 1u8;
    while started.elapsed() < soak {
        let fault = cycles % 3;
        match fault {
            // The far machine's session disappears and returns; the near one has to notice and
            // reconnect by itself.
            0 => {
                rig.far
                    .set_enabled(rig.far_session, false)
                    .await
                    .expect("the far session is switched off");
                tokio::time::sleep(OUTAGE).await;
                rig.far
                    .set_enabled(rig.far_session, true)
                    .await
                    .expect("the far session is switched back on");
            }
            // The keyboard is unplugged and plugged back in.
            1 => {
                rig.near_platform.detach("Soak Keys");
                rig.near.refresh_devices().await;
                tokio::time::sleep(OUTAGE).await;
                rig.near_platform.attach(keyboard());
                rig.near.refresh_devices().await;
            }
            // The synth port is switched off and on.
            _ => {
                rig.far
                    .set_enabled(rig.synth, false)
                    .await
                    .expect("the synth port is switched off");
                tokio::time::sleep(OUTAGE).await;
                rig.far
                    .set_enabled(rig.synth, true)
                    .await
                    .expect("the synth port is switched back on");
            }
        }

        value = if value >= 127 { 1 } else { value + 1 };
        let Some(took) = rig.probe(value).await else {
            panic!(
                "cycle {cycles} (fault {fault}) was not recovered within {RECOVERY_BUDGET:?}, \
                 {:?} into the run\n{}",
                started.elapsed(),
                rig.diagnose().await
            );
        };
        slowest = slowest.max(took);
        cycles += 1;
        if cycles % 50 == 0 {
            println!(
                "{:?}: {cycles} faults recovered, slowest {slowest:?}, {} messages played",
                started.elapsed(),
                played.load(Ordering::Relaxed)
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    playing.store(false, Ordering::Relaxed);
    let _ = traffic.await;
    println!(
        "done: {cycles} faults over {:?}, every one recovered, slowest {slowest:?}, {} messages played",
        started.elapsed(),
        played.load(Ordering::Relaxed)
    );
}
