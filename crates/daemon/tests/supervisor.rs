//! Retrying virtual ports and hardware that failed to open.
//!
//! The state machine gave each of these a retry time, and `status` showed it, but nothing acted on
//! it: a port refused once stayed shut, and a device held by another application stayed shut
//! after that application let go.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::Direction;
use midi_harbor_core::failure::FailureReason;
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::paths::Paths;
use midi_harbor_core::state::ConnectionPhase;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::{FakeMidiPlatform, Injected};
use midi_harbor_platform::midi::{DiscoveredDevice, MidiPlatform};
use std::sync::Arc;
use std::time::Duration;

/// Builds a daemon over a fake platform, rooted in its own temporary directory.
async fn daemon(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root =
        common::scratch("midi-harbor-supervisor").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        Paths::rooted_at(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
}

/// Returns the endpoint's phase, its last failure, and whether it holds a platform handle.
async fn state_of(
    daemon: &Arc<Daemon>,
    id: EndpointId,
) -> (ConnectionPhase, Option<FailureReason>, bool) {
    daemon
        .read(|_, runtime| {
            let runtime = runtime
                .get(&id)
                .expect("a listed endpoint has runtime state");
            (
                runtime.state.phase(),
                runtime.state.last_error().cloned(),
                runtime.handle.is_some(),
            )
        })
        .await
}

/// Waits for the endpoint to be connected, giving up after `limit`.
async fn connects_within(daemon: &Arc<Daemon>, id: EndpointId, limit: Duration) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < limit {
        if state_of(daemon, id).await.0 == ConnectionPhase::Connected {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Returns a configured port that the system refused to reopen, so it is waiting to retry.
///
/// Switched off and on rather than created into the failure, because a new port the system will
/// not create is refused outright and never kept.
async fn waiting_port(
    daemon: &Arc<Daemon>,
    platform: &FakeMidiPlatform,
) -> midi_harbor_core::endpoint::Endpoint {
    let port = daemon
        .create_virtual_port("Synth", 1, 1)
        .await
        .expect("the port Synth is created");
    daemon
        .set_enabled(port.id, false)
        .await
        .expect("the port Synth is switched off");
    platform.inject(Some(Injected::ResourceLimit));
    daemon
        .set_enabled(port.id, true)
        .await
        .expect("the port Synth is switched on, though it could not open");
    port
}

/// Proves that a port the system refuses is refused outright when new, but kept and retried
/// when already configured, and opens without help once the system allows it.
///
/// This is the spec's endpoint-limit case. A new port is told the limit was reached rather than
/// shown as a port that is not there: `port create` used to report success, and the port retried
/// forever. A configured port is different, because it holds routes and settings the user made.
/// The first backoff is at most 250 ms and the loop looks every 250 ms, so three seconds is ample.
#[tokio::test]
async fn a_refused_port_is_refused_when_new_and_retried_until_it_opens_when_configured() {
    let (daemon, platform) = daemon("refused").await;

    // A new port under the limit is refused and not kept.
    platform.inject(Some(Injected::ResourceLimit));
    let refused = daemon.create_virtual_port("Bass", 1, 1).await;
    assert!(
        matches!(
            refused,
            Err(midi_harbor_daemon::DaemonError::Failure(
                FailureReason::ResourceLimit
            ))
        ),
        "a new port the system refuses must say the limit was reached, got {refused:?}"
    );
    assert!(
        daemon.resolve("Bass").await.is_err(),
        "the refused port was kept"
    );

    // A configured port under the same refusal waits and is retried.
    let port = waiting_port(&daemon, &platform).await;
    let (phase, reason, open) = state_of(&daemon, port.id).await;
    assert_eq!(
        phase,
        ConnectionPhase::Unavailable,
        "a configured port the system refused must wait rather than be dropped"
    );
    assert_eq!(
        reason,
        Some(FailureReason::ResourceLimit),
        "the waiting port must say why it is waiting"
    );
    assert!(!open, "a refused port holds no platform handle");

    assert!(
        connects_within(&daemon, port.id, Duration::from_secs(3)).await,
        "the port stayed shut after the system would have allowed it"
    );
    assert!(
        platform.port_handle("Synth").is_some(),
        "no port exists on the platform"
    );
    let events = daemon.events(None, 100).await;
    assert!(
        events.iter().any(|event| event
            .detail
            .ends_with("Synth connected after 1 failed attempt")),
        "the recovery is missing from the history"
    );
}

/// Proves that a device another application holds is reported as claimed, and opens once that
/// application lets go.
///
/// Nothing announces the release, so only a retry can notice it.
#[tokio::test]
async fn a_device_another_application_held_opens_once_it_lets_go() {
    let (daemon, platform) = daemon("claimed").await;
    platform.attach(DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            name: "Keystation".to_owned(),
            ..DeviceFingerprint::default()
        },
        direction: Direction::Bidirectional,
        claimed_by: Some("Logic Pro".to_owned()),
        software: false,
    });

    let id = wait_for_endpoint(&daemon, "Keystation").await;
    let (_, reason, open) = state_of(&daemon, id).await;
    assert!(
        matches!(reason, Some(FailureReason::DeviceClaimed { .. })),
        "expected the claim to be reported, got {reason:?}"
    );
    assert!(!open, "a held device must not read as open");

    platform.release("Keystation");
    assert!(
        connects_within(&daemon, id, Duration::from_secs(3)).await,
        "the device stayed shut after the other application let go"
    );
    assert!(
        platform.device_handle("Keystation").is_some(),
        "the device reads connected but was never opened on the platform"
    );
    let history = daemon.events(None, 100).await;
    assert!(
        history
            .iter()
            .any(|event| event.detail.starts_with("Keystation could not open")),
        "the claim was never recorded, only the recovery"
    );
}

/// Proves that a port switched off while waiting to be retried stays shut, rather than being
/// opened by the retry behind the user's back.
///
/// The wait is a second, four times the first backoff, so a retry that ignored the switch would
/// have run.
#[tokio::test]
async fn a_port_disabled_while_waiting_is_not_opened_behind_the_users_back() {
    let (daemon, platform) = daemon("disabled").await;
    let port = waiting_port(&daemon, &platform).await;
    daemon
        .set_enabled(port.id, false)
        .await
        .expect("the waiting port is switched off");

    tokio::time::sleep(Duration::from_secs(1)).await;
    let (phase, _, open) = state_of(&daemon, port.id).await;
    assert_eq!(
        phase,
        ConnectionPhase::Disabled,
        "a port the user switched off must read as switched off"
    );
    assert!(!open, "a switched-off port must hold no handle");
    assert!(
        platform.port_handle("Synth").is_none(),
        "a retry opened the switched-off port on the platform"
    );
}

/// Waits for hardware to be listed, returning its endpoint.
async fn wait_for_endpoint(daemon: &Arc<Daemon>, name: &str) -> EndpointId {
    for _ in 0..60 {
        let found = daemon
            .read(|config, runtime| {
                config
                    .endpoints
                    .iter()
                    .find(|endpoint| endpoint.name.as_str() == name)
                    .filter(|endpoint| runtime.contains_key(&endpoint.id))
                    .map(|endpoint| endpoint.id)
            })
            .await;
        if let Some(id) = found {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{name} never appeared");
}
