# Command line

```text
midi-harbor [OPTIONS] [COMMAND]
```

Run with no command, `midi-harbor` opens the graphical interface, or prints help in a build
without one.

Every command other than `daemon`, `gui` and `service` talks to the running daemon. If none is
running, the command says so and exits with code 3.

## Options every command accepts

| Option | Effect |
|---|---|
| `--json` | Print machine-readable JSON on standard output instead of tables |
| `--socket <PATH>` | Reach the daemon at this socket instead of the standard one |
| `-v`, `--verbose` | Log more; repeat for more still |
| `--quiet` | Print nothing but errors |

With `--json`, every command writes one JSON document. A command that lists or shows something
writes what it found. One that changes something writes `ok`, whether it worked, `messages`, the
sentences it would have printed, and where it made or renamed something, `result`, with its `id`.
Commands that watch, such as `events --follow`, write one document per update.

Endpoints are named everywhere by name or by identifier. A name that matches more than one
endpoint is refused with the candidates listed, never resolved by guessing.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Failure not covered below |
| 2 | Usage error, or the graphical interface asked for in a build without one |
| 3 | The daemon is not running or cannot be reached |
| 4 | The capability needed is not available on this machine |
| 5 | Not found: no such endpoint, route, peer or device |
| 6 | Conflict: the name is already in use, or the route already exists |
| 7 | Confirmation required: run again with `--yes` |
| 8 | This client and the daemon are different versions |

## Running

| Command | |
|---|---|
| `daemon [--log-file PATH]` | Run the daemon in the foreground. Refuses to start while another is running. With `--log-file`, it logs to that file instead of the terminal, rolling it over at 10 MB. |
| `gui` | Open the graphical interface. |
| `service install [--start]` | Register the daemon to run at login, and with `--start`, start it now. Running it again updates the registration in place, and with `--start` stops a daemon already running so the one started is this copy. |
| `service uninstall` | Stop the daemon and remove the registration. The configuration stays. |
| `service start`, `service stop` | Start or stop the registered daemon. |
| `service status` | Whether it is registered, running, and whether the registration points at a binary that has since moved, and the running daemon's version and how long it has been up. It says when the daemon is another build than the program asked, as after an update the daemon has not been restarted for; `--json` gives that as `same_build`. |

See [Installation](installation.md) for what registering does on each platform.

## Virtual ports

Ports other applications on this computer see as MIDI devices.

| Command | |
|---|---|
| `port list` | The configured ports, how many MIDI In and MIDI Out connectors each has, and their state. |
| `port create <NAME> [--inputs N] [--outputs N]` | Create a port. It has MIDI In connectors, which applications send to, and MIDI Out connectors, which they receive from, one of each by default and up to 16. Several of one kind show to applications as numbered ports, `Keys 1` and `Keys 2`. `--direction`, from before connectors, is accepted and ignored. |
| `port connectors <PORT> --inputs N --outputs N` | Change how many connectors a port has. Routes on connectors it no longer has are removed, and the port is reopened, so applications using it may need to select it again. |
| `port rename <PORT> <NEW_NAME> --yes` | Rename a port. Routes naming it are rewritten, so they keep working. Applications using it may need to select it again, which is why `--yes` is required. |
| `port delete <PORT> --yes` | Delete a port, releasing any notes sounding through it first. Routes naming it are kept and shown as broken. Refused without `--yes`. |
| `port enable <PORT>`, `port disable <PORT>` | Switch a port on or off without losing its configuration. |

A port keeps its identity across restarts, so applications that remember it find it again.

## Network ports

MIDI over the network: RTP-MIDI sessions, which Apple calls network sessions, compatible with
Apple's Network MIDI and other RTP-MIDI implementations. `session` is accepted as well, the name
these commands had before.

