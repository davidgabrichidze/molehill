# Molehill macOS port — implementation plan

Design: `docs/superpowers/specs/2026-10-16-macos-port-design.md` (approved, rev 3).
Section references (§n) point at that document.

## Ground rules

- **Branches:**
  - Work happens on `feat/macos` in the fork (`origin`), branched from
    `origin/main`.
  - `wip/macos-port` keeps the spec and plan only; they never go upstream.
- **Commits:** six commits, one per milestone M1–M6 below. Each must pass
  Windows CI on its own; fixups are squashed into their milestone before the
  upstream PR.
- **Where things get verified:**
  - **Fork CI.** A draft PR `feat/macos → davidgabrichidze:main` *inside the
    fork* runs `ci.yml` (Windows, plus macOS from M1 on) on every push, without
    touching upstream. The fork needs Actions enabled; that is one click in
    the fork's Actions tab.
  - **The author's Apple Silicon Mac** and **the Intel Mac**: anything that
    needs a tunnel, root, the Keychain or the UI.
  - **A clean Mac / macOS VM without Homebrew**: the packaging tests.
  - This Linux container **can't** build either target: `ring` needs MSVC
    tools and the macOS crates need Apple's SDK. So it's for editing and
    `cargo fmt` only.
- **TDD where logic is portable:** write the failing test, implement, make it
  pass. Tests that are inherently platform-bound (`cfg(target_os = "macos")`)
  run on the macOS CI job.
- **The Windows rule (§3.5):**
  - Every Windows-path diff is a `cfg`, a `#[path]` move, or a row in the §3
    table.
  - Before each push, check
    `git diff origin/main -- crates/ ':!*_macos.rs' ':!*/macos/*'` by eye
    against that list.
- **Before each push:** `cargo fmt --check`, a re-read of the diff, then CI. No
  speculative pushes.

---

## M1 — cfg-gate Windows, macOS compiles, macOS CI job

Goal: `cargo clippy/test/build` passes on `windows-latest` (unchanged) and on
`macos-15`. On macOS, every platform function is a stub that returns
`Err("not supported on macOS yet")` or `None`.

1. **Cargo manifests**
   - Root `Cargo.toml`: keep `windows` in `[workspace.dependencies]` (the
     declaration is portable).
   - In each crate, move `windows.workspace = true`, `windows-service`,
     `tauri-winrt-notification` and `winresource` under
     `[target.'cfg(windows)'.dependencies]` / `build-dependencies`.
   - `libloading` stays shared; its Windows-only flags get gated in code.
   - Run `cargo update -w --locked` (or `cargo metadata`) so `Cargo.lock`
     doesn't change versions, only structure.
   - Verify: on Windows CI, `Cargo.lock` has no version changes
     (`git diff Cargo.lock` shows no `version =` lines).
2. **molehill-core**
   - `secrets.rs` → `#[cfg(windows)]`. Add `secrets_macos.rs` with stubs for
     the same three signatures. Wire it up in `lib.rs` with `#[path]`.
   - `paths.rs`: `cfg` the `APPDATA`/`ProgramData`/`Program Files` lookups. The
     macOS arm returns the §9/§4 paths:
     - user dir `~/Library/Application Support/Molehill`
     - service dir `/Library/Application Support/Molehill`
     - engines `/opt/molehill/engines`
     - `MOLEHILL_*` overrides honoured first
   - `ipc.rs`: `PIPE_NAME` under `cfg(windows)`; add
     `SOCKET_PATH = "/var/run/molehill.sock"` under `cfg(target_os = "macos")`.
   - **Test (portable):** `paths` overrides win when the env vars are set.
