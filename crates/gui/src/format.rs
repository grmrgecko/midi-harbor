//! Turning contract messages into what a person reads.
//!
//! Everything here is a pure function over generated types, so the wording a user sees is tested
//! without opening a window or reaching a daemon. The views below contain no formatting decisions
//! of their own; they arrange what this module returns.

use midi_harbor_ipc::pb::{
    Capability, ConnectionPhase, ConnectionState, Direction, DiscoveredBluetoothDevice, Endpoint,
    EndpointKind, FailureReason, InvitationPolicy, Peer, Route, RouteValidity, Severity,
    TrafficCounters, endpoint::Detail,
};
use prost_types::Timestamp;

/// How much attention a piece of state deserves, decided once and honoured by every view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Working as intended.
    Good,
    /// In motion, and expected to settle on its own.
    Busy,
    /// Down, but recovering without the user.
    Waiting,
    /// Down for something only the user can clear.
    Bad,
    /// Switched off deliberately, which is not a fault.
    Off,
}

/// Returns the word for a connection phase and how it should read.
///
/// The phrasing distinguishes the two kinds of down: `retrying` recovers on its own, while
/// `needs attention` does not, and a user deciding whether to intervene reads only this.
pub fn phase_label(phase: ConnectionPhase) -> (&'static str, Tone) {
    match phase {
        ConnectionPhase::Connected => ("connected", Tone::Good),
        ConnectionPhase::Connecting => ("connecting", Tone::Busy),
        ConnectionPhase::Retrying => ("retrying", Tone::Waiting),
        ConnectionPhase::Unavailable => ("needs attention", Tone::Bad),
        ConnectionPhase::Disconnected => ("disconnected", Tone::Waiting),
        ConnectionPhase::Disabled => ("off", Tone::Off),
        ConnectionPhase::Unspecified => ("unknown", Tone::Waiting),
    }
}

/// Describes an endpoint's state in the terms its own kind makes sense of.
///
/// Attached hardware has no connection lifecycle: a device is plugged in or it is not, and
/// reporting it as "connected" invites a user to look for a connection that was never made. The
/// wording matches the command line for the same endpoint, so the two never disagree about what
/// one device is doing.
pub fn endpoint_status(endpoint: &Endpoint) -> (&'static str, Tone) {
    if let Some(Detail::PhysicalDevice(device)) = &endpoint.detail {
        return if device.present {
            ("attached", Tone::Good)
        } else {
            ("absent", Tone::Waiting)
        };
    }
    let phase = endpoint
        .state
        .as_ref()
        .map_or(ConnectionPhase::Unspecified, |state| state.phase());
    if is_listening(endpoint) {
        return ("listening", Tone::Off);
    }
    if is_advertising(endpoint) {
        return ("advertising", Tone::Off);
    }
    phase_label(phase)
}

/// Reports whether an endpoint is a session waiting for another machine to invite it.
///
/// That is a session's resting state, not a fault. Showing it as "disconnected" in a warning
/// colour, and counting it against the connected total, sent users looking for a problem.
pub fn is_listening(endpoint: &Endpoint) -> bool {
    matches!(endpoint.detail, Some(Detail::NetworkSession(_)))
        && endpoint.enabled
        && endpoint
            .state
            .as_ref()
            .is_some_and(|state| state.phase() == ConnectionPhase::Disconnected)
}

/// Reports whether an endpoint is this machine's advertised Bluetooth port, waiting for a device
/// to connect.
///
/// Like a listening session, a resting state and not a fault.
pub fn is_advertising(endpoint: &Endpoint) -> bool {
    matches!(&endpoint.detail, Some(Detail::BluetoothDevice(device)) if device.peripheral_role)
        && endpoint.enabled
        && endpoint
            .state
            .as_ref()
            .is_some_and(|state| state.phase() == ConnectionPhase::Disconnected)
}

/// Renders a span in milliseconds as the coarsest unit that still says something.
fn span(millis: u64) -> String {
    const SECOND: u64 = 1_000;
    const MINUTE: u64 = 60 * SECOND;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    // Truncation towards zero is the intent: a span is rounded down to the unit being shown.
    #[allow(clippy::integer_division)]
    if millis < MINUTE {
        format!("{}s", millis / SECOND)
    } else if millis < HOUR {
        format!("{}m", millis / MINUTE)
    } else if millis < DAY {
        format!("{}h", millis / HOUR)
    } else {
        format!("{}d", millis / DAY)
    }
}

/// Says when a retrying endpoint tries again, or nothing when it has no attempt scheduled.
pub fn next_attempt(state: &ConnectionState, now_seconds: i64) -> Option<String> {
    let at = state.next_retry.as_ref()?;
    let waiting = at.seconds.saturating_sub(now_seconds);
    Some(if waiting <= 0 {
        "trying again now".to_owned()
    } else {
        format!(
            "next in {}",
            span(u64::try_from(waiting).unwrap_or(0).saturating_mul(1_000))
        )
    })
}

