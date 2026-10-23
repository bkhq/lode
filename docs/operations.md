# Operating lode

**English** · [中文](operations.zh-CN.md)

Installing lode, the `lode-cli` operator commands, logs, exit codes and common
problems. Configuration is covered in [`lode.example.toml`](lode.example.toml) and
[architecture §10](architecture.md); the app side in [integration](integration.md).

---

## 1. Install

### Container image

`docker.io/bkhq/lode` carries `/usr/bin/lode` and `/usr/bin/lode-cli`, with `lode` as
the entrypoint. Tags are release tags (`v0.0.1`) and `latest`. Each tag also has a
`-slim` variant (`v0.0.1-slim`, `latest-slim`) built on `debian:13-slim` instead of
`zzci/ubase`: glibc, a shell and `apt`, so the bun/node/deno runtimes still load, in
an image about a third of the size. It has no `ca-certificates`; lode does not need
them, an app that reads the system trust store does. A `-mini` variant
(`v0.0.1-mini`, `latest-mini`) is built on `gcr.io/distroless/static-debian13`, a few
MB in all: use it when the app is a static binary. It has CA certificates, tzdata
and a `nonroot` user but no libc and no shell, so a dynamically linked app or a
downloaded bun/node/deno runtime cannot start there. To add lode to your own image, copy the binary out of any of them:

```dockerfile
COPY --from=docker.io/bkhq/lode:latest /usr/bin/lode /usr/bin/lode
RUN ln -s lode /usr/bin/lode-cli
ENTRYPOINT ["/usr/bin/lode"]
```

### Binary

