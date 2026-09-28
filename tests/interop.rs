//! Interoperability against recorded sessions with other RTP-MIDI implementations (FR-011,
//! SC-011).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use midi_harbor_core::endpoint::InvitationPolicy;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::paths::Paths;
use midi_harbor_core::state::ConnectionPhase;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::MidiPlatform;
use midi_harbor_rtpmidi::journal::RecoveryJournal;
use midi_harbor_rtpmidi::{ControlPacket, Handshake, RtpMidiPacket};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// What each side plays during a recording: one of each channel voice message, several in one
/// go, then system messages. Apple's side of the capture plays the same.
const SCRIPT: &[&[u8]] = &[
    &[0x90, 60, 100],
    &[0x80, 60, 0],
    &[0x99, 36, 127],
    &[0xB0, 7, 90],
    &[0xC3, 12],
    &[0xE0, 0x00, 0x60],
    &[0xD5, 64],
    &[0xA2, 62, 30],
    &[0x90, 64, 80, 0x90, 67, 80, 0x90, 71, 80],
    &[0x80, 64, 0, 0x80, 67, 0, 0x80, 71, 0, 0x89, 36, 0],
    &[0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7],
    &[0xF8],
    &[0xB0, 123, 0],
];

/// Returns the datagrams in a recording, each with the name its line gives it.
fn recorded(text: &str) -> Vec<(&str, Vec<u8>)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (name, hex) = line.split_once(' ').expect("a name and bytes");
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex"))
                .collect();
            (name, bytes)
        })
        .collect()
}

/// Builds a note on at velocity 64 on MIDI channel 1, as Apple's side of the recordings plays.
fn note_on(note: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        channel: Channel::new(0).expect("channel 1"),
        note,
        velocity: 64,
    }
}

/// Locks that every datagram Apple's Network MIDI sent, when we invited it, parses, and that its
/// recovery journals decode to what was played before them.
///
/// Recorded on macOS 15 against "Session 1" (R-065). Apple clears the Y bit on every note it
/// logs, which RFC 6295 (A.6) makes a recommendation to skip the note on rather than play it
/// late, so recovering from these journals must sound nothing.
#[test]
fn everything_apple_sent_is_understood() {
    let recording = recorded(include_str!("interop/apple_network_midi.txt"));
    let mut notes = Vec::new();
    let mut journals = Vec::new();

    for (name, bytes) in &recording {
        match *name {
            "midi" => {
                let packet = RtpMidiPacket::parse(bytes).expect("apple's midi must parse");
                notes.extend(packet.messages.iter().map(|timed| timed.message));
                if let Some(journal) = &packet.journal {
                    journals.push(
                        RecoveryJournal::decode(journal).expect("apple's journal must decode"),
                    );
                }
            }
            control => {
                let packet =
                    ControlPacket::parse(bytes).expect("apple's control packet must parse");
                let command = match packet {
                    ControlPacket::Session { command, .. } => Some(command),
                    _ => None,
                };
                let want = match control {
                    "accepted" => Some(Handshake::Accepted),
                    "rejected" => Some(Handshake::Rejected),
                    "ended" => Some(Handshake::EndSession),
                    _ => None,
                };
                match want {
                    Some(want) => assert_eq!(
                        command,
                        Some(want),
                        "apple's {control} packet must be read as {want:?}"
                    ),
                    None => assert!(
                        matches!(packet, ControlPacket::ClockSync { .. }),
                        "apple's {control} packet must be a clock exchange"
                    ),
                }
            }
        }
    }

    assert_eq!(
        notes,
        vec![note_on(60), note_on(62), note_on(64)],
        "apple's messages must decode to the notes it played"
    );

    // Each journal logs the notes sent before its packet.
    let logged: Vec<Vec<(u8, u8, bool)>> = journals
        .iter()
        .map(|journal| {
            journal
                .channels
                .iter()
                .filter_map(|channel| channel.notes.as_ref())
                .flat_map(|chapter| chapter.notes.iter())
                .map(|log| (log.note, log.velocity, log.play))
                .collect()
        })
        .collect();
    assert_eq!(
        logged,
        vec![
            vec![(60, 64, false)],
            vec![(60, 64, false), (62, 64, false)]
        ],
        "each journal must log the notes sent before its packet, none marked to be played"
    );
    assert!(
        journals.iter().all(|journal| journal.recover().is_empty()),
        "recovering notes apple marked not to play must sound nothing"
    );
}

