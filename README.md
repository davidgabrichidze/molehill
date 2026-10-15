<p align="center"><img src="docs/logo.svg" width="120" alt="Molehill"></p>

<h1 align="center">molehill</h1>

<p align="center">One small, good-looking VPN client for Windows that digs all your tunnels:<br>
OpenVPN, Cisco AnyConnect, Fortinet and Palo Alto GlobalProtect.</p>

---

Molehill is the Windows sibling of
[omarchy-plugin-vpngui](https://github.com/giorgobiani/omarchy-plugin-vpngui):
an Omarchy-flavoured (Tokyo Night, monospace accents) app that still feels at
home on Windows 11 (native title bar, Fluent controls, tray icon).

## Status

| VPN | Sign-in | Status |
|-----|---------|--------|
| **OpenVPN**: from a `.ovpn`, fetched from an OpenVPN Access Server with your username/password, or entered by hand (address, port, UDP/TCP, CA, tls-auth/tls-crypt) | Username & password, certificates, private key passphrase, static & dynamic challenge (OTP), browser SSO / SAML (`WEB_AUTH`, `OPEN_URL`, `CR_TEXT`) | ✅ Milestone 1 |
| **Cisco AnyConnect** / ocserv (via OpenConnect) | Whatever form the server sends (group, username, password, OTP / second password), browser SSO / SAML, untrusted-certificate pinning | ✅ Milestone 2 |
| **Fortinet** FortiGate (via OpenConnect) | Username & password, realm, 2FA codes (SMS, email, FortiToken, push), browser SSO / SAML, certificate pinning | ✅ Milestone 3 |
| **Palo Alto GlobalProtect** (via OpenConnect) | Username & password, portal → gateway (picked automatically when there is one, or the saved one), 2FA challenges, browser SSO / SAML in your default browser, certificate pinning. No HIP reports yet | ✅ Milestone 4 |

What already works:

- Several VPNs connected at once, each with its own switch; the header switch is
  the master switch (off disconnects everything, on reconnects the last one).
- Password, OTP and challenge prompts show up inline on the connection card.
  Browser sign-in opens your default browser.
- **Remember password** stores it in Windows Credential Manager (per user, DPAPI).
  A password the server rejects is forgotten automatically.
- **Components**: Molehill finds the latest OpenVPN release, downloads the
  official MSI, checks it is signed by OpenVPN Inc., and installs only what it
  needs (core + DCO/TAP drivers, no second GUI). It checks for updates twice a
  day and offers them in the app.
- Tray icon colored by state (blue idle, yellow connecting, green connected),
  with its own themed menu to flip tunnels on and off. Closing the window
  keeps your tunnels up.
- Live traffic counters, connected time, and a per-tunnel log.

## Install

Download `molehill-setup-<version>.exe` from Releases and run it. It installs
the app and the Molehill service. Open Molehill. If OpenVPN is missing, the
app offers to install it.

## How it works

```
 molehill.exe (you)                     molehill-service.exe (LocalSystem)
 ┌─────────────────────┐   named pipe   ┌──────────────────────────────────┐
 │ Slint UI + tray     │ ◄────────────► │ tunnel manager                   │
 │ profiles (%APPDATA%)│   JSON lines   │  ├─ openvpn.exe ◄─ management if │
 │                     │                │  └─ oc-auth helper → openconnect │
 │ Credential Manager  │                │ component installer (MSI, signed)│
 └─────────────────────┘                └──────────────────────────────────┘
```

- **The app** runs as you. It keeps your profiles in `%APPDATA%\Molehill` and
  your saved passwords in Credential Manager, and opens browsers for SSO.
- **The service** runs as LocalSystem, because creating tunnels, routes and
  DNS needs it. It never shows UI. It drives `openvpn.exe` through the
  management interface, so credentials never touch the disk.
- **IPC** is the pipe `\\.\pipe\molehill`, limited to SYSTEM, Administrators
  and interactive users.

### Security model

openvpn.exe runs as SYSTEM, so a config is effectively code with full
privileges. Molehill locks it down:

1. When you import a profile, the app (running as you) inlines every file the
   `.ovpn` references (`ca`, `cert`, `key`, `tls-auth`, `pkcs12`, …). The
   service only ever receives one self-contained text.
2. The service re-checks that text and refuses anything that runs programs,
   loads libraries, writes files or still points at a path (`up`, `down`,
   `plugin`, `script-security`, `log`, `management`, `providers`, …).
   It also forces `--script-security 1`.
3. Generated configs and management passwords live in
   `%ProgramData%\Molehill\run`, readable only by SYSTEM and Administrators,
   and are deleted as soon as they are no longer needed.
4. The OpenVPN installer is downloaded over HTTPS and runs only if its
   Authenticode signature is valid and issued to OpenVPN Inc. OpenConnect
   publishes no signed Windows installer, so Molehill takes the official CI
   build of each tagged release from gitlab.com/openconnect/openconnect
   over HTTPS.
5. OpenConnect sign-in runs in a separate helper process that loads
   libopenconnect, so a failure there cannot take the service down.

## Building

Requirements: Rust (stable, 2024 edition) and the Visual Studio Build Tools
(MSVC). Everything else comes from crates.io.

```powershell
cargo build --release
# installer (needs Inno Setup 6: winget install JRSoftware.InnoSetup)
iscc installer\molehill.iss
```

### Development loop

```powershell
# elevated terminal: install the service from your build
target\debug\molehill-service.exe install
target\debug\molehill.exe
```

UI work without admin rights: run the service in the foreground against a
scratch directory (tunnels need elevation, everything else works):

```powershell
$env:MOLEHILL_SERVICE_DIR = "$env:TEMP\molehill-svc"
$env:MOLEHILL_USER_DIR    = "$env:TEMP\molehill-user"
Start-Process target\debug\molehill-service.exe run
target\debug\molehill.exe
```

```powershell
cargo test   # config sanitizer, importer, version compare
```

Headless screenshots of the main screens (demo data, nothing shown on screen):

```powershell
cargo run -p molehill --example snapshot -- snapshots
```

### Layout

```
crates/
  molehill-core/      shared: IPC protocol, profiles, .ovpn import/sanitize, secrets
  molehill-service/   Windows service: pipe server, tunnels, component installer
  molehill/           the app: Slint UI (ui/*.slint), tray, IPC client
installer/            Inno Setup script
```

## Roadmap

1. ✅ **OpenVPN**: profiles, every sign-in type, components, tray
2. ✅ **OpenConnect engine + Cisco AnyConnect**: installs and updates
   OpenConnect, server login forms, saved group/username/password, browser
   SSO, certificate pinning on first connect
3. ✅ **Fortinet**: password + 2FA through OpenConnect, realm, browser SSO
   (SAML) the FortiClient way, via a redirect to `127.0.0.1:8020`
4. ✅ **GlobalProtect**: portal and gateway login through OpenConnect; SAML in
   the default browser like gpauth (`default-browser=2` prelogin, then the
   `globalprotectcallback:` link, handled by Molehill only while it waits).
   Still to do: HIP reports (OpenConnect can't run the HIP script on Windows)
5. Molehill self-update, signed releases, a winget manifest, light theme, and
   following the Windows accent color

## License

MIT. JetBrains Mono is bundled under the SIL Open Font License
(`crates/molehill/ui/fonts/OFL.txt`).
