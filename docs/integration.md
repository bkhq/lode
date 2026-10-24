# Integrating an app with lode

**English** · [中文](integration.zh-CN.md)

Integration spans three files, each with one owner:

| File | Where | Who writes it | Purpose |
|---|---|---|---|
| **`lode.toml`** | local | the operator | how lode fetches & runs your app |
| **`state.json`** | local (`$LODE_DIR`) | lode **and** the app | runtime comms (status ↔ requests) |
| **release feed** | remote | the publisher | the signed asset listing — a native `manifest.json` **or** GitHub Releases |

The three steps below — **configure → run → publish** — are the whole integration. The
operator names the exact asset to install (`[update].asset`); that filename is the
selection key for both sources. For the full signing spec see
[source-adapters.md](source-adapters.md); for exhaustive field listings see
[`lode.example.toml`](lode.example.toml) and [`manifest.example.json`](manifest.example.json);
the deep design is in [architecture](architecture.md).

---

## 1. Configure lode (`lode.toml`)

The operator's file: *how to fetch and run your app*. The app never writes it.
Precedence is `CLI > env (LODE_*) > lode.toml > defaults`; by default lode reads
`/srv/lode/lode.toml` (override the base dir with `LODE_DIR`) and scaffolds a
starter there on first run.

```toml
[global]
app      = "myapp"          # must match the manifest "name"
dir = "/srv/lode"      # holds lode.toml + versions/ + state.json + lode.pid + runtime/

[update]
github   = "owner/myapp"                                        # GitHub Releases …
# manifest = "https://releases.example.com/myapp/manifest.json" # … OR a native manifest (pick one)
asset    = "myapp-linux-x64.tar.gz"   # the asset filename for THIS host (the selection key)
channel  = "stable"         # github: stable=/releases/latest, else newest prerelease ; native: channel key
policy   = "auto"           # off | check | auto
# pin    = "1.4.2"          # lock a version (disables auto-update)

[trust]
require_signature = "enforce"                       # off | auto | enforce
trusted_keys = ["<key_id>:<base64-pubkey>"]         # from `lode-cli keygen`

[command]
run     = "./myapp"         # literal command to launch the app (cwd = version dir; a manifest asset may override)
exec    = "./myapp"         # literal base for `lode <args>` passthrough
# workdir = "{dir}"         # optional; omit for the version dir (default). Set an absolute path to run elsewhere (e.g. for .env discovery)

[supervise]
readiness    = "state"      # none | state (commit a version only after the app reports ready)
health_grace = 15           # seconds a new version must survive to be "good" (else rollback)
stop_timeout = 10           # graceful-stop window before SIGKILL
restart      = "on-failure" # on-failure (default, keep-alive: retry then pause) | always | off (mirror child)
```

Common shapes:

- **Self-contained binary:** `run = "./myapp serve"`, `exec = "./myapp"` (or omit `[command]` and publish `run`/`exec` in the manifest asset instead).
- **Script under a runtime:** `run = "bun run"`, `exec = "bun"`, plus a `[runtime]` block to fetch `bun` if it's not on PATH — cached for reuse under `runtime/<key>/` (the key follows `version`/`download`, so changing either re-downloads), optionally version-pinned (`version`); note a runtime download is **not** signature-verified.
- **Private source:** add `[http].headers = ["Authorization: Bearer ${TOKEN}"]` — sent on manifest + artifact fetches, with `${ENV}` expansion.

See [`lode.example.toml`](lode.example.toml) for every option and `[runtime]`/`[signals]`/`restart_*`.

---

## 2. The app contract (`state.json`)

What *your app* implements. Any language — read/write one JSON file and handle `SIGTERM`.

> **Prefer the SDKs.** Dependency-light clients in [`../sdks`](../sdks) (TypeScript / Go /
> Rust) wrap this whole contract — read status; request upgrade / restart / rollback;
> `hold`/`release` for maintenance; report readiness; `watch` lode's notifications —
> with the correct atomic, `state.json.lock`-serialised writes. The raw contract below
> is exactly what they implement (and all you need to port it to any other language).

