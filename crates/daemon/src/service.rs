//! The gRPC service implementation.
//!
//! Calls that are not yet built return `UNIMPLEMENTED` rather than a plausible empty answer, so a
//! client can tell "not built yet" from "nothing configured".

use crate::network_port::NetworkPortChange;
use crate::state::{Change, Daemon, DaemonError, RouteRequest, Seen};
use midi_harbor_core::endpoint::{
    BleRole, Direction, Endpoint as DomainEndpoint, EndpointKind, InvitationPolicy,
};
use midi_harbor_core::failure::FailureReason;
use midi_harbor_core::fingerprint::MatchConfidence;
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::state::{ConnectionPhase, ConnectionState};
use midi_harbor_core::time::{Clock, SystemClock};
use midi_harbor_ipc::pb::{self, harbor_server::Harbor};
use midi_harbor_ipc::status::IntoStatus;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

/// How many stream items a subscriber may buffer before the daemon considers it lagging.
const STREAM_BUFFER: usize = 64;

/// How often coalesced traffic updates are emitted.
///
/// Counters move on every note; a display refreshing that fast is unreadable as well as wasteful.
const TRAFFIC_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// How often the event history is checked for anything new.
const EVENT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Most events delivered from one poll, so a burst cannot monopolise the stream.
const EVENT_PAGE: usize = 256;

/// Convenience alias for the streaming response type.
type StreamResponse<T> =
    Result<Response<Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>>, Status>;

/// Serves the Harbor contract from daemon state.
pub struct HarborService {
    daemon: Arc<Daemon>,
}

impl HarborService {
    /// Wraps a running daemon.
    pub fn new(daemon: Arc<Daemon>) -> Self {
        Self { daemon }
    }

    /// Returns the current wire representation of an endpoint.
    async fn endpoint_response(&self, id: EndpointId) -> Result<Response<pb::Endpoint>, Status> {
        let proto = self
            .daemon
            .read(|config, runtime| {
                config
                    .endpoint(id)
                    .map(|endpoint| to_proto_endpoint(endpoint, runtime.get(&id)))
            })
            .await;
        match proto {
            Some(endpoint) => Ok(Response::new(
                with_session_state(&self.daemon, endpoint).await,
            )),
            None => Err(Status::not_found(format!("{id} does not exist"))),
        }
    }

    /// Attaches an endpoint's traffic counters.
    ///
    /// Without this a display can say "connected" but never "connected and carrying nothing",
    /// which are different problems with different causes.
    async fn with_counters(&self, mut endpoint: pb::Endpoint) -> pb::Endpoint {
        let Ok(id) = EndpointId::parse(&endpoint.id) else {
            return endpoint;
        };
        if let Some(snapshot) = self.daemon.counters(id).await {
            endpoint.counters = Some(to_proto_counters(&snapshot));
        }
        // A network port's traffic says what the network carried; its automatic port's says
        // whether that reached the applications on this computer, which is the other half of
        // asking whether a message arrived.
        if let Some(pb::endpoint::Detail::NetworkSession(detail)) = &mut endpoint.detail {
            detail.automatic_port_counters = self
                .daemon
                .counters(crate::automatic::port_id(id))
                .await
                .map(|snapshot| to_proto_counters(&snapshot));
        }
        endpoint
    }

    /// Resolves a peer reference into an address to connect to.
    ///
    /// Tried in order: a literal address, then a discovered peer, then a hostname. Discovery is
    /// checked before hostname resolution deliberately — a peer's advertised name could otherwise
    /// resolve to some unrelated machine and we would connect to the wrong thing.
    ///
    /// Connecting by address is a first-class path, not a fallback: a network that filters
    /// multicast breaks discovery, and a peer whose address is known should still be reachable.
    async fn resolve_peer(&self, reference: &str) -> Result<SocketAddr, Status> {
        if let Some(address) = parse_literal_address(reference) {
            return Ok(address);
        }

        let peers = self.daemon.peers();
        let found = peers
            .iter()
            .find(|(peer, label)| peer.id.to_string() == reference || label == reference)
            .or_else(|| peers.iter().find(|(peer, _)| peer.name == reference));
        if let Some(address) = found.and_then(|(peer, _)| peer.address()) {
            return Ok(address);
        }

        // A hostname, resolved without blocking the runtime.
        if let Some(address) = resolve_hostname(reference).await {
            return Ok(address);
        }

        Err(Status::not_found(format!(
            "no peer named '{reference}'\n\
             run 'midi-harbor session discover' to list peers, \
             or give an address directly as HOST or HOST:PORT \
             (for example 192.0.2.3 or 192.0.2.3:5004)"
        )))
    }
}

/// Parses a literal IP address, with or without a port.
///
/// A bare address is accepted because that is what a user reads off another machine, and a bare
/// address uses the standard RTP-MIDI port. Anything that is not an address returns `None` so it
/// can be tried as a peer name instead.
pub(crate) fn parse_literal_address(reference: &str) -> Option<SocketAddr> {
    if let Ok(address) = reference.parse::<SocketAddr>() {
        return Some(address);
    }
    reference
        .parse::<std::net::IpAddr>()
        .ok()
        .map(|ip| SocketAddr::new(ip, crate::net::DEFAULT_CONTROL_PORT))
}

/// Resolves a hostname, adding the standard port when none was given.
///
/// Uses the runtime's resolver rather than the blocking one, since a name that does not resolve
/// takes as long as the system's timeout and would otherwise stall every other request.
///
/// A name commonly resolves to several addresses, and the first is often a link-local IPv6 one
/// that needs a scope to be usable. The same routability preference discovered peers get is
/// applied here, so typing a hostname and picking a peer from a list behave the same way.
async fn resolve_hostname(reference: &str) -> Option<SocketAddr> {
    // A name with a space cannot be a hostname, and looking one up only wastes a timeout.
    if reference.contains(' ') {
        return None;
    }
    let (host, port) = split_host_port(reference);
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .ok()?
        .collect();

    let addresses: Vec<std::net::IpAddr> = resolved.iter().map(SocketAddr::ip).collect();
    crate::net::choose_peer_address(&addresses, port)
}

/// Splits a reference into a host and a port, defaulting the port when none was given.
fn split_host_port(reference: &str) -> (String, u16) {
    // A bracketed IPv6 literal with a port, which rsplit would otherwise cut in the wrong place.
    if let Some(rest) = reference.strip_prefix('[')
        && let Some((host, port)) = rest.split_once("]:")
    {
        return (
            host.to_owned(),
            port.parse().unwrap_or(crate::net::DEFAULT_CONTROL_PORT),
        );
    }
    match reference.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => (
            host.to_owned(),
            port.parse().unwrap_or(crate::net::DEFAULT_CONTROL_PORT),
        ),
        _ => (reference.to_owned(), crate::net::DEFAULT_CONTROL_PORT),
    }
}

/// Returns the control port to bind, zero meaning "choose one".
///
/// Defaulting to Apple's port would collide with a Network MIDI session already running on this
/// machine, so an unspecified port asks the system instead. A number no UDP port has is refused:
/// reading it as zero put the network port somewhere the user did not ask for.
fn udp_port(requested: u32) -> Result<u16, DaemonError> {
    u16::try_from(requested).map_err(|_| {
        FailureReason::ConfigInvalid {
            detail: format!("{requested} is not a UDP port; choose 1 to 65534, or 0 for any"),
        }
        .into()
    })
}

/// Converts a match confidence into its wire representation.
fn to_proto_confidence(confidence: MatchConfidence) -> i32 {
    let mapped = match confidence {
        MatchConfidence::None => pb::MatchConfidence::None,
        MatchConfidence::Ambiguous => pb::MatchConfidence::Ambiguous,
        MatchConfidence::Probable => pb::MatchConfidence::Probable,
        MatchConfidence::Exact => pb::MatchConfidence::Exact,
    };
    mapped as i32
}

/// Converts a domain invitation policy into its wire representation.
fn to_proto_policy(policy: InvitationPolicy) -> i32 {
    let mapped = match policy {
        InvitationPolicy::Prompt => pb::InvitationPolicy::Prompt,
        InvitationPolicy::AcceptKnown => pb::InvitationPolicy::AcceptKnown,
        InvitationPolicy::AcceptAll => pb::InvitationPolicy::AcceptAll,
        InvitationPolicy::RejectAll => pb::InvitationPolicy::RejectAll,
    };
    mapped as i32
}

/// Converts a wire invitation policy into the domain, defaulting to prompting.
fn from_proto_policy(value: i32) -> InvitationPolicy {
    match pb::InvitationPolicy::try_from(value) {
        Ok(pb::InvitationPolicy::AcceptKnown) => InvitationPolicy::AcceptKnown,
        Ok(pb::InvitationPolicy::AcceptAll) => InvitationPolicy::AcceptAll,
        Ok(pb::InvitationPolicy::RejectAll) => InvitationPolicy::RejectAll,
        // Prompting is the default because silently accepting inbound connections is a
        // surprising thing for a program to do.
        _ => InvitationPolicy::Prompt,
    }
}

