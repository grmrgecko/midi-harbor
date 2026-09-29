# Research: Windows support

**Feature**: 013-windows-support | **Date**: 2026-09-26

Numbered after the original specification's research, whose last entry was R-082, so every R number in the
project names one entry. Findings marked **VERIFIED** were measured on the Windows test machine:
Windows 11 Pro 25H2 (build 26200) in a virtual machine, with rtpMIDI 1.1.14 and teVirtualMIDI 1.3.0.43,
reached over ssh as a standard administrator account and cross-compiled for from macOS.

---

## R-083: Task Scheduler does not restart a program that fails

**Status**: **VERIFIED** (2026-09-26).

**Decision**: The logon task runs `midi-harbor daemon --supervise`, which runs the daemon as a
child and starts it again after a failure: after 2 s, doubling while failures come less than a
minute apart, up to 60 s. A successful exit, which is what a daemon asked to stop returns, ends
supervision.

**Evidence**: A task with `RestartOnFailure` set to every minute, three times, ran
`cmd /c exit 3`. Task Scheduler recorded last result 3 and never ran it again. The setting covers
a task that fails to start, not a program that exits with an error.

**Rejected**: A trigger repeating every minute with `IgnoreNew`. It restarts after up to a minute
instead of two seconds, and it would start again a daemon the user had stopped.

**Also decided**: The task runs as the user with an interactive token and least privilege, so a
standard user can install it and the daemon runs in the user's session, where MIDI devices and
teVirtualMIDI ports are. A Windows service would run in session 0 and needs an administrator. The
command is `conhost.exe --headless`, so the console program opens no window at logon. The
priority is 4, Task Scheduler's normal; its default, 7, is below normal.

## R-084: The DNS Client service's DNS-SD and instance names

**Status**: **VERIFIED** (2026-09-26), with a capture on the Arch VM on the same subnet.

**Decision**: Advertise through `DnsServiceRegister`, looked up in `dnsapi.dll` at run time. A dot
in a session's name is sent as a hyphen.

