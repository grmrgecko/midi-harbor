//! The UDP transport a network session runs over.
//!
//! A session needs two adjacent ports: control on `n` and data on `n + 1`. Binding them
//! separately would sometimes get a non-adjacent pair, so they are acquired together and the
//! attempt is retried until a usable pair is found.

use midi_harbor_rtpmidi::session::Port;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::net::UdpSocket;
use tracing::debug;

/// Largest datagram accepted, which is comfortably above any RTP-MIDI packet with a journal.
pub const MAX_DATAGRAM: usize = 1500;

/// How many port pairs to try before giving up.
const BIND_ATTEMPTS: u32 = 32;

/// How many times a port pair asked for by number is tried while it is held, before another
/// pair is chosen.
///
/// Nine waits of `HELD_PORT_DELAY` come to 225 ms, far longer than a starting child holds a copy
/// of a socket, and short enough that a port another program keeps delays a session's start only
/// briefly.
const HELD_PORT_RETRIES: u32 = 10;

/// How long to wait between tries of a held port pair.
const HELD_PORT_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

/// The port Apple's implementation uses by default.
pub const DEFAULT_CONTROL_PORT: u16 = 5004;

/// Why the transport could not be established.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// Neither the requested port pair nor any nearby pair was available.
    #[error("could not bind an adjacent udp port pair after {BIND_ATTEMPTS} attempts")]
    NoPortPair,
    /// A socket operation failed.
    #[error("{operation} failed: {source}")]
    Io {
        /// What was attempted.
        operation: &'static str,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
}

impl NetError {
    /// Reports whether this failure means the machine has no route to send by.
    ///
    /// The kernel refuses such a send at once, before anything reaches the wire, which is how "no
    /// network" is told apart from a peer that is simply not answering. Checking the machine's
    /// addresses instead is no good: an interface that is up carries a link-local address with no
    /// network behind it, and on macOS tunnel interfaces carry one even with Wi-Fi off.
    pub fn is_no_route(&self) -> bool {
        match self {
            Self::Io { source, .. } => matches!(
                source.kind(),
                io::ErrorKind::NetworkUnreachable
                    | io::ErrorKind::HostUnreachable
                    | io::ErrorKind::AddrNotAvailable
            ),
            Self::NoPortPair => false,
        }
    }
}

/// A datagram received on one of the session's two ports.
#[derive(Debug)]
pub struct Datagram {
    /// Which port it arrived on.
    pub port: Port,
    /// Who sent it.
    pub from: SocketAddr,
    /// The bytes received.
    pub bytes: Vec<u8>,
}

/// The adjacent socket pair one session runs over.
#[derive(Debug)]
pub struct SessionSockets {
    control: UdpSocket,
    data: UdpSocket,
    control_port: u16,
    /// Whether the sockets are dual-stack, which changes how an IPv4 peer is addressed.
    dual_stack: bool,
}

impl SessionSockets {
    /// Binds an adjacent control and data port pair.
    ///
    /// A requested port of zero asks the system to choose. Both sockets are dual-stack where the
    /// platform allows it, so a peer reachable only over IPv6 is still reachable.
    pub async fn bind(requested: u16) -> Result<Self, NetError> {
        let mut candidate = requested;

        for attempt in 0..BIND_ATTEMPTS {
            // An odd control port would put the data port on an even one, which some
            // implementations refuse, so only even ports are tried.
            // Past the top of the range there is no even port to move to, so the system is asked
            // again. Saturating stayed on 65535 for every attempt.
            if candidate != 0 && !candidate.is_multiple_of(2) {
                candidate = candidate.checked_add(1).unwrap_or(0);
            }

            // The port asked for is waited for while it is held, since moving leaves behind every
            // peer that knew it. Whatever is tried after it is not.
            let bound = if attempt == 0 && candidate != 0 {
                bind_pair_waiting(candidate).await
            } else {
                bind_pair(candidate).await
            };
            match bound {
                Ok(sockets) => return Ok(sockets),
                // The system's choice is held to the same rule. An odd port it chose was kept,
                // then recorded, and on the next start moved up one to satisfy the rule above,
                // so a peer that had connected to it found nothing there.
                Err(Refusal::Odd(port)) => candidate = port.checked_add(1).unwrap_or(0),
                Err(Refusal::Taken { control, .. }) => {
                    candidate = next_candidate(control, attempt);
                }
            }
        }
        Err(NetError::NoPortPair)
    }

