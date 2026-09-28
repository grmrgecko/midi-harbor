//! The Bluetooth LE MIDI packet format.
//!
//! BLE carries MIDI in short attribute writes rather than a byte stream, so the framing is its
//! own. Every packet opens with a header byte, each message inside carries a thirteen-bit
//! millisecond timestamp, and a system-exclusive dump is spread across as many packets as it
//! takes. None of that survives being treated as plain MIDI bytes: a timestamp byte and a status
//! byte are both `1xxxxxxx`, so a parser that does not track position turns timing information
//! into notes.
//!
//! The encoder is deliberately stricter than the decoder. It never uses running status, because
//! the byte it saves is worth less than being understood by every receiver, while the decoder
//! accepts running status because devices in the field send it.
//!
//! Nothing here allocates, locks, or can panic, because both directions run on the data path.

use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::stream::SysExEnd;
use uuid::Uuid;

/// The GATT service every BLE MIDI device carries.
pub const SERVICE_UUID: Uuid = Uuid::from_u128(0x03B8_0E5A_EDE8_4B33_A751_6CE3_4EC4_C700);

/// The characteristic MIDI travels over, in both directions.
pub const CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x7772_E5DB_3868_4112_A1A9_F266_9D10_6BF3);

/// How many milliseconds the thirteen-bit timestamp counts before returning to zero.
pub const TIMESTAMP_PERIOD: u64 = 8192;

/// The largest packet this encoder will build.
///
/// An attribute write cannot exceed the negotiated MTU, and no BLE link negotiates beyond this.
pub const MAX_PACKET: usize = 512;

/// The smallest packet worth building: a header, a timestamp and a three-byte message.
pub const MIN_PACKET: usize = 5;

/// Why a packet could not be read.
///
/// Peripheral input is hostile input, so every one of these is a refusal rather than a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The write carried no bytes at all.
    #[error("packet carried no header byte")]
    Empty,
    /// The first byte was a data byte, so the packet is not BLE MIDI framing.
    #[error("packet opened with {0:#04x} rather than a header byte")]
    NotAHeader(u8),
    /// The packet stopped part-way through a message.
    #[error("packet ended part-way through a message")]
    Truncated,
    /// A data byte arrived with no status for it to inherit.
    #[error("data byte {0:#04x} arrived with no status to inherit")]
    Orphaned(u8),
}

/// Why a system-exclusive message could not be packed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// The payload was not a whole system-exclusive message.
    #[error("payload is not framed by {:#04x} and {:#04x}", SYSEX_START, SYSEX_END)]
    NotFramed,
}

/// The byte that opens a system-exclusive message.
const SYSEX_START: u8 = 0xF0;

/// The byte that closes a system-exclusive message.
const SYSEX_END: u8 = 0xF7;

/// The lowest status byte that is a real-time message.
const FIRST_REALTIME: u8 = 0xF8;

/// One thing read out of a packet, with the time the sender stamped on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// A complete channel, system common or real-time message.
    Message {
        /// Milliseconds since this decoder started, with the counter's turnover already undone.
        at: u64,
        /// The message.
        message: MidiMessage,
    },
    /// Part or all of a system-exclusive message.
    SysEx {
        /// Milliseconds since this decoder started, with the counter's turnover already undone.
        at: u64,
        /// The bytes, including `0xF0` on the first run and `0xF7` on the last.
        bytes: &'a [u8],
        /// Whether more is to come.
        end: SysExEnd,
    },
}

/// Reads packets from one peripheral, carrying the state that spans them.
///
/// One decoder belongs to one link. The timestamp counter's turnover and an unfinished dump both
/// continue across packets, so a decoder shared between two devices would splice one device's
/// dump onto the other's and read both clocks as one.
#[derive(Debug, Default)]
pub struct Decoder {
    /// The last thirteen-bit timestamp seen, which is how the counter's turnover is noticed.
    last_ticks: Option<u16>,
    /// Milliseconds already accounted for by earlier turnovers.
    epoch: u64,
    /// Whether a dump begun in an earlier packet is still open.
    in_sysex: bool,
}

