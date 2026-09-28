//! The persisted setup: endpoints, peers, routes and preferences.

use crate::endpoint::{Endpoint, EndpointKind, EndpointName, InvitationPolicy, KindTag};
use crate::ids::{EndpointId, PeerId, RouteId};
use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Schema version written by this build.
pub const CURRENT_SCHEMA: u32 = 1;

/// Why configuration could not be loaded or saved.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read or written.
    #[error("could not {operation} {path}: {source}")]
    Io {
        /// What was being attempted.
        operation: &'static str,
        /// Which file it concerned.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The document could not be serialised.
    #[error("could not encode configuration: {0}")]
    Encode(#[from] serde_yaml_ng::Error),
    /// A document offered for import or reload could not be read as configuration.
    ///
    /// The message is the parser's alone, because it always reaches the user inside a
    /// `config_invalid` failure that already says what kind of problem this is.
    #[error("{0}")]
    Decode(serde_yaml_ng::Error),
    /// A document offered for import or reload reads, but describes something impossible.
    #[error("{0}")]
    Invalid(String),
    /// The stored schema is newer than this build understands.
    #[error("configuration schema {found} is newer than this build supports ({CURRENT_SCHEMA})")]
    SchemaTooNew {
        /// The version found in the file.
        found: u32,
    },
}

/// A remembered network peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerConfig {
    /// Stable identity for this peer.
    ///
    /// Generated when absent, like an endpoint's, so a peer can be written by hand without one.
    #[serde(default)]
    pub id: PeerId,
    /// The name the peer advertises.
    pub name: String,
    /// Addresses it was last reachable at, in `host:port` form.
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Whether invitations from this peer are accepted without asking.
    #[serde(default)]
    pub trusted: bool,
}

/// A persisted MIDI connection between two endpoints.
///
/// Endpoints are referenced by name rather than by identifier, so the stored document can be
/// read and edited by hand. Identity is not lost by doing so: renaming an endpoint through the
/// daemon rewrites every route that names it, in the same operation. A name that matches nothing
/// leaves the route visible and marked broken rather than silently dropped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteConfig {
    /// The name of the endpoint MIDI comes from.
    pub from: String,
    /// The name of the endpoint MIDI goes to.
    pub to: String,
    /// Which kind `from` is, written only when another kind has an endpoint of that name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_kind: Option<KindTag>,
    /// Which kind `to` is, on the same terms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_kind: Option<KindTag>,
    /// Which of the source's MIDI In connectors MIDI comes from, counting from one. Written only
    /// for a virtual port with more than one; absent means the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_connector: Option<u8>,
    /// Which of the destination's MIDI Out connectors MIDI goes to, on the same terms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_connector: Option<u8>,
    /// Whether it also carries MIDI back, from the destination's MIDI In of the same number to
    /// the source's MIDI Out of the same number, as one route (FR-034a).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub both_ways: bool,
    /// Whether the user wants it delivering.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Namespace for deriving stable route identifiers from the endpoints a route joins.
const ROUTE_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x6d, 0x69, 0x64, 0x69, 0x68, 0x61, 0x72, 0x62, 0x6f, 0x72, 0x72, 0x6f, 0x75, 0x74, 0x65, 0x73,
]);

impl RouteConfig {
    /// Returns this route's identity, derived from the pair of endpoints it joins.
    ///
    /// Derived rather than stored, because the pair is already unique — duplicates are rejected —
    /// so persisting an identifier would put a value in the file that the user cannot read and
    /// must not edit. Deriving keeps it stable across restarts without writing it down.
    pub fn id(&self) -> RouteId {
        // Kinds join the key only when written, so every route that needs none keeps the
        // identity it always had.
        let mut key = if self.from_kind.is_none() && self.to_kind.is_none() {
            format!("{}\u{1}{}", self.from, self.to)
        } else {
            format!(
                "{}\u{1}{}\u{1}{:?}\u{1}{:?}",
                self.from, self.to, self.from_kind, self.to_kind
            )
        };
        // Connectors join the key on the same terms: only past the first, so a route written
        // before ports had connectors keeps its identity.
        let (from, to) = (self.from_index(), self.to_index());
        if from > 0 || to > 0 {
            key.push_str(&format!("\u{1}{from}\u{1}{to}"));
        }
        RouteId::from_uuid(uuid::Uuid::new_v5(&ROUTE_NAMESPACE, key.as_bytes()))
    }