3. **molehill-service**
   - `main.rs`: the SCM dispatcher, `install`/`uninstall` and `oc-stop` go
     under `cfg(windows)`. On macOS, `main` runs the server directly. Subcommand
     dispatch (`oc-auth`) stays shared.
   - `security.rs`, `job.rs`, `netstats.rs` → `cfg(windows)` via `#[path]`. Add
     `*_macos.rs` stubs with the same signatures (`job::adopt` is a no-op on
     macOS by design, §4).
   - **`server.rs` (§3 table, row 1):** make `handle_client` generic over
     `S: AsyncRead + AsyncWrite + Unpin + Send + 'static`. The named-pipe
     accept loop goes under `cfg(windows)`; add a macOS accept-loop stub.
     Existing test `server.rs` (1) must still pass on Windows.
   - `components.rs`: registry lookup, MSI/NSIS install and the Authenticode
     check go under `cfg(windows)`. The macOS arm reads versions from
     `/opt/molehill/engines` (stub for now). Version parsing stays shared.
   - `tunnel/openvpn.rs`, `tunnel/openconnect.rs`, `fortinet_sso.rs`,
     `openconnect_auth.rs`: `cfg` around `CREATE_NO_WINDOW` (a small
     `fn no_window(cmd)` helper that is a no-op on macOS), the binary names and
     `Api::load`'s loader flags.
   - Sweep for dead code under clippy `-D warnings` on macOS. Prefer
     `#[cfg(windows)]` on the item over `#[allow(dead_code)]`.
4. **molehill (app)**
   - `main.rs`: `#![cfg_attr(windows, windows_subsystem = "windows")]` (or
     keep the existing attribute's condition), and `CoInitializeEx` under
     `cfg(windows)`.
   - `platform.rs`, `selfupdate.rs`, `gpcallback.rs` → `cfg(windows)` via
     `#[path]`. Add `*_macos.rs` stubs with identical public functions; the
     window-tinting and badge functions are no-ops.
   - `client.rs`: named-pipe connect under `cfg(windows)`; macOS
     `UnixStream::connect(SOCKET_PATH)`. This is small enough to finish now.
   - `tray.rs`: `GetSystemMetrics` under `cfg`; macOS uses a fixed 18 pt
     template size.
   - `settings.rs`: dark-mode registry read under `cfg`; the macOS stub
     returns `false`.
   - `certs.rs`, `localauth.rs`: `cfg` the `.exe` names and
     `CREATE_NO_WINDOW`. `certs::list` returns an empty list on macOS.
   - `build.rs` is already gated (`build.rs:19`). Check that the `.ico`
     generation is harmless on macOS.
5. **CI:** add the `macos-15` job to `.github/workflows/ci.yml` as a new job;
   the Windows job's text stays byte-identical. Steps:
   1. `rustup target add x86_64-apple-darwin`
   2. clippy `-D warnings --all-targets`
   3. `cargo test --workspace --locked`
   4. release builds for both Apple targets
6. **Verify:** fork CI green on both jobs. The Windows-diff check from the
   ground rules holds. Commit: `macOS: gate Windows-only code, add macOS CI`.

## M2 — root daemon on macOS

1. **Tests first:**
   - `security_macos` creates the dirs with the right modes
     (`0755`/`0700`/`0644` logs) in a temp root, and refuses symlinked dirs.
   - The socket is created `0660`, a stale socket file is removed before
     bind, and a non-socket file at the path is an error, not a delete.
2. **`server.rs` macOS accept loop:**
   - `tokio::net::UnixListener` on `SOCKET_PATH`.
   - `chown root:staff` and `chmod 0660` after bind.
   - Log each connection's uid via `nix::unistd::getpeereid` (or
     `UnixStream::peer_cred`).
3. **Signals:** SIGTERM and SIGINT start the existing graceful shutdown path
   (25 s wait).
4. **Start-up sweep (§4/§6/§7)**, test-first with a fake `scutil` runner
   behind a small trait:
   - list `State:/Network/Service/Molehill-*` (and any keys recorded in
     `state/`), remove them
   - remove the host routes listed in `state/routes` via `/sbin/route delete`
   - clear `state/`
5. **Logging:** `/Library/Logs/Molehill/service.log` with the existing
   5 MB rotation.
