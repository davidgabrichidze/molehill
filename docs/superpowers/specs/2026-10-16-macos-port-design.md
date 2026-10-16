# Molehill on macOS — design

Status: **approved**, rev 3 · 2026-10-16 · author: David Gabrichidze (with Claude Code)

## 1. Goal

Run Molehill on macOS 15 Sequoia and later, on Apple Silicon (`aarch64-apple-darwin`)
and Intel (`x86_64-apple-darwin`), with everything the Windows build does today:

- OpenVPN, Cisco AnyConnect, Fortinet and GlobalProtect
- inline sign-in prompts and browser SSO
- saved passwords and client certificates
- several tunnels at once
- a menu-bar icon in place of the tray
- self-update

It ships as a downloadable release for the author, their colleagues and
anyone else, and goes upstream to `giorgobiani/molehill`.

**Hard constraint:** the Windows build *behaves* exactly as it does today. Most
Windows code only moves behind `#[cfg(windows)]`. Where shared code has to
change, §3 lists each edit and a test covers it.

### Non-goals

- Code signing and notarization. There is no paid Apple Developer account, so
  builds are ad-hoc signed.
- macOS 14 and older.
- Keychain-resident or smart-card client certificates. Only `.p12`/`.pfx`
  files are supported (§8); a PKCS#11 bridge can come later.
- Linux.

## 2. Decisions

| Topic | Decision |
|---|---|
| Upstream delivery | **One pull request**, fork → `giorgobiani:main`. Upstream squash-merges anyway. Inside the PR, commits are ordered so each one builds and passes Windows CI (§13). |
| Minimum macOS | 15 Sequoia, because we bundle Homebrew bottles built on `macos-15` runners. |
| Privileged part | `molehill-service` runs as a root **launchd LaunchDaemon** installed by a `.pkg`. Not SMAppService, which expects a signed app. |
| VPN engines | The `.pkg` bundles `openvpn`, `openconnect`, `libopenconnect` and their dylibs, at pinned versions. There is no on-demand download as on Windows. |
| Passwords | macOS Keychain (generic password items). |
| Client certificates | `.p12`/`.pfx` file, copied into Molehill's own data folder; passphrase in the Keychain. |
| Gatekeeper | The `.pkg` is unsigned. First install: System Settings → Privacy & Security → **Open Anyway**. Documented in the README and on the site. |
| Architectures | Two `.pkg` files, arm64 and x86_64, not a universal build. Self-update picks the one matching the running architecture. |
| Test servers | The author can reach real OpenVPN, AnyConnect, Fortinet and GlobalProtect servers. |

## 3. How the code is split by platform

Today 19 of the 34 `.rs` files call Win32/WinRT directly, and there is no `cfg`
gating anywhere.

1. **Cargo:**
   - Move `windows`, `windows-service` and `tauri-winrt-notification` to
     `[target.'cfg(windows)'.dependencies]`.
   - The macOS dependencies go under
     `[target.'cfg(target_os = "macos")'.dependencies]`: `security-framework`,
     `objc2` with `objc2-app-kit`, `objc2-foundation` and
     `objc2-user-notifications`, `nix`, and `libc`.
   - `cc` goes under the macOS `build-dependencies`.
   - `.cargo/config.toml` already gates `+crt-static` to Windows.
2. **Platform modules:** where a whole file is platform code (`job.rs`,
   `security.rs`, `netstats.rs`, `platform.rs`, `selfupdate.rs`,
   `gpcallback.rs`, `secrets.rs`):
   - The Windows file stays byte-for-byte as it is, declared with
     `#[cfg(windows)] #[path = "…"] mod x;`.
   - Its macOS sibling `x_macos.rs` has **the same function signatures**, so
     callers don't change.
3. **Mixed files:** small `cfg` blocks cover the platform lines: constants,
   binary names, `CREATE_NO_WINDOW`, and the pipe/socket type. Expect a
   dead-code clean-up pass so clippy `-D warnings` passes on macOS too.
