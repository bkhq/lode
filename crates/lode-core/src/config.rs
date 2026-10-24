//! Configuration: the resolved [`Config`] plus its loader.
//!
//! Precedence is `CLI > env (LODE_*) > lode.toml > default` (design §10). clap
//! folds env into each CLI field (every global arg carries `env = "LODE_…"`), so
//! [`merge`] sees just two layers — the CLI/env slot and the parsed TOML — and
//! the design's default table fills the rest. CLI-over-env within the first slot
//! is clap's contract; this module owns env/toml-over-default.
//!
//! Header values and trusted keys are stored verbatim and never expanded or
//! logged here; `${ENV}` expansion happens at fetch time in the http module.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::{Error, Result};

const DEFAULT_LOG_LEVEL: &str = "info";
const DEFAULT_APP: &str = "app";
/// Default base / run directory.
///
/// Holds `lode.toml`, `versions/`, `state.json`,
/// `lode.pid` and `runtime/`. Change the whole location with `--dir` /
/// `LODE_DIR` (config is then searched at `$LODE_DIR/lode.toml`).
#[cfg(not(windows))]
pub const DEFAULT_DIR: &str = "/srv/lode";
/// Default runtime/base directory on Windows.
#[cfg(windows)]
pub const DEFAULT_DIR: &str = r"C:\ProgramData\lode";

/// Minimal starter `lode.toml` scaffolded on first run when none exists (also
/// what `lode-cli init` writes).
///
/// Deliberately small — the complete documented
/// reference lives in `docs/lode.example.toml` (which a test also parses).
pub const STARTER_TOML: &str = include_str!("../../../docs/lode.starter.toml");
const DEFAULT_GITHUB_API: &str = "https://api.github.com";
const DEFAULT_CHANNEL: &str = "stable";
/// Default seconds between `[report]` metric exports.
pub const DEFAULT_REPORT_INTERVAL: u64 = 300;
/// Placeholder in `[command]` values that expands to the running version directory.
pub const DEFAULT_WORKDIR_PLACEHOLDER: &str = "{dir}";

// --- typed enums (shared by the CLI and TOML layers) -----------------------

/// Update policy (`update.policy`), design §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// No background checks; run current/pinned only.
    Off,
    /// Periodically check and advertise, but never auto-apply (default).
    Check,
    /// Periodically check and auto-apply newer versions.
    Auto,
}

/// How the app learns that a resource was updated (`[[resource]].notify`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Notify {
    /// Nothing; the app reads `state.json` or asks with `check_nonce` (default).
    #[default]
    None,
    /// Send `SIGHUP` to the app.
    Signal,
    /// Restart the app.
    Restart,
}

/// Signature-enforcement strength (`trust.require_signature`), design §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequireSignature {
    /// Integrity only (sha256), no signature check.
    Off,
    /// Enforce when keys are configured, else warn-and-skip (default).
    Auto,
    /// Always require a valid signature.
    Enforce,
}

/// What the manifest itself must prove (`trust.require_manifest`), design §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequireManifest {
    /// A signature or expiry is verified when present, never required (default).
    Off,
    /// The manifest must carry a valid top-level signature.
    Signed,
    /// Deprecated: the same as `signed`. It used to require a signed expiry as
    /// well; an expiry is still verified when the manifest carries one.
    Fresh,
}

impl RequireManifest {
    /// The value as written in `lode.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Signed => "signed",
            Self::Fresh => "fresh",
        }
    }
}

/// Readiness determination (`supervise.readiness`), design §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Readiness {
    /// Alive for `health_grace` seconds counts as ready (default).
    None,
    /// Wait for the app to write `state.ready`.
    State,
}

/// Crash-restart policy (`supervise.restart`), design §8.
///
/// Gates the bounded backoff machinery and the keep-alive *pause*; on `off` lode
/// mirrors the child's lifecycle. Update / rollback / explicit-restart relaunches
/// happen regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Mirror the child: lode exits with the child's code, never restarting. The
    /// orchestrator owns whole-process restart.
    Off,
    /// Restart on failure (non-zero exit or killed by a signal); a clean `exit(0)`
    /// makes lode exit too. After `restart_max` failed retries lode PAUSES (stays
    /// alive — does not exit) until a recovery trigger. The default.
    OnFailure,
    /// Like `on-failure` but restart on *any* child exit, including a clean
    /// `exit(0)`; pauses after `restart_max` retries.
    Always,
}

/// Restart strategy (`supervise.restart_mode`), design §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartMode {
    /// Stop the old child before starting the new one (default).
    StopStart,
    /// systemd socket-activation protocol; needs `listen`.
    SocketActivation,
    /// Overlap old and new via `SO_REUSEPORT`.
    ReuseportOverlap,
}

// --- resolved config (the 8 sections of design §10) ------------------------

/// Fully resolved lode configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// `[global]` section.
    pub global: Global,
    /// `[update]` section.
    pub update: Update,
    /// `[http]` section.
    pub http: Http,
    /// `[trust]` section.
    pub trust: Trust,
    /// `[command]` section.
    pub command: Command,
    /// `[runtime]` section.
    pub runtime: Runtime,
    /// `[supervise]` section.
    pub supervise: Supervise,
    /// `[signals]` section.
    pub signals: Signals,
    /// `[env]` — extra environment variables injected into the child (on top of
    /// the inherited host env; lode's own `LODE_*` introspection vars still win).
    pub env: BTreeMap<String, String>,
    /// `[[resource]]` — separately published bundles installed beside the app.
    pub resources: Vec<Resource>,
    /// `[report]` section.
    pub report: Report,
    /// The `lode.toml` path this config was read from (`None` when running
    /// file-less). The supervisor watches its mtime to recover a *paused* app after
    /// the operator edits it (design §8); a running app is never disturbed.
    #[allow(clippy::struct_field_names)] // mirrors the on-disk `lode.toml` concept
    pub config_path: Option<PathBuf>,
}

/// `[[resource]]` — a separately published, separately versioned bundle (config
/// files, data, assets) verified like the app and installed under
/// `$LODE_DIR/resources/<name>`. Its manifest `name` is `<app>/<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// Local id; also the manifest identity suffix.
    pub name: String,
    /// The resource's native `lode/v1` manifest URL.
    pub manifest: String,
    /// The asset filename to install.
    pub asset: String,
    /// Channel to follow (default: `[update].channel`).
    pub channel: String,
    /// Update policy (default: `[update].policy`).
    pub policy: Policy,
    /// How the app is told about an applied update.
    pub notify: Notify,
    /// The app does not start until the resource is installed.
    pub required: bool,
}

/// `[global]` — identity and storage.
#[derive(Debug, Clone)]
pub struct Global {
    /// App name; must match the manifest `name`.
    pub app: String,
    /// Base data directory (`$LODE_DIR`).
    pub dir: PathBuf,
    /// Log level: `trace`, `debug`, `info`, `warn` or `error`.
    pub log_level: String,
}

/// `[update]` — source and upgrade policy.
#[derive(Debug, Clone)]
pub struct Update {
    /// native source (mutually exclusive with [`Self::github`]).
    pub manifest: Option<String>,
    /// github source (mutually exclusive with [`Self::manifest`]).
    pub github: Option<String>,
    /// GitHub API base URL (override for GitHub Enterprise).
    pub github_api: String,
    /// GitHub token for a private repository (github source only). Stored raw;
    /// `${ENV}` is expanded per request and the value is never logged.
    pub github_token: Option<String>,
    /// The asset filename to install on this host — the source-agnostic selection
    /// key (source-adapters §3/§7). Required to resolve a download.
    pub asset: Option<String>,
    /// Release channel to follow.
    pub channel: String,
    /// Background update policy.
    pub policy: Policy,
    /// Seconds between update checks; `0` checks once at startup.
    pub check_interval: u64,
    /// Number of old versions kept for rollback.
    pub keep_versions: u32,
    /// Version or tag to force; disables auto-update.
    pub pin: Option<String>,
}

/// `[http]` — fetch credentials. Values are stored raw (never expanded/logged).
#[derive(Debug, Clone)]
pub struct Http {
    /// Extra request headers, as raw `Name: value` lines.
    pub headers: Vec<String>,
    /// Extra hosts (beyond the manifest/source origin) that may receive
    /// [`Self::headers`] on an artifact download. Empty by default; same-origin
    /// is always allowed (see [`crate::download::fetch_artifact`]).
    pub credential_hosts: Vec<String>,
    /// Permit non-HTTPS (plain `http`) remote fetches. Off by default; loopback
    /// http is always allowed regardless. See [`crate::http`].
    pub allow_insecure: bool,
}

/// `[trust]` — publisher-identity verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trust {
    /// Signature-enforcement strength.
    pub require_signature: RequireSignature,
    /// What the manifest itself must prove.
    pub require_manifest: RequireManifest,
    /// Trusted publisher key entries.
    pub trusted_keys: Vec<String>,
    /// File holding additional trusted key entries, one per line.
    pub trusted_keys_file: Option<String>,
    /// Optional URL of a signed revocation list for delegated keys.
    pub revocation_url: Option<String>,
}

