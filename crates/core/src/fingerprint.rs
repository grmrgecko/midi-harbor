//! Identifying physical MIDI hardware across unplugging and replugging.

use serde::{Deserialize, Serialize};
use std::fmt;

/// How confidently a stored fingerprint matches hardware currently attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchConfidence {
    /// Nothing matched.
    None,
    /// Only the reported name matched, or several devices matched equally well.
    ///
    /// Never rebinds a route on its own: binding the wrong hardware is worse than asking.
    Ambiguous,
    /// Manufacturer, model and name all matched, with no competing candidate.
    Probable,
    /// A hardware-unique value matched — a CoreMIDI unique id or a USB serial number.
    Exact,
}

impl MatchConfidence {
    /// Reports whether this match is strong enough to rebind a route without asking the user.
    pub fn is_automatic(&self) -> bool {
        matches!(self, Self::Exact | Self::Probable)
    }
}

/// The set of identifying values a platform reports for one piece of MIDI hardware.
///
/// No single field is reliable on every platform, so identity is a composite. `name` is the only
/// field always present and is never sufficient on its own, because two identical devices report
/// the same one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceFingerprint {
    /// CoreMIDI's `kMIDIPropertyUniqueID`, stable across replug on macOS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_id: Option<u32>,
    /// The USB serial number, when the device reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb_serial: Option<String>,
    /// The manufacturer as reported by the device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    /// The model as reported by the device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The display name. Always present, never sufficient alone.
    pub name: String,
    /// The physical connection path, stable per socket but not across sockets.
    ///
    /// The fallback for Linux hardware that reports no serial number, and the reason such devices
    /// can only ever match `Ambiguous` when moved to a different port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topology_path: Option<String>,
}

impl DeviceFingerprint {
    /// Creates a fingerprint carrying only a name, which is all some platforms report.
    pub fn from_name(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Reports whether this fingerprint carries a hardware-unique value.
    pub fn has_unique_identity(&self) -> bool {
        self.unique_id.is_some() || self.usb_serial.is_some()
    }

    /// Scores how well `candidate` matches this stored fingerprint.
    ///
    /// Unique values are checked first and are decisive in both directions: a stored unique value
    /// that disagrees with the candidate's means this is different hardware, whatever else lines
    /// up.
    pub fn compare(&self, candidate: &Self) -> MatchConfidence {
        // A hardware-unique value settles the question outright.
        if let (Some(mine), Some(theirs)) = (self.unique_id, candidate.unique_id) {
            return if mine == theirs {
                MatchConfidence::Exact
            } else {
                MatchConfidence::None
            };
        }
        if let (Some(mine), Some(theirs)) = (&self.usb_serial, &candidate.usb_serial) {
            return if mine == theirs {
                MatchConfidence::Exact
            } else {
                MatchConfidence::None
            };
        }

        // Without one, a full descriptive match is good enough to rebind automatically.
        if self.name != candidate.name {
            return MatchConfidence::None;
        }
        let descriptive = self.manufacturer.is_some()
            && self.model.is_some()
            && self.manufacturer == candidate.manufacturer
            && self.model == candidate.model;
        if descriptive {
            return MatchConfidence::Probable;
        }

        // The same socket is decent evidence, but only for hardware that has not been moved.
        if self.topology_path.is_some() && self.topology_path == candidate.topology_path {
            return MatchConfidence::Probable;
        }

        // Identical in everything known. There is nothing more to learn about this device, and
        // nothing says it is a different one. Refusing it would add the device again on every
        // enumeration, since the entry made for it would not match it either. Two such devices
        // still tie, and a tie is never bound.
        if self == candidate {
            return MatchConfidence::Probable;
        }

        // Name alone. Correct often enough to offer, never enough to assume.
        MatchConfidence::Ambiguous
    }

    /// Reports whether both were found in the same place.
    ///
    /// Only true when both know where they are: two devices that report no position are not in
    /// the same one, they are simply unplaced.
    fn same_position(&self, candidate: &Self) -> bool {
        self.topology_path.is_some() && self.topology_path == candidate.topology_path
    }

    /// Picks the best match for this fingerprint among `candidates`.
    ///
    /// Returns `Ambiguous` when several candidates tie at the best score, because choosing between
    /// two identical devices is the user's call.
    pub fn best_match<'a>(&self, candidates: &'a [Self]) -> (Option<&'a Self>, MatchConfidence) {
        let mut best: Option<&Self> = None;
        let mut best_score = MatchConfidence::None;
        let mut tied = false;

        for candidate in candidates {
            let score = self.compare(candidate);
            if score == MatchConfidence::None {
                continue;
            }
            if score > best_score {
                best_score = score;
                best = Some(candidate);
                tied = false;
            } else if score == best_score {
                // Where it is plugged in breaks a tie that nothing else can. Two identical
                // devices score the same on every descriptive field, so without this they are
                // ambiguous even when one of them is in the very socket this entry remembers.
                match (
                    self.same_position(candidate),
                    best.is_some_and(|b| self.same_position(b)),
                ) {
                    (true, false) => {
                        best = Some(candidate);
                        tied = false;
                    }
                    (false, true) => {}
                    _ => tied = true,
                }
            }
        }

        match (best, tied) {
            (None, _) => (None, MatchConfidence::None),
            (Some(found), true) => (Some(found), MatchConfidence::Ambiguous),
            (Some(found), false) => (Some(found), best_score),
        }
    }
}

