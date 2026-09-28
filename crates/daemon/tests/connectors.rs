//! Virtual ports with several MIDI In and MIDI Out connectors (FR-002a).
//!
//! A port with more than one connector of a kind shows to other applications as numbered ports,
//! and a route names one connector at each end. MIDI that arrives on one connector goes only
//! where that connector is routed, and leaves through the connector its route names.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::EndpointKind;
use midi_harbor_core::midi::{CC_ALL_NOTES_OFF, Channel, MidiMessage};
use midi_harbor_core::router::RouteValidity;
use midi_harbor_daemon::{Daemon, RouteRequest};
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::{MidiPlatform, PortHandle};
use std::sync::Arc;
use std::time::Duration;

/// Builds a daemon over a fake platform, rooted in its own temporary directory.
async fn daemon(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root =
        common::scratch("midi-harbor-connectors").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        common::quiet(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
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

/// Reports whether an all-notes-off went out through one connector of an endpoint.
fn released(platform: &FakeMidiPlatform, handle: PortHandle, connector: u8) -> bool {
    platform
        .sent_through(handle, connector)
        .iter()
        .any(|message| {
            matches!(
                message,
                MidiMessage::ControlChange {
                    controller: CC_ALL_NOTES_OFF,
                    ..
                }
            )
        })
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

/// Proves that every connector of a port records its own platform identifier.
///
/// Only one identifier per port was kept, so every connector but one would have been a new port
/// to other applications after a restart. Two MIDI Ins and three MIDI Outs need five identifiers.
#[tokio::test]
async fn every_connector_keeps_its_platform_identifier() {
    let (daemon, _platform) = daemon("identifiers").await;
    daemon
        .create_virtual_port("Keys", 2, 3)
        .await
        .expect("the port Keys with two MIDI Ins and three MIDI Outs is created");

    let (inputs, outputs) = daemon
        .read(|config, _| {
            config
                .endpoints
                .iter()
                .find(|e| e.name.as_str() == "Keys")
                .and_then(|e| match &e.kind {
                    EndpointKind::VirtualPort(port) => {
                        Some((port.input_ids.clone(), port.output_ids.clone()))
                    }
                    _ => None,
                })
                .expect("the port Keys is stored as a virtual port")
        })
        .await;
    assert_eq!(
        (inputs.len(), outputs.len()),
        (2, 3),
        "each of the two MIDI Ins and three MIDI Outs must keep an identifier"
    );
}

/// Proves that reducing a port's connectors removes the routes on the connectors that went,
/// keeps the others carrying, and reopens the port with its new connector count.
///
/// A route left on a connector the port no longer has would wait for one nobody is adding.
#[tokio::test]
async fn fewer_connectors_remove_the_routes_on_the_ones_that_went() {
    let (daemon, platform) = daemon("fewer").await;
    daemon
        .create_virtual_port("Keys", 1, 1)
        .await
        .expect("the port Keys is created");
    daemon
        .create_virtual_port("Synths", 1, 2)
        .await
        .expect("the port Synths with two MIDI Outs is created");
    daemon
        .create_route_through("Keys", 0, "Synths", 0)
        .await
        .expect("the route to MIDI Out 1 is created");
    daemon
        .create_route_through("Keys", 0, "Synths", 1)
        .await
        .expect("the route to MIDI Out 2 is created");

    daemon
        .set_virtual_port_connectors("Synths", 1, 1)
        .await
        .expect("Synths is reduced to one MIDI Out");

    let routes = daemon.read(|config, _| config.routes.clone()).await;
    assert_eq!(
        routes.len(),
        1,
        "only the route on the MIDI Out that went must be removed: {routes:?}"
    );
    assert_eq!(
        routes[0].to_index(),
        0,
        "the route that remains must be the one to MIDI Out 1"
    );
    let router = daemon.router().await;
    assert!(
        router
            .routes()
            .iter()
            .all(|route| route.validity == RouteValidity::Valid),
        "the remaining route should still carry"
    );
    let synths = handle_for(&platform, "Synths");
    assert_eq!(
        platform.connectors(synths),
        Some((1, 1)),
        "the port must be reopened with the connectors it has now"
    );
}

/// Builds Keys and Synth, each with two connectors of each kind, and a two-way route between
/// their second connectors.
async fn two_way(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>, String) {
    let (daemon, platform) = daemon(label).await;
    for name in ["Keys", "Synth"] {
        daemon
            .create_virtual_port(name, 2, 2)
            .await
            .unwrap_or_else(|error| panic!("the port {name} is created: {error}"));
    }
    let (route, _) = daemon
        .create_route_with(RouteRequest {
            from: "Keys",
            from_connector: 1,
            to: "Synth",
            to_connector: 1,
            both_ways: true,
        })
        .await
        .expect("the two-way route between the second connectors is created");
    (daemon, platform, route.id().to_string())
}

/// Proves that a route carries only between the connectors it names, in each direction.
///
/// Forward, only what arrives on the source's MIDI In 2 is carried, and it leaves only through
/// the destination's MIDI Out 2. Back, it runs from the destination's MIDI In of the same number
/// as its MIDI Out to the source's MIDI Out of the same number as its MIDI In: the pair other
/// applications see as one port.
#[tokio::test]
async fn a_two_way_route_carries_each_way_only_between_the_connectors_it_names() {
    let (_daemon, platform, _route) = two_way("both-ways").await;
    let (keys, synth) = (
        handle_for(&platform, "Keys"),
        handle_for(&platform, "Synth"),
    );

    // Forward: MIDI In 1 is not routed, MIDI In 2 is.
    assert!(
        platform.feed_connector(keys, 0, &[note(59)]),
        "the fake accepts MIDI fed into MIDI In 1"
    );
    assert!(
        platform.feed_connector(keys, 1, &[note(60)]),
        "the fake accepts MIDI fed into MIDI In 2"
    );
    assert!(
        eventually(|| platform.sent_through(synth, 1) == vec![note(60)]).await,
        "the synth's MIDI Out 2 got {:?}",
        platform.sent_through(synth, 1)
    );

    // Back: the synth's MIDI In 1 is not routed, MIDI In 2 is.
    assert!(
        platform.feed_connector(synth, 0, &[note(62)]),
        "the fake accepts MIDI fed into MIDI In 1"
    );
    assert!(
        platform.feed_connector(synth, 1, &[note(64)]),
        "the fake accepts MIDI fed into MIDI In 2"
    );
    assert!(
        eventually(|| platform.sent_through(keys, 1) == vec![note(64)]).await,
        "Keys' MIDI Out 2 got {:?}",
        platform.sent_through(keys, 1)
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        platform.sent_through(synth, 1),
        vec![note(60)],
        "MIDI In 1 of Keys was carried forward too"
    );
    assert!(
        platform.sent_through(synth, 0).is_empty(),
        "it left the synth through MIDI Out 1 as well"
    );
    assert_eq!(
        platform.sent_through(keys, 1),
        vec![note(64)],
        "MIDI In 1 of the synth was carried back too"
    );
    assert!(
        platform.sent_through(keys, 0).is_empty(),
        "it came back through MIDI Out 1"
    );
}

/// Proves that when a two-way route stops carrying, the notes it carried back to the source are
/// released there, whether the route is deleted or its destination is switched off.
///
/// The note is sounding at the source, where nothing else will send its note off.
#[tokio::test]
async fn a_two_way_route_that_stops_releases_what_it_carried_back() {
    #[derive(Clone, Copy)]
    enum Stop {
        DeleteRoute,
        SwitchOffDestination,
    }
    let cases = [
        ("route deleted", Stop::DeleteRoute),
        ("destination switched off", Stop::SwitchOffDestination),
    ];

    for (name, stop) in cases {
        let (daemon, platform, route) = two_way("release").await;
        let keys = handle_for(&platform, "Keys");
        let synth = handle_for(&platform, "Synth");
        assert!(
            platform.feed_connector(synth, 1, &[note(64)]),
            "{name}: the fake accepts MIDI fed into the synth's MIDI In 2"
        );
        assert!(
            eventually(|| platform.sent_through(keys, 1) == vec![note(64)]).await,
            "{name}: the note was not carried back to Keys"
        );

        match stop {
            Stop::DeleteRoute => daemon
                .delete_route(&route)
                .await
                .expect("the two-way route is deleted"),
            Stop::SwitchOffDestination => {
                let id = daemon.resolve("Synth").await.expect("the synth is listed");
                daemon
                    .set_enabled(id, false)
                    .await
                    .expect("the synth is switched off");
            }
        }

        assert!(
            released(&platform, keys, 1),
            "{name}: the note carried back was left sounding"
        );
    }
}

/// Proves that editing a route moves it to its new ends in place, keeps it switched off if it
/// was, and that once switched on it carries between the new ends only.
#[tokio::test]
async fn editing_a_route_moves_it_and_keeps_it_switched_off() {
    let (daemon, platform) = daemon("edit").await;
    for name in ["Keys", "Synth", "Piano"] {
        daemon
            .create_virtual_port(name, 1, 1)
            .await
            .unwrap_or_else(|error| panic!("the port {name} is created: {error}"));
    }
    let (route, _) = daemon
        .create_route("Keys", "Synth")
        .await
        .expect("the route from Keys to Synth is created");
    let id = route.id().to_string();
    daemon
        .set_route_enabled(&id, false)
        .await
        .expect("the route is switched off");

    let (moved, _) = daemon
        .update_route(
            &id,
            RouteRequest {
                from: "Keys",
                from_connector: 0,
                to: "Piano",
                to_connector: 0,
                both_ways: true,
            },
        )
        .await
        .expect("the route is edited to run both ways to Piano");

    let routes = daemon.read(|config, _| config.routes.clone()).await;
    assert_eq!(
        routes,
        vec![moved.clone()],
        "the route was added rather than moved"
    );
    assert_eq!(
        moved.to.as_str(),
        "Piano",
        "the edited route must end at Piano"
    );
    assert!(moved.both_ways, "the edited route must run both ways");
    assert!(!moved.enabled, "editing switched the route back on");

    daemon
        .set_route_enabled(&moved.id().to_string(), true)
        .await
        .expect("the edited route is switched on");
    let keys = handle_for(&platform, "Keys");
    let piano = handle_for(&platform, "Piano");
    assert!(
        platform.feed(piano, &[note(67)]),
        "the fake accepts MIDI fed into Piano"
    );
    assert!(
        eventually(|| platform.sent(keys) == vec![note(67)]).await,
        "the edited two-way route did not carry back from Piano"
    );
    assert!(
        platform.sent(handle_for(&platform, "Synth")).is_empty(),
        "the route's old destination still received MIDI"
    );
}

/// Proves that editing a route into a duplicate of another is refused, while an edit keeping
/// the route's own ends is not taken for a duplicate of itself.
#[tokio::test]
async fn editing_a_route_into_a_duplicate_is_refused_and_leaves_it_alone() {
    let (daemon, _platform) = daemon("duplicate").await;
    for name in ["Keys", "Synth", "Piano"] {
        daemon
            .create_virtual_port(name, 1, 1)
            .await
            .unwrap_or_else(|error| panic!("the port {name} is created: {error}"));
    }
    daemon
        .create_route("Keys", "Synth")
        .await
        .expect("the route from Keys to Synth is created");
    let (route, _) = daemon
        .create_route("Keys", "Piano")
        .await
        .expect("the route from Keys to Piano is created");

    let refused = daemon
        .update_route(
            &route.id().to_string(),
            RouteRequest {
                from: "Keys",
                from_connector: 0,
                to: "Synth",
                to_connector: 0,
                both_ways: false,
            },
        )
        .await;

    assert!(refused.is_err(), "a second route to the synth was allowed");

    daemon
        .update_route(
            &route.id().to_string(),
            RouteRequest {
                from: "Keys",
                from_connector: 0,
                to: "Piano",
                to_connector: 0,
                both_ways: true,
            },
        )
        .await
        .expect("the route is edited to carry both ways over the same ends");
    let routes = daemon.read(|config, _| config.routes.clone()).await;
    assert_eq!(
        routes.len(),
        2,
        "the refused edit must leave both routes as they were"
    );
    assert!(
        routes
            .iter()
            .any(|r| r.to.as_str() == "Piano" && r.both_ways),
        "the route's own ends are not a duplicate of itself, so the edit must apply: {routes:?}"
    );
}
