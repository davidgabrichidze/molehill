# Molehill on macOS — design

Status: draft for approval · 2026-10-16 · author: David Gabrichidze (with Claude Code)

## 1. Goal

Run Molehill on macOS 15 Sequoia and later, on Apple Silicon (`aarch64-apple-darwin`)
and Intel (`x86_64-apple-darwin`), with everything the Windows build does today:

- OpenVPN, Cisco AnyConnect, Fortinet and GlobalProtect
- inline sign-in prompts, browser SSO, saved passwords, client certificates
- several tunnels at once
- a menu-bar icon in place of the tray
- self-update

It ships as a downloadable release for the author, their colleagues and
anyone else, and goes upstream to `giorgobiani/molehill`.

**Hard constraint:** the Windows build behaves exactly as it does today. Windows
code may move behind `#[cfg(windows)]`, but its logic is not changed.

### Non-goals

- Code signing and notarization. There is no paid Apple Developer account, so
  builds are ad-hoc signed.
- macOS 14 and older.
- Keychain-resident or smart-card client certificates. Only `.p12`/`.pfx`
  files are supported (see §8). A PKCS#11 bridge can come later.
- Linux. The `cfg` split makes it easier later, but nothing here targets it.

## 2. Decisions

| Topic | Decision |
|---|---|
| Upstream delivery | **One pull request**, fork → `giorgobiani:main`. Upstream squash-merges anyway. Inside the PR, commits are ordered so each one builds and passes Windows CI (§13). |
| Minimum macOS | 15 Sequoia, because we bundle Homebrew bottles built on `macos-15` runners. |
| Privileged part | `molehill-service` runs as a root **launchd LaunchDaemon**, installed by a `.pkg`. Not SMAppService, which expects a signed app. |
| VPN engines | The `.pkg` bundles `openvpn` and `openconnect` (plus `libopenconnect` and its dylibs). There is no on-demand download as on Windows. |
| Passwords | macOS Keychain (generic password items). |
| Client certificates | `.p12`/`.pfx` file chosen in the profile editor; its passphrase is kept in the Keychain. |
| Gatekeeper | The `.pkg` is unsigned. First install: System Settings → Privacy & Security → **Open Anyway**. Documented in the README and on the site. |
| Architectures | Two `.pkg` files, one arm64 and one x86_64, not a universal build. Self-update picks the one matching the running architecture. |
| Test servers | The author can reach real OpenVPN, AnyConnect, Fortinet and GlobalProtect servers. |

## 3. How the code is split by platform

Today 19 of the 34 `.rs` files call Win32/WinRT directly, and there is no `cfg`
gating anywhere. The approach:

1. **Cargo:** move `windows`, `windows-service` and `tauri-winrt-notification`
   to `[target.'cfg(windows)'.dependencies]`. Add the macOS crates under
   `[target.'cfg(target_os = "macos")'.dependencies]`:
   - `security-framework` (Keychain)
   - `objc2` with `objc2-app-kit`, `objc2-foundation` and
     `objc2-user-notifications` (AppKit glue)
   - `nix` (signals, peer credentials)
   - `libc` (`getifaddrs`)
2. **Modules:** where a whole file is platform code (`job.rs`, `security.rs`,
   `netstats.rs`, `platform.rs`, `selfupdate.rs`, `gpcallback.rs`, `secrets.rs`),
   keep the Windows file as it is and declare it with
   `#[cfg(windows)] #[path = "…"] mod x;`. Its macOS sibling `x_macos.rs`
   exposes **the same function signatures**. Callers don't change.
3. **Mixed files** (`paths.rs`, `ipc.rs`, `update.rs`, `server.rs`, `client.rs`,
   `components.rs`, `tunnel/*.rs`, `openconnect_auth.rs`, `fortinet_sso.rs`,
   `gp_sso.rs`, `certs.rs`, `localauth.rs`, `settings.rs`, `tray.rs`,
   `main.rs`): only the platform-specific lines get small `cfg` blocks.
   These include constants, binary names, `CREATE_NO_WINDOW`, and the
   pipe/socket type. The portable logic (management-interface state machine,
   form handling, SSO flows) is shared as it is.
4. **Rule for reviewers:** every changed line in a Windows code path is either
   a `cfg` attribute or a move. Section 12 gives the check.

## 4. Service: root LaunchDaemon

