# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Status reporting from the builder.** `Config::builder()` gains
  `report_endpoint`, `report_interval`, `report_header`, `report_attribute` and
  `report_instance_id`, so a host embedding `serve_embedded` can turn on
  `[report]` without TOML; the embedding guide documents what it reports.

## [0.2.0] - 2026-10-23

### Added

- **Status reporting.** `[report]` (`--report-endpoint` / `LODE_REPORT_ENDPOINT`,
  `--report-interval` / `LODE_REPORT_INTERVAL`) pushes the host's status over
  OpenTelemetry OTLP/HTTP (JSON): metrics every `interval` (default 300 s) —
  `lode.app.up`, `lode.app.status`, `lode.app.restarts`, `lode.app.uptime`,
  `lode.update.available`, `lode.app.last_good`, `lode.resource.version` — and
  lifecycle events (`lode.started`, `lode.stopping`, `lode.updated`,
  `lode.rolled_back`, `lode.paused`, `lode.resource.updated`,
  `lode.check.failed`) with standard resource attributes (`service.name`,
  `service.version`, `service.instance.id`, `host.*`, `os.type`). Works with any
  OTLP receiver, a lode-server registry (`/r/otlp`) included. Off unless an
  endpoint is set; reporting runs on its own thread and never delays
  supervision.
- **Private GitHub repositories.** `[update].github_token` (`--github-token`,
  `LODE_GITHUB_TOKEN`, `${ENV}` expanded) authenticates the GitHub source.
  With a token, assets, `.sig` sidecars and delegations are downloaded through
  the release-asset API (`browser_download_url` answers 404 for a private
  repository), and the token is sent to the API host only — never to the
  storage host the API redirects to. A 404 from the API now says whether a
  token is missing or lacks access.

### Changed

- The Rust SDK (`sdks/lode.rs`) no longer uses `unsafe`: the `state.json.lock`
  guard takes std's `File::lock` (the lock lode itself takes) and
  `install_term_handler` uses `signal-hook`. Apps that vendor the file must add
  `signal-hook = "0.4"`; it needs Rust 1.89+.

### Fixed

- A token in `[http].headers` could not download assets from a private GitHub
  repository; use `[update].github_token`.
- Credentials stripped on a redirect hop (normal for pre-signed storage URLs)
  are logged at debug level instead of a warning on every download.

## [0.1.0] - 2026-10-15

### Added

- **Delegated signing.** A trusted (root) key can stay offline and delegate
  signing to a sub-key for one manifest name and a validity window
  (`lode-cli delegate`). The delegation travels as the manifest's top-level
  `delegation`, or as the `lode-delegation.json` asset of a GitHub release;
  `manifest`, `manifest-sign` and `verify` take `--delegation`.
- **Revocation lists.** `[trust].revocation_url` (`--revocation-url`,
  `LODE_REVOCATION_URL`) subscribes to a list of revoked sub-key ids signed by
  a root or a valid sub-key (`lode-cli revoke`). Verified lists are merged into
  `$LODE_DIR/revocations.json`; an unreachable list never blocks an update.
- **Resources.** `[[resource]]` distributes config files, data sets and other
  bundles under the app's trust model, published as `<app>/<name>`. They are
  installed under `$LODE_DIR/resources/<name>` before the app starts, passed to
  it as `LODE_RESOURCE_<NAME>`, updated per their own `policy`, and announced
  to the app by `SIGHUP` or a restart (`notify`). The app requests a version
  through `state.json` `resource_target`; `lode-cli pull` does the same from
  outside, and an older version rolls back.
- The SDKs gain `resource`, `applyResource` and `resourcePath`.
- `lode-cli status` reports the manifest's signing scheme, the configured
  resources and the loader's trust features.

### Changed

- **Breaking:** `Manifest` and `Asset` gain a `delegation` field, `Config` a
  `resources` field, `Trust` a `revocation_url` field and `State` the
  `resources` and `resource_target` fields; code that builds them with struct
  literals must set them.
- `[global].app` may no longer contain `/`, which separates an app from its
  resources.
- Hosts on 0.0.3 or older reject sub-key signatures; upgrade them before
  publishing with a sub-key.

### Security

- rustls 0.23.46 (RUSTSEC-2026-0346, -0347, -0348, -0349).

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
