//! Endpoints: everything MIDI can flow to or from.

use crate::fingerprint::{DeviceFingerprint, MatchConfidence};
use crate::ids::{EndpointId, PeerId};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Shortest allowed endpoint name.
const NAME_MIN: usize = 1;
/// Longest allowed endpoint name.
const NAME_MAX: usize = 128;

/// Why a proposed endpoint name was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// The name was empty, or only whitespace.
    #[error("endpoint name cannot be empty")]
    Empty,
    /// The name exceeded the length limit.
    #[error("endpoint name cannot exceed {NAME_MAX} characters, got {0}")]
    TooLong(usize),
    /// The name contained a control character.
    #[error("endpoint name cannot contain control characters")]
    ControlCharacter,
}

/// A validated endpoint name.
///
/// Names are trimmed on the way in, so trailing whitespace cannot make two endpoints look
/// distinct while displaying identically.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EndpointName(String);

impl EndpointName {
    /// Validates and normalises a proposed name.
    pub fn new(raw: impl AsRef<str>) -> Result<Self, NameError> {
        let trimmed = raw.as_ref().trim();
        if trimmed.chars().count() < NAME_MIN {
            return Err(NameError::Empty);
        }
        let length = trimmed.chars().count();
        if length > NAME_MAX {
            return Err(NameError::TooLong(length));
        }
        if trimmed.chars().any(char::is_control) {
            return Err(NameError::ControlCharacter);
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Returns the name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EndpointName {
    type Error = NameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<EndpointName> for String {
    fn from(value: EndpointName) -> Self {
        value.0
    }
}

impl fmt::Display for EndpointName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which way MIDI can travel through an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// MIDI arrives from it, so it can only be a route source.
    Input,
    /// MIDI leaves through it, so it can only be a route destination.
    Output,
    /// Both.
    Bidirectional,
}

impl Direction {
    /// Reports whether this endpoint can be a route source.
    pub fn can_source(&self) -> bool {
        matches!(self, Self::Input | Self::Bidirectional)
    }

    /// Reports whether this endpoint can be a route destination.
    pub fn can_sink(&self) -> bool {
        matches!(self, Self::Output | Self::Bidirectional)
    }
}

/// The most connectors of one kind a virtual port may have, as the IAC Driver allows.
pub const MAX_CONNECTORS: u8 = 16;

/// A virtual port that exists only so local applications can exchange MIDI.
///
/// It has MIDI In connectors, which other applications send to, and MIDI Out connectors, which
/// they receive from, at least one of each, as the IAC Driver's buses do (FR-002, FR-002a). A
/// count above one shows to other applications as numbered ports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualPort {
    /// How many MIDI In connectors it has: what other applications send to, and so what a route
    /// can start from.
    #[serde(default = "one_connector")]
    pub inputs: u8,
    /// How many MIDI Out connectors it has: what other applications receive from, and so what a
    /// route can end at.
    #[serde(default = "one_connector")]
    pub outputs: u8,
    /// The identifiers pinned on each MIDI In connector's platform endpoint, in order, so other
    /// applications keep recognising them across restarts. Assigned by the system on first
    /// creation, then reused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_ids: Vec<u32>,
    /// The identifiers pinned on each MIDI Out connector's platform endpoint, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_ids: Vec<u32>,
    /// The single identifier a port had before it had connectors, which was its MIDI Out's.
    /// Read, moved into `output_ids`, and never written again.
    #[serde(default, skip_serializing)]
    pub platform_unique_id: Option<u32>,
}

impl Default for VirtualPort {
    fn default() -> Self {
        Self {
            inputs: 1,
            outputs: 1,
            input_ids: Vec::new(),
            output_ids: Vec::new(),
            platform_unique_id: None,
        }
    }
}

impl VirtualPort {
    /// Creates a port with the given connector counts, each kept between one and sixteen.
    pub fn with_connectors(inputs: u8, outputs: u8) -> Self {
        Self {
            inputs: inputs.clamp(1, MAX_CONNECTORS),
            outputs: outputs.clamp(1, MAX_CONNECTORS),
            ..Self::default()
        }
    }

    /// Returns the port as a file left it, made whole: counts kept between one and sixteen, and
    /// a single identifier from before connectors moved to the MIDI Out it belonged to.
    ///
    /// Clamped rather than rejected, because a count out of range in a hand-edited file would
    /// otherwise make the whole file unreadable, and an unreadable file is set aside.
    fn settled(mut self) -> Self {
        self.inputs = self.inputs.clamp(1, MAX_CONNECTORS);
        self.outputs = self.outputs.clamp(1, MAX_CONNECTORS);
        if let Some(id) = self.platform_unique_id.take()
            && self.output_ids.is_empty()
        {
            self.output_ids.push(id);
        }
        self
    }
}

/// Returns one, the connector count a virtual port has when its file does not say.
fn one_connector() -> u8 {
    1
}

/// Returns the name other applications see for one connector of a port.
///
/// A port with one connector of a kind shows under its own name; with several, each is numbered
/// from one, as "Keys 1" and "Keys 2".
pub fn connector_name(name: &str, count: u8, index: u8) -> String {
    if count > 1 {
        format!("{name} {}", u16::from(index) + 1)
    } else {
        name.to_owned()
    }
}

/// MIDI hardware attached to this computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalDevice {
    /// The values used to recognise this hardware again after it is replugged.
    pub fingerprint: DeviceFingerprint,
    /// Whether the hardware is attached right now.
    #[serde(skip, default)]
    pub present: bool,
    /// How confidently the attached hardware was matched to this stored entry.
    #[serde(skip, default = "default_confidence")]
    pub confidence: MatchConfidence,
    /// Names the application holding the device exclusively, when one does.
    #[serde(skip, default)]
    pub claimed_by: Option<String>,
    /// Whether this is another application's port rather than hardware.
    ///
    /// Kept only while a route names it: an application's port that goes away with nothing
    /// routed to or from it is forgotten, where hardware would be remembered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub software: bool,
}

/// Returns the confidence a freshly loaded device entry starts with.
fn default_confidence() -> MatchConfidence {
    MatchConfidence::None
}

/// How a session treats incoming invitations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvitationPolicy {
    /// Ask the user each time. The default, because silently accepting inbound connections is a
    /// surprising thing for a program to do.
    #[default]
    Prompt,
    /// Accept without asking from peers the user has already trusted.
    AcceptKnown,
    /// Accept from anyone.
    AcceptAll,
    /// Refuse everything.
    RejectAll,
}

