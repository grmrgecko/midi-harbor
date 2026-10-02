# Research: AppImage

The investigation behind this spec, under its number in the project-wide research log.

---

## R-104: Whether an AppImage can carry the daemon

**Status**: **VERIFIED** (2026-09-29) on Arch Linux (x86_64) and Ubuntu 22.04 (both
architectures). Built as T246 and T247.

**The runtime.** AppImage's type 2 runtime (AppImage/type2-runtime, `runtime.c`, release
20251108) mounts the image through FUSE at `/tmp/.mount_<first six letters of the file name><six
random characters>`, sets `APPIMAGE` to the file and `APPDIR` to the mount, and replaces itself
with `AppRun`. A forked child serves the mount while any process holds the read end of a pipe it
leaves open across `exec`, and exits once none does. The runtime is static and needs only a setuid
`fusermount` or `fusermount3`, not libfuse2, so Ubuntu 22.04 and later run it as installed.

**A daemon works under systemd.** The unit's `ExecStart` names the AppImage file, so every start,
at login or after `Restart=always`, mounts it afresh. systemd tracks the process that became
`AppRun`, and the mount's child runs in the same cgroup. The daemon replaces itself with `exec`
only when CoreMIDI reports its server gone (R-079), which never happens on Linux; an `exec` would
keep the mount in any case, since the process keeps the pipe.

**Stopping has to be ordered.** systemd's default `KillMode=control-group` sends SIGTERM to every
process in the unit at once. The mount's child unmounts on SIGTERM while the daemon is still
shutting down, and the daemon then touches a page of its executable that was never read: every
`systemctl --user stop` and every logout ended in SIGBUS and a core dump, before the daemon had
released its notes. `KillMode=mixed` signals only the daemon, but SIGKILLs the rest the moment it
exits, before the child can unmount, which left one dead mount in `/tmp` per stop. The unit
therefore has an `ExecStop` that sends SIGTERM to `$MAINPID` and waits for it to exit. systemd
then signals what remains as usual, by which time the child has seen its pipe close and unmounted.
From a package there is nothing else in the unit, so it stops as before.

After a crash, systemd's SIGTERM reaches only the child, which unmounts but leaves its empty
directory in `/tmp`; a normal stop removes it. That is the runtime's behaviour and is cleared at
reboot.

**The unit must not name the running executable.** `current_exe` gives the path inside the mount,
which is gone once the daemon exits, so a unit naming it fails at the next login. Programs started
from an AppImage inherit both variables, so a copy installed from a package and started from an
AppImage terminal sees that terminal's `APPIMAGE`. The registered executable is therefore the
`APPIMAGE` file only when the running executable is under `APPDIR`.

**Libraries.** The release binary links `libxkbcommon`, `libavahi-client`, `libavahi-common`,
`libasound`, `libdbus-1`, `libm` and `libc`; the interface loads Wayland, X11, EGL and Vulkan while
it runs. Only the two Avahi libraries are bundled, from the Debian 12 sysroot:

- AppImage's excludelist (AppImageCommunity/pkg2appimage) names `libasound.so.2`, since a bundled
  copy finds no sound cards, and the graphics libraries, which belong to the driver.
- `libxkbcommon` stays with the host, whose `libxkbcommon-x11`, loaded by the interface, is built
  against the host's own.
- The Avahi client talks to the Avahi daemon over D-Bus, and a desktop may have the daemon without
  the client library.

The bundled libraries are found through the binary's RUNPATH, `$ORIGIN/../lib`, set with patchelf.
`LD_LIBRARY_PATH` in `AppRun` would reach every command the daemon runs, such as `systemctl`.

**The window on X11.** Tested in XFCE, the window was listed as "Untitled window" with the class
`Midi-Harbor-0.1.0-x86_64.AppImage`, so no desktop paired it with its entry. Two causes, neither
the AppImage's alone:

- The GUI never set a window title, so every Linux desktop showed none; the header draws its own,
  which is why it went unnoticed.
- libcosmic at the pinned revision (`iced/winit/src/conversion.rs`) builds winit's X11 attributes
  with the application ID as the window's name, then replaces them with the Wayland attributes, so
  on X11 winit falls back to the file name of `argv[0]` for `WM_CLASS`
  (`winit-x11/src/window.rs`). From a package that is `midi-harbor`; from an AppImage it is the
  AppImage's file name, which the runtime passes as `argv[0]`. The desktop entry named
  `com.mrgeckosmedia.MidiHarbor`, which only Wayland's application ID ever matched.

The entry's `StartupWMClass` is now `midi-harbor`, and the AppImage's `AppRun` is a bash script
that execs the binary with `exec -a midi-harbor`, so both builds carry that class. Wayland pairs by
the application ID and the entry's file name, which are unchanged. `exec` keeps the process, so the
unit's main process, the `ExecStop` and the runtime's mount behave as before.

**Updating the file.** Writing into the AppImage while the daemon runs fails with "Text file busy",
the mount's child running from it. Moving a new file over it works, and the daemon runs the new one
from its next start.

**Evidence**:

- Arch Linux, fuse3 only: `service install --start` wrote
  `ExecStart=/root/Apps/Midi-Harbor.AppImage daemon`; `/proc/<pid>/maps` showed both Avahi
  libraries from the mount and `libasound`, `libdbus-1` and `libxkbcommon` from `/usr/lib`; the
  ALSA sequencer's Midi Through port was listed; a network port was advertised on
  `_apple-midi._udp` and `network discover` found Windows peers.