/// Describes what is happening beneath a phase, when there is more to say than the phase itself.
///
/// A reconnecting endpoint reports which attempt it is on, because a count that keeps climbing is
/// how a user tells a slow recovery from a stuck one.
pub fn state_detail(state: &ConnectionState, now_seconds: i64) -> Option<String> {
    // Said first, because at any one moment a flapping link reads as connected or retrying, and
    // neither explains why it keeps going quiet.
    if state.waiting_for_network {
        return Some("waiting for a network; this computer has no route to the peer".to_owned());
    }
    if state.unstable {
        return Some("unstable: keeps dropping within seconds of connecting".to_owned());
    }
    let phase = state.phase();
    let retrying = matches!(
        phase,
        ConnectionPhase::Retrying | ConnectionPhase::Unavailable | ConnectionPhase::Connecting
    );
    if retrying && state.attempt > 0 {
        // When it tries again is what tells a user whether to wait or to go and fix something.
        let mut detail = format!("attempt {}", state.attempt);
        if let Some(next) = next_attempt(state, now_seconds) {
            detail.push_str(&format!(", {next}"));
        }
        if let Some(error) = &state.last_error
            && !error.message.is_empty()
        {
            detail.push_str(&format!(", last error: {}", error.message));
        }
        return Some(detail);
    }

    // A cleared failure is still worth showing, so a user who was away learns what happened.
    match &state.last_error {
        Some(error) if !error.message.is_empty() && phase != ConnectionPhase::Connected => {
            Some(error.message.clone())
        }
        _ => None,
    }
}

/// Returns the guidance for a failure the user has to act on, and nothing for one they do not.
///
/// Advice shown against a fault that clears itself teaches a user to ignore advice, so the
/// contract's own `needs_user_action` decides whether it appears.
pub fn guidance(reason: &FailureReason) -> Option<&str> {
    if reason.needs_user_action && !reason.guidance.is_empty() {
        Some(reason.guidance.as_str())
    } else {
        None
    }
}

/// Reports whether an endpoint with this direction may start a route, as the daemon decides it.
pub fn can_source(direction: Direction) -> bool {
    matches!(direction, Direction::Input | Direction::Bidirectional)
}

/// Reports whether an endpoint with this direction may end a route, as the daemon decides it.
pub fn can_sink(direction: Direction) -> bool {
    matches!(direction, Direction::Output | Direction::Bidirectional)
}

/// Finds a capability by its stable identifier, such as `bluetooth_central`.
pub fn capability<'a>(capabilities: &'a [Capability], id: &str) -> Option<&'a Capability> {
    capabilities.iter().find(|capability| capability.id == id)
}

/// Explains why a capability cannot be used here, or says nothing when it can (FR-053).
///
/// Unknown counts as unavailable. A daemon that did not answer for a capability has not said it
/// works, and a control that fails when pressed is the broken look this exists to avoid.
pub fn unavailable_because(capabilities: &[Capability], id: &str) -> Option<String> {
    match capability(capabilities, id) {
        Some(capability) if capability.available => None,
        Some(capability) if !capability.reason.is_empty() => {
            Some(format!("Unavailable: {}", capability.reason))
        }
        _ => Some("Unavailable on this computer".to_owned()),
    }
}

/// Describes a nearby Bluetooth device: its name, how strongly it is heard, and whether it is
/// already one of ours.
pub fn nearby_label(device: &DiscoveredBluetoothDevice) -> (String, String) {
    let name = device
        .name
        .clone()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unnamed device".to_owned());
    let mut detail = Vec::new();
    if let Some(rssi) = device.rssi {
        detail.push(format!("signal {rssi} dBm"));
    }
    if device.endpoint_id.is_some() {
        detail.push("remembered".to_owned());
    }
    (name, detail.join(" · "))
}

/// Summarises message counts in the order a user asks about them.
///
/// Received comes first because the usual question is whether anything is arriving at all.
pub fn traffic_summary(counters: &TrafficCounters) -> String {
    format!(
        "{} in · {} out",
        compact(counters.messages_received),
        compact(counters.messages_sent)
    )
}

