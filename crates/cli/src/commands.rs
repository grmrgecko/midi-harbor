//! The command tree.

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Midi Harbor's command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "midi-harbor",
    version = midi_harbor_core::VERSION,
    about = "Resilient MIDI connectivity for macOS, Linux and Windows",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Emit machine-readable output on stdout.
    #[arg(long, global = true)]
    pub json: bool,

    /// Use a specific daemon socket instead of the standard location.
    #[arg(long, global = true, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Raise log verbosity. Repeat for more.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress output that is not an error.
    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// The action to perform. Omitted, the graphical interface starts.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// What the user asked for.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the engine in the foreground, registering nothing with the system.
    Daemon {
        /// Write the log to this file instead of standard error, rolling it over as it grows.
        #[arg(long, value_name = "PATH")]
        log_file: Option<std::path::PathBuf>,

        /// Run the engine as a child and start it again whenever it fails.
        ///
        /// For service managers that do not restart a failed program themselves, which on the
        /// supported platforms means Task Scheduler.
        #[arg(long, hide = true)]
        supervise: bool,
    },

    /// Open the graphical interface.
    Gui,

    /// Install and control the background service.
    #[command(subcommand)]
    Service(ServiceCommand),

    /// Manage virtual MIDI ports.
    #[command(subcommand)]
    Port(PortCommand),

    /// Manage network ports: MIDI over the network, which Apple calls network sessions.
    ///
    /// `session` is accepted as well, the name these commands had before.
    #[command(subcommand, alias = "session")]
    Network(NetworkCommand),

    /// Manage MIDI connections between endpoints.
    #[command(subcommand)]
    Route(RouteCommand),

    /// Inspect attached MIDI hardware.
    #[command(subcommand)]
    Device(DeviceCommand),

    /// Connect Bluetooth MIDI devices, and offer this machine as one.
    #[command(subcommand)]
    Bluetooth(BluetoothCommand),

    /// Show endpoints and their connection health.
    Status {
        /// Redraw as state changes.
        #[arg(long)]
        watch: bool,
    },

    /// Show what has happened to connections.
    Events {
        /// Show only events newer than this identifier.
        #[arg(long)]
        since: Option<u64>,
        /// Limit how many are shown.
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Keep watching for new events instead of exiting.
        #[arg(long)]
        follow: bool,
    },

    /// Watch the MIDI passing through an endpoint.
    Monitor {
        /// The endpoint to watch, by name or identifier.
        endpoint: String,
        /// Show raw bytes as well as the decoded message.
        #[arg(long)]
        raw: bool,
    },

    /// Clear the warning that the MIDI service stopped, for every window and `status`, once it
    /// has been seen.
    DismissWarning,

    /// Send one note out of an endpoint, to test what listens there.
    ///
    /// The daemon sends the note-off itself once the note has sounded for its length.
    SendNote {
        /// The endpoint to send out of, by name or identifier.
        endpoint: String,
        /// The note, 0 to 127; 60 is middle C.
        #[arg(long, default_value_t = 60)]
        note: u32,
        /// The channel, 1 to 16.
        #[arg(long, default_value_t = 1)]
        channel: u32,
        /// The velocity, 1 to 127.
        #[arg(long, default_value_t = 100)]
        velocity: u32,
        /// How long the note sounds, in milliseconds, up to 10000.
        #[arg(long, default_value_t = 500)]
        length: u32,
    },

    /// Write a diagnostic report for a bug report.
    #[command(subcommand)]
    Diagnostics(DiagnosticsCommand),

    /// Show which capabilities are available on this machine.
    Capabilities,

    /// Inspect the configuration file.
    #[command(subcommand)]
    Config(ConfigCommand),
}

/// Service installation and control.
#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Register the daemon to start at login.
    Install {
        /// Start it immediately as well.
        #[arg(long)]
        start: bool,
    },
    /// Deregister the daemon, leaving configuration untouched.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the installed service.
    Stop,
    /// Report whether it is installed, running, and whether its registration is stale.
    Status,
}

