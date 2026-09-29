//! The window: a nav bar over six pages, with a docked panel for one endpoint's details.
//!
//! This module holds the window's state and routes user actions to the client. The pages,
//! panel and dialogs are drawn in [`crate::view`] and [`crate::dialogs`], and every piece of
//! wording lives in [`crate::format`], where it is tested.

use crate::client::{Client, Snapshot};
use crate::format;
use crate::onboarding;
use crate::parts;
use crate::{dialogs, view};
use cosmic::app::context_drawer::{self, ContextDrawer};
use cosmic::app::{Core, Task};
use cosmic::executor;
use cosmic::iced::Subscription;
use cosmic::prelude::*;
use cosmic::widget::{nav_bar, segmented_button};
use midi_harbor_ipc::pb::{Endpoint, MonitoredMessage, StateEvent};
use std::path::PathBuf;
use std::time::Duration;

/// How often the window rereads everything from the daemon.
///
/// Changes to endpoints and routes arrive on the state stream as they happen. This catches what
/// the stream does not carry, traffic counts and the event history, and corrects the cache.
///
/// Polling rather than holding `WatchState` open: the daemon's streams exist to avoid a client
/// missing an event, and a window that redraws every second misses nothing a person can see. It
/// also means a daemon restart is recovered by the next tick with no reconnect logic here.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// How long one press of Scan listens for Bluetooth devices, matching the command line's default.
pub const BLUETOOTH_SCAN_SECONDS: u32 = 30;

/// How many monitored messages are kept, newest first. Enough to read back a phrase, few enough
/// that a busy endpoint cannot grow the window's memory without bound.
const MONITOR_LINES: usize = 200;

/// The standard RTP-MIDI port, used when a machine is added by address without one.
pub const DEFAULT_RTP_PORT: u32 = 5004;

/// One page of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// Every endpoint, one section per kind.
    Endpoints,
    /// What is wired to what.
    Routes,
    /// Bluetooth devices nearby, and this computer offered as one.
    Bluetooth,
    /// Recent events from the daemon.
    Activity,
    /// The MIDI passing through one endpoint, as it happens.
    Monitor,
    /// Known machines, capabilities, diagnostics and the background service.
    Settings,
}

impl Page {
    /// Every page, in the order the nav bar lists them.
    pub const ALL: [Page; 6] = [
        Page::Endpoints,
        Page::Routes,
        Page::Bluetooth,
        Page::Activity,
        Page::Monitor,
        Page::Settings,
    ];

    /// Returns the page's name.
    pub const fn title(self) -> &'static str {
        match self {
            Page::Endpoints => "Endpoints",
            Page::Routes => "Routes",
            Page::Bluetooth => "Bluetooth",
            Page::Activity => "Activity",
            Page::Monitor => "Monitor",
            Page::Settings => "Settings",
        }
    }

    /// Returns the page's nav icon.
    const fn icon(self) -> &'static str {
        match self {
            Page::Endpoints => "list",
            Page::Routes => "routes",
            Page::Bluetooth => "bluetooth",
            Page::Activity => "activity",
            Page::Monitor => "monitor",
            Page::Settings => "settings",
        }
    }
}

/// A modal dialog, with what it is about.
///
/// Adding and editing the same thing share one dialog, filled in when editing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// A new virtual port.
    NewPort,
    /// Renaming a virtual port, by identifier.
    EditPort(String),
    /// Deleting a virtual port, which is confirmed first.
    DeletePort(String),
    /// A new network port.
    NewNetworkPort,
    /// Editing a network port, by identifier.
    EditNetworkPort(String),
    /// Deleting a network port, which is confirmed first.
    DeleteNetworkPort(String),
    /// A new route.
    NewRoute,
    /// An existing route, by identifier.
    EditRoute(String),
    /// Connecting a network port to a machine typed by address.
    ConnectByAddress(String),
    /// Remembering a machine typed by address.
    AddMachine,
}

/// The fields of whichever dialog is open.
#[derive(Default)]
pub struct Draft {
    /// The name being typed.
    pub name: String,
    /// A network port's UDP port, as typed.
    pub udp_port: String,
    /// The name other machines see a network port by, as typed; empty means its name.
    pub local_name: String,
    /// Where the chosen invitation policy sits in [`format::POLICIES`].
    pub policy: usize,
    /// A machine's address, as typed.
    pub address: String,
    /// A machine's port, as typed.
    pub machine_port: String,
    /// A virtual port's MIDI In connectors, one to sixteen.
    pub inputs: u8,
    /// A virtual port's MIDI Out connectors, one to sixteen.
    pub outputs: u8,
    /// The connectors a route may start from: each endpoint's MIDI Ins.
    pub sources: Choices,
    /// The connectors a route may end at: each endpoint's MIDI Outs.
    pub destinations: Choices,
    /// The chosen source, by identifier and connector.
    pub from: Option<(String, u8)>,
    /// The chosen destination, by identifier and connector.
    pub to: Option<(String, u8)>,
    /// Whether the route carries MIDI back as well.
    pub both_ways: bool,
    /// Whether a network port shows to other applications as a MIDI port of its name.
    pub automatic_port: bool,
    /// Whether a machine being added is let in without asking.
    pub trusted: bool,
}

/// Endpoints offered in one dropdown, with the labels shown and the identifiers they stand for.
#[derive(Default)]
pub struct Choices {
    /// What the dropdown shows.
    pub labels: Vec<String>,
    /// The endpoint each label stands for.
    pub ids: Vec<String>,
    /// Which of the endpoint's connectors each label stands for, counting from zero.
    pub connectors: Vec<u8>,
}

impl Choices {
    /// Offers every endpoint `wanted` accepts, labelled with its kind, because names repeat
    /// across kinds and a label alone would not say which was meant.
    pub fn of(endpoints: &[Endpoint], wanted: impl Fn(&Endpoint) -> bool) -> Self {
        let (labels, ids): (Vec<String>, Vec<String>) = endpoints
            .iter()
            .filter(|endpoint| wanted(endpoint))
            .map(|endpoint| {
                (
                    format!("{} · {}", endpoint.name, format::kind_name(endpoint)),
                    endpoint.id.clone(),
                )
            })
            .unzip();
        let connectors = vec![0; ids.len()];
        Self {
            labels,
            ids,
            connectors,
        }
    }