    /// Returns which of the source's MIDI In connectors this route starts from, counting from
    /// zero.
    pub fn from_index(&self) -> u8 {
        self.from_connector.unwrap_or(1).saturating_sub(1)
    }

    /// Returns which of the destination's MIDI Out connectors this route ends at, counting from
    /// zero.
    pub fn to_index(&self) -> u8 {
        self.to_connector.unwrap_or(1).saturating_sub(1)
    }

    /// Reports whether `endpoint` is this route's source.
    pub fn comes_from(&self, endpoint: &Endpoint) -> bool {
        names(&self.from, self.from_kind, endpoint)
    }

    /// Reports whether `endpoint` is this route's destination.
    pub fn goes_to(&self, endpoint: &Endpoint) -> bool {
        names(&self.to, self.to_kind, endpoint)
    }

    /// Reports whether `endpoint` is either end of this route.
    pub fn touches(&self, endpoint: &Endpoint) -> bool {
        self.comes_from(endpoint) || self.goes_to(endpoint)
    }

    /// Reports whether this route joins the same two endpoints as `other`.
    pub fn same_ends(&self, other: &RouteConfig) -> bool {
        self.id() == other.id()
    }
}

/// Reports whether a name, and a kind when one is written, designate `endpoint`.
fn names(name: &str, kind: Option<KindTag>, endpoint: &Endpoint) -> bool {
    endpoint.name.as_str() == name && kind.is_none_or(|kind| endpoint.kind.tag() == kind)
}

/// Returns the kind to write beside `endpoint`'s name on a route, which is its own kind when
/// another endpoint shares its name and nothing otherwise.
pub fn kind_to_write(endpoints: &[Endpoint], endpoint: &Endpoint) -> Option<KindTag> {
    endpoints
        .iter()
        .any(|other| other.id != endpoint.id && other.name == endpoint.name)
        .then(|| endpoint.kind.tag())
}

/// Returns the default for fields that should be on unless stated otherwise.
fn default_true() -> bool {
    true
}

/// Settings that are not about any one endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preferences {
    /// The name advertised for network sessions and Bluetooth advertising.
    ///
    /// Absent means "use this computer's name", which is what other machines expect to see and
    /// what distinguishes two machines on one network. Resolving it is the daemon's job, since
    /// asking the operating system its name is not something this crate does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_name: Option<String>,
    /// How invitations are treated when a session does not say.
    #[serde(default)]
    pub default_invitation_policy: InvitationPolicy,
    /// Whether to advertise this computer as a Bluetooth LE MIDI peripheral.
    #[serde(default)]
    pub bluetooth_advertising: bool,
    /// Whether network sessions are announced to other machines on the network.
    ///
    /// Off, a session still works and can be connected to by address, but no machine browsing
    /// the network sees it: for a network where announcing is unwelcome, and for test runs.
    #[serde(default = "default_true")]
    pub advertise_sessions: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            machine_name: None,
            default_invitation_policy: InvitationPolicy::default(),
            bluetooth_advertising: false,
            advertise_sessions: true,
        }
    }
}

/// The whole persisted setup.
///
/// Every MIDI connection the user has defined lives here — the endpoints themselves and the
/// routes between them — so the file is the complete description of a setup and is what
/// export and import move between machines.
///
/// Endpoints of every kind live in one list rather than four, because they already carry a kind
/// tag and share every other field. Splitting them would mean four parallel code paths for
/// validation, lookup and renaming, for no gain in the file's readability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Configuration {
    /// Which schema this document was written against.
    #[serde(default = "current_schema")]
    pub schema_version: u32,
    /// Settings not tied to an endpoint.
    #[serde(default)]
    pub preferences: Preferences,
    /// Every configured endpoint, of every kind.
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
    /// Peers the user has seen or added.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    /// The MIDI connections between endpoints. Persisted, so a setup survives a reboot.
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

