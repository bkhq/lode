# 20261015-0107-resource-distribution Distribute arbitrary resources

- **status**: completed
- **createdAt**: 2026-10-15 01:07
- **approvedAt**: 2026-10-15 01:20
- **relatedTask**: 20261015-0106-resource-distribution

## Context

Goal: ship program resources and config files through the same trust model as the
app binary (sha256 + publisher signature + delegation + revocation), versioned and
updated independently of the app release.

Current state:

- One `lode.toml` = one app (`[global].app`) = one manifest = one `current` version
  (architecture §1, "single program, single channel"). Resources today must be baked
  into the app asset (`tar.gz`/`zip`) and ship only with an app release.
- Reusable, source-agnostic pieces: `manifest::fetch` / `parse` / `resolve_target` /
  `select_asset`, `download::fetch_artifact`, `install` staging + unpack formats
  (`raw`, `gz`, `tar.gz`, `zip`), `switch_current`, `prune`, `gc`,
  `verify_manifest_identity`. They are keyed on `cfg.global.dir/versions` and
  `cfg.global.app`, so they need a root-dir / identity parameter to serve resources.
- Artifact signature message: `lode.artifact.v2\n{app}\n{asset}\n{version}\n{sha}\n{run}\n{exec}`.
- App IDs are restricted by `idval::validate_id` (`[A-Za-z0-9._-]`, no `/`).
- App contract: env `LODE_DIR`, `LODE_WORKDIR`, `LODE_ACTIVE_VERSION`, ...;
  `state.json` is the two-way channel; the supervisor checks updates every
  `check_interval` with `off|check|auto` policy.
- A reload refuses changes to trust anchors and update sources
  (`supervise::trust_anchor_changed`).

## Proposal

### 1. Model

A **resource** is a named, separately published, separately versioned bundle that
belongs to the app instance. It uses an ordinary `lode/v1` manifest (own channels,
versions, assets, catalog signature, `expires`, `delegation`). No new manifest schema.

```toml
[[resource]]
name = "config"                                   # local id (validate_id)
manifest = "https://releases.example.com/myapp-config/manifest.json"
channel = "stable"                                # default: [update].channel
asset = "config.tar.gz"
policy = "auto"                                   # off | check | auto (default: [update].policy)
notify = "signal"                                 # none | signal | restart (default: none)
required = true                                   # app does not start until installed
```

Env overrides follow the existing pattern only where needed (`LODE_RESOURCE_<NAME>_MANIFEST`).

### 2. Identity and trust

- The resource manifest's `name` must be `<app>/<resource>` (e.g. `myapp/config`).
  `/` is illegal in an app id, so a resource catalog can never pass as an app catalog.
- New artifact message with its own domain, no launch overrides:

  ```text
  lode.resource.v1\n{app}\n{resource}\n{asset}\n{version}\n{sha256}
  ```

  An app asset signature can never verify as a resource and vice versa. Resource
  assets carrying `run`/`exec` are rejected at parse.
- Same `[trust]` section (keys, `require_signature`, `require_manifest`,
  `revocation_url`). No per-resource trust override in this round.
- With delegation, the cert `name` scopes a sub-key: a cert for `myapp/config`
  can publish config but **cannot** publish the app binary (`myapp`). This lets an ops
  team hold a config-only signing key. Exact match only; no wildcards.

### 3. On-disk layout and app contract

```text
$LODE_DIR/resources/<name>/versions/<ver>/   # unpacked bundle
$LODE_DIR/resources/<name>/current           # atomic switch (same mechanism as app)
$LODE_DIR/resources/<name>/downloads/        # cache, pruned with keep_versions
```

- Env injected into the app: `LODE_RESOURCE_<NAME>` = path of
  `resources/<name>/current` (stable across updates).
- `state.json` gains lode-owned `resources: { "<name>": { "current": "<ver>",
  "available": "<ver>"?, "check_error": "..."? } }`, so the app can see the live
  version without env (env is fixed at spawn).
- App-triggered checks reuse the existing `check_nonce` / `checked_nonce` handshake
  (SDK `requestCheck()`): one requested check covers the app and every resource;
  `checked_nonce` is bumped only after all of them are done. A resource under
  `policy = "auto"` applies a found update; under `check` it only records `available`.
- New app-owned request `resource_target: { "<name>": "<ver>" }` (mirrors `target`)
  lets the app apply a resource version found by a `check`-policy check.
- SDKs (Go, Rust, TypeScript): `resource(name)` -> `{ path, current, available }`,
  `applyResource(name, version)`; `requestCheck()` is unchanged.

### 4. Lifecycle

- **Boot**: before the app spawns, each `required` resource is installed if missing
  (fetch failure with a cached version -> use the cache and warn; nothing cached ->
  boot fails like a missing app version). Non-required resources are fetched in the
  background loop.
- **Update check**: on every `check_interval`, the supervisor checks each resource
  with its own policy: `check` records `available`, `auto` downloads, verifies, installs,
  switches `current`, prunes, then notifies.