/// Explains why a route is not carrying anything, and returns nothing when it is fine.
pub fn route_problem(route: &Route) -> Option<String> {
    match route.validity() {
        RouteValidity::Valid => None,
        RouteValidity::Broken => Some(match route.missing.split_first() {
            Some((only, [])) => format!("'{only}' is not here; the route resumes when it returns"),
            Some((first, rest)) => {
                let names = std::iter::once(first)
                    .chain(rest)
                    .map(|name| format!("'{name}'"))
                    .collect::<Vec<_>>()
                    .join(" and ");
                format!("{names} are not here; the route resumes when they return")
            }
            None => "an endpoint is missing".to_owned(),
        }),
        RouteValidity::LoopDetected => {
            // Delivery is direct, so a cycle carries each message once and multiplies nothing.
            Some(
                "part of a chain of routes leading back to where it started; it still carries \
                 MIDI, but this is usually a mistake"
                    .to_owned(),
            )
        }
        RouteValidity::Suspended => Some(match route.waiting_on.split_first() {
            Some((only, [])) => {
                format!("'{only}' is not running; the route resumes when it is")
            }
            Some((first, rest)) => {
                let names = std::iter::once(first)
                    .chain(rest)
                    .map(|name| format!("'{name}'"))
                    .collect::<Vec<_>>()
                    .join(" and ");
                format!("{names} are not running; the route resumes when they are")
            }
            None => "an endpoint is not running".to_owned(),
        }),
        RouteValidity::Unspecified => None,
    }
}

/// Summarises what a route has carried, in the one direction a route has.
///
/// Deliberately not `traffic_summary`: an endpoint has two sides and a route has one, so "in"
/// and "out" would be the same number twice. A live route that has carried nothing says so,
/// because "the route is fine and nothing is playing" and "the route is not working" are the two
/// answers someone is looking at this row to tell apart.
pub fn route_traffic(route: &Route) -> Option<String> {
    // A route that is switched off or broken is already explained elsewhere on the row.
    if !route.enabled
        || matches!(
            route.validity(),
            RouteValidity::Broken | RouteValidity::Suspended
        )
    {
        return None;
    }
    let counters = route.counters.as_ref()?;
    if counters.messages_sent == 0 && counters.messages_dropped == 0 {
        return Some("nothing carried yet".to_owned());
    }

    // Compacted rather than spelled out, so a busy route does not widen the row, with one
    // singular case because "1 messages" is the kind of thing that makes a display look wrong.
    let mut summary = if counters.messages_sent == 1 {
        "1 message carried".to_owned()
    } else {
        format!("{} messages carried", compact(counters.messages_sent))
    };
    if counters.messages_dropped > 0 {
        summary.push_str(&format!(
            " · {} undelivered",
            compact(counters.messages_dropped)
        ));
    }
    Some(summary)
}

/// Returns how a route should read in a list.
pub fn route_tone(route: &Route) -> Tone {
    if !route.enabled {
        return Tone::Off;
    }
    match route.validity() {
        RouteValidity::Valid => Tone::Good,
        RouteValidity::Broken => Tone::Waiting,
        RouteValidity::LoopDetected => Tone::Bad,
        RouteValidity::Suspended => Tone::Waiting,
        RouteValidity::Unspecified => Tone::Waiting,
    }
}

/// Returns how an event's severity should read.
pub fn severity_tone(severity: Severity) -> Tone {
    match severity {
        Severity::Error => Tone::Bad,
        Severity::Warning => Tone::Waiting,
        Severity::Info | Severity::Unspecified => Tone::Good,
    }
}

/// Describes one machine in a network port: who connected to whom and where, then its latency
/// on a line of its own.
pub fn machine_detail(machine: &midi_harbor_ipc::pb::NetworkMachine) -> String {
    let how = if machine.invited {
        "Connected to"
    } else {
        "Connected from"
    };
    let first = if machine.joined {
        format!("{how} · {}", machine.address)
    } else {
        format!("Joining · {}", machine.address)
    };
    match machine.round_trip_us {
        Some(micros) => format!("{first}\nLatency {:.1} ms", micros as f64 / 1_000.0),
        None => first,
    }
}

/// Renders an age in milliseconds as the coarsest unit that still says something.
pub fn duration(millis: u64) -> String {
    const SECOND: u64 = 1_000;
    const MINUTE: u64 = 60 * SECOND;
    const HOUR: u64 = 60 * MINUTE;

    // Truncation towards zero is the intent: an age is rounded down to the unit being shown.
    #[allow(clippy::integer_division)]
    if millis < SECOND {
        "just now".to_owned()
    } else if millis < MINUTE {
        format!("{}s ago", millis / SECOND)
    } else if millis < HOUR {
        format!("{}m ago", millis / MINUTE)
    } else {
        format!("{}h ago", millis / HOUR)
    }
}

/// Describes how long ago a contract timestamp was, relative to a supplied "now".
///
/// The reference time is passed in rather than read from the clock, so the wording is tested
/// without waiting for real time to pass.
pub fn age(at: Option<&Timestamp>, now_seconds: i64) -> Option<String> {
    let at = at?;
    // A timestamp from the future means the clocks disagree, which is not worth a negative age.
    let elapsed = now_seconds.saturating_sub(at.seconds).max(0);
    Some(duration((elapsed as u64).saturating_mul(1_000)))
}