    /// Offers every connector a route can start from (`inputs`) or end at: each endpoint's MIDI
    /// Ins or MIDI Outs, one entry per connector, numbered as other applications see them when a
    /// port has several.
    pub fn of_connectors(endpoints: &[Endpoint], inputs: bool) -> Self {
        let mut choices = Self::default();
        for endpoint in endpoints {
            let (ins, outs) = format::connectors(endpoint);
            let count = if inputs { ins } else { outs };
            for connector in 0..count {
                choices.labels.push(format!(
                    "{} · {}",
                    format::connector_label(&endpoint.name, count, connector),
                    format::kind_name(endpoint)
                ));
                choices.ids.push(endpoint.id.clone());
                choices.connectors.push(connector);
            }
        }
        choices
    }

    /// Returns the position of a connector in the list, if it is still offered.
    pub fn position_of(&self, pick: Option<&(String, u8)>) -> Option<usize> {
        let (id, connector) = pick?;
        self.ids
            .iter()
            .zip(&self.connectors)
            .position(|(held, at)| held == id && at == connector)
    }

    /// Returns the connector at a position in the list.
    pub fn pick(&self, index: usize) -> Option<(String, u8)> {
        Some((self.ids.get(index)?.clone(), *self.connectors.get(index)?))
    }

    /// Returns the position of an identifier in the list, if it is still offered.
    pub fn position(&self, id: Option<&String>) -> Option<usize> {
        let id = id?;
        self.ids.iter().position(|candidate| candidate == id)
    }
}

/// The note the monitor page sends out of the endpoint it watches.
///
/// Kept apart from the monitor's state, which starts afresh for each endpoint chosen, so a note
/// set up to test a cue stays set while the user tries it on one port after another.
pub struct TestNote {
    /// The note, 0 to 127, chosen from `notes`.
    pub note: u8,
    /// Every MIDI note, by number and key, for the note picker.
    pub notes: Vec<String>,
    /// The channel, 1 to 16, as typed.
    pub channel: String,
    /// The velocity, 1 to 127, as typed.
    pub velocity: String,
}

impl Default for TestNote {
    fn default() -> Self {
        // Middle C on the first channel, at a velocity that sounds without being loud.
        Self {
            note: 60,
            notes: (0..=127).map(crate::format::note_label).collect(),
            channel: "1".to_owned(),
            velocity: "100".to_owned(),
        }
    }
}

/// Which part of the test note is being typed.
#[derive(Debug, Clone, Copy)]
pub enum TestNoteField {
    /// The channel.
    Channel,
    /// The velocity.
    Velocity,
}

/// What the monitor page is watching, and what it has seen.
#[derive(Default)]
pub struct MonitorState {
    /// Every endpoint that can be watched.
    pub choices: Choices,
    /// The endpoint being watched, by identifier.
    pub watching: Option<String>,
    /// What passed through it, newest first.
    pub seen: std::collections::VecDeque<MonitoredMessage>,
    /// Messages the daemon dropped for this window because it fell behind.
    pub dropped: u64,
    /// Why watching stopped, when it did.
    pub ended: Option<String>,
}

/// What the window knows about the service while it cannot reach the daemon.
#[derive(Default)]
pub struct ServiceState {
    /// What the service manager said.
    pub checked: Option<onboarding::Checked>,
    /// Whether an offer is being carried out, so it is not started twice.
    pub working: bool,
    /// Why the last attempt to check or set it up failed.
    pub failed: Option<String>,
}

/// What the window reacts to.
#[derive(Debug, Clone)]
pub enum Message {
    /// The periodic refresh fired.
    Tick,
    /// A connection attempt finished.
    Connected(Result<Client, String>),
    /// A snapshot arrived, or could not be read.
    Refreshed(Box<Result<Snapshot, String>>),
    /// The daemon changed something, as reported on its state stream.
    StateChanged(Box<StateEvent>),
    /// The state stream ended, so the cache may have missed changes and is read again.
    StateStreamEnded(String),
    /// A page was chosen from a button rather than the nav bar.
    Go(Page),
    /// An endpoint was clicked, opening its panel.
    Select(String),
    /// The panel was closed.
    ClosePanel,
    /// A dialog was asked for.
    Open(Dialog),
    /// A new route was asked for from one endpoint's panel, with that endpoint filled in.
    OpenRouteFrom(String),
    /// The open dialog was abandoned.
    CloseDialog,
    /// The open dialog was confirmed.
    Confirm,
    /// A dialog's name was edited.
    DraftName(String),
    /// A dialog's UDP port was edited.
    DraftUdpPort(String),
    /// The network port dialog's name for other machines changed.
    DraftLocalName(String),
    /// A dialog's invitation policy was chosen.
    DraftPolicy(usize),
    /// The network port dialog's automatic virtual port switch changed.
    DraftAutomaticPort(bool),
    /// A dialog's machine address was edited.
    DraftAddress(String),
    /// A dialog's machine port was edited.
    DraftMachinePort(String),
    /// A route's source was chosen.
    DraftFrom(usize),
    /// A route's destination was chosen.
    DraftTo(usize),
    /// Whether a route carries MIDI both ways was switched.
    DraftBothWays(bool),
    /// A port's MIDI In connector count was changed.
    DraftInputs(u8),
    /// A port's MIDI Out connector count was changed.
    DraftOutputs(u8),
    /// An endpoint was switched on or off.
    ToggleEndpoint(String, bool),
    /// A route was switched on or off.
    ToggleRoute(String, bool),
    /// A route was deleted.
    DeleteRoute(String),
    /// A network port was asked to connect to a machine it knows, beside those it has when the
    /// flag is set.
    ConnectPeer(String, String, bool),
    /// One machine was disconnected from a network port, by the port and the machine's address.
    DisconnectMachine(String, String),
    /// A network port's machines were disconnected.
    DisconnectPeer(String),
    /// A Bluetooth link was closed.
    DisconnectBluetooth(String),
    /// A Bluetooth device was forgotten.
    ForgetBluetooth(String),
    /// Remembered hardware was forgotten.
    ForgetDevice(String),
    /// A known machine was forgotten.
    ForgetMachine(String),
    /// A known machine's "let in without asking" was switched.
    TrustMachine(String, bool),
    /// The add-a-machine dialog's "let in without asking" was switched.
    DraftTrusted(bool),
    /// A machine that asked to connect was answered.
    AnswerInvitation {
        /// Which invitation.
        id: String,
        /// Whether to let the machine in.
        accept: bool,
        /// Whether to remember it, so it is never asked about again.
        always: bool,
    },
    /// A user action finished; only a failure is worth reporting.
    Acted(Result<(), String>),
    /// The last error notice was dismissed.
    DismissError,
    /// The warning that the MIDI service stopped was dismissed.
    DismissMidiServerWarning,
    /// An endpoint's MIDI was asked to be watched, from its panel.
    Watch(String),
    /// An endpoint was chosen to watch.
    PickMonitored(usize),
    /// The test note was chosen from the note picker, by its position.
    PickTestNote(usize),
    /// Part of the test note was typed.
    EditTestNote(TestNoteField, String),
    /// The test note was asked to be sent out of the endpoint being watched.
    SendTestNote,
    /// A message passed through the watched endpoint.
    Monitored(Box<MonitoredMessage>),
    /// Watching stopped, and why.
    MonitorEnded(String),
    /// The activity filter was changed.
    ActivityFilter(segmented_button::Entity),
    /// A Bluetooth scan was asked for.
    ScanBluetooth,
    /// A nearby Bluetooth device was chosen to connect to.
    ConnectBluetooth(String),
    /// Offering this computer as a Bluetooth device was switched on or off.
    SetAdvertising(bool),
    /// A diagnostic report was asked for.
    ExportDiagnostics,
    /// The diagnostic report was written, to the returned path, or could not be.
    DiagnosticsSaved(Result<String, String>),
    /// The service manager said what state the service is in.
    ServiceChecked(Result<onboarding::Checked, String>),
    /// The user took what was offered for the missing daemon.
    SetUpService,
    /// Setting the service up finished.
    ServiceSetUp(Result<(), String>),
    /// The user chose something through the app's menus, or the menu bar item in App Store mode.
    #[cfg(target_os = "macos")]
    Shell(midi_harbor_platform::appkit::ShellAction),
    /// The event loop is running, so the menus can replace winit's default.
    #[cfg(target_os = "macos")]
    InstallMenus,
    /// The app's daemon was started or found, or could not be (App Store mode).
    #[cfg(target_os = "macos")]
    DaemonOwned(Result<(), String>),
    /// The app's daemon has stopped, so macOS can finish quitting (App Store mode).
    #[cfg(target_os = "macos")]
    DaemonStopped,
    /// Start at login was switched on or off (App Store mode).
    #[cfg(target_os = "macos")]
    SetStartAtLogin(bool),
    /// The one-time offer of Start at login was answered (App Store mode).
    #[cfg(target_os = "macos")]
    AnswerLoginOffer(bool),
}

