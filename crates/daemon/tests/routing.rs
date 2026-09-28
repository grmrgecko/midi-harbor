//! MIDI actually travelling along a route.
//!
//! The configuration tests prove a route resolves. This proves bytes move, which is a different
//! claim and the one that matters.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::config::RouteConfig;
use midi_harbor_core::endpoint::Direction;
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::router::RouteValidity;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::DiscoveredDevice;
use midi_harbor_platform::midi::{MidiPlatform, PortHandle};
use std::sync::Arc;
use std::time::Duration;

/// Builds a daemon over a fake platform, rooted in its own temporary directory.
async fn daemon(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root =
        common::scratch("midi-harbor-routing").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());

    let daemon = Daemon::start(
        common::quiet(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
}

/// Builds a daemon whose configuration, written by hand, gives a port and a network port the name
/// Keystation, which creating either refuses but a hand-edited file can still hold.
async fn sharing_a_name(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root =
        common::scratch("midi-harbor-routing").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let paths = common::quiet(root);
    std::fs::write(
        paths.config_file(),
        "preferences:\n\
         \x20 advertise_sessions: false\n\
         endpoints:\n\
         - name: Keystation\n\
         \x20 kind: virtual_port\n\
         - name: Keystation\n\
         \x20 kind: network_port\n\
         \x20 control_port: 0\n",
    )
    .expect("the hand-written configuration is saved");
    let platform = Arc::new(FakeMidiPlatform::new());

    let daemon = Daemon::start(paths, Arc::clone(&platform) as Arc<dyn MidiPlatform>)
        .await
        .expect("the daemon starts over the hand-written configuration");
    (daemon, platform)
}

/// Creates a one-in, one-out virtual port for each name.
async fn ports(daemon: &Arc<Daemon>, names: &[&str]) {
    for name in names {
        daemon
            .create_virtual_port(name, 1, 1)
            .await
            .unwrap_or_else(|error| panic!("the port {name} is created: {error}"));
    }
}

/// Returns the platform handle backing a named port.
fn handle_for(platform: &FakeMidiPlatform, name: &str) -> PortHandle {
    platform
        .port_handle(name)
        .unwrap_or_else(|| panic!("no platform handle for {name}"))
}

/// Returns a note-on for `n` on the first channel.
fn note(n: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        channel: Channel::new(0).expect("channel 0 is a valid channel"),
        note: n,
        velocity: 100,
    }
}