    /// Reports whether the pair starting at `control` could be bound now, releasing it again.
    ///
    /// Asked before a running session is moved, so a pair that is taken refuses the move while
    /// the session still holds its old one. Stopping it first and then finding the new pair taken
    /// meant winning the old one back, and a daemon starting beside it could hold that for longer
    /// than the wait allows (R-082), leaving the session on a third pair.
    pub async fn pair_free(control: u16) -> bool {
        control.is_multiple_of(2) && bind_pair_waiting(control).await.is_ok()
    }

    /// Returns the control port, which is what a session advertises.
    pub fn control_port(&self) -> u16 {
        self.control_port
    }

    /// Sends a datagram on one of the two ports.
    pub async fn send(&self, port: Port, to: SocketAddr, bytes: &[u8]) -> Result<(), NetError> {
        let socket = match port {
            Port::Control => &self.control,
            Port::Data => &self.data,
        };
        socket
            .send_to(bytes, self.address_for(to))
            .await
            .map(|_| ())
            .map_err(|source| NetError::Io {
                operation: "send",
                source,
            })
    }

    /// Waits for a datagram on either port.
    pub async fn recv(&self) -> Result<Datagram, NetError> {
        let mut control_buffer = [0u8; MAX_DATAGRAM];
        let mut data_buffer = [0u8; MAX_DATAGRAM];

        // Both ports are watched together, because a session cannot make progress if either is
        // ignored while the other is read.
        tokio::select! {
            result = self.control.recv_from(&mut control_buffer) => {
                let (len, from) = result
                    .map_err(|source| NetError::Io { operation: "receive", source })?;
                Ok(Datagram {
                    port: Port::Control,
                    from,
                    bytes: control_buffer.get(..len).unwrap_or_default().to_vec(),
                })
            }
            result = self.data.recv_from(&mut data_buffer) => {
                let (len, from) = result
                    .map_err(|source| NetError::Io { operation: "receive", source })?;
                Ok(Datagram {
                    port: Port::Data,
                    from,
                    bytes: data_buffer.get(..len).unwrap_or_default().to_vec(),
                })
            }
        }
    }
}

impl SessionSockets {
    /// Rewrites a target address into the form these sockets can actually send to.
    ///
    /// A dual-stack socket refuses a plain IPv4 address outright: the send fails with an invalid
    /// argument rather than going nowhere quietly. IPv4 peers are the common case on a local
    /// network, so getting this wrong makes almost every peer unreachable.
    fn address_for(&self, to: SocketAddr) -> SocketAddr {
        match (self.dual_stack, to.ip()) {
            (true, IpAddr::V4(v4)) => SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), to.port()),
            _ => to,
        }
    }
}

/// Why a port pair could not be bound.
#[derive(Debug)]
enum Refusal {
    /// The system chose an odd control port, which a pair may not start on.
    Odd(u16),
    /// A port of the pair could not be bound.
    Taken {
        /// The control port tried, or zero when the system was asked to choose one.
        control: u16,
        /// Whether a socket held the port, rather than the bind failing some other way.
        in_use: bool,
    },
}

/// Binds the pair starting at `control`, waiting out a hold on either port for up to
/// `HELD_PORT_RETRIES` tries.
///
/// On Linux a process that starts another program hands the child a copy of every socket it
/// holds, and the copies stay open until the child has started, a few milliseconds later. A
/// session switched off and straight back on, or a daemon restarted, found its own port held by
/// such a copy and moved to another pair, away from every peer that knew the first (R-082).
async fn bind_pair_waiting(control: u16) -> Result<SessionSockets, Refusal> {
    let mut tries: u32 = 1;
    loop {
        match bind_pair(control).await {
            Err(Refusal::Taken { in_use: true, .. }) if tries < HELD_PORT_RETRIES => {
                tries = tries.saturating_add(1);
                tokio::time::sleep(HELD_PORT_DELAY).await;
            }
            bound => return bound,
        }
    }
}