// Client holds only a cloneable channel, so a message carrying one stays cheap.
impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Client")
    }
}

/// The window state.
pub struct App {
    core: Core,
    nav: nav_bar::Model,
    /// The page showing.
    pub page: Page,
    client: Option<Client>,
    /// What the daemon last said.
    pub snapshot: Snapshot,
    /// Why the daemon cannot be reached, which replaces the whole view when set.
    pub unreachable: Option<String>,
    /// Why the last action failed, shown as a notice above the current page.
    pub action_error: Option<String>,
    /// The socket named with `--socket`, used for every connection and reconnection.
    socket: Option<PathBuf>,
    /// The monitor page.
    pub monitor: MonitorState,
    /// The note the monitor page sends, as typed.
    pub test_note: TestNote,
    /// The background service, while the daemon cannot be reached.
    pub service: ServiceState,
    /// Counts state streams started, so an ended one is replaced by a new one rather than the
    /// subscription deciding it is still the same stream.
    stream_generation: u64,
    /// The endpoint whose panel is open, by identifier.
    pub selected: Option<String>,
    /// The dialog open, if any.
    pub dialog: Option<Dialog>,
    /// The open dialog's fields.
    pub draft: Draft,
    /// Everything, or problems only, on the activity page.
    pub activity_filter: segmented_button::SingleSelectModel,
    /// Where the last diagnostic report was written.
    pub saved_report: Option<String>,
    /// App Store mode, when this is the sandboxed App Store build (feature 014).
    #[cfg(target_os = "macos")]
    pub store: Option<crate::app_store::StoreState>,
}

