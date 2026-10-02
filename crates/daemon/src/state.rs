//! The daemon's owned state.

use crate::dataplane::{self, RtConsumer};
use crate::discovery::{DiscoveredPeer, Discovery, Followed, Step};
use crate::identity::{Identity, PortIdentity};
use crate::session::{
    Inbound, InvitationNotice, NetworkSession, Place, RESUME_AFTER_SLEEP, SessionNotice,
};
use midi_harbor_core::config::{self, Configuration, RouteConfig};
use midi_harbor_core::counters::TrafficCounters;
use midi_harbor_core::endpoint::{
    Direction, InvitationPolicy, NetworkSession as NetworkSessionConfig,
};
use midi_harbor_core::endpoint::{
    Endpoint, EndpointKind, EndpointName, VirtualPort, connector_name,
};
use midi_harbor_core::events::{self, EventKind, EventLog, Severity};
use midi_harbor_core::failure::FailureReason;
use midi_harbor_core::fingerprint::{DeviceFingerprint, MatchConfidence};
use midi_harbor_core::ids::{EndpointId, RouteId};
use midi_harbor_core::midi::{Channel, MidiMessage};
use midi_harbor_core::paths::Paths;
use midi_harbor_core::router::{Delivery, RouteError, Router};
use midi_harbor_core::rtchannel::RtProducer;
use midi_harbor_core::sounding::Sounding;
use midi_harbor_core::state::{ConnectionPhase, ConnectionState};
use midi_harbor_core::time::{Clock, SystemClock};
use midi_harbor_platform::bluetooth::{BluetoothPlatform, DiscoveredPeripheral, LinkHandle};
use midi_harbor_platform::fake::FakeBluetoothPlatform;
use midi_harbor_platform::midi::{
    ConnectorIds, MidiPlatform, MidiPlatformEvent, PortHandle, VirtualPortSpec,
};
use midi_harbor_platform::sysevents::{PolledSystemEvents, SystemEvent, SystemEvents};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, error, info, warn};

/// How often the platform is asked what changed about the MIDI environment.
///
/// SC-010a gives hardware two seconds to appear after being plugged in, and this is most of the
/// budget: the rest is the enumeration itself.
const DEVICE_EVENT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// How often machine-level events are collected.
///
/// A second is far below anything a person notices after a wake and far above what polling an
/// empty list costs.
pub const SYSTEM_EVENT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a drain task waits for MIDI before looking anyway.
///
/// Arriving MIDI wakes the drain at once through the doorbell, so this only paces the checks that
/// run when nothing is arriving: whether the endpoint still exists, and whether anything was
/// dropped. Draining every 2 ms instead cost a millisecond of latency on average, the whole of
/// SC-008's budget, and several percent of a core while idle, several times SC-010's.
const DISPATCH_IDLE_CHECK: std::time::Duration = std::time::Duration::from_millis(250);

/// Most messages taken from one endpoint per drain, so a flood cannot starve the others.
const DISPATCH_BATCH: usize = 512;

/// How many observed messages a monitor subscriber may fall behind before it starts missing them.
///
/// Small on purpose. A monitor that buffers heavily shows the user a stale picture, and the point
/// of watching is to see what is happening now.
const MONITOR_CAPACITY: usize = 256;

/// How many state updates a slow subscriber may fall behind before it is dropped.
const STATE_CHANNEL_CAPACITY: usize = 256;

/// Why a daemon operation failed.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The operation failed for a reason the domain already describes.
    #[error("{0}")]
    Failure(#[from] FailureReason),
    /// The named endpoint does not exist.
    #[error("no endpoint with id {0}")]
    NotFound(String),
    /// The name is ambiguous, and picking one would risk acting on the wrong endpoint.
    #[error("{count} endpoints are named {name}; use an id instead")]
    Ambiguous {
        /// The name that matched more than once.
        name: String,
        /// How many endpoints matched.
        count: usize,
    },
    /// The proposed name is not usable.
    #[error("{0}")]
    InvalidName(#[from] midi_harbor_core::endpoint::NameError),
    /// The route asked for cannot exist, such as one from an endpoint that sends nothing.
    #[error("{0}")]
    InvalidRoute(RouteError),
    /// Configuration could not be read or written.
    #[error("{0}")]
    Config(#[from] config::ConfigError),
    /// The operation needs the user to confirm it first.
    #[error("{detail}")]
    ConfirmationRequired {
        /// What the user is being asked to confirm.
        detail: String,
    },
    /// MIDI cannot be sent out of the endpoint as it is now.
    #[error("{0}")]
    CannotSend(String),
}

/// What changed, for clients watching the system.
#[derive(Debug, Clone)]
pub enum Change {
    /// An endpoint was created or discovered.
    EndpointAdded(EndpointId),
    /// An endpoint's configuration or state changed.
    EndpointChanged(EndpointId),
    /// An endpoint went away.
    EndpointRemoved(EndpointId),
    /// A route was created, changed or removed.
    RoutesChanged,
}

/// Stores the platform identifier an endpoint was assigned, reporting whether anything changed.
/// Finds an endpoint by identifier or by name.
///
/// A name that more than one endpoint answers to is refused rather than resolved, because acting
/// on the wrong endpoint is worse than making the user be specific.
fn find_endpoint<'a>(
    config: &'a Configuration,
    reference: &str,
) -> Result<&'a Endpoint, DaemonError> {
    if let Ok(id) = EndpointId::parse(reference)
        && let Some(endpoint) = config.endpoint(id)
    {
        return Ok(endpoint);
    }
    match config.endpoint_by_name(reference, None) {
        Ok(endpoint) => Ok(endpoint),
        Err(candidates) if candidates.is_empty() => {
            Err(DaemonError::NotFound(reference.to_owned()))
        }
        Err(candidates) => Err(DaemonError::Ambiguous {
            name: reference.to_owned(),
            count: candidates.len(),
        }),
    }
}

/// Stores the identifiers the platform gave a virtual port's connectors, reporting whether any
/// changed and so need saving.
///
/// Every connector's is kept, not only the first, or applications bound to the others would lose
/// them on the next restart. A platform with no such identifiers, as ALSA has none, changes
/// nothing.
pub(crate) fn record_platform_ids(
    config: &mut Configuration,
    id: EndpointId,
    assigned: &ConnectorIds,
) -> bool {
    let Some(endpoint) = config.endpoints.iter_mut().find(|e| e.id == id) else {
        return false;
    };
    let EndpointKind::VirtualPort(port) = &mut endpoint.kind else {
        return false;
    };
    let mut changed = false;
    if !assigned.inputs.is_empty() && port.input_ids != assigned.inputs {
        port.input_ids.clone_from(&assigned.inputs);
        changed = true;
    }
    if !assigned.outputs.is_empty() && port.output_ids != assigned.outputs {
        port.output_ids.clone_from(&assigned.outputs);
        changed = true;
    }
    changed
}

/// What a route should join: its ends by name or identifier, which of their connectors, each
/// counted from zero, and whether it carries MIDI back as well.
#[derive(Clone, Copy, Debug)]
pub struct RouteRequest<'a> {
    /// Where MIDI comes from.
    pub from: &'a str,
    /// Which of the source's MIDI In connectors.
    pub from_connector: u8,
    /// Where MIDI goes.
    pub to: &'a str,
    /// Which of the destination's MIDI Out connectors.
    pub to_connector: u8,
    /// Whether MIDI also comes back, through the same connector numbers the other way round.
    pub both_ways: bool,
}

/// Works out the route a request describes, refusing one that cannot be made alongside
/// `others`.
fn planned_route(
    config: &Configuration,
    others: &[RouteConfig],
    request: &RouteRequest<'_>,
) -> Result<RouteConfig, DaemonError> {
    // By identifier or by name, as the contract says. Only names were tried, so a client passing
    // identifiers, which is the only way to tell two endpoints of one name apart, was told they
    // did not exist.
    let source = find_endpoint(config, request.from)?;
    let destination = find_endpoint(config, request.to)?;

    Router::validate(
        others,
        source,
        request.from_connector,
        destination,
        request.to_connector,
        request.both_ways,
    )
    .map_err(|error| match error {
        RouteError::Duplicate { .. } => DaemonError::Failure(FailureReason::NameConflict {
            name: format!("{} -> {}", request.from, request.to),
        }),
        // A request that cannot be met, not a configuration that is wrong. Reported as an
        // invalid configuration, it read "configuration is invalid" and then repeated the same
        // reason as the thing to correct.
        other => DaemonError::InvalidRoute(other),
    })?;

    // A kind is written beside a name only when another kind uses the same one, and a connector
    // only past the first, so the file stays as plain as it can while every route still means
    // one thing.
    Ok(RouteConfig {
        from: source.name.as_str().to_owned(),
        to: destination.name.as_str().to_owned(),
        from_kind: config::kind_to_write(&config.endpoints, source),
        to_kind: config::kind_to_write(&config.endpoints, destination),
        from_connector: (request.from_connector > 0)
            .then(|| request.from_connector.saturating_add(1)),
        to_connector: (request.to_connector > 0).then(|| request.to_connector.saturating_add(1)),
        both_ways: request.both_ways,
        enabled: true,
    })
}

/// A virtual port just opened: its handle, its connectors' identifiers, and the rings its MIDI In
/// connectors fill, one per connector in order.
pub(crate) struct OpenedPort {
    /// The platform's handle for the whole port.
    pub handle: PortHandle,
    /// The identifiers to store for next time.
    pub ids: ConnectorIds,
    /// One ring per MIDI In connector, for the dispatch loop to drain.
    pub consumers: Vec<RtConsumer>,
}

/// Counts messages a route carried, so a route that is working can be told from one that is not.
///
/// A route with no counters is one that vanished from the configuration mid-dispatch, which is
/// not worth an error: the batch is already on its way to an endpoint that still exists.
fn record_carried(route: Option<&RouteRuntime>, messages: &[MidiMessage], at: jiff::Timestamp) {
    let Some(route) = route else {
        return;
    };
    for message in messages {
        route
            .counters
            .record_sent(u64::try_from(message.len()).unwrap_or(0), at);
    }
    // Tracked per route as well as per endpoint, because only the notes this route carried may
    // be stopped when it stops carrying: a destination can be fed by several sources.
    if let Ok(mut sounding) = route.sounding.lock() {
        for message in messages {
            sounding.record(message);
        }
    }
}

/// Counts one system-exclusive message a route carried, by its size on the wire.
fn record_carried_bytes(route: Option<&RouteRuntime>, len: usize, at: jiff::Timestamp) {
    if let Some(route) = route {
        route
            .counters
            .record_sent(u64::try_from(len).unwrap_or(0), at);
    }
}

/// Counts messages a route could not deliver.
///
/// Recorded as dropped rather than lost: the destination refused them here, which is our own
/// problem to fix, not the network losing packets in flight.
fn record_undelivered(route: Option<&RouteRuntime>, count: usize) {
    if let Some(route) = route {
        route
            .counters
            .record_dropped(u64::try_from(count).unwrap_or(0));
    }
}

/// What passed through an endpoint.
///
/// System-exclusive messages travel behind an `Arc` because a monitor broadcasts to every watcher
/// and a dump can be large; cloning one per subscriber would copy a patch bank for each open
/// window.
#[derive(Debug, Clone)]
pub enum Seen {
    /// A channel, system common or real-time message.
    Message(MidiMessage),
    /// A whole system-exclusive message, framing included.
    SysEx(Arc<[u8]>),
}

/// One message observed passing through an endpoint.
#[derive(Debug, Clone)]
pub struct Observed {
    /// When it was seen.
    pub at: jiff::Timestamp,
    /// What it was.
    pub seen: Seen,
    /// Whether it was leaving the endpoint rather than arriving.
    pub outbound: bool,
}

/// The runtime half of an endpoint, which is never persisted.
pub struct Runtime {
    /// Where this endpoint sits in its lifecycle.
    pub state: ConnectionState,
    /// The platform's handle, while the endpoint is open.
    pub handle: Option<PortHandle>,
    /// Live traffic counts, shared so the data path can increment them without a lock.
    pub counters: Arc<TrafficCounters>,
    /// The notes this endpoint has sounding, so they can be stopped before it stops carrying.
    ///
    /// A plain mutex rather than the outer lock: it is taken for the length of one update, never
    /// across an await, and the dispatch path already holds the outer lock for reading.
    pub sounding: std::sync::Mutex<Sounding>,
    /// The controller state this endpoint has been sent, resent when its link recovers (FR-027).
    pub controls: std::sync::Mutex<midi_harbor_core::controls::Controls>,
    /// What went out through this endpoint recently, and from where. Kept for sessions, whose
    /// sends are what a loop across machines brings back.
    pub sent: std::sync::Mutex<midi_harbor_core::loops::SentLog>,
    /// Messages passing through, for anyone watching.
    ///
    /// Deliberately lossy: a subscriber that cannot keep up misses messages rather than slowing
    /// the endpoint down. A monitor exists to show what is happening, not to alter it.
    pub monitor: broadcast::Sender<Observed>,
}

