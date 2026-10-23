# Embedding lode in a program

**English** · [中文](embedding.zh-CN.md)

Most users run the `lode` binary. This guide is for the other case: a Rust
program that links lode as a library. There are two things to embed, and they
are independent:

| You want | Use | Crates |
|---|---|---|
| Your program to replace **its own executable** with a signed release | [`SelfUpdate`](#2-update-your-own-executable) | `lode-core` |
| Your program to **run and update another app** as a supervised child | [`Engine` + `serve_embedded`](#3-supervise-another-app) | `lode-core`, `lode-supervisor` |

Both are shown as complete, runnable programs in
[`examples/embed`](../examples/embed).

## 1. Setup

The crates are not on crates.io (the names there belong to unrelated
projects). Depend on a release tag:

```toml
[dependencies]
lode-core       = { git = "https://github.com/bkhq/lode", tag = "v0.2.1" }
lode-supervisor = { git = "https://github.com/bkhq/lode", tag = "v0.2.1" }   # only to supervise a child
```

### Process-wide effects are opt-in

The `lode` binary installs a TLS crypto provider, a tracing subscriber, a panic
hook and core-dump suppression. A library user gets none of them unless it
asks:

```rust
lode_core::InitOptions::new()
    .crypto_provider(true)     // needed before the first HTTPS fetch
    .install();
```

| Option | Effect | Take it when |
|---|---|---|
| `crypto_provider(true)` | Installs the process-wide rustls provider | Always, unless your program installs one itself. Without a provider the first HTTPS request fails. |
| `log_level("info")` / `logging(Some(..))` | Installs lode's global `tracing` subscriber | Your program has no subscriber of its own. With one, leave it off: lode's events go to yours. |
| `panic_hook(true)` | Logs panics through `tracing` | You want lode's hook. |
| `suppress_core_dumps(true)` | Sets the core-dump limit to zero | Your process handles secrets and should not dump them. |

`install()` is idempotent.

### Errors

`lode-core` returns `lode_core::Error`, an enum whose variants name the
subsystem (`Config`, `Http`, `Manifest`, `Verify`, …) and carry a message.
`SelfUpdate` has its own, finer enum; see
[Errors](#errors-and-what-they-leave-behind).

## 2. Update your own executable

`lode_core::self_update::SelfUpdate` downloads a release of your program,
verifies it, and puts it in the place of the running executable. The process
keeps running the old code until it is restarted; when to restart is yours to
decide.

```rust
use lode_core::RequireManifest;
use lode_core::self_update::{self, Outcome, SelfUpdate, Source};

// Once, at startup: after an update the running file is no longer at its path.
let exe = self_update::current_exe()?;
self_update::remove_replaced(&exe);          // Windows: what the last update left
lode_core::InitOptions::new().crypto_provider(true).install();

let update = SelfUpdate::new("myapp", Source::GitHub("owner/myapp".into()), env!("CARGO_PKG_VERSION"))
    .asset("myapp-linux-x64.tar.gz")
    .trusted_keys(["<key_id>:<base64-public-key>"])
    .exe(exe);

if let Outcome::Installed { version, .. } = update.apply()? {
    println!("installed {version}; restart to run it");
}
```

### What you configure

| Method | Required | Meaning |
|---|---|---|
| `SelfUpdate::new(app, source, current_version)` | yes | `app` is the name the release is signed for (see [Publishing](#publishing-a-release-your-program-can-install)). `current_version` is the running version, normally `env!("CARGO_PKG_VERSION")`. |
| `.asset(name)` | yes | The release file for this platform. Its extension selects the format: `.tar.gz` / `.tgz`, `.zip`, `.gz`, or a raw file. |
| `.trusted_keys([..])` | yes | The `<key_id>:<public key>` entries a release must be signed by. **Compile them in.** A key read from a file or a setting can be changed by whoever can edit it. |
| `.binary(name)` | no | The executable's name inside the asset. Default: `app`, plus `.exe` on Windows. |
| `.exe(path)` | no | The file to replace. Default: the running executable at the time of the call. A long-running program resolves it once at startup and passes it. |
| `.alias(path)` | no | A second copy of the executable to replace as well (Windows, where a second name is a copy and not a symlink). Missing paths are skipped. |
| `.version(id)` | no | Install exactly this version instead of the channel's latest. This is also the only way to downgrade. |
| `.channel(name)` | no | Default `stable`. With a GitHub source any other channel follows the newest pre-release. |
| `.header("Name: value")` | no | A request header for a private source. `${VAR}` is filled in from the environment when the request is made. Sent to the source's own hosts only. |
| `.require_manifest(policy)` | no | `RequireManifest::Signed` or `Fresh`; see [Freeze attacks](#freeze-attacks-and-require_manifest). |

### Sources

| Source | What it reads | Notes |
|---|---|---|
| `Source::GitHub("owner/repo")` | The GitHub Releases API | The version is the release tag without a leading `v`. The signature is a `<asset>.sig` release asset. Unauthenticated calls are limited to 60 an hour per IP; pass a token with `.header("Authorization: Bearer ${GITHUB_TOKEN}")` if that is not enough. |
| `Source::Manifest(url)` | A `lode/v1` manifest | Your own server or a [lode-server](https://github.com/bkhq/lode-server) registry. Can carry channels, notes, a publication time and a manifest signature. See [source-adapters.md](source-adapters.md). |

Plain `http` is refused except on loopback.

### The three steps

`apply()` is `fetch()` followed by `install()`. Call them yourself when you
need what is in between.

| Call | Does | Touches the installation |
|---|---|---|
| `check()` | Reads the source and returns the release an update would install — `Available { version, entry }` — or `None`. | no |
| `fetch()` | Downloads the asset, verifies its digest and signature, unpacks it into a scratch directory and returns a `Fetched`, or `None` when up to date. | no |
| `install(&fetched)` | Copies the new binary beside the executable, runs it with `--version` and requires it to print the release's version, then puts it in the executable's place. | yes |

**Your program must answer `--version`** with output that contains the version
as a whitespace-separated word (`myapp 1.4.2` is fine). `install` uses it to
catch a release built for another platform, or a mislabelled one, before it
replaces a binary that works.

`check()` gives you the manifest entry, so a `--check` command can show what is
new:

```rust
if let Some(available) = update.check()? {
    println!("{} ({})", available.version, available.entry.published_at.as_deref().unwrap_or("?"));
    println!("{}", available.entry.notes.as_deref().unwrap_or(""));
}
```

`notes` and `published_at` are covered by no signature. Treat them as untrusted
text: strip control characters before printing to a terminal, escape them in
HTML.

### Files that ship with the executable

A `Fetched` holds the whole verified asset, unpacked, until it is dropped:

```rust
if let Some(fetched) = update.fetch()? {
    // e.g. a DLL that has to sit next to the executable
    install_companions(fetched.dir())?;
    update.install(&fetched)?;
}
```

| Accessor | Gives |
|---|---|
| `fetched.dir()` | The directory the asset was unpacked into |
| `fetched.binary()` | The new executable inside it |
| `fetched.version()` / `fetched.entry()` | The version and its manifest entry |

Dropping the `Fetched` deletes the scratch directory, so copy out what you need
first.

A program that prefers to replace its executable itself stops after `fetch()`
and never calls `install()`. It then owns the rest: checking that the new
binary starts, replacing a running executable on Windows, and recovery when
that fails. Do not run the binary from the scratch directory to check it — a
system temp directory may be mounted `noexec`; copy it beside the executable
first, as `install()` does.

### What is verified

Every update, with no way to turn it off:

1. **Digest.** The download's sha256 must be the one the source states.
2. **Signature.** That digest, the app name, the asset file name and the
   version must be signed by one of your `trusted_keys`. An unsigned release is
   refused.
3. **Order.** Following the channel's latest only moves forward: a latest that
   is older than `current_version` is refused, and so is one that cannot be
   compared (the two are not both semver). `.version(id)` installs any version
   deliberately.
4. **It runs.** See `--version` above.

The source cannot make your program install something your key did not sign,
under a name or version your key did not sign it for.

### Freeze attacks and `require_manifest`

The checks above do not stop a source from simply answering with an old
manifest forever: every signature in it is valid, and your program stays on an
old version. Per-asset signatures cannot detect that. A signed manifest with a
signed expiry can:

```rust
.require_manifest(RequireManifest::Fresh)
```

| Policy | The manifest must |
|---|---|
| `Off` (default) | Nothing. A signature or expiry is verified when present, never required. |
| `Signed` | Carry a valid signature over its channels and asset digests. |
| `Fresh` | As `Signed`, and carry a signed expiry that has not passed. |

This needs `Source::Manifest` and a publisher who signs the manifest
(`lode-cli manifest-sign --expires-in 30d`) and signs it again before it
expires. A GitHub source has no manifest to sign: combining it with `Signed` or
`Fresh` is a configuration error.

The expiry is compared with the local clock, with no tolerance: a manifest is
refused from the second it expires. Publishers should re-sign well ahead.

### Windows

A running executable cannot be overwritten or deleted on Windows, only
renamed. `install()` therefore renames the old file aside (a hidden
`.<name>.old-…` beside it), moves the new one into place, and puts the old one
back if that fails.

- The old file stays until no process runs it. Call
  `self_update::remove_replaced(&exe)` at startup to delete it.
- If your program is installed under two names, name the other one with
  `.alias(path)`; there is no symlink to follow.
- A directory that needs administrator rights (`Program Files`) makes the
  rename fail for an ordinary user. The executable is left as it was.

On Unix the replacement is one atomic `rename`, and the running process keeps
the old file open.

### Errors and what they leave behind

Every `SelfUpdate` method returns `SelfUpdateError`. Match on it to choose a
message; it converts into `lode_core::Error` with `?` when you do not care.

| Variant | Meaning | Installation |
|---|---|---|
| `Config` | No trusted key, no asset, an impossible combination | untouched |
| `Source` | The source could not be reached, or its answer could not be read | untouched |
| `Verify` | A digest or signature did not verify; the manifest is unsigned or expired under `require_manifest` | untouched |
| `Local` | Scratch space or unpacking failed | untouched |
| `Older { latest, current }` | The latest release is older than the running version | untouched |
| `Unordered { latest, current }` | The two versions are not both semver | untouched |
| `MissingBinary { asset, binary }` | The asset does not hold the executable | untouched |
| `NotRunnable { path, source }` | The new binary could not be started | untouched |
| `WrongVersion { expected, reported }` | The new binary reports another version | untouched |
| `Replace { path, source }` | The executable could not be replaced | untouched |
| `Alias { path, source }` | The executable was replaced, a copy of it was not | **two versions installed** |
| `Displaced { path, moved_to, source }` | Windows: the old executable was moved aside, the new one could not take its place, and the old one could not be moved back | **nothing at `path`; the old one is at `moved_to`** |

`error.is_untouched()` is `false` for the last two only. For `Alias`, retry the
update. For `Displaced`, rename `moved_to` back to `path`.

The enum is `#[non_exhaustive]`: keep a fallback arm.

### Publishing a release your program can install

The signature names the app, the asset file name and the version, so all three
have to match what the program asks for.

```bash
lode-cli keygen --out publisher            # once; compile the printed trustedKeys entry into the program
lode-cli sign myapp-linux-x64.tar.gz --app myapp --version 1.4.2 --key publisher.key
```

- **GitHub Releases**: upload the asset and a `myapp-linux-x64.tar.gz.sig` file
  holding the printed `sig`. The release tag is `v1.4.2` or `1.4.2`.
- **A manifest**: `lode-cli manifest … --into manifest.json`, then
  `lode-cli manifest-sign --into manifest.json --key publisher.key --expires-in 30d`
  if programs set `require_manifest`.
- **A lode-server registry**: `lode-cli publish`.

`--app` must be the `app` given to `SelfUpdate::new`, the file name must be the
`.asset(...)`, and the version must be what the new binary prints for
`--version`. Details: [source-adapters.md](source-adapters.md),
[integration.md](integration.md).

## 3. Supervise another app

Here your program is the host and lode's loop runs inside it: it starts the app
as a child, restarts it with backoff, applies updates, and rolls back a version
that does not stay up. This is what the `lode` binary does, minus the parts a
host wants to keep for itself.

```rust
use lode_core::{Config, Engine, InitOptions};
use lode_supervisor::{SuperviseOptions, serve_embedded, signal_channel};

InitOptions::new().crypto_provider(true).install();
let cfg = Config::from_toml(include_str!("worker.toml"))?;

let (signals, mut source) = signal_channel();
let supervised = cfg.clone();
let supervisor = std::thread::spawn(move || {
    serve_embedded(supervised, &mut source, SuperviseOptions::host_owned())
});

let engine = Engine::new(cfg);
engine.restart()?;                 // ask the loop to restart the app
signals.send(15)?;                 // SIGTERM: stop it gracefully
let exit = supervisor.join().expect("supervise thread")?;
```

### Configuration

A `Config` is what a `lode.toml` holds. Build it from TOML text or with the
builder:

```rust
let cfg = Config::from_toml(toml_text)?;                       // every key of lode.toml

let cfg = Config::builder()
    .app("worker")
    .dir("/var/lib/myhost/worker")
    .manifest("https://lode.bk.io/r/<namespace>/<slug>/manifest.json")
    .asset("worker-linux-x64.tar.gz")
    .run("./worker")
    .build()?;
```

The builder covers the source, the asset, the channel, the policy, a pin, the
launch commands and `[report]`. For `[trust]`, `[http]`, `[supervise]` or
`[runtime]` use `from_toml`; the keys are those of
[`lode.example.toml`](lode.example.toml). Neither reads a file or the `LODE_*`
environment.

### Status reporting

With a `[report]` endpoint the loop pushes the app's status over OTLP/HTTP,
exactly as the `lode` binary does: metrics (`lode.app.status`, restarts,
uptime, available and last good version, resource versions) at start, every
`interval` and once more on shutdown, and the `lode.*` lifecycle events
(`started`, `updated`, `rolled_back`, `paused`, `check.failed`, `stopping`).
`service.name` is the app, `process.pid` the host's own process.

```rust
let cfg = Config::builder()
    .app("worker")
    .manifest("https://lode.bk.io/r/<namespace>/<slug>/manifest.json")
    .report_endpoint("https://lode.bk.io/r/otlp")   // any OTLP/HTTP base
    .report_header("Authorization: Bearer ${LODE_REGISTRY_TOKEN}")
    .report_attribute("deployment.environment.name", "prod")
    .report_interval(300)                           // the default
    .build()?;
```

The headers go to the report endpoint only; `${ENV}` is expanded per request.
The endpoint must be https (or loopback http) unless `[http].allow_insecure`.
Reporting runs on its own thread and never blocks the loop; `[report]` is fixed
for the life of the loop, like `[trust]`. See
[operations.md](operations.md#status-reporting) for what each export carries.

### Who owns what

`serve_embedded(cfg, &mut signals, options)` blocks until the app has stopped,
so it gets a thread.

| | `SuperviseOptions::host_owned()` | `SuperviseOptions::owned()` |
|---|---|---|
| Signal handlers | The host's. It feeds the loop through `signal_channel()`. | lode's: pass an `OwnedSignalSource`. |
| Child subreaper | not set | set (for a process that is PID 1) |
| Single-instance lock (`lode.pid`) | not taken | taken |

With `host_owned()` lode changes nothing process-wide. The channel is the only
way signals reach the loop: `SIGTERM` (15), `SIGINT` (2) or `SIGQUIT` (3) stop
the app gracefully; a signal in `[signals].forward` (a standard set by default)
is passed on to the app; anything else is ignored.

### Driving the loop

`Engine` is the facade `lode-cli` is built on. Its calls do not talk to the
loop directly: they write a request into `state.json` in the data directory,
and the loop picks it up within about a second. A second process can make the
same calls.

| Call | Does |
|---|---|
| `check()` | Reads the source: the version the channel points at. Nothing is downloaded. |
| `install(None)` / `install(Some(v))` | Download, verify, install and activate the latest or `v`. |
| `rollback(None)` / `rollback(Some(v))` | Switch to the last good version, or to `v`. |
| `restart()` | Restart the app. |
| `resolve_target()` | Which installed version would start now. |
| `status()` / `versions()` | Print what `lode-cli status` / `versions` print, to stdout. |

To read the state instead of printing it, use
`lode_core::state::read(&dir.join("state.json"))`: `current`, `last_good`,
`available`, `status`, `pid`, `last_error` and the rollout `history`.

The app can take part in its own updates — report readiness, ask for a check,
pace the cut-over — through the same file; see
[integration.md](integration.md) and the SDKs in [`sdks/`](../sdks).

### Releases for the supervised app

Point the config at a source and require signatures:

```toml
[update]
manifest = "https://lode.bk.io/r/<namespace>/<slug>/manifest.json"   # or: github = "owner/repo"
asset    = "worker-linux-x64.tar.gz"
policy   = "auto"                    # off | check | auto

[trust]
require_signature = "enforce"
trusted_keys = ["<key_id>:<base64-public-key>"]
```

With nothing installed the loop downloads the channel's latest before it
starts the app. For tests and demos, `lode_core::install::seed_local` installs
a local file as a version without any source.

## 4. Platforms

| | Linux, macOS | Windows |
|---|---|---|
| `SelfUpdate` | yes | yes; see [Windows](#windows) |
| `Engine` | yes | yes |
| Supervise loop | Process groups and signals | A Job Object and console events; there are no signals to forward |

## 5. The examples

[`examples/embed`](../examples/embed) is a small crate of its own with two
programs, both offline:

```bash
cargo run --manifest-path examples/embed/Cargo.toml --bin self_update
cargo run --manifest-path examples/embed/Cargo.toml --bin supervise
```

- `self_update` builds and signs a release, serves it on loopback, then walks
  through `check`, `fetch`, a companion file, `install` and a refused update.
- `supervise` runs a worker under the loop, switches its version, restarts it
  and stops it.
