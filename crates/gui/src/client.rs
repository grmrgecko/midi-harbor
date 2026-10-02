//! Reaching the daemon, and holding what it last said.

use midi_harbor_core::paths::Paths;
use midi_harbor_ipc::pb::{
    AddManualPeerRequest, Capability, ConnectBluetoothDeviceRequest, ConnectPeerRequest,
    CreateNetworkSessionRequest, CreateRouteRequest, CreateVirtualPortRequest,
    DeleteNetworkPortRequest, DeleteRouteRequest, DeleteVirtualPortRequest, Direction,
    DisconnectBluetoothDeviceRequest, DisconnectMachineRequest, DisconnectPeerRequest,
    DiscoveredBluetoothDevice, DismissMidiServerWarningRequest, Endpoint, Event,
    ExportDiagnosticsRequest, ForgetBluetoothDeviceRequest, ForgetPhysicalDeviceRequest,
    GetCapabilitiesRequest, GetServerInfoRequest, GetStatusRequest, Invitation, InvitationPolicy,
    ListBluetoothDevicesRequest, ListEndpointsRequest, ListEventsRequest, ListPeersRequest,
    ListRoutesRequest, MonitorEndpointRequest, MonitoredMessage, Peer, RemovePeerRequest,
    RenameEndpointRequest, RespondToInvitationRequest, Route, SendTestNoteRequest,
    SetEndpointEnabledRequest, SetPeerTrustedRequest, SetPeripheralAdvertisingRequest,
    SetRouteEnabledRequest, SetVirtualPortConnectorsRequest, StartBluetoothScanRequest, StateEvent,
    StatusSummary, UpdateNetworkPortRequest, UpdateRouteRequest, WatchInvitationsRequest,
    WatchStateRequest, state_event::Change,
};
use midi_harbor_ipc::transport;
use midi_harbor_ipc::{HarborClient, check_compatibility};
use std::path::PathBuf;
use std::time::Duration;
use tonic::transport::Channel;

/// How long to wait for the invitation stream to finish replaying what is already waiting.
///
/// The stream stays open afterwards to carry new ones, so a refresh takes what has arrived and
/// moves on rather than holding the window while nothing happens.
const INVITATION_REPLAY: Duration = Duration::from_millis(150);

/// How many recent events the activity view keeps.
///
/// The daemon's own ring is larger; this is only what one screen can usefully show.
const EVENT_LIMIT: u32 = 100;

/// A connected client, cheap to clone into a background task.
///
/// Cloning shares the underlying channel rather than opening a second socket, which is what lets
/// each user action run as its own task without reconnecting.
#[derive(Clone)]
pub struct Client {
    inner: HarborClient<Channel>,
    /// What the daemon said about itself when the connection was made. Shared, so a clone of
    /// the client stays as small as its channel.
    server: std::sync::Arc<midi_harbor_ipc::pb::ServerInfo>,
}

/// Everything one refresh returns, so the interface never shows two halves of different moments.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    /// Totals across the daemon.
    pub status: Option<StatusSummary>,
    /// Every endpoint the daemon knows about.
    pub endpoints: Vec<Endpoint>,
    /// Every configured route.
    pub routes: Vec<Route>,
    /// Recent events, newest last as the daemon returns them.
    pub events: Vec<Event>,
    /// Machines waiting to be let in.
    pub invitations: Vec<Invitation>,
    /// What this machine can do, so what it cannot is shown as unavailable rather than broken.
    pub capabilities: Vec<Capability>,
    /// Bluetooth devices the radio can hear right now.
    pub nearby: Vec<DiscoveredBluetoothDevice>,
    /// Machines this one remembers, and those advertising on the network now.
    pub peers: Vec<Peer>,
}