impl Runtime {
    /// Creates runtime state for an endpoint in the given lifecycle position.
    pub fn new(state: ConnectionState, handle: Option<PortHandle>) -> Self {
        let (monitor, _) = broadcast::channel(MONITOR_CAPACITY);
        Self {
            state,
            handle,
            counters: Arc::new(TrafficCounters::new()),
            sounding: std::sync::Mutex::new(Sounding::new()),
            controls: std::sync::Mutex::new(midi_harbor_core::controls::Controls::new()),
            sent: std::sync::Mutex::new(midi_harbor_core::loops::SentLog::new()),
            monitor,
        }
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("state", &self.state)
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

/// Owns configuration, endpoint runtime state, and the platform backend.
pub struct Daemon {
    pub(crate) paths: Paths,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) midi: Arc<dyn MidiPlatform>,
    /// The radio, which may be unable to play either role and says so when asked.
    pub(crate) bluetooth: Arc<dyn BluetoothPlatform>,
    pub(crate) inner: RwLock<Inner>,
    pub(crate) changes: broadcast::Sender<Change>,
    started_at: jiff::Timestamp,
    /// When the platform's MIDI service was found gone, for the warning this process shows after
    /// replacing the one that lost it, until a client dismisses it.
    midi_server_replaced_at: RwLock<Option<jiff::Timestamp>>,
    /// When this process found the MIDI service gone, to hand to the process replacing it.
    midi_server_lost_at: RwLock<Option<jiff::Timestamp>>,
    /// The running network sessions, held apart from `inner` so a session operation never has to
    /// await while the configuration lock is held.
    pub(crate) sessions: RwLock<HashMap<EndpointId, Arc<NetworkSession>>>,
    /// Peers accepted for this run only, because the user said yes without saying always.
    ///
    /// Not persisted: the answer was about one connection. It has to outlive the invitation that
    /// prompted it, because the handshake it answers has a few seconds of patience and a person
    /// does not, so the peer's next attempt is the one that gets in.
    accepted_once: RwLock<Vec<std::net::IpAddr>>,
    /// Held for the length of one device re-enumeration.
    ///
    /// Two of these may now be in flight at once — the watcher's and a caller's — and they each
    /// read the platform, decide what changed, and then write. Interleaved, one can undo the
    /// other's conclusion or open a device the other just closed.
    refreshing: tokio::sync::Mutex<()>,
    /// Signalled when this process can no longer do its job and a new one has to take over.
    restart: tokio::sync::Notify,
    /// Signalled when a client asks the daemon to stop.
    stop: tokio::sync::Notify,
    /// Invitations waiting on the user, keyed by their identifier.
    ///
    /// Held apart from `inner` for the same reason as the sessions: answering one talks to a
    /// session, and doing that under the configuration lock would deadlock the daemon.
    pub(crate) invitations: RwLock<HashMap<String, PendingInvitation>>,
    /// Discovery, when it could be started. A machine without it still runs; peers simply have
    /// to be added by address.
    pub(crate) discovery: Option<Arc<Discovery>>,
    /// The key this daemon's network ports prove themselves with.
    identity: Arc<Identity>,
    /// Held while machines are followed to where they are advertised, so two looks at once do
    /// not each move the same machine.
    following: tokio::sync::Mutex<()>,
    /// Why the service cannot be installed on this machine, when it cannot. Asked once, since
    /// whether a service manager exists does not change while the daemon runs.
    pub(crate) service_unavailable: Option<midi_harbor_core::capability::UnavailableReason>,
    /// This computer's name, advertised when the configuration names none.
    host_name: String,
    /// Open Bluetooth links, held apart from `inner` for the same reason as the sessions.
    pub(crate) bt_links: RwLock<HashMap<EndpointId, LinkHandle>>,
    /// Peripherals the radio can see right now, which is not the same as those configured.
    pub(crate) bt_seen: RwLock<Vec<DiscoveredPeripheral>>,
    /// The endpoint standing for this machine's own advertised port, while it is advertising.
    pub(crate) bt_advertised: RwLock<Option<EndpointId>>,
    /// How many devices are subscribed to the advertised port, so the port reads connected only
    /// while one is.
    pub(crate) bt_centrals: std::sync::atomic::AtomicUsize,
    /// Bluetooth devices heard while their backoff was still running, each with a reconnect
    /// already scheduled for when it ends.
    pub(crate) bt_deferred: RwLock<HashSet<EndpointId>>,
    /// Why the radio is listening, if it is: a scan the user asked for, devices waiting to
    /// reconnect, or both.
    pub(crate) bt_scan: tokio::sync::Mutex<crate::bluetooth::ScanPlan>,
}

/// The mutable state, kept behind one lock so configuration and runtime cannot disagree.
pub(crate) struct Inner {
    pub(crate) config: Configuration,
    pub(crate) runtime: HashMap<EndpointId, Runtime>,
    /// Traffic counts and the monitor feed for each running network session.
    ///
    /// Apart from `runtime` because a session's lifecycle belongs to its supervisor, and every
    /// port and device loop reads `runtime`. Without it, nothing a session carried was counted
    /// and `monitor` called a session carrying MIDI "not running".
    pub(crate) session_runtime: HashMap<EndpointId, Runtime>,
    pub(crate) events: EventLog,
    /// Live traffic counts per route, kept beside the configuration that defines them.
    ///
    /// Held here rather than in its own lock so a route can never be dispatched before its
    /// counters exist: the write that adds the route adds them in the same section.
    pub(crate) route_counters: HashMap<RouteId, Arc<RouteRuntime>>,
    /// Hardware unplugged while the daemon runs, so its return can be told apart from its first
    /// opening at startup and recorded.
    ///
    /// Each keeps the traffic counters it had, which carry on when it returns. Starting them
    /// again from zero said a device that had played all session had received nothing, beside a
    /// route from it that had carried every message.
    pub(crate) unplugged: HashMap<EndpointId, Arc<TrafficCounters>>,
    /// The automatic ports of network ports, keyed by the identifier each carries in the data
    /// path (FR-015h). Apart from `runtime` because none is an endpoint in the configuration.
    pub(crate) automatic_ports: HashMap<EndpointId, crate::automatic::AutomaticPort>,
}

impl Inner {
    /// Returns what counts and watches the traffic of an endpoint, a running network port, or a
    /// network port's automatic port.
    pub(crate) fn traffic(&self, id: EndpointId) -> Option<&Runtime> {
        self.runtime
            .get(&id)
            .or_else(|| self.session_runtime.get(&id))
            .or_else(|| self.automatic_ports.get(&id).map(|port| &port.runtime))
    }
}

/// One machine this daemon knows about, whether it is on the network now or only remembered.
#[derive(Clone, Debug)]
pub struct KnownPeer {
    /// Stable identity.
    pub id: midi_harbor_core::ids::PeerId,
    /// The name it advertises, or the one the user gave it.
    pub name: String,
    /// Where it can be reached, in `host:port` form.
    pub addresses: Vec<String>,
    /// Whether it is advertising itself on the network right now.
    pub discovered: bool,
    /// Whether invitations from it are accepted without asking.
    pub trusted: bool,
}

/// An invitation waiting on the user, and what is needed to answer it.
#[derive(Clone, Debug)]
pub struct PendingInvitation {
    /// Stable for as long as this invitation is waiting, so an answer names one invitation.
    pub id: String,
    /// The session the peer invited.
    pub session: EndpointId,
    /// Where the invitation came from.
    pub peer: SocketAddr,
    /// The name the peer advertises, when it sent one.
    pub peer_name: Option<String>,
    /// When it first arrived. A peer invites repeatedly, and this is the first of them.
    pub first_seen: jiff::Timestamp,
}

/// Reports whether two stored addresses name the same machine.
///
/// Compared by host alone: a machine is what the user trusts, and the port a session happens to
/// listen on is neither chosen by them nor stable across restarts.
fn same_host(left: &str, right: &str) -> bool {
    fn host(value: &str) -> String {
        value
            .parse::<SocketAddr>()
            .map(|address| address.ip().to_canonical().to_string())
            .or_else(|_| {
                value
                    .parse::<std::net::IpAddr>()
                    .map(|address| address.to_canonical().to_string())
            })
            .unwrap_or_else(|_| value.to_owned())
    }
    host(left) == host(right)
}

/// Reports whether a remembered `host:port` is the control address `address`, comparing
/// addresses in canonical form so an IPv4-mapped IPv6 address matches the IPv4 one.
fn same_address(held: &str, address: SocketAddr) -> bool {
    let canonical =
        |address: SocketAddr| SocketAddr::new(address.ip().to_canonical(), address.port());
    held.parse::<SocketAddr>()
        .is_ok_and(|held| canonical(held) == canonical(address))
}

/// Returns the remembered machine at `address`, adding one when none is.
///
/// Found by the address exactly as stored, as a connected peer always has been. A machine
/// added here is not trusted: connecting out to a machine is not the same as letting it in
/// unasked. The session name advertised at the address, when there is one, is kept with the
/// machine, so it can be followed when that session moves (R-105).
fn known_peer_at(
    config: &mut Configuration,
    address: SocketAddr,
    advertised_as: Option<String>,
) -> midi_harbor_core::ids::PeerId {
    let stored = address.to_string();
    if let Some(known) = config
        .peers
        .iter_mut()
        .find(|known| known.addresses.iter().any(|held| held == &stored))
    {
        if advertised_as.is_some() {
            known.advertised_as = advertised_as;
        }
        return known.id;
    }
    let peer = config::PeerConfig {
        id: midi_harbor_core::ids::PeerId::new(),
        name: address.ip().to_canonical().to_string(),
        addresses: vec![stored],
        trusted: false,
        advertised_as,
        key: None,
        port_id: None,
    };
    let id = peer.id;
    config.peers.push(peer);
    id
}

/// Returns the stored settings of the network session `id`, if it is one.
fn network_session_mut(
    config: &mut Configuration,
    id: EndpointId,
) -> Option<&mut NetworkSessionConfig> {
    match config.endpoints.iter_mut().find(|e| e.id == id) {
        Some(Endpoint {
            kind: EndpointKind::NetworkSession(session),
            ..
        }) => Some(session),
        _ => None,
    }
}

/// Drops the remembered machines that hold nothing but an address no network port connects to.
///
/// Connecting to a machine records it, so the network port can name its peer. Once the port lets
/// it go, a record the user neither named nor trusted is only an old connection, and listing it
/// offers a machine that may no longer be listening there.
fn drop_unused_peers(config: &mut Configuration) {
    let used: Vec<midi_harbor_core::ids::PeerId> = config
        .endpoints
        .iter()
        .filter_map(|endpoint| match &endpoint.kind {
            EndpointKind::NetworkSession(session) => Some(session),
            _ => None,
        })
        .flat_map(|session| session.peer.iter().chain(session.other_peers.iter()))
        .copied()
        .collect();
    config.peers.retain(|peer| {
        // Named after its own address, as a machine recorded by connecting to it is.
        let unnamed = peer
            .addresses
            .iter()
            .any(|held| same_host(held, &peer.name));
        peer.trusted || !unnamed || used.contains(&peer.id)
    });
}

/// Lists remembered machines and advertised sessions together, one entry per session.
///
/// An advertisement from a trusted machine marks that machine as present rather than adding a
/// second entry, which would make the trusted one look like a different machine. Trust belongs
/// to the host, so any session it advertises marks it. A machine remembered without trust is
/// only an address, so it is marked by the session advertised at that address and by no other:
/// folding another session into it listed that session under a port nothing listens on. Two
/// advertisements from one machine stay two entries: they are separate sessions, and folding the
/// second into the first left it impossible to find.
fn merge_known_peers(
    remembered: &[config::PeerConfig],
    discovered: Vec<(DiscoveredPeer, String)>,
) -> Vec<KnownPeer> {
    let mut known: Vec<KnownPeer> = remembered
        .iter()
        .map(|peer| KnownPeer {
            id: peer.id,
            name: peer.name.clone(),
            addresses: peer.addresses.clone(),
            discovered: false,
            trusted: peer.trusted,
        })
        .collect();
    let remembered_count = known.len();

    for (peer, label) in discovered {
        let address = peer.address();
        let existing = address.and_then(|address| {
            let advertised = address.to_string();
            known.iter_mut().take(remembered_count).find(|known| {
                known.addresses.iter().any(|held| {
                    if known.trusted {
                        same_host(held, &advertised)
                    } else {
                        same_address(held, address)
                    }
                })
            })
        });
        match existing {
            Some(entry) => entry.discovered = true,
            None => known.push(KnownPeer {
                id: peer.id,
                name: label,
                addresses: address.map(|a| vec![a.to_string()]).unwrap_or_default(),
                discovered: true,
                trusted: false,
            }),
        }
    }

    known.sort_by(|left, right| left.name.cmp(&right.name));
    known
}

/// What one route accumulates while it is carrying.
///
/// The notes are tracked per route rather than per endpoint because a destination can be fed by
/// several sources. When one of them goes away, only the notes that came along that route may be
/// stopped; silencing the whole destination would cut off whoever else is playing through it.
#[derive(Debug, Default)]
pub struct RouteRuntime {
    /// Live traffic counts for this route.
    pub counters: TrafficCounters,
    /// The notes this route has left sounding at its destination.
    pub sounding: std::sync::Mutex<Sounding>,
    /// Echoes seen on a route from one session to another, which is where a loop across
    /// machines closes.
    pub loops: std::sync::Mutex<midi_harbor_core::loops::LoopWatch>,
}

/// Brings route counters in step with the configured routes.
///
/// Counters for a route that is gone are dropped rather than kept, so a route deleted and made
/// again starts from zero, which is what someone who just remade it is looking at the screen to
/// see.
pub(crate) fn sync_route_counters(inner: &mut Inner) {
    let configured: HashSet<RouteId> = inner.config.routes.iter().map(RouteConfig::id).collect();
    inner.route_counters.retain(|id, _| configured.contains(id));
    for id in configured {
        inner
            .route_counters
            .entry(id)
            .or_insert_with(|| Arc::new(RouteRuntime::default()));
    }
}

/// Carries route counters across a change that rewrites the names routes are made of.
///
/// A route's identity is derived from the pair of names it joins, so a rename changes it.
/// Re-keying positionally keeps a renamed route's history instead of making a rename read as a
/// reset, for the same reason a rename rewrites routes rather than dropping them.
fn move_route_counters(inner: &mut Inner, before: &[RouteId]) {
    let after: Vec<RouteId> = inner.config.routes.iter().map(RouteConfig::id).collect();
    let mut moved: HashMap<RouteId, Arc<RouteRuntime>> = HashMap::with_capacity(after.len());
    for (old, new) in before.iter().zip(after) {
        if let Some(counters) = inner.route_counters.remove(old) {
            moved.insert(new, counters);
        }
    }
    inner.route_counters = moved;
    sync_route_counters(inner);
}

impl Daemon {
    /// Loads configuration and brings up everything it describes except the radio.
    ///
    /// Bluetooth is left out because opening it is a side effect on the machine — a scan costs
    /// power on every device in range — and this is the entry point tests use. The binary calls
    /// `start_with_bluetooth` with a real backend; anything here reports both Bluetooth roles as
    /// unavailable, which is true of a daemon that never opened one.
    pub async fn start(
        paths: Paths,
        midi: Arc<dyn MidiPlatform>,
    ) -> Result<Arc<Self>, DaemonError> {
        Self::start_with(paths, midi, Arc::new(PolledSystemEvents::start())).await
    }

    /// Loads configuration and brings up every endpoint, watching `system` for machine changes.
    ///
    /// Separate from `start` so the source of machine events can be supplied, which is the only
    /// way to exercise a resume without suspending the machine running the test.
    pub async fn start_with(
        paths: Paths,
        midi: Arc<dyn MidiPlatform>,
        system: Arc<dyn SystemEvents>,
    ) -> Result<Arc<Self>, DaemonError> {
        Self::start_with_bluetooth(paths, midi, system, Arc::new(FakeBluetoothPlatform::new()))
            .await
    }

    /// Loads configuration and brings up every endpoint, over the given platform backends.
    ///
    /// The radio is supplied separately because it is the one seam that cannot be exercised on a
    /// build machine at all, so every test that is not about Bluetooth passes the in-memory one.
    pub async fn start_with_bluetooth(
        paths: Paths,
        midi: Arc<dyn MidiPlatform>,
        system: Arc<dyn SystemEvents>,
        bluetooth: Arc<dyn BluetoothPlatform>,
    ) -> Result<Arc<Self>, DaemonError> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let outcome = config::load(&paths)?;

        let mut events = EventLog::default();
        let now = clock.now();
        let _ = events.record(events::event(
            EventKind::DaemonStarted,
            Severity::Info,
            now,
            "daemon started",
        ));

        // A configuration that could not be read is reported rather than passed over, because the
        // user's setup appearing to vanish is exactly the surprise FR-051 exists to prevent.
        if let Some(repair) = &outcome.repaired {
            warn!(
                preserved = %repair.preserved_at.display(),
                reason = %repair.reason,
                "configuration could not be read and was preserved; starting from defaults"
            );
            let _ = events.record(events::event(
                EventKind::ConfigurationChanged,
                Severity::Error,
                now,
                format!(
                    "configuration was unreadable and was preserved at {}: {}",
                    repair.preserved_at.display(),
                    repair.reason
                ),
            ));
        }

        // A file edited by hand can give two ports one name, which creating and renaming refuse.
        // Both are kept, since dropping either would lose the user's setup, and the clash is
        // reported so it can be fixed by renaming one.
        for (first, second) in midi_harbor_core::endpoint::name_clashes(&outcome.config.endpoints) {
            let detail = config::clash_message(first, second);
            error!(clash = %detail, "configuration gives two endpoints one name");
            let _ = events.record(events::event(
                EventKind::ConfigurationChanged,
                Severity::Error,
                now,
                format!("{detail}; rename one so they can be told apart"),
            ));
        }

        let (changes, _) = broadcast::channel(STATE_CHANNEL_CAPACITY);
        // Discovery failing is not fatal: peers can still be added by address, and the capability
        // query reports honestly that browsing is unavailable.
        // A daemon that cannot keep its key still runs. It proves itself until it restarts, and
        // is a stranger to every machine after.
        let identity = Arc::new(Identity::load_or_create(&paths).unwrap_or_else(|error| {
            error!(error = %error, "failed to keep this daemon's key; using one for this run");
            Identity::generate()
        }));
        let discovery = match Discovery::start(&identity.public_key()) {
            Ok(discovery) => Some(discovery),
            Err(error) => {
                warn!(error = %error, "network discovery unavailable; peers must be added by address");
                None
            }
        };

        let service_unavailable = match midi_harbor_service::detect() {
            Ok(_) => None,
            // The App Store build has a service of sorts, its app, but nothing to install.
            Err(midi_harbor_service::ServiceError::AppStore) => {
                Some(midi_harbor_core::capability::UnavailableReason::NotBuilt)
            }
            Err(midi_harbor_service::ServiceError::NoServiceManager) => Some(
                midi_harbor_core::capability::UnavailableReason::MissingSystemComponent {
                    component: "systemd".to_owned(),
                },
            ),
            Err(error) => Some(
                midi_harbor_core::capability::UnavailableReason::MissingSystemComponent {
                    component: format!("a service manager ({error})"),
                },
            ),
        };

        let daemon = Arc::new(Self {
            paths,
            clock,
            midi,
            bluetooth,
            inner: RwLock::new(Inner {
                route_counters: outcome
                    .config
                    .routes
                    .iter()
                    .map(|route| (route.id(), Arc::new(RouteRuntime::default())))
                    .collect(),
                config: outcome.config,
                runtime: HashMap::new(),
                unplugged: HashMap::new(),
                session_runtime: HashMap::new(),
                automatic_ports: HashMap::new(),
                events,
            }),
            changes,
            started_at: now,
            midi_server_replaced_at: RwLock::new(None),
            midi_server_lost_at: RwLock::new(None),
            sessions: RwLock::new(HashMap::new()),
            invitations: RwLock::new(HashMap::new()),
            accepted_once: RwLock::new(Vec::new()),
            refreshing: tokio::sync::Mutex::new(()),
            restart: tokio::sync::Notify::new(),
            stop: tokio::sync::Notify::new(),
            discovery,
            identity,
            following: tokio::sync::Mutex::new(()),
            service_unavailable,
            host_name: default_machine_name(),
            bt_links: RwLock::new(HashMap::new()),
            bt_seen: RwLock::new(Vec::new()),
            bt_advertised: RwLock::new(None),
            bt_centrals: std::sync::atomic::AtomicUsize::new(0),
            bt_deferred: RwLock::new(HashSet::new()),
            bt_scan: tokio::sync::Mutex::new(crate::bluetooth::ScanPlan::default()),
        });

        daemon.reconcile().await;
        daemon.seed_bluetooth_states().await;
        daemon.refresh_devices().await;
        daemon.start_configured_sessions().await;
        daemon.watch_system_events(system);
        daemon.watch_midi_environment();
        daemon.watch_bluetooth();
        daemon.watch_retries();
        daemon.watch_discovery();
        daemon.watch_traffic_log();
        Ok(daemon)
    }

