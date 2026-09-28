//! The controller state an endpoint has been sent, so a recovered link can be brought back to it.
//!
//! A link that drops and returns has lost whatever changed while it was down, and a fresh session
//! starts with an empty recovery journal. Without a resend the receiver keeps the volume, program
//! and pitch it last heard, which is FR-027's stale value. Notes are deliberately absent: a note
//! is silenced when its link drops (FR-026) and must never be replayed.

use crate::midi::{CC_ALL_SOUND_OFF, CC_RESET_ALL_CONTROLLERS, Channel, MidiMessage};

/// Bank select, most and least significant bytes, which have to reach the receiver before the
/// program change they qualify.
const CC_BANK_SELECT_MSB: u8 = 0;
const CC_BANK_SELECT_LSB: u8 = 32;

/// Controllers that only mean something in sequence with others, and are not replayed.
///
/// Data entry (6 and 38) and increment and decrement (96 and 97) apply to whichever parameter
/// the RPN and NRPN selectors (98 to 101) last chose. Replayed in numeric order they would land
/// on the wrong parameter, so a stale pitch-bend range is the lesser harm than a wrong one.
const SEQUENCED: [u8; 8] = [6, 38, 96, 97, 98, 99, 100, 101];

/// The last values one channel was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChannelControls {
    /// Per controller below the channel-mode range, which starts at all-sound-off.
    controllers: [Option<u8>; CC_ALL_SOUND_OFF as usize],
    program: Option<u8>,
    pitch: Option<u16>,
    pressure: Option<u8>,
}

impl Default for ChannelControls {
    fn default() -> Self {
        Self {
            controllers: [None; CC_ALL_SOUND_OFF as usize],
            program: None,
            pitch: None,
            pressure: None,
        }
    }
}

/// The controller, program, pitch-bend and channel-pressure values an endpoint was last sent,
/// per channel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Controls {
    channels: [ChannelControls; 16],
}

impl Controls {
    /// Creates a record with nothing sent.
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates the record from one message on its way to the endpoint.
    pub fn record(&mut self, message: &MidiMessage) {
        let Some(channel) = message.channel() else {
            return;
        };
        let Some(state) = self.channels.get_mut(usize::from(channel.index())) else {
            return;
        };
        match *message {
            MidiMessage::ControlChange {
                controller, value, ..
            } => {
                // Reset all controllers returns a channel to its defaults, so there is nothing
                // left to restore on it.
                if controller == CC_RESET_ALL_CONTROLLERS {
                    *state = ChannelControls {
                        program: state.program,
                        ..ChannelControls::default()
                    };
                } else if !SEQUENCED.contains(&controller)
                    && let Some(slot) = state.controllers.get_mut(usize::from(controller))
                {
                    *slot = Some(value);
                }
            }
            MidiMessage::ProgramChange { program, .. } => state.program = Some(program),
            MidiMessage::PitchBend { value, .. } => state.pitch = Some(value),
            MidiMessage::ChannelAftertouch { pressure, .. } => state.pressure = Some(pressure),
            _ => {}
        }
    }

