# Implementation Plan: Windows support

**Branch**: `002-windows-support`, this spec's original name | **Date**: 2026-09-26 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/013-windows-support/spec.md`

## Summary

Give each of the four platform seams a Windows implementation, move the two pieces that were
Unix-only outside the seams (the IPC transport and service installation) onto a Windows
equivalent, and cross-compile from macOS with mingw-w64. The Windows test machine runs the
cross-compiled test binaries; nothing is compiled on it.

## Technical Context

**Target**: `x86_64-pc-windows-gnu`, linked by `x86_64-w64-mingw32-gcc` (Homebrew `mingw-w64`),
configured in `.cargo/config.toml`.

**New dependencies**: `windows` 0.62, `windows-core` 0.62 and `windows-collections` 0.3, for the
WinRT classes of Windows MIDI Services, whose bindings `scripts/midi2-bindings` generates from
Microsoft's metadata with `windows-bindgen` 0.64; `windows-sys` 0.61 (already in the tree through tokio), for WinMM, the power
manager, DNS-SD, events and loading DLLs; `socket2` 0.6 (already in the tree), for dual-stack
sockets on every platform. `zeroconf` moves from the daemon to the platform crate and is not built
on Windows, where it would need Apple's Bonjour SDK.

**Test machine**: Windows 11 Pro 25H2 (build 26200), with rtpMIDI 1.1.14 and so teVirtualMIDI
1.3.0.43, and the Windows MIDI Services SDK Runtime and Tools RC4 (1.0.17-rc.4.25) against the
in-box service 10.0.26100.7705; no Bluetooth radio and no USB MIDI hardware.

## Decisions

| Area | Windows implementation | Why |
|---|---|---|
| Virtual ports | Windows MIDI Services virtual devices, one function block and group per connector: through `Windows.Devices.Midi2` when Windows registers it, otherwise through the App SDK runtime | teVirtualMIDI may not be distributed without its author's clearance. Windows carries the API from its late-2026 update; the App SDK serves until then (R-093). |
| Service calls | Each on a thread of its own, waited for 5 s, 30 s when opening a session; once one outlives its wait, virtual ports are refused until it returns | The service before the late-2026 update never answers a device's disconnection (R-093). |
| Devices | WinMM, input by callback into the ring, output by short and long messages | Every other interface's ports appear in WinMM. |
| Device identity | Interface path for hardware; name alone for application ports; this process's own ports by their connector names and by the service's group names | teVirtualMIDI numbers the halves of a port separately and renumbers them (R-086); the service before the late-2026 update names ports after groups (R-093). |
| Hot-plug | The lists compared once a second | WinMM announces nothing without a window to post to. |
| IPC | Named pipe, remote clients refused, random name recorded at the socket path | Tokio has no AF_UNIX on Windows; the recorded name keeps a file permission as the gate (R-087). |
| Stopping | A named event in the session namespace, set by `service stop` | Windows has no SIGTERM; ending the task would skip releasing notes. |
| Service | Task Scheduler logon task, interactive token, least privilege, normal priority, `conhost --headless` | A Windows service runs in session 0 and needs an administrator. |
| Crash restart | `daemon --supervise`, restarting after 2 s, doubling to 60 s while failures come quickly | Task Scheduler does not restart a program that exits with an error (R-083). |
| Advertising | The DNS Client service's `DnsServiceRegister`, looked up at run time | The system responder answers other machines for as long as the registration lasts; dots become hyphens (R-084). |
| Sockets | `IPV6_V6ONLY` cleared explicitly | Windows defaults to IPv6-only (R-085). |
| Sleep and wake | `PowerRegisterSuspendResumeNotification`, holding a suspend up to 1.9 s | The polled watcher runs underneath, as on the other platforms. |
| Bluetooth | Central through btleplug's WinRT backend; peripheral not built | No radio was available to verify either role; the central code is shared with the other platforms. |

## Constitution Check

- **I. Resilience**: crash restart by the supervisor; opens retried while the device list moves;
  a stalled Windows MIDI Service costs virtual ports and nothing else.
- **II. Daemon owns state**: unchanged; the service is a logon task.
- **III. Real-time safety**: the WinMM and Windows MIDI Services callbacks only translate and scan
  into the ring and, for WinMM, hand system-exclusive buffers back. They allocate, lock and log nothing. Buffers are
  allocated when an input opens.
- **IV. Parity**: every seam has a Windows implementation except the BLE peripheral, declared in
  the Platform Support Matrix and reported unavailable (amendment 1.1.0).
- **V. Protocol correctness**: unchanged code; a Windows daemon joined a session with Linux.
- **VI. Testable without hardware**: device identity, port naming, UMP translation, message packing, service
  definitions and the supervisor's pacing are pure and tested on every platform.
- **VII. Observable**: unchanged.
- **Unsafe code**: confined to the platform crate, each block with a `SAFETY` comment.

## Complexity Tracking

| Deviation | Why | Simpler alternative rejected |
|---|---|---|
| BLE peripheral not built on Windows | No Windows machine with a radio to verify it | Enabling `ble-peripheral-rust`'s WinRT backend unverified |
| A supervisor mode in the binary | Task Scheduler does not restart a failed program (R-083) | A task repeating every minute, which restarts after up to a minute and also restarts a daemon the user stopped |

## Project Structure

```
.cargo/config.toml                     the mingw-w64 linker for the Windows target
crates/platform/src/dll.rs             run-time DLL loading
crates/platform/src/responder/         advertising: zeroconf, or DNS-SD on Windows
crates/platform/src/stop.rs            the stop event
crates/platform/src/midi/windows.rs    the backend behind the MIDI seam
crates/platform/src/midi/winmm.rs      WinMM inputs and outputs
crates/platform/src/midi/wms/         Windows MIDI Services ports, in-box and App SDK
crates/platform/src/midi/ump.rs        Universal MIDI Packet translation, tested everywhere
crates/platform/src/midi/winmm_identity.rs  identity and message packing, tested everywhere
crates/platform/src/sysevents/windows.rs    suspend and resume
crates/platform/tests/windows_midi.rs  the Windows integration tests
crates/ipc/src/transport/windows.rs    the named pipe
crates/service/src/taskscheduler.rs    the logon task
crates/service/src/supervisor.rs       restarting after a crash
scripts/midi2-bindings/                generating the Windows MIDI Services bindings
```