/// What to do about one invitation that has arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvitationDecision {
    /// Let the peer in.
    Accept,
    /// Tell the peer no.
    Refuse,
    /// Neither, until the user says.
    Ask,
}

impl InvitationPolicy {
    /// Decides what to do about an invitation from a peer.
    ///
    /// A peer the user has already trusted is let in under every policy but `RejectAll`, which is
    /// what "always accept from this machine" has to mean if answering a prompt is to be worth
    /// anything.
    pub fn decide(self, trusted: bool) -> InvitationDecision {
        match self {
            Self::RejectAll => InvitationDecision::Refuse,
            Self::AcceptAll => InvitationDecision::Accept,
            Self::AcceptKnown | Self::Prompt if trusted => InvitationDecision::Accept,
            // An unknown peer under AcceptKnown is refused rather than held: the user has already
            // said which machines may connect, so there is nothing left to ask them.
            Self::AcceptKnown => InvitationDecision::Refuse,
            Self::Prompt => InvitationDecision::Ask,
        }
    }
}

/// An RTP-MIDI session with another machine or device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSession {
    /// The name advertised to other machines.
    pub local_name: EndpointName,
    /// The control port. The data port is always this plus one.
    pub control_port: u16,
    /// The peer this session connects to, when one is chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<PeerId>,
    /// The machines this side connected beside the peer, reconnected like the peer until the
    /// user disconnects each (FR-015i). Written only when there are some.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_peers: Vec<PeerId>,
    /// How incoming invitations are handled.
    #[serde(default)]
    pub invitation_policy: InvitationPolicy,
    /// Whether other applications on this computer see the session as a MIDI port of its name,
    /// joined to it both ways (FR-015h).
    #[serde(default = "default_enabled")]
    pub automatic_port: bool,
    /// The platform identifier of the automatic port's MIDI In, pinned so other applications
    /// recognise the same port after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_input_id: Option<u32>,
    /// The platform identifier of the automatic port's MIDI Out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_output_id: Option<u32>,
}