/// `[report]` — push the host's status to an OpenTelemetry OTLP/HTTP endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// OTLP/HTTP base URL; exports go to `{endpoint}/v1/metrics` and
    /// `{endpoint}/v1/logs`. `None` turns reporting off.
    pub endpoint: Option<String>,
    /// Seconds between metric exports.
    pub interval: u64,
    /// Request headers for the endpoint only, as raw `Name: value` lines
    /// (`${ENV}` expanded per request, like `[http].headers`).
    pub headers: Vec<String>,
    /// Extra OpenTelemetry resource attributes.
    pub attributes: BTreeMap<String, String>,
    /// `service.instance.id`; the host name when unset.
    pub instance_id: Option<String>,
}

/// `[command]` — how to launch the app.
///
/// `run`/`exec` are LITERAL commands (whitespace-split; only `{dir}` expands, to
/// the running version dir) and are optional here: a manifest asset may publish
/// signed `run`/`exec` overrides that take precedence, and launch resolution errors
/// only when *neither* side supplies the command actually needed (see
/// `supervisor::effective_command`).
#[derive(Debug, Clone)]
pub struct Command {
    /// Command for bare `lode`.
    pub run: Option<String>,
    /// Base command for exec passthrough.
    pub exec: Option<String>,
    /// Working directory of the child; `{dir}` is the version directory.
    pub workdir: String,
}

/// `[runtime]` — optional runtime dependency.
// `runtime` mirrors the `[runtime] runtime = "…"` TOML key, so the field name is
// fixed by the schema even though it repeats the struct name.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone)]
pub struct Runtime {
    /// Runtime binary name the app needs on `PATH`.
    pub runtime: Option<String>,
    /// URL the runtime is downloaded from when it is missing.
    pub download: Option<String>,
    /// Expected runtime version. When set, lode probes the runtime (PATH, cache,
    /// or freshly downloaded) and requires its self-reported version to *contain*
    /// this string; a wrong-version PATH/cache entry is bypassed for a fresh
    /// download, and a downloaded mismatch is a hard error.
    pub version: Option<String>,
    /// Argument(s) that make the runtime print its version (whitespace-split,
    /// appended to the runtime binary). Defaults to `--version`. Only used when
    /// [`version`](Self::version) is set.
    pub version_check: Option<String>,
    /// Expected lowercase-hex sha256 of the `download` body. When set, a
    /// downloaded runtime whose digest differs is refused (and the download
    /// discarded) before it is placed on the child's PATH — the only integrity
    /// check a runtime gets, since `[runtime]` carries no publisher signature.
    pub sha256: Option<String>,
}

/// `[supervise]` — restart policy / health / rollback / stop / restart mode.
#[derive(Debug, Clone)]
pub struct Supervise {
    /// Crash-restart policy. `on-failure` (default) retries then pauses (keep-alive);
    /// `off` mirrors the child; `always` also retries a clean exit.
    pub restart: RestartPolicy,
    /// Backoff base/cap **in seconds** and the retry cap before pausing — only used
    /// when [`restart`](Self::restart) is not `off`. `restart_max=0` retries forever.
    pub restart_backoff: u64,
    /// Backoff cap in seconds.
    pub restart_backoff_max: u64,
    /// Retries before lode pauses; `0` retries forever.
    pub restart_max: u32,
    /// How a new instance is judged ready.
    pub readiness: Readiness,
    /// (`readiness=state`) Seconds to wait for the ready signal.
    pub ready_timeout: u64,
    /// (`readiness=state`) Seconds to wait for the app to ack a staged-update
    /// prepare prompt before forcing the cut-over; `0` (default) disables the
    /// timeout — the app paces the cut-over (design §8).
    pub prepare_timeout: u64,
    /// (`readiness=none`) Seconds a new version must survive.
    pub health_grace: u64,
    /// Graceful-stop seconds before `SIGKILL`.
    pub stop_timeout: u64,
    /// How a restart replaces the running instance.
    pub restart_mode: RestartMode,
    /// Listen address for socket activation.
    pub listen: Option<String>,
}

/// `[signals]` — signal forwarding. Empty `forward` => lode's standard set (§8).
#[derive(Debug, Clone)]
pub struct Signals {
    /// Signals forwarded to the child.
    pub forward: Vec<String>,
    /// Signal that triggers a graceful restart instead of being forwarded.
    pub restart: Option<String>,
}