6. **Dev loop doc** (README, *Building* section, macOS subsection): build,
   then
   `sudo cp target/debug/molehill-service /opt/molehill/bin/ && sudo launchctl kickstart -k system/io.github.giorgobiani.molehill.service`.
   The plist goes in `packaging/macos/` (created here, used by M6).
7. **Verify:**
   - On the Mac, load the plist by hand. The app's existing `Hello`/`Welcome`
     exchange works over the socket (the M1 client already speaks it).
   - `kill -9` the daemon: launchd restarts it within about 5 s.
   - CI green. Commit: `macOS: run the service as a launchd daemon over a Unix socket`.

## M3 — engines and tunnels

1. **Paths and versions:**
   - `components.rs` macOS: run `openvpn --version` and
     `openconnect --version` from `/opt/molehill/engines`.
   - Report `installable = false`.
   - Log an error if the OpenVPN major.minor isn't the pinned one (constant
     `PINNED_OPENVPN`).
2. **Blocklist (§3 table, `ovpn.rs`):**
   - Test first: `dns-updown x` and `client-crresponse x` are refused on
     macOS and still accepted on Windows (`cfg`-split test).
   - Then add `BLOCKED_MACOS`.
   - Also on macOS only: `dev tap` / `dev-type tap` refused with a clear
     message, and `persist-tun` added to the stripped list.
3. **`dns-up` / `dns-down` subcommands** (§6), most of this milestone's
   tests:
   - **Parser:** reads `dev`, `foreign_option_N`, the `dns_*` variables and
     `redirect_gateway`/`route_redirect_gateway_*` from an explicit map,
     never from `std::env` directly. `main` builds the map from an allowlist.
   - **Validation:** IPs must parse as `IpAddr`; domains must match
     `^[A-Za-z0-9-]{1,63}(\.[A-Za-z0-9-]{1,63})*\.?$`; at most 8 servers and
     32 domains; anything else is dropped and logged.
   - **Split/full rule** as in §6; the newest full tunnel is primary.
   - **Output:** the exact `scutil` stdin script as a string, so it can be
     tested; the run step calls `/usr/sbin/scutil` by absolute path with an
     empty environment.
   - **Tests:** hostile `foreign_option` (`"dhcp-option DNS 1.1.1.1\nremove State:/…"`),
     a `PATH` in the env map ignored, oversize lists, split vs full, IPv6
     DNS.
4. **`tunnel/openvpn.rs`** (§3 table, row 2):
   - A per-platform `fn script_args(id) -> Vec<String>`. On Windows it returns
     `["--script-security","1"]`, same as today.
   - On macOS it returns `--script-security 2`, `--up`/`--down` with the quoted
     absolute binary path plus `dns-up <id>`/`dns-down <id>`, `--up-restart`
     and `--dev-type tun`, plus `--dns-updown disable` if the version is 2.7 or
     newer.
   - **Test:** the generated arguments survive OpenVPN-style splitting with a
     path that contains a space.
5. **`tunnel/openconnect.rs`** (§3 table, row 3):
   - macOS: no `--interface`; `--script` is the bundled `vpnc-script`, quoted.
   - **Interface lookup:** parse the address from "Connected as &lt;ip&gt;" (the
     existing parser), then find the interface owning it via `getifaddrs`. Put
     the lookup in a per-platform function; Windows returns
     `Molehill-<id8>` as today.
   - Record the gateway host route in `state/routes` once connected.
   - **Stop:** `nix::sys::signal::kill(pid, SIGINT)`; the 10 s hard kill stays.
6. **`netstats_macos.rs`:**
   - `interface_bytes` via `sysctl([CTL_NET, PF_ROUTE, 0, 0, NET_RT_IFLIST2, 0])`,
     walking `if_msghdr2` and reading `ifm_data.ifi_ibytes/obytes` (64-bit).
   - **Test (macOS CI):** `lo0` counters exist and don't go backwards across a
     loopback send.
