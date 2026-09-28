//! The RFC 6295 recovery journal.
//!
//! The journal rides along with every packet and describes the sender's state, so a receiver that
//! misses packets can rebuild what it lost instead of being left with a note sounding or a
//! controller stuck at a stale value. Reconnecting alone does not fix either; this does.

pub mod chapters;
pub mod state;

pub use state::{Continuity, JournalState, ReceivedState, SequenceTracker};

pub use chapters::{
    ChapterC, ChapterError, ChapterN, ChapterP, ChapterT, ChapterW, ControllerLog, NoteLog,
};

use midi_harbor_core::midi::{Channel, MidiMessage};

/// Octets in the top-level journal header.
pub const JOURNAL_HEADER_LEN: usize = 3;

/// Octets in a channel journal header.
pub const CHANNEL_HEADER_LEN: usize = 3;

/// Largest value the 10-bit channel journal LENGTH field can hold.
const MAX_CHANNEL_LENGTH: usize = 0x03FF;

/// Why a journal could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JournalError {
    /// The journal ended before a field it declared.
    #[error("journal is {actual} octets, needs {expected}")]
    TooShort {
        /// How many octets were present.
        actual: usize,
        /// How many were needed.
        expected: usize,
    },
    /// A channel journal declared a length that runs past the journal.
    #[error("channel journal declares {declared} octets but only {available} remain")]
    LengthOverrun {
        /// The declared length.
        declared: usize,
        /// What was actually left.
        available: usize,
    },
    /// A chapter inside a channel journal could not be decoded.
    #[error("chapter: {0}")]
    Chapter(#[from] ChapterError),
}

/// Recovery information for one MIDI channel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChannelJournal {
    /// Which channel this describes.
    pub channel: u8,
    /// The most recent program change.
    pub program: Option<ChapterP>,
    /// The most recent value of each controller that has moved.
    pub controls: Option<ChapterC>,
    /// The most recent pitch wheel position.
    pub wheel: Option<ChapterW>,
    /// Which notes are sounding and which were released.
    pub notes: Option<ChapterN>,
    /// The most recent channel pressure.
    pub pressure: Option<ChapterT>,
}

impl ChannelJournal {
    /// Reports whether this journal carries nothing worth sending.
    pub fn is_empty(&self) -> bool {
        self.program.is_none()
            && self.controls.is_none()
            && self.wheel.is_none()
            && self.notes.is_none()
            && self.pressure.is_none()
    }

    /// Encodes the channel journal, including its own length.
    fn encode(&self, out: &mut Vec<u8>) {
        // Build the chapters first, because the header carries the total length.
        let mut body = Vec::new();
        let mut toc = 0u8;

        // Chapters appear in the order their bits appear in the table of contents.
        if let Some(chapter) = &self.program {
            toc |= 0x80;
            chapter.encode(&mut body);
        }
        // An empty controller chapter cannot be expressed, so it is omitted rather than encoded.
        if let Some(chapter) = &self.controls
            && !chapter.logs.is_empty()
        {
            toc |= 0x40;
            chapter.encode(&mut body);
        }
        if let Some(chapter) = &self.wheel {
            toc |= 0x10;
            chapter.encode(&mut body);
        }
        if let Some(chapter) = &self.notes {
            toc |= 0x08;
            chapter.encode(&mut body);
        }
        if let Some(chapter) = &self.pressure {
            toc |= 0x02;
            chapter.encode(&mut body);
        }

        let length = (CHANNEL_HEADER_LEN + body.len()).min(MAX_CHANNEL_LENGTH);
        let channel = self.channel & 0x0F;
        // S(1) CHAN(4) H(1) LENGTH(10), then the table of contents.
        out.push((channel << 3) | u8::try_from(length >> 8).unwrap_or(0) & 0x03);
        out.push(u8::try_from(length & 0xFF).unwrap_or(0));
        out.push(toc);
        out.extend_from_slice(&body);
    }