/// Binds the pair starting at `control`, or at a port the system chooses for zero.
async fn bind_pair(control: u16) -> Result<SessionSockets, Refusal> {
    let taken = |control: u16, error: &io::Error| Refusal::Taken {
        control,
        in_use: error.kind() == io::ErrorKind::AddrInUse,
    };
    let socket = bind_one(control)
        .await
        .map_err(|error| taken(control, &error))?;
    let local = socket
        .local_addr()
        .map_err(|error| taken(control, &error))?;
    let control_port = local.port();
    if !control_port.is_multiple_of(2) {
        return Err(Refusal::Odd(control_port));
    }

    // The data port must be exactly one above, so a failure here means this pair is unusable
    // however well the control port bound.
    let data = bind_one(control_port.saturating_add(1))
        .await
        .map_err(|error| taken(control_port, &error))?;
    let dual_stack = local.is_ipv6();
    debug!(
        control = control_port,
        dual_stack, "bound session port pair"
    );
    Ok(SessionSockets {
        control: socket,
        data,
        control_port,
        dual_stack,
    })
}

/// Binds one dual-stack UDP socket on the given port, or on one the system chooses for zero.
async fn bind_one(port: u16) -> io::Result<UdpSocket> {
    // Claim the port over IPv4 first. macOS binds a dual-stack socket to a port an IPv4 socket
    // already holds, whether the port is named or chosen by the system, and the IPv4 socket
    // then receives everything sent to the port over IPv4. Binding over IPv4 is refused when
    // any IPv4 socket holds the port, and the system's IPv4 choice avoids ports in use.
    let port = {
        let claim = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).await?;
        claim.local_addr()?.port()
    };

    // Binding the unspecified IPv6 address gives IPv4 too, once asked for, so one socket serves
    // peers on either family.
    match bind_dual_stack(port) {
        Ok(socket) => Ok(socket),
        // A port held over IPv6 is refused, not bound over IPv4 alone. The other socket of the
        // pair is dual-stack, sends to it are addressed as IPv6, and an IPv4 socket refuses
        // every one: the session never got past inviting the data port.
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => Err(error),
        // A system with IPv6 disabled still has to work.
        Err(_) => UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port))).await,
    }
}

/// Binds a UDP socket on the unspecified IPv6 address that takes IPv4 as well.
///
/// Linux and macOS make such a socket dual-stack by default; Windows makes it IPv6-only, and a
/// Windows daemon's every send to an IPv4 peer failed with "the requested address is not valid in
/// its context" (research R-085). Asking explicitly gives the same socket everywhere, and so does
/// claiming the port exclusively, which Windows otherwise shares (R-088).
fn bind_dual_stack(port: u16) -> io::Result<UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV6,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_only_v6(false)?;
    midi_harbor_platform::socket::claim_exclusively(&socket)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
    UdpSocket::from_std(socket.into())
}

/// Picks the next port pair to try after a failed attempt.
///
/// The first retry asks the system to choose, which almost always succeeds; later retries step
/// upward in case the system keeps handing back an unusable neighbour.
fn next_candidate(previous: u16, attempt: u32) -> u16 {
    if attempt == 0 {
        return 0;
    }
    previous.checked_add(2).unwrap_or(0)
}

/// Chooses the address to reach a peer on, from everything it advertised.
///
/// Discovery commonly reports a dozen addresses across bridge, loopback and link-local
/// interfaces. Taking the first would often pick one that cannot route to the peer at all.
pub fn choose_peer_address(addresses: &[IpAddr], port: u16) -> Option<SocketAddr> {
    let score = |address: &IpAddr| match address {
        // A routable address on a real interface is always the right answer.
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => 4,
        IpAddr::V6(v6) if !v6.is_loopback() && !is_link_local_v6(v6) => 3,
        // Link-local works within one segment, so it beats loopback.
        IpAddr::V4(v4) if !v4.is_loopback() => 2,
        IpAddr::V6(v6) if !v6.is_loopback() => 2,
        // Loopback only reaches this machine, which is almost never what was meant.
        _ => 1,
    };

    addresses
        .iter()
        .max_by_key(|address| score(address))
        .map(|address| SocketAddr::new(*address, port))
}