    /// Watches for hardware arriving and leaving, and brings the endpoint list into line.
    ///
    /// The backend reports these from its own thread and holds them until they are taken. Nothing
    /// took them, so hardware attached while the daemon ran never appeared and hardware removed
    /// stayed listed — the enumeration only ever ran at startup.
    fn watch_midi_environment(self: &Arc<Self>) {
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(DEVICE_EVENT_INTERVAL);
            loop {
                ticker.tick().await;
                let events = daemon.midi.drain_events();
                if events.is_empty() {
                    continue;
                }
                // Once the MIDI service is gone nothing here can reach it again, so there is
                // nothing to refresh: the process is replaced instead.
                if events.contains(&MidiPlatformEvent::ServerLost) {
                    error!(
                        "the MIDI server stopped answering; restarting the daemon to reach it again"
                    );
                    *daemon.midi_server_lost_at.write().await = Some(daemon.clock.now());
                    daemon.restart.notify_one();
                    return;
                }
                // What else changed is not worth inspecting: every one of these means the list of
                // attached hardware may differ from what is recorded, and re-enumerating is how
                // that is settled.
                daemon.refresh_devices().await;
            }
        });
    }

    /// Resolves when this process can no longer do its job and has to be replaced by a new one.
    pub async fn restart_requested(&self) {
        self.restart.notified().await;
    }

    /// Asks the daemon to stop through its graceful shutdown.
    ///
    /// Held until it is waited for, so a request that arrives before serving begins is not lost.
    pub fn request_stop(&self) {
        info!("asked to stop by a client");
        self.stop.notify_one();
    }

    /// Resolves when a client has asked the daemon to stop.
    pub async fn stop_requested(&self) {
        self.stop.notified().await;
    }

    /// Records that this process replaced one that lost the MIDI service at `lost_at`, or now
    /// when the process that noticed could not say.
    ///
    /// The event history lives in memory, so the process that noticed cannot leave the record;
    /// the one that replaced it says why it started, dated when the loss was found rather than
    /// when this process started, which can be half a minute later. The time is kept for the
    /// status too, since this daemon recovered but other applications using MIDI may not have,
    /// and nothing here can tell which: the warning stands until someone dismisses it.
    pub async fn note_replaced_after_midi_server_lost(&self, lost_at: Option<jiff::Timestamp>) {
        let at = lost_at.unwrap_or_else(|| self.clock.now());
        *self.midi_server_replaced_at.write().await = Some(at);
        let _ = self.inner.write().await.events.record(events::event(
            EventKind::MidiServerReplaced,
            Severity::Warning,
            at,
            "the MIDI service stopped and was restarted; Midi Harbor recovered, but other apps may \
             have lost their MIDI connection and need relaunching",
        ));
    }

    /// Returns when the MIDI service was found gone, while the warning about it stands.
    pub async fn midi_server_replaced_at(&self) -> Option<jiff::Timestamp> {
        *self.midi_server_replaced_at.read().await
    }

    /// Clears the warning that the MIDI service stopped, once someone has seen it, for every
    /// client at once; returns whether there was one.
    pub async fn dismiss_midi_server_warning(&self) -> bool {
        let dismissed = self.midi_server_replaced_at.write().await.take().is_some();
        if dismissed {
            info!("the warning that the MIDI service stopped was dismissed");
        }
        dismissed
    }

    /// Returns when this process found the MIDI service gone, if it did, for the process that
    /// replaces it.
    pub async fn midi_server_lost_at(&self) -> Option<jiff::Timestamp> {
        *self.midi_server_lost_at.read().await
    }

    /// Watches for machine-level changes and retries every waiting session when one arrives.
    ///
    /// The backoff reaches thirty seconds, so a session that was retrying when the lid closed
    /// waits up to that long after the lid opens. Recovery never depends on this — the sources
    /// are unreliable by nature — but the difference is between resuming when the machine wakes
    /// and resuming half a minute later.
    fn watch_system_events(self: &Arc<Self>, system: Arc<dyn SystemEvents>) {
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SYSTEM_EVENT_INTERVAL);
            let urgent = system.urgent();
            // When the last suspend was announced, until the machine is known to be back.
            // NetworkManager takes the network down as the machine goes, which reads as an address
            // change, and acting on it reconnected the sessions just ended for sleep (R-072). A
            // suspend that never happens is forgotten after as long awake as the sessions wait.
            let mut asleep: Option<std::time::Instant> = None;
            loop {
                match &urgent {
                    Some(urgent) => {
                        tokio::select! {
                            _ = ticker.tick() => {}
                            () = urgent.notified() => {}
                        }
                    }
                    None => {
                        ticker.tick().await;
                    }
                }
                let reported = system.drain_events();

                // A suspend still to come is the last chance to release what is held on other
                // machines: once this one sleeps, they hear nothing until their liveness check
                // gives up. One already followed by its resume is over, and silencing then could
                // cut a note someone has just started playing.
                let last_suspend = reported
                    .iter()
                    .rposition(|event| matches!(event, SystemEvent::Suspending));
                let last_resume = reported
                    .iter()
                    .rposition(|event| matches!(event, SystemEvent::Resumed));
                if let Some(suspend) = last_suspend
                    && last_resume.is_none_or(|resume| resume < suspend)
                {
                    daemon.silence_before_sleep().await;
                    daemon.suspend_sessions().await;
                    system.ready_for_sleep();
                    asleep = Some(std::time::Instant::now());
                    continue;
                }

                let woke = reported
                    .iter()
                    .any(|event| matches!(event, SystemEvent::Resumed));
                let moved = reported
                    .iter()
                    .any(|event| matches!(event, SystemEvent::NetworkChanged));
                let going = asleep.is_some_and(|since| since.elapsed() < RESUME_AFTER_SLEEP);
                if woke {
                    asleep = None;
                } else if !moved || going {
                    continue;
                }

                // A reconnection that happens on its own reads as a spontaneous dropout unless
                // the history says what prompted it, so the detail names which one it was.
                let detail = match (woke, moved) {
                    (true, true) => "the machine woke and its addresses changed; reconnecting",
                    (true, false) => "the machine woke; reconnecting rather than waiting",
                    (false, _) => "this machine's addresses changed; reconnecting",
                };

                let now = daemon.clock.now();
                {
                    let mut inner = daemon.inner.write().await;
                    let _ = inner.events.record(events::event(
                        EventKind::SystemResumed,
                        Severity::Info,
                        now,
                        detail,
                    ));
                }
                for session in daemon.sessions.read().await.values() {
                    let _ = session.nudge().await;
                }
            }
        });
    }

    /// Announces every running session, or withdraws every announcement, to match the
    /// preference, without restarting any session.
    pub(crate) async fn apply_session_advertising(&self, announce: bool) {
        let Some(discovery) = &self.discovery else {
            return;
        };
        let names: HashMap<EndpointId, String> = self
            .read(|config, _| {
                config
                    .endpoints
                    .iter()
                    .filter_map(|endpoint| match &endpoint.kind {
                        EndpointKind::NetworkSession(session) => {
                            Some((endpoint.id, session.local_name.as_str().to_owned()))
                        }
                        _ => None,
                    })
                    .collect()
            })
            .await;
        let running: Vec<(EndpointId, u16)> = self
            .sessions
            .read()
            .await
            .iter()
            .map(|(id, session)| (*id, session.control_port()))
            .collect();
        for (id, port) in running {
            let Some(name) = names.get(&id) else {
                continue;
            };
            if announce {
                if let Err(error) = discovery.advertise(name, port, id) {
                    warn!(endpoint = %name, error = %error, "could not advertise session");
                }
            } else {
                discovery.withdraw(name);
            }
        }
        info!(announce, "network session advertising changed");
    }

    /// Returns the session names this machine is announcing on the network now.
    pub fn announced_sessions(&self) -> Vec<String> {
        self.discovery
            .as_ref()
            .map(|discovery| discovery.announcing())
            .unwrap_or_default()
    }

    /// Reports whether network discovery started, for the capability query.
    pub(crate) fn discovery_started(&self) -> bool {
        self.discovery.is_some()
    }

    /// Returns when the daemon started, for status reporting.
    pub fn started_at(&self) -> jiff::Timestamp {
        self.started_at
    }

    /// Returns the resolved paths this daemon is using.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Subscribes to state changes.
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.changes.subscribe()
    }

    /// Brings the platform into line with the configuration.
    ///
    /// Called at startup and after any configuration change. Only endpoints whose configuration
    /// actually differs are touched, so editing one port does not interrupt the others.
    pub async fn reconcile(self: &Arc<Self>) {
        let mut inner = self.inner.write().await;
        let now = self.clock.now();

        let wanted: Vec<Endpoint> = inner.config.virtual_ports().cloned().collect();
        let mut learned_identifiers = false;

        for endpoint in wanted {
            if inner.runtime.contains_key(&endpoint.id) {
                continue;
            }
            let state = if endpoint.enabled {
                match self.open_virtual_port(&endpoint) {
                    Ok(OpenedPort {
                        handle,
                        ids,
                        consumers,
                    }) => {
                        // The platform assigns identifiers the first time a port is created.
                        // Storing them is what makes other applications recognise the same port
                        // after a restart instead of treating it as a new one.
                        if record_platform_ids(&mut inner.config, endpoint.id, &ids) {
                            learned_identifiers = true;
                        }
                        info!(endpoint = %endpoint.name, "virtual port opened");
                        let _ = inner.events.record(events::event(
                            EventKind::EndpointStateChanged,
                            Severity::Info,
                            now,
                            format!("{} connected", endpoint.name),
                        ));
                        let mut state = ConnectionState::enabled(now);
                        let _ = state.apply_now(
                            midi_harbor_core::state::Event::Attempting,
                            self.clock.as_ref(),
                        );
                        let _ = state.apply_now(
                            midi_harbor_core::state::Event::Established,
                            self.clock.as_ref(),
                        );
                        inner
                            .runtime
                            .insert(endpoint.id, Runtime::new(state, Some(handle)));
                        // Draining begins as soon as the port is open, so MIDI arriving before
                        // any route exists is discarded by the ring rather than queued forever.
                        for consumer in consumers {
                            self.start_dispatch(consumer);
                        }
                        continue;
                    }
                    Err(reason) => {
                        warn!(endpoint = %endpoint.name, error = %reason, "virtual port failed to open");
                        // Recorded as well as logged: the recovery is recorded, and a history
                        // that shows a port coming back without ever showing it fail explains
                        // nothing.
                        let mut failed = events::event(
                            EventKind::EndpointStateChanged,
                            Severity::Error,
                            now,
                            format!("{} could not open: {reason}", endpoint.name),
                        );
                        failed.endpoint = Some(endpoint.id);
                        let _ = inner.events.record(failed);
                        let mut state = ConnectionState::enabled(now);
                        let _ = state.apply_now(
                            midi_harbor_core::state::Event::Attempting,
                            self.clock.as_ref(),
                        );
                        let _ = state.apply_now(
                            midi_harbor_core::state::Event::Failed(reason),
                            self.clock.as_ref(),
                        );
                        state
                    }
                }
            } else {
                ConnectionState::disabled(now)
            };
            inner.runtime.insert(endpoint.id, Runtime::new(state, None));
        }

        // Persist any identifier learned on first creation, so the next start can pin it.
        if learned_identifiers && let Err(error) = config::save(&self.paths, &inner.config) {
            warn!(error = %error, "could not persist platform identifiers; ports may change identity on restart");
        }
        drop(inner);

        // Hardware is reconciled alongside virtual ports. Leaving this to device refresh alone
        // meant a device switched off and on again stayed shut until the next hot-plug or
        // restart, while still listing as attached and accepting routes.
        self.open_present_devices().await;
    }

    /// Opens one virtual port on the platform, with every connector it has.
    ///
    /// Each MIDI In connector gets a ring of its own: the platform is handed the producing halves,
    /// and the consuming halves are returned for the dispatch loop to drain. One ring per
    /// connector is what keeps a dump arriving on one from being interleaved with another's.
    pub(crate) fn open_virtual_port(
        &self,
        endpoint: &Endpoint,
    ) -> Result<OpenedPort, FailureReason> {
        let port = match &endpoint.kind {
            EndpointKind::VirtualPort(port) => port.clone(),
            _ => VirtualPort::default(),
        };
        let spec = VirtualPortSpec {
            name: endpoint.name.as_str().to_owned(),
            inputs: port.inputs,
            outputs: port.outputs,
            pinned_inputs: port.input_ids,
            pinned_outputs: port.output_ids,
        };
        let (producers, consumers): (Vec<RtProducer>, Vec<RtConsumer>) = (0..spec.inputs)
            .map(|connector| dataplane::connector_channel(endpoint.id, connector))
            .unzip();
        let (handle, ids) = self
            .midi
            .create_virtual_port(&spec, producers)
            .map_err(|error| error.as_failure_reason())?;
        Ok(OpenedPort {
            handle,
            ids,
            consumers,
        })
    }

    /// Runs `f` against the configuration and runtime together.
    pub async fn read<T>(
        &self,
        f: impl FnOnce(&Configuration, &HashMap<EndpointId, Runtime>) -> T,
    ) -> T {
        let inner = self.inner.read().await;
        f(&inner.config, &inner.runtime)
    }

    /// Returns a copy of the recent event history.
    pub async fn events(&self, after: Option<u64>, limit: usize) -> Vec<events::Event> {
        let inner = self.inner.read().await;
        inner
            .events
            .since(after.map(midi_harbor_core::ids::EventId::from_raw), limit)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Changes how many MIDI In and MIDI Out connectors a virtual port has, each kept between one
    /// and sixteen.
    ///
    /// Routes on connectors the port no longer has are removed with the notes they held
    /// silenced, rather than left waiting for connectors nobody is going to add back. The port is
    /// reopened with its new connectors, keeping the identifiers of those it still has.
    pub async fn set_virtual_port_connectors(
        self: &Arc<Self>,
        target: &str,
        inputs: u8,
        outputs: u8,
    ) -> Result<Endpoint, DaemonError> {
        let (inputs, outputs) = (
            inputs.clamp(1, midi_harbor_core::endpoint::MAX_CONNECTORS),
            outputs.clamp(1, midi_harbor_core::endpoint::MAX_CONNECTORS),
        );
        let (id, orphaned) = {
            let inner = self.inner.read().await;
            let endpoint = find_endpoint(&inner.config, target)?;
            if !matches!(endpoint.kind, EndpointKind::VirtualPort(_)) {
                return Err(FailureReason::ConfigInvalid {
                    detail: format!("{} is not a virtual port", endpoint.name),
                }
                .into());
            }
            let orphaned: Vec<String> = inner
                .config
                .routes
                .iter()
                .filter(|route| {
                    (route.comes_from(endpoint) && route.from_index() >= inputs)
                        || (route.goes_to(endpoint) && route.to_index() >= outputs)
                })
                .map(|route| route.id().to_string())
                .collect();
            (endpoint.id, orphaned)
        };
        for route in orphaned {
            self.delete_route(&route).await?;
        }

        {
            let mut inner = self.inner.write().await;
            let Some(endpoint) = inner.config.endpoints.iter_mut().find(|e| e.id == id) else {
                return Err(FailureReason::DeviceRemoved.into());
            };
            if let EndpointKind::VirtualPort(port) = &mut endpoint.kind {
                port.inputs = inputs;
                port.outputs = outputs;
                port.input_ids.truncate(usize::from(inputs));
                port.output_ids.truncate(usize::from(outputs));
            }
            config::save(&self.paths, &inner.config)?;
        }

        // Closed and opened again, since the platform cannot add or remove a port's connectors
        // in place. What was sounding through it is silenced first, as for any restart.
        self.silence_endpoint(id).await;
        {
            let mut inner = self.inner.write().await;
            if let Some(runtime) = inner.runtime.remove(&id)
                && let Some(handle) = runtime.handle
            {
                let _ = self.midi.destroy_virtual_port(handle);
            }
        }
        self.reconcile().await;
        let _ = self.changes.send(Change::EndpointChanged(id));
        info!(endpoint = %target, inputs, outputs, "virtual port connectors changed");
        let inner = self.inner.read().await;
        inner
            .config
            .endpoints
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .ok_or_else(|| FailureReason::DeviceRemoved.into())
    }

    /// Creates a virtual port with the given MIDI In and MIDI Out connector counts, persists it,
    /// and opens it.
    pub async fn create_virtual_port(
        self: &Arc<Self>,
        name: &str,
        inputs: u8,
        outputs: u8,
    ) -> Result<Endpoint, DaemonError> {
        let name = EndpointName::new(name)?;
        let mut inner = self.inner.write().await;

        if midi_harbor_core::endpoint::name_conflicts(
            &inner.config.endpoints,
            &name,
            "virtual",
            None,
        ) {
            return Err(FailureReason::NameConflict {
                name: name.as_str().to_owned(),
            }
            .into());
        }

        let endpoint = Endpoint::new(
            name,
            EndpointKind::VirtualPort(VirtualPort::with_connectors(inputs, outputs)),
        );
        inner.config.add_endpoint(endpoint.clone());
        config::save(&self.paths, &inner.config)?;
        drop(inner);

        self.reconcile().await;

        // A new port the system would not create is refused rather than kept. Nothing depends on
        // it yet, and keeping it had `port create` report a port no application could see,
        // retrying against a limit only deleting ports can lift.
        let refused = self
            .inner
            .read()
            .await
            .runtime
            .get(&endpoint.id)
            .filter(|runtime| runtime.handle.is_none())
            .and_then(|runtime| runtime.state.last_error().cloned());
        if let Some(reason) = refused {
            let mut inner = self.inner.write().await;
            inner.config.endpoints.retain(|e| e.id != endpoint.id);
            inner.runtime.remove(&endpoint.id);
            config::save(&self.paths, &inner.config)?;
            return Err(reason.into());
        }

        let _ = self.changes.send(Change::EndpointAdded(endpoint.id));
        info!(endpoint = %endpoint.name, "virtual port created");
        Ok(endpoint)
    }

    /// Deletes a virtual port, silencing it first and reporting the routes it orphaned.
    pub async fn delete_virtual_port(
        self: &Arc<Self>,
        id: EndpointId,
    ) -> Result<Vec<String>, DaemonError> {
        // Silenced first, so nothing is left sounding on the far side. It has to happen before
        // the lock is taken, because the port is still open only until the teardown below.
        self.silence_endpoint(id).await;
        // Its routes stop carrying with it, and with them the only source of the note offs for
        // whatever it was playing, so those are stopped now while the routes still resolve.
        self.silence_routes_from(id).await;

        let mut inner = self.inner.write().await;
        let Some(index) = inner.config.endpoints.iter().position(|e| e.id == id) else {
            return Err(DaemonError::NotFound(id.to_string()));
        };
        let endpoint = inner.config.endpoints.remove(index);

        if let Some(runtime) = inner.runtime.remove(&id)
            && let Some(handle) = runtime.handle
            && let Err(error) = self.midi.destroy_virtual_port(handle)
        {
            warn!(endpoint = %endpoint.name, error = %error, "could not close virtual port");
        }

        let orphaned: Vec<String> = inner
            .config
            .routes
            .iter()
            .filter(|route| route.touches(&endpoint))
            .map(|route| format!("{} -> {}", route.from, route.to))
            .collect();

        // A network port takes the machines only it connected to with it.
        drop_unused_peers(&mut inner.config);
        config::save(&self.paths, &inner.config)?;
        drop(inner);

        let _ = self.changes.send(Change::EndpointRemoved(id));
        info!(endpoint = %endpoint.name, "virtual port deleted");
        Ok(orphaned)
    }

    /// Forgets remembered hardware, reporting the routes it orphans.
    ///
    /// Hardware that is still attached is rediscovered immediately afterwards as something never
    /// seen before, which is what makes this the way to start over with a device whose settings
    /// have gone wrong. The caller is told so it can say so.
    pub async fn forget_device(
        self: &Arc<Self>,
        id: EndpointId,
    ) -> Result<(Vec<String>, bool), DaemonError> {
        // Silenced while it is still open, for the same reason a port is: whatever it was
        // playing is about to have nothing left to stop it.
        self.silence_endpoint(id).await;
        self.silence_routes_from(id).await;

        let (name, present, orphaned) = {
            let mut inner = self.inner.write().await;
            let Some(index) = inner.config.endpoints.iter().position(|e| e.id == id) else {
                return Err(DaemonError::NotFound(id.to_string()));
            };
            let EndpointKind::PhysicalDevice(device) = &inner
                .config
                .endpoints
                .get(index)
                .map(|endpoint| endpoint.kind.clone())
                .ok_or_else(|| DaemonError::NotFound(id.to_string()))?
            else {
                return Err(DaemonError::Failure(FailureReason::ConfigInvalid {
                    detail: "only hardware can be forgotten; ports are deleted".to_owned(),
                }));
            };
            let present = device.present;
            let endpoint = inner.config.endpoints.remove(index);

            if let Some(runtime) = inner.runtime.remove(&id)
                && let Some(handle) = runtime.handle
                && let Err(error) = self.midi.close_device(handle)
            {
                debug!(device = %endpoint.name, error = %error, "could not close forgotten hardware");
            }

            let orphaned: Vec<String> = inner
                .config
                .routes
                .iter()
                .filter(|route| route.touches(&endpoint))
                .map(|route| format!("{} -> {}", route.from, route.to))
                .collect();

            config::save(&self.paths, &inner.config)?;
            (endpoint.name.to_string(), present, orphaned)
        };

        let _ = self.changes.send(Change::EndpointRemoved(id));
        info!(device = %name, present, "hardware forgotten");

        // Hardware that is still attached is re-enumerated at once. Nothing changed on the
        // platform, so no event will prompt it, and leaving the list without a device that is
        // plugged in would be wrong until something unrelated happened to refresh it.
        if present {
            self.refresh_devices().await;
        }
        Ok((orphaned, present))
    }

    /// Binds a stored device entry to one particular piece of attached hardware.
    ///
    /// Two identical devices cannot be told apart by anything but where they are plugged in, so
    /// this is the user saying which one they meant. The stored entry takes on the chosen
    /// hardware's fingerprint, and the separate entry that was standing in for it goes, so the
    /// routes bound to the stored name follow the device the user picked.
    pub async fn resolve_device(
        self: &Arc<Self>,
        id: EndpointId,
        chosen: &DeviceFingerprint,
    ) -> Result<Endpoint, DaemonError> {
        let resolved = {
            let mut inner = self.inner.write().await;

            // The chosen hardware currently has an entry of its own, which would otherwise match
            // the adopted fingerprint just as well and leave the pair ambiguous all over again.
            if let Some(index) = inner.config.endpoints.iter().position(|endpoint| {
                endpoint.id != id
                    && matches!(&endpoint.kind, EndpointKind::PhysicalDevice(device)
                        if device.fingerprint.compare(chosen) != MatchConfidence::None)
            }) {
                let _ = inner.config.endpoints.remove(index);
            }

            let Some(endpoint) = inner.config.endpoints.iter_mut().find(|e| e.id == id) else {
                return Err(DaemonError::NotFound(id.to_string()));
            };
            let EndpointKind::PhysicalDevice(device) = &mut endpoint.kind else {
                return Err(DaemonError::Failure(FailureReason::ConfigInvalid {
                    detail: "only hardware can be ambiguous".to_owned(),
                }));
            };
            device.fingerprint = chosen.clone();
            device.confidence = MatchConfidence::Exact;
            let resolved = endpoint.clone();
            config::save(&self.paths, &inner.config)?;
            resolved
        };

        self.refresh_devices().await;
        let _ = self.changes.send(Change::EndpointChanged(id));
        info!(endpoint = %resolved.name, "device ambiguity resolved");
        Ok(resolved)
    }

    /// Renames an endpoint, rewriting every route that names it.
    pub async fn rename_endpoint(
        self: &Arc<Self>,
        id: EndpointId,
        new_name: &str,
        confirm: bool,
    ) -> Result<Endpoint, DaemonError> {
        let new_name = EndpointName::new(new_name)?;
        let mut inner = self.inner.write().await;

        let Some(current) = inner.config.endpoints.iter().find(|e| e.id == id).cloned() else {
            return Err(DaemonError::NotFound(id.to_string()));
        };
        if midi_harbor_core::endpoint::name_conflicts(
            &inner.config.endpoints,
            &new_name,
            current.kind.slug(),
            Some(id),
        ) {
            return Err(FailureReason::NameConflict {
                name: new_name.as_str().to_owned(),
            }
            .into());
        }
        // Other applications address a port by name, so a rename makes them lose it.
        if !confirm {
            return Err(DaemonError::ConfirmationRequired {
                detail: format!(
                    "applications connected to '{}' may need to reselect it after the rename",
                    current.name
                ),
            });
        }

        let route_ids: Vec<RouteId> = inner.config.routes.iter().map(RouteConfig::id).collect();
        inner.config.rename_endpoint(id, new_name);
        move_route_counters(&mut inner, &route_ids);
        config::save(&self.paths, &inner.config)?;
        let renamed = inner
            .config
            .endpoints
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .ok_or_else(|| DaemonError::NotFound(id.to_string()))?;
        drop(inner);

        // A virtual port's name is its name on the platform, and renaming only the configuration
        // left other applications seeing the old one until the daemon restarted. The port is
        // created again under the new name with the same platform identifier, which is what
        // applications that remember a port by identifier reconnect to.
        if matches!(renamed.kind, EndpointKind::VirtualPort(_)) {
            self.take_down(&current).await;
            self.reconcile().await;
        }
        // A network port's automatic port carries its name, so it is opened again under the new
        // one, keeping its identifiers.
        self.settle_automatic_port(&renamed, true).await;

        let _ = self.changes.send(Change::EndpointChanged(id));
        Ok(renamed)
    }

    /// Switches an endpoint on or off without deleting its configuration.
    pub async fn set_enabled(
        self: &Arc<Self>,
        id: EndpointId,
        enabled: bool,
    ) -> Result<Endpoint, DaemonError> {
        // The advertised Bluetooth endpoint's switch is the advertising switch. Treating it as
        // an ordinary endpoint left the two disagreeing about whether this machine advertises.
        let advertised = self
            .read(|config, _| {
                config.endpoint(id).is_some_and(|endpoint| {
                    matches!(
                        &endpoint.kind,
                        EndpointKind::BluetoothDevice(device)
                            if device.role == midi_harbor_core::endpoint::BleRole::Peripheral
                    )
                })
            })
            .await;
        if advertised {
            self.set_peripheral_advertising(enabled, None).await?;
            return self
                .read(|config, _| config.endpoint(id).cloned())
                .await
                .ok_or_else(|| DaemonError::NotFound(id.to_string()));
        }

        // Silenced before the lock is taken, because silencing has to go out through the port
        // while it is still open, and the teardown below closes it.
        if !enabled {
            self.silence_endpoint(id).await;
            // A source switched off suspends its routes, and nothing else will send the note
            // offs for what it was playing.
            self.silence_routes_from(id).await;
        }

        let mut inner = self.inner.write().await;
        let Some(endpoint) = inner.config.endpoints.iter_mut().find(|e| e.id == id) else {
            return Err(DaemonError::NotFound(id.to_string()));
        };
        endpoint.enabled = enabled;
        let updated = endpoint.clone();

        // Tear the runtime down so reconcile rebuilds it in the requested state. Hardware is
        // closed rather than destroyed: the device belongs to the system, and only the ports
        // opened onto it are ours to release.
        let physical = matches!(updated.kind, EndpointKind::PhysicalDevice(_));
        if let Some(runtime) = inner.runtime.remove(&id)
            && let Some(handle) = runtime.handle
        {
            let _ = if physical {
                self.midi.close_device(handle)
            } else {
                self.midi.destroy_virtual_port(handle)
            };
        }
        config::save(&self.paths, &inner.config)?;
        drop(inner);

        self.reconcile().await;
        // A session runs in its own supervisor rather than the runtime map, so reconciling does
        // not reach it. Without this a disabled session went on listening, advertising and
        // carrying MIDI, and one enabled after startup never started.
        if let EndpointKind::NetworkSession(session) = &updated.kind {
            if enabled {
                let running = self.sessions.read().await.contains_key(&id);
                if !running && let Err(error) = self.start_session(&updated).await {
                    warn!(endpoint = %updated.name, error = %error, "could not start network session");
                }
            } else {
                self.stop_session(id, session.local_name.as_str()).await;
            }
        }
        let _ = self.changes.send(Change::EndpointChanged(id));
        Ok(updated)
    }

    /// Stops a session's supervisor and withdraws its advertisement.
    ///
    /// Notes this machine sent are stopped at the peer before the session ends, because once it
    /// has ended nothing can reach the peer to stop them.
    pub(crate) async fn stop_session(self: &Arc<Self>, id: EndpointId, local_name: &str) {
        self.close_automatic_port(id).await;
        let Some(session) = self.sessions.write().await.remove(&id) else {
            return;
        };
        let _ = self.inner.write().await.session_runtime.remove(&id);
        let silence: Vec<MidiMessage> = Channel::all()
            .flat_map(midi_harbor_core::midi::silence_channel)
            .collect();
        let _ = session.send(silence).await;
        session.shutdown().await;
        if let Some(discovery) = &self.discovery {
            discovery.withdraw(local_name);
        }
        info!(endpoint = %local_name, "network session stopped");
    }

    /// Resolves an endpoint by identifier or by name.
    ///
    /// An ambiguous name is refused rather than resolved arbitrarily, because acting on the wrong
    /// endpoint is worse than making the user be specific.
    pub async fn resolve(&self, reference: &str) -> Result<EndpointId, DaemonError> {
        let inner = self.inner.read().await;
        find_endpoint(&inner.config, reference).map(|endpoint| endpoint.id)
    }

    /// Returns how many endpoints are currently carrying MIDI.
    ///
    /// Counts network sessions as well as virtual ports. Their lifecycle lives in their own
    /// supervisors rather than the runtime map, so counting only the map reports every connected
    /// session as disconnected.
    pub async fn connected_count(&self) -> usize {
        let ports = {
            let inner = self.inner.read().await;
            inner
                .runtime
                .values()
                .filter(|runtime| runtime.state.phase() == ConnectionPhase::Connected)
                .count()
        };

        let running: Vec<Arc<NetworkSession>> = {
            let sessions = self.sessions.read().await;
            sessions.values().map(Arc::clone).collect()
        };
        let mut sessions = 0;
        for session in running {
            if session.status().await.state.phase() == ConnectionPhase::Connected {
                sessions += 1;
            }
        }
        ports + sessions
    }
}

