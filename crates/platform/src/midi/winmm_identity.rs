//! What WinMM reports about a MIDI endpoint, and what that means for identity.
//!
//! WinMM numbers endpoints in the order the system lists them, so an index says nothing after a
//! device is replugged or another appears. The device interface path each endpoint answers with
//! is what does: for USB hardware it names the vendor and product, and the instance, which is
//! stable across sockets for a device with a serial number and stable per socket for one without.
//!
//! Plain string handling, so it is tested on every platform even though only Windows produces
//! the input.

use crate::midi::DiscoveredDevice;
use midi_harbor_core::endpoint::Direction;
use midi_harbor_core::fingerprint::DeviceFingerprint;

/// One WinMM endpoint, input or output, as enumeration found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WinmmEndpoint {
    /// Its position in WinMM's list right now.
    pub index: u32,
    /// The name WinMM reports, which it cuts to 31 characters.
    pub name: String,
    /// The device interface path, empty for an endpoint with none, such as a software synthesiser.
    pub interface: String,
}

/// The name WinMM gives a teVirtualMIDI port that has closed but is still listed.
pub const UNAVAILABLE: &str = "(unavailable)";

/// Reports whether an endpoint belongs to hardware rather than to another application.
///
/// USB and Bluetooth devices, and MIDI on a sound card, are hardware. Ports that loopMIDI,
/// rtpMIDI and other teVirtualMIDI users create hang off a root-enumerated software device, and
/// the built-in synthesiser has no interface at all.
pub fn is_hardware(interface: &str) -> bool {
    let path = interface.to_ascii_lowercase();
    ["\\usb#", "\\bth", "\\hdaudio#", "\\pci#"]
        .iter()
        .any(|bus| path.contains(bus))
}

/// Reads the USB vendor and product identifiers out of an interface path, as four hex digits each.
pub fn usb_ids(interface: &str) -> Option<(String, String)> {
    let path = interface.to_ascii_lowercase();
    let hex_after = |marker: &str| -> Option<String> {
        let start = path.find(marker)?.checked_add(marker.len())?;
        let digits = path.get(start..start.checked_add(4)?)?;
        digits
            .chars()
            .all(|c| c.is_ascii_hexdigit())
            .then(|| digits.to_ascii_uppercase())
    };
    Some((hex_after("vid_")?, hex_after("pid_")?))
}

/// Builds the fingerprint an endpoint is remembered by.
///
/// For hardware the interface path is kept whole as the position, since it is the one value that
/// tells two identical devices apart. USB vendor and product fill the manufacturer and model, so
/// the same device moved to another socket still matches as probable by name and model together.
///
/// An application's port is known by its name alone. teVirtualMIDI numbers the halves of one
/// port separately, so its input and output can carry different paths, and it numbers them again
/// whenever ports come and go; the path said nothing lasting and split one port into two. The
/// driver refuses a name another port has, so the name is enough.
pub fn fingerprint(name: &str, interface: &str) -> DeviceFingerprint {
    if !is_hardware(interface) {
        return DeviceFingerprint::from_name(name);
    }
    let ids = usb_ids(interface);
    DeviceFingerprint {
        name: name.to_owned(),
        manufacturer: ids
            .as_ref()
            .map(|(vendor, _)| format!("USB vendor {vendor}")),
        model: ids.map(|(_, product)| format!("USB product {product}")),
        topology_path: Some(interface.to_owned()),
        ..DeviceFingerprint::default()
    }
}

/// Reports whether an endpoint is what a stored fingerprint describes, well enough to open it.
pub fn matches(stored: &DeviceFingerprint, endpoint: &WinmmEndpoint) -> bool {
    stored
        .compare(&fingerprint(&endpoint.name, &endpoint.interface))
        .is_automatic()
}