impl NetworkSession {
    /// Creates a session advertised under `local_name`, with its automatic port on.
    pub fn new(local_name: EndpointName, control_port: u16, policy: InvitationPolicy) -> Self {
        Self {
            local_name,
            control_port,
            peer: None,
            other_peers: Vec::new(),
            invitation_policy: policy,
            automatic_port: true,
            port_input_id: None,
            port_output_id: None,
        }
    }
}

/// Which side of a Bluetooth link this endpoint represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BleRole {
    /// We connected out to a device.
    Central,
    /// A device connected to us while we were advertising.
    Peripheral,
}

/// A Bluetooth LE MIDI link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BluetoothDevice {
    /// The platform's address for the device.
    pub address: String,
    /// Which side of the link we are.
    pub role: BleRole,
    /// Whether the user has paired with this device before.
    #[serde(default)]
    pub paired: bool,
    /// Signal strength, when the platform reports it.
    #[serde(skip, default)]
    pub rssi: Option<i16>,
}

/// What kind of thing an endpoint is, with the data specific to that kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EndpointKind {
    /// A virtual port for local applications.
    VirtualPort(VirtualPort),
    /// Attached MIDI hardware.
    PhysicalDevice(PhysicalDevice),
    /// A network port: an RTP-MIDI session with other machines.
    ///
    /// Written as `network_port`; `network_session`, the name it had before, is still read.
    #[serde(rename = "network_port", alias = "network_session")]
    NetworkSession(NetworkSession),
    /// A Bluetooth LE MIDI link.
    BluetoothDevice(BluetoothDevice),
}

/// Which kind an endpoint is, as the configuration file names it.
///
/// Written on a route only when the name alone would not say which endpoint it means, since names
/// may repeat across kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KindTag {
    /// A virtual port.
    VirtualPort,
    /// Attached hardware, or another application's port.
    PhysicalDevice,
    /// A network port. Written as `network_port`; `network_session` is still read.
    #[serde(rename = "network_port", alias = "network_session")]
    NetworkSession,
    /// A Bluetooth link.
    BluetoothDevice,
}

impl EndpointKind {
    /// Returns which kind this is, as the configuration file names it.
    pub fn tag(&self) -> KindTag {
        match self {
            Self::VirtualPort(_) => KindTag::VirtualPort,
            Self::PhysicalDevice(_) => KindTag::PhysicalDevice,
            Self::NetworkSession(_) => KindTag::NetworkSession,
            Self::BluetoothDevice(_) => KindTag::BluetoothDevice,
        }
    }

    /// Returns a short slug naming the kind, used in listings and filters.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::VirtualPort(_) => "virtual",
            Self::PhysicalDevice(_) => "physical",
            Self::NetworkSession(_) => "network",
            Self::BluetoothDevice(_) => "bluetooth",
        }
    }
}

/// Anything MIDI can flow to or from.
///
/// The kind is flattened into the endpoint when stored, so a configuration entry reads as one
/// flat block rather than nesting a `kind` map inside a `kind` field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredEndpoint")]
pub struct Endpoint {
    /// Stable identity, used by routes at runtime and by the daemon API.
    ///
    /// Generated when absent, so a hand-written configuration file need not contain any
    /// identifiers. The daemon fills it in the next time it writes the file.
    #[serde(default)]
    pub id: EndpointId,
    /// The user-facing name.
    pub name: EndpointName,
    /// What kind of endpoint this is.
    #[serde(flatten)]
    pub kind: EndpointKind,
    /// Whether the user wants this endpoint running. Independent of whether it currently is.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Which way MIDI can travel.
    #[serde(default = "default_direction")]
    pub direction: Direction,
}

