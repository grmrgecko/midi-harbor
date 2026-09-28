//! MIDI 1.0 message semantics.
//!
//! Shared by routing, note silencing, the recovery journal and the message monitor, because all
//! four need to know what a run of bytes means rather than just how long it is.

/// Highest value a data byte may carry. Anything above has the status bit set.
pub const MAX_DATA: u8 = 0x7F;

/// Channels in MIDI 1.0.
pub const CHANNELS: u8 = 16;

/// Controller number for all-sound-off.
pub const CC_ALL_SOUND_OFF: u8 = 120;
/// Controller number for reset-all-controllers.
pub const CC_RESET_ALL_CONTROLLERS: u8 = 121;
/// Controller number for all-notes-off.
pub const CC_ALL_NOTES_OFF: u8 = 123;
/// Controller number for the sustain pedal.
pub const CC_SUSTAIN: u8 = 64;

/// A MIDI channel, zero-based internally and one-based when shown to people.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Channel(u8);

impl Channel {
    /// Creates a channel from a zero-based number, returning `None` when out of range.
    pub fn new(zero_based: u8) -> Option<Self> {
        (zero_based < CHANNELS).then_some(Self(zero_based))
    }

    /// Creates a channel from the low nibble of a status byte, which is always in range.
    pub fn from_status(status: u8) -> Self {
        Self(status & 0x0F)
    }

    /// Returns the zero-based number, as it appears on the wire.
    pub fn index(&self) -> u8 {
        self.0
    }

    /// Returns the one-based number, as people refer to it.
    pub fn number(&self) -> u8 {
        self.0.saturating_add(1)
    }

    /// Returns every channel, for operations that sweep all of them.
    pub fn all() -> impl Iterator<Item = Self> {
        (0..CHANNELS).map(Self)
    }
}

/// A parsed MIDI 1.0 message.
///
/// System-exclusive payloads are deliberately absent: they are unbounded and must not be copied
/// around on the data path, so they travel as handles into a pre-allocated pool instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiMessage {
    /// A key was released.
    NoteOff {
        /// Which channel.
        channel: Channel,
        /// Which key.
        note: u8,
        /// How fast it was released.
        velocity: u8,
    },
    /// A key was pressed. A velocity of zero means the same as note off.
    NoteOn {
        /// Which channel.
        channel: Channel,
        /// Which key.
        note: u8,
        /// How hard it was pressed.
        velocity: u8,
    },
    /// Pressure applied to one held key.
    PolyAftertouch {
        /// Which channel.
        channel: Channel,
        /// Which key.
        note: u8,
        /// How much pressure.
        pressure: u8,
    },
    /// A controller moved.
    ControlChange {
        /// Which channel.
        channel: Channel,
        /// Which controller.
        controller: u8,
        /// Its new value.
        value: u8,
    },
    /// The sound selection changed.
    ProgramChange {
        /// Which channel.
        channel: Channel,
        /// Which program.
        program: u8,
    },
    /// Pressure applied to the whole channel.
    ChannelAftertouch {
        /// Which channel.
        channel: Channel,
        /// How much pressure.
        pressure: u8,
    },
    /// The pitch wheel moved, centred at 8192.
    PitchBend {
        /// Which channel.
        channel: Channel,
        /// The fourteen-bit position.
        value: u16,
    },
    /// A system common message that carries data: a time code quarter frame, a song position
    /// or a song select.
    SystemCommon {
        /// The status byte, `0xF1` to `0xF3`.
        status: u8,
        /// The data bytes, of which a quarter frame and a song select use only the first.
        data: [u8; 2],
    },
    /// A system message of one byte, which is every real-time message and a tune request.
    System {
        /// The status byte.
        status: u8,
    },
}