**Environment lode injects:** `LODE_ACTIVE_VERSION` (current version), `LODE_DIR`
(lode's dir — `state.json` lives at `$LODE_DIR/state.json`), `LODE_WORKDIR` (the app's
run dir, i.e. its cwd), `LODE_CONFIG` (path to the `lode.toml` lode loaded — read it
read-only to see lode's config), `LODE_INSTANCE` (unique id for this launch — write it
to `state.ready`), `LODE_READINESS` (`none`|`state`), `LODE_RESOURCE_<NAME>` per
installed `[[resource]]` (its active path), and lode's own build info:
`LODE_VERSION` (lode's version), `LODE_COMMIT` (the git commit lode was built from;
absent when unknown) and `LODE_FEATURES` (comma-separated
[features](#lode-features)). The host env (e.g. `PORT`) passes
through; internal `LODE_*` are stripped. The operator can add more via the `[env]`
table — those are **defaults**: an inherited host env var of the same name (e.g. a
per-deploy `-e PORT`) wins over them, and lode's injected vars always win over everything.
See [Data directories & persistence](#data-directories--persistence) for the
`LODE_DIR`/`LODE_WORKDIR` vs app-owned `ROOT_DIR`/`DATA_DIR` model.

**state.json** — lode writes status, the app writes requests; the field sets don't overlap:

```jsonc
{
  // lode writes (app reads):
  "current": "1.4.2", "last_good": "1.4.2", "available": "1.5.0",
  "status": "running",        // starting|running|held|updating|rolling-back|stopping|stopped|error
  "pid": 12345, "last_check": "…", "last_error": null,
  "config_generation": 0,     // lode bumps this on a lode.toml edit while you run => restart to apply
  "checked_nonce": 0,         // the check_nonce lode last answered; "check_error" says why that check failed
  "resources": {},            // per [[resource]]: { current, available, path, error }
  "lode": { "version": "0.3.0", "commit": "…", "features": ["target", "…"] },  // the supervising lode
  // app writes (requests / readiness):
  "target": null,             // a version or "latest" => request up/down-grade
  "restart_nonce": 0,          // increment => restart the current version
  "check_nonce": 0,           // increment => check for a newer version now
  "hold": false,              // set true => lode will NOT (re)start the process (maintenance) => status "held"
  "resource_target": {},      // { "<name>": version | "latest" } => activate that resource version
  "ready": null               // set to LODE_INSTANCE => "I can serve now"
}
```

<a id="lode-features"></a>**Which lode is this?** `LODE_VERSION` / `LODE_COMMIT` /
`LODE_FEATURES` and `state.json`'s `lode` object (written when the loop starts) name
the supervising lode. Check a feature before relying on the part of this contract
it covers, rather than comparing versions. Names are stable and only ever added:

| Feature | Covers |
|---|---|
| `target` | `target` requests (a version or `"latest"`) |
| `restart` | `restart_nonce` |
| `check` | `check_nonce` / `checked_nonce` / `check_error` |
| `hold` | `hold` and `status = "held"` |
| `readiness-state` | `readiness = "state"` (`ready = LODE_INSTANCE`) |
| `staged-cutover` | the phased `-0` / `-1` / `-2` prepare handshake |
| `config-generation` | `config_generation` bumps on a `lode.toml` edit |
| `resources` | `[[resource]]`, `LODE_RESOURCE_<NAME>`, `resources` |
| `resource-target` | `resource_target` requests |
| `report` | `[report]` status reporting over OTLP/HTTP |

SDK: `lodeVersion()`, `lodeCommit()`, `lodeFeatures()`, `hasFeature(name)`, and
`read().lode`.

Implement these (all but `SIGTERM` are optional, but recommended):

- **Graceful stop (required):** on `SIGTERM`, drain and `exit(0)` within `stop_timeout`, or you're `SIGKILL`ed. lode sets `status = updating|stopping` first so you can distinguish. On Windows the request is a `CTRL_BREAK` console event instead (`SIGBREAK` in Node, `os.Interrupt` in Go); the SDKs' `onTerminate` handles both.
  ```ts
  process.on("SIGTERM", async () => { await drain(); process.exit(0) })
  ```
- **Readiness (if `readiness = "state"`):** after you can actually serve, atomically write `state.ready = LODE_INSTANCE` (temp-file + rename, preserving lode's fields). lode won't commit the version until then; missing it past `ready_timeout` → rollback.
  - **Phased prepare handshake (opt-in):** report serving with `state.ready = "{LODE_INSTANCE}-0"` instead of the bare token. On an update lode writes `state.ready = "{LODE_INSTANCE}-1"` to prompt you to prepare; drain/checkpoint, then ack with `"{LODE_INSTANCE}-2"` and the cut-over begins. The cut-over is app-paced by default (`prepare_timeout = 0`); a non-zero `[supervise].prepare_timeout` (seconds) forces it if you never ack. Mechanism details in [architecture](architecture.md) §8.
- **Health:** `exit(non-zero)` on startup failure. A version that exits within `health_grace` is rolled back to the last good one (single-strike).
- **Self-report version** (e.g. `GET /version`) matching `LODE_ACTIVE_VERSION`.
- **Request an update/restart (optional):** atomically patch `state.json` — set `target` (a version or `"latest"`) or bump `restart_nonce`. lode polls the file's mtime (~1s) and acts; the file *is* the notification.
- **Check for an update on demand (optional):** bump `check_nonce` to make lode check the source now, whatever `[update].policy` or `pin` say — the way to back a "check for updates" button. The check is finished once `checked_nonce` reaches the value you wrote: `check_error` then holds the reason if it failed, otherwise `available` is the newer version (absent when you are up to date). The check only reports; apply the version by setting `target`. Under `policy = "auto"` (without a `pin`) lode applies it by itself, as its periodic check would. SDK: `requestCheck()`.
- **Hold off a (re)start (optional):** set `hold = true` to tell lode **not** to (re)start the process — for planned maintenance that must finish before the app comes up (e.g. a DB migration needing CLI intervention). lode reports `status = "held"` and waits — at boot, after the child exits, and across `restart_nonce`/`target` requests — until you set `hold = false`. A hold gates a *start*, not a running child: lode never kills the current process, so to take the app down for maintenance set `hold = true` then `exit(0)` yourself (lode then holds instead of respawning). An operator can drive this the same way (`hold` is just a `state.json` field).
- **Resources (optional):** each `[[resource]]` (config files, data, assets —
  [architecture](architecture.md) §5) is installed before you start; read it from
  `$LODE_RESOURCE_<NAME>`. lode updates it per the resource's `policy`, then notifies
  you per its `notify` (`SIGHUP`, a restart, or nothing — watch `resources.<name>` in
  `state.json`). To pull one now, set `resource_target.<name>` to a version,
  `"latest"`, or an older version to roll back; `check_nonce` checks resources too.
  SDK: `resource(name)`, `applyResource(name, version)`, `resourcePath(name)`.
- **Read lode's config (optional, read-only):** `$LODE_CONFIG` is the path to the `lode.toml` lode loaded — read it to inspect how lode is configured. It is the **operator's** file: never write it (the app's write channel is `state.json`). SDK: `configPath()` / `readConfig()` (raw text; parse with your own TOML lib for fields).
- **Apply a `lode.toml`/`[env]` change while running (optional):** lode **never auto-restarts** on a config edit (a running app is never disturbed). When `lode.toml` is edited, lode **bumps `config_generation`** to notify you; apply the change at your own pace by bumping `restart_nonce` — that relaunch **re-reads `lode.toml`** (new `[env]`/config takes effect). Watch `config_generation` if you want to react to operator edits. (Host-process env — `-e`/k8s — still requires restarting lode itself.) `[trust]`, the `[update]` source, the `[[resource]]` sources, the `[http]` credential scope and the `[runtime]` download are **pinned for the life of the lode process**: a reload never changes them (the edit is refused and logged), so a restart request from the app can never weaken verification — those edits need lode itself restarted.

### Concurrent writes — the `state.json.lock` contract

`state.json` has two writers (lode and your app), and a read-modify-write —
read, patch your fields, write back — can silently lose the other side's
concurrent update. lode serialises **all of its own RMWs** with an exclusive
`flock(2)` on the sibling file `$LODE_DIR/state.json.lock` (created on
demand, never deleted; the lock lives on a sibling because `state.json` itself
is replaced by temp-file + rename on every write).

- **Apps doing RMW SHOULD take the same lock:** open `state.json.lock`
  (create if absent), `flock(fd, LOCK_EX)` (blocks only microseconds), then
  read `state.json` → patch your fields → write atomically (temp + rename) →
  unlock (closing the fd releases it). Hold the lock for the whole cycle and
  nothing more.
- **Plain reads need no lock:** the temp + rename replacement is atomic, so
  any read of `state.json` always sees a complete, consistent snapshot.

> Recommended: use the [SDKs](../sdks) (and see the [`../examples`](../examples), which
> integrate through them). A hand-rolled, zero-dependency Rust + Bun pair (no SDK) lives
> in [`../tests/apps`](../tests/apps) as the from-scratch reference.

---

## 3. Publish the release feed

lode resolves a **channel → version → asset**, verifies it, and installs/runs it.
The asset each host installs is chosen by **filename** (`[update].asset`), and every
asset carries a signature (ed25519 by default; ECDSA P-256 / P-384 also supported —
see [source-adapters §1 *Keys*](source-adapters.md#keys)) over the canonical message
`lode.artifact.v2\n{app}\n{name}\n{version}\n{sha256}\n{run}\n{exec}` (UTF-8, `\n`-separated, no trailing
newline; `run`/`exec` are empty string when absent). `app` is the app name (the manifest `name`, your operators' `[global].app`) and `name` is the asset filename. Full spec, including the native manifest
shape and field tables: [source-adapters.md](source-adapters.md).

Packaging + signing are the **publisher's** job, doable in any CI. `lode-cli` is a
reference implementation; any tooling that produces the same signature works.

### Keys (once)

`lode-cli keygen` prints `key_id`, the `trusted_keys` entry (`<key_id>:<base64url>`,
hand to operators), and the secret seed — keep it offline. Pass `--alg ecdsa-p256`
or `--alg ecdsa-p384` for an ECDSA key: its entry and key file are then tagged
`<alg>:…`, and every `--key` / `--key-env` / `--pubkey` accepts that tagged form.
Printed values are unpadded base64url; standard base64 is accepted on input as well.

### GitHub Releases (`github = "owner/repo"`)

Drop this workflow into **your app's** repo. It builds your assets and — **only if a
signing key is configured** — signs each one and uploads the signature beside it as a
`<asset>.sig` sidecar asset. With no key it uploads unsigned, so it works before you
adopt signing.

```yaml
# .github/workflows/release.yml — publish your app's assets for lode
on:
  release:
    types: [published]      # cut the release (UI or `gh release create`); this attaches assets
permissions:
  contents: write
jobs:
  release:
    runs-on: ubuntu-latest
    env:
      GH_TOKEN: ${{ github.token }}
      TAG: ${{ github.event.release.tag_name }}
      LODE_SIGNING_KEY: ${{ secrets.LODE_SIGNING_KEY }}   # optional — set it to enable signing
    steps:
      - uses: actions/checkout@v4

      - name: Build assets                # -> dist/<app>-<os>-<arch>.<ext>   (you supply this)
        run: ./build.sh "$TAG"

      - name: Publish (sign only if a key is set)
        run: |
          set -euo pipefail
          if [ -n "${LODE_SIGNING_KEY:-}" ]; then
            curl -fsSL https://github.com/bkhq/lode/releases/latest/download/lode-linux-x64.tar.gz \
              | tar -xz lode lode-cli                 # fetch lode-cli to sign with
          fi
          for f in dist/*; do
            if [ -n "${LODE_SIGNING_KEY:-}" ]; then
              ./lode-cli sign "$f" --app <app> --version "${TAG#v}" --key-env LODE_SIGNING_KEY \
                | awk '/^sig:/ {print $2}' > "$f.sig"          # sign the tag minus its `v`
              gh release upload "$TAG" "$f" "$f.sig" --clobber  # signature = `.sig` sidecar
            else
              gh release upload "$TAG" "$f" --clobber          # unsigned
            fi
          done
```

- **Enable signing:** run `lode-cli keygen` once; put the secret seed in the repo's
  `LODE_SIGNING_KEY` secret (keep a copy offline), and give operators the public
  `trusted_keys` entry. No secret set → assets upload unsigned (fine until you adopt it;
  the sign branch never runs).
- lode picks the asset whose `name` equals the operator's `[update].asset`; `sha256`
  comes from the asset `digest` (re-verified against the bytes), `version` from the tag
  minus its leading `v` (the id the signature must bind — hence `${TAG#v}` above), and
  the signature from the `<asset>.sig` sidecar asset (or the asset `label`, which
  GitHub would show in place of the filename).
  `channel = stable` → `/releases/latest`; other channels → newest non-draft prerelease;
  `pin` → a specific tag. No `manifest.json` asset is needed. Private repo:
  `[update].github_token = "${GITHUB_TOKEN}"` (a fine-grained token with *Contents:
  read-only* on the repository); lode then downloads through the release-asset API.
- Name assets `<app>-<os>-<arch>.<ext>`; each operator sets `[update].asset` to the exact
  filename for their host.

### Native manifest (`manifest = "https://.../manifest.json"`)

Host a `lode/v1` manifest whose per-version `assets[]` are keyed by `name`, plus the
assets at any HTTPS URLs:

```bash
lode-cli manifest "$f" --app myapp --version 1.5.0 --url "$URL" \
    --run ./myapp --exec ./myapp \
    --key private.key --into manifest.json   # upserts the asset by name, sets channels.latest; --run/--exec are optional
lode-cli manifest-sign --into manifest.json --key private.key   # optional: tamper-evidence over the catalog
```

Manifest shape + the per-asset field table live in
[source-adapters.md §6](source-adapters.md). `manifest-sign` is **optional**
(verify-if-present tamper-evidence); `channels.<c>.latest` rollback is caught
client-side by the downgrade floor, and a `pin` removes trust in the pointer
entirely.

Add `--expires-in 30d` to `manifest-sign` to stamp a signed expiry. lode refuses
the manifest once it passes, so a source serving an old copy cannot keep hosts on
an old version; operators make it mandatory with
`[trust].require_manifest = "fresh"`. Re-run `manifest-sign` on a schedule shorter
than the expiry (a CI cron job), or hosts stop taking updates when it lapses. A
lode older than this feature ignores the expiry and keeps verifying the same
manifest.

### lode-server registry (`lode-cli publish`)

A [lode-server](https://github.com/bkhq/lode-server) registry hosts the manifest and the
assets for you. One command releases a version: it creates the draft, uploads every
asset and publishes the version to a channel.

```bash
LODE_REGISTRY_TOKEN=pat_... lode-cli publish \
    --package <namespace>/<slug> --version 1.5.0 \
    --key-env LODE_SIGNING_KEY \
    dist/*
```

| Option | Meaning |
|---|---|
| `--registry <url>` | Base URL of the registry, including a base path if it has one. Default `https://lode.bk.io/u`. |
| `--package <ns>/<slug>` | The package to release. |
| `--version <v>` | The version id. |
| `--channel <name>` | Channel that will point at the version. Default `stable`. |
| `--notes <text>` | Release notes, stored when the draft is created. |
| `--key <file>` / `--key-env <VAR>` | ed25519 signing key, as `lode-cli sign` takes it. Without one the assets go up unsigned. |
| `--run <cmd>` / `--exec <cmd>` | Launch overrides published with every asset and bound into each signature. |
| `--allow-insecure` | Permit a plain-`http` registry that is not on loopback. |
| `<file>...` | The assets. Each is stored under its file name, which is what `[update].asset` selects. |

- The token is read from `LODE_REGISTRY_TOKEN` only and needs a `write` grant on the package.
- With a key, its public half is registered for the package on the first run. The
  registry verifies ed25519 only, so an ECDSA key is refused before anything is sent.
- Assets up to 95 MiB go up in one request; larger ones, up to 500 MiB, as a multipart
  upload in the part size the registry dictates.
- Re-running after a failure is safe: the run continues into the draft and an asset
  uploaded again replaces the one of the same name. A published version is immutable.
- Progress goes to stdout, errors to stderr with the HTTP status and the registry's
  error code. The exit status is 0 only when the version is published.

Operators then point lode at the package's read URL:
`manifest = "https://lode.bk.io/r/<namespace>/<slug>/manifest.json"`.

### Signing model (all sources)

- The artifact signature binds **`name` (filename) / `version` / `sha256` / `run` / `exec`**. `format` is derived from the filename extension (`.tar.gz`/`.tgz` → tar.gz, `.gz` → gz, `.zip` → zip, else raw). `run`/`exec` are bound into the signature when present (empty string otherwise) — a tampered catalog cannot inject malicious launch commands under `require_signature = auto` (with keys) or `enforce`.
- Under `require_signature = enforce`, every installed asset must carry a valid
  signature (github: a `<asset>.sig` sidecar asset or the `label`; native: the `sig`
  field or a `.sig` sidecar).
  `auto` is fail-closed once any trusted key is configured; without keys it installs
  **UNVERIFIED** with a warning.

### Delegated signing (root key offline)

Keep the key operators trust (the root) offline and sign releases with a short-lived
sub-key instead. Operators configure only the root in `trusted_keys`.

```sh
lode-cli keygen --out root                  # once; keep root.key offline
lode-cli keygen --out ci                    # the sub-key CI signs with
# offline, with the root key: let `ci` sign for myapp for 30 days
lode-cli delegate --app myapp --key root.key --sub ci.pub --valid-for 30d --out delegation.json
# CI: sign as usual with ci.key, then embed the delegation
lode-cli manifest dist/myapp.tar.gz --app myapp --version 1.5.0 --url https://… \
  --key ci.key --delegation delegation.json --into manifest.json
lode-cli manifest-sign --into manifest.json --key ci.key --delegation delegation.json --expires-in 7d
```

- GitHub Releases: upload `delegation.json` as the release asset
  **`lode-delegation.json`**; sign the assets with the sub-key (`<asset>.sig`).
- Renew the delegation before `not_after` (e.g. 30-day delegations renewed weekly);
  a manifest whose delegation expired no longer installs.
- Revoke a leaked sub-key: `lode-cli revoke --key root.key --list revocations.json
  --add <sub_key_id>` (or sign with a still-valid sub-key and its `--delegation`), publish
  the file, and point operators' `[trust].revocation_url` at it. The list is optional;
  without it a leaked sub-key stays usable until its delegation expires.
- Hosts on lode 0.0.3 or older reject sub-key signatures; upgrade them first
  (`lode-cli status` lists `features: delegation, revocation`).

### Resources (`[[resource]]`)

A resource is a native `lode/v1` manifest of its own, published under the identity
**`<app>/<name>`** — the manifest `name`, the `--app` every signature binds, and the
`--app` of its delegation. Sign it with the same keys as the app:

```sh
lode-cli manifest dist/settings.tar.gz --app myapp/config --version 3 \
  --url https://releases.example.com/myapp/config/3/settings.tar.gz \
  --key ci.key --delegation config-delegation.json --into config.json
lode-cli manifest-sign --into config.json --key ci.key --delegation config-delegation.json
```

Operators add it to `lode.toml` (`[[resource]]` with `name`, `manifest`, `asset`; see
[`lode.example.toml`](lode.example.toml)). Resource assets carry no `run`/`exec`.
Delegations name one identity, so a sub-key for `myapp` cannot sign `myapp/config`
(issue one per identity). Version strings are free-form; `auto` only applies a
provable semver upgrade.

### Checklist

- [ ] each host's `[update].asset` names the exact asset filename for its platform.
- [ ] `sha256` is of the raw file; `sig` is over `name/version/sha256/run/exec` (the §1 message) with a trusted `key_id`.
- [ ] github: signature uploaded as the **`<asset>.sig`** sidecar asset (or set as the `label`), made over the tag minus its leading `v`. native: `sig` inline or a `.sig` sidecar, and the catalog re-signed (`manifest-sign`) after the final edit.
- [ ] `channels.<c>.latest` points at a real version (native), or tag/latest resolves (github).
- [ ] private key offline; operators hold only the public `trusted_keys` with `require_signature = enforce`.

---

## Data directories & persistence

Two kinds of directory, named so they never collide.

**lode-provided** — lode injects these; your app reads them:

| env | what it is | lifetime |
|---|---|---|
| `LODE_DIR` | lode's own dir — `lode.toml`, `versions/<ver>/`, `state.json` (+`.lock`), `lode.pid`, `runtime/`, `downloads/` | **persistent** — give it a volume (`-v lode-data:/srv/lode`) |
| `LODE_WORKDIR` | the app's run dir under lode (its cwd; default = the active version dir) | **rotates per version** — pruned by `keep_versions` |

(The `lode` binary itself ships in the image and holds no state. Operator config: `[global].dir` / `--dir` sets `LODE_DIR`; the child's cwd defaults to the version dir and is overridable via `[command].workdir` in `lode.toml` (no CLI flag) — lode then injects the resolved dir as `LODE_WORKDIR`.)

**app-implemented** — your app's own directory convention, so the *same binary works with or without lode*. **lode never reads or sets these — it passes them through to the child untouched** (set them via the host env / `-e` / the `[env]` table); the resolution below is entirely your app's (or the SDK's) doing:

| env | what it is |
|---|---|
| `ROOT_DIR` | your app's root/run dir — the only one you need standalone |
| `DATA_DIR` | your app's persistent data dir |

Resolve your data dir as **`DATA_DIR` > `LODE_DIR` > `ROOT_DIR`**, so you only ever *need* `ROOT_DIR`:

- standalone, set just `ROOT_DIR` and everything falls back to it;
- under lode, `LODE_DIR` (a persistent volume) is used automatically;
- set `DATA_DIR` to override (e.g. a separate volume).

```ts
const dataDir = process.env.DATA_DIR ?? process.env.LODE_DIR ?? process.env.ROOT_DIR;
```

The SDKs expose this directly — `dataDir()` (the resolution above), plus `rootDir()` / `lodeDir()` / `workdir()`.

> **Don't write persistent data to the cwd.** `LODE_WORKDIR` defaults to the per-version directory, which lode replaces on every update and prunes by `keep_versions`. Keep app state under your resolved `DATA_DIR` (a persistent location), never the version dir.
