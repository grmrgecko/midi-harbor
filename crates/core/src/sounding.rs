//! Which notes an endpoint has left sounding.
//!
//! A note on with no matching note off is the failure a musician hears, and the one they cannot
//! fix from the application that caused it — the note is sounding on a synth that is no longer
//! being told anything. Everything that stops carrying MIDI has to stop the notes it started
//! first, and that means knowing which ones those are.

use crate::midi::{CC_ALL_NOTES_OFF, CC_ALL_SOUND_OFF, Channel, MidiMessage, silence_channel};

/// How many notes one channel can hold, which is the full seven-bit range.
const NOTES_PER_CHANNEL: u8 = 128;

/// The notes one endpoint currently has sounding, by channel.
///
/// Tracked per channel rather than silencing all sixteen, because a reset on a channel nothing
/// was playing still clears sustain and cuts sound for whatever else is driving that port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sounding {
    /// One bit per note, one word per channel.
    notes: [u128; 16],
}

impl Sounding {
    /// Creates a record with nothing sounding.
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates the record from one message on its way out of an endpoint.
    pub fn record(&mut self, message: &MidiMessage) {
        // A note on with zero velocity is a note off by convention, so `stops_note` is asked
        // first: treating it as a start is the classic way to leave a note hanging forever.
        if let Some((channel, note)) = message.stops_note() {
            self.set(channel, note, false);
            return;
        }
        if let MidiMessage::NoteOn { channel, note, .. } = message {
            self.set(*channel, *note, true);
            return;
        }
        // A reset the sender issued itself leaves the channel quiet, so silencing later has
        // nothing left to say about it.
        if let MidiMessage::ControlChange {
            channel,
            controller,
            ..
        } = message
            && (*controller == CC_ALL_NOTES_OFF || *controller == CC_ALL_SOUND_OFF)
        {
            self.clear_channel(*channel);
        }
    }

    /// Returns the messages that stop everything this endpoint has sounding.
    ///
    /// Each played channel gets its pedal released, a note off for every note still sounding,
    /// then the two broad resets. The note offs are for a synth that ignores all-notes-off, and
    /// come after the pedal release so the pedal does not hold them.
    ///
    /// Empty when nothing is sounding, which is how a caller avoids disturbing a port that was
    /// never playing anything.
    pub fn silence(&self) -> Vec<MidiMessage> {
        self.silence_beside(0)
    }

    /// Returns the channels with anything sounding, one bit per channel.
    pub fn channels(&self) -> u16 {
        self.notes
            .iter()
            .enumerate()
            .filter(|(_, word)| **word != 0)
            .fold(0, |channels, (index, _)| channels | (1 << index))
    }

    /// Returns the messages that stop what this record has sounding, on a port others also play.
    ///
    /// A channel in `shared`, one bit per channel as [`Sounding::channels`] gives them, has
    /// another sender's notes sounding on it. It gets only this record's note offs: the pedal
    /// release and the broad resets would stop the other sender's notes as well.
    pub fn silence_beside(&self, shared: u16) -> Vec<MidiMessage> {
        let mut messages = Vec::new();
        for channel in Channel::all() {
            let Some(word) = self
                .notes
                .get(usize::from(channel.index()))
                .copied()
                .filter(|word| *word != 0)
            else {
                continue;
            };
            let alone = shared & (1 << channel.index()) == 0;
            let [pedal, notes_off, sound_off] = silence_channel(channel);
            if alone {
                messages.push(pedal);
            }
            messages.extend(
                (0..NOTES_PER_CHANNEL)
                    .filter(|note| bit(*note).is_some_and(|bit| word & bit != 0))
                    .map(|note| MidiMessage::NoteOff {
                        channel,
                        note,
                        velocity: 0,
                    }),
            );
            if alone {
                messages.push(notes_off);
                messages.push(sound_off);
            }
        }
        messages
    }

    /// Forgets everything, for an endpoint that is no longer able to sound anything.
    pub fn clear(&mut self) {
        self.notes = [0; 16];
    }

    /// Records one note as sounding or stopped.
    fn set(&mut self, channel: Channel, note: u8, sounding: bool) {
        let Some(word) = self.notes.get_mut(usize::from(channel.index())) else {
            return;
        };
        let Some(bit) = bit(note) else {
            return;
        };
        if sounding {
            *word |= bit;
        } else {
            *word &= !bit;
        }
    }