/// An endpoint as a configuration file may state it, before the gaps are filled.
///
/// A session written by hand as a name and a kind made the whole file unreadable, because its
/// advertised name and port were required, and an unreadable file is set aside for an empty
/// one. Both now default as `session create` defaults them: the advertised name is the
/// endpoint's own, and the port is the system's choice.
#[derive(Deserialize)]
struct StoredEndpoint {
    #[serde(default)]
    id: EndpointId,
    name: EndpointName,
    #[serde(flatten)]
    kind: StoredKind,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_direction")]
    direction: Direction,
}

/// The kinds as a file may state them; only a session has anything left to fill in.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredKind {
    VirtualPort(VirtualPort),
    PhysicalDevice(PhysicalDevice),
    #[serde(rename = "network_port", alias = "network_session")]
    NetworkSession(StoredSession),
    BluetoothDevice(BluetoothDevice),
}

/// A session as a file may state it.
#[derive(Deserialize)]
struct StoredSession {
    #[serde(default)]
    local_name: Option<EndpointName>,
    #[serde(default)]
    control_port: u16,
    #[serde(default)]
    peer: Option<PeerId>,
    #[serde(default)]
    other_peers: Vec<PeerId>,
    #[serde(default)]
    invitation_policy: InvitationPolicy,
    #[serde(default = "default_enabled")]
    automatic_port: bool,
    #[serde(default)]
    port_input_id: Option<u32>,
    #[serde(default)]
    port_output_id: Option<u32>,
}

impl From<StoredEndpoint> for Endpoint {
    fn from(stored: StoredEndpoint) -> Self {
        let kind = match stored.kind {
            StoredKind::VirtualPort(port) => EndpointKind::VirtualPort(port.settled()),
            StoredKind::PhysicalDevice(device) => EndpointKind::PhysicalDevice(device),
            StoredKind::BluetoothDevice(device) => EndpointKind::BluetoothDevice(device),
            StoredKind::NetworkSession(session) => EndpointKind::NetworkSession(NetworkSession {
                local_name: session.local_name.unwrap_or_else(|| stored.name.clone()),
                control_port: session.control_port,
                peer: session.peer,
                other_peers: session.other_peers,
                invitation_policy: session.invitation_policy,
                automatic_port: session.automatic_port,
                port_input_id: session.port_input_id,
                port_output_id: session.port_output_id,
            }),
        };
        // A virtual port always has a connector of each kind, so an in-only or out-only port
        // from before connectors is read as one of each (R-078).
        let direction = match &kind {
            EndpointKind::VirtualPort(_) => Direction::Bidirectional,
            _ => stored.direction,
        };
        Self {
            id: stored.id,
            name: stored.name,
            kind,
            enabled: stored.enabled,
            direction,
        }
    }
}

/// Returns the default for endpoints that do not say whether they are switched on.
fn default_enabled() -> bool {
    true
}

/// Returns the default direction for endpoints that do not state one.
fn default_direction() -> Direction {
    Direction::Bidirectional
}

impl Endpoint {
    /// Creates an enabled, bidirectional endpoint.
    pub fn new(name: EndpointName, kind: EndpointKind) -> Self {
        Self {
            id: EndpointId::new(),
            name,
            kind,
            enabled: true,
            direction: Direction::Bidirectional,
        }
    }

    /// Reports whether this endpoint may be the source of a route.
    pub fn can_source(&self) -> bool {
        self.direction.can_source()
    }

    /// Reports whether this endpoint may be the destination of a route.
    pub fn can_sink(&self) -> bool {
        self.direction.can_sink()
    }
}

/// Reports whether `name` is already taken by a different endpoint that shares its namespace.
///
/// Names may repeat across most kinds, but two virtual ports called the same thing would be
/// indistinguishable to other applications. A network port shows to them as a port of its name
/// (FR-015h), so virtual ports and network ports share one namespace.
pub fn name_conflicts(
    endpoints: &[Endpoint],
    name: &EndpointName,
    kind_slug: &str,
    excluding: Option<EndpointId>,
) -> bool {
    endpoints.iter().any(|existing| {
        namespace(existing.kind.slug()) == namespace(kind_slug)
            && &existing.name == name
            && Some(existing.id) != excluding
    })
}

