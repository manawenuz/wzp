# Installing WarzonePhone

Release builds are **unsigned**. Each platform section below explains how to
get past the operating system's warning for unsigned apps.

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
