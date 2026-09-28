//! The RTP-MIDI payload: an RTP header, a list of timed MIDI messages, and an optional journal.
//!
//! System-exclusive travels in the same list, divided into segments when a dump is longer than
//! one packet should carry (RFC 6295, section 3.2). A segment's first and last bytes say where it
//! falls in the dump, which is what lets a receiver put one back together.

use midi_harbor_core::midi::MidiMessage;

/// RTP version this payload format uses.
pub const RTP_VERSION: u8 = 2;

/// Payload type Apple's implementation uses for RTP-MIDI.
pub const PAYLOAD_TYPE: u8 = 97;

/// Bytes in a fixed RTP header with no contributing sources.
pub const RTP_HEADER_LEN: usize = 12;

/// Longest MIDI list a short command header can describe.
const SHORT_LENGTH_MAX: usize = 0x0F;

/// Longest MIDI list a long command header can describe.
const LONG_LENGTH_MAX: usize = 0x0FFF;

/// Most messages accepted from one packet, bounding work on hostile input.
const MAX_MESSAGES: usize = 1024;

/// Bytes a delta time may occupy.
const MAX_DELTA_BYTES: usize = 4;

/// Why an RTP-MIDI packet could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PacketError {
    /// The packet ended before a field it declared.
    #[error("packet is {actual} bytes, needs at least {expected}")]
    TooShort {
        /// How many bytes were present.
        actual: usize,
        /// How many were needed.
        expected: usize,
    },
    /// The RTP version is not the one this format uses.
    #[error("rtp version {0} is not supported")]
    UnsupportedVersion(u8),
    /// The payload type does not identify RTP-MIDI.
    #[error("payload type {0} is not rtp-midi")]
    UnexpectedPayloadType(u8),
    /// The command section declared a length that runs past the packet.
    #[error("midi list declares {declared} bytes but only {available} remain")]
    LengthOverrun {
        /// The declared length.
        declared: usize,
        /// What was actually left.
        available: usize,
    },
    /// A delta time ran longer than the format allows.
    #[error("delta time exceeds {MAX_DELTA_BYTES} bytes")]
    DeltaTooLong,
    /// A MIDI message in the list could not be parsed.
    #[error("malformed midi at offset {0}")]
    MalformedMidi(usize),
    /// The packet declared more messages than this implementation will accept.
    #[error("packet carries more than {MAX_MESSAGES} messages")]
    TooManyMessages,
    /// A system-exclusive segment reached the end of the list without its closing byte.
    #[error("system-exclusive at offset {0} is not closed")]
    UnterminatedSysEx(usize),
}

/// Where a system-exclusive segment falls in its dump, as its framing bytes say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysExPart {
    /// A whole dump in one segment: `F0 ... F7`.
    Whole,
    /// The first segment of a dump that continues: `F0 ... F0`.
    First,
    /// A segment between the first and last: `F7 ... F0`.
    Middle,
    /// The segment that ends a dump: `F7 ... F7`.
    Last,
    /// The sender abandoned the dump: a segment closed by `F4`.
    Cancelled,
}

impl SysExPart {
    /// Returns the byte that opens a segment in this part.
    fn opening(self) -> u8 {
        match self {
            Self::Whole | Self::First => SYSEX_START,
            Self::Middle | Self::Last | Self::Cancelled => SYSEX_END,
        }
    }

    /// Returns the byte that closes a segment in this part.
    fn closing(self) -> u8 {
        match self {
            Self::Whole | Self::Last => SYSEX_END,
            Self::First | Self::Middle => SYSEX_START,
            Self::Cancelled => SYSEX_CANCEL,
        }
    }