| Command | |
|---|---|
| `network list` | The configured network ports, their UDP ports, whether each has its automatic port, whether they are listening, connected, or retrying, and the other machines each carries beside its peer. |
| `network create <NAME> [--port N] [--policy P] [--bonjour-name NAME] [--no-automatic-port]` | Create a network port other machines can connect to. `--port` is the UDP port, which has to be even, and the data port is the next one up. Without `--port` the system chooses one, and it is kept from then on. Other machines see it by `--bonjour-name`, or by NAME without it; no two network ports may show the same name. Without `--policy` it takes the configuration's `default_invitation_policy`, which is `prompt` unless changed. Other applications on this computer see it as a MIDI port called NAME, joined to it both ways, the way macOS shows its own network sessions. `--no-automatic-port` leaves that port out, so the network port carries only what is routed to it. |
| `network edit <NETWORK_PORT> [--bonjour-name NAME] [--udp-port N] [--policy P] [--automatic-port on\|off]` | Change a network port's settings; what is not given stays as it is. A new Bonjour name reaches machines that connect from then on, and those already connected stay connected. A new UDP port restarts the network port on it: it connects again to the machine it had connected to, and machines that connected to it need the new port. A UDP port that is odd, taken, or used by another network port is refused, and nothing about the network port changes. Switching its automatic port off removes that port from other applications, stopping any notes it had sounding first. |
| `network discover` | The RTP-MIDI sessions advertised on this network, not counting this daemon's own network ports. Another application's sessions on this machine, such as Apple's Network MIDI, are listed. |
| `network connect <NETWORK_PORT> <PEER> [--alongside]` | Connect a network port to a peer, by its discovered name or by address: `192.0.2.5` or `192.0.2.5:5004`. A bare address uses the standard UDP port, 5004. Connecting by address needs no discovery, so it works on networks that block it. The network port remembers its peer and connects to it again whenever it starts, after a restart or a reboot. Without `--alongside` the new peer replaces the old one. With it, the machine carries MIDI beside those already connected, and is remembered and reconnected as the peer is, after a restart and when its link is lost, until it is disconnected with `network disconnect --machine`. |
| `network machines <NETWORK_PORT>` | Every machine taking part, the peer first: its name, address, whether this machine connected to it or it to this machine, whether it has joined, and its latency. |
| `network disconnect <NETWORK_PORT> [--machine ADDRESS]` | Drop the connection. The network port goes on listening, and no longer reconnects when it starts. With `--machine`, only that machine goes and is forgotten, by the address `network machines` shows, and the others stay connected; when it was the peer, a remaining machine takes its place and the network port no longer reconnects to it. |
| `network policy <NETWORK_PORT> <P>` | Change how the network port treats invitations. |
| `network delete <NETWORK_PORT> --yes` | Delete a network port. Anything it was carrying is silenced first, here and on the machines connected to it, which are then disconnected; its automatic port goes, and so does its advertisement. Routes naming it stay, with a missing endpoint, and carry again if a network port of its name is made. |
| `network enable <NETWORK_PORT>`, `network disable <NETWORK_PORT>` | Switch a network port on or off without deleting it. Off, it neither listens nor connects. |
| `network invitations` | Machines waiting to be let in. |
| `network respond <INVITATION> --accept\|--refuse [--always]` | Answer one. `--always` remembers the machine, so it is not asked about again. |
| `network peer add <ADDRESS> [--name NAME] [--no-trust]` | Remember a machine, so its invitations are accepted without asking. `--no-trust` remembers it without that, so its invitations are still asked about. |
| `network peer trust <PEER> on\|off` | Switch whether a remembered machine is let in without asking. Off takes effect at the machine's next invitation, including when it was accepted once earlier in this run. |
| `network peer remove <PEER>` | Forget one, so its invitations are asked about again, including when it was accepted once earlier in this run. |

Invitation policies:

| Policy | Invitations are |
|---|---|
| `prompt` | accepted from remembered machines, and held from anyone else until answered with `network respond` (the default) |
| `known` | accepted from remembered machines, and refused from anyone else |
| `all` | accepted from anyone |
| `reject` | refused |

A connected network port that stops hearing from its peer reconnects by itself, backing off
gradually while the peer stays away. When a link is lost, notes that were sounding through it are
released. When it comes back, the peer is sent the controller, program, pitch-bend and pressure
values it was last sent, including any that changed while it was away. Notes are never replayed. Between two Midi Harbor machines, a restart at either end is noticed within a second,
and the network port reconnects at once. With other implementations it can take the 35 seconds a
silent peer takes.

