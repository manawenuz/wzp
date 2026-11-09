# Installing WarzonePhone

All builds are **unsigned**. Each release asset has a matching `.sha256`
file — verify it before installing (`sha256sum -c <file>.sha256`).

## Linux

Builds are provided for `x86_64` and `aarch64` (arm64), produced on
Ubuntu 24.04. Pick one format:

| File | For |
|------|-----|
| `WarzonePhone_<ver>_linux_<arch>.deb` | Debian / Ubuntu / Mint / Pop!_OS |
| `WarzonePhone_<ver>_linux_<arch>.rpm` | Fedora / openSUSE / RHEL-family |
| `WarzonePhone_<ver>_linux_<arch>.AppImage` | any distro, no install |
| `wzp-cli-linux-<arch>.tar.gz` | CLI + server binaries (`wzp-relay`, `wzp-client`, `wzp-web`, `wzp-bench`) |

**Debian / Ubuntu**

```bash
sudo apt install ./WarzonePhone_<ver>_linux_<arch>.deb
```

`apt` pulls in the runtime dependencies (WebKitGTK 4.1, GTK 3,
`libwebrtc-audio-processing1` for echo cancellation). Remove with
`sudo apt remove warzonephone`.

**Fedora / openSUSE**

```bash
sudo dnf install ./WarzonePhone_<ver>_linux_<arch>.rpm     # or: sudo zypper install ./...
```

The app links the 0.3 line of the WebRTC audio-processing library
(`libwebrtc_audio_processing.so.1`); if your distro only ships the 1.x line,
use the AppImage instead.

**AppImage**

```bash
chmod +x WarzonePhone_<ver>_linux_<arch>.AppImage
./WarzonePhone_<ver>_linux_<arch>.AppImage
```

If it fails with a FUSE error, install `libfuse2` (Ubuntu 24.04:
`libfuse2t64`) or run it with `--appimage-extract-and-run`.

**CLI / relay tarball**

```bash
tar xzf wzp-cli-linux-<arch>.tar.gz
./wzp-relay --help
```

The binaries need glibc 2.39+ (Ubuntu 24.04, Debian 13, Fedora 40 or newer)
and `libasound2` at runtime.

Because the packages are unsigned, no repository key is involved; your
package manager may print a warning about an unsigned package, which is expected.
