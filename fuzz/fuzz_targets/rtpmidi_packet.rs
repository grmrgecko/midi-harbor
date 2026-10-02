//! Feeds hostile datagrams to the three RTP-MIDI parsers a peer on the network can reach.
//!
//! Both ports read whatever arrives, so the control parser, the identity parser and the data
//! parser each get every input. None may allocate more than a small multiple of what it was sent,
//! and whatever one accepts has to come back unchanged after being encoded and parsed again: a
//! packet the daemon would read one way and send another is a peer misreading us.

#![no_main]

use libfuzzer_sys::fuzz_target;
use midi_harbor_rtpmidi::{ControlPacket, IdentityPacket, RtpMidiPacket};

/// Heap bytes a parse may hold per byte of input, which covers a vector doubling past a message
/// list at one message per input byte.
const BYTES_PER_INPUT_BYTE: u64 = 64;

/// Heap bytes a parse may hold whatever its input, for fixed-size bookkeeping.
const FIXED_BYTES: u64 = 4096;

fuzz_target!(
    init: {
        // The allocation checks below prove nothing unless the counter is really installed.
        let probe = allocation_counter::measure(|| drop(std::hint::black_box(vec![0_u8; 64])));
        assert!(probe.count_total > 0, "the allocation counter is not installed");
    },
    |data: &[u8]| {
        let bound = bound(data);

        let mut control = None;
        let allocations = allocation_counter::measure(|| control = ControlPacket::parse(data).ok());
        assert!(allocations.bytes_max <= bound, "control parse held {allocations:?}");
        if let Some(packet) = control {
            assert_eq!(ControlPacket::parse(&packet.encode()), Ok(packet));
        }

        let mut identity = None;
        let allocations = allocation_counter::measure(|| identity = IdentityPacket::parse(data).ok());
        assert!(allocations.bytes_max <= bound, "identity parse held {allocations:?}");
        if let Some(packet) = identity {
            assert_eq!(IdentityPacket::parse(&packet.encode()), Ok(packet));
        }

        let mut rtp = None;
        let allocations = allocation_counter::measure(|| rtp = RtpMidiPacket::parse(data).ok());
        assert!(allocations.bytes_max <= bound, "rtp parse held {allocations:?}");
        if let Some(packet) = rtp {
            assert!(packet.messages.len() <= data.len(), "more messages than bytes");
            assert_eq!(RtpMidiPacket::parse(&packet.encode()), Ok(packet));
        }
    }
);

/// The most heap a parse of `data` may hold at once.
fn bound(data: &[u8]) -> u64 {
    u64::try_from(data.len())
        .unwrap_or(u64::MAX)
        .saturating_mul(BYTES_PER_INPUT_BYTE)
        .saturating_add(FIXED_BYTES)
}