4. **Shared-code edits.** These are not just `cfg` lines. Each is listed in the
   PR description and covered by a test:

   | Where | Change | Windows effect |
   |---|---|---|
   | `server.rs:265` `handle_client`, `client.rs:8,37` | generic over `AsyncRead + AsyncWrite + Unpin` instead of the concrete named-pipe types | none; the type is inferred |
   | `tunnel/openvpn.rs:208` | script-security level and the up/down arguments come from a per-platform function | still `1`, no up/down |
   | `tunnel/openconnect.rs:262,295` | the interface name for netstats comes from a per-platform lookup (§7) | still `Molehill-<id8>` |
   | `localauth.rs:34` | service binary name from a constant | still `molehill-service.exe` |
   | `openconnect_auth.rs` `Api::load` | library path and loader per platform | same DLL and flags |
   | `openconnect_auth.rs` `on_form` | new branch for `auth_id == "openconnect_pkcs12"` ahead of `plan_form` (§8) | GnuTLS on Windows asks the same way when a store key needs a PIN. The branch only prompts and never uses the login password, which is safer. |
   | `ocauth.rs` `AuthRequest` | `#[serde(default)] cert_password: Option<String>` | absent, so `None` |
   | `ovpn.rs` | a second list, `BLOCKED_MACOS = ["dns-updown", "client-crresponse"]`, checked only under `cfg(target_os = "macos")` | none. Extending the list to Windows is proposed in a **separate upstream issue** (`docs/superpowers/issues/ovpn-blocklist-dns-updown.md`), not in this PR. If upstream accepts it first, the macOS-only list is folded into `BLOCKED`. |

5. **Rule for reviewers:** a Windows code path changes only by a `cfg`
   attribute, a `#[path]` move, or an edit listed in the table.

## 4. Service: root LaunchDaemon