| | Windows today | macOS |
|---|---|---|
| Host | SCM service, LocalSystem | `/Library/LaunchDaemons/io.github.giorgobiani.molehill.service.plist`, root, `KeepAlive=true`, `RunAtLoad=true` |
| Binary | `C:\Program Files\Molehill\molehill-service.exe` | `/Library/Application Support/Molehill/bin/molehill-service` |
| Entry | `windows-service` dispatcher; `install`/`uninstall` subcommands | Plain `main`, which runs the server when started by launchd. `install`/`uninstall` belong to the `.pkg` scripts on macOS. `oc-auth` is the same; `oc-stop` isn't needed. |
| IPC | Named pipe `\\.\pipe\molehill`, SDDL allows logged-on users | Unix socket `/var/run/molehill.sock`, owner `root:staff`, mode `0660` (every local login user is in `staff`) |
| Wire protocol | JSON lines (`ipc.rs`) | **Unchanged.** `server.rs` / `client.rs` are already generic over any `AsyncRead + AsyncWrite`. |
| Child cleanup | Job object with KILL_ON_JOB_CLOSE | Engines run in the daemon's process group, and launchd kills the whole group when the job stops (`AbandonProcessGroup` left false). On start, the daemon also kills any engine PIDs left in `run/*.pid` from a crash. |
| Restart on failure | SCM failure actions (5 s, 30 s) | launchd `KeepAlive` plus `ThrottleInterval=5` |
| Shutdown | waits up to 25 s for tunnels | Same logic on SIGTERM; plist `ExitTimeOut=30` |
| Machine data | `%ProgramData%\Molehill` (icacls lock) | `/Library/Application Support/Molehill/` owned by `root:wheel`, `0755`; `run/` is `0700`. Logs in `/Library/Logs/Molehill/service.log`, `0644`, same 5 MB rotation. |

`security_macos.rs` does what `security.rs` does: create the directories with
the modes above, refuse to start if any is a symlink (the equivalent of the
reparse-point check), and log the client's uid with `getpeereid` for each
connection.

## 5. Engines and where they live

- On the CI runner, `brew install openvpn openconnect`, then copy into
  `/Library/Application Support/Molehill/engines/`:
  - `openvpn`
  - `openconnect`
  - `libopenconnect.5.dylib`
  - all of their dylibs (GnuTLS, nettle, p11-kit, libxml2, lz4, lzo, …)
  - Homebrew's `vpnc-script`
- Rewrite install names with `dylibbundler` (or `install_name_tool` driven by
  `otool -L`) so nothing points into `/opt/homebrew` or `/usr/local`. Then
  **ad-hoc re-sign every Mach-O** (`codesign --force -s -`), because editing
  install names breaks the linker's ad-hoc signature and arm64 refuses to run
  unsigned code.
- A CI check runs `otool -L` on every bundled file and fails on any path
  outside `@loader_path`/`@rpath` or `/usr/lib` and `/System`.
- `paths.rs` and `components.rs` on macOS point at this directory and nowhere else.
- The Components page shows the bundled versions as installed. On macOS the
  Install/Update buttons are hidden, and engines are updated by Molehill
  releases. This is one Slint property, `components-installable`, set to true
  on Windows, so the Windows UI is unchanged.
- **Licensing:**
  - OpenVPN is GPLv2 and OpenConnect is LGPLv2.1, and we redistribute their
    binaries. The `.pkg` installs their license texts under `engines/licenses/`.
  - The release attaches the exact source tarballs Homebrew built from
    (`brew fetch --build-from-source` gives the URLs), which satisfies the
    source-offer obligation.
  - Molehill itself stays MIT.

## 6. OpenVPN on macOS

- Device: `utun`, created by OpenVPN itself. No TAP or DCO; `ensure_adapter` is
  `cfg(windows)`. `tunnel/openvpn.rs` adds `--dev-type tun` and no `--dev-node`.
- **DNS:**
  - On Windows the driver applies pushed DNS. On macOS OpenVPN applies routes
    but not DNS, so DNS needs a script.
  - We keep the security property that matters: **no script ever comes from a
    profile.** `ovpn.rs` still strips `up`, `down`, `route-up` and friends.
  - On macOS only, the service adds
    `--script-security 2 --up <engines>/molehill-dns --down <engines>/molehill-dns --up-restart`.
  - `molehill-dns` is a root-owned script that ships in the `.pkg`.
  - It reads `foreign_option_*` (`dhcp-option DNS` / `DOMAIN` / `DOMAIN-SEARCH`)
    and the newer `--dns` variables, writes a per-tunnel
    `State:/Network/Service/Molehill-<id>/DNS` key with `scutil`, and removes
    it on down.
  - Split DNS (pushed domains only) uses `SupplementalMatchDomains`. Full DNS
    sets the tunnel resolver as primary. Several tunnels at once each get
    their own key.
