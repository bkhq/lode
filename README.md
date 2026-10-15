# lode

**English** · [中文](README.zh-CN.md)

> A universal **“verify · launch · update”** loader: one small static Rust binary that
> verifies a packaged app (integrity **and** publisher identity), launches it, supervises
> it, and hot-updates it. Bake it into a generic image once — switching apps is just a
> different manifest, never an image rebuild.

- **Image:** `docker.io/bkhq/lode` ([Docker Hub](https://hub.docker.com/r/bkhq/lode))
- **Binaries:** Linux (x86_64 / aarch64, musl-static), macOS (x86_64 / arm64) and Windows (x86_64) — [Releases](https://github.com/bkhq/lode/releases); upgrade in place with `lode-cli self-update`
- **Platforms:** Linux, macOS and Windows. On Windows the app runs in a Job Object and is asked to stop with `CTRL_BREAK`; signal forwarding is Unix only. → [Operations](docs/operations.md)

## Start here — by role

| You are… | You want to… | Go to |
|---|---|---|
| **Operator** | run & keep an app updated in a container | [Quick start](#quick-start) + [`lode.example.toml`](docs/lode.example.toml) + [Operations](docs/operations.md) |
| **App author** | make your app updatable by lode | [Integration §2 — the app contract](docs/integration.md) |
| **Publisher** | package, sign & publish a release | [Integration §3 — publish versions](docs/integration.md) |
| **Curious** | understand the design | [Architecture](docs/architecture.md) |

The [Integration guide](docs/integration.md) covers the whole chain — configure (`lode.toml`) → run (`state.json`) → publish (`manifest.json`).

Full doc index (bilingual): [`docs/`](docs/README.md). Working examples:
[`tests/apps`](tests/apps) (a Rust + a Bun server) and [`tests/compose`](tests/compose) (live update/rollback).

## Quick start

Point lode at a signed manifest and run the generic image. By default lode reads
`/srv/lode/lode.toml` and keeps its state under `/srv/lode`:

```bash
docker run --rm \
  -v "$PWD/lode.toml:/srv/lode/lode.toml:ro" \
  -e LODE_TRUSTED_KEYS="<key_id>:<base64-pubkey>" \
  docker.io/bkhq/lode:latest
```

A minimal `lode.toml` (see [`docs/lode.example.toml`](docs/lode.example.toml) for all options):

```toml
[global]
app = "myapp"
[update]
manifest = "https://releases.example.com/myapp/manifest.json"   # or: github = "owner/repo"
asset    = "myapp-linux-x86_64.tar.gz"                          # the exact asset filename this host installs
policy   = "auto"                                               # off | check | auto
[command]
run = "./myapp"                                                # how to launch the app (literal command, cwd = version dir)
[trust]
require_signature = "enforce"
```

> If `/srv/lode/lode.toml` is missing on first run, lode scaffolds a starter there and tells
> you to fill in the source. Override the base dir with `LODE_DIR`. No config file needed
> if you pass `--manifest`/`--github` (or `LODE_*`) instead.

To build your own app image, layer lode onto any base:

```dockerfile
FROM oven/bun:1                       # or any runtime your app needs
COPY --from=docker.io/bkhq/lode:latest /usr/bin/lode /usr/bin/lode
ENTRYPOINT ["/usr/bin/lode"]
```

## How it works

```
generic image           ┌─────────────────────────────────────┐
zzci/ubase         ────► │  lode  (static Rust binary)         │
                         └───────────────────┬─────────────────┘
                                             │ lode.toml + env + CLI
                                             ▼
   [update].manifest ──HTTPS(+headers)──► manifest.json  (channels → versions → assets[name])
                                             │  (remote; never stored locally)
                      pick asset by filename ─┤── download → verify sha256 + signature
                                             ▼
                    $LODE_DIR/versions/<ver>  ──(atomic rename)──► current
                                             │
                                             ▼
              lode            → runs `run`  (supervised service: auto-update + rollback)
              lode <args…>    → runs `exec` + <args>  (one-shot CLI passthrough)
```

## Two binaries, one file

lode is a **multi-call binary**. As `lode` it is the loader with **no subcommands** — arguments
are forwarded to the app. One caveat: lode parses its own flags first (`--version`, `--help` and
the `LODE_*` global options), so a leading app argument that matches one of them is consumed by
lode. Use `lode -- <args…>` to forward flag-like arguments verbatim. As **`lode-cli`** (a symlink
shipped alongside it) it is the operator/publisher toolkit.

| Invocation | Does |
|---|---|
| `lode` | start & supervise the app (`[command].run`); auto-update per policy |
| `lode <args…>` / `lode -- <args…>` | passthrough: run `[command].exec` + `<args>` (e.g. `lode run db:init`); use `--` when an arg collides with a lode flag |
| `lode-cli status` / `update` / `rollback` / `restart` / `versions` / `pull` / `seed` | manage a running instance (via `state.json`); `seed` installs a local version offline for dev/testing |
| `lode-cli self-update` | replace this lode binary with the newest lode release (verified against the release key built into the binary); restart lode to run it |
| `lode-cli keygen` / `sign` / `verify` / `manifest` / `manifest-sign` / `delegate` / `revoke` / `publish` / `init` | publisher/operator tools |

## Three files

- **`lode.toml`** — local TOML; the operator's config (how to fetch & run). The app never writes it. → [`docs/lode.example.toml`](docs/lode.example.toml)
- **`state.json`** — local JSON; runtime comms. lode writes status; the app writes requests (`target`/`restart_nonce`/`check_nonce`/`ready`). → [Integration §2](docs/integration.md)
- **`manifest.json`** — remote JSON; the signed version catalog (never stored locally). → [`docs/manifest.example.json`](docs/manifest.example.json)

## Key behaviors

- **Update** `[update].policy = off | check | auto`; source is either `manifest` (native `lode/v1` JSON) **or** `github = "owner/repo"` (Releases).
- **Rollback** — a new version that exits within `health_grace` is reverted to the last known-good (single-strike).
- **Restart** `[supervise].restart = off | on-failure | always` — `on-failure` (default, keep-alive) retries a failing app `restart_max` times then **pauses** (lode stays alive, never crash-looping the container); `off` opts back into mirroring the child; lode-initiated update/rollback/restart always relaunch.
- **Trust** — `sha256` + `ed25519` (or ECDSA P-256 / P-384, tagged on the key); set `[trust].trusted_keys` + `require_signature = off | auto | enforce`. Note: verification defaults to `auto` (enforced only when trusted keys are configured; without keys a manifest `run`/`exec` override is refused) — set `require_signature = "enforce"` for production (the starter config pins it). Trust settings, the update source and the runtime download are fixed for the life of the process — a config reload never changes them. The root key can stay offline: it delegates signing to short-lived sub-keys, with an optional revocation list (`[trust].revocation_url`). Signing is the publisher's job — see [Integration §3](docs/integration.md).
- **Resources** — `[[resource]]` distributes config files, data and other bundles under the same trust model, published as `<app>/<name>`: installed before the app starts, handed to it as `LODE_RESOURCE_<NAME>`, updated per their own `policy` (the app notified by `SIGHUP` or a restart), and pulled on demand via `lode-cli pull` or the SDK. → [Architecture §5](docs/architecture.md)
- **Private sources** — `[http].headers` (with `${ENV}` expansion) authenticate the manifest and same-origin asset fetches; other hosts receive them only when listed in `[http].credential_hosts`.
- **Publishing** — sign and attach assets to GitHub Releases, host a native manifest, or release to a [lode-server](https://github.com/bkhq/lode-server) registry with `lode-cli publish` (default `https://lode.bk.io/u`). → [Integration §3](docs/integration.md)

## Use as a library

lode is three crates: **`lode-core`** (clap-free, signal-free — config, manifest
resolution, verified download/install, the `Engine` facade), **`lode-supervisor`**
(the supervise loop + readiness/stop handshakes, over `lode-core`), and **`lode`**
(the binary: clap + authoring). Embed either library without inheriting the CLI or
lode's process-global side effects — those are opt-in via `InitOptions`.

The crates are **not published on crates.io** (the `lode` / `lode-core` names there
belong to unrelated projects) — depend on a release tag of this repository:

```toml
[dependencies]
lode-core = { git = "https://github.com/bkhq/lode", tag = "v0.1.0" }        # config + Engine (no clap, no signals)
lode-supervisor = { git = "https://github.com/bkhq/lode", tag = "v0.1.0" }  # + the supervise loop, driven by an injected SignalSource
```

The guide is [docs/embedding.md](docs/embedding.md): a program that updates its own
executable (`SelfUpdate`), and a host that supervises another app (`Engine` +
`serve_embedded`). Both are complete, runnable programs in
[`examples/embed`](examples/embed).

```bash
cargo run -p lode-core --example engine        # config in code + read-only Engine
cargo run -p lode-supervisor --example embedded # host-owned signals, no subreaper/flock
```

## Build from source

```bash
cargo build --profile dist --target x86_64-unknown-linux-musl    # release static binary
cargo fmt --all -- --check && cargo clippy --workspace --all-targets   # lint
cargo nextest run --workspace && cargo test --doc --workspace          # tests
cargo build --bins && cd tests && bun install && LODE_BIN=../target/debug/lode bun test --timeout 30000 src/   # e2e
```

The toolchain is pinned in `rust-toolchain.toml`; the minimum supported Rust version is 1.96.

## License

MIT
