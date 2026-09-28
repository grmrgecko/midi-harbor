# Configuration file

Everything Midi Harbor is set up to do lives in one YAML file per user: the virtual ports, network ports,
hardware and Bluetooth devices, the machines it knows, and the routes between them. The daemon
reads it at startup and writes it whenever the setup changes. It can also be written or edited
by hand.

| Platform | Location |
|---|---|
| macOS | `~/Library/Application Support/midi-harbor/config.yaml` |
| Linux | `$XDG_CONFIG_HOME/midi-harbor/config.yaml`, which is `~/.config/midi-harbor/config.yaml` when the variable is not set |
| Windows | `%APPDATA%\midi-harbor\config.yaml` |

`midi-harbor config path` prints the location in use.

## Writing it by hand

Only names and kinds are required. This is a complete configuration:

```yaml
endpoints:
  - name: Sequencer Bus
    kind: virtual_port
  - name: Stage
    kind: network_port
routes:
  - from: Sequencer Bus
    to: Stage
```

Everything left out takes its default. The next time the daemon writes the file, it fills in what
it chose: identifiers, the UDP port the system gave the network port, and the identity the platform
gave the virtual port. Leave those in place. They are how the same virtual port and network port
come back after a restart.

A file edited while the daemon runs takes effect on `midi-harbor config reload`. Only what changed
is touched: editing one route does not interrupt the others. Comments are not kept, since the
daemon writes the file from what it holds.

## What the daemon writes

A file the daemon has written looks like this:

```yaml
schema_version: 1
preferences:
  default_invitation_policy: prompt
  bluetooth_advertising: false
endpoints:
- id: 5876a1e4-fbc5-4d41-bf12-f9cd21ced243
  name: Sequencer Bus
  kind: virtual_port
  inputs: 1
  outputs: 1
  input_ids:
  - 3209924026
  output_ids:
  - 3209924025
  enabled: true
  direction: bidirectional
- id: e5b1f4ce-1d00-40ad-a5ad-cbb8160b518a
  name: Stage
  kind: network_port
  local_name: Stage
  control_port: 5104
  invitation_policy: accept_known
  enabled: true
  direction: bidirectional
- id: 631de477-c3d4-4d29-9290-61b279a966aa
  name: Keystation 49
  kind: physical_device
  fingerprint:
    unique_id: 2750102245
    manufacturer: M-Audio
    name: Keystation 49
  enabled: true
  direction: bidirectional
peers:
- id: 2a7026ac-9b34-4e57-b19b-918962a74e8f
  name: Studio PC
  addresses:
  - 192.0.2.13:5004
  trusted: true
routes:
- from: Sequencer Bus
  to: Stage
  enabled: true
```

## Endpoints

Every endpoint has these fields, whatever its kind:

| Field | | Default |
|---|---|---|
| `name` | What it is called everywhere, and what routes refer to it by. Up to 128 characters. Leading and trailing spaces are dropped. | required |
| `kind` | `virtual_port`, `network_port`, `physical_device` or `bluetooth_device` | required |
| `id` | A stable identifier. | generated |
| `enabled` | Whether it should be running. `false` keeps it configured but switched off. | `true` |
| `direction` | `input` (MIDI arrives from it), `output` (MIDI leaves through it), or `bidirectional`. A virtual port is always `bidirectional`; its connectors say the rest. | `bidirectional` |

Names must be unique among endpoints of the same kind.

### `virtual_port`

A port applications on this computer see as a MIDI device. It has MIDI In connectors, which
applications send to, and MIDI Out connectors, which they receive from, as an IAC Driver bus
does. Several of one kind show to applications as numbered ports, `Keys 1` and `Keys 2`.

| Field | | Default |
|---|---|---|
| `inputs` | How many MIDI In connectors it has, 1 to 16. | `1` |
| `outputs` | How many MIDI Out connectors it has, 1 to 16. | `1` |
| `input_ids`, `output_ids` | The identity the platform gave each connector the first time it was created. Written by the daemon, so that applications which remember a connector recognise it after a restart. Leave them alone. | none |

A count outside 1 to 16 is read as the nearest of those. A port written before connectors, with a
`platform_unique_id` and an `input` or `output` direction, is read as one connector of each, and
keeps its identity.

### `network_port`

An RTP-MIDI session. `network_session`, the name this kind had before, is still accepted, here and
in a route's `from_kind` and `to_kind`.

A network port may share its name with a virtual port, but with its automatic port on, other
applications then see two MIDI ports of that name and cannot tell which is which. Rename one, or
switch the automatic port off.

| Field | | Default |
|---|---|---|
| `local_name` | The name other machines see. | the endpoint's `name` |
| `control_port` | The UDP port it listens on, an even number. The data port is always the next one up. `0` lets the system choose; the daemon then records its choice here so it does not move. An odd port written here is moved up one when it listens, and a taken one moves to a nearby free pair, so the network port stays up; `network list` and the window show where it is listening. | `0` |
| `invitation_policy` | `prompt`, `accept_known`, `accept_all` or `reject_all`. See the invitation policies in [Command line](cli.md). A network port written here without one prompts. | `prompt` |
| `peer` | The `id` of the peer, under `peers`, this network port connects to when it starts. Set by `network connect` and cleared by `network disconnect`. | none |
| `other_peers` | The `id`s of further peers, under `peers`, this network port connects to beside `peer` when it starts, and again whenever one's link is lost. One is added by `network connect --alongside` and removed by `network disconnect --machine`; `network disconnect` clears them all. Written only when there are some. | none |
| `automatic_port` | Whether other applications on this computer see it as a MIDI port of its `name`, joined to it both ways: what they send that port goes out over the network, and what arrives over the network comes out of it as well as along the network port's routes. The port is renamed and removed with the network port, and no route names it. | `true` |
| `port_input_id`, `port_output_id` | The platform identifiers of the automatic port, written by Midi Harbor so other applications recognise the same port after a restart. | none |