| | Windows today | macOS |
|---|---|---|
| Host | SCM service, LocalSystem | `/Library/LaunchDaemons/io.github.giorgobiani.molehill.service.plist`, root, `KeepAlive=true`, `RunAtLoad=true`, `ThrottleInterval=5`, `ExitTimeOut=30` |
| Binaries | `C:\Program Files\Molehill\` | `/opt/molehill/bin/molehill-service`, `/opt/molehill/engines/…`. The path has **no spaces**: OpenVPN's `--up` and openconnect's `--script` are split like shell commands. Arguments are quoted anyway, and a test covers it. |
| Entry | `windows-service` dispatcher; `install`/`uninstall`/`oc-auth`/`oc-stop` | Plain `main`, which runs the server when started by launchd. On macOS, `install`/`uninstall` belong to the `.pkg` scripts. Kept: `oc-auth`. Added: `dns-up`/`dns-down` (§6). Not needed: `oc-stop`. |
| IPC | Named pipe `\\.\pipe\molehill`, SDDL allows interactive users | Unix socket `/var/run/molehill.sock`, `root:staff` `0660`. Any stale socket is removed before `bind`. The client's uid is logged via `getpeereid`. |
| Wire protocol | JSON lines (`ipc.rs`) | **Unchanged** |
| Child cleanup | Job object with KILL_ON_JOB_CLOSE | launchd kills the daemon's process group when the job ends (`AbandonProcessGroup` false). No PID files. |
| Crash leftovers | driver cleans up adapters | At start, the daemon removes every `State:/Network/Service/Molehill-*` key and every host route recorded in `state/routes`, then clears `state/` (§6, §7). |
| Shutdown | waits up to 25 s | same logic on SIGTERM |
| Machine data | `%ProgramData%\Molehill` (icacls lock) | `/Library/Application Support/Molehill/`: `root:wheel 0755`; `run/` and `state/` are `0700`. Logs in `/Library/Logs/Molehill/service.log` (`0644`, 5 MB rotation). |

`security_macos.rs` creates these directories with the modes above, and the
daemon refuses to start if any of them is a symlink, matching the reparse-point
check on Windows.

## 5. Engines

- **Bundling, done by a separate `engines.yml` workflow** for each
  architecture:
  - `brew install openvpn openconnect` at pinned versions.
  - Copy `openvpn`, `openconnect`, `libopenconnect.5.dylib`, all their dylibs
    and Homebrew's `vpnc-script` into `engines/`.
  - Rewrite install names to `@loader_path` and **ad-hoc re-sign every Mach-O**
    (`codesign --force -s -`). Editing install names breaks the linker's
    ad-hoc signature, and arm64 won't run unsigned code.
  - An `otool -L` gate fails the build on any path outside
    `@loader_path`/`@rpath`, `/usr/lib` and `/System`.
  - Upload the result as a release asset, `engines-<openvpn>-<openconnect>-<arch>.tar.gz`.
  - The Molehill release workflow downloads that pinned bundle rather than
    rebuilding it. This guards against Homebrew dropping Intel bottles and
    against the `macos-15-intel` runner being retired.
- **CA trust:**
  - Homebrew's GnuTLS has `$(brew --prefix)/etc/gnutls/cert.pem` compiled in,
    which doesn't exist on a Mac without Homebrew.
  - `postinstall` writes `/opt/molehill/engines/cacert.pem` from
    `/System/Library/Keychains/SystemRootCertificates.keychain` using
    `security find-certificate -a -p`.
  - `oc-auth` calls `openconnect_set_system_trust(vpn, 0)` and
    `openconnect_set_cafile(vpn, <cacert.pem>)`. The tunnel step is already
    pinned with `--servercert`.
  - Each update rewrites the file, so it follows OS root updates.
- **Paths and Components page:**
  - `paths.rs` and `components.rs` on macOS point only at `/opt/molehill/engines`.
  - The Components page shows the bundled versions. The Install/Update buttons
    are hidden through a Slint property `components-installable` (true on
    Windows), and engines update with Molehill releases.
  - `openvpn --version` is checked at start. Anything other than the pinned
    major.minor is logged as an error, because the §6 blocklist review is per
    version.
- **Licensing:**
  - OpenVPN is GPLv2 and OpenConnect is LGPLv2.1.
  - `/opt/molehill/engines/licenses/` holds their license texts.
  - The release attaches the exact upstream source tarballs, Homebrew's
    formula patches and the `vpnc-script` source.
  - Molehill stays MIT.

## 6. OpenVPN on macOS

- **Device:** `utun`, created by OpenVPN.
  - `ensure_adapter` is `cfg(windows)`, and `--dev-type tun` is passed.
  - A profile with `dev tap` is rejected on macOS with a clear error, because
    macOS has no TAP.
  - `persist-tun` is stripped on macOS, because it would stop up/down from
    running again on a reconnect and leave stale DNS behind.
- **DNS:** OpenVPN on macOS applies routes but not DNS. The security rule still
  holds: **no profile ever supplies or influences a script.**
  - **Blocklist:** on macOS, `ovpn.rs` also refuses `dns-updown` (OpenVPN 2.7)
    and `client-crresponse` (§3; Windows is the subject of a separate issue).
    The service passes `--dns-updown disable` when the bundled version
    understands it.
  - **The hook is Molehill's own binary, not a shell script.** The arguments
    are
    `--script-security 2 --up "'/opt/molehill/bin/molehill-service' dns-up <id>" --down "… dns-down <id>" --up-restart`.
  - **What `dns-up` reads:**
    - It ignores the environment except `dev`, `foreign_option_*` and the 2.6+
      `dns_*` variables, and it never reads `PATH`. Profiles can `setenv`, so
      nothing else in the environment is trusted.
    - Every address must parse as an IP, every domain must be plain hostname
      characters, and there is a fixed upper limit on how many are accepted.
      Anything else is dropped and logged.
  - **What `dns-up` writes:**
    - It calls `/usr/sbin/scutil` by absolute path, with commands built from
      validated values only.
    - Split DNS (pushed domains and no `redirect-gateway`): a
      `State:/Network/Service/Molehill-<id>/DNS` key with
      `SupplementalMatchDomains`.
    - Full DNS (`redirect-gateway`, or no pushed domains): the DNS key plus
      `State:/Network/Service/Molehill-<id>/IPv4` with `InterfaceName=<dev>`,
      `Router` and `OverridePrimary=1`. This is the same mechanism vpnc-script
      uses.
    - With two full-DNS tunnels, the newest one is primary. Its `dns-down`
      removes only its own keys, so the other tunnel's settings take over
      again. The overlap is logged.
  - **Cleanup:** `dns-down` removes the tunnel's keys. After a hard kill or a
    crash, the daemon's start-up sweep (§4) removes them.
- **Unchanged:** the management interface, `>PASSWORD`, CRV1/SCRV1,
  WEB_AUTH/OPEN_URL, and `>BYTECOUNT`.

## 7. OpenConnect on macOS (AnyConnect, Fortinet, GlobalProtect)

- **Interface:** macOS picks the `utunN` name itself.
  - On macOS `--interface` is left out, and `--script` is the bundled
    `vpnc-script`, which handles routes and DNS via `scutil` itself.
  - The interface name for counters comes from the address in "Connected as
    &lt;ip&gt;" (`openconnect.rs:330`), matched against `getifaddrs`, so no
    wrapper script is needed.
  - The VPN gateway's host route is written to `state/routes` once the tunnel
    is up, so the start-up sweep can remove it after a crash. vpnc-script's
    `State:` keys go into the same sweep.
- **Traffic counters:**
  - `netstats_macos.rs` reads `sysctl` `NET_RT_IFLIST2` (64-bit `if_data64`).
  - It does not use `getifaddrs`' `if_data`, which is 32-bit and wraps at 4 GiB.
- **Stop:** `kill(pid, SIGINT)` replaces the `oc-stop` Ctrl+C helper. The hard
  kill after 10 s stays.
- **libopenconnect:**
  - Loaded from `engines/libopenconnect.5.dylib`.
  - **Bug to fix for Apple Silicon:** `ProgressCb` (`openconnect_auth.rs:84`)
    declares a variadic C callback with fixed arguments. On arm64, Apple
    passes variadic arguments on the stack, so the log text would be garbage
    or the process would crash.
  - Fix: a ~20-line C shim, built with `cc` on macOS (both architectures, one
    code path). It exposes the real `void (*)(void*, int, const char*, ...)`,
    formats with `vsnprintf`, and forwards the string to a non-variadic Rust
    callback.
  - Windows keeps its current code.
- **GlobalProtect OS:** on macOS, `gp_sso.rs:96-97` sends `clientos=Mac` and a
  macOS `os-version`, matching what libopenconnect reports by default on
  macOS (`mac-intel`). A portal that only admits Windows clients would refuse
  this; that is on the test matrix.
- **Fortinet SSO:** `/usr/bin/curl` instead of `System32\curl.exe`, and
  `-o /dev/null` instead of `NUL`. LibreSSL curl supports
  `--pinnedpubkey sha256//`.