    /// Names the part a segment's opening and closing bytes describe.
    fn framed_by(opening: u8, closing: u8) -> Option<Self> {
        match (opening, closing) {
            (_, SYSEX_CANCEL) => Some(Self::Cancelled),
            (SYSEX_START, SYSEX_END) => Some(Self::Whole),
            (SYSEX_START, SYSEX_START) => Some(Self::First),
            (SYSEX_END, SYSEX_START) => Some(Self::Middle),
            (SYSEX_END, SYSEX_END) => Some(Self::Last),
            _ => None,
        }
    }
}

/// One system-exclusive segment in a packet's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysExSegment {
    /// How many of the packet's messages come before it, which places it among them.
    pub position: usize,
    /// Ticks since the previous command in this packet.
    pub delta: u32,
    /// Where in its dump this segment falls.
    pub part: SysExPart,
    /// The data bytes between the segment's framing bytes.
    pub payload: Vec<u8>,
}

/// Opens a dump, and closes a segment of one that continues.
const SYSEX_START: u8 = 0xF0;

/// Closes a dump, and opens a segment that continues one.
const SYSEX_END: u8 = 0xF7;

/// Closes a segment whose dump the sender abandoned.
const SYSEX_CANCEL: u8 = 0xF4;

/// Lowest status byte of a system real-time message, the only kind allowed inside a dump.
const REAL_TIME: u8 = 0xF8;

/// One MIDI message with the time since the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedMessage {
    /// Ticks since the previous message in this packet.
    pub delta: u32,
    /// The message itself.
    pub message: MidiMessage,
}

impl TimedMessage {
    /// Creates a message that happens at the same instant as the one before it.
    pub fn immediate(message: MidiMessage) -> Self {
        Self { delta: 0, message }
    }
}

/// A parsed RTP-MIDI packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpMidiPacket {
    /// Increments once per packet, and is what reveals loss.
    pub sequence: u16,
    /// The sender's clock when the first message in this packet occurred.
    pub timestamp: u32,
    /// Identifies the sending endpoint.
    pub ssrc: u32,
    /// The messages this packet carries, in order.
    pub messages: Vec<TimedMessage>,
    /// The system-exclusive segments this packet carries, each placed among the messages.
    pub sysex: Vec<SysExSegment>,
    /// The recovery journal, when the sender attached one.
    pub journal: Option<Vec<u8>>,
}

impl RtpMidiPacket {
    /// Creates a packet carrying the given messages and no journal.
    pub fn new(sequence: u16, timestamp: u32, ssrc: u32, messages: Vec<TimedMessage>) -> Self {
        Self {
            sequence,
            timestamp,
            ssrc,
            messages,
            sysex: Vec::new(),
            journal: None,
        }
    }

    /// Encodes the packet onto the wire.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(RTP_HEADER_LEN + 64);

