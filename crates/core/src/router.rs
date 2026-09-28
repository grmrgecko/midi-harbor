//! The route graph: which endpoints feed which.
//!
//! Delivery is **direct only**. MIDI arriving at an endpoint goes to that endpoint's immediate
//! destinations and no further. Transitive delivery would mean adding one route silently changes
//! where existing traffic goes, which is the opposite of what a patchbay should do — and it turns
//! any cycle in the configuration into unbounded amplification.
//!
//! Cycles are still detected, because a user who draws one has almost certainly made a mistake
//! and FR-033 requires telling them. The loop that actually multiplies messages spans two
//! machines, and is caught by the session identity carried on the wire rather than here.

use crate::endpoint::{Endpoint, EndpointKind};
use crate::ids::{EndpointId, RouteId};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Whether a route can currently deliver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteValidity {
    /// Both endpoints exist and the route can carry MIDI.
    Valid,
    /// One or both endpoints are missing, named here so the user knows what to restore.
    Broken {
        /// The endpoint names that matched nothing.
        missing: Vec<String>,
    },
    /// The route takes part in a cycle.
    ///
    /// Not an error: the route still delivers, because delivery is direct and a cycle cannot
    /// multiply messages. It is surfaced because it is almost always a mistake.
    LoopDetected {
        /// The routes forming the cycle, in order.
        cycle: Vec<RouteId>,
    },
    /// Both endpoints exist, but one of them is not running.
    ///
    /// Distinct from broken: nothing is missing and nothing needs restoring, so the user has
    /// nothing to repair. It is distinct from valid because the route is carrying nothing, and a
    /// route that reads as fine while dropping everything is the worst of the three to debug.
    Suspended {
        /// The endpoint names that exist but are not available.
        waiting_on: Vec<String>,
    },
}

impl RouteValidity {
    /// Reports whether MIDI can flow along this route right now.
    pub fn can_deliver(&self) -> bool {
        !matches!(self, Self::Broken { .. } | Self::Suspended { .. })
    }

    /// Reports whether the user has something to repair.
    ///
    /// A suspended route needs nothing: it resumes on its own when the endpoint comes back.
    pub fn needs_repair(&self) -> bool {
        matches!(self, Self::Broken { .. })
    }
}

/// Why a route could not be created.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    /// A route from an endpoint to itself would do nothing useful.
    #[error("a route cannot start and end at the same endpoint")]
    SelfRoute,
    /// The source cannot produce MIDI.
    #[error("{0} has no input side, so it cannot be a route source")]
    NotASource(String),
    /// The destination cannot accept MIDI.
    #[error("{0} has no output side, so it cannot be a route destination")]
    NotADestination(String),
    /// A connector was named that the port does not have.
    #[error("{name} has no {kind} {number}")]
    NoSuchConnector {
        /// The port's name.
        name: String,
        /// "MIDI In" or "MIDI Out".
        kind: &'static str,
        /// The connector asked for, counting from one.
        number: u16,
    },
    /// The same pair is already routed.
    #[error("{from} is already routed to {to}")]
    Duplicate {
        /// The source name.
        from: String,
        /// The destination name.
        to: String,
    },
}

/// One resolved route between two endpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRoute {
    /// Derived from the endpoint pair, so it is stable across restarts.
    pub id: RouteId,
    /// The source endpoint's name, as the configuration stores it.
    pub from: String,
    /// The destination endpoint's name.
    pub to: String,
    /// The source endpoint, when it exists.
    pub source: Option<EndpointId>,
    /// The destination endpoint, when it exists.
    pub destination: Option<EndpointId>,
    /// Which of the source's MIDI In connectors it starts from, counting from zero.
    pub from_connector: u8,
    /// Which of the destination's MIDI Out connectors it ends at, counting from zero.
    pub to_connector: u8,
    /// Whether it also carries MIDI back, from the destination's MIDI In `to_connector` to the
    /// source's MIDI Out `from_connector`.
    pub both_ways: bool,
    /// Whether the user wants it delivering.
    pub enabled: bool,
    /// Whether it currently can.
    pub validity: RouteValidity,
}