/// Returns this computer's name, for advertising to other machines.
///
/// Asking the operating system its name is not something the core crate does, so the default is
/// resolved here rather than baked into the configuration schema.
fn default_machine_name() -> String {
    gethostname::gethostname()
        .into_string()
        .ok()
        .map(|name| name.trim_end_matches(".local").to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Midi Harbor".to_owned())
}

impl Daemon {
    /// Returns the name this machine advertises under `config`.
    ///
    /// The advertised name defaults to this computer's, which is what other machines expect to
    /// see and what tells two machines on one network apart. Read from the configuration on each
    /// use, so a changed name applies without a restart.
    pub(crate) fn machine_name<'a>(&'a self, config: &'a Configuration) -> &'a str {
        config
            .preferences
            .machine_name
            .as_deref()
            .unwrap_or(&self.host_name)
    }

    /// Returns the peers discovery can currently see.
    ///
    /// Empty when discovery could not start, which is reported through the capability query
    /// rather than mistaken for an empty network.
    pub fn peers(&self) -> Vec<(DiscoveredPeer, String)> {
        self.discovery
            .as_ref()
            .map(|d| d.peers())
            .unwrap_or_default()
    }

    /// Returns every machine this daemon knows about, remembered or currently advertising.
    ///
    /// Both halves belong in one list. A machine the user has trusted is worth showing whether or
    /// not it is switched on this minute, and it cannot be forgotten if it cannot be seen.
    pub async fn known_peers(&self) -> Vec<KnownPeer> {
        let remembered: Vec<config::PeerConfig> = {
            let inner = self.inner.read().await;
            inner.config.peers.clone()
        };
        merge_known_peers(&remembered, self.peers())
    }

    /// Returns where a remembered machine is reached, by identifier or name.
    pub async fn remembered_address(&self, reference: &str) -> Option<SocketAddr> {
        let inner = self.inner.read().await;
        inner
            .config
            .peers
            .iter()
            .find(|known| known.id.to_string() == reference || known.name == reference)?
            .addresses
            .iter()
            .find_map(|address| address.parse::<SocketAddr>().ok())
    }

    /// Remembers a machine by address, so it is let in without being asked about.
    pub async fn add_manual_peer(
        self: &Arc<Self>,
        address: &str,
        name: Option<String>,
    ) -> Result<KnownPeer, DaemonError> {
        self.add_known_machine(address, name, true).await
    }

    /// Remembers a machine by address, letting it in without asking when `trusted`.
    ///
    /// A machine already known by that address takes the new name and trust rather than being
    /// added twice.
    pub async fn add_known_machine(
        self: &Arc<Self>,
        address: &str,
        name: Option<String>,
        trusted: bool,
    ) -> Result<KnownPeer, DaemonError> {
        let resolved = crate::service::parse_literal_address(address)
            .ok_or_else(|| DaemonError::NotFound(address.to_owned()))?;
        let stored = resolved.to_string();
        let label = name.unwrap_or_else(|| resolved.ip().to_string());

        let peer = {
            let mut inner = self.inner.write().await;
            if let Some(existing) = inner
                .config
                .peers
                .iter_mut()
                .find(|peer| peer.addresses.iter().any(|held| same_host(held, &stored)))
            {
                existing.trusted = trusted;
                existing.name = label;
                existing.clone()
            } else {
                let peer = config::PeerConfig {
                    id: midi_harbor_core::ids::PeerId::new(),
                    name: label,
                    addresses: vec![stored],
                    trusted,
                    advertised_as: None,
                    key: None,
                    port_id: None,
                };
                inner.config.peers.push(peer.clone());
                peer
            }
        };
        {
            let inner = self.inner.read().await;
            config::save(&self.paths, &inner.config)?;
        }

        if !trusted {
            self.forget_answers(&peer.addresses).await;
        }
        self.push_invitation_policy().await;
        info!(peer = %peer.name, trusted, "peer added");
        Ok(KnownPeer {
            id: peer.id,
            name: peer.name,
            addresses: peer.addresses,
            discovered: false,
            trusted: peer.trusted,
        })
    }

    /// Forgets a machine, by identifier, name or address.
    ///
    /// Takes effect at once rather than at the next restart: revoking trust that stays in force
    /// until a restart is not revoking it.
    pub async fn remove_peer(self: &Arc<Self>, reference: &str) -> Result<String, DaemonError> {
        let removed = {
            let mut inner = self.inner.write().await;
            let Some(index) = inner.config.peers.iter().position(|peer| {
                peer.id.to_string() == reference
                    || peer.name == reference
                    || peer.addresses.iter().any(|held| same_host(held, reference))
            }) else {
                return Err(DaemonError::NotFound(reference.to_owned()));
            };
            let removed = inner.config.peers.remove(index);
            config::save(&self.paths, &inner.config)?;
            removed
        };

        self.forget_answers(&removed.addresses).await;
        self.push_invitation_policy().await;
        info!(peer = %removed.name, "peer forgotten");
        Ok(removed.name)
    }

    /// Switches whether a known machine is let in without asking, by identifier, name or
    /// address. Takes effect at once, for the next invitation it sends.
    pub async fn set_peer_trusted(
        self: &Arc<Self>,
        reference: &str,
        trusted: bool,
    ) -> Result<KnownPeer, DaemonError> {
        let peer = {
            let mut inner = self.inner.write().await;
            let Some(peer) = inner.config.peers.iter_mut().find(|peer| {
                peer.id.to_string() == reference
                    || peer.name == reference
                    || peer.addresses.iter().any(|held| same_host(held, reference))
            }) else {
                return Err(DaemonError::NotFound(reference.to_owned()));
            };
            peer.trusted = trusted;
            let peer = peer.clone();
            config::save(&self.paths, &inner.config)?;
            peer
        };
        if !trusted {
            self.forget_answers(&peer.addresses).await;
        }
        self.push_invitation_policy().await;
        info!(peer = %peer.name, trusted, "peer trust changed");
        let discovered = self
            .known_peers()
            .await
            .iter()
            .any(|known| known.id == peer.id && known.discovered);
        Ok(KnownPeer {
            id: peer.id,
            name: peer.name,
            addresses: peer.addresses,
            discovered,
            trusted: peer.trusted,
        })
    }

    /// Drops the invitations answered for this run from a machine's hosts, so a machine no
    /// longer trusted, or forgotten, is asked about again rather than let in until a restart.
    async fn forget_answers(&self, addresses: &[String]) {
        let hosts: Vec<std::net::IpAddr> = addresses
            .iter()
            .filter_map(|address| {
                address
                    .parse::<SocketAddr>()
                    .map(|socket| socket.ip())
                    .or_else(|_| address.parse::<std::net::IpAddr>())
                    .ok()
            })
            .map(|host| host.to_canonical())
            .collect();
        self.accepted_once
            .write()
            .await
            .retain(|host| !hosts.contains(&host.to_canonical()));
    }

    /// Starts a supervisor for every enabled network session that does not have one.
    pub(crate) async fn start_configured_sessions(self: &Arc<Self>) {
        let configured: Vec<Endpoint> = {
            let inner = self.inner.read().await;
            inner
                .config
                .endpoints
                .iter()
                .filter(|e| matches!(e.kind, EndpointKind::NetworkSession(_)) && e.enabled)
                .cloned()
                .collect()
        };
        for endpoint in configured {
            // Also called after a configuration change, when most sessions are already running
            // and a second supervisor for one would contend for its port.
            if self.sessions.read().await.contains_key(&endpoint.id) {
                continue;
            }
            if let Err(error) = self.start_session(&endpoint).await {
                warn!(endpoint = %endpoint.name, error = %error, "could not start network session");
            }
        }
    }

    /// Starts one session's supervisor and advertises it.
    pub(crate) async fn start_session(
        self: &Arc<Self>,
        endpoint: &Endpoint,
    ) -> Result<(), DaemonError> {
        let EndpointKind::NetworkSession(config) = &endpoint.kind else {
            return Ok(());
        };

        // MIDI arriving from the peer is dispatched like anything else, so a session is a route
        // source in its own right. Without this a repeater carries traffic one way only.
        let (deliver, mut inbound) = tokio::sync::mpsc::channel::<Inbound>(256);
        let id = endpoint.id;
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(arrived) = inbound.recv().await {
                match arrived {
                    Inbound::Messages(messages) => {
                        debug!(endpoint = %id, count = messages.len(), "midi received from peer");
                        daemon.dispatch(id, &messages).await;
                    }
                    Inbound::SysEx(dump) => {
                        debug!(endpoint = %id, bytes = dump.len(), "system-exclusive received from peer");
                        daemon.dispatch_sysex(id, &Arc::from(dump)).await;
                    }
                }
            }
        });

        // What a session reports, since it cannot reach the event log itself: invitations only
        // the user can answer, and its connecting and losing connection, which belong in the
        // history like any other endpoint's.
        let (notices, mut arriving) = tokio::sync::mpsc::channel::<SessionNotice>(64);
        let watcher = Arc::clone(self);
        let session_name = endpoint.name.to_string();
        tokio::spawn(async move {
            while let Some(notice) = arriving.recv().await {
                match notice {
                    SessionNotice::Invitation(invitation) => {
                        watcher.record_invitation(id, invitation).await;
                    }
                    change => {
                        watcher
                            .record_session_change(id, &session_name, change)
                            .await;
                        // A link that went down may be one whose session is already advertised
                        // somewhere else, seen while the link was still up. One that came up
                        // may be to a session whose name and key are not held yet.
                        watcher.follow_discovery().await;
                    }
                }
            }
        });

        let session = NetworkSession::start(
            config.local_name.as_str().to_owned(),
            config.control_port,
            deliver,
            config.invitation_policy,
            Some(notices),
        )
        .await
        .map_err(|error| {
            DaemonError::Failure(FailureReason::ProtocolError {
                detail: error.to_string(),
            })
        })?;

        // The peers the user has already accepted, so a trusted one is not asked about again.
        let trusted = self.trusted_addresses().await;
        let _ = session.configure(config.invitation_policy, trusted).await;
        let _ = session
            .identify(Arc::clone(&self.identity), endpoint.id)
            .await;

        let port = session.control_port();
        // A session left to the system's choice of port keeps the port it was given. Choosing
        // again on every start moved it whenever it was switched off and on or the daemon
        // restarted, and a peer that had connected by address went on retrying the old port
        // for good. If the port is taken next time, binding falls back to a nearby pair.
        if config.control_port == 0 {
            let mut inner = self.inner.write().await;
            let pinned = inner
                .config
                .endpoints
                .iter_mut()
                .find(|e| e.id == endpoint.id);
            if let Some(Endpoint {
                kind: EndpointKind::NetworkSession(held),
                ..
            }) = pinned
                && held.control_port == 0
            {
                held.control_port = port;
                if let Err(error) = config::save(&self.paths, &inner.config) {
                    warn!(endpoint = %endpoint.name, error = %error, "could not keep the session's port; it may move on restart");
                }
            }
        }
        let announce = self
            .read(|config, _| config.preferences.advertise_sessions)
            .await;
        if announce
            && let Some(discovery) = &self.discovery
            && let Err(error) = discovery.advertise(config.local_name.as_str(), port, endpoint.id)
        {
            warn!(endpoint = %endpoint.name, error = %error, "could not advertise session");
        }

        info!(endpoint = %endpoint.name, port, "network session listening");
        // Counters and a monitor feed, fresh for each run. The phase is the supervisor's to keep,
        // so what is stored here is never read.
        {
            let now = self.clock.now();
            let mut inner = self.inner.write().await;
            let _ = inner.session_runtime.insert(
                endpoint.id,
                Runtime::new(ConnectionState::enabled(now), None),
            );
        }
        let session = Arc::new(session);
        let _ = self
            .sessions
            .write()
            .await
            .insert(endpoint.id, Arc::clone(&session));
        self.open_automatic_port(endpoint.id).await;

        // A session this machine had connected goes back to its peer, after a restart as after a
        // reboot (FR-015). Without this it came back listening, and nothing reconnected it.
        if let Some(peer) = self.remembered_peer(config.peer).await {
            info!(endpoint = %endpoint.name, %peer, "reconnecting network session to its remembered peer");
            let _ = session.connect(peer).await;
        }
        // The machines it had connected beside the peer come back with it (FR-015i).
        for other in &config.other_peers {
            if let Some(machine) = self.remembered_peer(Some(*other)).await {
                info!(endpoint = %endpoint.name, peer = %machine, "reconnecting a machine remembered beside the peer");
                let _ = session.invite(machine).await;
            }
        }
        Ok(())
    }

    /// Returns the address of the peer a session is set to connect to, if it has one that can
    /// be reached by address.
    async fn remembered_peer(
        &self,
        peer: Option<midi_harbor_core::ids::PeerId>,
    ) -> Option<SocketAddr> {
        let peer = peer?;
        let inner = self.inner.read().await;
        inner
            .config
            .peers
            .iter()
            .find(|known| known.id == peer)?
            .addresses
            .iter()
            .find_map(|address| address.parse::<SocketAddr>().ok())
    }

    /// Records which peer a session is connected to, or that it is connected to nothing at all.
    ///
    /// The peer is kept among the remembered machines, found by address or added, and is not
    /// trusted by this. Connected to none, the session forgets the machines it had connected
    /// beside the peer too, since disconnecting a session ends every machine's part. A machine
    /// let go this way that was remembered only for the connection is dropped with it.
    async fn remember_session_peer(
        &self,
        id: EndpointId,
        peer: Option<SocketAddr>,
    ) -> Result<(), DaemonError> {
        let advertised_as = peer.and_then(|address| self.advertised_at(address));
        let mut inner = self.inner.write().await;
        let before = inner.config.clone();
        let chosen = peer.map(|address| known_peer_at(&mut inner.config, address, advertised_as));
        let Some(session) = network_session_mut(&mut inner.config, id) else {
            return Ok(());
        };
        session.peer = chosen;
        match chosen {
            Some(chosen) => session.other_peers.retain(|other| *other != chosen),
            None => session.other_peers.clear(),
        }
        drop_unused_peers(&mut inner.config);
        if inner.config == before {
            return Ok(());
        }
        config::save(&self.paths, &inner.config)?;
        Ok(())
    }

    /// Records a machine connected beside a session's peer, so it is connected again after a
    /// restart and when its link is lost, as the peer is (FR-015i).
    async fn remember_other_peer(
        &self,
        id: EndpointId,
        machine: SocketAddr,
    ) -> Result<(), DaemonError> {
        let advertised_as = self.advertised_at(machine);
        let mut inner = self.inner.write().await;
        let before = inner.config.clone();
        let known = known_peer_at(&mut inner.config, machine, advertised_as);
        let Some(session) = network_session_mut(&mut inner.config, id) else {
            return Ok(());
        };
        if session.peer != Some(known) && !session.other_peers.contains(&known) {
            session.other_peers.push(known);
        }
        if inner.config == before {
            return Ok(());
        }
        config::save(&self.paths, &inner.config)?;
        Ok(())
    }

    /// Forgets a machine the user disconnected from a session, so it is not connected again, and
    /// keeps the remembered peer in step with the session's.
    ///
    /// When the peer goes and a machine remembered beside it becomes the session's peer, it
    /// becomes the remembered peer as well.
    async fn forget_session_machine(
        &self,
        id: EndpointId,
        machine: SocketAddr,
        now_peer: Option<SocketAddr>,
    ) -> Result<(), DaemonError> {
        let mut inner = self.inner.write().await;
        let peers = inner.config.peers.clone();
        let at = |peer: midi_harbor_core::ids::PeerId, address: SocketAddr| {
            peers.iter().any(|known| {
                known.id == peer
                    && known
                        .addresses
                        .iter()
                        .any(|held| same_address(held, address))
            })
        };
        let Some(session) = network_session_mut(&mut inner.config, id) else {
            return Ok(());
        };
        let before = session.clone();
        session.other_peers.retain(|other| !at(*other, machine));
        if session.peer.is_some_and(|peer| at(peer, machine)) {
            session.peer = None;
        }
        if session.peer.is_none()
            && let Some(now_peer) = now_peer
            && let Some(index) = session
                .other_peers
                .iter()
                .position(|other| at(*other, now_peer))
        {
            session.peer = Some(session.other_peers.remove(index));
        }
        if *session == before {
            return Ok(());
        }
        drop_unused_peers(&mut inner.config);
        config::save(&self.paths, &inner.config)?;
        Ok(())
    }

    /// Returns the name of the session advertised at `address`, if one is.
    fn advertised_at(&self, address: SocketAddr) -> Option<String> {
        self.peers()
            .into_iter()
            .find(|(peer, _)| peer.is_at(address))
            .map(|(peer, _)| peer.name)
    }

    /// Follows the sessions discovery sees now.
    async fn follow_discovery(self: &Arc<Self>) {
        let advertised: Vec<DiscoveredPeer> =
            self.peers().into_iter().map(|(peer, _)| peer).collect();
        self.follow_advertised(&advertised).await;
    }

    /// Returns this daemon's public key in hexadecimal, as its sessions advertise it.
    pub fn identity_key(&self) -> String {
        hex::encode(self.identity.public_key())
    }

    /// Brings what each network port holds about the machines it connects to into line with
    /// the sessions advertised now, and moves a machine whose link is down to where its session
    /// is advertised (R-105, R-106).
    ///
    /// A network port invites the address it connected to until it answers. A session that
    /// comes back on another port, or a machine given another address, never answers there, and
    /// its advertisement is the only thing that says where it went. The new address is stored,
    /// so a restart goes straight to it.
    pub async fn follow_advertised(self: &Arc<Self>, advertised: &[DiscoveredPeer]) {
        let _following = self.following.lock().await;

        // Decide what the advertisements ask for, machine by machine.
        let steps: Vec<(
            EndpointId,
            midi_harbor_core::ids::PeerId,
            SocketAddr,
            bool,
            Step,
        )> = {
            let inner = self.inner.read().await;
            let mut steps = Vec::new();
            for endpoint in &inner.config.endpoints {
                let EndpointKind::NetworkSession(session) = &endpoint.kind else {
                    continue;
                };
                for id in session.peer.iter().chain(session.other_peers.iter()) {
                    let Some(known) = inner.config.peers.iter().find(|known| known.id == *id)
                    else {
                        continue;
                    };
                    let Some(held) = known
                        .addresses
                        .iter()
                        .find_map(|address| address.parse::<SocketAddr>().ok())
                    else {
                        continue;
                    };
                    let followed = Followed {
                        held,
                        trusted: known.trusted,
                        advertised_as: known.advertised_as.as_deref(),
                        identity: known
                            .key
                            .as_deref()
                            .zip(known.port_id)
                            .and_then(|(key, port)| PortIdentity::new(key, port)),
                    };
                    if let Some(step) = crate::discovery::next_step(&followed, advertised) {
                        steps.push((endpoint.id, *id, held, known.trusted, step));
                    }
                }
            }
            steps
        };

        for (endpoint, peer, held, trusted, step) in steps {
            let session = self.sessions.read().await.get(&endpoint).map(Arc::clone);
            let Some(session) = session else {
                continue;
            };
            match step {
                // Keep what the session where the machine is says about it. Its identity is
                // kept only once proved there: an advertisement can name any address.
                Step::Learn { name, identity } => {
                    let proved = match identity {
                        Some(identity) if session.prove(held, identity).await => Some(identity),
                        _ => None,
                    };
                    if name.is_none() && proved.is_none() {
                        continue;
                    }
                    self.change_known_peer(peer, |known| {
                        if let Some(name) = name {
                            known.advertised_as = Some(name);
                        }
                        if let Some(identity) = proved {
                            known.key = Some(identity.key_text());
                            known.port_id = Some(identity.port);
                        }
                    })
                    .await;
                }
                // Move the link. The session refuses for a machine carrying MIDI, whose
                // advertisement elsewhere may be another machine that took the name.
                Step::Move { to, name, prove } => {
                    if let Some(identity) = prove
                        && !session.prove(to, identity).await
                    {
                        debug!(%endpoint, %to, "a session advertised as a known port did not prove it");
                        continue;
                    }
                    if !session.move_machine(held, to).await {
                        continue;
                    }
                    let stored = held.to_string();
                    self.change_known_peer(peer, |known| {
                        known.advertised_as = Some(name);
                        if let Some(address) = known
                            .addresses
                            .iter_mut()
                            .find(|address| **address == stored)
                        {
                            *address = to.to_string();
                        }
                    })
                    .await;
                    // Trust is held by host, so a trusted machine proved on another host is
                    // trusted there from now on.
                    if trusted && to.ip().to_canonical() != held.ip().to_canonical() {
                        self.push_invitation_policy().await;
                    }
                    let _ = self.changes.send(Change::EndpointChanged(endpoint));
                }
            }
        }
    }

    /// Changes one remembered machine and stores the configuration.
    async fn change_known_peer(
        &self,
        peer: midi_harbor_core::ids::PeerId,
        change: impl FnOnce(&mut config::PeerConfig),
    ) {
        let mut inner = self.inner.write().await;
        let Some(known) = inner.config.peers.iter_mut().find(|known| known.id == peer) else {
            return;
        };
        change(known);
        if let Err(error) = config::save(&self.paths, &inner.config) {
            error!(peer = %peer, error = %error, "failed to store what is known about a machine");
        }
    }

    /// Watches the sessions discovery sees come and go, and follows each machine a network port
    /// connects to to where it is advertised.
    fn watch_discovery(self: &Arc<Self>) {
        let Some(discovery) = self.discovery.clone() else {
            return;
        };
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                discovery.changed().await;
                daemon.follow_discovery().await;
            }
        });
    }

    /// Creates a network session with its automatic port, persists it, and starts listening.
    pub async fn create_network_session(
        self: &Arc<Self>,
        name: &str,
        control_port: u16,
        policy: InvitationPolicy,
    ) -> Result<Endpoint, DaemonError> {
        self.create_network_port(name, None, control_port, policy, true)
            .await
    }

    /// Creates a network port, with or without the automatic port other applications see it
    /// by, persists it, and starts listening.
    ///
    /// Other machines see it by `local_name`, its Bonjour name, or by `name` when that is not
    /// given, which is what a user naming it expects them to show.
    pub async fn create_network_port(
        self: &Arc<Self>,
        name: &str,
        local_name: Option<&str>,
        control_port: u16,
        policy: InvitationPolicy,
        automatic_port: bool,
    ) -> Result<Endpoint, DaemonError> {
        let name = EndpointName::new(name)?;
        let advertised = match local_name {
            Some(local_name) => EndpointName::new(local_name)?,
            None => name.clone(),
        };
        crate::network_port::check_control_port(control_port)?;

        let endpoint = {
            let mut inner = self.inner.write().await;
            if midi_harbor_core::endpoint::name_conflicts(
                &inner.config.endpoints,
                &name,
                "network",
                None,
            ) {
                return Err(FailureReason::NameConflict {
                    name: name.as_str().to_owned(),
                }
                .into());
            }
            // Two network ports advertising one name look like one machine to every peer.
            if inner.config.endpoints.iter().any(|e| {
                matches!(&e.kind, EndpointKind::NetworkSession(held) if held.local_name == advertised)
            }) {
                return Err(FailureReason::NameConflict {
                    name: advertised.as_str().to_owned(),
                }
                .into());
            }

            let endpoint = Endpoint {
                direction: Direction::Bidirectional,
                ..Endpoint::new(
                    name,
                    EndpointKind::NetworkSession(NetworkSessionConfig {
                        automatic_port,
                        ..NetworkSessionConfig::new(advertised, control_port, policy)
                    }),
                )
            };
            inner.config.add_endpoint(endpoint.clone());
            config::save(&self.paths, &inner.config)?;
            endpoint
        };

        self.start_session(&endpoint).await?;
        let _ = self.changes.send(Change::EndpointAdded(endpoint.id));
        Ok(endpoint)
    }

    /// Connects a session to a peer.
    pub async fn connect_peer(
        self: &Arc<Self>,
        id: EndpointId,
        peer: SocketAddr,
    ) -> Result<(), DaemonError> {
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(&id).map(Arc::clone)
        };
        let Some(session) = session else {
            return Err(DaemonError::NotFound(id.to_string()));
        };

        if !session.connect(peer).await {
            return Err(FailureReason::NetworkUnreachable.into());
        }
        self.remember_session_peer(id, Some(peer)).await?;
        let _ = self.changes.send(Change::EndpointChanged(id));
        Ok(())
    }

    /// Invites a machine to carry MIDI beside those a session already has, or connects to it
    /// when it has none.
    ///
    /// Remembered and reconnected as the peer is, after a restart and when its link is lost,
    /// until the user disconnects it (FR-015i).
    pub async fn invite_machine(
        self: &Arc<Self>,
        id: EndpointId,
        machine: SocketAddr,
    ) -> Result<(), DaemonError> {
        let session = self.sessions.read().await.get(&id).map(Arc::clone);
        let Some(session) = session else {
            return Err(DaemonError::NotFound(id.to_string()));
        };
        // With nothing connected the machine becomes the peer, which is remembered as a connect
        // would remember it. Which it became is the supervisor's answer, since the status lags a
        // connect still queued ahead of the invitation.
        match session.invite(machine).await {
            Some(Place::Peer) => self.remember_session_peer(id, Some(machine)).await?,
            Some(Place::Beside) => self.remember_other_peer(id, machine).await?,
            None => return Err(FailureReason::NetworkUnreachable.into()),
        }
        let _ = self.changes.send(Change::EndpointChanged(id));
        Ok(())
    }

    /// Ends one machine's part in a session, leaving the others connected.
    ///
    /// The machine is forgotten, so the session does not connect to it again when it starts.
    pub async fn disconnect_machine(
        self: &Arc<Self>,
        id: EndpointId,
        machine: SocketAddr,
    ) -> Result<(), DaemonError> {
        let session = self.sessions.read().await.get(&id).map(Arc::clone);
        let Some(session) = session else {
            return Err(DaemonError::NotFound(id.to_string()));
        };
        if !session.disconnect_machine(machine).await {
            return Err(DaemonError::NotFound(format!("machine {machine}")));
        }
        let now_peer = session.status().await.peer_address;
        self.forget_session_machine(id, machine, now_peer).await?;
        let _ = self.changes.send(Change::EndpointChanged(id));
        Ok(())
    }

    /// Disconnects a session from its peer, leaving it configured and listening.
    pub async fn disconnect_peer(self: &Arc<Self>, id: EndpointId) -> Result<(), DaemonError> {
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(&id).map(Arc::clone)
        };
        let Some(session) = session else {
            return Err(DaemonError::NotFound(id.to_string()));
        };
        let _ = session.disconnect().await;
        self.remember_session_peer(id, None).await?;
        let _ = self.changes.send(Change::EndpointChanged(id));
        Ok(())
    }

    /// Returns what a running session currently observes, if it is running.
    pub async fn session_status(&self, id: EndpointId) -> Option<crate::session::SessionStatus> {
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(&id).map(Arc::clone)
        };
        match session {
            Some(session) => Some(session.status().await),
            None => None,
        }
    }
}