    /// Decodes a channel journal, returning it and how many octets it consumed.
    fn decode(bytes: &[u8]) -> Result<(Self, usize), JournalError> {
        let first = byte(bytes, 0, CHANNEL_HEADER_LEN)?;
        let second = byte(bytes, 1, CHANNEL_HEADER_LEN)?;
        let toc = byte(bytes, 2, CHANNEL_HEADER_LEN)?;

        let channel = (first >> 3) & 0x0F;
        let declared = (usize::from(first & 0x03) << 8) | usize::from(second);
        if declared < CHANNEL_HEADER_LEN || declared > bytes.len() {
            return Err(JournalError::LengthOverrun {
                declared,
                available: bytes.len(),
            });
        }

        // Reading only within the declared length is what stops a malformed chapter from
        // consuming the journals that follow it.
        let body = bytes.get(CHANNEL_HEADER_LEN..declared).unwrap_or_default();
        let mut offset = 0;
        let mut journal = Self {
            channel,
            ..Self::default()
        };

        if toc & 0x80 != 0 {
            let slice = body.get(offset..).unwrap_or_default();
            journal.program = Some(ChapterP::decode(slice)?);
            offset = offset.saturating_add(chapters::CHAPTER_P_LEN);
        }
        if toc & 0x40 != 0 {
            let slice = body.get(offset..).unwrap_or_default();
            let (chapter, used) = ChapterC::decode(slice)?;
            journal.controls = Some(chapter);
            offset = offset.saturating_add(used);
        }
        if toc & 0x10 != 0 {
            let slice = body.get(offset..).unwrap_or_default();
            journal.wheel = Some(ChapterW::decode(slice)?);
            offset = offset.saturating_add(chapters::CHAPTER_W_LEN);
        }
        if toc & 0x08 != 0 {
            let slice = body.get(offset..).unwrap_or_default();
            let (chapter, used) = ChapterN::decode(slice)?;
            journal.notes = Some(chapter);
            offset = offset.saturating_add(used);
        }
        if toc & 0x02 != 0 {
            let slice = body.get(offset..).unwrap_or_default();
            journal.pressure = Some(ChapterT::decode(slice)?);
        }

        Ok((journal, declared))
    }

    /// Returns the messages that rebuild this channel's state.
    ///
    /// Ordered so the result is correct rather than merely complete: releases first so nothing is
    /// left sounding, then bank and program, then continuous state, then notes to play.
    pub fn recover(&self) -> Vec<MidiMessage> {
        let Some(channel) = Channel::new(self.channel) else {
            return Vec::new();
        };
        let mut messages = Vec::new();

        // Stop anything the sender has released before doing anything else.
        if let Some(notes) = &self.notes {
            for note in &notes.released {
                messages.push(MidiMessage::NoteOff {
                    channel,
                    note: *note,
                    velocity: 0,
                });
            }
        }
        if let Some(program) = &self.program {
            messages.extend(program.recover(channel));
        }
        if let Some(controls) = &self.controls {
            messages.extend(controls.recover(channel));
        }
        if let Some(wheel) = &self.wheel {
            messages.push(wheel.recover(channel));
        }
        if let Some(pressure) = &self.pressure {
            messages.push(pressure.recover(channel));
        }
        // Notes last, so they sound against state that is already correct.
        if let Some(notes) = &self.notes {
            for log in &notes.notes {
                if log.play {
                    messages.push(MidiMessage::NoteOn {
                        channel,
                        note: log.note,
                        velocity: log.velocity.max(1),
                    });
                }
            }
        }
        messages
    }
}

/// A complete recovery journal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryJournal {
    /// The oldest packet this journal's history covers.
    pub checkpoint_seqnum: u16,
    /// Set when the journal covers only a single packet of loss.
    pub single_packet_loss: bool,
    /// One journal per channel that has state worth protecting, in ascending channel order.
    pub channels: Vec<ChannelJournal>,
}

impl RecoveryJournal {
    /// Encodes the journal.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(JOURNAL_HEADER_LEN + 16);

        let present: Vec<&ChannelJournal> =
            self.channels.iter().filter(|c| !c.is_empty()).collect();
        // TOTCHAN codes the count minus one, so an empty list is signalled by the A bit instead.
        let mut first = 0u8;
        if self.single_packet_loss {
            first |= 0x80;
        }
        if !present.is_empty() {
            first |= 0x20;
            let total = present.len().min(16).saturating_sub(1);
            first |= u8::try_from(total).unwrap_or(0) & 0x0F;
        }
        out.push(first);
        out.extend_from_slice(&self.checkpoint_seqnum.to_be_bytes());

