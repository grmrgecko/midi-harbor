//! What the history records about hardware coming and going (FR-046, SC-013).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::Direction;
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::paths::Paths;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::{DiscoveredDevice, MidiPlatform};
use std::sync::Arc;

/// Proves that an endpoint going away and coming back is recorded both ways, in words that fit
/// what it is: hardware is unplugged and plugged back in, another program's port goes away and
/// is back.
///
/// The unplug was recorded and the return was not, so the history showed a device as gone that
/// had long since come back. Another program's port, or the kernel's Midi Through, was recorded
/// as unplugged hardware when it closed, though nothing was plugged into anything. Each row
/// routes from the endpoint, because a provided port nothing is routed from is forgotten when
/// it closes and so has no history. An endpoint found at startup is not news, so nothing is
/// recorded about it before it goes away.
#[tokio::test]
async fn an_endpoint_that_returns_is_recorded_as_back_in_words_that_fit_it() {
    struct Case {
        name: &'static str,
        software: bool,
        want: [&'static str; 2],
    }
    let cases = [
        Case {
            name: "hardware",
            software: false,
            want: [
                "Keystation was unplugged; routes using it are waiting",
                "Keystation was plugged back in; routes using it resume",
            ],
        },
        Case {
            name: "another program's port",
            software: true,
            want: [
                "Keystation went away; routes using it are waiting",
                "Keystation is back; routes using it resume",
            ],
        },
    ];

    for case in cases {
        let root = common::scratch("midi-harbor-history").join(uuid::Uuid::new_v4().to_string());
        let platform = Arc::new(FakeMidiPlatform::new());
        let daemon = Daemon::start(
            Paths::rooted_at(root),
            Arc::clone(&platform) as Arc<dyn MidiPlatform>,
        )
        .await
        .expect("the daemon starts over a scratch directory");
        let keystation = || DiscoveredDevice {
            fingerprint: DeviceFingerprint {
                name: "Keystation".to_owned(),
                ..DeviceFingerprint::default()
            },
            direction: Direction::Bidirectional,
            claimed_by: None,
            software: case.software,
        };

        platform.attach(keystation());
        daemon.refresh_devices().await;
        daemon
            .create_virtual_port("Harbor Out", 1, 1)
            .await
            .expect("the port Harbor Out is created");
        daemon
            .create_route("Keystation", "Harbor Out")
            .await
            .expect("the route from Keystation to Harbor Out is created");
        let id = daemon
            .resolve("Keystation")
            .await
            .expect("Keystation is listed once attached");
        let about = |events: Vec<midi_harbor_core::events::Event>| -> Vec<String> {
            events
                .into_iter()
                .filter(|event| event.endpoint == Some(id))
                .map(|event| event.detail)
                .collect()
        };
        assert!(
            about(daemon.events(None, 100).await).is_empty(),
            "{}: an endpoint found at startup is not news",
            case.name
        );

        platform.detach("Keystation");
        daemon.refresh_devices().await;
        platform.attach(keystation());
        daemon.refresh_devices().await;

        assert_eq!(
            about(daemon.events(None, 100).await),
            case.want.map(str::to_owned).to_vec(),
            "{}: the history must record the departure and the return, in words that fit",
            case.name
        );
    }
}