/// Virtual port management.
#[derive(Debug, Subcommand)]
pub enum PortCommand {
    /// List virtual ports.
    List,
    /// Create a virtual port other applications can see.
    ///
    /// It has MIDI In connectors, which other applications send to, and MIDI Out connectors,
    /// which they receive from, one of each unless more are asked for. Several of one kind show
    /// to applications as numbered ports, "Keys 1" and "Keys 2".
    Create {
        /// The name other applications will show.
        name: String,
        /// MIDI In connectors, one to sixteen.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=16))]
        inputs: u8,
        /// MIDI Out connectors, one to sixteen.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=16))]
        outputs: u8,
        /// No longer used: every virtual port has a MIDI In and a MIDI Out. Accepted so
        /// scripts written before connectors keep running.
        #[arg(long, value_enum, hide = true)]
        direction: Option<DirectionArg>,
    },
    /// Change how many MIDI In and MIDI Out connectors a port has.
    ///
    /// Routes on connectors it no longer has are removed, and the port is reopened, so
    /// applications using it may need to choose it again.
    Connectors {
        /// The port to change, by name or identifier.
        target: String,
        /// MIDI In connectors, one to sixteen.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=16))]
        inputs: u8,
        /// MIDI Out connectors, one to sixteen.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=16))]
        outputs: u8,
    },
    /// Rename a port, rewriting the routes that name it.
    Rename {
        /// The port to rename, by name or identifier.
        target: String,
        /// The new name.
        new_name: String,
        /// Confirm that connected applications may need to reselect the port.
        #[arg(long)]
        yes: bool,
    },
    /// Delete a port, silencing it first.
    Delete {
        /// The port to delete, by name or identifier.
        target: String,
        /// Confirm the deletion. Without it, nothing is deleted.
        #[arg(long)]
        yes: bool,
    },
    /// Switch a port on.
    Enable {
        /// The port to enable, by name or identifier.
        target: String,
    },
    /// Switch a port off without deleting its configuration.
    Disable {
        /// The port to disable, by name or identifier.
        target: String,
    },
}

/// Connections between endpoints.
#[derive(Debug, Subcommand)]
pub enum RouteCommand {
    /// List connections.
    List {
        /// Show only connections whose endpoints are missing.
        #[arg(long)]
        broken: bool,
    },
    /// Connect one endpoint to another.
    Create {
        /// Where MIDI comes from, by name.
        from: String,
        /// Where MIDI goes, by name.
        to: String,
        /// Which of the source's MIDI In connectors, for a virtual port with several.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=16))]
        from_connector: u8,
        /// Which of the destination's MIDI Out connectors, for a virtual port with several.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=16))]
        to_connector: u8,
        /// Carry MIDI back as well, from TO to FROM, as one route.
        #[arg(long)]
        both_ways: bool,
    },
    /// Change a route's ends, connectors, or whether it carries MIDI both ways.
    ///
    /// Only what is given changes. Its identifier changes when its ends do.
    Edit {
        /// The route's identifier, from `route list`.
        id: String,
        /// Where MIDI comes from, by name.
        #[arg(long)]
        from: Option<String>,
        /// Where MIDI goes, by name.
        #[arg(long)]
        to: Option<String>,
        /// Which of the source's MIDI In connectors.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=16))]
        from_connector: Option<u8>,
        /// Which of the destination's MIDI Out connectors.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=16))]
        to_connector: Option<u8>,
        /// Carry MIDI back as well.
        #[arg(long, conflicts_with = "one_way")]
        both_ways: bool,
        /// Carry MIDI one way only.
        #[arg(long)]
        one_way: bool,
    },
    /// Remove a connection.
    Delete {
        /// The connection's identifier, from `route list`.
        id: String,
    },
    /// Switch a connection on.
    Enable {
        /// The connection's identifier.
        id: String,
    },
    /// Switch a connection off without removing it.
    Disable {
        /// The connection's identifier.
        id: String,
    },
}

