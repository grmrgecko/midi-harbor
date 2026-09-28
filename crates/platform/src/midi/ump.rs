//! Translating between MIDI 1.0 bytes and Universal MIDI Packets.
//!
//! Windows MIDI Services carries everything as UMP, including MIDI 1.0: a channel voice message
//! becomes one 32-bit type 2 word, a system common or real-time message a type 1 word, and a
//! system-exclusive message a run of 64-bit type 3 packets holding up to six data bytes each. The
//! group number in each packet is which connector of a port it belongs to. The rest of Midi Harbor
//! works in MIDI 1.0 bytes, so this is the boundary.
//!
//! Pure functions over integers, so they are tested on every platform. Nothing here allocates,
//! because decoding runs in the receive callback.

/// UMP message type for utility messages, which carry no MIDI and are ignored.
const TYPE_UTILITY: u32 = 0x0;
/// UMP message type for system common and real-time messages.
const TYPE_SYSTEM: u32 = 0x1;
/// UMP message type for MIDI 1.0 channel voice messages.
const TYPE_MIDI1_CHANNEL_VOICE: u32 = 0x2;
/// UMP message type for 7-bit system-exclusive data.
const TYPE_SYSEX7: u32 = 0x3;

/// Where a type 3 packet falls in its message.
const SYSEX_COMPLETE: u32 = 0x0;
const SYSEX_START: u32 = 0x1;
const SYSEX_CONTINUE: u32 = 0x2;
const SYSEX_END: u32 = 0x3;

/// How many data bytes one type 3 packet carries at most.
pub const SYSEX_BYTES_PER_PACKET: usize = 6;

/// How many groups one endpoint has, and so how many connectors a port can have.
pub const GROUPS: u8 = 16;

/// Returns how many 32-bit words a packet takes, from the message type in its first word.
pub fn packet_words(first: u32) -> usize {
    match first >> 28 {
        0x0..=0x2 | 0x6 | 0x7 => 1,
        0x3 | 0x4 | 0x8..=0xA => 2,
        0xB | 0xC => 3,
        _ => 4,
    }
}

/// Returns the group a packet is addressed to, counting from zero.
pub fn group_of(first: u32) -> u8 {
    ((first >> 24) & 0xF) as u8
}

/// Encodes one MIDI 1.0 message that is not system-exclusive as a single UMP word on `group`.
///
/// Returns nothing for an empty message or one that starts with a data byte, which has no UMP
/// form on its own.
pub fn encode_message(message: &[u8], group: u8) -> Option<u32> {
    let status = u32::from(*message.first()?);
    let data = |index: usize| u32::from(message.get(index).copied().unwrap_or(0) & 0x7F);
    let kind = match status {
        0x80..=0xEF => TYPE_MIDI1_CHANNEL_VOICE,
        0xF1..=0xF6 | 0xF8..=0xFF => TYPE_SYSTEM,
        _ => return None,
    };
    Some(kind << 28 | (u32::from(group) & 0xF) << 24 | status << 16 | data(1) << 8 | data(2))
}

/// Encodes one whole system-exclusive message as type 3 packets on `group`, calling `emit` with
/// each packet's two words in order.
///
/// `message` includes the `0xF0` and `0xF7` framing, which UMP leaves out: its packets carry only
/// the data bytes between them.
pub fn encode_sysex(message: &[u8], group: u8, emit: &mut impl FnMut(u32, u32)) {
    let start = usize::from(message.first() == Some(&0xF0));
    let end = message.len() - usize::from(message.len() > start && message.last() == Some(&0xF7));
    let data = message.get(start..end).unwrap_or_default();

    let chunks = data.chunks(SYSEX_BYTES_PER_PACKET);
    let count = chunks.len();
    if count == 0 {
        // An empty dump is still a message: one complete packet with nothing in it.
        emit(sysex_header(group, SYSEX_COMPLETE, &[]), 0);
        return;
    }
    for (index, chunk) in chunks.enumerate() {
        let place = match (index == 0, index + 1 == count) {
            (true, true) => SYSEX_COMPLETE,
            (true, false) => SYSEX_START,
            (false, false) => SYSEX_CONTINUE,
            (false, true) => SYSEX_END,
        };
        let byte = |at: usize| u32::from(chunk.get(at).copied().unwrap_or(0) & 0x7F);
        emit(
            sysex_header(group, place, chunk),
            byte(2) << 24 | byte(3) << 16 | byte(4) << 8 | byte(5),
        );
    }
}

