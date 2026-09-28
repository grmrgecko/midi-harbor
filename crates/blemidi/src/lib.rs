//! Bluetooth LE MIDI packet codec.
//!
//! Pure framing, with no radio and no platform behind it, so every rule in the format is testable
//! on a machine with no Bluetooth at all.

pub mod codec;

pub use codec::{
    CHARACTERISTIC_UUID, DecodeError, Decoder, EncodeError, Encoder, Event, MAX_PACKET, MIN_PACKET,
    SERVICE_UUID, TIMESTAMP_PERIOD,
};
