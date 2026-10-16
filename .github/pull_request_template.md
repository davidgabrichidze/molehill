## What and why

<!-- One or two sentences: what changes for the user, and why. -->

## How it was tested

<!-- e.g. unit tests, snapshot screenshots, connected to a real Cisco / Fortinet / GlobalProtect / OpenVPN server. -->

## Checklist

- [ ] `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass
- [ ] UI changes: screenshots attached (`cargo run -p molehill --example snapshot -- snapshots`)
- [ ] Anything that changes what runs as SYSTEM (service, configs, installers) is called out above