impl Daemon {
    /// Brings the endpoint list into line with the hardware currently attached.
    ///
    /// Stored devices that are absent are kept so their routes survive being unplugged, and
    /// hardware never seen before is added. Called at startup and whenever the platform reports
    /// the MIDI environment changed.
    pub async fn refresh_devices(self: &Arc<Self>) {
        let _one_at_a_time = self.refreshing.lock().await;

        let attached = match self.midi.list_devices() {
            Ok(devices) => devices,
            Err(error) => {
                warn!(error = %error, "could not enumerate midi hardware");
                return;
            }
        };

        // Our own virtual ports appear in the platform's device list, and adopting them as
        // hardware would duplicate every one of them. They are recognised by the identifier the
        // platform gave them. Matching by name hid any real device the user had named a port
        // after: a port called "Keystation" made the Keystation keyboard disappear. The name is
        // the fallback only for a port whose identifier is not known yet.
        let (our_ids, our_unidentified, stored): (Vec<u32>, Vec<String>, Vec<Endpoint>) = {
            let inner = self.inner.read().await;
            let ports: Vec<&Endpoint> = inner.config.virtual_ports().collect();
            // Every connector's identifier and, for a port not identified yet, every connector's
            // name: missing one would adopt "Keys 2" as hardware.
            let ids = ports
                .iter()
                .filter_map(|e| match &e.kind {
                    EndpointKind::VirtualPort(port) => {
                        Some(port.input_ids.iter().chain(&port.output_ids).copied())
                    }
                    _ => None,
                })
                .flatten()
                // A network port's automatic port is ours too.
                .chain(
                    inner
                        .config
                        .endpoints
                        .iter()
                        .filter_map(|e| match &e.kind {
                            EndpointKind::NetworkSession(session) if session.automatic_port => {
                                Some(
                                    session
                                        .port_input_id
                                        .into_iter()
                                        .chain(session.port_output_id),
                                )
                            }
                            _ => None,
                        })
                        .flatten(),
                )
                .collect();
            let unidentified = ports
                .iter()
                .filter_map(|e| match &e.kind {
                    EndpointKind::VirtualPort(port)
                        if port.input_ids.is_empty() || port.output_ids.is_empty() =>
                    {
                        let name = e.name.as_str();
                        let names: Vec<String> = (0..port.inputs)
                            .map(|index| connector_name(name, port.inputs, index))
                            .chain(
                                (0..port.outputs)
                                    .map(|index| connector_name(name, port.outputs, index)),
                            )
                            .collect();
                        Some(names)
                    }
                    _ => None,
                })
                .flatten()
                .chain(inner.config.endpoints.iter().filter_map(|e| match &e.kind {
                    EndpointKind::NetworkSession(session)
                        if session.automatic_port
                            && (session.port_input_id.is_none()
                                || session.port_output_id.is_none()) =>
                    {
                        Some(e.name.as_str().to_owned())
                    }
                    _ => None,
                }))
                .collect();
            (ids, unidentified, inner.config.endpoints.clone())
        };
        let attached: Vec<_> = attached
            .into_iter()
            .filter(|device| match device.fingerprint.unique_id {
                Some(id) => !our_ids.contains(&id),
                None => !our_unidentified.contains(&device.fingerprint.name),
            })
            .collect();

        let refreshed = crate::devices::endpoints_for(&stored, &attached);
        let mut changed = Vec::new();
        let mut gone: Vec<EndpointId> = Vec::new();

        {
            let mut inner = self.inner.write().await;
            // Replace only the physical entries, leaving virtual ports and sessions alone.
            let before = inner.config.endpoints.clone();
            let existing: Vec<EndpointId> = inner
                .config
                .endpoints
                .iter()
                .filter(|e| matches!(e.kind, EndpointKind::PhysicalDevice(_)))
                .map(|e| e.id)
                .collect();

            inner
                .config
                .endpoints
                .retain(|e| !matches!(e.kind, EndpointKind::PhysicalDevice(_)));
            let (refreshed, forgotten) =
                crate::devices::forget_unused_software(refreshed, &inner.config.routes);
            // Nothing is routed from a forgotten port, so there is nothing to silence, only a
            // handle to close if it was open.
            for endpoint in forgotten {
                if let Some(runtime) = inner.runtime.remove(&endpoint.id)
                    && let Some(handle) = runtime.handle
                {
                    let _ = self.midi.close_device(handle);
                }
                debug!(port = %endpoint.name.as_str(), "forgot an application's port that has gone");
            }
            for endpoint in refreshed {
                if !existing.contains(&endpoint.id) {
                    changed.push(endpoint.id);
                }
                inner.config.endpoints.push(endpoint);
            }
            inner.config.qualify_routes(&before);

            if let Err(error) = config::save(&self.paths, &inner.config) {
                warn!(error = %error, "could not persist discovered devices");
            }

            // Hardware that has gone is closed rather than left open. A handle to a device that
            // is not there carries nothing, and leaving the runtime entry behind makes the
            // replug look like the device is already open, so it never reopens.
            let departed: Vec<(EndpointId, String, bool)> = inner
                .config
                .endpoints
                .iter()
                .filter_map(|endpoint| match &endpoint.kind {
                    EndpointKind::PhysicalDevice(device) if !device.present => Some((
                        endpoint.id,
                        endpoint.name.as_str().to_owned(),
                        device.software,
                    )),
                    _ => None,
                })
                .filter(|(id, _, _)| inner.runtime.contains_key(id))
                .collect();

            let now = self.clock.now();
            for (id, name, software) in departed {
                gone.push(id);
                let runtime = inner.runtime.remove(&id);
                if let Some(handle) = runtime.as_ref().and_then(|runtime| runtime.handle)
                    && let Err(error) = self.midi.close_device(handle)
                {
                    debug!(device = %name, error = %error, "could not close a device that went away");
                }
                // A provided port was closed by whatever provides it, not unplugged.
                let detail = if software {
                    info!(device = %name, "provided port went away");
                    format!("{name} went away; routes using it are waiting")
                } else {
                    info!(device = %name, "hardware went away");
                    format!("{name} was unplugged; routes using it are waiting")
                };
                let counters = runtime.map_or_else(
                    || Arc::new(TrafficCounters::new()),
                    |runtime| runtime.counters,
                );
                let _ = inner.unplugged.insert(id, counters);
                let mut event = events::event(
                    EventKind::EndpointStateChanged,
                    Severity::Warning,
                    now,
                    detail,
                );
                event.endpoint = Some(id);
                let _ = inner.events.record(event);
            }
        }

        // A device unplugged mid-phrase leaves its notes sounding wherever it was routed, and
        // nobody can release them: the keyboard that would send the note off is in a bag.
        for id in gone {
            self.silence_routes_from(id).await;
        }

        for id in changed {
            let _ = self.changes.send(Change::EndpointAdded(id));
        }

        self.open_present_devices().await;
    }

