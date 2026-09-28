//! What this computer can actually do right now.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A thing Midi Harbor may or may not be able to do on the current machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityName {
    /// Creating virtual MIDI ports other applications can see.
    VirtualPorts,
    /// Enumerating and opening attached MIDI hardware.
    PhysicalDevices,
    /// Establishing RTP-MIDI sessions with peers.
    NetworkSessions,
    /// Advertising and discovering peers over mDNS.
    MdnsResponder,
    /// Connecting out to Bluetooth LE MIDI devices.
    BluetoothCentral,
    /// Advertising this computer as a Bluetooth LE MIDI device.
    BluetoothPeripheral,
    /// Installing a service that starts at login.
    ServiceManager,
}

impl CapabilityName {
    /// Returns the stable identifier clients match on, the same as the serialised form.
    ///
    /// The display text is for people and may be reworded. A client choosing what to show from
    /// a capability needs a name that will not change under it.
    pub fn id(&self) -> &'static str {
        match self {
            Self::VirtualPorts => "virtual_ports",
            Self::PhysicalDevices => "physical_devices",
            Self::NetworkSessions => "network_sessions",
            Self::MdnsResponder => "mdns_responder",
            Self::BluetoothCentral => "bluetooth_central",
            Self::BluetoothPeripheral => "bluetooth_peripheral",
            Self::ServiceManager => "service_manager",
        }
    }

    /// Returns every capability, so a platform backend cannot forget to answer for one.
    pub fn all() -> [Self; 7] {
        [
            Self::VirtualPorts,
            Self::PhysicalDevices,
            Self::NetworkSessions,
            Self::MdnsResponder,
            Self::BluetoothCentral,
            Self::BluetoothPeripheral,
            Self::ServiceManager,
        ]
    }
}

impl fmt::Display for CapabilityName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::VirtualPorts => "virtual ports",
            Self::PhysicalDevices => "physical devices",
            Self::NetworkSessions => "network sessions",
            Self::MdnsResponder => "network discovery",
            Self::BluetoothCentral => "bluetooth connections",
            Self::BluetoothPeripheral => "bluetooth advertising",
            Self::ServiceManager => "service installation",
        };
        f.write_str(text)
    }
}

/// Why a capability is not available.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// The hardware is not present.
    NoAdapter,
    /// The hardware is present but switched off.
    AdapterOff,
    /// The operating system has not granted permission.
    PermissionDenied {
        /// What the user needs to grant.
        what: String,
    },
    /// A required system service is missing.
    MissingSystemComponent {
        /// Names the component, so the user knows what to install or start.
        component: String,
    },
    /// This build does not include the capability.
    NotBuilt,
    /// The platform genuinely cannot do this.
    UnsupportedPlatform,
}

impl fmt::Display for UnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAdapter => write!(f, "no adapter is present"),
            Self::AdapterOff => write!(f, "the adapter is switched off"),
            Self::PermissionDenied { what } => write!(f, "{what} permission was not granted"),
            Self::MissingSystemComponent { component } => {
                write!(f, "{component} is not available on this system")
            }
            Self::NotBuilt => write!(f, "this build does not include it"),
            Self::UnsupportedPlatform => write!(f, "this platform does not support it"),
        }
    }
}

/// Whether one capability is available, and why not when it is not.
///
/// Queried at runtime so the interface can present something unavailable as unavailable rather
/// than letting the user try it and meet a failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    /// Which capability this describes.
    pub name: CapabilityName,
    /// Whether it can be used right now.
    pub available: bool,
    /// Why not, when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<UnavailableReason>,
}

impl Capability {
    /// Declares a capability as available.
    pub fn available(name: CapabilityName) -> Self {
        Self {
            name,
            available: true,
            reason: None,
        }
    }

    /// Declares a capability as unavailable, with the reason the user needs.
    pub fn unavailable(name: CapabilityName, reason: UnavailableReason) -> Self {
        Self {
            name,
            available: false,
            reason: Some(reason),
        }
    }
}

/// Every capability's status on this machine.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(Vec<Capability>);

impl CapabilitySet {
    /// Builds a set from a list of capabilities.
    pub fn new(capabilities: Vec<Capability>) -> Self {
        Self(capabilities)
    }

    /// Looks up one capability.
    pub fn get(&self, name: CapabilityName) -> Option<&Capability> {
        self.0.iter().find(|c| c.name == name)
    }

    /// Reports whether a capability is available, treating an unknown one as unavailable.
    pub fn is_available(&self, name: CapabilityName) -> bool {
        self.get(name).is_some_and(|c| c.available)
    }

    /// Returns every capability.
    pub fn all(&self) -> &[Capability] {
        &self.0
    }

    /// Returns the capabilities this platform has not answered for.
    ///
    /// Principle IV forbids a silent gap, so an unanswered capability is a bug in a backend, not
    /// an implicit "no".
    pub fn unanswered(&self) -> Vec<CapabilityName> {
        CapabilityName::all()
            .into_iter()
            .filter(|name| self.get(*name).is_none())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each capability's identifier is exactly the name serde writes for it.
    ///
    /// The identifier travels to clients over the gRPC contract and the serialised name travels
    /// in JSON output, so a client matching either must see the same spelling. Two hand-kept
    /// spellings of one name drift apart otherwise.
    #[test]
    fn each_identifier_is_the_serialised_name() {
        for name in CapabilityName::all() {
            let serialised = serde_json::to_string(&name).expect("a capability name serialises");
            assert_eq!(
                serialised,
                format!("\"{}\"", name.id()),
                "{name:?}: the contract identifier must match the serialised name"
            );
        }
    }
}