- The rest is unchanged: management interface, `>PASSWORD`, CRV1/SCRV1,
  WEB_AUTH/OPEN_URL, and `>BYTECOUNT` for traffic stats.

## 7. OpenConnect on macOS (AnyConnect, Fortinet, GlobalProtect)

- **Interface name:** macOS chooses `utunN` itself, so `--interface=Molehill-xxxx`
  is dropped on macOS. `--script` points at `molehill-vpnc`, a small
  root-owned wrapper. It writes `$TUNDEV` to `run/<id>.dev` and then calls the
  bundled `vpnc-script`, which already handles macOS routes and DNS via
  `scutil`. `netstats` reads that name.
- **Traffic counters:** `netstats_macos.rs` implements
  `interface_bytes(name)` using `getifaddrs` and `if_data` (`ifi_ibytes`/`ifi_obytes`).
- **Stop:** `kill(pid, SIGINT)` replaces the `oc-stop` Ctrl+C helper. The hard
  kill after 10 s stays.
- **libopenconnect:**
  - `openconnect_auth.rs` loads `engines/libopenconnect.5.dylib`.
  - **Bug to fix for Apple Silicon:** `ProgressCb` (`openconnect_auth.rs:84`)
    declares a variadic C callback with fixed arguments. That works on x64
    Windows, but on arm64 Apple's ABI passes variadic arguments on the stack,
    so the log text would be garbage or the process would crash.
  - Fix: a ~20-line C shim (built with the `cc` crate, **macOS only**) exposes
    the real `void (*)(void*, int, const char*, ...)`, formats the message with
    `vsnprintf`, and forwards the finished string to a non-variadic Rust
    callback.
  - Windows keeps its current code path untouched.
- **GlobalProtect `clientos`:**
  - `gp_sso.rs:96` hard-codes `Windows`. On macOS it sends `Mac` and runs
    openconnect with `--os=mac-intel` (openconnect's name for macOS, used on
    both architectures), so the prelogin and the tunnel report the same OS.
  - A portal that only admits Windows clients would refuse this. That is on
    the test matrix (§11).
- **Fortinet SSO:** `/usr/bin/curl` instead of `System32\curl.exe`, and
  `-o /dev/null` instead of `NUL`. The loopback port 8020 flow is unchanged.

## 8. Client certificates

- The profile editor gets **"Certificate file…"** on macOS, using the `rfd`
  file picker filtered to `.p12`/`.pfx`. The profile stores the path. The
  passphrase is stored as a Keychain item `Molehill/<id>/cert`.
- As on Windows, the **app** runs `molehill-service oc-auth` as the logged-in
  user (`localauth.rs`), because the file lives in the user's home. The app
  calls `openconnect_set_client_cert(path, NULL)`. GnuTLS asks for the PKCS#12
  passphrase through the form callback, and Molehill fills it from the
  Keychain or prompts for it. The service only starts the tunnel with the
  resulting cookie.
- `certs.rs` (`list-system-keys.exe`) is `cfg(windows)`.
- **UI change:** the Windows editor keeps its certificate-store list. The file
  picker appears only on macOS, through a Slint property.

## 9. App: menu bar, windows, OS integration