/// Locks that every datagram Apple's Network MIDI sent, when it invited us, parses.
///
/// Recorded on macOS 15 from "Session 1" in Audio MIDI Setup's directory (R-068). As initiator
/// Apple opens and closes the clock exchange itself, and its invitation carries the Mac's name
/// with a typographic apostrophe, which must survive as UTF-8.
#[test]
fn apple_inviting_us_is_understood() {
    let recording = recorded(include_str!("interop/apple_network_midi_inviting.txt"));
    let mut messages = Vec::new();
    let mut journals = Vec::new();

    for (name, bytes) in &recording {
        if *name == "midi" {
            let packet = RtpMidiPacket::parse(bytes).expect("apple's midi must parse");
            messages.extend(packet.messages.iter().map(|timed| timed.message));
            if let Some(journal) = &packet.journal {
                journals
                    .push(RecoveryJournal::decode(journal).expect("apple's journal must decode"));
            }
            continue;
        }
        let packet = ControlPacket::parse(bytes).expect("apple's control packet must parse");
        match *name {
            "invited" => match packet {
                ControlPacket::Session {
                    command: Handshake::Invitation,
                    name,
                    ..
                } => assert_eq!(
                    name.as_deref(),
                    Some("Studio Mac"),
                    "the invitation must carry the Mac's name intact"
                ),
                other => panic!("expected an invitation, got {other:?}"),
            },
            "clock-open" | "clock-close" => {
                let ControlPacket::ClockSync { count, .. } = packet else {
                    panic!("expected a clock exchange, got {packet:?}");
                };
                let want = if *name == "clock-open" { 0 } else { 2 };
                assert_eq!(
                    count, want,
                    "apple's {name} must be clock exchange step {want}"
                );
            }
            "feedback" => assert_eq!(
                packet,
                ControlPacket::feedback(0x3719_3C27, 0xFDE5),
                "apple's feedback must acknowledge sequence 0xFDE5 in the upper half"
            ),
            other => panic!("unexpected line {other}"),
        }
    }

    let note_off = |note| MidiMessage::NoteOff {
        channel: Channel::new(0).expect("channel 1"),
        note,
        velocity: 64,
    };
    assert_eq!(
        messages,
        vec![
            note_on(60),
            note_off(60),
            note_on(62),
            note_off(62),
            note_on(64),
            note_off(64),
        ],
        "apple's messages must decode to the notes it played"
    );
    assert_eq!(
        journals.len(),
        3,
        "each of apple's three note offs must carry a journal of the note on before it"
    );
    assert!(
        journals.iter().all(|journal| journal.recover().is_empty()),
        "recovering notes apple marked not to play must sound nothing"
    );
}

/// Locks that every datagram rtpmidid and rtpMIDI sent in a recorded session parses, and carries
/// the script both played.
///
/// Recorded from rtpmidid at 7f552d2 on Arch Linux (R-071) and rtpMIDI 1.1.14.247 on Windows 11
/// (R-073). Neither sends a recovery journal. They disagree on receiver feedback: rtpmidid puts
/// the acknowledged sequence number in the lower 16 bits, where Apple and rtpMIDI put it in the
/// upper, so each row names the acknowledgements its peer sent as raw 32-bit values.
#[test]
fn everything_rtpmidid_and_rtpmidi_sent_is_understood() {
    let cases: [(&str, &str, &str, &str, &[u32]); 2] = [
        (
            "rtpmidid",
            include_str!("interop/rtpmidid.txt"),
            "Linux rtp",
            "aseqdump",
            &[0x0000_4711, 0x0000_FB8F],
        ),
        (
            "rtpMIDI",
            include_str!("interop/rtpmidi.txt"),
            "WINDOWS-PC",
            "WINDOWS-PC",
            // Sequence 0x28A3 in the upper half: 0x28A3 << 16.
            &[0x28A3_0000],
        ),
    ];

    let channel = |number| Channel::new(number).expect("a channel");
    let on = |note| MidiMessage::NoteOn {
        channel: channel(0),
        note,
        velocity: 0x64,
    };
    let off = |note| MidiMessage::NoteOff {
        channel: channel(0),
        note,
        velocity: 0x40,
    };
    let control = |controller, value| MidiMessage::ControlChange {
        channel: channel(0),
        controller,
        value,
    };
    let bend = |value| MidiMessage::PitchBend {
        channel: channel(0),
        value,
    };
    let played = vec![
        on(0x30),
        off(0x30),
        on(0x32),
        control(0x07, 0x5A),
        MidiMessage::ProgramChange {
            channel: channel(3),
            program: 0x0C,
        },
        bend(0x60 << 7),
        off(0x32),
        on(0x34),
        off(0x34),
        control(0x40, 0x7F),
        control(0x40, 0x00),
        bend(0x20 << 7),
    ];
    let dumped = vec![vec![0x7E, 0x7F, 0x06, 0x01], vec![0x7E, 0x7F, 0x06, 0x02]];

    for (peer, recording, accepted_by, invited_by, acknowledgements) in cases {
        let mut messages = Vec::new();
        let mut dumps = Vec::new();
        for (name, bytes) in &recorded(recording) {
            if *name == "midi" {
                let packet = RtpMidiPacket::parse(bytes)
                    .unwrap_or_else(|error| panic!("{peer}'s midi must parse: {error}"));
                assert!(
                    packet.journal.is_none(),
                    "{peer} sends no journal, so none may be read into its packets"
                );
                messages.extend(packet.messages.iter().map(|timed| timed.message));
                dumps.extend(packet.sysex.iter().map(|segment| segment.payload.clone()));
                continue;
            }
            let packet = ControlPacket::parse(bytes)
                .unwrap_or_else(|error| panic!("{peer}'s {name} packet must parse: {error}"));
            match (*name, &packet) {
                (
                    "accepted" | "invited",
                    ControlPacket::Session {
                        command,
                        name: from,
                        ..
                    },
                ) => {
                    let (want_command, want_name) = if *name == "accepted" {
                        (Handshake::Accepted, accepted_by)
                    } else {
                        (Handshake::Invitation, invited_by)
                    };
                    assert_eq!(
                        *command, want_command,
                        "{peer}'s {name} packet must be read as {want_command:?}"
                    );
                    assert_eq!(
                        from.as_deref(),
                        Some(want_name),
                        "{peer}'s {name} packet must carry the name it sent"
                    );
                }
                ("ended", ControlPacket::Session { command, .. }) => {
                    assert_eq!(
                        *command,
                        Handshake::EndSession,
                        "{peer}'s goodbye must be read as ending the session"
                    );
                }
                (
                    "clock-open" | "clock-answer" | "clock-close",
                    ControlPacket::ClockSync { count, .. },
                ) => {
                    let want = match *name {
                        "clock-open" => 0,
                        "clock-answer" => 1,
                        _ => 2,
                    };
                    assert_eq!(
                        *count, want,
                        "{peer}'s {name} must be clock exchange step {want}"
                    );
                }
                ("feedback", ControlPacket::ReceiverFeedback { acknowledged, .. }) => {
                    assert!(
                        acknowledgements.contains(acknowledged),
                        "{peer} must acknowledge a sequence it sent, not {acknowledged:#010x}"
                    );
                }
                (name, packet) => panic!("{peer}'s recording has an unexpected {name}: {packet:?}"),
            }
        }
        assert_eq!(
            messages, played,
            "{peer}'s messages must decode to the script it played"
        );
        assert_eq!(
            dumps, dumped,
            "{peer}'s system exclusive dumps must decode whole"
        );
    }
}

