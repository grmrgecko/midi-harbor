//! The AppleMIDI session control protocol.
//!
//! Control packets travel on the session's control port and carry no MIDI. They open a session,
//! close it, and keep the two sides' clocks aligned.
//!
//! The layout was verified against Apple's own implementation rather than taken from
//! documentation: an invitation sent to a Mac running Network MIDI was accepted, and the reply
//! is kept below as a test fixture.

use std::fmt;

/// Marks a packet as session control rather than RTP.
///
/// Two 0xFF bytes cannot begin a valid RTP header, which is how a receiver tells the two apart on
/// a shared port.
pub const SIGNATURE: [u8; 2] = [0xFF, 0xFF];

/// The only protocol version Apple's implementation speaks.
pub const PROTOCOL_VERSION: u32 = 2;

/// Bytes before the name field in an invitation-family packet.
const INVITATION_HEADER_LEN: usize = 16;

/// Bytes in a clock synchronisation packet carrying all three timestamps.
const CLOCK_PACKET_LEN: usize = 36;

/// Bytes in a receiver feedback packet.
const FEEDBACK_PACKET_LEN: usize = 12;

/// Longest peer name accepted, to bound allocation on hostile input.
const MAX_NAME_LEN: usize = 256;

/// Highest valid clock synchronisation count.
const MAX_CLOCK_COUNT: u8 = 2;

/// Why a control packet could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The packet did not begin with the control signature.
    #[error("not a session control packet")]
    NotControl,
    /// The packet ended before a field it declared.
    #[error("packet is {actual} bytes, needs at least {expected}")]
    TooShort {
        /// How many bytes were present.
        actual: usize,
        /// How many were needed.
        expected: usize,
    },
    /// The two command bytes are not a command this implementation knows.
    #[error("unknown session command {0:?}")]
    UnknownCommand([u8; 2]),
    /// The peer speaks a protocol version this implementation does not.
    #[error("peer speaks protocol version {found}, expected {PROTOCOL_VERSION}")]
    UnsupportedVersion {
        /// The version the peer declared.
        found: u32,
    },
    /// The name field is longer than any legitimate peer name.
    #[error("peer name exceeds {MAX_NAME_LEN} bytes")]
    NameTooLong,
    /// The clock packet declared more timestamps than the protocol allows.
    #[error("clock count {0} exceeds the maximum of {MAX_CLOCK_COUNT}")]
    ClockCountOutOfRange(u8),
}

/// Which control message a packet carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCommand {
    /// Asking to open a session.
    Invitation,
    /// Accepting an invitation.
    Accepted,
    /// Refusing an invitation.
    Rejected,
    /// Ending a session.
    EndSession,
    /// Exchanging timestamps.
    ClockSync,
    /// Reporting the highest sequence number received, which trims the sender's journal.
    ReceiverFeedback,
}

impl SessionCommand {
    /// Returns the two ASCII bytes identifying this command on the wire.
    pub fn as_bytes(&self) -> [u8; 2] {
        match self {
            Self::Invitation => *b"IN",
            Self::Accepted => *b"OK",
            Self::Rejected => *b"NO",
            Self::EndSession => *b"BY",
            Self::ClockSync => *b"CK",
            Self::ReceiverFeedback => *b"RS",
        }
    }

    /// Reads a command from its two wire bytes.
    pub fn from_bytes(bytes: [u8; 2]) -> Result<Self, ParseError> {
        match &bytes {
            b"IN" => Ok(Self::Invitation),
            b"OK" => Ok(Self::Accepted),
            b"NO" => Ok(Self::Rejected),
            b"BY" => Ok(Self::EndSession),
            b"CK" => Ok(Self::ClockSync),
            b"RS" => Ok(Self::ReceiverFeedback),
            _ => Err(ParseError::UnknownCommand(bytes)),
        }
    }
}

impl fmt::Display for SessionCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.as_bytes();
        f.write_str(std::str::from_utf8(&bytes).unwrap_or("??"))
    }
}

