//! Packet loss injection: the test the recovery journal exists to pass.
//!
//! Reconnecting after a drop is not enough. If a note off is lost, the note sounds forever; if a
//! controller change is lost, the receiver is left at a stale value. These tests induce real loss
//! and assert that neither happens.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_rtpmidi::journal::{Continuity, JournalState, RecoveryJournal, SequenceTracker};
use midi_harbor_rtpmidi::{RtpMidiPacket, TimedMessage};
use std::collections::BTreeMap;

/// A deterministic pseudo-random source, so a failure can be reproduced exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64, chosen for being short and reproducible rather than for quality.
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Returns true with roughly the given percentage chance.
    fn hits(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// What one side believes is currently true of the MIDI stream.
#[derive(Debug, Default, PartialEq, Eq)]
struct MidiState {
    /// Velocity per sounding note, keyed by channel and note.
    sounding: BTreeMap<(u8, u8), u8>,
    /// Value per controller, keyed by channel and controller number.
    controllers: BTreeMap<(u8, u8), u8>,
    /// Program per channel.
    programs: BTreeMap<u8, u8>,
    /// Pitch wheel per channel.
    wheels: BTreeMap<u8, u16>,
}

impl MidiState {
    /// Applies a message, updating what is sounding and what the controllers read.
    fn apply(&mut self, message: &MidiMessage) {
        let Some(channel) = message.channel().map(|c| c.index()) else {
            return;
        };
        match message {
            MidiMessage::NoteOn { note, velocity, .. } if *velocity > 0 => {
                let _ = self.sounding.insert((channel, *note), *velocity);
            }
            MidiMessage::NoteOn { note, .. } | MidiMessage::NoteOff { note, .. } => {
                let _ = self.sounding.remove(&(channel, *note));
            }
            MidiMessage::ControlChange {
                controller, value, ..
            } => {
                let _ = self.controllers.insert((channel, *controller), *value);
            }
            MidiMessage::ProgramChange { program, .. } => {
                let _ = self.programs.insert(channel, *program);
            }
            MidiMessage::PitchBend { value, .. } => {
                let _ = self.wheels.insert(channel, *value);
            }
            _ => {}
        }
    }

    /// Returns the notes left sounding, which is what a listener would actually hear.
    #[allow(clippy::doc_markdown)]
    fn stuck_notes(&self) -> Vec<(u8, u8)> {
        self.sounding.keys().copied().collect()
    }
}

/// How many journal-carrying packets a sender emits after its last message.
///
/// The journal only repairs when a later packet arrives, so a sender that falls silent
/// immediately after its final message leaves a lost note off unrepairable. A real sender keeps
/// emitting while state is unacknowledged, and this models that.
const TRAILING_PACKETS: u32 = 8;

/// Runs a stream of messages through a lossy link and returns both sides' final state.
///
/// `loss_percent` is applied per packet. Every packet carries a journal built from what the
/// sender has sent, and the receiver applies a journal whenever it notices a gap.
fn run(messages: Vec<MidiMessage>, loss_percent: u64, seed: u64) -> (MidiState, MidiState, u64) {
    let mut rng = Rng(seed);
    let mut sender_state = MidiState::default();
    let mut receiver_state = MidiState::default();
    let mut journal = JournalState::new();
    let mut tracker = SequenceTracker::new();

    // RTP streams start at a random sequence number, so this one does too.
    let mut sequence = u16::try_from(seed % 60_000).unwrap_or(0);
    let mut recovered_total = 0u64;

    // Each message is sent in its own packet, then the sender keeps emitting journal-only
    // packets so the final messages are protected like any others.
    let carried: Vec<Option<MidiMessage>> = messages
        .into_iter()
        .map(Some)
        .chain((0..TRAILING_PACKETS).map(|_| None))
        .collect();

    for carried_message in carried {
        sequence = sequence.wrapping_add(1);
        if let Some(message) = &carried_message {
            sender_state.apply(message);
            journal.observe(message, sequence);
        }

        let mut packet = RtpMidiPacket::new(
            sequence,
            u32::from(sequence).saturating_mul(480),
            0x1234,
            carried_message
                .map(|m| vec![TimedMessage::immediate(m)])
                .unwrap_or_default(),
        );
        packet.journal = journal.build().map(|j| j.encode());

        // Encode and decode every packet, so the wire format is exercised rather than bypassed.
        let wire = packet.encode();
        if rng.hits(loss_percent) {
            continue;
        }
        let Ok(received) = RtpMidiPacket::parse(&wire) else {
            continue;
        };

        // A gap means packets were missed, so rebuild from the journal before going on.
        if let Continuity::Gap { .. } = tracker.observe(received.sequence)
            && let Some(bytes) = &received.journal
            && let Ok(decoded) = RecoveryJournal::decode(bytes)
        {
            let repairs = decoded.recover();
            recovered_total = recovered_total.saturating_add(repairs.len() as u64);
            for repair in repairs {
                receiver_state.apply(&repair);
            }
        }

        for timed in &received.messages {
            receiver_state.apply(&timed.message);
        }

        // The receiver tells the sender what it has, which is what trims the journal.
        if let Some(highest) = tracker.highest() {
            journal.trim(highest);
        }
    }

    (sender_state, receiver_state, recovered_total)
}

/// Builds a phrase: notes played and released, with controller movement in between.
fn phrase(rounds: u32) -> Vec<MidiMessage> {
    let channel = Channel::new(0).expect("channel 0");
    let mut messages = Vec::new();

    for round in 0..rounds {
        let note = u8::try_from(48 + (round % 24)).unwrap_or(60);
        messages.push(MidiMessage::NoteOn {
            channel,
            note,
            velocity: 100,
        });
        messages.push(MidiMessage::ControlChange {
            channel,
            controller: 7,
            value: u8::try_from(round % 128).unwrap_or(0),
        });
        messages.push(MidiMessage::PitchBend {
            channel,
            value: u16::try_from((round * 97) % 16_384).unwrap_or(8192),
        });
        messages.push(MidiMessage::NoteOff {
            channel,
            note,
            velocity: 0,
        });
    }
    messages
}

/// Builds a note held from the start while other notes come and go around it.
fn held_under_traffic() -> Vec<MidiMessage> {
    let channel = Channel::new(0).expect("channel 1 is in range");
    let mut messages = vec![MidiMessage::NoteOn {
        channel,
        note: 60,
        velocity: 100,
    }];
    // Traffic on other notes, which is what gets lost.
    for round in 0..200u32 {
        let note = u8::try_from(70 + (round % 20)).unwrap_or(70);
        messages.push(MidiMessage::NoteOn {
            channel,
            note,
            velocity: 80,
        });
        messages.push(MidiMessage::NoteOff {
            channel,
            note,
            velocity: 0,
        });
    }
    messages
}

/// Builds notes and a controller moving on four channels at once.
fn four_channels() -> Vec<MidiMessage> {
    let mut messages = Vec::new();
    for round in 0..150u32 {
        for channel_index in [0u8, 3, 9, 15] {
            let channel = Channel::new(channel_index).expect("channels below 16 are in range");
            let note = u8::try_from(40 + (round % 30)).unwrap_or(60);
            messages.push(MidiMessage::NoteOn {
                channel,
                note,
                velocity: 90,
            });
            messages.push(MidiMessage::ControlChange {
                channel,
                controller: 74,
                value: u8::try_from(round % 128).unwrap_or(0),
            });
            messages.push(MidiMessage::NoteOff {
                channel,
                note,
                velocity: 0,
            });
        }
    }
    messages
}

/// However many packets a link loses, the receiver ends where the sender is: no note left
/// sounding that the sender released, no note dropped that the sender still holds, and every
/// controller and pitch wheel at the sender's value.
///
/// The lossless row is the control case: if it fails, a loss result means nothing. SC-006 sets
/// 5% loss as the bar, run over twenty seeds; 20%, 40% and 60% are well beyond anything a working
/// network produces, and show the journal is not merely masking a low loss rate. The held note
/// is the opposite failure to a stuck one, and four channels at once show each channel journal
/// is kept apart from the others.
#[test]
fn a_lossy_link_leaves_the_receiver_where_the_sender_is() {
    let cases: [(&str, Vec<MidiMessage>, u64, Vec<u64>); 7] = [
        ("a lossless link", phrase(200), 0, vec![1]),
        ("SC-006's 5% loss", phrase(300), 5, (1..=20).collect()),
        ("20% loss", phrase(400), 20, vec![7]),
        ("40% loss", phrase(400), 40, vec![11]),
        ("60% loss", phrase(400), 60, vec![13]),
        (
            "a note held through 30% loss",
            held_under_traffic(),
            30,
            vec![99],
        ),
        (
            "four channels through 15% loss",
            four_channels(),
            15,
            vec![4242],
        ),
    ];
    for (name, messages, loss, seeds) in cases {
        for seed in seeds {
            let (sender, receiver, _) = run(messages.clone(), loss, seed);
            assert_eq!(
                receiver, sender,
                "{name}, seed {seed}: the receiver must end in the sender's state"
            );
        }
    }
}

/// The journal is what keeps notes from sticking, not a kind loss pattern.
///
/// The same stream through the same 30% loss pattern, replayed with recovery disabled, strands
/// notes; with the journal it strands none and recovers messages. Without this, a pass above
/// could be the loss pattern happening to drop no note off.
#[test]
fn the_journal_actually_does_the_work() {
    let messages = phrase(300);
    let (_, with_journal, recovered) = run(messages.clone(), 30, 5);
    assert!(
        recovered > 0,
        "no messages were recovered, so the journal was never exercised"
    );
    assert!(
        with_journal.stuck_notes().is_empty(),
        "with the journal, no note may be left sounding"
    );

    // Replay the identical loss pattern with recovery disabled, including the trailing packets
    // so the two runs see the same sequence of coin flips.
    let mut rng = Rng(5);
    let mut naive = MidiState::default();
    for message in messages
        .iter()
        .map(Some)
        .chain((0..TRAILING_PACKETS).map(|_| None))
    {
        let dropped = rng.hits(30);
        if let (false, Some(message)) = (dropped, message) {
            naive.apply(message);
        }
    }
    assert!(
        !naive.stuck_notes().is_empty(),
        "the loss pattern strands no notes even without recovery, so it proves nothing"
    );
}

/// The journal stays bounded over a long session whose receiver acknowledges as it goes.
///
/// SC-007: the journal must not grow for the life of a session. Twenty thousand notes played and
/// released, `2 * 20_000 = 40_000` packets acknowledged four behind, pass over every note number
/// about 156 times, and the largest journal stays under 512 octets.
#[test]
fn a_long_session_keeps_the_journal_bounded() {
    let channel = Channel::new(0).expect("channel 0");
    let mut journal = JournalState::new();
    let mut sequence = 0u16;
    let mut largest = 0usize;

    for round in 0..20_000u32 {
        let note = u8::try_from(round % 128).unwrap_or(0);
        sequence = sequence.wrapping_add(1);
        journal.observe(
            &MidiMessage::NoteOn {
                channel,
                note,
                velocity: 100,
            },
            sequence,
        );
        sequence = sequence.wrapping_add(1);
        journal.observe(
            &MidiMessage::NoteOff {
                channel,
                note,
                velocity: 0,
            },
            sequence,
        );

        if let Some(built) = journal.build() {
            largest = largest.max(built.encode().len());
        }
        // The receiver acknowledges, a little behind, as a real one would.
        journal.trim(sequence.wrapping_sub(4));
    }

    assert!(
        largest > 0,
        "the journal was never populated, so the bound proves nothing"
    );
    assert!(
        largest < 512,
        "the journal grew to {largest} octets over a long session"
    );
}