impl App {
    /// Runs a call against the daemon, reporting only whether it failed.
    fn act<F, Fut>(&self, call: F) -> Task<Message>
    where
        F: FnOnce(Client) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let Some(client) = self.client.clone() else {
            return Task::none();
        };
        let future = call(client);
        cosmic::task::future(async move { Message::Acted(future.await) })
    }

    /// Reports whether the chosen ends could carry MIDI back the other way, which needs the
    /// destination to send and the source to receive.
    pub fn two_way_possible(&self) -> bool {
        let (Some((from, _)), Some((to, _))) = (&self.draft.from, &self.draft.to) else {
            return false;
        };
        match (self.endpoint(from), self.endpoint(to)) {
            (Some(source), Some(destination)) => format::can_be_two_way(source, destination),
            _ => false,
        }
    }

    /// Reports whether a network port has any machine taking part, so a new one joins beside
    /// them rather than replacing its peer.
    pub fn has_machines(&self, session: &str) -> bool {
        matches!(
            self.endpoint(session).and_then(|e| e.detail.as_ref()),
            Some(midi_harbor_ipc::pb::endpoint::Detail::NetworkSession(detail))
                if !detail.machines.is_empty()
        )
    }

    /// Returns an endpoint by identifier.
    pub fn endpoint(&self, id: &str) -> Option<&Endpoint> {
        self.snapshot
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == id)
    }

    /// Goes to a page, keeping the nav bar in step and closing the panel, which belongs to the
    /// page it was opened from.
    fn go(&mut self, page: Page) {
        self.page = page;
        if let Some(position) = Page::ALL.iter().position(|p| *p == page) {
            self.nav
                .activate_position(u16::try_from(position).unwrap_or(0));
        }
        self.core.window.show_context = false;
    }

    /// Rebuilds the monitor's choices from the cached endpoints.
    fn rebuild_choices(&mut self) {
        self.monitor.choices = Choices::of(&self.snapshot.endpoints, |_| true);
    }

    /// Fills the draft for a dialog about to open.
    fn open(&mut self, dialog: Dialog) {
        let mut draft = Draft {
            inputs: 1,
            outputs: 1,
            automatic_port: true,
            trusted: true,
            sources: Choices::of_connectors(&self.snapshot.endpoints, true),
            destinations: Choices::of_connectors(&self.snapshot.endpoints, false),
            ..Draft::default()
        };
        match &dialog {
            Dialog::EditPort(id) => {
                if let Some(endpoint) = self.endpoint(id) {
                    draft.name = endpoint.name.clone();
                    (draft.inputs, draft.outputs) = format::connectors(endpoint);
                }
            }
            Dialog::EditNetworkPort(id) => {
                if let Some(endpoint) = self.endpoint(id) {
                    draft.name = endpoint.name.clone();
                    if let Some(midi_harbor_ipc::pb::endpoint::Detail::NetworkSession(session)) =
                        &endpoint.detail
                    {
                        draft.udp_port = session.control_port.to_string();
                        draft.local_name = session.local_name.clone();
                        draft.policy = format::policy_position(session.invitation_policy());
                        draft.automatic_port = session.automatic_port;
                    }
                }
            }
            Dialog::EditRoute(id) => {
                if let Some(route) = self.snapshot.routes.iter().find(|r| r.id == *id) {
                    let index = |number: u32| u8::try_from(number.saturating_sub(1)).unwrap_or(0);
                    draft.from = route
                        .from_id
                        .clone()
                        .map(|id| (id, index(route.from_connector)));
                    draft.to = route
                        .to_id
                        .clone()
                        .map(|id| (id, index(route.to_connector)));
                    draft.both_ways = route.both_ways;
                }
            }
            _ => {}
        }
        self.draft = draft;
        self.dialog = Some(dialog);
    }

    /// Carries out the open dialog.
    fn confirm(&mut self) -> Task<Message> {
        let Some(dialog) = self.dialog.take() else {
            return Task::none();
        };
        let name = self.draft.name.trim().to_owned();
        let policy = format::POLICIES
            .get(self.draft.policy)
            .map_or(midi_harbor_ipc::pb::InvitationPolicy::Prompt, |(_, p)| *p);
        match dialog {
            Dialog::NewPort if !name.is_empty() => {
                let (inputs, outputs) = (self.draft.inputs, self.draft.outputs);
                self.act(move |client| async move {
                    client.create_virtual_port(name, inputs, outputs).await
                })
            }
            Dialog::EditPort(id) => {
                let Some(endpoint) = self.endpoint(&id) else {
                    return Task::none();
                };
                let rename = !name.is_empty() && endpoint.name != name;
                let counts = (self.draft.inputs, self.draft.outputs);
                let recount = format::connectors(endpoint) != counts;
                self.act(move |client| async move {
                    if rename {
                        client.rename_endpoint(id.clone(), name).await?;
                    }
                    if recount {
                        client.set_connectors(id, counts.0, counts.1).await?;
                    }
                    Ok(())
                })
            }
            Dialog::DeletePort(id) => {
                if self.selected.as_ref() == Some(&id) {
                    self.selected = None;
                    self.core.window.show_context = false;
                }
                self.act(move |client| async move { client.delete_virtual_port(id).await })
            }
            Dialog::DeleteNetworkPort(id) => {
                if self.selected.as_ref() == Some(&id) {
                    self.selected = None;
                    self.core.window.show_context = false;
                }
                self.act(move |client| async move { client.delete_network_port(id).await })
            }
            Dialog::NewNetworkPort if !name.is_empty() => {
                // An empty port lets the daemon choose one.
                let port = self.draft.udp_port.trim().parse().unwrap_or(0);
                let automatic_port = self.draft.automatic_port;
                let local_name =
                    Some(self.draft.local_name.trim().to_owned()).filter(|typed| !typed.is_empty());
                self.act(move |client| async move {
                    client
                        .create_network_port(name, port, policy, automatic_port, local_name)
                        .await
                })
            }
            Dialog::EditNetworkPort(id) => {
                let rename = self
                    .endpoint(&id)
                    .is_some_and(|e| !name.is_empty() && e.name != name);
                let current = match self.endpoint(&id).and_then(|e| e.detail.as_ref()) {
                    Some(midi_harbor_ipc::pb::endpoint::Detail::NetworkSession(session)) => {
                        Some((session.local_name.as_str(), session.control_port))
                    }
                    _ => None,
                };
                let (local_name, control_port) = format::network_port_changes(
                    &self.draft.local_name,
                    &self.draft.udp_port,
                    current,
                );
                let automatic_port = self.draft.automatic_port;
                self.act(move |client| async move {
                    if rename {
                        client.rename_endpoint(id.clone(), name).await?;
                    }
                    client
                        .update_network_port(id, local_name, control_port, policy, automatic_port)
                        .await
                })
            }
            Dialog::NewRoute => {
                let (Some(from), Some(to)) = (self.draft.from.clone(), self.draft.to.clone())
                else {
                    return Task::none();
                };
                let both_ways = self.draft.both_ways && self.two_way_possible();
                self.act(
                    move |client| async move { client.create_route(from, to, both_ways).await },
                )
            }
            Dialog::EditRoute(id) => {
                let (Some(from), Some(to)) = (self.draft.from.clone(), self.draft.to.clone())
                else {
                    return Task::none();
                };
                let both_ways = self.draft.both_ways && self.two_way_possible();
                let id = id.clone();
                self.act(
                    move |client| async move { client.update_route(id, from, to, both_ways).await },
                )
            }
            Dialog::ConnectByAddress(session) => {
                let Some((address, port, name)) = self.typed_machine() else {
                    return Task::none();
                };
                let alongside = self.has_machines(&session);
                self.act(move |client| async move {
                    client
                        .connect_by_address(session, address, port, name, alongside)
                        .await
                })
            }
            Dialog::AddMachine => {
                let Some((address, port, name)) = self.typed_machine() else {
                    return Task::none();
                };
                let trusted = self.draft.trusted;
                self.act(move |client| async move {
                    client
                        .add_machine(address, port, name, trusted)
                        .await
                        .map(|_| ())
                })
            }
            _ => Task::none(),
        }
    }

    /// Returns the machine typed into a dialog: its address, its port, defaulting to 5004, and
    /// its name, when one was given.
    fn typed_machine(&self) -> Option<(String, u32, Option<String>)> {
        let address = self.draft.address.trim().to_owned();
        if address.is_empty() {
            return None;
        }
        let port = self
            .draft
            .machine_port
            .trim()
            .parse()
            .unwrap_or(DEFAULT_RTP_PORT);
        let name = Some(self.draft.name.trim().to_owned()).filter(|name| !name.is_empty());
        Some((address, port, name))
    }

    /// Reads the snapshot again at once, so a control reflects what the daemon did.
    fn refresh(&self) -> Task<Message> {
        let Some(client) = self.client.clone() else {
            return Task::none();
        };
        cosmic::task::future(async move { Message::Refreshed(Box::new(client.snapshot().await)) })
    }
}

