//! Building a journal from what was sent, and detecting what was lost.
//!
//! The sender keeps the current state of every channel alongside the sequence number at which
//! each piece of it last changed. A journal then carries only what changed after the checkpoint
//! the receiver has acknowledged, which is what stops it growing without bound on a long session.

use super::chapters::{ChapterC, ChapterN, ChapterP, ChapterT, ChapterW, ControllerLog, NoteLog};
use super::{ChannelJournal, RecoveryJournal};
use midi_harbor_core::midi::{CHANNELS, Channel, MidiMessage};

/// Note numbers in MIDI 1.0.
const NOTES: usize = 128;

/// Controller numbers in MIDI 1.0.
const CONTROLLERS: usize = 128;

/// Bank select most significant controller number.
const CC_BANK_MSB: u8 = 0;
/// Bank select least significant controller number.
const CC_BANK_LSB: u8 = 32;

/// Reports whether `a` is at or after `b` in sequence-number space.
///
/// Sequence numbers wrap at 16 bits, so a plain comparison would treat the wrap as the receiver
/// falling 65,000 packets behind and hold the journal forever.
fn at_or_after(a: u16, b: u16) -> bool {
    a.wrapping_sub(b) < 0x8000
}

/// What one channel has most recently sent, with when each part of it changed.
#[derive(Debug, Clone)]
struct ChannelState {
    /// Velocity per sounding note, and the sequence that started it.
    sounding: [Option<(u8, u16)>; NOTES],
    /// Sequence at which each note was released, for notes released and not restarted.
    released: [Option<u16>; NOTES],
    /// Current value per controller, and the sequence that set it.
    controllers: [Option<(u8, u16)>; CONTROLLERS],
    /// Current program and the sequence that set it.
    program: Option<(u8, u16)>,
    /// Current wheel position and the sequence that set it.
    wheel: Option<(u16, u16)>,
    /// Current channel pressure and the sequence that set it.
    pressure: Option<(u8, u16)>,
}

impl Default for ChannelState {
    fn default() -> Self {
        Self {
            sounding: [None; NOTES],
            released: [None; NOTES],
            controllers: [None; CONTROLLERS],
            program: None,
            wheel: None,
            pressure: None,
        }
    }
}

impl ChannelState {
    /// Reports whether anything has ever been sent on this channel.
    fn is_untouched(&self) -> bool {
        self.program.is_none()
            && self.wheel.is_none()
            && self.pressure.is_none()
            && self.sounding.iter().all(Option::is_none)
            && self.released.iter().all(Option::is_none)
            && self.controllers.iter().all(Option::is_none)
    }
}

/// Tracks what has been sent, so a journal can be built for any checkpoint.
#[derive(Debug, Clone)]
pub struct JournalState {
    channels: Vec<ChannelState>,
    /// The sequence the journal's history starts from.
    ///
    /// Unset until the first packet is sent, because RTP streams start at a random sequence
    /// number. Assuming zero would make every acknowledgement look stale on a session that
    /// happened to start high, and the journal would then never be trimmed.
    checkpoint: Option<u16>,
    highest_sent: Option<u16>,
}

impl Default for JournalState {
    fn default() -> Self {
        Self::new()
    }
}

impl JournalState {
    /// Creates state with nothing recorded.
    pub fn new() -> Self {
        Self {
            channels: vec![ChannelState::default(); usize::from(CHANNELS)],
            checkpoint: None,
            highest_sent: None,
        }
    }

    /// Returns the sequence number the journal's history currently starts from.
    ///
    /// Zero before anything has been sent, which is the value a journal would carry anyway.
    pub fn checkpoint(&self) -> u16 {
        self.checkpoint.unwrap_or(0)
    }

    /// Reports whether `sequence` names a packet sent since the checkpoint, so that an
    /// acknowledgement of it can trim the journal.
    pub fn acknowledges(&self, sequence: u16) -> bool {
        match (self.checkpoint, self.highest_sent) {
            (Some(checkpoint), Some(highest)) => {
                at_or_after(sequence, checkpoint) && at_or_after(highest, sequence)
            }
            _ => false,
        }
    }