When this machine goes to sleep, its network ports disconnect first, and reconnect when it wakes, whichever
machine made the connection. Another Midi Harbor machine waits rather than inviting it back, since
its invitations would wake a Mac on mains power every minute or so. Other implementations are
told the session ended and may invite it back.

Apple's Network MIDI passes no MIDI for the first couple of seconds of a session, while the
two sides agree on timing. Notes played in that time are lost.

A network port carries system-exclusive as well as notes. A dump too long for one packet is sent in
pieces and put back together at the other end; one that loses a piece on the way is not delivered,
since half a dump is a different message from the one sent.

A network port connects to one peer, and carries other machines as well. A machine that invites it
while it has a peer is let in beside it, as the invitation policy allows, and carries the same
MIDI both ways. `network list` names them under ALSO WITH, and the history records each arriving
and leaving. One that leaves or stops answering is let go rather than reconnected, since it
invited itself in. A machine this side connected with `--alongside` is different: it is remembered
beside the peer and reconnected like the peer, until it is disconnected. When the peer leaves and
the network port goes back to listening, a machine still connected takes its place, and one this
side connected is remembered as the peer from then on. When a machine that connected to this one
disconnects on purpose, the network port is not reconnected to it, so disconnecting from the other
machine sticks.

## Routes

A route carries MIDI from one endpoint to another. Any endpoint can be either end: a virtual
port, a device, a network port, or a Bluetooth link.

| Command | |
|---|---|
| `route list [--broken]` | Every route, whether it is carrying, and why not when it is not. |
| `route create <FROM> <TO> [--from-connector N] [--to-connector N]` | Connect two endpoints, by name. For a port with several connectors, `--from-connector` picks which of FROM's MIDI Ins, and `--to-connector` which of TO's MIDI Outs; both default to the first. `route list` shows a connector past the first, as `Keys (MIDI Out 2)`. |
| `route create <FROM> <TO> --both-ways` | One route carrying MIDI each way: from FROM to TO, and back from TO's MIDI In of the same number to FROM's MIDI Out of the same number. `route list` shows it as `FROM ↔ TO`. A route already carrying either way is a duplicate. |
| `route edit <ID> [--from NAME] [--to NAME] [--from-connector N] [--to-connector N] [--both-ways \| --one-way]` | Change a route's ends, connectors, or whether it carries both ways. What is not given stays as it was, and so does whether it is switched on. Notes it was holding are stopped first. Its identifier changes with its ends. |
| `route delete <ID>` | Remove a route, by the identifier `route list` shows. |
| `route enable <ID>`, `route disable <ID>` | Switch a route on or off without removing it. |

A route's state in `route list`:

| State | Meaning |
|---|---|
| `ok` | Carrying MIDI whenever there is any. |
| `waiting` | One end is switched off, unplugged, or disconnected. It resumes by itself when that end returns; the note says which. |
| `broken` | One end names an endpoint that does not exist. Creating or renaming an endpoint to that name mends it. |
| `loop` | It is part of a chain of routes leading back to where it started. It still carries MIDI, and a message is never delivered twice, but a loop is almost always a mistake. |

A loop across machines cannot be seen in any one machine's routes. A route from one network port to
another switches itself off when the MIDI it forwards is what went out through that network port
moments before, many times over, and `events` says which route and why. Change the routes on the
other machine, then switch it back on with `route enable`.

## Hardware

| Command | |
|---|---|
| `device list [--all]` | Attached MIDI hardware; with `--all`, remembered hardware that is not plugged in. |
| `device resolve <DEVICE> [CHOSEN]` | Say which of two identical devices a remembered entry means. Without `CHOSEN`, list the candidates. |
| `device forget <DEVICE>` | Forget a device and everything configured about it. |
| `device enable <DEVICE>`, `device disable <DEVICE>` | Switch a device on or off, keeping its routes. Off, it is not opened, even when plugged in. |

