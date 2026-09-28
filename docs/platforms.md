# Platforms

Every feature works on macOS and Linux: virtual ports, hardware, network ports, Bluetooth in both
directions, the service, the command line and the graphical interface. Windows has all of them
except advertising this computer as a Bluetooth MIDI device, and until its late-2026 update its
virtual ports need a runtime Windows does not include. Where the platforms themselves differ, Midi Harbor behaves as alike as
they allow. The differences that remain are listed here.

## Virtual ports on Windows

Midi Harbor creates its ports through Windows MIDI Services. Windows includes it from its
late-2026 update, and Midi Harbor uses it from then on without being told. Before that update,
install Microsoft's [Windows MIDI Services SDK Runtime and Tools][wms-releases] once. Without
either, `midi-harbor capabilities` reports virtual ports as unavailable and names what to install,
and everything else works.

The Windows MIDI Service before that update has a fault Microsoft fixed in the update: once a
virtual port is closed, the service stops answering. Deleting, renaming or disabling a port, or
stopping Midi Harbor, leaves virtual ports unavailable, and `midi-harbor capabilities` says so.
Everything else keeps working. Restarting the Windows MIDI Service recovers it, from an
administrator PowerShell:

```powershell
Stop-Process -Name MidiSrv -Force
Start-Service midisrv
```

Before the update, other programs also list a port with several connectors after its groups,
"Sequencer Bus" and "Sequencer Bus Gr 2", rather than as "Sequencer Bus 1" and "Sequencer Bus 2".

[wms-releases]: https://github.com/microsoft/MIDI/releases

## How a port keeps its identity

On **macOS**, each port has an identifier that CoreMIDI assigns the first time it is created.
Midi Harbor records it in the configuration and asks for it again every time. An application
that remembered the port finds the same one after a restart, even one that remembers ports by
identifier rather than name.

On **Linux**, ALSA has no such identifier. A port is known by its client's name,
`Midi Harbor`, and its own name, and those stay the same. The client *number* ALSA assigns
changes from one start to the next. Applications and scripts that connect by name
(`aconnect "Midi Harbor:Sequencer Bus" …`) keep working. Ones that stored numbers such as
`128:0` must be pointed at the port again.

On **Windows**, a port is known by its name alone. Programs list MIDI ports by position, and
the position changes as devices come and go, so a program that remembers a port by name finds it
again and one that remembers a position may not.

## How hardware is recognised

On **macOS**, a device is recognised by the identifier CoreMIDI keeps for it.

On **Linux**, a device is recognised by its USB serial number when it reports one, which holds
whichever socket it is plugged into. A device that reports no serial number is recognised by
the socket. Moved to another socket, it is left for `midi-harbor device resolve` rather than
guessed at.

On **Windows**, a device is recognised by the path Windows gives it, which names its USB vendor
and product. A device moved to another socket is still recognised by name and product, unless
two of the same model are attached, which are left for `midi-harbor device resolve`.

## A device another application is using

On **Linux**, an application can hold a device for itself. Midi Harbor reports the device as
claimed, names the application when ALSA says which one it is, and opens the device as soon as
it is let go.

On **macOS**, CoreMIDI shares every device among all applications, so a device is never claimed.

On **Windows**, Midi Harbor reports a device as claimed when Windows refuses to open it because
another program has it. Virtual ports, Midi Harbor's own and those loopMIDI and rtpMIDI create,
are shared.

## Sleep

Every platform tells Midi Harbor a sleep is coming. It releases notes held on other machines
before the machine goes, and reconnects when it wakes. Midi Harbor also notices a sleep on its
own, from the clock, because no platform's notice is reliable. On Linux, logind does not
announce the return from hibernation. Windows waits only about two seconds for a program before
it sleeps, which is ample for releasing notes.

## Advertising network ports

On **macOS**, the system's own Bonjour service advertises network ports.

On **Linux**, `avahi-daemon` must be running. Without it, network ports still work, but other
machines have to connect to them by address.

On **Windows**, the system's own DNS Client service advertises network ports. It splits a name at
a dot, so a dot in a port's name is advertised as a hyphen, and it advertises a name containing
any character outside plain ASCII in lower case: "Café Bus" is seen as "café bus" on other
machines.

## The firewall on Windows

The first time the daemon opens a network port, Windows Defender Firewall asks whether to allow Midi
Harbor on public and private networks. Allow it for other machines to find and invite this one;
Windows may ask for an administrator's approval. Connecting out to other machines works either way.
On a network Windows has marked Public, Windows also refuses traffic other machines start unless it
is allowed. The answer is kept for that copy of `midi-harbor.exe` at that path, so a binary moved or
installed elsewhere is asked about again.

## Running at login

On **macOS**, the service is a launchd agent. The Mac App Store build cannot register one from its
sandbox: the app runs the daemon itself, restarting it after a crash, and starts at login through
its **Start at login** setting. A crash of the window leaves the daemon and every connection
running, and opening the app again picks it up. On **Linux** it is a systemd user unit. A Linux
machine without systemd cannot install the service: run `midi-harbor daemon` from whatever
starts programs at login there. `midi-harbor capabilities` says when this is the case.

On **Windows** it is a Task Scheduler task that starts at sign-in, as the user and without
administrator rights. Task Scheduler does not restart a program that fails, so the task runs the
daemon under a small supervisor of its own, which starts it again two seconds after a crash.

## Advertising over Bluetooth

On **macOS**, the whole advertisement has to fit in one 31-byte packet, and the Bluetooth MIDI
service already takes most of it. A name of up to five characters fits beside it (`--name HM`).
A longer one is left out, and phones and tablets list the Mac under its own name, as set in
System Settings under General > Sharing.

On **Linux**, a name of any length is sent, in the scan response that follows the
advertisement.

On **Windows**, advertising this computer as a Bluetooth MIDI device is not available yet.
Connecting to Bluetooth MIDI devices is.

## Bluetooth permission

On **macOS**, a program must be granted permission to use Bluetooth. See
[Installation](installation.md#bluetooth-on-macos).

On **Linux**, BlueZ must be running and the adapter switched on. `midi-harbor capabilities`
reports which is missing.

On **Windows**, Bluetooth must be switched on in Settings.

## Moving over from Apple's MIDI setup

On **macOS**, `midi-harbor config import-apple` takes over the IAC Driver's buses and the network
sessions set up in Audio MIDI Setup. It reads Apple's setup and never changes it, so switch the
IAC Driver off in Audio MIDI Setup afterwards if applications should see only one of each port.
Linux and Windows have no Apple setup, and the command says so.
