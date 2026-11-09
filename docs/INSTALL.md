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

## Windows

Supported: Windows 10 (1809+) and Windows 11, x86_64.

Download one of these from the GitHub release (or from the `wzp-windows-x64`
CI artifact):

- `WarzonePhone_<version>_windows_x64-setup.exe`: NSIS installer, per-user,
  no admin rights needed. Use this one unless you need an MSI.
- `WarzonePhone_<version>_windows_x64.msi`: MSI package for machine-wide or
  managed (GPO/Intune) installs. Needs admin rights.
- `WarzonePhone_<version>_windows_x64.sha256`: checksums. Verify in
  PowerShell with `Get-FileHash .\WarzonePhone_*_windows_x64-setup.exe` and
  compare the hash with the one in the file.

### Getting past SmartScreen (unsigned build)

1. Run the installer. Windows Defender SmartScreen shows
   "Windows protected your PC".
2. Click **More info**.
3. Check that the file name is the one you downloaded, then click
   **Run anyway**.
4. If the browser blocked the download, keep it from the browser's downloads
   list (Edge: "..." > **Keep** > **Show more** > **Keep anyway**).
   You can also right-click the file > **Properties**, tick **Unblock**, and
   click **OK** before you run it.

### WebView2

The app needs the Microsoft Edge WebView2 runtime. It is already installed on
Windows 11 and on most up-to-date Windows 10 systems. If it is missing, the
installer downloads and installs it, so you need an internet connection the
first time. For offline machines, install the
[Evergreen Standalone runtime](https://developer.microsoft.com/microsoft-edge/webview2/)
first.

### Firewall prompt

WarzonePhone uses QUIC over UDP. Outgoing connections to a relay need no
firewall changes. When the app first opens a listening UDP socket for direct
peer-to-peer transfers, Windows Defender Firewall may show
"Windows Defender Firewall has blocked some features of this app":

- Allow **Private networks** to let peers on your home or office LAN connect
  to you directly.
- Allow **Public networks** only if you want direct connections on untrusted
  Wi-Fi too.
- If you click **Cancel**, Windows adds a block rule. Transfers still work,
  but they go through the relay (or need the other side to accept the
  connection). To change this later, go to
  **Windows Security > Firewall & network protection > Allow an app through
  firewall** and edit the WarzonePhone entry.

### Uninstalling

Use **Settings > Apps > Installed apps > WarzonePhone > Uninstall**.

## macOS

Download `WarzonePhone_<version>_macos_universal.dmg` (or
`..._macos_universal.app.tar.gz`) from the GitHub release. It is a
universal binary for Apple Silicon and Intel Macs, macOS 11 (Big Sur) or
newer.

1. Open the `.dmg` and drag **WarzonePhone** into **Applications**
   (or unpack the tarball: `tar xzf WarzonePhone_*_macos_universal.app.tar.gz -C /Applications`).
2. The app is only ad-hoc signed (not notarized), so Gatekeeper blocks
   the first launch ("cannot be opened because Apple cannot check it for
   malicious software" / "is damaged"). Bypass it once, either way:
   - **GUI:** try to open the app, dismiss the warning, then go to
     **System Settings → Privacy & Security**, scroll down and click
     **Open Anyway** next to the WarzonePhone message; confirm with your
     password. (On macOS 14 and older, right-click the app → **Open** also works.)
   - **Terminal:** remove the quarantine flag:
     ```sh
     xattr -dr com.apple.quarantine /Applications/WarzonePhone.app
     ```
3. On first use macOS asks for **Microphone**, **Camera** and (macOS 15+)
   **Local Network** access. Allow them; Local Network is needed to find
   and connect directly to devices on your LAN. You can change these later
   in System Settings → Privacy & Security.

Notes:
- On Intel Macs the software AV1 codecs (dav1d / SVT-AV1) are not
  included because their prebuilt libraries exist only for arm64; H.264 /
  HEVC via VideoToolbox work on both architectures.
- To verify a download: `shasum -a 256 -c SHA256SUMS-macos.txt`.

## Android

Builds come from `.github/workflows/release-android.yml` and are attached to
each GitHub release as `WarzonePhone_<version>_android_arm64.apk` (64-bit ARM,
i.e. practically every phone from the last ~8 years; Android 8.0 / API 26 or
newer). An `.aab` is published too, but that is only useful for app stores —
for sideloading, use the `.apk`. The app is not on Google Play, so you install
it by sideloading.

1. On the phone, open the release page and download
   `WarzonePhone_<version>_android_arm64.apk` (or copy it over USB).
   Optionally compare its SHA-256 with `SHA256SUMS-android.txt`.
2. Open the downloaded file (from the browser's downloads or a file manager).
3. Android will say that installing from this source is blocked. Tap
   **Settings** and enable **Allow from this source** for the app you used to
   open the APK (browser / Files). On older Android versions (7 and below)
   the setting is a global **Unknown sources** switch under
   Settings > Security.
4. Go back and tap **Install**. Play Protect may warn about an app from an
   unknown developer — choose **More details > Install anyway**.
5. On first launch, grant the microphone permission (and camera if asked).

You can turn "Allow from this source" off again after installing.

With adb instead: `adb install -r WarzonePhone_<version>_android_arm64.apk`.

**Updates.** Installing a newer APK over the old one keeps your data, as long
as both were signed with the same key. Official releases are signed with the
project's release key. If an APK was built without that key (e.g. a CI build
from a fork; the workflow prints a "THROWAWAY key" warning), Android refuses
the update with "App not installed" / `INSTALL_FAILED_UPDATE_INCOMPATIBLE`;
uninstall the old app first (this deletes its local data).

**armv7 (32-bit) phones** are not built by default; maintainers can produce an
`_android_armv7.apk` by running the workflow manually with the *armv7* option.