7. **libopenconnect** (§7):
   - **Loader:** `Api::load` on macOS uses
     `/opt/molehill/engines/libopenconnect.5.dylib` with default flags.
   - **Variadic shim:** add `crates/molehill-service/src/oc_progress.c`:
     ```c
     #include <stdarg.h>
     #include <stdio.h>
     typedef void (*mh_progress_fn)(void *ctx, int level, const char *msg);
     static mh_progress_fn g_rust_cb;
     void mh_set_progress(mh_progress_fn f) { g_rust_cb = f; }
     void mh_progress(void *ctx, int level, const char *fmt, ...) {
         char buf[2048]; va_list ap; va_start(ap, fmt);
         vsnprintf(buf, sizeof buf, fmt, ap); va_end(ap);
         if (g_rust_cb) g_rust_cb(ctx, level, buf);
     }
     ```
     Build it with `cc` from `build.rs` under `CARGO_CFG_TARGET_OS == "macos"`.
     On macOS, pass `mh_progress` to `vpninfo_new` instead of the Rust fn. The
     Windows code path is untouched.
   - **Test (macOS CI):** call `mh_progress(ctx, 1, "%s-%d", "x", 7)` and
     assert the Rust side gets `"x-7"`.
   - **CA trust:** after `vpninfo_new`, on macOS, call
     `openconnect_set_system_trust(vpn, 0)` and
     `openconnect_set_cafile(vpn, "/opt/molehill/engines/cacert.pem")`. Both
     symbols are optional in `Api`, so missing ones are logged, not fatal.
8. **SSO:**
   - `fortinet_sso.rs`: the curl path and `-o` target come from `cfg`
     constants.
   - `gp_sso.rs`: `clientos`/`os-version` from `cfg` constants (`"Windows"`
     unchanged; macOS `"Mac"` plus a macOS version string read from
     `sw_vers -productVersion`).
   - Existing tests (2 + 2) stay green; add a macOS-cfg assertion for the
     GP query string.
9. **Verify on the Mac**, engines installed by hand with `brew` and
   copied/relinked using the M6 bundling script, run early:
   - OpenVPN: password and SSO; split and full DNS (`scutil --dns` shows the
     resolver; `scutil` keys gone after disconnect).
   - AnyConnect / Fortinet / GP password logins.
   - Two tunnels at once.
   - `kill -9` the daemon mid-tunnel, then check the sweep cleaned DNS and
     routes.
   - CI green. Commit: `macOS: OpenVPN and OpenConnect tunnels`.

## M4 — Keychain and `.p12` client certificates

1. **`secrets_macos.rs`:**
   - `security_framework::passwords::{set,get,delete}_generic_password`, with
     service `Molehill` and account `<id>`.
   - Add `save/load/delete_cert_password(id)` using account `<id>/cert`.
     Windows gets the same three functions on the Credential Manager, target
     `Molehill/<id>/cert`. That is a new function, so Windows behaviour is
     unchanged.
   - **Test (macOS CI, login keychain present on runners):** round-trip,
     delete, missing → `None`.
2. **IPC (§3 table):** `AuthRequest.cert_password: Option<String>` with
   `#[serde(default)]`. Test: old JSON without the field still deserializes.
3. **`on_form` branch (§3 table, §8):**
   - **Tests first**, using the existing `plan_form` test harness
     (`openconnect_auth.rs` has 8):
     - a form with `auth_id == "openconnect_pkcs12"` and a request with
       `cert_password = Some(p)` → filled with `p`, `password_used` unchanged
     - the same form with `None` → a `Prompt::Form` with the passphrase
       wording
     - a second `openconnect_pkcs12` form, meaning a wrong passphrase → a
       prompt, not a refill
     - the normal login form afterwards still auto-fills the login password
   - Then implement.
4. **Editor (Slint):**
   - New property `cert-mode: "store" | "file"`, set from Rust by platform
     (Windows `"store"`, so the UI is unchanged).
   - The `"file"` mode shows "Certificate file…" using `rfd`. On pick, copy to
     `<user dir>/certs/<id>.p12` (`0600`) and store the path.
   - Deleting a profile removes the file and the Keychain items.
   - `snapshot` example: add a macOS-mode screenshot for the PR.