impl Decoder {
    /// Creates a decoder with no packet history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads one packet, handing each message to `emit` in the order it appeared.
    ///
    /// Emits through a closure rather than returning a collection because this runs on the data
    /// path, where a returned `Vec` would mean allocating per notification. Whatever was read
    /// before a malformed byte has already been emitted when the error is returned, because a
    /// packet that goes wrong half-way through still delivered the notes in its first half.
    pub fn decode<'a>(
        &mut self,
        packet: &'a [u8],
        emit: &mut impl FnMut(Event<'a>),
    ) -> Result<(), DecodeError> {
        // Read the header, which carries the high six bits every timestamp in this packet shares.
        let Some(header) = packet.first().copied() else {
            return Err(DecodeError::Empty);
        };
        if header < 0x80 {
            return Err(DecodeError::NotAHeader(header));
        }
        let mut clock = PacketClock {
            high: header & 0x3F,
            low: None,
        };
        let mut running: Option<u8> = None;
        let mut index = 1;

        while index < packet.len() {
            // A dump carries on until something ends it, whether it began in this packet, in an
            // earlier one, or before the real-time message that just interrupted it. Its payload
            // bytes carry no timestamps, so they are read here rather than below.
            if self.in_sysex {
                let resume = self.sysex_run(packet, index, false, self.now(), &mut clock, emit);
                if resume > index {
                    index = resume;
                    continue;
                }
                if self.in_sysex {
                    // The packet ended on a timestamp byte with nothing behind it.
                    return Err(DecodeError::Truncated);
                }
                // The dump was abandoned, and the byte it stopped on times what follows.
            }

            let Some(byte) = packet.get(index).copied() else {
                return Ok(());
            };

            // A byte with the high bit set here is a timestamp, not a status: the status byte it
            // introduces is the one after it.
            let at = if byte >= 0x80 {
                index = index.saturating_add(1);
                let ticks = clock.tick(byte);
                self.advance(ticks)
            } else {
                self.now()
            };

            let Some(status) = packet.get(index).copied() else {
                return Err(DecodeError::Truncated);
            };

            // Begin a dump, which may well outlast this packet. The loop above reads it, because
            // what follows the opening byte is the same continuation it already handles.
            if status == SYSEX_START {
                // System common clears running status, and this is the loudest case of it.
                running = None;
                self.in_sysex = true;
                let resume = self.sysex_run(packet, index, true, at, &mut clock, emit);
                if resume <= index {
                    return Ok(());
                }
                index = resume;
                continue;
            }

            // Real-time bytes belong to no other message, so they never touch running status.
            if status >= FIRST_REALTIME {
                emit(Event::Message {
                    at,
                    message: MidiMessage::System { status },
                });
                index = index.saturating_add(1);
                continue;
            }

            // A terminator with nothing open frames nothing, so passing it on would only confuse
            // a receiver.
            if status == SYSEX_END {
                index = index.saturating_add(1);
                continue;
            }

            let Some(rest) = packet.get(index..) else {
                return Ok(());
            };
            let Some((message, consumed)) = MidiMessage::parse(rest, running) else {
                return Err(if status < 0x80 && running.is_none() {
                    DecodeError::Orphaned(status)
                } else {
                    DecodeError::Truncated
                });
            };
            if status >= 0x80 {
                running = (message.status() < 0xF0).then_some(message.status());
            }
            emit(Event::Message { at, message });
            if consumed == 0 {
                return Ok(());
            }
            index = index.saturating_add(consumed);
        }
        Ok(())
    }

    /// Emits one run of system-exclusive bytes, returning where parsing resumes.
    ///
    /// `start` points either at the `0xF0` that opens a dump, which `opens` says, or at the first
    /// byte of one already in progress. The caller says which because the byte cannot: a
    /// continuation may begin with a timestamp, and a timestamp of 112 is `0xF0`. Unlike a plain
    /// MIDI stream, the terminator does not follow the payload
    /// directly — a timestamp byte sits between them — so it is emitted as a run of its own to
    /// keep the framing bytes travelling with the message.
    fn sysex_run<'a>(
        &mut self,
        packet: &'a [u8],
        start: usize,
        opens: bool,
        at: u64,
        clock: &mut PacketClock,
        emit: &mut impl FnMut(Event<'a>),
    ) -> usize {
        // Find where the payload stops, which is the first byte carrying a high bit.
        let mut cursor = if opens {
            start.saturating_add(1)
        } else {
            start
        };
        while packet.get(cursor).is_some_and(|byte| *byte < 0x80) {
            cursor = cursor.saturating_add(1);
        }

        // Nothing follows, which for a dump of any size is the ordinary case rather than a fault.
        if cursor >= packet.len() {
            emit_run(packet, start, cursor, at, SysExEnd::Open, emit);
            return cursor;
        }

        // The byte is a timestamp; the one after it says what interrupted the payload.
        let terminator = cursor.saturating_add(1);
        match packet.get(terminator).copied() {
            Some(SYSEX_END) => {
                emit_run(packet, start, cursor, at, SysExEnd::Open, emit);
                let ends_at = self.stamp(packet, cursor, clock, at);
                emit_run(
                    packet,
                    terminator,
                    terminator.saturating_add(1),
                    ends_at,
                    SysExEnd::Complete,
                    emit,
                );
                self.in_sysex = false;
                terminator.saturating_add(1)
            }
            // A real-time message may interrupt a dump without ending it.
            Some(status) if status >= FIRST_REALTIME => {
                emit_run(packet, start, cursor, at, SysExEnd::Open, emit);
                let sent_at = self.stamp(packet, cursor, clock, at);
                emit(Event::Message {
                    at: sent_at,
                    message: MidiMessage::System { status },
                });
                terminator.saturating_add(1)
            }
            // Anything else means the sender moved on and this dump will never be completed.
            Some(_) => {
                emit_run(packet, start, cursor, at, SysExEnd::Abandoned, emit);
                self.in_sysex = false;
                cursor
            }
            // The packet ended on a dangling timestamp byte. Resuming on it lets the caller
            // report the truncation rather than inventing a message to blame it on.
            None => {
                emit_run(packet, start, cursor, at, SysExEnd::Open, emit);
                cursor
            }
        }
    }