    /// Records a message being sent in the packet with the given sequence number.
    pub fn observe(&mut self, message: &MidiMessage, sequence: u16) {
        // The first packet establishes where this stream's history begins.
        if self.checkpoint.is_none() {
            self.checkpoint = Some(sequence);
        }
        self.highest_sent = Some(sequence);

        let Some(channel) = message.channel() else {
            return;
        };
        let Some(state) = self.channels.get_mut(usize::from(channel.index())) else {
            return;
        };

        match message {
            // A note on with zero velocity is a release, which is why this is not two arms.
            MidiMessage::NoteOn { note, velocity, .. } if *velocity > 0 => {
                let index = usize::from(*note);
                if let Some(slot) = state.sounding.get_mut(index) {
                    *slot = Some((*velocity, sequence));
                }
                if let Some(slot) = state.released.get_mut(index) {
                    *slot = None;
                }
            }
            MidiMessage::NoteOn { note, .. } | MidiMessage::NoteOff { note, .. } => {
                let index = usize::from(*note);
                if let Some(slot) = state.sounding.get_mut(index) {
                    *slot = None;
                }
                if let Some(slot) = state.released.get_mut(index) {
                    *slot = Some(sequence);
                }
            }
            MidiMessage::ControlChange {
                controller, value, ..
            } => {
                if let Some(slot) = state.controllers.get_mut(usize::from(*controller)) {
                    *slot = Some((*value, sequence));
                }
            }
            MidiMessage::ProgramChange { program, .. } => {
                state.program = Some((*program, sequence));
            }
            MidiMessage::PitchBend { value, .. } => {
                state.wheel = Some((*value, sequence));
            }
            MidiMessage::ChannelAftertouch { pressure, .. } => {
                state.pressure = Some((*pressure, sequence));
            }
            // Poly pressure and system messages are outside the chapters this implementation
            // carries, so they are not protected.
            MidiMessage::PolyAftertouch { .. }
            | MidiMessage::SystemCommon { .. }
            | MidiMessage::System { .. } => {}
        }
    }

    /// Moves the checkpoint forward to just past what the receiver has acknowledged.
    ///
    /// Everything the receiver has confirmed no longer needs protecting, so the state recorded
    /// before it is dropped. Without this the journal grows for the life of the session.
    pub fn trim(&mut self, acknowledged: u16) {
        // An acknowledgement before the current checkpoint is a stale or duplicate report and
        // must not move the history backwards.
        if self
            .checkpoint
            .is_some_and(|current| !at_or_after(acknowledged, current))
        {
            return;
        }
        self.checkpoint = Some(acknowledged.wrapping_add(1));

        // A released note confirmed by the receiver needs no further protection. A sounding note
        // is kept whatever its age, because it is still sounding and must be recoverable.
        for channel in &mut self.channels {
            for slot in &mut channel.released {
                if slot.is_some_and(|seq| at_or_after(acknowledged, seq)) {
                    *slot = None;
                }
            }
        }
    }

    /// Builds the journal to attach to an outgoing packet.
    ///
    /// Returns `None` when nothing needs protecting, so an idle session sends no journal at all.
    pub fn build(&self) -> Option<RecoveryJournal> {
        let mut channels = Vec::new();

        for (index, state) in self.channels.iter().enumerate() {
            if state.is_untouched() {
                continue;
            }
            let Some(channel) = Channel::new(u8::try_from(index).unwrap_or(0)) else {
                continue;
            };
            if let Some(journal) = self.build_channel(channel, state) {
                channels.push(journal);
            }
        }

        if channels.is_empty() {
            return None;
        }
        Some(RecoveryJournal {
            checkpoint_seqnum: self.checkpoint(),
            single_packet_loss: false,
            channels,
        })
    }