impl ResolvedRoute {
    /// Reports whether MIDI should actually be carried along this route right now.
    pub fn is_live(&self) -> bool {
        self.enabled && self.validity.can_deliver()
    }
}

/// One delivery dispatch should make: where the message goes, and which route carries it.
///
/// The route travels with the destination because dispatch is the only place that knows which
/// route delivered a message, and that is what per-route traffic has to be attributed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delivery {
    /// The endpoint to deliver to.
    pub destination: EndpointId,
    /// Which of its MIDI Out connectors to deliver through, counting from zero.
    pub connector: u8,
    /// The route responsible for the delivery.
    pub route: RouteId,
}

/// The route graph, prepared for dispatch.
#[derive(Debug, Default)]
pub struct Router {
    routes: Vec<ResolvedRoute>,
    /// Deliveries per source and MIDI In connector, so dispatch is a lookup rather than a scan.
    fan_out: HashMap<(EndpointId, u8), Vec<Delivery>>,
}

impl Router {
    /// Builds a router from configured routes and the endpoints that exist.
    /// `running` names the endpoints that could carry MIDI this instant — a virtual port that is
    /// open, hardware that is attached, a session that exists. An endpoint missing from it is
    /// configured but not available, which suspends every route touching it.
    pub fn build(
        configured: &[crate::config::RouteConfig],
        endpoints: &[Endpoint],
        running: &HashSet<EndpointId>,
    ) -> Self {
        let mut routes: Vec<ResolvedRoute> = configured
            .iter()
            .map(|route| {
                // A name several endpoints share, with no kind written to choose between them,
                // means none of them: picking one bound the route to whichever came last.
                let source = resolve(endpoints, &route.from, |e| route.comes_from(e));
                let destination = resolve(endpoints, &route.to, |e| route.goes_to(e));

                let mut missing = Vec::new();
                if let Err(reason) = &source {
                    missing.push(reason.clone());
                }
                if let Err(reason) = &destination {
                    missing.push(reason.clone());
                }
                let source = source.ok();
                let destination = destination.ok();

                // A connector the port no longer has is missing, as an endpoint would be, so the
                // route waits for it rather than delivering somewhere else.
                let (from_connector, to_connector) = (route.from_index(), route.to_index());
                if let Some(endpoint) = source
                    && from_connector >= connectors(endpoint).0
                {
                    missing.push(connector_label(&route.from, "MIDI In", from_connector));
                }
                if let Some(endpoint) = destination
                    && to_connector >= connectors(endpoint).1
                {
                    missing.push(connector_label(&route.to, "MIDI Out", to_connector));
                }
                // The way back needs the same numbers the other way round.
                if route.both_ways {
                    if let Some(endpoint) = destination
                        && to_connector >= connectors(endpoint).0
                    {
                        missing.push(connector_label(&route.to, "MIDI In", to_connector));
                    }
                    if let Some(endpoint) = source
                        && from_connector >= connectors(endpoint).1
                    {
                        missing.push(connector_label(&route.from, "MIDI Out", from_connector));
                    }
                }

                ResolvedRoute {
                    id: route.id(),
                    from: route.from.clone(),
                    to: route.to.clone(),
                    source: source.map(|e| e.id),
                    destination: destination.map(|e| e.id),
                    from_connector,
                    to_connector,
                    both_ways: route.both_ways,
                    enabled: route.enabled,
                    validity: if missing.is_empty() {
                        RouteValidity::Valid
                    } else {
                        RouteValidity::Broken { missing }
                    },
                }
            })
            .collect();

        mark_cycles(&mut routes);
        mark_suspended(&mut routes, running);

        // Only live routes take part in dispatch, so a disabled or broken one costs nothing.
        let mut fan_out: HashMap<(EndpointId, u8), Vec<Delivery>> = HashMap::new();
        for route in routes.iter().filter(|r| r.is_live()) {
            if let (Some(source), Some(destination)) = (route.source, route.destination) {
                fan_out
                    .entry((source, route.from_connector))
                    .or_default()
                    .push(Delivery {
                        destination,
                        connector: route.to_connector,
                        route: route.id,
                    });
                // The way back travels under the same route, so what it carries and the notes it
                // leaves sounding are the route's.
                if route.both_ways {
                    fan_out
                        .entry((destination, route.to_connector))
                        .or_default()
                        .push(Delivery {
                            destination: source,
                            connector: route.from_connector,
                            route: route.id,
                        });
                }
            }
        }

        Self { routes, fan_out }
    }