**Evidence**: Registered with no addresses, the service answers with the host's own and keeps
answering: `avahi-browse -r` on the Arch VM resolved `WinHarbor` to `192.0.2.47:58332`, and the
records went out with goodbyes when the daemon stopped. A name with a dot, escaped as DNS-SD
defines, went out as two labels, `Win Harbor\` (eleven bytes, the backslash kept) and `Test`,
and no browser listed it. A name with any character outside ASCII is lowercased: `Café Test` was
announced as `café test`, while `WinHarbor` kept its case. So the dot's stand-in is ASCII; a
lookalike dot lowercased the whole name. This daemon recognises its own records by address and
port, so the changed name does not make it list itself.

**Consequence**: A session whose name holds non-ASCII characters is advertised lowercased on
Windows. Nothing breaks; the name looks different on other machines.

**Browsing** through `mdns-sd` works on Windows as elsewhere: a Windows daemon found a Linux
daemon's port and resolved it.

## R-085: Windows makes a socket on `[::]` IPv6-only

**Status**: **VERIFIED** (2026-09-26). Fixed.

Every invitation a Windows daemon sent to an IPv4 peer failed with "the requested address is not
valid in its context" (10049), and the session waited for a network that was there. Linux and
macOS make a socket bound to the unspecified IPv6 address dual-stack; Windows sets `IPV6_V6ONLY`
by default. The session sockets now clear it explicitly through `socket2`. The existing test that
sends to an IPv4 peer fails on Windows without the change. With it, a Windows daemon joined a
Linux daemon's session at 1.0 ms latency.

## R-086: teVirtualMIDI and WinMM under churn

**Status**: **VERIFIED** (2026-09-26), by PowerShell probes against the driver and by the Windows
integration tests.

- **A slot reused at once keeps its old name.** Closing a port and creating another under a new
  name straight away left WinMM showing the old name, in every process, on six of ten tries, for
  at least five seconds. Created while the old port was still open, the new port showed its own
  name within 28 to 54 ms on ten of ten. With half a second or more between the close and the
  create, the new port always appeared. Renaming in the daemon is a destroy and a create, so the
  backend retires a destroyed port and closes it once the next port exists or after a second.
  With retirement off, the eight-rename test failed five runs of five, on the first rename each
  time; with it, it passed five of five.
- **A closed port lingers.** WinMM often lists a closed port, under its old name or as
  `(unavailable)`, until the driver's next change, in fresh processes too; this is the driver,
  not a cache in one process. The backend keeps names of ports it closed out of its device list
  for up to a minute, and never offers `(unavailable)` application ports.
- **The halves of one port are listed separately.** A new port's input can be listed before its
  output, and each can carry a different `tevmidiN` slot number. Application ports are therefore
  identified by name, which the driver keeps unique.
- **A name already taken** is refused with `ERROR_ALIAS_EXISTS` (1379) by driver 1.3.0.43, not
  the `ERROR_ALREADY_EXISTS` its header suggests. Both are reported as a naming conflict.
- **The first message after another application opens a port can be lost.** In four of
  thirteen runs of the suite, one note sent a few milliseconds after the open arrived only when
  sent again. The tests send until heard and report how many sends it took.
- **Positions move.** WinMM opens by position, and a port closing while another is opened shifted
  the list under the open: a handle once opened a closing port and failed with "undefined
  external error". The backend reads the lists until two readings agree, checks each opened
  handle's own position against what was meant, and retries.

**Timing**: a new port appeared in WinMM 30 to 390 ms after creation.

## R-087: The control channel on Windows

**Status**: **VERIFIED** (2026-09-26).

**Decision**: A named pipe with a random name, `\\.\pipe\midi-harbor-<uuid>`, which refuses
remote clients. The daemon creates it before recording its name in a file at the socket path,
in the user's temporary directory, and clients read the name from there. A record naming anything
but one of our pipes is not followed.

**Rationale**: Tokio has no AF_UNIX on Windows. A fixed pipe name could be created first by
another local user, who would then receive every command the CLI sent. A random name recorded in
a file only the user can read keeps a file permission as the gate, as the socket's mode is on
Unix. The pipe's default access lets other users open it only for reading, which is not enough
to send a request.

**Stopping**: Windows has no terminate signal to send. The daemon waits on an event named
`Local\midi-harbor-<uuid>-stop`, in the session's namespace, and `service stop` sets it, so the
daemon silences held notes and ends sessions before exiting. Ending the task instead is the last
resort after five seconds. Verified: `service stop` against a running daemon logged
"daemon stopped" after its shutdown path.

## R-088: Windows lets a dual-stack socket share its port

**Status**: **VERIFIED** (2026-09-26). Fixed.

The daemon's session tests failed on Windows: a switched-off session's port read as free while
the session still held it. Probed from Rust on the test machine, with a dual-stack `socket2`
socket bound, a plain IPv4 socket and a plain IPv6 socket could each bind the same port; a
second IPv4 socket on an IPv4 socket's port was refused; and a dual-stack socket bound a port an
IPv4 socket already held. Any program could take a session's datagrams. With
`SO_EXCLUSIVEADDRUSE` set before binding, the other binds were refused with 10013, and the
exclusive socket was refused a port held by either family with 10048. Session sockets set it on
Windows. It is the Windows form of the sharing R-080 found on macOS.

## R-089: Cross-compiling and testing

**Status**: **VERIFIED** (2026-09-26).

`x86_64-pc-windows-gnu` with Homebrew's `mingw-w64` builds the whole workspace, the libcosmic
interface included, on the development Mac. `zeroconf` does not build for Windows, since it wants
Apple's Bonjour SDK, so it moved behind the platform crate's responder module.

Test binaries are built with `cargo test --no-run` and run on the test machine by
`scripts/windows-test.sh`. Tests that find the binary or the sources through paths compiled in on
the Mac, `CARGO_BIN_EXE_midi-harbor` and `CARGO_MANIFEST_DIR`, read them from the root of the
current drive on Windows, so the script mirrors the tree under `C:\mh-win\root` and runs the tests
from a drive substituted onto it.

The interface cross-builds at the pinned libcosmic revision `87ab8179` without the `accesskit`
override the owner's notes (`veris/docs/libcosmic.md`) need for newer libcosmic, and it rendered on
the test machine: the Endpoints page listed the daemon's ports. The executable is a console program,
so the interface releases a console only it is attached to, which closes the empty window Windows
otherwise opens beside it.

## R-090: A daemon that did not finish exiting

**Status**: **CLOSED** (2026-09-26). Mitigated.

A daemon run as the service for 27 minutes, with a teVirtualMIDI port, the software synthesiser
open, and a session whose peer had just left, logged "daemon stopped" on `service stop` and never
exited. One thread remained, using no CPU; `taskkill /F` and `Stop-Process -Force` both failed,
and it still held its executable open. A process that cannot be killed is waiting in kernel mode,
so a driver's cleanup at process exit is the likely place. It no longer held a teVirtualMIDI port.

Fifteen start and stop cycles since, from the service and from a shell, with and without a
virtual port and the synthesiser open, all exited. The daemon now closes every
port and device it holds after ending its sessions, before returning, so no driver's close runs in
the process's teardown. Whether that removes the cause is unknown until it recurs or does not.

## R-091: Moving a network port without stopping it first

**Status**: **CLOSED** (2026-09-26). Fixed in T230.

`a_udp_port_that_cannot_be_bound_leaves_the_network_port_where_it_was` failed in two of the Linux
gate runs, the network port found on a third pair after a refused move was undone. It passed in
71 isolated runs, and in three full runs each of this branch and of the commit before it. Moving
a port stopped it, tried the new pair, and on finding it taken started it again on the old one; a
daemon starting beside it in the same test process could hold the old pair for longer than the
bind waits (R-082). The new pair is now bound and released before the port is touched, so a
taken pair is refused while the port still runs where it was, and the test checks the port was
never restarted.

## R-092: Windows Defender Firewall

**Status**: **VERIFIED** (2026-09-26).

Windows asked whether to allow `midi-harbor` on public and private networks the first time the
daemon opened a network port, with the test machine's network marked Public. Sessions the Windows
daemon started worked before any answer. Once the owner allowed it, a Linux daemon invited the
Windows port by address and joined at 0.9 ms. The rules Windows makes are for one program path: a
copy at another path, `C:\mh-win\bin\midi-harbor.exe`, was prompted for again, and ended with
block rules when the prompt went unanswered.

## R-093: Windows MIDI Services

**Status**: **VERIFIED** (2026-09-26) through the App SDK; the in-box API awaits Windows'
late-2026 update.

Windows MIDI Services has two forms of its API over one service. `Windows.Devices.Midi2` is part
of Windows from the update Microsoft's repository says ships starting the last week of November
2026. Its preview packages are for development only. The App SDK,
`Microsoft.Windows.Devices.Midi2`, RC4 1.0.17-rc.4.25, which upstream now labels old, works with
the service Windows already has (10.0.26100.7705 on the test machine) once the user installs the
"Windows MIDI Services SDK Runtime and Tools". A program reaches it through the COM class
`MidiClientInitializer`, created first and kept. The backend uses the in-box API whenever Windows
registers `Windows.Devices.Midi2.MidiSession` under `ActivatableClassId`, and the App SDK
otherwise. It asks the registry rather than activating the class: the preview API copied beside
the program activated on today's Windows, and `CreateVirtualDevice` through it never returned.

- **Session 0.** A virtual device created from an SSH session is never answered. From the
  desktop session, run as an interactive scheduled task, the same code creates it at once. The
  daemon runs at logon in the desktop session; the integration tests have to be started there.
- **The identifier.** The service refused a product instance identifier containing spaces, with
  status 808. It takes ASCII letters, digits, `-` and `_`, 32 at most, so the port's name is
  hashed: `mh-` and sixteen hex digits of its 64-bit FNV-1a. A second device of the same name has
  the same identifier and is refused with no object and no error code, which the bindings report
  as "The operation completed successfully"; the backend refuses a name its own port already
  shows before asking.
- **Receiving.** A port sends as soon as its connection opens, but receives nothing until the
  device is added to the connection as a message processing plugin, as Microsoft's sample does.
- **Naming.** Through RC4 against today's service, a port of two connectors appeared in WinMM as
  "Harbor Split 8640" and "Harbor Split 8640 Gr 2". The service ignored the function block names
  and fell back to naming by group. Upstream's current source calls that fallback a last chance
  that should not be seen in production. The backend recognises its own ports under either
  naming.
- **Closing.** `DisconnectEndpointConnection` never returns, and from then on the service
  answers nothing: the next `CreateVirtualDevice` waited for good. This is Microsoft's issue
  #1236, a duplicate of #1047, fixed for the late-2026 update. Keeping the device object until
  after the disconnect, as the sample does, changed nothing. Ending MidiSrv and starting the
  service recovered it every time. So every call into the service runs on a thread of its own and
  is waited for 5 s. A call that outlives that wait is left running and counted. While any is,
  opening a session or creating a port is refused at once, and virtual ports are reported
  unavailable, naming the service to restart. With all seven integration tests in one process,
  the first passed, its close was given up after 5 s, and the other six were refused within the
  same run of 7.4 s, none waiting on the service.
- **Starting.** Opening a session on a service that had just been stopped took more than 5 s once,
  and was refused. Opening a session is waited for 30 s.

With the service restarted before each, six of the seven integration tests passed: MIDI and a
system-exclusive dump both ways, own ports kept out of the device list, two connectors in the
right directions, a taken name refused, another application's port announced. The rename test
closes a port and creates another, which today's service cannot do.

## Notes

- **Distributing the Windows build.** The teVirtualMIDI SDK page (2026-09-26) says: "Software
  linking to this SDK MAY NOT BE DISTRIBUTED in any way without prior clearance with me (Tobias
  Erichsen)", and offers the MSI module that installs the driver to licensees only. Clearance is
  requested at info@tobias-erichsen.de. Midi Harbor no longer uses it: virtual ports go through
  Windows MIDI Services (R-093).
- **USB hardware exclusivity.** WinMM gave the synthesiser and teVirtualMIDI ports to two
  processes at once on the test machine. The daemon opens every present device, as it does on
  the other platforms; if a Windows release opens USB MIDI devices exclusively, the daemon would
  keep other applications from them.
