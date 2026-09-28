//! Attached MIDI hardware, and keeping routes bound to it across replugging.
//!
//! Devices are discovered rather than created. The problem this solves is identity: a device
//! unplugged and plugged back in — possibly into a different socket — must be recognised as the
//! same device, or every route referring to it breaks. Two identical devices attached at once
//! must stay distinct, or a route addresses the wrong hardware.

use midi_harbor_core::config::RouteConfig;
use midi_harbor_core::endpoint::{Endpoint, EndpointKind, EndpointName, PhysicalDevice};
use midi_harbor_core::fingerprint::{DeviceFingerprint, MatchConfidence};
use midi_harbor_platform::midi::DiscoveredDevice;

/// What matching attached hardware against a stored entry concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceMatch {
    /// The stored entry and this hardware are the same device.
    Same {
        /// How confident the match is.
        confidence: MatchConfidence,
    },
    /// Several attached devices matched equally well, so the user must choose.
    ///
    /// Never resolved automatically: binding a route to the wrong instrument is worse than
    /// asking which one was meant.
    Ambiguous {
        /// How many attached devices matched.
        candidates: usize,
    },
    /// Nothing attached matches this entry.
    Absent,
}

/// Matches stored device entries against what is currently attached.
///
/// Returns one decision per stored entry, in the order they were given.
pub fn reconcile(stored: &[DeviceFingerprint], attached: &[DiscoveredDevice]) -> Vec<DeviceMatch> {
    let fingerprints: Vec<DeviceFingerprint> = attached
        .iter()
        .map(|device| device.fingerprint.clone())
        .collect();

    stored
        .iter()
        .map(|entry| {
            let (found, confidence) = entry.best_match(&fingerprints);
            match (found, confidence) {
                (None, _) | (_, MatchConfidence::None) => DeviceMatch::Absent,
                (Some(_), MatchConfidence::Ambiguous) => DeviceMatch::Ambiguous {
                    candidates: fingerprints
                        .iter()
                        .filter(|candidate| entry.compare(candidate) != MatchConfidence::None)
                        .count(),
                },
                (Some(_), confidence) => DeviceMatch::Same { confidence },
            }
        })
        .collect()
}

/// Returns a name for newly seen hardware that no other endpoint already answers to.
///
/// Two identical devices report identical names, and routes name their endpoints, so leaving them
/// the same would let a route bind to whichever one happened to be looked up first — silently, and
/// not necessarily the same one after a restart. Both get told apart rather than one keeping the
/// plain name, because there is nothing to choose between them.
fn unique_name(device: &DiscoveredDevice, attached: &[DiscoveredDevice], taken: &[&str]) -> String {
    let plain = device.fingerprint.name.clone();

    let twins = attached
        .iter()
        .filter(|other| other.fingerprint.name == plain)
        .count();
    if twins <= 1 && !taken.contains(&plain.as_str()) {
        return plain;
    }

    // Where it is plugged in is all that distinguishes hardware that is identical in every other
    // way, which is why moving the cable renames it. Nothing else can tell them apart.
    let Some(detail) = discriminator(&device.fingerprint) else {
        // Nothing to tell it apart by, but two endpoints answering to one name is still worse
        // than a number: a route would bind to whichever was looked up first.
        for attempt in 2..=u16::MAX {
            let numbered = format!("{plain} ({attempt})");
            if !taken.contains(&numbered.as_str()) {
                return numbered;
            }
        }
        return plain;
    };
    let named = format!("{plain} ({detail})");
    if !taken.contains(&named.as_str()) {
        return named;
    }

    // Two devices reporting the same position is not something a name can fix, but two endpoints
    // answering to one name is worse: a route would bind to whichever was looked up first.
    for attempt in 2..=u16::MAX {
        let numbered = format!("{plain} ({detail}, {attempt})");
        if !taken.contains(&numbered.as_str()) {
            return numbered;
        }
    }
    named
}