/// Renders a contract timestamp as a clock time in `zone`, with its date only when that is not
/// the day of `now`.
pub fn clock(at: &Timestamp, zone: &jiff::tz::TimeZone, now: jiff::Timestamp) -> String {
    let Ok(at) = jiff::Timestamp::new(at.seconds, at.nanos) else {
        return "an unknown time".to_owned();
    };
    let at = at.to_zoned(zone.clone());
    if at.date() == now.to_zoned(zone.clone()).date() {
        at.strftime("%H:%M:%S").to_string()
    } else {
        at.strftime("%Y-%m-%d %H:%M:%S").to_string()
    }
}

/// Says when traffic last went each way, or nothing when none has.
pub fn last_traffic(
    counters: &TrafficCounters,
    zone: &jiff::tz::TimeZone,
    now: jiff::Timestamp,
) -> Option<String> {
    let when = |at: Option<&Timestamp>| at.map(|at| clock(at, zone, now));
    match (
        when(counters.last_received.as_ref()),
        when(counters.last_sent.as_ref()),
    ) {
        (None, None) => None,
        (Some(received), None) => Some(format!("Last received at {received}, nothing sent")),
        (None, Some(sent)) => Some(format!("Nothing received, last sent at {sent}")),
        (Some(received), Some(sent)) => {
            Some(format!("Last received at {received}, last sent at {sent}"))
        }
    }
}

/// Describes a network port's automatic port's traffic: what it passed on to the applications on
/// this computer, and what they sent it.
pub fn automatic_port_traffic(
    counters: &TrafficCounters,
    zone: &jiff::tz::TimeZone,
    now: jiff::Timestamp,
) -> String {
    let last = |at: Option<&Timestamp>| {
        at.map(|at| format!(", last at {}", clock(at, zone, now)))
            .unwrap_or_default()
    };
    format!(
        "Passed {} to apps on this computer{}; apps sent it {}{}",
        plural(
            u32::try_from(counters.messages_sent).unwrap_or(u32::MAX),
            "message",
            "messages"
        ),
        last(counters.last_sent.as_ref()),
        plural(
            u32::try_from(counters.messages_received).unwrap_or(u32::MAX),
            "message",
            "messages"
        ),
        last(counters.last_received.as_ref()),
    )
}

/// Labels a note for the note picker with its number and key, marking middle C, since the
/// octave numbering it follows (middle C is C3) is not the only one in use.
pub fn note_label(note: u8) -> String {
    let key = midi_harbor_core::midi::note_name(note);
    if note == 60 {
        format!("{note} · {key} · middle C")
    } else {
        format!("{note} · {key}")
    }
}

/// Returns a noun agreeing with its count, so a summary never reads "1 routes".
pub fn plural(count: u32, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// Shortens a count so a column stays the same width as traffic climbs.
///
/// Thresholds are spelled out rather than derived, since only three are ever used: 1_000 for
/// thousands, 1_000_000 for millions, and 1_000_000_000 for billions.
pub fn compact(value: u64) -> String {
    const THOUSAND: u64 = 1_000;
    const MILLION: u64 = 1_000_000;
    const BILLION: u64 = 1_000_000_000;

    if value < THOUSAND {
        return value.to_string();
    }
    let (scaled, suffix) = if value < MILLION {
        (value as f64 / THOUSAND as f64, "k")
    } else if value < BILLION {
        (value as f64 / MILLION as f64, "M")
    } else {
        (value as f64 / BILLION as f64, "G")
    };
    // One decimal below ten keeps 1.2k distinct from 1.9k, above which the digit adds nothing.
    if scaled < 10.0 {
        format!("{scaled:.1}{suffix}")
    } else {
        format!("{scaled:.0}{suffix}")
    }
}

/// The groups the endpoints page lists, in the order it lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Ports Midi Harbor makes for other applications on this computer.
    VirtualPorts,
    /// RTP-MIDI sessions, which the window calls network ports.
    NetworkPorts,
    /// Bluetooth MIDI devices, and this computer offered as one.
    Bluetooth,
    /// MIDI hardware.
    Hardware,
    /// Ports another application or the system makes, such as an IAC bus.
    Provided,
}

impl Section {
    /// Every section, in the order the page lists them.
    pub const ALL: [Section; 5] = [
        Section::VirtualPorts,
        Section::NetworkPorts,
        Section::Bluetooth,
        Section::Hardware,
        Section::Provided,
    ];

    /// Returns the section's heading.
    pub fn title(self) -> &'static str {
        match self {
            Section::VirtualPorts => {
                "Virtual ports · other applications on this computer see these"
            }
            Section::NetworkPorts => "Network ports · RTP-MIDI",
            Section::Bluetooth => "Bluetooth devices",
            Section::Hardware => "USB and hardware",
            Section::Provided if cfg!(target_os = "macos") => {
                "IAC buses, Apple network sessions and other applications · provided by macOS"
            }
            Section::Provided => "Other applications' ports",
        }
    }
}

