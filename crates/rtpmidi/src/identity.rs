//! Midi Harbor's identity exchange, an extension to the AppleMIDI control protocol.
//!
//! One network port asks another to prove which port it is. The answer carries the public key
//! of the daemon that runs the port, the port's identifier, and a signature over both with the
//! challenge. It lets a port that has moved to another address be recognised as the port it was,
//! which an advertised name alone cannot do (R-106).
//!
//! The packets travel on the control port under the control signature, with commands of their
//! own. No other implementation knows them, so they are sent only to a session that advertises
//! a key. This module carries the bytes; signing and verifying belong to the caller.

use crate::control::SIGNATURE;

/// The version of the identity exchange this implementation speaks.
pub const IDENTITY_VERSION: u32 = 1;

/// Bytes in a challenge's nonce.
pub const NONCE_LEN: usize = 32;

/// Bytes in an Ed25519 public key.
pub const KEY_LEN: usize = 32;

/// Bytes in a port identifier, a UUID.
pub const PORT_ID_LEN: usize = 16;

/// Bytes in an Ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;

/// The command bytes of a challenge.
const CHALLENGE: [u8; 2] = *b"HQ";

/// The command bytes of a proof.
const PROOF: [u8; 2] = *b"HA";

/// Bytes before the fields: the signature, the command and the version.
const HEADER_LEN: usize = 2 + 2 + 4;

/// Bytes in a challenge.
const CHALLENGE_LEN: usize = HEADER_LEN + NONCE_LEN;

/// Bytes in a proof.
const PROOF_LEN: usize = HEADER_LEN + NONCE_LEN + KEY_LEN + PORT_ID_LEN + SIGNATURE_LEN;

/// Why bytes could not be read as an identity packet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// The bytes are some other packet, which is ordinary: MIDI and session control share the
    /// port.
    #[error("not an identity packet")]
    NotIdentity,
    /// The packet ended before a field it must carry.
    #[error("packet is {actual} bytes, needs at least {expected}")]
    TooShort {
        /// How many bytes were present.
        actual: usize,
        /// How many were needed.
        expected: usize,
    },
    /// The sender speaks a version of the exchange this implementation does not.
    #[error("identity exchange version {found}, expected {IDENTITY_VERSION}")]
    UnsupportedVersion {
        /// The version the sender declared.
        found: u32,
    },
}

/// A packet of the identity exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityPacket {
    /// Asks the port listening where this is sent to prove which port it is.
    Challenge {
        /// Chosen at random by the asker, so an old proof cannot answer a new challenge.
        nonce: [u8; NONCE_LEN],
    },
    /// Answers a challenge.
    Proof {
        /// The challenge's nonce, tying the answer to its question.
        nonce: [u8; NONCE_LEN],
        /// The public key of the daemon that runs the port.
        key: [u8; KEY_LEN],
        /// The port's identifier, which a rename does not change.
        port_id: [u8; PORT_ID_LEN],
        /// The signature over the nonce, the asker's address and the port's identifier.
        signature: [u8; SIGNATURE_LEN],
    },
}

