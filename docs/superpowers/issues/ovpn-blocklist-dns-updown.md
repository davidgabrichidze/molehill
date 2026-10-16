**Title:** `ovpn::sanitize`: also block `dns-updown` and `client-crresponse`

---

### Summary

`crates/molehill-core/src/ovpn.rs` says that `sanitize` "rejects anything that
executes programs, loads libraries, writes files, or still references a path",
and `BLOCKED` lists every OpenVPN directive that runs a script. Two
script-running directives are missing:

| Directive | Since | What it does |
|---|---|---|
| `dns-updown <cmd>` | OpenVPN 2.7 | Runs `<cmd>` to apply and remove pushed DNS settings. |
| `client-crresponse <cmd>` | OpenVPN 2.6 | Runs `<cmd>` with a client's challenge/response answer. It's a server option, but so are `client-connect` and `auth-user-pass-verify`, which are already blocked. |

I suggest adding both to `BLOCKED`, plus a test in the style of
`blocks_scripts_and_strips_cred_files`.

### Is this exploitable today?

As far as I can tell, **no**. The service starts OpenVPN with
`--script-security 1` (`tunnel/openvpn.rs:208`), and `sanitize` strips any
`script-security` in a profile. At level 1, OpenVPN refuses to run
user-defined scripts, and as I understand it that includes a `dns-updown`
command. I haven't confirmed that with a running 2.7 build, so please treat
that part as "very likely" rather than verified.

Even so, the change is worth making:

- **Defence in depth.** `ovpn.rs` treats a config as "code running with full
  privileges" and keeps the blocklist as its own layer, independent of
  `--script-security`. With these two missing, that layer quietly no longer
  covers everything.
- **The OpenVPN on Windows is a moving target.** `components.rs` installs the
  newest OpenVPN release, so Molehill users get 2.7, and `dns-updown` with it,
  without any change to Molehill.
- **The macOS port needs it.** On macOS, OpenVPN applies routes but not DNS, so
  the port has to start OpenVPN with `--script-security 2` and an `--up`/`--down`
  hook that is Molehill's own binary. At that level the blocklist becomes the
  thing that keeps profile-supplied scripts out. The port adds these two
  directives to the blocklist on macOS regardless. This issue asks whether
  you'd also like them blocked on Windows, so the two platforms filter
  profiles the same way.

### Effect on users

A profile containing either directive will be refused at import, with the
existing message "`dns-updown` is not allowed (runs programs or writes files
as SYSTEM)". I don't expect real client profiles to use either one:
`dns-updown` is new and normally set by distributions, not by VPN providers,
and `client-crresponse` is server-only.

### Suggested change

```rust
const BLOCKED: &[&str] = &[
    // ...existing entries...
    "tls-crypt-v2-verify", "x509-track",
    "dns-updown", "client-crresponse",
];
```

```rust
#[test]
fn blocks_new_script_hooks() {
    for d in ["dns-updown /tmp/x.sh", "client-crresponse /tmp/x.sh"] {
        let problems = sanitize(&format!("client\n{d}\n")).unwrap_err();
        assert_eq!(problems.len(), 1, "{d}");
    }
}
```

It may also be worth checking the `BLOCKED` list against the OpenVPN 2.7
manual's script-hook section from time to time, since new hooks get added.

I'm happy to open the PR if you agree.