    /// Opens attached hardware so it can actually carry MIDI.
    ///
    /// Listing a device without opening it produces the worst kind of failure: the endpoint
    /// appears, a route to it is accepted and reported valid, and nothing ever arrives.
    async fn open_present_devices(self: &Arc<Self>) {
        let present: Vec<(EndpointId, DeviceFingerprint, bool, bool)> = {
            let inner = self.inner.read().await;
            inner
                .config
                .endpoints
                .iter()
                .filter_map(|endpoint| match &endpoint.kind {
                    EndpointKind::PhysicalDevice(device) if device.present && endpoint.enabled => {
                        Some((
                            endpoint.id,
                            device.fingerprint.clone(),
                            device.software,
                            inner.runtime.contains_key(&endpoint.id),
                        ))
                    }
                    _ => None,
                })
                .collect()
        };

        for (id, fingerprint, software, already_open) in present {
            if already_open {
                continue;
            }
            let (producer, consumer) = dataplane::channel(id);
            let now = self.clock.now();

            match self
                .midi
                .open_device_with_sink(&fingerprint, Some(producer))
            {
                Ok(handle) => {
                    let mut state = ConnectionState::enabled(now);
                    let _ = state.apply_now(
                        midi_harbor_core::state::Event::Attempting,
                        self.clock.as_ref(),
                    );
                    let _ = state.apply_now(
                        midi_harbor_core::state::Event::Established,
                        self.clock.as_ref(),
                    );

                    {
                        let mut inner = self.inner.write().await;
                        let mut runtime = Runtime::new(state, Some(handle));
                        let returned = inner.unplugged.remove(&id);
                        if let Some(counters) = &returned {
                            runtime.counters = Arc::clone(counters);
                        }
                        inner.runtime.insert(id, runtime);
                        // Its return is history as much as its going was (FR-046, SC-013).
                        if returned.is_some() {
                            let detail = if software {
                                format!("{} is back; routes using it resume", fingerprint.name)
                            } else {
                                format!(
                                    "{} was plugged back in; routes using it resume",
                                    fingerprint.name
                                )
                            };
                            let mut event = events::event(
                                EventKind::EndpointStateChanged,
                                Severity::Info,
                                now,
                                detail,
                            );
                            event.endpoint = Some(id);
                            let _ = inner.events.record(event);
                        }
                    }
                    self.start_dispatch(consumer);
                    if software {
                        info!(device = %fingerprint.name, "opened a provided port");
                    } else {
                        info!(device = %fingerprint.name, "opened attached hardware");
                    }
                }
                Err(error) => {
                    // A device that cannot be opened is reported rather than left looking
                    // available, since a route to it would otherwise claim to be valid.
                    let reason = error.as_failure_reason();
                    warn!(device = %fingerprint.name, error = %reason, "could not open device");
                    let detail = format!("{} could not open: {reason}", fingerprint.name);

                    let mut state = ConnectionState::enabled(now);
                    let _ = state.apply_now(
                        midi_harbor_core::state::Event::Attempting,
                        self.clock.as_ref(),
                    );
                    let _ = state.apply_now(
                        midi_harbor_core::state::Event::Failed(reason),
                        self.clock.as_ref(),
                    );

                    let mut inner = self.inner.write().await;
                    inner.runtime.insert(id, Runtime::new(state, None));
                    let mut failed = events::event(
                        EventKind::EndpointStateChanged,
                        Severity::Error,
                        now,
                        detail,
                    );
                    failed.endpoint = Some(id);
                    let _ = inner.events.record(failed);
                }
            }
        }
    }

    /// Returns the route graph as it currently resolves.
    pub async fn router(&self) -> Router {
        // Running means dispatch would actually attempt delivery there: a platform endpoint with
        // an open handle, or a session with a supervisor. Anything else configured is suspended,
        // and saying so is the difference between a route that looks fine while dropping
        // everything and one that explains itself.
        let sessions: HashSet<EndpointId> = self.sessions.read().await.keys().copied().collect();
        let links: HashSet<EndpointId> = self.bt_links.read().await.keys().copied().collect();
        let advertised: HashSet<EndpointId> =
            self.bt_advertised.read().await.iter().copied().collect();
        let inner = self.inner.read().await;
        let running: HashSet<EndpointId> = inner
            .runtime
            .iter()
            .filter(|(_, runtime)| runtime.handle.is_some())
            .map(|(id, _)| *id)
            .chain(sessions)
            .chain(links)
            .chain(advertised)
            .collect();

        Router::build(&inner.config.routes, &inner.config.endpoints, &running)
    }

    /// Creates a route between two endpoints.
    ///
    /// A route that completes a cycle is still created, because delivery is direct and a cycle
    /// cannot multiply messages. The caller is told so it can warn.
    pub async fn create_route(
        self: &Arc<Self>,
        from: &str,
        to: &str,
    ) -> Result<(RouteConfig, Vec<String>), DaemonError> {
        self.create_route_through(from, 0, to, 0).await
    }

    /// Creates a route between connectors of two endpoints, each counted from zero: one of the
    /// source's MIDI Ins and one of the destination's MIDI Outs.
    pub async fn create_route_through(
        self: &Arc<Self>,
        from: &str,
        from_connector: u8,
        to: &str,
        to_connector: u8,
    ) -> Result<(RouteConfig, Vec<String>), DaemonError> {
        self.create_route_with(RouteRequest {
            from,
            from_connector,
            to,
            to_connector,
            both_ways: false,
        })
        .await
    }

    /// Creates the route a request describes.
    pub async fn create_route_with(
        self: &Arc<Self>,
        request: RouteRequest<'_>,
    ) -> Result<(RouteConfig, Vec<String>), DaemonError> {
        let created = {
            let mut inner = self.inner.write().await;
            let route = planned_route(&inner.config, &inner.config.routes, &request)?;
            inner.config.routes.push(route.clone());
            sync_route_counters(&mut inner);
            config::save(&self.paths, &inner.config)?;
            route
        };
        let warning = self.loop_warning().await;
        let _ = self.changes.send(Change::RoutesChanged);
        info!(from = %request.from, to = %request.to, "route created");
        Ok((created, warning))
    }

    /// Changes a route's ends, connectors and whether it carries MIDI both ways, keeping whether
    /// it is switched on.
    ///
    /// The notes it was holding are silenced first, since they may be sounding at an end it no
    /// longer reaches. Its identifier changes when its ends do, as it is derived from them.
    pub async fn update_route(
        self: &Arc<Self>,
        id: &str,
        request: RouteRequest<'_>,
    ) -> Result<(RouteConfig, Vec<String>), DaemonError> {
        self.silence_route_by_id(id).await;
        let updated = {
            let mut inner = self.inner.write().await;
            let Some(index) = inner
                .config
                .routes
                .iter()
                .position(|route| route.id().to_string() == id)
            else {
                return Err(DaemonError::NotFound(id.to_owned()));
            };
            // Checked against every route but this one, which it replaces.
            let others: Vec<RouteConfig> = inner
                .config
                .routes
                .iter()
                .enumerate()
                .filter(|(at, _)| *at != index)
                .map(|(_, route)| route.clone())
                .collect();
            let mut route = planned_route(&inner.config, &others, &request)?;
            if let Some(held) = inner.config.routes.get_mut(index) {
                route.enabled = held.enabled;
                *held = route.clone();
            }
            sync_route_counters(&mut inner);
            config::save(&self.paths, &inner.config)?;
            route
        };
        let warning = self.loop_warning().await;
        let _ = self.changes.send(Change::RoutesChanged);
        info!(%id, from = %request.from, to = %request.to, "route changed");
        Ok((updated, warning))
    }

    /// Returns the routes forming a loop, which FR-033 asks to report; they still deliver.
    async fn loop_warning(self: &Arc<Self>) -> Vec<String> {
        self.router()
            .await
            .looping()
            .into_iter()
            .map(|route| format!("{} -> {}", route.from, route.to))
            .collect()
    }

    /// Deletes a route.
    pub async fn delete_route(self: &Arc<Self>, id: &str) -> Result<(), DaemonError> {
        // Silenced before the route is gone, while its destination and its held notes are both
        // still known.
        self.silence_route_by_id(id).await;

        let mut inner = self.inner.write().await;
        let before = inner.config.routes.len();
        inner
            .config
            .routes
            .retain(|route| route.id().to_string() != id);

        if inner.config.routes.len() == before {
            return Err(DaemonError::NotFound(id.to_owned()));
        }
        sync_route_counters(&mut inner);
        config::save(&self.paths, &inner.config)?;
        drop(inner);

        let _ = self.changes.send(Change::RoutesChanged);
        Ok(())
    }

    /// Switches a route on or off without deleting it.
    pub async fn set_route_enabled(
        self: &Arc<Self>,
        id: &str,
        enabled: bool,
    ) -> Result<RouteConfig, DaemonError> {
        if !enabled {
            self.silence_route_by_id(id).await;
        }

        let updated = {
            let mut inner = self.inner.write().await;
            let Some(route) = inner
                .config
                .routes
                .iter_mut()
                .find(|route| route.id().to_string() == id)
            else {
                return Err(DaemonError::NotFound(id.to_owned()));
            };
            route.enabled = enabled;
            let updated = route.clone();
            config::save(&self.paths, &inner.config)?;
            updated
        };

        let _ = self.changes.send(Change::RoutesChanged);
        Ok(updated)
    }
}

impl Daemon {
    /// Starts draining one endpoint's ring buffer and fanning its MIDI along the route graph.
    ///
    /// One task per endpoint, so a busy device cannot delay a quiet one, and a drain that falls
    /// behind shows up as that endpoint's own dropped count rather than as jitter everywhere.
    pub(crate) fn start_dispatch(self: &Arc<Self>, mut consumer: RtConsumer) {
        let daemon = Arc::clone(self);
        let source = consumer.source();
        let connector = consumer.connector();

        tokio::spawn(async move {
            let arrived = dataplane::arrived();
            // Armed before every drain, so MIDI arriving while a pass runs wakes the next one
            // rather than waiting out the idle check. The first pass does not wait at all, in
            // case something arrived before this task was running to hear it.
            let mut notified = Box::pin(arrived.notified());
            notified.as_mut().enable();
            let mut more_waiting = true;
            let mut reported_drops = 0u64;
            let mut reported_malformed = 0u64;
            // One assembler per endpoint, held by the task that drains it: a partial dump dies
            // with the endpoint rather than being forwarded truncated or spliced onto the next.
            let mut assembler = dataplane::SysExAssembler::new();
            let mut reported_discards = 0u64;
            // A Bluetooth device stamps what it plays with its own clock, and its messages are
            // held to that timing rather than delivered in the bunches its radio packets make
            // (FR-019). Every other source's timestamp is when it arrived, which says nothing
            // the order of arrival does not.
            let mut timing = daemon
                .is_bluetooth(source)
                .await
                .then(midi_harbor_core::devicetime::DeviceTiming::new);
            let started = std::time::Instant::now();

            loop {
                // A full batch means more is already waiting, so there is nothing to wait for.
                if more_waiting {
                    tokio::task::yield_now().await;
                } else {
                    tokio::select! {
                        () = notified.as_mut() => {}
                        () = tokio::time::sleep(DISPATCH_IDLE_CHECK) => {}
                    }
                }
                notified = Box::pin(arrived.notified());
                notified.as_mut().enable();

                // Bounded, so one endpoint flooding cannot hold this task indefinitely.
                let batch = consumer.drain(DISPATCH_BATCH);
                more_waiting = batch.len() >= DISPATCH_BATCH;
                let dropped = consumer.dropped();

                let malformed = consumer.malformed();

                if dropped > reported_drops {
                    warn!(
                        endpoint = %source,
                        dropped = dropped - reported_drops,
                        "midi dropped: the data path fell behind"
                    );
                }
                if dropped > reported_drops || malformed > reported_malformed {
                    daemon
                        .record_discards(
                            source,
                            dropped.saturating_sub(reported_drops),
                            malformed.saturating_sub(reported_malformed),
                        )
                        .await;
                    reported_drops = dropped;
                    reported_malformed = malformed;
                }
                if batch.is_empty() {
                    // The endpoint going away is what ends this task.
                    if !daemon.is_open(source).await {
                        if assembler.abandon() {
                            warn!(
                                endpoint = %source,
                                "system-exclusive discarded: the endpoint went away mid-message"
                            );
                        }
                        return;
                    }
                    continue;
                }

                // Order is preserved across both kinds: a program change that follows a dump has
                // to arrive after it, so plain messages are held only until the dump they precede
                // is whole, then sent ahead of it.
                let mut pending: Vec<MidiMessage> = Vec::new();
                let drained_at = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                for item in &batch {
                    match item {
                        dataplane::Drained::Message { message, timestamp } => {
                            if let Some(timing) = timing.as_mut() {
                                let hold = timing.hold(*timestamp, drained_at);
                                if !hold.is_zero() {
                                    // What is due sooner goes first, so holding this message
                                    // never delays the ones played before it.
                                    if !pending.is_empty() {
                                        daemon.dispatch_from(source, connector, &pending).await;
                                        pending.clear();
                                    }
                                    tokio::time::sleep(hold).await;
                                }
                            }
                            pending.push(*message);
                        }
                        dataplane::Drained::SysEx { bytes, end, .. } => {
                            let Some(whole) = assembler.push(bytes, *end) else {
                                continue;
                            };
                            if !pending.is_empty() {
                                daemon.dispatch_from(source, connector, &pending).await;
                                pending.clear();
                            }
                            daemon
                                .dispatch_sysex_from(source, connector, &Arc::from(whole))
                                .await;
                        }
                    }
                }
                if !pending.is_empty() {
                    daemon.dispatch_from(source, connector, &pending).await;
                }

                if assembler.discarded() > reported_discards {
                    warn!(
                        endpoint = %source,
                        discarded = assembler.discarded() - reported_discards,
                        "system-exclusive discarded: the message never completed"
                    );
                    reported_discards = assembler.discarded();
                }
            }
        });
    }

    /// Reports whether an endpoint is a Bluetooth link, whose messages carry the device's clock.
    async fn is_bluetooth(&self, id: EndpointId) -> bool {
        self.inner
            .read()
            .await
            .config
            .endpoints
            .iter()
            .any(|endpoint| {
                endpoint.id == id && matches!(endpoint.kind, EndpointKind::BluetoothDevice(_))
            })
    }