## 8. Client certificates

- **Picking the file:** on macOS the profile editor gets **"Certificate
  file…"** (the `rfd` picker, filtered to `.p12`/`.pfx`).
  - The chosen file is **copied** to
    `~/Library/Application Support/Molehill/certs/<id>.p12` (`0600`), and the
    profile stores that path.
  - Copying avoids the macOS privacy prompt for files in Downloads and
    Documents (TCC), which an ad-hoc build would get again after every update.
- **The passphrase:** stored in the Keychain as service `Molehill`, account
  `<id>/cert`.
- **Signing in:** as on Windows, the **app** runs `molehill-service oc-auth` as
  the user (`localauth.rs`).
  - It calls `openconnect_set_client_cert(path, path)`, unchanged from
    `main.rs:661`.
  - GnuTLS asks for the PKCS#12 passphrase through the form callback, with
    `auth_id = "openconnect_pkcs12"`.
  - The new `on_form` branch (§3) fills it once from
    `AuthRequest.cert_password`. Otherwise it prompts "Certificate passphrase",
    with a "save" option.
  - It **never** uses the login password, and it doesn't affect the
    `password_used` logic.
  - The service then only starts the tunnel with the cookie.
- **Known limit, same as Windows:** the tunnel runs with the cookie only. An
  ASA that requires the certificate on the tunnel connection itself fails on
  both platforms. Out of scope.