        // Write the RTP header.
        out.push(RTP_VERSION << 6);
        out.push(PAYLOAD_TYPE);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());

        // The Z flag says the first command carries a delta time. Apple and rtpmidid leave out a
        // delta of zero, and rtpmidid logs a warning for every packet that carries one (R-071).
        let z_flag = first_delta(&self.messages, &self.sysex).is_some_and(|delta| delta != 0);

        // Build the MIDI list before its header, because the header carries its length.
        let list = encode_list(&self.messages, &self.sysex, z_flag);
        let has_journal = self.journal.is_some();
        if list.len() <= SHORT_LENGTH_MAX {
            let mut header = u8::try_from(list.len()).unwrap_or(0) & 0x0F;
            if has_journal {
                header |= 0x40;
            }
            if z_flag {
                header |= 0x20;
            }
            out.push(header);
        } else {
            let length = list.len().min(LONG_LENGTH_MAX);
            let mut first = 0x80 | u8::try_from(length >> 8).unwrap_or(0) & 0x0F;
            if has_journal {
                first |= 0x40;
            }
            if z_flag {
                first |= 0x20;
            }
            out.push(first);
            out.push(u8::try_from(length & 0xFF).unwrap_or(0));
        }

        out.extend_from_slice(&list);
        if let Some(journal) = &self.journal {
            out.extend_from_slice(journal);
        }
        out
    }

    /// Parses a packet received from a peer.
    pub fn parse(bytes: &[u8]) -> Result<Self, PacketError> {
        // Validate the RTP header.
        if bytes.len() < RTP_HEADER_LEN + 1 {
            return Err(PacketError::TooShort {
                actual: bytes.len(),
                expected: RTP_HEADER_LEN + 1,
            });
        }
        let first = bytes.first().copied().unwrap_or(0);
        let version = first >> 6;
        if version != RTP_VERSION {
            return Err(PacketError::UnsupportedVersion(version));
        }
        let payload_type = bytes.get(1).copied().unwrap_or(0) & 0x7F;
        if payload_type != PAYLOAD_TYPE {
            return Err(PacketError::UnexpectedPayloadType(payload_type));
        }

        let sequence = read_u16(bytes, 2)?;
        let timestamp = read_u32(bytes, 4)?;
        let ssrc = read_u32(bytes, 8)?;

        // Read the command section header, which may be one or two bytes.
        let header = bytes.get(RTP_HEADER_LEN).copied().unwrap_or(0);
        let long_form = header & 0x80 != 0;
        let has_journal = header & 0x40 != 0;
        let first_has_delta = header & 0x20 != 0;

        let (declared, list_start) = if long_form {
            let low = bytes
                .get(RTP_HEADER_LEN + 1)
                .copied()
                .ok_or(PacketError::TooShort {
                    actual: bytes.len(),
                    expected: RTP_HEADER_LEN + 2,
                })?;
            (
                usize::from(header & 0x0F) << 8 | usize::from(low),
                RTP_HEADER_LEN + 2,
            )
        } else {
            (usize::from(header & 0x0F), RTP_HEADER_LEN + 1)
        };

        let available = bytes.len().saturating_sub(list_start);
        if declared > available {
            return Err(PacketError::LengthOverrun {
                declared,
                available,
            });
        }
        let list_end = list_start.saturating_add(declared);
        let list = bytes.get(list_start..list_end).unwrap_or_default();

        let (messages, sysex) = decode_list(list, first_has_delta)?;
        let journal = if has_journal {
            bytes
                .get(list_end..)
                .filter(|tail| !tail.is_empty())
                .map(<[u8]>::to_vec)
        } else {
            None
        };

        Ok(Self {
            sequence,
            timestamp,
            ssrc,
            messages,
            sysex,
            journal,
        })
    }
}

/// Returns the delta time of the command `encode_list` writes first, if there is one.
fn first_delta(messages: &[TimedMessage], sysex: &[SysExSegment]) -> Option<u32> {
    sysex
        .iter()
        .find(|segment| segment.position == 0)
        .map(|segment| segment.delta)
        .or_else(|| messages.first().map(|timed| timed.delta))
        .or_else(|| sysex.first().map(|segment| segment.delta))
}

/// Encodes a list of timed messages and segments, applying running status where it saves a byte.
///
/// `first_delta` says whether the first command's delta time is written, as the Z flag declares.
fn encode_list(messages: &[TimedMessage], sysex: &[SysExSegment], first_delta: bool) -> Vec<u8> {
    let payload: usize = sysex.iter().map(|segment| segment.payload.len() + 3).sum();
    let mut out = Vec::with_capacity(messages.len() * 4 + payload);
    let mut running: Option<u8> = None;
    let mut with_delta = first_delta;

    for (index, timed) in messages.iter().enumerate() {
        for segment in sysex.iter().filter(|segment| segment.position == index) {
            encode_segment(segment, with_delta, &mut out);
            with_delta = true;
            running = None;
        }
        if with_delta {
            encode_delta(timed.delta, &mut out);
        }
        with_delta = true;

        let status = timed.message.status();
        // Running status may only be carried across channel messages; a system message breaks it.
        let omit_status = running == Some(status) && status < 0xF0;

        let mut buffer = [0u8; 3];
        let written = timed.message.encode(&mut buffer);
        if written == 0 {
            continue;
        }
        let encoded = buffer.get(..written).unwrap_or_default();

        if omit_status {
            if let Some(payload) = encoded.get(1..) {
                out.extend_from_slice(payload);
            }
        } else {
            out.extend_from_slice(encoded);
            running = (status < 0xF0).then_some(status);
        }
    }
    // Segments placed after every message, including any whose position is past the end.
    for segment in sysex
        .iter()
        .filter(|segment| segment.position >= messages.len())
    {
        encode_segment(segment, with_delta, &mut out);
        with_delta = true;
    }
    out
}