/// The four commands that open and close a session.
///
/// Narrower than `SessionCommand` on purpose: a session packet cannot carry a clock exchange or a
/// feedback report, and the type should not pretend otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handshake {
    /// Asking to open a session.
    Invitation,
    /// Accepting an invitation.
    Accepted,
    /// Refusing an invitation.
    Rejected,
    /// Ending a session.
    EndSession,
}

impl Handshake {
    /// Returns the wire command this handshake corresponds to.
    pub fn command(&self) -> SessionCommand {
        match self {
            Self::Invitation => SessionCommand::Invitation,
            Self::Accepted => SessionCommand::Accepted,
            Self::Rejected => SessionCommand::Rejected,
            Self::EndSession => SessionCommand::EndSession,
        }
    }

    /// Reads a handshake from a wire command, returning `None` for the other two.
    pub fn from_command(command: SessionCommand) -> Option<Self> {
        match command {
            SessionCommand::Invitation => Some(Self::Invitation),
            SessionCommand::Accepted => Some(Self::Accepted),
            SessionCommand::Rejected => Some(Self::Rejected),
            SessionCommand::EndSession => Some(Self::EndSession),
            SessionCommand::ClockSync | SessionCommand::ReceiverFeedback => None,
        }
    }
}

impl fmt::Display for Handshake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.command().fmt(f)
    }
}

/// A parsed session control packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPacket {
    /// An invitation, acceptance, rejection, or session end.
    Session {
        /// Which of the four this is.
        command: Handshake,
        /// Chosen by the initiator and echoed by the responder, tying a reply to its request.
        token: u32,
        /// Identifies the sending endpoint for the life of the session.
        ssrc: u32,
        /// The peer's display name. Absent on `BY`, where it carries no information.
        name: Option<String>,
    },
    /// A timestamp exchange.
    ClockSync {
        /// The sender's synchronisation source.
        ssrc: u32,
        /// How far through the three-message exchange this packet is.
        count: u8,
        /// Timestamps in units of 100 microseconds, indexed by exchange position.
        timestamps: [u64; 3],
    },
    /// A receiver reporting what it has seen, which trims the sender's journal.
    ///
    /// ReceiverFeedback's 32-bit field is kept whole because implementations disagree on it:
    /// Apple's Network MIDI writes the RTP sequence number in the upper half and reads it from
    /// there, while rtpmidid reads the upper half but writes the lower (R-071).
    ReceiverFeedback {
        /// The reporting endpoint.
        ssrc: u32,
        /// The field as sent, holding an RTP sequence number in one half or the other.
        acknowledged: u32,
    },
}

impl ControlPacket {
    /// Builds receiver feedback for an RTP sequence number, in the upper half as Apple writes it.
    pub fn feedback(ssrc: u32, sequence: u16) -> Self {
        Self::ReceiverFeedback {
            ssrc,
            acknowledged: u32::from(sequence) << 16,
        }
    }

    /// Builds an invitation to open a session.
    pub fn invitation(token: u32, ssrc: u32, name: impl Into<String>) -> Self {
        Self::Session {
            command: Handshake::Invitation,
            token,
            ssrc,
            name: Some(name.into()),
        }
    }

    /// Builds an acceptance of an invitation.
    pub fn accepted(token: u32, ssrc: u32, name: impl Into<String>) -> Self {
        Self::Session {
            command: Handshake::Accepted,
            token,
            ssrc,
            name: Some(name.into()),
        }
    }

    /// Builds a rejection of an invitation.
    pub fn rejected(token: u32, ssrc: u32) -> Self {
        Self::Session {
            command: Handshake::Rejected,
            token,
            ssrc,
            name: None,
        }
    }

    /// Builds a message ending a session.
    pub fn end_session(token: u32, ssrc: u32) -> Self {
        Self::Session {
            command: Handshake::EndSession,
            token,
            ssrc,
            name: None,
        }
    }