### `physical_device`

Attached hardware, and other applications' ports. The daemon adds an entry the first time it sees
one. Writing one by hand is rarely useful.

| Field | |
|---|---|
| `fingerprint` | What the device reports about itself, and how it is recognised when it comes back: `name`, and any of `unique_id`, `usb_serial`, `manufacturer`, `model` and `topology_path` the platform provides. |
| `software` | `true` for another application's port rather than hardware: a program's own port, or on macOS one of Apple's IAC buses or network sessions. Written only when true. |

Removing an entry forgets the device. If it is still attached, it comes back as new.

Hardware stays in the file while it is unplugged, so its routes resume when it returns. An
application's port stays only while it is open, or while a route names it. So a synth you route to
comes back with its routes, and a tool that opened a port once leaves nothing behind.

### `bluetooth_device`

A Bluetooth MIDI device, added by `midi-harbor bluetooth connect`.

| Field | |
|---|---|
| `address` | The device's identifier as the platform reports it. |
| `role` | `central` for a device this computer connects to, or `peripheral` for this computer advertising itself. |
| `paired` | Whether it has been paired with before. |

## Routes

| Field | | Default |
|---|---|---|
| `from` | The name of the endpoint MIDI comes from. | required |
| `to` | The name of the endpoint MIDI goes to. | required |
| `enabled` | Whether it should carry MIDI. | `true` |
| `from_kind`, `to_kind` | Which kind of endpoint `from` or `to` is: `virtual_port`, `physical_device`, `network_port` or `bluetooth_device`. Needed only when endpoints of different kinds share the name. | none |
| `from_connector`, `to_connector` | Which of the source's MIDI In connectors, and which of the destination's MIDI Out connectors, counting from 1. Needed only for a virtual port with more than one. A route on a connector the port no longer has waits, shown as broken. | `1` |
| `both_ways` | Whether it also carries MIDI back, from `to`'s MIDI In `to_connector` to `from`'s MIDI Out `from_connector`. Both ends must be able to send and receive. | `false` |

Routes name their endpoints rather than using identifiers, so they can be read and written by
hand. Renaming an endpoint through Midi Harbor rewrites every route that names it. Renaming it by
editing this file does not, so change the routes too. A route naming an endpoint that does not
exist is kept and shown as broken, and mends itself when an endpoint of that name appears.

A virtual port and a network port may not share a name, because other applications see a network
port as a port of its name and could not tell the two apart. Midi Harbor refuses to create or
rename either into the other's name, and refuses to import or reload a file that gives them one.
A file edited by hand that already does is still loaded at startup with both kept, and the clash
is logged and recorded in the history as an error until one is renamed.

Endpoints of other kinds may share a name, a virtual port named after attached hardware for
instance. A route then needs `from_kind` or `to_kind` to say which one it means. Midi
Harbor writes them itself: when it creates a route to a shared name, and when an endpoint arrives with the name
of one a route already uses. A route naming a shared name without a kind is shown as broken until
one is added.

## Peers

Machines this one knows, added by `network peer add`, by answering an invitation with `--always`,
or by `network connect`, which remembers where it connected without trusting it.

| Field | | Default |
|---|---|---|
| `name` | What to call it. | required |
| `addresses` | Where it was last reached, as `host:port`. | none |
| `trusted` | Whether its invitations are accepted without asking. | `false` |
| `id` | A stable identifier. | generated |

## Preferences

| Field | | Default |
|---|---|---|
| `machine_name` | The name this computer advertises over Bluetooth when `bluetooth advertise` is given none. `config reload` applies a change to the next `bluetooth advertise`; one already advertising keeps its name. | the computer's name |
| `default_invitation_policy` | The invitation policy `network create` gives a network port when `--policy` is not used. | `prompt` |
| `bluetooth_advertising` | Whether this computer advertises itself as a Bluetooth MIDI device. | `false` |
| `advertise_sessions` | Whether network ports are announced to other machines. Off, they still work and can be connected to by address, but browsing machines do not see them. `config reload` applies a change without restarting any network port. | `true` |

## When the file cannot be read

A file that is not valid YAML, or does not describe a possible setup, is never deleted or
overwritten. The daemon moves it aside as `config.corrupt-<time>`, next to where it was, and
starts with an empty setup. It records the problem in `midi-harbor events`, with the reason and
where the file went. Fix the saved copy and move it back, or import it with `config import`.

A file written by a newer version of Midi Harbor (a higher `schema_version`) is not moved aside.
The daemon refuses to start instead, rather than silently drop settings it does not understand.

The daemon writes the file by writing a new copy and renaming it over the old one, so an
interrupted write never leaves a half-written file.

## Moving a setup to another machine

```bash
midi-harbor config export --output studio.yaml    # on the first machine
midi-harbor config import studio.yaml             # on the second
```

An import merges by default: it adds what the machine lacks and changes nothing it already has.
`--mode replace --yes` makes the machine match the file exactly.