impl cosmic::Application for App {
    type Executor = executor::Default;
    type Flags = Option<PathBuf>;
    type Message = Message;

    const APP_ID: &'static str = "com.mrgeckosmedia.MidiHarbor";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(mut core: Core, socket: Self::Flags) -> (Self, Task<Self::Message>) {
        // Docked beside the page rather than laid over it, so a dialog can open on top of the
        // panel without closing it: an overlay panel and a dialog are both popovers, and only
        // one shows at a time.
        core.window.context_is_overlay = false;

        let mut nav = nav_bar::Model::default();
        for page in Page::ALL {
            nav.insert()
                .text(page.title())
                .icon(parts::nav_icon(page.icon()))
                .data(page);
        }
        nav.activate_position(0);

        let mut activity_filter = segmented_button::SingleSelectModel::default();
        activity_filter.insert().text("Everything");
        activity_filter.insert().text("Problems only");
        activity_filter.activate_position(0);

        let mut app = App {
            core,
            nav,
            page: Page::Endpoints,
            client: None,
            snapshot: Snapshot::default(),
            unreachable: None,
            action_error: None,
            socket: socket.clone(),
            monitor: MonitorState::default(),
            test_note: TestNote::default(),
            service: ServiceState::default(),
            stream_generation: 0,
            selected: None,
            dialog: None,
            draft: Draft::default(),
            activity_filter,
            saved_report: None,
            #[cfg(target_os = "macos")]
            store: crate::app_store::StoreState::begin(socket.as_ref()),
        };

