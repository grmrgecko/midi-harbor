//! The RTP-MIDI session state machine.
//!
//! Pure: it consumes events and produces actions, and never touches a socket or a clock. That is
//! what lets every recovery path be tested by inducing the failure directly rather than by
//! arranging for a real network to misbehave.
//!
//! A session runs over two ports. The control port carries invitations and teardown; the data
//! port one above it carries clock synchronisation and MIDI. Both must be invited separately,
//! which is the part most easily got wrong: a session whose control port is accepted but whose
//! data port is not looks connected and carries nothing.

use crate::clock::{ClockAction, ClockSync};
use crate::control::{ControlPacket, Handshake};
use crate::journal::{Continuity, JournalState, ReceivedState, RecoveryJournal, SequenceTracker};
use crate::packet::{RtpMidiPacket, SysExPart, SysExSegment, TimedMessage};
use midi_harbor_core::midi::{Channel, MidiMessage, silence_channel};
use std::time::Duration;

/// How long to wait for an invitation to be answered before trying again.
pub const INVITE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a probe waits for an answer before the peer is treated as gone.
///
/// Reached only after something told us the link is suspect — a wake, or the machine's addresses
/// changing. The ordinary liveness timeout stays long, because a session that is merely quiet is
/// not a session that is broken.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Most bytes of a dump sent in one packet.
///
/// With the headers and a journal this stays well inside the 1500-byte Ethernet frame, so no
/// packet is fragmented, and inside the 4095 bytes one packet's list can declare.
pub const SYSEX_SEGMENT_BYTES: usize = 1024;

/// Largest dump put back together from a peer's segments, the same bound the daemon puts on one
/// from a port.
///
/// A peer that never ends a dump would otherwise grow the buffer for as long as it sends.
pub const MAX_SYSEX_BYTES: usize = 262_144;

/// How many invitations to send before concluding the peer is not there.
///
/// Reaching this does not end the session. It reports the peer as unreachable so the supervisor
/// can back off, and the supervisor tries again; nothing here ever gives up.
pub const INVITE_ATTEMPTS: u32 = 3;

/// Marks a goodbye sent ahead of sleep, in the token field, "SLEP" in ASCII.
///
/// A peer told only that the session ended cannot tell sleep from a restart, and re-invites; its
/// invitations woke a sleeping Mac on mains power (R-070). Other implementations ignore the
/// token on a goodbye, so to them this is an ordinary one.
pub const SLEEP_GOODBYE_TOKEN: u32 = u32::from_be_bytes(*b"SLEP");

/// Which end of the session this side is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// This side sent the invitation.
    Initiator,
    /// This side accepted one.
    Responder,
}

/// Where a session sits in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Nothing has been attempted.
    Idle,
    /// The control port handshake is in flight.
    ///
    /// The initiator is waiting for its invitation to be answered; the responder is waiting for
    /// the data port invitation to arrive. Both sides pass through the same stages in the same
    /// order, so the phase names describe the stage rather than the role.
    ControlHandshake,
    /// Control is agreed; the data port handshake is in flight.
    DataHandshake,
    /// Both ports accepted; waiting for the first clock exchange.
    Synchronising,
    /// Carrying MIDI.
    Established,
    /// Ended, by either side.
    Closed,
}

impl Phase {
    /// Reports whether MIDI can flow in this phase.
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Established)
    }
}

/// Why a session ended or failed to establish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionFailure {
    /// The peer refused the invitation.
    Rejected,
    /// The peer stopped answering.
    Timeout,
    /// The peer ended the session.
    PeerLeft,
    /// The peer ended the session ahead of sleeping, and reconnects when it wakes.
    PeerAsleep,
    /// The peer did not answer the invitation.
    Unreachable,
}

/// Which socket an outgoing packet belongs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    /// The control port.
    Control,
    /// The data port, one above the control port.
    Data,
}

/// Something the caller must do as a result of an event.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Send this control packet on the given port.
    SendControl {
        /// Which socket to send on.
        port: Port,
        /// The packet to send.
        packet: ControlPacket,
    },
    /// Send this MIDI packet on the data port.
    SendRtp(Box<RtpMidiPacket>),
    /// Deliver these messages to the local endpoint.
    Deliver(Vec<MidiMessage>),
    /// Deliver one whole system-exclusive message, framing included, to the local endpoint.
    DeliverSysEx(Vec<u8>),
    /// The session is now carrying MIDI.
    Established,
    /// The session ended.
    Failed(SessionFailure),
    /// Schedule a wake-up after this delay.
    Wake(Duration),
}

/// One RTP-MIDI session with a peer.
#[derive(Debug)]
pub struct Session {
    role: Role,
    ssrc: u32,
    token: u32,
    name: String,
    phase: Phase,
    peer_ssrc: Option<u32>,
    peer_name: Option<String>,
    clock: ClockSync,
    journal: JournalState,
    tracker: SequenceTracker,
    /// What has been delivered, so recovery repeats none of it.
    received: ReceivedState,
    next_sequence: u16,
    invite_attempts: u32,
    last_invite_ticks: u64,
    last_clock_ticks: u64,
    /// When an outstanding probe gives up on the peer, in wire ticks.
    probe_deadline: Option<u64>,
    /// A dump arriving in segments, framing included, until its last segment arrives.
    sysex_in: Option<Vec<u8>>,
}

impl Session {
    /// Creates a session that will invite a peer.
    pub fn initiator(ssrc: u32, token: u32, name: impl Into<String>) -> Self {
        Self::new(Role::Initiator, ssrc, token, name)
    }

    /// Creates a session that has accepted an invitation.
    pub fn responder(ssrc: u32, token: u32, name: impl Into<String>) -> Self {
        Self::new(Role::Responder, ssrc, token, name)
    }

    /// Creates a session in the idle phase.
    fn new(role: Role, ssrc: u32, token: u32, name: impl Into<String>) -> Self {
        Self {
            role,
            ssrc,
            token,
            name: name.into(),
            phase: Phase::Idle,
            peer_ssrc: None,
            peer_name: None,
            clock: ClockSync::new(),
            journal: JournalState::new(),
            tracker: SequenceTracker::new(),
            received: ReceivedState::new(),
            // Streams start at a random-looking sequence, derived from the token so a session is
            // reproducible in tests without being predictable across sessions.
            next_sequence: u16::try_from(token & 0xFFFF).unwrap_or(0),
            invite_attempts: 0,
            last_invite_ticks: 0,
            last_clock_ticks: 0,
            probe_deadline: None,
            sysex_in: None,
        }
    }