    /// Builds one channel's journal, carrying only what changed after the checkpoint.
    fn build_channel(&self, channel: Channel, state: &ChannelState) -> Option<ChannelJournal> {
        // Include a value only when it changed at or after the checkpoint, except for sounding
        // notes: a note held since before the checkpoint is still sounding and must be protected.
        let checkpoint = self.checkpoint;
        let after_checkpoint =
            |sequence: u16| checkpoint.is_none_or(|start| at_or_after(sequence, start));

        let notes: Vec<NoteLog> = state
            .sounding
            .iter()
            .enumerate()
            .filter_map(|(note, slot)| {
                let (velocity, _) = (*slot)?;
                Some(NoteLog {
                    note: u8::try_from(note).unwrap_or(0),
                    velocity: velocity.max(1),
                    play: true,
                })
            })
            .collect();

        let released: Vec<u8> = state
            .released
            .iter()
            .enumerate()
            .filter_map(|(note, slot)| {
                let sequence = (*slot)?;
                after_checkpoint(sequence).then(|| u8::try_from(note).unwrap_or(0))
            })
            .collect();

        let controls: Vec<ControllerLog> = state
            .controllers
            .iter()
            .enumerate()
            .filter_map(|(number, slot)| {
                let (value, sequence) = (*slot)?;
                // Bank select is carried by chapter P, so it is not duplicated here.
                let number = u8::try_from(number).unwrap_or(0);
                if number == CC_BANK_MSB || number == CC_BANK_LSB {
                    return None;
                }
                after_checkpoint(sequence).then_some(ControllerLog { number, value })
            })
            .collect();

        let program = state.program.and_then(|(program, sequence)| {
            after_checkpoint(sequence).then(|| ChapterP {
                program,
                bank_msb: state
                    .controllers
                    .get(usize::from(CC_BANK_MSB))
                    .and_then(|slot| slot.map(|(value, _)| value)),
                bank_lsb: state
                    .controllers
                    .get(usize::from(CC_BANK_LSB))
                    .and_then(|slot| slot.map(|(value, _)| value)),
            })
        });

        let wheel = state
            .wheel
            .and_then(|(value, sequence)| after_checkpoint(sequence).then_some(ChapterW { value }));
        let pressure = state.pressure.and_then(|(pressure, sequence)| {
            after_checkpoint(sequence).then_some(ChapterT { pressure })
        });

        let notes_chapter = (!notes.is_empty() || !released.is_empty()).then_some(ChapterN {
            notes,
            released,
            bitfield_valid: true,
        });

        let journal = ChannelJournal {
            channel: channel.index(),
            program,
            controls: (!controls.is_empty()).then_some(ChapterC { logs: controls }),
            wheel,
            notes: notes_chapter,
            pressure,
        };
        (!journal.is_empty()).then_some(journal)
    }
}

/// Watches incoming sequence numbers and reports what was lost.
#[derive(Debug, Clone, Default)]
pub struct SequenceTracker {
    highest: Option<u16>,
    lost: u64,
    recovered: u64,
}

/// What a received packet means for the stream's continuity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuity {
    /// The first packet of the stream.
    First,
    /// The packet directly after the previous one.
    InOrder,
    /// Packets were missed, and the journal should be applied.
    Gap {
        /// How many packets were skipped.
        missing: u64,
    },
    /// An older packet arriving late, which carries nothing new.
    Duplicate,
}

impl SequenceTracker {
    /// Creates a tracker that has seen nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the highest sequence number seen, which is what the peer is told.
    pub fn highest(&self) -> Option<u16> {
        self.highest
    }

    /// Returns how many packets have been lost.
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// Returns how many messages have been rebuilt from journals.
    pub fn recovered(&self) -> u64 {
        self.recovered
    }

    /// Records how many messages a journal rebuilt.
    pub fn record_recovered(&mut self, count: u64) {
        self.recovered = self.recovered.saturating_add(count);
    }

    /// Records an arriving sequence number and reports what it means.
    pub fn observe(&mut self, sequence: u16) -> Continuity {
        let Some(highest) = self.highest else {
            self.highest = Some(sequence);
            return Continuity::First;
        };

        // A packet at or before what we have already seen is a late duplicate, not a gap.
        if !at_or_after(sequence, highest.wrapping_add(1)) {
            return Continuity::Duplicate;
        }

        let missing = u64::from(sequence.wrapping_sub(highest).saturating_sub(1));
        self.highest = Some(sequence);

        if missing == 0 {
            Continuity::InOrder
        } else {
            self.lost = self.lost.saturating_add(missing);
            Continuity::Gap { missing }
        }
    }
}

