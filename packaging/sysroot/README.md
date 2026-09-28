# Building the sysroots

The release build cross-compiles for Linux against Debian 12's libraries, so a Linux package runs
on Debian 12, Ubuntu 22.04 and anything newer. `build.sh` builds a Debian image for each
architecture and keeps only its headers, libraries and pkg-config files, in
`target/sysroot/linux_amd64` and `target/sysroot/linux_arm64`.

It needs Docker with the buildx extension and emulation for the other architecture. Docker
Desktop has both. On Linux, install qemu's binfmt support and check it is registered:

```bash
ls /proc/sys/fs/binfmt_misc/
```

Then, from the repository root:

```bash
make build-sysroot
```
