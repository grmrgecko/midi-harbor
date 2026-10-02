//! Which daemon this is, provably, and which of its network ports a session is.
//!
//! A network port follows a machine it connected to when that machine's session is advertised
//! somewhere else. An advertisement proves nothing, so before a machine is followed to another
//! host it is asked to sign a challenge with the key it held where it was connected to (R-106).
//! Only another Midi Harbor can answer. Every other implementation is followed by name, and
//! only where that cannot let a stranger in.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::paths::Paths;
use midi_harbor_rtpmidi::identity::{KEY_LEN, NONCE_LEN, PORT_ID_LEN, SIGNATURE_LEN};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

/// File name of the daemon's key, beside the configuration document.
const KEY_FILE: &str = "identity.key";

/// Opens every message signed, so a signature made for this exchange means nothing anywhere else.
const DOMAIN: &[u8] = b"midi-harbor network port identity v1\0";

/// Why the daemon's key could not be read or kept.
#[derive(Debug, thiserror::Error)]
pub enum IdentityFileError {
    /// The key file could not be read or written.
    #[error("could not use {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        source: std::io::Error,
    },
    /// The key file holds something other than a key.
    #[error("{path} does not hold a key")]
    Malformed {
        /// The file.
        path: PathBuf,
    },
}

/// One network port of one daemon, as a session proves it and a remembered machine stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortIdentity {
    /// The public key of the daemon that runs the port.
    pub key: [u8; KEY_LEN],
    /// The port's identifier in that daemon's configuration, which a rename does not change.
    pub port: EndpointId,
}

impl PortIdentity {
    /// Reads an identity from a key in hexadecimal and a port identifier, as the configuration
    /// writes them.
    pub fn new(key: &str, port: EndpointId) -> Option<Self> {
        let mut bytes = [0u8; KEY_LEN];
        hex::decode_to_slice(key, &mut bytes).ok()?;
        Some(Self { key: bytes, port })
    }

    /// Reads an identity from an advertisement's two properties.
    pub fn from_text(key: &str, port: &str) -> Option<Self> {
        Self::new(key, EndpointId::parse(port).ok()?)
    }

    /// Returns the key in hexadecimal.
    pub fn key_text(&self) -> String {
        hex::encode(self.key)
    }
}

/// The key this daemon signs with.
pub struct Identity {
    signing: SigningKey,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The public half only. The private one must not reach a log.
        write!(f, "Identity({})", hex::encode(self.public_key()))
    }
}

impl Identity {
    /// Reads the daemon's key, making and storing one the first time.
    ///
    /// Kept in a file of its own rather than in the configuration document, which is exported,
    /// copied to other machines and pasted into bug reports. Two machines sharing a key would
    /// each pass for the other.
    pub fn load_or_create(paths: &Paths) -> Result<Self, IdentityFileError> {
        let path = paths.config_dir().join(KEY_FILE);
        let io = |source| IdentityFileError::Io {
            path: path.clone(),
            source,
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut seed = [0u8; KEY_LEN];
                hex::decode_to_slice(text.trim(), &mut seed)
                    .map_err(|_| IdentityFileError::Malformed { path: path.clone() })?;
                Ok(Self {
                    signing: SigningKey::from_bytes(&seed),
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let identity = Self::generate();
                std::fs::create_dir_all(paths.config_dir()).map_err(io)?;
                write_private(&path, &hex::encode(identity.signing.to_bytes())).map_err(io)?;
                Ok(identity)
            }
            Err(error) => Err(io(error)),
        }
    }

    /// Makes a key that is not stored, for a daemon that cannot keep one. It still answers
    /// challenges, and is a stranger to every machine after a restart.
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::from_bytes(&rand::random::<[u8; KEY_LEN]>()),
        }
    }

    /// Returns the public key other machines know this daemon by.
    pub fn public_key(&self) -> [u8; KEY_LEN] {
        self.signing.verifying_key().to_bytes()
    }

    /// Signs the answer to a challenge from `asker`, for the port `port`.
    pub fn prove(
        &self,
        nonce: &[u8; NONCE_LEN],
        asker: SocketAddr,
        port: EndpointId,
    ) -> [u8; SIGNATURE_LEN] {
        self.signing
            .sign(&signed_message(nonce, asker, port))
            .to_bytes()
    }
}

/// Reports whether `signature` is `identity`'s answer to the challenge `nonce` from `asker`.
pub fn verifies(
    identity: &PortIdentity,
    nonce: &[u8; NONCE_LEN],
    asker: SocketAddr,
    signature: &[u8; SIGNATURE_LEN],
) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(&identity.key) else {
        return false;
    };
    key.verify_strict(
        &signed_message(nonce, asker, identity.port),
        &Signature::from_bytes(signature),
    )
    .is_ok()
}

/// Builds the bytes a proof signs: the challenge, who asked, and which port answers.
///
/// The asker's address, as the answering port saw it, is what stops a machine passing a
/// challenge on to the real port and returning its answer as its own: the real port would sign
/// the go-between's address, not the asker's. The address is written as sixteen bytes, an IPv4
/// one mapped into IPv6, then the port, so both sides build the same bytes whichever family the
/// packet travelled in.
fn signed_message(nonce: &[u8; NONCE_LEN], asker: SocketAddr, port: EndpointId) -> Vec<u8> {
    let address = match asker.ip().to_canonical() {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
    };
    let mut message = Vec::with_capacity(DOMAIN.len() + NONCE_LEN + 16 + 2 + PORT_ID_LEN);
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(nonce);
    message.extend_from_slice(&address.octets());
    message.extend_from_slice(&asker.port().to_be_bytes());
    message.extend_from_slice(&port.to_bytes());
    message
}

/// Writes a file only its owner can read.
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    let _ = options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Read and write for the owner, nothing for anyone else.
        let _ = options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves a proof is accepted only as the answer it was signed as: by that key, for that
    /// port, to that challenge, from that asker. An IPv4 asker seen through a dual-stack socket
    /// arrives IPv4-mapped, and is the same asker.
    #[test]
    fn a_proof_answers_only_the_challenge_it_was_signed_for() {
        let identity = Identity::generate();
        let port = PortIdentity {
            key: identity.public_key(),
            port: EndpointId::new(),
        };
        let nonce = [5; NONCE_LEN];
        let asker: SocketAddr = "192.0.2.10:5004".parse().unwrap();
        let signature = identity.prove(&nonce, asker, port.port);

        let other_port = PortIdentity {
            port: EndpointId::new(),
            ..port
        };
        let other_key = PortIdentity {
            key: Identity::generate().public_key(),
            ..port
        };
        let cases = [
            ("the challenge it answers", port, nonce, asker, true),
            (
                "the same asker, IPv4-mapped",
                port,
                nonce,
                "[::ffff:192.0.2.10]:5004".parse().unwrap(),
                true,
            ),
            ("another challenge", port, [6; NONCE_LEN], asker, false),
            (
                "another asker, as when the challenge was passed on",
                port,
                nonce,
                "192.0.2.11:5004".parse().unwrap(),
                false,
            ),
            (
                "another port of the daemon",
                other_port,
                nonce,
                asker,
                false,
            ),
            ("another daemon's key", other_key, nonce, asker, false),
        ];
        for (name, expected, nonce, asker, want) in cases {
            assert_eq!(
                verifies(&expected, &nonce, asker, &signature),
                want,
                "{name}: accepted or refused wrongly"
            );
        }
    }
}