/// Encodes one system-exclusive segment with its framing bytes.
///
/// A byte with the high bit set would end the segment early on the receiver, so none is written
/// as payload.
fn encode_segment(segment: &SysExSegment, with_delta: bool, out: &mut Vec<u8>) {
    if with_delta {
        encode_delta(segment.delta, out);
    }
    out.push(segment.part.opening());
    out.extend(segment.payload.iter().copied().filter(|byte| *byte < 0x80));
    out.push(segment.part.closing());
}

/// Decodes a segment starting at `offset`, returning it, the real-time messages inside it, and
/// how many bytes it took.
fn decode_segment(
    list: &[u8],
    offset: usize,
    delta: u32,
    position: usize,
) -> Result<(SysExSegment, Vec<MidiMessage>, usize), PacketError> {
    let opening = list
        .get(offset)
        .copied()
        .ok_or(PacketError::UnterminatedSysEx(offset))?;
    let mut payload = Vec::new();
    let mut real_time = Vec::new();
    let mut at = offset.saturating_add(1);

    loop {
        let byte = list
            .get(at)
            .copied()
            .ok_or(PacketError::UnterminatedSysEx(offset))?;
        at = at.saturating_add(1);
        match byte {
            0x00..=0x7F => payload.push(byte),
            // Real-time messages may interrupt a dump; they are played, not kept in it.
            REAL_TIME..=0xFF => real_time.push(MidiMessage::System { status: byte }),
            _ => {
                let part = SysExPart::framed_by(opening, byte)
                    .ok_or(PacketError::MalformedMidi(at.saturating_sub(1)))?;
                let segment = SysExSegment {
                    position: position.saturating_add(real_time.len()),
                    delta,
                    part,
                    payload,
                };
                return Ok((segment, real_time, at.saturating_sub(offset)));
            }
        }
    }
}

/// Decodes a list of timed messages and system-exclusive segments.
fn decode_list(
    list: &[u8],
    first_has_delta: bool,
) -> Result<(Vec<TimedMessage>, Vec<SysExSegment>), PacketError> {
    let mut messages = Vec::new();
    let mut sysex = Vec::new();
    let mut offset = 0;
    let mut running: Option<u8> = None;
    let mut expect_delta = first_has_delta;

    while offset < list.len() {
        // Read this message's delta time, which the first message may omit.
        let delta = if expect_delta {
            let (value, used) = decode_delta(list, offset)?;
            offset = offset.saturating_add(used);
            value
        } else {
            0
        };
        expect_delta = true;

        if offset >= list.len() {
            break;
        }

        // A segment opens with the start or the end byte of a dump, and cancels running status.
        if matches!(list.get(offset), Some(&(SYSEX_START | SYSEX_END))) {
            let (segment, real_time, consumed) =
                decode_segment(list, offset, delta, messages.len())?;
            messages.extend(real_time.into_iter().map(TimedMessage::immediate));
            sysex.push(segment);
            if messages.len().saturating_add(sysex.len()) > MAX_MESSAGES {
                return Err(PacketError::TooManyMessages);
            }
            running = None;
            offset = offset.saturating_add(consumed);
            continue;
        }

        let rest = list.get(offset..).unwrap_or_default();
        let (message, consumed) =
            MidiMessage::parse(rest, running).ok_or(PacketError::MalformedMidi(offset))?;

        // A status byte present in the stream becomes the running status for what follows.
        if rest.first().copied().unwrap_or(0) >= 0x80 && message.status() < 0xF0 {
            running = Some(message.status());
        }

        messages.push(TimedMessage { delta, message });
        if messages.len().saturating_add(sysex.len()) > MAX_MESSAGES {
            return Err(PacketError::TooManyMessages);
        }
        // A message that consumes nothing would loop forever on malformed input.
        if consumed == 0 {
            return Err(PacketError::MalformedMidi(offset));
        }
        offset = offset.saturating_add(consumed);
    }
    Ok((messages, sysex))
}

