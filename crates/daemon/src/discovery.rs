//! Finding peers on the local network, and letting them find us.
//!
//! Two stacks, each used where it actually works — established by testing against a second
//! machine, not by preference:
//!
//! - **Browsing** uses the pure-Rust `mdns-sd`. It reliably sees services advertised by any
//!   responder, including Apple's Network MIDI, and reports usable addresses.
//! - **Advertising** uses the platform responder (Bonjour on macOS, Avahi on Linux, the DNS
//!   Client service on Windows), through `midi_harbor_platform::responder`. `mdns-sd`'s responder announces once at registration and then does not answer
//!   queries from other machines, so a service registered through it is visible for a moment and
//!   then invisible to every peer — including to Apple's own browser.
//!
//! The decisive test: the same machine advertising through Apple's `dns-sd -R` was seen
//! immediately by both browsers; advertising the same service through `mdns-sd` was seen by
//! neither. Browsing was never the problem.

use crate::identity::PortIdentity;
use midi_harbor_core::ids::{EndpointId, PeerId};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info};

/// The service type Apple's Network MIDI advertises and browses for.
pub const SERVICE_TYPE: &str = "_apple-midi._udp.local.";

/// The TXT property holding the public key of the daemon advertising a session, in hexadecimal.
///
/// Only Midi Harbor publishes it, which is how a session is known to answer the identity
/// exchange before it is sent a packet no other implementation understands (R-106).
pub const KEY_PROPERTY: &str = "mhkey";

/// The TXT property holding the identifier of the network port a session is.
pub const PORT_PROPERTY: &str = "mhport";

/// How long a peer may go unseen before it is dropped from the list.
pub const PEER_TTL: Duration = Duration::from_secs(120);

/// Why discovery could not start.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// The responder could not be started.
    #[error("could not start mdns: {0}")]
    Start(String),
    /// A service could not be registered.
    #[error("could not advertise as {name}: {detail}")]
    Advertise {
        /// The name that failed to register.
        name: String,
        /// What went wrong.
        detail: String,
    },
}

/// A peer found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPeer {
    /// Stable identity for this peer within this daemon run.
    pub id: PeerId,
    /// The name the peer advertises.
    pub name: String,
    /// The fully qualified service name, which is unique even when names collide.
    pub fullname: String,
    /// Every address the peer advertised.
    pub addresses: Vec<IpAddr>,
    /// The peer's control port.
    pub port: u16,
    /// Set when this is our own advertisement reflected back.
    pub is_self: bool,
    /// Which Midi Harbor network port it says it is. Unproven: an advertisement can say anything.
    pub identity: Option<PortIdentity>,
}

/// What a network port holds about a machine it connects to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Followed<'a> {
    /// The address it is connected to at.
    pub held: SocketAddr,
    /// Whether its invitations are accepted without asking.
    pub trusted: bool,
    /// The session name it was advertising when last seen at its address.
    pub advertised_as: Option<&'a str>,
    /// Which network port it proved it is, when it is a Midi Harbor.
    pub identity: Option<PortIdentity>,
}

/// What the advertisements ask to be done about a machine a network port connects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A session is advertised where the machine is. Keep its name when that is given, and its
    /// identity once it proves it there.
    Learn {
        /// The session's name, when it is not the one held.
        name: Option<String>,
        /// The identity it advertises, when it is not the one held.
        identity: Option<PortIdentity>,
    },
    /// The machine's session is advertised somewhere else. Connect to it there, once it proves
    /// `prove` there when that is set.
    Move {
        /// Where it is advertised now.
        to: SocketAddr,
        /// The name it is advertised under now.
        name: String,
        /// What it must prove at the new address before it is followed.
        prove: Option<PortIdentity>,
    },
}