        for channel in present.iter().take(16) {
            channel.encode(&mut out);
        }
        out
    }

    /// Decodes a journal received from a peer.
    pub fn decode(bytes: &[u8]) -> Result<Self, JournalError> {
        let first = byte(bytes, 0, JOURNAL_HEADER_LEN)?;
        let high = byte(bytes, 1, JOURNAL_HEADER_LEN)?;
        let low = byte(bytes, 2, JOURNAL_HEADER_LEN)?;

        let single_packet_loss = first & 0x80 != 0;
        let has_system = first & 0x40 != 0;
        let has_channels = first & 0x20 != 0;
        let total_channels = usize::from(first & 0x0F) + 1;

        let mut offset = JOURNAL_HEADER_LEN;

        // The system journal is not interpreted, but its length must be honoured to reach the
        // channel journals that follow it.
        if has_system {
            let sys_first = byte(bytes, offset, offset + 2)?;
            let sys_second = byte(bytes, offset + 1, offset + 2)?;
            let length = (usize::from(sys_first & 0x03) << 8) | usize::from(sys_second);
            offset = offset.saturating_add(length.max(2));
        }

        let mut channels = Vec::new();
        if has_channels {
            for _ in 0..total_channels {
                let slice = bytes.get(offset..).unwrap_or_default();
                if slice.is_empty() {
                    break;
                }
                let (channel, used) = ChannelJournal::decode(slice)?;
                channels.push(channel);
                if used == 0 {
                    break;
                }
                offset = offset.saturating_add(used);
            }
        }

        Ok(Self {
            checkpoint_seqnum: u16::from(high) << 8 | u16::from(low),
            single_packet_loss,
            channels,
        })
    }

    /// Returns the messages that rebuild every channel's state.
    pub fn recover(&self) -> Vec<MidiMessage> {
        self.channels
            .iter()
            .flat_map(ChannelJournal::recover)
            .collect()
    }
}