- After `kill -9`, systemd started the daemon again within four seconds on a new mount, and the old
  mount was gone.
- With the unit as first written, `systemctl --user stop` and logging out both ended in
  `code=dumped, status=7/BUS`. With the `ExecStop`, three stops in a row each logged "daemon
  stopped", left no process and no mount, and a crash still restarted it.
- Logging out, letting root's user manager stop, and logging in again over SSH started the unit
  with `default.target`, which is what a login does. With the `ExecStop`, the logout stopped the
  daemon cleanly and left one mount, the new daemon's.
- Renamed to `Midi-Harbor-0.2.0-x86_64.AppImage`, `service status` reported the registration
  stale, naming the old path, and `service install` from the new name repaired it.
- Ubuntu 22.04, glibc 2.35, without Avahi: the arm64 AppImage ran through FUSE in a container.
  The x86_64 one ran unpacked, since Docker's x86_64 emulation refuses the AppImage magic bytes
  in the ELF header's padding, and resolved both Avahi libraries from the bundle.
- The window, on the Arch VM's Hyprland session as its desktop user, offered to register with
  systemd. A new AppImage's first launch took 22 seconds to show it, reading the image from the
  VM's disk; later launches took three to six seconds, even with a fresh home directory and the
  VM's page cache dropped, the host's cache still holding the image. The first two launches of the
  first build were checked at about eight seconds and taken for failures before this was known.

- XFCE on X11, as the desktop user: the window's "Install and start" registered
  `ExecStart=/home/<user>/Applications/Midi-Harbor-0.1.0-x86_64.AppImage daemon`, started it, and
  listed the ALSA Midi Through port. With the fixes the window is listed with the class
  `midi-harbor` and the title "Midi Harbor", and launched through its desktop entry, as AppImage
  integration tools install it, the taskbar shows the entry's icon.
- A new build moved over the registered file while the daemon ran, then `systemctl --user restart`:
  the old daemon logged "daemon stopped", the new one started, and no core dump.

- A reboot with the service installed from the window: the daemon logged "daemon stopped" as the
  machine shut down, and started from the AppImage 35 seconds later, when SDDM logged the user in.

**Not checked**: an arm64 machine outside Docker.

---

## R-109: The AppImage's window showed the generic Wayland icon

**Status**: **FIXED** (2026-10-02), on X11; on Plasma under Wayland the entry is found and the
icon's redraw in a running shell is not yet confirmed. Built as T254.

The owner reported that the window of the AppImage showed the Wayland icon instead of Midi
Harbor's. A Wayland desktop does not take an icon from the window. It takes the window's
application ID, `com.mrgeckosmedia.MidiHarbor`, and looks for a desktop entry of that name in the
XDG data directories. A package installs one in `/usr/share/applications`. The AppImage carries
the entry and the icon inside its own tree, where no desktop looks unless an integration tool such
as AppImageLauncher copies them out, so the lookup found nothing. R-104 checked the icon only for
an AppImage launched through an entry such a tool had installed.

**Fix.** Run from an AppImage, the window installs the entry and the icon itself before it opens:
`com.mrgeckosmedia.MidiHarbor.desktop` under `$XDG_DATA_HOME/applications` and the SVG under
`icons/hicolor/scalable/apps`, read from the AppImage's own tree. `Exec` is rewritten to the
AppImage file, quoted as the Desktop Entry Specification requires, and `TryExec` names the file,
so a desktop drops the entry once the AppImage is deleted, which is how an AppImage is removed.
Each file is written only when it differs, so an AppImage that has moved is followed and one
that has not touches nothing.

**Not over a package.** An entry in the user's directory takes precedence over the system's. With
a package installed as well, it would point the package's menu item at the AppImage, so nothing is
installed when a directory in `XDG_DATA_DIRS` already holds the entry. The package's entry gives
the window its icon in that case.

**The other way considered.** The `xdg-toplevel-icon-v1` protocol lets a window hand the
compositor an icon. Few compositors implement it and the pinned libcosmic's winit does not, and it
would not put the AppImage in the application menu.

**Checked** on Arch Linux under XFCE on X11, with the program run from a directory laid out as the
AppImage runtime mounts it, in a path holding a space, and `APPIMAGE` and `APPDIR` set as the
runtime sets them. The entry installed passed `desktop-file-validate`, and the taskbar, which had
shown the window with no icon, showed Midi Harbor's. On the first run the icon was not written,
because that machine's `icons/hicolor` directory belonged to root from an earlier test; the entry
was installed without it, and the icon followed once the directory was the user's.

**On Plasma, a blank icon.** With the released AppImage on Plasma 6 under Wayland the generic
icon was gone and a blank stood in its place. The entry and the icon were both installed, a new
KDE process resolved the icon's name to the file (`kiconfinder6`), and KDE's renderer drew it
correctly (`ksvgtopng`). The shell itself had been running for two weeks and had read the icon
theme's directories when it started, before `hicolor/scalable/apps` existed under the user's data
directory, so it found the entry and no icon. After writing the icon the window now sends
`org.kde.KIconLoader.iconChanged` on the session bus, which is what KDE's own programs send to
have every running program reload its icons, and sets the theme directory's modification time
for loaders that compare times. Both are done only when the icon was written.

**Not covered:** whether the shell redraws the icon on that signal without the window being
opened again, which only the owner's desktop can show, and a real AppImage carrying this last
change.
