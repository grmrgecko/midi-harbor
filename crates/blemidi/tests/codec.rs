//! The three parts of BLE MIDI framing that are known to break in the field.
//!
//! A thirteen-bit millisecond counter turns over every eight seconds, running status may be sent
//! by a device but must not be assumed to survive a packet boundary, and a dump larger than the
//! MTU is spread over packets that carry no timestamps of their own. Each is a place where a
//! plausible-looking parser produces plausible-looking nonsense, so each gets a test that would
//! fail rather than merely producing different notes.
//!
//! Byte layouts follow the BLE-MIDI 1.0 specification (MMA/AMEI, 2015): a header byte
//! `10hhhhhh` carrying the high six timestamp bits, then a timestamp byte `1lllllll` carrying the
//! low seven before each message, so a timestamp in milliseconds is `high * 128 + low`.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use midi_harbor_blemidi::codec::{
    DecodeError, Decoder, EncodeError, Encoder, Event, MAX_PACKET, MIN_PACKET, TIMESTAMP_PERIOD,
};
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::stream::SysExEnd;
use proptest::prelude::*;

/// What a decode produced, copied so system-exclusive runs outlive the packet they came from.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Read {
    Message(u64, MidiMessage),
    SysEx(u64, Vec<u8>, SysExEnd),
}

/// Reads one packet, returning everything it carried and any complaint about how it ended.
fn decode(decoder: &mut Decoder, packet: &[u8]) -> (Vec<Read>, Result<(), DecodeError>) {
    let mut out = Vec::new();
    let result = decoder.decode(packet, &mut |event| match event {
        Event::Message { at, message } => out.push(Read::Message(at, message)),
        Event::SysEx { at, bytes, end } => out.push(Read::SysEx(at, bytes.to_vec(), end)),
    });
    (out, result)
}

/// Encodes a run of timestamped messages into whole packets.
fn encode(capacity: usize, messages: &[(u64, MidiMessage)]) -> Vec<Vec<u8>> {
    let mut encoder = Encoder::new(capacity);
    let mut packets = Vec::new();
    for (at, message) in messages {
        encoder.push(*at, message, &mut |packet| packets.push(packet.to_vec()));
    }
    encoder.flush(&mut |packet| packets.push(packet.to_vec()));
    packets
}

/// Reads a whole run of packets through one decoder, as a link would.
fn decode_all(packets: &[Vec<u8>]) -> Vec<Read> {
    let mut decoder = Decoder::new();
    let mut out = Vec::new();
    for packet in packets {
        let (read, result) = decode(&mut decoder, packet);
        assert_eq!(
            result,
            Ok(()),
            "packet {packet:02x?} came from the encoder, so it must decode"
        );
        out.extend(read);
    }
    out
}

fn channel(index: u8) -> Channel {
    Channel::new(index % 16).expect("index is masked into range")
}

fn note_on(note: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        channel: channel(0),
        note,
        velocity: 100,
    }
}

prop_compose! {
    /// Any message the encoder can carry, which is every channel message plus real time.
    fn any_message()(
        kind in 0_u8..8,
        channel_index in 0_u8..16,
        first in 0_u8..128,
        second in 0_u8..128,
        realtime in 0xF8_u8..=0xFF,
    ) -> MidiMessage {
        let channel = channel(channel_index);
        match kind {
            0 => MidiMessage::NoteOff { channel, note: first, velocity: second },
            1 => MidiMessage::NoteOn { channel, note: first, velocity: second },
            2 => MidiMessage::PolyAftertouch { channel, note: first, pressure: second },
            3 => MidiMessage::ControlChange { channel, controller: first, value: second },
            4 => MidiMessage::ProgramChange { channel, program: first },
            5 => MidiMessage::ChannelAftertouch { channel, pressure: first },
            6 => MidiMessage::PitchBend {
                channel,
                value: (u16::from(second) << 7) | u16::from(first),
            },
            _ => MidiMessage::System { status: realtime },
        }
    }
}