5. **`localauth.rs` / `main.rs:640-677` flow:** pass `cert_password` from the
   Keychain. On a successful login with a typed passphrase and "save" ticked,
   store it.
6. **Verify on the Mac:** AnyConnect with `.p12`, with and without a saved
   passphrase, and with a wrong passphrase. CI green. Commit:
   `macOS: Keychain passwords and .p12 client certificates`.

## M5 — the app on macOS

1. **Tray:**
   - Create it inside `slint::invoke_from_event_loop`, queued before
     `run_event_loop_until_quit` (`main.rs:194`).
   - Template PNGs per state go in `crates/molehill/assets/macos/` (generated
     from the brand SVGs, `@1x`/`@2x`).
   - Native `muda` menu built from the same model the Slint popup uses. Rebuild
     or update items on every state change.
   - `with_menu_on_left_click(false)`; a left click shows the window.
2. **`platform_macos.rs`** (via `objc2-app-kit`):
   - activation policy switching (Accessory when hidden or `--hidden`,
     Regular when shown)
   - Dock badge via `NSApp.dockTile.setBadgeLabel`
   - `is_dark()` from `NSApp.effectiveAppearance`
   - single-instance `flock`; the second instance runs `open -b` and exits
3. **Apple Events:** register handlers on `NSAppleEventManager` for
   `kAEReopenApplication` (show the window) and `kAEGetURL` (forward
   `globalprotectcallback:` URLs into the existing `SsoCallback` path). Do it
   before winit finishes launching, so a cold launch from a URL isn't lost;
   test that case by hand.
4. **`gpcallback_macos.rs`:**
   - `register()`: remember the current default handler via
     `LSCopyDefaultHandlerForURLScheme`, then
     `LSSetDefaultHandlerForURLScheme(scheme, our bundle id)`.
   - `unregister()`: restore it.
   - At startup, if GlobalProtect.app is installed and we're the default,
     hand the scheme back.
   - The existing 2 tests are Windows-only; add a macOS test for the
     "remember and restore" logic behind a trait (no real LaunchServices
     calls in CI).
5. **`selfupdate_macos.rs` notification:** `UNUserNotificationCenter`
   request plus post; on authorization failure, run
   `osascript -e 'display notification …'`.
6. **Bundle:** `packaging/macos/Info.plist` with:
   - `CFBundleIdentifier io.github.giorgobiani.Molehill`
   - `CFBundleURLTypes` (`globalprotectcallback`)
   - `LSMinimumSystemVersion 15.0`
   - `NSHighResolutionCapable`

   Plus `packaging/macos/make-app.sh`, which builds `Molehill.app` from the
   release binary and the `.icns`, then runs `codesign -s - --force`.
7. **Verify on the Mac:**
   - menu bar in light and dark mode
   - close → no Dock icon; Finder or Spotlight reopen → window
   - Cmd+Q
   - GP SSO with and without GlobalProtect.app, in both install orders
   - `--hidden` start with no Dock flash

   Then CI green. Commit: `macOS: menu bar, Apple Events, Dock and notifications`.

## M6 — packaging, release, self-update, docs

1. **`packaging/macos/bundle-engines.sh <arch> <outdir>`:**
   - `brew install` the pinned formulae.
   - Copy the binaries, `libopenconnect` and `vpnc-script`.
   - `dylibbundler -od -b -x … -d <outdir>/lib -p @loader_path/lib`, then
     `codesign -s - --force` on every Mach-O.
   - `otool -L` gate: fail on any path outside `@loader_path`/`@rpath`,
     `/usr/lib` and `/System`.
   - Write `licenses/` and `SOURCES.txt` (tarball URLs plus formula patch
     URLs).