    /// Returns every resolved route, including broken and disabled ones.
    ///
    /// A broken route is kept rather than dropped, so it resumes when its endpoint comes back.
    pub fn routes(&self) -> &[ResolvedRoute] {
        &self.routes
    }

    /// Returns where MIDI arriving at one of `source`'s MIDI In connectors, counting from zero,
    /// should be delivered, and along which route.
    ///
    /// Direct destinations only. Empty for an endpoint with no live routes, which is the common
    /// case and costs a single lookup.
    pub fn deliveries(&self, source: EndpointId, connector: u8) -> &[Delivery] {
        self.fan_out
            .get(&(source, connector))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Returns the routes that take part in a cycle.
    pub fn looping(&self) -> Vec<&ResolvedRoute> {
        self.routes
            .iter()
            .filter(|route| matches!(route.validity, RouteValidity::LoopDetected { .. }))
            .collect()
    }

    /// Returns the routes that cannot deliver because an endpoint is missing.
    pub fn broken(&self) -> Vec<&ResolvedRoute> {
        self.routes
            .iter()
            .filter(|route| matches!(route.validity, RouteValidity::Broken { .. }))
            .collect()
    }

    /// Checks whether a proposed route may be created, between connectors counted from zero,
    /// carrying MIDI back as well when `both_ways`.
    pub fn validate(
        existing: &[crate::config::RouteConfig],
        source: &Endpoint,
        from_connector: u8,
        destination: &Endpoint,
        to_connector: u8,
        both_ways: bool,
    ) -> Result<(), RouteError> {
        if source.id == destination.id {
            return Err(RouteError::SelfRoute);
        }
        if !source.can_source() {
            return Err(RouteError::NotASource(source.name.as_str().to_owned()));
        }
        if !destination.can_sink() {
            return Err(RouteError::NotADestination(
                destination.name.as_str().to_owned(),
            ));
        }
        // Both ways, each end has to be the other as well.
        if both_ways && !destination.can_source() {
            return Err(RouteError::NotASource(destination.name.as_str().to_owned()));
        }
        if both_ways && !source.can_sink() {
            return Err(RouteError::NotADestination(source.name.as_str().to_owned()));
        }

        if from_connector >= connectors(source).0 {
            return Err(RouteError::NoSuchConnector {
                name: source.name.as_str().to_owned(),
                kind: "MIDI In",
                number: u16::from(from_connector) + 1,
            });
        }
        if to_connector >= connectors(destination).1 {
            return Err(RouteError::NoSuchConnector {
                name: destination.name.as_str().to_owned(),
                kind: "MIDI Out",
                number: u16::from(to_connector) + 1,
            });
        }

        if both_ways && to_connector >= connectors(destination).0 {
            return Err(RouteError::NoSuchConnector {
                name: destination.name.as_str().to_owned(),
                kind: "MIDI In",
                number: u16::from(to_connector) + 1,
            });
        }
        if both_ways && from_connector >= connectors(source).1 {
            return Err(RouteError::NoSuchConnector {
                name: source.name.as_str().to_owned(),
                kind: "MIDI Out",
                number: u16::from(from_connector) + 1,
            });
        }

        // A route already carrying either way the new one would is a duplicate: a two-way route
        // carries its reverse too, and delivering one message twice doubles every note.
        let duplicate = existing.iter().any(|route| {
            carries(route, source, from_connector, destination, to_connector)
                || (both_ways && carries(route, destination, to_connector, source, from_connector))
        });
        if duplicate {
            return Err(RouteError::Duplicate {
                from: source.name.as_str().to_owned(),
                to: destination.name.as_str().to_owned(),
            });
        }
        Ok(())
    }
}

/// Reports whether a configured route already carries MIDI from `source`'s MIDI In
/// `from_connector` to `destination`'s MIDI Out `to_connector`, either as it is written or, for a
/// two-way route, the other way round.
fn carries(
    route: &crate::config::RouteConfig,
    source: &Endpoint,
    from_connector: u8,
    destination: &Endpoint,
    to_connector: u8,
) -> bool {
    let forward = route.comes_from(source)
        && route.goes_to(destination)
        && route.from_index() == from_connector
        && route.to_index() == to_connector;
    let back = route.both_ways
        && route.comes_from(destination)
        && route.goes_to(source)
        && route.from_index() == to_connector
        && route.to_index() == from_connector;
    forward || back
}

/// Returns how many MIDI In and MIDI Out connectors an endpoint has. Only a virtual port may have
/// more than one of either.
pub fn connectors(endpoint: &Endpoint) -> (u8, u8) {
    match &endpoint.kind {
        EndpointKind::VirtualPort(port) => (port.inputs, port.outputs),
        _ => (1, 1),
    }
}

/// Names a connector a route needs and its endpoint lacks, as a broken route reports it.
fn connector_label(name: &str, kind: &str, index: u8) -> String {
    format!("{name} {kind} {}", u16::from(index) + 1)
}

/// Finds the one endpoint a route's end designates, or says why there is none.
fn resolve<'a>(
    endpoints: &'a [Endpoint],
    name: &str,
    designates: impl Fn(&Endpoint) -> bool,
) -> Result<&'a Endpoint, String> {
    let mut found = endpoints.iter().filter(|endpoint| designates(endpoint));
    match (found.next(), found.next()) {
        (Some(endpoint), None) => Ok(endpoint),
        (None, _) => Err(name.to_owned()),
        (Some(_), Some(_)) => Err(format!("{name} (more than one endpoint has this name)")),
    }
}