/// Returns the schema version this build writes.
fn current_schema() -> u32 {
    CURRENT_SCHEMA
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA,
            preferences: Preferences::default(),
            endpoints: Vec::new(),
            peers: Vec::new(),
            routes: Vec::new(),
        }
    }
}

impl Configuration {
    /// Finds an endpoint by identity.
    pub fn endpoint(&self, id: EndpointId) -> Option<&Endpoint> {
        self.endpoints.iter().find(|e| e.id == id)
    }

    /// Finds an endpoint by name, optionally restricted to one kind.
    ///
    /// Returns `Err` listing the candidates when the name is ambiguous, because silently picking
    /// one would mean acting on hardware the user did not mean.
    pub fn endpoint_by_name(
        &self,
        name: &str,
        kind_slug: Option<&str>,
    ) -> Result<&Endpoint, Vec<EndpointId>> {
        let matches: Vec<&Endpoint> = self
            .endpoints
            .iter()
            .filter(|e| e.name.as_str() == name)
            .filter(|e| kind_slug.is_none_or(|slug| e.kind.slug() == slug))
            .collect();

        match matches.as_slice() {
            [only] => Ok(only),
            many => Err(many.iter().map(|e| e.id).collect()),
        }
    }

    /// Returns every endpoint of one kind.
    pub fn endpoints_of_kind<'a>(&'a self, slug: &'a str) -> impl Iterator<Item = &'a Endpoint> {
        self.endpoints.iter().filter(move |e| e.kind.slug() == slug)
    }

    /// Resolves a route's endpoints, returning the names that matched nothing.
    ///
    /// A route naming a missing endpoint is reported rather than dropped, so the interface can
    /// show it as broken and restore it if that endpoint comes back (FR-035).
    pub fn resolve_route(
        &self,
        route: &RouteConfig,
    ) -> Result<(EndpointId, EndpointId), Vec<String>> {
        let source = self
            .endpoints
            .iter()
            .find(|e| e.name.as_str() == route.from);
        let destination = self.endpoints.iter().find(|e| e.name.as_str() == route.to);

        let mut missing = Vec::new();
        if source.is_none() {
            missing.push(route.from.clone());
        }
        if destination.is_none() {
            missing.push(route.to.clone());
        }
        match (source, destination) {
            (Some(from), Some(to)) if missing.is_empty() => Ok((from.id, to.id)),
            _ => Err(missing),
        }
    }

    /// Renames an endpoint and rewrites every route that names it.
    ///
    /// Doing both in one operation is what keeps connections intact across a rename now that
    /// routes reference names (FR-004).
    pub fn rename_endpoint(&mut self, id: EndpointId, new_name: EndpointName) -> Option<String> {
        let old_name = self
            .endpoints
            .iter()
            .find(|e| e.id == id)?
            .name
            .as_str()
            .to_owned();
        if old_name == new_name.as_str() {
            return Some(old_name);
        }

        // Only the routes that mean this endpoint: a route naming another kind's endpoint of the
        // same name, or naming a name several endpoints share without saying which, is not about
        // this one.
        let renamed = self.endpoints.iter().find(|e| e.id == id)?.clone();
        let shared = self
            .endpoints
            .iter()
            .any(|other| other.id != id && other.name == renamed.name);
        let means = |name: &str, kind: Option<KindTag>| match kind {
            Some(kind) => name == old_name && kind == renamed.kind.tag(),
            None => name == old_name && !shared,
        };
        for route in &mut self.routes {
            if means(&route.from, route.from_kind) {
                route.from = new_name.as_str().to_owned();
            }
            if means(&route.to, route.to_kind) {
                route.to = new_name.as_str().to_owned();
            }
        }
        for endpoint in &mut self.endpoints {
            if endpoint.id == id {
                endpoint.name = new_name.clone();
            }
        }
        Some(old_name)
    }

    /// Adds an endpoint, first writing the kind onto any route that named an existing endpoint
    /// by the name the new one shares.
    pub fn add_endpoint(&mut self, endpoint: Endpoint) {
        let before = self.endpoints.clone();
        self.endpoints.push(endpoint);
        self.qualify_routes(&before);
    }