| Feature | Windows | macOS |
|---|---|---|
| Tray / menu bar | `tray-icon` plus a custom Slint popup placed by `platform::place_menu` | `tray-icon` (NSStatusItem) with a **native menu** (`muda`, via `tray-icon`) that has the same items and state. Template (monochrome) icons per state, so it fits light and dark menu bars. |
| Left click | show window | show window (`menu_on_left_click(false)`) |
| Close window | hides | hides. The activation policy switches to Accessory (no Dock icon) while hidden, and back to Regular while shown. |
| Single instance | named mutex + window message | LaunchServices normally gives one instance. For launches from Terminal, an `flock` on `~/Library/Application Support/Molehill/.lock`; a second instance just activates the first through `open -b`. |
| Dark mode | `AppsUseLightTheme` registry value | `NSApp.effectiveAppearance` |
| Title-bar colour, DPI subclassing, window icon tint | DWM / Win32 | no-op |
| Taskbar badge | overlay icon | Dock tile badge (connected-tunnel count) via `NSDockTile` |
| Notification ("update available") | WinRT toast | `UNUserNotificationCenter`. Ad-hoc signed bundles may be refused authorization [Guessing], so we fall back to `osascript display notification`. |
| `globalprotectcallback:` | HKCU handler registered only during sign-in, restored afterwards | `CFBundleURLTypes` in `Info.plist`. The app installs a `kAEGetURL` Apple Event handler and forwards the callback **in-process**, with no second instance. To match Windows, Molehill makes itself the default handler only while a sign-in waits (`LSSetDefaultHandlerForURLScheme`) and restores the previous handler afterwards. That way an installed GlobalProtect.app is not hijacked. |
| Autostart | installer checkbox, `Run` key with `--hidden` | The `.pkg` installs `/Library/LaunchAgents/io.github.giorgobiani.molehill.plist` (`--hidden`). Users can switch it off in System Settings → General → Login Items. |
| Bundle | n/a | `/Applications/Molehill.app`, bundle id `io.github.giorgobiani.Molehill` (the owner may prefer a different id; renaming is trivial before the first release) |

User data moves from `%APPDATA%\Molehill` to
`~/Library/Application Support/Molehill/`, with the same files. The
`MOLEHILL_USER_DIR` and `MOLEHILL_SERVICE_DIR` overrides keep working.

`main.rs`:
- `CoInitializeEx` and `windows_subsystem` become `cfg(windows)`.
- On macOS the tray must be created on the main thread after the winit event
  loop starts. This is a known `tray-icon` requirement, and it goes in Slint's
  event-loop init hook.

## 10. Keychain and secrets

- `secrets_macos.rs` implements the same three functions (`save_password`,
  `load_password`, `delete_password`) using `security-framework` generic
  passwords: service `Molehill`, account `<profile id>`.
- **Known cost of being unsigned:**
  - The Keychain ties each item's access to the code signature of the app
    that created it. An ad-hoc signature changes with every build.
  - So after each self-update, macOS asks once per saved password
    "Molehill wants to use your confidential information…". **Always Allow**
    fixes it until the next update.
  - This is documented. It goes away if the project ever gets a Developer ID.

## 11. Shipping: `.pkg`, release, self-update

**`.pkg`**, built with `pkgbuild` and `productbuild`, one per architecture
(`Molehill-<v>-arm64.pkg`, `Molehill-<v>-x86_64.pkg`):

- Payload:
  - `Molehill.app` (ad-hoc signed)
  - `bin/molehill-service`
  - `engines/…`
  - `molehill-dns`
  - `molehill-vpnc`
  - the LaunchDaemon and LaunchAgent plists
  - `uninstall.sh`
- `preinstall`: `launchctl bootout system/io.github.giorgobiani.molehill.service`
  if it is loaded.
- `postinstall`:
  - fix owners and modes
  - `launchctl bootstrap system …`
  - if this is an update, relaunch the app as the console user
    (`launchctl asuser <uid> open -a Molehill --args --hidden`)
- The `.pkg` refuses to install on macOS < 15 (`<allowed-os-versions>`) or on
  the wrong architecture (`hostArchitectures`).
- Uninstall: `sudo "/Library/Application Support/Molehill/uninstall.sh"`,
  documented in the README.

**Release workflow** (`release.yml`): the existing `windows-latest` job is
untouched. Two macOS jobs are added:
- `macos-15` builds arm64.
- `macos-15-intel` builds x86_64. That label exists today
  [Likely: it's used by CPython and others, but I couldn't confirm a
  retirement date]. If it goes away, the fallback is an x86_64 Homebrew under
  Rosetta on the arm runner, used only to fetch the engine bottles. Rust
  cross-compiles to x86_64 either way.

Each job:
1. runs the tag/version check, tests and a release build
2. bundles the engines
3. builds the `.pkg`
4. writes its `.sha256`
5. uploads the engine source tarballs

The final `gh release create` step collects all assets. Fixed-name copies
`Molehill-arm64.pkg` and `Molehill-x86_64.pkg` give permanent download links,
as `molehill-setup.exe` does today.

**Self-update:**
- `update.rs` picks its asset by target:
  `molehill-setup-*.exe` on Windows (unchanged), `Molehill-*-{arm64|x86_64}.pkg`
  on macOS.
