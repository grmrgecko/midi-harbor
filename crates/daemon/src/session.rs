//! The running half of a network session.
//!
//! The state machine in `midi-harbor-rtpmidi` decides what should happen; this drives it. It owns
//! the sockets, feeds the machine datagrams and time, carries out the actions it returns, and
//! reports what it sees into the connection state the rest of the daemon reads.
//!
//! The resilience rules live here. A session that fails is retried with backoff forever while the
//! user has it enabled, and every failure silences the endpoint before anything else, so a peer
//! that vanishes mid-phrase cannot leave a note sounding.

use crate::identity::{Identity, PortIdentity};
use crate::net::{Datagram, NetError, SessionSockets};
use midi_harbor_core::backoff::BackoffPolicy;
use midi_harbor_core::controls::Controls;
use midi_harbor_core::endpoint::{InvitationDecision, InvitationPolicy};
use midi_harbor_core::failure::FailureReason;
use midi_harbor_core::ids::EndpointId;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::state::{ConnectionPhase, ConnectionState, Effect, Event};
use midi_harbor_core::time::{Clock, SystemClock};
use midi_harbor_rtpmidi::clock::ticks_from;
use midi_harbor_rtpmidi::identity::NONCE_LEN;
use midi_harbor_rtpmidi::session::{Action, Port, Role, Session, SessionFailure};
use midi_harbor_rtpmidi::{ControlPacket, IdentityError, IdentityPacket, RtpMidiPacket};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, error, info, warn};

/// How often the session machine is given a chance to act on elapsed time.
///
/// Fast enough that a due clock exchange is not noticeably late, slow enough to cost nothing on
/// an idle session.
pub const TICK_INTERVAL: Duration = Duration::from_millis(250);

/// How long before the same address is told again that its session does not exist here.
///
/// A peer sends a clock exchange every few seconds and MIDI far more often, and one goodbye a
/// second is enough for it to hear.
pub const GOODBYE_INTERVAL: Duration = Duration::from_secs(1);

/// How long a machine has to prove which network port it is.
///
/// A Midi Harbor on the same network answers in a millisecond or two. Long enough for a
/// challenge lost on the way to be sent again, short enough that a machine that will never
/// answer does not hold up following the others.
pub const PROOF_WAIT: Duration = Duration::from_secs(2);

/// How long before an unanswered challenge is sent again.
const CHALLENGE_INTERVAL: Duration = Duration::from_millis(500);

/// How long a session that ended itself ahead of sleep waits, awake, before reconnecting
/// without being told the machine woke.
///
/// The wake notification usually reconnects it at once, but recovery must not depend on one.
/// Measured on `Instant`, which stands still while the machine sleeps on macOS and Linux, so this
/// counts only time spent awake. Windows does not promise its `Instant` stops; if it counts
/// through a long sleep, the wait is over on waking and the session reconnects then, which is what
/// the wait exists for. That includes the time between the notice of sleep and
/// sleep itself: macOS lingered five seconds, and a slow Bluetooth acknowledgement another one
/// and a half, so at five seconds the session reconnected as the machine went down, and the
/// peer's invitation woke it half a minute later (R-070).
pub const RESUME_AFTER_SLEEP: Duration = Duration::from_secs(60);

/// What the supervisor can be asked to do.
#[derive(Debug)]
pub enum Command {
    /// Connect to a peer, replacing any current connection.
    Connect(SocketAddr),
    /// Disconnect, leaving the session configured but idle.
    Disconnect,
    /// Send messages to the peer.
    Send(Vec<MidiMessage>),
    /// Send one whole system-exclusive message to the peer.
    SendSysEx(Arc<[u8]>),
    /// Conditions changed, so try again now rather than waiting out the backoff.
    Nudge,
    /// The machine is about to sleep: end the session, and answer once the peer has been told.
    Suspend(oneshot::Sender<()>),
    /// Replace how invitations are treated, and which peers are already trusted.
    Configure {
        /// What to do about an invitation from a peer.
        policy: InvitationPolicy,
        /// The addresses of peers the user has already accepted.
        trusted: Vec<IpAddr>,
    },
    /// Invite a machine to carry MIDI beside the peer, or connect to it when there is none,
    /// answering which of the two it became.
    Invite(SocketAddr, oneshot::Sender<Place>),
    /// End one machine's part, answering whether it was taking part.
    DisconnectMachine(SocketAddr, oneshot::Sender<bool>),
    /// Connect to a machine this side connected to at a new address in place of the old one,
    /// unless it is carrying MIDI, answering whether it was moved.
    Move {
        /// Where it was connected to.
        from: SocketAddr,
        /// Where it is connected to from now on.
        to: SocketAddr,
        /// Answered with whether it moved.
        done: oneshot::Sender<bool>,
    },
    /// Take the key to answer challenges with, and the identifier of the network port this is.
    Identify {
        /// The daemon's key.
        identity: Arc<Identity>,
        /// The network port's identifier.
        port: EndpointId,
    },
    /// Ask the port listening at an address to prove it is `expected`, answering whether it did.
    Prove {
        /// The control address to ask.
        at: SocketAddr,
        /// The port it must prove it is.
        expected: PortIdentity,
        /// Answered with whether it proved it in time.
        done: oneshot::Sender<bool>,
    },
    /// Take a new name, told to machines from the next invitation on. A machine already
    /// connected keeps the name it was told, as Apple's sessions do.
    Rename(String),
    /// Stop the supervisor entirely.
    Shutdown,
}

/// The place a machine invited into a session takes in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// The session's peer, because it had none.
    Peer,
    /// Carried beside the peer.
    Beside,
}

/// What became of a datagram from a machine other than the session's peer.
enum Admission {
    /// It was an invitation, and the machine is now a guest.
    Admitted,
    /// It was an invitation, refused or held for the user.
    Answered,
    /// It was not an invitation.
    NotAnInvitation,
}

/// MIDI arriving from the peer, in the order it was sent.
///
/// One channel carries both, so a note sent after a dump cannot overtake it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    /// Channel, system common and real-time messages.
    Messages(Vec<MidiMessage>),
    /// One whole system-exclusive message, framing included.
    SysEx(Vec<u8>),
}

/// Something a session tells the daemon, for its history and for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionNotice {
    /// A peer asked to connect and is waiting for the user.
    Invitation(InvitationNotice),
    /// The session is carrying MIDI with a peer.
    Connected {
        /// The peer, by the name it advertises or its address.
        peer: String,
        /// How many attempts failed before this one.
        attempts: u32,
    },
    /// The peer ended a session that was carrying MIDI, rather than going quiet.
    Left {
        /// The peer that left.
        peer: String,
        /// Whether this side is inviting it back, which it does only for a peer it invited.
        reconnecting: bool,
    },
    /// A machine let in beside the session's peer ended its part, or stopped answering.
    GuestLeft {
        /// The machine that left.
        peer: String,
    },
    /// A session that was carrying MIDI stopped.
    Lost {
        /// The peer it was connected to.
        peer: String,
        /// Why.
        reason: FailureReason,
    },
    /// The first attempt of a streak failed. Later ones fail the same way and are not repeated.
    CouldNotConnect {
        /// The peer it tried.
        peer: String,
        /// Why.
        reason: FailureReason,
    },
}

/// An invitation that arrived and needs a decision before the peer can be let in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvitationNotice {
    /// The session the peer invited.
    pub session: String,
    /// Where the invitation came from, on the peer's control port.
    pub peer: SocketAddr,
    /// The name the peer advertises, when it sent one.
    pub peer_name: Option<String>,
}

/// What the supervisor currently observes about its session.
#[derive(Debug, Clone)]
pub struct SessionStatus {
    /// Where the connection sits in its lifecycle.
    pub state: ConnectionState,
    /// The peer's advertised name, once known.
    pub peer_name: Option<String>,
    /// The address currently in use.
    pub peer_address: Option<SocketAddr>,
    /// The control port this session listens on.
    pub control_port: u16,
    /// The most recent round-trip measurement.
    pub round_trip: Duration,
    /// How many packets the network lost.
    pub lost: u64,
    /// How many messages the journal rebuilt.
    pub recovered: u64,
    /// Whether the last send found no route at all, meaning this machine has no network to reach
    /// the peer by. Cleared by the next send that goes out.
    pub waiting_for_network: bool,
    /// The machines carried beside the peer, by name or address.
    pub guests: Vec<String>,
    /// Every machine taking part, the peer first (FR-015i).
    pub machines: Vec<Machine>,
}

/// One machine taking part in a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    /// Its control address.
    pub address: SocketAddr,
    /// The name it advertises, once known.
    pub name: Option<String>,
    /// Whether this side connected to it, rather than it to this side.
    pub invited: bool,
    /// Whether it has finished joining and is carrying MIDI.
    pub joined: bool,
    /// The most recent round trip to it, once one has been measured.
    pub round_trip: Option<Duration>,
}

