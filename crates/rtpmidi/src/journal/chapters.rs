//! The recovery journal chapters this implementation carries.
//!
//! Five of the eight defined chapters are implemented: N, C, P, W and T. Together they protect
//! notes, controllers, program, pitch wheel and channel pressure, which is everything needed to
//! stop a lost packet leaving a note sounding or a controller stale. Chapters M, E and A cover
//! parameter-system, overlapping-note and poly-pressure cases that no common sender produces.
//!
//! Layouts follow RFC 6295 Appendix A. Field widths are quoted beside each structure because a
//! one-bit error here is silent on the wire and audible on the far side.

use midi_harbor_core::midi::{Channel, MidiMessage};

/// Octets in a chapter P structure.
pub const CHAPTER_P_LEN: usize = 3;
/// Octets in a chapter W structure.
pub const CHAPTER_W_LEN: usize = 2;
/// Octets in a chapter T structure.
pub const CHAPTER_T_LEN: usize = 1;
/// Octets in one chapter C controller log.
pub const CONTROLLER_LOG_LEN: usize = 2;
/// Octets in one chapter N note log.
pub const NOTE_LOG_LEN: usize = 2;
/// Octets in the chapter N header.
pub const CHAPTER_N_HEADER_LEN: usize = 2;

/// Note numbers described by one OFFBITS octet.
pub const NOTES_PER_OFFBITS: u8 = 8;
/// OFFBITS octets needed to cover every note number.
pub const OFFBITS_OCTETS: usize = 16;

/// Largest value a seven-bit field can hold.
const SEVEN_BITS: u8 = 0x7F;
/// LOW value that, with HIGH of zero or one, codes an empty NoteOff bitfield.
const EMPTY_OFFBITS_LOW: u8 = 15;

/// Why a chapter could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChapterError {
    /// The chapter ended before a field it declared.
    #[error("chapter is {actual} octets, needs {expected}")]
    TooShort {
        /// How many octets were present.
        actual: usize,
        /// How many were needed.
        expected: usize,
    },
    /// The OFFBITS range is one the format forbids.
    #[error("offbits range low {low} high {high} is not valid")]
    InvalidOffbitsRange {
        /// The declared low octet index.
        low: u8,
        /// The declared high octet index.
        high: u8,
    },
}

/// Chapter P: the most recent program change, with bank select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChapterP {
    /// The program number.
    pub program: u8,
    /// The most significant bank byte, when one has been sent.
    pub bank_msb: Option<u8>,
    /// The least significant bank byte, when one has been sent.
    pub bank_lsb: Option<u8>,
}

impl ChapterP {
    /// Encodes the chapter, three octets: `S PROGRAM(7) | B BANK-MSB(7) | X BANK-LSB(7)`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.program & SEVEN_BITS);
        // The B bit says the bank values are meaningful rather than filler.
        let has_bank = self.bank_msb.is_some() || self.bank_lsb.is_some();
        let msb = self.bank_msb.unwrap_or(0) & SEVEN_BITS;
        out.push(if has_bank { 0x80 | msb } else { msb });
        out.push(self.bank_lsb.unwrap_or(0) & SEVEN_BITS);
    }

    /// Decodes the chapter.
    pub fn decode(bytes: &[u8]) -> Result<Self, ChapterError> {
        let program = read(bytes, 0, CHAPTER_P_LEN)?;
        let second = read(bytes, 1, CHAPTER_P_LEN)?;
        let third = read(bytes, 2, CHAPTER_P_LEN)?;

        let has_bank = second & 0x80 != 0;
        Ok(Self {
            program: program & SEVEN_BITS,
            bank_msb: has_bank.then_some(second & SEVEN_BITS),
            bank_lsb: has_bank.then_some(third & SEVEN_BITS),
        })
    }

    /// Returns the messages that restore this state on the receiving side.
    pub fn recover(&self, channel: Channel) -> Vec<MidiMessage> {
        let mut messages = Vec::with_capacity(3);
        // Bank select must precede the program change or it selects nothing.
        if let Some(msb) = self.bank_msb {
            messages.push(MidiMessage::ControlChange {
                channel,
                controller: 0,
                value: msb,
            });
        }
        if let Some(lsb) = self.bank_lsb {
            messages.push(MidiMessage::ControlChange {
                channel,
                controller: 32,
                value: lsb,
            });
        }
        messages.push(MidiMessage::ProgramChange {
            channel,
            program: self.program,
        });
        messages
    }
}

/// One controller's most recent value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerLog {
    /// Which controller.
    pub number: u8,
    /// Its value.
    pub value: u8,
}

/// Chapter C: the most recent value of each controller that has moved.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChapterC {
    /// The controller logs, oldest first.
    pub logs: Vec<ControllerLog>,
}