    /// Returns the current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Returns which end of the session this side is.
    ///
    /// The supervisor chases a peer that went quiet, whichever end invited. A peer that invited us
    /// and then said goodbye is let go: it chose to leave, and knows where we are.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Returns the peer's advertised name, once known.
    pub fn peer_name(&self) -> Option<&str> {
        self.peer_name.as_deref()
    }

    /// Returns the clock estimate, which carries the round trip and liveness.
    pub fn clock(&self) -> &ClockSync {
        &self.clock
    }

    /// Returns how many messages the journal has rebuilt.
    pub fn recovered(&self) -> u64 {
        self.tracker.recovered()
    }

    /// Returns how many packets have been lost.
    pub fn lost(&self) -> u64 {
        self.tracker.lost()
    }

    /// Starts the session by inviting the peer on the control port.
    pub fn start(&mut self, now_ticks: u64) -> Vec<Action> {
        self.phase = Phase::ControlHandshake;
        self.invite_attempts = 1;
        self.last_invite_ticks = now_ticks;
        vec![
            Action::SendControl {
                port: Port::Control,
                packet: ControlPacket::invitation(self.token, self.ssrc, self.name.clone()),
            },
            Action::Wake(INVITE_TIMEOUT),
        ]
    }

    /// Handles a control packet arriving on one of the two ports.
    pub fn on_control(
        &mut self,
        port: Port,
        packet: &ControlPacket,
        now_ticks: u64,
    ) -> Vec<Action> {
        match packet {
            ControlPacket::Session {
                command: Handshake::Accepted,
                ssrc,
                name,
                ..
            } => self.on_accepted(port, *ssrc, name.clone(), now_ticks),
            ControlPacket::Session {
                command: Handshake::Rejected,
                ..
            } => {
                self.phase = Phase::Closed;
                vec![Action::Failed(SessionFailure::Rejected)]
            }
            ControlPacket::Session {
                command: Handshake::EndSession,
                token,
                ..
            } => {
                self.phase = Phase::Closed;
                let failure = if *token == SLEEP_GOODBYE_TOKEN {
                    SessionFailure::PeerAsleep
                } else {
                    SessionFailure::PeerLeft
                };
                // A peer leaving can strand anything it was holding, so silence before stopping.
                vec![Action::Deliver(all_notes_off()), Action::Failed(failure)]
            }
            ControlPacket::Session {
                command: Handshake::Invitation,
                ssrc,
                name,
                token,
            } => self.on_invited(port, *ssrc, name.clone(), *token, now_ticks),
            ControlPacket::ClockSync { .. } => self.on_clock(packet, now_ticks),
            ControlPacket::ReceiverFeedback { ssrc, acknowledged } => {
                // The peer reporting what it has is what lets the journal shrink. Feedback from
                // any other source describes another session's packets.
                if self.peer_ssrc == Some(*ssrc)
                    && let Some(sequence) = self.acknowledged(*acknowledged)
                {
                    self.journal.trim(sequence);
                }
                Vec::new()
            }
        }
    }

    /// Accepts an invitation, advancing the handshake as each port is agreed.
    ///
    /// The acceptance echoes the peer's token, not ours: a peer cannot match a reply to its own
    /// request otherwise.
    fn on_invited(
        &mut self,
        port: Port,
        peer_ssrc: u32,
        peer_name: Option<String>,
        peer_token: u32,
        now_ticks: u64,
    ) -> Vec<Action> {
        // A control invitation under a new SSRC once the handshake is over is the peer starting
        // again after a restart. Whatever it held before is lost with it, so this session ends
        // silenced, and the invitation is left to begin the next one. A repeat under the same
        // SSRC is an acceptance that went missing, and is answered again below.
        let running = matches!(self.phase, Phase::Synchronising | Phase::Established);
        if running && port == Port::Control && self.peer_ssrc != Some(peer_ssrc) {
            self.phase = Phase::Closed;
            return vec![
                Action::Deliver(all_notes_off()),
                Action::Failed(SessionFailure::PeerLeft),
            ];
        }

        self.peer_ssrc = Some(peer_ssrc);
        if peer_name.is_some() {
            self.peer_name = peer_name;
        }

        let mut actions = vec![Action::SendControl {
            port,
            packet: ControlPacket::accepted(peer_token, self.ssrc, self.name.clone()),
        }];

        match (self.phase, port) {
            // Control agreed, so the data port invitation is what comes next.
            (Phase::Idle | Phase::ControlHandshake, Port::Control) => {
                self.phase = Phase::DataHandshake;
                self.last_invite_ticks = now_ticks;
            }
            // Data agreed. The initiator opens the clock exchange; a responder waits to be
            // asked, so that both sides do not open one at once.
            (Phase::DataHandshake, Port::Data) => {
                self.phase = Phase::Synchronising;
                self.last_clock_ticks = now_ticks;
                if self.role == Role::Initiator {
                    actions.push(Action::SendControl {
                        port: Port::Data,
                        packet: self.clock.begin(self.ssrc, now_ticks),
                    });
                }
            }
            _ => {}
        }
        actions
    }

    /// Handles an acceptance on either port.
    fn on_accepted(
        &mut self,
        port: Port,
        peer_ssrc: u32,
        peer_name: Option<String>,
        now_ticks: u64,
    ) -> Vec<Action> {
        self.peer_ssrc = Some(peer_ssrc);
        if peer_name.is_some() {
            self.peer_name = peer_name;
        }

        match (self.phase, port) {
            // Control accepted, so invite the data port. A session that stops here looks
            // connected and carries nothing.
            (Phase::ControlHandshake, Port::Control) => {
                self.phase = Phase::DataHandshake;
                self.invite_attempts = 1;
                self.last_invite_ticks = now_ticks;
                vec![
                    Action::SendControl {
                        port: Port::Data,
                        packet: ControlPacket::invitation(self.token, self.ssrc, self.name.clone()),
                    },
                    Action::Wake(INVITE_TIMEOUT),
                ]
            }
            // Data accepted, so begin synchronising.
            (Phase::DataHandshake, Port::Data) => {
                self.phase = Phase::Synchronising;
                self.last_clock_ticks = now_ticks;
                vec![
                    Action::SendControl {
                        port: Port::Data,
                        packet: self.clock.begin(self.ssrc, now_ticks),
                    },
                    Action::Wake(self.clock.next_interval()),
                ]
            }
            _ => Vec::new(),
        }
    }