/// Returns each pair of endpoints that hold one name within one namespace, earlier one first.
///
/// A configuration written by hand can hold such a pair, which creating and renaming refuse.
pub fn name_clashes(endpoints: &[Endpoint]) -> Vec<(&Endpoint, &Endpoint)> {
    let mut clashes = Vec::new();
    for (index, later) in endpoints.iter().enumerate() {
        let earlier = endpoints.get(..index).unwrap_or_default();
        if let Some(first) = earlier.iter().find(|held| {
            namespace(held.kind.slug()) == namespace(later.kind.slug()) && held.name == later.name
        }) {
            clashes.push((first, later));
        }
    }
    clashes
}

/// Returns the namespace names of a kind are unique within.
///
/// Virtual ports and network ports share one, because other applications see both as ports.
fn namespace(kind_slug: &str) -> &str {
    match kind_slug {
        "virtual" | "network" => "port",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each invitation policy decides as the user means it, trusted peer or not.
    ///
    /// A trusted peer is let in under every policy but refusing everything, or answering "always
    /// accept from this machine" at a prompt would mean nothing, and refusing everything must
    /// refuse even a trusted peer or there is no way to shut out a machine trusted once. Under
    /// accept-known an unknown peer is refused rather than asked about, because the user has
    /// already said which machines may connect. The default asks, because silently accepting
    /// inbound connections is a surprising thing for a program to do.
    #[test]
    fn each_policy_decides_as_the_user_means_it() {
        use InvitationDecision::{Accept, Ask, Refuse};
        let cases = [
            (
                "the default asks about an unknown peer",
                InvitationPolicy::default(),
                false,
                Ask,
            ),
            (
                "prompting accepts a trusted peer",
                InvitationPolicy::Prompt,
                true,
                Accept,
            ),
            (
                "accept-known accepts a trusted peer",
                InvitationPolicy::AcceptKnown,
                true,
                Accept,
            ),
            (
                "accept-known refuses an unknown peer",
                InvitationPolicy::AcceptKnown,
                false,
                Refuse,
            ),
            (
                "accept-all accepts an unknown peer",
                InvitationPolicy::AcceptAll,
                false,
                Accept,
            ),
            (
                "reject-all refuses a trusted peer",
                InvitationPolicy::RejectAll,
                true,
                Refuse,
            ),
            (
                "reject-all refuses an unknown peer",
                InvitationPolicy::RejectAll,
                false,
                Refuse,
            ),
        ];
        for (case, policy, trusted, want) in cases {
            assert_eq!(
                policy.decide(trusted),
                want,
                "{case}: the policy must decide as the user set it"
            );
        }
    }

    /// A name is trimmed, and accepted up to 128 characters without control characters.
    ///
    /// Trimming stops two endpoints that display identically from coexisting. The limit is on
    /// characters rather than bytes, so the longest accepted name sits beside the shortest
    /// refused one.
    #[test]
    fn a_name_is_trimmed_and_held_to_its_limits() {
        let longest = "a".repeat(NAME_MAX);
        let too_long = "a".repeat(NAME_MAX + 1);
        let cases = [
            (
                "padding is trimmed",
                "  Sequencer Bus  ",
                Ok("Sequencer Bus"),
            ),
            (
                "the longest name is accepted",
                longest.as_str(),
                Ok(longest.as_str()),
            ),
            (
                "one character more is refused",
                too_long.as_str(),
                Err(NameError::TooLong(NAME_MAX + 1)),
            ),
            ("an empty name is refused", "", Err(NameError::Empty)),
            ("whitespace alone is refused", "   ", Err(NameError::Empty)),
            (
                "a bell is refused",
                "bad\u{7}name",
                Err(NameError::ControlCharacter),
            ),
            (
                "a newline is refused",
                "bad\nname",
                Err(NameError::ControlCharacter),
            ),
        ];
        for (case, raw, want) in cases {
            assert_eq!(
                EndpointName::new(raw).as_ref().map(EndpointName::as_str),
                want.as_ref().map(|text| *text),
                "{case}: a name must be accepted exactly within its limits"
            );
        }
    }
}