/// Returns the section an endpoint is listed under.
///
/// A device is hardware unless the daemon says another application or the system makes it: an
/// IAC bus looked like a keyboard when both were simply "device".
pub fn section(endpoint: &Endpoint) -> Section {
    match (&endpoint.detail, endpoint.kind()) {
        (Some(Detail::PhysicalDevice(device)), _) if device.software => Section::Provided,
        (_, EndpointKind::VirtualPort) => Section::VirtualPorts,
        (_, EndpointKind::NetworkSession) => Section::NetworkPorts,
        (_, EndpointKind::BluetoothDevice) => Section::Bluetooth,
        _ => Section::Hardware,
    }
}

/// Returns what an endpoint is, as a person would say it.
pub fn kind_name(endpoint: &Endpoint) -> &'static str {
    match section(endpoint) {
        Section::VirtualPorts => "Virtual port",
        Section::NetworkPorts => "Network port · RTP-MIDI",
        Section::Bluetooth => "Bluetooth device",
        Section::Hardware => "Hardware",
        Section::Provided if cfg!(target_os = "macos") => "Provided by macOS or an application",
        Section::Provided => "Another application's port",
    }
}

/// The invitation policies, in the order a dropdown offers them, with their labels.
pub const POLICIES: [(&str, InvitationPolicy); 4] = [
    ("Ask each time", InvitationPolicy::Prompt),
    ("Known machines", InvitationPolicy::AcceptKnown),
    ("Anyone", InvitationPolicy::AcceptAll),
    ("No one", InvitationPolicy::RejectAll),
];

/// Returns the labels of the invitation policies, for a dropdown.
pub fn policy_labels() -> Vec<&'static str> {
    POLICIES.iter().map(|(label, _)| *label).collect()
}

/// Returns where a policy sits in [`POLICIES`], treating an unset one as asking each time.
pub fn policy_position(policy: InvitationPolicy) -> usize {
    POLICIES
        .iter()
        .position(|(_, held)| *held == policy)
        .unwrap_or(0)
}

/// Returns the name of a known machine, by its identifier.
pub fn peer_name<'a>(peers: &'a [Peer], id: &str) -> Option<&'a str> {
    peers
        .iter()
        .find(|peer| peer.id == id)
        .map(|peer| peer.advertised_name.as_str())
}

/// Reports whether a machine is one this computer remembers, rather than one merely advertising
/// on the network right now.
pub fn is_remembered(peer: &Peer) -> bool {
    peer.trusted || !peer.discovered
}

/// Describes an endpoint in one line under its name: what distinguishes it from others of its
/// kind.
pub fn detail_line(endpoint: &Endpoint, peers: &[Peer]) -> String {
    match &endpoint.detail {
        Some(Detail::NetworkSession(session)) => {
            // The first machine by the name it advertises, then how many more: a machine this
            // side invited beside the first is not a guest, so the count does not say so.
            let first = session
                .machines
                .first()
                .and_then(|machine| machine.name.clone());
            let with = match (first, session.peer_id.as_deref()) {
                (Some(name), _) => name,
                (None, Some(id)) => peer_name(peers, id).unwrap_or(id).to_owned(),
                (None, None) => "waiting for a machine".to_owned(),
            };
            let others = if session.machines.is_empty() {
                session.guests.len()
            } else {
                session.machines.len().saturating_sub(1)
            };
            let with = match others {
                0 => with,
                1 => format!("{with} and 1 other machine"),
                n => format!("{with} and {n} other machines"),
            };
            let policy = POLICIES
                .get(policy_position(session.invitation_policy()))
                .map_or("Ask each time", |(label, _)| *label);
            format!("UDP {} · {with} · {policy}", session.control_port)
        }
        Some(Detail::BluetoothDevice(device)) if device.peripheral_role => {
            "This computer, offered to phones and tablets".to_owned()
        }
        Some(Detail::BluetoothDevice(device)) => match device.rssi {
            Some(rssi) => format!("Signal {rssi} dBm"),
            None => "Reconnects by itself when heard".to_owned(),
        },
        Some(Detail::PhysicalDevice(device)) => {
            let fingerprint = device.fingerprint.as_ref();
            let maker = fingerprint.and_then(|f| f.manufacturer.clone());
            let model = fingerprint.and_then(|f| f.model.clone());
            let mut line = match (maker, model) {
                (Some(maker), Some(model)) => format!("{maker} {model}"),
                (Some(one), None) | (None, Some(one)) => one,
                // Nothing beyond the name, which the row already shows.
                (None, None) => String::new(),
            };
            if !device.present {
                if !line.is_empty() {
                    line.push_str(" · ");
                }
                line.push_str("remembered, not plugged in");
            }
            line
        }
        _ => {
            let (inputs, outputs) = connectors(endpoint);
            format!("MIDI In {inputs} · MIDI Out {outputs}")
        }
    }
}