    /// Stops every note an endpoint has sounding, before it stops being able to.
    ///
    /// Sends directly through the platform rather than along the route graph: the endpoint is
    /// about to be closed, and what has to be stopped is what this endpoint itself started.
    /// Quiet endpoints send nothing, so disabling an idle port disturbs nobody.
    pub(crate) async fn silence_endpoint(&self, id: EndpointId) {
        let (handle, messages, name, outputs) = {
            let inner = self.inner.read().await;
            let Some(runtime) = inner
                .runtime
                .get(&id)
                .or_else(|| inner.automatic_ports.get(&id).map(|port| &port.runtime))
            else {
                return;
            };
            // No platform handle is not the same as nowhere to send: a Bluetooth link carries
            // MIDI without one, and a note held there needs stopping as much as any other.
            let handle = runtime.handle;
            let messages = match runtime.sounding.lock() {
                Ok(sounding) => sounding.silence(),
                // A poisoned lock means a task panicked mid-update. Silencing every channel is
                // the safe reading of an unknown state; leaving notes sounding is not.
                Err(_) => Channel::all()
                    .flat_map(midi_harbor_core::midi::silence_channel)
                    .collect(),
            };
            let name = inner
                .config
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == id)
                .map(|endpoint| endpoint.name.to_string())
                .unwrap_or_else(|| id.to_string());
            // Notes are tracked per endpoint, not per connector, so each MIDI Out a virtual
            // port has is sent the silence: a note-off where no note sounds changes nothing.
            let outputs = inner
                .config
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == id)
                .map_or(1, |endpoint| {
                    midi_harbor_core::router::connectors(endpoint).1
                });
            (handle, messages, name, outputs)
        };
        if messages.is_empty() {
            return;
        }

        let mut silenced = false;
        for connector in 0..outputs {
            silenced |= self.deliver_through(id, handle, connector, &messages).await;
        }
        if !silenced {
            // Logged rather than returned: the endpoint is being torn down either way, and
            // failing to silence must not become a reason to leave it open.
            warn!(endpoint = %name, "could not silence notes before closing");
            return;
        }

        let at = self.clock.now();
        self.observe(id, &messages, true, at).await;
        {
            let inner = self.inner.read().await;
            if let Some(runtime) = inner.runtime.get(&id)
                && let Ok(mut sounding) = runtime.sounding.lock()
            {
                sounding.clear();
            }
        }
        {
            let mut inner = self.inner.write().await;
            let mut event = events::event(
                EventKind::NotesSilenced,
                Severity::Info,
                at,
                format!("silenced notes still sounding on '{name}'"),
            );
            event.endpoint = Some(id);
            let _ = inner.events.record(event);
        }
        info!(endpoint = %name, "notes silenced");
    }

    /// Stops the notes one route left sounding at its destination.
    ///
    /// Only that route's notes: a destination can be fed by several sources, and cutting off the
    /// others because this one went away would turn one silent instrument into all of them.
    async fn silence_route(&self, route: RouteId, destination: EndpointId) {
        // A two-way route holds notes it carried back as well, sounding at its source. Its held
        // notes are tracked together, so both ends are sent the silence: a note-off where no
        // note sounds changes nothing.
        let router = self.router().await;
        let back: Option<(EndpointId, u8)> = router
            .routes()
            .iter()
            .find(|resolved| resolved.id == route && resolved.both_ways)
            .and_then(|resolved| {
                resolved
                    .source
                    .map(|source| (source, resolved.from_connector))
            });
        let (handle, messages, connector) = {
            let inner = self.inner.read().await;
            // Through the MIDI Out the route used, since that is where its notes are sounding.
            let connector = inner
                .config
                .routes
                .iter()
                .find(|configured| configured.id() == route)
                .map_or(0, RouteConfig::to_index);

            // Channels another route still has notes sounding on, through the same MIDI Out.
            // Read before this route's record is locked, so no record is held while another is
            // taken and two routes silenced at once cannot deadlock.
            let shared = router
                .routes()
                .iter()
                .filter(|other| {
                    other.id != route
                        && other.destination == Some(destination)
                        && other.to_connector == connector
                })
                .filter_map(|other| inner.route_counters.get(&other.id))
                .filter_map(|other| {
                    other
                        .sounding
                        .lock()
                        .ok()
                        .map(|sounding| sounding.channels())
                })
                .fold(0, |shared, channels| shared | channels);

            let Some(runtime) = inner.route_counters.get(&route) else {
                return;
            };
            let Ok(mut sounding) = runtime.sounding.lock() else {
                return;
            };
            let messages = sounding.silence_beside(shared);
            sounding.clear();

            let handle = inner
                .runtime
                .get(&destination)
                .and_then(|endpoint| endpoint.handle);
            (handle, messages, connector)
        };

        if messages.is_empty() {
            return;
        }
        if let Some((source, source_connector)) = back {
            let source_handle = self
                .inner
                .read()
                .await
                .runtime
                .get(&source)
                .and_then(|endpoint| endpoint.handle);
            if self
                .deliver_through(source, source_handle, source_connector, &messages)
                .await
            {
                let at = self.clock.now();
                self.observe(source, &messages, true, at).await;
            }
        }
        if !self
            .deliver_through(destination, handle, connector, &messages)
            .await
        {
            warn!(endpoint = %destination, "could not silence a route's notes");
            return;
        }

        let at = self.clock.now();
        self.observe(destination, &messages, true, at).await;
        info!(%route, endpoint = %destination, "silenced the notes a route was holding");
    }

    /// Stops the notes a route is holding, found by its identifier.
    ///
    /// Nothing happens for a route that is not carrying, which is every route that was never
    /// delivering and every one whose destination is already gone.
    async fn silence_route_by_id(self: &Arc<Self>, id: &str) {
        let found = self
            .router()
            .await
            .routes()
            .iter()
            .find(|route| route.id.to_string() == id)
            .and_then(|route| route.destination.map(|destination| (route.id, destination)));

        if let Some((route, destination)) = found {
            self.silence_route(route, destination).await;
        }
    }

    /// Stops the notes every live route from `source` left sounding at its destinations.
    ///
    /// Called when a source stops being able to finish what it started — hardware unplugged
    /// mid-phrase, which is FR-015f, and the note nobody can release from the keyboard that is
    /// now in a bag.
    pub(crate) async fn silence_routes_from(self: &Arc<Self>, source: EndpointId) {
        // Every route from this source, not only the ones still live. By the time this runs the
        // source has stopped running, which suspends its routes — and being suspended is exactly
        // why the notes they were carrying need stopping.
        let holding: Vec<(RouteId, EndpointId)> = self
            .router()
            .await
            .routes()
            .iter()
            // A two-way route carries from its destination too, so a destination that went away
            // leaves notes sounding back at the source.
            .filter(|route| {
                route.source == Some(source)
                    || (route.both_ways && route.destination == Some(source))
            })
            .filter_map(|route| route.destination.map(|destination| (route.id, destination)))
            .collect();

        for (route, destination) in holding {
            self.silence_route(route, destination).await;
        }
    }

    /// Ends every network session ahead of the machine going to sleep.
    ///
    /// A peer that is not told invites again once its liveness check gives up, and each
    /// invitation woke a sleeping Mac on mains power for most of a minute (R-070). Sessions that
    /// made their connection make it again on waking.
    pub async fn suspend_sessions(&self) {
        let sessions: Vec<Arc<NetworkSession>> = self
            .sessions
            .read()
            .await
            .values()
            .map(Arc::clone)
            .collect();
        for session in sessions {
            session.suspend().await;
        }
    }

    /// Stops the notes every route is holding, ahead of the machine going to sleep.
    ///
    /// Route by route rather than endpoint by endpoint, because a network session or a Bluetooth
    /// link has no runtime entry of its own, and those are the destinations that go on sounding
    /// while this machine sleeps.
    async fn silence_before_sleep(self: &Arc<Self>) {
        let holding: Vec<(RouteId, EndpointId)> = self
            .router()
            .await
            .routes()
            .iter()
            .filter_map(|route| route.destination.map(|destination| (route.id, destination)))
            .collect();
        info!(
            routes = holding.len(),
            "the machine is going to sleep; releasing held notes"
        );
        for (route, destination) in holding {
            self.silence_route(route, destination).await;
        }
    }

    /// Stops every note any endpoint has sounding.
    ///
    /// Called on the way out. A daemon that stops with notes held leaves them sounding on
    /// hardware nothing is talking to any more, and starting it again does not clear them —
    /// the only thing that knew a note was playing has gone.
    pub async fn silence_all(&self) {
        let ids: Vec<EndpointId> = {
            let inner = self.inner.read().await;
            inner
                .runtime
                .keys()
                .chain(inner.automatic_ports.keys())
                .copied()
                .collect()
        };
        for id in ids {
            self.silence_endpoint(id).await;
        }
    }

    /// Ends every network session, telling each peer it is over.
    ///
    /// Called on the way out, after `silence_all`. A peer that is not told keeps the session in
    /// its list, and Apple's Network MIDI goes on sending its MIDI to the old ports, where the
    /// next daemon answers only with a goodbye Apple does not recognise.
    pub async fn end_sessions(&self) {
        let sessions: Vec<Arc<NetworkSession>> = self
            .sessions
            .write()
            .await
            .drain()
            .map(|(_, session)| session)
            .collect();
        for session in sessions {
            session.shutdown().await;
        }
    }

    /// Closes every port and device the platform backend holds.
    ///
    /// Called on the way out, after `end_sessions`, so they close while the process is whole
    /// rather than in its teardown.
    pub fn release_platform(&self) {
        self.midi.shutdown();
    }

    /// Records a session connecting, losing its connection, or failing to connect.
    async fn record_session_change(
        self: &Arc<Self>,
        session: EndpointId,
        name: &str,
        change: SessionNotice,
    ) {
        let (severity, detail) = match change {
            SessionNotice::Connected { peer, attempts: 0 } => {
                (Severity::Info, format!("{name} connected to {peer}"))
            }
            SessionNotice::Connected { peer, attempts } => (
                Severity::Info,
                format!(
                    "{name} connected to {peer} after {}",
                    failed_attempts(attempts)
                ),
            ),
            SessionNotice::Left {
                peer,
                reconnecting: false,
            } => (
                Severity::Info,
                format!("{peer} disconnected from {name}; {name} is listening again"),
            ),
            SessionNotice::Left {
                peer,
                reconnecting: true,
            } => (
                Severity::Warning,
                format!("{peer} disconnected from {name}; reconnecting"),
            ),
            SessionNotice::GuestLeft { peer } => (Severity::Info, format!("{peer} left {name}")),
            SessionNotice::Lost { peer, reason } => (
                Severity::Warning,
                format!("{name} lost its connection to {peer}: {reason}"),
            ),
            SessionNotice::CouldNotConnect { peer, reason } => (
                Severity::Warning,
                format!("{name} could not connect to {peer}: {reason}; retrying"),
            ),
            SessionNotice::Invitation(_) => return,
        };
        let now = self.clock.now();
        {
            let mut inner = self.inner.write().await;
            let mut event = events::event(EventKind::EndpointStateChanged, severity, now, detail);
            event.endpoint = Some(session);
            let _ = inner.events.record(event);
        }
        let _ = self.changes.send(Change::EndpointChanged(session));
    }

    /// Records an invitation that only the user can answer.
    ///
    /// A peer invites several times in a row and again after backing off, so the same peer is
    /// recorded once and kept until it is answered. Re-recording it would fill the log with one
    /// machine knocking.
    async fn record_invitation(self: &Arc<Self>, session: EndpointId, notice: InvitationNotice) {
        let id = format!("{session}:{}", notice.peer);
        {
            let mut waiting = self.invitations.write().await;
            if waiting.contains_key(&id) {
                return;
            }
            let _ = waiting.insert(
                id.clone(),
                PendingInvitation {
                    id: id.clone(),
                    session,
                    peer: notice.peer,
                    peer_name: notice.peer_name.clone(),
                    first_seen: self.clock.now(),
                },
            );
        }

        let who = notice.peer_name.unwrap_or_else(|| notice.peer.to_string());
        let now = self.clock.now();
        {
            let mut inner = self.inner.write().await;
            let mut event = events::event(
                EventKind::InvitationReceived,
                Severity::Warning,
                now,
                format!(
                    "'{who}' asked to connect to '{}'; waiting for an answer",
                    notice.session
                ),
            );
            event.endpoint = Some(session);
            let _ = inner.events.record(event);
        }
        let _ = self.changes.send(Change::EndpointChanged(session));
    }

    /// Returns the invitations waiting on the user, oldest first.
    ///
    /// An invitation stops waiting when it is answered, and also when it is settled some other
    /// way: the machine was trusted by address instead, or the session it wanted has since
    /// connected. A question that has already been answered by events is not a question, and
    /// leaving it on screen asks the user to decide something twice.
    pub async fn pending_invitations(&self) -> Vec<PendingInvitation> {
        let trusted = self.trusted_addresses().await;
        let busy: Vec<EndpointId> = {
            let sessions = self.sessions.read().await;
            let mut busy = Vec::new();
            for (id, session) in sessions.iter() {
                if session.status().await.state.phase() == ConnectionPhase::Connected {
                    busy.push(*id);
                }
            }
            busy
        };

        let mut waiting: Vec<PendingInvitation> = {
            let mut invitations = self.invitations.write().await;
            invitations.retain(|_, invitation| {
                let known = invitation.peer.ip().to_canonical();
                !busy.contains(&invitation.session)
                    && !trusted
                        .iter()
                        .any(|address| address.to_canonical() == known)
            });
            invitations.values().cloned().collect()
        };

        waiting.sort_by_key(|invitation| invitation.first_seen);
        waiting
    }

    /// Answers an invitation, optionally remembering the peer so it is never asked about again.
    ///
    /// Accepting does not complete the handshake that prompted this: the peer's invitation has a
    /// few seconds of patience and a person does not. It records the decision, so the invitation
    /// the peer is already sending, or the next one it sends, is accepted without asking.
    pub async fn respond_to_invitation(
        self: &Arc<Self>,
        invitation_id: &str,
        accept: bool,
        always: bool,
    ) -> Result<(), DaemonError> {
        let Some(invitation) = self.invitations.write().await.remove(invitation_id) else {
            return Err(DaemonError::NotFound(invitation_id.to_owned()));
        };

        if accept {
            if always {
                self.remember_peer(&invitation).await?;
            } else {
                let mut once = self.accepted_once.write().await;
                if !once.contains(&invitation.peer.ip()) {
                    once.push(invitation.peer.ip());
                }
            }
        }
        self.push_invitation_policy().await;
        info!(peer = %invitation.peer, accept, always, "invitation answered");
        Ok(())
    }

    /// Remembers a peer across restarts, so it is never asked about again.
    async fn remember_peer(
        self: &Arc<Self>,
        invitation: &PendingInvitation,
    ) -> Result<(), DaemonError> {
        let mut inner = self.inner.write().await;
        let address = invitation.peer.to_string();
        let name = invitation
            .peer_name
            .clone()
            .unwrap_or_else(|| invitation.peer.to_string());

        if let Some(existing) = inner
            .config
            .peers
            .iter_mut()
            .find(|peer| peer.addresses.iter().any(|known| known == &address))
        {
            existing.trusted = true;
            existing.name = name;
        } else {
            inner.config.peers.push(config::PeerConfig {
                id: midi_harbor_core::ids::PeerId::new(),
                name,
                addresses: vec![address],
                trusted: true,
                advertised_as: None,
                key: None,
                port_id: None,
            });
        }
        config::save(&self.paths, &inner.config)?;
        Ok(())
    }

    /// Returns the addresses of peers the user has accepted, remembered or not.
    async fn trusted_addresses(&self) -> Vec<std::net::IpAddr> {
        let mut addresses: Vec<std::net::IpAddr> = self.accepted_once.read().await.clone();
        let inner = self.inner.read().await;
        addresses.extend(
            inner
                .config
                .peers
                .iter()
                .filter(|peer| peer.trusted)
                .flat_map(|peer| peer.addresses.iter())
                .filter_map(|address| {
                    address
                        .parse::<SocketAddr>()
                        .map(|socket| socket.ip())
                        .or_else(|_| address.parse::<std::net::IpAddr>())
                        .ok()
                }),
        );
        addresses
    }

    /// Tells every session the current policy and the current set of trusted peers.
    pub(crate) async fn push_invitation_policy(self: &Arc<Self>) {
        let trusted = self.trusted_addresses().await;
        let policies: Vec<(EndpointId, InvitationPolicy)> = {
            let inner = self.inner.read().await;
            inner
                .config
                .endpoints
                .iter()
                .filter_map(|endpoint| match &endpoint.kind {
                    EndpointKind::NetworkSession(session) => {
                        Some((endpoint.id, session.invitation_policy))
                    }
                    _ => None,
                })
                .collect()
        };

        let sessions = self.sessions.read().await;
        for (id, policy) in policies {
            if let Some(session) = sessions.get(&id) {
                let _ = session.configure(policy, trusted.clone()).await;
            }
        }
    }

    /// Reports whether an endpoint is still open.
    async fn is_open(&self, id: EndpointId) -> bool {
        let inner = self.inner.read().await;
        inner.runtime.contains_key(&id) || inner.automatic_ports.contains_key(&id)
    }

    /// Delivers messages to every endpoint the route graph says should receive them.
    ///
    /// Direct destinations only, per the routing rules: a message goes where its source is
    /// routed and no further.
    pub async fn dispatch(self: &Arc<Self>, source: EndpointId, messages: &[MidiMessage]) {
        self.dispatch_from(source, 0, messages).await;
    }

    /// Delivers messages that arrived on one of a source's MIDI In connectors, counting from
    /// zero, to every endpoint the route graph says should receive them.
    pub async fn dispatch_from(
        self: &Arc<Self>,
        source: EndpointId,
        connector: u8,
        messages: &[MidiMessage],
    ) {
        let at = self.clock.now();

        // Traffic is recorded whether or not it goes anywhere. An endpoint carrying MIDI into a
        // system with no routes is doing something, and a display that shows nothing there sends
        // the user looking for a fault in the wrong place.
        self.observe(source, messages, false, at).await;

        // What another application sends a network port's automatic port goes out over the
        // network and nowhere else.
        if let Some(session) = self.automatic_port_owner(source).await {
            if self.deliver(session, None, messages).await {
                self.observe(session, messages, true, at).await;
                self.remember_sent(session, source, messages, at).await;
            }
            return;
        }

        let targets = self.targets(source, connector).await;
        let from_session = self.sessions.read().await.contains_key(&source);
        // What arrives over the network comes out of the automatic port too.
        if from_session
            && let Some((port, handle)) = self.automatic_port_of(source).await
            && self.deliver_through(port, Some(handle), 0, messages).await
        {
            self.observe(port, messages, true, at).await;
        }
        for (delivery, handle, route_counters) in targets {
            let id = delivery.destination;
            let to_session = self.sessions.read().await.contains_key(&id);

            // A route from one session to another is where a loop across machines closes.
            if from_session
                && to_session
                && self
                    .closes_loop(source, id, messages, route_counters.as_deref(), at)
                    .await
            {
                self.break_loop(delivery.route, source, id).await;
                continue;
            }

            if self
                .deliver_through(id, handle, delivery.connector, messages)
                .await
            {
                self.observe(id, messages, true, at).await;
                record_carried(route_counters.as_deref(), messages, at);
                if to_session {
                    self.remember_sent(id, source, messages, at).await;
                }
            } else {
                record_undelivered(route_counters.as_deref(), messages.len());
            }
        }
    }

    /// Records what went out through a session, so it is recognised if it comes back.
    async fn remember_sent(
        &self,
        session: EndpointId,
        source: EndpointId,
        messages: &[MidiMessage],
        at: jiff::Timestamp,
    ) {
        let inner = self.inner.read().await;
        let Some(runtime) = inner.session_runtime.get(&session) else {
            return;
        };
        if let Ok(mut sent) = runtime.sent.lock() {
            for message in messages {
                sent.record(*message, source, at);
            }
        }
    }

    /// Reports whether forwarding from one session to another would send back out what that
    /// session sent moments ago from somewhere else, often enough to be a loop.
    async fn closes_loop(
        &self,
        from: EndpointId,
        to: EndpointId,
        messages: &[MidiMessage],
        route: Option<&RouteRuntime>,
        at: jiff::Timestamp,
    ) -> bool {
        let Some(route) = route else {
            return false;
        };
        let echoes = {
            let inner = self.inner.read().await;
            let Some(runtime) = inner.session_runtime.get(&to) else {
                return false;
            };
            let Ok(sent) = runtime.sent.lock() else {
                return false;
            };
            messages
                .iter()
                .filter(|message| sent.came_back(message, from, at))
                .count()
        };
        let Ok(mut watch) = route.loops.lock() else {
            return false;
        };
        (0..echoes).fold(false, |tripped, _| watch.echo(at) || tripped)
    }

    /// Switches off the route that closed a loop across machines, and says so.
    ///
    /// Switched off rather than dropped message by message, so the loop stays broken across a
    /// restart, and the user sees a route switched off with the reason in the history instead of
    /// one that quietly carries less than it should.
    async fn break_loop(self: &Arc<Self>, route: RouteId, from: EndpointId, to: EndpointId) {
        let (from_name, to_name) = self
            .read(|config, _| {
                let name = |id: EndpointId| {
                    config
                        .endpoint(id)
                        .map(|endpoint| endpoint.name.to_string())
                        .unwrap_or_else(|| id.to_string())
                };
                (name(from), name(to))
            })
            .await;
        warn!(%route, from = %from_name, to = %to_name, "switched off a route closing a loop across machines");
        if let Err(error) = self.set_route_enabled(&route.to_string(), false).await {
            tracing::error!(%route, error = %error, "failed to switch off a route closing a loop");
            return;
        }
        let now = self.clock.now();
        let mut inner = self.inner.write().await;
        let _ = inner.events.record(events::event(
            EventKind::RouteValidityChanged,
            Severity::Warning,
            now,
            format!(
                "switched off the route from {from_name} to {to_name}: MIDI it sent out through \
                 {to_name} came straight back through {from_name}, so it closed a loop across \
                 machines; change the routes on the other machine, then switch it back on with \
                 'midi-harbor route enable {route}'"
            ),
        ));
    }

    /// Sends messages to one endpoint by whatever carries MIDI to it, reporting whether they went.
    ///
    /// The one place that knows every way MIDI reaches an endpoint: a Bluetooth link, the
    /// advertised peripheral, a network session, or a platform port. Routing and silencing both
    /// come through here. Silencing once knew only platform ports, so a note held on another
    /// machine or a Bluetooth synth was never released when its source went away.
    pub(crate) async fn deliver(
        &self,
        destination: EndpointId,
        handle: Option<PortHandle>,
        messages: &[MidiMessage],
    ) -> bool {
        self.deliver_through(destination, handle, 0, messages).await
    }

    /// Sends one note out of an endpoint for testing: the note-on now and the note-off after
    /// `length`.
    ///
    /// It is delivered and recorded as a route's traffic is, so a monitor shows it leaving, the
    /// counters count it, and it is silenced with the endpoint's other notes. The note-off is
    /// sent from a task of its own, so a client that goes away cannot leave the note sounding.
    pub async fn send_test_note(
        self: &Arc<Self>,
        id: EndpointId,
        channel: Channel,
        note: u8,
        velocity: u8,
        length: std::time::Duration,
    ) -> Result<(), DaemonError> {
        // Check it is something a route could send to, and switched on.
        let (name, handle) = {
            let inner = self.inner.read().await;
            let endpoint = inner
                .config
                .endpoint(id)
                .ok_or_else(|| DaemonError::NotFound(id.to_string()))?;
            let name = endpoint.name.as_str().to_owned();
            if !endpoint.can_sink() {
                return Err(DaemonError::CannotSend(format!(
                    "{name} only sends MIDI, so nothing can be sent out of it"
                )));
            }
            if !endpoint.enabled {
                return Err(DaemonError::CannotSend(format!(
                    "{name} is switched off; switch it on to send it a note"
                )));
            }
            (
                name,
                inner.runtime.get(&id).and_then(|runtime| runtime.handle),
            )
        };

        // Send the note-on.
        let on = [MidiMessage::NoteOn {
            channel,
            note,
            velocity,
        }];
        if !self.deliver(id, handle, &on).await {
            return Err(DaemonError::CannotSend(format!(
                "{name} is not connected, so the note could not be sent"
            )));
        }
        self.observe(id, &on, true, self.clock.now()).await;
        info!(
            endpoint = %name,
            channel = channel.number(),
            note,
            velocity,
            "sent a test note"
        );

        // Send the note-off once it has sounded for its length.
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(length).await;
            let off = [MidiMessage::NoteOff {
                channel,
                note,
                velocity: 0,
            }];
            let handle = daemon
                .inner
                .read()
                .await
                .runtime
                .get(&id)
                .and_then(|runtime| runtime.handle);
            if daemon.deliver(id, handle, &off).await {
                daemon.observe(id, &off, true, daemon.clock.now()).await;
            }
        });
        Ok(())
    }

    /// Sends messages to one endpoint through one of its MIDI Out connectors, counting from
    /// zero, reporting whether they went. Only a virtual port has more than one.
    pub(crate) async fn deliver_through(
        &self,
        destination: EndpointId,
        handle: Option<PortHandle>,
        connector: u8,
        messages: &[MidiMessage],
    ) -> bool {
        if let Some(link) = self.bt_links.read().await.get(&destination).copied() {
            return match self.bluetooth.send(link, messages) {
                Ok(()) => true,
                Err(error) => {
                    debug!(endpoint = %destination, error = %error, "could not deliver midi over bluetooth");
                    false
                }
            };
        }
        if *self.bt_advertised.read().await == Some(destination) {
            // With no device subscribed the radio sends nothing, and counting it as sent showed
            // traffic leaving a port nobody was listening to.
            if self.bt_centrals.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                return false;
            }
            return match self.bluetooth.notify(messages) {
                Ok(()) => true,
                Err(error) => {
                    debug!(endpoint = %destination, error = %error, "could not notify subscribed centrals");
                    false
                }
            };
        }
        let session = self.sessions.read().await.get(&destination).map(Arc::clone);
        if let Some(session) = session {
            let sent = session.send(messages.to_vec()).await;
            if !sent {
                debug!(endpoint = %destination, "could not hand midi to the session");
            }
            return sent;
        }
        let Some(handle) = handle else {
            return false;
        };
        match self.midi.send_to(handle, connector, messages) {
            Ok(()) => true,
            Err(error) => {
                debug!(endpoint = %destination, error = %error, "could not deliver midi");
                false
            }
        }
    }

    /// Delivers one whole system-exclusive message along the route graph.
    ///
    /// Separate from `dispatch` because a dump carries its own framing and is unbounded, so it
    /// cannot travel as a `MidiMessage`. It reaches the same destinations by the same rules.
    pub async fn dispatch_sysex(self: &Arc<Self>, source: EndpointId, bytes: &Arc<[u8]>) {
        self.dispatch_sysex_from(source, 0, bytes).await;
    }

    /// Delivers one whole system-exclusive message that arrived on one of a source's MIDI In
    /// connectors, counting from zero, along the route graph.
    pub async fn dispatch_sysex_from(
        self: &Arc<Self>,
        source: EndpointId,
        connector: u8,
        bytes: &Arc<[u8]>,
    ) {
        let at = self.clock.now();
        self.observe_sysex(source, bytes, false, at).await;

        if let Some(session) = self.automatic_port_owner(source).await {
            let running = self.sessions.read().await.get(&session).map(Arc::clone);
            if let Some(running) = running
                && running.send_sysex(Arc::clone(bytes)).await
            {
                self.observe_sysex(session, bytes, true, at).await;
            }
            return;
        }
        if let Some((port, handle)) = self.automatic_port_of(source).await {
            match self.midi.send_sysex_to(handle, 0, bytes) {
                Ok(()) => self.observe_sysex(port, bytes, true, at).await,
                Err(error) => {
                    debug!(error = %error, "could not deliver system-exclusive to an automatic port");
                }
            }
        }

        let targets = self.targets(source, connector).await;
        if targets.is_empty() {
            return;
        }
        // Copied rather than held: handing a dump to a session waits on its queue, and holding
        // the lock through that would stall anything starting or stopping a session.
        let sessions = self.sessions.read().await.clone();
        let links = self.bt_links.read().await;
        let advertised = *self.bt_advertised.read().await;

        for (delivery, handle, route_counters) in targets {
            let id = delivery.destination;
            // The advertised port has no platform handle, so without its own branch a dump routed
            // to it reached the end of this loop and vanished uncounted.
            if advertised == Some(id) {
                if self.bt_centrals.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                    record_undelivered(route_counters.as_deref(), 1);
                    continue;
                }
                match self.bluetooth.notify_sysex(bytes) {
                    Ok(()) => {
                        self.observe_sysex(id, bytes, true, at).await;
                        record_carried_bytes(route_counters.as_deref(), bytes.len(), at);
                    }
                    Err(error) => {
                        debug!(endpoint = %id, error = %error, "could not notify subscribed centrals of system-exclusive");
                        record_undelivered(route_counters.as_deref(), 1);
                    }
                }
                continue;
            }
            if let Some(link) = links.get(&id) {
                match self.bluetooth.send_sysex(*link, bytes) {
                    Ok(()) => {
                        self.observe_sysex(id, bytes, true, at).await;
                        record_carried_bytes(route_counters.as_deref(), bytes.len(), at);
                    }
                    Err(error) => {
                        debug!(endpoint = %id, error = %error, "could not deliver system-exclusive over bluetooth");
                        record_undelivered(route_counters.as_deref(), 1);
                    }
                }
                continue;
            }
            if let Some(session) = sessions.get(&id) {
                if session.send_sysex(Arc::clone(bytes)).await {
                    self.observe_sysex(id, bytes, true, at).await;
                    record_carried_bytes(route_counters.as_deref(), bytes.len(), at);
                } else {
                    debug!(endpoint = %id, "could not hand system-exclusive to the session");
                    record_undelivered(route_counters.as_deref(), 1);
                }
                continue;
            }
            let Some(handle) = handle else {
                continue;
            };
            match self.midi.send_sysex_to(handle, delivery.connector, bytes) {
                Ok(()) => {
                    self.observe_sysex(id, bytes, true, at).await;
                    record_carried_bytes(route_counters.as_deref(), bytes.len(), at);
                }
                Err(error) => {
                    debug!(endpoint = %id, error = %error, "could not deliver system-exclusive");
                    record_undelivered(route_counters.as_deref(), 1);
                }
            }
        }
    }

    /// Resolves where traffic from a source goes, with the counters of the route carrying it.
    ///
    /// Network sessions and platform endpoints are reached differently, so each destination is
    /// resolved to whichever it is. The route's counters come from the same pass, because
    /// attributing traffic to a route afterwards would mean taking the lock twice.
    async fn targets(
        self: &Arc<Self>,
        source: EndpointId,
        connector: u8,
    ) -> Vec<(Delivery, Option<PortHandle>, Option<Arc<RouteRuntime>>)> {
        let deliveries: Vec<Delivery> = {
            let router = self.router().await;
            router.deliveries(source, connector).to_vec()
        };
        if deliveries.is_empty() {
            return Vec::new();
        }

        let inner = self.inner.read().await;
        deliveries
            .iter()
            .map(|delivery| {
                (
                    *delivery,
                    inner
                        .runtime
                        .get(&delivery.destination)
                        .and_then(|runtime| runtime.handle),
                    inner.route_counters.get(&delivery.route).map(Arc::clone),
                )
            })
            .collect()
    }

    /// Records a system-exclusive message against an endpoint and shows it to anyone watching.
    async fn observe_sysex(
        &self,
        id: EndpointId,
        bytes: &Arc<[u8]>,
        outbound: bool,
        at: jiff::Timestamp,
    ) {
        let inner = self.inner.read().await;
        let Some(runtime) = inner.traffic(id) else {
            return;
        };

        let len = u64::try_from(bytes.len()).unwrap_or(0);
        if outbound {
            runtime.counters.record_sent(len, at);
        } else {
            runtime.counters.record_received(len, at);
        }
        let _ = runtime.monitor.send(Observed {
            at,
            seen: Seen::SysEx(Arc::clone(bytes)),
            outbound,
        });
    }

    /// Adds what an endpoint's input discarded to its counters.
    ///
    /// The ring counts both where the platform can reach it without a lock; they reach the
    /// endpoint here, where a client reading its counters sees them.
    async fn record_discards(&self, id: EndpointId, dropped: u64, malformed: u64) {
        let inner = self.inner.read().await;
        let Some(runtime) = inner
            .runtime
            .get(&id)
            .or_else(|| inner.session_runtime.get(&id))
        else {
            return;
        };
        if dropped > 0 {
            runtime.counters.record_dropped(dropped);
        }
        if malformed > 0 {
            runtime.counters.record_malformed(malformed);
        }
    }

    /// Records messages against an endpoint's counters and publishes them to anyone watching.
    pub(crate) async fn observe(
        &self,
        id: EndpointId,
        messages: &[MidiMessage],
        outbound: bool,
        at: jiff::Timestamp,
    ) {
        let inner = self.inner.read().await;
        let Some(runtime) = inner.traffic(id) else {
            return;
        };

        for message in messages {
            let bytes = u64::try_from(message.len()).unwrap_or(0);
            if outbound {
                runtime.counters.record_sent(bytes, at);
                // Only what leaves is tracked: a note sounds on whatever received it, and this
                // endpoint is what will have to stop it.
                if let Ok(mut sounding) = runtime.sounding.lock() {
                    sounding.record(message);
                }
                if let Ok(mut controls) = runtime.controls.lock() {
                    controls.record(message);
                }
            } else {
                runtime.counters.record_received(bytes, at);
            }

            // Sending fails only when nothing is watching, which is the normal case and not
            // worth a branch anywhere else.
            let _ = runtime.monitor.send(Observed {
                at,
                seen: Seen::Message(*message),
                outbound,
            });
        }
    }

    /// Subscribes to the messages passing through an endpoint.
    pub async fn watch_endpoint(&self, id: EndpointId) -> Option<broadcast::Receiver<Observed>> {
        // A Bluetooth endpoint carries MIDI over a link, or as the advertised port, and never has
        // a platform handle, so `monitor` refused every one of them however connected it was.
        // Read before the main lock and released, never held across it.
        let over_bluetooth = self.bt_links.read().await.contains_key(&id)
            || *self.bt_advertised.read().await == Some(id);
        let inner = self.inner.read().await;
        // A runtime entry exists even for an endpoint that is switched off, so the handle is what
        // decides. Subscribing to one without a handle hands back a stream that can never carry
        // anything, and a user waits at it believing nothing is being sent.
        inner
            .runtime
            .get(&id)
            .filter(|runtime| runtime.handle.is_some() || over_bluetooth)
            // A session has no platform handle; its entry exists only while it runs.
            .or_else(|| inner.session_runtime.get(&id))
            .map(|runtime| runtime.monitor.subscribe())
    }

    /// Returns an endpoint's traffic counters.
    pub async fn counters(
        &self,
        id: EndpointId,
    ) -> Option<midi_harbor_core::counters::CounterSnapshot> {
        let inner = self.inner.read().await;
        inner
            .traffic(id)
            .map(|runtime| runtime.counters.snapshot())
            .or_else(|| inner.unplugged.get(&id).map(|counters| counters.snapshot()))
    }

    /// Returns every route's traffic counters, keyed by route.
    pub async fn route_counters(
        &self,
    ) -> HashMap<RouteId, midi_harbor_core::counters::CounterSnapshot> {
        let inner = self.inner.read().await;
        inner
            .route_counters
            .iter()
            .map(|(id, route)| (*id, route.counters.snapshot()))
            .collect()
    }

    /// Returns every endpoint's traffic counters.
    pub async fn all_counters(
        &self,
    ) -> Vec<(EndpointId, midi_harbor_core::counters::CounterSnapshot)> {
        let inner = self.inner.read().await;
        inner
            .runtime
            .iter()
            .chain(inner.session_runtime.iter())
            .map(|(id, runtime)| (*id, runtime.counters.snapshot()))
            .collect()
    }

    /// Returns the traffic counters of every open automatic port, keyed by its network port.
    pub async fn automatic_port_counters(
        &self,
    ) -> Vec<(EndpointId, midi_harbor_core::counters::CounterSnapshot)> {
        let inner = self.inner.read().await;
        inner
            .automatic_ports
            .values()
            .map(|port| (port.session, port.runtime.counters.snapshot()))
            .collect()
    }
}