        // Connect before the first tick, so the window does not open on an empty list it could
        // have filled.
        let connect =
            cosmic::task::future(async { Message::Connected(Client::connect(socket).await) });
        // The window's title, which taskbars and window lists show; libcosmic leaves it empty,
        // and the header draws its own.
        let connect = match app.core.main_window_id() {
            Some(id) => Task::batch([connect, app.set_window_title("Midi Harbor".to_owned(), id)]),
            None => connect,
        };
        // The menus as a message rather than now: the window is set up before the event loop
        // runs, and winit builds its default menu once it does, over anything installed earlier.
        #[cfg(target_os = "macos")]
        let connect = Task::batch([
            connect,
            Task::done(cosmic::Action::App(Message::InstallMenus)),
            app.start_store(),
        ]);
        (app, connect)
    }

    fn nav_model(&self) -> Option<&nav_bar::Model> {
        Some(&self.nav)
    }

    fn on_nav_select(&mut self, id: nav_bar::Id) -> Task<Self::Message> {
        self.nav.activate(id);
        if let Some(page) = self.nav.data::<Page>(id).copied() {
            self.go(page);
        }
        Task::none()
    }

    fn context_drawer(&self) -> Option<ContextDrawer<'_, Self::Message>> {
        if !self.core.window.show_context {
            return None;
        }
        let endpoint = self.endpoint(self.selected.as_deref()?)?;
        Some(
            context_drawer::context_drawer(view::panel(self, endpoint), Message::ClosePanel)
                .title(endpoint.name.clone()),
        )
    }

    #[cfg(target_os = "macos")]
    fn on_close_requested(&self, _id: cosmic::iced::window::Id) -> Option<Self::Message> {
        // In App Store mode closing the window leaves Midi Harbor running in the menu bar.
        self.store
            .as_ref()
            .map(|_| Message::Shell(midi_harbor_platform::appkit::ShellAction::CloseWindow))
    }

    fn dialog(&self) -> Option<Element<'_, Self::Message>> {
        self.dialog
            .as_ref()
            .map(|dialog| dialogs::view(self, dialog))
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let tick = cosmic::iced::time::every(REFRESH_INTERVAL).map(|_| Message::Tick);
        // The stream runs only while its page is showing: a monitor nobody is looking at still
        // costs the daemon a copy of every message.
        let watching = self
            .monitor
            .watching
            .clone()
            .filter(|_| self.page == Page::Monitor);
        let mut subscriptions = vec![tick];
        if let Some(id) = watching {
            subscriptions.push(Subscription::run_with(
                (self.socket.clone(), id),
                monitor_stream,
            ));
        }
        // Followed only while connected; a reconnection starts a new one.
        if self.client.is_some() {
            subscriptions.push(Subscription::run_with(
                (self.socket.clone(), self.stream_generation),
                state_stream,
            ));
        }
        #[cfg(target_os = "macos")]
        subscriptions.push(Subscription::run(shell_actions));
        Subscription::batch(subscriptions)
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            #[cfg(target_os = "macos")]
            Message::Shell(action) => self.shell(action),
            #[cfg(target_os = "macos")]
            Message::InstallMenus => {
                let pages = Page::ALL.map(Page::title);
                let store = self.store.is_some();
                if let Err(error) = midi_harbor_platform::appkit::install_menus(store, &pages) {
                    tracing::error!(error = %error, "failed to set up the menu bar");
                }
                Task::none()
            }
            #[cfg(target_os = "macos")]
            Message::DaemonOwned(result) => {
                if let Some(store) = &mut self.store {
                    store.failed = result.err();
                }
                // Connect at once rather than at the next tick.
                let socket = self.socket.clone();
                cosmic::task::future(async { Message::Connected(Client::connect(socket).await) })
            }
            #[cfg(target_os = "macos")]
            Message::DaemonStopped => {
                midi_harbor_platform::appkit::reply_to_terminate();
                Task::none()
            }
            #[cfg(target_os = "macos")]
            Message::SetStartAtLogin(on) => {
                self.set_start_at_login(on);
                Task::none()
            }
            #[cfg(target_os = "macos")]
            Message::AnswerLoginOffer(yes) => {
                if let Some(store) = &mut self.store {
                    store.login_offered();
                }
                if yes {
                    self.set_start_at_login(true);
                }
                Task::none()
            }
            Message::Tick => {
                // A tick with no client is a reconnect attempt, which is how the window recovers
                // from a daemon that was restarted while it was open.
                if self.client.is_none() {
                    let socket = self.socket.clone();
                    return cosmic::task::future(async {
                        Message::Connected(Client::connect(socket).await)
                    });
                }
                self.refresh()
            }
            Message::Connected(Ok(client)) => {
                self.client = Some(client);
                self.unreachable = None;
                self.service = ServiceState::default();
                self.refresh()
            }
            Message::Connected(Err(error)) => {
                self.unreachable = Some(error);
                // Only the standard socket is the service's. A window pointed elsewhere with
                // --socket would not reach a daemon the service started, so it offers nothing.
                #[cfg(target_os = "macos")]
                if self.store.is_some() {
                    return Task::none();
                }
                let ask = self.socket.is_none()
                    && self.service.checked.is_none()
                    && !self.service.working;
                if ask {
                    cosmic::task::future(async {
                        Message::ServiceChecked(onboarding::check().await)
                    })
                } else {
                    Task::none()
                }
            }
            Message::ServiceChecked(result) => {
                match result {
                    Ok(checked) => self.service.checked = Some(checked),
                    Err(error) => self.service.failed = Some(error),
                }
                Task::none()
            }
            Message::SetUpService => {
                let Some(checked) = &self.service.checked else {
                    return Task::none();
                };
                if self.service.working {
                    return Task::none();
                }
                self.service.working = true;
                self.service.failed = None;
                let offer = checked.offer.clone();
                cosmic::task::future(
                    async move { Message::ServiceSetUp(onboarding::take(offer).await) },
                )
            }
            Message::ServiceSetUp(result) => {
                self.service.working = false;
                match result {
                    // The next tick reconnects; asking again then would only repeat the offer.
                    Ok(()) => self.service.checked = None,
                    Err(error) => self.service.failed = Some(error),
                }
                Task::none()
            }
            Message::Refreshed(result) => {
                match *result {
                    Ok(snapshot) => {
                        self.snapshot = snapshot;
                        self.rebuild_choices();
                        self.unreachable = None;
                        // A panel for an endpoint that has gone closes with it.
                        if self
                            .selected
                            .as_deref()
                            .is_some_and(|id| self.endpoint(id).is_none())
                        {
                            self.selected = None;
                            self.core.window.show_context = false;
                        }
                    }
                    Err(error) => {
                        // Drop the client so the next tick reconnects rather than retrying a
                        // channel whose daemon has gone.
                        self.client = None;
                        self.unreachable = Some(error);
                    }
                }
                Task::none()
            }
            Message::StateChanged(event) => {
                self.snapshot.apply_change(*event);
                self.rebuild_choices();
                Task::none()
            }
            Message::StateStreamEnded(reason) => {
                // Changes may have been missed, including by falling behind, which the daemon
                // reports as the stream ending. Read everything again and follow a new stream.
                tracing::debug!(reason = %reason, "state stream ended; resynchronising");
                self.stream_generation = self.stream_generation.wrapping_add(1);
                self.refresh()
            }
            Message::Go(page) => {
                self.go(page);
                Task::none()
            }
            Message::Select(id) => {
                self.selected = Some(id);
                self.core.window.show_context = true;
                Task::none()
            }
            Message::ClosePanel => {
                self.core.window.show_context = false;
                Task::none()
            }
            Message::Open(dialog) => {
                self.open(dialog);
                Task::none()
            }
            Message::OpenRouteFrom(id) => {
                self.open(Dialog::NewRoute);
                // The endpoint fills whichever end it can take, preferring the source, at its
                // first connector.
                if self.draft.sources.ids.contains(&id) {
                    self.draft.from = Some((id, 0));
                } else if self.draft.destinations.ids.contains(&id) {
                    self.draft.to = Some((id, 0));
                }
                Task::none()
            }
            Message::CloseDialog => {
                self.dialog = None;
                Task::none()
            }
            Message::Confirm => self.confirm(),
            Message::DraftName(name) => {
                self.draft.name = name;
                Task::none()
            }
            Message::DraftLocalName(typed) => {
                self.draft.local_name = typed;
                Task::none()
            }
            Message::DraftUdpPort(port) => {
                self.draft.udp_port = port;
                Task::none()
            }
            Message::DraftAutomaticPort(on) => {
                self.draft.automatic_port = on;
                Task::none()
            }
            Message::DraftPolicy(policy) => {
                self.draft.policy = policy;
                Task::none()
            }
            Message::DraftAddress(address) => {
                self.draft.address = address;
                Task::none()
            }
            Message::DraftMachinePort(port) => {
                self.draft.machine_port = port;
                Task::none()
            }
            Message::DraftFrom(index) => {
                self.draft.from = self.draft.sources.pick(index);
                Task::none()
            }
            Message::DraftTo(index) => {
                self.draft.to = self.draft.destinations.pick(index);
                Task::none()
            }
            Message::DraftBothWays(on) => {
                self.draft.both_ways = on;
                Task::none()
            }
            Message::DraftInputs(count) => {
                self.draft.inputs = count.clamp(1, 16);
                Task::none()
            }
            Message::DraftOutputs(count) => {
                self.draft.outputs = count.clamp(1, 16);
                Task::none()
            }
            Message::ToggleEndpoint(id, enabled) => self
                .act(move |client| async move { client.set_endpoint_enabled(id, enabled).await }),
            Message::ToggleRoute(id, enabled) => {
                self.act(move |client| async move { client.set_route_enabled(id, enabled).await })
            }
            Message::DeleteRoute(id) => {
                self.dialog = None;
                self.act(move |client| async move { client.delete_route(id).await })
            }
            Message::ConnectPeer(session, peer, alongside) => self.act(move |client| async move {
                client.connect_peer(session, peer, alongside).await
            }),
            Message::DisconnectMachine(session, address) => {
                self.act(
                    move |client| async move { client.disconnect_machine(session, address).await },
                )
            }
            Message::DisconnectPeer(id) => {
                self.act(move |client| async move { client.disconnect_peer(id).await })
            }
            Message::DisconnectBluetooth(id) => {
                self.act(move |client| async move { client.disconnect_bluetooth(id).await })
            }
            Message::ForgetBluetooth(id) => {
                self.act(move |client| async move { client.forget_bluetooth(id).await })
            }
            Message::ForgetDevice(id) => {
                self.act(move |client| async move { client.forget_device(id).await })
            }
            Message::TrustMachine(id, trusted) => {
                self.act(move |client| async move { client.set_machine_trusted(id, trusted).await })
            }
            Message::DraftTrusted(on) => {
                self.draft.trusted = on;
                Task::none()
            }
            Message::ForgetMachine(id) => {
                self.act(move |client| async move { client.remove_machine(id).await })
            }
            Message::AnswerInvitation { id, accept, always } => {
                // Dropped from the snapshot straight away rather than waiting for the next
                // refresh, so the prompt cannot be answered twice while the call is in flight.
                self.snapshot
                    .invitations
                    .retain(|invitation| invitation.invitation_id != id);
                self.act(move |client| async move {
                    client.respond_to_invitation(id, accept, always).await
                })
            }
            Message::Acted(result) => {
                self.action_error = result.err();
                self.refresh()
            }
            Message::DismissError => {
                self.action_error = None;
                Task::none()
            }
            Message::DismissMidiServerWarning => {
                // The daemon is told, so every window and the command line stop showing it; it
                // goes from this window at once rather than at the next refresh.
                if let Some(status) = self.snapshot.status.as_mut() {
                    status.midi_server_replaced_at = None;
                }
                self.act(|client| async move { client.dismiss_midi_server_warning().await })
            }
            Message::Watch(id) => {
                self.go(Page::Monitor);
                if self.monitor.watching.as_ref() != Some(&id) {
                    self.monitor = MonitorState {
                        choices: std::mem::take(&mut self.monitor.choices),
                        watching: Some(id),
                        ..MonitorState::default()
                    };
                }
                Task::none()
            }
            Message::PickMonitored(index) => {
                let chosen = self.monitor.choices.ids.get(index).cloned();
                if chosen != self.monitor.watching {
                    self.monitor = MonitorState {
                        choices: std::mem::take(&mut self.monitor.choices),
                        watching: chosen,
                        ..MonitorState::default()
                    };
                }
                Task::none()
            }
            Message::PickTestNote(index) => {
                if let Ok(note) = u8::try_from(index)
                    && note <= 127
                {
                    self.test_note.note = note;
                }
                Task::none()
            }
            Message::EditTestNote(field, value) => {
                let typed = match field {
                    TestNoteField::Channel => &mut self.test_note.channel,
                    TestNoteField::Velocity => &mut self.test_note.velocity,
                };
                *typed = value;
                Task::none()
            }
            Message::SendTestNote => {
                let Some(endpoint) = self.monitor.watching.clone() else {
                    return Task::none();
                };
                // The daemon checks each against MIDI's ranges; here only that it is a number.
                let number = |typed: &str, what: &str| {
                    typed
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| format!("the {what} must be a number"))
                };
                let request = number(&self.test_note.channel, "channel").and_then(|channel| {
                    Ok(midi_harbor_ipc::pb::SendTestNoteRequest {
                        endpoint_id: endpoint,
                        channel,
                        note: u32::from(self.test_note.note),
                        velocity: number(&self.test_note.velocity, "velocity")?,
                        length_ms: None,
                    })
                });
                match request {
                    Ok(request) => {
                        self.action_error = None;
                        self.act(move |client| async move { client.send_test_note(request).await })
                    }
                    Err(error) => {
                        self.action_error = Some(error);
                        Task::none()
                    }
                }
            }
            Message::Monitored(message) => {
                self.monitor.dropped = self.monitor.dropped.saturating_add(message.dropped);
                self.monitor.ended = None;
                self.monitor.seen.push_front(*message);
                self.monitor.seen.truncate(MONITOR_LINES);
                Task::none()
            }
            Message::MonitorEnded(reason) => {
                self.monitor.ended = Some(reason);
                Task::none()
            }
            Message::ActivityFilter(entity) => {
                self.activity_filter.activate(entity);
                Task::none()
            }
            Message::ScanBluetooth => self
                .act(|client| async move { client.scan_bluetooth(BLUETOOTH_SCAN_SECONDS).await }),
            Message::ConnectBluetooth(address) => {
                self.act(move |client| async move { client.connect_bluetooth(address).await })
            }
            Message::SetAdvertising(enabled) => {
                self.act(move |client| async move { client.set_advertising(enabled).await })
            }
            Message::ExportDiagnostics => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                cosmic::task::future(async move {
                    let saved = match client.export_diagnostics().await {
                        Ok(report) => save_report(&report),
                        Err(error) => Err(error),
                    };
                    Message::DiagnosticsSaved(saved)
                })
            }
            Message::DiagnosticsSaved(result) => {
                match result {
                    Ok(path) => self.saved_report = Some(path),
                    Err(error) => self.action_error = Some(error),
                }
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Self::Message> {
        view::window(self)
    }
}