- **Notify**: `none` (app watches `state.json` / the path, or calls
  `requestCheck()` itself), `signal` (always `SIGHUP`, not configurable; on Windows
  falls back to `none` with a warning), `restart` (graceful restart through the
  existing restart path, same version of the app).
- **Rollback**: no readiness gate for resources in this round. Manual
  `lode-cli rollback --resource <name>` switches back to the previous installed version.
- **Reload**: `[[resource]]` manifest URLs are trust-relevant and pinned like the
  app source in `trust_anchor_changed`.

### 5. Code changes

- `lode-core`: `config.rs` (`[[resource]]`, validation, env overrides); a small
  "install root" abstraction (`root dir`, `identity`) so `install`/`switch_current`/
  `prune`/`gc`/`download` serve both app and resources without duplication;
  `verify.rs` (`resource_message`); `manifest.rs` (name check, reject `run`/`exec`);
  new `resource.rs` (resolve/fetch/install/switch per resource); `state.rs`
  (`resources` map); `commands/{status,versions,update,rollback}.rs`
  (`--resource <name>`).
- `lode-supervisor`: boot ordering, per-resource checks in the update loop, notify,
  env injection, reload pinning.
- `lode` CLI: `--resource` flags; `sign --resource <name>` to produce the resource
  message; `manifest` accepts `--name myapp/config`.
- `lode-cli pull`: one-shot fetch + verify + install + switch of a resource outside the
  supervisor, for cron/CI. Reads the same `lode.toml` (`--resource <name>`, or all) and
  takes the state lock like other commands. Exit code 0 = up to date or updated,
  non-zero = failed.
- SDKs (`sdks/`): `resource(name)`, `applyResource(name, version)`.
- Docs: architecture (new section), integration, operations, `lode.example.toml`,
  zh-CN mirrors, `CHANGELOG.md`.

### 6. Tests (RED first)

- Config parsing/validation of `[[resource]]` (duplicate names, bad ids, defaults).
- Cross-domain rejection: app signature presented as resource and vice versa;
  `myapp/config` cert cannot sign `myapp`; resource asset with `run` rejected.
- Install/switch/prune under `resources/<name>/`.
- Supervisor: required resource gates boot; `auto` update switches and signals;
  failure keeps the old version; reload refuses a changed resource manifest URL;
  `check_nonce` covers resources and `checked_nonce` waits for all; `resource_target`
  applies a version.
- `lode-cli pull`: updates, no-op when current, fails without touching `current` on a
  bad signature.
- SDK unit tests for `resource` / `applyResource` (state.json read/write under lock).

## Risks

- **Scope**: largest change so far; touches supervisor loop and the install path
  shared with the app. Mitigated by the install-root refactor landing first with no
  behaviour change.
- **Partial updates**: app and resource versions move independently; an app may see a
  resource version it cannot read. No compatibility constraint in this round (see
  Annotations).
- **Signal semantics**: an app that does not handle `SIGHUP` may terminate on
  `notify = "signal"`; docs must say so.
- **Disk usage**: `keep_versions` applies per resource.

## Scope

- ~2000+ lines incl. tests across `lode-core`, `lode-supervisor`, `lode`, SDKs, docs.
- Depends on 20261015-0103-delegated-signing-keys for the delegation scoping; can
  start after it lands (shared `effective_keys`).
- Out of scope: lode-server `publish` for resources, GitHub-releases resources,
  resource readiness/auto-rollback, app/resource compatibility constraints,
  configurable notify signals, and a `lode-core` embedding API (easy follow-up on top
  of `resource.rs`).

## Alternatives

- **Resources inside the app manifest** (`resources` section): one fetch and one
  catalog signature, but couples publishing cadence and publishers; loses
  config-only delegation. Rejected.
- **Separate lode instance per resource**: works today for data-only bundles with
  `[command]` set to a no-op, but one lode = one supervised child; no notification to
  the real app and doubled locks/state. Rejected.
- **One-shot `lode-cli pull` only**: simplest, but needs an external scheduler and has
  no app notification. Included alongside the supervisor path, not instead of it.

## Annotations

Decisions (2026-10-15):

1. No configurable signals; `signal` means `SIGHUP`. The SDK lets the app trigger a
   check itself (existing `requestCheck()` now covers resources) and apply a resource
   version (`applyResource`).
2. No app/resource compatibility constraint.
3. `lode-cli pull` is in the first round.

Implementation deviations (2026-10-15):

1. No `lode.resource.v1` message: the identity `<app>/<name>` is used as the `app`
   field of the existing `lode.artifact.v2` message and as the manifest `name`.
   App ids may not contain `/`, so app and resource signatures cannot be swapped,
   and delegations scope to an identity unchanged.
2. `required` defaults to `false`.
3. The per-resource state field is `error` (not `last_error`).
4. No env/CLI override for `[[resource]]`; resources come from `lode.toml` only.
5. `lode-cli pull [--resource] [--version]` replaces `versions --resource`,
   `update --resource` and `rollback --resource`; an older `--version` rolls back.
6. Trust capabilities are reported by `status` only.
7. `[[resource]]` sources (`name`, `manifest`) are pinned across config reloads,
   like the `[update]` source.