Hardware is remembered by what it reports about itself, so a device unplugged and plugged back in
is recognised and its routes resume. Another application's port is listed while it is open, and
remembered after it closes only if a route names it. On Linux, a device that reports no serial number is known by
the socket it is plugged into, so moving it to another socket asks for `device resolve`. Two
devices identical in every respect cannot be told apart at all, and are left for
`device resolve` rather than guessed at.

## Bluetooth

| Command | |
|---|---|
| `bluetooth scan [--seconds N]` | Look for Bluetooth MIDI devices for N seconds (10 by default), then list them. |
| `bluetooth list` | What the radio can hear now, without scanning again. |
| `bluetooth connect <ADDRESS>` | Connect a device, and remember it so it reconnects whenever it comes back into range. |
| `bluetooth disconnect <DEVICE>` | Close the link, still remembering the device. |
| `bluetooth forget <DEVICE>` | Forget it, so it no longer reconnects. |
| `bluetooth enable <DEVICE>`, `bluetooth disable <DEVICE>` | Switch a device on or off without forgetting it. Off, it is not reconnected. |
| `bluetooth advertise [--name NAME] [--off]` | Offer this computer to phones and tablets as a Bluetooth MIDI device, or stop. On macOS a name longer than five characters is left out, and devices see the computer's own name; the command says so when that happens. |

A Bluetooth device stamps each message with when it was played, and sends several together in
each radio packet. Midi Harbor delivers them as far apart as they were played rather than all at
once, which adds at most 10 ms.

## Watching

| Command | |
|---|---|
| `status [--watch]` | Every endpoint, its state, the messages it has received and sent and when it last did each, and its last error. A network port's automatic port has a row beneath it: what it passed to applications on this computer, and what they sent it. An endpoint that is reconnecting says when it tries next. After the system's MIDI service stopped and Midi Harbor recovered, a warning above the table says so, since other applications may have lost their MIDI connection and need relaunching, until it is dismissed. `--watch` redraws as things change. |
| `events [--since ID] [--limit N] [--follow]` | What has happened to connections, oldest first, each with the local time it happened: failures, recoveries, notes released, invitations. `--follow` keeps watching. With `--json`, each event has its time in UTC and the endpoint or route it concerns. |
| `monitor <ENDPOINT> [--raw]` | The MIDI passing through one endpoint, as it passes. `--raw` adds the bytes. |
| `dismiss-warning` | Clears the warning that the system's MIDI service stopped, which `status` and the window show after Midi Harbor recovers from it, once it has been seen. Dismissing it anywhere clears it everywhere. |
| `send-note <ENDPOINT> [--note 60] [--channel 1] [--velocity 100] [--length 500]` | Sends one note out of an endpoint to test what listens there, such as a cue a program plays on a note. The note-off follows after `--length` milliseconds, up to 10000, sent by the daemon so the note cannot be left sounding. A `monitor` of the endpoint shows it leaving. |
| `capabilities` | Which features this machine can use, and why not when it cannot. |
| `diagnostics export [--output FILE]` | Configuration, connection history and counters in one file, for a bug report. |

`monitor` never slows MIDI down: when it cannot keep up, it skips messages and says how many.

## Configuration

| Command | |
|---|---|
| `config path` | Where the configuration file is. |
| `config show` | Print it. |
| `config reload` | Apply changes made to the file by hand. Only what changed is disturbed. |
| `config export [--output FILE]` | Write the running setup, ready to import on this machine or another. |
| `config import <FILE> [--mode merge\|replace] [--yes]` | Apply an exported setup. `merge` (the default) adds what this machine lacks and changes nothing it has. `replace` makes this machine match the file, removing what the file does not mention, and requires `--yes`. |
| `config import-apple [--yes]` | macOS only: take over the IAC Driver's buses, as virtual ports of the same names, and the network sessions set up in Audio MIDI Setup. Apple's setup is read, never changed. Without `--yes` it shows what it would create and changes nothing; names already in use are left alone, as is a bus named like a network port or a session named like a port. |

See [Configuration file](configuration.md) for the format.