#[cfg(target_os = "macos")]
impl App {
    /// Starts App Store mode: the Dock icon, the window's first state, and the daemon.
    fn start_store(&self) -> Task<Message> {
        let Some(store) = &self.store else {
            return Task::none();
        };
        midi_harbor_platform::appkit::set_dock_visible(store.visible);
        let mut tasks = Vec::new();
        if !store.visible
            && let Some(id) = self.core.main_window_id()
        {
            tasks.push(cosmic::iced::window::set_mode(
                id,
                cosmic::iced::window::Mode::Hidden,
            ));
        }
        let owner = std::sync::Arc::clone(&store.owner);
        let socket = store.socket.clone();
        tasks.push(cosmic::task::future(async move {
            Message::DaemonOwned(crate::app_store::own_daemon(owner, socket).await)
        }));
        Task::batch(tasks)
    }

    /// Carries out what the user chose through the app's menus or the menu bar item.
    fn shell(&mut self, action: midi_harbor_platform::appkit::ShellAction) -> Task<Message> {
        use cosmic::iced::window;
        use midi_harbor_platform::appkit::{ShellAction, set_dock_visible};

        // What the menus do in the window, in either build. A new endpoint or route waits for
        // the daemon, since the dialog could not create it, and opens on the page listing it.
        let reachable = self.client.is_some() && self.unreachable.is_none();
        match action {
            ShellAction::ShowSettings => {
                self.go(Page::Settings);
                return Task::none();
            }
            ShellAction::ShowPage(position) => {
                if let Some(page) = Page::ALL.get(position) {
                    self.go(*page);
                }
                return Task::none();
            }
            ShellAction::NewVirtualPort | ShellAction::NewNetworkPort if reachable => {
                self.go(Page::Endpoints);
                self.open(if action == ShellAction::NewVirtualPort {
                    Dialog::NewPort
                } else {
                    Dialog::NewNetworkPort
                });
                return Task::none();
            }
            ShellAction::NewRoute if reachable => {
                self.go(Page::Routes);
                self.open(Dialog::NewRoute);
                return Task::none();
            }
            ShellAction::NewVirtualPort | ShellAction::NewNetworkPort | ShellAction::NewRoute => {
                return Task::none();
            }
            // Settings is where the report says where it was saved.
            ShellAction::ExportDiagnostics => {
                self.go(Page::Settings);
                return Task::done(cosmic::Action::App(Message::ExportDiagnostics));
            }
            ShellAction::Zoom => {
                return self
                    .core
                    .main_window_id()
                    .map_or_else(Task::none, window::toggle_maximize);
            }
            // Outside App Store mode, closing the window ends it, as its own close button does.
            ShellAction::CloseWindow if self.store.is_none() => {
                return self
                    .core
                    .main_window_id()
                    .map_or_else(Task::none, window::close);
            }
            ShellAction::Open | ShellAction::CloseWindow | ShellAction::Quit => {}
        }

        let (Some(store), Some(id)) = (&mut self.store, self.core.main_window_id()) else {
            return Task::none();
        };
        match action {
            ShellAction::Open => {
                store.visible = true;
                set_dock_visible(true);
                window::set_mode(id, window::Mode::Windowed).chain(window::gain_focus(id))
            }
            ShellAction::CloseWindow => {
                store.visible = false;
                set_dock_visible(false);
                window::set_mode(id, window::Mode::Hidden)
            }
            ShellAction::Quit => {
                if store.quitting {
                    return Task::none();
                }
                store.quitting = true;
                store.remember_at_quit();
                let owner = std::sync::Arc::clone(&store.owner);
                cosmic::task::future(async move {
                    crate::app_store::stop_daemon(owner).await;
                    Message::DaemonStopped
                })
            }
            // The menus' own actions were carried out above.
            _ => Task::none(),
        }
    }