    /// Records a whole channel as quiet.
    fn clear_channel(&mut self, channel: Channel) {
        if let Some(word) = self.notes.get_mut(usize::from(channel.index())) {
            *word = 0;
        }
    }
}

/// Returns the bit standing for one note, or nothing for a value outside the seven-bit range.
fn bit(note: u8) -> Option<u128> {
    (note < NOTES_PER_CHANNEL).then(|| 1u128 << note)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::midi::CC_SUSTAIN;

    fn channel(index: u8) -> Channel {
        Channel::new(index).expect("channel in range")
    }

    fn note_on(index: u8, note: u8, velocity: u8) -> MidiMessage {
        MidiMessage::NoteOn {
            channel: channel(index),
            note,
            velocity,
        }
    }

    fn note_off(index: u8, note: u8) -> MidiMessage {
        MidiMessage::NoteOff {
            channel: channel(index),
            note,
            velocity: 0,
        }
    }

    fn controller(index: u8, controller: u8) -> MidiMessage {
        MidiMessage::ControlChange {
            channel: channel(index),
            controller,
            value: 0,
        }
    }

    /// Returns the full silence for one channel with the given notes held, in the order it is
    /// sent: pedal release, a note off per note, all-notes-off, all-sound-off.
    fn silenced(index: u8, notes: &[u8]) -> Vec<MidiMessage> {
        let mut messages = vec![controller(index, CC_SUSTAIN)];
        messages.extend(notes.iter().map(|note| note_off(index, *note)));
        messages.push(controller(index, CC_ALL_NOTES_OFF));
        messages.push(controller(index, CC_ALL_SOUND_OFF));
        messages
    }

    /// Silencing stops exactly the notes a sender left sounding, on only the channels it played,
    /// and leaves alone what another sender is playing.
    ///
    /// The pedal is released first because a held sustain pedal keeps notes sounding through
    /// all-notes-off, and each held note gets its own note off for a synth that ignores
    /// all-notes-off. A channel nothing played is not reset, since a reset still clears sustain
    /// for whatever else drives the port, and on a channel another sender is playing the pedal
    /// release and broad resets would stop that sender's notes too, so only note offs go. A note
    /// on with zero velocity is a note off (MIDI 1.0), and a note number outside the seven-bit
    /// range is hostile input that must be ignored rather than index out of bounds.
    #[test]
    fn silencing_stops_exactly_what_was_left_sounding() {
        let cases = [
            (
                "a released note",
                vec![note_on(0, 60, 100), note_off(0, 60)],
                vec![],
                vec![],
            ),
            (
                "a note released by a note on with zero velocity",
                vec![note_on(0, 60, 100), note_on(0, 60, 0)],
                vec![],
                vec![],
            ),
            (
                "the same note struck twice and released once",
                vec![note_on(0, 60, 100), note_on(0, 60, 90), note_off(0, 60)],
                vec![],
                vec![],
            ),
            (
                "notes the sender silenced itself",
                vec![
                    note_on(0, 60, 100),
                    note_on(0, 64, 100),
                    controller(0, CC_ALL_NOTES_OFF),
                ],
                vec![],
                vec![],
            ),
            (
                "notes out of the seven-bit range",
                vec![note_on(0, 200, 100), note_off(0, 255)],
                vec![],
                vec![],
            ),
            (
                "held drum notes on channel 10 only, released by name in ascending order",
                vec![
                    note_on(9, 82, 100),
                    note_on(9, 36, 100),
                    note_on(9, 40, 100),
                    note_off(9, 40),
                ],
                vec![],
                silenced(9, &[36, 82]),
            ),
            (
                "notes on three channels, each silenced in channel order",
                vec![
                    note_on(15, 60, 100),
                    note_on(0, 60, 100),
                    note_on(9, 60, 100),
                ],
                vec![],
                [silenced(0, &[60]), silenced(9, &[60]), silenced(15, &[60])].concat(),
            ),
            (
                "a channel another sender is playing gets only note offs",
                vec![note_on(0, 60, 100), note_on(9, 36, 100)],
                vec![note_on(9, 38, 100)],
                [silenced(0, &[60]), vec![note_off(9, 36)]].concat(),
            ),
        ];
        for (case, played, beside, want) in cases {
            let mut sounding = Sounding::new();
            for message in &played {
                sounding.record(message);
            }
            let mut other = Sounding::new();
            for message in &beside {
                other.record(message);
            }
            assert_eq!(
                sounding.silence_beside(other.channels()),
                want,
                "{case}: silencing must stop what was left sounding and nothing else"
            );
        }
    }
}