impl Snapshot {
    /// Applies one change from the daemon's state stream to the cached view.
    ///
    /// The cache is fed by the stream and corrected by a full resync whenever the stream ends,
    /// which is how the contract says a client stays consistent with the daemon.
    pub fn apply_change(&mut self, event: StateEvent) {
        match event.change {
            Some(Change::EndpointChanged(endpoint) | Change::EndpointAdded(endpoint)) => match self
                .endpoints
                .iter_mut()
                .find(|held| held.id == endpoint.id)
            {
                Some(held) => *held = endpoint,
                None => self.endpoints.push(endpoint),
            },
            Some(Change::EndpointRemoved(id)) => self.endpoints.retain(|held| held.id != id),
            Some(Change::RouteChanged(route)) => {
                match self.routes.iter_mut().find(|held| held.id == route.id) {
                    Some(held) => *held = route,
                    None => self.routes.push(route),
                }
            }
            Some(Change::RouteRemoved(id)) => self.routes.retain(|held| held.id != id),
            Some(Change::CapabilitiesChanged(changed)) => {
                self.capabilities = changed.capabilities;
            }
            None => {}
        }
    }
}

impl Client {
    /// Asks the daemon to stop through its graceful shutdown.
    #[cfg(target_os = "macos")]
    pub async fn stop_daemon(&self) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.stop_daemon(midi_harbor_ipc::pb::StopDaemonRequest {})
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Follows the daemon's changes as they happen.
    pub async fn watch_state(&self) -> Result<tonic::Streaming<StateEvent>, String> {
        let mut rpc = self.inner.clone();
        rpc.watch_state(WatchStateRequest {})
            .await
            .map(tonic::Response::into_inner)
            .map_err(describe)
    }

    /// Connects to the daemon and checks that both sides speak the same contract.
    ///
    /// The version check runs before anything else, so a mismatch is reported as a mismatch
    /// instead of surfacing later as a call that mysteriously does not exist.
    pub async fn connect(socket: Option<PathBuf>) -> Result<Self, String> {
        let socket = Paths::resolve()
            .map_err(|error| format!("could not find the daemon socket: {error}"))?
            .with_socket(socket)
            .socket_file();

        if !transport::probe(&socket).await {
            return Err(
                "the daemon is not running\nrun 'midi-harbor service install --start' to \
                 install it and start it at login"
                    .to_owned(),
            );
        }
        let channel = transport::connect(&socket)
            .await
            .map_err(|error| error.to_string())?;
        let mut inner = HarborClient::new(channel);

        let server = inner
            .get_server_info(GetServerInfoRequest {})
            .await
            .map_err(|status| format!("the daemon did not respond: {}", status.message()))?
            .into_inner();
        if let Err(mismatch) = check_compatibility(&server) {
            return Err(mismatch.guidance());
        }
        Ok(Self {
            inner,
            server: std::sync::Arc::new(server),
        })
    }

    /// Returns the version the daemon reported.
    pub fn daemon_version(&self) -> &str {
        &self.server.daemon_version
    }

    /// Reports whether the daemon is the build this window is. A daemon too old to say is not.
    pub fn same_build(&self) -> bool {
        self.server.build_id == midi_harbor_core::BUILD_ID
    }

    /// Reads the whole visible state in one pass.
    ///
    /// The four calls are sequential rather than concurrent because they share one channel and
    /// the daemon answers each from the same lock; interleaving them would buy nothing and could
    /// return halves of two different moments.
    pub async fn snapshot(&self) -> Result<Snapshot, String> {
        let mut rpc = self.inner.clone();

        let status = rpc
            .get_status(GetStatusRequest {})
            .await
            .map_err(describe)?
            .into_inner();
        let endpoints = rpc
            .list_endpoints(ListEndpointsRequest::default())
            .await
            .map_err(describe)?
            .into_inner()
            .endpoints;
        let routes = rpc
            .list_routes(ListRoutesRequest::default())
            .await
            .map_err(describe)?
            .into_inner()
            .routes;
        let events = rpc
            .list_events(ListEventsRequest {
                after_id: None,
                limit: EVENT_LIMIT,
                endpoint_id: None,
            })
            .await
            .map_err(describe)?
            .into_inner()
            .events;

        let capabilities = rpc
            .get_capabilities(GetCapabilitiesRequest {})
            .await
            .map_err(describe)?
            .into_inner()
            .capabilities;
        let nearby = rpc
            .list_bluetooth_devices(ListBluetoothDevicesRequest {})
            .await
            .map_err(describe)?
            .into_inner()
            .devices;
        let peers = rpc
            .list_peers(ListPeersRequest {})
            .await
            .map_err(describe)?
            .into_inner()
            .peers;

        Ok(Snapshot {
            status: Some(status),
            endpoints,
            routes,
            events,
            invitations: self.invitations().await,
            capabilities,
            nearby,
            peers,
        })
    }

