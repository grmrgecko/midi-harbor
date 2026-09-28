//! The path MIDI bytes actually travel.
//!
//! The ring buffers themselves live in `midi-harbor-core` so the platform layer can hold a
//! producer. What lives here is the consuming side: draining those rings and fanning messages out
//! along the route graph.
//!
//! Buffers are sized once at setup. When one fills, the event is dropped and counted — never
//! queued by growing the buffer, because a buffer that grows under load is a buffer that
//! allocates on the real-time path.

use midi_harbor_core::counters::TrafficCounters;
use midi_harbor_core::stream::SysExEnd;

use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::rtchannel::{self, Doorbell};
pub use midi_harbor_core::rtchannel::{Drained, RtConsumer, RtProducer};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

/// How long the doorbell's listener waits before looking again when nothing rings.
///
/// Only a backstop: every push rings, so this bounds nothing on the data path.
const DOORBELL_PATIENCE: Duration = Duration::from_secs(1);

/// Rung by every endpoint's producer when something arrives.
static DOORBELL: LazyLock<Arc<Doorbell>> = LazyLock::new(|| Arc::new(Doorbell::new()));

/// Wakes the drain tasks when the doorbell rings.
///
/// A real-time callback cannot reach this directly, because waking a task takes a lock. A plain
/// thread listens to the doorbell, which the callback can ring, and passes the news on here.
static ARRIVED: LazyLock<Arc<tokio::sync::Notify>> = LazyLock::new(|| {
    let arrived = Arc::new(tokio::sync::Notify::new());
    let notify = Arc::clone(&arrived);
    let listening = std::thread::Builder::new()
        .name("harbor-doorbell".to_owned())
        .spawn(move || {
            loop {
                if DOORBELL.wait(DOORBELL_PATIENCE) {
                    notify.notify_waiters();
                }
            }
        });
    if let Err(error) = listening {
        tracing::error!(error = %error, "could not start the doorbell listener; midi will wait for the fallback check");
    }
    arrived
});

/// Creates the two halves of one endpoint's data path, wired to wake the drain when MIDI arrives.
pub fn channel(source: EndpointId) -> (RtProducer, RtConsumer) {
    connector_channel(source, 0)
}

/// Creates the data path for one of an endpoint's MIDI In connectors, counting from zero, wired to
/// wake the drain when MIDI arrives.
pub fn connector_channel(source: EndpointId, connector: u8) -> (RtProducer, RtConsumer) {
    LazyLock::force(&ARRIVED);
    rtchannel::connector_channel(source, connector, Arc::clone(&DOORBELL))
}

/// Returns what wakes the drain tasks when MIDI arrives on any endpoint.
pub fn arrived() -> Arc<tokio::sync::Notify> {
    Arc::clone(&ARRIVED)
}

/// The largest system-exclusive message that will be rebuilt.
///
/// Comfortably larger than a full patch bank from any instrument, and bounded so a source that
/// never sends a terminator cannot grow this buffer without limit.
pub const MAX_SYSEX_BYTES: usize = 262_144;

/// Rebuilds whole system-exclusive messages from the runs the data path delivers.
///
/// A dump arrives in as many pieces as the platform felt like splitting it into. It is rebuilt
/// rather than forwarded piecemeal because a partial dump still frames correctly to a receiver,
/// which will act on whatever arrived — so nothing is forwarded until the terminator says the
/// message is whole.
#[derive(Debug, Default)]
pub struct SysExAssembler {
    buffer: Vec<u8>,
    /// Set when the message outgrew the limit, so the remainder is read and thrown away rather
    /// than leaving the next message spliced onto this one.
    overflowed: bool,
    discarded: u64,
}

impl SysExAssembler {
    /// Creates an assembler with nothing in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a run, returning the message once it is whole.
    pub fn push(&mut self, bytes: &[u8], end: SysExEnd) -> Option<Vec<u8>> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_SYSEX_BYTES {
            self.overflowed = true;
            self.buffer.clear();
        }
        if !self.overflowed {
            self.buffer.extend_from_slice(bytes);
        }

        match end {
            SysExEnd::Open => None,
            SysExEnd::Complete if self.overflowed => {
                self.discard();
                None
            }
            SysExEnd::Complete => Some(std::mem::take(&mut self.buffer)),
            SysExEnd::Abandoned => {
                self.discard();
                None
            }
        }
    }

    /// Throws away any message in progress, reporting whether there was one.
    ///
    /// Called when the source goes away mid-dump, so the next thing that endpoint sends does not
    /// arrive spliced onto the tail of a message nobody finished.
    pub fn abandon(&mut self) -> bool {
        let had_one = !self.buffer.is_empty() || self.overflowed;
        if had_one {
            self.discard();
        }
        had_one
    }

    /// Returns how many messages were thrown away rather than forwarded.
    pub fn discarded(&self) -> u64 {
        self.discarded
    }

    /// Clears the buffer and counts one message lost.
    fn discard(&mut self) {
        self.buffer.clear();
        self.overflowed = false;
        self.discarded = self.discarded.saturating_add(1);
    }
}