// --- raw TOML layer (everything optional) ----------------------------------
//
// Every struct is `deny_unknown_fields`: a typo'd key in `lode.toml` is a hard
// parse error naming the key, not a silent no-op. This matters most for the
// keep-alive pause-recovery flow, where the operator fixes the file while the
// app is paused — a silently ignored key would leave it paused with no hint.
// (`[env]` stays open: it is a BTreeMap of user-defined variables.)

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlConfig {
    #[serde(default)]
    global: TomlGlobal,
    #[serde(default)]
    update: TomlUpdate,
    #[serde(default)]
    http: TomlHttp,
    #[serde(default)]
    trust: TomlTrust,
    #[serde(default)]
    command: TomlCommand,
    #[serde(default)]
    runtime: TomlRuntime,
    #[serde(default)]
    supervise: TomlSupervise,
    #[serde(default)]
    signals: TomlSignals,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    resource: Vec<TomlResource>,
    #[serde(default)]
    report: TomlReport,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlReport {
    endpoint: Option<String>,
    interval: Option<u64>,
    headers: Option<Vec<String>>,
    attributes: Option<BTreeMap<String, String>>,
    instance_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlResource {
    name: String,
    manifest: String,
    asset: String,
    channel: Option<String>,
    policy: Option<Policy>,
    #[serde(default)]
    notify: Notify,
    #[serde(default)]
    required: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlGlobal {
    app: Option<String>,
    dir: Option<String>,
    log_level: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlUpdate {
    manifest: Option<String>,
    github: Option<String>,
    github_api: Option<String>,
    github_token: Option<String>,
    asset: Option<String>,
    channel: Option<String>,
    policy: Option<Policy>,
    check_interval: Option<u64>,
    keep_versions: Option<u32>,
    pin: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlHttp {
    headers: Option<Vec<String>>,
    credential_hosts: Option<Vec<String>>,
    allow_insecure: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlTrust {
    require_signature: Option<RequireSignature>,
    require_manifest: Option<RequireManifest>,
    trusted_keys: Option<Vec<String>>,
    trusted_keys_file: Option<String>,
    revocation_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlCommand {
    run: Option<String>,
    exec: Option<String>,
    workdir: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlRuntime {
    runtime: Option<String>,
    download: Option<String>,
    version: Option<String>,
    version_check: Option<String>,
    sha256: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlSupervise {
    restart: Option<RestartPolicy>,
    restart_backoff: Option<u64>,
    restart_backoff_max: Option<u64>,
    restart_max: Option<u32>,
    readiness: Option<Readiness>,
    ready_timeout: Option<u64>,
    prepare_timeout: Option<u64>,
    health_grace: Option<u64>,
    stop_timeout: Option<u64>,
    restart_mode: Option<RestartMode>,
    listen: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlSignals {
    forward: Option<Vec<String>>,
    restart: Option<String>,
}

// --- CLI/env override layer (clap-free merge seam) -------------------------
//
// `Overrides` is the single CLI/env slot that [`merge`] lays over the TOML layer
// and the defaults. It is a plain, clap-free mirror of exactly the CLI globals
// the `merge_*` helpers read, so this crate never depends on clap: the `lode`
// binary projects its parsed globals onto it (`config_cli::overrides_from_globals`),
// while the library paths ([`Config::from_toml`], [`ConfigBuilder`]) fill it
// directly. Field names mirror the matching CLI flag / `LODE_*` knob;
// `None`/empty/`false` means "not set — fall through to TOML, then the default".
//
// `[command].workdir` and `[supervise].prepare_timeout` are intentionally
// absent: they are TOML-only (no CLI/env slot), so `merge` reads them straight
// off the TOML layer.
/// Per-invocation overrides from CLI flags and `LODE_*` variables.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    /// Overrides `[global].log_level`.
    pub log_level: Option<String>,
    /// Overrides `[global].app`.
    pub app: Option<String>,
    /// Overrides `[global].dir`.
    pub dir: Option<String>,
    /// Overrides `[update].manifest`.
    pub manifest: Option<String>,
    /// Overrides `[update].github`.
    pub github: Option<String>,
    /// Overrides `[update].github_api`.
    pub github_api: Option<String>,
    /// Overrides `[update].github_token`.
    pub github_token: Option<String>,
    /// Overrides `[update].asset`.
    pub asset: Option<String>,
    /// Overrides `[update].channel`.
    pub channel: Option<String>,
    /// Overrides `[update].policy`.
    pub policy: Option<Policy>,
    /// Overrides `[update].check_interval`.
    pub interval: Option<u64>,
    /// Overrides `[update].keep_versions`.
    pub keep: Option<u32>,
    /// Overrides `[update].pin`.
    pub pin: Option<String>,
    /// Replaces `[http].headers` when non-empty.
    pub header: Vec<String>,
    /// Replaces `[http].credential_hosts` when non-empty.
    pub credential_host: Vec<String>,
    /// Sets `[http].allow_insecure`.
    pub allow_insecure_http: bool,
    /// Overrides `[trust].require_signature`.
    pub require_signature: Option<RequireSignature>,
    /// Overrides `[trust].require_manifest`.
    pub require_manifest: Option<RequireManifest>,
    /// Overrides `[trust].trusted_keys` (comma-separated).
    pub trusted_keys: Option<String>,
    /// Overrides `[trust].trusted_keys_file`.
    pub trusted_keys_file: Option<String>,
    /// Overrides `[trust].revocation_url`.
    pub revocation_url: Option<String>,
    /// Overrides `[command].run`.
    pub run: Option<String>,
    /// Overrides `[command].exec`.
    pub exec: Option<String>,
    /// Overrides `[runtime].runtime`.
    pub runtime: Option<String>,
    /// Overrides `[runtime].download`.
    pub runtime_download: Option<String>,
    /// Overrides `[runtime].version`.
    pub runtime_version: Option<String>,
    /// Overrides `[runtime].version_check`.
    pub runtime_version_check: Option<String>,
    /// Overrides `[runtime].sha256`.
    pub runtime_sha256: Option<String>,
    /// Overrides `[supervise].restart`.
    pub restart: Option<RestartPolicy>,
    /// Overrides `[supervise].restart_backoff`.
    pub restart_backoff: Option<u64>,
    /// Overrides `[supervise].restart_backoff_max`.
    pub restart_backoff_max: Option<u64>,
    /// Overrides `[supervise].restart_max`.
    pub restart_max: Option<u32>,
    /// Overrides `[supervise].readiness`.
    pub readiness: Option<Readiness>,
    /// Overrides `[supervise].ready_timeout`.
    pub ready_timeout: Option<u64>,
    /// Overrides `[supervise].health_grace`.
    pub health_grace: Option<u64>,
    /// Overrides `[supervise].stop_timeout`.
    pub stop_timeout: Option<u64>,
    /// Overrides `[supervise].restart_mode`.
    pub restart_mode: Option<RestartMode>,
    /// Overrides `[supervise].listen`.
    pub listen: Option<String>,
    /// Overrides `[signals].forward` (comma-separated).
    pub forward_signals: Option<String>,
    /// Overrides `[signals].restart`.
    pub restart_signal: Option<String>,
    /// Overrides `[report].endpoint`.
    pub report_endpoint: Option<String>,
    /// Overrides `[report].interval`.
    pub report_interval: Option<u64>,
}

// --- resolution ------------------------------------------------------------

/// Resolve the effective configuration from an already-projected CLI/env layer
/// (`overrides`), the `lode.toml` at `config_path` (when one was located) and the
/// design defaults, then validate it.
///
/// Locating the file is the caller's job ([`find_config_path`]) — the `lode`
/// binary adds the CLI/env search + first-run scaffold on top ([`scaffold_starter_config`]).
///
/// # Errors
/// Returns [`Error::Config`] when `config_path` cannot be read, a TOML parse
/// error, or a validation failure.
pub fn resolve_with(overrides: &Overrides, config_path: Option<PathBuf>) -> Result<Config> {
    let toml = match config_path.as_deref() {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| Error::Config(format!("read config {}: {e}", path.display())))?;
            toml::from_str(&text)?
        }
        None => TomlConfig::default(),
    };
    let mut cfg = merge(overrides, &toml);
    cfg.config_path = config_path;
    validate(&cfg)?;
    Ok(cfg)
}

/// Write a minimal **sourceless** `lode.toml` (no update source, `policy=off`) at
/// `<dir>/lode.toml` when one is absent, so a `seed`-prepared data dir runs
/// offline with bare `lode`.
///
/// Never clobbers an existing config. Used by
/// `lode-cli seed` before it resolves, so seeding "just works" on a fresh dir
/// without tripping the source-requiring starter scaffold. `seed_source` is the
/// file being seeded — its name derives the scaffolded `[command]` launch command.
///
/// `dir`/`app` are the CLI/env values when set, else lode's defaults.
///
/// # Errors
/// Returns [`Error::Config`] when the directory or the file cannot be written.
pub fn ensure_sourceless_toml(
    dir: Option<&str>,
    app: Option<&str>,
    seed_source: &Path,
) -> Result<()> {
    let path = Path::new(dir.unwrap_or(DEFAULT_DIR)).join("lode.toml");
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Config(format!("create {}: {e}", dir.display())))?;
    }
    let app = app.unwrap_or(DEFAULT_APP);
    let command = seed_run_command(seed_source, app);
    let body = format!(
        "# Sourceless config for OFFLINE local testing (written by `lode-cli seed`).\n\
         # No [update].manifest/github => lode never downloads; it runs whatever is\n\
         # seeded under versions/. policy=off disables the background update check.\n\
         [global]\n\
         app = \"{app}\"\n\n\
         [update]\n\
         policy = \"off\"\n\n\
         # Literal launch commands (cwd = the version dir); edit if the seeded\n\
         # artifact needs a runtime (e.g. run = \"bun run app.ts\").\n\
         [command]\n\
         run  = \"{command}\"\n\
         exec = \"{command}\"\n"
    );
    std::fs::write(&path, body)
        .map_err(|e| Error::Config(format!("write {}: {e}", path.display())))?;
    tracing::info!(path = %path.display(), "seed: wrote sourceless lode.toml");
    Ok(())
}

/// Best-effort launch command for a seeded artifact, mirroring where
/// `install::seed_local` lands the file: a raw file keeps its filename, a `.gz`
/// drops the suffix, and an archive's binary is conventionally `./{app}` at the
/// extraction root (the scaffold comment tells the operator to edit otherwise).
fn seed_run_command(seed_source: &Path, app: &str) -> String {
    let name = seed_source
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(app);
    match crate::manifest::format_from_name(name) {
        "raw" => format!("./{name}"),
        "gz" => format!("./{}", name.strip_suffix(".gz").unwrap_or(name)),
        _ => format!("./{app}"),
    }
}

/// Locate `lode.toml` without reading it.
///
/// An `explicit` path (`--config`/`LODE_CONFIG`) always wins (even if the file is
/// missing — the read will report it); otherwise search `<dir>/lode.toml`
/// (`--dir`/`LODE_DIR`, else lode's default base dir), then `./lode.toml`.
#[must_use]
pub fn find_config_path(explicit: Option<&str>, dir: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(PathBuf::from(path));
    }
    let in_data = Path::new(dir.unwrap_or(DEFAULT_DIR)).join("lode.toml");
    if in_data.is_file() {
        return Some(in_data);
    }
    let local = PathBuf::from("lode.toml");
    if local.is_file() { Some(local) } else { None }
}

/// Cheap pre-logging peek at `[global].log_level` in the `lode.toml` at `path`,
/// so the tracing subscriber (which must be up before [`resolve_with`]) can
/// honour the TOML value.
///
/// Lenient by design: a missing, unreadable or malformed
/// file yields `None` — the subsequent full resolve reports the real error with
/// logging already up.
#[must_use]
pub fn peek_log_level(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: TomlConfig = toml::from_str(&text).ok()?;
    parsed.global.log_level
}

/// First-run convenience: scaffold a starter `lode.toml` at `path`.
///
/// Best-effort (creating parent dirs); returns the guiding [`Error::Config`] the
/// loader stops on, instead of failing later on a placeholder source.
#[must_use]
pub fn scaffold_starter_config(path: &Path) -> Error {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(path, STARTER_TOML) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "no lode.toml found — wrote a starter config");
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not write starter lode.toml");
            }
        }
    }
    Error::Config(format!(
        "no lode.toml found — wrote a starter to {}; set [update].manifest (or [update].github) \
         to your real source and re-run, or pass --manifest/--github (LODE_MANIFEST/LODE_GITHUB)",
        path.display()
    ))
}

/// Merge the CLI/env layer (`o`) over the TOML layer over the defaults.
/// Infallible; semantic checks live in [`validate`]. Clap-free: it reads only the
/// plain [`Overrides`] seam, so every construction path (CLI, file-less, builder)
/// funnels through the same precedence and per-field logic.
fn merge(o: &Overrides, t: &TomlConfig) -> Config {
    Config {
        global: merge_global(o, &t.global),
        update: merge_update(o, &t.update),
        http: merge_http(o, &t.http),
        trust: merge_trust(o, &t.trust),
        command: merge_command(o, &t.command),
        runtime: merge_runtime(o, &t.runtime),
        supervise: merge_supervise(o, &t.supervise),
        signals: merge_signals(o, &t.signals),
        // `[env]` is config-file only — no CLI/env override layer. To override an
        // entry at deploy time, set it directly in the process env (it wins as a
        // host env var; see `child_env`).
        env: t.env.clone(),
        resources: merge_resources(o, t),
        report: merge_report(o, &t.report),
        config_path: None, // filled in by `resolve` after merge
    }
}

