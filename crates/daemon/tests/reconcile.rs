//! Applying a changed configuration without a restart (FR-050) and moving a setup between
//! machines (FR-052).
//!
//! A platform handle that is the same before and after is the evidence that an endpoint was left
//! alone: closing and reopening a port always gets a new one.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::config::{self, Configuration};
use midi_harbor_core::endpoint::{Direction, EndpointKind, InvitationPolicy};
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::state::ConnectionPhase;
use midi_harbor_daemon::Daemon;
use midi_harbor_daemon::reconcile::Applied;
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::{DiscoveredDevice, MidiPlatform};
use std::sync::Arc;
use std::time::Duration;

/// Builds a daemon over a fake platform, rooted in its own temporary directory.
async fn daemon(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root =
        common::scratch("midi-harbor-reconcile").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        common::quiet(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
}

/// Builds a daemon with three ports and a route from Keyboard to Synth.
async fn with_ports(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let (daemon, platform) = daemon(label).await;
    for name in ["Keyboard", "Synth", "Drums"] {
        daemon
            .create_virtual_port(name, 1, 1)
            .await
            .unwrap_or_else(|error| panic!("the port {name} is created: {error}"));
    }
    daemon
        .create_route("Keyboard", "Synth")
        .await
        .expect("the route from Keyboard to Synth is created");
    (daemon, platform)
}

/// Attaches the Keystation keyboard and waits for the daemon to open it.
async fn attach_keystation(daemon: &Arc<Daemon>, platform: &FakeMidiPlatform) {
    platform.attach(DiscoveredDevice {
        fingerprint: DeviceFingerprint {
            name: "Keystation".to_owned(),
            ..DeviceFingerprint::default()
        },
        direction: Direction::Bidirectional,
        claimed_by: None,
        software: false,
    });
    daemon.refresh_devices().await;
    assert!(
        eventually(|| platform.device_handle("Keystation").is_some()).await,
        "attached hardware is opened when found"
    );
}

/// Edits the configuration file as a person would, then asks for a reload.
async fn edit_and_reload(daemon: &Arc<Daemon>, edit: impl FnOnce(&mut Configuration)) -> Applied {
    let path = daemon.paths().config_file();
    let text = std::fs::read_to_string(&path).expect("the configuration file is readable");
    let mut stored = config::parse(&text).expect("the configuration file parses");
    edit(&mut stored);
    std::fs::write(
        &path,
        config::to_text(&stored).expect("the edited configuration serialises"),
    )
    .expect("the edited configuration is saved");
    daemon
        .reload_configuration()
        .await
        .expect("the edited configuration is reloaded")
}

/// Returns what shows whether an endpoint was left alone: a session's control port, or the
/// platform handle of a port or device, or `None` when it is not open.
async fn identity(daemon: &Arc<Daemon>, platform: &FakeMidiPlatform, name: &str) -> Option<String> {
    if let Ok(id) = daemon.resolve(name).await
        && let Some(status) = daemon.session_status(id).await
    {
        return Some(format!("session on port {}", status.control_port));
    }
    platform
        .port_handle(name)
        .or_else(|| platform.device_handle(name))
        .map(|handle| format!("handle {handle:?}"))
}

/// Returns the names as owned strings, for comparing with what a reload reports.
fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
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
async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..40 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Proves that a reload disturbs only what the edit changed: an edited port is reopened, an
/// added one opens, a removed or switched-off one closes, a new route carries, and everything
/// else, including attached hardware and a session whose invitation policy changed, keeps the
/// handle or control port it had (FR-050).
///
/// Every row starts from three ports, a route from Keyboard to Synth, the Keystation keyboard
/// attached, and a session named Stage. Whether hardware is attached is observed rather than
/// stored, so nothing in the file says a device is open, and a reload must not take that as a
/// reason to reopen it. A policy is read on each invitation, so changing it needs no restart.
#[tokio::test]
async fn a_reload_disturbs_only_what_the_edit_changed() {
    struct Case {
        name: &'static str,
        edit: Option<fn(&mut Configuration)>,
        want: Applied,
        kept: &'static [&'static str],
        reopened: &'static [&'static str],
        closed: &'static [&'static str],
        disabled: Option<&'static str>,
        carries: Option<(&'static str, &'static str)>,
    }
    let cases = [
        Case {
            name: "unchanged file",
            edit: None,
            want: Applied::default(),
            kept: &["Keyboard", "Synth", "Drums", "Keystation", "Stage"],
            reopened: &[],
            closed: &[],
            disabled: None,
            carries: None,
        },
        Case {
            name: "a port given another MIDI Out",
            edit: Some(|config| {
                let synth = config
                    .endpoints
                    .iter_mut()
                    .find(|e| e.name.as_str() == "Synth")
                    .expect("Synth is in the file");
                if let EndpointKind::VirtualPort(port) = &mut synth.kind {
                    port.outputs = 2;
                }
            }),
            want: Applied {
                restarted: names(&["Synth"]),
                ..Applied::default()
            },
            kept: &["Keyboard", "Drums", "Keystation", "Stage"],
            reopened: &["Synth"],
            closed: &[],
            disabled: None,
            carries: None,
        },
        Case {
            name: "a port added and another removed",
            edit: Some(|config| {
                config.endpoints.retain(|e| e.name.as_str() != "Drums");
                let mut added = config.endpoints[0].clone();
                added.id = midi_harbor_core::ids::EndpointId::new();
                added.name = midi_harbor_core::endpoint::EndpointName::new("Bass")
                    .expect("Bass is a valid name");
                added.kind = EndpointKind::VirtualPort(Default::default());
                config.endpoints.push(added);
            }),
            want: Applied {
                added: names(&["Bass"]),
                removed: names(&["Drums"]),
                ..Applied::default()
            },
            kept: &["Keyboard", "Synth", "Keystation", "Stage"],
            reopened: &["Bass"],
            closed: &["Drums"],
            disabled: None,
            carries: None,
        },
        Case {
            name: "a port switched off",
            edit: Some(|config| {
                config
                    .endpoints
                    .iter_mut()
                    .find(|e| e.name.as_str() == "Drums")
                    .expect("Drums is in the file")
                    .enabled = false;
            }),
            want: Applied::default(),
            kept: &["Keyboard", "Synth", "Keystation", "Stage"],
            reopened: &[],
            closed: &["Drums"],
            disabled: Some("Drums"),
            carries: None,
        },
        Case {
            name: "a route added",
            edit: Some(|config| {
                let mut route = config.routes[0].clone();
                route.to = "Drums".to_owned();
                config.routes.push(route);
            }),
            want: Applied {
                routes_added: 1,
                ..Applied::default()
            },
            kept: &["Keyboard", "Synth", "Drums", "Keystation", "Stage"],
            reopened: &[],
            closed: &[],
            disabled: None,
            carries: Some(("Keyboard", "Drums")),
        },
        Case {
            name: "a session's invitation policy changed",
            edit: Some(|config| {
                let endpoint = config
                    .endpoints
                    .iter_mut()
                    .find(|e| e.name.as_str() == "Stage")
                    .expect("Stage is in the file");
                if let EndpointKind::NetworkSession(held) = &mut endpoint.kind {
                    held.invitation_policy = InvitationPolicy::RejectAll;
                }
            }),
            want: Applied::default(),
            kept: &["Keyboard", "Synth", "Drums", "Keystation", "Stage"],
            reopened: &[],
            closed: &[],
            disabled: None,
            carries: None,
        },
    ];

    for case in cases {
        let (daemon, platform) = with_ports("reload").await;
        attach_keystation(&daemon, &platform).await;
        daemon
            .create_network_session("Stage", 0, InvitationPolicy::Prompt)
            .await
            .expect("the session Stage is created");
        let mut before = Vec::new();
        for name in case.kept.iter().chain(case.reopened) {
            before.push((*name, identity(&daemon, &platform, name).await));
        }

        let applied = match case.edit {
            Some(edit) => edit_and_reload(&daemon, edit).await,
            None => daemon
                .reload_configuration()
                .await
                .expect("the unchanged configuration is reloaded"),
        };

        assert_eq!(
            applied, case.want,
            "{}: the reload must report what the edit changed and nothing else",
            case.name
        );
        for (name, was) in before {
            let now = identity(&daemon, &platform, name).await;
            if case.kept.contains(&name) {
                assert!(
                    was.is_some() && now == was,
                    "{}: {name} was disturbed though the edit did not touch it ({was:?} became {now:?})",
                    case.name
                );
            } else {
                assert!(
                    now.is_some() && now != was,
                    "{}: {name} must be open under a new handle after the edit ({was:?} became {now:?})",
                    case.name
                );
            }
        }
        for name in case.closed {
            assert_eq!(
                identity(&daemon, &platform, name).await,
                None,
                "{}: {name} is still open after the edit removed or switched it off",
                case.name
            );
        }
        if let Some(name) = case.disabled {
            let id = daemon
                .resolve(name)
                .await
                .unwrap_or_else(|error| panic!("{}: {name} is still listed: {error}", case.name));
            let phase = daemon
                .read(|_, runtime| runtime.get(&id).map(|r| r.state.phase()))
                .await;
            assert_eq!(
                phase,
                Some(ConnectionPhase::Disabled),
                "{}: {name} must read as switched off",
                case.name
            );
        }
        if let Some((from, to)) = case.carries {
            let destination = platform
                .port_handle(to)
                .unwrap_or_else(|| panic!("{}: {to} is open", case.name));
            let source = platform
                .port_handle(from)
                .unwrap_or_else(|| panic!("{}: {from} is open", case.name));
            assert!(
                platform.feed(source, &[note(60)]),
                "{}: the fake accepts MIDI fed into an open port",
                case.name
            );
            assert!(
                eventually(|| platform.sent(destination).contains(&note(60))).await,
                "{}: the new route from {from} to {to} carried nothing",
                case.name
            );
        }
    }
}