/// Combines WinMM's separate input and output lists into devices.
///
/// An input and an output with the same name and interface are two directions of one device. A
/// name `own` recognises is one of this process's virtual ports and is left out, because routing
/// to our own port through WinMM would loop.
pub fn pair(
    inputs: &[WinmmEndpoint],
    outputs: &[WinmmEndpoint],
    own: &dyn Fn(&str) -> bool,
) -> Vec<DiscoveredDevice> {
    let mut devices: Vec<DiscoveredDevice> = Vec::new();
    let mut add = |endpoint: &WinmmEndpoint, direction: Direction| {
        // A closed application port WinMM has not dropped yet is nothing to connect to.
        let closed = endpoint.name == UNAVAILABLE && !is_hardware(&endpoint.interface);
        if own(&endpoint.name) || closed {
            return;
        }
        let fingerprint = fingerprint(&endpoint.name, &endpoint.interface);
        if let Some(existing) = devices
            .iter_mut()
            .find(|device| device.fingerprint == fingerprint)
        {
            if existing.direction != direction {
                existing.direction = Direction::Bidirectional;
            }
            return;
        }
        devices.push(DiscoveredDevice {
            fingerprint,
            direction,
            claimed_by: None,
            software: !is_hardware(&endpoint.interface),
        });
    };
    for endpoint in inputs {
        add(endpoint, Direction::Input);
    }
    for endpoint in outputs {
        add(endpoint, Direction::Output);
    }
    devices
}

/// Returns the identifier Windows MIDI Services is given for a port this process creates.
///
/// The service keys the port's device on it, so the same name gives the same identifier every
/// time the port is created, and applications that remember the device find it again. It
/// refuses an identifier with anything but ASCII letters, digits, `-` and `_`, or longer than 32
/// characters, and a port name was refused for its spaces (research R-093), so the name is hashed
/// rather than used: `mh-` and sixteen hex digits of its 64-bit FNV-1a hash.
pub fn virtual_device_id(name: &str) -> String {
    // FNV-1a's 64-bit offset basis and prime.
    const OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01B3;
    let hash = name.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    });
    format!("mh-{hash:016x}")
}

/// The most UTF-16 units of a name WinMM reports: `MAXPNAMELEN`, 32, less its terminator.
pub const NAME_UNITS: usize = 32 - 1;

/// Cuts `name` to its first `units` UTF-16 units, as WinMM and the service cut port names.
pub fn cut(name: &str, units: usize) -> String {
    let wide: Vec<u16> = name.encode_utf16().take(units).collect();
    String::from_utf16_lossy(&wide)
}

/// Returns every name WinMM may list this process's virtual port under, given the names of its
/// connectors, each cut as WinMM cuts it.
///
/// Windows MIDI Services names a port after its connector's function block. The service Windows
/// shipped before its late-2026 update names one after its group instead (R-093): the device's
/// name for the first, and for the rest the name cut to 25 units, then ` Gr ` and the group's
/// number counted from one.
pub fn virtual_port_names(name: &str, connectors: &[String]) -> Vec<String> {
    // The group suffix is at most six units, " Gr 16", which the service leaves room for.
    const GROUP_SUFFIX_UNITS: usize = 6;
    let mut names: Vec<String> = connectors
        .iter()
        .map(|connector| cut(connector, NAME_UNITS))
        .collect();
    for group in 0..connectors.len() {
        names.push(if group == 0 {
            cut(name, NAME_UNITS)
        } else {
            format!(
                "{} Gr {}",
                cut(name, NAME_UNITS - GROUP_SUFFIX_UNITS),
                group + 1
            )
        });
    }
    names.sort();
    names.dedup();
    names
}

/// Returns how many bytes a WinMM short message with this status byte carries.
///
/// WinMM packs a whole message into one integer and never uses running status, so the status is
/// always present. A byte that is not a status, or one that begins a system-exclusive message,
/// has no short form and yields zero.
pub fn short_length(status: u8) -> usize {
    match status {
        0x80..=0xBF | 0xE0..=0xEF | 0xF2 => 3,
        0xC0..=0xDF | 0xF1 | 0xF3 => 2,
        0xF6 | 0xF8..=0xFF => 1,
        _ => 0,
    }
}