/// One slot per MIDI channel.
const CHANNEL_SLOTS: usize = CHANNELS as usize;

/// Marks a value the receiver has not seen yet.
const UNKNOWN: u8 = 0xFF;

/// Marks a pitch-bend position the receiver has not seen yet; a real one is fourteen bits.
const UNKNOWN_BEND: u16 = u16::MAX;

/// What the receiving side has already delivered, per channel.
///
/// A journal covers everything since the sender's last acknowledged packet, which includes
/// packets that did arrive. Replayed whole, it delivered their messages a second time: across a
/// real network with 5% loss, a synth received the same note on twice in a row 55 times in a
/// minute, which starts a second voice on a synth that stacks them. Recovery now passes on only
/// what changes what was delivered, as RFC 6295 has the receiver do.
#[derive(Debug, Clone)]
pub struct ReceivedState {
    /// Sounding notes, one bit per key, per channel.
    notes: [u128; CHANNEL_SLOTS],
    /// The last value of each controller, per channel.
    controllers: [[u8; 128]; CHANNEL_SLOTS],
    programs: [u8; CHANNEL_SLOTS],
    bends: [u16; CHANNEL_SLOTS],
    pressures: [u8; CHANNEL_SLOTS],
}

impl Default for ReceivedState {
    fn default() -> Self {
        Self {
            notes: [0; CHANNEL_SLOTS],
            controllers: [[UNKNOWN; 128]; CHANNEL_SLOTS],
            programs: [UNKNOWN; CHANNEL_SLOTS],
            bends: [UNKNOWN_BEND; CHANNEL_SLOTS],
            pressures: [UNKNOWN; CHANNEL_SLOTS],
        }
    }
}

