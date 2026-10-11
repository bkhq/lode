# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
