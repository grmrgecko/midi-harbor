# Research: Remembered Machines

The investigations behind this spec, under their numbers in the project-wide research log.

---

## R-105: A machine remembered at a port it no longer listens on

**Status**: **FIXED** (2026-10-02). Built as T249 and T250.

A network port listed a machine called `192.0.2.13` as "On this network ·
192.0.2.13:51474". Connect failed with "no peer named '9c22fef1-f84d-4b45-a09b-0bfb4aaa93f4'". The
user had removed that connection. A Bonjour browser showed the host advertising one session,
under another name, at `192.0.2.13:40180`: the port the connection had been to was deleted on
that machine, and a new one made.

Four defects, the first three behind what was seen:

- **A machine stayed remembered after its connection was removed.** Connecting records the
  machine among the remembered ones, named after its address, so the network port can name its
  peer. Disconnecting cleared the port's reference and left the record.
- **An advertisement was matched to a remembered machine by host alone.** That is right for a
  trusted machine, since trust is held by host. For a record that is only an address it marked
  the old port as present and folded the new session into it, so the session at the new port was
  not listed at all.
- **Connect resolved identifiers only among advertised sessions.** A remembered machine is listed
  under the identifier of its record, which no advertisement carries, so Connect failed for every
  remembered machine, present or not.
- **A network port invites one address until it answers.** The supervisor holds a socket address
  and retries it with backoff; nothing looked at discovery again. R-049 met the same fault from
  the other side and pinned Midi Harbor's own ports, which does nothing for another program's.
  Found by reading the code, not seen here: the session at the new port was a different one.

**Fixes.** A machine with no name of its own, no trust and no network port connecting to it is
dropped when a port lets it go. An untrusted record is matched by an advertisement at its exact
address only. Connect resolves remembered machines by identifier or name. And the session name
advertised at a machine's address is stored with it as `advertised_as`; when discovery sees a
session come or go, or a session reports a change, each machine whose session is advertised
somewhere else is moved there if its link is down, and the new address is stored.

**Which records are dropped.** The configuration has no mark for how a record came to be, and
none was added. A record made by connecting is named after its own address, so that, without
trust and without a port using it, is the rule. A machine added by address with neither a name
nor trust is dropped by it too, once connected to and disconnected; it held nothing but the
address.

**Why trust limits following by name.** An advertisement is unauthenticated. Rewriting a trusted
machine's address to another host would let whichever machine advertised the name in without
asking. A new port on the same host changes nothing about who is trusted, so it is followed for
every machine; a new host only for an untrusted one, where the worst outcome is an invitation
sent to the wrong machine. R-106 lifts this for a machine that can prove which it is.

**Why only while the link is down.** A second machine may take the name while the first is
connected, and the first must not be dropped for it.

**Checked live** on macOS between two daemons on one Mac, each with its own home directory and
socket. Near connected to Follow Far by its discovered name at `192.0.2.20:21000` and stored
`advertised_as: Follow Far`. Far's port was moved to 21020 with `network edit --udp-port`. Near
logged the peer ending the session, and 0.95 s later "following the peer to where it is
advertised now", and listed the machine joined at `:21020` at the first look two seconds after
the move; the configuration held the new address.

**Not covered:** a program that dies without withdrawing its advertisement, where what the
browser reports depends on the old record expiring.

---

## R-106: Proving which network port a session is

**Status**: **DONE** (2026-10-02). Built as T251.

R-105 could not follow a trusted machine to another host, and could not tell a port renamed from
a different port, or a port made again under an old name from the old one. The owner asked for a
proof, by signature, of the host and of the port.

**The exchange.** Each daemon holds an Ed25519 key. Every session it advertises carries two TXT
properties: `mhkey`, the public key as 64 hexadecimal digits, and `mhport`, the network port's
identifier, a UUID that a rename does not change. A network port asks the port at an address to
prove both with two packets on the control port, under AppleMIDI's `FF FF` signature and with
commands of their own. All integers are big-endian.

| Packet | Bytes |
|---|---|
| Challenge | `FF FF` `48 51` ("HQ"), version `00 00 00 01`, nonce (32). 40 bytes |
| Proof | `FF FF` `48 41` ("HA"), version `00 00 00 01`, nonce (32), public key (32), port identifier (16), signature (64). 152 bytes |

The signature is over `midi-harbor network port identity v1`, a zero byte, the nonce, the asker's
address as the answering port saw it (16 bytes, an IPv4 address mapped into IPv6, then the port,
2 bytes), and the port identifier. Bytes after the last field are ignored, so a later version can
add to a packet.

**Why the asker's address is signed.** Without it a machine could pass a challenge on to the real
port and return the answer as its own. The real port signs the address the challenge came from,
which is then the go-between's. The asker accepts a proof only for one of its own addresses and
its own control port, and only from the address it challenged. A machine that can forge both
source addresses and read the reply already controls the network, where trust in an address
means nothing anyway.