- **Windows:** `certs.rs` (`list-system-keys.exe`) is `cfg(windows)`. The
  Windows editor keeps the certificate-store list; the file picker is
  macOS-only through a Slint property.

## 9. App: menu bar, windows, OS integration

| Feature | Windows | macOS |
|---|---|---|
| Tray / menu bar | `tray-icon` plus a Slint popup placed by `platform::place_menu` | `tray-icon` (NSStatusItem) with a **native menu** (`muda`) that has the same items and state. Template (monochrome) icons per state. Created in a `slint::invoke_from_event_loop` closure queued before `run_event_loop_until_quit` (`main.rs:194`), so it runs on the main thread after the loop starts. |
| Left click | show window | show window (`with_menu_on_left_click(false)`) |
| Close window | hides | hides. Activation policy Accessory (no Dock icon) while hidden, Regular while shown. Started with `--hidden`, the app goes straight to Accessory, so there is no Dock flash at login. |
| Re-open while hidden | second instance sends "show" (`platform.rs:237`) | A `kAEReopenApplication` Apple Event handler shows the window. It covers clicks in Finder, Spotlight and Launchpad. |
| Quit | tray menu Quit | same menu item. Cmd+Q does the same. Tunnels keep running in the daemon, as on Windows. |
| Single instance | named mutex | LaunchServices normally gives one instance. For launches from Terminal, an `flock` on `~/Library/Application Support/Molehill/.lock`; the second instance runs `open -b <bundle id>`, which reaches the reopen handler. |
| Dark mode | registry | `NSApp.effectiveAppearance` |
| Title-bar colour, DPI subclassing, window icon tint | Win32 | no-op |
| Taskbar badge | overlay icon | Dock badge (connected count) while the Dock icon is shown. The menu-bar icon always shows the state. |
| Notification ("update available") | WinRT toast | `UNUserNotificationCenter`; falls back to `osascript display notification` if authorization fails. |
| `globalprotectcallback:` | HKCU handler only during sign-in, previous one restored | `CFBundleURLTypes` in `Info.plist` plus a `kAEGetURL` Apple Event handler that forwards in-process. A static plist entry lets LaunchServices pick Molehill as the default at install time. To avoid hijacking the scheme: **at startup**, if GlobalProtect.app is installed and Molehill is the default, give the scheme back (mirrors `gpcallback::unregister`, `gpcallback.rs:83`). During a sign-in, claim it with `LSSetDefaultHandlerForURLScheme`; restore it afterwards. |
| Autostart | installer checkbox, `Run` key | `/Library/LaunchAgents/io.github.giorgobiani.molehill.plist`: `--hidden`, `KeepAlive=false`, `LimitLoadToSessionType=Aqua`, `AbandonProcessGroup=true`. Each user can switch it off in System Settings → General → Login Items. |
| Bundle | n/a | `/Applications/Molehill.app`, bundle id `io.github.giorgobiani.Molehill`. This is a placeholder until the owner decides. |

User data moves from `%APPDATA%\Molehill` to
`~/Library/Application Support/Molehill/`, with the same files plus `certs/`.
`MOLEHILL_USER_DIR` and `MOLEHILL_SERVICE_DIR` still work. `CoInitializeEx` and
`windows_subsystem` become `cfg(windows)`.

## 10. Keychain and secrets

- `secrets_macos.rs` implements the same three functions using
  `security-framework` generic passwords:
  - service `Molehill`
  - account `<profile id>` for the login password
  - account `<profile id>/cert` for the certificate passphrase
