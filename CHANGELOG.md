# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.3] - 2026-10-12

### Added

- `SelfUpdate::channel` follows a channel other than `stable`, and
  `SelfUpdate::header` adds a request header for a source that needs
  credentials. `${VAR}` in a header value comes from the environment, and the
  header goes to the source's own hosts only.
- `SelfUpdate::require_manifest` requires the manifest to be signed (`Signed`)
  or signed with an unexpired expiry (`Fresh`), so a source cannot hold a
  program on an old version by replaying an old manifest.
- `SelfUpdate::fetch` downloads, verifies and unpacks a release without
  installing it, and `SelfUpdate::install` puts it in place: a host that ships
  more than its executable takes the other files from `Fetched::dir` in
  between, and one that replaces its executable itself takes it from
  `Fetched::binary` and never calls `install`. `apply` is the two in one step.
- A manifest version may carry `published_at`; the GitHub adapter fills it and
  `notes` from the release.
- An embedding guide, `docs/embedding.md`, and a second demo program in
  `examples/embed` that walks through a self-update end to end, offline.

### Changed

- **Breaking:** `SelfUpdate::check` returns the release (`Available`: version
  and manifest entry) instead of the version id alone.
- **Breaking:** `SelfUpdate` methods return `SelfUpdateError`, an enum with a
  variant per failure, instead of the crate's message-carrying `Error`.
  `is_untouched()` tells a failure that changed nothing from one that left
  the installation to be put right. It converts into `Error` with `?`.

## [0.0.2] - 2026-10-12

### Added

- **Self-update on Windows.** `lode-cli self-update` and
  `lode_core::self_update::SelfUpdate` now work there. A running executable
  cannot be overwritten on Windows, so the old file is renamed aside, the new
  one takes its place, and the old one is deleted the next time lode starts
  (`self_update::remove_replaced` for a host that embeds the library).
  `lode-cli self-update` replaces both `lode.exe` and `lode-cli.exe`;
  `SelfUpdate::alias` names such a second copy.

### Changed

- `SelfUpdate` looks for `<app>.exe` in a Windows asset by default.
- Workflows run on `ubuntu-26.04` and current action releases.

## [0.0.1] - 2026-10-11

First release.

### Added

- **Loader (`lode`)**: resolves a version from a native `lode/v1` manifest or
  GitHub Releases, downloads the selected asset, verifies its sha256 and
  publisher signature, installs it and launches the app.
- **Supervisor**: PID-1 subreaper, signal forwarding, `exec` passthrough,
  crash restart with backoff and a keep-alive pause, readiness by uptime or by
  the `state.json` handshake, staged updates with an app-paced cut-over, and
  single-strike rollback to the last good version.
- **Updates**: `off` / `check` / `auto` policy, channels, version pinning, a
  downgrade floor, and pruning of old versions.
- **Trust**: ed25519, ECDSA P-256 and P-384 publisher keys;
  `require_signature = off | auto | enforce`; signatures bind the app name,
  the asset name, version, digest and launch overrides. A manifest can carry
  a signed catalog and expiry, required with `require_manifest = signed | fresh`.
- **Runtime dependency**: optional download and version check of a runtime
  such as Bun, Node or Deno.
- **Operator and publisher toolkit (`lode-cli`)**: `status`, `versions`,
  `update`, `rollback`, `restart`, `self-update`, `seed`, `init`, `keygen`,
  `sign`, `verify`, `manifest`, `manifest-sign`, and `publish`, which
  releases a version to a lode-server registry (default `https://lode.bk.io/u`).
- **Embedding**: `lode-core` (config and engine, no CLI or signal handling) and
  `lode-supervisor` (the supervise loop behind an injected signal source);
  `lode_core::self_update` replaces a host's own binary with a signed release.
- **App SDKs** for Go, Rust and TypeScript under `sdks/`.
- **Distribution**: static musl binaries for Linux x86_64 / aarch64, binaries
  for macOS x86_64 / arm64 and Windows x86_64, and the `bkhq/lode` container
  images.