impl fmt::Display for DeviceFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if let Some(serial) = &self.usb_serial {
            write!(f, " (serial {serial})")?;
        } else if let Some(id) = self.unique_id {
            write!(f, " (id {id})")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyboard() -> DeviceFingerprint {
        DeviceFingerprint {
            unique_id: None,
            usb_serial: Some("SN-001".to_owned()),
            manufacturer: Some("Acme".to_owned()),
            model: Some("K61".to_owned()),
            name: "Acme K61".to_owned(),
            topology_path: Some("usb-0:1.2".to_owned()),
        }
    }

    /// Attached hardware is matched to a stored entry by the strongest evidence available, and
    /// two identical devices tie rather than one being picked.
    ///
    /// A serial number is decisive both ways: it follows the device to another socket (FR-015e)
    /// and a different one is different hardware whatever else agrees. Manufacturer, model and
    /// name without a serial are enough to rebind. A device reporting only a name matches itself,
    /// or the entry made for it would never match and it would be added again on every
    /// enumeration, but two such devices attached at once are ambiguous (FR-015g), because
    /// binding the wrong one is worse than asking.
    #[test]
    fn hardware_is_matched_by_its_strongest_evidence() {
        let moved = DeviceFingerprint {
            topology_path: Some("usb-0:4.1".to_owned()),
            ..keyboard()
        };
        let other_serial = DeviceFingerprint {
            usb_serial: Some("SN-002".to_owned()),
            ..keyboard()
        };
        let no_serial = DeviceFingerprint {
            usb_serial: None,
            ..keyboard()
        };
        let named = DeviceFingerprint::from_name("Keystation");
        let cases = [
            (
                "a serial matches across a different socket",
                keyboard(),
                vec![moved],
                MatchConfidence::Exact,
            ),
            (
                "a differing serial is different hardware",
                keyboard(),
                vec![other_serial],
                MatchConfidence::None,
            ),
            (
                "manufacturer, model and name match without a serial",
                no_serial.clone(),
                vec![no_serial],
                MatchConfidence::Probable,
            ),
            (
                "a different name never matches",
                named.clone(),
                vec![DeviceFingerprint::from_name("Other Thing")],
                MatchConfidence::None,
            ),
            (
                "a device known only by name matches itself",
                named.clone(),
                vec![named.clone()],
                MatchConfidence::Probable,
            ),
            (
                "two identical devices known only by name tie",
                named.clone(),
                vec![named.clone(), named],
                MatchConfidence::Ambiguous,
            ),
        ];
        for (case, stored, attached, want) in cases {
            assert_eq!(
                stored.best_match(&attached).1,
                want,
                "{case}: the match must be as strong as the evidence and no stronger"
            );
        }
    }
}
