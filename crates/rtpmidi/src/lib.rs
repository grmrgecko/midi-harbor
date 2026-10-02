//! AppleMIDI session control and the RFC 6295 recovery journal.
//!
//! Pure: this crate parses and builds bytes and advances state machines, and never touches a
//! socket or a clock. Everything here must be exercisable on a machine with no network at all.
//!
//! All input is treated as hostile. A malformed packet from a peer produces an error, never a
//! panic, an unbounded allocation, or an out-of-bounds read.

pub mod clock;
pub mod control;
pub mod identity;
pub mod journal;
pub mod packet;
pub mod session;

pub use clock::{ClockAction, ClockSync};
pub use control::{ControlPacket, Handshake, ParseError, SessionCommand};
pub use identity::{IdentityError, IdentityPacket};
pub use packet::{PacketError, RtpMidiPacket, SysExPart, SysExSegment, TimedMessage};
pub use session::{Action, Phase, Port, Role, Session, SessionFailure};