/// Decides what the advertisements ask to be done about one machine.
///
/// A machine that proved which network port it is, is that port wherever it is advertised and
/// whatever it is called, and nothing else is: a port deleted and made again is another port. It
/// is followed to another host once it proves itself there. Any other machine is known by its
/// session name alone, which proves nothing. It is followed to a new port on the host it was on,
/// and to another host only when it is not trusted: trust is held by host, so following a name
/// would let whichever machine took it in unasked.
pub fn next_step(known: &Followed<'_>, advertised: &[DiscoveredPeer]) -> Option<Step> {
    let advertised = || advertised.iter().filter(|peer| !peer.is_self);

    // A session where the machine is says what it is called and which port it is.
    if let Some(here) = advertised().find(|peer| peer.is_at(known.held)) {
        let name = (known.advertised_as != Some(here.name.as_str())).then(|| here.name.clone());
        let identity = here.identity.filter(|_| here.identity != known.identity);
        return (name.is_some() || identity.is_some()).then_some(Step::Learn { name, identity });
    }

    // Otherwise it may be advertised somewhere else.
    let moved = match known.identity {
        Some(identity) => advertised().find(|peer| peer.identity == Some(identity)),
        None => advertised().find(|peer| known.advertised_as == Some(peer.name.as_str())),
    }?;
    let name = moved.name.clone();
    if moved.is_on(known.held.ip()) {
        return Some(Step::Move {
            to: SocketAddr::new(known.held.ip(), moved.port),
            name,
            prove: None,
        });
    }
    if known.identity.is_none() && known.trusted {
        return None;
    }
    Some(Step::Move {
        to: moved.address()?,
        name,
        prove: known.identity,
    })
}

impl DiscoveredPeer {
    /// Returns the best address to reach this peer on.
    pub fn address(&self) -> Option<SocketAddr> {
        crate::net::choose_peer_address(&self.addresses, self.port)
    }

    /// Reports whether this session is advertised at `address`.
    pub fn is_at(&self, address: SocketAddr) -> bool {
        self.port == address.port() && self.is_on(address.ip())
    }

    /// Reports whether `host` is one of the addresses this session is advertised on.
    fn is_on(&self, host: IpAddr) -> bool {
        self.addresses
            .iter()
            .any(|address| address.to_canonical() == host.to_canonical())
    }

    /// Returns a label that distinguishes this peer from another with the same name.
    ///
    /// Two machines advertising the same name is ordinary on a network of identical laptops, and
    /// showing both as the same thing would make one of them unselectable.
    pub fn label(&self, ambiguous: bool) -> String {
        if !ambiguous {
            return self.name.clone();
        }
        match self.address() {
            Some(address) => format!("{} ({})", self.name, address.ip()),
            None => format!("{} ({})", self.name, self.fullname),
        }
    }
}

/// The set of peers currently visible.
#[derive(Debug, Default)]
pub struct PeerTable {
    peers: HashMap<String, DiscoveredPeer>,
}

impl PeerTable {
    /// Records a peer, replacing any earlier record of the same service.
    pub fn insert(&mut self, peer: DiscoveredPeer) {
        // Keying on the fully qualified name rather than the display name is what keeps two
        // machines called the same thing apart.
        match self.peers.get_mut(&peer.fullname) {
            // Keep the identity stable across re-resolution, so a peer does not appear to be
            // replaced every time its record is refreshed.
            Some(existing) => {
                existing.addresses = peer.addresses;
                existing.port = peer.port;
                existing.name = peer.name;
                existing.identity = peer.identity;
            }
            None => {
                let _ = self.peers.insert(peer.fullname.clone(), peer);
            }
        }
    }

    /// Removes a peer that has gone away.
    pub fn remove(&mut self, fullname: &str) {
        let _ = self.peers.remove(fullname);
    }

    /// Returns the peers a user may connect to, with labels that distinguish duplicates.
    ///
    /// Our own advertisement is excluded: offering to connect a machine to itself is never what
    /// was meant, and the responder does reflect it back.
    pub fn connectable(&self) -> Vec<(DiscoveredPeer, String)> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for peer in self.peers.values().filter(|p| !p.is_self) {
            *counts.entry(peer.name.as_str()).or_insert(0) += 1;
        }