impl MidiMessage {
    /// Returns the channel this message applies to, or `None` for system messages.
    pub fn channel(&self) -> Option<Channel> {
        match self {
            Self::NoteOff { channel, .. }
            | Self::NoteOn { channel, .. }
            | Self::PolyAftertouch { channel, .. }
            | Self::ControlChange { channel, .. }
            | Self::ProgramChange { channel, .. }
            | Self::ChannelAftertouch { channel, .. }
            | Self::PitchBend { channel, .. } => Some(*channel),
            Self::SystemCommon { .. } | Self::System { .. } => None,
        }
    }

    /// Reports whether this message starts a note sounding.
    ///
    /// A note on with zero velocity is a note off by convention, and treating it as a start is
    /// the classic way to leave a note hanging forever.
    pub fn starts_note(&self) -> bool {
        matches!(self, Self::NoteOn { velocity, .. } if *velocity > 0)
    }

    /// Reports whether this message stops a note sounding.
    pub fn stops_note(&self) -> Option<(Channel, u8)> {
        match self {
            Self::NoteOff { channel, note, .. } => Some((*channel, *note)),
            Self::NoteOn {
                channel,
                note,
                velocity,
            } if *velocity == 0 => Some((*channel, *note)),
            _ => None,
        }
    }

    /// Returns the status byte for this message.
    pub fn status(&self) -> u8 {
        match self {
            Self::NoteOff { channel, .. } => 0x80 | channel.index(),
            Self::NoteOn { channel, .. } => 0x90 | channel.index(),
            Self::PolyAftertouch { channel, .. } => 0xA0 | channel.index(),
            Self::ControlChange { channel, .. } => 0xB0 | channel.index(),
            Self::ProgramChange { channel, .. } => 0xC0 | channel.index(),
            Self::ChannelAftertouch { channel, .. } => 0xD0 | channel.index(),
            Self::PitchBend { channel, .. } => 0xE0 | channel.index(),
            Self::SystemCommon { status, .. } | Self::System { status } => *status,
        }
    }

    /// Returns how many bytes this message occupies, including its status byte.
    pub fn len(&self) -> usize {
        match self {
            Self::ProgramChange { .. } | Self::ChannelAftertouch { .. } => 2,
            Self::SystemCommon { status, .. } | Self::System { status } => {
                system_message_len(*status)
            }
            _ => 3,
        }
    }

    /// Reports whether this message carries no bytes, which cannot happen.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Writes this message into `out`, returning how many bytes were written.
    ///
    /// Writes nothing and returns zero when `out` is too small, so a caller on the data path
    /// never has to handle a panic or a reallocation.
    pub fn encode(&self, out: &mut [u8]) -> usize {
        let needed = self.len();
        if out.len() < needed {
            return 0;
        }

        let bytes: [u8; 3] = match self {
            Self::NoteOff { note, velocity, .. } => [self.status(), *note, *velocity],
            Self::NoteOn { note, velocity, .. } => [self.status(), *note, *velocity],
            Self::PolyAftertouch { note, pressure, .. } => [self.status(), *note, *pressure],
            Self::ControlChange {
                controller, value, ..
            } => [self.status(), *controller, *value],
            Self::ProgramChange { program, .. } => [self.status(), *program, 0],
            Self::ChannelAftertouch { pressure, .. } => [self.status(), *pressure, 0],
            Self::PitchBend { value, .. } => {
                // Fourteen bits split across two seven-bit bytes, least significant first.
                [
                    self.status(),
                    (*value & 0x7F) as u8,
                    ((*value >> 7) & 0x7F) as u8,
                ]
            }
            Self::SystemCommon {
                status,
                data: [first, second],
            } => [*status, *first, *second],
            Self::System { status } => [*status, 0, 0],
        };

        for (slot, byte) in out.iter_mut().zip(bytes.iter()).take(needed) {
            *slot = *byte;
        }
        needed
    }

