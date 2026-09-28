//! Feeds hostile recovery journals to the decoder, and what it accepts to recovery.
//!
//! A journal arrives on the tail of any data packet, and recovery runs whenever a packet was lost,
//! so both are reachable by a peer that simply lies. Neither may allocate more than a small
//! multiple of the journal's size, and a decoded journal has to encode back to one that decodes
//! the same.

#![no_main]

use libfuzzer_sys::fuzz_target;
use midi_harbor_rtpmidi::journal::RecoveryJournal;

/// Channels a journal can describe, which the format caps with a four-bit count.
const MAX_CHANNELS: usize = 16;

/// Messages recovery may produce per journal octet. The densest chapter is a note-off bitmap,
/// which releases eight notes per octet.
const MESSAGES_PER_OCTET: usize = 8;

/// Heap bytes decoding and recovery may hold per journal octet.
const BYTES_PER_OCTET: u64 = 256;

/// Heap bytes decoding and recovery may hold whatever the input, for fixed-size bookkeeping.
const FIXED_BYTES: u64 = 8192;

fuzz_target!(
    init: {
        // The allocation checks below prove nothing unless the counter is really installed.
        let probe = allocation_counter::measure(|| drop(std::hint::black_box(vec![0_u8; 64])));
        assert!(probe.count_total > 0, "the allocation counter is not installed");
    },
    |data: &[u8]| {
        let bound = u64::try_from(data.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(BYTES_PER_OCTET)
            .saturating_add(FIXED_BYTES);

        let mut journal = None;
        let allocations =
            allocation_counter::measure(|| journal = RecoveryJournal::decode(data).ok());
        assert!(allocations.bytes_max <= bound, "decode held {allocations:?}");
        let Some(journal) = journal else {
            return;
        };
        assert!(journal.channels.len() <= MAX_CHANNELS, "more channels than the format has");

        let mut recovered = Vec::new();
        let allocations = allocation_counter::measure(|| recovered = journal.recover());
        assert!(allocations.bytes_max <= bound, "recovery held {allocations:?}");
        assert!(
            recovered.len() <= data.len().saturating_mul(MESSAGES_PER_OCTET),
            "recovered {} messages from {} octets",
            recovered.len(),
            data.len(),
        );

        let again = RecoveryJournal::decode(&journal.encode());
        assert_eq!(again.as_ref().map(RecoveryJournal::recover), Ok(recovered));
    }
);