    /// Advances the clock over the timestamp byte at `index`, keeping `fallback` if it is not one.
    fn stamp(
        &mut self,
        packet: &[u8],
        index: usize,
        clock: &mut PacketClock,
        fallback: u64,
    ) -> u64 {
        match packet.get(index).copied() {
            Some(byte) if byte >= 0x80 => {
                let ticks = clock.tick(byte);
                self.advance(ticks)
            }
            _ => fallback,
        }
    }

    /// Turns a thirteen-bit packet timestamp into a millisecond count that only moves forward.
    ///
    /// The counter returns to zero every eight seconds, so a timestamp lower than the one before
    /// it is a turnover rather than time running backwards.
    fn advance(&mut self, ticks: u16) -> u64 {
        if self.last_ticks.is_some_and(|last| ticks < last) {
            self.epoch = self.epoch.saturating_add(TIMESTAMP_PERIOD);
        }
        self.last_ticks = Some(ticks);
        self.epoch.saturating_add(u64::from(ticks))
    }

    /// The time the last timestamp named, for bytes that carry none of their own.
    fn now(&self) -> u64 {
        self.epoch
            .saturating_add(u64::from(self.last_ticks.unwrap_or(0)))
    }
}

/// The timestamp bits one packet carries: six opened by its header, seven per message.
///
/// The low seven bits count to 128 milliseconds, which a single packet can outlast, so the high
/// bits carry rather than staying fixed for the whole packet.
struct PacketClock {
    /// The high six bits, as opened by the header and carried since.
    high: u8,
    /// The low seven bits most recently read, which is how a carry is noticed.
    low: Option<u8>,
}

impl PacketClock {
    /// Reads one timestamp byte, returning the thirteen-bit value it completes.
    fn tick(&mut self, byte: u8) -> u16 {
        let next = byte & 0x7F;
        if self.low.is_some_and(|previous| next < previous) {
            self.high = self.high.wrapping_add(1) & 0x3F;
        }
        self.low = Some(next);
        (u16::from(self.high) << 7) | u16::from(next)
    }
}

/// Emits one slice of system-exclusive bytes, skipping runs that say nothing.
///
/// An empty run still has to be reported when it ends a message, because that is how a consumer
/// learns to stop waiting or to discard what it has.
fn emit_run<'a>(
    packet: &'a [u8],
    start: usize,
    end: usize,
    at: u64,
    kind: SysExEnd,
    emit: &mut impl FnMut(Event<'a>),
) {
    let bytes = packet.get(start..end).unwrap_or_default();
    if bytes.is_empty() && kind == SysExEnd::Open {
        return;
    }
    emit(Event::SysEx {
        at,
        bytes,
        end: kind,
    });
}

/// Packs MIDI into packets no larger than the link will carry.
///
/// One encoder belongs to one link, because the packet being built and the timestamp bits in its
/// header are per-link state.
#[derive(Debug)]
pub struct Encoder {
    /// The packet being built, header byte included.
    buffer: [u8; MAX_PACKET],
    /// How much of the buffer is in use.
    used: usize,
    /// The largest packet this link will carry.
    capacity: usize,
    /// The seven timestamp bits most recently written into the open packet.
    low: Option<u8>,
}