/// Returns how many MIDI In and MIDI Out connectors an endpoint has: a virtual port's counts, and
/// one or none of each for anything else, by the way MIDI moves through it.
///
/// A MIDI In is what a route can start from, and a MIDI Out what it can end at.
pub fn connectors(endpoint: &Endpoint) -> (u8, u8) {
    match &endpoint.detail {
        Some(Detail::VirtualPort(port)) => (
            u8::try_from(port.inputs.clamp(1, 16)).unwrap_or(1),
            u8::try_from(port.outputs.clamp(1, 16)).unwrap_or(1),
        ),
        _ => (
            u8::from(can_source(endpoint.direction())),
            u8::from(can_sink(endpoint.direction())),
        ),
    }
}

/// Returns the Bonjour name and UDP port to send for an edited network port: only those that
/// differ from what it has, so an untouched UDP port does not restart it. An empty name means no
/// change, and so does a port that is not a number.
pub fn network_port_changes(
    typed_name: &str,
    typed_port: &str,
    current: Option<(&str, u32)>,
) -> (Option<String>, Option<u32>) {
    let name = typed_name.trim();
    let local_name =
        (!name.is_empty() && current.is_none_or(|(held, _)| held != name)).then(|| name.to_owned());
    let control_port = typed_port
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|typed| current.is_none_or(|(_, held)| held != *typed));
    (local_name, control_port)
}

/// Reports whether both ends of a route can carry MIDI back the other way: the destination has a
/// MIDI In and the source a MIDI Out.
pub fn can_be_two_way(source: &Endpoint, destination: &Endpoint) -> bool {
    connectors(source).1 > 0 && connectors(destination).0 > 0
}

/// Names one connector of an endpoint for a route choice: the endpoint's name alone when it has
/// one of that kind, and numbered as other applications see it when it has several.
pub fn connector_label(name: &str, count: u8, index: u8) -> String {
    if count > 1 {
        format!("{name} {}", u16::from(index) + 1)
    } else {
        name.to_owned()
    }
}

/// Names one end of a route, with its connector when it is past the first.
pub fn route_end(name: &str, kind: &str, connector: u32) -> String {
    if connector > 1 {
        format!("{name} ({kind} {connector})")
    } else {
        name.to_owned()
    }
}

/// Returns an endpoint's kind and detail as the line under its name, leaving out a detail that
/// has nothing to add.
pub fn kind_and_detail(endpoint: &Endpoint, peers: &[Peer]) -> String {
    let detail = detail_line(endpoint, peers);
    if detail.is_empty() {
        kind_name(endpoint).to_owned()
    } else {
        format!("{} · {detail}", kind_name(endpoint))
    }
}