/// Writes a delta time as one to four seven-bit groups, most significant first.
fn encode_delta(mut value: u32, out: &mut Vec<u8>) {
    // Four seven-bit groups is the format's maximum, so larger values saturate.
    let max = (1u32 << 28) - 1;
    value = value.min(max);

    let mut groups = [0u8; MAX_DELTA_BYTES];
    let mut count = 0;
    loop {
        if let Some(slot) = groups.get_mut(count) {
            *slot = u8::try_from(value & 0x7F).unwrap_or(0);
        }
        count += 1;
        value >>= 7;
        if value == 0 || count >= MAX_DELTA_BYTES {
            break;
        }
    }

    // Groups were produced least significant first, so emit them in reverse with the
    // continuation bit set on all but the last.
    for index in (0..count).rev() {
        let group = groups.get(index).copied().unwrap_or(0);
        let last = index == 0;
        out.push(if last { group } else { group | 0x80 });
    }
}

/// Reads a delta time, returning its value and how many bytes it used.
fn decode_delta(list: &[u8], offset: usize) -> Result<(u32, usize), PacketError> {
    let mut value = 0u32;
    for index in 0..MAX_DELTA_BYTES {
        let byte =
            list.get(offset.saturating_add(index))
                .copied()
                .ok_or(PacketError::TooShort {
                    actual: list.len(),
                    expected: offset + index + 1,
                })?;
        value = (value << 7) | u32::from(byte & 0x7F);
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    Err(PacketError::DeltaTooLong)
}

/// Reads a big-endian `u16` without risking an out-of-bounds read.
fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, PacketError> {
    let end = offset.saturating_add(2);
    bytes
        .get(offset..end)
        .and_then(|slice| slice.try_into().ok())
        .map(u16::from_be_bytes)
        .ok_or(PacketError::TooShort {
            actual: bytes.len(),
            expected: end,
        })
}

/// Reads a big-endian `u32` without risking an out-of-bounds read.
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PacketError> {
    let end = offset.saturating_add(4);
    bytes
        .get(offset..end)
        .and_then(|slice| slice.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or(PacketError::TooShort {
            actual: bytes.len(),
            expected: end,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi_harbor_core::midi::Channel;

    /// The RTP header of a packet with sequence 1, timestamp 2 and SSRC 3: version 2 in the top
    /// two bits of the first byte (`2 << 6 = 0x80`), then payload type 97 (`0x61`).
    const HEADER: [u8; RTP_HEADER_LEN] = [0x80, 0x61, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];

    fn note_on(note: u8, velocity: u8) -> MidiMessage {
        MidiMessage::NoteOn {
            channel: Channel::new(0).expect("channel 1 is in range"),
            note,
            velocity,
        }
    }

    fn timed(delta: u32, message: MidiMessage) -> TimedMessage {
        TimedMessage { delta, message }
    }

    fn segment(position: usize, part: SysExPart, payload: &[u8]) -> SysExSegment {
        SysExSegment {
            position,
            delta: 0,
            part,
            payload: payload.to_vec(),
        }
    }

    /// Returns [`HEADER`] followed by `rest`, a packet as it arrives from a peer.
    fn packet_bytes(rest: &[u8]) -> Vec<u8> {
        let mut bytes = HEADER.to_vec();
        bytes.extend_from_slice(rest);
        bytes
    }

    /// Packets encode to the RFC 6295 layout byte for byte, and parse back to themselves.
    ///
    /// The command section header (RFC 6295, section 3) is `B J Z P LEN(4)`, or with B set
    /// `LEN(12)` over two bytes. J (`0x40`) says a journal follows the list, and Z (`0x20`) says
    /// the first command carries a delta time. Apple and rtpmidid leave a delta of zero out, and
    /// rtpmidid warns on every packet that carries one (R-071). Running status repeats no status
    /// byte for the same channel message, and a system message or a dump cancels it, so the
    /// status after one is written again. Six notes one tick apart take `4 + 5 * 3 = 19 = 0x13`
    /// bytes, past the four-bit limit of fifteen, so the header takes B and a second byte.
    #[test]
    fn packets_encode_to_the_rfc_6295_layout_and_parse_back() {
        let with =
            |messages: Vec<TimedMessage>, sysex: Vec<SysExSegment>, journal: Option<Vec<u8>>| {
                let mut packet = RtpMidiPacket::new(1, 2, 3, messages);
                packet.sysex = sysex;
                packet.journal = journal;
                packet
            };
        let now = |message| timed(0, message);
        let cases: [(&str, RtpMidiPacket, Vec<u8>); 10] = [
            (
                "Apple's note on 60 from R-065",
                with(vec![now(note_on(60, 64))], vec![], None),
                vec![0x03, 0x90, 60, 64],
            ),
            (
                "a delayed first note",
                with(vec![timed(5, note_on(60, 64))], vec![], None),
                vec![0x24, 5, 0x90, 60, 64],
            ),
            (
                "running status across three notes",
                with(
                    vec![
                        now(note_on(60, 100)),
                        now(note_on(62, 100)),
                        now(note_on(64, 100)),
                    ],
                    vec![],
                    None,
                ),
                vec![0x09, 0x90, 60, 100, 0, 62, 100, 0, 64, 100],
            ),
            (
                "a system message between two notes",
                with(
                    vec![
                        now(note_on(60, 100)),
                        now(MidiMessage::System { status: 0xF8 }),
                        now(note_on(62, 100)),
                    ],
                    vec![],
                    None,
                ),
                vec![0x09, 0x90, 60, 100, 0, 0xF8, 0, 0x90, 62, 100],
            ),
            (
                "a whole dump alone",
                with(
                    vec![],
                    vec![segment(0, SysExPart::Whole, &[0x7E, 0x7F])],
                    None,
                ),
                vec![0x04, 0xF0, 0x7E, 0x7F, 0xF7],
            ),
            (
                "a dump ahead of a delayed note",
                with(
                    vec![timed(3, note_on(60, 64))],
                    vec![segment(0, SysExPart::Whole, &[0x7E])],
                    None,
                ),
                vec![0x07, 0xF0, 0x7E, 0xF7, 3, 0x90, 60, 64],
            ),
            (
                "a dump between two notes",
                with(
                    vec![now(note_on(60, 100)), now(note_on(62, 100))],
                    vec![segment(1, SysExPart::Whole, &[0x7E, 0x7F, 0x06, 0x01])],
                    None,
                ),
                vec![
                    0x0E, 0x90, 60, 100, 0, 0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7, 0, 0x90, 62, 100,
                ],
            ),
            (
                "a note with a journal",
                with(
                    vec![now(note_on(60, 100))],
                    vec![],
                    Some(vec![0xA1, 0xB2, 0xC3]),
                ),
                vec![0x43, 0x90, 60, 100, 0xA1, 0xB2, 0xC3],
            ),
            (
                "a list too long for the short header",
                with(
                    (60..66).map(|note| timed(1, note_on(note, 100))).collect(),
                    vec![],
                    None,
                ),
                vec![
                    0xA0, 0x13, 1, 0x90, 60, 100, 1, 61, 100, 1, 62, 100, 1, 63, 100, 1, 64, 100,
                    1, 65, 100,
                ],
            ),
            ("an empty packet", with(vec![], vec![], None), vec![0x00]),
        ];
        for (name, packet, list) in cases {
            let encoded = packet.encode();
            assert_eq!(
                encoded,
                packet_bytes(&list),
                "{name}: the encoding must match the RFC 6295 layout"
            );
            assert_eq!(
                RtpMidiPacket::parse(&encoded),
                Ok(packet),
                "{name}: the encoding must parse back to the same packet"
            );
        }
    }

    /// Delta times are one to four seven-bit groups, most significant first, with the high bit
    /// set on every group but the last (RFC 6295, section 3.1).
    ///
    /// Each width starts at a power of 128: `128 = 2^7` needs two groups, `16_384 = 2^14` three,
    /// `2_097_152 = 2^21` four. Four groups hold at most `2^28 - 1`, so a larger value saturates
    /// there rather than wrapping into a short, wrong delay.
    #[test]
    fn delta_times_use_one_to_four_seven_bit_groups() {
        let most = (1_u32 << 28) - 1;
        let cases: [(&str, u32, &[u8], u32); 6] = [
            ("the widest single group", 127, &[0x7F], 127),
            ("the narrowest two groups", 128, &[0x81, 0x00], 128),
            (
                "the narrowest three groups",
                16_384,
                &[0x81, 0x80, 0x00],
                16_384,
            ),
            (
                "the narrowest four groups",
                2_097_152,
                &[0x81, 0x80, 0x80, 0x00],
                2_097_152,
            ),
            (
                "the widest four groups",
                most,
                &[0xFF, 0xFF, 0xFF, 0x7F],
                most,
            ),
            (
                "past four groups",
                u32::MAX,
                &[0xFF, 0xFF, 0xFF, 0x7F],
                most,
            ),
        ];
        for (name, value, bytes, decoded) in cases {
            let mut encoded = Vec::new();
            encode_delta(value, &mut encoded);
            assert_eq!(
                encoded, bytes,
                "{name}: the delta must encode to these groups"
            );
            assert_eq!(
                decode_delta(&encoded, 0),
                Ok((decoded, bytes.len())),
                "{name}: the groups must decode to this delta and consume every byte"
            );
        }
    }

    /// A peer's list with system-exclusive in it parses into messages and segments, each segment
    /// placed among the messages and named by its framing bytes.
    ///
    /// RFC 6295 section 3.2 frames a whole dump `F0 ... F7`, a first segment `F0 ... F0`, a
    /// middle `F7 ... F0`, a last `F7 ... F7`, and a cancelled one closed by `F4`. A real-time
    /// byte may interrupt a dump; it is played and kept out of the dump, and counts as a message
    /// before the segment. The first row is a regression: the dump's first data byte was read as
    /// a message, and the whole packet, note included, was refused.
    #[test]
    fn a_peers_dumps_parse_into_placed_segments() {
        let cases = [
            (
                "a note then a whole dump",
                vec![0x09, 0x90, 0x3C, 0x40, 0x00, 0xF0, 0x7D, 0x01, 0x02, 0xF7],
                vec![timed(0, note_on(60, 64))],
                vec![segment(1, SysExPart::Whole, &[0x7D, 0x01, 0x02])],
            ),
            (
                "a first segment",
                vec![0x03, 0xF0, 0x01, 0xF0],
                vec![],
                vec![segment(0, SysExPart::First, &[0x01])],
            ),
            (
                "a middle segment",
                vec![0x03, 0xF7, 0x01, 0xF0],
                vec![],
                vec![segment(0, SysExPart::Middle, &[0x01])],
            ),
            (
                "a last segment",
                vec![0x03, 0xF7, 0x01, 0xF7],
                vec![],
                vec![segment(0, SysExPart::Last, &[0x01])],
            ),
            (
                "a cancelled segment",
                vec![0x03, 0xF7, 0x01, 0xF4],
                vec![],
                vec![segment(0, SysExPart::Cancelled, &[0x01])],
            ),
            (
                "a clock inside a dump",
                vec![0x05, 0xF0, 0x01, 0xF8, 0x02, 0xF7],
                vec![timed(0, MidiMessage::System { status: 0xF8 })],
                vec![segment(1, SysExPart::Whole, &[0x01, 0x02])],
            ),
        ];
        for (name, list, messages, sysex) in cases {
            let packet = RtpMidiPacket::parse(&packet_bytes(&list))
                .unwrap_or_else(|err| panic!("{name}: a well-formed list must parse, got {err}"));
            assert_eq!(
                packet.messages, messages,
                "{name}: these messages must be read"
            );
            assert_eq!(
                packet.sysex, sysex,
                "{name}: these segments must be read, in these places"
            );
        }
    }

    /// Parsing accepts each well-formed boundary and refuses the malformed input beside it,
    /// naming what was wrong.
    ///
    /// A declared length is the classic way to make a parser read past its buffer, so a list of
    /// exactly the remaining three bytes is accepted and a declared four is not. A dump must be
    /// closed, and nothing but data and real-time bytes may appear inside one: a status byte at
    /// offset 1 of the list closes nothing. Payload type 97 is RTP-MIDI's, and 96 is not.
    #[test]
    fn parse_accepts_each_boundary_and_refuses_the_input_past_it() {
        let mut wrong_payload_type = packet_bytes(&[0x00]);
        wrong_payload_type[1] = 96;
        let cases = [
            (
                "a list exactly as long as declared",
                packet_bytes(&[0x03, 0x90, 60, 64]),
                Ok(()),
            ),
            (
                "a list declared one byte longer than what remains",
                packet_bytes(&[0x04, 0x90, 60, 64]),
                Err(PacketError::LengthOverrun {
                    declared: 4,
                    available: 3,
                }),
            ),
            (
                "a closed dump",
                packet_bytes(&[0x03, 0xF0, 0x01, 0xF7]),
                Ok(()),
            ),
            (
                "a dump never closed",
                packet_bytes(&[0x03, 0xF0, 0x01, 0x02]),
                Err(PacketError::UnterminatedSysEx(0)),
            ),
            (
                "a status byte inside a dump",
                packet_bytes(&[0x03, 0xF0, 0x90, 0xF7]),
                Err(PacketError::MalformedMidi(1)),
            ),
            (
                "payload type 96",
                wrong_payload_type,
                Err(PacketError::UnexpectedPayloadType(96)),
            ),
        ];
        for (name, bytes, want) in cases {
            assert_eq!(
                RtpMidiPacket::parse(&bytes).map(|_| ()),
                want,
                "{name}: the parse must return this result"
            );
        }
    }

    /// Every truncation of a packet is refused, rather than read past its end or misread as a
    /// shorter packet.
    ///
    /// The command section header declares the list's length, so without a journal a packet cut
    /// anywhere short of its end is missing bytes the header promised.
    #[test]
    fn a_truncated_packet_is_refused() {
        let mut packet = RtpMidiPacket::new(
            1,
            2,
            3,
            vec![timed(0, note_on(60, 100)), timed(300, note_on(64, 100))],
        );
        packet.sysex = vec![segment(1, SysExPart::Whole, &[0x7E, 0x7F])];
        let encoded = packet.encode();

        for length in 0..encoded.len() {
            assert!(
                RtpMidiPacket::parse(&encoded[..length]).is_err(),
                "a packet cut to {length} of {} bytes must be refused",
                encoded.len()
            );
        }
    }
}