/// Waits for a condition, so the test does not depend on dispatch timing.
async fn eventually(mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Returns what a route has carried, once its counters reach at least `least`.
async fn carried(daemon: &Arc<Daemon>, route: &RouteConfig, least: u64) -> u64 {
    let id = route.id();
    let mut seen = 0;
    for _ in 0..200 {
        seen = daemon
            .route_counters()
            .await
            .get(&id)
            .map(|counters| counters.messages_sent)
            .unwrap_or_default();
        if seen >= least {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    seen
}

/// Proves that MIDI fed into a port reaches every destination routed from it, in the order it
/// was played, and that each route of the fan-out counts only what it delivered.
///
/// The endpoint's own counters cannot say which route is working: one message leaving Keyboard
/// is two deliveries. A route that has carried nothing reports zero rather than being absent, so
/// a route row answers "is it carrying" rather than only "is it valid".
#[tokio::test]
async fn midi_reaches_every_destination_of_a_fan_out_and_each_route_counts_its_own() {
    let (daemon, platform) = daemon("fanout").await;
    ports(&daemon, &["Keyboard", "Synth", "Recorder"]).await;
    let (to_synth, _) = daemon
        .create_route("Keyboard", "Synth")
        .await
        .expect("the route from Keyboard to Synth is created");
    let (to_recorder, _) = daemon
        .create_route("Keyboard", "Recorder")
        .await
        .expect("the route from Keyboard to Recorder is created");

    let counters = daemon.route_counters().await;
    for route in [&to_synth, &to_recorder] {
        assert_eq!(
            counters.get(&route.id()).map(|c| c.messages_sent),
            Some(0),
            "a route that has carried nothing must say zero rather than be absent"
        );
    }

    // Push MIDI in as another application sending into the port would.
    assert!(
        platform.feed(handle_for(&platform, "Keyboard"), &[note(60), note(64)]),
        "the fake accepts MIDI fed into an open port"
    );

    for destination in ["Synth", "Recorder"] {
        let handle = handle_for(&platform, destination);
        assert!(
            eventually(|| platform.sent(handle).len() >= 2).await,
            "midi did not reach {destination}"
        );
        assert_eq!(
            platform.sent(handle),
            vec![note(60), note(64)],
            "{destination} must receive both notes once each, in the order they were played"
        );
    }
    // Two notes were fed, so each route delivered two messages.
    assert_eq!(
        carried(&daemon, &to_synth, 2).await,
        2,
        "the route to Synth counts the two notes it delivered, not the fan-out's four"
    );
    assert_eq!(
        carried(&daemon, &to_recorder, 2).await,
        2,
        "the route to Recorder counts the two notes it delivered, not the fan-out's four"
    );
}

/// Proves that MIDI goes only where a route sends it directly.
///
/// A port with no route must be inert, not quietly broadcasting to everything. Delivery is
/// direct, so adding Synth to Recorder must not silently change where Keyboard's traffic goes.
#[tokio::test]
async fn midi_goes_nowhere_a_route_does_not_send_it_directly() {
    struct Case {
        name: &'static str,
        routes: &'static [(&'static str, &'static str)],
        reached: Option<&'static str>,
        silent: &'static str,
    }
    let cases = [
        Case {
            name: "no route at all",
            routes: &[],
            reached: None,
            silent: "Synth",
        },
        Case {
            name: "a route onward from the destination",
            routes: &[("Keyboard", "Synth"), ("Synth", "Recorder")],
            reached: Some("Synth"),
            silent: "Recorder",
        },
    ];

    for case in cases {
        let (daemon, platform) = daemon("nowhere").await;
        ports(&daemon, &["Keyboard", "Synth", "Recorder"]).await;
        for (from, to) in case.routes {
            daemon
                .create_route(from, to)
                .await
                .unwrap_or_else(|error| panic!("{}: the route {from} to {to}: {error}", case.name));
        }

        assert!(
            platform.feed(handle_for(&platform, "Keyboard"), &[note(60)]),
            "{}: the fake accepts MIDI fed into an open port",
            case.name
        );
        if let Some(reached) = case.reached {
            let handle = handle_for(&platform, reached);
            assert!(
                eventually(|| !platform.sent(handle).is_empty()).await,
                "{}: midi did not reach {reached}, which is routed directly",
                case.name
            );
        }
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(
            platform.sent(handle_for(&platform, case.silent)).is_empty(),
            "{}: midi reached {}, which no route from Keyboard names",
            case.name,
            case.silent
        );
    }
}

/// Proves that a switched-off route carries nothing and that switching it back on resumes
/// delivery without the route being recreated.
#[tokio::test]
async fn a_disabled_route_carries_nothing_then_resumes() {
    let (daemon, platform) = daemon("disabled").await;
    ports(&daemon, &["Keyboard", "Synth"]).await;
    let (route, _) = daemon
        .create_route("Keyboard", "Synth")
        .await
        .expect("the route from Keyboard to Synth is created");
    daemon
        .set_route_enabled(&route.id().to_string(), false)
        .await
        .expect("the route is switched off");

    assert!(
        platform.feed(handle_for(&platform, "Keyboard"), &[note(60)]),
        "the fake accepts MIDI fed into an open port"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        platform.sent(handle_for(&platform, "Synth")).is_empty(),
        "a switched-off route carried MIDI"
    );

    daemon
        .set_route_enabled(&route.id().to_string(), true)
        .await
        .expect("the route is switched back on");
    assert!(
        platform.feed(handle_for(&platform, "Keyboard"), &[note(62)]),
        "the fake accepts MIDI fed into an open port"
    );

    let synth = handle_for(&platform, "Synth");
    assert!(
        eventually(|| !platform.sent(synth).is_empty()).await,
        "delivery did not resume when the route was re-enabled"
    );
}

/// Proves that a route's count survives a rename of its source but starts from zero when the
/// route is deleted and made again.
///
/// A rename rewrites routes rather than dropping them, so it must not read as a reset. A remade
/// route has the same identifier, since it is derived from the pair of names, but it is not the
/// same route to the person who just deleted it.
#[tokio::test]
async fn a_routes_count_survives_a_rename_but_not_being_made_again() {
    #[derive(Clone, Copy)]
    enum Edit {
        RenameSource,
        DeleteAndRemake,
    }
    struct Case {
        name: &'static str,
        edit: Edit,
        from_after: &'static str,
        want: u64,
    }
    let cases = [
        Case {
            name: "source renamed",
            edit: Edit::RenameSource,
            from_after: "Stage Keyboard",
            want: 1,
        },
        Case {
            name: "route deleted and made again",
            edit: Edit::DeleteAndRemake,
            from_after: "Keyboard",
            want: 0,
        },
    ];

    for case in cases {
        let (daemon, platform) = daemon("counted-edit").await;
        ports(&daemon, &["Keyboard", "Synth"]).await;
        let keyboard = daemon
            .resolve("Keyboard")
            .await
            .expect("the keyboard port is listed");
        let (route, _) = daemon
            .create_route("Keyboard", "Synth")
            .await
            .expect("the route from Keyboard to Synth is created");
        assert!(
            platform.feed(handle_for(&platform, "Keyboard"), &[note(60)]),
            "{}: the fake accepts MIDI fed into an open port",
            case.name
        );
        assert_eq!(
            carried(&daemon, &route, 1).await,
            1,
            "{}: one note was fed, so the route carried one message",
            case.name
        );

        match case.edit {
            Edit::RenameSource => {
                daemon
                    .rename_endpoint(keyboard, "Stage Keyboard", true)
                    .await
                    .expect("the keyboard is renamed");
            }
            Edit::DeleteAndRemake => {
                daemon
                    .delete_route(&route.id().to_string())
                    .await
                    .expect("the route is deleted");
                daemon
                    .create_route("Keyboard", "Synth")
                    .await
                    .expect("the route is made again");
            }
        }

        let after = RouteConfig {
            from: case.from_after.to_owned(),
            to: "Synth".to_owned(),
            from_kind: None,
            to_kind: None,
            from_connector: None,
            to_connector: None,
            both_ways: false,
            enabled: true,
        };
        assert_eq!(
            daemon
                .route_counters()
                .await
                .get(&after.id())
                .map(|counters| counters.messages_sent),
            Some(case.want),
            "{}: the route's count after the edit",
            case.name
        );
    }
}

/// Proves that renaming a port renames it where other applications see it, keeps its platform
/// identity, and keeps it carrying MIDI along its rewritten route.
///
/// Renaming changed only the configuration, so other applications kept seeing the old name until
/// the daemon restarted. A new identity would lose every connection other applications had made
/// to the port.
#[tokio::test]
async fn a_renamed_port_is_renamed_where_other_applications_see_it() {
    let (daemon, platform) = daemon("platform-rename").await;
    ports(&daemon, &["Keyboard", "Synth"]).await;
    let keyboard = daemon
        .resolve("Keyboard")
        .await
        .expect("the keyboard port is listed");
    daemon
        .create_route("Keyboard", "Synth")
        .await
        .expect("the route from Keyboard to Synth is created");
    let pinned = |config: &midi_harbor_core::config::Configuration| {
        config.endpoint(keyboard).and_then(|e| match &e.kind {
            midi_harbor_core::endpoint::EndpointKind::VirtualPort(port) => {
                port.output_ids.first().copied()
            }
            _ => None,
        })
    };
    let identity = daemon.read(|config, _| pinned(config)).await;
    assert!(
        identity.is_some(),
        "a created port records the identifier the platform gave it"
    );

    daemon
        .rename_endpoint(keyboard, "Stage Keyboard", true)
        .await
        .expect("the keyboard is renamed");

    assert_eq!(
        platform.port_names(),
        vec!["Stage Keyboard".to_owned(), "Synth".to_owned()],
        "other applications must see the new name without a restart"
    );
    assert_eq!(
        daemon.read(|config, _| pinned(config)).await,
        identity,
        "the port came back with a different identity, so connections to it are lost"
    );
    let synth = handle_for(&platform, "Synth");
    assert!(
        platform.feed(handle_for(&platform, "Stage Keyboard"), &[note(61)]),
        "the fake accepts MIDI fed into the renamed port"
    );
    assert!(
        eventually(|| platform.sent(synth).contains(&note(61))).await,
        "the renamed port no longer carries MIDI along its route"
    );
}

/// Proves that hardware sharing a name with one of the daemon's own ports is still listed.
///
/// Our own ports are told apart from hardware by the identifier the platform gave them. Telling
/// them apart by name made a real Keystation vanish once a port was named after it.
#[tokio::test]
async fn a_port_named_after_a_device_does_not_hide_the_device() {
    let (daemon, platform) = daemon("namesake").await;
    ports(&daemon, &["Keystation"]).await;
    platform.attach(DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            name: "Keystation".to_owned(),
            unique_id: Some(0x4B53),
            ..DeviceFingerprint::default()
        },
        direction: Direction::Bidirectional,
        claimed_by: None,
        software: false,
    });
    daemon.refresh_devices().await;

    assert_eq!(
        remembered(&daemon).await,
        vec!["Keystation".to_owned()],
        "the device was hidden behind a port of the same name"
    );
}

/// Proves that a name two endpoints share is refused as ambiguous rather than missing, and that
/// a route naming its ends by identifier carries from exactly the endpoint it was given.
///
/// The contract accepts a name or an identifier, and an identifier is the only way to tell apart
/// two endpoints sharing a name. Only names were looked up, so a client passing identifiers was
/// told the endpoints did not exist; then routes stored only names, and resolving them by name
/// alone bound this one to the network port created after the virtual port, so the port's MIDI
/// went nowhere.
#[tokio::test]
async fn a_shared_name_is_ambiguous_and_an_identifier_says_which_endpoint_a_route_means() {
    let (daemon, platform) = sharing_a_name("by-identifier").await;
    let keys = daemon
        .read(|config, _| {
            config
                .virtual_ports()
                .find(|e| e.name.as_str() == "Keystation")
                .cloned()
        })
        .await
        .expect("the hand-written Keystation port is listed");
    let synth = daemon
        .create_virtual_port("Synth", 1, 1)
        .await
        .expect("the synth port is created");

    let refused = daemon.create_route("Keystation", "Synth").await;
    assert!(
        matches!(
            refused,
            Err(midi_harbor_daemon::DaemonError::Ambiguous { count: 2, .. })
        ),
        "a name two endpoints share must be refused as ambiguous between both, got {refused:?}"
    );

    daemon
        .create_route(&keys.id.to_string(), &synth.id.to_string())
        .await
        .expect("the route named by identifiers is created");

    assert!(
        platform.feed(handle_for(&platform, "Keystation"), &[note(71)]),
        "the fake accepts MIDI fed into an open port"
    );
    let synth_handle = handle_for(&platform, "Synth");
    assert!(
        eventually(|| platform.sent(synth_handle).contains(&note(71))).await,
        "the port's MIDI never reached the synth"
    );
}

/// Proves that a route to a switched-off endpoint reads as suspended, carries and counts
/// nothing while it waits, and resumes with no further action when the endpoint comes back.
///
/// A route that reports ok while dropping everything is the worst state to debug: the display
/// agrees with the user that it should be working. It is not broken either, since nothing needs
/// restoring.
#[tokio::test]
async fn a_route_to_a_switched_off_endpoint_waits_and_resumes_when_it_is_back() {
    let (daemon, platform) = daemon("suspended").await;
    ports(&daemon, &["Keyboard", "Synth"]).await;
    let (route, _) = daemon
        .create_route("Keyboard", "Synth")
        .await
        .expect("the route from Keyboard to Synth is created");

    let synth = daemon.resolve("Synth").await.expect("the synth is listed");
    daemon
        .set_enabled(synth, false)
        .await
        .expect("the synth is switched off");

    let router = daemon.router().await;
    assert_eq!(
        router.routes().first().map(|r| &r.validity),
        Some(&RouteValidity::Suspended {
            waiting_on: vec!["Synth".to_owned()]
        }),
        "a route to a switched-off endpoint must say what it waits on rather than read as fine"
    );
    assert!(
        router.broken().is_empty(),
        "a switched-off endpoint is not a broken route: nothing needs restoring"
    );

    assert!(
        platform.feed(handle_for(&platform, "Keyboard"), &[note(60)]),
        "the fake accepts MIDI fed into an open port"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        daemon
            .route_counters()
            .await
            .get(&route.id())
            .map(|counters| counters.messages_sent),
        Some(0),
        "a suspended route counted traffic it did not carry"
    );

    daemon
        .set_enabled(synth, true)
        .await
        .expect("the synth is switched back on");
    assert_eq!(
        daemon.router().await.routes().first().map(|r| &r.validity),
        Some(&RouteValidity::Valid),
        "the route must read valid again once its endpoint is back"
    );

    assert!(
        platform.feed(handle_for(&platform, "Keyboard"), &[note(60)]),
        "the fake accepts MIDI fed into an open port"
    );
    let reopened = handle_for(&platform, "Synth");
    assert!(
        eventually(|| !platform.sent(reopened).is_empty()).await,
        "delivery did not resume when the endpoint came back"
    );
}

/// Describes hardware the fake can present as attached.
fn device(name: &str) -> DiscoveredDevice {
    DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            name: name.to_owned(),
            ..DeviceFingerprint::default()
        },
        direction: Direction::Bidirectional,
        claimed_by: None,
        software: false,
    }
}