/// A running network session, and the means to talk to it.
pub struct NetworkSession {
    commands: mpsc::Sender<Command>,
    status: Arc<Mutex<SessionStatus>>,
    control_port: u16,
    /// The supervisor's task, awaited on shutdown so its sockets are closed by the time it
    /// returns.
    running: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl NetworkSession {
    /// Starts a supervisor for a session, binding its port pair.
    ///
    /// `deliver` receives MIDI arriving from the peer, including anything the journal rebuilds.
    pub async fn start(
        name: String,
        requested_port: u16,
        deliver: mpsc::Sender<Inbound>,
        policy: InvitationPolicy,
        notices: Option<mpsc::Sender<SessionNotice>>,
    ) -> Result<Self, crate::net::NetError> {
        let sockets = SessionSockets::bind(requested_port).await?;
        let control_port = sockets.control_port();

        let clock = SystemClock;
        let status = Arc::new(Mutex::new(SessionStatus {
            state: ConnectionState::with_policy(
                ConnectionPhase::Disconnected,
                clock.now(),
                BackoffPolicy::responsive(),
            ),
            peer_name: None,
            peer_address: None,
            control_port,
            round_trip: Duration::ZERO,
            lost: 0,
            recovered: 0,
            waiting_for_network: false,
            guests: Vec::new(),
            machines: Vec::new(),
        }));

        let (commands, inbox) = mpsc::channel(32);
        let supervisor = Supervisor {
            name,
            sockets,
            deliver,
            status: Arc::clone(&status),
            clock,
            session: None,
            peer: None,
            started: Instant::now(),
            policy,
            trusted: Vec::new(),
            notices,
            last_goodbye: None,
            asleep: None,
            asleep_guests: Vec::new(),
            controls: Controls::new(),
            restore_due: false,
            guests: HashMap::new(),
            peer_invited: false,
            invited_guests: HashMap::new(),
            joined_guests: HashSet::new(),
            identity: None,
            proving: Vec::new(),
        };
        let running = tokio::spawn(supervisor.run(inbox));

        Ok(Self {
            commands,
            status,
            control_port,
            running: Mutex::new(Some(running)),
        })
    }

    /// Returns the control port this session listens on.
    pub fn control_port(&self) -> u16 {
        self.control_port
    }

    /// Returns what the supervisor currently observes.
    pub async fn status(&self) -> SessionStatus {
        self.status.lock().await.clone()
    }

    /// Asks the supervisor to connect to a peer.
    pub async fn connect(&self, peer: SocketAddr) -> bool {
        self.commands.send(Command::Connect(peer)).await.is_ok()
    }

    /// Asks the supervisor to disconnect.
    pub async fn disconnect(&self) -> bool {
        self.commands.send(Command::Disconnect).await.is_ok()
    }

    /// Sends messages to the peer.
    pub async fn send(&self, messages: Vec<MidiMessage>) -> bool {
        self.commands.send(Command::Send(messages)).await.is_ok()
    }

    /// Sends one whole system-exclusive message to the peer, framing included.
    pub async fn send_sysex(&self, dump: Arc<[u8]>) -> bool {
        self.commands.send(Command::SendSysEx(dump)).await.is_ok()
    }

    /// Replaces how this session treats invitations, and which peers are already trusted.
    pub async fn configure(&self, policy: InvitationPolicy, trusted: Vec<IpAddr>) -> bool {
        self.commands
            .send(Command::Configure { policy, trusted })
            .await
            .is_ok()
    }

    /// Invites a machine to carry MIDI beside the peer, or connects to it when there is none,
    /// returning the place it took, or `None` when the supervisor has stopped.
    ///
    /// A machine invited beside the peer is chased as the peer is: invited again with backoff when
    /// its link is lost, until it is disconnected. The place is the supervisor's answer rather
    /// than read from the status, which lags the commands queued ahead of this one: with a
    /// connect sent just before, the status still showed no peer, and the machine was
    /// remembered as the peer in place of the one connected.
    pub async fn invite(&self, machine: SocketAddr) -> Option<Place> {
        let (done, answer) = oneshot::channel();
        self.commands
            .send(Command::Invite(machine, done))
            .await
            .ok()?;
        answer.await.ok()
    }

    /// Ends one machine's part, leaving the others connected, and reports whether it was
    /// taking part.
    pub async fn disconnect_machine(&self, machine: SocketAddr) -> bool {
        let (done, answer) = oneshot::channel();
        if self
            .commands
            .send(Command::DisconnectMachine(machine, done))
            .await
            .is_err()
        {
            return false;
        }
        answer.await.unwrap_or(false)
    }

    /// Connects to a machine this side connected to at `to` in place of `from`, and reports
    /// whether it did.
    ///
    /// A machine carrying MIDI is left where it is, and so is one the session does not connect
    /// to. The supervisor decides, since only it knows which of its links are up.
    pub async fn move_machine(&self, from: SocketAddr, to: SocketAddr) -> bool {
        let (done, answer) = oneshot::channel();
        if self
            .commands
            .send(Command::Move { from, to, done })
            .await
            .is_err()
        {
            return false;
        }
        answer.await.unwrap_or(false)
    }

    /// Gives the session the key it answers challenges with, and says which network port it is.
    pub async fn identify(&self, identity: Arc<Identity>, port: EndpointId) -> bool {
        self.commands
            .send(Command::Identify { identity, port })
            .await
            .is_ok()
    }

    /// Asks the port listening at `at` to prove it is `expected`, and reports whether it did
    /// within `PROOF_WAIT`.
    ///
    /// Sent only to a session that advertises a key. No other implementation knows the packet.
    pub async fn prove(&self, at: SocketAddr, expected: PortIdentity) -> bool {
        let (done, answer) = oneshot::channel();
        if self
            .commands
            .send(Command::Prove { at, expected, done })
            .await
            .is_err()
        {
            return false;
        }
        answer.await.unwrap_or(false)
    }

    /// Gives the session a new name for the machines it invites or accepts from now on.
    pub async fn rename(&self, name: String) -> bool {
        self.commands.send(Command::Rename(name)).await.is_ok()
    }

    /// Tells the supervisor that conditions changed and a retry is worth attempting now.
    ///
    /// Never required for recovery: the backoff gets there on its own. It is the difference
    /// between resuming when the lid opens and resuming up to half a minute later.
    pub async fn nudge(&self) -> bool {
        self.commands.send(Command::Nudge).await.is_ok()
    }

    /// Ends the session ahead of the machine sleeping, returning once the peer has been told.
    ///
    /// A peer left to find out for itself invites again when its liveness check gives up, and
    /// each invitation woke a sleeping Mac on mains power (R-070). A session that made the
    /// connection makes it again on waking.
    pub async fn suspend(&self) {
        let (done, finished) = oneshot::channel();
        if self.commands.send(Command::Suspend(done)).await.is_ok() {
            let _ = finished.await;
        }
    }

    /// Stops the supervisor, returning once its ports are free.
    ///
    /// Waits for the task rather than only asking it to stop. A session switched straight back
    /// on otherwise found its own port still held and bound another, moving away from every
    /// peer that knew the first.
    pub async fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown).await;
        if let Some(running) = self.running.lock().await.take() {
            let _ = running.await;
        }
    }
}

/// Owns the sockets and drives the session machine.
struct Supervisor {
    name: String,
    sockets: SessionSockets,
    deliver: mpsc::Sender<Inbound>,
    status: Arc<Mutex<SessionStatus>>,
    clock: SystemClock,
    session: Option<Session>,
    peer: Option<SocketAddr>,
    started: Instant,
    /// What to do about an invitation that arrives.
    policy: InvitationPolicy,
    /// Peers the user has already accepted, by address.
    ///
    /// Matched on the address rather than the port: what a user trusts is a machine, and the
    /// port a session listens on is not something they chose or can recognise.
    trusted: Vec<IpAddr>,
    /// Where an invitation goes when only the user can answer it.
    notices: Option<mpsc::Sender<SessionNotice>>,
    /// The address last told that its session does not exist here, and when.
    last_goodbye: Option<(SocketAddr, Instant)>,
    /// The peer to reconnect to after sleep, and when the session ended for it.
    asleep: Option<(SocketAddr, Instant)>,
    /// The machines this side invited beside the peer, invited again when the session resumes
    /// after sleep.
    asleep_guests: Vec<SocketAddr>,
    /// The controller state this session has been sent, kept across attempts so a recovered link
    /// can be brought back to it (FR-027).
    controls: Controls,
    /// Set when the connection state asks for that restoration, until it is sent.
    restore_due: bool,
    /// Machines carried beside the peer, by control address, while each has a session running.
    ///
    /// Each runs a session machine of its own and carries the same MIDI. One that invited itself
    /// in is let go rather than chased when it leaves (the simultaneous-invitations edge case);
    /// one this side invited is chased as the peer is.
    guests: HashMap<SocketAddr, Session>,
    /// Whether this side connected to the peer, rather than the peer to this side.
    peer_invited: bool,
    /// The guests this side invited, rather than ones that invited it, each with the lifecycle
    /// that decides when it is invited again.
    ///
    /// Kept while its link is down, so it is retried with backoff independently of the peer, as
    /// the peer is, until the user disconnects it (FR-015i).
    invited_guests: HashMap<SocketAddr, ConnectionState>,
    /// The guests that have finished joining.
    joined_guests: HashSet<SocketAddr>,
    /// The key challenges are answered with, and which network port this is.
    identity: Option<(Arc<Identity>, EndpointId)>,
    /// The challenges sent and not yet answered.
    proving: Vec<Challenge>,
}

/// A challenge sent to a port, waiting for its proof.
struct Challenge {
    /// The control address asked.
    at: SocketAddr,
    /// What was sent, which the proof must echo.
    nonce: [u8; NONCE_LEN],
    /// The port it must prove it is.
    expected: PortIdentity,
    /// When it was first sent.
    asked: Instant,
    /// When it was last sent.
    sent: Instant,
    /// Answered with whether it was proved.
    done: oneshot::Sender<bool>,
}