    /// Returns which command this packet carries.
    pub fn command(&self) -> SessionCommand {
        match self {
            Self::Session { command, .. } => command.command(),
            Self::ClockSync { .. } => SessionCommand::ClockSync,
            Self::ReceiverFeedback { .. } => SessionCommand::ReceiverFeedback,
        }
    }

    /// Returns the sending endpoint's synchronisation source.
    pub fn ssrc(&self) -> u32 {
        match self {
            Self::Session { ssrc, .. }
            | Self::ClockSync { ssrc, .. }
            | Self::ReceiverFeedback { ssrc, .. } => *ssrc,
        }
    }

    /// Encodes the packet onto the wire.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(CLOCK_PACKET_LEN);
        out.extend_from_slice(&SIGNATURE);
        out.extend_from_slice(&self.command().as_bytes());

        match self {
            Self::Session {
                token, ssrc, name, ..
            } => {
                out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
                out.extend_from_slice(&token.to_be_bytes());
                out.extend_from_slice(&ssrc.to_be_bytes());
                if let Some(name) = name {
                    // Truncate on a character boundary so a long name cannot produce invalid
                    // UTF-8 on the wire.
                    let mut end = name.len().min(MAX_NAME_LEN);
                    while end > 0 && !name.is_char_boundary(end) {
                        end -= 1;
                    }
                    if let Some(text) = name.get(..end) {
                        out.extend_from_slice(text.as_bytes());
                    }
                    out.push(0);
                }
            }
            Self::ClockSync {
                ssrc,
                count,
                timestamps,
            } => {
                out.extend_from_slice(&ssrc.to_be_bytes());
                out.push(*count);
                // Three bytes of padding keep the timestamps eight-byte aligned.
                out.extend_from_slice(&[0, 0, 0]);
                for timestamp in timestamps {
                    out.extend_from_slice(&timestamp.to_be_bytes());
                }
            }
            Self::ReceiverFeedback { ssrc, acknowledged } => {
                out.extend_from_slice(&ssrc.to_be_bytes());
                out.extend_from_slice(&acknowledged.to_be_bytes());
            }
        }
        out
    }

    /// Parses a packet received from a peer.
    ///
    /// Returns `NotControl` rather than an error for anything that is not a control packet, since
    /// RTP data shares the same socket pair and reaching here is normal.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        // Validate the framing before reading anything.
        let signature = bytes.get(..2).ok_or(ParseError::NotControl)?;
        if signature != SIGNATURE {
            return Err(ParseError::NotControl);
        }
        let raw_command: [u8; 2] =
            bytes
                .get(2..4)
                .and_then(|s| s.try_into().ok())
                .ok_or(ParseError::TooShort {
                    actual: bytes.len(),
                    expected: 4,
                })?;
        let command = SessionCommand::from_bytes(raw_command)?;

        match command {
            SessionCommand::ClockSync => parse_clock_sync(bytes),
            SessionCommand::ReceiverFeedback => parse_feedback(bytes),
            other => {
                // The remaining four are exactly the handshake family, so this cannot fail.
                let handshake = Handshake::from_command(other)
                    .ok_or(ParseError::UnknownCommand(raw_command))?;
                parse_session(handshake, bytes)
            }
        }
    }
}

/// Parses an invitation, acceptance, rejection or session end.
fn parse_session(command: Handshake, bytes: &[u8]) -> Result<ControlPacket, ParseError> {
    if bytes.len() < INVITATION_HEADER_LEN {
        return Err(ParseError::TooShort {
            actual: bytes.len(),
            expected: INVITATION_HEADER_LEN,
        });
    }
    let version = read_u32(bytes, 4)?;
    if version != PROTOCOL_VERSION {
        return Err(ParseError::UnsupportedVersion { found: version });
    }
    let token = read_u32(bytes, 8)?;
    let ssrc = read_u32(bytes, 12)?;

    // The name is optional and null-terminated. A peer that omits it is legitimate.
    let name = match bytes.get(INVITATION_HEADER_LEN..) {
        None | Some([]) => None,
        Some(tail) => {
            if tail.len() > MAX_NAME_LEN {
                return Err(ParseError::NameTooLong);
            }
            let end = tail.iter().position(|b| *b == 0).unwrap_or(tail.len());
            tail.get(..end)
                .map(|text| String::from_utf8_lossy(text).into_owned())
        }
    };

    Ok(ControlPacket::Session {
        command,
        token,
        ssrc,
        name,
    })
}