fn merge_resources(o: &Overrides, t: &TomlConfig) -> Vec<Resource> {
    let update = merge_update(o, &t.update);
    t.resource
        .iter()
        .map(|r| Resource {
            name: r.name.clone(),
            manifest: r.manifest.clone(),
            asset: r.asset.clone(),
            channel: r.channel.clone().unwrap_or_else(|| update.channel.clone()),
            policy: r.policy.unwrap_or(update.policy),
            notify: r.notify,
            required: r.required,
        })
        .collect()
}

fn merge_report(o: &Overrides, t: &TomlReport) -> Report {
    Report {
        endpoint: o
            .report_endpoint
            .clone()
            .or_else(|| t.endpoint.clone())
            .filter(|e| !e.trim().is_empty()),
        interval: o
            .report_interval
            .or(t.interval)
            .unwrap_or(DEFAULT_REPORT_INTERVAL),
        headers: t.headers.clone().unwrap_or_default(),
        attributes: t.attributes.clone().unwrap_or_default(),
        instance_id: t.instance_id.clone(),
    }
}

fn merge_global(o: &Overrides, t: &TomlGlobal) -> Global {
    Global {
        app: o
            .app
            .clone()
            .or_else(|| t.app.clone())
            .unwrap_or_else(|| DEFAULT_APP.to_owned()),
        dir: o
            .dir
            .clone()
            .or_else(|| t.dir.clone())
            .map_or_else(|| PathBuf::from(DEFAULT_DIR), PathBuf::from),
        log_level: o
            .log_level
            .clone()
            .or_else(|| t.log_level.clone())
            .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_owned()),
    }
}

fn merge_update(o: &Overrides, t: &TomlUpdate) -> Update {
    Update {
        manifest: o.manifest.clone().or_else(|| t.manifest.clone()),
        github: o.github.clone().or_else(|| t.github.clone()),
        github_api: o
            .github_api
            .clone()
            .or_else(|| t.github_api.clone())
            .unwrap_or_else(|| DEFAULT_GITHUB_API.to_owned()),
        github_token: o
            .github_token
            .clone()
            .or_else(|| t.github_token.clone())
            .filter(|t| !t.trim().is_empty()),
        asset: o.asset.clone().or_else(|| t.asset.clone()),
        channel: o
            .channel
            .clone()
            .or_else(|| t.channel.clone())
            .unwrap_or_else(|| DEFAULT_CHANNEL.to_owned()),
        policy: o.policy.or(t.policy).unwrap_or(Policy::Check),
        check_interval: o.interval.or(t.check_interval).unwrap_or(300),
        keep_versions: o.keep.or(t.keep_versions).unwrap_or(3),
        pin: o.pin.clone().or_else(|| t.pin.clone()),
    }
}

fn merge_http(o: &Overrides, t: &TomlHttp) -> Http {
    let headers = if o.header.is_empty() {
        t.headers.clone().unwrap_or_default()
    } else {
        o.header.clone()
    };
    let credential_hosts = if o.credential_host.is_empty() {
        t.credential_hosts.clone().unwrap_or_default()
    } else {
        o.credential_host.clone()
    };
    // `--allow-insecure-http` is a one-way switch (it can only turn the gate on),
    // so CLI-true wins; otherwise fall back to the TOML value, default false.
    let allow_insecure = o.allow_insecure_http || t.allow_insecure.unwrap_or(false);
    Http {
        headers,
        credential_hosts,
        allow_insecure,
    }
}

fn merge_trust(o: &Overrides, t: &TomlTrust) -> Trust {
    let trusted_keys = o.trusted_keys.as_ref().map_or_else(
        || t.trusted_keys.clone().unwrap_or_default(),
        |list| split_csv(list),
    );
    Trust {
        require_signature: o
            .require_signature
            .or(t.require_signature)
            .unwrap_or(RequireSignature::Auto),
        require_manifest: o
            .require_manifest
            .or(t.require_manifest)
            .unwrap_or(RequireManifest::Off),
        trusted_keys,
        trusted_keys_file: o
            .trusted_keys_file
            .clone()
            .or_else(|| t.trusted_keys_file.clone()),
        revocation_url: o
            .revocation_url
            .clone()
            .or_else(|| t.revocation_url.clone()),
    }
}

fn merge_command(o: &Overrides, t: &TomlCommand) -> Command {
    // `run`/`exec` have NO default: an unset command stays `None`, and launch
    // resolution decides (a manifest asset may supply the override; otherwise it
    // is a clear "no run command" error at launch, not at config parse).
    Command {
        run: o.run.clone().or_else(|| t.run.clone()),
        exec: o.exec.clone().or_else(|| t.exec.clone()),
        // No CLI flag: the child's working dir defaults to the version dir
        // (`{dir}`) and is overridable only via `[command].workdir` in lode.toml.
        workdir: t
            .workdir
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKDIR_PLACEHOLDER.to_owned()),
    }
}

fn merge_runtime(o: &Overrides, t: &TomlRuntime) -> Runtime {
    Runtime {
        runtime: o.runtime.clone().or_else(|| t.runtime.clone()),
        download: o.runtime_download.clone().or_else(|| t.download.clone()),
        version: o.runtime_version.clone().or_else(|| t.version.clone()),
        version_check: o
            .runtime_version_check
            .clone()
            .or_else(|| t.version_check.clone()),
        sha256: o.runtime_sha256.clone().or_else(|| t.sha256.clone()),
    }
}

fn merge_supervise(o: &Overrides, t: &TomlSupervise) -> Supervise {
    Supervise {
        // Default keep-alive (design §8): a failing app is retried, then the
        // supervisor PAUSES (stays alive — never crash-loops the container) rather
        // than exiting. `off` opts back into mirror-the-child (lode exits with it).
        restart: o.restart.or(t.restart).unwrap_or(RestartPolicy::OnFailure),
        restart_backoff: o.restart_backoff.or(t.restart_backoff).unwrap_or(1),
        restart_backoff_max: o
            .restart_backoff_max
            .or(t.restart_backoff_max)
            .unwrap_or(30),
        // Retry this many times after a failure, then pause (0 = retry forever).
        restart_max: o.restart_max.or(t.restart_max).unwrap_or(3),
        readiness: o.readiness.or(t.readiness).unwrap_or(Readiness::None),
        ready_timeout: o.ready_timeout.or(t.ready_timeout).unwrap_or(30),
        // TOML-only (no CLI flag): an advanced staged-update knob. 0 = disabled,
        // the app paces the cut-over (the documented default behaviour).
        prepare_timeout: t.prepare_timeout.unwrap_or(0),
        health_grace: o.health_grace.or(t.health_grace).unwrap_or(15),
        stop_timeout: o.stop_timeout.or(t.stop_timeout).unwrap_or(10),
        restart_mode: o
            .restart_mode
            .or(t.restart_mode)
            .unwrap_or(RestartMode::StopStart),
        listen: o.listen.clone().or_else(|| t.listen.clone()),
    }
}

fn merge_signals(o: &Overrides, t: &TomlSignals) -> Signals {
    let forward = o.forward_signals.as_ref().map_or_else(
        || t.forward.clone().unwrap_or_default(),
        |list| split_csv(list),
    );
    Signals {
        forward,
        restart: o.restart_signal.clone().or_else(|| t.restart.clone()),
    }
}

/// Split a comma-separated list, trimming whitespace and dropping empties.
fn split_csv(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

// --- clap-free construction (library API) ----------------------------------

impl Config {
    /// Build a fully-resolved, validated [`Config`] from `lode.toml` text alone —
    /// no CLI/env layer, no clap, no filesystem. The override slot is empty
    /// ([`Overrides::default`]), so values come from the parsed TOML over the
    /// design defaults — exactly what `resolve` would produce for this file with
    /// no flags set. [`config_path`](Config::config_path) is `None` (this config
    /// was not read from a located file).
    ///
    /// ```
    /// # fn main() -> lode_core::Result<()> {
    /// // Clap-free: parse `lode.toml` text directly, no CLI/env layer.
    /// let cfg = lode_core::Config::from_toml("[global]\napp = \"demo\"\n[update]\npolicy = \"off\"\n")?;
    /// assert_eq!(cfg.global.app, "demo");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    /// Returns [`Error::Toml`] if `text` is not valid `lode.toml` (including an
    /// unknown key), or [`Error::Config`] if the resolved config fails the same
    /// semantic [`validate`] checks `resolve` applies.
    pub fn from_toml(text: &str) -> Result<Self> {
        let parsed: TomlConfig = toml::from_str(text)?;
        let cfg = merge(&Overrides::default(), &parsed);
        validate(&cfg)?;
        Ok(cfg)
    }

    /// Start a programmatic, clap-free [`ConfigBuilder`] for a [`Config`].
    #[must_use]
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::default()
    }
}