    /// Returns the messages that bring a receiver back to what it was last sent.
    ///
    /// Per channel: bank select, then the program it qualifies, then every other controller,
    /// then pitch bend and pressure. Empty when nothing was ever sent.
    pub fn restore(&self) -> Vec<MidiMessage> {
        let mut messages = Vec::new();
        for channel in Channel::all() {
            let Some(state) = self.channels.get(usize::from(channel.index())) else {
                continue;
            };
            let control = |controller: u8| {
                state
                    .controllers
                    .get(usize::from(controller))
                    .copied()
                    .flatten()
                    .map(|value| MidiMessage::ControlChange {
                        channel,
                        controller,
                        value,
                    })
            };
            messages.extend(control(CC_BANK_SELECT_MSB));
            messages.extend(control(CC_BANK_SELECT_LSB));
            messages.extend(
                state
                    .program
                    .map(|program| MidiMessage::ProgramChange { channel, program }),
            );
            messages.extend(
                (0..CC_ALL_SOUND_OFF)
                    .filter(|controller| {
                        *controller != CC_BANK_SELECT_MSB && *controller != CC_BANK_SELECT_LSB
                    })
                    .filter_map(control),
            );
            messages.extend(
                state
                    .pitch
                    .map(|value| MidiMessage::PitchBend { channel, value }),
            );
            messages.extend(
                state
                    .pressure
                    .map(|pressure| MidiMessage::ChannelAftertouch { channel, pressure }),
            );
        }
        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(number: u8) -> Channel {
        Channel::new(number).expect("a channel")
    }

    fn control(number: u8, controller: u8, value: u8) -> MidiMessage {
        MidiMessage::ControlChange {
            channel: channel(number),
            controller,
            value,
        }
    }

    fn program(number: u8, program: u8) -> MidiMessage {
        MidiMessage::ProgramChange {
            channel: channel(number),
            program,
        }
    }

    fn pitch(number: u8, value: u16) -> MidiMessage {
        MidiMessage::PitchBend {
            channel: channel(number),
            value,
        }
    }

    fn pressure(number: u8, pressure: u8) -> MidiMessage {
        MidiMessage::ChannelAftertouch {
            channel: channel(number),
            pressure,
        }
    }

    /// The resend after a recovery brings a receiver back to the last value of each control, in
    /// an order the receiver can act on, and replays nothing that would do harm (FR-027).
    ///
    /// Per channel, bank select comes before the program change it qualifies, as the MIDI 1.0
    /// specification requires for a bank to apply. Data entry, increment and decrement and the
    /// RPN and NRPN selectors (controllers 6, 38 and 96 to 101) only mean something in sequence,
    /// so they are left out. Notes are silenced when a link drops (FR-026) and never replayed,
    /// and reset-all-controllers (121) leaves only the program to restore.
    #[test]
    fn a_recovered_link_is_sent_the_last_value_of_each_control_in_an_order_it_can_use() {
        let cases = [
            ("nothing sent restores nothing", vec![], vec![]),
            (
                "the last value of each control is restored, channel by channel",
                vec![
                    control(0, 7, 40),
                    control(0, 7, 90),
                    control(2, 10, 64),
                    program(2, 12),
                    pitch(0, 9_000),
                    pressure(0, 33),
                ],
                vec![
                    control(0, 7, 90),
                    pitch(0, 9_000),
                    pressure(0, 33),
                    program(2, 12),
                    control(2, 10, 64),
                ],
            ),
            (
                "bank select goes before the program it qualifies",
                vec![
                    control(0, 1, 20),
                    program(0, 5),
                    control(0, 32, 3),
                    control(0, 0, 1),
                ],
                vec![
                    control(0, 0, 1),
                    control(0, 32, 3),
                    program(0, 5),
                    control(0, 1, 20),
                ],
            ),
            (
                "notes and poly pressure are never replayed",
                vec![
                    MidiMessage::NoteOn {
                        channel: channel(0),
                        note: 60,
                        velocity: 100,
                    },
                    MidiMessage::PolyAftertouch {
                        channel: channel(0),
                        note: 60,
                        pressure: 50,
                    },
                ],
                vec![],
            ),
            (
                "sequenced controllers and channel-mode messages are left out",
                SEQUENCED
                    .iter()
                    .map(|controller| control(0, *controller, 1))
                    .chain([control(0, CC_ALL_SOUND_OFF, 0)])
                    .collect(),
                vec![],
            ),
            (
                "reset all controllers leaves only the program",
                vec![
                    control(0, 7, 90),
                    program(0, 9),
                    pitch(0, 100),
                    control(0, CC_RESET_ALL_CONTROLLERS, 0),
                ],
                vec![program(0, 9)],
            ),
        ];
        for (case, sent, want) in cases {
            let mut controls = Controls::new();
            for message in &sent {
                controls.record(message);
            }
            assert_eq!(
                controls.restore(),
                want,
                "{case}: the resend must bring the receiver back to what it was last sent"
            );
        }
    }
}