/// Proves that a reload of an unreadable file is refused, leaves every port running, and
/// leaves the file where it is.
///
/// At startup an unreadable file is set aside and defaults are used, because there is nothing to
/// lose. Doing that on a reload would tear down a working setup over one typo, and moving the
/// file would take it from under the person editing it.
#[tokio::test]
async fn an_unreadable_file_is_refused_and_everything_keeps_running() {
    let (daemon, platform) = with_ports("unreadable").await;
    let synth = platform.port_handle("Synth");
    let path = daemon.paths().config_file();
    std::fs::write(&path, "endpoints: [ this is not yaml")
        .expect("the broken configuration is saved");

    assert!(
        daemon.reload_configuration().await.is_err(),
        "a file that does not parse must be refused"
    );
    assert_eq!(
        platform.port_handle("Synth"),
        synth,
        "a refused reload disturbed a port"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("the broken file is still readable"),
        "endpoints: [ this is not yaml",
        "the file being edited was moved or rewritten"
    );
}

/// Proves that a merging import adds what this machine lacks and leaves what it has alone,
/// keeping the existing port's handle and identifier (FR-052).
///
/// The export holds Keyboard, Synth, Drums and one route. The target already has Synth, so only
/// Keyboard and Drums are added.
#[tokio::test]
async fn merging_adds_what_is_missing_and_touches_nothing_here() {
    let (source, _) = with_ports("merge-source").await;
    let exported = source
        .export_configuration()
        .await
        .expect("the source machine exports its setup");

    let (daemon, platform) = daemon("merge-target").await;
    let synth = daemon
        .create_virtual_port("Synth", 1, 1)
        .await
        .expect("the target's own Synth port is created");
    let handle = platform.port_handle("Synth");

    let applied = daemon
        .import_configuration(&exported, false)
        .await
        .expect("the export is merged");

    let mut added = applied.added.clone();
    added.sort();
    assert_eq!(
        added,
        names(&["Drums", "Keyboard"]),
        "only the ports this machine lacks must be added"
    );
    assert_eq!(
        applied.routes_added, 1,
        "the export's one route must be added"
    );
    assert!(
        applied.restarted.is_empty() && applied.removed.is_empty(),
        "a merge must neither restart nor remove anything: {applied:?}"
    );
    assert_eq!(
        platform.port_handle("Synth"),
        handle,
        "the port already here was disturbed"
    );
    assert_eq!(
        daemon
            .resolve("Synth")
            .await
            .expect("Synth is still listed"),
        synth.id,
        "the port already here must keep its identifier"
    );
}