    /// Turns Start at login on or off, and shows what macOS reports afterwards.
    fn set_start_at_login(&mut self, on: bool) {
        let Some(store) = &mut self.store else {
            return;
        };
        match midi_harbor_platform::appkit::login_item::set(on) {
            Ok(state) => {
                store.login = state;
                store.login_error = None;
            }
            Err(error) => {
                store.login = midi_harbor_platform::appkit::login_item::status();
                store.login_error = Some(error);
            }
        }
    }
}

/// Streams what the user chooses through the menu bar item and the app's menus.
#[cfg(target_os = "macos")]
fn shell_actions() -> impl cosmic::iced::futures::Stream<Item = Message> {
    let actions = midi_harbor_platform::appkit::take_actions();
    cosmic::iced::futures::stream::unfold(actions, |actions| async move {
        let mut actions = actions?;
        let action = actions.recv().await?;
        Some((Message::Shell(action), Some(actions)))
    })
}

/// Writes a diagnostic report into the user's Downloads folder, returning where.
///
/// Downloads rather than a save dialog, because this libcosmic build has no file chooser on
/// macOS; the settings page says where the file went.
fn save_report(report: &str) -> Result<String, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "could not find your home folder to save the report in".to_owned())?;
    let downloads = home.join("Downloads");
    let folder = if downloads.is_dir() { downloads } else { home };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let path = folder.join(format!("midi-harbor-diagnostics-{stamp}.json"));
    std::fs::write(&path, report)
        .map_err(|error| format!("could not save the report to {}: {error}", path.display()))?;
    Ok(path.display().to_string())
}

/// Follows the daemon's state stream, for keeping the cached view current between resyncs.
///
/// Ends with the reason whenever the stream does, including when the daemon drops a window that
/// fell behind, so the window knows to read everything again.
fn state_stream(
    key: &(Option<PathBuf>, u64),
) -> cosmic::iced::futures::stream::BoxStream<'static, Message> {
    use cosmic::iced::futures::StreamExt;

    enum Step {
        Start(Option<PathBuf>),
        // Boxed, because a stream is many times the size of the other steps.
        Streaming(Box<tonic::Streaming<StateEvent>>),
        Done,
    }

    let socket = key.0.clone();
    cosmic::iced::futures::stream::unfold(Step::Start(socket), |step| async move {
        let mut stream = match step {
            Step::Done => return None,
            Step::Streaming(stream) => stream,
            Step::Start(socket) => {
                let opened = match Client::connect(socket).await {
                    Ok(client) => client.watch_state().await,
                    Err(error) => Err(error),
                };
                match opened {
                    Ok(stream) => Box::new(stream),
                    Err(error) => return Some((Message::StateStreamEnded(error), Step::Done)),
                }
            }
        };
        match stream.message().await {
            Ok(Some(event)) => Some((
                Message::StateChanged(Box::new(event)),
                Step::Streaming(stream),
            )),
            Ok(None) => Some((
                Message::StateStreamEnded("the daemon closed the stream".to_owned()),
                Step::Done,
            )),
            Err(status) => Some((
                Message::StateStreamEnded(status.message().to_owned()),
                Step::Done,
            )),
        }
    })
    .boxed()
}

/// Streams what passes through one endpoint, for the monitor page.
///
/// A plain function over its key, as the subscription requires, so the stream is started when
/// the key appears and stopped when it changes or goes, and never restarted while it stays.
fn monitor_stream(
    key: &(Option<PathBuf>, String),
) -> cosmic::iced::futures::stream::BoxStream<'static, Message> {
    use cosmic::iced::futures::StreamExt;

    enum Step {
        Start(Option<PathBuf>, String),
        Streaming(tonic::Streaming<MonitoredMessage>),
        Done,
    }

    let (socket, id) = key.clone();
    cosmic::iced::futures::stream::unfold(Step::Start(socket, id), |step| async move {
        let mut stream = match step {
            Step::Done => return None,
            Step::Streaming(stream) => stream,
            Step::Start(socket, id) => {
                let opened = match Client::connect(socket).await {
                    Ok(client) => client.monitor(id).await,
                    Err(error) => Err(error),
                };
                match opened {
                    Ok(stream) => stream,
                    Err(error) => return Some((Message::MonitorEnded(error), Step::Done)),
                }
            }
        };
        match stream.message().await {
            Ok(Some(seen)) => Some((Message::Monitored(Box::new(seen)), Step::Streaming(stream))),
            Ok(None) => Some((
                Message::MonitorEnded("the daemon closed the stream".to_owned()),
                Step::Done,
            )),
            Err(status) => Some((
                Message::MonitorEnded(status.message().to_owned()),
                Step::Done,
            )),
        }
    })
    .boxed()
}

/// Returns the current wall-clock time in seconds, for turning event timestamps into ages.
///
/// A clock that has moved behind the epoch is reported as the epoch rather than failing, since an
/// activity log with slightly wrong ages is better than one that will not draw.
pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_secs().try_into().unwrap_or(i64::MAX)
        })
}