    /// Parses one message from `bytes`, returning it and how many bytes it consumed.
    ///
    /// `running_status` supplies the status byte when the message omits it, which is legal inside
    /// an RTP-MIDI list and common from hardware.
    pub fn parse(bytes: &[u8], running_status: Option<u8>) -> Option<(Self, usize)> {
        let first = bytes.first().copied()?;

        // Resolve the status byte, which may be carried over from the previous message.
        let (status, payload_offset) = if first >= 0x80 {
            (first, 1)
        } else {
            (running_status?, 0)
        };

        // A byte with the high bit set is never data. Finding one here means a status arrived
        // before this message was complete, and forwarding it as data would put a status byte in
        // the middle of the next device's stream.
        let len = if status >= 0xF0 {
            system_message_len(status)
        } else {
            channel_message_len(status)
        };
        let data = bytes.get(payload_offset..)?;
        let needed = data.get(..len.saturating_sub(1))?;
        if needed.iter().any(|byte| *byte >= 0x80) {
            return None;
        }

        // System messages carry no channel.
        if status >= 0xF0 {
            let message = if len > 1 {
                Self::SystemCommon {
                    status,
                    data: [
                        needed.first().copied().unwrap_or(0),
                        needed.get(1).copied().unwrap_or(0),
                    ],
                }
            } else {
                Self::System { status }
            };
            return Some((message, payload_offset + len.saturating_sub(1)));
        }

        let channel = Channel::from_status(status);
        let first_data = data.first().copied()?;

        let message = match status & 0xF0 {
            0x80 => {
                let velocity = data.get(1).copied()?;
                Self::NoteOff {
                    channel,
                    note: first_data,
                    velocity,
                }
            }
            0x90 => {
                let velocity = data.get(1).copied()?;
                Self::NoteOn {
                    channel,
                    note: first_data,
                    velocity,
                }
            }
            0xA0 => {
                let pressure = data.get(1).copied()?;
                Self::PolyAftertouch {
                    channel,
                    note: first_data,
                    pressure,
                }
            }
            0xB0 => {
                let value = data.get(1).copied()?;
                Self::ControlChange {
                    channel,
                    controller: first_data,
                    value,
                }
            }
            0xC0 => Self::ProgramChange {
                channel,
                program: first_data,
            },
            0xD0 => Self::ChannelAftertouch {
                channel,
                pressure: first_data,
            },
            0xE0 => {
                let msb = data.get(1).copied()?;
                Self::PitchBend {
                    channel,
                    value: u16::from(first_data) | (u16::from(msb) << 7),
                }
            }
            _ => return None,
        };

        let consumed = payload_offset + message.len().saturating_sub(1);
        Some((message, consumed))
    }
}

/// Returns how many bytes a channel message occupies, including its status byte.
fn channel_message_len(status: u8) -> usize {
    match status & 0xF0 {
        0xC0 | 0xD0 => 2,
        _ => 3,
    }
}

/// Returns how many bytes a system message occupies.
///
/// System-exclusive is unbounded, so it reports its status byte alone and the caller scans for
/// the terminator itself.
fn system_message_len(status: u8) -> usize {
    match status {
        // Time code quarter frame and song select carry one data byte.
        0xF1 | 0xF3 => 2,
        // Song position pointer carries two.
        0xF2 => 3,
        _ => 1,
    }
}

/// Returns a note number's key and octave, as Apple's MIDI tools write it: middle C, note 60, is C3.
///
/// Every place a note is shown uses this, so the monitor and the window's note picker name a note
/// alike.
pub fn note_name(note: u8) -> String {
    /// Key names, indexed by semitone within an octave.
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let key = NAMES.get(usize::from(note % 12)).copied().unwrap_or("?");
    // Twelve semitones per octave, and note 0 sits two octaves below octave 0.
    #[allow(clippy::integer_division)]
    let octave = i16::from(note / 12) - 2;
    format!("{key}{octave}")
}

