# Contributing to Molehill

## Workflow

`main` is always releasable. Nothing is pushed to it directly.

1. **Branch** from `main`: `feat/split-tunnel`, `fix/forti-connected-state`,
   `docs/readme`, `chore/deps`.
2. **Open a pull request** into `main`. Fill in the template: what changes,
   how you tested it, and anything that touches what runs as SYSTEM.
3. **CI must pass** (clippy with warnings as errors, tests, release build), and
   the branch must be up to date with `main`.
4. **Squash and merge.** The pull request title becomes the commit on `main`,
   so write it as one: `Fortinet: recognise "Configured as" as connected`.
   The branch is deleted after the merge.

Dependabot opens weekly pull requests for Rust crates and GitHub Actions;
they follow the same rules.

## Releasing

Releases are cut from `main` with a version tag; the
[release workflow](.github/workflows/release.yml) does the rest.

1. In a pull request, bump `version` in the root `Cargo.toml`
   (`[workspace.package]`) and run `cargo build` so `Cargo.lock` follows.
   Merge it.
2. Tag the merge commit and push the tag:
   ```powershell
   git switch main; git pull
   git tag v0.2.0
   git push origin v0.2.0
   ```
3. The workflow checks the tag matches `Cargo.toml`, runs the tests, builds
   the release binaries and the installer, and publishes a GitHub Release
   with `molehill-setup-0.2.0.exe`, its `.sha256`, and generated notes.

Running copies of Molehill check the latest release at start and twice a day,
show a notification and an **Update** button, then download the installer,
verify its SHA-256 and run it silently. The service restarts during the
update, so tunnels drop for a moment.

Versioning is [SemVer](https://semver.org): patch for fixes, minor for
features, major for changes that need users to act (e.g. profile format).

## Local checks

```powershell
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p molehill --example snapshot -- snapshots   # UI screenshots, demo data
```

See the README's *Building* section for the service development loop.