    /// Handles a clock packet, and reports the session established once one exchange completes.
    fn on_clock(&mut self, packet: &ControlPacket, now_ticks: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        match self.clock.handle(packet, self.ssrc, now_ticks) {
            ClockAction::Reply(reply) => {
                // The peer answered, so whatever made the link look suspect did not break it.
                self.probe_deadline = None;
                actions.push(Action::SendControl {
                    port: Port::Data,
                    packet: reply,
                });
            }
            ClockAction::Completed => self.probe_deadline = None,
            ClockAction::Ignored => {}
        }

        if self.clock.is_established() {
            self.last_clock_ticks = now_ticks;
            if matches!(self.phase, Phase::Synchronising | Phase::DataHandshake) {
                self.phase = Phase::Established;
                actions.push(Action::Established);
            }
        }
        actions
    }

    /// Handles an RTP-MIDI packet, applying the journal when packets were missed.
    pub fn on_rtp(&mut self, packet: &RtpMidiPacket) -> Vec<Action> {
        // Only the source agreed in the handshake is this session's peer. A peer that restarted
        // its session keeps sending from the old one for a moment, and those packets, taken as
        // this session's, set its sequence so far ahead that everything the new one sends looks
        // late and is dropped (R-070).
        if self.peer_ssrc != Some(packet.ssrc) {
            return Vec::new();
        }
        let mut delivered = Vec::new();
        let continuity = self.tracker.observe(packet.sequence);

        // A packet older than one already handled is either a repeat or arrived out of order,
        // after a newer packet's journal already rebuilt what it carried. Played now, it would
        // replay old state over new: a late note on arriving after its note off leaves the note
        // sounding.
        if continuity == Continuity::Duplicate {
            return Vec::new();
        }

        // A gap means packets were lost, so rebuild what they carried before delivering what
        // arrived. Doing it the other way round would replay stale state over fresh messages.
        if let Continuity::Gap { .. } = continuity
            && let Some(bytes) = &packet.journal
            && let Ok(journal) = RecoveryJournal::decode(bytes)
        {
            let repairs: Vec<MidiMessage> = journal
                .recover()
                .into_iter()
                .filter(|repair| self.received.changes(repair))
                .collect();
            self.tracker.record_recovered(repairs.len() as u64);
            for repair in &repairs {
                self.received.observe(repair);
            }
            delivered.extend(repairs);
        }
        // A lost packet may have carried part of a dump, and part of a dump is never delivered.
        if matches!(continuity, Continuity::Gap { .. }) {
            self.sysex_in = None;
        }

        // Deliver in the order the peer sent: messages up to a dump, the dump, then the rest.
        let mut actions = Vec::new();
        let count = packet.messages.len();
        for index in 0..=count {
            for segment in packet.sysex.iter().filter(|segment| {
                segment.position == index || (index == count && segment.position > count)
            }) {
                if let Some(dump) = self.reassemble(segment) {
                    if !delivered.is_empty() {
                        actions.push(Action::Deliver(std::mem::take(&mut delivered)));
                    }
                    actions.push(Action::DeliverSysEx(dump));
                }
            }
            if let Some(timed) = packet.messages.get(index) {
                self.received.observe(&timed.message);
                delivered.push(timed.message);
            }
        }
        if !delivered.is_empty() {
            actions.push(Action::Deliver(delivered));
        }
        // Telling the peer what we have is what trims its journal. Apple sends this on the
        // control port, so ours goes there too.
        if let Some(highest) = self.tracker.highest() {
            actions.push(Action::SendControl {
                port: Port::Control,
                packet: ControlPacket::feedback(self.ssrc, highest),
            });
        }
        actions
    }

    /// Adds one segment to the dump being put back together, returning the dump once whole.
    fn reassemble(&mut self, segment: &SysExSegment) -> Option<Vec<u8>> {
        match segment.part {
            SysExPart::Whole => {
                self.sysex_in = None;
                let mut dump = Vec::with_capacity(segment.payload.len() + 2);
                dump.push(0xF0);
                dump.extend_from_slice(&segment.payload);
                dump.push(0xF7);
                (dump.len() <= MAX_SYSEX_BYTES).then_some(dump)
            }
            SysExPart::First => {
                let mut dump = Vec::with_capacity(segment.payload.len() + 1);
                dump.push(0xF0);
                dump.extend_from_slice(&segment.payload);
                self.sysex_in = (dump.len() < MAX_SYSEX_BYTES).then_some(dump);
                None
            }
            SysExPart::Middle => {
                // A middle with no first is the rest of a dump whose start was lost.
                let dump = self.sysex_in.as_mut()?;
                dump.extend_from_slice(&segment.payload);
                if dump.len() >= MAX_SYSEX_BYTES {
                    self.sysex_in = None;
                }
                None
            }
            SysExPart::Last => {
                let mut dump = self.sysex_in.take()?;
                dump.extend_from_slice(&segment.payload);
                dump.push(0xF7);
                (dump.len() <= MAX_SYSEX_BYTES).then_some(dump)
            }
            SysExPart::Cancelled => {
                self.sysex_in = None;
                None
            }
        }
    }