/// Attached MIDI hardware.
#[derive(Debug, Subcommand)]
pub enum DeviceCommand {
    /// List attached hardware, and the ports the system or other applications provide.
    List {
        /// Include hardware that is remembered but not plugged in.
        #[arg(long)]
        all: bool,
    },
    /// Say which of two identical devices a remembered entry means.
    ///
    /// Hardware that is identical in every way but where it is plugged in cannot be told apart,
    /// so the entry is left unbound rather than guessing. Naming the one that was meant binds it.
    Resolve {
        /// The remembered entry, by name or identifier.
        device: String,
        /// The attached hardware to bind it to, by name or identifier. Omitted, the candidates
        /// are listed.
        chosen: Option<String>,
    },
    /// Forget remembered hardware and everything configured about it.
    ///
    /// Hardware that is still attached comes straight back as something never seen before, which
    /// is how to start over with a device whose settings have gone wrong.
    Forget {
        /// The device, by name or identifier.
        device: String,
    },
    /// Switch a device on.
    Enable {
        /// The device to enable, by name or identifier.
        target: String,
    },
    /// Switch a device off without forgetting it.
    Disable {
        /// The device to disable, by name or identifier.
        target: String,
    },
}

/// Bluetooth LE MIDI, in both directions.
#[derive(Debug, Subcommand)]
pub enum BluetoothCommand {
    /// Look for devices in range, and list what was found.
    ///
    /// Scanning costs power on every device in range, so it stops on its own.
    Scan {
        /// How long to look, in seconds.
        #[arg(long, default_value_t = 10)]
        seconds: u32,
    },
    /// List what the radio can currently hear, without starting a new scan.
    List,
    /// Connect a device, and remember it so it reconnects by itself next time.
    Connect {
        /// The device, by the address shown in a scan.
        address: String,
    },
    /// Close a link without forgetting the device.
    Disconnect {
        /// The device, by name or identifier.
        device: String,
    },
    /// Forget a device, so it stops reconnecting when it comes back into range.
    Forget {
        /// The device, by name or identifier.
        device: String,
    },
    /// Offer this machine to phones and tablets as a Bluetooth MIDI device.
    Advertise {
        /// Stop advertising instead of starting.
        #[arg(long)]
        off: bool,
        /// The name other devices will see. Defaults to this machine's name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Switch a Bluetooth device on.
    Enable {
        /// The device to enable, by name or identifier.
        target: String,
    },
    /// Switch a Bluetooth device off without forgetting it.
    Disable {
        /// The device to disable, by name or identifier.
        target: String,
    },
}

/// Network port management.
#[derive(Debug, Subcommand)]
pub enum NetworkCommand {
    /// List configured network ports.
    List,
    /// Create a network port other machines can connect to.
    Create {
        /// The name to show in listings.
        name: String,
        /// The UDP port to listen on. Omitted, the system chooses.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// How to treat invitations from other machines. Omitted, the configuration's default
        /// is used, which is to prompt unless changed.
        #[arg(long, value_enum)]
        policy: Option<PolicyArg>,
        /// Do not show it to other applications on this computer as a MIDI port of its name.
        #[arg(long)]
        no_automatic_port: bool,
        /// The name other machines see it by. Omitted, they see NAME.
        #[arg(long)]
        bonjour_name: Option<String>,
    },
    /// Change a network port's settings. What is not given stays as it is.
    Edit {
        /// The network port to change, by name or identifier.
        #[arg(value_name = "NETWORK_PORT")]
        session: String,
        /// The name other machines see it by. Machines already connected keep the old one.
        #[arg(long)]
        bonjour_name: Option<String>,
        /// The UDP port to listen on, even, or 0 to let the system choose. The network port
        /// restarts on it, and machines that connected to it need the new port.
        #[arg(long)]
        udp_port: Option<u16>,
        /// How to treat invitations from other machines.
        #[arg(long, value_enum)]
        policy: Option<PolicyArg>,
        /// Whether other applications on this computer see it as a MIDI port of its name.
        #[arg(long, value_enum)]
        automatic_port: Option<Switch>,
    },
    /// Show the machines advertising themselves on this network.
    Discover,
    /// Connect a network port to a machine, by discovered name or by address.
    ///
    /// An address may be given as HOST or HOST:PORT. A bare host uses the standard RTP-MIDI
    /// port, 5004. Connecting by address needs no discovery at all, which matters on a network
    /// that filters multicast.
    Connect {
        /// The network port to connect, by name or identifier.
        #[arg(value_name = "NETWORK_PORT")]
        session: String,
        /// The peer: a discovered name, or an address such as 192.0.2.3 or 192.0.2.3:5004.
        #[arg(value_name = "PEER_OR_ADDRESS")]
        peer: String,
        /// Carry this machine beside those already connected, instead of in place of the peer.
        /// It is remembered and reconnected as the peer is, until it is disconnected.
        #[arg(long)]
        alongside: bool,
    },
    /// Disconnect a network port, leaving it listening, or one machine from it.
    Disconnect {
        /// The network port to disconnect, by name or identifier.
        #[arg(value_name = "NETWORK_PORT")]
        session: String,
        /// Disconnect only this machine, by the address `network machines` shows.
        #[arg(long)]
        machine: Option<String>,
    },
    /// Delete a network port, stopping what it carried first.
    Delete {
        /// The network port, by name or identifier.
        #[arg(value_name = "NETWORK_PORT")]
        session: String,
        /// Confirm the deletion. Without it, nothing is deleted.
        #[arg(long)]
        yes: bool,
    },
    /// Show the machines taking part in a network port.
    Machines {
        /// The network port, by name or identifier.
        #[arg(value_name = "NETWORK_PORT")]
        session: String,
    },
    /// Show the machines waiting to be let in.
    Invitations,
    /// Answer a machine that asked to connect.
    Respond {
        /// The invitation, as shown by `network invitations`.
        invitation: String,
        /// Let the machine in.
        #[arg(long, conflicts_with = "refuse")]
        accept: bool,
        /// Turn the machine away.
        #[arg(long)]
        refuse: bool,
        /// Remember the machine, so it is never asked about again.
        #[arg(long)]
        always: bool,
    },
    /// Manage the machines this one knows about.
    #[command(subcommand)]
    Peer(PeerCommand),
    /// Change how a network port treats invitations from other machines.
    Policy {
        /// The network port to change, by name or identifier.
        #[arg(value_name = "NETWORK_PORT")]
        session: String,
        /// How to treat invitations.
        #[arg(value_enum)]
        policy: PolicyArg,
    },
    /// Switch a network port on.
    Enable {
        /// The network port to enable, by name or identifier.
        target: String,
    },
    /// Switch a network port off without forgetting it.
    Disable {
        /// The network port to disable, by name or identifier.
        target: String,
    },
}

/// Managing the machines this one knows about.
#[derive(Debug, Subcommand)]
pub enum PeerCommand {
    /// Remember a machine by address, so it is let in without being asked about.
    Add {
        /// The address, as HOST or HOST:PORT. A bare host uses the standard port, 5004.
        address: String,
        /// What to call it. Omitted, the address is used.
        #[arg(long)]
        name: Option<String>,
        /// Remember it without letting it in unasked; its invitations are still asked about.
        #[arg(long)]
        no_trust: bool,
    },
    /// Switch whether a known machine is let in without being asked about.
    Trust {
        /// The machine, by name, address or identifier.
        peer: String,
        /// On lets it in without asking; off asks about its invitations again.
        #[arg(value_enum)]
        state: Switch,
    },
    /// Forget a machine, so invitations from it are asked about again.
    Remove {
        /// The machine, by name, address or identifier.
        peer: String,
    },
}

/// How a network port treats invitations from other machines.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PolicyArg {
    /// Ask before accepting.
    Prompt,
    /// Accept without asking from machines already trusted.
    Known,
    /// Accept from anyone.
    All,
    /// Refuse everything.
    Reject,
}

