# Midi Harbor

Midi Harbor manages MIDI connections on macOS, Linux and Windows: virtual ports that applications on
the same computer can use, attached MIDI hardware, network ports that reach other computers over
RTP-MIDI, and Bluetooth LE MIDI devices. Connections repair themselves. A network connection that
drops is reconnected, hardware that is unplugged and plugged back in picks up where it left off, and
notes that were sounding when a link went away are released rather than left ringing.

A daemon owns every connection and runs whether or not anything is watching it. The command line
and the graphical interface are both clients of it: closing either changes nothing.

- [Installation](installation.md) — building, installing, and running the daemon at login.
- [Command line](cli.md) — every command, its options, and its exit codes.
- [Configuration file](configuration.md) — where the setup is kept, and how to write it by hand.
- [Platforms](platforms.md) — where macOS, Linux and Windows behave differently, and why.

## A first setup

```bash
midi-harbor service install --start      # run the daemon now and at every login
midi-harbor port create "Sequencer Bus"  # a port every MIDI application can see
midi-harbor status
```

To join another computer on the network, create a network port and connect it:

```bash
midi-harbor network create "Studio"
midi-harbor network discover             # the machines advertising on this network
midi-harbor network connect "Studio" "Stage Mac"
midi-harbor route create "Sequencer Bus" "Studio"
```

MIDI played into "Sequencer Bus" now reaches "Stage Mac", and keeps reaching it across sleep,
network changes, and either machine restarting.