/// Returns how many endpoints have a name beginning with `prefix`.
async fn named_like(daemon: &Arc<Daemon>, prefix: &str) -> usize {
    daemon
        .read(|config, _| {
            config
                .endpoints
                .iter()
                .filter(|e| e.name.as_str().starts_with(prefix))
                .count()
        })
        .await
}

/// Proves that enumerating hardware again neither adds it a second time nor reopens it.
///
/// A device known only by its name matched nothing, not even the entry made for it, so every
/// enumeration added it again under the same name and opened it again.
#[tokio::test]
async fn enumerating_again_neither_duplicates_nor_reopens_hardware() {
    let (daemon, platform) = daemon("re-enumerate").await;
    platform.attach(device("Keystation"));
    daemon.refresh_devices().await;
    let handle = platform.device_handle("Keystation");
    assert!(handle.is_some(), "attached hardware is opened when found");

    daemon.refresh_devices().await;
    daemon.refresh_devices().await;

    assert_eq!(
        named_like(&daemon, "Keystation").await,
        1,
        "the device was added again"
    );
    assert_eq!(
        platform.device_handle("Keystation"),
        handle,
        "the device was reopened"
    );
}

/// Proves that a route to unplugged hardware reads as suspended, and that plugging it back in
/// resumes the route and carries MIDI with no user action.
///
/// The configuration is kept so the route survives the unplug, which is what makes it read as
/// valid. Saying nothing more would leave a route reporting ok while carrying nothing.
#[tokio::test]
async fn a_route_to_unplugged_hardware_waits_rather_than_claiming_to_work() {
    let (daemon, platform) = daemon("unplugged").await;
    platform.attach(device("Keystation"));
    daemon.refresh_devices().await;
    ports(&daemon, &["Synth"]).await;
    daemon
        .create_route("Keystation", "Synth")
        .await
        .expect("the route from Keystation to Synth is created");
    assert_eq!(
        daemon.router().await.routes().first().map(|r| &r.validity),
        Some(&RouteValidity::Valid),
        "a route between attached hardware and an open port is valid"
    );

    platform.detach("Keystation");
    daemon.refresh_devices().await;

    assert_eq!(
        daemon.router().await.routes().first().map(|r| &r.validity),
        Some(&RouteValidity::Suspended {
            waiting_on: vec!["Keystation".to_owned()]
        }),
        "a route to unplugged hardware must say it waits on the hardware"
    );

    platform.attach(device("Keystation"));
    daemon.refresh_devices().await;
    assert_eq!(
        daemon.router().await.routes().first().map(|r| &r.validity),
        Some(&RouteValidity::Valid),
        "the route did not resume when the hardware came back"
    );

    // Fed repeatedly rather than once: the watcher re-enumerates on its own schedule, so the
    // handle the fake reports can be a moment behind the one the daemon has just opened.
    let synth = handle_for(&platform, "Synth");
    let mut arrived = false;
    for _ in 0..40 {
        if let Some(keystation) = platform.device_handle("Keystation") {
            let _ = platform.feed(keystation, &[note(60)]);
        }
        if !platform.sent(synth).is_empty() {
            arrived = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        arrived,
        "the replugged hardware was listed as open but carried nothing"
    );
}

/// Proves that hardware plugged in while the daemon runs is listed within two seconds with no
/// command (FR-015c, SC-010a).
///
/// The backend reports arrivals from its own thread and holds them until they are taken; nothing
/// took them, so the enumeration only ever ran at startup. SC-010a promises two seconds, so that
/// is the whole of the wait.
#[tokio::test]
async fn hardware_plugged_in_while_running_appears_on_its_own() {
    let (daemon, platform) = daemon("hotplug").await;
    assert!(
        daemon.read(|config, _| config.endpoints.is_empty()).await,
        "the daemon started with endpoints it should not have"
    );

    platform.attach(device("Keystation"));

    let started = std::time::Instant::now();
    let mut appeared = false;
    while started.elapsed() < Duration::from_secs(2) {
        appeared = named_like(&daemon, "Keystation").await > 0;
        if appeared {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        appeared,
        "hardware plugged in while running did not appear within two seconds"
    );
}

/// Proves that hardware replugged into another port keeps its endpoint and that the route drawn
/// to it carries MIDI again without being drawn anew (FR-015e).
///
/// A device replugged elsewhere is the same device, and its USB serial settles that even though
/// its topology path changed.
#[tokio::test]
async fn hardware_moved_to_another_port_keeps_its_endpoint_and_its_routes() {
    let (daemon, platform) = daemon("moved").await;
    let at = |socket: &str| DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            name: "Keystation".to_owned(),
            usb_serial: Some("KS-0042#0".to_owned()),
            topology_path: Some(format!("usb-{socket}:0")),
            ..DeviceFingerprint::default()
        },
        direction: Direction::Bidirectional,
        claimed_by: None,
        software: false,
    };
    ports(&daemon, &["Synth"]).await;
    platform.attach(at("1-1.1"));
    daemon.refresh_devices().await;
    let id = daemon
        .resolve("Keystation")
        .await
        .expect("the device is listed once attached");
    daemon
        .create_route("Keystation", "Synth")
        .await
        .expect("the route from Keystation to Synth is created");

    platform.detach("Keystation");
    daemon.refresh_devices().await;
    platform.attach(at("3-2"));
    daemon.refresh_devices().await;

    assert_eq!(
        daemon
            .resolve("Keystation")
            .await
            .expect("the moved device is still listed"),
        id,
        "the moved device came back as a new endpoint"
    );
    assert_eq!(
        named_like(&daemon, "Keystation").await,
        1,
        "the moved device was listed a second time beside its old entry"
    );
    let keys = platform
        .device_handle("Keystation")
        .expect("the moved device is reopened");
    let synth = handle_for(&platform, "Synth");
    assert!(
        platform.feed(keys, &[note(67)]),
        "the fake accepts MIDI fed from the reopened device"
    );
    assert!(
        eventually(|| platform.sent(synth).contains(&note(67))).await,
        "the route no longer carries MIDI from the moved device"
    );
}

