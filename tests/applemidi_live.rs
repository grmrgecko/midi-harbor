//! Live interoperability against a real RTP-MIDI peer.
//!
//! Ignored by default because it needs another machine. Run it against one with:
//!
//! ```text
//! HARBOR_PEER=192.0.2.4:5004 cargo test --test applemidi_live -- --ignored --nocapture
//! ```
//!
//! This is the test that proves the codec against Apple's own implementation rather than against
//! our encoder agreeing with itself.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_rtpmidi::clock::{ClockAction, ClockSync};
use midi_harbor_rtpmidi::{ControlPacket, Handshake, RtpMidiPacket, TimedMessage};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Our synchronisation source for the session.
const OUR_SSRC: u32 = 0x4D48_0001;

/// How long to wait for a peer to answer before giving up.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// Sends a control packet and waits for the reply the peer sends back.
fn exchange(socket: &UdpSocket, peer: SocketAddr, packet: &ControlPacket) -> Option<ControlPacket> {
    socket.send_to(&packet.encode(), peer).ok()?;

    let mut buffer = [0u8; 1500];
    let deadline = Instant::now() + REPLY_TIMEOUT;
    while Instant::now() < deadline {
        let Ok((len, _)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        if let Ok(reply) = ControlPacket::parse(buffer.get(..len).unwrap_or_default()) {
            return Some(reply);
        }
    }
    None
}

/// Opens one half of a session on the given port.
fn invite(
    port: u16,
    peer_port: u16,
    peer_host: &str,
    token: u32,
) -> Option<(UdpSocket, SocketAddr)> {
    let socket = UdpSocket::bind(("0.0.0.0", port)).ok()?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .ok()?;
    let peer: SocketAddr = format!("{peer_host}:{peer_port}").parse().ok()?;

    let invitation = ControlPacket::invitation(token, OUR_SSRC, "Midi Harbor Interop");
    match exchange(&socket, peer, &invitation) {
        Some(ControlPacket::Session {
            command: Handshake::Accepted,
            name,
            ..
        }) => {
            println!("  port {port}: accepted by {}", name.unwrap_or_default());
            Some((socket, peer))
        }
        other => {
            println!("  port {port}: not accepted ({other:?})");
            None
        }
    }
}

/// Locks that a real RTP-MIDI peer accepts both invitations, completes a clock exchange, and is
/// sent a phrase our own decoder reads back unchanged.
///
/// The recorded fixtures in `interop.rs` prove we read what peers send; this proves peers take
/// what we send, which our encoder agreeing with itself cannot.
#[test]
#[ignore = "needs a real RTP-MIDI peer; set HARBOR_PEER"]
fn a_full_session_with_a_real_peer() {
    let Ok(target) = std::env::var("HARBOR_PEER") else {
        println!("HARBOR_PEER is not set; nothing to talk to");
        return;
    };
    let (host, control_port) = match target.rsplit_once(':') {
        Some((host, port)) => (host.to_owned(), port.parse::<u16>().unwrap_or(5004)),
        None => (target.clone(), 5004),
    };

    // Open the control channel, then the data channel one port above it.
    println!("inviting {host}:{control_port}");
    let token = 0x0BAD_F00D;
    let control = invite(0, control_port, &host, token);
    assert!(control.is_some(), "the peer refused the control invitation");

    let data_local = control
        .as_ref()
        .and_then(|(socket, _)| socket.local_addr().ok())
        .map(|addr| addr.port().saturating_add(1))
        .unwrap_or(0);
    let data = invite(data_local, control_port.saturating_add(1), &host, token);
    assert!(data.is_some(), "the peer refused the data invitation");

    let Some((data_socket, data_peer)) = data else {
        return;
    };

    // Synchronise clocks, which is also what keeps the peer from timing us out.
    let mut sync = ClockSync::new();
    let now = 0u64;
    let opening = sync.begin(OUR_SSRC, now);
    assert!(
        data_socket.send_to(&opening.encode(), data_peer).is_ok(),
        "the clock exchange must reach the peer's data port"
    );

    let mut buffer = [0u8; 1500];
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut synchronised = false;
    while Instant::now() < deadline && !synchronised {
        let Ok((len, _)) = data_socket.recv_from(&mut buffer) else {
            continue;
        };
        let Ok(reply) = ControlPacket::parse(buffer.get(..len).unwrap_or_default()) else {
            continue;
        };
        if let ClockAction::Reply(next) = sync.handle(&reply, OUR_SSRC, now + 20) {
            let _ = data_socket.send_to(&next.encode(), data_peer);
            synchronised = sync.is_established();
        }
    }
    println!(
        "  clock synchronised: {synchronised}, round trip {:?}",
        sync.round_trip()
    );
    assert!(synchronised, "the peer did not complete a clock exchange");

    // Send a short phrase the peer can actually play.
    let channel = Channel::new(0).expect("channel 0");
    let mut sequence = 1u16;
    for note in [60u8, 64, 67] {
        let messages = vec![
            TimedMessage::immediate(MidiMessage::NoteOn {
                channel,
                note,
                velocity: 100,
            }),
            TimedMessage {
                delta: 2400,
                message: MidiMessage::NoteOff {
                    channel,
                    note,
                    velocity: 0,
                },
            },
        ];
        let packet = RtpMidiPacket::new(sequence, u32::from(sequence) * 4800, OUR_SSRC, messages);

        // Our own encoder must round-trip before it is trusted on the wire.
        let encoded = packet.encode();
        assert_eq!(
            RtpMidiPacket::parse(&encoded).unwrap(),
            packet,
            "a packet must decode to what was encoded before it is sent"
        );

        assert!(
            data_socket.send_to(&encoded, data_peer).is_ok(),
            "note {note} must reach the peer's data port"
        );
        println!("  sent note {note} as sequence {sequence}");
        sequence = sequence.saturating_add(1);
        std::thread::sleep(Duration::from_millis(250));
    }

    // Close both halves so the peer does not hold a dead session.
    let goodbye = ControlPacket::end_session(token, OUR_SSRC).encode();
    let _ = data_socket.send_to(&goodbye, data_peer);
    if let Some((socket, peer)) = control {
        let _ = socket.send_to(&goodbye, peer);
    }
    println!("  session closed");
}