proptest! {
    /// Whatever goes in comes out, in order, however the packets happen to divide, and every
    /// packet stands alone.
    ///
    /// Packets are separate attribute writes and any one can be lost, so the encoder never uses
    /// running status: byte 2 of every packet, after the header and the first timestamp, must be
    /// a status byte. A receiver that saw only that packet can then still read it.
    #[test]
    fn every_message_survives_the_round_trip(
        messages in prop::collection::vec(any_message(), 1..40),
        capacity in MIN_PACKET..=64_usize,
    ) {
        // Messages are spaced a millisecond apart so the run crosses packet boundaries on
        // timestamp changes as well as on capacity.
        let timed: Vec<_> = messages
            .iter()
            .enumerate()
            .map(|(step, message)| (step as u64, *message))
            .collect();

        let packets = encode(capacity, &timed);
        for packet in &packets {
            prop_assert!(
                packet.len() <= capacity,
                "packet {:02x?} is longer than the {} bytes the link can write", packet, capacity
            );
            prop_assert!(
                packet[2] & 0x80 == 0x80,
                "packet {:02x?} opens on a data byte, relying on status from an earlier packet",
                packet
            );
        }

        let read = decode_all(&packets);
        let recovered: Vec<_> = read
            .iter()
            .map(|entry| match entry {
                Read::Message(at, message) => (*at, *message),
                Read::SysEx(..) => panic!("no dump was sent, so none can be read"),
            })
            .collect();
        prop_assert_eq!(recovered, timed, "the decoder must return exactly what was encoded");
    }

    /// Timing survives the thirteen-bit counter returning to zero, however far into the cycle a
    /// run starts.
    ///
    /// The counter holds `2^13 = 8192` milliseconds, so it turns over every 8.192 seconds. Four
    /// notes a second apart span three seconds, and whether the counter turns over part-way
    /// through depends entirely on where the run started.
    #[test]
    fn timestamps_stay_ordered_across_the_turnover(start in 0_u64..TIMESTAMP_PERIOD) {
        let timed: Vec<_> = (0..4)
            .map(|step| (start + step * 1000, note_on(60)))
            .collect();

        let read = decode_all(&encode(MAX_PACKET, &timed));
        let times: Vec<u64> = read
            .iter()
            .map(|entry| match entry {
                Read::Message(at, _) => *at,
                Read::SysEx(at, ..) => *at,
            })
            .collect();

        // The decoder counts from its own zero rather than the sender's, so what must hold is the
        // spacing, not the absolute value.
        prop_assert_eq!(times.len(), timed.len(), "every note sent must be read back");
        for pair in times.windows(2) {
            prop_assert_eq!(
                pair[1] - pair[0], 1000,
                "a one-second gap was lost across the turnover: {:?}", times
            );
        }
    }

    /// A dump of any size arrives byte-identical however many packets it took.
    ///
    /// Continuation packets carry a header and then data with no timestamp bytes, so a decoder
    /// that expected timestamps in them would eat every other byte of the dump.
    #[test]
    fn a_dump_split_across_packets_is_rejoined_byte_for_byte(
        body in prop::collection::vec(0_u8..128, 0..600),
        capacity in MIN_PACKET..=64_usize,
    ) {
        let mut payload = vec![0xF0];
        payload.extend_from_slice(&body);
        payload.push(0xF7);

        let mut encoder = Encoder::new(capacity);
        let mut packets = Vec::new();
        encoder
            .push_sysex(1234, &payload, &mut |packet| packets.push(packet.to_vec()))
            .expect("the payload is framed by F0 and F7, so the encoder must accept it");
        encoder.flush(&mut |packet| packets.push(packet.to_vec()));

        prop_assert!(
            packets.iter().all(|packet| packet.len() <= capacity),
            "a dump packet is longer than the link can write"
        );

        let mut rejoined = Vec::new();
        let mut ended = None;
        for entry in decode_all(&packets) {
            match entry {
                Read::SysEx(_, bytes, end) => {
                    rejoined.extend_from_slice(&bytes);
                    if end != SysExEnd::Open {
                        ended = Some(end);
                    }
                }
                Read::Message(_, message) => panic!("a dump alone produced {message:?}"),
            }
        }
        prop_assert_eq!(ended, Some(SysExEnd::Complete), "the dump must be read as complete");
        prop_assert_eq!(rejoined, payload, "the dump must be rejoined byte for byte");
    }
}