impl Supervisor {
    /// Runs until asked to stop.
    async fn run(mut self, mut inbox: mpsc::Receiver<Command>) {
        let mut ticker = tokio::time::interval(TICK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                command = inbox.recv() => {
                    match command {
                        Some(Command::Shutdown) | None => {
                            self.close().await;
                            return;
                        }
                        Some(command) => self.on_command(command).await,
                    }
                }
                received = self.sockets.recv() => {
                    match received {
                        Ok(datagram) => self.on_datagram(datagram).await,
                        // A receive error on a bound socket is worth reporting but not worth
                        // tearing the session down for; the next datagram may arrive fine.
                        Err(error) => debug!(error = %error, "session receive failed"),
                    }
                }
                _ = ticker.tick() => self.on_tick().await,
            }
        }
    }

    /// Returns the current time in the wire's hundred-microsecond units.
    fn ticks(&self) -> u64 {
        ticks_from(self.started.elapsed())
    }

    /// Handles a command from the daemon.
    async fn on_command(&mut self, command: Command) {
        match command {
            Command::Invite(machine, done) => {
                let place = self.invite(machine).await;
                let _ = done.send(place);
            }
            Command::DisconnectMachine(machine, done) => {
                let found = self.disconnect_machine(machine).await;
                let _ = done.send(found);
            }
            Command::Move { from, to, done } => {
                let moved = self.move_machine(from, to).await;
                let _ = done.send(moved);
            }
            Command::Connect(peer) => {
                self.connect(peer).await;
                // Connecting ends any wait for sleep to pass, so the machines invited beside the
                // peer before it are invited again with it.
                self.wake_guests().await;
            }
            Command::Disconnect => {
                info!(session = %self.name, "disconnecting network session");
                self.asleep = None;
                self.asleep_guests.clear();
                self.close().await;
                self.listen().await;
            }
            Command::Send(messages) => {
                for message in &messages {
                    self.controls.record(message);
                }
                let ticks = self.ticks();
                if let Some(session) = &mut self.session {
                    let actions = session.send(&messages, ticks);
                    self.carry_out(actions).await;
                }
                for guest in self.guest_addresses() {
                    let ticks = self.ticks();
                    if let Some(session) = self.guests.get_mut(&guest) {
                        let actions = session.send(&messages, ticks);
                        self.carry_out_guest(guest, actions).await;
                    }
                }
            }
            Command::SendSysEx(dump) => {
                let ticks = self.ticks();
                if let Some(session) = &mut self.session {
                    let actions = session.send_sysex(&dump, ticks);
                    self.carry_out(actions).await;
                }
                for guest in self.guest_addresses() {
                    let ticks = self.ticks();
                    if let Some(session) = self.guests.get_mut(&guest) {
                        let actions = session.send_sysex(&dump, ticks);
                        self.carry_out_guest(guest, actions).await;
                    }
                }
            }
            Command::Suspend(done) => {
                self.suspend().await;
                let _ = done.send(());
            }
            Command::Nudge => {
                if let Some((peer, _)) = self.asleep.take() {
                    self.resume(peer).await;
                    return;
                }
                self.retry_guests(true).await;
                let phase = self.status.lock().await.state.phase();
                match phase {
                    // A session that believes it is connected is the dangerous case: after a
                    // suspend it looks exactly like a working one, and the liveness timeout takes
                    // thirty-five seconds to disagree. Ask the peer now instead of assuming.
                    ConnectionPhase::Connected => {
                        let ticks = self.ticks();
                        if let Some(session) = &mut self.session {
                            debug!(session = %self.name, "checking the link after a machine-level change");
                            let actions = session.probe(ticks);
                            self.carry_out(actions).await;
                        }
                    }
                    ConnectionPhase::Retrying | ConnectionPhase::Unavailable => {
                        if self.peer.is_none() {
                            return;
                        }
                        debug!(session = %self.name, "retrying after a machine-level change");
                        self.apply_event(Event::Nudge).await;
                        self.begin_attempt().await;
                    }
                    // Switched off, or already attempting; neither has anything to gain.
                    _ => {}
                }
            }
            Command::Configure { policy, trusted } => {
                self.policy = policy;
                self.trusted = trusted;
            }
            Command::Identify { identity, port } => self.identity = Some((identity, port)),
            Command::Prove { at, expected, done } => {
                let now = Instant::now();
                let challenge = Challenge {
                    at: canonical(at),
                    nonce: rand::random(),
                    expected,
                    asked: now,
                    sent: now,
                    done,
                };
                self.send_challenge(challenge.at, challenge.nonce).await;
                self.proving.push(challenge);
            }
            Command::Rename(name) => {
                info!(session = %self.name, to = %name, "network session renamed");
                self.name = name;
            }
            Command::Shutdown => self.close().await,
        }
    }

    /// Starts a connection attempt.
    async fn begin_attempt(&mut self) {
        let Some(peer) = self.peer else {
            return;
        };
        let ticks = self.ticks();

        // A fresh session per attempt, so nothing carries over from a failed one.
        let mut session = Session::initiator(
            rand::random::<u32>(),
            rand::random::<u32>(),
            self.name.clone(),
        );
        let actions = session.start(ticks);
        self.session = Some(session);

        self.apply_event(Event::Attempting).await;
        {
            let mut status = self.status.lock().await;
            status.peer_address = Some(peer);
        }
        self.carry_out(actions).await;
    }

    /// Sends a challenge to the port listening at `at`.
    async fn send_challenge(&self, at: SocketAddr, nonce: [u8; NONCE_LEN]) {
        let packet = IdentityPacket::Challenge { nonce };
        self.send_to(Port::Control, at, &packet.encode()).await;
    }

    /// Sends unanswered challenges again, and gives up on those out of time.
    async fn tend_challenges(&mut self) {
        let now = Instant::now();
        let (waiting, expired): (Vec<Challenge>, Vec<Challenge>) =
            std::mem::take(&mut self.proving)
                .into_iter()
                .partition(|challenge| now.saturating_duration_since(challenge.asked) < PROOF_WAIT);
        for challenge in expired {
            debug!(session = %self.name, at = %challenge.at, "no proof of which port this is");
            let _ = challenge.done.send(false);
        }
        self.proving = waiting;
        let mut again = Vec::new();
        for challenge in &mut self.proving {
            if now.saturating_duration_since(challenge.sent) >= CHALLENGE_INTERVAL {
                challenge.sent = now;
                again.push((challenge.at, challenge.nonce));
            }
        }
        for (at, nonce) in again {
            self.send_challenge(at, nonce).await;
        }
    }

    /// Answers a challenge, or takes a proof for one this side sent.
    async fn on_identity(&mut self, packet: IdentityPacket, from: SocketAddr) {
        match packet {
            IdentityPacket::Challenge { nonce } => {
                let Some((identity, port)) = &self.identity else {
                    return;
                };
                let proof = IdentityPacket::Proof {
                    nonce,
                    key: identity.public_key(),
                    port_id: port.to_bytes(),
                    signature: identity.prove(&nonce, from, *port),
                };
                self.send_to(Port::Control, from, &proof.encode()).await;
            }
            IdentityPacket::Proof {
                nonce,
                key,
                port_id,
                signature,
            } => {
                let Some(index) = self
                    .proving
                    .iter()
                    .position(|challenge| challenge.at == from && challenge.nonce == nonce)
                else {
                    return;
                };
                // The answer must be the port asked for, signed for a challenge from this
                // port: sent from one of this machine's addresses, on this control port. A
                // wrong answer is ignored rather than ending the wait, so a forged one cannot
                // spoil a real one on its way.
                let claimed = PortIdentity {
                    key,
                    port: EndpointId::from_bytes(port_id),
                };
                let control_port = self.sockets.control_port();
                let proved = self.proving.get(index).is_some_and(|challenge| {
                    claimed == challenge.expected
                        && crate::discovery::local_addresses().into_iter().any(|own| {
                            crate::identity::verifies(
                                &claimed,
                                &nonce,
                                SocketAddr::new(own, control_port),
                                &signature,
                            )
                        })
                });
                if proved {
                    let challenge = self.proving.swap_remove(index);
                    let _ = challenge.done.send(true);
                }
            }
        }
    }

    /// Handles a datagram arriving on either port.
    async fn on_datagram(&mut self, datagram: Datagram) {
        // The identity exchange stands apart from any session: a port is asked which it is
        // whether or not it is connected to the asker.
        if datagram.port == Port::Control {
            match IdentityPacket::parse(&datagram.bytes) {
                Ok(packet) => {
                    self.on_identity(packet, canonical(datagram.from)).await;
                    return;
                }
                Err(IdentityError::NotIdentity) => {}
                Err(error) => {
                    debug!(from = %datagram.from, error = %error, "discarding an identity packet");
                    return;
                }
            }
        }

        // A guest's traffic goes to the guest's own session machine, never the peer's.
        let from = control_address(&datagram);
        if self.guests.contains_key(&from) {
            self.feed_guest(from, &datagram).await;
            return;
        }

        // Anything else from a machine other than the peer is never fed to the peer's machine:
        // an outsider's goodbye ended the session, and its invitation took over the peer's SSRC
        // while the acceptance went to the peer. An invitation the policy allows lets the machine
        // in as a guest.
        if self.session.is_some() && !self.is_from_peer(&datagram) {
            match self.admit_guest(&datagram).await {
                Admission::Admitted => self.feed_guest(from, &datagram).await,
                Admission::Answered => {}
                Admission::NotAnInvitation => self.answer_outsider(&datagram).await,
            }
            return;
        }

        if self.session.is_some() {
            self.feed(&datagram).await;
        }

        // A datagram arriving with no session is a peer inviting us. Ignoring it would make this
        // side unreachable: other machines could see the advertisement and never connect. It may
        // also be the invitation that just ended the session, from a peer that restarted.
        if self.session.is_none() {
            let ticks = self.ticks();
            self.adopt_invitation(&datagram, ticks).await;
            if self.session.is_some() {
                self.feed(&datagram).await;
            } else if is_session_traffic(&datagram.bytes) {
                self.say_goodbye(&datagram).await;
            }
        }
    }

    /// Reports whether a datagram came from the session's peer, on the port it should use.
    ///
    /// The whole address is compared, not only the machine, because one machine can run several
    /// sessions, and each is a different peer.
    fn is_from_peer(&self, datagram: &Datagram) -> bool {
        self.peer.is_some_and(|peer| {
            peer.ip().to_canonical() == datagram.from.ip().to_canonical()
                && peer.port() == control_address(datagram).port()
        })
    }

    /// Answers a datagram from a machine other than the session's peer.
    async fn answer_outsider(&mut self, datagram: &Datagram) {
        if let Ok(ControlPacket::Session {
            command: midi_harbor_rtpmidi::Handshake::Invitation,
            token,
            ssrc,
            ..
        }) = ControlPacket::parse(&datagram.bytes)
        {
            // Refused rather than held: the session has a peer, and the user cannot let a second
            // one in whatever they answer. The machine can try again once this one is over.
            let peer = control_address(datagram);
            info!(session = %self.name, %peer, "refusing an invitation; the session already has a peer");
            self.reply(datagram.port, peer, &ControlPacket::rejected(token, ssrc))
                .await;
        } else if is_session_traffic(&datagram.bytes) {
            self.say_goodbye(datagram).await;
        }
    }

    /// Tells a machine that the session it is sending to does not exist here.
    ///
    /// A peer still sending clock exchanges or MIDI believes a session is running, typically
    /// because this side restarted or gave up on it. Told so, a Midi Harbor peer reconnects at
    /// once instead of waiting out the thirty-five second liveness timeout. At most one goodbye
    /// goes to an address per `GOODBYE_INTERVAL`, however much it sends.
    async fn say_goodbye(&mut self, datagram: &Datagram) {
        let now = Instant::now();
        if !goodbye_due(self.last_goodbye, datagram.from, now) {
            return;
        }
        self.last_goodbye = Some((datagram.from, now));

        let peer = control_address(datagram);
        debug!(session = %self.name, %peer, "ending a session this side does not have");
        let goodbye = ControlPacket::end_session(rand::random::<u32>(), rand::random::<u32>());
        self.reply(Port::Data, peer, &goodbye).await;
        self.reply(Port::Control, peer, &goodbye).await;
    }

    /// Feeds a datagram from the peer to the session machine.
    async fn feed(&mut self, datagram: &Datagram) {
        let ticks = self.ticks();
        let Some(session) = &mut self.session else {
            return;
        };

        // Control and RTP share the port pair, so which one this is decides how it is read.
        let actions = match ControlPacket::parse(&datagram.bytes) {
            Ok(packet) => session.on_control(datagram.port, &packet, ticks),
            Err(_) => match RtpMidiPacket::parse(&datagram.bytes) {
                Ok(packet) => session.on_rtp(&packet),
                Err(error) => {
                    debug!(from = %datagram.from, error = %error, "discarding unreadable datagram");
                    Vec::new()
                }
            },
        };
        self.carry_out(actions).await;
    }

    /// Starts a responder session for a peer that invited us.
    ///
    /// The peer's address is taken from the datagram rather than from configuration, because an
    /// inbound session is with whoever arrived and we have nothing else to go on.
    async fn adopt_invitation(&mut self, datagram: &Datagram, ticks: u64) {
        let Ok(packet) = ControlPacket::parse(&datagram.bytes) else {
            return;
        };
        let ControlPacket::Session {
            command: midi_harbor_rtpmidi::Handshake::Invitation,
            token,
            ssrc,
            ref name,
        } = packet
        else {
            return;
        };

        let peer = control_address(datagram);
        if !self.admits(datagram, token, ssrc, name).await {
            return;
        }
        info!(session = %self.name, %peer, "accepting an invitation from a peer");

        self.peer = Some(peer);
        self.peer_invited = false;
        self.session = Some(Session::responder(
            rand::random::<u32>(),
            rand::random::<u32>(),
            self.name.clone(),
        ));
        let _ = ticks;

        // A session left listening after a disconnect is switched off in its state, which would
        // ignore this attempt and report it switched off while it carries MIDI.
        self.apply_event(Event::Enable).await;
        self.apply_event(Event::Attempting).await;
        let mut status = self.status.lock().await;
        status.peer_address = Some(peer);
    }

    /// Applies the invitation policy to an invitation, refusing it or holding it for the user
    /// when the policy says so, and reporting whether it may be accepted.
    async fn admits(
        &mut self,
        datagram: &Datagram,
        token: u32,
        ssrc: u32,
        name: &Option<String>,
    ) -> bool {
        let peer = control_address(datagram);

        // Who may connect is the user's decision, not the caller's.
        // Compared in canonical form. These sockets are dual-stack, so an IPv4 peer arrives as
        // an IPv4-mapped IPv6 address, which never equals the plain IPv4 address a user typed or
        // a configuration stored — trust would silently never apply.
        let known = peer.ip().to_canonical();
        let trusted = self
            .trusted
            .iter()
            .any(|address| address.to_canonical() == known);
        match self.policy.decide(trusted) {
            InvitationDecision::Accept => true,
            InvitationDecision::Refuse => {
                info!(session = %self.name, %peer, "refusing an invitation");
                self.reply(datagram.port, peer, &ControlPacket::rejected(token, ssrc))
                    .await;
                false
            }
            InvitationDecision::Ask => {
                // Left unanswered rather than refused: the peer keeps inviting for a few seconds
                // and retries after that, so an answer given soon is acted on without the user
                // having to ask the other machine to try again.
                info!(session = %self.name, %peer, "holding an invitation until the user answers");
                // Offered rather than awaited. A peer invites repeatedly, so a full queue means
                // the same question is already asked; waiting for room would stall the
                // supervisor, and with it every message this session is carrying.
                self.notify(SessionNotice::Invitation(InvitationNotice {
                    session: self.name.clone(),
                    peer: SocketAddr::new(known, peer.port()),
                    peer_name: name.clone(),
                }));
                false
            }
        }
    }

    /// Gives the session machine a chance to act on elapsed time.
    async fn on_tick(&mut self) {
        self.tend_challenges().await;
        for guest in self.guest_addresses() {
            let ticks = self.ticks();
            if let Some(session) = self.guests.get_mut(&guest) {
                let actions = session.tick(ticks);
                self.carry_out_guest(guest, actions).await;
            }
        }
        self.retry_guests(false).await;

        let ticks = self.ticks();

        if let Some(session) = &mut self.session {
            let actions = session.tick(ticks);
            self.carry_out(actions).await;
            return;
        }

        if let Some((peer, since)) = self.asleep
            && resume_due(since, Instant::now())
        {
            self.asleep = None;
            self.resume(peer).await;
            return;
        }

        // No session, so this is where a retry becomes due.
        let retry_due = {
            let status = self.status.lock().await;
            matches!(
                status.state.phase(),
                ConnectionPhase::Retrying | ConnectionPhase::Unavailable
            ) && status
                .state
                .next_retry()
                .is_some_and(|at| at <= self.clock.now())
        };
        if retry_due && self.peer.is_some() {
            self.apply_event(Event::RetryDue).await;
            self.begin_attempt().await;
        }
    }

    /// Carries out the actions the session machine returned.
    async fn carry_out(&mut self, actions: Vec<Action>) {
        let mut queue: std::collections::VecDeque<Action> = actions.into();
        while let Some(action) = queue.pop_front() {
            match action {
                Action::SendControl { port, packet } => {
                    self.transmit(port, &packet.encode()).await;
                }
                Action::SendRtp(packet) => {
                    self.transmit(Port::Data, &packet.encode()).await;
                }
                Action::Deliver(messages) => {
                    // A full queue means the local endpoint is not keeping up; dropping is
                    // correct here, because blocking would stall the whole session.
                    if self.deliver.try_send(Inbound::Messages(messages)).is_err() {
                        debug!(session = %self.name, "dropped delivery, local endpoint is behind");
                    }
                }
                Action::DeliverSysEx(dump) => {
                    // Dropped when the local side is behind, as messages are.
                    if self.deliver.try_send(Inbound::SysEx(dump)).is_err() {
                        debug!(session = %self.name, "dropped a dump, local endpoint is behind");
                    }
                }
                Action::Established => {
                    self.on_established().await;
                    // Queued rather than sent from here, which would carry out actions from
                    // within carrying them out.
                    if std::mem::take(&mut self.restore_due) {
                        queue.extend(self.restoration());
                    }
                }
                Action::Failed(reason) => self.on_failed(reason).await,
                // The tick interval is short enough that an explicit wake adds nothing.
                Action::Wake(_) => {}
            }
        }
        self.refresh_status().await;
    }

    /// Sends bytes to the peer on the given port.
    ///
    /// The data port is always one above the control port, which is why only one address is held.
    async fn transmit(&self, port: Port, bytes: &[u8]) {
        let Some(peer) = self.peer else {
            return;
        };
        let target = match port {
            Port::Control => peer,
            Port::Data => on_port(peer, peer.port().saturating_add(1)),
        };
        let sent = self.sockets.send(port, target, bytes).await;
        let no_route = sent.as_ref().is_err_and(NetError::is_no_route);
        if let Err(error) = &sent {
            debug!(session = %self.name, %target, error = %error, "send failed");
        }
        let mut status = self.status.lock().await;
        if no_route && !status.waiting_for_network {
            info!(session = %self.name, "no network to reach the peer by; waiting for one");
        }
        status.waiting_for_network = no_route;
    }

    /// Answers one packet to a peer that is not the session's own, refusing an invitation or
    /// ending a session this side does not have.
    ///
    /// Separate from `transmit`, which sends to the peer this session is with. An answer goes to
    /// whoever asked, precisely because they are not that peer.
    async fn reply(&self, port: Port, peer: SocketAddr, packet: &ControlPacket) {
        self.send_to(port, peer, &packet.encode()).await;
    }

    /// Sends bytes to a machine other than the session's peer, on the given port.
    async fn send_to(&self, port: Port, peer: SocketAddr, bytes: &[u8]) {
        let target = match port {
            Port::Control => peer,
            Port::Data => on_port(peer, peer.port().saturating_add(1)),
        };
        if let Err(error) = self.sockets.send(port, target, bytes).await {
            debug!(session = %self.name, %target, error = %error, "could not answer a peer");
        }
    }

    /// Records that the session is carrying MIDI.
    async fn on_established(&mut self) {
        info!(session = %self.name, "network session established");
        let attempts = self.status.lock().await.state.attempt();
        self.apply_event(Event::Established).await;
        let peer = self.peer_label().await;
        self.notify(SessionNotice::Connected { peer, attempts });
    }

    /// Returns what brings a recovered peer back to the controller state it was last sent.
    ///
    /// A fresh session starts with an empty recovery journal, so whatever changed while the link
    /// was down, and what the peer lost when it restarted, would otherwise stay stale there.
    fn restoration(&mut self) -> Vec<Action> {
        let messages = self.controls.restore();
        if messages.is_empty() {
            return Vec::new();
        }
        let ticks = self.ticks();
        let Some(session) = &mut self.session else {
            return Vec::new();
        };
        info!(
            session = %self.name,
            messages = messages.len(),
            "restoring controller state on a recovered session"
        );
        session.send(&messages, ticks)
    }

    /// Names the peer for the user: by what it advertises when known, by address otherwise.
    async fn peer_label(&self) -> String {
        let status = self.status.lock().await;
        status
            .peer_name
            .clone()
            .or_else(|| status.peer_address.map(|address| address.to_string()))
            .or_else(|| self.peer.map(|address| address.to_string()))
            .unwrap_or_else(|| "its peer".to_owned())
    }

    /// Passes a notice to the daemon without waiting.
    ///
    /// Offered rather than awaited: a full queue must never stall the supervisor, and with it
    /// every message this session is carrying. The queue holds far more than a burst produces.
    fn notify(&self, notice: SessionNotice) {
        if let Some(sender) = &self.notices {
            let _ = sender.try_send(notice);
        }
    }

    /// Records a failure and schedules the next attempt.
    async fn on_failed(&mut self, failure: SessionFailure) {
        // A peer that invited us and then said goodbye chose to leave, and knows where we are if
        // it wants to come back. Invited straight back, it could never disconnect from its side.
        // One that went quiet is still chased, whoever invited: that is a link that broke.
        // A peer that went to sleep said it would reconnect on waking, and an invitation would
        // only wake it.
        let invited = self.session.as_ref().map(Session::role) == Some(Role::Responder);
        let let_go = match failure {
            SessionFailure::PeerAsleep => true,
            SessionFailure::PeerLeft => invited,
            _ => false,
        };
        let (no_network, was_up, attempts) = {
            let status = self.status.lock().await;
            (
                status.waiting_for_network,
                status.state.phase() == ConnectionPhase::Connected,
                status.state.attempt(),
            )
        };
        let reason = failure_reason(&failure, no_network);

        // A lost connection is worth a warning every time. A retry that fails the same way as the
        // last is not: the first says what is wrong, and the rest would bury it.
        let left = matches!(
            failure,
            SessionFailure::PeerLeft | SessionFailure::PeerAsleep
        );
        if was_up && left {
            info!(session = %self.name, "the peer ended the network session");
        } else if was_up {
            warn!(session = %self.name, reason = %reason, "network session lost");
        } else if no_network {
            debug!(session = %self.name, "still no network");
        } else if attempts == 0 {
            info!(session = %self.name, reason = %reason, "could not connect; retrying");
        } else {
            debug!(session = %self.name, reason = %reason, attempts, "still could not connect");
        }

        self.session = None;

        // Losing an established link is reported differently from failing to establish one,
        // because only the former can have left notes sounding.
        // Recorded as well as logged, on the same terms: every loss, and the first failure of a
        // streak. A session dropping was otherwise visible only to someone reading the log.
        let peer = self.peer_label().await;
        // A peer that said goodbye is not "not responding", and was reported as though it were.
        if was_up && left {
            self.notify(SessionNotice::Left {
                peer,
                reconnecting: !let_go,
            });
        } else if was_up {
            self.notify(SessionNotice::Lost {
                peer,
                reason: reason.clone(),
            });
        } else if attempts == 0 {
            self.notify(SessionNotice::CouldNotConnect {
                peer,
                reason: reason.clone(),
            });
        }

        let event = if was_up {
            Event::Lost(reason)
        } else {
            Event::Failed(reason)
        };
        self.apply_event(event).await;

        if let_go {
            info!(session = %self.name, "the peer that invited this session left; listening again");
            self.listen().await;
        }
    }

    /// Ends the session ahead of sleep, keeping the peer, and the machines this side invited
    /// beside it, to reconnect to on waking.
    ///
    /// Kept whichever side made the connection: the goodbye tells a Midi Harbor peer to wait
    /// rather than invite, so this side is the one that reconnects. A session still retrying stops,
    /// so it does not invite from a machine about to sleep.
    async fn suspend(&mut self) {
        let mut invited: Vec<SocketAddr> = self.invited_guests.keys().copied().collect();
        invited.sort();
        self.close_guests(true).await;
        let Some(peer) = self.peer else {
            return;
        };
        info!(session = %self.name, %peer, "ending the network session ahead of sleep");
        if let Some(session) = &mut self.session {
            let actions = session.close_for_sleep();
            self.carry_out(actions).await;
            self.session = None;
        }
        self.listen().await;
        self.asleep = Some((peer, Instant::now()));
        self.asleep_guests = invited;
    }

    /// Reconnects to the peer a session left ahead of sleep, and invites again the machines this
    /// side had invited beside it.
    async fn resume(&mut self, peer: SocketAddr) {
        info!(session = %self.name, %peer, "reconnecting network session after sleep");
        self.peer = Some(peer);
        self.peer_invited = true;
        self.apply_event(Event::Enable).await;
        self.begin_attempt().await;
        self.wake_guests().await;
    }

    /// Leaves the session listening, with no peer and no retry scheduled.
    ///
    /// Switching off and back on is what clears a pending retry and its backoff. Left switched off,
    /// the session read "disabled" in every listing while it went on accepting invitations.
    async fn listen(&mut self) {
        self.peer = None;
        self.apply_event(Event::Disable).await;
        self.apply_event(Event::Enable).await;
        self.promote_guest().await;
    }

    /// Applies an event to the connection state and carries out its effects.
    async fn apply_event(&mut self, event: Event) {
        let effects = {
            let mut status = self.status.lock().await;
            status.state.apply_now(event, &self.clock)
        };

        for effect in effects {
            match effect {
                Effect::SilenceNotes => self.silence(),
                // The machine schedules the retry; the tick loop notices when it is due.
                Effect::ScheduleRetry(delay) => {
                    debug!(session = %self.name, ?delay, "retry scheduled");
                }
                Effect::RestoreState => self.restore_due = true,
                Effect::StartConnect | Effect::CancelPending => {}
            }
        }
        self.refresh_status().await;
    }

    /// Silences every note on the local side after a lost link.
    ///
    /// Silencing is the one effect that must never be skipped: a lost link with notes held
    /// leaves them sounding until something stops them.
    fn silence(&self) {
        let silence: Vec<MidiMessage> = midi_harbor_core::midi::Channel::all()
            .flat_map(midi_harbor_core::midi::silence_channel)
            .collect();
        if self.deliver.try_send(Inbound::Messages(silence)).is_err() {
            error!(session = %self.name, "could not silence notes on a lost link");
        }
    }

    /// Copies what the session machine knows into the shared status.
    async fn refresh_status(&self) {
        let mut guests: Vec<String> = self
            .guest_addresses()
            .into_iter()
            .map(|guest| self.guest_label(guest))
            .collect();
        guests.sort();
        let mut status = self.status.lock().await;
        status.guests = guests;

        // Every machine, the peer first.
        let measured = |session: &Session| {
            Some(session.clock().round_trip()).filter(|round_trip| !round_trip.is_zero())
        };
        let mut machines = Vec::with_capacity(self.guests.len().saturating_add(1));
        if let Some(peer) = self.peer {
            machines.push(Machine {
                address: peer,
                name: self
                    .session
                    .as_ref()
                    .and_then(Session::peer_name)
                    .map(str::to_owned),
                invited: self.peer_invited,
                joined: status.state.phase() == ConnectionPhase::Connected,
                round_trip: self.session.as_ref().and_then(measured),
            });
        }
        // A guest this side invited is listed while its link is down too, as the peer is, so it
        // can be seen coming back and disconnected.
        let mut guests: Vec<SocketAddr> = self.guest_addresses();
        guests.extend(
            self.invited_guests
                .keys()
                .filter(|guest| !self.guests.contains_key(guest)),
        );
        guests.sort();
        for guest in guests {
            let session = self.guests.get(&guest);
            machines.push(Machine {
                address: guest,
                name: session.and_then(Session::peer_name).map(str::to_owned),
                invited: self.invited_guests.contains_key(&guest),
                joined: self.joined_guests.contains(&guest),
                round_trip: session.and_then(measured),
            });
        }
        status.machines = machines;

        let Some(session) = &self.session else {
            return;
        };
        status.peer_name = session.peer_name().map(str::to_owned);
        status.round_trip = session.clock().round_trip();
        status.lost = session.lost();
        status.recovered = session.recovered();
    }

    /// Ends the session, telling the peer.
    async fn close(&mut self) {
        self.close_guests(false).await;
        let Some(session) = &mut self.session else {
            return;
        };
        let actions = session.close();
        self.carry_out(actions).await;
        self.session = None;
    }

    /// Returns the guests' addresses, so each can be served while the map is borrowed mutably.
    fn guest_addresses(&self) -> Vec<SocketAddr> {
        self.guests.keys().copied().collect()
    }

    /// Lets a machine that invited the session while it had a peer in as a guest, if the
    /// invitation policy allows it.
    async fn admit_guest(&mut self, datagram: &Datagram) -> Admission {
        // A guest begins on the control port, as a peer does; a stray data-port invitation from
        // a machine that was never accepted there is an outsider's.
        let Ok(ControlPacket::Session {
            command: midi_harbor_rtpmidi::Handshake::Invitation,
            token,
            ssrc,
            ref name,
        }) = ControlPacket::parse(&datagram.bytes)
        else {
            return Admission::NotAnInvitation;
        };
        if datagram.port != Port::Control {
            return Admission::NotAnInvitation;
        }
        if !self.admits(datagram, token, ssrc, name).await {
            return Admission::Answered;
        }
        let guest = control_address(datagram);
        info!(session = %self.name, peer = %guest, "letting a second machine into the session");
        let _ = self.guests.insert(
            guest,
            Session::responder(
                rand::random::<u32>(),
                rand::random::<u32>(),
                self.name.clone(),
            ),
        );
        Admission::Admitted
    }

    /// Feeds a datagram from a guest to its session machine.
    async fn feed_guest(&mut self, guest: SocketAddr, datagram: &Datagram) {
        let ticks = self.ticks();
        let Some(session) = self.guests.get_mut(&guest) else {
            return;
        };
        let actions = match ControlPacket::parse(&datagram.bytes) {
            Ok(packet) => session.on_control(datagram.port, &packet, ticks),
            Err(_) => match RtpMidiPacket::parse(&datagram.bytes) {
                Ok(packet) => session.on_rtp(&packet),
                Err(error) => {
                    debug!(from = %datagram.from, error = %error, "discarding unreadable datagram");
                    Vec::new()
                }
            },
        };
        self.carry_out_guest(guest, actions).await;
    }

    /// Carries out what a guest's session machine returned, ending the guest's part when its
    /// link fails.
    async fn carry_out_guest(&mut self, guest: SocketAddr, actions: Vec<Action>) {
        let mut queue: std::collections::VecDeque<Action> = actions.into();
        let mut failed = None;
        while let Some(action) = queue.pop_front() {
            match action {
                Action::SendControl { port, packet } => self.reply(port, guest, &packet).await,
                Action::SendRtp(packet) => {
                    self.send_to(Port::Data, guest, &packet.encode()).await;
                }
                Action::Deliver(messages) => {
                    if self.deliver.try_send(Inbound::Messages(messages)).is_err() {
                        debug!(session = %self.name, "dropped delivery, local endpoint is behind");
                    }
                }
                Action::DeliverSysEx(dump) => {
                    if self.deliver.try_send(Inbound::SysEx(dump)).is_err() {
                        debug!(session = %self.name, "dropped a dump, local endpoint is behind");
                    }
                }
                Action::Established => {
                    info!(session = %self.name, peer = %guest, "a second machine joined the session");
                    let _ = self.joined_guests.insert(guest);
                    let attempts = self
                        .invited_guests
                        .get(&guest)
                        .map_or(0, ConnectionState::attempt);
                    let peer = self.guest_label(guest);
                    self.notify(SessionNotice::Connected { peer, attempts });
                    // Queued rather than sent from here, as the peer's restoration is.
                    if self.guest_event(guest, Event::Established) {
                        queue.extend(self.guest_restoration(guest));
                    }
                }
                Action::Failed(failure) => failed = Some(failure),
                Action::Wake(_) => {}
            }
        }
        if let Some(failure) = failed {
            self.end_guest(guest, failure);
        }
        self.refresh_status().await;
    }

    /// Ends a guest's part after its link failed.
    ///
    /// A guest this side invited is invited again with backoff on the peer's terms, whether it
    /// went quiet, refused or said goodbye, until the user disconnects it. One that went to
    /// sleep, or invited itself in and then said goodbye, is let go as the peer would be; its
    /// notes were released by its own goodbye.
    fn end_guest(&mut self, guest: SocketAddr, failure: SessionFailure) {
        let was_up = self.joined_guests.remove(&guest);
        let responder = self.guests.get(&guest).map(Session::role) == Some(Role::Responder);
        let peer = self.guest_label(guest);
        let _ = self.guests.remove(&guest);
        let let_go = match failure {
            SessionFailure::PeerAsleep => true,
            SessionFailure::PeerLeft => responder,
            _ => false,
        };
        let reason = failure_reason(&failure, false);

        if let_go || !self.invited_guests.contains_key(&guest) {
            // One this side invited that never joined could not be reached, which is worth
            // saying differently.
            let invited = self.invited_guests.remove(&guest).is_some();
            if invited && !was_up {
                info!(session = %self.name, %peer, "could not bring a second machine into the session");
                self.notify(SessionNotice::CouldNotConnect { peer, reason });
            } else {
                info!(session = %self.name, %peer, "a second machine left the session");
                self.notify(SessionNotice::GuestLeft { peer });
            }
            return;
        }

        // Reported on the peer's terms: every loss, and the first failure of a streak.
        let attempts = self
            .invited_guests
            .get(&guest)
            .map_or(0, ConnectionState::attempt);
        let left = matches!(failure, SessionFailure::PeerLeft);
        if was_up && left {
            info!(session = %self.name, %peer, "a second machine ended its part; inviting it again");
            self.notify(SessionNotice::Left {
                peer,
                reconnecting: true,
            });
        } else if was_up {
            warn!(session = %self.name, %peer, reason = %reason, "lost a second machine; inviting it again");
            self.notify(SessionNotice::Lost {
                peer,
                reason: reason.clone(),
            });
        } else if attempts == 0 {
            info!(session = %self.name, %peer, reason = %reason, "could not bring a second machine into the session; retrying");
            self.notify(SessionNotice::CouldNotConnect {
                peer,
                reason: reason.clone(),
            });
        } else {
            debug!(session = %self.name, %peer, reason = %reason, attempts, "still could not bring a second machine into the session");
        }
        let event = if was_up {
            Event::Lost(reason)
        } else {
            Event::Failed(reason)
        };
        let _ = self.guest_event(guest, event);
    }

    /// Applies an event to the lifecycle of a guest this side invited, carrying out its effects,
    /// and reports whether the guest is owed the controller state it was last sent.
    fn guest_event(&mut self, guest: SocketAddr, event: Event) -> bool {
        let Some(state) = self.invited_guests.get_mut(&guest) else {
            return false;
        };
        let effects = state.apply_now(event, &self.clock);
        let mut restore = false;
        for effect in effects {
            match effect {
                Effect::SilenceNotes => self.silence(),
                Effect::RestoreState => restore = true,
                Effect::ScheduleRetry(delay) => {
                    debug!(session = %self.name, peer = %guest, ?delay, "retry scheduled");
                }
                Effect::StartConnect | Effect::CancelPending => {}
            }
        }
        restore
    }

    /// Returns what brings a guest that came back to the controller state it was last sent.
    fn guest_restoration(&mut self, guest: SocketAddr) -> Vec<Action> {
        let messages = self.controls.restore();
        if messages.is_empty() {
            return Vec::new();
        }
        let ticks = self.ticks();
        let Some(session) = self.guests.get_mut(&guest) else {
            return Vec::new();
        };
        info!(
            session = %self.name,
            peer = %guest,
            messages = messages.len(),
            "restoring controller state on a machine that came back"
        );
        session.send(&messages, ticks)
    }

    /// Returns a fresh lifecycle for a machine this side invites beside the peer, retried on the
    /// peer's policy.
    fn chased(&self) -> ConnectionState {
        ConnectionState::with_policy(
            ConnectionPhase::Disconnected,
            self.clock.now(),
            BackoffPolicy::responsive(),
        )
    }

    /// Starts an attempt to bring a machine this side invited into the session beside the peer.
    async fn begin_guest_attempt(&mut self, machine: SocketAddr) {
        let ticks = self.ticks();
        let mut session = Session::initiator(
            rand::random::<u32>(),
            rand::random::<u32>(),
            self.name.clone(),
        );
        let actions = session.start(ticks);
        let _ = self.guests.insert(machine, session);
        let _ = self.guest_event(machine, Event::Attempting);
        self.carry_out_guest(machine, actions).await;
    }

    /// Invites again each machine this side invited whose link is down and whose retry is due,
    /// or each at once when conditions changed.
    async fn retry_guests(&mut self, nudged: bool) {
        let now = self.clock.now();
        let mut due: Vec<SocketAddr> = self
            .invited_guests
            .iter()
            .filter(|(machine, _)| !self.guests.contains_key(machine))
            .filter(|(_, state)| {
                matches!(
                    state.phase(),
                    ConnectionPhase::Retrying | ConnectionPhase::Unavailable
                ) && (nudged || state.next_retry().is_some_and(|at| at <= now))
            })
            .map(|(machine, _)| *machine)
            .collect();
        due.sort();
        let event = if nudged {
            Event::Nudge
        } else {
            Event::RetryDue
        };
        for machine in due {
            let _ = self.guest_event(machine, event.clone());
            debug!(session = %self.name, peer = %machine, "inviting a second machine again");
            self.begin_guest_attempt(machine).await;
        }
    }

    /// Invites again the machines this side had invited beside the peer before sleep.
    async fn wake_guests(&mut self) {
        for machine in std::mem::take(&mut self.asleep_guests) {
            let _ = self.invite(machine).await;
        }
    }

    /// Names a guest by what it advertises, or by its address.
    fn guest_label(&self, guest: SocketAddr) -> String {
        self.guests
            .get(&guest)
            .and_then(Session::peer_name)
            .map_or_else(|| guest.to_string(), str::to_owned)
    }

    /// Ends every guest's part, telling each, ahead of sleep or when the session is closed.
    async fn close_guests(&mut self, for_sleep: bool) {
        // Forgotten first, so closing one is not taken for a link to chase.
        self.invited_guests.clear();
        for guest in self.guest_addresses() {
            if let Some(session) = self.guests.get_mut(&guest) {
                let actions = if for_sleep {
                    session.close_for_sleep()
                } else {
                    session.close()
                };
                self.carry_out_guest(guest, actions).await;
            }
        }
        self.guests.clear();
        self.joined_guests.clear();
    }

    /// Makes a machine the session's peer and connects to it, replacing any peer it had.
    async fn connect(&mut self, peer: SocketAddr) {
        info!(session = %self.name, %peer, "connecting network session");
        self.asleep = None;
        self.peer = Some(peer);
        self.peer_invited = true;
        // Disconnecting switches the session off, and the state machine ignores an attempt
        // while it is off. Without re-enabling, a connect after a disconnect reports success and
        // quietly does nothing.
        self.apply_event(Event::Enable).await;
        self.begin_attempt().await;
    }

    /// Invites a machine beside the peer, or connects to it when the session has no peer, and
    /// returns the place it took.
    ///
    /// A machine invited beside the peer is chased as the peer is when its link is lost.
    async fn invite(&mut self, machine: SocketAddr) -> Place {
        let machine = canonical(machine);
        if self.peer.is_none() && self.session.is_none() {
            self.connect(machine).await;
            return Place::Peer;
        }
        if self.peer.map(canonical) == Some(machine) {
            return Place::Peer;
        }
        if self.invited_guests.contains_key(&machine) {
            return Place::Beside;
        }
        let mut state = self.chased();
        // A machine that invited itself in is this side's to chase from the moment it is asked
        // for.
        if self.guests.contains_key(&machine) {
            let _ = state.apply_now(Event::Attempting, &self.clock);
            if self.joined_guests.contains(&machine) {
                let _ = state.apply_now(Event::Established, &self.clock);
            }
            let _ = self.invited_guests.insert(machine, state);
            self.refresh_status().await;
            return Place::Beside;
        }
        info!(session = %self.name, peer = %machine, "inviting a second machine into the session");
        let _ = self.invited_guests.insert(machine, state);
        self.begin_guest_attempt(machine).await;
        Place::Beside
    }

    /// Ends one machine's part, telling it, and reports whether it was taking part.
    ///
    /// The peer's going leaves a guest to become the peer, so the machines still connected
    /// stay carried by something the session reports. A machine this side invited is
    /// forgotten, so it is not invited again.
    async fn disconnect_machine(&mut self, machine: SocketAddr) -> bool {
        let machine = canonical(machine);
        if self.peer.map(canonical) == Some(machine) {
            info!(session = %self.name, peer = %machine, "disconnecting one machine from the session");
            self.asleep = None;
            if let Some(session) = &mut self.session {
                let actions = session.close();
                self.carry_out(actions).await;
            }
            self.session = None;
            self.listen().await;
            self.refresh_status().await;
            return true;
        }
        // Forgotten before it is told, so its goodbye is not taken for a link to chase.
        let chased = self.invited_guests.remove(&machine).is_some();
        let before = self.asleep_guests.len();
        self.asleep_guests.retain(|asleep| *asleep != machine);
        let asleep = self.asleep_guests.len() != before;
        let Some(session) = self.guests.get_mut(&machine) else {
            // One waiting to be invited again has nothing to tell.
            if chased || asleep {
                info!(session = %self.name, peer = %machine, "disconnecting one machine from the session");
                self.refresh_status().await;
            }
            return chased || asleep;
        };
        info!(session = %self.name, peer = %machine, "disconnecting one machine from the session");
        let actions = session.close();
        self.carry_out_guest(machine, actions).await;
        let _ = self.guests.remove(&machine);
        let _ = self.joined_guests.remove(&machine);
        self.refresh_status().await;
        true
    }

    /// Connects to a machine this side connected to at `to` in place of `from`, unless it is
    /// carrying MIDI, and reports whether it moved.
    ///
    /// The attempt in progress at the old address is dropped without a goodbye, since nothing
    /// was established there to end.
    async fn move_machine(&mut self, from: SocketAddr, to: SocketAddr) -> bool {
        let (from, to) = (canonical(from), canonical(to));
        if self.peer.map(canonical) == Some(from) {
            if self.status.lock().await.state.phase() == ConnectionPhase::Connected {
                return false;
            }
            info!(session = %self.name, %from, %to, "following the peer to where it is advertised now");
            self.connect(to).await;
            return true;
        }
        if self.invited_guests.contains_key(&from) {
            if self.joined_guests.contains(&from) {
                return false;
            }
            info!(session = %self.name, %from, %to, "following a second machine to where it is advertised now");
            let _ = self.invited_guests.remove(&from);
            let _ = self.guests.remove(&from);
            let _ = self.invite(to).await;
            return true;
        }
        // Waiting out sleep, it is reconnected to at the new address on waking.
        if let Some((peer, since)) = self.asleep
            && canonical(peer) == from
        {
            self.asleep = Some((to, since));
            return true;
        }
        if let Some(asleep) = self
            .asleep_guests
            .iter_mut()
            .find(|asleep| canonical(**asleep) == from)
        {
            *asleep = to;
            return true;
        }
        false
    }

    /// Makes a guest the session's peer once the session has no other, so the machines still
    /// connected are not left carried by nothing the session reports.
    ///
    /// A running guest this side invited is preferred, then any running guest, then one this
    /// side invited that is waiting to be invited again. Ties go to the lowest address, so the
    /// choice does not depend on the order of a map.
    async fn promote_guest(&mut self) {
        if self.peer.is_some() || self.session.is_some() {
            return;
        }
        let mut running = self.guest_addresses();
        running.sort_by_key(|guest| (!self.invited_guests.contains_key(guest), *guest));
        let Some(guest) = running.first().copied() else {
            self.promote_waiting_guest().await;
            return;
        };
        let Some(session) = self.guests.remove(&guest) else {
            return;
        };
        let established = session.phase() == midi_harbor_rtpmidi::Phase::Established;
        info!(session = %self.name, peer = %guest, "a remaining machine becomes the session's peer");
        self.peer = Some(guest);
        self.peer_invited = self.invited_guests.remove(&guest).is_some();
        let _ = self.joined_guests.remove(&guest);
        self.session = Some(session);
        self.apply_event(Event::Attempting).await;
        if established {
            self.apply_event(Event::Established).await;
        }
        let mut status = self.status.lock().await;
        status.peer_address = Some(guest);
    }

    /// Makes a machine this side invited, waiting to be invited again, the session's peer, and
    /// starts connecting to it as the peer.
    async fn promote_waiting_guest(&mut self) {
        let mut waiting: Vec<SocketAddr> = self.invited_guests.keys().copied().collect();
        waiting.sort();
        let Some(guest) = waiting.first().copied() else {
            return;
        };
        let _ = self.invited_guests.remove(&guest);
        info!(session = %self.name, peer = %guest, "a remaining machine becomes the session's peer");
        self.peer = Some(guest);
        self.peer_invited = true;
        // Boxed, because an attempt can fail at once and fall back here through `listen`.
        Box::pin(self.begin_attempt()).await;
    }
}