/// Parses a timestamp exchange.
fn parse_clock_sync(bytes: &[u8]) -> Result<ControlPacket, ParseError> {
    if bytes.len() < CLOCK_PACKET_LEN {
        return Err(ParseError::TooShort {
            actual: bytes.len(),
            expected: CLOCK_PACKET_LEN,
        });
    }
    let ssrc = read_u32(bytes, 4)?;
    let count = *bytes.get(8).ok_or(ParseError::TooShort {
        actual: bytes.len(),
        expected: 9,
    })?;
    if count > MAX_CLOCK_COUNT {
        return Err(ParseError::ClockCountOutOfRange(count));
    }

    let mut timestamps = [0u64; 3];
    for (index, slot) in timestamps.iter_mut().enumerate() {
        // Timestamps begin after four signature and command bytes, four ssrc, one count and
        // three padding.
        let offset = 12 + index * 8;
        *slot = read_u64(bytes, offset)?;
    }
    Ok(ControlPacket::ClockSync {
        ssrc,
        count,
        timestamps,
    })
}

/// Parses a receiver feedback packet.
fn parse_feedback(bytes: &[u8]) -> Result<ControlPacket, ParseError> {
    if bytes.len() < FEEDBACK_PACKET_LEN {
        return Err(ParseError::TooShort {
            actual: bytes.len(),
            expected: FEEDBACK_PACKET_LEN,
        });
    }
    Ok(ControlPacket::ReceiverFeedback {
        ssrc: read_u32(bytes, 4)?,
        acknowledged: read_u32(bytes, 8)?,
    })
}