Each [release](https://github.com/bkhq/lode/releases) has one tarball per platform —
`lode-linux-x64`, `lode-linux-arm64`, `lode-darwin-x64`, `lode-darwin-arm64` — plus a
`.sig` beside each and a `SHA256SUMS` file. A tarball holds `lode` and the `lode-cli`
symlink.

```bash
base=https://github.com/bkhq/lode/releases/latest/download
curl -fsSLO "$base/lode-linux-x64.tar.gz" -O "$base/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf lode-linux-x64.tar.gz -C /usr/local/bin lode lode-cli
```

The Linux binaries are static (musl) and run on any distribution.

### Windows

`lode-windows-x64.zip` holds `lode.exe` and `lode-cli.exe` (the same program under two
names). Unpack both into a directory on `PATH`.

lode supervises and updates an app on Windows as it does on Unix. The differences:

| Area | On Windows |
|---|---|
| Stopping the app | lode sets `status` to `stopping` or `updating`, then sends **`CTRL_BREAK`** to the app's process group. After `stop_timeout` it terminates the app's whole process tree. Node receives it as `SIGBREAK`, Go as `os.Interrupt`; a program that ignores it is killed at the timeout. |
| Process tree | The app runs in a Job Object that is closed when lode exits, however it exits. Nothing the app started outlives lode. |
| Stopping lode | Ctrl-C, Ctrl-Break, closing the console, logoff and shutdown all stop the app gracefully first. |
| Signals | There is nothing to forward: `[signals]` has no effect. |
| `lode <args…>` | There is no `exec`: the app runs as a child and lode exits with its status. |
| `[runtime]` | The runtime binary is `<name>.exe`. |
| Data directory | Default `C:\ProgramData\lode`. There is no `current` link; `state.json` records the current version. |
| `lode-cli self-update` | Replaces both `lode.exe` and `lode-cli.exe`. A running executable cannot be overwritten, so the old files are renamed aside and deleted the next time lode starts. |
| Console | `CTRL_BREAK` needs lode and the app to share a console. Without one the app must watch `status` in `state.json`, or it is terminated at `stop_timeout`. |

### Upgrade lode itself

```bash
lode-cli self-update                     # newest release
lode-cli self-update --version v0.0.1    # a specific tag; also how to downgrade
```

`self-update` downloads the release asset for this host, verifies it against the
release keys **built into the running binary**, and replaces the executable in place.
It never reads the app's `lode.toml`. A running supervisor keeps the old binary until
lode is restarted. The directory holding the executable must be writable.

If the release was signed with a key the installed binary does not know (after a key
rotation), pass the new key explicitly: `--release-key '<key_id>:<base64>'`.

---

## 2. Commands

`lode` is the loader and has no subcommands; `lode-cli` is the toolkit. Both accept the
global options of [architecture §10](architecture.md) (`--dir`, `--config`, …) and the
matching `LODE_*` variables, so point `lode-cli` at the same data directory as the
running instance.

| Command | What it does |
|---|---|
| `lode` | Start and supervise the app (`[command].run`). Takes the single-instance lock. |
| `lode <args…>` | Run `[command].exec` + `<args>` in place of lode: no lock, no supervision. The exit status is the app's. Use `lode -- <args…>` when an argument looks like a lode option. |
| `lode-cli status` | Print the resolved configuration, a summary of the remote source and `state.json`. Works offline. |
| `lode-cli versions` | List installed versions; `*` marks the current one. |
| `lode-cli update [--version <v>]` | Download, verify and install the channel's latest version, or `<v>`. |
| `lode-cli rollback [--version <v>]` | Request a switch to the last good version, or `<v>`. |
| `lode-cli restart` | Request a restart of the app on its current version. |
| `lode-cli pull [--resource <name>] [--version <v>]` | Fetch, verify and activate `[[resource]]` bundles: all, or `<name>` at the channel latest or `<v>` (an older `<v>` rolls back). A running instance applies it and notifies the app. |
| `lode-cli self-update` | Replace the lode binary (see §1). |
| `lode-cli init [path]` | Write a starter `lode.toml` to `path`, or print it. |
| `lode-cli seed <file>` | Install a local file as a version, for development. See [local testing](dev-local-testing.md). |
| `lode-cli keygen` / `sign` / `verify` / `manifest` / `manifest-sign` / `publish` | Publisher commands. See [integration §3](integration.md). |

### How the management commands reach a running instance

`update`, `rollback` and `restart` do not signal the supervisor. They write a request
into `state.json`, which the supervisor polls about once a second:

- **`update`** installs the version first. With a supervisor running it then writes
  `target`, and the supervisor hot-updates. With none running it activates the version
  directly, so the next `lode` starts it.
- **`rollback`** writes `target`. It is local: nothing is downloaded, so the version
  must already be installed.
- **`restart`** increments `restart_nonce`. The supervisor re-reads `lode.toml` before
  relaunching, so this is also how an edited config is applied — except `[trust]`, the
  `[update]` source, the `[http]` credential scope and the `[runtime]` download, which
  are pinned for the life of the process (a reload that changes them is refused and
  logged; restart lode to apply them).

```console
$ lode-cli versions
lode versions (2 installed)
* 1.1.0
  1.0.0
$ lode-cli rollback --version 1.0.0
lode rollback: target set to 1.0.0
$ lode-cli restart
lode restart: requested (restart_nonce=1)
```

`status` ends with the contents of `state.json`. `status` there is one of `starting`,
`running`, `held`, `updating`, `rolling-back`, `stopping`, `stopped`, `error`:

```console
$ lode-cli status
…
[state] /srv/lode/state.json
{
  "current": "1.0.0",
  "last_good": "1.0.0",
  "status": "running",
  "pid": 2056087,
  "config_generation": 0,
  "checked_nonce": 0,
  "restart_nonce": 0,
  "check_nonce": 0,
  "hold": false
}
```

---

## 3. Logs

lode writes its log to **stdout**, one line per event, next to whatever the app prints
(the app inherits lode's stdout and stderr):

```
2026-10-10T13:13:12.412945Z  INFO spawned child version="1.0.0" pid=2056087 instance="2056085-k3x9q2w7m4p8zt6n"
2026-10-10T13:13:14.414835Z  INFO restart requested via state.json nonce=1
2026-10-10T13:13:16.468901Z  INFO termination signal received; stopping child
```

- Level: `[global].log_level`, `--log-level` or `LODE_LOG_LEVEL` — `trace`, `debug`,
  `info` (default), `warn`, `error`.
- Lines are coloured with ANSI codes; set `NO_COLOR=1` for plain text.
- A failed command logs `ERROR lode failed: <reason>` and exits non-zero.
  `lode-cli publish` is the exception: it reports on stderr as `publish: <reason>`.
- Header values, tokens and private keys are never logged; URLs are logged without
  their query string.

### Status reporting

To see a fleet's hosts in one place, set `[report]`: lode then pushes each host's status
(app version, run state, restarts, available update) and lifecycle events (start, stop,
update, rollback, pause) over OpenTelemetry OTLP/HTTP:

```toml
[report]
endpoint = "https://lode.bk.io/r/otlp"   # any OTLP/HTTP receiver
headers  = ["Authorization: Bearer ${LODE_REGISTRY_TOKEN}"]
# interval = 300
```

- A lode-server registry shows the hosts on the package's Observability tab; the token
  needs a grant on the package. An OpenTelemetry Collector or another OTLP backend takes
  the same block with its own endpoint and headers.
- `--report-endpoint` / `LODE_REPORT_ENDPOINT` and `--report-interval` /
  `LODE_REPORT_INTERVAL` set it without editing `lode.toml`.
- A failing receiver only logs `status reporting: …` warnings; supervision is not
  affected. `export partly rejected` means the receiver refused the data — for
  lode-server, usually a token without a grant on the package, or a `[global].app` that
  is not the package slug.
- Changing `[report]` takes a restart of lode.

---

## 4. Exit status

| Situation | Status |
|---|---|
| A `lode-cli` command succeeded | `0` |
| A command failed (bad config, lock held, fetch or verification error, …) | `1` |
| `lode <args…>` passthrough | the app's own status |
| Supervised app exited and lode follows it (`restart = off`, or a clean exit under `on-failure`) | the app's status; `128 + signal` when it was killed by a signal |
| lode received `SIGTERM` / `SIGINT` / `SIGQUIT` | the app's status after the graceful stop |

Under `restart = on-failure` (the default) and `always`, lode does **not** exit when the
app keeps failing: it pauses with `status = error` and stays alive.

---

## 5. Common problems

**`lock: another lode instance is already running (pid …)`**
Another supervisor holds this data directory. Use a different `--dir`, or stop the
other instance. A lock left by a killed lode is reclaimed automatically.

**The app is paused (`status = error`)**
The app failed `restart_max` times in a row, or a rollback target failed too. lode
logs `app failed to stay up — pausing` and waits. Fix the cause, then do one of:
edit `lode.toml` (lode reloads it and retries), run `lode-cli restart`, or install a
different version with `lode-cli update --version <v>`.

**`no trusted keys configured; asset is UNVERIFIED`**
`require_signature = auto` with no key: only the sha256 is checked. Set
`[trust].trusted_keys`, and `require_signature = "enforce"` in production.

**`delegation rejected: delegation expired (not_after=…, now=…)`**
The manifest is signed by a delegated sub-key whose delegation has expired, or the
host clock is wrong. The publisher renews the delegation (`lode-cli delegate`) and
re-signs; check the clock otherwise. `lode-cli status` shows the signing key and
when its delegation expires. `delegated key … is revoked` means the sub-key is on
the `[trust].revocation_url` list (merged into `$LODE_DIR/revocations.json`).

**`manifest: no update source configured`**
Neither `[update].manifest` nor `[update].github` is set. That is expected for a
seeded, offline data directory; `update` needs a source.

**`refusing non-HTTPS URL`**
Remote fetches must be HTTPS. Plain `http` is accepted for loopback hosts only, unless
`[http].allow_insecure = true`.

**`header references unset env var ${NAME}`**
A `${NAME}` in `[http].headers` is expanded on every request and must be set in lode's
environment.

**A new version was rolled back**
The new version exited within `health_grace`, or did not report ready within
`ready_timeout`. `state.json` records it in `history` with `result: "bad"` and the
reason in `last_error`.

**`lode.toml` was edited but nothing changed**
A running app is never restarted by an edit. lode increments `config_generation`;
apply the change with `lode-cli restart`.

**An app argument is swallowed by lode**
lode parses its own options first. Write `lode -- <args…>` to pass everything through.
