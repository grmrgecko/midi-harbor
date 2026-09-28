//! A network port's automatic port (FR-015h).
//!
//! Other applications see a network port as a MIDI port of its name, joined to it both ways: what
//! they send the port goes out over the network, and what arrives over the network comes out of
//! the port. It is the network port's own, renamed and removed with it and never listed apart.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::{EndpointKind, InvitationPolicy};
use midi_harbor_core::fingerprint::DeviceFingerprint;
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::midi::{CC_ALL_NOTES_OFF, Channel, MidiMessage};
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::{DiscoveredDevice, MidiPlatform, PortHandle};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Starts a daemon standing in for one machine, over a scratch directory of its own, returning
/// it with its fake platform.
async fn machine(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root = common::scratch("midi-harbor-automatic-port")
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

/// Returns a note on channel 1 at velocity 100.
fn note(n: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        channel: Channel::new(0).expect("channel 0 is valid"),
        note: n,
        velocity: 100,
    }
}

/// Reports whether an all-notes-off was sent among `sent`.
fn notes_released(sent: &[MidiMessage]) -> bool {
    sent.iter().any(|message| {
        matches!(
            message,
            MidiMessage::ControlChange {
                controller: CC_ALL_NOTES_OFF,
                ..
            }
        )
    })
}

/// Waits for a condition, so the test does not depend on dispatch or network timing.
async fn eventually(mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..300 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Returns the platform identifiers stored for a network port's automatic port.
async fn stored_ids(daemon: &Arc<Daemon>, id: EndpointId) -> (Option<u32>, Option<u32>) {
    daemon
        .read(|config, _| match config.endpoint(id).map(|e| &e.kind) {
            Some(EndpointKind::NetworkSession(session)) => {
                (session.port_input_id, session.port_output_id)
            }
            _ => panic!("not a network port"),
        })
        .await
}

/// Builds two machines whose network ports are connected over loopback: Stage on the near one,
/// Front of House on the far one.
async fn joined() -> (
    Arc<Daemon>,
    Arc<FakeMidiPlatform>,
    Arc<Daemon>,
    Arc<FakeMidiPlatform>,
) {
    let (far, far_platform) = machine("far").await;
    let accepting = far
        .create_network_session("Front of House", 0, InvitationPolicy::AcceptAll)
        .await
        .expect("the far network port is created");
    let port = far
        .session_status(accepting.id)
        .await
        .expect("the far network port is listening")
        .control_port;

    let (near, near_platform) = machine("near").await;
    let stage = near
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near network port is created");
    near.connect_peer(stage.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("the near network port invites the far one");
    (near, near_platform, far, far_platform)
}

/// Returns the automatic ports of Stage on the near platform and Front of House on the far one.
fn automatic_ports(near: &FakeMidiPlatform, far: &FakeMidiPlatform) -> (PortHandle, PortHandle) {
    (
        near.port_handle("Stage")
            .expect("Stage has an automatic port"),
        far.port_handle("Front of House")
            .expect("Front of House has an automatic port"),
    )
}

/// Proves that a network port shows to other applications as a port of its name, and that one
/// made with its automatic port switched off does not.
#[tokio::test]
async fn a_network_port_shows_to_other_applications_as_a_port_of_its_name() {
    let (daemon, platform) = machine("shown").await;
    daemon
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the network port Stage is created");
    daemon
        .create_network_port("Booth", None, 0, InvitationPolicy::Prompt, false)
        .await
        .expect("the network port Booth is created without its automatic port");

    assert!(
        platform.port_handle("Stage").is_some(),
        "a network port is not shown to other applications as a port of its name"
    );
    assert!(
        platform.port_handle("Booth").is_none(),
        "a network port with its automatic port switched off still made one"
    );
}

/// Proves that what an application sends into one automatic port comes out of the far one, with
/// no route on either machine, for channel messages and for system exclusive, and is not echoed
/// back out of the port it was sent into.
///
/// System exclusive travels as a pool handle rather than inline (Principle III), so it takes a
/// path of its own through the automatic port. Each message is sent again until it arrives,
/// since the connection may still be settling; the note's number changes each time so a late
/// arrival of an earlier try is not mistaken for the one looked for.
#[tokio::test]
async fn midi_crosses_the_network_between_automatic_ports() {
    struct Case {
        name: &'static str,
        /// Sends the message for a try into the near automatic port.
        send: fn(&FakeMidiPlatform, PortHandle, u8),
        /// Reports whether the message for a try came out of the far automatic port.
        arrived: fn(&FakeMidiPlatform, PortHandle, u8) -> bool,
        /// Reports whether anything came back out of the near automatic port.
        echoed: fn(&FakeMidiPlatform, PortHandle) -> bool,
    }
    const DUMP: [u8; 6] = [0xF0, 0x7D, 0x01, 0x02, 0x03, 0xF7];
    let cases = [
        Case {
            name: "a note",
            send: |platform, port, try_| {
                platform.feed(port, &[note(try_)]);
            },
            arrived: |platform, port, try_| platform.sent(port).contains(&note(try_)),
            echoed: |platform, port| !platform.sent(port).is_empty(),
        },
        Case {
            name: "a system exclusive dump",
            send: |platform, port, _| {
                platform.feed_bytes(port, &DUMP);
            },
            arrived: |platform, port, _| platform.sent_sysex(port).iter().any(|sent| sent == &DUMP),
            echoed: |platform, port| !platform.sent_sysex(port).is_empty(),
        },
    ];

    for case in cases {
        let (_near, near_platform, _far, far_platform) = joined().await;
        let (stage, front) = automatic_ports(&near_platform, &far_platform);

        let mut try_ = 0u8;
        let arrived = eventually(|| {
            try_ = try_.wrapping_add(1) % 100;
            (case.send)(&near_platform, stage, try_);
            (case.arrived)(&far_platform, front, try_)
        })
        .await;
        assert!(
            arrived,
            "{}: nothing came out of the far automatic port",
            case.name
        );
        assert!(
            !(case.echoed)(&near_platform, stage),
            "{}: what was sent into Stage came back out of it",
            case.name
        );
    }
}

/// Proves that an automatic port switched off is removed from the platform and, switched on
/// again, comes back under the same platform identifiers, so other applications see the same
/// port rather than a new one.
#[tokio::test]
async fn switching_the_automatic_port_off_and_on_removes_and_restores_it() {
    let (daemon, platform) = machine("switch").await;
    let stage = daemon
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the network port is created");
    let ids = stored_ids(&daemon, stage.id).await;
    assert!(
        ids.0.is_some() && ids.1.is_some(),
        "its identifiers were not kept"
    );

    daemon
        .set_automatic_port(stage.id, false)
        .await
        .expect("the automatic port is switched off");
    assert!(
        platform.port_handle("Stage").is_none(),
        "an automatic port switched off is still on the platform"
    );

    daemon
        .set_automatic_port(stage.id, true)
        .await
        .expect("the automatic port is switched on again");
    assert!(
        platform.port_handle("Stage").is_some(),
        "an automatic port switched on again is not on the platform"
    );
    assert_eq!(
        stored_ids(&daemon, stage.id).await,
        ids,
        "it came back as a different port to other applications"
    );
}

/// Proves that renaming a network port renames its automatic port on the platform, keeping its
/// platform identifiers.
#[tokio::test]
async fn renaming_a_network_port_renames_its_automatic_port() {
    let (daemon, platform) = machine("rename").await;
    let stage = daemon
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .expect("the network port is created");
    let ids = stored_ids(&daemon, stage.id).await;

    daemon
        .rename_endpoint(stage.id, "Main Stage", true)
        .await
        .expect("the network port is renamed");

    assert!(
        platform.port_handle("Stage").is_none(),
        "the old name stayed"
    );
    assert!(
        platform.port_handle("Main Stage").is_some(),
        "the automatic port does not carry the new name"
    );
    assert_eq!(
        stored_ids(&daemon, stage.id).await,
        ids,
        "renaming made the automatic port a different port to other applications"
    );
}

/// What ends a far automatic port's part in carrying a note.
#[derive(Clone, Copy, Debug)]
enum Ending {
    /// Front of House is switched off.
    SwitchedOff,
    /// The far daemon stops, silencing everything on its way out.
    DaemonStopped,
}

/// Proves that a note that came over the network and out of an automatic port is released there
/// when the network port is switched off or the daemon stops (Principle I: no note is left
/// sounding).
///
/// Once the network port is switched off its automatic port is gone, and once the daemon stops
/// nothing is left to talk to the port, so either way nothing could release the note afterwards.
#[tokio::test]
async fn an_automatic_port_releases_what_it_has_sounding_when_its_network_port_ends() {
    struct Case {
        name: &'static str,
        ending: Ending,
        /// Whether the automatic port is gone from the platform afterwards.
        removed: bool,
    }
    let cases = [
        Case {
            name: "switching the network port off",
            ending: Ending::SwitchedOff,
            removed: true,
        },
        Case {
            name: "stopping the daemon",
            ending: Ending::DaemonStopped,
            removed: false,
        },
    ];

    for case in cases {
        let (_near, near_platform, far, far_platform) = joined().await;
        let (stage, front) = automatic_ports(&near_platform, &far_platform);
        assert!(
            eventually(|| {
                near_platform.feed(stage, &[note(60)]);
                far_platform.sent(front).contains(&note(60))
            })
            .await,
            "{}: the note never came out of the far automatic port",
            case.name
        );

        match case.ending {
            Ending::SwitchedOff => {
                let id = far
                    .resolve("Front of House")
                    .await
                    .expect("the far network port exists");
                far.set_enabled(id, false)
                    .await
                    .expect("the far network port is switched off");
            }
            Ending::DaemonStopped => far.silence_all().await,
        }

        assert!(
            notes_released(&far_platform.sent(front)),
            "{}: the note that came over the network was left sounding",
            case.name
        );
        assert_eq!(
            far_platform.port_handle("Front of House").is_none(),
            case.removed,
            "{}: the automatic port should be removed only when its network port is switched off",
            case.name
        );
    }
}

/// Proves that the daemon's own automatic port, listed by the platform beside everyone else's
/// ports, is not adopted as another application's port, whether it is recognised by its
/// platform identifiers or, before the platform has given it any, by its name alone.
///
/// CoreMIDI and ALSA list this daemon's own ports beside everyone else's, and adopting the
/// automatic port would list it apart from its network port. The platform refusing the port once
/// (an injected resource limit) leaves it without identifiers; until it has them, its name is all
/// there is to recognise it by, as for a virtual port.
#[tokio::test]
async fn an_automatic_port_the_platform_lists_is_not_taken_for_an_application() {
    struct Case {
        name: &'static str,
        /// Whether the platform has given the automatic port its identifiers.
        identified: bool,
    }
    let cases = [
        Case {
            name: "with its platform identifiers",
            identified: true,
        },
        Case {
            name: "without its platform identifiers",
            identified: false,
        },
    ];

    for case in cases {
        let (daemon, platform) = machine("listed").await;
        if !case.identified {
            platform.inject(Some(midi_harbor_platform::fake::Injected::ResourceLimit));
        }
        let stage = daemon
            .create_network_session("Stage", 0, InvitationPolicy::Prompt)
            .await
            .expect("the network port is created");
        let (input, output) = stored_ids(&daemon, stage.id).await;
        assert_eq!(
            (input.is_some(), output.is_some()),
            (case.identified, case.identified),
            "{}: the automatic port's identifiers are not as the row needs",
            case.name
        );

        platform.attach(DiscoveredDevice {
            fingerprint: DeviceFingerprint {
                name: "Stage".to_owned(),
                unique_id: input,
                ..DeviceFingerprint::default()
            },
            direction: midi_harbor_core::endpoint::Direction::Bidirectional,
            claimed_by: None,
            software: true,
        });
        daemon.refresh_devices().await;

        let listed = daemon
            .read(|config, _| {
                config
                    .endpoints
                    .iter()
                    .filter(|e| matches!(e.kind, EndpointKind::PhysicalDevice(_)))
                    .count()
            })
            .await;
        assert_eq!(
            listed, 0,
            "{}: the automatic port was listed on its own",
            case.name
        );
    }
}

/// Proves that deleting a network port releases the notes on both sides of it, reports the
/// routes it leaves without an end, stops it and removes its automatic port.
///
/// A note played on the far machine sounds on Keys through the route from Stage, and a note sent
/// into Stage sounds on the far machine; deleting Stage leaves nothing to release either, so the
/// deletion must.
#[tokio::test]
async fn deleting_a_network_port_silences_its_machine_and_removes_its_port() {
    let (near, near_platform, _far, far_platform) = joined().await;
    let (stage, front) = automatic_ports(&near_platform, &far_platform);
    assert!(
        eventually(|| {
            near_platform.feed(stage, &[note(60)]);
            far_platform.sent(front).contains(&note(60))
        })
        .await,
        "the note sent into Stage never reached the far machine"
    );
    near.create_virtual_port("Keys", 1, 1)
        .await
        .expect("the port Keys is created");
    near.create_route("Keys", "Stage")
        .await
        .expect("the route from Keys to Stage is created");
    near.create_route("Stage", "Keys")
        .await
        .expect("the route from Stage to Keys is created");
    let keys = near_platform
        .port_handle("Keys")
        .expect("Keys has a platform port");
    assert!(
        eventually(|| {
            far_platform.feed(front, &[note(64)]);
            near_platform.sent(keys).contains(&note(64))
        })
        .await,
        "the note played on the far machine never reached Keys"
    );

    let id = near
        .resolve("Stage")
        .await
        .expect("the near network port exists");
    let orphaned = near
        .delete_network_port(id)
        .await
        .expect("the near network port is deleted");

    assert_eq!(
        orphaned,
        vec!["Keys -> Stage".to_owned(), "Stage -> Keys".to_owned()],
        "both routes naming Stage are reported as left without an end"
    );
    assert!(
        notes_released(&near_platform.sent(keys)),
        "the note the far machine played here was left sounding"
    );
    assert!(
        eventually(|| notes_released(&far_platform.sent(front))).await,
        "the note sent to the other machine was left sounding"
    );
    assert!(
        near_platform.port_handle("Stage").is_none(),
        "the deleted network port's automatic port is still on the platform"
    );
    assert!(
        near.resolve("Stage").await.is_err(),
        "it is still configured"
    );
    assert!(
        near.session_status(id).await.is_none(),
        "it is still running"
    );
}