impl ReceivedState {
    /// Creates a state in which nothing has been delivered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a message as delivered.
    pub fn observe(&mut self, message: &MidiMessage) {
        match *message {
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
            } if velocity > 0 => self.set_note(channel, note, true),
            MidiMessage::NoteOn { channel, note, .. }
            | MidiMessage::NoteOff { channel, note, .. } => {
                self.set_note(channel, note, false);
            }
            MidiMessage::ControlChange {
                channel,
                controller,
                value,
            } => {
                // All sound off and all notes off leave nothing sounding.
                if (controller == 120 || controller == 123)
                    && let Some(notes) = self.notes.get_mut(usize::from(channel.index()))
                {
                    *notes = 0;
                }
                if let Some(slot) = self
                    .controllers
                    .get_mut(usize::from(channel.index()))
                    .and_then(|controllers| controllers.get_mut(usize::from(controller)))
                {
                    *slot = value;
                }
            }
            MidiMessage::ProgramChange { channel, program } => {
                if let Some(slot) = self.programs.get_mut(usize::from(channel.index())) {
                    *slot = program;
                }
            }
            MidiMessage::PitchBend { channel, value } => {
                if let Some(slot) = self.bends.get_mut(usize::from(channel.index())) {
                    *slot = value;
                }
            }
            MidiMessage::ChannelAftertouch { channel, pressure } => {
                if let Some(slot) = self.pressures.get_mut(usize::from(channel.index())) {
                    *slot = pressure;
                }
            }
            _ => {}
        }
    }

    /// Reports whether a recovered message would change what was delivered.
    ///
    /// A note off always passes: stopping a note that was not sounding does nothing, and one
    /// held back because this state was wrong would leave a note ringing.
    pub fn changes(&self, recovered: &MidiMessage) -> bool {
        match *recovered {
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
            } if velocity > 0 => !self.is_sounding(channel, note),
            MidiMessage::ControlChange {
                channel,
                controller,
                value,
            } => self
                .controllers
                .get(usize::from(channel.index()))
                .and_then(|controllers| controllers.get(usize::from(controller)))
                .is_none_or(|known| *known != value),
            MidiMessage::ProgramChange { channel, program } => self
                .programs
                .get(usize::from(channel.index()))
                .is_none_or(|known| *known != program),
            MidiMessage::PitchBend { channel, value } => self
                .bends
                .get(usize::from(channel.index()))
                .is_none_or(|known| *known != value),
            MidiMessage::ChannelAftertouch { channel, pressure } => self
                .pressures
                .get(usize::from(channel.index()))
                .is_none_or(|known| *known != pressure),
            _ => true,
        }
    }

    fn is_sounding(&self, channel: Channel, note: u8) -> bool {
        let bit = 1u128.checked_shl(u32::from(note)).unwrap_or(0);
        self.notes
            .get(usize::from(channel.index()))
            .is_some_and(|notes| notes & bit != 0)
    }

    fn set_note(&mut self, channel: Channel, note: u8, sounding: bool) {
        let bit = 1u128.checked_shl(u32::from(note)).unwrap_or(0);
        if let Some(notes) = self.notes.get_mut(usize::from(channel.index())) {
            if sounding {
                *notes |= bit;
            } else {
                *notes &= !bit;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One thing that happens to a sender's journal state.
    enum Step {
        /// A message goes out in the packet with this sequence number.
        Send(MidiMessage, u16),
        /// The receiver acknowledges this sequence number.
        Ack(u16),
    }

    fn channel() -> Channel {
        Channel::new(0).expect("channel 1 is in range")
    }

    fn control(controller: u8, value: u8) -> MidiMessage {
        MidiMessage::ControlChange {
            channel: channel(),
            controller,
            value,
        }
    }

    fn note_off(note: u8) -> MidiMessage {
        MidiMessage::NoteOff {
            channel: channel(),
            note,
            velocity: 0,
        }
    }

    /// The journal a sender builds after a run of sends and acknowledgements carries what a
    /// receiver could have missed since the acknowledged checkpoint, and nothing older.
    ///
    /// The end-to-end behaviour, held notes and moving controllers surviving loss on several
    /// channels, is proved in `tests/loss_recovery.rs`. These rows pin what that run never
    /// reaches. A note on at velocity 0 is a note off in MIDI 1.0, so it is journalled as a
    /// release. Bank select travels in chapter P beside the program, never also in chapter C, or
    /// recovery would apply it twice. Sequence numbers are sixteen bits and an RTP stream starts
    /// at a random one, so trimming compares across the wrap (`0x0002` is after `0xFFFE`), starts
    /// from the first packet sent rather than zero, and ignores an acknowledgement older than the
    /// checkpoint. Acknowledging packet `n` moves the checkpoint to `n + 1`.
    #[test]
    fn the_journal_carries_only_what_the_receiver_may_have_missed() {
        struct Case {
            name: &'static str,
            steps: Vec<Step>,
            want_checkpoint: u16,
            want: Option<Vec<ChannelJournal>>,
        }
        let notes = |sounding: &[u8], released: &[u8]| ChannelJournal {
            notes: Some(ChapterN {
                notes: sounding
                    .iter()
                    .map(|note| NoteLog {
                        note: *note,
                        velocity: 100,
                        play: true,
                    })
                    .collect(),
                released: released.to_vec(),
                bitfield_valid: true,
            }),
            ..ChannelJournal::default()
        };
        let note_on = |note: u8, velocity: u8| MidiMessage::NoteOn {
            channel: channel(),
            note,
            velocity,
        };
        let bend = MidiMessage::PitchBend {
            channel: channel(),
            value: 9000,
        };

        let cases = [
            Case {
                name: "a note on at velocity 0",
                steps: vec![
                    Step::Send(note_on(60, 100), 1),
                    Step::Send(note_on(60, 0), 2),
                ],
                want_checkpoint: 1,
                want: Some(vec![notes(&[], &[60])]),
            },
            Case {
                name: "an unacknowledged controller and wheel",
                steps: vec![Step::Send(control(7, 100), 1), Step::Send(bend, 2)],
                want_checkpoint: 1,
                want: Some(vec![ChannelJournal {
                    controls: Some(ChapterC {
                        logs: vec![ControllerLog {
                            number: 7,
                            value: 100,
                        }],
                    }),
                    wheel: Some(ChapterW { value: 9000 }),
                    ..ChannelJournal::default()
                }]),
            },
            Case {
                name: "an acknowledged controller and wheel",
                steps: vec![
                    Step::Send(control(7, 100), 1),
                    Step::Send(bend, 2),
                    Step::Ack(2),
                ],
                want_checkpoint: 3,
                want: None,
            },
            Case {
                name: "bank select before a program change",
                steps: vec![
                    Step::Send(control(CC_BANK_MSB, 1), 1),
                    Step::Send(
                        MidiMessage::ProgramChange {
                            channel: channel(),
                            program: 5,
                        },
                        2,
                    ),
                ],
                want_checkpoint: 1,
                want: Some(vec![ChannelJournal {
                    program: Some(ChapterP {
                        program: 5,
                        bank_msb: Some(1),
                        bank_lsb: None,
                    }),
                    ..ChannelJournal::default()
                }]),
            },
            Case {
                name: "a stream starting at a high sequence",
                steps: vec![Step::Send(note_off(60), 0xF000), Step::Ack(0xF000)],
                want_checkpoint: 0xF001,
                want: None,
            },
            Case {
                name: "acknowledgements across the sequence wrap",
                steps: vec![
                    Step::Send(note_off(60), 0xFFFE),
                    Step::Ack(0xFFFE),
                    Step::Send(note_off(61), 0x0002),
                    Step::Ack(0x0002),
                ],
                want_checkpoint: 0x0003,
                want: None,
            },
            Case {
                name: "an acknowledgement older than the checkpoint",
                steps: vec![
                    Step::Send(note_off(60), 100),
                    Step::Ack(200),
                    Step::Ack(150),
                ],
                want_checkpoint: 201,
                want: None,
            },
        ];
        for case in cases {
            let mut state = JournalState::new();
            for step in &case.steps {
                match step {
                    Step::Send(message, sequence) => state.observe(message, *sequence),
                    Step::Ack(sequence) => state.trim(*sequence),
                }
            }
            assert_eq!(
                state.checkpoint(),
                case.want_checkpoint,
                "{}: the checkpoint must sit just past what was acknowledged",
                case.name
            );
            assert_eq!(
                state.build(),
                case.want.map(|channels| RecoveryJournal {
                    checkpoint_seqnum: case.want_checkpoint,
                    single_packet_loss: false,
                    channels,
                }),
                "{}: the journal must carry exactly this",
                case.name
            );
        }
    }

    /// Arriving sequence numbers are read as in order, a gap, or a late duplicate, across the
    /// sixteen-bit wrap.
    ///
    /// After 11, packet 15 means `15 - 11 - 1 = 3` were lost. A packet at or before the highest
    /// seen is a late duplicate, which is neither loss nor news, so it moves neither the count
    /// nor the high-water mark the peer is told. After `0xFFFF` comes `0x0000`, `0x0002` means
    /// two were lost, and `0xFFFF` arriving after `0x0001` is late rather than 65 533 ahead.
    #[test]
    fn sequence_numbers_reveal_loss_across_the_wrap() {
        let cases = [
            (
                "three missed",
                vec![10, 11],
                15,
                Continuity::Gap { missing: 3 },
                15,
                3,
            ),
            (
                "a late duplicate",
                vec![10, 11],
                10,
                Continuity::Duplicate,
                11,
                0,
            ),
            (
                "in order across the wrap",
                vec![0xFFFF],
                0x0000,
                Continuity::InOrder,
                0x0000,
                0,
            ),
            (
                "a gap across the wrap",
                vec![0xFFFF],
                0x0002,
                Continuity::Gap { missing: 2 },
                0x0002,
                2,
            ),
            (
                "a late packet from before the wrap",
                vec![0xFFFF, 0x0001],
                0xFFFF,
                Continuity::Duplicate,
                0x0001,
                1,
            ),
        ];
        for (name, seen, next, want, want_highest, want_lost) in cases {
            let mut tracker = SequenceTracker::new();
            for sequence in seen {
                let _ = tracker.observe(sequence);
            }
            assert_eq!(
                tracker.observe(next),
                want,
                "{name}: the packet must be read this way"
            );
            assert_eq!(
                tracker.highest(),
                Some(want_highest),
                "{name}: the high-water mark told to the peer must be this"
            );
            assert_eq!(
                tracker.lost(),
                want_lost,
                "{name}: this many packets must be counted lost"
            );
        }
    }
}