/// Returns the most stable thing that tells this hardware apart from another of its kind.
fn discriminator(fingerprint: &DeviceFingerprint) -> Option<String> {
    if let Some(serial) = &fingerprint.usb_serial {
        // The port suffix makes the identity per port, but twins differ in the serial itself.
        let device = serial.split('#').next().unwrap_or(serial);
        return Some(format!("serial {device}"));
    }
    if let Some(path) = &fingerprint.topology_path {
        // The whole position, less the scheme that is the same for everything on the bus. The
        // last component alone is not enough: two ALSA clients both have a port zero.
        if let Some(socket) = path.strip_prefix("usb-") {
            return Some(format!("socket {socket}"));
        }
        let short = path.strip_prefix("alsa:").unwrap_or(path.as_str());
        return Some(format!("port {short}"));
    }
    fingerprint.unique_id.map(|id| format!("id {id:x}"))
}

/// Builds the endpoint list for attached hardware, carrying over what was stored.
///
/// Hardware that is attached but was never seen before appears as a new endpoint; hardware that
/// is stored but absent is kept, so its routes survive being unplugged.
pub fn endpoints_for(stored: &[Endpoint], attached: &[DiscoveredDevice]) -> Vec<Endpoint> {
    let stored_devices: Vec<(&Endpoint, &DeviceFingerprint)> = stored
        .iter()
        .filter_map(|endpoint| match &endpoint.kind {
            EndpointKind::PhysicalDevice(device) => Some((endpoint, &device.fingerprint)),
            _ => None,
        })
        .collect();

    let fingerprints: Vec<DeviceFingerprint> = stored_devices
        .iter()
        .map(|(_, fingerprint)| (*fingerprint).clone())
        .collect();
    let decisions = reconcile(&fingerprints, attached);

    // Carry each stored entry forward, marking whether its hardware is here.
    let mut endpoints: Vec<Endpoint> = Vec::new();
    let mut claimed: Vec<usize> = Vec::new();

    for (index, (endpoint, fingerprint)) in stored_devices.iter().enumerate() {
        let decision = decisions.get(index).cloned().unwrap_or(DeviceMatch::Absent);
        // The best match, not the first that is merely possible. With two identical devices
        // every candidate compares as something, so taking the first bound each stored entry to
        // whichever device came earliest in the list and left the rest to be added again on every
        // pass. Already-claimed hardware is skipped, because two entries binding to one device
        // would both report it as theirs.
        let matched = matches!(decision, DeviceMatch::Same { .. })
            .then(|| {
                attached
                    .iter()
                    .enumerate()
                    .filter(|(position, _)| !claimed.contains(position))
                    .map(|(position, device)| (position, fingerprint.compare(&device.fingerprint)))
                    .filter(|(_, score)| *score != MatchConfidence::None)
                    .max_by_key(|(_, score)| *score)
                    .map(|(position, _)| position)
            })
            .flatten();
        if let Some(position) = matched {
            claimed.push(position);
        }

        let (present, confidence, claimed_by) = match (&decision, matched) {
            (DeviceMatch::Same { confidence }, Some(position)) => (
                true,
                *confidence,
                attached
                    .get(position)
                    .and_then(|device| device.claimed_by.clone()),
            ),
            (DeviceMatch::Ambiguous { .. }, _) => (false, MatchConfidence::Ambiguous, None),
            _ => (false, MatchConfidence::None, None),
        };

        // Whether it is software is what the platform says of it now, or what was stored while
        // it is away.
        let stored_software =
            matches!(&endpoint.kind, EndpointKind::PhysicalDevice(device) if device.software);
        let software = matched
            .and_then(|position| attached.get(position))
            .map_or(stored_software, |device| device.software);

        // A maker or model the stored entry lacks is taken from the hardware now matched to it,
        // so a device remembered before either was read is described like one seen today. What
        // is stored is never overwritten.
        let mut described = (*fingerprint).clone();
        if let Some(device) = matched.and_then(|position| attached.get(position)) {
            if described.manufacturer.is_none() {
                described.manufacturer = device.fingerprint.manufacturer.clone();
            }
            if described.model.is_none() {
                described.model = device.fingerprint.model.clone();
            }
        }

        let mut carried = (*endpoint).clone();
        carried.kind = EndpointKind::PhysicalDevice(PhysicalDevice {
            fingerprint: described,
            present,
            confidence,
            claimed_by,
            software,
        });
        endpoints.push(carried);
    }

    // A software port that returns under a new identifier, as an IAC bus does after being
    // edited in Audio MIDI Setup, matches nothing by identifier. It is taken back by name, but
    // only when that settles it: one absent software entry of the name and one unclaimed
    // software port of it. Otherwise it appeared again beside the old entry the routes name.
    let absent_software = |endpoint: &Endpoint| match &endpoint.kind {
        EndpointKind::PhysicalDevice(device) if device.software && !device.present => {
            Some(device.fingerprint.name.clone())
        }
        _ => None,
    };
    let returning: Vec<(usize, usize)> = endpoints
        .iter()
        .enumerate()
        .filter_map(|(index, endpoint)| {
            let name = absent_software(endpoint)?;
            let alone = endpoints
                .iter()
                .filter(|other| absent_software(other).as_deref() == Some(name.as_str()))
                .count()
                == 1;
            let mut candidates = attached.iter().enumerate().filter(|(position, device)| {
                device.software && device.fingerprint.name == name && !claimed.contains(position)
            });
            let (position, _) = candidates.next()?;
            (alone && candidates.next().is_none()).then_some((index, position))
        })
        .collect();
    for (index, position) in returning {
        let (Some(endpoint), Some(device)) = (endpoints.get_mut(index), attached.get(position))
        else {
            continue;
        };
        if let EndpointKind::PhysicalDevice(held) = &mut endpoint.kind {
            held.fingerprint.unique_id = device.fingerprint.unique_id;
            if held.fingerprint.manufacturer.is_none() {
                held.fingerprint.manufacturer = device.fingerprint.manufacturer.clone();
            }
            if held.fingerprint.model.is_none() {
                held.fingerprint.model = device.fingerprint.model.clone();
            }
            held.present = true;
            held.confidence = MatchConfidence::Probable;
            held.claimed_by = device.claimed_by.clone();
        }
        claimed.push(position);
    }

    // Anything attached that no stored entry claimed is hardware we have not seen before.
    for (position, device) in attached.iter().enumerate() {
        if claimed.contains(&position) {
            continue;
        }
        let taken: Vec<&str> = endpoints
            .iter()
            .map(|endpoint| endpoint.name.as_str())
            .collect();
        let Ok(name) = EndpointName::new(unique_name(device, attached, &taken)) else {
            continue;
        };
        endpoints.push(Endpoint {
            direction: device.direction,
            ..Endpoint::new(
                name,
                EndpointKind::PhysicalDevice(PhysicalDevice {
                    fingerprint: device.fingerprint.clone(),
                    present: true,
                    confidence: MatchConfidence::Exact,
                    claimed_by: device.claimed_by.clone(),
                    software: device.software,
                }),
            )
        });
    }

    endpoints
}