    /// Queues one whole system-exclusive message for the peer, returning the packets to send.
    ///
    /// A dump longer than one packet should carry is divided into segments, one per packet, in
    /// the framing RFC 6295 (3.2) gives them. Returns nothing when the session is not carrying
    /// MIDI, or when `dump` is not framed by `F0` and `F7`.
    pub fn send_sysex(&mut self, dump: &[u8], now_ticks: u64) -> Vec<Action> {
        if !self.phase.is_usable() {
            return Vec::new();
        }
        let Some(payload) = dump
            .strip_prefix(&[0xF0])
            .and_then(|rest| rest.strip_suffix(&[0xF7]))
        else {
            return Vec::new();
        };

        let segments: Vec<&[u8]> = if payload.is_empty() {
            vec![payload]
        } else {
            payload.chunks(SYSEX_SEGMENT_BYTES).collect()
        };
        let last = segments.len().saturating_sub(1);
        segments
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| {
                let part = match (index == 0, index == last) {
                    (true, true) => SysExPart::Whole,
                    (true, false) => SysExPart::First,
                    (false, false) => SysExPart::Middle,
                    (false, true) => SysExPart::Last,
                };
                let mut packet = self.next_packet(Vec::new(), now_ticks);
                packet.sysex.push(SysExSegment {
                    position: 0,
                    delta: 0,
                    part,
                    payload: chunk.to_vec(),
                });
                Action::SendRtp(Box::new(packet))
            })
            .collect()
    }

    /// Starts the next packet: its sequence number, and a journal describing the ones before it.
    ///
    /// The journal is taken before this packet's messages join it (RFC 6295, section 4). Taken
    /// after, it described its own packet too, and a receiver recovering from a gap played that
    /// packet's notes twice.
    fn next_packet(&mut self, timed: Vec<TimedMessage>, now_ticks: u64) -> RtpMidiPacket {
        self.next_sequence = self.next_sequence.wrapping_add(1);
        let journal = self.journal.build().map(|j| j.encode());
        let mut packet = RtpMidiPacket::new(
            self.next_sequence,
            u32::try_from(now_ticks & u64::from(u32::MAX)).unwrap_or(0),
            self.ssrc,
            timed,
        );
        packet.journal = journal;
        packet
    }

    /// Queues messages for the peer, returning the packet to send.
    ///
    /// Returns nothing when the session is not carrying MIDI, so messages are dropped rather than
    /// queued unboundedly while a link is down.
    pub fn send(&mut self, messages: &[MidiMessage], now_ticks: u64) -> Vec<Action> {
        if !self.phase.is_usable() || messages.is_empty() {
            return Vec::new();
        }
        let timed: Vec<TimedMessage> = messages
            .iter()
            .map(|m| TimedMessage::immediate(*m))
            .collect();
        let packet = self.next_packet(timed, now_ticks);
        for message in messages {
            self.journal.observe(message, packet.sequence);
        }
        vec![Action::SendRtp(Box::new(packet))]
    }

    /// Advances time, resending invitations and clock exchanges as they come due.
    ///
    /// This is where a silent disconnection is noticed: a peer that stops answering clock
    /// exchanges is detected here and nowhere else.
    pub fn tick(&mut self, now_ticks: u64) -> Vec<Action> {
        match self.phase {
            Phase::ControlHandshake | Phase::DataHandshake => self.tick_inviting(now_ticks),
            Phase::Synchronising | Phase::Established => self.tick_running(now_ticks),
            Phase::Idle | Phase::Closed => Vec::new(),
        }
    }

    /// Resends an unanswered invitation, or reports the peer unreachable.
    ///
    /// A responder sends no invitations, so it waits rather than retrying. A peer that invited us
    /// and then went quiet is the peer's problem to retry; it knows where we are.
    fn tick_inviting(&mut self, now_ticks: u64) -> Vec<Action> {
        if self.role == Role::Responder {
            return Vec::new();
        }
        let elapsed = crate::clock::duration_from(now_ticks.saturating_sub(self.last_invite_ticks));
        if elapsed < INVITE_TIMEOUT {
            return Vec::new();
        }

        if self.invite_attempts >= INVITE_ATTEMPTS {
            self.phase = Phase::Closed;
            return vec![Action::Failed(SessionFailure::Unreachable)];
        }

        self.invite_attempts = self.invite_attempts.saturating_add(1);
        self.last_invite_ticks = now_ticks;
        let port = if self.phase == Phase::ControlHandshake {
            Port::Control
        } else {
            Port::Data
        };
        vec![
            Action::SendControl {
                port,
                packet: ControlPacket::invitation(self.token, self.ssrc, self.name.clone()),
            },
            Action::Wake(INVITE_TIMEOUT),
        ]
    }

    /// Asks the peer to prove the link still works, and stops waiting long for an answer.
    ///
    /// A session that has been suspended, or whose machine changed address, looks exactly like a
    /// working one until the next exchange fails — which the ordinary liveness timeout notices
    /// thirty-five seconds later. This is for when something already said the link is suspect:
    /// ask now, and give up in seconds rather than half a minute.
    pub fn probe(&mut self, now_ticks: u64) -> Vec<Action> {
        if self.phase != Phase::Established {
            return Vec::new();
        }
        // Only the first probe sets the deadline, so repeated hints cannot keep pushing it out.
        if self.probe_deadline.is_none() {
            self.probe_deadline =
                Some(now_ticks.saturating_add(crate::clock::ticks_from(PROBE_TIMEOUT)));
        }
        self.last_clock_ticks = now_ticks;
        vec![Action::SendControl {
            port: Port::Data,
            packet: self.clock.begin(self.ssrc, now_ticks),
        }]
    }

    /// Keeps the clock exchange running, and notices a peer that has stopped answering.
    fn tick_running(&mut self, now_ticks: u64) -> Vec<Action> {
        // A probe that went unanswered is the peer being gone, found in seconds rather than in
        // the thirty-five the ordinary timeout takes.
        if self
            .probe_deadline
            .is_some_and(|deadline| now_ticks >= deadline)
        {
            self.probe_deadline = None;
            self.phase = Phase::Closed;
            return vec![
                Action::Deliver(all_notes_off()),
                Action::Failed(SessionFailure::Timeout),
            ];
        }

        if !self.clock.is_alive(now_ticks) {
            self.phase = Phase::Closed;
            // A peer that vanished mid-phrase can leave notes sounding here, which is the exact
            // failure this product exists to prevent.
            return vec![
                Action::Deliver(all_notes_off()),
                Action::Failed(SessionFailure::Timeout),
            ];
        }

        let due = crate::clock::ticks_from(self.clock.next_interval());
        if now_ticks.saturating_sub(self.last_clock_ticks) < due {
            return Vec::new();
        }
        self.last_clock_ticks = now_ticks;
        vec![
            Action::SendControl {
                port: Port::Data,
                packet: self.clock.begin(self.ssrc, now_ticks),
            },
            Action::Wake(self.clock.next_interval()),
        ]
    }

    /// Returns the sequence number a feedback field acknowledges, if it names a packet this side
    /// has sent since the journal's checkpoint.
    ///
    /// Apple writes the number in the upper half and leaves the lower half unspecified; rtpmidid
    /// writes the lower half and leaves the upper zero (R-071). Read as Apple's alone, rtpmidid's
    /// feedback named packet 0: ignored as stale, the journal was never trimmed, and otherwise the
    /// checkpoint went back to 1. Whichever half names a packet actually sent is the
    /// acknowledgement, the upper tried first.
    fn acknowledged(&self, field: u32) -> Option<u16> {
        let [a, b, c, d] = field.to_be_bytes();
        [u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d])]
            .into_iter()
            .find(|sequence| self.journal.acknowledges(*sequence))
    }

    /// Ends the session, telling the peer on both ports.
    pub fn close(&mut self) -> Vec<Action> {
        self.end(self.token)
    }

    /// Ends the session ahead of sleep, telling the peer to wait rather than invite again.
    pub fn close_for_sleep(&mut self) -> Vec<Action> {
        self.end(SLEEP_GOODBYE_TOKEN)
    }

    /// Ends the session, telling the peer on both ports under `token`.
    fn end(&mut self, token: u32) -> Vec<Action> {
        if self.phase == Phase::Closed {
            return Vec::new();
        }
        self.phase = Phase::Closed;
        let goodbye = ControlPacket::end_session(token, self.ssrc);
        vec![
            Action::Deliver(all_notes_off()),
            Action::SendControl {
                port: Port::Data,
                packet: goodbye.clone(),
            },
            Action::SendControl {
                port: Port::Control,
                packet: goodbye,
            },
        ]
    }
}