/// Unpacks a short message WinMM delivered as an integer, status in the low byte.
pub fn unpack(packed: usize) -> [u8; 3] {
    let bytes = packed.to_le_bytes();
    [
        bytes.first().copied().unwrap_or(0),
        bytes.get(1).copied().unwrap_or(0),
        bytes.get(2).copied().unwrap_or(0),
    ]
}

/// Packs up to three bytes of one message into the integer WinMM sends, status in the low byte.
pub fn pack(message: &[u8]) -> u32 {
    let mut bytes = [0u8; 4];
    for (slot, byte) in bytes.iter_mut().zip(message.iter().take(3)) {
        *slot = *byte;
    }
    u32::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A USB MIDI device's interface path, shaped as `DRV_QUERYDEVICEINTERFACE` reports one:
    /// synthetic, with Akai's vendor identifier.
    const USB: &str = r"\\?\usb#vid_09e8&pid_0069&mi_00#7&1f2a3b4c&0&0000#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\global";

    /// A loopMIDI port's interface path, shaped as teVirtualMIDI's root-enumerated software device
    /// reports one: synthetic.
    const LOOPMIDI: &str = r"\\?\root#media#0000#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\tevmidi1";

    /// Builds an endpoint as WinMM enumeration reports it.
    fn endpoint(name: &str, interface: &str) -> WinmmEndpoint {
        WinmmEndpoint {
            index: 0,
            name: name.to_owned(),
            interface: interface.to_owned(),
        }
    }

    /// Locks how a device interface path is read: the bus prefix Windows gives USB and Bluetooth
    /// LE devices marks hardware, a root-enumerated device or none marks software, and a USB path
    /// gives its `vid_` and `pid_` as four hex digits each, or nothing when they are malformed.
    #[test]
    fn an_interface_path_tells_hardware_and_its_usb_ids() {
        let cases = [
            ("a USB interface", USB, true, Some(("09E8", "0069"))),
            (
                "a Bluetooth LE device",
                r"\\?\bthledevice#{03b80e5a-ede8-4b33-a751-6ce34ec4c700}_dev_vid&02004c_pid&0001#8&1&0",
                true,
                None,
            ),
            ("a loopMIDI port", LOOPMIDI, false, None),
            ("the built-in synthesiser, with no path", "", false, None),
            (
                "a vendor that is not hex",
                r"\\?\usb#vid_09zz&pid_0069",
                true,
                None,
            ),
            (
                "a product cut short",
                r"\\?\usb#vid_09e8&pid_00",
                true,
                None,
            ),
        ];
        for (name, interface, want_hardware, want_ids) in cases {
            assert_eq!(
                is_hardware(interface),
                want_hardware,
                "{name}: hardware must be told from another application's port"
            );
            assert_eq!(
                usb_ids(interface),
                want_ids.map(|(vendor, product)| (vendor.to_owned(), product.to_owned())),
                "{name}: the USB vendor and product must be read exactly or not at all"
            );
        }
    }

    /// Locks that a USB device moved to another socket, which changes the instance part of its
    /// interface path, still matches what was stored of it by name and USB model, and that
    /// another device in that socket does not.
    #[test]
    fn a_device_in_another_socket_matches_by_name_and_model() {
        let stored = fingerprint("Pad Controller", USB);
        let moved = USB.replace("7&1f2a3b4c&0&0000", "7&55aa66bb&0&0000");
        let cases = [
            ("the same device", "Pad Controller", true),
            ("another device", "Other", false),
        ];
        for (name, endpoint_name, want) in cases {
            assert_eq!(
                matches(&stored, &endpoint(endpoint_name, &moved)),
                want,
                "{name} in another socket: only the same device may be opened in its place"
            );
        }
    }

    /// Locks how WinMM's separate input and output lists combine into devices.
    ///
    /// A device's halves share a name and path and become one device. teVirtualMIDI numbers the
    /// halves of one application port separately, so those pair by name alone. WinMM goes on
    /// listing a closed teVirtualMIDI port as "(unavailable)", which is hidden, but hardware is
    /// never hidden by its name. Two identical devices, which Windows lists as "Pad Controller" and
    /// "2- Pad Controller", stay two, and this process's own ports are left out, since routing to them
    /// through WinMM would loop.
    #[test]
    fn inputs_and_outputs_combine_into_devices() {
        let second = USB.replace("7&1f2a3b4c", "7&99887766");
        let slot3 = LOOPMIDI.replace("tevmidi1", "tevmidi3");
        let slot2 = LOOPMIDI.replace("tevmidi1", "tevmidi2");
        let cases = [
            (
                "a device's two halves",
                vec![endpoint("Pad Controller", USB)],
                vec![endpoint("Pad Controller", USB)],
                None,
                vec![("Pad Controller", Direction::Bidirectional, false)],
            ),
            (
                "an application port's halves in different slots",
                vec![endpoint("Harbor Bus", &slot3)],
                vec![endpoint("Harbor Bus", &slot2)],
                None,
                vec![("Harbor Bus", Direction::Bidirectional, true)],
            ),
            (
                "the built-in synthesiser",
                Vec::new(),
                vec![endpoint("Microsoft GS Wavetable Synth", "")],
                None,
                vec![("Microsoft GS Wavetable Synth", Direction::Output, true)],
            ),
            (
                "our own port",
                vec![endpoint("Harbor Bus", LOOPMIDI)],
                vec![endpoint("Harbor Bus", LOOPMIDI)],
                Some("Harbor Bus"),
                Vec::new(),
            ),
            (
                "a closed application port",
                vec![endpoint(UNAVAILABLE, LOOPMIDI)],
                Vec::new(),
                None,
                Vec::new(),
            ),
            (
                "hardware whose name reads unavailable",
                vec![endpoint(UNAVAILABLE, USB)],
                Vec::new(),
                None,
                vec![(UNAVAILABLE, Direction::Input, false)],
            ),
            (
                "two identical devices",
                vec![
                    endpoint("Pad Controller", USB),
                    endpoint("2- Pad Controller", &second),
                ],
                Vec::new(),
                None,
                vec![
                    ("Pad Controller", Direction::Input, false),
                    ("2- Pad Controller", Direction::Input, false),
                ],
            ),
        ];
        for (name, inputs, outputs, own, want) in cases {
            let devices = pair(&inputs, &outputs, &|port| Some(port) == own);
            let listed: Vec<(&str, Direction, bool)> = devices
                .iter()
                .map(|device| {
                    (
                        device.fingerprint.name.as_str(),
                        device.direction,
                        device.software,
                    )
                })
                .collect();
            assert_eq!(listed, want, "{name}: the devices listed are wrong");
        }
    }

    /// Locks the identifier Windows MIDI Services is given for a port, exactly: `mh-` and the
    /// 64-bit FNV-1a hash of the name in sixteen hex digits, within the service's rule of ASCII
    /// letters, digits, `-` and `_`, at most 32 characters (research R-093).
    ///
    /// The service keys the port's device on it, so a change to the hash would make every
    /// application lose every port once. FNV-1a of the empty string is its offset basis,
    /// `0xcbf29ce484222325`; the other values were computed independently of this code.
    #[test]
    fn a_virtual_device_id_is_the_names_fnv_1a_hash_within_the_services_rule() {
        let cases = [
            ("an empty name", "", "mh-cbf29ce484222325"),
            ("a name with a space", "Harbor Bus", "mh-62a8de8c35da0d45"),
            (
                "a name one character longer",
                "Harbor Bus 2",
                "mh-50ffafe91496cbf7",
            ),
            ("a name that is not ASCII", "Flügel", "mh-dfa1ed21522fb3ac"),
        ];
        for (name, port, want) in cases {
            let id = virtual_device_id(port);
            assert_eq!(
                id, want,
                "{name}: the identifier must not change between builds"
            );
            assert!(
                id.len() <= 32
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{name}: the service refuses an identifier outside its rule"
            );
        }
    }

    /// Locks every name WinMM may list a port under: each connector's name, and the group names
    /// of the service Windows shipped before its late-2026 update, the port's name for group 1
    /// and for the others the name cut to 25 UTF-16 units, " Gr " and the group number (research
    /// R-093). Every name is cut to WinMM's 31 units, `MAXPNAMELEN` less its terminator.
    ///
    /// The long name's 25-unit cut ends on a space, so its group name has two spaces before
    /// "Gr". A character outside the basic plane counts two units, as UTF-16 encodes it.
    #[test]
    fn a_virtual_port_is_known_by_its_connector_and_group_names_cut_as_winmm_cuts_them() {
        let long = "A very long virtual port name here";
        let astral = format!("\u{1F3B9}{}", "a".repeat(30));
        let cases = [
            (
                "two connectors",
                "Harbor Split",
                vec!["Harbor Split 1".to_owned(), "Harbor Split 2".to_owned()],
                vec![
                    "Harbor Split".to_owned(),
                    "Harbor Split 1".to_owned(),
                    "Harbor Split 2".to_owned(),
                    "Harbor Split Gr 2".to_owned(),
                ],
            ),
            (
                "one connector",
                "Harbor Bus",
                vec!["Harbor Bus".to_owned()],
                vec!["Harbor Bus".to_owned()],
            ),
            (
                "a name longer than WinMM keeps",
                long,
                vec![format!("{long} 1"), format!("{long} 2")],
                vec![
                    "A very long virtual port  Gr 2".to_owned(),
                    "A very long virtual port name h".to_owned(),
                ],
            ),
            (
                "a character outside the basic plane",
                astral.as_str(),
                vec![astral.clone()],
                vec![format!("\u{1F3B9}{}", "a".repeat(29))],
            ),
        ];
        for (name, port, connectors, want) in cases {
            assert_eq!(
                virtual_port_names(port, &connectors),
                want,
                "{name}: the port must be recognised under every name WinMM may list it by"
            );
        }
    }

    /// Locks the length of a WinMM short message by its status byte, per the MIDI 1.0
    /// specification's message table: three bytes for note, controller, pitch bend and song
    /// position; two for program change, channel pressure, time code and song select; one for
    /// tune request and real-time. System-exclusive and data bytes have no short form.
    #[test]
    fn a_short_message_has_the_length_its_status_gives() {
        let cases = [
            ("note on", 0x90, 3),
            ("control change", 0xB5, 3),
            ("pitch bend", 0xE1, 3),
            ("song position", 0xF2, 3),
            ("program change", 0xC3, 2),
            ("channel pressure", 0xD0, 2),
            ("time code quarter frame", 0xF1, 2),
            ("song select", 0xF3, 2),
            ("tune request", 0xF6, 1),
            ("clock", 0xF8, 1),
            ("reset", 0xFF, 1),
            ("system-exclusive start", 0xF0, 0),
            ("a data byte", 0x40, 0),
        ];
        for (name, status, want) in cases {
            assert_eq!(
                short_length(status),
                want,
                "{name}: WinMM must be given exactly the bytes the status calls for"
            );
        }
    }

    /// Locks the packing `midiOutShortMsg` takes and `MIM_DATA` delivers: status in the low byte,
    /// then the first and second data bytes, with unused bytes zero.
    #[test]
    fn a_short_message_packs_status_first_in_the_low_byte_and_unpacks() {
        let cases: [(&str, &[u8], u32, [u8; 3]); 3] = [
            (
                "a note on",
                &[0x90, 0x3C, 0x64],
                0x0064_3C90,
                [0x90, 0x3C, 0x64],
            ),
            (
                "a program change",
                &[0xC1, 0x05],
                0x0000_05C1,
                [0xC1, 0x05, 0x00],
            ),
            ("a clock", &[0xF8], 0x0000_00F8, [0xF8, 0x00, 0x00]),
        ];
        for (name, message, want_packed, want_unpacked) in cases {
            let packed = pack(message);
            assert_eq!(
                packed, want_packed,
                "{name}: WinMM reads the status from the low byte"
            );
            assert_eq!(
                unpack(packed as usize),
                want_unpacked,
                "{name}: what WinMM delivers must unpack to the message"
            );
        }
    }
}