/// Proves that forgetting unplugged hardware removes its entry and names the routes it
/// orphaned, which are kept and read broken rather than being quietly removed.
///
/// Every device ever seen is remembered so its routes survive being unplugged, which means there
/// has to be a way to say a device is not coming back. The user decides what happens to the
/// routes.
#[tokio::test]
async fn forgetting_hardware_removes_it_and_names_what_it_orphaned() {
    let (daemon, platform) = daemon("forget").await;
    platform.attach(device("Keystation"));
    daemon.refresh_devices().await;
    ports(&daemon, &["Synth"]).await;
    daemon
        .create_route("Keystation", "Synth")
        .await
        .expect("the route from Keystation to Synth is created");

    platform.detach("Keystation");
    daemon.refresh_devices().await;
    let id = daemon
        .resolve("Keystation")
        .await
        .expect("unplugged hardware is still remembered");

    let (orphaned, present) = daemon
        .forget_device(id)
        .await
        .expect("the unplugged device is forgotten");
    assert!(
        !present,
        "the hardware was not attached, so it must not be reported as present"
    );
    assert_eq!(
        orphaned,
        vec!["Keystation -> Synth".to_owned()],
        "forgetting must name the one route the device fed"
    );

    assert_eq!(
        named_like(&daemon, "Keystation").await,
        0,
        "the forgotten device is still remembered"
    );

    let router = daemon.router().await;
    assert_eq!(
        router.routes().len(),
        1,
        "the orphaned route must be kept for the user to decide on"
    );
    assert!(
        !router.broken().is_empty(),
        "the orphaned route must read broken"
    );
}