/// Builds the first word of a type 3 packet: its place in the message, its byte count, and its
/// first two data bytes.
fn sysex_header(group: u8, place: u32, chunk: &[u8]) -> u32 {
    let byte = |at: usize| u32::from(chunk.get(at).copied().unwrap_or(0) & 0x7F);
    TYPE_SYSEX7 << 28
        | (u32::from(group) & 0xF) << 24
        | place << 20
        | (chunk.len().min(SYSEX_BYTES_PER_PACKET) as u32) << 16
        | byte(0) << 8
        | byte(1)
}

/// Decodes one packet into MIDI 1.0 bytes, written into `out`, returning how many were written.
///
/// A system-exclusive packet gives its data bytes with the framing its place calls for: `0xF0`
/// before the first packet's and `0xF7` after the last's, so consecutive packets fed to a scanner
/// make the whole message. A packet with no MIDI 1.0 form, such as a utility message or MIDI 2.0
/// channel voice, gives nothing.
pub fn decode(words: &[u32], out: &mut [u8; 8]) -> usize {
    let Some(&first) = words.first() else {
        return 0;
    };
    let byte = |word: u32, shift: u32| ((word >> shift) & 0xFF) as u8;
    match first >> 28 {
        TYPE_MIDI1_CHANNEL_VOICE | TYPE_SYSTEM => {
            let status = byte(first, 16);
            let length = super::winmm_identity::short_length(status);
            let bytes = [status, byte(first, 8) & 0x7F, byte(first, 0) & 0x7F];
            let mut written = 0;
            for (slot, value) in out.iter_mut().zip(bytes.iter().take(length)) {
                *slot = *value;
                written += 1;
            }
            written
        }
        TYPE_SYSEX7 => {
            let second = words.get(1).copied().unwrap_or(0);
            let place = (first >> 20) & 0xF;
            let count = (((first >> 16) & 0xF) as usize).min(SYSEX_BYTES_PER_PACKET);
            let data = [
                byte(first, 8),
                byte(first, 0),
                byte(second, 24),
                byte(second, 16),
                byte(second, 8),
                byte(second, 0),
            ];
            let mut written = 0;
            let mut put = |value: u8| {
                if let Some(slot) = out.get_mut(written) {
                    *slot = value;
                    written += 1;
                }
            };
            if place == SYSEX_COMPLETE || place == SYSEX_START {
                put(0xF0);
            }
            for value in data.iter().take(count) {
                put(value & 0x7F);
            }
            if place == SYSEX_COMPLETE || place == SYSEX_END {
                put(0xF7);
            }
            written
        }
        TYPE_UTILITY => 0,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes one packet into the MIDI 1.0 bytes it gives.
    fn decoded(words: &[u32]) -> Vec<u8> {
        let mut out = [0u8; 8];
        let written = decode(words, &mut out);
        out.get(..written).unwrap_or_default().to_vec()
    }

    /// Locks the one-word layouts of the UMP specification (Universal MIDI Packet Format and MIDI
    /// 2.0 Protocol, M2-104-UM): message type in the top nibble, 2 for MIDI 1.0 channel voice and
    /// 1 for system common and real-time, then the group, the status byte and two data bytes.
    /// Each decodes back to the bytes it came from, at the length its status gives.
    ///
    /// A message beginning with a data byte, or with `0xF0`, has no one-word form: running status
    /// does not exist in UMP, and system-exclusive travels as type 3 packets.
    #[test]
    fn one_word_messages_have_the_spec_layout_and_decode_back() {
        let cases: [(&str, &[u8], u8, Option<u32>); 7] = [
            ("a note on, group 6", &[0x93, 60, 100], 5, Some(0x2593_3C64)),
            ("a program change", &[0xC0, 7], 0, Some(0x20C0_0700)),
            ("a clock, group 2", &[0xF8], 1, Some(0x11F8_0000)),
            ("a song position", &[0xF2, 0x10, 0x20], 0, Some(0x10F2_1020)),
            ("a bare data byte", &[0x40, 1], 0, None),
            ("a system-exclusive start", &[0xF0, 1, 0xF7], 0, None),
            ("nothing", &[], 0, None),
        ];
        for (name, message, group, want) in cases {
            let word = encode_message(message, group);
            assert_eq!(word, want, "{name}: the word must have the UMP layout");
            let Some(word) = word else {
                continue;
            };
            assert_eq!(group_of(word), group, "{name}: the group must be read back");
            assert_eq!(packet_words(word), 1, "{name}: types 1 and 2 are one word");
            assert_eq!(
                decoded(&[word]),
                message,
                "{name}: the word must decode to the bytes it came from"
            );
        }
    }

    /// Locks system-exclusive as UMP type 3 packets: up to six data bytes each, the `0xF0` and
    /// `0xF7` framing left out, and the packet's place in the nibble after the group (0 complete,
    /// 1 start, 2 continue, 3 end) with its byte count after that. Decoding the packets in order
    /// restores the framed message.
    ///
    /// Twenty data bytes take four packets, 6 + 6 + 6 + 2; twelve end on a full packet; an empty
    /// dump is still a message, one complete packet holding no bytes.
    #[test]
    fn system_exclusive_splits_into_type_3_packets_and_rejoins() {
        /// One dump and the packets it must become.
        struct Case<'a> {
            name: &'a str,
            data: &'a [u8],
            group: u8,
            want: &'a [(u32, u32)],
        }
        let twelve: Vec<u8> = (0..12).collect();
        let twenty: Vec<u8> = (0..20).collect();
        let cases = [
            Case {
                name: "an empty dump",
                data: &[],
                group: 0,
                want: &[(0x3000_0000, 0)],
            },
            Case {
                name: "three bytes on group 3",
                data: &[0x7D, 0x01, 0x02],
                group: 2,
                want: &[(0x3203_7D01, 0x0200_0000)],
            },
            Case {
                name: "twelve bytes",
                data: &twelve,
                group: 0,
                want: &[(0x3016_0001, 0x0203_0405), (0x3036_0607, 0x0809_0A0B)],
            },
            Case {
                name: "twenty bytes",
                data: &twenty,
                group: 0,
                want: &[
                    (0x3016_0001, 0x0203_0405),
                    (0x3026_0607, 0x0809_0A0B),
                    (0x3026_0C0D, 0x0E0F_1011),
                    (0x3032_1213, 0x0000_0000),
                ],
            },
        ];
        for Case {
            name,
            data,
            group,
            want,
        } in cases
        {
            let mut dump = vec![0xF0];
            dump.extend(data);
            dump.push(0xF7);
            let mut packets = Vec::new();
            encode_sysex(&dump, group, &mut |first, second| {
                packets.push((first, second))
            });
            assert_eq!(
                packets, want,
                "{name}: the packets must have the UMP layout"
            );

            let mut rejoined = Vec::new();
            for (first, second) in packets {
                assert_eq!(
                    packet_words(first),
                    2,
                    "{name}: type 3 packets are two words"
                );
                rejoined.extend(decoded(&[first, second]));
            }
            assert_eq!(
                rejoined, dump,
                "{name}: the packets must rejoin into the dump"
            );
        }
    }

    /// Locks packet sizes by message type, from the UMP specification's table: types 0 to 2 are
    /// one word, 3 and 4 two, 0xB three, and 5 and 0xF four. A packet with no MIDI 1.0 form,
    /// such as a utility message or a MIDI 2.0 note, decodes to nothing and is still skipped by
    /// its whole size, so the packets after it stay aligned.
    ///
    /// An empty read decodes to nothing rather than panicking.
    #[test]
    fn packets_are_sized_by_type_and_those_without_a_midi_1_form_decode_to_nothing() {
        let cases: [(&str, &[u32], Option<usize>); 6] = [
            ("a utility no-op", &[0x0000_0000], Some(1)),
            ("a MIDI 2.0 note on", &[0x4090_3C00, 0xFFFF_0000], Some(2)),
            ("a type 0xB packet", &[0xB000_0000, 0, 0], Some(3)),
            ("a type 5 data packet", &[0x5000_0000, 0, 0, 0], Some(4)),
            ("a UMP stream packet", &[0xF000_0000, 0, 0, 0], Some(4)),
            ("nothing", &[], None),
        ];
        for (name, words, want_words) in cases {
            assert_eq!(
                words.first().copied().map(packet_words),
                want_words,
                "{name}: the packet's size must follow its message type"
            );
            assert!(
                decoded(words).is_empty(),
                "{name}: a packet with no MIDI 1.0 form must decode to nothing"
            );
        }
    }
}