- Download, SHA-256 check and timing (5 s, then every 12 h) are unchanged.
  The download goes to `$TMPDIR/Molehill-update`.
- Install:
  `osascript -e 'do shell script "/usr/sbin/installer -pkg <path> -target /" with administrator privileges'`.
  The user enters their password once, like the single UAC prompt on Windows,
  and the app quits.
- `postinstall` restarts the daemon (tunnels drop for a moment, as on Windows)
  and relaunches the app.
- `installer(8)` does not apply Gatekeeper to the `.pkg`, so updates don't
  need "Open Anyway"; only the first install does.

## 12. CI

`ci.yml` keeps the `windows-latest` job as it is and adds a `macos-15` job:
1. `cargo clippy --workspace --all-targets --locked -- -D warnings`
2. `cargo test --workspace --locked` (native arm64)
3. `cargo build --release --workspace --locked --target aarch64-apple-darwin`
4. the same build with `--target x86_64-apple-darwin` (cross-compile; nothing
   is run)

`job.rs` and `gpcallback.rs` tests stay `cfg(windows)`. Their macOS siblings
get their own unit tests where that makes sense: `netstats` against `lo0`, the
socket permission check, and the format path of the variadic shim.

**"Windows unchanged" check** (in the PR description and done before pushing):
- Windows CI is green.
- `git diff main -- '*.rs'` shows only `cfg` attributes, `#[path]` module
  moves and new `*_macos.rs` files in Windows code paths.
- `installer/molehill.iss` and the Windows steps of `release.yml` are
  untouched.

## 13. Commit order inside the single PR

Each commit builds, and passes clippy and tests, on Windows:

1. `cfg`-gate the Windows code and target-gate the dependencies. macOS
   compiles with stubs that return "unsupported". Add the macOS CI job.
2. Daemon on macOS: Unix socket, launchd entry, directories and permissions,
   process cleanup.
3. Engines: paths, OpenVPN on `utun` plus `molehill-dns`, OpenConnect plus
   `molehill-vpnc`, the variadic shim, netstats, and the SSO changes (curl,
   GP `clientos`).
4. Keychain secrets and `.p12` client certificates.
5. App: menu bar, Dock badge, appearance, single instance, notifications,
   GP URL handler.
6. Packaging: `.pkg` scripts, engine bundling, `release.yml` macOS jobs,
   macOS self-update, README/site docs.

This design doc lives on `wip/macos-port` and is **not** part of the upstream
PR. `docs/` is the project site, so the doc is dropped from the PR branch.

## 14. Testing

- **Automated:** the CI above.
- **Manual, on the author's Mac, before opening the PR:**

| Protocol | Cases |
|---|---|
| OpenVPN | password, static/dynamic challenge, WEB_AUTH SSO, full-DNS and split-DNS push |
| AnyConnect | password form, SSO (external browser), `.p12` certificate |
| Fortinet | password, SAML SSO (port 8020) |
| GlobalProtect | password, SAML SSO via `globalprotectcallback:`, with and without GlobalProtect.app installed |

  Plus:
  - two tunnels at once (DNS keys don't clobber each other)
  - kill -9 the daemon: launchd restarts it and the engines are cleaned up
  - sleep/wake
  - install → update from vN to vN+1 → uninstall
  - Keychain prompt after the update
- **Intel:** needs an Intel Mac, or at least the x86_64 `.pkg` installing and
  connecting on one. Without one, Intel is "builds in CI, untested", and the
  PR says so.

## 15. Risks

| Risk | Mitigation |
|---|---|
| Notification authorization refused for an ad-hoc bundle | `osascript` fallback; only one notification exists anyway. |
| Keychain re-prompt after each update | Documented; goes away if the app is ever signed. |
| `macos-15-intel` runner retired | Rosetta Homebrew fallback (§11). |
| Homebrew bottle layout or deps change | The `otool -L` CI gate fails the release instead of shipping a broken `.pkg`. |
| GP portal admits Windows clients only | Found in testing. If needed, a per-profile "report OS" override, as a follow-up. |
| `LSSetDefaultHandlerForURLScheme` is deprecated | Still works on macOS 15/26 [Likely]. If it stops, the fallback is always owning the scheme while Molehill is installed, plus a note. |
| Reviewer load of one large PR | Ordered commits (§13) and the "Windows unchanged" diff rule (§12). |