/// Proves that forgetting hardware that is still attached says so, and that the hardware is
/// listed again at once as a new entry.
///
/// That is how to start over with a device whose settings have gone wrong. Saying so is the
/// difference between that and a command that looks like it did nothing. Nothing changed on the
/// platform, so no event would have prompted a refresh; the new entry must appear without one.
#[tokio::test]
async fn forgetting_hardware_that_is_still_attached_says_it_is_still_attached() {
    let (daemon, platform) = daemon("forget-present").await;
    platform.attach(device("Keystation"));
    daemon.refresh_devices().await;
    let id = daemon
        .resolve("Keystation")
        .await
        .expect("the device is listed once attached");

    let (_orphaned, present) = daemon
        .forget_device(id)
        .await
        .expect("the attached device is forgotten");
    assert!(
        present,
        "the hardware was attached and should be reported so"
    );

    let back = daemon
        .resolve("Keystation")
        .await
        .expect("the attached device is listed again at once");
    assert_ne!(
        back, id,
        "it came back as the same entry rather than a new one"
    );
}

/// Builds hardware that is identical but for where it is plugged in.
fn twin(name: &str, port: &str) -> DiscoveredDevice {
    DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            name: name.to_owned(),
            topology_path: Some(format!("alsa:128:{port}")),
            ..DeviceFingerprint::default()
        },
        direction: Direction::Bidirectional,
        claimed_by: None,
        software: false,
    }
}