/// Plays our side of a recording against a peer, by port on this machine or by address.
///
/// ```text
/// HARBOR_RECORD_PEER=5004 cargo test --test interop record -- --ignored --nocapture
/// HARBOR_RECORD_PEER=192.0.2.10:5004 cargo test --test interop record -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "drives a recording; needs a peer to connect to"]
async fn record_a_session() {
    let named = std::env::var("HARBOR_RECORD_PEER").expect("HARBOR_RECORD_PEER");
    let peer: SocketAddr = named
        .parse()
        .or_else(|_| {
            named
                .parse::<u16>()
                .map(|port| SocketAddr::from(([127, 0, 0, 1], port)))
        })
        .expect("a port or an address");
    let root = std::env::temp_dir().join(format!("mh-interop-{}", std::process::id()));
    let platform = Arc::new(FakeMidiPlatform::new());
    let daemon = Daemon::start(
        Paths::rooted_at(&root),
        Arc::clone(&platform) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("a daemon");
    for name in ["Keys", "Synth"] {
        daemon.create_virtual_port(name, 1, 1).await.unwrap();
    }
    let session = daemon
        .create_network_session("Harbor Interop", 0, InvitationPolicy::Prompt)
        .await
        .unwrap();
    daemon.create_route("Keys", "Harbor Interop").await.unwrap();
    daemon
        .create_route("Harbor Interop", "Synth")
        .await
        .unwrap();
    daemon.connect_peer(session.id, peer).await.unwrap();
    let mut connected = false;
    for _ in 0..100 {
        let phase = daemon
            .session_status(session.id)
            .await
            .map(|status| status.state.phase());
        if phase == Some(ConnectionPhase::Connected) {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(connected, "the session must connect to the peer at {peer}");
    println!("CONNECTED");

    // The peer plays first.
    tokio::time::sleep(Duration::from_millis(4_500)).await;
    let keys = platform.port_handle("Keys").unwrap();
    for step in SCRIPT {
        assert!(platform.feed_bytes(keys, step));
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;

    let synth = platform.port_handle("Synth").unwrap();
    for outgoing in platform.outgoing(synth) {
        println!("RECEIVED {outgoing:?}");
    }
    let status = daemon.session_status(session.id).await.unwrap();
    println!(
        "lost {} recovered {} round trip {:?}",
        status.lost, status.recovered, status.round_trip
    );
    daemon.set_enabled(session.id, false).await.unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