/// Separates the entries worth keeping from applications' ports that have gone unused.
///
/// Hardware is kept while unplugged, so its routes resume when it returns. An application's port
/// is kept while present, and after it goes only if a route names it: a synth someone routes to
/// comes back with its routes, and a tool that opened a port once leaves nothing behind.
pub fn forget_unused_software(
    endpoints: Vec<Endpoint>,
    routes: &[RouteConfig],
) -> (Vec<Endpoint>, Vec<Endpoint>) {
    endpoints.into_iter().partition(|endpoint| {
        let EndpointKind::PhysicalDevice(device) = &endpoint.kind else {
            return true;
        };
        !device.software || device.present || routes.iter().any(|route| route.touches(endpoint))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi_harbor_core::endpoint::Direction;

    /// Builds attached hardware or an application's port as the platform reports it.
    fn attached(fingerprint: DeviceFingerprint, software: bool) -> DiscoveredDevice {
        DiscoveredDevice {
            fingerprint,
            direction: Direction::Bidirectional,
            claimed_by: None,
            software,
        }
    }

    /// Builds a stored entry for hardware that is not attached, as the configuration holds it.
    fn stored(name: &str, fingerprint: DeviceFingerprint, software: bool) -> Endpoint {
        Endpoint::new(
            EndpointName::new(name).expect("the stored name is a valid endpoint name"),
            EndpointKind::PhysicalDevice(PhysicalDevice {
                fingerprint,
                present: false,
                confidence: MatchConfidence::None,
                claimed_by: None,
                software,
            }),
        )
    }

    /// Returns a K61 keyboard's fingerprint as USB reports it, with an optional serial number.
    fn keyboard(serial: Option<&str>) -> DeviceFingerprint {
        DeviceFingerprint {
            usb_serial: serial.map(str::to_owned),
            manufacturer: Some("Acme".to_owned()),
            model: Some("K61".to_owned()),
            name: "Acme K61".to_owned(),
            ..DeviceFingerprint::default()
        }
    }

    /// Returns a K61 keyboard's fingerprint as ALSA reports it at a client and port, which is all
    /// that tells two of them apart.
    fn keyboard_at(position: &str) -> DeviceFingerprint {
        DeviceFingerprint {
            topology_path: Some(format!("alsa:{position}")),
            ..keyboard(None)
        }
    }

    /// Returns a CoreMIDI port's fingerprint, which carries a name and a unique identifier.
    fn port(name: &str, unique_id: u32) -> DeviceFingerprint {
        DeviceFingerprint {
            unique_id: Some(unique_id),
            name: name.to_owned(),
            ..DeviceFingerprint::default()
        }
    }

    /// Returns what an endpoint carries as a physical device.
    fn device(endpoint: &Endpoint) -> &PhysicalDevice {
        match &endpoint.kind {
            EndpointKind::PhysicalDevice(device) => device,
            other => panic!("expected a physical device, got {other:?}"),
        }
    }

    /// Returns each endpoint's name and whether its hardware is attached, in list order.
    fn listed(endpoints: &[Endpoint]) -> Vec<(String, bool)> {
        endpoints
            .iter()
            .map(|endpoint| (endpoint.name.to_string(), device(endpoint).present))
            .collect()
    }

    /// Proves which attached ports a stored entry takes back when its identifier no longer
    /// matches. Editing an IAC bus in Audio MIDI Setup gives it a new CoreMIDI identifier, so a
    /// software port is taken back by name, but only when that settles it: one absent software
    /// entry of the name and one unclaimed software port of it. Hardware with a different
    /// identifier or serial is different hardware of the same model, and guessing between two
    /// candidates would bind a route to whichever was listed first.
    #[test]
    fn a_port_is_taken_back_by_name_only_when_that_settles_it() {
        struct Case {
            name: &'static str,
            stored: Vec<Endpoint>,
            attached: Vec<DiscoveredDevice>,
            want: Vec<(&'static str, bool)>,
            want_first_id: Option<u32>,
        }
        let cases = [
            Case {
                name: "a software port back under a new identifier is the same port",
                stored: vec![stored("Bus", port("Bus", 1), true)],
                attached: vec![attached(port("Bus", 2), true)],
                want: vec![("Bus", true)],
                want_first_id: Some(2),
            },
            Case {
                name: "hardware under a new identifier is different hardware",
                stored: vec![stored("Keystation", port("Keystation", 1), false)],
                attached: vec![attached(port("Keystation", 2), false)],
                want: vec![("Keystation", false), ("Keystation (id 2)", true)],
                want_first_id: Some(1),
            },
            Case {
                name: "hardware with another serial is different hardware",
                stored: vec![stored("Acme K61", keyboard(Some("SN-001")), false)],
                attached: vec![attached(keyboard(Some("SN-999")), false)],
                want: vec![("Acme K61", false), ("Acme K61 (serial SN-999)", true)],
                want_first_id: None,
            },
            Case {
                name: "two ports of the name returning are not guessed between",
                stored: vec![stored("Bus", port("Bus", 1), true)],
                attached: vec![
                    attached(port("Bus", 2), true),
                    attached(port("Bus", 3), true),
                ],
                want: vec![("Bus", false), ("Bus (id 2)", true), ("Bus (id 3)", true)],
                want_first_id: Some(1),
            },
            Case {
                name: "two remembered entries of the name are not guessed between",
                stored: vec![
                    stored("Bus", port("Bus", 1), true),
                    stored("Bus 2", port("Bus", 4), true),
                ],
                attached: vec![attached(port("Bus", 2), true)],
                want: vec![("Bus", false), ("Bus 2", false), ("Bus (id 2)", true)],
                want_first_id: Some(1),
            },
            Case {
                name: "a port matched by its identifier is not handed to another entry",
                stored: vec![
                    stored("Bus", port("Bus", 1), true),
                    stored("Bus 2", port("Bus", 5), true),
                ],
                attached: vec![attached(port("Bus", 1), true)],
                want: vec![("Bus", true), ("Bus 2", false)],
                want_first_id: Some(1),
            },
            Case {
                name: "a port still here keeps its identifier when another of its name appears",
                stored: vec![stored("Bus", port("Bus", 1), true)],
                attached: vec![
                    attached(port("Bus", 1), true),
                    attached(port("Bus", 2), true),
                ],
                want: vec![("Bus", true), ("Bus (id 2)", true)],
                want_first_id: Some(1),
            },
            Case {
                name: "a hardware port does not take a software entry's place",
                stored: vec![stored("Bus", port("Bus", 1), true)],
                attached: vec![attached(port("Bus", 2), false)],
                want: vec![("Bus", false), ("Bus (id 2)", true)],
                want_first_id: Some(1),
            },
            Case {
                name: "a software port does not take a hardware entry's place",
                stored: vec![stored("Keystation", port("Keystation", 1), false)],
                attached: vec![attached(port("Keystation", 2), true)],
                want: vec![("Keystation", false), ("Keystation (id 2)", true)],
                want_first_id: Some(1),
            },
        ];

        for case in cases {
            let endpoints = endpoints_for(&case.stored, &case.attached);
            let want: Vec<(String, bool)> = case
                .want
                .iter()
                .map(|(name, present)| ((*name).to_owned(), *present))
                .collect();
            assert_eq!(
                listed(&endpoints),
                want,
                "{}: the entries and which are attached are wrong",
                case.name
            );
            assert_eq!(
                endpoints
                    .first()
                    .and_then(|endpoint| device(endpoint).fingerprint.unique_id),
                case.want_first_id,
                "{}: the first stored entry must carry the identifier the next pass finds it by",
                case.name
            );
        }
    }

    /// Proves that two identical devices each keep their own stored entry when where they are
    /// plugged in tells them apart, and that neither entry is bound when nothing does (FR-015g).
    /// Taking the first possible match bound both entries to one device and added the other again
    /// on every pass, a configuration that grew by an entry every refresh; and binding a route to
    /// the wrong instrument is worse than asking which was meant.
    #[test]
    fn identical_devices_bind_to_their_own_entry_or_to_none() {
        struct Case {
            name: &'static str,
            stored: Vec<Endpoint>,
            want: Vec<(&'static str, bool)>,
            want_confidence: MatchConfidence,
        }
        let cases = [
            Case {
                name: "each entry remembered at a position binds to the twin there",
                stored: vec![
                    stored("Acme K61", keyboard_at("128:0"), false),
                    stored("Acme K61 (port 128:1)", keyboard_at("128:1"), false),
                ],
                want: vec![("Acme K61", true), ("Acme K61 (port 128:1)", true)],
                want_confidence: MatchConfidence::Probable,
            },
            Case {
                name: "an entry with nothing to tell the twins apart is left unbound",
                stored: vec![stored("Acme K61", keyboard(None), false)],
                want: vec![
                    ("Acme K61", false),
                    ("Acme K61 (port 128:0)", true),
                    ("Acme K61 (port 128:1)", true),
                ],
                want_confidence: MatchConfidence::Ambiguous,
            },
        ];
        let twins = [
            attached(keyboard_at("128:0"), false),
            attached(keyboard_at("128:1"), false),
        ];

        for case in cases {
            let endpoints = endpoints_for(&case.stored, &twins);
            let want: Vec<(String, bool)> = case
                .want
                .iter()
                .map(|(name, present)| ((*name).to_owned(), *present))
                .collect();
            assert_eq!(
                listed(&endpoints),
                want,
                "{}: the entries and which are attached are wrong",
                case.name
            );
            assert_eq!(
                endpoints
                    .first()
                    .map(|endpoint| device(endpoint).confidence),
                Some(case.want_confidence),
                "{}: the first entry's match confidence is wrong",
                case.name
            );
        }
    }

    /// Proves the names given to newly seen hardware. Routes name their endpoints, so two
    /// endpoints answering to one name let a route bind to whichever is looked up first, silently
    /// and not always the same one (FR-015g). Twins are told apart by position, then by a number
    /// when even the position is shared; a device on its own keeps its plain name, because the
    /// suffix is only worth its ugliness when something has to be told apart.
    #[test]
    fn newly_seen_hardware_is_named_so_no_two_endpoints_share_a_name() {
        struct Case {
            name: &'static str,
            attached: Vec<DiscoveredDevice>,
            want: Vec<&'static str>,
        }
        let cases = [
            Case {
                name: "a device on its own keeps its plain name",
                attached: vec![attached(keyboard_at("128:0"), false)],
                want: vec!["Acme K61"],
            },
            Case {
                name: "twins are told apart by where they are plugged in",
                attached: vec![
                    attached(keyboard_at("128:0"), false),
                    attached(keyboard_at("128:1"), false),
                ],
                want: vec!["Acme K61 (port 128:0)", "Acme K61 (port 128:1)"],
            },
            Case {
                name: "twins reporting one position are numbered",
                attached: vec![
                    attached(keyboard_at("129:0"), false),
                    attached(keyboard_at("129:0"), false),
                    attached(keyboard_at("129:0"), false),
                ],
                want: vec![
                    "Acme K61 (port 129:0)",
                    "Acme K61 (port 129:0, 2)",
                    "Acme K61 (port 129:0, 3)",
                ],
            },
        ];

        for case in cases {
            let names: Vec<String> = endpoints_for(&[], &case.attached)
                .iter()
                .map(|endpoint| endpoint.name.to_string())
                .collect();
            assert_eq!(names, case.want, "{}: the names given are wrong", case.name);
        }
    }

    /// Proves that a maker or model a stored entry lacks is taken from the hardware matched to
    /// it, that what is stored is never overwritten, and that hardware matched to nothing lends
    /// nothing. Devices remembered on macOS before maker and model were read kept none for good.
    #[test]
    fn a_stored_entry_takes_only_the_description_it_lacks_from_its_own_hardware() {
        struct Case {
            name: &'static str,
            stored: (Option<&'static str>, Option<&'static str>),
            serial: &'static str,
            want: (Option<&'static str>, Option<&'static str>),
        }
        let cases = [
            Case {
                name: "an entry remembered without a maker takes the one reported now",
                stored: (None, None),
                serial: "SN-001",
                want: (Some("Acme"), Some("K61")),
            },
            Case {
                name: "a stored maker and model are not overwritten",
                stored: (Some("Acme Corp"), Some("K61 MkII")),
                serial: "SN-001",
                want: (Some("Acme Corp"), Some("K61 MkII")),
            },
            Case {
                name: "different hardware of the same name lends nothing",
                stored: (None, None),
                serial: "SN-002",
                want: (None, None),
            },
        ];

        for case in cases {
            let remembered = DeviceFingerprint {
                manufacturer: case.stored.0.map(str::to_owned),
                model: case.stored.1.map(str::to_owned),
                ..keyboard(Some("SN-001"))
            };
            let endpoints = endpoints_for(
                &[stored("Acme K61", remembered, false)],
                &[attached(keyboard(Some(case.serial)), false)],
            );
            let described =
                &device(endpoints.first().expect("the stored entry is listed first")).fingerprint;
            assert_eq!(
                (
                    described.manufacturer.as_deref(),
                    described.model.as_deref()
                ),
                case.want,
                "{}: the stored entry's maker and model are wrong",
                case.name
            );
        }
    }
}