impl ChapterC {
    /// Encodes the chapter: `S LEN(7)` then `S NUMBER(7) | A VALUE(7)` per log.
    ///
    /// LEN codes the number of logs minus one, so an empty chapter cannot be expressed and is
    /// never emitted.
    pub fn encode(&self, out: &mut Vec<u8>) {
        if self.logs.is_empty() {
            return;
        }
        let count = self.logs.len().min(usize::from(SEVEN_BITS) + 1);
        out.push(u8::try_from(count - 1).unwrap_or(SEVEN_BITS) & SEVEN_BITS);

        for log in self.logs.iter().take(count) {
            out.push(log.number & SEVEN_BITS);
            // The A bit selects the value tool, which carries the controller value directly.
            out.push(log.value & SEVEN_BITS);
        }
    }

    /// Decodes the chapter, returning it and how many octets it consumed.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), ChapterError> {
        let header = read(bytes, 0, 1)?;
        let count = usize::from(header & SEVEN_BITS) + 1;
        let needed = 1 + count * CONTROLLER_LOG_LEN;
        if bytes.len() < needed {
            return Err(ChapterError::TooShort {
                actual: bytes.len(),
                expected: needed,
            });
        }

        let mut logs = Vec::with_capacity(count);
        for index in 0..count {
            let offset = 1 + index * CONTROLLER_LOG_LEN;
            logs.push(ControllerLog {
                number: read(bytes, offset, needed)? & SEVEN_BITS,
                value: read(bytes, offset + 1, needed)? & SEVEN_BITS,
            });
        }
        Ok((Self { logs }, needed))
    }

    /// Returns the messages that restore these controllers on the receiving side.
    pub fn recover(&self, channel: Channel) -> Vec<MidiMessage> {
        self.logs
            .iter()
            .map(|log| MidiMessage::ControlChange {
                channel,
                controller: log.number,
                value: log.value,
            })
            .collect()
    }
}

/// Chapter W: the most recent pitch wheel position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChapterW {
    /// The fourteen-bit wheel position.
    pub value: u16,
}

impl ChapterW {
    /// Encodes the chapter, two octets: `S FIRST(7) | R SECOND(7)`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(u8::try_from(self.value & 0x7F).unwrap_or(0));
        out.push(u8::try_from((self.value >> 7) & 0x7F).unwrap_or(0));
    }

    /// Decodes the chapter.
    pub fn decode(bytes: &[u8]) -> Result<Self, ChapterError> {
        let first = read(bytes, 0, CHAPTER_W_LEN)? & SEVEN_BITS;
        let second = read(bytes, 1, CHAPTER_W_LEN)? & SEVEN_BITS;
        Ok(Self {
            value: u16::from(first) | (u16::from(second) << 7),
        })
    }

    /// Returns the message that restores the wheel on the receiving side.
    pub fn recover(&self, channel: Channel) -> MidiMessage {
        MidiMessage::PitchBend {
            channel,
            value: self.value,
        }
    }
}

/// Chapter T: the most recent channel pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChapterT {
    /// The pressure value.
    pub pressure: u8,
}

impl ChapterT {
    /// Encodes the chapter, one octet: `S PRESSURE(7)`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.pressure & SEVEN_BITS);
    }

    /// Decodes the chapter.
    pub fn decode(bytes: &[u8]) -> Result<Self, ChapterError> {
        Ok(Self {
            pressure: read(bytes, 0, CHAPTER_T_LEN)? & SEVEN_BITS,
        })
    }

    /// Returns the message that restores channel pressure on the receiving side.
    pub fn recover(&self, channel: Channel) -> MidiMessage {
        MidiMessage::ChannelAftertouch {
            channel,
            pressure: self.pressure,
        }
    }
}

/// One sounding note and the velocity it started with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteLog {
    /// Which note.
    pub note: u8,
    /// The velocity it was played at. Never zero, since that would mean a note off.
    pub velocity: u8,
    /// Whether the sender recommends actually playing this recovered note.
    pub play: bool,
}

/// Chapter N: which notes are sounding, and which were released.
///
/// The chapter that prevents stuck notes, and the reason this crate exists rather than using an
/// off-the-shelf one. Notes still down are listed with their velocity; notes released are marked
/// in a bitfield, so a lost note off is reconstructed rather than leaving the note sounding
/// forever.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChapterN {
    /// Notes currently sounding, oldest first.
    pub notes: Vec<NoteLog>,
    /// Notes released since the checkpoint.
    pub released: Vec<u8>,
    /// Set unless the previous packet already carried a note off for this channel.
    pub bitfield_valid: bool,
}

impl ChapterN {
    /// Encodes the chapter: `B LEN(7) | LOW(4) HIGH(4)`, note logs, then OFFBITS octets.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let count = self.notes.len().min(usize::from(SEVEN_BITS));
        let (low, high, offbits) = self.build_offbits();

        let mut first = u8::try_from(count).unwrap_or(SEVEN_BITS) & SEVEN_BITS;
        if self.bitfield_valid {
            first |= 0x80;
        }
        out.push(first);
        out.push((low << 4) | (high & 0x0F));