/// Packets as devices write them decode as the BLE-MIDI 1.0 specification frames them, and the
/// malformed ones are refused rather than guessed at.
///
/// Running status is accepted inside a packet, because devices send it, but not across a packet
/// boundary, because a lost packet between the two would attach the data to the wrong status. A
/// real-time byte may interrupt a dump with its own timestamp byte before it, which is the shape a
/// naive parser mistakes for the terminator. A dump interrupted by a channel message was
/// abandoned by its sender and must say so, or a receiver acts on the fragment. Timestamps in the
/// wants are `high * 128 + low` from the header and timestamp bytes: `0x82, 0xC0` is
/// `2 * 128 + 64 = 320`, and a timestamp byte of `0xF0` is `0x70 = 112`.
#[test]
fn device_packets_decode_as_the_specification_frames_them() {
    struct Case {
        name: &'static str,
        packets: &'static [&'static [u8]],
        want: Vec<Read>,
        want_last: Result<(), DecodeError>,
    }
    let cases = [
        Case {
            name: "running status inside one packet",
            packets: &[&[0x82, 0xC0, 0x90, 60, 100, 62, 100]],
            want: vec![
                Read::Message(320, note_on(60)),
                Read::Message(320, note_on(62)),
            ],
            want_last: Ok(()),
        },
        Case {
            name: "running status carried across a packet boundary",
            packets: &[&[0x80, 0x80, 0x90, 60, 100], &[0x80, 0x80, 62, 100]],
            want: vec![Read::Message(0, note_on(60))],
            want_last: Err(DecodeError::Orphaned(62)),
        },
        Case {
            name: "a clock inside a dump",
            packets: &[&[
                0x80, 0x80, 0xF0, 0x43, 0x10, 0x81, 0xF8, 0x44, 0x45, 0x81, 0xF7,
            ]],
            want: vec![
                Read::SysEx(0, vec![0xF0, 0x43, 0x10], SysExEnd::Open),
                Read::Message(1, MidiMessage::System { status: 0xF8 }),
                Read::SysEx(1, vec![0x44, 0x45], SysExEnd::Open),
                Read::SysEx(1, vec![0xF7], SysExEnd::Complete),
            ],
            want_last: Ok(()),
        },
        Case {
            name: "a dump abandoned for a note",
            packets: &[&[0x80, 0x80, 0xF0, 0x43, 0x10, 0x81, 0x90, 60, 100]],
            want: vec![
                Read::SysEx(0, vec![0xF0, 0x43, 0x10], SysExEnd::Abandoned),
                Read::Message(1, note_on(60)),
            ],
            want_last: Ok(()),
        },
        Case {
            name: "a continuation whose timestamp byte reads as 0xF0",
            packets: &[&[0x80, 0x80, 0xF0, 0x43], &[0x80, 0xF0, 0xF7]],
            want: vec![
                Read::SysEx(0, vec![0xF0, 0x43], SysExEnd::Open),
                Read::SysEx(112, vec![0xF7], SysExEnd::Complete),
            ],
            want_last: Ok(()),
        },
        Case {
            name: "an empty write",
            packets: &[&[]],
            want: vec![],
            want_last: Err(DecodeError::Empty),
        },
        Case {
            name: "a write opening on a data byte",
            packets: &[&[0x00, 0x90, 60, 100]],
            want: vec![],
            want_last: Err(DecodeError::NotAHeader(0x00)),
        },
        Case {
            name: "a timestamp with no message after it",
            packets: &[&[0x80, 0x80]],
            want: vec![],
            want_last: Err(DecodeError::Truncated),
        },
    ];

    for case in cases {
        let mut decoder = Decoder::new();
        let mut read = Vec::new();
        let mut last = Ok(());
        for packet in case.packets {
            let (events, result) = decode(&mut decoder, packet);
            read.extend(events);
            last = result;
        }
        assert_eq!(
            read, case.want,
            "{}: the decoder must read exactly these events",
            case.name
        );
        assert_eq!(
            last, case.want_last,
            "{}: the last packet must end with this result",
            case.name
        );
    }
}

/// The encoder sends a dump only when it is a whole message, framed by `F0` and `F7`.
///
/// Half a dump on the wire is worse than none: the receiver waits for a terminator that is never
/// coming and swallows everything after it. The smallest framed dump, `F0 F7`, is the accepted
/// boundary, and goes out as header, timestamp, `F0`, timestamp, `F7`, with every timestamp zero.
#[test]
fn only_a_framed_dump_is_sent() {
    let cases = [
        (
            "the smallest framed dump",
            vec![0xF0, 0xF7],
            Ok(()),
            vec![vec![0x80, 0x80, 0xF0, 0x80, 0xF7]],
        ),
        (
            "a dump with its terminator missing",
            vec![0xF0, 0x43, 0x10],
            Err(EncodeError::NotFramed),
            vec![],
        ),
        (
            "a dump with its opening byte missing",
            vec![0x43, 0x10, 0xF7],
            Err(EncodeError::NotFramed),
            vec![],
        ),
        (
            "an opening byte alone",
            vec![0xF0],
            Err(EncodeError::NotFramed),
            vec![],
        ),
    ];

    for (name, payload, want, want_packets) in cases {
        let mut encoder = Encoder::new(MAX_PACKET);
        let mut packets = Vec::new();
        let result = encoder.push_sysex(0, &payload, &mut |packet| packets.push(packet.to_vec()));
        encoder.flush(&mut |packet| packets.push(packet.to_vec()));

        assert_eq!(result, want, "{name}: the framing decides acceptance");
        assert_eq!(
            packets, want_packets,
            "{name}: only an accepted dump may reach the wire"
        );
    }
}
