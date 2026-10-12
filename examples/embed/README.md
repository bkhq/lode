# Embedding lode in a host program

Most users run the `lode` binary. This example shows the alternative: a program that
links lode as a library and keeps control of its own process.

Use it when the host already owns the process — it has its own signal handling,
runs other work beside the supervised app, or is itself PID 1 of something larger.

## What the host does

[`src/main.rs`](src/main.rs) is a complete host in about 130 lines:

| Step | API |
|---|---|
| Build the configuration in code | `Config::from_toml` (or `Config::builder()`) |
| Opt into lode's process-wide effects | `InitOptions` — here only logging |
| Install two local versions | `lode_core::install::seed_local` |
| Own the signals and forward a stop | `signal_channel()` + the host's own handlers |
| Run the supervise loop on a thread | `serve_embedded(cfg, &mut signals, SuperviseOptions::host_owned())` |
| Switch the worker to another version | `Engine::rollback(Some("1.0.0"))` |
| Restart the worker | `Engine::restart()` |
| Stop it and collect the exit status | send `SIGTERM` through the channel, join the thread |

`SuperviseOptions::host_owned()` means lode installs **no** signal handlers, does
**not** become a subreaper and takes **no** single-instance lock. Everything else is
the same loop the binary runs: restart with backoff, readiness, hot-update, rollback.

The `Engine` calls do not talk to the loop directly. They write a request into
`state.json`, exactly as `lode-cli` does, and the loop picks it up within about a
second. A host in another process could make the same calls.

## Run it

```bash
cargo run --manifest-path examples/embed/Cargo.toml
```

The demo is offline and takes about ten seconds:

```
INFO spawned child version="1.1.0" …
worker 1.1.0: started (LODE_ACTIVE_VERSION=1.1.0)
host: switching the worker to 1.0.0
INFO applying update target from="1.1.0" to="1.0.0"
worker 1.1.0: stopping
worker 1.0.0: started (LODE_ACTIVE_VERSION=1.0.0)
INFO update ready — committing as last_good version="1.0.0"
host: restarting the worker
worker 1.0.0: stopping
worker 1.0.0: started (LODE_ACTIVE_VERSION=1.0.0)
host: shutting down
worker 1.0.0: stopping
host: the worker stopped with ExitCode(unix_exit_status(0))
```

Press Ctrl-C at any point: the host's handler forwards it and the worker is stopped
gracefully.

## Using a real release source

The demo seeds its versions locally so that it runs anywhere. For a real app, point
the config at a source and let lode download and verify:

```toml
[update]
manifest = "https://lode.bk.io/r/<namespace>/<slug>/manifest.json"   # or: github = "owner/repo"
asset    = "worker-linux-x64.tar.gz"
policy   = "auto"

[trust]
require_signature = "enforce"
trusted_keys = ["<key_id>:<base64-public-key>"]
```

Then:

- call `InitOptions::new().crypto_provider(true)` before the first fetch, so rustls
  has its provider;
- drop the `seed` calls — with nothing installed, the loop bootstraps the channel's
  latest version;
- use `Engine::check()` to look for a newer version and `Engine::install(None)` to
  install it, or leave it to `policy = "auto"`.

## Updating the host binary itself

Everything above updates a worker that the host supervises. A host that ships as
one binary can also replace **itself** in place with
`lode_core::self_update::SelfUpdate`:

```rust
use lode_core::self_update::{Outcome, SelfUpdate, Source};

// Once, at startup: after an update the running file is reported as deleted.
let exe = lode_core::self_update::current_exe()?;

let update = SelfUpdate::new("myapp", Source::GitHub("owner/myapp".into()), env!("CARGO_PKG_VERSION"))
    .asset("myapp-linux-x64.tar.gz")
    .trusted_keys(["<key_id>:<base64-public-key>"])   // compiled in, not read from config
    .exe(exe);

if let Outcome::Installed { version, .. } = update.apply()? {
    // The process is still the old binary: restart or re-exec when convenient.
}
```

`check()` returns the version an update would install without downloading it.
Following the newest release needs semver versions on both sides: with any other
version id an update is refused unless `.version(...)` names it, because an id
that cannot be ordered could be an older release. A
release is always verified under `enforce`, the downloaded binary must answer
`--version` with the target version before it replaces anything, and there is no
automatic rollback — nothing outside the process watches the new version.

On Windows a running executable cannot be overwritten, so the old file is renamed
aside and stays until no process runs it. Call
`lode_core::self_update::remove_replaced(&exe)` at startup to delete it, and name
any second copy of the binary with `.alias(path)` so it is replaced as well.

## In your own project

The crates are not on crates.io. Depend on a release tag:

```toml
[dependencies]
lode-core       = { git = "https://github.com/bkhq/lode", tag = "v0.0.2" }
lode-supervisor = { git = "https://github.com/bkhq/lode", tag = "v0.0.2" }
```

The demo's worker is a shell script, so the demo itself runs on Unix. The same API works on Windows.