impl PolicyArg {
    /// Returns the wire value for this policy.
    pub fn to_proto(self) -> i32 {
        let mapped = match self {
            Self::Prompt => midi_harbor_ipc::pb::InvitationPolicy::Prompt,
            Self::Known => midi_harbor_ipc::pb::InvitationPolicy::AcceptKnown,
            Self::All => midi_harbor_ipc::pb::InvitationPolicy::AcceptAll,
            Self::Reject => midi_harbor_ipc::pb::InvitationPolicy::RejectAll,
        };
        mapped as i32
    }
}

/// A setting switched on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Switch {
    /// Switched on.
    On,
    /// Switched off.
    Off,
}

/// Diagnostic reporting.
#[derive(Debug, Subcommand)]
pub enum DiagnosticsCommand {
    /// Write configuration, connection history and counters to a file.
    Export {
        /// Where to write the report. Omitted, it goes to standard output.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

/// Configuration inspection, transfer and reloading.
#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCommand {
    /// Print where the configuration file lives.
    Path,
    /// Print the configuration file.
    Show,
    /// Write the running configuration, ready to import on this machine or another.
    Export {
        /// Where to write it. Omitted, it goes to standard output.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Apply a configuration exported from this machine or another.
    Import {
        /// The exported file.
        path: PathBuf,
        /// Merge adds what this machine lacks; replace makes this machine match the file.
        #[arg(long, value_enum, default_value_t = ImportMode::Merge)]
        mode: ImportMode,
        /// Confirm a replacing import, which removes what the file does not mention.
        #[arg(long)]
        yes: bool,
    },
    /// Apply changes made to the configuration file by hand, disturbing only what changed.
    Reload,
    /// Take over the IAC buses and network sessions set up in Audio MIDI Setup, as virtual
    /// ports and network ports.
    ///
    /// Apple's setup is read, never changed. Without --yes, this only shows what it would
    /// create.
    ImportApple {
        /// Create what the preview lists.
        #[arg(long)]
        yes: bool,
    },
}

/// How an imported configuration combines with the one already here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ImportMode {
    /// Add endpoints, routes and peers this machine lacks, and change nothing it has.
    Merge,
    /// Make this machine's setup match the file, removing anything the file does not mention.
    Replace,
}

/// Which directions a port exposes.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DirectionArg {
    /// MIDI arrives from it only.
    In,
    /// MIDI leaves through it only.
    Out,
    /// Both.
    Both,
}

impl DirectionArg {
    /// Returns the wire value for this direction.
    pub fn to_proto(self) -> i32 {
        let mapped = match self {
            Self::In => midi_harbor_ipc::pb::Direction::Input,
            Self::Out => midi_harbor_ipc::pb::Direction::Output,
            Self::Both => midi_harbor_ipc::pb::Direction::Bidirectional,
        };
        mapped as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Locks that the command tree passes clap's own consistency checks.
    ///
    /// Clap only reports a clash between two definitions, such as a repeated short flag or a
    /// conflict naming an argument that does not exist, when the affected command is parsed. This
    /// walks every subcommand, so a clash in a rarely used one fails here rather than for a user.
    #[test]
    fn the_command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// Locks that `port create --direction` still parses after connectors replaced it.
    ///
    /// The flag no longer does anything, but scripts written before connectors pass it, and
    /// rejecting it would break them for no gain.
    #[test]
    fn a_script_from_before_connectors_still_parses() {
        let parsed =
            Cli::try_parse_from(["midi-harbor", "port", "create", "Keys", "--direction", "in"]);
        assert!(
            parsed.is_ok(),
            "a script passing --direction must keep working: {parsed:?}"
        );
    }
}