/// Programmatic, clap-free builder for [`Config`].
///
/// The library construction path alongside [`Config::from_toml`]. Each setter
/// fills the same CLI/env override slot ([`Overrides`]) the CLI uses, so the
/// precedence and defaults are identical to a `resolve` with those flags set;
/// [`build`](ConfigBuilder::build) runs the same [`validate`]. Fields left unset
/// fall through to the design defaults.
#[derive(Debug, Default, Clone)]
pub struct ConfigBuilder {
    overrides: Overrides,
    report_headers: Vec<String>,
    report_attributes: BTreeMap<String, String>,
    report_instance_id: Option<String>,
}

impl ConfigBuilder {
    /// `[global].app` — application name (namespaces the data dir + lock).
    #[must_use]
    pub fn app(mut self, app: impl Into<String>) -> Self {
        self.overrides.app = Some(app.into());
        self
    }

    /// `[global].dir` — base/run directory (defaults to `/srv/lode`).
    #[must_use]
    pub fn dir(mut self, dir: impl Into<String>) -> Self {
        self.overrides.dir = Some(dir.into());
        self
    }

    /// `[update].manifest` — native source URL (mutually exclusive with
    /// [`github`](Self::github); the conflict is caught by `build`'s validate).
    #[must_use]
    pub fn manifest(mut self, manifest: impl Into<String>) -> Self {
        self.overrides.manifest = Some(manifest.into());
        self
    }

    /// `[update].github` — `owner/name` source (mutually exclusive with
    /// [`manifest`](Self::manifest)).
    #[must_use]
    pub fn github(mut self, github: impl Into<String>) -> Self {
        self.overrides.github = Some(github.into());
        self
    }

    /// `[update].asset` — asset filename to install on this host.
    #[must_use]
    pub fn asset(mut self, asset: impl Into<String>) -> Self {
        self.overrides.asset = Some(asset.into());
        self
    }

    /// `[update].channel` — channel to follow (defaults to `stable`).
    #[must_use]
    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.overrides.channel = Some(channel.into());
        self
    }

    /// `[update].policy` — update policy (defaults to [`Policy::Check`]).
    #[must_use]
    pub const fn policy(mut self, policy: Policy) -> Self {
        self.overrides.policy = Some(policy);
        self
    }

    /// `[update].pin` — pin to a specific version/tag (disables auto-update).
    #[must_use]
    pub fn pin(mut self, pin: impl Into<String>) -> Self {
        self.overrides.pin = Some(pin.into());
        self
    }

    /// `[command].run` — bare-run launch command.
    #[must_use]
    pub fn run(mut self, run: impl Into<String>) -> Self {
        self.overrides.run = Some(run.into());
        self
    }

    /// `[command].exec` — CLI-passthrough base command.
    #[must_use]
    pub fn exec(mut self, exec: impl Into<String>) -> Self {
        self.overrides.exec = Some(exec.into());
        self
    }

    /// `[report].endpoint` — OTLP/HTTP base URL; turns status reporting on.
    #[must_use]
    pub fn report_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.overrides.report_endpoint = Some(endpoint.into());
        self
    }

    /// `[report].interval` — seconds between metric exports (defaults to 300).
    #[must_use]
    pub const fn report_interval(mut self, seconds: u64) -> Self {
        self.overrides.report_interval = Some(seconds);
        self
    }

    /// Add a `[report].headers` line (`Name: value`, `${ENV}` expanded per
    /// request); sent to the report endpoint only.
    #[must_use]
    pub fn report_header(mut self, line: impl Into<String>) -> Self {
        self.report_headers.push(line.into());
        self
    }

    /// Add a `[report].attributes` resource attribute.
    #[must_use]
    pub fn report_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.report_attributes.insert(key.into(), value.into());
        self
    }

    /// `[report].instance_id` — `service.instance.id` (defaults to the host name).
    #[must_use]
    pub fn report_instance_id(mut self, id: impl Into<String>) -> Self {
        self.report_instance_id = Some(id.into());
        self
    }

    /// Resolve over the design defaults and run the same semantic [`validate`] as
    /// `resolve`. [`config_path`](Config::config_path) is `None` (file-less).
    ///
    /// # Errors
    /// Returns [`Error::Config`] if the resulting config fails validation (empty
    /// key field, both sources set, an out-of-range backoff window, or an
    /// insecure report endpoint).
    pub fn build(self) -> Result<Config> {
        let mut cfg = merge(&self.overrides, &TomlConfig::default());
        cfg.report.headers = self.report_headers;
        cfg.report.attributes = self.report_attributes;
        cfg.report.instance_id = self.report_instance_id;
        validate(&cfg)?;
        Ok(cfg)
    }
}

/// `[[resource]]` entries: valid unique names and non-blank source fields.
fn validate_resources(resources: &[Resource]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for r in resources {
        crate::idval::validate_id("resource name", &r.name)
            .map_err(|e| Error::Config(e.to_string()))?;
        if !seen.insert(r.name.as_str()) {
            return Err(Error::Config(format!(
                "duplicate [[resource]] name {:?}",
                r.name
            )));
        }
        for (key, value) in [("manifest", &r.manifest), ("asset", &r.asset)] {
            if value.trim().is_empty() {
                return Err(Error::Config(format!(
                    "[[resource]] {:?}: {key} must not be empty",
                    r.name
                )));
            }
        }
    }
    Ok(())
}

/// `[report]`: an `http(s)` endpoint (HTTPS unless loopback or
/// `allow_insecure`) and a positive interval.
fn validate_report(report: &Report, allow_insecure: bool) -> Result<()> {
    let Some(endpoint) = report.endpoint.as_deref() else {
        return Ok(());
    };
    let lower = endpoint.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err(Error::Config(
            "report.endpoint must be an http(s) URL (the OTLP/HTTP base)".to_owned(),
        ));
    }
    if !allow_insecure && !crate::http::scheme_allowed(endpoint) {
        return Err(Error::Config(
            "report.endpoint must be https (or loopback http); set [http].allow_insecure=true to override"
                .to_owned(),
        ));
    }
    if report.interval == 0 {
        return Err(Error::Config(
            "report.interval must be at least 1 second".to_owned(),
        ));
    }
    Ok(())
}