        for log in self.notes.iter().take(count) {
            let mut note = log.note & SEVEN_BITS;
            // The S bit is left clear; recovery does not depend on it here.
            note &= SEVEN_BITS;
            out.push(note);
            let velocity = log.velocity.clamp(1, SEVEN_BITS);
            out.push(if log.play { 0x80 | velocity } else { velocity });
        }
        out.extend_from_slice(&offbits);
    }

    /// Builds the OFFBITS structure, returning the low index, high index and octets.
    ///
    /// An empty set is coded with low 15 and high 0, which the format reserves for exactly that.
    fn build_offbits(&self) -> (u8, u8, Vec<u8>) {
        if self.released.is_empty() {
            return (EMPTY_OFFBITS_LOW, 0, Vec::new());
        }

        let mut full = [0u8; OFFBITS_OCTETS];
        for note in &self.released {
            let index = usize::from(note / NOTES_PER_OFFBITS);
            let bit = note % NOTES_PER_OFFBITS;
            if let Some(octet) = full.get_mut(index) {
                // The most significant bit codes the lowest note in the group.
                *octet |= 0x80 >> bit;
            }
        }

        let low = full.iter().position(|o| *o != 0).unwrap_or(0);
        let high = full.iter().rposition(|o| *o != 0).unwrap_or(0);
        let octets = full.get(low..=high).map(<[u8]>::to_vec).unwrap_or_default();
        (
            u8::try_from(low).unwrap_or(0),
            u8::try_from(high).unwrap_or(0),
            octets,
        )
    }

    /// Decodes the chapter, returning it and how many octets it consumed.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), ChapterError> {
        let first = read(bytes, 0, CHAPTER_N_HEADER_LEN)?;
        let second = read(bytes, 1, CHAPTER_N_HEADER_LEN)?;

        let bitfield_valid = first & 0x80 != 0;
        let count = usize::from(first & SEVEN_BITS);
        let low = second >> 4;
        let high = second & 0x0F;

        // Low 15 with high 0 or 1 codes an empty bitfield; any other low above high is invalid.
        let offbits_len = if low == EMPTY_OFFBITS_LOW && high <= 1 {
            0
        } else if low <= high {
            usize::from(high - low) + 1
        } else {
            return Err(ChapterError::InvalidOffbitsRange { low, high });
        };

        let needed = CHAPTER_N_HEADER_LEN + count * NOTE_LOG_LEN + offbits_len;
        if bytes.len() < needed {
            return Err(ChapterError::TooShort {
                actual: bytes.len(),
                expected: needed,
            });
        }

        let mut notes = Vec::with_capacity(count);
        for index in 0..count {
            let offset = CHAPTER_N_HEADER_LEN + index * NOTE_LOG_LEN;
            let note = read(bytes, offset, needed)?;
            let velocity = read(bytes, offset + 1, needed)?;
            notes.push(NoteLog {
                note: note & SEVEN_BITS,
                velocity: velocity & SEVEN_BITS,
                play: velocity & 0x80 != 0,
            });
        }

        let mut released = Vec::new();
        let offbits_start = CHAPTER_N_HEADER_LEN + count * NOTE_LOG_LEN;
        for index in 0..offbits_len {
            let octet = read(bytes, offbits_start + index, needed)?;
            for bit in 0..NOTES_PER_OFFBITS {
                if octet & (0x80 >> bit) != 0 {
                    let base = low.saturating_add(u8::try_from(index).unwrap_or(0));
                    released.push(base.saturating_mul(NOTES_PER_OFFBITS).saturating_add(bit));
                }
            }
        }

        Ok((
            Self {
                notes,
                released,
                bitfield_valid,
            },
            needed,
        ))
    }
}

/// Reads one octet, reporting the expected length rather than panicking on a short chapter.
fn read(bytes: &[u8], offset: usize, expected: usize) -> Result<u8, ChapterError> {
    bytes.get(offset).copied().ok_or(ChapterError::TooShort {
        actual: bytes.len(),
        expected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    prop_compose! {
        /// Any chapter N with a handful of sounding notes and any set of releases.
        fn any_chapter_n()(
            notes in prop::collection::vec((0_u8..128, 1_u8..128, any::<bool>()), 0..20),
            released in prop::collection::btree_set(0_u8..128, 0..40),
            bitfield_valid in any::<bool>(),
        ) -> ChapterN {
            ChapterN {
                notes: notes
                    .into_iter()
                    .map(|(note, velocity, play)| NoteLog { note, velocity, play })
                    .collect(),
                released: released.into_iter().collect(),
                bitfield_valid,
            }
        }
    }

    proptest! {
        /// Chapter N round-trips every sounding note and every release, and says how many octets
        /// it took.
        ///
        /// OFFBITS (RFC 6295, Appendix A.6) codes note `n` in octet `n / 8` at bit `n % 8` counted
        /// from the most significant, so an off-by-one in the bit order moves a released note by
        /// up to seven semitones and an error in LOW or HIGH moves it by eight. Either would
        /// strand the note the chapter exists to release.
        #[test]
        fn chapter_n_round_trips(chapter in any_chapter_n()) {
            let mut bytes = Vec::new();
            chapter.encode(&mut bytes);

            prop_assert_eq!(
                ChapterN::decode(&bytes),
                Ok((chapter, bytes.len())),
                "chapter N must decode to what was encoded and consume all of it"
            );
        }
    }
}