impl From<DaemonError> for Status {
    fn from(error: DaemonError) -> Self {
        match error {
            DaemonError::Failure(reason) => reason.into_status(),
            DaemonError::NotFound(what) => Status::not_found(format!("{what} does not exist")),
            DaemonError::Ambiguous { .. } => Status::invalid_argument(error.to_string()),
            DaemonError::InvalidName(_) | DaemonError::InvalidRoute(_) => {
                Status::invalid_argument(error.to_string())
            }
            DaemonError::ConfirmationRequired { detail } => Status::failed_precondition(detail),
            DaemonError::CannotSend(detail) => Status::failed_precondition(detail),
            DaemonError::Config(_) => FailureReason::ConfigInvalid {
                detail: error.to_string(),
            }
            .into_status(),
        }
    }
}

/// Converts a domain timestamp into the protobuf representation.
fn to_proto_time(at: jiff::Timestamp) -> Option<prost_types::Timestamp> {
    Some(prost_types::Timestamp {
        seconds: at.as_second(),
        nanos: at.subsec_nanosecond(),
    })
}

/// Converts a domain phase into its wire representation.
fn to_proto_phase(phase: ConnectionPhase) -> i32 {
    let mapped = match phase {
        ConnectionPhase::Disabled => pb::ConnectionPhase::Disabled,
        ConnectionPhase::Disconnected => pb::ConnectionPhase::Disconnected,
        ConnectionPhase::Connecting => pb::ConnectionPhase::Connecting,
        ConnectionPhase::Connected => pb::ConnectionPhase::Connected,
        ConnectionPhase::Retrying => pb::ConnectionPhase::Retrying,
        ConnectionPhase::Unavailable => pb::ConnectionPhase::Unavailable,
    };
    mapped as i32
}

/// Converts a domain direction into its wire representation.
fn to_proto_direction(direction: Direction) -> i32 {
    let mapped = match direction {
        Direction::Input => pb::Direction::Input,
        Direction::Output => pb::Direction::Output,
        Direction::Bidirectional => pb::Direction::Bidirectional,
    };
    mapped as i32
}

/// Converts a domain failure into its wire representation.
fn to_proto_reason(reason: &FailureReason) -> pb::FailureReason {
    pb::FailureReason {
        code: reason.code().to_owned(),
        message: reason.to_string(),
        guidance: reason.guidance().unwrap_or_default(),
        needs_user_action: reason.needs_user_action(),
    }
}

/// Merges a running session's state into its endpoint.
///
/// A session's lifecycle lives in its supervisor rather than in the virtual-port runtime map,
/// so without this a configured session reports as having no state at all.
async fn with_session_state(daemon: &Daemon, mut endpoint: pb::Endpoint) -> pb::Endpoint {
    if endpoint.kind != pb::EndpointKind::NetworkSession as i32 {
        return endpoint;
    }
    let Ok(id) = EndpointId::parse(&endpoint.id) else {
        return endpoint;
    };
    let Some(status) = daemon.session_status(id).await else {
        // Only a disabled session has no supervisor, and saying so beats reporting no state.
        if !endpoint.enabled {
            endpoint.state = Some(pb::ConnectionState {
                phase: pb::ConnectionPhase::Disabled as i32,
                ..pb::ConnectionState::default()
            });
        }
        return endpoint;
    };

    let mut state = to_proto_state(&status.state);
    state.waiting_for_network = status.waiting_for_network;
    endpoint.state = Some(state);
    // Loss and recovery come from the session; the rest is already counted on the endpoint.
    let mut counters = endpoint.counters.unwrap_or_default();
    counters.messages_lost = status.lost;
    counters.messages_recovered = status.recovered;
    endpoint.counters = Some(counters);

    // The bound port is what the supervisor actually got, which differs from the requested
    // one whenever the system chose.
    if let Some(pb::endpoint::Detail::NetworkSession(detail)) = &mut endpoint.detail {
        detail.control_port = u32::from(status.control_port);
        detail.peer_id = status.peer_address.map(|address| address.to_string());
        detail.guests = status.guests.clone();
        detail.machines = status
            .machines
            .iter()
            .map(|machine| pb::NetworkMachine {
                address: machine.address.to_string(),
                name: machine.name.clone(),
                invited: machine.invited,
                joined: machine.joined,
                round_trip_us: machine
                    .round_trip
                    .and_then(|round_trip| u64::try_from(round_trip.as_micros()).ok()),
            })
            .collect();
        detail.round_trip_us = u64::try_from(status.round_trip.as_micros())
            .ok()
            .filter(|micros| *micros > 0);
    }
    endpoint
}

/// Builds one endpoint as every reply and every stream reports it: stored configuration, runtime
/// state, and for a session the state its supervisor holds.
///
/// Some replies and the state stream built endpoints without the session part, so a session read
/// differently depending on which call a client made, and a client patching its cache from the
/// stream lost every session's state.
async fn endpoint_as_reported(daemon: &Daemon, id: EndpointId) -> Option<pb::Endpoint> {
    let endpoint = daemon
        .read(|config, runtime| {
            config
                .endpoint(id)
                .map(|endpoint| to_proto_endpoint(endpoint, runtime.get(&id)))
        })
        .await?;
    Some(with_session_state(daemon, endpoint).await)
}

/// Builds the wire representation of a connection's state.
///
/// Whether a link is unstable depends on how long its current connection has lasted, so it is
/// judged against the system clock, which is the clock every daemon runs on.
fn to_proto_state(state: &ConnectionState) -> pb::ConnectionState {
    pb::ConnectionState {
        phase: to_proto_phase(state.phase()),
        since: to_proto_time(state.since()),
        last_error: state.last_error().map(to_proto_reason),
        attempt: state.attempt(),
        next_retry: state.next_retry().and_then(to_proto_time),
        unstable: state.is_unstable(SystemClock.now()),
        // Only a session can tell, from how its sends fail, and it fills this in itself.
        waiting_for_network: false,
    }
}

/// Builds the wire representation of an endpoint, including its runtime state.
fn to_proto_endpoint(
    endpoint: &DomainEndpoint,
    runtime: Option<&crate::state::Runtime>,
) -> pb::Endpoint {
    let state = runtime.map(|runtime| to_proto_state(&runtime.state));

    let (kind, detail) = match &endpoint.kind {
        EndpointKind::VirtualPort(port) => (
            pb::EndpointKind::VirtualPort,
            Some(pb::endpoint::Detail::VirtualPort(pb::VirtualPortDetail {
                platform_unique_id: port.output_ids.first().copied(),
                inputs: u32::from(port.inputs),
                outputs: u32::from(port.outputs),
            })),
        ),
        EndpointKind::PhysicalDevice(device) => (
            pb::EndpointKind::PhysicalDevice,
            Some(pb::endpoint::Detail::PhysicalDevice(
                pb::PhysicalDeviceDetail {
                    fingerprint: Some(pb::DeviceFingerprint {
                        unique_id: device.fingerprint.unique_id,
                        usb_serial: device.fingerprint.usb_serial.clone(),
                        manufacturer: device.fingerprint.manufacturer.clone(),
                        model: device.fingerprint.model.clone(),
                        name: device.fingerprint.name.clone(),
                        topology_path: device.fingerprint.topology_path.clone(),
                    }),
                    present: device.present,
                    confidence: to_proto_confidence(device.confidence),
                    claimed_by: device.claimed_by.clone(),
                    software: device.software,
                },
            )),
        ),
        EndpointKind::NetworkSession(session) => (
            pb::EndpointKind::NetworkSession,
            Some(pb::endpoint::Detail::NetworkSession(
                pb::NetworkSessionDetail {
                    local_name: session.local_name.as_str().to_owned(),
                    control_port: u32::from(session.control_port),
                    peer_id: None,
                    invitation_policy: to_proto_policy(session.invitation_policy),
                    last_sync_age_ms: None,
                    clock_offset_ns: None,
                    round_trip_us: None,
                    guests: Vec::new(),
                    automatic_port: session.automatic_port,
                    machines: Vec::new(),
                    automatic_port_counters: None,
                },
            )),
        ),
        EndpointKind::BluetoothDevice(device) => (
            pb::EndpointKind::BluetoothDevice,
            Some(pb::endpoint::Detail::BluetoothDevice(
                pb::BluetoothDeviceDetail {
                    address: device.address.clone(),
                    paired: device.paired,
                    rssi: device.rssi.map(i32::from),
                    peripheral_role: device.role == BleRole::Peripheral,
                },
            )),
        ),
    };

    pb::Endpoint {
        id: endpoint.id.to_string(),
        name: endpoint.name.as_str().to_owned(),
        kind: kind as i32,
        enabled: endpoint.enabled,
        direction: to_proto_direction(endpoint.direction),
        state,
        counters: None,
        detail,
    }
}