/// Says how many attempts failed, as a person would: "1 failed attempt", "3 failed attempts".
pub(crate) fn failed_attempts(count: u32) -> String {
    if count == 1 {
        "1 failed attempt".to_owned()
    } else {
        format!("{count} failed attempts")
    }
}

#[cfg(test)]
mod known_peer_tests {
    use super::{DiscoveredPeer, config, merge_known_peers};
    use midi_harbor_core::ids::PeerId;
    use std::net::{IpAddr, Ipv4Addr};

    /// Builds an Apple MIDI advertisement from 192.0.2.10 as discovery labels it.
    fn advertised(name: &str, port: u16) -> (DiscoveredPeer, String) {
        let peer = DiscoveredPeer {
            id: PeerId::new(),
            name: name.to_owned(),
            fullname: format!("{name}._apple-midi._udp.local."),
            addresses: vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))],
            port,
            is_self: false,
            identity: None,
        };
        (peer, name.to_owned())
    }

    /// Proves how remembered and advertised machines merge into one list. A trusted machine is
    /// matched by host, so its advertisement marks it discovered under the name the user gave it
    /// instead of listing it twice; two sessions one machine advertises, which Apple's Network
    /// MIDI lets a Mac run side by side, are each listed when nothing remembers that host.
    ///
    /// Regression: a machine remembered without trust at a port it no longer listened on was
    /// matched by host too, so another session that host advertised was listed as the old
    /// connection, on this network, at the dead port.
    #[test]
    fn a_machine_is_listed_once_however_it_is_known() {
        let studio_mac = config::PeerConfig {
            id: PeerId::new(),
            name: "Studio Mac".to_owned(),
            addresses: vec!["192.0.2.10:5004".to_owned()],
            trusted: true,
            advertised_as: None,
            key: None,
            port_id: None,
        };
        let old_connection = config::PeerConfig {
            id: PeerId::new(),
            name: "192.0.2.10".to_owned(),
            addresses: vec!["192.0.2.10:5004".to_owned()],
            trusted: false,
            advertised_as: None,
            key: None,
            port_id: None,
        };
        let cases = [
            (
                "two sessions advertised by one machine are both listed",
                vec![],
                vec![advertised("Apple Two", 5006), advertised("Studio", 5004)],
                vec![("Apple Two", true, false), ("Studio", true, false)],
            ),
            (
                "a trusted machine that advertises is listed once, as remembered",
                vec![studio_mac.clone()],
                vec![advertised("Studio", 5004)],
                vec![("Studio Mac", true, true)],
            ),
            (
                "a trusted machine is marked by a session on any port of its host",
                vec![studio_mac],
                vec![advertised("Studio", 5006)],
                vec![("Studio Mac", true, true)],
            ),
            (
                "an untrusted address is marked by the session advertised there",
                vec![old_connection.clone()],
                vec![advertised("Studio", 5004)],
                vec![("192.0.2.10", true, false)],
            ),
            (
                "an untrusted address does not take a session on another port",
                vec![old_connection],
                vec![advertised("Studio", 5006)],
                vec![("192.0.2.10", false, false), ("Studio", true, false)],
            ),
        ];
        for (name, remembered, discovered, want) in cases {
            let known = merge_known_peers(&remembered, discovered);
            let listed: Vec<(&str, bool, bool)> = known
                .iter()
                .map(|peer| (peer.name.as_str(), peer.discovered, peer.trusted))
                .collect();
            assert_eq!(
                listed, want,
                "{name}: the names, discovered flags and trust listed are wrong"
            );
        }
    }
}
