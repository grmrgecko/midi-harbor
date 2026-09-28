//! What a USB sound card says about itself, read from sysfs.
//!
//! ALSA numbers its clients and ports in the order things appear, so neither survives a device
//! moving or the machine restarting with other hardware attached. The USB device beneath the sound
//! card has what does: a serial number when the maker set one, the physical socket it sits in, and
//! its own manufacturer and product strings.
//!
//! Plain file reads under a root that tests replace, so the parsing is checked on any machine and
//! the real tree is only ever read on Linux.

use std::path::{Path, PathBuf};

/// How many directories above the sound card to look for its USB device.
///
/// The card hangs off a USB interface, whose parent is the device, so two is the usual answer.
/// A few more allows for hubs that insert a layer, without wandering up to the host controller.
const MAX_ANCESTORS: usize = 4;

/// Identity read from the USB device beneath one sound card.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UsbIdentity {
    /// The serial number, when the device reports one.
    pub serial: Option<String>,
    /// The manufacturer string from the device's descriptors.
    pub manufacturer: Option<String>,
    /// The product string from the device's descriptors.
    pub product: Option<String>,
    /// The socket the device is plugged into, such as `1-1.1`: bus, then the port at each hub.
    pub socket: Option<String>,
}

/// Reads the USB identity of sound card `card`, or nothing for a card that is not USB.
///
/// `sysfs` is `/sys` outside tests.
pub fn identity_of_card(sysfs: &Path, card: i32) -> Option<UsbIdentity> {
    // Resolve the card to its device, then find the USB device it hangs off.
    let link = sysfs
        .join("class/sound")
        .join(format!("card{card}"))
        .join("device");
    let mut dir: PathBuf = std::fs::canonicalize(link).ok()?;
    for _ in 0..MAX_ANCESTORS {
        if dir.join("idVendor").is_file() {
            return Some(UsbIdentity {
                serial: read(&dir, "serial"),
                manufacturer: read(&dir, "manufacturer"),
                product: read(&dir, "product"),
                socket: dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned),
            });
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// Reads one attribute, treating a missing or blank one as absent.
fn read(dir: &Path, attribute: &str) -> Option<String> {
    let value = std::fs::read_to_string(dir.join(attribute)).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// Builds a sysfs tree shaped as Linux lays one out for a USB sound card, synthetic: card 3's
    /// `device` link leads to the USB interface `1-1.1:1.0`, whose parent, the device in socket
    /// `1-1.1`, holds `idVendor` and the descriptor strings, each ending in a newline as sysfs
    /// writes them. Card 5 is the `snd-virmidi` platform device, which is not USB.
    fn tree(serial: Option<&str>) -> (tempfile_root::Root, PathBuf) {
        let root = tempfile_root::Root::new();
        let device = root.path().join("devices/pci0000:00/usb1/1-1/1-1.1");
        let interface = device.join("1-1.1:1.0");
        std::fs::create_dir_all(&interface).unwrap();
        std::fs::write(device.join("idVendor"), "0582\n").unwrap();
        std::fs::write(device.join("manufacturer"), "Roland\n").unwrap();
        std::fs::write(device.join("product"), "UM-ONE\n").unwrap();
        if let Some(serial) = serial {
            std::fs::write(device.join("serial"), format!("{serial}\n")).unwrap();
        }
        let card = root.path().join("class/sound/card3");
        std::fs::create_dir_all(&card).unwrap();
        symlink(&interface, card.join("device")).unwrap();

        let virmidi = root.path().join("devices/platform/snd_virmidi.0");
        std::fs::create_dir_all(&virmidi).unwrap();
        let card = root.path().join("class/sound/card5");
        std::fs::create_dir_all(&card).unwrap();
        symlink(&virmidi, card.join("device")).unwrap();

        let sysfs = root.path().to_path_buf();
        (root, sysfs)
    }

    /// Locks how a sound card's USB identity is read from sysfs: up from the card's device to the
    /// first directory holding `idVendor`, whose name is the socket and whose `serial`,
    /// `manufacturer` and `product` files are trimmed of sysfs's newline. A device with no serial
    /// still has its socket; a card that is not USB, or not there, has no USB identity.
    ///
    /// ALSA's client and port numbers do not survive a replug or a restart, so these are what
    /// identify the device.
    #[test]
    fn a_sound_cards_usb_identity_is_read_from_sysfs() {
        let described = |serial: Option<&str>| UsbIdentity {
            serial: serial.map(str::to_owned),
            manufacturer: Some("Roland".to_owned()),
            product: Some("UM-ONE".to_owned()),
            socket: Some("1-1.1".to_owned()),
        };
        let cases = [
            (
                "a USB card with a serial",
                Some("A1B2"),
                3,
                Some(described(Some("A1B2"))),
            ),
            ("a USB card with no serial", None, 3, Some(described(None))),
            ("a card that is not USB", None, 5, None),
            ("a card that is not there", None, 9, None),
        ];
        for (name, serial, card, want) in cases {
            let (_root, sysfs) = tree(serial);
            assert_eq!(
                identity_of_card(&sysfs, card),
                want,
                "{name}: the identity must be what the USB device beneath the card reports"
            );
        }
    }

    /// A temporary directory removed when dropped, so the tests leave nothing behind.
    mod tempfile_root {
        use std::path::{Path, PathBuf};

        pub struct Root(PathBuf);

        impl Root {
            pub fn new() -> Self {
                static NEXT: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!("mh-usb-{}-{n}", std::process::id()));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Root {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
