//! Feeds hostile notifications to the BLE MIDI decoder, the way a peripheral in radio range can.
//!
//! Beyond not panicking, the decoder must allocate nothing at all, because it runs on every
//! notification a link delivers. What it hands on has to be something the rest of the daemon can
//! trust: times that never run backwards, dumps framed and seven-bit clean, and messages that
//! survive being sent back out through the encoder.

#![no_main]

use libfuzzer_sys::arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use midi_harbor_blemidi::{Decoder, Encoder, Event};
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::stream::SysExEnd;

/// A run of notifications from one peripheral, which is what a decoder's state spans.
#[derive(Debug, Arbitrary)]
struct Input {
    /// The write size the encoder is built with when the messages are sent back out.
    capacity: u16,
    /// The notifications, in the order they arrived.
    packets: Vec<Vec<u8>>,
}

/// What one decoder made of the run.
#[derive(Default)]
struct Received {
    messages: Vec<MidiMessage>,
    dumps: Vec<Vec<u8>>,
}

fuzz_target!(
    init: {
        // The allocation checks below prove nothing unless the counter is really installed.
        let probe = allocation_counter::measure(|| drop(std::hint::black_box(vec![0_u8; 64])));
        assert!(probe.count_total > 0, "the allocation counter is not installed");
    },
    |input: Input| {
        let received = decode_checked(&input.packets);

        // Send everything back out and read it again. The decoder accepted these, so the encoder
        // has to be able to carry them, and a fresh decoder has to read the same thing.
        let mut encoder = Encoder::new(usize::from(input.capacity));
        let mut wire: Vec<Vec<u8>> = Vec::new();
        for message in &received.messages {
            encoder.push(0, message, &mut |packet| wire.push(packet.to_vec()));
        }
        for dump in &received.dumps {
            let framed =
                encoder.push_sysex(0, dump, &mut |packet| wire.push(packet.to_vec()));
            assert!(framed.is_ok(), "a dump the decoder completed was refused by the encoder");
        }
        encoder.flush(&mut |packet| wire.push(packet.to_vec()));

        let again = decode_checked(&wire);
        assert_eq!(again.messages, received.messages);
        assert_eq!(again.dumps, received.dumps);
    }
);

/// Decodes a run of packets, checking every property the daemon relies on as it goes.
fn decode_checked(packets: &[Vec<u8>]) -> Received {
    let mut received = Received::default();
    let mut decoder = Decoder::new();
    // A second decoder fed the same bytes, so the first can be measured with nothing in its
    // closure that allocates. Both are deterministic, so they see the same events.
    let mut collecting = Decoder::new();
    let mut last_at = 0_u64;
    let mut dump: Option<Vec<u8>> = None;

    for packet in packets {
        let mut events = 0_usize;
        let mut dump_bytes = 0_usize;
        let mut ordered = true;
        let allocations = allocation_counter::measure(|| {
            let _ = decoder.decode(packet, &mut |event| {
                events = events.saturating_add(1);
                let at = match event {
                    Event::Message { at, .. } => at,
                    Event::SysEx { at, bytes, .. } => {
                        dump_bytes = dump_bytes.saturating_add(bytes.len());
                        at
                    }
                };
                ordered &= at >= last_at;
                last_at = at;
            });
        });
        assert_eq!(allocations.count_total, 0, "decoding allocated");
        assert!(ordered, "a timestamp ran backwards");
        assert!(events <= packet.len(), "more events than bytes");
        assert!(dump_bytes <= packet.len(), "more dump bytes than were sent");

        let _ = collecting.decode(packet, &mut |event| match event {
            Event::Message { message, .. } => received.messages.push(message),
            Event::SysEx { bytes, end, .. } => {
                let open = dump.get_or_insert_with(Vec::new);
                open.extend_from_slice(bytes);
                match end {
                    SysExEnd::Open => {}
                    SysExEnd::Abandoned => dump = None,
                    SysExEnd::Complete => {
                        if let Some(whole) = dump.take() {
                            check_framed(&whole);
                            received.dumps.push(whole);
                        }
                    }
                }
            }
        });
    }
    received
}

/// A complete dump opens and closes with its framing bytes and carries nothing but data between.
fn check_framed(dump: &[u8]) {
    assert_eq!(dump.first(), Some(&0xF0), "a dump did not open with 0xF0");
    assert_eq!(dump.last(), Some(&0xF7), "a dump did not close with 0xF7");
    let body = dump.get(1..dump.len().saturating_sub(1)).unwrap_or_default();
    assert!(body.iter().all(|byte| *byte < 0x80), "a status byte inside a dump");
}