/// Returns a state word with its first letter capitalised, for a status pill.
pub fn capitalised(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi_harbor_ipc::pb::{BluetoothDeviceDetail, NetworkSessionDetail, PhysicalDeviceDetail};

    /// Builds a connection state in a phase, with nothing else to say about it.
    fn state(phase: ConnectionPhase) -> ConnectionState {
        ConnectionState {
            phase: phase.into(),
            ..ConnectionState::default()
        }
    }

    /// Builds an endpoint of the given detail, switched on or off, resting in a phase.
    fn endpoint(detail: Detail, enabled: bool, phase: ConnectionPhase) -> Endpoint {
        Endpoint {
            enabled,
            detail: Some(detail),
            state: Some(state(phase)),
            ..Endpoint::default()
        }
    }

    /// Builds the last error a connection state reports.
    fn failure(message: &str) -> Option<FailureReason> {
        Some(FailureReason {
            code: "unreachable".to_owned(),
            message: message.to_owned(),
            ..FailureReason::default()
        })
    }

    /// Locks the word and tone each endpoint reads with, where the kind or a resting state
    /// overrides the bare phase.
    ///
    /// A user decides whether to intervene from this alone. Hardware has no connection lifecycle,
    /// so it reads attached or absent. A session waiting to be invited and this computer's
    /// advertised Bluetooth port are resting, not faulty: both once read "disconnected" in a
    /// warning colour and sent users looking for a problem. And the two kinds of down stay apart:
    /// "retrying" recovers on its own, "needs attention" does not.
    #[test]
    fn each_endpoint_reads_in_the_terms_its_kind_makes_sense_of() {
        use ConnectionPhase::{Connected, Disabled, Disconnected, Retrying, Unavailable};
        let hardware = |present| {
            Detail::PhysicalDevice(PhysicalDeviceDetail {
                present,
                ..PhysicalDeviceDetail::default()
            })
        };
        let session = || Detail::NetworkSession(NetworkSessionDetail::default());
        let bluetooth = |peripheral_role| {
            Detail::BluetoothDevice(BluetoothDeviceDetail {
                peripheral_role,
                ..BluetoothDeviceDetail::default()
            })
        };
        let cases = [
            (
                "plugged-in hardware",
                endpoint(hardware(true), true, Connected),
                ("attached", Tone::Good),
            ),
            (
                "unplugged hardware",
                endpoint(hardware(false), true, Connected),
                ("absent", Tone::Waiting),
            ),
            (
                "idle session",
                endpoint(session(), true, Disconnected),
                ("listening", Tone::Off),
            ),
            (
                "switched-off idle session",
                endpoint(session(), false, Disconnected),
                ("disconnected", Tone::Waiting),
            ),
            (
                "advertised port",
                endpoint(bluetooth(true), true, Disconnected),
                ("advertising", Tone::Off),
            ),
            (
                "device this computer connects to",
                endpoint(bluetooth(false), true, Disconnected),
                ("disconnected", Tone::Waiting),
            ),
            (
                "retrying session",
                endpoint(session(), true, Retrying),
                ("retrying", Tone::Waiting),
            ),
            (
                "stuck session",
                endpoint(session(), true, Unavailable),
                ("needs attention", Tone::Bad),
            ),
            (
                "disabled session",
                endpoint(session(), false, Disabled),
                ("off", Tone::Off),
            ),
        ];
        for (name, endpoint, want) in cases {
            assert_eq!(
                endpoint_status(&endpoint),
                want,
                "the {name} must read as {want:?}"
            );
        }
    }

    /// Locks what the line beneath a phase says, and which cause wins when several apply.
    ///
    /// Having no network is said before anything else, because retry counts against an
    /// unreachable peer send a user after the wrong fault. A flapping link reads as connected at
    /// any one moment, so the instability is said even while it is up. A retrying endpoint gives
    /// its attempt, when it tries next (US5/AC3) and what failed. A failure that has cleared stays
    /// visible on a link that is still down, and is dropped once the link is connected.
    #[test]
    fn the_line_under_a_phase_names_the_cause_that_matters_most() {
        let retrying = |attempt, next_retry: i64, error: &str| ConnectionState {
            attempt,
            next_retry: Some(Timestamp {
                seconds: next_retry,
                nanos: 0,
            }),
            last_error: failure(error),
            ..state(ConnectionPhase::Retrying)
        };
        let cases = [
            (
                "no network",
                ConnectionState {
                    waiting_for_network: true,
                    ..retrying(5, 1_004, "network unreachable")
                },
                Some("waiting for a network; this computer has no route to the peer"),
            ),
            (
                "flapping but up",
                ConnectionState {
                    unstable: true,
                    ..state(ConnectionPhase::Connected)
                },
                Some("unstable: keeps dropping within seconds of connecting"),
            ),
            (
                // 1_004 - 1_000 = 4 seconds until the next attempt.
                "retrying with an attempt scheduled",
                retrying(3, 1_004, "connection refused"),
                Some("attempt 3, next in 4s, last error: connection refused"),
            ),
            (
                "retrying past its scheduled time",
                retrying(3, 990, "connection refused"),
                Some("attempt 3, trying again now, last error: connection refused"),
            ),
            (
                "down after a failure",
                ConnectionState {
                    last_error: failure("connection refused"),
                    ..state(ConnectionPhase::Disconnected)
                },
                Some("connection refused"),
            ),
            (
                "recovered",
                ConnectionState {
                    last_error: failure("connection refused"),
                    ..state(ConnectionPhase::Connected)
                },
                None,
            ),
        ];
        for (name, state, want) in cases {
            assert_eq!(
                state_detail(&state, 1_000).as_deref(),
                want,
                "the {name} case must say {want:?}"
            );
        }
    }

    /// Locks that spans and ages round down to the unit shown and never go negative.
    ///
    /// Rounding up would claim time that has not passed: 3m 59s reads "3m", never "4m". A
    /// timestamp from a machine whose clock runs ahead reads "just now" rather than "-3s ago",
    /// which would look like a fault in the daemon.
    #[test]
    fn spans_round_down_to_the_unit_shown_and_never_go_negative() {
        let at = |seconds| Some(Timestamp { seconds, nanos: 0 });
        let retry_at = |seconds| ConnectionState {
            next_retry: at(seconds),
            ..ConnectionState::default()
        };
        let cases = [
            // 1_000 + 3 * 60 + 59 = 1_239 seconds away.
            (
                "next attempt 3m 59s away",
                next_attempt(&retry_at(1_239), 1_000),
                Some("next in 3m"),
            ),
            // 1_000 + 49 * 3_600 = 177_400 seconds away.
            (
                "next attempt 49h away",
                next_attempt(&retry_at(177_400), 1_000),
                Some("next in 2d"),
            ),
            (
                "an age under a second",
                Some(duration(999)),
                Some("just now"),
            ),
            // 90_000 ms is 1m 30s.
            ("an age of 1m 30s", Some(duration(90_000)), Some("1m ago")),
            (
                "an event from a clock running ahead",
                age(at(2_000).as_ref(), 1_000),
                Some("just now"),
            ),
            (
                "an event 30s ago",
                age(at(1_000).as_ref(), 1_030),
                Some("30s ago"),
            ),
            ("an event with no time", age(None, 1_000), None),
        ];
        for (name, got, want) in cases {
            assert_eq!(got.as_deref(), want, "{name} must read {want:?}");
        }
    }

    /// Locks where a count switches unit and how many digits it keeps.
    ///
    /// The thresholds are 1_000, 1_000_000 and 1_000_000_000. Below ten of a unit one decimal
    /// keeps 1.2k apart from 1.9k; from ten up the decimal adds width and nothing else.
    #[test]
    fn counts_shorten_without_losing_the_leading_digits() {
        let cases = [
            (999, "999"),
            (1_200, "1.2k"),
            (1_900, "1.9k"),
            (10_000, "10k"),
            (45_000, "45k"),
            (2_500_000, "2.5M"),
            (7_000_000_000, "7.0G"),
        ];
        for (value, want) in cases {
            assert_eq!(compact(value), want, "{value} must shorten to {want}");
        }
    }

    /// Locks how a live route's row explains what it carried, or why it carries nothing.
    ///
    /// "Working and silent" and "not working" are the two answers this row exists to tell apart,
    /// so silence is stated. A route that is off, broken or suspended is explained by its problem
    /// line, and a traffic line beside it would invite a search for a fault already explained. A
    /// suspended endpoint has not gone anywhere, so its wording must not send the user to restore
    /// it the way a missing one does.
    #[test]
    fn a_route_row_says_whether_it_is_silent_or_not_working() {
        let route = |validity: RouteValidity, enabled, sent, dropped| Route {
            validity: validity.into(),
            enabled,
            counters: Some(TrafficCounters {
                messages_sent: sent,
                messages_dropped: dropped,
                ..TrafficCounters::default()
            }),
            ..Route::default()
        };
        let cases = [
            (
                "silent live route",
                route(RouteValidity::Valid, true, 0, 0),
                None,
                Some("nothing carried yet"),
            ),
            (
                "busy route that dropped some",
                route(RouteValidity::Valid, true, 1_200, 3),
                None,
                Some("1.2k messages carried · 3 undelivered"),
            ),
            (
                "switched-off route",
                route(RouteValidity::Valid, false, 5, 0),
                None,
                None,
            ),
            (
                "route missing an end",
                Route {
                    missing: vec!["Keystation".to_owned()],
                    ..route(RouteValidity::Broken, true, 5, 0)
                },
                Some("'Keystation' is not here; the route resumes when it returns"),
                None,
            ),
            (
                "route waiting on a stopped end",
                Route {
                    waiting_on: vec!["Synth".to_owned()],
                    ..route(RouteValidity::Suspended, true, 5, 0)
                },
                Some("'Synth' is not running; the route resumes when it is"),
                None,
            ),
        ];
        for (name, route, problem, traffic) in cases {
            assert_eq!(
                route_problem(&route).as_deref(),
                problem,
                "the {name} must explain itself as {problem:?}"
            );
            assert_eq!(
                route_traffic(&route).as_deref(),
                traffic,
                "the {name} must report its traffic as {traffic:?}"
            );
        }
    }

    /// Locks that an edited network port sends only the settings that changed.
    ///
    /// Sending the UDP port it already has restarts the port and drops every machine in it, so an
    /// untouched or unparseable port field sends nothing. The name is trimmed, and an empty one
    /// means no change rather than a blank name.
    #[test]
    fn an_edit_sends_only_the_network_port_settings_that_changed() {
        let current = Some(("Stage", 5004));
        let cases = [
            ("nothing edited", "Stage", "5004", current, (None, None)),
            (
                "a padded new name",
                " Main Stage ",
                "5004",
                current,
                (Some("Main Stage"), None),
            ),
            (
                "a new UDP port",
                "Stage",
                "5006",
                current,
                (None, Some(5006)),
            ),
            ("both fields cleared", "", "", current, (None, None)),
            (
                "a port that is not a number",
                "Stage",
                "50o4",
                current,
                (None, None),
            ),
            (
                "a port with nothing held yet",
                "Stage",
                "5004",
                None,
                (Some("Stage"), Some(5004)),
            ),
        ];
        for (name, typed_name, typed_port, current, (want_name, want_port)) in cases {
            let (got_name, got_port) = network_port_changes(typed_name, typed_port, current);
            assert_eq!(
                (got_name.as_deref(), got_port),
                (want_name, want_port),
                "{name} must send only what changed"
            );
        }
    }
}