#[tonic::async_trait]
impl Harbor for HarborService {
    async fn stop_daemon(
        &self,
        _request: Request<pb::StopDaemonRequest>,
    ) -> Result<Response<pb::StopDaemonResponse>, Status> {
        // Answered first: serving then ends gracefully, letting this reply reach the client.
        self.daemon.request_stop();
        Ok(Response::new(pb::StopDaemonResponse {}))
    }

    async fn get_server_info(
        &self,
        _request: Request<pb::GetServerInfoRequest>,
    ) -> Result<Response<pb::ServerInfo>, Status> {
        Ok(Response::new(pb::ServerInfo {
            daemon_version: midi_harbor_core::VERSION.to_owned(),
            protocol_major: midi_harbor_ipc::PROTOCOL_MAJOR,
            protocol_minor: midi_harbor_ipc::PROTOCOL_MINOR,
            started_at: to_proto_time(self.daemon.started_at()),
            config_path: self.daemon.paths().config_file().display().to_string(),
            socket_path: self.daemon.paths().socket_file().display().to_string(),
        }))
    }

    async fn get_status(
        &self,
        _request: Request<pb::GetStatusRequest>,
    ) -> Result<Response<pb::StatusSummary>, Status> {
        let server = self
            .get_server_info(Request::new(pb::GetServerInfoRequest {}))
            .await?;
        let connected = self.daemon.connected_count().await;

        let (endpoints, routes, broken) = self
            .daemon
            .read(|config, _| {
                let broken = config
                    .routes
                    .iter()
                    .filter(|route| config.resolve_route(route).is_err())
                    .count();
                (config.endpoints.len(), config.routes.len(), broken)
            })
            .await;

        Ok(Response::new(pb::StatusSummary {
            server: Some(server.into_inner()),
            endpoint_count: u32::try_from(endpoints).unwrap_or(u32::MAX),
            connected_count: u32::try_from(connected).unwrap_or(u32::MAX),
            route_count: u32::try_from(routes).unwrap_or(u32::MAX),
            broken_route_count: u32::try_from(broken).unwrap_or(u32::MAX),
            midi_server_replaced_at: self
                .daemon
                .midi_server_replaced_at()
                .await
                .and_then(to_proto_time),
        }))
    }

    async fn get_capabilities(
        &self,
        _request: Request<pb::GetCapabilitiesRequest>,
    ) -> Result<Response<pb::GetCapabilitiesResponse>, Status> {
        let capabilities = self
            .daemon
            .capabilities()
            .all()
            .iter()
            .map(|capability| pb::Capability {
                id: capability.name.id().to_owned(),
                name: capability.name.to_string(),
                available: capability.available,
                reason: capability
                    .reason
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            })
            .collect();
        Ok(Response::new(pb::GetCapabilitiesResponse { capabilities }))
    }