/// Semantic validation: non-empty key fields, source XOR, and numeric ranges.
/// (Enum values are already checked by clap/serde at parse time.)
fn validate(cfg: &Config) -> Result<()> {
    if cfg.global.app.trim().is_empty() {
        return Err(Error::Config(
            "global.app must not be empty (the app name namespaces the data dir and lock)"
                .to_owned(),
        ));
    }
    if cfg.global.app.contains('/') {
        return Err(Error::Config(
            "global.app must not contain `/` (`<app>/<name>` names a resource)".to_owned(),
        ));
    }
    validate_resources(&cfg.resources)?;
    if cfg.update.channel.trim().is_empty() {
        return Err(Error::Config(
            "update.channel must not be empty (e.g. \"stable\")".to_owned(),
        ));
    }
    if let Some(asset) = &cfg.update.asset
        && asset.trim().is_empty()
    {
        return Err(Error::Config(
            "update.asset must not be empty (set the asset filename to install, or omit it)"
                .to_owned(),
        ));
    }
    match (&cfg.update.manifest, &cfg.update.github) {
        (Some(_), Some(_)) => {
            return Err(Error::Config(
                "update.manifest and update.github are mutually exclusive (set exactly one)"
                    .to_owned(),
            ));
        }
        (None, None) => {
            tracing::debug!("no update source configured (neither manifest nor github set)");
        }
        _ => {}
    }
    if cfg.update.github_token.is_some() && cfg.update.github.is_none() {
        return Err(Error::Config(
            "update.github_token is only used with update.github (a private GitHub repository)"
                .to_owned(),
        ));
    }
    if cfg.update.github.is_some() && cfg.trust.require_manifest != RequireManifest::Off {
        return Err(Error::Config(format!(
            "trust.require_manifest = \"{}\" needs a native manifest source: \
             a GitHub release has no manifest to sign",
            cfg.trust.require_manifest.as_str()
        )));
    }
    if let Some(sha) = cfg.runtime.sha256.as_deref()
        && !(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Error::Config(
            "runtime.sha256 must be the 64-character hex sha256 of the runtime download".to_owned(),
        ));
    }
    validate_report(&cfg.report, cfg.http.allow_insecure)?;
    if cfg.trust.require_manifest == RequireManifest::Fresh {
        tracing::warn!(
            "trust.require_manifest = \"fresh\" is deprecated and now means \"signed\": \
             an expiry is verified when present, no longer required"
        );
    }
    if cfg.supervise.restart_backoff_max < cfg.supervise.restart_backoff {
        return Err(Error::Config(format!(
            "supervise.restart_backoff_max ({}) must be >= restart_backoff ({})",
            cfg.supervise.restart_backoff_max, cfg.supervise.restart_backoff
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI/env override layer with no flags set (the all-unset baseline), so
    /// [`merge`] sees an empty top slot and resolves TOML-over-default. Field
    /// names mirror the CLI knobs, so the existing tests mutate it verbatim.
    fn blank_cli() -> Overrides {
        Overrides::default()
    }

    #[test]
    fn github_token_needs_the_github_source() {
        let cfg = Config::from_toml("[update]\ngithub = \"o/r\"\ngithub_token = \"${GH_TOKEN}\"\n")
            .unwrap();
        assert_eq!(cfg.update.github_token.as_deref(), Some("${GH_TOKEN}"));
        assert!(
            Config::from_toml(
                "[update]\nmanifest = \"https://r.example/m.json\"\ngithub_token = \"t\"\n"
            )
            .is_err()
        );
        let mut cli = blank_cli();
        cli.github_token = Some("${OTHER}".to_owned());
        let parsed: TomlConfig = toml::from_str("[update]\ngithub_token = \"x\"\n").unwrap();
        assert_eq!(
            merge(&cli, &parsed).update.github_token.as_deref(),
            Some("${OTHER}")
        );
    }

    #[test]
    fn report_is_off_by_default() {
        let cfg = merge(&blank_cli(), &TomlConfig::default());
        assert_eq!(cfg.report.endpoint, None);
        assert_eq!(cfg.report.interval, 300);
        assert!(cfg.report.headers.is_empty());
    }

    #[test]
    fn report_parses_and_cli_overrides() {
        let toml = "[report]\nendpoint = \"https://otel.example/r/otlp\"\ninterval = 60\n\
                    headers = [\"Authorization: Bearer ${T}\"]\ninstance_id = \"web-01\"\n\
                    attributes = { region = \"hk\" }\n";
        let parsed: TomlConfig = toml::from_str(toml).unwrap();
        let cfg = merge(&blank_cli(), &parsed);
        assert_eq!(
            cfg.report.endpoint.as_deref(),
            Some("https://otel.example/r/otlp")
        );
        assert_eq!(cfg.report.interval, 60);
        assert_eq!(
            cfg.report.headers,
            vec!["Authorization: Bearer ${T}".to_owned()]
        );
        assert_eq!(cfg.report.instance_id.as_deref(), Some("web-01"));
        assert_eq!(
            cfg.report.attributes.get("region").map(String::as_str),
            Some("hk")
        );

        let mut cli = blank_cli();
        cli.report_endpoint = Some("https://other.example".to_owned());
        cli.report_interval = Some(30);
        let cfg = merge(&cli, &parsed);
        assert_eq!(
            cfg.report.endpoint.as_deref(),
            Some("https://other.example")
        );
        assert_eq!(cfg.report.interval, 30);
    }

    #[test]
    fn report_endpoint_must_be_secure_and_interval_positive() {
        let base = "[update]\npolicy = \"off\"\n";
        let with = |report: &str| Config::from_toml(&format!("{base}[report]\n{report}"));
        assert!(with("endpoint = \"https://otel.example\"").is_ok());
        assert!(with("endpoint = \"http://127.0.0.1:4318\"").is_ok());
        assert!(with("endpoint = \"http://otel.example\"").is_err());
        assert!(with("endpoint = \"otel.example\"").is_err());
        assert!(with("endpoint = \"https://otel.example\"\ninterval = 0").is_err());
        assert!(with("interval = 0").is_ok()); // off: nothing to check
        assert!(
            Config::from_toml(&format!(
                "{base}[http]\nallow_insecure = true\n[report]\nendpoint = \"http://otel.example\""
            ))
            .is_ok()
        );
    }

    #[test]
    fn default_fallback() {
        let cfg = merge(&blank_cli(), &TomlConfig::default());
        assert_eq!(cfg.global.app, "app");
        assert_eq!(cfg.global.dir, PathBuf::from("/srv/lode"));
        assert_eq!(cfg.global.log_level, "info");
        assert_eq!(cfg.update.policy, Policy::Check);
        assert_eq!(cfg.update.check_interval, 300);
        assert_eq!(cfg.update.keep_versions, 3);
        assert_eq!(cfg.update.channel, "stable");
        assert_eq!(cfg.update.github_api, "https://api.github.com");
        assert_eq!(cfg.trust.require_signature, RequireSignature::Auto);
        // No default launch command: unset stays None (resolved at launch, where a
        // manifest asset override may supply it).
        assert_eq!(cfg.command.run, None);
        assert_eq!(cfg.command.exec, None);
        assert_eq!(cfg.command.workdir, "{dir}");
        assert_eq!(cfg.supervise.readiness, Readiness::None);
        // Keep-alive by default: retry a failing app, then pause (don't exit).
        assert_eq!(cfg.supervise.restart, RestartPolicy::OnFailure);
        assert_eq!(cfg.supervise.restart_max, 3);
        assert_eq!(cfg.supervise.restart_mode, RestartMode::StopStart);
        assert_eq!(cfg.supervise.restart_backoff, 1);
        // prepare_timeout defaults to 0 = disabled (app-paced cut-over, §8).
        assert_eq!(cfg.supervise.prepare_timeout, 0);
        assert!(cfg.http.headers.is_empty());
        assert!(cfg.http.credential_hosts.is_empty());
        assert!(!cfg.http.allow_insecure); // secure by default
        assert!(validate(&cfg).is_ok());
    }

    const RESOURCE_BASE: &str = "[global]\napp = \"myapp\"\n[update]\n\
        manifest = \"https://r.example/m.json\"\nchannel = \"beta\"\npolicy = \"auto\"\n";

    #[test]
    fn resources_parse_with_defaults() {
        let cfg = Config::from_toml(&format!(
            "{RESOURCE_BASE}[[resource]]\nname = \"config\"\n\
             manifest = \"https://r.example/config.json\"\nasset = \"config.tar.gz\"\n\
             [[resource]]\nname = \"models\"\nmanifest = \"https://r.example/models.json\"\n\
             asset = \"models.zip\"\nchannel = \"stable\"\npolicy = \"check\"\n\
             notify = \"signal\"\nrequired = true\n"
        ))
        .unwrap();
        assert_eq!(
            cfg.resources,
            vec![
                Resource {
                    name: "config".to_owned(),
                    manifest: "https://r.example/config.json".to_owned(),
                    asset: "config.tar.gz".to_owned(),
                    channel: "beta".to_owned(),
                    policy: Policy::Auto,
                    notify: Notify::None,
                    required: false,
                },
                Resource {
                    name: "models".to_owned(),
                    manifest: "https://r.example/models.json".to_owned(),
                    asset: "models.zip".to_owned(),
                    channel: "stable".to_owned(),
                    policy: Policy::Check,
                    notify: Notify::Signal,
                    required: true,
                },
            ]
        );
        assert!(
            Config::from_toml(RESOURCE_BASE)
                .unwrap()
                .resources
                .is_empty()
        );
    }

    #[test]
    fn resources_and_app_names_are_validated() {
        let one = |body: &str| Config::from_toml(&format!("{RESOURCE_BASE}[[resource]]\n{body}"));
        assert!(one("name = \"c\"\nmanifest = \"https://r/c.json\"\nasset = \"c.zip\"\n").is_ok());
        // Bad name, missing/blank fields, unknown keys.
        assert!(
            one("name = \"../c\"\nmanifest = \"https://r/c.json\"\nasset = \"c.zip\"\n").is_err()
        );
        assert!(one("name = \"c\"\nasset = \"c.zip\"\n").is_err());
        assert!(one("name = \"c\"\nmanifest = \"https://r/c.json\"\nasset = \" \"\n").is_err());
        assert!(
            one("name = \"c\"\nmanifest = \"https://r/c.json\"\nasset = \"c.zip\"\nrun = \"x\"\n")
                .is_err()
        );
        // Duplicate names.
        let dup = "name = \"c\"\nmanifest = \"https://r/c.json\"\nasset = \"c.zip\"\n";
        assert!(one(&format!("{dup}[[resource]]\n{dup}")).is_err());
        // An app id never contains `/`: `<app>/<resource>` is a resource identity.
        assert!(Config::from_toml("[global]\napp = \"my/app\"\n").is_err());
    }

    #[test]
    fn revocation_url_from_toml_then_cli_overrides() {
        assert_eq!(
            merge_trust(&blank_cli(), &TomlTrust::default()).revocation_url,
            None
        );
        let t = TomlTrust {
            revocation_url: Some("https://r.example/rev.json".to_owned()),
            ..TomlTrust::default()
        };
        assert_eq!(
            merge_trust(&blank_cli(), &t).revocation_url.as_deref(),
            Some("https://r.example/rev.json")
        );
        let mut cli = blank_cli();
        cli.revocation_url = Some("https://other.example/rev.json".to_owned());
        assert_eq!(
            merge_trust(&cli, &t).revocation_url.as_deref(),
            Some("https://other.example/rev.json")
        );
    }

    #[test]
    fn credential_hosts_from_toml_then_cli_overrides() {
        // TOML supplies the allowlist when the CLI slot is empty…
        let t = TomlHttp {
            credential_hosts: Some(vec!["cdn.example".to_owned()]),
            ..TomlHttp::default()
        };
        assert_eq!(
            merge_http(&blank_cli(), &t).credential_hosts,
            vec!["cdn.example".to_owned()]
        );
        // …and `--credential-host` (repeatable) overrides it entirely.
        let mut cli = blank_cli();
        cli.credential_host = vec!["a.example".to_owned(), "b.example".to_owned()];
        assert_eq!(
            merge_http(&cli, &t).credential_hosts,
            vec!["a.example".to_owned(), "b.example".to_owned()]
        );
    }

    #[test]
    fn allow_insecure_http_precedence() {
        // Default: the gate is off.
        assert!(
            !merge(&blank_cli(), &TomlConfig::default())
                .http
                .allow_insecure
        );

        // TOML opts in.
        let t = TomlConfig {
            http: TomlHttp {
                allow_insecure: Some(true),
                ..TomlHttp::default()
            },
            ..TomlConfig::default()
        };
        assert!(merge(&blank_cli(), &t).http.allow_insecure);

        // The CLI flag forces it on even when TOML is unset/false.
        let mut cli = blank_cli();
        cli.allow_insecure_http = true;
        assert!(merge(&cli, &TomlConfig::default()).http.allow_insecure);
    }

    #[test]
    fn toml_only() {
        let t = TomlConfig {
            global: TomlGlobal {
                app: Some("myapp".to_owned()),
                ..TomlGlobal::default()
            },
            update: TomlUpdate {
                policy: Some(Policy::Auto),
                check_interval: Some(60),
                manifest: Some("https://example.com/m.json".to_owned()),
                ..TomlUpdate::default()
            },
            trust: TomlTrust {
                trusted_keys: Some(vec!["id:key".to_owned()]),
                ..TomlTrust::default()
            },
            command: TomlCommand {
                run: Some("bun run".to_owned()),
                ..TomlCommand::default()
            },
            ..TomlConfig::default()
        };
        let cfg = merge(&blank_cli(), &t);
        assert_eq!(cfg.global.app, "myapp");
        assert_eq!(cfg.update.policy, Policy::Auto);
        assert_eq!(cfg.update.check_interval, 60);
        assert_eq!(
            cfg.update.manifest.as_deref(),
            Some("https://example.com/m.json")
        );
        assert_eq!(cfg.trust.trusted_keys, vec!["id:key".to_owned()]);
        assert_eq!(cfg.command.run.as_deref(), Some("bun run"));
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn cli_overrides_toml() {
        // The CLI/env slot (clap folds env into these same fields) wins over TOML;
        // CLI-over-env within the slot is clap's own contract.
        let t = TomlConfig {
            global: TomlGlobal {
                app: Some("from_toml".to_owned()),
                ..TomlGlobal::default()
            },
            update: TomlUpdate {
                policy: Some(Policy::Auto),
                check_interval: Some(60),
                ..TomlUpdate::default()
            },
            ..TomlConfig::default()
        };

        let mut cli = blank_cli();
        cli.policy = Some(Policy::Off);
        cli.interval = Some(9);
        cli.app = Some("from_cli".to_owned());

        let cfg = merge(&cli, &t);
        assert_eq!(cfg.update.policy, Policy::Off);
        assert_eq!(cfg.update.check_interval, 9);
        assert_eq!(cfg.global.app, "from_cli");
    }

    #[test]
    fn parses_partial_toml_with_defaulted_sections() {
        // Missing sections and missing keys must default rather than error, so a
        // sparse lode.toml is valid.
        let parsed: TomlConfig =
            toml::from_str("[update]\npolicy = \"auto\"\nmanifest = \"https://x/m.json\"\n")
                .unwrap();
        let cfg = merge(&blank_cli(), &parsed);
        assert_eq!(cfg.update.policy, Policy::Auto);
        assert_eq!(cfg.update.manifest.as_deref(), Some("https://x/m.json"));
        assert_eq!(cfg.global.app, "app"); // default, no [global] table present
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn parses_example_toml() {
        // The shipped full reference must round-trip through the parser + merge.
        let text = include_str!("../../../docs/lode.example.toml");
        let parsed: TomlConfig = toml::from_str(text).unwrap();
        let cfg = merge(&blank_cli(), &parsed);
        assert_eq!(cfg.global.app, "myapp");
        assert_eq!(cfg.update.policy, Policy::Check);
        assert_eq!(cfg.command.run.as_deref(), Some("bun run app.js"));
        assert_eq!(cfg.command.exec.as_deref(), Some("bun"));
        assert_eq!(cfg.supervise.restart_mode, RestartMode::StopStart);
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn parses_starter_toml() {
        // The minimal scaffolded starter must parse + validate against the structs.
        let parsed: TomlConfig = toml::from_str(STARTER_TOML).unwrap();
        let cfg = merge(&blank_cli(), &parsed);
        assert_eq!(cfg.global.app, "myapp");
        assert_eq!(cfg.command.run.as_deref(), Some("./myapp"));
        assert_eq!(cfg.command.exec.as_deref(), Some("./myapp"));
        assert!(cfg.update.manifest.is_some());
        assert_eq!(
            cfg.update.asset.as_deref(),
            Some("myapp-linux-x86_64.tar.gz")
        );
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn parses_toml_without_command_section() {
        // A lode.toml with no [command] at all is valid: a manifest asset may
        // supply the launch command, so the gap is resolved at launch time.
        let parsed: TomlConfig =
            toml::from_str("[update]\nmanifest = \"https://x/m.json\"\n").unwrap();
        let cfg = merge(&blank_cli(), &parsed);
        assert_eq!(cfg.command.run, None);
        assert_eq!(cfg.command.exec, None);
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn seed_run_command_mirrors_landing_rules() {
        // raw keeps the filename; .gz drops the suffix; archives fall back to the
        // `./{app}` convention (operator edits the scaffold otherwise).
        assert_eq!(
            seed_run_command(Path::new("/x/myapp.bin"), "app"),
            "./myapp.bin"
        );
        assert_eq!(seed_run_command(Path::new("/x/tool.gz"), "app"), "./tool");
        assert_eq!(
            seed_run_command(Path::new("/x/rel.tar.gz"), "myapp"),
            "./myapp"
        );
        assert_eq!(
            seed_run_command(Path::new("/x/rel.zip"), "myapp"),
            "./myapp"
        );
    }

    #[test]
    fn unknown_toml_key_rejected_and_named() {
        // An unknown key (e.g. a typo) must be a hard parse error naming the key
        // (operators fix lode.toml while the app is paused — a silent no-op strands them).
        let err = toml::from_str::<TomlConfig>("[update]\nbogus_key = \"stable\"\n").unwrap_err();
        let rendered = crate::error::Error::from(err).to_string();
        assert!(rendered.contains("bogus_key"), "got: {rendered}");

        // ...in every section, including the top level.
        assert!(toml::from_str::<TomlConfig>("[globall]\napp = \"x\"\n").is_err());
        assert!(toml::from_str::<TomlConfig>("[supervise]\nrestart_maxx = 3\n").is_err());

        // `[env]` stays open: arbitrary user-defined variable names are the point.
        let parsed: TomlConfig = toml::from_str("[env]\nMY_CUSTOM_VAR = \"1\"\n").unwrap();
        assert_eq!(
            parsed.env.get("MY_CUSTOM_VAR").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn cli_log_level_overrides_toml() {
        let t = TomlConfig {
            global: TomlGlobal {
                log_level: Some("debug".to_owned()),
                ..TomlGlobal::default()
            },
            ..TomlConfig::default()
        };
        // TOML supplies the level when the CLI/env slot is unset…
        assert_eq!(merge(&blank_cli(), &t).global.log_level, "debug");
        // …and an explicit CLI/env value wins over it.
        let mut cli = blank_cli();
        cli.log_level = Some("warn".to_owned());
        assert_eq!(merge(&cli, &t).global.log_level, "warn");
    }

    #[test]
    fn peek_log_level_reads_toml_leniently() {
        let dir = std::env::temp_dir().join(format!("lode-peek-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lode.toml");

        // A config file with a level → Some(level).
        std::fs::write(&path, "[global]\nlog_level = \"debug\"\n").unwrap();
        assert_eq!(peek_log_level(&path).as_deref(), Some("debug"));

        // Present but without the key → None.
        std::fs::write(&path, "[global]\napp = \"x\"\n").unwrap();
        assert_eq!(peek_log_level(&path), None);

        // Malformed → None (full resolve reports the error later, logging up).
        std::fs::write(&path, "[global\nlog_level = ").unwrap();
        assert_eq!(peek_log_level(&path), None);

        // Absent → None.
        assert_eq!(peek_log_level(&dir.join("missing.toml")), None);

        // An explicit path wins over the search; the search falls back to
        // `<dir>/lode.toml` and reports None when nothing is there.
        assert_eq!(
            find_config_path(Some("/tmp/explicit.toml"), Some("/tmp")),
            Some(PathBuf::from("/tmp/explicit.toml"))
        );
        assert_eq!(
            find_config_path(None, dir.to_str()),
            Some(dir.join("lode.toml"))
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_key_fields_rejected() {
        // app
        let mut cli = blank_cli();
        cli.app = Some(String::new());
        let err = validate(&merge(&cli, &TomlConfig::default())).unwrap_err();
        assert!(err.to_string().contains("global.app"), "got: {err}");

        // channel
        let mut cli = blank_cli();
        cli.channel = Some("  ".to_owned());
        let err = validate(&merge(&cli, &TomlConfig::default())).unwrap_err();
        assert!(err.to_string().contains("update.channel"), "got: {err}");

        // asset (only when set — None stays fine)
        let mut cli = blank_cli();
        cli.asset = Some(String::new());
        let err = validate(&merge(&cli, &TomlConfig::default())).unwrap_err();
        assert!(err.to_string().contains("update.asset"), "got: {err}");

        // run/exec are deliberately NOT validated here: an empty/unset command is
        // resolved (or clearly rejected) at launch, where the manifest override is
        // known — see supervisor::effective_command.
        let mut cli = blank_cli();
        cli.run = Some(String::new());
        assert!(validate(&merge(&cli, &TomlConfig::default())).is_ok());
    }

    #[test]
    fn manifest_github_xor_rejected() {
        let mut cli = blank_cli();
        cli.manifest = Some("https://example.com/m.json".to_owned());
        cli.github = Some("owner/name".to_owned());
        let cfg = merge(&cli, &TomlConfig::default());
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn runtime_sha256_must_be_64_hex() {
        let good = "a".repeat(64);
        let cfg = Config::from_toml(&format!(
            "[runtime]\nruntime = \"bun\"\ndownload = \"https://x/bun.zip\"\nsha256 = \"{good}\"\n"
        ))
        .unwrap();
        assert_eq!(cfg.runtime.sha256.as_deref(), Some(good.as_str()));
        for bad in ["abc", &"g".repeat(64), &"a".repeat(63), "sha256:deadbeef"] {
            let err = Config::from_toml(&format!(
                "[runtime]\nruntime = \"bun\"\ndownload = \"https://x/bun.zip\"\nsha256 = \"{bad}\"\n"
            ))
            .unwrap_err();
            assert!(matches!(err, Error::Config(_)), "{bad:?}: {err}");
        }
    }

    #[test]
    fn backoff_range_rejected() {
        let mut cli = blank_cli();
        cli.restart_backoff = Some(60);
        cli.restart_backoff_max = Some(30);
        let cfg = merge(&cli, &TomlConfig::default());
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn split_csv_trims_and_drops_empties() {
        assert_eq!(split_csv(" a , b ,, c "), vec!["a", "b", "c"]);
        assert!(split_csv("").is_empty());
    }

    #[test]
    fn from_toml_parses_merges_and_validates() {
        // Pure clap-free path: parse text → merge over defaults → validate. The
        // empty override slot means TOML wins where set, defaults fill the rest.
        let cfg = Config::from_toml(
            "[global]\napp = \"fromtoml\"\n\
             [update]\npolicy = \"auto\"\nmanifest = \"https://x/m.json\"\n\
             [command]\nrun = \"bun run app.js\"\n",
        )
        .unwrap();
        assert_eq!(cfg.global.app, "fromtoml");
        assert_eq!(cfg.update.policy, Policy::Auto);
        assert_eq!(cfg.update.manifest.as_deref(), Some("https://x/m.json"));
        assert_eq!(cfg.command.run.as_deref(), Some("bun run app.js"));
        // No located file → no config_path; untouched sections take defaults.
        assert!(cfg.config_path.is_none());
        assert_eq!(cfg.update.channel, "stable");
        assert_eq!(cfg.supervise.restart, RestartPolicy::OnFailure);
    }

    #[test]
    fn require_manifest_defaults_off_and_needs_a_native_source() {
        let native = "[update]\nmanifest = \"https://x/m.json\"\n";
        let cfg = Config::from_toml(native).unwrap();
        assert_eq!(cfg.trust.require_manifest, RequireManifest::Off);

        let cfg =
            Config::from_toml(&format!("{native}[trust]\nrequire_manifest = \"fresh\"\n")).unwrap();
        assert_eq!(cfg.trust.require_manifest, RequireManifest::Fresh);

        // The override layer (CLI / env) wins over the file.
        let mut cli = blank_cli();
        cli.manifest = Some("https://x/m.json".to_owned());
        cli.require_manifest = Some(RequireManifest::Signed);
        let toml: TomlConfig = toml::from_str("[trust]\nrequire_manifest = \"fresh\"\n").unwrap();
        assert_eq!(
            merge(&cli, &toml).trust.require_manifest,
            RequireManifest::Signed
        );

        // A GitHub release has no manifest to sign.
        let err = Config::from_toml(
            "[update]\ngithub = \"o/r\"\n[trust]\nrequire_manifest = \"signed\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("native manifest"), "got: {err}");
    }

    #[test]
    fn from_toml_surfaces_parse_and_validate_errors() {
        // Unknown key → parse error (same deny_unknown_fields contract as resolve).
        assert!(Config::from_toml("[update]\nbogus_key = 1\n").is_err());
        // Mutually-exclusive sources → validate error.
        let err = Config::from_toml(
            "[update]\nmanifest = \"https://x/m.json\"\ngithub = \"owner/name\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn builder_produces_validated_config() {
        let cfg = Config::builder()
            .app("built")
            .dir("/tmp/built")
            .manifest("https://x/m.json")
            .asset("built-linux-x86_64.tar.gz")
            .channel("beta")
            .policy(Policy::Auto)
            .pin("v1.2.3")
            .run("./built")
            .exec("./built")
            .build()
            .unwrap();
        assert_eq!(cfg.global.app, "built");
        assert_eq!(cfg.global.dir, PathBuf::from("/tmp/built"));
        assert_eq!(cfg.update.manifest.as_deref(), Some("https://x/m.json"));
        assert_eq!(
            cfg.update.asset.as_deref(),
            Some("built-linux-x86_64.tar.gz")
        );
        assert_eq!(cfg.update.channel, "beta");
        assert_eq!(cfg.update.policy, Policy::Auto);
        assert_eq!(cfg.update.pin.as_deref(), Some("v1.2.3"));
        assert_eq!(cfg.command.run.as_deref(), Some("./built"));
        assert_eq!(cfg.command.exec.as_deref(), Some("./built"));
        // Unset knobs still fall through to the design defaults.
        assert_eq!(cfg.supervise.restart, RestartPolicy::OnFailure);
        assert!(cfg.config_path.is_none());
    }

    #[test]
    fn builder_sets_report() {
        let cfg = Config::builder()
            .report_endpoint("https://otel.example/r/otlp")
            .report_interval(60)
            .report_header("Authorization: Bearer ${T}")
            .report_header("X-Team: ops")
            .report_attribute("region", "hk")
            .report_instance_id("web-01")
            .build()
            .unwrap();
        assert_eq!(
            cfg.report.endpoint.as_deref(),
            Some("https://otel.example/r/otlp")
        );
        assert_eq!(cfg.report.interval, 60);
        assert_eq!(
            cfg.report.headers,
            ["Authorization: Bearer ${T}", "X-Team: ops"]
        );
        assert_eq!(cfg.report.attributes["region"], "hk");
        assert_eq!(cfg.report.instance_id.as_deref(), Some("web-01"));
        // Off by default.
        let cfg = Config::builder().build().unwrap();
        assert!(cfg.report.endpoint.is_none());
        assert_eq!(cfg.report.interval, DEFAULT_REPORT_INTERVAL);
    }

    #[test]
    fn builder_validates_report() {
        let err = Config::builder()
            .report_endpoint("http://otel.example")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("report.endpoint"), "got: {err}");
        let err = Config::builder()
            .report_endpoint("https://otel.example")
            .report_interval(0)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("report.interval"), "got: {err}");
    }

    #[test]
    fn builder_runs_the_same_validate() {
        // Empty app fails the same semantic check `resolve`/`from_toml` apply.
        let err = Config::builder().app("").build().unwrap_err();
        assert!(err.to_string().contains("global.app"), "got: {err}");
        // Both sources set is rejected too.
        let err = Config::builder()
            .manifest("https://x/m.json")
            .github("owner/name")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "got: {err}");
    }
}