/// Reports whether a session that ended itself ahead of sleep has been awake long enough to
/// reconnect without being told the machine woke.
fn resume_due(since: Instant, now: Instant) -> bool {
    now.saturating_duration_since(since) >= RESUME_AFTER_SLEEP
}

/// Returns the reason a session failure is reported under.
///
/// A peer that went quiet, or invitations that went unanswered, are the network's doing only when
/// this machine could not even send; when packets went out, the network is fine and the peer is
/// not answering. Losing Wi-Fi drops this machine's addresses, and the check on the peer that
/// follows then times out with nothing having left the machine (R-069).
fn failure_reason(failure: &SessionFailure, no_network: bool) -> FailureReason {
    match failure {
        SessionFailure::Rejected => FailureReason::PeerRejected,
        SessionFailure::PeerLeft | SessionFailure::PeerAsleep => FailureReason::PeerTimeout,
        SessionFailure::Timeout | SessionFailure::Unreachable if no_network => {
            FailureReason::NetworkUnreachable
        }
        SessionFailure::Timeout | SessionFailure::Unreachable => FailureReason::PeerTimeout,
    }
}

/// Returns the peer's control address, from a datagram that arrived on either port.
///
/// The data port is one above the control port, so the control address is the one stored.
fn control_address(datagram: &Datagram) -> SocketAddr {
    let control = match datagram.port {
        Port::Control => datagram.from,
        Port::Data => on_port(datagram.from, datagram.from.port().saturating_sub(1)),
    };
    canonical(control)
}