- **Cost of being unsigned:**
  - Keychain access is tied to the code signature, and an ad-hoc signature
    changes with every build.
  - After each update, macOS asks once per item ("Molehill wants to use your
    confidential information…"). **Always Allow** clears it until the next
    update.
  - Documented; it goes away if the app is ever signed.

## 11. Shipping: `.pkg`, release, self-update

**`.pkg`**, built with `pkgbuild` and `productbuild`, one per architecture
(`Molehill-<v>-arm64.pkg`, `Molehill-<v>-x86_64.pkg`):

- **Payload:**
  - `/Applications/Molehill.app` (ad-hoc signed)
  - `/opt/molehill/bin/molehill-service`
  - `/opt/molehill/engines/…`
  - the LaunchDaemon and LaunchAgent plists
  - `/opt/molehill/uninstall.sh`
- **Install rules:** `<allowed-os-versions>` is macOS 15 or later, and
  `hostArchitectures` matches the package.
- **`preinstall`:** `launchctl bootout system/io.github.giorgobiani.molehill.service`
  if it is loaded.
- **`postinstall`:**
  1. Fix owners and modes.
  2. Write `cacert.pem` (§5).
  3. `launchctl bootstrap system …`.
  4. For the console user, `launchctl bootstrap gui/$uid …` and
     `launchctl kickstart -k gui/$uid/io.github.giorgobiani.molehill`.
  - This starts the app **as that user**. `launchctl asuser … open` would run
    it as root.
- **`uninstall.sh`** (sudo):
  - `bootout` the daemon, and the agent for each logged-in `gui/<uid>`.
  - Remove `/opt/molehill`, the app, both plists,
    `/Library/Application Support/Molehill` and `/Library/Logs/Molehill`.
  - Run the start-up DNS/route sweep one last time.
  - `pkgutil --forget`.
  - Documented in the README.

**Release workflow:** the `windows-latest` job is untouched. A macOS job per
architecture (`macos-15` for arm64, `macos-15-intel` for x86_64; Rust also
cross-compiles from arm64 if needed):

1. tag/version check, tests, release build
2. fetch the pinned engine bundle (§5)
3. build the `.pkg`
4. write its `.sha256`

`gh release create` collects all assets: both `.pkg` files, the engine source
tarballs, and fixed-name copies `Molehill-arm64.pkg` and `Molehill-x86_64.pkg`
for permanent links.

**Self-update:**
- `update.rs` chooses its asset by target: `molehill-setup-*.exe` on Windows
  (unchanged), `Molehill-*-{arm64|x86_64}.pkg` on macOS.
- Download, SHA-256 check and timing are unchanged; downloads go to
  `$TMPDIR/Molehill-update`.
- Install:
  `osascript -e 'do shell script "/usr/sbin/installer -pkg <path> -target /" with administrator privileges'`.
  The user enters their password once, like the single UAC prompt on Windows.
- The child is spawned with `Command::process_group(0)`, so the app quitting
  can't kill it. The agent also has `AbandonProcessGroup=true`.
- The app quits, `postinstall` restarts the daemon (tunnels drop for a moment,
  as on Windows) and kickstarts the app.
- `installer(8)` doesn't run a Gatekeeper check, and `ureq` downloads aren't
  quarantined, so only the first install needs "Open Anyway".

## 12. CI

**`ci.yml`:** the `windows-latest` job is unchanged. A new `macos-15` job runs:

1. clippy `-D warnings` on all targets
2. `cargo test --workspace --locked` (native arm64)
3. release builds for `aarch64-apple-darwin` and `x86_64-apple-darwin`

**Tests:**
- Windows-only tests (`job.rs`, `gpcallback.rs`) stay `cfg(windows)`.
- New macOS unit tests:
  - netstats against `lo0`
  - socket mode and stale-socket handling
  - the variadic shim's formatting
  - `dns-up` input validation (hostile `foreign_option_*`, `PATH`, oversize
    lists)
  - quoting of arguments for paths with spaces
  - `dev tap` rejection and `persist-tun` stripping
  - the `ovpn.rs` macOS blocklist additions
- New tests on every platform:
  - the `openconnect_pkcs12` form branch
  - `AuthRequest` deserializing without `cert_password`

**"Windows behaviour unchanged" check**, done before pushing and stated in the
PR:
- Windows CI is green.
- Every Windows-path diff is a `cfg`, a `#[path]` move, or a row in the §3 table.
- `installer/molehill.iss` and the Windows steps of `release.yml` are untouched.

## 13. Commit order inside the single PR

Each commit builds and passes clippy and tests on Windows:

1. `cfg`-gate the Windows code and target-gate the dependencies. macOS compiles
   with stubs that return "unsupported". Make `handle_client` generic. Add the
   macOS CI job.
2. Daemon on macOS: Unix socket, directories and permissions, `security_macos`,
   start-up sweep.
3. Engines: paths, CA file, OpenVPN on `utun`, `dns-up`/`dns-down`, blocklist
   additions, OpenConnect interface lookup and stop, the variadic shim,
   netstats, and the SSO changes (curl, GP OS).
4. Keychain secrets and `.p12` client certificates (including the
   `openconnect_pkcs12` branch).
5. App: menu bar, activation policy, reopen and URL Apple Events, GP scheme
   handling, Dock badge, appearance, single instance, notifications.
6. Packaging: `engines.yml`, `.pkg` scripts, macOS jobs in `release.yml`, macOS
   self-update, README/site docs.

This design doc lives on `wip/macos-port` and is **not** part of the upstream
PR. `docs/` is the project site, so the doc is dropped from the PR branch.

## 14. Testing

- **Automated:** the CI above.
- **Manual, on the author's Apple Silicon Mac:**

  | Protocol | Cases |
  |---|---|
  | OpenVPN | password, static/dynamic challenge, WEB_AUTH SSO, split DNS, full DNS |
  | AnyConnect | password form, external-browser SSO, `.p12` certificate with and without a saved passphrase |
  | Fortinet | password, SAML SSO (port 8020) |
  | GlobalProtect | password, SAML SSO via `globalprotectcallback:`, with and without GlobalProtect.app installed, in both install orders |

  Plus:
  - two tunnels at once, including two full-DNS tunnels
  - `kill -9` the daemon: launchd restarts it, and the sweep removes stale DNS
    and routes
  - sleep/wake
  - install → update vN→vN+1 (Keychain prompt as expected) → uninstall
- **Clean machine:** install on a Mac **without Homebrew** (a colleague's Mac,
  or a macOS VM such as UTM on the author's Mac). This catches leaked Homebrew
  paths, CA trust, and the first-launch Gatekeeper steps.
- **Intel:** an Intel Mac is available. The x86_64 `.pkg` gets the install →
  connect (at least one OpenConnect protocol and OpenVPN) → update → uninstall
  pass on it.

## 15. Risks

| Risk | Mitigation |
|---|---|
| Profile sanitiser gaps become root code execution under `script-security 2` | macOS blocklist additions, pinned OpenVPN version with a per-version review, `--dns-updown disable`, the hook is our own binary that trusts no environment, and a version check at start. |
| Notification authorization refused for an ad-hoc bundle | `osascript` fallback; only one notification exists. |
| Keychain re-prompt after each update | Documented. |
| Homebrew drops Intel bottles, or `macos-15-intel` is retired | Engine bundles are built once per version and stored as release assets (§5). |
| Bundled GnuTLS finds no CA store | `cacert.pem` plus explicit `set_cafile` and the clean-machine test. |
| GP portal admits Windows clients only | Found in testing; a per-profile OS override as a follow-up. |
| `LSSetDefaultHandlerForURLScheme` is deprecated | Still works for custom schemes. If it stops, fall back to owning the scheme while installed, plus a note. |
| macOS 15 Local Network prompt for app-side `oc-auth` to LAN servers, repeated after updates | Documented; the daemon is exempt. |
| One large PR to review | Ordered commits (§13), the §3 edit table, and the §12 check. |