impl IdentityPacket {
    /// Encodes the packet for the wire.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PROOF_LEN);
        out.extend_from_slice(&SIGNATURE);
        match self {
            Self::Challenge { nonce } => {
                out.extend_from_slice(&CHALLENGE);
                out.extend_from_slice(&IDENTITY_VERSION.to_be_bytes());
                out.extend_from_slice(nonce);
            }
            Self::Proof {
                nonce,
                key,
                port_id,
                signature,
            } => {
                out.extend_from_slice(&PROOF);
                out.extend_from_slice(&IDENTITY_VERSION.to_be_bytes());
                out.extend_from_slice(nonce);
                out.extend_from_slice(key);
                out.extend_from_slice(port_id);
                out.extend_from_slice(signature);
            }
        }
        out
    }

    /// Parses a packet received on the control port.
    ///
    /// Bytes past the last field are ignored, so a later version can add to a packet without
    /// this one refusing it.
    pub fn parse(bytes: &[u8]) -> Result<Self, IdentityError> {
        // Validate the framing before reading anything.
        if bytes.get(..2) != Some(SIGNATURE.as_slice()) {
            return Err(IdentityError::NotIdentity);
        }
        let (challenge, expected) = match bytes.get(2..4) {
            Some(command) if command == CHALLENGE => (true, CHALLENGE_LEN),
            Some(command) if command == PROOF => (false, PROOF_LEN),
            _ => return Err(IdentityError::NotIdentity),
        };
        if bytes.len() < expected {
            return Err(IdentityError::TooShort {
                actual: bytes.len(),
                expected,
            });
        }
        let version = u32::from_be_bytes(field(bytes, 4)?);
        if version != IDENTITY_VERSION {
            return Err(IdentityError::UnsupportedVersion { found: version });
        }

        // Read the fields, each following the last.
        let nonce = field(bytes, HEADER_LEN)?;
        if challenge {
            return Ok(Self::Challenge { nonce });
        }
        let key_at = HEADER_LEN + NONCE_LEN;
        let port_id_at = key_at + KEY_LEN;
        let signature_at = port_id_at + PORT_ID_LEN;
        Ok(Self::Proof {
            nonce,
            key: field(bytes, key_at)?,
            port_id: field(bytes, port_id_at)?,
            signature: field(bytes, signature_at)?,
        })
    }
}

/// Reads a fixed-size field without risking an out-of-bounds read.
fn field<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], IdentityError> {
    let end = offset.saturating_add(N);
    bytes
        .get(offset..end)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(IdentityError::TooShort {
            actual: bytes.len(),
            expected: end,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves both packets survive the wire, that anything else on the control port is passed
    /// over as not an identity packet, and that a truncated packet or another version is refused
    /// without reading past its end.
    ///
    /// A challenge is 8 + 32 = 40 bytes and a proof 8 + 32 + 32 + 16 + 64 = 152. `FF FF 49 4E`
    /// opens an AppleMIDI invitation, which shares the port.
    #[test]
    fn identity_packets_survive_the_wire_and_nothing_else_is_taken_for_one() {
        let challenge = IdentityPacket::Challenge { nonce: [7; 32] };
        let proof = IdentityPacket::Proof {
            nonce: [7; 32],
            key: [8; 32],
            port_id: [9; 16],
            signature: [10; 64],
        };
        assert_eq!(challenge.encode().len(), 40, "a challenge is 40 bytes");
        assert_eq!(proof.encode().len(), 152, "a proof is 152 bytes");
        for packet in [challenge.clone(), proof.clone()] {
            assert_eq!(
                IdentityPacket::parse(&packet.encode()),
                Ok(packet),
                "a packet must read back as it was sent"
            );
        }

        let mut newer = challenge.encode();
        newer[7] = 2;
        let mut longer = challenge.encode();
        longer.extend_from_slice(&[1, 2, 3]);
        let cases: [(&str, Vec<u8>, Result<IdentityPacket, IdentityError>); 5] = [
            (
                "an AppleMIDI invitation",
                vec![0xFF, 0xFF, b'I', b'N', 0, 0, 0, 2],
                Err(IdentityError::NotIdentity),
            ),
            (
                "nothing at all",
                Vec::new(),
                Err(IdentityError::NotIdentity),
            ),
            (
                "a proof cut short",
                proof.encode()[..100].to_vec(),
                Err(IdentityError::TooShort {
                    actual: 100,
                    expected: 152,
                }),
            ),
            (
                "a later version",
                newer,
                Err(IdentityError::UnsupportedVersion { found: 2 }),
            ),
            ("a challenge with more after it", longer, Ok(challenge)),
        ];
        for (name, bytes, want) in cases {
            assert_eq!(IdentityPacket::parse(&bytes), want, "{name}: read wrongly");
        }
    }
}