/// Returns an address on another port of the same machine.
///
/// The address is kept whole rather than rebuilt from its IP, because an IPv6 link-local address
/// is only reachable with its scope, the interface it was heard on.
fn on_port(address: SocketAddr, port: u16) -> SocketAddr {
    let mut moved = address;
    moved.set_port(port);
    moved
}

/// Returns an address in the form machines are kept by.
///
/// The sockets are bound to the IPv6 wildcard, so an IPv4 machine's packets arrive from its
/// IPv4-mapped address. Keeping that form beside the plain one a user typed made a machine this
/// side invited look like an outsider when it answered, and it was sent a goodbye.
///
/// An IPv6 address keeps its scope. Apple's Network MIDI invites over the link-local address
/// Bonjour gives it, and the answer to `fe80::` with no scope goes nowhere: Audio MIDI Setup
/// reported that the port "didn't respond to the connection request".
fn canonical(address: SocketAddr) -> SocketAddr {
    match address.ip().to_canonical() {
        IpAddr::V4(v4) => SocketAddr::new(IpAddr::V4(v4), address.port()),
        IpAddr::V6(_) => address,
    }
}

/// Reports whether a datagram is something only a running session sends.
///
/// Clock exchanges, receiver feedback, MIDI, and an acceptance of an invitation already given up
/// on. An invitation is not: it asks for a session rather than assuming one. Nor is a refusal or a
/// goodbye, because answering those with a goodbye would have two sides trading them for good.
fn is_session_traffic(bytes: &[u8]) -> bool {
    match ControlPacket::parse(bytes) {
        Ok(ControlPacket::ClockSync { .. } | ControlPacket::ReceiverFeedback { .. }) => true,
        Ok(ControlPacket::Session { command, .. }) => {
            command == midi_harbor_rtpmidi::Handshake::Accepted
        }
        Err(_) => RtpMidiPacket::parse(bytes).is_ok(),
    }
}