/// Proves that with two identical devices attached, the remembered entry is not guessed onto
/// either, and saying which one was meant binds it to that one (FR-015g).
///
/// Two identical devices cannot be told apart by anything but where they are plugged in, so the
/// entry is left unbound rather than guessing which one a route meant.
#[tokio::test]
async fn saying_which_identical_device_was_meant_binds_it() {
    let (daemon, platform) = daemon("ambiguous").await;
    platform.attach(twin("Acme K61", "0"));
    daemon.refresh_devices().await;
    let stored = daemon
        .resolve("Acme K61")
        .await
        .expect("the first device is listed");

    // A second, identical one arrives.
    platform.attach(twin("Acme K61", "1"));
    daemon.refresh_devices().await;

    let names = remembered(&daemon).await;
    assert_eq!(
        names
            .iter()
            .filter(|name| name.as_str() == "Acme K61")
            .count(),
        1,
        "more than one endpoint answers to the same name: {names:?}"
    );

    // The user says which one the remembered entry means.
    let chosen = DeviceFingerprint {
        name: "Acme K61".to_owned(),
        topology_path: Some("alsa:128:1".to_owned()),
        ..DeviceFingerprint::default()
    };
    let resolved = daemon
        .resolve_device(stored, &chosen)
        .await
        .expect("the remembered entry is bound to the chosen device");
    assert_eq!(
        resolved.name.as_str(),
        "Acme K61",
        "the bound entry keeps the name routes know it by"
    );

    // The stand-in entry is gone, and the remembered one is bound to the hardware chosen.
    let devices = daemon
        .read(|config, _| {
            config
                .endpoints
                .iter()
                .filter_map(|endpoint| match &endpoint.kind {
                    midi_harbor_core::endpoint::EndpointKind::PhysicalDevice(device) => Some((
                        endpoint.name.to_string(),
                        device.present,
                        device.fingerprint.topology_path.clone(),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .await;
    assert_eq!(
        devices.len(),
        2,
        "two devices are attached, so exactly two entries remain: {devices:?}"
    );
    let bound = devices
        .iter()
        .find(|(name, _, _)| name == "Acme K61")
        .expect("the remembered entry is still listed");
    assert_eq!(
        bound.2.as_deref(),
        Some("alsa:128:1"),
        "the remembered entry must be bound to the device plugged in where the user chose"
    );
    assert!(bound.1, "the resolved entry should be present");
}

/// Proves that a route from an endpoint with no input side is refused as an invalid request,
/// while the same endpoint is accepted as a route's destination.
///
/// It was reported as an invalid configuration, which read "configuration is invalid" and then
/// gave the same reason again as the thing to correct, though no configuration was involved. The
/// gRPC code and message are what the CLI and GUI show. Hardware is where a one-way endpoint
/// remains: a virtual port always has both sides.
#[tokio::test]
async fn a_route_from_an_endpoint_that_sends_nothing_is_an_invalid_request() {
    let (daemon, platform) = daemon("not-a-source").await;
    platform.attach(DiscoveredDevice {
        direction: Direction::Output,
        ..device("Speaker")
    });
    daemon.refresh_devices().await;
    ports(&daemon, &["Synth"]).await;

    let refused = daemon
        .create_route("Speaker", "Synth")
        .await
        .expect_err("a route from an output-only device is refused");
    assert!(
        matches!(
            refused,
            midi_harbor_daemon::DaemonError::InvalidRoute(
                midi_harbor_core::router::RouteError::NotASource(_)
            )
        ),
        "the refusal must say the endpoint is not a source, got {refused:?}"
    );
    let status = tonic::Status::from(refused);
    assert_eq!(
        status.code(),
        tonic::Code::InvalidArgument,
        "a bad route request is the caller's argument, not the configuration"
    );
    assert_eq!(
        status.message(),
        "Speaker has no input side, so it cannot be a route source",
        "the message must say once what is wrong"
    );

    daemon
        .create_route("Synth", "Speaker")
        .await
        .expect("an output-only device is accepted as a route's destination");
}

/// Returns the names of the device entries the configuration holds.
async fn remembered(daemon: &Arc<Daemon>) -> Vec<String> {
    daemon
        .read(|config, _| {
            config
                .endpoints
                .iter()
                .filter(|e| {
                    matches!(
                        e.kind,
                        midi_harbor_core::endpoint::EndpointKind::PhysicalDevice(_)
                    )
                })
                .map(|e| e.name.as_str().to_owned())
                .collect()
        })
        .await
}

/// Proves that another application's port is remembered after it closes only while a route
/// names it, while hardware is remembered after an unplug whether routed or not.
///
/// Every program that ever opened a port stayed in the configuration as absent hardware:
/// aseqdump on Linux, Apple's network session on macOS.
#[tokio::test]
async fn an_applications_port_is_remembered_only_while_a_route_names_it() {
    let (daemon, platform) = daemon("software-ports").await;
    ports(&daemon, &["Keys"]).await;
    let software = |name: &str| DiscoveredDevice {
        software: true,
        ..device(name)
    };

    // A passing tool is shown while it is there, and leaves nothing behind.
    platform.attach(software("aseqdump"));
    assert!(
        eventually_async(|| async { remembered(&daemon).await.contains(&"aseqdump".to_owned()) })
            .await,
        "an application's open port must be listed while it is there"
    );
    platform.detach("aseqdump");
    assert!(
        eventually_async(|| async { !remembered(&daemon).await.contains(&"aseqdump".to_owned()) })
            .await,
        "an unrouted application's port must be forgotten when it closes"
    );

    // A synth someone routes to is kept while it is closed, so the route resumes.
    platform.attach(software("FluidSynth"));
    assert!(
        eventually_async(|| async { remembered(&daemon).await.contains(&"FluidSynth".to_owned()) })
            .await,
        "an application's open port must be listed while it is there"
    );
    daemon
        .create_route("Keys", "FluidSynth")
        .await
        .expect("the route from Keys to FluidSynth is created");
    platform.detach("FluidSynth");

    // Hardware is kept while unplugged, as before, route or not.
    platform.attach(device("Keystation"));
    assert!(
        eventually_async(|| async { remembered(&daemon).await.contains(&"Keystation".to_owned()) })
            .await,
        "plugged-in hardware must be listed"
    );
    platform.detach("Keystation");
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    let kept = remembered(&daemon).await;
    assert!(
        kept.contains(&"FluidSynth".to_owned()),
        "a closed application's port a route names must be remembered: {kept:?}"
    );
    assert!(
        kept.contains(&"Keystation".to_owned()),
        "unplugged hardware must be remembered, route or not: {kept:?}"
    );
    assert!(
        !kept.contains(&"aseqdump".to_owned()),
        "a closed application's port no route names must not come back: {kept:?}"
    );
}

/// Waits for an asynchronous condition.
async fn eventually_async<F, Fut>(mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Proves that an IAC bus edited in Audio MIDI Setup keeps its one entry and its routes, which
/// go on carrying MIDI to it.
///
/// Editing a bus gives it a new CoreMIDI unique identifier. It came back listed a second time,
/// and the route went on waiting for the entry that would never return.
#[tokio::test]
async fn an_iac_bus_edited_in_audio_midi_setup_keeps_its_routes() {
    let (daemon, platform) = daemon("iac-edit").await;
    ports(&daemon, &["Keys"]).await;
    let bus = |unique_id: u32| DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            unique_id: Some(unique_id),
            name: "IAC Driver Bus 1".to_owned(),
            ..DeviceFingerprint::default()
        },
        software: true,
        ..device("IAC Driver Bus 1")
    };
    // Far above the identifiers the fake gives this daemon's own ports.
    platform.attach(bus(9_001));
    daemon.refresh_devices().await;
    daemon
        .create_route("Keys", "IAC Driver Bus 1")
        .await
        .expect("the route from Keys to the bus is created");

    platform.detach("IAC Driver Bus 1");
    daemon.refresh_devices().await;
    platform.attach(bus(9_002));
    daemon.refresh_devices().await;

    let listed = remembered(&daemon).await;
    assert_eq!(
        listed
            .iter()
            .filter(|name| name.starts_with("IAC Driver Bus 1"))
            .count(),
        1,
        "the edited bus must keep its one entry rather than be listed again: {listed:?}"
    );
    let router = daemon.router().await;
    assert!(
        router
            .routes()
            .iter()
            .all(|route| route.validity == RouteValidity::Valid),
        "the route to the edited bus must read valid rather than wait: {:?}",
        router.routes()
    );
    let keys = handle_for(&platform, "Keys");
    let to_bus = platform
        .device_handle("IAC Driver Bus 1")
        .expect("the edited bus is open");
    assert!(
        platform.feed(keys, &[note(60)]),
        "the fake accepts MIDI fed into an open port"
    );
    assert!(
        eventually(|| platform.sent(to_bus) == vec![note(60)]).await,
        "the route to the edited bus carried nothing"
    );
}

/// Returns how many messages an endpoint has received, once it reaches at least `least`.
async fn received(daemon: &Arc<Daemon>, name: &str, least: u64) -> u64 {
    let id = daemon
        .resolve(name)
        .await
        .unwrap_or_else(|error| panic!("{name} is listed: {error}"));
    let mut seen = 0;
    for _ in 0..200 {
        seen = daemon
            .counters(id)
            .await
            .map(|counters| counters.messages_received)
            .unwrap_or_default();
        if seen >= least {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    seen
}

/// Proves that a device's counters survive an unplug and go on from there when it returns.
///
/// Every replug started the device's counters from zero, so a pad controller played all session
/// reported receiving nothing, beside a route from it that had carried every note.
#[tokio::test]
async fn a_device_keeps_its_counts_when_it_is_unplugged() {
    let (daemon, platform) = daemon("counted-unplug").await;
    platform.attach(device("Keystation"));
    daemon.refresh_devices().await;
    ports(&daemon, &["Synth"]).await;
    daemon
        .create_route("Keystation", "Synth")
        .await
        .expect("the route from Keystation to Synth is created");

    let keystation = platform
        .device_handle("Keystation")
        .expect("attached hardware is opened");
    assert!(
        platform.feed(keystation, &[note(60), note(62)]),
        "the fake accepts MIDI fed from an open device"
    );
    assert_eq!(
        received(&daemon, "Keystation", 2).await,
        2,
        "two notes were fed, so the device received two messages"
    );

    platform.detach("Keystation");
    daemon.refresh_devices().await;
    assert_eq!(
        received(&daemon, "Keystation", 2).await,
        2,
        "unplugged, the device must still show the two messages it received"
    );

    platform.attach(device("Keystation"));
    daemon.refresh_devices().await;
    let keystation = platform
        .device_handle("Keystation")
        .expect("replugged hardware is reopened");
    assert!(
        platform.feed(keystation, &[note(64)]),
        "the fake accepts MIDI fed from the reopened device"
    );
    assert_eq!(
        received(&daemon, "Keystation", 3).await,
        3,
        "two notes before the unplug and one after make three"
    );
}