impl Encoder {
    /// Creates an encoder for a link that will carry `capacity` bytes per write.
    ///
    /// The value is clamped rather than rejected: a link that negotiates an unusable MTU should
    /// send small packets, not refuse to send.
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: [0; MAX_PACKET],
            used: 0,
            capacity: capacity.clamp(MIN_PACKET, MAX_PACKET),
            low: None,
        }
    }

    /// Adds one message, emitting a packet whenever one fills.
    ///
    /// `at` is a millisecond count of the caller's choosing; only its position within the
    /// thirteen-bit cycle travels over the link.
    pub fn push(&mut self, at: u64, message: &MidiMessage, emit: &mut impl FnMut(&[u8])) {
        let mut encoded = [0_u8; 3];
        let len = message.encode(&mut encoded);
        let Some(bytes) = encoded.get(..len) else {
            return;
        };
        if bytes.is_empty() {
            return;
        }

        // Start a packet whose header can carry this timestamp, flushing one that cannot.
        self.open_for(at, bytes.len().saturating_add(1), emit);
        self.write_timestamp(at);
        for byte in bytes {
            self.write(*byte);
        }
    }

    /// Adds one whole system-exclusive message, spreading it over as many packets as it needs.
    ///
    /// The payload is the complete message, both framing bytes included, because a dump that is
    /// only half a message has no legitimate place on the wire.
    pub fn push_sysex(
        &mut self,
        at: u64,
        payload: &[u8],
        emit: &mut impl FnMut(&[u8]),
    ) -> Result<(), EncodeError> {
        // Validate the framing.
        if payload.first().copied() != Some(SYSEX_START)
            || payload.last().copied() != Some(SYSEX_END)
            || payload.len() < 2
        {
            return Err(EncodeError::NotFramed);
        }
        let Some(body) = payload.get(1..payload.len().saturating_sub(1)) else {
            return Err(EncodeError::NotFramed);
        };

        // Open the dump, which needs room for a timestamp and the start byte.
        self.open_for(at, 2, emit);
        self.write_timestamp(at);
        self.write(SYSEX_START);

        // Payload bytes carry no timestamps, so a continuation packet is a header and then data.
        for byte in body {
            if self.used >= self.capacity {
                self.flush(emit);
                self.write_header(at);
            }
            self.write(*byte);
        }

        // Close it, which needs room for a timestamp and the terminator together.
        if self.used.saturating_add(2) > self.capacity {
            self.flush(emit);
            self.write_header(at);
        }
        self.write_timestamp(at);
        self.write(SYSEX_END);
        Ok(())
    }

    /// Emits the packet being built, if there is one.
    ///
    /// A packet is never held back waiting for company: latency is the whole point of this
    /// transport, so the caller flushes as soon as it has nothing more to add.
    pub fn flush(&mut self, emit: &mut impl FnMut(&[u8])) {
        if self.used == 0 {
            return;
        }
        if let Some(packet) = self.buffer.get(..self.used) {
            emit(packet);
        }
        self.used = 0;
        self.low = None;
    }

    /// Makes room for `needed` bytes at timestamp `at`, flushing the open packet if it cannot.
    ///
    /// A packet's header fixes the high six bits of every timestamp in it, so a message that has
    /// moved past that window needs a packet of its own even when the old one has space.
    fn open_for(&mut self, at: u64, needed: usize, emit: &mut impl FnMut(&[u8])) {
        let ticks = ticks_of(at);
        let high = high_bits(ticks);
        let low = low_bits(ticks);

        let header_fits = self
            .buffer
            .first()
            .is_some_and(|byte| *byte == (0x80 | high));
        // A timestamp lower than the last one in this packet would read as the counter turning
        // over, which would put the message eight seconds into the future.
        let ordered = self.low.is_none_or(|previous| low >= previous);

        if self.used > 0 && (!header_fits || !ordered) {
            self.flush(emit);
        }
        if self.used > 0 && self.used.saturating_add(needed) > self.capacity {
            self.flush(emit);
        }
        if self.used == 0 {
            self.write_header(at);
        }
    }

    /// Writes the header byte that opens a packet.
    fn write_header(&mut self, at: u64) {
        self.write(0x80 | high_bits(ticks_of(at)));
        self.low = None;
    }

    /// Writes the timestamp byte that precedes a message.
    fn write_timestamp(&mut self, at: u64) {
        let low = low_bits(ticks_of(at));
        self.write(0x80 | low);
        self.low = Some(low);
    }

    /// Appends one byte, dropping it rather than overrunning the buffer.
    ///
    /// Callers reserve space before writing, so a drop here means a bug above rather than a full
    /// link, and losing the byte is still better than a panic on the data path.
    fn write(&mut self, byte: u8) {
        if let Some(slot) = self.buffer.get_mut(self.used) {
            *slot = byte;
            self.used = self.used.saturating_add(1);
        }
    }
}

/// The position of a millisecond count within the thirteen-bit timestamp cycle.
fn ticks_of(at: u64) -> u16 {
    (at % TIMESTAMP_PERIOD) as u16
}

/// The six timestamp bits carried in a packet's header byte.
fn high_bits(ticks: u16) -> u8 {
    ((ticks >> 7) & 0x3F) as u8
}

/// The seven timestamp bits carried in a message's timestamp byte.
fn low_bits(ticks: u16) -> u8 {
    (ticks & 0x7F) as u8
}