    async fn list_endpoints(
        &self,
        request: Request<pb::ListEndpointsRequest>,
    ) -> Result<Response<pb::ListEndpointsResponse>, Status> {
        let filter = request
            .into_inner()
            .kind
            .and_then(|k| pb::EndpointKind::try_from(k).ok());

        let mut endpoints: Vec<pb::Endpoint> = self
            .daemon
            .read(|config, runtime| {
                config
                    .endpoints
                    .iter()
                    .map(|endpoint| to_proto_endpoint(endpoint, runtime.get(&endpoint.id)))
                    .filter(|endpoint| match filter {
                        Some(wanted) if wanted != pb::EndpointKind::Unspecified => {
                            endpoint.kind == wanted as i32
                        }
                        _ => true,
                    })
                    .collect()
            })
            .await;

        order_endpoints(&mut endpoints);

        // Session state is merged after the fact, since reading it needs the supervisor rather
        // than the configuration lock.
        let mut merged = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let endpoint = self.with_counters(endpoint).await;
            merged.push(with_session_state(&self.daemon, endpoint).await);
        }
        Ok(Response::new(pb::ListEndpointsResponse {
            endpoints: merged,
        }))
    }

    async fn get_endpoint(
        &self,
        request: Request<pb::GetEndpointRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let reference = request.into_inner().id;
        let id = self.daemon.resolve(&reference).await?;

        let found = self
            .daemon
            .read(|config, runtime| {
                config
                    .endpoint(id)
                    .map(|endpoint| to_proto_endpoint(endpoint, runtime.get(&id)))
            })
            .await;
        match found {
            Some(endpoint) => {
                let endpoint = self.with_counters(endpoint).await;
                Ok(Response::new(
                    with_session_state(&self.daemon, endpoint).await,
                ))
            }
            None => Err(Status::not_found(format!("{reference} does not exist"))),
        }
    }

    async fn list_routes(
        &self,
        request: Request<pb::ListRoutesRequest>,
    ) -> Result<Response<pb::ListRoutesResponse>, Status> {
        let only_broken = request.into_inner().only_broken;
        let router = self.daemon.router().await;
        let counters = self.daemon.route_counters().await;

        let routes: Vec<pb::Route> = router
            .routes()
            .iter()
            .filter(|route| !only_broken || route.validity.needs_repair())
            .map(|route| to_proto_route(route, counters.get(&route.id)))
            .collect();
        Ok(Response::new(pb::ListRoutesResponse { routes }))
    }

    async fn list_events(
        &self,
        request: Request<pb::ListEventsRequest>,
    ) -> Result<Response<pb::ListEventsResponse>, Status> {
        let message = request.into_inner();
        let limit = if message.limit == 0 {
            100
        } else {
            message.limit as usize
        };

        let events = self
            .daemon
            .events(message.after_id, limit)
            .await
            .into_iter()
            .map(|event| pb::Event {
                id: event.id.get(),
                at: to_proto_time(event.at),
                endpoint_id: event.endpoint.map(|id| id.to_string()),
                route_id: event.route.map(|id| id.to_string()),
                severity: match event.severity {
                    midi_harbor_core::events::Severity::Info => pb::Severity::Info,
                    midi_harbor_core::events::Severity::Warning => pb::Severity::Warning,
                    midi_harbor_core::events::Severity::Error => pb::Severity::Error,
                } as i32,
                kind: event.kind.as_str().to_owned(),
                detail: event.detail,
            })
            .collect();

        Ok(Response::new(pb::ListEventsResponse { events }))
    }

    async fn create_virtual_port(
        &self,
        request: Request<pb::CreateVirtualPortRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let message = request.into_inner();
        // A client from before connectors sends a direction and no counts; it gets one of each,
        // which is what every virtual port now has at least.
        let endpoint = self
            .daemon
            .create_virtual_port(
                &message.name,
                connector_count(message.inputs),
                connector_count(message.outputs),
            )
            .await?;

        let id = endpoint.id;
        let proto = self
            .daemon
            .read(|config, runtime| {
                config
                    .endpoint(id)
                    .map(|e| to_proto_endpoint(e, runtime.get(&id)))
            })
            .await;
        proto
            .map(Response::new)
            .ok_or_else(|| Status::internal("the created port vanished"))
    }

    async fn set_virtual_port_connectors(
        &self,
        request: Request<pb::SetVirtualPortConnectorsRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let message = request.into_inner();
        let endpoint = self
            .daemon
            .set_virtual_port_connectors(
                &message.id,
                connector_count(message.inputs),
                connector_count(message.outputs),
            )
            .await?;
        let id = endpoint.id;
        self.daemon
            .read(|config, runtime| {
                config
                    .endpoint(id)
                    .map(|e| to_proto_endpoint(e, runtime.get(&id)))
            })
            .await
            .map(Response::new)
            .ok_or_else(|| Status::internal("the changed port vanished"))
    }

    async fn rename_endpoint(
        &self,
        request: Request<pb::RenameEndpointRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let message = request.into_inner();
        let id = self.daemon.resolve(&message.id).await?;
        let endpoint = self
            .daemon
            .rename_endpoint(id, &message.new_name, message.confirm)
            .await?;
        // The runtime state as well, which the reply once left out for every kind.
        let proto = endpoint_as_reported(&self.daemon, endpoint.id)
            .await
            .ok_or_else(|| Status::not_found(format!("{id} does not exist")))?;
        Ok(Response::new(proto))
    }

    async fn delete_virtual_port(
        &self,
        request: Request<pb::DeleteVirtualPortRequest>,
    ) -> Result<Response<pb::DeleteVirtualPortResponse>, Status> {
        let reference = request.into_inner().id;
        let id = self.daemon.resolve(&reference).await?;
        let orphaned = self.daemon.delete_virtual_port(id).await?;
        Ok(Response::new(pb::DeleteVirtualPortResponse {
            orphaned_routes: orphaned,
        }))
    }

    async fn set_endpoint_enabled(
        &self,
        request: Request<pb::SetEndpointEnabledRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let message = request.into_inner();
        let id = self.daemon.resolve(&message.id).await?;
        let endpoint = self.daemon.set_enabled(id, message.enabled).await?;
        let proto = endpoint_as_reported(&self.daemon, endpoint.id)
            .await
            .ok_or_else(|| Status::not_found(format!("{id} does not exist")))?;
        Ok(Response::new(proto))
    }

    async fn dismiss_midi_server_warning(
        &self,
        _request: Request<pb::DismissMidiServerWarningRequest>,
    ) -> Result<Response<pb::DismissMidiServerWarningResponse>, Status> {
        let dismissed = self.daemon.dismiss_midi_server_warning().await;
        Ok(Response::new(pb::DismissMidiServerWarningResponse {
            dismissed,
        }))
    }

    async fn send_test_note(
        &self,
        request: Request<pb::SendTestNoteRequest>,
    ) -> Result<Response<pb::SendTestNoteResponse>, Status> {
        /// The length of a note when the request gives none.
        const DEFAULT_LENGTH_MS: u32 = 500;
        /// The longest a test note may sound, so a mistyped length does not hold a note for hours.
        const MAX_LENGTH_MS: u32 = 10_000;

        // Check every number against MIDI's ranges before anything is sent.
        let message = request.into_inner();
        let channel = message
            .channel
            .checked_sub(1)
            .and_then(|zero_based| u8::try_from(zero_based).ok())
            .and_then(midi_harbor_core::midi::Channel::new)
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "channel {} is not a MIDI channel; use 1 to 16",
                    message.channel
                ))
            })?;
        let note = u8::try_from(message.note)
            .ok()
            .filter(|note| *note <= 127)
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "note {} is not a MIDI note; use 0 to 127",
                    message.note
                ))
            })?;
        let velocity = u8::try_from(message.velocity)
            .ok()
            .filter(|velocity| (1..=127).contains(velocity))
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "velocity {} is out of range; use 1 to 127",
                    message.velocity
                ))
            })?;
        let length_ms = message.length_ms.unwrap_or(DEFAULT_LENGTH_MS);
        if !(1..=MAX_LENGTH_MS).contains(&length_ms) {
            return Err(Status::invalid_argument(format!(
                "a note of {length_ms} ms is out of range; use 1 to {MAX_LENGTH_MS}"
            )));
        }

        let id = self.daemon.resolve(&message.endpoint_id).await?;
        self.daemon
            .send_test_note(
                id,
                channel,
                note,
                velocity,
                std::time::Duration::from_millis(u64::from(length_ms)),
            )
            .await?;
        Ok(Response::new(pb::SendTestNoteResponse {}))
    }

    type WatchStateStream = Pin<Box<dyn Stream<Item = Result<pb::StateEvent, Status>> + Send>>;

    async fn watch_state(
        &self,
        _request: Request<pb::WatchStateRequest>,
    ) -> StreamResponse<pb::StateEvent> {
        let daemon = Arc::clone(&self.daemon);
        let mut changes = daemon.subscribe();
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);

        tokio::spawn(async move {
            loop {
                let change = match changes.recv().await {
                    Ok(change) => change,
                    // State is lossless by contract: a client that cannot keep up is told, rather
                    // than left with a view that silently diverges from the daemon's.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        let _ = tx
                            .send(Err(Status::resource_exhausted(format!(
                                "fell {missed} updates behind; reconnect and re-read the state"
                            ))))
                            .await;
                        return;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                };

                let event = match change {
                    Change::EndpointAdded(id) | Change::EndpointChanged(id) => {
                        let endpoint = endpoint_as_reported(&daemon, id).await;
                        match endpoint {
                            Some(endpoint) => pb::StateEvent {
                                change: Some(pb::state_event::Change::EndpointChanged(endpoint)),
                            },
                            None => continue,
                        }
                    }
                    Change::EndpointRemoved(id) => pb::StateEvent {
                        change: Some(pb::state_event::Change::EndpointRemoved(id.to_string())),
                    },
                    Change::RoutesChanged => continue,
                };

                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type WatchEventsStream = Pin<Box<dyn Stream<Item = Result<pb::Event, Status>> + Send>>;

    async fn watch_events(
        &self,
        request: Request<pb::WatchEventsRequest>,
    ) -> StreamResponse<pb::Event> {
        let after = request.into_inner().after_id;
        let daemon = Arc::clone(&self.daemon);
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);

        tokio::spawn(async move {
            // Events are lossless by contract, so the history is polled from where the client
            // left off rather than pushed from a channel it could fall behind on.
            let mut cursor = after;
            let mut ticker = tokio::time::interval(EVENT_POLL_INTERVAL);
            loop {
                ticker.tick().await;
                for event in daemon.events(cursor, EVENT_PAGE).await {
                    cursor = Some(event.id.get());
                    if tx.send(Ok(to_proto_event(&event))).await.is_err() {
                        return;
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type WatchInvitationsStream =
        Pin<Box<dyn Stream<Item = Result<pb::Invitation, Status>> + Send>>;

    async fn watch_invitations(
        &self,
        _request: Request<pb::WatchInvitationsRequest>,
    ) -> StreamResponse<pb::Invitation> {
        // Replays what is already waiting before streaming what arrives. A client that starts
        // after an invitation would otherwise never learn about it, and the peer is meanwhile
        // waiting on an answer nobody knows to give.
        let daemon = Arc::clone(&self.daemon);
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);
        let mut changes = daemon.subscribe();

        tokio::spawn(async move {
            let mut sent: Vec<String> = Vec::new();
            loop {
                for invitation in daemon.pending_invitations().await {
                    if sent.contains(&invitation.id) {
                        continue;
                    }
                    sent.push(invitation.id.clone());
                    if tx.send(Ok(to_proto_invitation(&invitation))).await.is_err() {
                        return;
                    }
                }
                // Woken by any state change, which is what recording an invitation sends.
                match changes.recv().await {
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type WatchTrafficStream = Pin<Box<dyn Stream<Item = Result<pb::TrafficUpdate, Status>> + Send>>;

    async fn watch_traffic(
        &self,
        request: Request<pb::WatchTrafficRequest>,
    ) -> StreamResponse<pb::TrafficUpdate> {
        let wanted = request.into_inner().endpoint_ids;
        let daemon = Arc::clone(&self.daemon);
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);

        tokio::spawn(async move {
            // Coalesced rather than emitted per message: counters change on every note, and a
            // display refreshing that fast is unreadable as well as wasteful.
            let mut ticker = tokio::time::interval(TRAFFIC_INTERVAL);
            let mut previous: std::collections::HashMap<String, u64> =
                std::collections::HashMap::new();
            // Updates this subscriber was too slow to take, reported on the next it does.
            let mut dropped = 0u64;

            loop {
                ticker.tick().await;
                for (id, snapshot) in daemon.all_counters().await {
                    let id = id.to_string();
                    if !wanted.is_empty() && !wanted.contains(&id) {
                        continue;
                    }

                    // Only endpoints whose traffic actually moved are reported, so an idle
                    // system costs nothing to watch.
                    let total = snapshot
                        .messages_sent
                        .saturating_add(snapshot.messages_received);
                    if previous.get(&id) == Some(&total) {
                        continue;
                    }

                    let update = pb::TrafficUpdate {
                        endpoint_id: id.clone(),
                        counters: Some(to_proto_counters(&snapshot)),
                        dropped,
                    };
                    // Lossy by contract: offered, never awaited, so a stalled subscriber costs
                    // it updates rather than holding anything up. One that is dropped is sent
                    // again on the next tick, because the total it reports is not yet recorded
                    // as seen.
                    match tx.try_send(Ok(update)) {
                        Ok(()) => {
                            dropped = 0;
                            let _ = previous.insert(id, total);
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            dropped = dropped.saturating_add(1);
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type MonitorEndpointStream =
        Pin<Box<dyn Stream<Item = Result<pb::MonitoredMessage, Status>> + Send>>;

    async fn monitor_endpoint(
        &self,
        request: Request<pb::MonitorEndpointRequest>,
    ) -> StreamResponse<pb::MonitoredMessage> {
        let reference = request.into_inner().endpoint_id;
        let id = self.daemon.resolve(&reference).await?;
        let mut watching = self.daemon.watch_endpoint(id).await.ok_or_else(|| {
            // Names what the user typed and why, rather than echoing an identifier they have
            // never seen and cannot act on.
            Status::failed_precondition(format!(
                "'{reference}' is not running, so nothing passes through it to watch"
            ))
        })?;

        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);
        tokio::spawn(async move {
            let mut dropped = 0u64;
            loop {
                match watching.recv().await {
                    Ok(observed) => {
                        let (data, decoded) = match &observed.seen {
                            Seen::Message(message) => {
                                (encode_message(message), describe_message(message))
                            }
                            Seen::SysEx(bytes) => (bytes.to_vec(), describe_sysex(bytes)),
                        };
                        let message = pb::MonitoredMessage {
                            at: to_proto_time(observed.at),
                            data,
                            decoded,
                            outbound: observed.outbound,
                            dropped,
                        };
                        // Offered, never awaited: a watcher that cannot take it misses it and is told
                        // how many on the next one it does.
                        match tx.try_send(Ok(message)) {
                            Ok(()) => dropped = 0,
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                dropped = dropped.saturating_add(1);
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                        }
                    }
                    // Lossy by contract: a watcher that falls behind misses messages and is told
                    // how many, rather than slowing the endpoint down.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        dropped = dropped.saturating_add(missed);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    // Network sessions arrive with user story 3.
    async fn create_network_session(
        &self,
        request: Request<pb::CreateNetworkSessionRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let message = request.into_inner();
        // A request that does not say takes the configured default, which is what the
        // preference exists for; it was once written to the file and never read.
        let policy = if message.invitation_policy == pb::InvitationPolicy::Unspecified as i32 {
            self.daemon
                .read(|config, _| config.preferences.default_invitation_policy)
                .await
        } else {
            from_proto_policy(message.invitation_policy)
        };
        let endpoint = self
            .daemon
            .create_network_port(
                &message.name,
                message.local_name.as_deref(),
                udp_port(message.control_port)?,
                policy,
                message.automatic_port.unwrap_or(true),
            )
            .await?;

        endpoint_as_reported(&self.daemon, endpoint.id)
            .await
            .map(Response::new)
            .ok_or_else(|| Status::internal("the created network port vanished"))
    }

    async fn delete_network_port(
        &self,
        request: Request<pb::DeleteNetworkPortRequest>,
    ) -> Result<Response<pb::DeleteNetworkPortResponse>, Status> {
        let id = self.daemon.resolve(&request.into_inner().id).await?;
        let orphaned = self.daemon.delete_network_port(id).await?;
        Ok(Response::new(pb::DeleteNetworkPortResponse {
            orphaned_routes: orphaned,
        }))
    }

    async fn update_network_port(
        &self,
        request: Request<pb::UpdateNetworkPortRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let message = request.into_inner();
        let id = self.daemon.resolve(&message.id).await?;
        let change = NetworkPortChange {
            local_name: message.local_name,
            control_port: message.control_port.map(udp_port).transpose()?,
            policy: message
                .invitation_policy
                .filter(|policy| *policy != pb::InvitationPolicy::Unspecified as i32)
                .map(from_proto_policy),
            automatic_port: message.automatic_port,
        };
        self.daemon.update_network_port(id, change).await?;
        endpoint_as_reported(&self.daemon, id)
            .await
            .map(Response::new)
            .ok_or_else(|| Status::internal("the changed network port vanished"))
    }

    async fn connect_peer(
        &self,
        request: Request<pb::ConnectPeerRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let message = request.into_inner();
        let id = self.daemon.resolve(&message.session_endpoint_id).await?;

        // A peer is addressed either by what discovery found or by an address the user typed,
        // so both forms resolve here rather than in two separate calls.
        let address = self.resolve_peer(&message.peer_id).await?;
        if message.alongside {
            self.daemon.invite_machine(id, address).await?;
        } else {
            self.daemon.connect_peer(id, address).await?;
        }
        self.endpoint_response(id).await
    }

    async fn disconnect_machine(
        &self,
        request: Request<pb::DisconnectMachineRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let message = request.into_inner();
        let id = self.daemon.resolve(&message.session_endpoint_id).await?;
        let address: SocketAddr = message.address.parse().map_err(|_| {
            Status::invalid_argument(format!(
                "'{}' is not a machine address; use host:port as the network port lists it",
                message.address
            ))
        })?;
        self.daemon.disconnect_machine(id, address).await?;
        self.endpoint_response(id).await
    }
    async fn disconnect_peer(
        &self,
        request: Request<pb::DisconnectPeerRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let id = self
            .daemon
            .resolve(&request.into_inner().session_endpoint_id)
            .await?;
        self.daemon.disconnect_peer(id).await?;
        self.endpoint_response(id).await
    }
    async fn add_manual_peer(
        &self,
        request: Request<pb::AddManualPeerRequest>,
    ) -> Result<Response<pb::Peer>, Status> {
        let message = request.into_inner();
        // The port travels separately in the contract, so a caller may give either form.
        let address = if message.port == 0 {
            message.address.clone()
        } else {
            format!("{}:{}", message.address, message.port)
        };

        let peer = self
            .daemon
            .add_known_machine(&address, message.name, message.trusted.unwrap_or(true))
            .await?;
        Ok(Response::new(pb::Peer {
            id: peer.id.to_string(),
            advertised_name: peer.name,
            addresses: peer.addresses,
            discovered: peer.discovered,
            trusted: peer.trusted,
            last_seen: None,
        }))
    }
    async fn remove_peer(
        &self,
        request: Request<pb::RemovePeerRequest>,
    ) -> Result<Response<pb::RemovePeerResponse>, Status> {
        self.daemon
            .remove_peer(&request.into_inner().peer_id)
            .await?;
        Ok(Response::new(pb::RemovePeerResponse {}))
    }
    async fn set_peer_trusted(
        &self,
        request: Request<pb::SetPeerTrustedRequest>,
    ) -> Result<Response<pb::Peer>, Status> {
        let message = request.into_inner();
        let peer = self
            .daemon
            .set_peer_trusted(&message.peer_id, message.trusted)
            .await?;
        Ok(Response::new(pb::Peer {
            id: peer.id.to_string(),
            advertised_name: peer.name,
            addresses: peer.addresses,
            discovered: peer.discovered,
            trusted: peer.trusted,
            last_seen: None,
        }))
    }
    async fn respond_to_invitation(
        &self,
        request: Request<pb::RespondToInvitationRequest>,
    ) -> Result<Response<pb::RespondToInvitationResponse>, Status> {
        let message = request.into_inner();
        self.daemon
            .respond_to_invitation(&message.invitation_id, message.accept, message.always)
            .await?;
        Ok(Response::new(pb::RespondToInvitationResponse {}))
    }
    async fn set_invitation_policy(
        &self,
        request: Request<pb::SetInvitationPolicyRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let message = request.into_inner();
        let id = self.daemon.resolve(&message.session_endpoint_id).await?;
        let endpoint = self
            .daemon
            .set_invitation_policy(id, from_proto_policy(message.policy))
            .await?;
        let proto = endpoint_as_reported(&self.daemon, endpoint.id)
            .await
            .ok_or_else(|| Status::not_found(format!("{id} does not exist")))?;
        Ok(Response::new(proto))
    }
    async fn list_peers(
        &self,
        _request: Request<pb::ListPeersRequest>,
    ) -> Result<Response<pb::ListPeersResponse>, Status> {
        let peers = self
            .daemon
            .known_peers()
            .await
            .into_iter()
            .map(|peer| pb::Peer {
                id: peer.id.to_string(),
                advertised_name: peer.name,
                addresses: peer.addresses,
                discovered: peer.discovered,
                trusted: peer.trusted,
                last_seen: None,
            })
            .collect();
        Ok(Response::new(pb::ListPeersResponse { peers }))
    }

    // Physical devices arrive with user story 4.
    async fn forget_physical_device(
        &self,
        request: Request<pb::ForgetPhysicalDeviceRequest>,
    ) -> Result<Response<pb::ForgetPhysicalDeviceResponse>, Status> {
        let reference = request.into_inner().id;
        let id = self.daemon.resolve(&reference).await?;
        let (orphaned_routes, still_attached) = self.daemon.forget_device(id).await?;
        Ok(Response::new(pb::ForgetPhysicalDeviceResponse {
            orphaned_routes,
            still_attached,
        }))
    }
    async fn resolve_ambiguous_device(
        &self,
        request: Request<pb::ResolveAmbiguousDeviceRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let message = request.into_inner();
        let id = self.daemon.resolve(&message.id).await?;
        let chosen = message
            .chosen
            .map(from_proto_fingerprint)
            .ok_or_else(|| Status::invalid_argument("say which device was meant"))?;

        let endpoint = self.daemon.resolve_device(id, &chosen).await?;
        let proto = self
            .daemon
            .read(|_, runtime| to_proto_endpoint(&endpoint, runtime.get(&id)))
            .await;
        Ok(Response::new(proto))
    }

    // Routing arrives with user story 4.
    async fn create_route(
        &self,
        request: Request<pb::CreateRouteRequest>,
    ) -> Result<Response<pb::CreateRouteResponse>, Status> {
        let message = request.into_inner();
        let (created, loop_warning) = self
            .daemon
            .create_route_with(RouteRequest {
                from: &message.from,
                from_connector: connector_index(message.from_connector),
                to: &message.to,
                to_connector: connector_index(message.to_connector),
                both_ways: message.both_ways,
            })
            .await?;

        let router = self.daemon.router().await;
        let counters = self.daemon.route_counters().await;
        let route = router
            .routes()
            .iter()
            .find(|route| route.id == created.id())
            .map(|route| to_proto_route(route, counters.get(&route.id)))
            .ok_or_else(|| Status::internal("the created route vanished"))?;

        Ok(Response::new(pb::CreateRouteResponse {
            route: Some(route),
            loop_warning,
        }))
    }
    async fn update_route(
        &self,
        request: Request<pb::UpdateRouteRequest>,
    ) -> Result<Response<pb::UpdateRouteResponse>, Status> {
        let message = request.into_inner();
        let (updated, loop_warning) = self
            .daemon
            .update_route(
                &message.id,
                RouteRequest {
                    from: &message.from,
                    from_connector: connector_index(message.from_connector),
                    to: &message.to,
                    to_connector: connector_index(message.to_connector),
                    both_ways: message.both_ways,
                },
            )
            .await?;

        let router = self.daemon.router().await;
        let counters = self.daemon.route_counters().await;
        let route = router
            .routes()
            .iter()
            .find(|route| route.id == updated.id())
            .map(|route| to_proto_route(route, counters.get(&route.id)))
            .ok_or_else(|| Status::internal("the changed route vanished"))?;
        Ok(Response::new(pb::UpdateRouteResponse {
            route: Some(route),
            loop_warning,
        }))
    }

    async fn delete_route(
        &self,
        request: Request<pb::DeleteRouteRequest>,
    ) -> Result<Response<pb::DeleteRouteResponse>, Status> {
        self.daemon.delete_route(&request.into_inner().id).await?;
        Ok(Response::new(pb::DeleteRouteResponse {}))
    }
    async fn set_route_enabled(
        &self,
        request: Request<pb::SetRouteEnabledRequest>,
    ) -> Result<Response<pb::Route>, Status> {
        let message = request.into_inner();
        let updated = self
            .daemon
            .set_route_enabled(&message.id, message.enabled)
            .await?;

        let router = self.daemon.router().await;
        let counters = self.daemon.route_counters().await;
        router
            .routes()
            .iter()
            .find(|route| route.id == updated.id())
            .map(|route| to_proto_route(route, counters.get(&route.id)))
            .map(Response::new)
            .ok_or_else(|| Status::internal("the updated route vanished"))
    }

    async fn start_bluetooth_scan(
        &self,
        request: Request<pb::StartBluetoothScanRequest>,
    ) -> Result<Response<pb::StartBluetoothScanResponse>, Status> {
        let seconds = request.into_inner().duration_seconds;
        let duration = (seconds > 0).then(|| std::time::Duration::from_secs(u64::from(seconds)));
        self.daemon.start_bluetooth_scan(duration).await?;
        Ok(Response::new(pb::StartBluetoothScanResponse {}))
    }
    async fn stop_bluetooth_scan(
        &self,
        _r: Request<pb::StopBluetoothScanRequest>,
    ) -> Result<Response<pb::StopBluetoothScanResponse>, Status> {
        self.daemon.stop_bluetooth_scan().await?;
        Ok(Response::new(pb::StopBluetoothScanResponse {}))
    }
    async fn list_bluetooth_devices(
        &self,
        _r: Request<pb::ListBluetoothDevicesRequest>,
    ) -> Result<Response<pb::ListBluetoothDevicesResponse>, Status> {
        let in_range = self.daemon.bluetooth_in_range().await;
        let known = self.daemon.bluetooth_endpoints().await;

        let devices = in_range
            .into_iter()
            .map(|found| {
                let address = found.id.to_string();
                pb::DiscoveredBluetoothDevice {
                    endpoint_id: known.get(&address).map(ToString::to_string),
                    paired: known.contains_key(&address),
                    address,
                    name: found.name,
                    rssi: found.rssi.map(i32::from),
                }
            })
            .collect();
        Ok(Response::new(pb::ListBluetoothDevicesResponse { devices }))
    }
    async fn connect_bluetooth_device(
        &self,
        request: Request<pb::ConnectBluetoothDeviceRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let endpoint = self
            .daemon
            .connect_bluetooth(&request.into_inner().address)
            .await?;
        let id = endpoint.id;
        let proto = self
            .daemon
            .read(|_, runtime| to_proto_endpoint(&endpoint, runtime.get(&id)))
            .await;
        Ok(Response::new(proto))
    }
    async fn disconnect_bluetooth_device(
        &self,
        request: Request<pb::DisconnectBluetoothDeviceRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let id = self.daemon.resolve(&request.into_inner().id).await?;
        let endpoint = self.daemon.disconnect_bluetooth(id).await?;
        let proto = self
            .daemon
            .read(|_, runtime| to_proto_endpoint(&endpoint, runtime.get(&id)))
            .await;
        Ok(Response::new(proto))
    }
    async fn forget_bluetooth_device(
        &self,
        request: Request<pb::ForgetBluetoothDeviceRequest>,
    ) -> Result<Response<pb::ForgetBluetoothDeviceResponse>, Status> {
        let id = self.daemon.resolve(&request.into_inner().id).await?;
        self.daemon.forget_bluetooth(id).await?;
        Ok(Response::new(pb::ForgetBluetoothDeviceResponse {}))
    }
    async fn set_peripheral_advertising(
        &self,
        request: Request<pb::SetPeripheralAdvertisingRequest>,
    ) -> Result<Response<pb::SetPeripheralAdvertisingResponse>, Status> {
        let message = request.into_inner();
        let endpoint = self
            .daemon
            .set_peripheral_advertising(message.enabled, message.name)
            .await?;
        let name = endpoint
            .as_ref()
            .map(|endpoint| endpoint.name.as_str().to_owned())
            .unwrap_or_default();
        Ok(Response::new(pb::SetPeripheralAdvertisingResponse {
            advertising: endpoint.is_some(),
            name_sent: endpoint.is_some()
                && midi_harbor_platform::bluetooth::advertised_name_fits(&name),
            name,
        }))
    }

    async fn export_configuration(
        &self,
        _request: Request<pb::ExportConfigurationRequest>,
    ) -> Result<Response<pb::ExportConfigurationResponse>, Status> {
        let yaml = self.daemon.export_configuration().await?;
        Ok(Response::new(pb::ExportConfigurationResponse { yaml }))
    }

    async fn import_configuration(
        &self,
        request: Request<pb::ImportConfigurationRequest>,
    ) -> Result<Response<pb::ImportConfigurationResponse>, Status> {
        let request = request.into_inner();
        let applied = self
            .daemon
            .import_configuration(&request.yaml, request.replace)
            .await?;
        Ok(Response::new(pb::ImportConfigurationResponse {
            endpoints_added: u32::try_from(applied.added.len()).unwrap_or(u32::MAX),
            routes_added: u32::try_from(applied.routes_added).unwrap_or(u32::MAX),
            restarted_endpoints: applied.restarted,
            removed_endpoints: applied.removed,
            pending_restart: applied.pending_restart,
        }))
    }

    async fn reload_configuration(
        &self,
        _request: Request<pb::ReloadConfigurationRequest>,
    ) -> Result<Response<pb::ReloadConfigurationResponse>, Status> {
        let applied = self.daemon.reload_configuration().await?;
        Ok(Response::new(pb::ReloadConfigurationResponse {
            restarted_endpoints: applied.restarted,
            repaired_from: None,
            added_endpoints: applied.added,
            removed_endpoints: applied.removed,
            routes_added: u32::try_from(applied.routes_added).unwrap_or(u32::MAX),
            pending_restart: applied.pending_restart,
        }))
    }

    async fn read_apple_setup(
        &self,
        _request: Request<pb::ReadAppleSetupRequest>,
    ) -> Result<Response<pb::ReadAppleSetupResponse>, Status> {
        // CoreMIDI's queries block, so they run off the runtime's worker threads.
        let setup = tokio::task::spawn_blocking(midi_harbor_platform::midi::apple_setup::read)
            .await
            .map_err(|error| Status::internal(format!("could not read apple's setup: {error}")))?;
        Ok(Response::new(match setup {
            Some(setup) => pb::ReadAppleSetupResponse {
                available: true,
                iac_online: setup.iac_online,
                buses: setup.buses,
                sessions: setup.sessions,
            },
            None => pb::ReadAppleSetupResponse::default(),
        }))
    }

    async fn export_diagnostics(
        &self,
        _request: Request<pb::ExportDiagnosticsRequest>,
    ) -> Result<Response<pb::ExportDiagnosticsResponse>, Status> {
        let server = self
            .get_server_info(Request::new(pb::GetServerInfoRequest {}))
            .await?
            .into_inner();
        let endpoints = self
            .list_endpoints(Request::new(pb::ListEndpointsRequest {
                kind: None,
                include_absent: true,
            }))
            .await?
            .into_inner();
        let routes = self
            .list_routes(Request::new(pb::ListRoutesRequest { only_broken: false }))
            .await?
            .into_inner();

        // A bug report is worth nothing without the history, so the whole retained log goes in.
        let events = self.daemon.events(None, usize::MAX).await;
        let counters = self.daemon.all_counters().await;
        let automatic = self.daemon.automatic_port_counters().await;
        let replaced_at = self.daemon.midi_server_replaced_at().await;

        // The configuration goes in whole, preferences and each endpoint's own settings included,
        // because a session's peer or a device's address explains more failures than its state.
        let configuration = self
            .daemon
            .read(|config, _| serde_json::to_value(config))
            .await
            .map_err(|error| {
                Status::internal(format!("could not encode the configuration: {error}"))
            })?;

        let report = serde_json::json!({
            "daemon": {
                "version": server.daemon_version,
                "protocol": format!("{}.{}", server.protocol_major, server.protocol_minor),
                "config_path": server.config_path,
                "socket_path": server.socket_path,
                "midi_server_replaced_at": replaced_at.map(|at| at.to_string()),
            },
            "configuration": configuration,
            "capabilities": self.daemon.capabilities()
                .all()
                .iter()
                .map(|c| serde_json::json!({
                    "name": c.name.to_string(),
                    "available": c.available,
                    "reason": c.reason.as_ref().map(ToString::to_string),
                }))
                .collect::<Vec<_>>(),
            "endpoints": endpoints.endpoints.iter().map(|e| serde_json::json!({
                "id": e.id,
                "name": e.name,
                "kind": e.kind,
                "enabled": e.enabled,
                "state": e.state.as_ref().map(|s| s.phase),
                "last_error": e.state.as_ref().and_then(|s| s.last_error.as_ref())
                    .map(|error| error.code.clone()),
            })).collect::<Vec<_>>(),
            "routes": routes.routes.iter().map(|r| serde_json::json!({
                "from": r.from_name,
                "to": r.to_name,
                "enabled": r.enabled,
                "validity": r.validity,
                "missing": r.missing,
            })).collect::<Vec<_>>(),
            "counters": counters.iter().map(|(id, snapshot)| serde_json::json!({
                "endpoint": id.to_string(),
                "sent": snapshot.messages_sent,
                "received": snapshot.messages_received,
                "lost": snapshot.messages_lost,
                "recovered": snapshot.messages_recovered,
                "dropped": snapshot.messages_dropped,
                "malformed": snapshot.packets_malformed,
                "last_received": snapshot.last_received.map(|at| at.to_string()),
                "last_sent": snapshot.last_sent.map(|at| at.to_string()),
            })).collect::<Vec<_>>(),
            // Received is what other applications sent the automatic port, sent is what it
            // passed on to them from the network.
            "automatic_ports": automatic.iter().map(|(session, snapshot)| serde_json::json!({
                "network_port": session.to_string(),
                "sent": snapshot.messages_sent,
                "received": snapshot.messages_received,
                "dropped": snapshot.messages_dropped,
                "last_received": snapshot.last_received.map(|at| at.to_string()),
                "last_sent": snapshot.last_sent.map(|at| at.to_string()),
            })).collect::<Vec<_>>(),
            "events": events.iter().map(|event| serde_json::json!({
                "id": event.id.get(),
                "at": event.at.to_string(),
                "kind": format!("{:?}", event.kind),
                "detail": event.detail,
            })).collect::<Vec<_>>(),
        });

        let encoded = serde_json::to_string_pretty(&report)
            .map_err(|error| Status::internal(format!("could not encode the report: {error}")))?;
        Ok(Response::new(pb::ExportDiagnosticsResponse {
            report: encoded,
        }))
    }
}

/// Converts a resolved route into its wire representation.
fn to_proto_route(
    route: &midi_harbor_core::router::ResolvedRoute,
    counters: Option<&midi_harbor_core::counters::CounterSnapshot>,
) -> pb::Route {
    let (validity, missing, cycle, waiting_on) = match &route.validity {
        midi_harbor_core::router::RouteValidity::Valid => {
            (pb::RouteValidity::Valid, Vec::new(), Vec::new(), Vec::new())
        }
        midi_harbor_core::router::RouteValidity::Broken { missing } => (
            pb::RouteValidity::Broken,
            missing.clone(),
            Vec::new(),
            Vec::new(),
        ),
        midi_harbor_core::router::RouteValidity::LoopDetected { cycle } => (
            pb::RouteValidity::LoopDetected,
            Vec::new(),
            cycle.iter().map(ToString::to_string).collect(),
            Vec::new(),
        ),
        midi_harbor_core::router::RouteValidity::Suspended { waiting_on } => (
            pb::RouteValidity::Suspended,
            Vec::new(),
            Vec::new(),
            waiting_on.clone(),
        ),
    };

    pb::Route {
        id: route.id.to_string(),
        from_name: route.from.clone(),
        to_name: route.to.clone(),
        from_id: route.source.map(|id| id.to_string()),
        to_id: route.destination.map(|id| id.to_string()),
        enabled: route.enabled,
        validity: validity as i32,
        missing,
        cycle,
        waiting_on,
        from_connector: u32::from(route.from_connector) + 1,
        to_connector: u32::from(route.to_connector) + 1,
        both_ways: route.both_ways,
        // Always present, even at zero: a route that has carried nothing is a fact worth
        // showing, and an absent field would read as "not measured" instead.
        counters: Some(counters.map(to_proto_counters).unwrap_or_default()),
    }
}

/// Reads a connector count off the wire, where zero means one and anything past sixteen is
/// sixteen.
fn connector_count(count: u32) -> u8 {
    u8::try_from(count.clamp(1, u32::from(midi_harbor_core::endpoint::MAX_CONNECTORS))).unwrap_or(1)
}

/// Reads a connector named on the wire, counting from one, as an index counting from zero.
/// Zero means the first, as one does.
fn connector_index(number: u32) -> u8 {
    u8::try_from(number.saturating_sub(1)).unwrap_or(u8::MAX)
}

/// Converts a counter snapshot into its wire representation.
fn to_proto_counters(
    snapshot: &midi_harbor_core::counters::CounterSnapshot,
) -> pb::TrafficCounters {
    pb::TrafficCounters {
        messages_sent: snapshot.messages_sent,
        messages_received: snapshot.messages_received,
        bytes_sent: snapshot.bytes_sent,
        bytes_received: snapshot.bytes_received,
        messages_lost: snapshot.messages_lost,
        messages_recovered: snapshot.messages_recovered,
        messages_dropped: snapshot.messages_dropped,
        packets_malformed: snapshot.packets_malformed,
        last_activity: snapshot.last_activity.and_then(to_proto_time),
        last_received: snapshot.last_received.and_then(to_proto_time),
        last_sent: snapshot.last_sent.and_then(to_proto_time),
    }
}

/// Reads a fingerprint off the wire.
fn from_proto_fingerprint(
    fingerprint: pb::DeviceFingerprint,
) -> midi_harbor_core::fingerprint::DeviceFingerprint {
    midi_harbor_core::fingerprint::DeviceFingerprint {
        unique_id: fingerprint.unique_id,
        usb_serial: fingerprint.usb_serial,
        manufacturer: fingerprint.manufacturer,
        model: fingerprint.model,
        name: fingerprint.name,
        topology_path: fingerprint.topology_path,
    }
}

/// Converts a waiting invitation into its wire representation.
fn to_proto_invitation(invitation: &crate::state::PendingInvitation) -> pb::Invitation {
    pb::Invitation {
        invitation_id: invitation.id.clone(),
        session_endpoint_id: invitation.session.to_string(),
        peer_name: invitation
            .peer_name
            .clone()
            .unwrap_or_else(|| invitation.peer.ip().to_string()),
        peer_address: invitation.peer.to_string(),
        received_at: to_proto_time(invitation.first_seen),
        // An invitation that is waiting is by definition from a peer we have not accepted.
        peer_known: false,
    }
}

/// Converts a recorded event into its wire representation.
fn to_proto_event(event: &midi_harbor_core::events::Event) -> pb::Event {
    pb::Event {
        id: event.id.get(),
        at: to_proto_time(event.at),
        endpoint_id: event.endpoint.map(|id| id.to_string()),
        route_id: event.route.map(|id| id.to_string()),
        severity: match event.severity {
            midi_harbor_core::events::Severity::Info => pb::Severity::Info,
            midi_harbor_core::events::Severity::Warning => pb::Severity::Warning,
            midi_harbor_core::events::Severity::Error => pb::Severity::Error,
        } as i32,
        kind: event.kind.as_str().to_owned(),
        detail: event.detail.clone(),
    }
}

/// Describes a system-exclusive message for the monitor's readable column.
///
/// Names the identifier rather than decoding the payload: what the bytes mean is defined by each
/// manufacturer, and guessing at one would be worse than showing the size and the raw view.
fn describe_sysex(bytes: &[u8]) -> String {
    let id = match bytes.get(1).copied() {
        Some(0x7E) => "universal non-realtime".to_owned(),
        Some(0x7F) => "universal realtime".to_owned(),
        Some(0x00) => "extended id".to_owned(),
        Some(id) => format!("manufacturer {id:#04X}"),
        None => "empty".to_owned(),
    };
    format!("system exclusive, {id}, {} bytes", bytes.len())
}

/// Encodes a message back to its wire bytes, for the raw view.
fn encode_message(message: &MidiMessage) -> Vec<u8> {
    let mut buffer = [0u8; 3];
    let written = message.encode(&mut buffer);
    buffer.get(..written).unwrap_or_default().to_vec()
}

/// Describes a message the way a person reads it.
fn describe_message(message: &MidiMessage) -> String {
    use midi_harbor_core::midi::note_name;

    match message {
        MidiMessage::NoteOn {
            channel,
            note,
            velocity,
        } if *velocity > 0 => format!(
            "ch{} note on  {} vel {velocity}",
            channel.number(),
            note_name(*note)
        ),
        MidiMessage::NoteOn { channel, note, .. } | MidiMessage::NoteOff { channel, note, .. } => {
            format!("ch{} note off {}", channel.number(), note_name(*note))
        }
        MidiMessage::PolyAftertouch {
            channel,
            note,
            pressure,
        } => format!(
            "ch{} aftertouch {} {pressure}",
            channel.number(),
            note_name(*note)
        ),
        MidiMessage::ControlChange {
            channel,
            controller,
            value,
        } => {
            format!("ch{} cc {controller} = {value}", channel.number())
        }
        MidiMessage::ProgramChange { channel, program } => {
            format!("ch{} program {program}", channel.number())
        }
        MidiMessage::ChannelAftertouch { channel, pressure } => {
            format!("ch{} pressure {pressure}", channel.number())
        }
        MidiMessage::PitchBend { channel, value } => {
            format!("ch{} pitch bend {value}", channel.number())
        }
        MidiMessage::SystemCommon {
            status: 0xF1,
            data: [frame, _],
        } => format!("time code {frame:#04x}"),
        MidiMessage::SystemCommon {
            status: 0xF2,
            data: [low, high],
        } => format!("song position {}", u16::from(*low) | u16::from(*high) << 7),
        MidiMessage::SystemCommon {
            status: 0xF3,
            data: [song, _],
        } => format!("song select {song}"),
        MidiMessage::SystemCommon { status, .. } | MidiMessage::System { status } => {
            format!("system 0x{status:02X}")
        }
    }
}

/// Puts endpoints in an order that does not change between calls.
///
/// Configured endpoints load first and discovered hardware is appended, so the same machine lists
/// its endpoints differently after a restart or a hot-plug. A client that redraws every second
/// would move a row under the user's cursor between them reading it and clicking it. Kind groups
/// hardware together, name orders within a group, and the identifier settles a tie between two
/// endpoints sharing a name.
fn order_endpoints(endpoints: &mut [pb::Endpoint]) {
    endpoints.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
}

#[cfg(test)]
mod tests {
    use super::{
        describe_message, order_endpoints, parse_literal_address, pb, resolve_hostname,
        split_host_port, udp_port,
    };
    use crate::net::DEFAULT_CONTROL_PORT;
    use midi_harbor_core::midi::{Channel, MidiMessage};
    use std::net::SocketAddr;

    /// Proves how the contract's 32-bit port field narrows to a UDP port: zero asks the system to
    /// choose, 65535 is the highest UDP port, and 65536, one past it, is refused. Reading an
    /// out-of-range number as zero once put the network port somewhere the user did not ask for.
    #[test]
    fn the_contracts_port_number_narrows_to_a_udp_port_or_is_refused() {
        let cases = [
            (0, Some(0)),
            (5004, Some(5004)),
            (65_535, Some(65_535)),
            (65_536, None),
        ];
        for (requested, want) in cases {
            assert_eq!(
                udp_port(requested).ok(),
                want,
                "{requested}: narrowed wrongly to a UDP port"
            );
        }
    }

    /// Proves how a watched message reads. Note 60 is middle C, written C3 in the convention Apple
    /// and Yamaha use (octave = 60 / 12 - 2 = 3); channels are one-based for people and zero-based
    /// on the wire; a note on with velocity zero is a note off (MIDI 1.0 running-status
    /// convention); a song position is 14 bits, low byte first, so 0x10 | 0x02 << 7 = 272.
    #[test]
    fn a_message_is_described_the_way_a_musician_reads_it() {
        let first = Channel::new(0).expect("channel 0 is a valid MIDI channel");
        let cases = [
            (
                MidiMessage::NoteOn {
                    channel: first,
                    note: 60,
                    velocity: 100,
                },
                "ch1 note on  C3 vel 100",
            ),
            (
                MidiMessage::NoteOn {
                    channel: first,
                    note: 60,
                    velocity: 0,
                },
                "ch1 note off C3",
            ),
            (
                MidiMessage::SystemCommon {
                    status: 0xF2,
                    data: [0x10, 0x02],
                },
                "song position 272",
            ),
        ];
        for (message, want) in cases {
            assert_eq!(
                describe_message(&message),
                want,
                "{message:?}: described wrongly"
            );
        }
    }

    /// Proves which references are literal addresses. A bare address is what a user reads off
    /// another machine, so it takes the standard RTP-MIDI port 5004; IPv6 is accepted bare or
    /// bracketed with a port. A peer name, even "localhost", is never read as an address, since
    /// names are checked against discovery first and a guess would connect to whatever the
    /// resolver returned.
    #[test]
    fn only_a_literal_address_is_read_as_one() {
        let cases = [
            ("192.0.2.3:5678", Some("192.0.2.3:5678")),
            ("192.0.2.3", Some("192.0.2.3:5004")),
            ("::1", Some("[::1]:5004")),
            ("[::1]:5004", Some("[::1]:5004")),
            ("Stage Laptop", None),
            ("localhost", None),
        ];
        for (reference, want) in cases {
            let want: Option<SocketAddr> =
                want.map(|address| address.parse().expect("the expected address parses"));
            assert_eq!(
                parse_literal_address(reference),
                want,
                "{reference}: parsed wrongly as a literal address"
            );
        }
    }

    /// Proves a hostname reference splits into host and port, taking the standard port when none
    /// is given, and that a bracketed IPv6 literal is not cut at its own colons.
    #[test]
    fn a_host_and_port_are_split_correctly() {
        let cases = [
            ("example.local:5678", "example.local", 5678),
            ("example.local", "example.local", DEFAULT_CONTROL_PORT),
            ("[fe80::1]:5004", "fe80::1", 5004),
        ];
        for (reference, host, port) in cases {
            assert_eq!(
                split_host_port(reference),
                (host.to_owned(), port),
                "{reference}: split wrongly"
            );
        }
    }

    /// Proves a hostname resolves through the system resolver with the standard port, and that a
    /// name with a space is not looked up at all, since it cannot be a hostname and the lookup
    /// would only spend the resolver's timeout.
    #[tokio::test]
    async fn a_hostname_resolves_with_the_standard_port() {
        let resolved = resolve_hostname("localhost")
            .await
            .expect("localhost resolves on every machine");
        assert!(
            resolved.ip().is_loopback() && resolved.port() == DEFAULT_CONTROL_PORT,
            "localhost must resolve to loopback on the standard port, got {resolved}"
        );
        assert_eq!(
            resolve_hostname("Stage Laptop").await,
            None,
            "a name with a space must not be looked up"
        );
    }

    /// Proves the endpoint list has one settled order, by kind in the contract's numbering, then
    /// name, then identifier, whatever order the endpoints arrive in. A restart loads the
    /// configuration first and appends discovered hardware, so the incoming order differs between
    /// runs of one machine, and two identical devices must not swap places between refreshes.
    #[test]
    fn the_same_endpoints_list_the_same_way_whatever_order_they_arrive_in() {
        let endpoint = |kind: pb::EndpointKind, name: &str, id: &str| pb::Endpoint {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: kind.into(),
            ..pb::Endpoint::default()
        };
        let arrivals = [
            vec![
                endpoint(pb::EndpointKind::PhysicalDevice, "MIDI Interface", "d2"),
                endpoint(pb::EndpointKind::VirtualPort, "Synth", "v2"),
                endpoint(pb::EndpointKind::NetworkSession, "Studio Mac", "n1"),
                endpoint(pb::EndpointKind::VirtualPort, "Drums", "v1"),
                endpoint(pb::EndpointKind::PhysicalDevice, "MIDI Interface", "d1"),
            ],
            vec![
                endpoint(pb::EndpointKind::PhysicalDevice, "MIDI Interface", "d1"),
                endpoint(pb::EndpointKind::VirtualPort, "Drums", "v1"),
                endpoint(pb::EndpointKind::NetworkSession, "Studio Mac", "n1"),
                endpoint(pb::EndpointKind::PhysicalDevice, "MIDI Interface", "d2"),
                endpoint(pb::EndpointKind::VirtualPort, "Synth", "v2"),
            ],
        ];
        for mut endpoints in arrivals {
            order_endpoints(&mut endpoints);
            let ids: Vec<&str> = endpoints.iter().map(|e| e.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["v1", "v2", "d1", "d2", "n1"],
                "the list must be ordered by kind, then name, then identifier"
            );
        }
    }
}
