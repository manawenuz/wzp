# Installing WarzonePhone

Release builds are **unsigned**: no Apple / Microsoft code-signing
certificates are used. Each platform section below explains how to get
the OS to run them anyway.

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