/// Reads one octet, reporting the expected length rather than panicking.
fn byte(bytes: &[u8], offset: usize, expected: usize) -> Result<u8, JournalError> {
    bytes.get(offset).copied().ok_or(JournalError::TooShort {
        actual: bytes.len(),
        expected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(note: u8, velocity: u8, play: bool) -> NoteLog {
        NoteLog {
            note,
            velocity,
            play,
        }
    }

    fn journal(checkpoint_seqnum: u16, channels: Vec<ChannelJournal>) -> RecoveryJournal {
        RecoveryJournal {
            checkpoint_seqnum,
            single_packet_loss: false,
            channels,
        }
    }

    /// Journals encode to the RFC 6295 layout byte for byte, and decode back to themselves.
    ///
    /// The journal header (section 5) is `S Y A H TOTCHAN(4)` then the 16-bit checkpoint, where
    /// A (`0x20`) says channel journals follow and TOTCHAN is their count minus one. Each channel
    /// journal (section 5.2) opens `S CHAN(4) H LENGTH(10)`, so channel 2 is `2 << 3 = 0x10` and
    /// channel 9 is `0x48`, with LENGTH counting its own three header bytes, then a table of
    /// contents `P C M W N E T A` (`0x80 0x40 0x20 0x10 0x08 0x04 0x02 0x01`). Chapters follow in
    /// that order, laid out as Appendix A gives them: P is `S PROGRAM | B BANK-MSB | X BANK-LSB`,
    /// C is `S LEN` with LEN one less than the log count, then `NUMBER, VALUE` pairs, W is the
    /// wheel's low seven bits then its high seven (`8192 = 0x40 << 7`), N is `B LEN | LOW HIGH`,
    /// logs of `NOTENUM, Y VELOCITY`, then OFFBITS octets with the most significant bit coding the
    /// lowest note, and T is the pressure. Notes 64 and 67 fall in octet `64 / 8 = 8` at bits 0
    /// and 3, so `0x80 | 0x10 = 0x90` with LOW and HIGH both 8, and an empty set is coded LOW 15,
    /// HIGH 0.
    #[test]
    fn journals_encode_to_the_rfc_6295_layout_and_decode_back() {
        let only = |channel: u8, chapters: ChannelJournal| ChannelJournal {
            channel,
            ..chapters
        };
        let program = |bank: Option<(u8, u8)>| ChannelJournal {
            program: Some(ChapterP {
                program: 5,
                bank_msb: bank.map(|(msb, _)| msb),
                bank_lsb: bank.map(|(_, lsb)| lsb),
            }),
            ..ChannelJournal::default()
        };
        let pressure = |value: u8| ChannelJournal {
            pressure: Some(ChapterT { pressure: value }),
            ..ChannelJournal::default()
        };
        let every_chapter = ChannelJournal {
            channel: 0,
            program: Some(ChapterP {
                program: 5,
                bank_msb: None,
                bank_lsb: None,
            }),
            controls: Some(ChapterC {
                logs: vec![ControllerLog {
                    number: 7,
                    value: 100,
                }],
            }),
            wheel: Some(ChapterW { value: 8192 }),
            notes: Some(ChapterN {
                notes: vec![note(60, 100, true)],
                released: vec![],
                bitfield_valid: true,
            }),
            pressure: Some(ChapterT { pressure: 77 }),
        };

        let cases: [(&str, RecoveryJournal, Vec<u8>); 9] = [
            (
                "an empty journal",
                journal(0x1234, vec![]),
                vec![0x00, 0x12, 0x34],
            ),
            (
                "single-packet loss, chapter T on channel 2",
                RecoveryJournal {
                    checkpoint_seqnum: 1,
                    single_packet_loss: true,
                    channels: vec![only(2, pressure(77))],
                },
                vec![0xA0, 0x00, 0x01, 0x10, 0x04, 0x02, 0x4D],
            ),
            (
                "chapter P without a bank",
                journal(1, vec![program(None)]),
                vec![0x20, 0x00, 0x01, 0x00, 0x06, 0x80, 0x05, 0x00, 0x00],
            ),
            (
                "chapter P with a bank",
                journal(1, vec![program(Some((1, 2)))]),
                vec![0x20, 0x00, 0x01, 0x00, 0x06, 0x80, 0x05, 0x81, 0x02],
            ),
            (
                "chapter C with two logs",
                journal(
                    1,
                    vec![ChannelJournal {
                        controls: Some(ChapterC {
                            logs: vec![
                                ControllerLog {
                                    number: 7,
                                    value: 100,
                                },
                                ControllerLog {
                                    number: 74,
                                    value: 12,
                                },
                            ],
                        }),
                        ..ChannelJournal::default()
                    }],
                ),
                vec![0x20, 0x00, 0x01, 0x00, 0x08, 0x40, 0x01, 7, 100, 74, 12],
            ),
            (
                "chapter W at the centre",
                journal(
                    1,
                    vec![ChannelJournal {
                        wheel: Some(ChapterW { value: 8192 }),
                        ..ChannelJournal::default()
                    }],
                ),
                vec![0x20, 0x00, 0x01, 0x00, 0x05, 0x10, 0x00, 0x40],
            ),
            (
                "chapter N with a note sounding and two released",
                journal(
                    1,
                    vec![ChannelJournal {
                        notes: Some(ChapterN {
                            notes: vec![note(60, 100, true)],
                            released: vec![64, 67],
                            bitfield_valid: true,
                        }),
                        ..ChannelJournal::default()
                    }],
                ),
                vec![
                    0x20,
                    0x00,
                    0x01,
                    0x00,
                    0x08,
                    0x08,
                    0x81,
                    0x88,
                    60,
                    0x80 | 100,
                    0x90,
                ],
            ),
            (
                "every chapter, in table-of-contents order",
                journal(1, vec![every_chapter]),
                vec![
                    0x20,
                    0x00,
                    0x01,
                    0x00,
                    0x10,
                    0xDA, // headers
                    0x05,
                    0x00,
                    0x00, // P
                    0x00,
                    7,
                    100, // C
                    0x00,
                    0x40, // W
                    0x81,
                    0xF0,
                    60,
                    0x80 | 100, // N, nothing released
                    77,         // T
                ],
            ),
            (
                "two channels kept apart",
                journal(1, vec![only(0, pressure(1)), only(9, pressure(2))]),
                vec![
                    0x21, 0x00, 0x01, 0x00, 0x04, 0x02, 0x01, 0x48, 0x04, 0x02, 0x02,
                ],
            ),
        ];
        for (name, journal, bytes) in cases {
            let encoded = journal.encode();
            assert_eq!(
                encoded, bytes,
                "{name}: the encoding must match the RFC 6295 layout"
            );
            assert_eq!(
                RecoveryJournal::decode(&encoded),
                Ok(journal),
                "{name}: the encoding must decode back to the same journal"
            );
        }
    }

    /// Recovery orders its messages so the result is correct rather than merely complete.
    ///
    /// Releases come first, or a recovered note on could leave a released note sounding. Bank
    /// select precedes the program change, or it selects nothing. Controllers, wheel and pressure
    /// come before notes, so a note sounds against state that is already right. A note the sender
    /// marked not to play (Y clear, RFC 6295 Appendix A.6) is skipped, and a recovered note on
    /// never carries velocity 0, which would make it a note off.
    #[test]
    fn recovery_releases_first_and_plays_notes_last() {
        let channel = Channel::new(0).expect("channel 1 is in range");
        let journal = journal(
            0,
            vec![ChannelJournal {
                channel: 0,
                program: Some(ChapterP {
                    program: 5,
                    bank_msb: Some(1),
                    bank_lsb: Some(2),
                }),
                controls: Some(ChapterC {
                    logs: vec![ControllerLog {
                        number: 7,
                        value: 90,
                    }],
                }),
                wheel: Some(ChapterW { value: 100 }),
                notes: Some(ChapterN {
                    notes: vec![note(64, 0, true), note(65, 80, false)],
                    released: vec![60],
                    bitfield_valid: true,
                }),
                pressure: Some(ChapterT { pressure: 64 }),
            }],
        );

        assert_eq!(
            journal.recover(),
            vec![
                MidiMessage::NoteOff {
                    channel,
                    note: 60,
                    velocity: 0
                },
                MidiMessage::ControlChange {
                    channel,
                    controller: 0,
                    value: 1
                },
                MidiMessage::ControlChange {
                    channel,
                    controller: 32,
                    value: 2
                },
                MidiMessage::ProgramChange {
                    channel,
                    program: 5
                },
                MidiMessage::ControlChange {
                    channel,
                    controller: 7,
                    value: 90
                },
                MidiMessage::PitchBend {
                    channel,
                    value: 100
                },
                MidiMessage::ChannelAftertouch {
                    channel,
                    pressure: 64
                },
                MidiMessage::NoteOn {
                    channel,
                    note: 64,
                    velocity: 1
                },
            ],
            "recovery must release, restore state, then play, in exactly this order"
        );
    }

    /// Decoding accepts each well-formed boundary and refuses the malformed input beside it.
    ///
    /// A channel journal's LENGTH must fit in what remains: a chapter T journal is `3 + 1 = 4`
    /// octets, so LENGTH 4 is accepted and 5 is not. A malformed chapter must not consume the
    /// journals after it. Chapter N's OFFBITS range has LOW at most HIGH, except that LOW 15 with
    /// HIGH 0 or 1 codes an empty set (RFC 6295, Appendix A.6), so LOW 15 HIGH 1 is accepted and
    /// LOW 3 HIGH 1 is refused.
    #[test]
    fn decode_accepts_each_boundary_and_refuses_the_input_past_it() {
        let cases = [
            (
                "a channel length equal to what remains",
                vec![0x20, 0x00, 0x01, 0x00, 0x04, 0x02, 0x4D],
                Ok(()),
            ),
            (
                "a channel length one past what remains",
                vec![0x20, 0x00, 0x01, 0x00, 0x05, 0x02, 0x4D],
                Err(JournalError::LengthOverrun {
                    declared: 5,
                    available: 4,
                }),
            ),
            (
                "OFFBITS LOW 15 HIGH 1, an empty set",
                vec![0x20, 0x00, 0x01, 0x00, 0x05, 0x08, 0x80, 0xF1],
                Ok(()),
            ),
            (
                "OFFBITS LOW 3 HIGH 1",
                vec![0x20, 0x00, 0x01, 0x00, 0x05, 0x08, 0x80, 0x31],
                Err(JournalError::Chapter(ChapterError::InvalidOffbitsRange {
                    low: 3,
                    high: 1,
                })),
            ),
        ];
        for (name, bytes, want) in cases {
            assert_eq!(
                RecoveryJournal::decode(&bytes).map(|_| ()),
                want,
                "{name}: the decode must return this result"
            );
        }
    }

    /// Every truncation of a journal is refused or read as carrying no channel, never read past
    /// its end or misread into a channel journal it does not hold.
    ///
    /// Cut at the end of the three-octet header, a journal has room for no channel journal, which
    /// is the one truncation that decodes; any cut inside the channel journal breaks its LENGTH.
    #[test]
    fn a_truncated_journal_yields_no_channel() {
        let encoded = journal(
            7,
            vec![ChannelJournal {
                channel: 3,
                program: Some(ChapterP {
                    program: 5,
                    bank_msb: Some(1),
                    bank_lsb: None,
                }),
                controls: Some(ChapterC {
                    logs: vec![ControllerLog {
                        number: 1,
                        value: 2,
                    }],
                }),
                wheel: Some(ChapterW { value: 8192 }),
                notes: Some(ChapterN {
                    notes: vec![note(60, 100, true), note(62, 100, true)],
                    released: vec![64, 65],
                    bitfield_valid: true,
                }),
                pressure: Some(ChapterT { pressure: 9 }),
            }],
        )
        .encode();

        for length in 0..encoded.len() {
            let decoded = RecoveryJournal::decode(&encoded[..length]);
            assert!(
                decoded.as_ref().map_or(true, |j| j.channels.is_empty()),
                "a journal cut to {length} of {} octets must yield no channel, got {decoded:?}",
                encoded.len()
            );
        }
    }
}
