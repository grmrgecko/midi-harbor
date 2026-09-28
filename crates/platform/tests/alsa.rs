//! Sending to hardware through the real ALSA backend.
//!
//! Ignored by default because it needs the kernel's Midi Through port (`snd-seq-dummy`), which
//! plays the part the IAC bus plays on macOS: it passes what it receives straight back out. Run:
//!
//! ```text
//! cargo test -p midi-harbor-platform --test alsa -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::rtchannel::{self, Drained};
use std::time::{Duration, Instant};

/// Locks that a message sent to a device reaches it, through Midi Through, which passes what it
/// receives straight back out.
///
/// The same promise the CoreMIDI backend's live test makes, held against the ALSA sequencer.
#[test]
#[ignore = "needs the kernel's Midi Through port"]
fn midi_sent_to_hardware_reaches_it() {
    let backend = midi_harbor_platform::midi_backend().expect("the real backend");
    let through = backend
        .list_devices()
        .expect("devices")
        .into_iter()
        .find(|device| device.fingerprint.name.starts_with("Midi Through"))
        .expect("Midi Through; load it with 'modprobe snd-seq-dummy'");

    let (heard, mut listening) = rtchannel::channel(EndpointId::new());
    let handle = backend
        .open_device_with_sink(&through.fingerprint, Some(heard))
        .expect("Midi Through opens");
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
        through.fingerprint.name
    );
}

/// Locks that a port another sequencer client opens is announced as a setup change.
///
/// The announcement used to be queued under a mutex on the sequencer's read path, which may
/// neither lock nor allocate. It is a flag now, and must still arrive.
#[test]
#[ignore = "needs the ALSA sequencer"]
fn another_client_opening_a_port_is_announced() {
    let backend = midi_harbor_platform::midi_backend().expect("the real backend");
    let _ = backend.drain_events();

    let other = alsa::Seq::open(None, None, false).expect("a second sequencer client");
    let name = std::ffi::CString::new("Harbor Announcement Probe").unwrap();
    other.set_client_name(&name).expect("a client name");
    let caps = alsa::seq::PortCap::READ | alsa::seq::PortCap::SUBS_READ;
    other
        .create_simple_port(&name, caps, alsa::seq::PortType::MIDI_GENERIC)
        .expect("a port");

    let opened = Instant::now();
    while opened.elapsed() < Duration::from_secs(2) {
        if backend
            .drain_events()
            .contains(&midi_harbor_platform::midi::MidiPlatformEvent::SetupChanged)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("a port another client opened was never announced");
}