        let mut listed: Vec<(DiscoveredPeer, String)> = self
            .peers
            .values()
            .filter(|peer| !peer.is_self)
            .map(|peer| {
                let ambiguous = counts
                    .get(peer.name.as_str())
                    .is_some_and(|count| *count > 1);
                (peer.clone(), peer.label(ambiguous))
            })
            .collect();
        listed.sort_by(|a, b| a.1.cmp(&b.1));
        listed
    }
}

/// One advertisement, running on its own thread until dropped.
struct Advertisement {
    /// Cleared to ask the thread to withdraw the service and stop.
    running: Arc<AtomicBool>,
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// Browses for peers and advertises this machine.
pub struct Discovery {
    daemon: mdns_sd::ServiceDaemon,
    table: Arc<Mutex<PeerTable>>,
    /// Every name and control port this machine advertises, so its own records are recognised
    /// coming back.
    advertised: Arc<Mutex<Vec<(String, u16)>>>,
    /// The running advertisements, keyed by the name each publishes.
    services: Mutex<HashMap<String, Advertisement>>,
    /// Notified each time a session is resolved or goes, so what follows sessions looks again.
    changed: Arc<tokio::sync::Notify>,
    /// This daemon's public key in hexadecimal, published with every session it advertises.
    key: String,
}

impl Discovery {
    /// Starts the responder and begins browsing. `key` is this daemon's public key, published
    /// with every session it advertises.
    pub fn start(key: &[u8]) -> Result<Arc<Self>, DiscoveryError> {
        let daemon = mdns_sd::ServiceDaemon::new()
            .map_err(|error| DiscoveryError::Start(error.to_string()))?;

        let discovery = Arc::new(Self {
            daemon,
            table: Arc::new(Mutex::new(PeerTable::default())),
            advertised: Arc::new(Mutex::new(Vec::new())),
            services: Mutex::new(HashMap::new()),
            changed: Arc::new(tokio::sync::Notify::new()),
            key: hex::encode(key),
        });

        let receiver = discovery
            .daemon
            .browse(SERVICE_TYPE)
            .map_err(|error| DiscoveryError::Start(error.to_string()))?;

        let table = Arc::clone(&discovery.table);
        let advertised = Arc::clone(&discovery.advertised);
        let changed = Arc::clone(&discovery.changed);
        std::thread::Builder::new()
            .name("mdns-browse".to_owned())
            .spawn(move || browse_loop(receiver, table, advertised, changed))
            .map_err(|error| DiscoveryError::Start(error.to_string()))?;

        Ok(discovery)
    }

    /// Advertises a session so other machines can find it.
    ///
    /// Registered through the platform responder rather than the pure-Rust one, because the
    /// latter announces once and then stops answering queries from other machines. This
    /// daemon's key and the network port's identifier `id` are published with it, so another
    /// Midi Harbor knows which port the session is and that it can be asked to prove it.
    pub fn advertise(&self, name: &str, port: u16, id: EndpointId) -> Result<(), DiscoveryError> {
        // Remembering what we publish is what lets our own records be recognised coming back.
        // Comparing against the machine name would not: a session advertises its own name.
        if let Ok(mut advertised) = self.advertised.lock() {
            advertised.retain(|(existing, _)| existing != name);
            advertised.push((name.to_owned(), port));
        }

        let running = Arc::new(AtomicBool::new(true));
        let (ready, started) = std::sync::mpsc::channel();

        let thread_running = Arc::clone(&running);
        let thread_name = name.to_owned();
        let properties = vec![
            (KEY_PROPERTY.to_owned(), self.key.clone()),
            (PORT_PROPERTY.to_owned(), id.to_string()),
        ];
        std::thread::Builder::new()
            .name(format!("mdns-advertise-{name}"))
            .spawn(move || {
                midi_harbor_platform::responder::advertise(
                    &thread_name,
                    port,
                    &properties,
                    &thread_running,
                    &ready,
                );
            })
            .map_err(|error| DiscoveryError::Advertise {
                name: name.to_owned(),
                detail: error.to_string(),
            })?;

        // Waiting for the responder to accept the registration turns a silent failure into a
        // reported one, which matters because an unadvertised session looks perfectly healthy.
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(detail)) => {
                return Err(DiscoveryError::Advertise {
                    name: name.to_owned(),
                    detail,
                });
            }
            Err(_) => {
                return Err(DiscoveryError::Advertise {
                    name: name.to_owned(),
                    detail: "the platform responder did not answer".to_owned(),
                });
            }
        }