/// Reports whether an address is due a goodbye, given the last one sent.
fn goodbye_due(last: Option<(SocketAddr, Instant)>, to: SocketAddr, now: Instant) -> bool {
    match last {
        Some((address, at)) if address == to => {
            now.saturating_duration_since(at) >= GOODBYE_INTERVAL
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::DEFAULT_CONTROL_PORT;

    /// Returns 127.0.0.1 at a port.
    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// Proves the address a machine is answered at is the one it was heard from: an IPv4 machine
    /// seen through a dual-stack socket by its plain address, and an IPv6 link-local one with its
    /// scope, on the control port whichever port it sent from.
    ///
    /// Regression: the scope was dropped, so Apple's Network MIDI, which invites over the
    /// link-local address Bonjour resolves, was never answered. Scope 14 stands for the interface
    /// the invitation arrived on.
    #[test]
    fn a_machine_is_answered_at_the_address_it_was_heard_from() {
        let link_local = |port: u16| {
            SocketAddr::V6(std::net::SocketAddrV6::new(
                "fe80::1".parse().unwrap(),
                port,
                0,
                14,
            ))
        };
        let cases = [
            (
                "IPv4 through a dual-stack socket",
                Port::Control,
                "[::ffff:192.0.2.10]:5004".parse().unwrap(),
                "192.0.2.10:5004".parse().unwrap(),
            ),
            (
                "link-local on the control port",
                Port::Control,
                link_local(5004),
                link_local(5004),
            ),
            (
                "link-local on the data port",
                Port::Data,
                link_local(5005),
                link_local(5004),
            ),
        ];
        for (name, port, from, want) in cases {
            let datagram = Datagram {
                port,
                from,
                bytes: Vec::new(),
            };
            assert_eq!(
                control_address(&datagram),
                want,
                "{name}: the wrong address is answered"
            );
            assert_eq!(
                on_port(control_address(&datagram), want.port() + 1),
                on_port(want, want.port() + 1),
                "{name}: the data port is not on the same machine and interface"
            );
        }
    }

    /// Starts a session on loopback under a policy, keeping what it delivers and what it reports.
    async fn started(
        name: &str,
        port: u16,
        policy: InvitationPolicy,
    ) -> (
        NetworkSession,
        mpsc::Receiver<Inbound>,
        mpsc::Receiver<SessionNotice>,
    ) {
        let (deliver, delivered) = mpsc::channel(64);
        let (notices, noticed) = mpsc::channel(64);
        let session = NetworkSession::start(name.to_owned(), port, deliver, policy, Some(notices))
            .await
            .unwrap_or_else(|error| panic!("the session {name} failed to start: {error}"));
        (session, delivered, noticed)
    }

    /// Starts a session accepting every invitation, on a port of the system's choosing unless
    /// one is given.
    async fn accepting(
        name: &str,
        port: u16,
    ) -> (
        NetworkSession,
        mpsc::Receiver<Inbound>,
        mpsc::Receiver<SessionNotice>,
    ) {
        started(name, port, InvitationPolicy::AcceptAll).await
    }

    /// Stops a session the way a crash would, without telling its peer.
    async fn crash(session: &NetworkSession) {
        if let Some(task) = session.running.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
    }

    /// Waits for a session to reach a phase, reporting whether it did in time.
    async fn reaches(session: &NetworkSession, phase: ConnectionPhase, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if session.status().await.state.phase() == phase {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Waits for the first notice matching a test, returning it.
    async fn awaited(
        notices: &mut mpsc::Receiver<SessionNotice>,
        within: Duration,
        wanted: impl Fn(&SessionNotice) -> bool,
    ) -> Option<SessionNotice> {
        tokio::time::timeout(within, async {
            while let Some(notice) = notices.recv().await {
                if wanted(&notice) {
                    return Some(notice);
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
    }

    /// Waits for a session's machines to satisfy a condition, returning the last seen.
    async fn machines_until(
        session: &NetworkSession,
        wanted: impl Fn(&[Machine]) -> bool,
    ) -> Vec<Machine> {
        let mut seen = Vec::new();
        for _ in 0..200 {
            seen = session.status().await.machines;
            if wanted(&seen) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        seen
    }

    /// Reports whether an invitation reaches `socket` after whatever it has already received,
    /// within eight seconds, longer than the first retry and a whole failed attempt take.
    async fn invited_again(socket: &tokio::net::UdpSocket) -> bool {
        let mut buffer = [0_u8; 512];
        while socket.try_recv_from(&mut buffer).is_ok() {}
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while let Ok(Ok((length, _))) =
            tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
        {
            if matches!(
                ControlPacket::parse(buffer.get(..length).unwrap_or_default()),
                Ok(ControlPacket::Session {
                    command: midi_harbor_rtpmidi::Handshake::Invitation,
                    ..
                })
            ) {
                return true;
            }
        }
        false
    }

    /// Proves an enabled session connecting to a peer that can never answer keeps retrying on a
    /// schedule, and that a nudge starts the next attempt at once instead of when the timer is
    /// due (Constitution Principle I: no terminal give-up state on a transient error). The
    /// backoff reaches thirty seconds, so a session retrying when the machine suspended would
    /// otherwise wait up to that long after it wakes. Port 9 is the discard service, so nothing
    /// ever answers there; an invitation is tried three times at a two-second timeout before the
    /// first retry is scheduled, which is why the wait allows fifteen seconds.
    #[tokio::test]
    async fn an_unanswered_session_keeps_retrying_and_a_nudge_retries_at_once() {
        let (session, _delivered, _notices) = accepting("Studio", 0).await;
        let unreachable = loopback(9);
        assert!(
            session.connect(unreachable).await,
            "the supervisor must take the connect command"
        );

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut waiting = None;
        while Instant::now() < deadline {
            let status = session.status().await;
            if matches!(
                status.state.phase(),
                ConnectionPhase::Retrying | ConnectionPhase::Unavailable
            ) && status.state.next_retry().is_some()
            {
                waiting = Some(status);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let waiting = waiting.expect("the session reaches a state that schedules its next attempt");
        assert_eq!(
            waiting.peer_address,
            Some(unreachable),
            "a session that failed must keep the peer it is retrying"
        );
        assert!(
            waiting.state.phase().recovers_on_its_own(),
            "an enabled session must never reach a state it cannot leave"
        );

        assert!(session.nudge().await, "the supervisor must take the nudge");
        // The attempt starts on the nudge rather than when the timer was due, so the scheduled
        // retry is gone as soon as the command is handled.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut attempted = false;
        while Instant::now() < deadline {
            if session.status().await.state.next_retry().is_none() {
                attempted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(attempted, "the nudge did not start a fresh attempt");
        session.shutdown().await;
    }

    /// Proves a nudge leaves a working session alone. A nudge arrives whenever the machine
    /// reports anything, including while everything is fine, and reconnecting a healthy link would
    /// turn a harmless hint into a dropout.
    #[tokio::test]
    async fn a_nudge_leaves_a_working_session_alone() {
        let (alice, _alice_heard, _) = accepting("Alice", 0).await;
        let (bob, _bob_heard, _) = accepting("Bob", 0).await;
        assert!(
            alice.connect(loopback(bob.control_port())).await,
            "the supervisor must take the connect command"
        );
        assert!(
            reaches(&alice, ConnectionPhase::Connected, Duration::from_secs(5)).await,
            "the session never connected, so the nudge would prove nothing"
        );

        assert!(alice.nudge().await, "the supervisor must take the nudge");
        // Time for a wrongly handled nudge to tear the link down.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(
            alice.status().await.state.phase(),
            ConnectionPhase::Connected,
            "a nudge tore down a working link"
        );
        alice.shutdown().await;
        bob.shutdown().await;
    }

    /// Proves a session whose policy refuses everything lets nobody in. The policy was stored,
    /// displayed and settable while every invitation was accepted regardless, so anything that
    /// could reach the control port was in.
    #[tokio::test]
    async fn a_session_that_refuses_everything_lets_nobody_in() {
        let (alice, _alice_heard, _) = accepting("Alice", 0).await;
        let (bob, _bob_heard, _) = started("Bob", 0, InvitationPolicy::RejectAll).await;
        assert!(
            alice.connect(loopback(bob.control_port())).await,
            "the supervisor must take the connect command"
        );

        // Three seconds covers the first invitation and its answer.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            assert_ne!(
                alice.status().await.state.phase(),
                ConnectionPhase::Connected,
                "a session that refuses everything let a peer in"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            bob.status().await.state.phase(),
            ConnectionPhase::Disconnected,
            "the refused invitation started a session anyway"
        );

        alice.shutdown().await;
        bob.shutdown().await;
    }

    /// Proves a session that asks first holds an invitation, reports it naming the session and
    /// the inviting peer, and lets in the invitation still in flight once the peer's machine is
    /// trusted by address. A machine is remembered by host with the standard port 5004, not the
    /// port the peer happens to invite from, which is why trust matches the host alone.
    #[tokio::test]
    async fn trusting_a_machine_lets_in_the_invitation_already_in_flight() {
        let (alice, _alice_heard, _) = accepting("Alice", 0).await;
        let (bob, _bob_heard, mut arriving) = started("Bob", 0, InvitationPolicy::Prompt).await;
        assert!(
            alice.connect(loopback(bob.control_port())).await,
            "the supervisor must take the connect command"
        );

        let notice = tokio::time::timeout(Duration::from_secs(3), arriving.recv())
            .await
            .expect("an invitation is reported within three seconds")
            .expect("the notice channel stays open");
        let SessionNotice::Invitation(notice) = notice else {
            panic!("expected an invitation, got {notice:?}");
        };
        assert_eq!(
            (notice.session.as_str(), notice.peer_name.as_deref()),
            ("Bob", Some("Alice")),
            "the prompt must name the session asked and the machine asking"
        );
        assert_ne!(
            alice.status().await.state.phase(),
            ConnectionPhase::Connected,
            "an invitation waiting on the user must be held, not accepted"
        );

        let remembered = loopback(DEFAULT_CONTROL_PORT);
        assert!(
            bob.configure(InvitationPolicy::Prompt, vec![remembered.ip()])
                .await,
            "the supervisor must take the new trust list"
        );
        assert!(
            reaches(&alice, ConnectionPhase::Connected, Duration::from_secs(15)).await,
            "trusting the machine did not let in the invitation it was still sending"
        );

        alice.shutdown().await;
        bob.shutdown().await;
    }

    /// Proves a machine invited beside the peer that never answers is reported, stays listed and
    /// is invited again for as long as it is wanted (FR-015i), and that disconnecting it forgets
    /// it and stops the invitations without disturbing the peer.
    #[tokio::test]
    async fn a_machine_that_never_answers_is_reported_and_invited_again_until_disconnected() {
        let (alice, _, _) = accepting("Alice", 0).await;
        let (bob, _, mut bob_notices) = accepting("Bob", 0).await;
        assert!(
            bob.connect(loopback(alice.control_port())).await,
            "the supervisor must take the connect command"
        );
        let _ = machines_until(&bob, |machines| machines.first().is_some_and(|m| m.joined)).await;

        // A socket that receives the invitation and never answers it.
        let silent = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a silent socket binds on loopback");
        let nobody = silent.local_addr().expect("a bound socket has an address");
        assert_eq!(
            bob.invite(nobody).await,
            Some(Place::Beside),
            "with a peer connected, another machine is invited beside it"
        );
        let waiting = machines_until(&bob, |machines| {
            machines.iter().any(|machine| machine.address == nobody)
        })
        .await;
        assert!(
            waiting
                .iter()
                .find(|machine| machine.address == nobody)
                .is_some_and(|machine| !machine.joined && machine.round_trip.is_none()),
            "a machine that never answered must be listed as not joined: {waiting:?}"
        );

        let reported = awaited(&mut bob_notices, Duration::from_secs(15), |notice| {
            matches!(notice, SessionNotice::CouldNotConnect { .. })
        })
        .await;
        assert!(
            matches!(&reported, Some(SessionNotice::CouldNotConnect { peer, .. }) if *peer == nobody.to_string()),
            "the failed invitation must be reported against the silent machine, got {reported:?}"
        );

        let machines = bob.status().await.machines;
        assert_eq!(
            machines.len(),
            2,
            "the peer and the silent machine must both stay listed: {machines:?}"
        );
        assert!(
            invited_again(&silent).await,
            "the machine was let go rather than invited again"
        );

        assert!(
            bob.disconnect_machine(nobody).await,
            "a listed machine can be disconnected"
        );
        let machines = bob.status().await.machines;
        assert_eq!(
            machines
                .iter()
                .map(|machine| machine.address)
                .collect::<Vec<_>>(),
            vec![loopback(alice.control_port())],
            "disconnecting the silent machine must leave exactly the peer"
        );
        assert!(
            !invited_again(&silent).await,
            "a disconnected machine was invited again"
        );

        alice.shutdown().await;
        bob.shutdown().await;
    }

    /// Proves a machine invited beside the peer that restarts without a goodbye is invited back,
    /// and the peer is never disturbed. The restarted machine says it has no session when the
    /// next clock exchange reaches it, which is how the restart is noticed.
    #[tokio::test]
    async fn a_machine_invited_beside_the_peer_is_invited_again_when_it_restarts() {
        let (alice, _, _) = accepting("Alice", 0).await;
        let (carol, _, _) = accepting("Carol", 0).await;
        let carol_port = carol.control_port();
        let (bob, _, mut bob_notices) = accepting("Bob", 0).await;
        assert!(
            bob.connect(loopback(alice.control_port())).await,
            "the supervisor must take the connect command"
        );
        assert_eq!(
            bob.invite(loopback(carol_port)).await,
            Some(Place::Beside),
            "with a peer connected, another machine is invited beside it"
        );
        let joined = machines_until(&bob, |machines| {
            machines.len() == 2 && machines.iter().all(|machine| machine.joined)
        })
        .await;
        assert_eq!(joined.len(), 2, "both machines must join first: {joined:?}");

        crash(&carol).await;
        let (carol, _, _) = accepting("Carol", carol_port).await;
        let left = awaited(&mut bob_notices, Duration::from_secs(10), |notice| {
            matches!(
                notice,
                SessionNotice::Left {
                    reconnecting: true,
                    ..
                }
            )
        })
        .await;
        assert!(left.is_some(), "bob did not hear that carol restarted");

        let back = awaited(
            &mut bob_notices,
            Duration::from_secs(10),
            |notice| matches!(notice, SessionNotice::Connected { peer, .. } if peer == "Carol"),
        )
        .await;
        assert!(back.is_some(), "carol was not invited back");
        let machines = machines_until(&bob, |machines| {
            machines.len() == 2 && machines.iter().all(|machine| machine.joined)
        })
        .await;
        assert!(
            machines.len() == 2 && machines.iter().all(|machine| machine.joined),
            "the peer and the restarted machine must both be joined again: {machines:?}"
        );
        assert_eq!(
            bob.status().await.state.phase(),
            ConnectionPhase::Connected,
            "the session must stay connected to its peer throughout"
        );

        for session in [alice, bob, carol] {
            session.shutdown().await;
        }
    }

    /// Proves a peer that restarted without a goodbye is reconnected in seconds. The next clock
    /// exchange reaches a peer with no session, which says so, rather than the session waiting out
    /// the 35-second liveness timeout.
    #[tokio::test]
    async fn a_peer_that_restarted_is_reconnected_in_seconds() {
        let (alice, _, mut alice_notices) = accepting("Alice", 0).await;
        let (bob, _, _) = accepting("Bob", 0).await;
        let bob_port = bob.control_port();
        assert!(
            alice.connect(loopback(bob_port)).await,
            "the supervisor must take the connect command"
        );
        assert!(
            reaches(&alice, ConnectionPhase::Connected, Duration::from_secs(5)).await,
            "the session must connect before the restart"
        );

        crash(&bob).await;
        let (bob, _, _) = accepting("Bob", bob_port).await;

        let lost = awaited(&mut alice_notices, Duration::from_secs(5), |notice| {
            matches!(
                notice,
                SessionNotice::Left {
                    reconnecting: true,
                    ..
                }
            )
        })
        .await;
        assert!(lost.is_some(), "alice did not hear that bob restarted");
        assert!(
            reaches(&alice, ConnectionPhase::Connected, Duration::from_secs(10)).await,
            "alice did not invite the restarted bob back"
        );

        alice.shutdown().await;
        bob.shutdown().await;
    }

    /// Proves a peer that restarted and invites again from the same port under a new SSRC
    /// replaces its old session. What the old peer held is released only by ending the old
    /// session, and the invitation that ended it begins the new one: left for the peer to repeat,
    /// it would cost the two seconds the peer waits before inviting again, which is why the new
    /// session must connect within a second and a half.
    #[tokio::test]
    async fn a_peer_that_restarted_and_invites_again_replaces_its_old_session() {
        let (alice, _, _) = accepting("Alice", 0).await;
        let (bob, _, mut bob_notices) = accepting("Bob", 0).await;
        let alice_port = alice.control_port();
        assert!(
            alice.connect(loopback(bob.control_port())).await,
            "the supervisor must take the connect command"
        );
        assert!(
            reaches(&alice, ConnectionPhase::Connected, Duration::from_secs(5)).await,
            "the session must connect before the restart"
        );
        assert!(
            awaited(&mut bob_notices, Duration::from_secs(5), |notice| {
                matches!(notice, SessionNotice::Connected { .. })
            })
            .await
            .is_some(),
            "bob must report the first session connected"
        );

        crash(&alice).await;
        let (alice, _, _) = accepting("Alice", alice_port).await;
        assert!(
            alice.connect(loopback(bob.control_port())).await,
            "the supervisor must take the connect command"
        );

        let lost = awaited(&mut bob_notices, Duration::from_secs(5), |notice| {
            matches!(notice, SessionNotice::Left { .. })
        })
        .await;
        assert!(lost.is_some(), "bob kept the old session");
        assert!(
            reaches(
                &alice,
                ConnectionPhase::Connected,
                Duration::from_millis(1500)
            )
            .await,
            "the invitation that ended the old session did not begin the new one"
        );
        assert!(
            reaches(&bob, ConnectionPhase::Connected, Duration::from_secs(5)).await,
            "bob did not connect the new session"
        );

        alice.shutdown().await;
        bob.shutdown().await;
    }

    /// Proves a peer that invited us and then said goodbye is let go rather than chased. Invited
    /// straight back, it could not disconnect from its side: leaving found it back in the session
    /// a second later. The goodbye is recorded as the peer leaving, not as one that stopped
    /// answering.
    #[tokio::test]
    async fn a_peer_that_invited_us_and_said_goodbye_is_let_go() {
        let (alice, _, _) = accepting("Alice", 0).await;
        let (bob, _, mut bob_notices) = accepting("Bob", 0).await;
        assert!(
            alice.connect(loopback(bob.control_port())).await,
            "the supervisor must take the connect command"
        );
        assert!(
            reaches(&bob, ConnectionPhase::Connected, Duration::from_secs(5)).await,
            "the session must connect before the goodbye"
        );

        assert!(
            alice.disconnect().await,
            "the supervisor must take the disconnect command"
        );
        let ended = awaited(&mut bob_notices, Duration::from_secs(2), |notice| {
            matches!(
                notice,
                SessionNotice::Left { .. } | SessionNotice::Lost { .. }
            )
        })
        .await;
        assert!(
            matches!(
                ended,
                Some(SessionNotice::Left {
                    reconnecting: false,
                    ..
                })
            ),
            "a peer that said goodbye must be recorded as leaving and not chased, got {ended:?}"
        );
        assert!(
            reaches(&bob, ConnectionPhase::Disconnected, Duration::from_secs(2)).await,
            "bob must go back to listening, as a session nobody has invited does"
        );
        assert!(
            !reaches(&bob, ConnectionPhase::Connected, Duration::from_secs(4)).await,
            "bob invited alice back"
        );

        alice.shutdown().await;
        bob.shutdown().await;
    }

    /// Proves a session listening after a disconnect reads connected once it accepts the next
    /// invitation. It read "disabled" while listening, and went on reading switched off after it
    /// accepted the next invitation and carried MIDI.
    #[tokio::test]
    async fn a_listening_session_invited_again_says_it_is_connected() {
        let (alice, _, _) = accepting("Alice", 0).await;
        let (bob, _, _) = accepting("Bob", 0).await;
        assert!(
            alice.connect(loopback(bob.control_port())).await,
            "the supervisor must take the connect command"
        );
        assert!(
            reaches(&alice, ConnectionPhase::Connected, Duration::from_secs(5)).await,
            "the first session must connect"
        );
        assert!(
            alice.disconnect().await,
            "the supervisor must take the disconnect command"
        );
        assert!(
            reaches(
                &alice,
                ConnectionPhase::Disconnected,
                Duration::from_secs(2)
            )
            .await,
            "a disconnected session must read disconnected while it listens"
        );

        assert!(
            bob.connect(loopback(alice.control_port())).await,
            "the supervisor must take the connect command"
        );
        assert!(
            reaches(&alice, ConnectionPhase::Connected, Duration::from_secs(5)).await,
            "the listening session accepted an invitation but did not read connected"
        );

        alice.shutdown().await;
        bob.shutdown().await;
    }

    /// Proves a machine that is not the peer cannot end a session with a goodbye, and that its
    /// invitation lets it in beside the peer rather than being refused or taking the peer's place.
    /// The packets are hand-built AppleMIDI, as a hostile or confused machine would send them.
    #[tokio::test]
    async fn another_machine_cannot_end_a_session_and_joins_as_a_guest() {
        let (alice, _, _) = accepting("Alice", 0).await;
        let (bob, _, _) = accepting("Bob", 0).await;
        assert!(
            alice.connect(loopback(bob.control_port())).await,
            "the supervisor must take the connect command"
        );
        assert!(
            reaches(&bob, ConnectionPhase::Connected, Duration::from_secs(5)).await,
            "the session must connect before carol speaks"
        );

        let carol = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("carol's socket binds on loopback");
        let bob_control = loopback(bob.control_port());

        let goodbye = ControlPacket::end_session(1, 0x3333).encode();
        carol
            .send_to(&goodbye, bob_control)
            .await
            .expect("carol's goodbye is sent");
        let invitation = ControlPacket::invitation(7, 0x3333, "Carol").encode();
        carol
            .send_to(&invitation, bob_control)
            .await
            .expect("carol's invitation is sent");
        let mut buffer = [0u8; 512];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), carol.recv_from(&mut buffer))
            .await
            .expect("bob answers carol within two seconds")
            .expect("the answer is received");
        let answer = ControlPacket::parse(buffer.get(..len).unwrap_or_default())
            .expect("bob's answer is a valid control packet");
        assert!(
            matches!(
                answer,
                ControlPacket::Session {
                    command: midi_harbor_rtpmidi::Handshake::Accepted,
                    token: 7,
                    ..
                }
            ),
            "carol's invitation must be accepted under her token, got {answer:?}"
        );

        // Time for a wrongly honoured goodbye to end the session.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let status = bob.status().await;
        assert_eq!(
            (status.state.phase(), status.peer_name.as_deref()),
            (ConnectionPhase::Connected, Some("Alice")),
            "a goodbye from a guest must not end the session or replace its peer"
        );

        alice.shutdown().await;
        bob.shutdown().await;
    }

    /// Proves which stray packets a session with no running session answers with a goodbye:
    /// only what a running session sends (clock sync, an acceptance, RTP-MIDI data), which tells
    /// a restarted peer's partner that the session is gone. Answering an invitation would ask for
    /// a session, and answering a refusal or a goodbye would trade goodbyes forever.
    #[test]
    fn only_what_a_running_session_sends_is_answered_with_a_goodbye() {
        let clock = ControlPacket::ClockSync {
            ssrc: 1,
            count: 0,
            timestamps: [0, 0, 0],
        };
        let cases: [(&str, Vec<u8>, bool); 7] = [
            ("clock sync", clock.encode(), true),
            (
                "an acceptance",
                ControlPacket::accepted(1, 2, "a").encode(),
                true,
            ),
            (
                "RTP-MIDI data",
                RtpMidiPacket::new(1, 0, 2, Vec::new()).encode(),
                true,
            ),
            (
                "an invitation",
                ControlPacket::invitation(1, 2, "a").encode(),
                false,
            ),
            ("a refusal", ControlPacket::rejected(1, 2).encode(), false),
            (
                "a goodbye",
                ControlPacket::end_session(1, 2).encode(),
                false,
            ),
            ("noise", b"noise".to_vec(), false),
        ];
        for (name, bytes, want) in cases {
            assert_eq!(
                is_session_traffic(&bytes),
                want,
                "{name}: answered wrongly with or without a goodbye"
            );
        }
    }

    /// Proves a link lost while there is no network blames the network rather than the peer.
    /// Losing Wi-Fi was recorded as "peer not responding" (R-069). A peer that left or refused is
    /// the peer's doing whatever the network is doing.
    #[test]
    fn a_link_lost_with_no_network_blames_the_network() {
        let cases = [
            (
                SessionFailure::Timeout,
                true,
                FailureReason::NetworkUnreachable,
            ),
            (SessionFailure::Timeout, false, FailureReason::PeerTimeout),
            (
                SessionFailure::Unreachable,
                true,
                FailureReason::NetworkUnreachable,
            ),
            (
                SessionFailure::Unreachable,
                false,
                FailureReason::PeerTimeout,
            ),
            (SessionFailure::PeerLeft, true, FailureReason::PeerTimeout),
            (SessionFailure::Rejected, true, FailureReason::PeerRejected),
        ];
        for (failure, no_network, want) in cases {
            assert_eq!(
                failure_reason(&failure, no_network),
                want,
                "{failure:?} with no_network {no_network}: the wrong failure was blamed"
            );
        }
    }
}