/// Returns the messages that silence every channel.
///
/// Sent whenever a session ends for any reason, because a peer that vanished mid-phrase leaves
/// notes sounding that nothing else will stop.
fn all_notes_off() -> Vec<MidiMessage> {
    Channel::all().flat_map(silence_channel).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{LIVENESS_TIMEOUT, ticks_from};

    const OUR_SSRC: u32 = 0x1111;
    const PEER_SSRC: u32 = 0x2222;
    const TOKEN: u32 = 0xABCD;

    /// When [`established`] completes its clock exchange, in wire ticks.
    const ESTABLISHED_AT: u64 = 40;

    fn note_on(note: u8) -> MidiMessage {
        MidiMessage::NoteOn {
            channel: Channel::new(0).expect("channel 1 is in range"),
            note,
            velocity: 100,
        }
    }

    fn note_off(note: u8) -> MidiMessage {
        MidiMessage::NoteOff {
            channel: Channel::new(0).expect("channel 1 is in range"),
            note,
            velocity: 0,
        }
    }

    /// Returns a peer's answer to the clock exchange a session opened at `opened`.
    fn clock_answer(opened: u64, now: u64) -> ControlPacket {
        let opening = ControlPacket::ClockSync {
            ssrc: OUR_SSRC,
            count: 0,
            timestamps: [opened, 0, 0],
        };
        match ClockSync::new().handle(&opening, PEER_SSRC, now) {
            ClockAction::Reply(packet) => packet,
            other => panic!("a peer must answer an opening clock packet, got {other:?}"),
        }
    }

    /// Returns an initiator that has invited the peer at tick 0 and heard nothing back.
    fn inviting() -> Session {
        let mut session = Session::initiator(OUR_SSRC, TOKEN, "Studio");
        let _ = session.start(0);
        session
    }

    /// Returns an initiator whose handshake and first clock exchange completed at
    /// [`ESTABLISHED_AT`].
    fn established() -> Session {
        let mut session = inviting();
        let accept = ControlPacket::accepted(TOKEN, PEER_SSRC, "Stage");
        let _ = session.on_control(Port::Control, &accept, 10);
        let _ = session.on_control(Port::Data, &accept, 20);
        let _ = session.on_control(Port::Data, &clock_answer(20, 30), ESTABLISHED_AT);
        assert_eq!(
            session.phase(),
            Phase::Established,
            "a completed handshake and clock exchange must establish the session"
        );
        session
    }

    /// Returns a responder that has accepted the peer's control port invitation at tick 0.
    fn responding() -> Session {
        let mut session = Session::responder(OUR_SSRC, TOKEN, "Studio");
        let _ = session.on_control(
            Port::Control,
            &ControlPacket::invitation(0x9999, PEER_SSRC, "Stage"),
            0,
        );
        session
    }

    /// Returns what a list of actions delivers, flattened in delivery order.
    fn delivered(actions: &[Action]) -> Vec<MidiMessage> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Deliver(messages) => Some(messages.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// Returns the journal carried by the one packet a list of actions sends.
    fn sent_journal(actions: &[Action]) -> Option<RecoveryJournal> {
        let Some(Action::SendRtp(packet)) = actions.first() else {
            panic!("an established session must send a packet, got {actions:?}");
        };
        packet.journal.as_deref().map(|bytes| {
            RecoveryJournal::decode(bytes).expect("a journal this side encoded must decode")
        })
    }

    /// Every way a session ends reports why, and silences first whenever MIDI was flowing.
    ///
    /// A peer that leaves or vanishes mid-phrase strands every note it was holding, so those
    /// endings deliver all-notes-off before anything else; one that never connected has nothing
    /// sounding. An unanswered invitation is resent until `INVITE_ATTEMPTS = 3` have gone
    /// unanswered, each after `INVITE_TIMEOUT`, and then reported unreachable so the supervisor
    /// decides what next. A peer silent for the whole `LIVENESS_TIMEOUT` is gone, and one tick
    /// short of it is not. A probe, sent when a wake or address change makes the link suspect,
    /// gives up after `PROBE_TIMEOUT`, two seconds rather than thirty-five, to meet SC-004's
    /// fifteen-second wake budget; a later hint must not push that deadline out, or a stream of
    /// hints would keep a dead peer alive, and an answered probe keeps the session.
    #[test]
    fn every_way_a_session_ends_says_why() {
        struct Case {
            name: &'static str,
            start: fn() -> Session,
            drive: fn(&mut Session) -> Vec<Action>,
            want_phase: Phase,
            want_failure: Option<SessionFailure>,
            want_silenced: bool,
        }
        let cases = [
            Case {
                name: "the peer rejects the invitation",
                start: inviting,
                drive: |session| {
                    session.on_control(
                        Port::Control,
                        &ControlPacket::rejected(TOKEN, PEER_SSRC),
                        10,
                    )
                },
                want_phase: Phase::Closed,
                want_failure: Some(SessionFailure::Rejected),
                want_silenced: false,
            },
            Case {
                name: "two invitations short of the limit",
                start: inviting,
                drive: |session| {
                    let step = ticks_from(INVITE_TIMEOUT);
                    (1..INVITE_ATTEMPTS)
                        .flat_map(|attempt| session.tick(step * u64::from(attempt)))
                        .collect()
                },
                want_phase: Phase::ControlHandshake,
                want_failure: None,
                want_silenced: false,
            },
            Case {
                name: "the last invitation goes unanswered",
                start: inviting,
                drive: |session| {
                    let step = ticks_from(INVITE_TIMEOUT);
                    (1..=INVITE_ATTEMPTS)
                        .flat_map(|attempt| session.tick(step * u64::from(attempt)))
                        .collect()
                },
                want_phase: Phase::Closed,
                want_failure: Some(SessionFailure::Unreachable),
                want_silenced: false,
            },
            Case {
                name: "the peer says goodbye",
                start: established,
                drive: |session| {
                    session.on_control(
                        Port::Control,
                        &ControlPacket::end_session(TOKEN, PEER_SSRC),
                        100,
                    )
                },
                want_phase: Phase::Closed,
                want_failure: Some(SessionFailure::PeerLeft),
                want_silenced: true,
            },
            Case {
                name: "the peer is quiet one tick short of the liveness timeout",
                start: established,
                drive: |session| session.tick(ESTABLISHED_AT + ticks_from(LIVENESS_TIMEOUT) - 1),
                want_phase: Phase::Established,
                want_failure: None,
                want_silenced: false,
            },
            Case {
                name: "the peer is quiet for the whole liveness timeout",
                start: established,
                drive: |session| session.tick(ESTABLISHED_AT + ticks_from(LIVENESS_TIMEOUT)),
                want_phase: Phase::Closed,
                want_failure: Some(SessionFailure::Timeout),
                want_silenced: true,
            },
            Case {
                name: "a probe goes unanswered",
                start: established,
                drive: |session| {
                    let _ = session.probe(100);
                    session.tick(100 + ticks_from(PROBE_TIMEOUT))
                },
                want_phase: Phase::Closed,
                want_failure: Some(SessionFailure::Timeout),
                want_silenced: true,
            },
            Case {
                name: "a second hint arrives just before the probe's deadline",
                start: established,
                drive: |session| {
                    let deadline = 100 + ticks_from(PROBE_TIMEOUT);
                    let _ = session.probe(100);
                    let _ = session.probe(deadline - 10);
                    session.tick(deadline)
                },
                want_phase: Phase::Closed,
                want_failure: Some(SessionFailure::Timeout),
                want_silenced: true,
            },
            Case {
                name: "a probe is answered",
                start: established,
                drive: |session| {
                    let _ = session.probe(100);
                    let _ = session.on_control(Port::Data, &clock_answer(100, 110), 120);
                    session.tick(100 + ticks_from(PROBE_TIMEOUT) + 1)
                },
                want_phase: Phase::Established,
                want_failure: None,
                want_silenced: false,
            },
        ];
        for case in cases {
            let mut session = (case.start)();
            let actions = (case.drive)(&mut session);
            let failure = actions.iter().find_map(|action| match action {
                Action::Failed(failure) => Some(failure.clone()),
                _ => None,
            });

            assert_eq!(
                session.phase(),
                case.want_phase,
                "{}: the session must be left in this phase",
                case.name
            );
            assert_eq!(
                failure, case.want_failure,
                "{}: the session must report this reason, got {actions:?}",
                case.name
            );
            assert_eq!(
                actions.first() == Some(&Action::Deliver(all_notes_off())),
                case.want_silenced,
                "{}: silencing must come first exactly when MIDI was flowing, got {actions:?}",
                case.name
            );
        }
    }

    /// An invitation is answered with an acceptance echoing the peer's token, unless it shows the
    /// peer has restarted.
    ///
    /// Apple's session protocol matches a reply to its request by the initiator's token, so
    /// echoing our own would leave the peer waiting. A repeat under the SSRC already agreed is an
    /// acceptance that went missing, and is answered again. A control invitation under a new SSRC
    /// once the session runs is the peer starting over after a restart; whatever it held is lost
    /// with it, so the old session ends silenced and the peer will never release those notes.
    #[test]
    fn an_invitation_is_answered_by_where_the_session_stands() {
        let accepted = vec![Action::SendControl {
            port: Port::Control,
            packet: ControlPacket::accepted(0x9999, OUR_SSRC, "Studio"),
        }];
        let cases = [
            (
                "a first invitation to a responder",
                false,
                PEER_SSRC,
                accepted.clone(),
                Phase::DataHandshake,
            ),
            (
                "a repeat from the peer already agreed",
                true,
                PEER_SSRC,
                accepted,
                Phase::Established,
            ),
            (
                "an invitation under a new SSRC",
                true,
                PEER_SSRC + 1,
                vec![
                    Action::Deliver(all_notes_off()),
                    Action::Failed(SessionFailure::PeerLeft),
                ],
                Phase::Closed,
            ),
        ];
        for (name, running, ssrc, want, want_phase) in cases {
            let mut session = if running {
                established()
            } else {
                Session::responder(OUR_SSRC, TOKEN, "Studio")
            };
            let actions = session.on_control(
                Port::Control,
                &ControlPacket::invitation(0x9999, ssrc, "Stage"),
                100,
            );
            assert_eq!(
                actions, want,
                "{name}: the invitation must be answered this way"
            );
            assert_eq!(
                session.phase(),
                want_phase,
                "{name}: the session must be left in this phase"
            );
        }
    }

    /// A responder only answers: it never retries or times out a handshake, and never opens the
    /// clock exchange.
    ///
    /// The peer that invited us knows where we are and retries itself, so chasing it is not our
    /// job. The initiator opens the clock exchange; both sides opening one at once doubles the
    /// packets and interleaves two exchanges.
    #[test]
    fn a_responder_only_answers() {
        let mut session = responding();

        assert!(
            session.tick(ticks_from(INVITE_TIMEOUT) * 100).is_empty(),
            "a responder must not retry or give up on a handshake the peer drives"
        );
        assert_eq!(
            session.phase(),
            Phase::DataHandshake,
            "a responder must still be waiting for the data port invitation"
        );

        let actions = session.on_control(
            Port::Data,
            &ControlPacket::invitation(0x9999, PEER_SSRC, "Stage"),
            10,
        );
        assert_eq!(
            actions,
            vec![Action::SendControl {
                port: Port::Data,
                packet: ControlPacket::accepted(0x9999, OUR_SSRC, "Studio"),
            }],
            "a responder must accept the data port and open no clock exchange"
        );
    }

    /// Closing silences, then says goodbye on both ports, once.
    ///
    /// A goodbye on only one port leaves the peer holding half a session; Apple's Network MIDI
    /// kept such a session listed and sent its MIDI there (R-068). A second close has nothing
    /// left to say.
    #[test]
    fn closing_says_goodbye_on_both_ports_once() {
        let mut session = established();
        let goodbye = ControlPacket::end_session(TOKEN, OUR_SSRC);

        assert_eq!(
            session.close(),
            vec![
                Action::Deliver(all_notes_off()),
                Action::SendControl {
                    port: Port::Data,
                    packet: goodbye.clone(),
                },
                Action::SendControl {
                    port: Port::Control,
                    packet: goodbye,
                },
            ],
            "closing must silence and then tell the peer on both ports"
        );
        assert!(
            session.close().is_empty(),
            "a closed session must not say goodbye again"
        );
    }

    /// The peer's receiver feedback trims the journal from whichever half of the field names a
    /// packet this side sent, and only when it comes from the peer.
    ///
    /// Apple writes the sequence number in the upper sixteen bits and leaves the lower half
    /// unspecified; rtpmidid writes the lower half and leaves the upper zero (R-071). Read as
    /// Apple's alone, rtpmidid's named packet 0, was ignored as stale, and the journal never
    /// shrank. Acknowledging the second of three packets moves the checkpoint to just past it;
    /// feedback naming nothing sent, or from another source, leaves it at the first.
    #[test]
    fn the_peers_feedback_trims_the_journal_from_either_half() {
        struct Case {
            name: &'static str,
            ssrc: u32,
            field: fn(u16) -> u32,
            trims: bool,
        }
        let cases = [
            Case {
                name: "Apple's, with junk in the lower half",
                ssrc: PEER_SSRC,
                field: |sequence| u32::from(sequence) << 16 | 0x907C,
                trims: true,
            },
            Case {
                name: "rtpmidid's, in the lower half",
                ssrc: PEER_SSRC,
                field: u32::from,
                trims: true,
            },
            Case {
                name: "naming no packet sent",
                ssrc: PEER_SSRC,
                field: |sequence| u32::from(sequence.wrapping_add(0x4000)) * 0x1_0001,
                trims: false,
            },
            Case {
                name: "from another source",
                ssrc: PEER_SSRC + 1,
                field: |sequence| u32::from(sequence) << 16,
                trims: false,
            },
        ];
        for Case {
            name,
            ssrc,
            field,
            trims,
        } in cases
        {
            let mut session = established();
            let sent: Vec<u16> = [60, 62, 64]
                .into_iter()
                .filter_map(|note| match session.send(&[note_on(note)], 0).first() {
                    Some(Action::SendRtp(packet)) => Some(packet.sequence),
                    _ => None,
                })
                .collect();

            let feedback = ControlPacket::ReceiverFeedback {
                ssrc,
                acknowledged: field(sent[1]),
            };
            let _ = session.on_control(Port::Control, &feedback, 0);
            let journal = sent_journal(&session.send(&[note_on(67)], 0))
                .expect("notes still sounding must be protected");

            let want_checkpoint = if trims {
                sent[1].wrapping_add(1)
            } else {
                sent[0]
            };
            assert_eq!(
                journal.checkpoint_seqnum, want_checkpoint,
                "{name}: the checkpoint must move just past an acknowledged packet and no further"
            );
        }
    }

    /// Receiving MIDI tells the peer, on the control port, the highest sequence number seen, in
    /// the upper half of the field.
    ///
    /// That is where and how Apple's Network MIDI sends it and reads it back, and rtpmidid also
    /// reads the upper half (R-071). Packet 500 is `0x01F4`, so the field is `0x01F4_0000`.
    #[test]
    fn receiving_midi_acknowledges_it_as_apple_does() {
        let mut session = established();
        let actions = session.on_rtp(&RtpMidiPacket::new(
            500,
            0,
            PEER_SSRC,
            vec![TimedMessage::immediate(note_on(60))],
        ));

        assert_eq!(
            actions.last(),
            Some(&Action::SendControl {
                port: Port::Control,
                packet: ControlPacket::ReceiverFeedback {
                    ssrc: OUR_SSRC,
                    acknowledged: 0x01F4_0000,
                },
            }),
            "feedback must go on the control port with the sequence in the upper half"
        );
    }

    /// A packet that carries nothing new is not played: one from a source other than the agreed
    /// peer, one older than a packet already handled, or a repeat.
    ///
    /// A restarted peer keeps sending from its old session for a moment; taken as this session's,
    /// packet 40 000 set the sequence so far ahead that everything the new one sent looked late
    /// and was dropped (R-070). A late packet arrives after a newer one whose journal already
    /// covered it, and its note on, played after the note off that followed it, leaves the note
    /// sounding.
    #[test]
    fn a_packet_with_nothing_new_is_not_played() {
        let cases = [
            (
                "a packet from another source",
                vec![
                    (PEER_SSRC + 1, 40_000, note_on(60), false),
                    (PEER_SSRC, 20_000, note_on(62), true),
                ],
            ),
            (
                "a packet older than the newest",
                vec![
                    (PEER_SSRC, 100, note_on(60), true),
                    (PEER_SSRC, 102, note_off(60), true),
                    (PEER_SSRC, 101, note_on(60), false),
                ],
            ),
            (
                "a repeat of the newest",
                vec![
                    (PEER_SSRC, 102, note_off(60), true),
                    (PEER_SSRC, 102, note_off(60), false),
                ],
            ),
        ];
        for (name, packets) in cases {
            let mut session = established();
            for (index, (ssrc, sequence, message, want_played)) in packets.into_iter().enumerate() {
                let actions = session.on_rtp(&RtpMidiPacket::new(
                    sequence,
                    0,
                    ssrc,
                    vec![TimedMessage::immediate(message)],
                ));
                let want = if want_played { vec![message] } else { vec![] };
                assert_eq!(
                    delivered(&actions),
                    want,
                    "{name}: packet {index} must be played exactly when it is new"
                );
            }
        }
    }

    /// A packet's journal describes the packets before it, never its own.
    ///
    /// RFC 6295 section 4 takes the journal before the packet's own messages join it. Describing
    /// its own packet too, a journal made a receiver recovering from a gap play that packet's
    /// notes twice, once from the journal and once from the packet. The first packet has nothing
    /// before it, so it carries no journal.
    #[test]
    fn a_packets_journal_describes_the_packets_before_it() {
        let mut session = established();

        assert_eq!(
            sent_journal(&session.send(&[note_on(60)], 0)),
            None,
            "the first packet has nothing before it to protect"
        );
        let journal = sent_journal(&session.send(&[note_on(64)], 0))
            .expect("the note sent before must be protected");
        assert_eq!(
            journal.recover(),
            vec![note_on(60)],
            "the journal must describe the earlier packet and not its own"
        );
    }

    /// After a gap, the journal's repairs are delivered before what arrived, and only those that
    /// change what was already delivered.
    ///
    /// Repairs after the fresh messages would replay stale state over them. A journal covers
    /// everything since the last acknowledgement, packets that arrived included, so replayed
    /// whole it struck a held note a second time, which starts a second voice on a synth that
    /// stacks them: across a network with 5% loss, 55 times in a minute. Here packet 100 arrived
    /// with note 60 and volume 90, and 101 to 104 were lost with a release of note 55 and a
    /// modulation change among them. Of the four repairs only those two are new.
    #[test]
    fn a_gap_is_repaired_first_and_repeats_nothing() {
        let mut session = established();
        let volume = MidiMessage::ControlChange {
            channel: Channel::new(0).expect("channel 1 is in range"),
            controller: 7,
            value: 90,
        };
        let modulation = MidiMessage::ControlChange {
            channel: Channel::new(0).expect("channel 1 is in range"),
            controller: 1,
            value: 64,
        };
        let _ = session.on_rtp(&RtpMidiPacket::new(
            100,
            0,
            PEER_SSRC,
            vec![
                TimedMessage::immediate(note_on(60)),
                TimedMessage::immediate(volume),
            ],
        ));

        let mut sender = JournalState::new();
        sender.observe(&note_on(60), 100);
        sender.observe(&volume, 100);
        sender.observe(&note_off(55), 101);
        sender.observe(&modulation, 102);
        let mut later = RtpMidiPacket::new(
            105,
            0,
            PEER_SSRC,
            vec![TimedMessage::immediate(note_on(67))],
        );
        later.journal = sender.build().map(|journal| journal.encode());

        assert_eq!(
            delivered(&session.on_rtp(&later)),
            vec![note_off(55), modulation, note_on(67)],
            "only the new repairs must be delivered, ahead of what arrived"
        );
        assert_eq!(
            session.recovered(),
            2,
            "only the delivered repairs count as recovered"
        );
    }

    /// Only a whole dump is delivered, and only one within `MAX_SYSEX_BYTES`.
    ///
    /// Part of a dump, played, is a different message from the one sent, so a dump that lost a
    /// segment to a gap is dropped, and the next dump is unaffected. A peer that never ends a
    /// dump would otherwise grow the buffer for as long as it sends. With a first segment of 10
    /// bytes and an empty last one, a middle of `MAX_SYSEX_BYTES - 12` makes the dump exactly
    /// `1 + 10 + (MAX_SYSEX_BYTES - 12) + 1 = MAX_SYSEX_BYTES` bytes with its framing, and one
    /// byte more is past the bound.
    #[test]
    fn only_whole_dumps_within_the_bound_are_delivered() {
        let cases = [
            ("a whole dump", vec![(1, SysExPart::Whole, 40)], vec![42]),
            (
                "a dump that lost its middle, then a whole one",
                vec![
                    (1, SysExPart::First, SYSEX_SEGMENT_BYTES),
                    (3, SysExPart::Last, 10),
                    (4, SysExPart::Whole, 40),
                ],
                vec![42],
            ),
            (
                "a dump exactly at the bound",
                vec![
                    (1, SysExPart::First, 10),
                    (2, SysExPart::Middle, MAX_SYSEX_BYTES - 12),
                    (3, SysExPart::Last, 0),
                ],
                vec![MAX_SYSEX_BYTES],
            ),
            (
                "a dump one byte past the bound",
                vec![
                    (1, SysExPart::First, 10),
                    (2, SysExPart::Middle, MAX_SYSEX_BYTES - 11),
                    (3, SysExPart::Last, 0),
                ],
                vec![],
            ),
        ];
        for (name, segments, want) in cases {
            let mut session = established();
            let mut dumps = Vec::new();
            for (sequence, part, length) in segments {
                let mut packet = RtpMidiPacket::new(sequence, 0, PEER_SSRC, Vec::new());
                packet.sysex.push(SysExSegment {
                    position: 0,
                    delta: 0,
                    part,
                    payload: vec![0x01; length],
                });
                dumps.extend(session.on_rtp(&packet).into_iter().filter_map(
                    |action| match action {
                        Action::DeliverSysEx(dump) => Some(dump),
                        _ => None,
                    },
                ));
            }
            let want: Vec<Vec<u8>> = want
                .into_iter()
                .map(|length| {
                    let mut dump = vec![0x01; length];
                    dump[0] = 0xF0;
                    dump[length - 1] = 0xF7;
                    dump
                })
                .collect();
            assert_eq!(dumps, want, "{name}: exactly these dumps must be delivered");
        }
    }

    /// A dump is delivered in its place among the messages of the packet that carried it.
    ///
    /// A peer may put a dump between two notes in one packet (RFC 6295, section 3.2), and a
    /// program change after a dump usually selects the program the dump just loaded, so the
    /// order is part of the meaning.
    #[test]
    fn a_dump_is_delivered_in_its_place_among_the_notes() {
        let mut session = established();
        let mut packet = RtpMidiPacket::new(
            50,
            0,
            PEER_SSRC,
            vec![
                TimedMessage::immediate(note_on(60)),
                TimedMessage::immediate(note_on(62)),
            ],
        );
        packet.sysex.push(SysExSegment {
            position: 1,
            delta: 0,
            part: SysExPart::Whole,
            payload: vec![0x01],
        });

        let deliveries: Vec<Action> = session
            .on_rtp(&packet)
            .into_iter()
            .filter(|action| matches!(action, Action::Deliver(_) | Action::DeliverSysEx(_)))
            .collect();
        assert_eq!(
            deliveries,
            vec![
                Action::Deliver(vec![note_on(60)]),
                Action::DeliverSysEx(vec![0xF0, 0x01, 0xF7]),
                Action::Deliver(vec![note_on(62)]),
            ],
            "the dump must be delivered between the two notes it was sent between"
        );
    }
}