        if let Ok(mut services) = self.services.lock() {
            let _ = services.insert(name.to_owned(), Advertisement { running });
        }
        info!(name, port, "advertising network session");
        Ok(())
    }

    /// Stops advertising a session.
    pub fn withdraw(&self, name: &str) {
        if let Ok(mut services) = self.services.lock() {
            // Dropping the handle stops the thread, which withdraws the service.
            let _ = services.remove(name);
        }
        if let Ok(mut advertised) = self.advertised.lock() {
            advertised.retain(|(existing, _)| existing != name);
        }
    }

    /// Returns the names this machine is announcing now.
    pub fn announcing(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .services
            .lock()
            .map(|services| services.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Waits until a session has been resolved or has gone since this was last waited on.
    ///
    /// A change while nothing was waiting is not missed: the next wait returns at once.
    pub async fn changed(&self) {
        self.changed.notified().await;
    }

    /// Returns the peers a user may connect to.
    pub fn peers(&self) -> Vec<(DiscoveredPeer, String)> {
        match self.table.lock() {
            Ok(table) => table.connectable(),
            Err(poisoned) => poisoned.into_inner().connectable(),
        }
    }
}

/// Reads discovery events until the daemon stops.
fn browse_loop(
    receiver: mdns_sd::Receiver<mdns_sd::ServiceEvent>,
    table: Arc<Mutex<PeerTable>>,
    advertised: Arc<Mutex<Vec<(String, u16)>>>,
    changed: Arc<tokio::sync::Notify>,
) {
    while let Ok(event) = receiver.recv() {
        match event {
            mdns_sd::ServiceEvent::ServiceResolved(info) => {
                let addresses: Vec<IpAddr> = info
                    .get_addresses()
                    .iter()
                    .map(|a| a.to_ip_addr())
                    .collect();
                let port = info.get_port();
                // Our own records come back to us. Read the machine's addresses on each one,
                // because they change as the machine moves between networks.
                let local = local_addresses();
                let is_self = match advertised.lock() {
                    Ok(own) => is_own_record(&own, &local, &addresses, port),
                    Err(poisoned) => {
                        is_own_record(&poisoned.into_inner(), &local, &addresses, port)
                    }
                };
                let identity = info
                    .get_property_val_str(KEY_PROPERTY)
                    .zip(info.get_property_val_str(PORT_PROPERTY))
                    .and_then(|(key, port)| PortIdentity::from_text(key, port));
                let peer = DiscoveredPeer {
                    id: PeerId::new(),
                    is_self,
                    name: instance_name(info.get_fullname()).to_owned(),
                    fullname: info.get_fullname().to_owned(),
                    addresses,
                    port,
                    identity,
                };
                debug!(peer = %peer.fullname, own = peer.is_self, "resolved network peer");

                match table.lock() {
                    Ok(mut table) => table.insert(peer),
                    Err(poisoned) => poisoned.into_inner().insert(peer),
                }
                changed.notify_one();
            }
            mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                match table.lock() {
                    Ok(mut table) => table.remove(&fullname),
                    Err(poisoned) => poisoned.into_inner().remove(&fullname),
                }
                // A session renamed is advertised under its new name before the old one is
                // withdrawn. Until then the old one says the port is still where it was.
                changed.notify_one();
            }
            // A service that is found but never resolves is invisible to the user, so the two
            // stages are logged separately: they fail for different reasons.
            mdns_sd::ServiceEvent::ServiceFound(kind, fullname) => {
                debug!(%kind, %fullname, "service found, awaiting resolution");
            }
            other => debug!(event = ?other, "discovery event"),
        }
    }
}