/// Proves that a replacing import makes the setup match the file, removing what the file lacks,
/// while keeping this machine's identity for what matches and keeping hardware attached here
/// (FR-052).
///
/// A file from another machine cannot mention hardware plugged into this one. Removing it only
/// had it rediscovered under a new identity, after reporting it removed. The export holds
/// Keyboard, Synth and Drums, so Extra goes and those three remain.
#[tokio::test]
async fn replacing_matches_the_file_but_keeps_this_machines_identity() {
    let (source, _) = with_ports("replace-source").await;
    let exported = source
        .export_configuration()
        .await
        .expect("the source machine exports its setup");

    let (daemon, platform) = daemon("replace-target").await;
    let synth = daemon
        .create_virtual_port("Synth", 1, 1)
        .await
        .expect("the target's own Synth port is created");
    daemon
        .create_virtual_port("Extra", 1, 1)
        .await
        .expect("the target's Extra port is created");
    attach_keystation(&daemon, &platform).await;
    let keystation = daemon
        .resolve("Keystation")
        .await
        .expect("the attached Keystation is listed");
    let handle = platform.port_handle("Synth");
    let device = platform.device_handle("Keystation");

    let applied = daemon
        .import_configuration(&exported, true)
        .await
        .expect("the export replaces the setup");

    assert_eq!(
        applied.removed,
        names(&["Extra"]),
        "only the port the file lacks must be removed, not hardware attached here"
    );
    assert!(
        applied.restarted.is_empty(),
        "a matching port must not be restarted: {:?}",
        applied.restarted
    );
    assert!(
        platform.port_handle("Extra").is_none(),
        "the removed port is still open"
    );
    assert_eq!(
        platform.port_handle("Synth"),
        handle,
        "the matching port was reopened"
    );
    assert_eq!(
        daemon
            .resolve("Synth")
            .await
            .expect("Synth is still listed"),
        synth.id,
        "the matching port must keep this machine's identifier"
    );
    assert_eq!(
        platform.port_names(),
        names(&["Drums", "Keyboard", "Synth"]),
        "the ports must match the file"
    );
    assert_eq!(
        daemon
            .resolve("Keystation")
            .await
            .expect("the attached Keystation is still listed"),
        keystation,
        "attached hardware must keep its identity across a replacing import"
    );
    assert_eq!(
        platform.device_handle("Keystation"),
        device,
        "attached hardware was reopened by a replacing import"
    );
}