/// Marks every route whose endpoints exist but are not currently able to carry anything.
///
/// Applied only to routes that are otherwise valid: a route that is broken or part of a cycle has
/// something to say that matters more, and both of those outlast whatever is switched off today.
fn mark_suspended(routes: &mut [ResolvedRoute], running: &HashSet<EndpointId>) {
    for route in routes.iter_mut() {
        if !matches!(route.validity, RouteValidity::Valid) {
            continue;
        }

        let mut waiting_on = Vec::new();
        if route.source.is_some_and(|id| !running.contains(&id)) {
            waiting_on.push(route.from.clone());
        }
        if route.destination.is_some_and(|id| !running.contains(&id)) {
            waiting_on.push(route.to.clone());
        }
        if !waiting_on.is_empty() {
            route.validity = RouteValidity::Suspended { waiting_on };
        }
    }
}

/// Marks every route that takes part in a cycle.
///
/// Walks the graph by endpoint name, so a cycle drawn through an endpoint that does not exist yet
/// is still reported rather than hidden behind a broken route.
fn mark_cycles(routes: &mut [ResolvedRoute]) {
    // Adjacency by name, since a cycle is a property of the configuration rather than of what
    // happens to be plugged in right now.
    // A two-way route is an edge each way. Its own reverse is never taken as a way back: a
    // message goes where its source is routed and no further, so A to B and back is not a loop.
    let mut edges: HashMap<&str, Vec<(usize, &str)>> = HashMap::new();
    for (index, route) in routes.iter().enumerate() {
        edges
            .entry(route.from.as_str())
            .or_default()
            .push((index, route.to.as_str()));
        if route.both_ways {
            edges
                .entry(route.to.as_str())
                .or_default()
                .push((index, route.from.as_str()));
        }
    }

    let mut in_cycle: BTreeSet<usize> = BTreeSet::new();
    for start in 0..routes.len() {
        let Some(route) = routes.get(start) else {
            continue;
        };
        let mut ways = vec![(route.from.as_str(), route.to.as_str())];
        if route.both_ways {
            ways.push((route.to.as_str(), route.from.as_str()));
        }
        for (origin, first) in ways {
            // A cycle exists when the far end can reach the near end again.
            let mut visited: HashSet<&str> = HashSet::new();
            let mut frontier = vec![(first, vec![start])];

            while let Some((node, path)) = frontier.pop() {
                if node == origin {
                    in_cycle.extend(path.iter().copied());
                    break;
                }
                if !visited.insert(node) {
                    continue;
                }
                for (next, to) in edges.get(node).map(Vec::as_slice).unwrap_or_default() {
                    if *next == start {
                        continue;
                    }
                    let mut extended = path.clone();
                    extended.push(*next);
                    frontier.push((to, extended));
                }
            }
        }
    }

    let cycle: Vec<RouteId> = in_cycle
        .iter()
        .filter_map(|index| routes.get(*index).map(|r| r.id))
        .collect();
    for index in in_cycle {
        if let Some(route) = routes.get_mut(index) {
            // A broken route keeps its more useful reason: naming the missing endpoint tells the
            // user what to fix, where a loop warning would not.
            if matches!(route.validity, RouteValidity::Valid) {
                route.validity = RouteValidity::LoopDetected {
                    cycle: cycle.clone(),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouteConfig;
    use crate::endpoint::{
        Direction, EndpointKind, EndpointName, InvitationPolicy, KindTag, NetworkSession,
        VirtualPort,
    };

    fn name(text: &str) -> EndpointName {
        EndpointName::new(text).expect("a valid name")
    }

    fn endpoint(text: &str, direction: Direction) -> Endpoint {
        Endpoint {
            direction,
            ..port(text)
        }
    }

    fn port(text: &str) -> Endpoint {
        port_with(text, 1, 1)
    }

    fn port_with(text: &str, inputs: u8, outputs: u8) -> Endpoint {
        Endpoint::new(
            name(text),
            EndpointKind::VirtualPort(VirtualPort::with_connectors(inputs, outputs)),
        )
    }

    fn session(text: &str) -> Endpoint {
        Endpoint::new(
            name(text),
            EndpointKind::NetworkSession(NetworkSession::new(
                name(text),
                0,
                InvitationPolicy::Prompt,
            )),
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

    fn connected(from: &str, from_connector: u8, to: &str, to_connector: u8) -> RouteConfig {
        RouteConfig {
            from_connector: Some(from_connector),
            to_connector: Some(to_connector),
            ..route(from, to)
        }
    }

    fn both_ways(route: RouteConfig) -> RouteConfig {
        RouteConfig {
            both_ways: true,
            ..route
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|text| (*text).to_owned()).collect()
    }

    /// A route is valid, broken or suspended by what it names and what is running, and only a
    /// valid one takes part in dispatch.
    ///
    /// Broken names what matched nothing, so the user knows what to restore, and outranks
    /// suspended because it outlasts whatever is switched off today. A name two kinds share
    /// designates neither unless the route writes the kind, since resolving by name alone bound
    /// the route to whichever came last. A connector the port lacks is missing like an endpoint,
    /// so the route waits rather than delivering somewhere nobody asked; a two-way route needs
    /// the connectors of the way back too, the destination's MIDI In and the source's MIDI Out
    /// of the same numbers.
    #[test]
    fn a_route_is_valid_broken_or_suspended_by_what_it_names_and_what_runs() {
        struct Case {
            name: &'static str,
            endpoints: Vec<Endpoint>,
            route: RouteConfig,
            running: Option<&'static [&'static str]>,
            want: RouteValidity,
        }
        let cases = [
            Case {
                name: "both ends exist and run",
                endpoints: vec![port("Keyboard"), port("Synth")],
                route: route("Keyboard", "Synth"),
                running: None,
                want: RouteValidity::Valid,
            },
            Case {
                name: "the destination is not running",
                endpoints: vec![port("Keyboard"), port("Synth")],
                route: route("Keyboard", "Synth"),
                running: Some(&["Keyboard"]),
                want: RouteValidity::Suspended {
                    waiting_on: names(&["Synth"]),
                },
            },
            Case {
                name: "neither end is running",
                endpoints: vec![port("Keyboard"), port("Synth")],
                route: route("Keyboard", "Synth"),
                running: Some(&[]),
                want: RouteValidity::Suspended {
                    waiting_on: names(&["Keyboard", "Synth"]),
                },
            },
            Case {
                name: "the destination is missing",
                endpoints: vec![port("Keyboard")],
                route: route("Keyboard", "Gone Away"),
                running: None,
                want: RouteValidity::Broken {
                    missing: names(&["Gone Away"]),
                },
            },
            Case {
                name: "a missing end outranks one that is not running",
                endpoints: vec![port("Keyboard")],
                route: route("Keyboard", "Gone Away"),
                running: Some(&[]),
                want: RouteValidity::Broken {
                    missing: names(&["Gone Away"]),
                },
            },
            Case {
                name: "a name two kinds share, with the kind written",
                endpoints: vec![port("Keystation"), session("Keystation"), port("Synth")],
                route: RouteConfig {
                    from_kind: Some(KindTag::VirtualPort),
                    ..route("Keystation", "Synth")
                },
                running: None,
                want: RouteValidity::Valid,
            },
            Case {
                name: "a name two kinds share, with no kind written",
                endpoints: vec![port("Keystation"), session("Keystation"), port("Synth")],
                route: route("Keystation", "Synth"),
                running: None,
                want: RouteValidity::Broken {
                    missing: names(&["Keystation (more than one endpoint has this name)"]),
                },
            },
            Case {
                name: "a MIDI In the port no longer has",
                endpoints: vec![port("Keys"), port("Synth")],
                route: connected("Keys", 2, "Synth", 1),
                running: None,
                want: RouteValidity::Broken {
                    missing: names(&["Keys MIDI In 2"]),
                },
            },
            Case {
                name: "a two-way route without the destination's MIDI In of the way back",
                endpoints: vec![port_with("Keys", 2, 2), port_with("Synth", 1, 2)],
                route: both_ways(connected("Keys", 2, "Synth", 2)),
                running: None,
                want: RouteValidity::Broken {
                    missing: names(&["Synth MIDI In 2"]),
                },
            },
            Case {
                name: "a two-way route without the source's MIDI Out of the way back",
                endpoints: vec![port_with("Keys", 2, 1), port_with("Synth", 2, 2)],
                route: both_ways(connected("Keys", 2, "Synth", 2)),
                running: None,
                want: RouteValidity::Broken {
                    missing: names(&["Keys MIDI Out 2"]),
                },
            },
        ];
        for case in cases {
            let running: HashSet<EndpointId> = case
                .endpoints
                .iter()
                .filter(|e| case.running.is_none_or(|on| on.contains(&e.name.as_str())))
                .map(|e| e.id)
                .collect();
            let router =
                Router::build(std::slice::from_ref(&case.route), &case.endpoints, &running);
            let resolved = &router.routes()[0];
            assert_eq!(
                resolved.validity, case.want,
                "{}: the route must say why it can or cannot carry MIDI",
                case.name
            );
            let delivers = resolved.source.is_some_and(|source| {
                !router
                    .deliveries(source, case.route.from_index())
                    .is_empty()
            });
            assert_eq!(
                delivers,
                case.want == RouteValidity::Valid,
                "{}: only a valid route may take part in dispatch",
                case.name
            );
        }
    }

    /// Every route in a cycle is reported, and none outside it, while each still delivers
    /// (FR-033).
    ///
    /// Delivery is direct, so a cycle cannot multiply messages and refusing it would be stricter
    /// than necessary, but one drawn by hand is almost always a mistake. A two-way route is an
    /// edge each way, yet never its own way back: what reaches B from A is not sent on, so A to
    /// B and back is no loop. A route to a missing endpoint keeps the broken reason, which says
    /// what to restore where a loop warning would not.
    #[test]
    fn every_route_in_a_cycle_is_reported_and_still_delivers() {
        let cases = [
            (
                "two routes each way",
                &["A", "B"][..],
                vec![route("A", "B"), route("B", "A")],
                &["A->B", "B->A"][..],
            ),
            (
                "three routes round",
                &["A", "B", "C"][..],
                vec![route("A", "B"), route("B", "C"), route("C", "A")],
                &["A->B", "B->C", "C->A"][..],
            ),
            (
                "a route leaving the cycle",
                &["A", "B", "C"][..],
                vec![route("A", "B"), route("B", "A"), route("A", "C")],
                &["A->B", "B->A"][..],
            ),
            (
                "a two-way route alone",
                &["A", "B"][..],
                vec![both_ways(route("A", "B"))],
                &[][..],
            ),
            (
                "a two-way route closing a cycle with two others",
                &["A", "B", "C"][..],
                vec![both_ways(route("A", "B")), route("B", "C"), route("C", "A")],
                &["A->B", "B->C", "C->A"][..],
            ),
            (
                "a cycle through the ways back of two two-way routes",
                &["A", "B", "C"][..],
                vec![
                    both_ways(route("B", "A")),
                    both_ways(route("C", "B")),
                    route("C", "A"),
                ],
                &["B->A", "C->A", "C->B"][..],
            ),
            (
                "a cycle through a missing endpoint",
                &["A"][..],
                vec![route("A", "B"), route("B", "A")],
                &[][..],
            ),
        ];
        for (case, present, configured, want) in cases {
            let endpoints: Vec<Endpoint> = present.iter().map(|text| port(text)).collect();
            let running = endpoints.iter().map(|e| e.id).collect();
            let router = Router::build(&configured, &endpoints, &running);
            let looping = router.looping();
            let mut found: Vec<String> = looping
                .iter()
                .map(|route| format!("{}->{}", route.from, route.to))
                .collect();
            found.sort();
            assert_eq!(
                found, want,
                "{case}: exactly the routes in the cycle must be reported"
            );
            assert!(
                looping.iter().all(|route| route.is_live()),
                "{case}: a route in a cycle must still deliver"
            );
        }
    }

    /// A proposed route is refused exactly when it could not carry MIDI as asked, or would carry
    /// what an existing route already does.
    ///
    /// The source must send and the destination receive, and a two-way route needs each end to
    /// do both. Connectors are counted from zero here and named from one in the refusal, so
    /// connector index 1 is "MIDI In 2". A route carrying the same way as an existing one, as
    /// written or as the way back of a two-way route, would deliver every message twice. The
    /// reverse direction, and the same ports through other connectors, are other routes.
    #[test]
    fn a_proposed_route_is_refused_exactly_when_it_cannot_or_need_not_exist() {
        struct Case {
            name: &'static str,
            existing: Vec<RouteConfig>,
            source: Endpoint,
            from: u8,
            destination: Endpoint,
            to: u8,
            both_ways: bool,
            want: Result<(), RouteError>,
        }
        let itself = port("A");
        let missing_connector = |name: &str, kind: &'static str| {
            Err(RouteError::NoSuchConnector {
                name: name.to_owned(),
                kind,
                number: 2,
            })
        };
        let duplicate = |from: &str, to: &str| {
            Err(RouteError::Duplicate {
                from: from.to_owned(),
                to: to.to_owned(),
            })
        };
        let cases = [
            Case {
                name: "an endpoint routed to itself",
                existing: vec![],
                source: itself.clone(),
                from: 0,
                destination: itself,
                to: 0,
                both_ways: false,
                want: Err(RouteError::SelfRoute),
            },
            Case {
                name: "an input-only endpoint to an output-only one",
                existing: vec![],
                source: endpoint("Keyboard", Direction::Input),
                from: 0,
                destination: endpoint("Speaker", Direction::Output),
                to: 0,
                both_ways: false,
                want: Ok(()),
            },
            Case {
                name: "an output-only endpoint as the source",
                existing: vec![],
                source: endpoint("Speaker", Direction::Output),
                from: 0,
                destination: port("Synth"),
                to: 0,
                both_ways: false,
                want: Err(RouteError::NotASource("Speaker".to_owned())),
            },
            Case {
                name: "an input-only endpoint as the destination",
                existing: vec![],
                source: port("Keys"),
                from: 0,
                destination: endpoint("Pads", Direction::Input),
                to: 0,
                both_ways: false,
                want: Err(RouteError::NotADestination("Pads".to_owned())),
            },
            Case {
                name: "two-way to an endpoint that sends nothing",
                existing: vec![],
                source: port("Keys"),
                from: 0,
                destination: endpoint("Speaker", Direction::Output),
                to: 0,
                both_ways: true,
                want: Err(RouteError::NotASource("Speaker".to_owned())),
            },
            Case {
                name: "two-way from an endpoint that receives nothing",
                existing: vec![],
                source: endpoint("Pads", Direction::Input),
                from: 0,
                destination: port("Synth"),
                to: 0,
                both_ways: true,
                want: Err(RouteError::NotADestination("Pads".to_owned())),
            },
            Case {
                name: "a second MIDI Out the destination has",
                existing: vec![],
                source: port("Keys"),
                from: 0,
                destination: port_with("Synth", 1, 2),
                to: 1,
                both_ways: false,
                want: Ok(()),
            },
            Case {
                name: "a second MIDI In the source lacks",
                existing: vec![],
                source: port("Keys"),
                from: 1,
                destination: port_with("Synth", 1, 2),
                to: 0,
                both_ways: false,
                want: missing_connector("Keys", "MIDI In"),
            },
            Case {
                name: "two-way without the destination's MIDI In of the way back",
                existing: vec![],
                source: port_with("Keys", 2, 2),
                from: 1,
                destination: port_with("Synth", 1, 2),
                to: 1,
                both_ways: true,
                want: missing_connector("Synth", "MIDI In"),
            },
            Case {
                name: "two-way without the source's MIDI Out of the way back",
                existing: vec![],
                source: port_with("Keys", 2, 1),
                from: 1,
                destination: port_with("Synth", 2, 2),
                to: 1,
                both_ways: true,
                want: missing_connector("Keys", "MIDI Out"),
            },
            Case {
                name: "the same route again",
                existing: vec![route("A", "B")],
                source: port("A"),
                from: 0,
                destination: port("B"),
                to: 0,
                both_ways: false,
                want: duplicate("A", "B"),
            },
            Case {
                name: "the reverse of an existing route",
                existing: vec![route("A", "B")],
                source: port("B"),
                from: 0,
                destination: port("A"),
                to: 0,
                both_ways: false,
                want: Ok(()),
            },
            Case {
                name: "the same ports through another connector",
                existing: vec![connected("Keys", 1, "Synth", 1)],
                source: port("Keys"),
                from: 0,
                destination: port_with("Synth", 1, 2),
                to: 1,
                both_ways: false,
                want: Ok(()),
            },
            Case {
                name: "the way back of an existing two-way route",
                existing: vec![both_ways(route("Keys", "Stage"))],
                source: port("Stage"),
                from: 0,
                destination: port("Keys"),
                to: 0,
                both_ways: false,
                want: duplicate("Stage", "Keys"),
            },
            Case {
                name: "two-way over an existing route the other way",
                existing: vec![route("Stage", "Keys")],
                source: port("Keys"),
                from: 0,
                destination: port("Stage"),
                to: 0,
                both_ways: true,
                want: duplicate("Keys", "Stage"),
            },
        ];
        for case in cases {
            assert_eq!(
                Router::validate(
                    &case.existing,
                    &case.source,
                    case.from,
                    &case.destination,
                    case.to,
                    case.both_ways,
                ),
                case.want,
                "{}: a route must be refused exactly when it cannot or need not exist",
                case.name
            );
        }
    }
}