/// Returns the instance name of a service's full name, the part before the service type.
///
/// Splitting at the first dot instead cut "Mr. Keys" down to "Mr".
fn instance_name(fullname: &str) -> &str {
    fullname
        .strip_suffix(SERVICE_TYPE)
        .and_then(|instance| instance.strip_suffix('.'))
        .unwrap_or(fullname)
}

/// Returns every address this machine's interfaces hold, loopback included.
pub(crate) fn local_addresses() -> Vec<IpAddr> {
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces.iter().map(if_addrs::Interface::ip).collect(),
        Err(error) => {
            debug!(%error, "could not list this machine's addresses");
            Vec::new()
        }
    }
}

/// Reports whether a record is one this machine advertises: a port it advertises on, at an
/// address it holds.
///
/// Matching by name hid another machine's session that shared a name with one of ours, and
/// missed our own record once the responder renamed it over a conflict ("Stage (2)").
fn is_own_record(
    advertised: &[(String, u16)],
    local: &[IpAddr],
    addresses: &[IpAddr],
    port: u16,
) -> bool {
    advertised.iter().any(|(_, own)| *own == port)
        && addresses.iter().any(|address| local.contains(address))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// Returns 192.0.2.x, an address from the range reserved for documentation.
    fn at(last_octet: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, last_octet))
    }

    /// Builds a resolved record for an Apple MIDI service as mdns-sd reports one.
    fn peer(name: &str, instance: &str, last_octet: u8, is_self: bool) -> DiscoveredPeer {
        DiscoveredPeer {
            id: PeerId::new(),
            name: name.to_owned(),
            fullname: format!("{instance}._apple-midi._udp.local."),
            addresses: vec![at(last_octet)],
            port: 5004,
            is_self,
            identity: None,
        }
    }

    /// Proves the instance name is everything before the service type, because DNS-SD instance
    /// names may contain dots (RFC 6763 section 4.3). Splitting at the first dot cut "Mr. Keys"
    /// down to "Mr".
    #[test]
    fn an_instance_name_is_everything_before_the_service_type() {
        let cases = [
            ("Stage._apple-midi._udp.local.", "Stage"),
            ("Mr. Keys._apple-midi._udp.local.", "Mr. Keys"),
        ];
        for (fullname, want) in cases {
            assert_eq!(
                instance_name(fullname),
                want,
                "{fullname}: the instance name must keep any dots of its own"
            );
        }
    }

    /// Proves our own advertisement is recognised by an advertised port on one of this machine's
    /// addresses, and by nothing less: two laptops each with a session called "Stage" are peers of
    /// each other, and another session on this machine uses a port we do not.
    #[test]
    fn our_own_record_is_the_one_on_our_port_at_our_address() {
        let advertised = vec![("Stage".to_owned(), 5004)];
        let local = vec![at(10)];
        let cases = [
            ("our port at our address", at(10), 5004, true),
            (
                "another machine advertising a name of ours",
                at(13),
                5004,
                false,
            ),
            ("another session on this machine", at(10), 5006, false),
        ];
        for (name, address, port, want) in cases {
            assert_eq!(
                is_own_record(&advertised, &local, &[address], port),
                want,
                "{name}: recognised wrongly as our own record or as a peer"
            );
        }
    }

    /// Proves what the advertisements ask to be done about a machine a network port connects
    /// to. A session where it is gives it its name, and its identity to prove. A session by its
    /// name on another port of its host is followed. On another host it is followed by name only
    /// when it is not trusted, and by identity whatever it is called, once proved. A machine with
    /// an identity is never followed by name: a port deleted and made again under the same name
    /// is another port.
    #[test]
    fn an_advertisement_says_where_a_machine_is_and_what_it_must_prove() {
        let held = SocketAddr::new(at(13), 5004);
        let port_a = PortIdentity {
            key: [1; 32],
            port: EndpointId::from_bytes([2; 16]),
        };
        let port_b = PortIdentity {
            port: EndpointId::from_bytes([3; 16]),
            ..port_a
        };
        let session = |name: &str, last_octet: u8, port: u16, identity| DiscoveredPeer {
            port,
            identity,
            ..peer(name, name, last_octet, false)
        };
        let known = |trusted, advertised_as, identity| Followed {
            held,
            trusted,
            advertised_as,
            identity,
        };
        let moved = |last_octet: u8, port: u16, name: &str, prove| {
            Some(Step::Move {
                to: SocketAddr::new(at(last_octet), port),
                name: name.to_owned(),
                prove,
            })
        };
        let cases = [
            (
                "where it is, under the name held",
                known(false, Some("Pad"), None),
                session("Pad", 13, 5004, None),
                None,
            ),
            (
                "where it is, not yet named, advertising an identity",
                known(false, None, None),
                session("Pad", 13, 5004, Some(port_a)),
                Some(Step::Learn {
                    name: Some("Pad".to_owned()),
                    identity: Some(port_a),
                }),
            ),
            (
                "by name on another port of its host, trusted",
                known(true, Some("Pad"), None),
                session("Pad", 13, 5010, None),
                moved(13, 5010, "Pad", None),
            ),
            (
                "by name on another host, not trusted",
                known(false, Some("Pad"), None),
                session("Pad", 14, 5004, None),
                moved(14, 5004, "Pad", None),
            ),
            (
                "by name on another host, trusted",
                known(true, Some("Pad"), None),
                session("Pad", 14, 5004, None),
                None,
            ),
            (
                "by identity on another host, renamed, trusted",
                known(true, Some("Pad"), Some(port_a)),
                session("Stage Pad", 14, 5004, Some(port_a)),
                moved(14, 5004, "Stage Pad", Some(port_a)),
            ),
            (
                "another port under the name of one with an identity",
                known(false, Some("Pad"), Some(port_a)),
                session("Pad", 13, 5010, Some(port_b)),
                None,
            ),
        ];
        for (name, known, advertised, want) in cases {
            assert_eq!(
                next_step(&known, &[advertised]),
                want,
                "{name}: the wrong thing is asked for"
            );
        }
    }

    /// Proves the peer list leaves out our own advertisement, which the responder reflects back,
    /// and labels two machines advertising one name apart by address while a unique name is shown
    /// plainly. Identical laptops advertise identical names, and two identical labels make one
    /// of them unselectable.
    #[test]
    fn the_peer_list_offers_each_other_machine_under_a_label_that_tells_it_apart() {
        let cases = [
            (
                "our own advertisement is left out",
                vec![
                    peer("Studio Mac", "Studio Mac", 1, true),
                    peer("Stage Laptop", "Stage Laptop", 2, false),
                ],
                vec!["Stage Laptop"],
            ),
            (
                "two machines of one name are labelled by address",
                vec![
                    peer("MacBook Pro", "MacBook Pro", 5, false),
                    peer("MacBook Pro", "MacBook Pro (2)", 6, false),
                ],
                vec!["MacBook Pro (192.0.2.5)", "MacBook Pro (192.0.2.6)"],
            ),
        ];
        for (name, records, want) in cases {
            let mut table = PeerTable::default();
            for record in records {
                table.insert(record);
            }
            let labels: Vec<String> = table
                .connectable()
                .into_iter()
                .map(|(_, label)| label)
                .collect();
            assert_eq!(labels, want, "{name}: the peer list is wrong");
        }
    }

    /// Proves a peer resolved again keeps the identity it was first given while taking its new
    /// address, because a peer that appeared replaced on every refresh could not be held as a
    /// stored connection, and the address is what actually changed.
    #[test]
    fn re_resolving_a_peer_keeps_its_identity() {
        let mut table = PeerTable::default();
        let first = peer("Stage", "Stage", 2, false);
        let id = first.id;
        table.insert(first);
        table.insert(peer("Stage", "Stage", 3, false));

        let held: Vec<(PeerId, Vec<IpAddr>)> = table
            .connectable()
            .into_iter()
            .map(|(peer, _)| (peer.id, peer.addresses))
            .collect();
        assert_eq!(
            held,
            vec![(id, vec![at(3)])],
            "the refreshed record must replace the first in place, under its identity"
        );
    }
}