/// Reads a big-endian `u32` without risking an out-of-bounds read.
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ParseError> {
    let end = offset.saturating_add(4);
    bytes
        .get(offset..end)
        .and_then(|slice| slice.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or(ParseError::TooShort {
            actual: bytes.len(),
            expected: end,
        })
}

/// Reads a big-endian `u64` without risking an out-of-bounds read.
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ParseError> {
    let end = offset.saturating_add(8);
    bytes
        .get(offset..end)
        .and_then(|slice| slice.try_into().ok())
        .map(u64::from_be_bytes)
        .ok_or(ParseError::TooShort {
            actual: bytes.len(),
            expected: end,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An acceptance captured from Apple's Network MIDI on macOS 15, replying to an invitation
    /// this implementation sent, with the peer's machine name replaced by a generic one.
    const APPLE_OK: &[u8] = &[
        0xff, 0xff, 0x4f, 0x4b, // signature, "OK"
        0x00, 0x00, 0x00, 0x02, // protocol version 2
        0x12, 0x34, 0x56, 0x78, // our token, echoed back
        0xc0, 0x85, 0x8e, 0xfe, // the peer's ssrc
        // "Studio Mac\0", the peer's name, which ends the packet
        0x53, 0x74, 0x75, 0x64, 0x69, 0x6f, 0x20, 0x4d, 0x61, 0x63, 0x00,
    ];

    /// The invitation that drew [`APPLE_OK`] from Apple's Network MIDI, so known to be accepted.
    const OUR_IN: &[u8] = &[
        0xff, 0xff, 0x49, 0x4e, // signature, "IN"
        0x00, 0x00, 0x00, 0x02, // protocol version 2
        0x12, 0x34, 0x56, 0x78, // our token
        0x0b, 0xad, 0xf0, 0x0d, // our ssrc
        // "Harbor Spike\0"
        0x48, 0x61, 0x72, 0x62, 0x6f, 0x72, 0x20, 0x53, 0x70, 0x69, 0x6b, 0x65, 0x00,
    ];

    /// Apple's Network MIDI acknowledging RTP packet 0xFDE5, as recorded in R-068.
    const APPLE_RS: &[u8] = &[
        0xff, 0xff, b'R', b'S', // signature, "RS"
        0x37, 0x19, 0x3c, 0x27, // the peer's ssrc
        0xfd, 0xe5, 0x00, 0x00, // the sequence number in the upper half
    ];

    /// Bytes Apple's Network MIDI sent or accepted parse to the packet they carry, and that
    /// packet encodes back to the same bytes.
    ///
    /// Interoperability is a requirement, so the authority is Apple's own traffic rather than our
    /// encoder agreeing with itself. The `RS` row pins the half Apple uses: the sequence number
    /// `0xFDE5` in the upper sixteen bits, `0xFDE5 << 16 = 0xFDE5_0000`.
    #[test]
    fn apples_wire_bytes_parse_and_encode_exactly() {
        let cases = [
            (
                "Apple's acceptance",
                APPLE_OK,
                ControlPacket::accepted(0x1234_5678, 0xc085_8efe, "Studio Mac"),
            ),
            (
                "our invitation Apple accepted",
                OUR_IN,
                ControlPacket::invitation(0x1234_5678, 0x0bad_f00d, "Harbor Spike"),
            ),
            (
                "Apple's receiver feedback",
                APPLE_RS,
                ControlPacket::feedback(0x3719_3c27, 0xfde5),
            ),
        ];
        for (name, bytes, packet) in cases {
            assert_eq!(
                ControlPacket::parse(bytes).as_ref(),
                Ok(&packet),
                "{name}: Apple's bytes must parse to this packet"
            );
            assert_eq!(
                packet.encode(),
                bytes,
                "{name}: this packet must encode to exactly Apple's bytes"
            );
        }
    }

    /// Parsing accepts each boundary of the AppleMIDI layout and refuses the input just past it,
    /// naming what was wrong.
    ///
    /// RTP data shares the port pair, so it is reported as not control rather than as an error.
    /// Apple speaks only protocol version 2, and a clock exchange has counts 0 to 2. A name is
    /// bounded at `MAX_NAME_LEN = 256` bytes including its terminator, so 255 characters and a
    /// null are accepted and 256 characters and a null are not. A goodbye may carry no name at
    /// all, which leaves the 16-byte header, and 15 bytes is one short of it.
    #[test]
    fn parse_accepts_each_boundary_and_refuses_the_input_past_it() {
        let with_version = |version: u32| {
            let mut bytes = ControlPacket::invitation(1, 2, "x").encode();
            bytes[4..8].copy_from_slice(&version.to_be_bytes());
            bytes
        };
        let with_count = |count: u8| {
            let mut bytes = ControlPacket::ClockSync {
                ssrc: 1,
                count: 0,
                timestamps: [10, 20, 30],
            }
            .encode();
            bytes[8] = count;
            bytes
        };
        let with_name =
            |length: usize| ControlPacket::invitation(1, 2, "A".repeat(length)).encode();
        let goodbye = ControlPacket::end_session(7, 8).encode();
        let mut unknown = ControlPacket::invitation(1, 2, "x").encode();
        unknown[2..4].copy_from_slice(b"ZZ");

        let cases: [(&str, Vec<u8>, Result<ControlPacket, ParseError>); 11] = [
            (
                "RTP data on the shared port",
                vec![0x80, 0x61, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
                Err(ParseError::NotControl),
            ),
            (
                "an unknown command",
                unknown,
                Err(ParseError::UnknownCommand(*b"ZZ")),
            ),
            (
                "protocol version 2",
                with_version(2),
                Ok(ControlPacket::invitation(1, 2, "x")),
            ),
            (
                "protocol version 3",
                with_version(3),
                Err(ParseError::UnsupportedVersion { found: 3 }),
            ),
            (
                "clock count 2, the last of an exchange",
                with_count(2),
                Ok(ControlPacket::ClockSync {
                    ssrc: 1,
                    count: 2,
                    timestamps: [10, 20, 30],
                }),
            ),
            (
                "clock count 3",
                with_count(3),
                Err(ParseError::ClockCountOutOfRange(3)),
            ),
            (
                "a name filling the bound",
                with_name(MAX_NAME_LEN - 1),
                Ok(ControlPacket::invitation(
                    1,
                    2,
                    "A".repeat(MAX_NAME_LEN - 1),
                )),
            ),
            (
                "a name one byte past the bound",
                with_name(MAX_NAME_LEN),
                Err(ParseError::NameTooLong),
            ),
            (
                "a goodbye with no name",
                goodbye.clone(),
                Ok(ControlPacket::end_session(7, 8)),
            ),
            (
                "a header one byte short",
                goodbye[..INVITATION_HEADER_LEN - 1].to_vec(),
                Err(ParseError::TooShort {
                    actual: INVITATION_HEADER_LEN - 1,
                    expected: INVITATION_HEADER_LEN,
                }),
            ),
            (
                "receiver feedback",
                ControlPacket::feedback(3, 4).encode(),
                Ok(ControlPacket::feedback(3, 4)),
            ),
        ];
        for (name, bytes, want) in cases {
            assert_eq!(
                ControlPacket::parse(&bytes),
                want,
                "{name}: the parse must return this result"
            );
        }
    }

    /// Every truncation of a packet short of its fixed layout is refused rather than read past
    /// or misread.
    ///
    /// An invitation's fixed part is its 16-byte header, a clock packet is 36 bytes (four of
    /// signature and command, four of SSRC, one count, three padding, three eight-byte
    /// timestamps), and receiver feedback is 12. A shorter packet that parsed would have taken
    /// its fields from the wrong offsets.
    #[test]
    fn a_truncated_packet_is_refused() {
        let cases = [
            (
                "an invitation",
                ControlPacket::invitation(1, 2, "Studio Mac").encode(),
                16,
            ),
            (
                "a clock packet",
                ControlPacket::ClockSync {
                    ssrc: 1,
                    count: 2,
                    timestamps: [1, 2, 3],
                }
                .encode(),
                36,
            ),
            (
                "receiver feedback",
                ControlPacket::feedback(1, 2).encode(),
                12,
            ),
        ];
        for (name, packet, fixed) in cases {
            for length in 0..fixed {
                assert!(
                    ControlPacket::parse(&packet[..length]).is_err(),
                    "{name} cut to {length} bytes must be refused, not read as a shorter packet"
                );
            }
        }
    }

    /// A name longer than the bound is cut on a character boundary when encoded, so what goes on
    /// the wire is bounded and still valid UTF-8.
    ///
    /// U+2019 is three bytes in UTF-8, so 256 of them are 768 bytes. The last character boundary
    /// at or below `MAX_NAME_LEN = 256` is `85 * 3 = 255`, so 85 characters are sent and the
    /// packet is `16 + 255 + 1 = 272` bytes with the terminator.
    #[test]
    fn an_encoded_name_is_cut_on_a_character_boundary() {
        let encoded = ControlPacket::invitation(1, 2, "\u{2019}".repeat(MAX_NAME_LEN)).encode();

        assert_eq!(
            encoded.len(),
            272,
            "the name must be cut to the last whole character within the bound"
        );
        assert_eq!(
            ControlPacket::parse(&encoded),
            Ok(ControlPacket::invitation(1, 2, "\u{2019}".repeat(85))),
            "the cut name must parse back as whole characters"
        );
    }
}