    /// Writes the kind onto every route end whose name designated one endpoint in `before` and
    /// now matches several.
    ///
    /// Only then is it still known which endpoint the route meant. Left alone, a route to a
    /// session named "Studio" would break the moment hardware of the same name was plugged in.
    pub fn qualify_routes(&mut self, before: &[Endpoint]) {
        let now = &self.endpoints;
        let meant = |name: &str| {
            let mut earlier = before.iter().filter(|e| e.name.as_str() == name);
            let only = earlier.next().filter(|_| earlier.next().is_none())?;
            let shared = now.iter().filter(|e| e.name.as_str() == name).count() > 1;
            shared.then(|| only.kind.tag())
        };
        let mut qualified = Vec::new();
        for (index, route) in self.routes.iter().enumerate() {
            let from = route.from_kind.or_else(|| meant(&route.from));
            let to = route.to_kind.or_else(|| meant(&route.to));
            qualified.push((index, from, to));
        }
        for (index, from, to) in qualified {
            if let Some(route) = self.routes.get_mut(index) {
                route.from_kind = from;
                route.to_kind = to;
            }
        }
    }

    /// Returns the virtual ports, which are what must be recreated on every startup.
    pub fn virtual_ports(&self) -> impl Iterator<Item = &Endpoint> {
        self.endpoints
            .iter()
            .filter(|e| matches!(e.kind, EndpointKind::VirtualPort(_)))
    }
}

/// What happened while loading configuration.
#[derive(Debug)]
pub struct LoadOutcome {
    /// The configuration to use.
    pub config: Configuration,
    /// Set when the stored file was unreadable and was preserved rather than lost.
    pub repaired: Option<Repair>,
}

/// A record of configuration that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    /// Where the unreadable file was moved to.
    pub preserved_at: PathBuf,
    /// What was wrong with it.
    pub reason: String,
}

/// Loads configuration, falling back to defaults when the file is missing or unreadable.
///
/// A file that cannot be parsed is never deleted and never silently replaced: it is renamed aside
/// so the user can recover their setup, and the caller is told so it can be reported.
pub fn load(paths: &Paths) -> Result<LoadOutcome, ConfigError> {
    let path = paths.config_file();

    // Absent configuration is the normal first-run case, not a problem.
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadOutcome {
                config: Configuration::default(),
                repaired: None,
            });
        }
        Err(source) => {
            return Err(ConfigError::Io {
                operation: "read",
                path,
                source,
            });
        }
    };

    match serde_yaml_ng::from_str::<Configuration>(&text) {
        Ok(config) if config.schema_version > CURRENT_SCHEMA => {
            // A newer schema is not corruption. Refusing loudly is safer than dropping fields we
            // do not understand and then writing the file back without them.
            Err(ConfigError::SchemaTooNew {
                found: config.schema_version,
            })
        }
        Ok(config) => Ok(LoadOutcome {
            config,
            repaired: None,
        }),
        Err(parse_error) => {
            let preserved_at = preserve_unreadable(&path)?;
            Ok(LoadOutcome {
                config: Configuration::default(),
                repaired: Some(Repair {
                    preserved_at,
                    reason: parse_error.to_string(),
                }),
            })
        }
    }
}

/// Reads a configuration document offered for import or reload.
///
/// Unlike `load`, nothing here falls back to defaults or moves a file aside. At startup an
/// unreadable file must not stop the daemon, but a document offered while it runs is a request to
/// change a working setup, and one that cannot be read is refused so the setup stays as it was.
pub fn parse(text: &str) -> Result<Configuration, ConfigError> {
    let config: Configuration = serde_yaml_ng::from_str(text).map_err(ConfigError::Decode)?;
    if config.schema_version > CURRENT_SCHEMA {
        return Err(ConfigError::SchemaTooNew {
            found: config.schema_version,
        });
    }

    // Two entries under one name make every route to that name ambiguous, and two under one
    // identifier make the entries themselves indistinguishable.
    let mut seen_ids = std::collections::HashSet::new();
    for endpoint in &config.endpoints {
        if !seen_ids.insert(endpoint.id) {
            return Err(ConfigError::Invalid(format!(
                "two endpoints share the identifier {}",
                endpoint.id
            )));
        }
    }
    check_names(&config)?;
    Ok(config)
}