    /// Takes the invitations currently waiting on an answer.
    ///
    /// A failure here is reported as none waiting rather than as a failed refresh: an older
    /// daemon that does not answer this call should still show its endpoints.
    async fn invitations(&self) -> Vec<Invitation> {
        let mut rpc = self.inner.clone();
        let Ok(response) = rpc.watch_invitations(WatchInvitationsRequest {}).await else {
            return Vec::new();
        };

        let mut stream = response.into_inner();
        let mut waiting = Vec::new();
        while let Ok(Ok(Some(invitation))) =
            tokio::time::timeout(INVITATION_REPLAY, stream.message()).await
        {
            waiting.push(invitation);
        }
        waiting
    }

    /// Answers a machine that asked to connect.
    /// Tells the daemon the warning that the MIDI service stopped has been seen.
    pub async fn dismiss_midi_server_warning(&self) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.dismiss_midi_server_warning(DismissMidiServerWarningRequest {})
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Sends one note out of an endpoint, for testing what listens there.
    pub async fn send_test_note(&self, request: SendTestNoteRequest) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.send_test_note(request)
            .await
            .map(|_| ())
            .map_err(describe)
    }

    pub async fn respond_to_invitation(
        &self,
        invitation: String,
        accept: bool,
        always: bool,
    ) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.respond_to_invitation(RespondToInvitationRequest {
            invitation_id: invitation,
            accept,
            always,
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Switches an endpoint on or off.
    pub async fn set_endpoint_enabled(&self, id: String, enabled: bool) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.set_endpoint_enabled(SetEndpointEnabledRequest { id, enabled })
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Starts watching the MIDI passing through one endpoint.
    ///
    /// The daemon drops messages for a subscriber that falls behind, rather than let a slow
    /// window hold up the endpoint, and says how many on the next one it delivers.
    pub async fn monitor(
        &self,
        endpoint_id: String,
    ) -> Result<tonic::Streaming<MonitoredMessage>, String> {
        let mut rpc = self.inner.clone();
        rpc.monitor_endpoint(MonitorEndpointRequest { endpoint_id })
            .await
            .map(tonic::Response::into_inner)
            .map_err(describe)
    }

    /// Listens for Bluetooth MIDI devices for a while.
    pub async fn scan_bluetooth(&self, seconds: u32) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.start_bluetooth_scan(StartBluetoothScanRequest {
            duration_seconds: seconds,
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Connects to a Bluetooth MIDI device the radio heard, and remembers it.
    pub async fn connect_bluetooth(&self, address: String) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.connect_bluetooth_device(ConnectBluetoothDeviceRequest { address })
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Closes a Bluetooth link, still remembering the device.
    pub async fn disconnect_bluetooth(&self, id: String) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.disconnect_bluetooth_device(DisconnectBluetoothDeviceRequest { id })
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Offers this machine as a Bluetooth MIDI device, or stops.
    pub async fn set_advertising(&self, enabled: bool) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.set_peripheral_advertising(SetPeripheralAdvertisingRequest {
            enabled,
            name: None,
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Creates a virtual port other applications can use, carrying MIDI both ways.
    pub async fn create_virtual_port(
        &self,
        name: String,
        inputs: u8,
        outputs: u8,
    ) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.create_virtual_port(CreateVirtualPortRequest {
            name,
            direction: Direction::Bidirectional.into(),
            inputs: u32::from(inputs),
            outputs: u32::from(outputs),
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Changes how many MIDI In and MIDI Out connectors a virtual port has.
    pub async fn set_connectors(&self, id: String, inputs: u8, outputs: u8) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.set_virtual_port_connectors(SetVirtualPortConnectorsRequest {
            id,
            inputs: u32::from(inputs),
            outputs: u32::from(outputs),
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Renames an endpoint, rewriting the routes that name it.
    ///
    /// Confirmed on the caller's behalf: the window asks before sending this.
    pub async fn rename_endpoint(&self, id: String, new_name: String) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.rename_endpoint(RenameEndpointRequest {
            id,
            new_name,
            confirm: true,
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Deletes a network port, stopping what it carried first.
    pub async fn delete_network_port(&self, id: String) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.delete_network_port(DeleteNetworkPortRequest { id })
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Deletes a virtual port, stopping the notes sounding through it first.
    pub async fn delete_virtual_port(&self, id: String) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.delete_virtual_port(DeleteVirtualPortRequest { id })
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Creates a route between connectors of two endpoints, named by identifier, each connector
    /// counted from zero.
    pub async fn create_route(
        &self,
        (from, from_connector): (String, u8),
        (to, to_connector): (String, u8),
        both_ways: bool,
    ) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.create_route(CreateRouteRequest {
            from,
            to,
            from_connector: u32::from(from_connector) + 1,
            to_connector: u32::from(to_connector) + 1,
            both_ways,
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Changes a route's ends, connectors and whether it carries MIDI both ways.
    pub async fn update_route(
        &self,
        id: String,
        (from, from_connector): (String, u8),
        (to, to_connector): (String, u8),
        both_ways: bool,
    ) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.update_route(UpdateRouteRequest {
            id,
            from,
            to,
            from_connector: u32::from(from_connector) + 1,
            to_connector: u32::from(to_connector) + 1,
            both_ways,
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Switches a route on or off.
    pub async fn set_route_enabled(&self, id: String, enabled: bool) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.set_route_enabled(SetRouteEnabledRequest { id, enabled })
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Connects a network port to a machine it knows, beside the machines it already has when
    /// `alongside`, or in place of its peer.
    pub async fn connect_peer(
        &self,
        session: String,
        peer: String,
        alongside: bool,
    ) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.connect_peer(ConnectPeerRequest {
            session_endpoint_id: session,
            peer_id: peer,
            alongside,
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Makes a network port other machines can find and join.
    pub async fn create_network_port(
        &self,
        name: String,
        control_port: u32,
        policy: InvitationPolicy,
        automatic_port: bool,
        local_name: Option<String>,
    ) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.create_network_session(CreateNetworkSessionRequest {
            name,
            control_port,
            invitation_policy: policy.into(),
            automatic_port: Some(automatic_port),
            local_name,
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Changes a network port's settings in one call. What is `None` stays as it is.
    pub async fn update_network_port(
        &self,
        session: String,
        local_name: Option<String>,
        control_port: Option<u32>,
        policy: InvitationPolicy,
        automatic_port: bool,
    ) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.update_network_port(UpdateNetworkPortRequest {
            id: session,
            local_name,
            control_port,
            invitation_policy: Some(policy.into()),
            automatic_port: Some(automatic_port),
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Remembers a machine by its address, letting it in without asking when `trusted`, and
    /// returns it.
    pub async fn add_machine(
        &self,
        address: String,
        port: u32,
        name: Option<String>,
        trusted: bool,
    ) -> Result<Peer, String> {
        let mut rpc = self.inner.clone();
        rpc.add_manual_peer(AddManualPeerRequest {
            address,
            port,
            name,
            trusted: Some(trusted),
        })
        .await
        .map(tonic::Response::into_inner)
        .map_err(describe)
    }

    /// Remembers a machine by its address and connects a network port to it.
    pub async fn connect_by_address(
        &self,
        session: String,
        address: String,
        port: u32,
        name: Option<String>,
        alongside: bool,
    ) -> Result<(), String> {
        // A machine this computer connects to is one it trusts to connect back.
        let peer = self.add_machine(address, port, name, true).await?;
        self.connect_peer(session, peer.id, alongside).await
    }

    /// Ends one machine's part in a network port, leaving the others connected.
    pub async fn disconnect_machine(&self, session: String, address: String) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.disconnect_machine(DisconnectMachineRequest {
            session_endpoint_id: session,
            address,
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Switches whether a known machine is let in without asking.
    pub async fn set_machine_trusted(&self, id: String, trusted: bool) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.set_peer_trusted(SetPeerTrustedRequest {
            peer_id: id,
            trusted,
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }

    /// Forgets a machine, so its invitations are asked about again.
    pub async fn remove_machine(&self, id: String) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.remove_peer(RemovePeerRequest { peer_id: id })
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Forgets a Bluetooth device, so it stops reconnecting when heard.
    pub async fn forget_bluetooth(&self, id: String) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.forget_bluetooth_device(ForgetBluetoothDeviceRequest { id })
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Forgets remembered hardware and everything configured about it.
    pub async fn forget_device(&self, id: String) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.forget_physical_device(ForgetPhysicalDeviceRequest { id })
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Removes a route.
    pub async fn delete_route(&self, id: String) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.delete_route(DeleteRouteRequest { id })
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Reads the diagnostic report, for a bug report.
    pub async fn export_diagnostics(&self) -> Result<String, String> {
        let mut rpc = self.inner.clone();
        rpc.export_diagnostics(ExportDiagnosticsRequest {
            include_message_log: false,
        })
        .await
        .map(|response| response.into_inner().report)
        .map_err(describe)
    }

    /// Drops a session's peer connection and stops it reconnecting.
    pub async fn disconnect_peer(&self, session: String) -> Result<(), String> {
        let mut rpc = self.inner.clone();
        rpc.disconnect_peer(DisconnectPeerRequest {
            session_endpoint_id: session,
        })
        .await
        .map(|_| ())
        .map_err(describe)
    }
}

/// Reduces a transport status to the sentences a user should see.
///
/// The status code is dropped deliberately: the daemon already phrases its failures for people,
/// and prefixing them with a gRPC code sends users searching for the wrong thing. What to do
/// about it follows when the daemon says, because a notice that names a problem and not its
/// remedy leaves the user where they started.
fn describe(status: tonic::Status) -> String {
    match midi_harbor_ipc::status::reason_guidance(&status) {
        Some(guidance) => format!("{}. {guidance}", status.message()),
        None => status.message().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(id: &str, name: &str) -> Endpoint {
        Endpoint {
            id: id.to_owned(),
            name: name.to_owned(),
            ..Endpoint::default()
        }
    }

    /// Locks that the cached snapshot follows the daemon's change stream by identifier.
    ///
    /// A change replaces what is held, an addition appends, a removal drops, and an addition
    /// announced again is held once: a resync can land between two events about the same
    /// endpoint, and a duplicate row would show one endpoint twice.
    #[test]
    fn a_streamed_change_is_applied_once_by_identifier() {
        let mut snapshot = Snapshot {
            endpoints: vec![endpoint("a", "Keys"), endpoint("b", "Synth")],
            ..Snapshot::default()
        };

        for change in [
            Change::EndpointChanged(endpoint("a", "Stage Keys")),
            Change::EndpointAdded(endpoint("c", "Drums")),
            Change::EndpointRemoved("b".to_owned()),
            Change::EndpointAdded(endpoint("c", "Drums")),
        ] {
            snapshot.apply_change(StateEvent {
                change: Some(change),
            });
        }

        let names: Vec<&str> = snapshot.endpoints.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Stage Keys", "Drums"],
            "the snapshot must hold each endpoint once, as last announced"
        );
    }
}