/// Builds the messages that silence a channel completely.
///
/// Sent on every link teardown and recovery. All-notes-off alone is not enough: a held sustain
/// pedal keeps notes sounding through it. This sends the pedal release first, then the two broad
/// resets. A synth that ignores all-notes-off needs a note off for each note, which only a caller
/// that knows the notes can send; `Sounding::silence` does.
pub fn silence_channel(channel: Channel) -> [MidiMessage; 3] {
    [
        MidiMessage::ControlChange {
            channel,
            controller: CC_SUSTAIN,
            value: 0,
        },
        MidiMessage::ControlChange {
            channel,
            controller: CC_ALL_NOTES_OFF,
            value: 0,
        },
        MidiMessage::ControlChange {
            channel,
            controller: CC_ALL_SOUND_OFF,
            value: 0,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks Apple's octave numbering, which the monitor and the note picker both show.
    ///
    /// Audio MIDI Setup and Logic write middle C, note 60, as C3; other tools write it C4, so the
    /// convention is a choice that has to stay put. Note 0 is C-2 and note 127 is G8, the ends of
    /// MIDI's range: 127 is ten octaves and seven semitones above 0.
    #[test]
    fn a_note_is_named_with_middle_c_as_c3() {
        for (note, want) in [(0, "C-2"), (60, "C3"), (61, "C#3"), (69, "A3"), (127, "G8")] {
            assert_eq!(note_name(note), want, "note {note} must read {want}");
        }
    }

    fn channel(index: u8) -> Channel {
        Channel::new(index).expect("channel in range")
    }

    /// Every kind of message encodes to the bytes the MIDI 1.0 specification gives it and parses
    /// back from them.
    ///
    /// The status byte is the kind in the high nibble and the zero-based channel in the low one.
    /// Program change and channel pressure carry one data byte, quarter frame and song select one,
    /// song position two, and real-time messages none. Pitch bend is fourteen bits sent least
    /// significant seven first, so the centre, 8192 = 0x40 << 7 | 0x00, is `E1 00 40`.
    #[test]
    fn messages_encode_to_the_bytes_the_specification_gives_and_parse_back() {
        let cases = [
            (
                "note off",
                MidiMessage::NoteOff {
                    channel: channel(0),
                    note: 60,
                    velocity: 64,
                },
                &[0x80, 60, 64][..],
            ),
            (
                "note on, channel 16",
                MidiMessage::NoteOn {
                    channel: channel(15),
                    note: 127,
                    velocity: 100,
                },
                &[0x9F, 127, 100][..],
            ),
            (
                "poly pressure",
                MidiMessage::PolyAftertouch {
                    channel: channel(3),
                    note: 40,
                    pressure: 90,
                },
                &[0xA3, 40, 90][..],
            ),
            (
                "control change",
                MidiMessage::ControlChange {
                    channel: channel(7),
                    controller: 74,
                    value: 12,
                },
                &[0xB7, 74, 12][..],
            ),
            (
                "program change",
                MidiMessage::ProgramChange {
                    channel: channel(2),
                    program: 5,
                },
                &[0xC2, 5][..],
            ),
            (
                "channel pressure",
                MidiMessage::ChannelAftertouch {
                    channel: channel(9),
                    pressure: 77,
                },
                &[0xD9, 77][..],
            ),
            (
                "pitch bend centred",
                MidiMessage::PitchBend {
                    channel: channel(1),
                    value: 8192,
                },
                &[0xE1, 0x00, 0x40][..],
            ),
            (
                "time code quarter frame",
                MidiMessage::SystemCommon {
                    status: 0xF1,
                    data: [0x35, 0],
                },
                &[0xF1, 0x35][..],
            ),
            (
                "song position",
                MidiMessage::SystemCommon {
                    status: 0xF2,
                    data: [0x10, 0x20],
                },
                &[0xF2, 0x10, 0x20][..],
            ),
            (
                "song select",
                MidiMessage::SystemCommon {
                    status: 0xF3,
                    data: [0x07, 0],
                },
                &[0xF3, 0x07][..],
            ),
            (
                "timing clock",
                MidiMessage::System { status: 0xF8 },
                &[0xF8][..],
            ),
        ];
        for (case, message, bytes) in cases {
            let mut buffer = [0u8; 3];
            let written = message.encode(&mut buffer);
            assert_eq!(
                &buffer[..written],
                bytes,
                "{case}: the encoding must be the specification's bytes"
            );
            assert_eq!(
                MidiMessage::parse(bytes, None),
                Some((message, bytes.len())),
                "{case}: the specification's bytes must parse back to the message, consumed whole"
            );
        }
    }

    proptest::proptest! {
        /// Whatever parses from a run of bytes encodes back to exactly the bytes it consumed.
        ///
        /// A message the daemon would read one way and send another is a peer misreading us.
        #[test]
        fn what_is_parsed_encodes_back_to_the_bytes_it_came_from(
            bytes in proptest::collection::vec(proptest::num::u8::ANY, 1..4),
        ) {
            proptest::prop_assume!(bytes.first().is_some_and(|status| *status >= 0x80));
            if let Some((message, consumed)) = MidiMessage::parse(&bytes, None) {
                let mut buffer = [0u8; 3];
                let written = message.encode(&mut buffer);
                proptest::prop_assert_eq!(buffer.get(..written), bytes.get(..consumed));
            }
        }
    }

    /// Running status supplies a missing status byte, and bytes that do not make a whole message
    /// are refused rather than completed by guessing.
    ///
    /// Running status is legal inside an RTP-MIDI list and common from hardware, so data bytes
    /// with a status carried over are a message. With nothing carried over, or with the message
    /// cut short, nothing establishes what the bytes mean.
    #[test]
    fn running_status_completes_a_message_and_nothing_else_does() {
        let note_on = MidiMessage::NoteOn {
            channel: channel(0),
            note: 62,
            velocity: 100,
        };
        let cases = [
            (
                "data bytes under running status",
                &[62, 100][..],
                Some(0x90),
                Some((note_on, 2)),
            ),
            (
                "data bytes with no running status",
                &[62, 100][..],
                None,
                None,
            ),
            (
                "a note on missing its velocity",
                &[0x90, 60][..],
                None,
                None,
            ),
            ("a status byte alone", &[0x90][..], None, None),
            ("nothing at all", &[][..], Some(0x90), None),
        ];
        for (case, bytes, running, want) in cases {
            assert_eq!(
                MidiMessage::parse(bytes, running),
                want,
                "{case}: only a whole message may be parsed"
            );
        }
    }

    /// A byte with the high bit set is never read as data, wherever it falls in a message.
    ///
    /// MIDI 1.0 reserves the high bit for status bytes. One arriving before a message is complete
    /// abandons it; forwarding it as data would put a status byte in the middle of the next
    /// device's stream, which that device reads as the start of something else.
    #[test]
    fn a_status_byte_is_never_read_as_data() {
        for status in (0x80..=0xE0).step_by(0x10).chain([0xF1, 0xF2, 0xF3]) {
            let len = if status >= 0xF0 {
                system_message_len(status)
            } else {
                channel_message_len(status)
            };
            for intruder in 0x80..=0xFF {
                for position in 1..len {
                    let mut bytes = [status, 0x00, 0x00];
                    bytes[position] = intruder;
                    assert_eq!(
                        MidiMessage::parse(&bytes, None),
                        None,
                        "{bytes:02X?}: a status byte inside a message must refuse it"
                    );
                }
            }
        }
    }

    /// A note on with zero velocity stops a note rather than starting one.
    ///
    /// The MIDI 1.0 specification defines it as a note off, and senders use it to keep running
    /// status across a run of notes. Treating it as a start is the classic way to leave a note
    /// hanging forever.
    #[test]
    fn a_note_on_with_zero_velocity_stops_a_note() {
        let cases = [
            ("velocity zero", 0, false, Some((channel(0), 60))),
            ("velocity one", 1, true, None),
        ];
        for (case, velocity, starts, stops) in cases {
            let message = MidiMessage::NoteOn {
                channel: channel(0),
                note: 60,
                velocity,
            };
            assert_eq!(
                (message.starts_note(), message.stops_note()),
                (starts, stops),
                "{case}: only a note on with velocity starts a note"
            );
        }
    }
}