/// Refuses a setup in which two endpoints hold a name only one of them may have.
///
/// Every route to that name would be ambiguous, and two ports of one name cannot be told apart by
/// other applications.
pub fn check_names(config: &Configuration) -> Result<(), ConfigError> {
    match crate::endpoint::name_clashes(&config.endpoints).first() {
        None => Ok(()),
        Some((first, second)) => Err(ConfigError::Invalid(clash_message(first, second))),
    }
}

/// Describes two endpoints that hold one name.
pub fn clash_message(first: &Endpoint, second: &Endpoint) -> String {
    if first.kind.slug() == second.kind.slug() {
        format!(
            "two {} endpoints are both named '{}'",
            first.kind.slug(),
            first.name
        )
    } else {
        format!(
            "a {} endpoint and a {} endpoint are both named '{}'",
            first.kind.slug(),
            second.kind.slug(),
            first.name
        )
    }
}

/// Returns the document exactly as `save` would write it.
pub fn to_text(config: &Configuration) -> Result<String, ConfigError> {
    Ok(serde_yaml_ng::to_string(config)?)
}

/// Writes configuration atomically, so an interrupted write cannot corrupt the stored setup.
///
/// The document is written to a temporary file in the same directory, flushed to disk, and then
/// renamed over the target. Rename within a directory is atomic, so a reader sees either the old
/// document or the new one and never a partial write.
pub fn save(paths: &Paths, config: &Configuration) -> Result<(), ConfigError> {
    let dir = paths.config_dir();
    fs::create_dir_all(dir).map_err(|source| ConfigError::Io {
        operation: "create",
        path: dir.to_path_buf(),
        source,
    })?;

    let encoded = serde_yaml_ng::to_string(config)?;
    let target = paths.config_file();
    let temporary = dir.join(format!(
        "{}.tmp-{}",
        crate::paths::CONFIG_FILE,
        std::process::id()
    ));

    // Write and flush the whole document before anything points at it.
    {
        let mut file = fs::File::create(&temporary).map_err(|source| ConfigError::Io {
            operation: "create",
            path: temporary.clone(),
            source,
        })?;
        file.write_all(encoded.as_bytes())
            .map_err(|source| ConfigError::Io {
                operation: "write",
                path: temporary.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| ConfigError::Io {
            operation: "flush",
            path: temporary.clone(),
            source,
        })?;
    }

    fs::rename(&temporary, &target).map_err(|source| ConfigError::Io {
        operation: "replace",
        path: target,
        source,
    })
}

/// Moves an unreadable configuration file aside, returning where it was put.
fn preserve_unreadable(path: &Path) -> Result<PathBuf, ConfigError> {
    let stamp = jiff::Timestamp::now().as_second();
    let mut preserved = path.to_path_buf();
    preserved.set_extension(format!("corrupt-{stamp}"));

    fs::rename(path, &preserved).map_err(|source| ConfigError::Io {
        operation: "preserve",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(preserved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{MAX_CONNECTORS, NetworkSession, PhysicalDevice, VirtualPort};
    use crate::fingerprint::{DeviceFingerprint, MatchConfidence};

    /// Creates a paths root under a unique temporary directory.
    ///
    /// The shared root is emptied on first use in each run, so scratch from earlier runs does not
    /// pile up.
    fn temp_paths(label: &str) -> Paths {
        static EMPTIED: std::sync::Once = std::sync::Once::new();
        let shared = std::env::temp_dir().join("midi-harbor-tests");
        EMPTIED.call_once(|| {
            let _ = std::fs::remove_dir_all(&shared);
        });
        Paths::rooted_at(shared.join(format!("{label}-{}", uuid::Uuid::new_v4())))
    }

    fn name(text: &str) -> EndpointName {
        EndpointName::new(text).expect("a valid endpoint name")
    }

    fn port(text: &str) -> Endpoint {
        Endpoint::new(
            name(text),
            EndpointKind::VirtualPort(VirtualPort::default()),
        )
    }

    fn session(text: &str, session: NetworkSession) -> Endpoint {
        Endpoint::new(name(text), EndpointKind::NetworkSession(session))
    }

    fn hardware(text: &str) -> Endpoint {
        Endpoint::new(
            name(text),
            EndpointKind::PhysicalDevice(PhysicalDevice {
                fingerprint: DeviceFingerprint::from_name(text),
                present: true,
                confidence: MatchConfidence::Exact,
                claimed_by: None,
                software: false,
            }),
        )
    }

    fn route(from: &str, to: &str) -> RouteConfig {
        RouteConfig {
            from: from.to_owned(),
            to: to.to_owned(),
            from_kind: None,
            to_kind: None,
            from_connector: None,
            to_connector: None,
            both_ways: false,
            enabled: true,
        }
    }

    /// Returns the configuration with every generated identifier replaced by the nil UUID, so a
    /// document read without identifiers can be compared whole.
    fn without_ids(mut config: Configuration) -> Configuration {
        for endpoint in &mut config.endpoints {
            endpoint.id = EndpointId::from_uuid(uuid::Uuid::nil());
        }
        for peer in &mut config.peers {
            peer.id = PeerId::from_uuid(uuid::Uuid::nil());
        }
        config
    }

    /// A route's identity is derived from its ends and never changes for a route that existed
    /// before connectors did.
    ///
    /// Identity is derived, not stored, so a change to the derivation gives every existing route
    /// a new identifier and a client holding one loses track of it. The key of a route with no
    /// kinds and first connectors is `from`, U+0001, `to`, hashed as a UUID v5 under
    /// `ROUTE_NAMESPACE`, which is what routes were keyed by before connectors existed.
    #[test]
    fn a_route_identity_is_derived_from_its_ends_as_it_was_before_connectors() {
        let before_connectors = RouteId::from_uuid(uuid::Uuid::new_v5(
            &ROUTE_NAMESPACE,
            "Keys\u{1}Synth".as_bytes(),
        ));
        assert_eq!(
            route("Keys", "Synth").id(),
            before_connectors,
            "a route without connectors must keep the identity derived before connectors"
        );

        let cases = [
            (
                "naming the first connectors is the route naming none",
                RouteConfig {
                    from_connector: Some(1),
                    to_connector: Some(1),
                    ..route("Keys", "Synth")
                },
                true,
            ),
            (
                "a second MIDI Out is another route",
                RouteConfig {
                    to_connector: Some(2),
                    ..route("Keys", "Synth")
                },
                false,
            ),
            (
                "the reverse direction is another route",
                route("Synth", "Keys"),
                false,
            ),
        ];
        for (case, other, same) in cases {
            assert_eq!(
                other.id() == before_connectors,
                same,
                "{case}: identity must follow the ends and connectors, not the spelling"
            );
        }
    }

    /// A whole setup saved to disk loads back identical, written as block YAML a person can edit.
    ///
    /// The file is the complete description of a setup and is what export and import carry, so
    /// every field must survive: connector identifiers, a network port's peers and its automatic
    /// port, routes and preferences. Flow style (`{...}`) is valid YAML but unreadable by hand.
    #[test]
    fn a_setup_round_trips_through_disk_as_block_yaml() {
        let paths = temp_paths("round-trip");
        let studio_pc = PeerId::new();
        let beside = PeerId::new();
        let original = Configuration {
            preferences: Preferences {
                machine_name: Some("Studio Mac".to_owned()),
                default_invitation_policy: InvitationPolicy::AcceptKnown,
                bluetooth_advertising: true,
                advertise_sessions: false,
            },
            endpoints: vec![
                Endpoint::new(
                    name("Sequencer Bus"),
                    EndpointKind::VirtualPort(VirtualPort {
                        input_ids: vec![11, 12],
                        output_ids: vec![21],
                        ..VirtualPort::with_connectors(2, 1)
                    }),
                ),
                session(
                    "Stage",
                    NetworkSession {
                        peer: Some(studio_pc),
                        other_peers: vec![beside],
                        automatic_port: false,
                        port_input_id: Some(7),
                        port_output_id: Some(8),
                        ..NetworkSession::new(
                            name("Front of House"),
                            5104,
                            InvitationPolicy::AcceptAll,
                        )
                    },
                ),
            ],
            peers: vec![PeerConfig {
                id: studio_pc,
                name: "Studio PC".to_owned(),
                addresses: vec!["192.0.2.13:5004".to_owned()],
                trusted: true,
            }],
            routes: vec![RouteConfig {
                from_connector: Some(2),
                both_ways: true,
                enabled: false,
                ..route("Sequencer Bus", "Stage")
            }],
            ..Configuration::default()
        };

        save(&paths, &original).expect("the configuration saves");
        let loaded = load(&paths).expect("the configuration loads");
        assert!(
            loaded.repaired.is_none(),
            "a file this build wrote must never be set aside as unreadable"
        );
        assert_eq!(
            loaded.config, original,
            "every field of the setup must survive a save and load"
        );

        let text = fs::read_to_string(paths.config_file()).expect("the file reads");
        assert!(
            !text.trim_start().starts_with('{'),
            "the file must be block YAML a person can edit, not flow style: {text}"
        );
    }

    /// A document written by hand, or by an older build, reads with every omitted field filled in
    /// as the daemon would have filled it.
    ///
    /// People write this file from scratch, so nothing in it may require a UUID or a field the
    /// author would not think of, and files from before a rename or a new field must still load.
    /// An unreadable file is set aside for an empty one at startup, which loses the whole setup,
    /// so each of these refusing is the failure being guarded.
    #[test]
    fn hand_written_and_older_documents_read_with_their_gaps_filled() {
        let earlier_peer =
            PeerId::parse("6f1c2d3e-0000-4000-8000-000000000001").expect("a valid peer identifier");
        let with = |endpoints: Vec<Endpoint>| Configuration {
            endpoints,
            ..Configuration::default()
        };
        let cases = [
            (
                "endpoints and a route written with no identifiers",
                "endpoints:\n\
                 - name: Sequencer Bus\n\
                 \x20 kind: virtual_port\n\
                 - name: Stage Laptop\n\
                 \x20 kind: network_session\n\
                 \x20 local_name: Studio Mac\n\
                 \x20 control_port: 5004\n\
                 routes:\n\
                 - from: Sequencer Bus\n\
                 \x20 to: Stage Laptop\n",
                Configuration {
                    routes: vec![route("Sequencer Bus", "Stage Laptop")],
                    ..with(vec![
                        port("Sequencer Bus"),
                        session(
                            "Stage Laptop",
                            NetworkSession::new(name("Studio Mac"), 5004, InvitationPolicy::Prompt),
                        ),
                    ])
                },
            ),
            (
                "a peer written with no identifier",
                "peers:\n\
                 - name: Studio PC\n\
                 \x20 addresses: [\"192.0.2.13:5004\"]\n\
                 \x20 trusted: true\n",
                Configuration {
                    peers: vec![PeerConfig {
                        id: PeerId::from_uuid(uuid::Uuid::nil()),
                        name: "Studio PC".to_owned(),
                        addresses: vec!["192.0.2.13:5004".to_owned()],
                        trusted: true,
                    }],
                    ..Configuration::default()
                },
            ),
            (
                "a session written as a name and a kind advertises its name on a system port",
                "endpoints:\n\
                 - name: Stage\n\
                 \x20 kind: network_session\n",
                with(vec![session(
                    "Stage",
                    NetworkSession::new(name("Stage"), 0, InvitationPolicy::Prompt),
                )]),
            ),
            (
                "a network port from before other peers has none",
                "endpoints:\n\
                 - name: Stage\n\
                 \x20 kind: network_port\n\
                 \x20 control_port: 5004\n\
                 \x20 peer: 6f1c2d3e-0000-4000-8000-000000000001\n\
                 \x20 automatic_port: false\n",
                with(vec![session(
                    "Stage",
                    NetworkSession {
                        peer: Some(earlier_peer),
                        automatic_port: false,
                        ..NetworkSession::new(name("Stage"), 5004, InvitationPolicy::Prompt)
                    },
                )]),
            ),
            (
                "an out-only port from before connectors gains a MIDI In and keeps its identifier",
                "endpoints:\n\
                 - name: Keys\n\
                 \x20 kind: virtual_port\n\
                 \x20 platform_unique_id: 1234\n\
                 \x20 direction: output\n",
                with(vec![Endpoint::new(
                    name("Keys"),
                    EndpointKind::VirtualPort(VirtualPort {
                        output_ids: vec![1234],
                        ..VirtualPort::default()
                    }),
                )]),
            ),
            (
                "connector counts out of range are kept between one and sixteen",
                "endpoints:\n\
                 - name: Keys\n\
                 \x20 kind: virtual_port\n\
                 \x20 inputs: 0\n\
                 \x20 outputs: 40\n",
                with(vec![Endpoint::new(
                    name("Keys"),
                    EndpointKind::VirtualPort(VirtualPort {
                        inputs: 1,
                        outputs: MAX_CONNECTORS,
                        ..VirtualPort::default()
                    }),
                )]),
            ),
        ];
        for (case, text, want) in cases {
            let read = parse(text).unwrap_or_else(|err| panic!("{case}: refused with {err}"));
            assert_eq!(
                without_ids(read),
                without_ids(want),
                "{case}: omitted fields must take the values the daemon would have given them"
            );
        }
    }

    /// A document read under an older spelling is written back in the current one, and says
    /// nothing about fields it does not use.
    ///
    /// Network sessions were renamed network ports, and a port's single identifier moved into
    /// `output_ids` when ports gained connectors. Writing the old spellings back would keep them
    /// alive forever; writing empty fields would clutter a file people read.
    #[test]
    fn older_spellings_are_written_back_in_the_current_form() {
        let cases = [
            (
                "a session under its old kind name",
                "endpoints:\n\
                 - name: Stage\n\
                 \x20 kind: network_session\n",
                &["kind: network_port"][..],
                &["network_session", "other_peers"][..],
            ),
            (
                "a port with the identifier it had before connectors",
                "endpoints:\n\
                 - name: Keys\n\
                 \x20 kind: virtual_port\n\
                 \x20 platform_unique_id: 1234\n",
                &["output_ids"][..],
                &["platform_unique_id"][..],
            ),
            (
                "a route naming the old kind",
                "routes:\n\
                 - from: Stage\n\
                 \x20 to: Keys\n\
                 \x20 from_kind: network_session\n",
                &["from_kind: network_port"][..],
                &["network_session"][..],
            ),
        ];
        for (case, text, has, lacks) in cases {
            let read = parse(text).unwrap_or_else(|err| panic!("{case}: refused with {err}"));
            let written = to_text(&read).expect("the configuration encodes");
            for wanted in has {
                assert!(
                    written.contains(wanted),
                    "{case}: the current spelling {wanted:?} must be written: {written}"
                );
            }
            for unwanted in lacks {
                assert!(
                    !written.contains(unwanted),
                    "{case}: {unwanted:?} must not be written back: {written}"
                );
            }
        }
    }

    /// A route keeps meaning the endpoint it meant when an endpoint of another kind arrives under
    /// the same name, and renaming one of the two rewrites only the routes that mean it.
    ///
    /// Hardware plugged in under a session's name would otherwise break the route to the session,
    /// since a name several endpoints share designates none of them. The kind is written onto the
    /// route while it is still known which endpoint the route meant.
    #[test]
    fn a_route_keeps_meaning_its_endpoint_when_another_kind_takes_the_name() {
        let mut config = Configuration::default();
        let studio = session(
            "Studio",
            NetworkSession::new(name("Studio"), 0, InvitationPolicy::Prompt),
        );
        let studio_id = studio.id;
        config.add_endpoint(studio);
        config.add_endpoint(port("Synth"));
        config.routes.push(route("Studio", "Synth"));

        config.add_endpoint(hardware("Studio"));
        assert_eq!(
            (config.routes[0].from_kind, config.routes[0].to_kind),
            (Some(KindTag::NetworkSession), None),
            "only the end whose name is now shared must be given the kind it meant"
        );

        config.routes.push(RouteConfig {
            from_kind: Some(KindTag::PhysicalDevice),
            ..route("Studio", "Synth")
        });
        config.rename_endpoint(studio_id, name("Stage"));
        assert_eq!(
            config.routes[0].from, "Stage",
            "the route meaning the session must follow its rename"
        );
        assert_eq!(
            config.routes[1].from, "Studio",
            "the route meaning the hardware must keep the name the hardware still has"
        );
    }
}
