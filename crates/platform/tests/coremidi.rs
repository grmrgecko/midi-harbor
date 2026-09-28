//! Sending to hardware through the real CoreMIDI backend.
//!
//! Ignored by default because it needs a real MIDI system and an IAC bus. Enable "IAC Driver" in
//! Audio MIDI Setup, then run:
//!
//! ```text
//! cargo test -p midi-harbor-platform --test coremidi -- --ignored --nocapture
//! ```

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::rtchannel::{self, Drained};
use std::time::{Duration, Instant};

/// Locks that a message sent to a device reaches it, through the IAC bus, which passes what it
/// receives back out of its source.
///
/// A CoreMIDI device's source and destination have unique identifiers of their own. Opening a
/// device once looked for a destination with its source's identifier and never found one, so
/// every message sent to hardware was refused.
#[test]
#[ignore = "needs a real MIDI system with an IAC bus"]
fn midi_sent_to_hardware_reaches_it() {
    let backend = midi_harbor_platform::midi_backend().expect("the real backend");
    let bus = backend
        .list_devices()
        .expect("devices")
        .into_iter()
        .find(|device| device.fingerprint.name.starts_with("IAC Driver"))
        .expect("an IAC bus; enable IAC Driver in Audio MIDI Setup");

    let (heard, mut listening) = rtchannel::channel(EndpointId::new());
    let handle = backend
        .open_device_with_sink(&bus.fingerprint, Some(heard))
        .expect("the bus opens");
    let note = MidiMessage::NoteOn {
        channel: Channel::new(0).unwrap(),
        note: 61,
        velocity: 90,
    };
    backend
        .send(handle, &[note])
        .expect("a device must accept what it is sent");

    let sent = Instant::now();
    while sent.elapsed() < Duration::from_secs(2) {
        let back = listening
            .drain(16)
            .into_iter()
            .any(|drained| matches!(drained, Drained::Message { message, .. } if message == note));
        if back {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "{} accepted the note and it never arrived",
        bus.fingerprint.name
    );
}