**Why TXT, and not asking every peer.** No other implementation knows the two commands, and what
each does with an unknown one on its control port was not something to find out on someone's
show. A session that advertises `mhkey` is a Midi Harbor that answers. One that does not is
never sent a challenge. The advertised key proves nothing by itself, since anyone can copy it;
it only says whom to ask.

**When a key is kept.** A key is stored with a machine only once the port at the address the
network port connects to proves it. A forged advertisement naming a trusted machine's address
would otherwise plant a key its forger could prove later from anywhere.

**What it changes.** A machine stored with a key and port identifier is that port wherever it is
advertised and whatever it is called, and nothing else is. It is followed to another host once it
proves both there, trusted or not, and a trusted machine's trust moves with its address. A
session of the same name with another identifier is another port and is not followed, which is
the owner's case of a port deleted and made again. A machine with no key is followed as R-105
describes.

**Where the key lives.** `identity.key` beside the configuration document, readable by its owner
only, holding the 32-byte seed in hexadecimal. Not in the configuration, which is exported,
copied to other machines and pasted into bug reports: two machines sharing a key would each pass
for the other. A daemon that cannot write it uses a key for that run and says so in the log.

**A defect the live check found.** A port renamed and moved was not followed. The new name is
advertised before the old one is withdrawn, and while the old advertisement stands it says the
port is still where it was. The withdrawal arrived a second later and prompted nothing. Discovery
now prompts a look when a session goes as well as when one is resolved.

**Checked live** on macOS between two daemons on one Mac. `dns-sd -L` showed both properties on
Follow Far. Near connected to it at `192.0.2.10:21000` and stored its key and port identifier
within three seconds. Far was renamed and moved with `network edit --bonjour-name "Follow
Renamed" --udp-port 21020`; near logged "following the peer" 1.25 s after the session ended, was
joined at `:21020`, and stored the new name. Far's port was then deleted and another created as
Follow Renamed on 21040; nine seconds later near was still inviting `:21020`.

**Checked against other implementations** on 2026-10-02, with a network port advertised through
Avahi from a Fedora 43 machine on the LAN. That account could not open the ALSA sequencer, so the
daemon ran over the in-memory MIDI platform; its network side was the real one.

| Implementation | What was checked | Result |
|---|---|---|
| Apple's Network MIDI, macOS 27 | `dns-sd -L` resolves the session with both properties; Audio MIDI Setup lists it in its directory | Listed as RTP-MIDI beside the others |
| Apple's Network MIDI | The port connected out to a second Apple session by its discovered name; a note each way | Joined; note on and off arrived at Network Session 2, and the port counted the two sent back. `advertised_as` was stored and no key, so no challenge was sent |
| rtpmidid, Arch Linux | Discovered the session through Avahi and invited it when an ALSA port was subscribed; a note each way | Joined, clocks synchronised at 0.2 ms, both notes arrived |

Apple inviting the port failed at first, for a reason older than this work, and works since
R-107.

**Seen once, and not explained.** In rtpmidid's first two starts its IPv6 resolver timed out on
the session ("Timeout reached"), and rtpmidid then drops the peer whichever family resolved. Two
sessions published beside it with `avahi-publish`, one with the same TXT record and one with
none, resolved on both families, so the record is not the cause. In five later starts, with the
last committed build advertising beside the new one, every session resolved on both families and
none was dropped.

**The Linux gates** pass on Arch Linux with this change: fmt, clippy, 312 tests, and the
headless build.

**Not covered:** another host in a live check, which needs a second machine and is covered by
`a_trusted_machine_is_followed_to_another_host_once_it_proves_which_port_it_is` over the two
loopback families; the TXT record through the DNS Client service on Windows, which was compiled
and not run; rtpMIDI on Windows; and an inbound invitation from a trusted machine at a new
address, which is still asked about.

---

## R-107: Apple's invitation over a link-local address was never answered

**Status**: **FIXED** (2026-10-02). Built as T252.

The owner selected the Linux port in Audio MIDI Setup and clicked Connect. Apple reported
"fe80::2:21500 didn't respond to the connection request", and the port listed
the Mac as joining at `[fe80::1]:5004`, never joined. A note sent from the port
did not arrive.

Apple's Network MIDI invites over the IPv6 link-local address Bonjour resolves when the host
advertises one. A link-local address is reachable only with its scope, the interface it belongs
to. The supervisor kept every sender's address through `canonical`, which rebuilt it from the IP
and the port and so dropped the scope, and built the data port's address the same way. The
acceptance went to `fe80::` with no interface and was never delivered. The fault is in the code
as last committed; R-056's check of Apple inviting was over IPv6 loopback, which has no scope, and
this spec's earlier checks connected over IPv4.

**Fix.** An IPv6 sender's address is kept whole, and the data port's address is the same address
with the port changed. An IPv4 sender seen through the dual-stack socket is still kept by its
plain address.

**Checked live.** With the fix on the Linux machine the owner clicked Connect again: the port
listed the Mac joined at `[fe80::1%9]:5004` with 3.1 ms latency, a note from the
port arrived at Network Session 1, and a note sent into that session was counted by the port.
`a_machine_is_answered_at_the_address_it_was_heard_from` fails with the scope dropped again.
