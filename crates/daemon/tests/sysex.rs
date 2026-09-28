//! System-exclusive messages travelling along a route.
//!
//! A dump is the one MIDI message with no fixed size, arriving in as many pieces as the platform
//! chose to split it into. These tests exist because every one of those pieces is a chance to
//! forward something that frames correctly to a receiver but is not what the sender sent.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use midi_harbor_core::endpoint::InvitationPolicy;
use midi_harbor_core::paths::Paths;
use midi_harbor_core::state::ConnectionPhase;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::{FakeMidiPlatform, Outgoing};
use midi_harbor_platform::midi::{MidiPlatform, PortHandle};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// A short universal device-inquiry dump, which is the smallest realistic whole message.
const INQUIRY: [u8; 6] = [0xF0, 0x7E, 0x00, 0x06, 0x01, 0xF7];

/// Builds a daemon over a fake platform with one route from Keyboard to Synth.
async fn routed(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root =
        common::scratch("midi-harbor-sysex").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        Paths::rooted_at(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");

    for name in ["Keyboard", "Synth"] {
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

/// Returns the platform handle backing a named port.
fn handle_for(platform: &FakeMidiPlatform, name: &str) -> PortHandle {
    platform
        .port_handle(name)
        .unwrap_or_else(|| panic!("no platform handle for {name}"))
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

/// Gives dispatch time to do the wrong thing, for the tests that assert nothing arrives.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// Proves that a dump arriving in several reads is forwarded once, whole, and only after its
/// terminator.
///
/// A patch dump is larger than one platform read, so arriving in pieces is the ordinary case.
#[tokio::test]
async fn a_dump_split_across_reads_arrives_whole_and_once() {
    let (_daemon, platform) = routed("split").await;
    let keyboard = handle_for(&platform, "Keyboard");

    assert!(
        platform.feed_bytes(keyboard, &[0xF0, 0x43, 0x10]),
        "the fake accepts the first piece of the dump"
    );
    assert!(
        platform.feed_bytes(keyboard, &[0x4C, 0x00]),
        "the fake accepts the second piece of the dump"
    );

    let synth = handle_for(&platform, "Synth");
    settle().await;
    assert!(
        platform.sent_sysex(synth).is_empty(),
        "an unfinished dump was forwarded before its terminator arrived"
    );

    assert!(
        platform.feed_bytes(keyboard, &[0x01, 0xF7]),
        "the fake accepts the last piece of the dump"
    );
    assert!(
        eventually(|| !platform.sent_sysex(synth).is_empty()).await,
        "the finished dump did not reach the destination"
    );
    assert_eq!(
        platform.sent_sysex(synth),
        vec![vec![0xF0, 0x43, 0x10, 0x4C, 0x00, 0x01, 0xF7]],
        "the three pieces must arrive as one dump, once"
    );
}

/// Proves that a dump the sender abandons for another message is dropped, while the message
/// that interrupted it gets through.
///
/// A truncated dump still frames correctly to a receiver, which will act on whatever arrived.
#[tokio::test]
async fn an_abandoned_dump_is_not_forwarded_but_what_follows_is() {
    let (_daemon, platform) = routed("abandoned").await;
    let keyboard = handle_for(&platform, "Keyboard");

    // The sender starts a dump and then sends a note instead of finishing it.
    assert!(
        platform.feed_bytes(keyboard, &[0xF0, 0x43, 0x10, 0x90, 0x3C, 0x64]),
        "the fake accepts the interrupted dump and the note"
    );

    let synth = handle_for(&platform, "Synth");
    assert!(
        eventually(|| !platform.sent(synth).is_empty()).await,
        "the note that interrupted the dump did not arrive"
    );
    settle().await;

    assert!(
        platform.sent_sysex(synth).is_empty(),
        "a dump the sender abandoned was forwarded anyway"
    );
    assert_eq!(
        platform.sent(synth).len(),
        1,
        "the note must arrive exactly once"
    );
}

/// Proves that a dump sent in one read arrives whole and keeps its place between the messages
/// around it.
///
/// A program change that follows a dump has to arrive after it: the dump usually is the program
/// the change then selects. A note, the dump and a program change are three messages.
#[tokio::test]
async fn a_dump_keeps_its_place_among_the_messages_around_it() {
    let (_daemon, platform) = routed("order").await;
    let keyboard = handle_for(&platform, "Keyboard");

    let mut stream = vec![0x90, 0x3C, 0x64];
    stream.extend_from_slice(&INQUIRY);
    stream.extend_from_slice(&[0xC0, 0x05]);
    assert!(
        platform.feed_bytes(keyboard, &stream),
        "the fake accepts the note, dump and program change"
    );

    let synth = handle_for(&platform, "Synth");
    assert!(
        eventually(|| platform.outgoing(synth).len() >= 3).await,
        "the three messages did not all arrive"
    );

    let outgoing = platform.outgoing(synth);
    assert_eq!(
        outgoing.len(),
        3,
        "a note, a dump and a program change are three messages: {outgoing:?}"
    );
    assert!(
        matches!(outgoing[0], Outgoing::Message(_)),
        "the note must come first: {outgoing:?}"
    );
    assert_eq!(
        outgoing[1],
        Outgoing::SysEx(INQUIRY.to_vec()),
        "the dump must arrive whole and second"
    );
    assert!(
        matches!(outgoing[2], Outgoing::Message(_)),
        "the program change must come after the dump: {outgoing:?}"
    );
}

/// Proves that a real-time clock byte inside a dump does not break the dump.
///
/// Real-time bytes are legal in the middle of a dump, and a sequencer sending clock while a
/// patch transfers is ordinary rather than exotic.
#[tokio::test]
async fn a_clock_running_through_a_dump_does_not_break_it() {
    let (_daemon, platform) = routed("clocked").await;
    let keyboard = handle_for(&platform, "Keyboard");

    assert!(
        platform.feed_bytes(keyboard, &[0xF0, 0x7E, 0xF8, 0x00, 0x06, 0x01, 0xF7]),
        "the fake accepts the dump with a clock inside it"
    );

    let synth = handle_for(&platform, "Synth");
    assert!(
        eventually(|| !platform.sent_sysex(synth).is_empty()).await,
        "a dump with a clock inside it did not arrive"
    );
    assert_eq!(
        platform.sent_sysex(synth),
        vec![INQUIRY.to_vec()],
        "the dump must arrive whole, without the clock byte inside it"
    );
}

/// Proves that a dump counts as one message received, by its full size in bytes.
#[tokio::test]
async fn a_dump_counts_as_one_message_of_its_full_size() {
    let (daemon, platform) = routed("counted").await;
    assert!(
        platform.feed_bytes(handle_for(&platform, "Keyboard"), &INQUIRY),
        "the fake accepts the dump"
    );

    let synth = handle_for(&platform, "Synth");
    assert!(
        eventually(|| !platform.sent_sysex(synth).is_empty()).await,
        "the dump did not reach the destination"
    );

    let counters = daemon.all_counters().await;
    let total_received: u64 = counters.iter().map(|(_, c)| c.messages_received).sum();
    let total_bytes: u64 = counters.iter().map(|(_, c)| c.bytes_received).sum();

    assert_eq!(total_received, 1, "the dump was not counted as one message");
    assert_eq!(
        total_bytes,
        INQUIRY.len() as u64,
        "the dump was not counted by its size"
    );
}

/// Starts a daemon whose sessions are not announced, for tests that run two over loopback.
async fn machine(label: &str) -> (Arc<Daemon>, Arc<FakeMidiPlatform>) {
    let root =
        common::scratch("midi-harbor-sysex").join(format!("{label}-{}", uuid::Uuid::new_v4()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        common::quiet(root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("the daemon starts over a scratch directory");
    (daemon, platform)
}

/// Proves that a dump longer than one RTP-MIDI packet crosses a network session whole, and that
/// a note sent after it does not overtake it.
///
/// Sessions counted every dump as undelivered, and one arriving from a peer made the whole
/// packet unreadable, the notes beside it included. The dump is 3000 data bytes plus its start
/// and end bytes, 3002 in all, which is three packets' worth.
#[tokio::test]
async fn a_long_dump_crosses_a_network_session_whole_and_in_order() {
    let (far, far_platform) = machine("far").await;
    far.create_virtual_port("Synth", 1, 1)
        .await
        .expect("the far machine's Synth port is created");
    let stage = far
        .create_network_session("Stage", 0, InvitationPolicy::AcceptAll)
        .await
        .expect("the far machine's session is created");
    far.create_route("Stage", "Synth")
        .await
        .expect("the route from Stage to Synth is created");
    let port = far
        .session_status(stage.id)
        .await
        .expect("the far session reports its status")
        .control_port;

    let (near, near_platform) = machine("near").await;
    near.create_virtual_port("Keys", 1, 1)
        .await
        .expect("the near machine's Keys port is created");
    let link = near
        .create_network_session("Link", 0, InvitationPolicy::Prompt)
        .await
        .expect("the near machine's session is created");
    near.create_route("Keys", "Link")
        .await
        .expect("the route from Keys to Link is created");
    near.connect_peer(link.id, SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("the near session invites the far one");
    let mut up = false;
    for _ in 0..100 {
        let phase = near.session_status(link.id).await.map(|s| s.state.phase());
        if phase == Some(ConnectionPhase::Connected) {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(up, "the session never connected");

    let mut dump = vec![0xF0];
    dump.extend((0..3000).map(|index: u32| u8::try_from(index % 128).expect("below 128")));
    dump.push(0xF7);
    let mut played = dump.clone();
    played.extend_from_slice(&[0x90, 0x3C, 0x64]);
    assert!(
        near_platform.feed_bytes(handle_for(&near_platform, "Keys"), &played),
        "the fake accepts the dump and the note"
    );

    let synth = handle_for(&far_platform, "Synth");
    let arrived = eventually(|| far_platform.outgoing(synth).len() >= 2).await;
    assert!(
        arrived,
        "the dump and the note did not both cross the session: {:?}",
        far_platform.outgoing(synth)
    );
    let outgoing = far_platform.outgoing(synth);
    assert_eq!(
        outgoing.first(),
        Some(&Outgoing::SysEx(dump)),
        "the dump must arrive first and whole"
    );
    assert!(
        matches!(
            outgoing.get(1),
            Some(Outgoing::Message(
                midi_harbor_core::midi::MidiMessage::NoteOn { note: 0x3C, .. }
            ))
        ),
        "the note must arrive after the dump rather than overtake it: {outgoing:?}"
    );
}
