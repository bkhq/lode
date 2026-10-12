# Embedding lode: two host programs

Most users run the `lode` binary. These two programs show the alternative: a
Rust program that links lode as a library. The guide that explains every call
is [`docs/embedding.md`](../../docs/embedding.md); this page says what the demos
do and how to run them.

Both are offline and need nothing but a Rust toolchain. They live in a crate of
their own, outside lode's workspace.

| Program | Shows | Crates |
|---|---|---|
| [`self_update`](src/bin/self_update.rs) | A program replacing its own executable with a signed release | `lode-core` |
| [`supervise`](src/bin/supervise.rs) | A host running and updating another app as a supervised child | `lode-core`, `lode-supervisor` |

## `self_update`

```bash
cargo run --manifest-path examples/embed/Cargo.toml --bin self_update
```

The demo plays both sides. As the publisher it builds release 2.0.0 of `myapp`,
signs the asset and the manifest, and serves them on loopback. As the installed
1.0.0 it then does what a real program does:

| Step | API |
|---|---|
| Describe the update: source, asset, trusted key, manifest policy | `SelfUpdate::new(..).asset(..).trusted_keys(..).require_manifest(RequireManifest::Fresh).exe(..)` |
| Look for a release and show its notes | `check()` → `Available { version, entry }` |
| Download, verify and unpack it | `fetch()` → `Fetched` |
| Take a file that ships beside the executable | `fetched.dir()` |
| Prove the new binary runs, then replace the executable | `install(&fetched)` → `Outcome::Installed` |
| Tell a refused update from a broken installation | `match` on `SelfUpdateError`, `is_untouched()` |

```
installed:  myapp 1.0.0
available:  2.0.0 (published 2026-10-12T10:00:00Z): Faster startup.
companion:  plugin for 2.0.0
installed:  2.0.0 at /tmp/lode-demo-self-update-…/bin/myapp
now runs:   myapp 2.0.0
check:      None
wrong key:  refused (verify: manifest signature did not match any trusted key); installation untouched: true
```

What a real program does differently:

- It passes `lode_core::self_update::current_exe()`, resolved once at startup,
  to `.exe(..)`, and calls `self_update::remove_replaced(&exe)` there too.
- It compiles its trusted key in. The demo generates one because it is its own
  publisher.
- It calls `InitOptions::new().crypto_provider(true).install()` before the
  first fetch: its source is HTTPS.
- Its publisher signs with `lode-cli sign --app myapp` and
  `lode-cli manifest-sign --expires-in`, where the demo signs in process. The
  crates under "Only the self_update demo needs these" in `Cargo.toml` exist
  for that and are not needed by a real program.
- The stand-in program is a shell script, so this demo runs on Unix only. The
  API is the same on Windows.

## `supervise`

```bash
cargo run --manifest-path examples/embed/Cargo.toml --bin supervise
```

The host keeps control of its own process: its own signal handlers, its own
decisions about when to switch versions, lode's supervise loop on a thread.

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

It takes about ten seconds:

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

Press Ctrl-C at any point: the host's handler forwards it and the worker is
stopped gracefully.

The demo seeds its versions locally so that it runs anywhere. A real host
points the config at a release source, requires signatures, and drops the
`seed` calls; see
[Releases for the supervised app](../../docs/embedding.md#releases-for-the-supervised-app).

## In your own project

```toml
[dependencies]
lode-core       = { git = "https://github.com/bkhq/lode", tag = "v0.0.3" }
lode-supervisor = { git = "https://github.com/bkhq/lode", tag = "v0.0.3" }   # only to supervise a child
```