/// Reports whether an IPv6 address is link-local, which needs a scope to be usable.
fn is_link_local_v6(address: &Ipv6Addr) -> bool {
    address
        .segments()
        .first()
        .is_some_and(|first| (first & 0xFFC0) == 0xFE80)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// Proves which socket errors count as having no network, the distinction that lets a lost
    /// link blame the network rather than the peer (R-069). An unreachable network or host, or an
    /// address that went away with its interface, means no route; a peer refusing or a full
    /// buffer says nothing about whether there is a network.
    #[test]
    fn only_a_missing_route_counts_as_no_network() {
        let cases = [
            (io::ErrorKind::NetworkUnreachable, true),
            (io::ErrorKind::HostUnreachable, true),
            (io::ErrorKind::AddrNotAvailable, true),
            (io::ErrorKind::ConnectionRefused, false),
            (io::ErrorKind::WouldBlock, false),
        ];
        for (kind, want) in cases {
            let failure = NetError::Io {
                operation: "send",
                source: io::Error::from(kind),
            };
            assert_eq!(
                failure.is_no_route(),
                want,
                "{kind:?}: classified wrongly as having or lacking a network"
            );
        }
    }

    /// Asks for a pair in a shape a caller can request.
    #[derive(Debug, Clone, Copy)]
    enum Request {
        /// Port zero, sixteen times with every pair held, so the system chooses sixteen.
        SystemChoice,
        /// 65535, which is odd and has nothing above it.
        Highest,
        /// The control port of a pair another session already holds.
        Held,
    }

    /// Proves that however a pair is asked for, binding yields an even control port with the data
    /// port directly above it, as RTP-MIDI requires. An odd port the system chose moved the next
    /// time the session started, away from any peer that knew it; 65535 once kept binding on
    /// itself for every attempt and gave up; and a held port must fall back to another working
    /// pair rather than fail the session.
    #[tokio::test]
    async fn binding_always_yields_an_even_adjacent_pair() {
        for request in [Request::SystemChoice, Request::Highest, Request::Held] {
            let occupied = SessionSockets::bind(0)
                .await
                .expect("a first pair binds to be held");
            let (asked, times) = match request {
                Request::SystemChoice => (0, 16),
                Request::Highest => (u16::MAX, 1),
                Request::Held => (occupied.control_port(), 1),
            };
            let mut held = Vec::new();
            for _ in 0..times {
                let sockets = SessionSockets::bind(asked)
                    .await
                    .unwrap_or_else(|error| panic!("{request:?}: no pair was bound: {error}"));
                let control = sockets.control_port();
                assert!(
                    control != 0 && control.is_multiple_of(2),
                    "{request:?}: the control port {control} must be even and real"
                );
                assert_eq!(
                    sockets.data.local_addr().map(|a| a.port()).ok(),
                    control.checked_add(1),
                    "{request:?}: the data port must sit directly above the control port"
                );
                if matches!(request, Request::Held) {
                    assert_ne!(
                        control,
                        occupied.control_port(),
                        "a held pair must not be shared"
                    );
                }
                held.push(sockets);
            }
        }
    }

    /// Proves no other socket can bind either port of a bound pair, over IPv4 or IPv6. Windows let
    /// a plain socket bind the port of a dual-stack socket beside it, and take its datagrams,
    /// until the pair claimed its ports exclusively.
    #[tokio::test]
    async fn no_other_socket_can_take_a_bound_pair() {
        let sockets = SessionSockets::bind(0)
            .await
            .expect("a pair binds on a system-chosen port");
        for port in [sockets.control_port(), sockets.control_port() + 1] {
            assert!(
                std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_err(),
                "an IPv4 socket bound {port}, and would take the session's datagrams"
            );
            assert!(
                std::net::UdpSocket::bind((Ipv6Addr::UNSPECIFIED, port)).is_err(),
                "an IPv6 socket bound {port}, and would take the session's datagrams"
            );
        }
    }

    /// Sends a datagram over IPv4 to each of the pair's ports and checks the pair receives it.
    async fn assert_reachable_over_ipv4(sockets: &SessionSockets, held: Port) {
        let control_port = sockets.control_port();
        let sender = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a sender binds on loopback");

        for (port, target) in [
            (Port::Control, control_port),
            (Port::Data, control_port + 1),
        ] {
            let to = SocketAddr::from((Ipv4Addr::LOCALHOST, target));
            sender
                .send_to(b"hello", to)
                .await
                .expect("the datagram is sent");

            let received = tokio::time::timeout(std::time::Duration::from_secs(2), sockets.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "with the {held:?} port held over IPv4, nothing arrived on the {port:?} port {target}"
                    )
                })
                .expect("a datagram is received");
            assert_eq!(
                (received.port, received.bytes.as_slice()),
                (port, b"hello".as_slice()),
                "with the {held:?} port held over IPv4, the datagram arrived on the wrong port"
            );
        }
    }

    /// Proves a pair is never bound on a port an IPv4-only socket already holds. macOS lets a
    /// dual-stack socket bind such a port while the IPv4 socket goes on receiving everything sent
    /// to it over IPv4, so a session given that port never heard its peer.
    #[tokio::test]
    async fn a_port_held_over_ipv4_is_not_shared() {
        for held in [Port::Control, Port::Data] {
            // Hold the control or the data port of some even pair, over IPv4 only.
            let (holder, pair) = loop {
                let holder = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                    .expect("a system-chosen IPv4 port binds");
                let port = holder
                    .local_addr()
                    .expect("a bound socket has an address")
                    .port();
                match (held, port % 2) {
                    (Port::Control, 0) => break (holder, port),
                    (Port::Data, 1) => break (holder, port - 1),
                    _ => continue,
                }
            };

            let sockets = SessionSockets::bind(pair)
                .await
                .expect("a pair binds beside the held port");
            assert_reachable_over_ipv4(&sockets, held).await;
            drop(holder);
        }
    }

    /// Returns an even port that is free over IPv4, with the one above it free too.
    fn free_pair() -> u16 {
        loop {
            let probe = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                .expect("a system-chosen IPv4 port binds");
            let port = probe
                .local_addr()
                .expect("a bound socket has an address")
                .port();
            drop(probe);
            if port.is_multiple_of(2)
                && port < u16::MAX
                && std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port + 1)).is_ok()
            {
                return port;
            }
        }
    }

    /// Proves a port held only for a moment is waited for rather than abandoned. A child process
    /// holds a copy of every socket until it has started, so a session switched off and on while
    /// the daemon started a program found its own port held, moved to another, and left its peers
    /// retrying the old one.
    #[tokio::test]
    async fn a_port_held_for_a_moment_is_waited_for() {
        let pair = free_pair();
        let holder = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, pair))
            .expect("the free port binds to be held");
        let released = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            drop(holder);
        });

        let sockets = SessionSockets::bind(pair)
            .await
            .expect("the pair binds once released");
        assert_eq!(
            sockets.control_port(),
            pair,
            "the session moved off its port instead of waiting for the brief holder"
        );
        released.await.expect("the holder is released");
    }

    /// Proves a data port held over IPv6 leaves no IPv4-only socket in the pair. The claim over
    /// IPv4 succeeds beside an IPv6 socket on the loopback address and the dual-stack bind then
    /// fails; falling back to IPv4 alone put an IPv4 data socket beside a dual-stack control
    /// socket, which refused every send to an IPv4 peer.
    #[tokio::test]
    async fn a_port_held_over_ipv6_leaves_no_ipv4_only_socket_in_the_pair() {
        let pair = free_pair();
        let _holder = std::net::UdpSocket::bind((Ipv6Addr::LOCALHOST, pair + 1))
            .expect("the data port binds over IPv6 to be held");

        let sockets = SessionSockets::bind(pair)
            .await
            .expect("a pair binds beside the IPv6 holder");
        let peer = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("an IPv4 peer binds on loopback");
        let to = peer.local_addr().expect("a bound socket has an address");
        sockets
            .send(Port::Data, to, b"hello")
            .await
            .expect("the data port sends to an IPv4 peer");
        let mut buffer = [0u8; 16];
        let received = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            peer.recv_from(&mut buffer),
        )
        .await
        .expect("the datagram arrives within two seconds")
        .expect("a datagram is received");
        assert_eq!(
            buffer.get(..received.0),
            Some(b"hello".as_slice()),
            "the IPv4 peer must receive exactly what the data port sent"
        );
    }

    /// Proves which advertised address a peer is reached at. Discovery reports many addresses and
    /// the first is often one that cannot route, so a routable address beats link-local, which
    /// works within one segment and so beats loopback, which only reaches this machine; an
    /// IPv6-only peer is still reachable.
    #[test]
    fn a_peer_is_reached_at_its_most_routable_address() {
        let routable = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 4));
        let link_local = IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1));
        let global_v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        let cases = [
            (
                "routable beats loopback and link-local",
                vec![
                    IpAddr::V6(Ipv6Addr::LOCALHOST),
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    link_local,
                    routable,
                ],
                Some(routable),
            ),
            (
                "link-local beats loopback",
                vec![IpAddr::V4(Ipv4Addr::LOCALHOST), link_local],
                Some(link_local),
            ),
            (
                "an IPv6-only peer is reachable",
                vec![global_v6],
                Some(global_v6),
            ),
            ("no address reaches nothing", vec![], None),
        ];
        for (name, addresses, want) in cases {
            assert_eq!(
                choose_peer_address(&addresses, 5004),
                want.map(|address| SocketAddr::new(address, 5004)),
                "{name}: the wrong address was chosen"
            );
        }
    }
}