2. **`.github/workflows/engines.yml`** (manual dispatch, inputs: openvpn and
   openconnect versions):
   - matrix `macos-15` and `macos-15-intel`
   - run `bundle-engines.sh`
   - upload `engines-<ov>-<oc>-<arch>.tar.gz` plus source tarballs to a
     release tagged `engines-<ov>-<oc>`
3. **`.pkg` scripts:** `packaging/macos/scripts/{preinstall,postinstall}`,
   `distribution.xml` (`allowed-os-versions` ≥ 15, `hostArchitectures`),
   `uninstall.sh`, and the two plists. All per §11, including:
   - writing `cacert.pem` via
     `security find-certificate -a -p /System/Library/Keychains/SystemRootCertificates.keychain`
   - the `launchctl kickstart -k gui/$uid/…` relaunch

   `packaging/macos/build-pkg.sh <version> <arch>` assembles them.
4. **`release.yml`:**
   - Add a `macos` matrix job (`macos-15` arm64, `macos-15-intel` x86_64).
   - Steps: version check, tests, release build, download the pinned engine
     bundle, `make-app.sh`, `build-pkg.sh`, `.sha256`, fixed-name copies.
   - The existing Windows job is untouched. The final publish step collects
     the macOS artifacts as well, via `needs:` plus `download-artifact`.
   - Verify by running the workflow on the fork with a throwaway tag
     `v0.0.0-mac.1`. Delete the release afterwards.
5. **`update.rs`:**
   - Asset selection per target:
     `molehill-setup-*.exe` (unchanged) /
     `Molehill-*-{arm64|x86_64}.pkg`, picked with
     `cfg(target_arch)`.
   - Extend the existing 2 tests with a macOS release-JSON fixture.
6. **`selfupdate_macos.rs` install:**
   - Run the `osascript … installer -pkg` command line from §11, spawned with
     `process_group(0)`, then quit.
   - Test the command-line construction, including a `$TMPDIR` path with a
     space.
7. **Docs:**
   - README: macOS install (both `.pkg` files, Open Anyway), uninstall, the
     Keychain prompt after updates, the Local Network prompt, and the dev
     loop.
   - `docs/index.html`: a macOS download.
   - CONTRIBUTING: macOS local checks.
8. **Verify:**
   - **Clean machine** (no Homebrew): first install via Open Anyway; connect
     with each protocol; certificates are trusted without prompts.
   - **Intel Mac:** install, OpenVPN plus one OpenConnect protocol, update,
     uninstall.
   - **Update path:** install `v0.0.0-mac.1`, publish `v0.0.0-mac.2` on the
     fork (temporarily pointing `update.rs`'s repo constant at the fork via an
     env var, `MOLEHILL_UPDATE_REPO`, debug builds only); the update prompts
     once for the password, tunnels drop and come back, and the Keychain asks
     once.

   CI green. Commit: `macOS: .pkg installer, release workflow and self-update`.

## M7 — upstream PR (no code)

1. **Rebase `feat/macos` onto the latest `origin/main`** (synced from upstream
   `main`); keep exactly six commits.
   - Run the full §14 matrix once more on both Macs.
2. **PR body** (repo template: *What and why / How it was tested / Checklist*):
   - the §3 shared-code edit table
   - the "runs as root" call-outs required by the template: daemon,
     `dns-up`, `script-security 2` on macOS, `.pkg` scripts
   - the test matrix with results
   - screenshots from `snapshot`
   - a link to the separate blocklist issue
3. **Before opening:** confirm with the user. Opening an upstream PR is
   outward-facing.

---

## Open items that block nothing now

- Post `docs/superpowers/issues/ovpn-blocklist-dns-updown.md` upstream as an
  issue. The user posts it, because this session can't attach
  `giorgobiani/molehill` alongside the fork.
- The bundle id `io.github.giorgobiani.Molehill` is a placeholder until
  giorgobiani confirms it.
- Pin versions: choose `PINNED_OPENVPN` and the OpenConnect version when M3
  starts (the latest Homebrew stable at that point), and review `BLOCKED`
  against that OpenVPN version's manual.
