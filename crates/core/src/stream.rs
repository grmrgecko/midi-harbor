//! Splitting a run of raw MIDI bytes into whole messages and system-exclusive runs.
//!
//! Every platform backend receives bytes rather than messages, and every one of them has to solve
//! the same three problems: running status, system-exclusive messages that span several reads, and
//! real-time bytes that may appear in the middle of one. Solving them here means they are solved
//! once and can be tested on a machine with no MIDI hardware.
//!
//! Nothing in this module allocates, locks, or can panic, because it runs inside platform read
//! callbacks where none of those are permitted.

use crate::midi::MidiMessage;

/// How a run of system-exclusive bytes ends.
///
/// The distinction matters because an abandoned message must never be forwarded: a dump cut short
/// still looks like valid framing to a receiver, which will act on whatever arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysExEnd {
    /// More bytes of this message are still to come.
    Open,
    /// The terminator arrived, so the message is whole.
    Complete,
    /// The sender started something else mid-message, so what arrived is unusable.
    Abandoned,
}

/// One thing found in a run of raw MIDI bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chunk<'a> {
    /// A complete channel, system common or real-time message.
    Message(MidiMessage),
    /// Part or all of a system-exclusive message, framing bytes included.
    SysEx {
        /// The bytes, including `0xF0` on the first run and `0xF7` on the last.
        bytes: &'a [u8],
        /// Whether more is to come.
        end: SysExEnd,
    },
}

/// Splits raw MIDI bytes into messages, carrying the state that spans reads.
///
/// One scanner belongs to one source. Running status and an unfinished system-exclusive message
/// both continue across reads, so a scanner shared between two sources would splice one device's
/// dump onto another's.
#[derive(Debug, Default)]
pub struct Scanner {
    /// The status byte a data-only message inherits.
    running: Option<u8>,
    /// Whether a system-exclusive message is still open.
    in_sysex: bool,
}

impl Scanner {
    /// Creates a scanner with no message in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// Splits `data` into chunks, handing each to `emit` in the order it appeared.
    ///
    /// Emits through a closure rather than returning a collection because this runs on the
    /// real-time path, where a returned `Vec` would mean allocating in a read callback.
    pub fn scan<'a>(&mut self, data: &'a [u8], emit: &mut impl FnMut(Chunk<'a>)) {
        let mut index = 0;

        while index < data.len() {
            let Some(byte) = data.get(index).copied() else {
                return;
            };

            // Continue or begin a system-exclusive message.
            if self.in_sysex || byte == 0xF0 {
                if byte == 0xF0 {
                    // A dump that is still open when another begins was abandoned, and saying so
                    // is what stops a consumer gluing the two together.
                    if self.in_sysex {
                        emit(Chunk::SysEx {
                            bytes: &[],
                            end: SysExEnd::Abandoned,
                        });
                    }
                    // System common clears running status, and this is the loudest case of it.
                    self.running = None;
                    self.in_sysex = true;
                }
                let resume = self.sysex_run(data, index, emit);
                if resume <= index {
                    return;
                }
                index = resume;
                continue;
            }

            // Real-time bytes may appear anywhere and belong to no other message, so they never
            // touch running status.
            if byte >= 0xF8 {
                emit(Chunk::Message(MidiMessage::System { status: byte }));
                index = index.saturating_add(1);
                continue;
            }

            // A terminator with nothing open is the tail of a message whose start was lost. It
            // frames nothing, so passing it on would only confuse a receiver.
            if byte == 0xF7 {
                index = index.saturating_add(1);
                continue;
            }

            let Some(rest) = data.get(index..) else {
                return;
            };
            let Some((message, consumed)) = MidiMessage::parse(rest, self.running) else {
                // A status byte before this message was complete abandons it, and the status
                // starts the next one. Anything else is an incomplete tail or data with no status
                // to inherit, and the rest of this read cannot be trusted.
                let Some(next) = rest.iter().skip(1).position(|byte| *byte >= 0x80) else {
                    return;
                };
                index = index.saturating_add(next).saturating_add(1);
                continue;
            };
            if byte >= 0x80 {
                self.running = (message.status() < 0xF0).then_some(message.status());
            }
            emit(Chunk::Message(message));
            if consumed == 0 {
                return;
            }
            index = index.saturating_add(consumed);
        }
    }

    /// Emits one run of system-exclusive bytes, returning where scanning resumes.
    ///
    /// `start` points either at the `0xF0` that opens a message or at the first payload byte of a
    /// message already in progress.
    fn sysex_run<'a>(
        &mut self,
        data: &'a [u8],
        start: usize,
        emit: &mut impl FnMut(Chunk<'a>),
    ) -> usize {
        // Find where the payload stops, which is the first byte that is a status of any kind.
        let opens = data.get(start).copied() == Some(0xF0);
        let mut cursor = if opens {
            start.saturating_add(1)
        } else {
            start
        };
        while data.get(cursor).is_some_and(|byte| *byte < 0x80) {
            cursor = cursor.saturating_add(1);
        }

        match data.get(cursor).copied() {
            // The read ended mid-message, which is ordinary for a dump of any size.
            None => {
                emit_run(data, start, cursor, SysExEnd::Open, emit);
                cursor
            }
            // The terminator belongs to the message, so it travels with it.
            Some(0xF7) => {
                let end = cursor.saturating_add(1);
                emit_run(data, start, end, SysExEnd::Complete, emit);
                self.in_sysex = false;
                end
            }
            // A real-time byte may interrupt a dump without ending it.
            Some(status) if status >= 0xF8 => {
                emit_run(data, start, cursor, SysExEnd::Open, emit);
                emit(Chunk::Message(MidiMessage::System { status }));
                cursor.saturating_add(1)
            }
            // Anything else means the sender moved on and this dump will never be completed.
            Some(_) => {
                emit_run(data, start, cursor, SysExEnd::Abandoned, emit);
                self.in_sysex = false;
                cursor
            }
        }
    }
}