/// Records what a drained batch did to an endpoint's counters.
pub fn record(counters: &TrafficCounters, batch: &[Drained], dropped: u64, at: jiff::Timestamp) {
    for item in batch {
        let bytes = match item {
            Drained::Message { message, .. } => message.len() as u64,
            Drained::SysEx { bytes, .. } => bytes.len() as u64,
        };
        counters.record_received(bytes, at);
    }
    if dropped > 0 {
        counters.record_dropped(dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi_harbor_core::ids::EndpointId;
    use midi_harbor_core::midi::{Channel, MidiMessage};

    /// Proves that pushing onto an endpoint's ring allocates nothing, the rule of AGENTS.md's
    /// real-time discipline that is easiest to break by accident: the push runs inside a CoreMIDI
    /// or ALSA read callback, where an allocation can block on the allocator's lock and stall the
    /// MIDI thread. Asserted with a counting allocator rather than trusted.
    #[test]
    fn the_hot_path_does_not_allocate() {
        let (mut producer, mut consumer) = channel(EndpointId::new());
        let channel = Channel::new(0).expect("channel 0 is a valid MIDI channel");
        let note = |number: u8| MidiMessage::NoteOn {
            channel,
            note: number,
            velocity: 100,
        };

        // Warm the rings, since the first push may touch pages the allocator has not yet faulted.
        for _ in 0..64 {
            let _ = producer.push(note(60), 0);
        }
        let _ = consumer.drain(64);

        // Counted per thread, because the test runner executes other tests in parallel and a
        // global count would attribute their allocations to this one. A thousand pushes overflow
        // the ring as well, so the drop-and-count path is measured too.
        let before = allocations();
        for n in 0..1000u32 {
            let note_number = u8::try_from(n % 128).unwrap_or(60);
            let _ = producer.push(note(note_number), u64::from(n));
        }
        let after = allocations();

        assert_eq!(
            after,
            before,
            "pushing onto the real-time path allocated {} times, and an allocation can stall the MIDI thread",
            after.saturating_sub(before)
        );
    }

    thread_local! {
        /// THREAD_ALLOCATIONS counts allocations made by this thread, so a parallel test cannot
        /// pollute the count.
        static THREAD_ALLOCATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// Returns how many allocations this thread has made.
    fn allocations() -> u64 {
        THREAD_ALLOCATIONS.with(std::cell::Cell::get)
    }

    /// Records one allocation against the calling thread.
    ///
    /// Reading a thread local can itself allocate during lazy initialisation, so a failure to
    /// access it is ignored rather than counted.
    fn note_allocation() {
        let _ = THREAD_ALLOCATIONS.try_with(|count| count.set(count.get().saturating_add(1)));
    }

    /// Wraps the system allocator and counts calls.
    struct Counting;

    // SAFETY: every method forwards directly to the system allocator with the layout it was
    // given, adding only a relaxed counter increment, so the allocator contract is unchanged.
    unsafe impl std::alloc::GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            note_allocation();
            unsafe { std::alloc::System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(
            &self,
            ptr: *mut u8,
            layout: std::alloc::Layout,
            new_size: usize,
        ) -> *mut u8 {
            note_allocation();
            unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;
}

#[cfg(test)]
mod sysex_tests {
    use super::*;

    /// A short universal device inquiry, the smallest realistic whole message.
    const INQUIRY: [u8; 3] = [0xF0, 0x7E, 0xF7];

    /// Proves that a dump thrown away before it finished is counted once and leaves nothing
    /// behind, so the next message from that endpoint arrives clean rather than spliced onto the
    /// tail of one nobody finished. Two ways a dump is thrown away: its source goes away mid-dump,
    /// and a source that never sends a terminator outgrows MAX_SYSEX_BYTES (262,144), which bounds
    /// the buffer against hostile input. Sixty-eight runs of 4,096 bytes are 278,528 bytes: the
    /// 64 runs that fill the limit exactly, plus four more to cross it.
    #[test]
    fn a_thrown_away_dump_is_counted_once_and_leaves_the_next_message_clean() {
        struct Case {
            name: &'static str,
            throw_away: fn(&mut SysExAssembler),
        }
        let cases = [
            Case {
                name: "the source goes away mid-dump",
                throw_away: |assembler| {
                    assert_eq!(
                        assembler.push(&[0xF0, 0x43, 0x10], SysExEnd::Open),
                        None,
                        "an unfinished dump must not be forwarded"
                    );
                    assert!(
                        assembler.abandon(),
                        "abandoning must report the partial dump it threw away"
                    );
                    assert!(
                        !assembler.abandon(),
                        "a second abandon has nothing left to throw away"
                    );
                },
            },
            Case {
                name: "the dump never ends and outgrows the limit",
                throw_away: |assembler| {
                    let run = vec![0x00; 4096];
                    let past_the_limit = MAX_SYSEX_BYTES.div_ceil(run.len()).saturating_add(4);
                    for _ in 0..past_the_limit {
                        assert_eq!(
                            assembler.push(&run, SysExEnd::Open),
                            None,
                            "an unfinished dump must not be forwarded"
                        );
                    }
                    assert_eq!(
                        assembler.push(&[0xF7], SysExEnd::Complete),
                        None,
                        "a dump cut down to the limit must be discarded rather than delivered truncated"
                    );
                },
            },
        ];

        for case in cases {
            let mut assembler = SysExAssembler::new();
            (case.throw_away)(&mut assembler);
            assert_eq!(
                assembler.discarded(),
                1,
                "{}: the thrown-away dump must be counted exactly once",
                case.name
            );
            assert_eq!(
                assembler.push(&INQUIRY, SysExEnd::Complete),
                Some(INQUIRY.to_vec()),
                "{}: the next message must arrive whole and unspliced",
                case.name
            );
        }
    }
}