/// Emits one slice of system-exclusive bytes, skipping runs that say nothing.
///
/// An empty run still has to be reported when it ends a message, because that is how a consumer
/// learns to stop waiting or to discard what it has.
fn emit_run<'a>(
    data: &'a [u8],
    start: usize,
    end: usize,
    kind: SysExEnd,
    emit: &mut impl FnMut(Chunk<'a>),
) {
    let bytes = data.get(start..end).unwrap_or_default();
    if bytes.is_empty() && kind == SysExEnd::Open {
        return;
    }
    emit(Chunk::SysEx { bytes, end: kind });
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::midi::Channel;

    /// Collects everything a scan emits, copying system-exclusive runs so they outlive the input.
    #[derive(Debug, PartialEq, Eq)]
    enum Owned {
        Message(MidiMessage),
        SysEx(Vec<u8>, SysExEnd),
    }

    fn scan(scanner: &mut Scanner, data: &[u8]) -> Vec<Owned> {
        let mut out = Vec::new();
        scanner.scan(data, &mut |chunk| match chunk {
            Chunk::Message(message) => out.push(Owned::Message(message)),
            Chunk::SysEx { bytes, end } => out.push(Owned::SysEx(bytes.to_vec(), end)),
        });
        out
    }

    fn note_on(note: u8) -> Owned {
        Owned::Message(MidiMessage::NoteOn {
            channel: Channel::new(0).expect("channel 0"),
            note,
            velocity: 100,
        })
    }

    /// Raw bytes split across reads come out as the messages MIDI 1.0 says they are.
    ///
    /// A data byte inherits the last channel status, across reads, but system common clears it,
    /// and a dump is the loudest case of that. A terminator with nothing open is the tail of a
    /// dump whose start was lost and frames nothing. A dump begun while another is open abandons
    /// the first, which must be said or a consumer glues the two together. A status arriving
    /// before a message is complete abandons that message and starts the next. An empty read in
    /// the middle of a dump neither ends it nor loses its place. The dump cases the daemon
    /// forwards, whole, split, interrupted by clock and abandoned, are proved end to end in
    /// `crates/daemon/tests/sysex.rs`.
    #[test]
    fn raw_bytes_split_into_the_messages_they_are() {
        type Reads = &'static [&'static [u8]];
        let cases: [(&str, Reads, Vec<Owned>); 7] = [
            (
                "running status carries across reads",
                &[&[0x90, 0x3C, 0x64], &[0x3E, 0x64]],
                vec![note_on(0x3C), note_on(0x3E)],
            ),
            (
                "a dump clears running status",
                &[&[0x90, 0x3C, 0x64], &[0xF0, 0x7E, 0xF7], &[0x40, 0x64]],
                vec![
                    note_on(0x3C),
                    Owned::SysEx(vec![0xF0, 0x7E, 0xF7], SysExEnd::Complete),
                ],
            ),
            (
                "a terminator with nothing open is dropped",
                &[&[0xF7, 0x90, 0x3C, 0x64]],
                vec![note_on(0x3C)],
            ),
            (
                "a dump begun while another is open abandons the first",
                &[&[0xF0, 0x01], &[0xF0, 0x02, 0xF7]],
                vec![
                    Owned::SysEx(vec![0xF0, 0x01], SysExEnd::Open),
                    Owned::SysEx(vec![], SysExEnd::Abandoned),
                    Owned::SysEx(vec![0xF0, 0x02, 0xF7], SysExEnd::Complete),
                ],
            ),
            (
                "a status that interrupts a message starts the next one",
                &[&[0x90, 0x3C, 0x80, 0x3C, 0x00]],
                vec![Owned::Message(MidiMessage::NoteOff {
                    channel: Channel::new(0).expect("channel 0"),
                    note: 0x3C,
                    velocity: 0,
                })],
            ),
            (
                "a song position keeps both data bytes",
                &[&[0xF2, 0x10, 0x20]],
                vec![Owned::Message(MidiMessage::SystemCommon {
                    status: 0xF2,
                    data: [0x10, 0x20],
                })],
            ),
            (
                "an empty read neither ends a dump nor loses its place",
                &[&[], &[0xF0, 0x43], &[], &[0x10, 0xF7]],
                vec![
                    Owned::SysEx(vec![0xF0, 0x43], SysExEnd::Open),
                    Owned::SysEx(vec![0x10, 0xF7], SysExEnd::Complete),
                ],
            ),
        ];
        for (case, reads, want) in cases {
            let mut scanner = Scanner::new();
            let got: Vec<Owned> = reads
                .iter()
                .flat_map(|read| scan(&mut scanner, read))
                .collect();
            assert_eq!(
                got, want,
                "{case}: the bytes must come out as the messages they are"
            );
        }
    }

    /// Every pair of bytes, repeated, scans to an end without panicking or stalling.
    ///
    /// Device input is hostile, and this scanner runs inside platform read callbacks where a
    /// panic takes down the MIDI system's thread. No fuzz target reaches it, so all 65,536 byte
    /// pairs stand in for one.
    #[test]
    fn hostile_input_neither_panics_nor_stalls() {
        let mut scanner = Scanner::new();
        for first in 0u8..=255 {
            for second in 0u8..=255 {
                let _ = scan(&mut scanner, &[first, second, first, second]);
            }
        }
    }
}
