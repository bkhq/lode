//! `lode-cli publish` — release one version of a package to a lode-server
//! registry: register the signing key, create the draft, upload every asset,
//! publish to a channel. With a signing key the release is a signed manifest
//! commit ([`commit`]); `lode-cli yank` and `channel` make commits too.
//!
//! Everything that can be checked locally is checked before the first request.
//! Re-running after a failure continues into the draft the earlier run left.
//! Asset bodies are streamed from disk with a declared length. The token is read
//! from `LODE_REGISTRY_TOKEN` only and never appears in output.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Seek as _, SeekFrom, Write as _};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use lode_core::manifest::RuntimeSpec;
use lode_core::verify::{Algorithm, Artifact, B64, sha256_hex_file};

use crate::authoring::{self, SecretKey};
use crate::cli::{ChannelArgs, CommitTarget, PublishArgs, YankArgs};

mod commit;

/// Registry used when `--registry` is not given.
pub(crate) const DEFAULT_REGISTRY: &str = "https://lode.bk.io/u";
const TOKEN_ENV: &str = "LODE_REGISTRY_TOKEN";
/// Largest asset the registry takes in one request (95 MiB).
const SINGLE_REQUEST_MAX: u64 = 99_614_720;
/// Largest asset the registry takes at all (500 MiB).
const ASSET_MAX: u64 = 524_288_000;
/// Tries per part, counting the first.
const PART_ATTEMPTS: u32 = 3;
const RETRY_PAUSE: Duration = Duration::from_secs(2);
/// End-to-end cap for a JSON call; completing a large upload makes the registry
/// re-read the whole object.
const CALL_CAP: Duration = Duration::from_mins(10);
/// End-to-end cap for one streamed body.
const UPLOAD_CAP: Duration = Duration::from_hours(1);
const REPLY_LIMIT: u64 = 1024 * 1024;

/// One `publish` run, as asked for on the command line.
struct Request<'a> {
    registry: &'a str,
    package: &'a str,
    version: &'a str,
    channel: &'a str,
    notes: Option<&'a str>,
    key: Option<&'a str>,
    key_env: Option<&'a str>,
    run: Option<&'a str>,
    exec: Option<&'a str>,
    runtime: Option<&'a RuntimeSpec>,
    allow_insecure: bool,
    files: &'a [String],
}

/// What a run takes from its surroundings rather than from the command line.
struct Env<'a> {
    token: &'a str,
    interrupted: &'a AtomicBool,
    retry_pause: Duration,
    single_max: u64,
    asset_max: u64,
}

/// `publish` — see the module docs. Failures are reported on stderr; stdout
/// carries the progress lines.
pub(crate) fn command(args: &PublishArgs) -> ExitCode {
    match publish(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "publish: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn publish(args: &PublishArgs) -> Result<()> {
    let runtime = args.asset_runtime.spec()?;
    let req = Request {
        registry: &args.registry,
        package: &args.package,
        version: &args.version,
        channel: &args.channel,
        notes: args.notes.as_deref(),
        key: args.key.as_deref(),
        key_env: args.key_env.as_deref(),
        run: args.run.as_deref(),
        exec: args.exec.as_deref(),
        runtime: runtime.as_ref(),
        allow_insecure: args.allow_insecure,
        files: &args.files,
    };
    let token = Zeroizing::new(std::env::var(TOKEN_ENV).unwrap_or_default());
    let interrupted = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&interrupted))
            .context("install signal handlers")?;
    }
    let env = Env {
        token: &token,
        interrupted: &interrupted,
        retry_pause: RETRY_PAUSE,
        single_max: SINGLE_REQUEST_MAX,
        asset_max: ASSET_MAX,
    };
    run(&req, &env, &mut io::stdout().lock())
}

fn run(req: &Request<'_>, env: &Env<'_>, out: &mut dyn io::Write) -> Result<()> {
    let assets = validate(req, env)?;
    let key = registry_key(req.key, req.key_env)?;
    let client = Client::new(req.registry, env);
    let package = req.package;
    let version = req.version;
    let drafts_url = format!("{}/packages/{package}/versions", client.api);
    let version_url = format!("{drafts_url}/{version}");

    if let Some(key) = &key {
        register_key(&client, package, key, out)?;
    }
    create_draft(&client, req, &drafts_url, &version_url, out)?;

    let mut entries = Vec::with_capacity(assets.len());
    for asset in &assets {
        client.check_interrupt()?;
        // The registry verifies the signature under the package slug, which is
        // the manifest `name` an operator sets as `[global].app`.
        let artifact = Artifact {
            app: package.split_once('/').map_or(package, |(_, slug)| slug),
            name: &asset.name,
            version,
            path: &asset.path,
            run: req.run,
            exec: req.exec,
            runtime: req.runtime,
        };
        let (sha256, sig) = match &key {
            Some(key) => {
                let (sha256, sig, _) = authoring::sign_artifact(&artifact, key)
                    .with_context(|| format!("sign {}", asset.name))?;
                (sha256, Some(sig))
            }
            None => (
                sha256_hex_file(Path::new(&asset.path)).map_err(|e| anyhow!("{e}"))?,
                None,
            ),
        };
        let signed = if sig.is_some() { ", signed" } else { "" };
        writeln!(
            out,
            "publish: uploading {} ({} bytes{signed})",
            asset.name, asset.size
        )?;
        let upload = Upload {
            asset,
            sha256: &sha256,
            sig: sig.as_deref(),
            run: req.run,
            exec: req.exec,
            runtime: req.runtime,
        };
        if asset.size <= env.single_max {
            upload_single(&client, &version_url, &upload)?;
        } else {
            upload_multipart(&client, &version_url, &upload, out)?;
        }
        entries.push(manifest_asset(&upload, key.as_ref()));
    }

    if let Some(key) = &key {
        let change = commit::Change::Publish {
            version,
            channel: req.channel,
            notes: req.notes,
            assets: entries,
        };
        commit::commit(&client, package, key, &change, out)?;
        writeln!(
            out,
            "publish: published {package}@{version} to {}",
            req.channel
        )?;
        return Ok(());
    }

    client.check_interrupt()?;
    let reply = client.post(
        &format!("{version_url}/publish"),
        Some(&json!({ "channel": req.channel })),
    )?;
    let step = format!("publication of {package}@{version} to {}", req.channel);
    reply.accept(&step)?;
    writeln!(
        out,
        "publish: published {package}@{version} to {}",
        req.channel
    )?;
    Ok(())
}

/// The manifest asset object of an uploaded asset; the registry fills in `url`.
fn manifest_asset(upload: &Upload<'_>, key: Option<&SecretKey>) -> Value {
    let mut asset = json!({
        "name": upload.asset.name,
        "url": "",
        "sha256": upload.sha256,
        "size": upload.asset.size,
    });
    if let (Some(sig), Some(key)) = (upload.sig, key) {
        asset["sig"] = json!(sig);
        asset["key_id"] = json!(key.public().key_id());
    }
    for (field, value) in [("run", upload.run), ("exec", upload.exec)] {
        if let Some(value) = value {
            asset[field] = json!(value);
        }
    }
    if let Some(rt) = upload.runtime {
        asset["runtime"] = json!(rt);
    }
    asset
}

/// `yank` — see [`commit::Change::Yank`].
pub(crate) fn yank(args: &YankArgs) -> ExitCode {
    let change = commit::Change::Yank {
        version: &args.version,
    };
    report("yank", commit_to(&args.target, &change))
}

/// `channel` — see [`commit::Change::Channel`].
pub(crate) fn channel(args: &ChannelArgs) -> ExitCode {
    let change = commit::Change::Channel {
        channel: &args.channel,
        version: &args.version,
    };
    report("channel", commit_to(&args.target, &change))
}

fn report(command: &str, result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "{command}: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn commit_to(target: &CommitTarget, change: &commit::Change<'_>) -> Result<()> {
    let token = Zeroizing::new(std::env::var(TOKEN_ENV).unwrap_or_default());
    let interrupted = AtomicBool::new(false);
    let env = Env {
        token: &token,
        interrupted: &interrupted,
        retry_pause: RETRY_PAUSE,
        single_max: SINGLE_REQUEST_MAX,
        asset_max: ASSET_MAX,
    };
    commit_with(target, change, &env, &mut io::stdout().lock())
}

fn commit_with(
    target: &CommitTarget,
    change: &commit::Change<'_>,
    env: &Env<'_>,
    out: &mut dyn io::Write,
) -> Result<()> {
    check_registry(&target.registry, target.allow_insecure)?;
    if !is_package(&target.package) {
        bail!("--package must be <namespace>/<slug>");
    }
    match change {
        commit::Change::Yank { version } => check_id("--version", version)?,
        commit::Change::Channel { channel, version } => {
            check_id("--channel", channel)?;
            check_id("--version", version)?;
        }
        commit::Change::Publish { .. } => {}
    }
    check_token(env.token)?;
    let key = registry_key(target.key.as_deref(), target.key_env.as_deref())?
        .context("a commit is signed: pass --key or --key-env")?;
    let client = Client::new(&target.registry, env);
    commit::commit(&client, &target.package, &key, change, out)
}

// --- local checks -----------------------------------------------------------

/// One asset file, checked.
struct Asset {
    path: String,
    name: String,
    size: u64,
}

/// lode's id rule: `[A-Za-z0-9._-]`, not empty, no leading `.` or `-`.
fn is_id(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(['.', '-'])
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `<namespace>/<slug>`: `^[a-z][a-z0-9]*/[a-z0-9][a-z0-9-]*$`.
fn is_package(value: &str) -> bool {
    let Some((namespace, slug)) = value.split_once('/') else {
        return false;
    };
    let lower_or_digit = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    namespace.starts_with(|c: char| c.is_ascii_lowercase())
        && namespace.bytes().all(lower_or_digit)
        && slug.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && slug.bytes().all(|b| lower_or_digit(b) || b == b'-')
}

const ID_RULE: &str = "may only contain A-Z a-z 0-9 . _ - and must not start with . or -";

fn check_registry(registry: &str, allow_insecure: bool) -> Result<()> {
    if !(registry.starts_with("https://") || registry.starts_with("http://")) {
        bail!("--registry must be an http(s) URL");
    }
    if !allow_insecure && !lode_core::http::scheme_allowed(registry) {
        bail!("refusing a non-HTTPS registry; pass --allow-insecure to override");
    }
    Ok(())
}

fn check_id(flag: &str, value: &str) -> Result<()> {
    if !is_id(value) {
        bail!("{flag} {ID_RULE}");
    }
    Ok(())
}

fn check_token(token: &str) -> Result<()> {
    if token.is_empty() {
        bail!("{TOKEN_ENV} is not set");
    }
    // The token becomes a header value, so it has to be plain.
    let plain = |b: u8| b.is_ascii_alphanumeric() || b"._~+/=-".contains(&b);
    if !token.bytes().all(plain) {
        bail!("{TOKEN_ENV} contains characters a token never has");
    }
    Ok(())
}

/// Check the arguments, the files and the token; nothing is sent.
fn validate(req: &Request<'_>, env: &Env<'_>) -> Result<Vec<Asset>> {
    check_registry(req.registry, req.allow_insecure)?;
    if !is_package(req.package) {
        bail!("--package must be <namespace>/<slug>");
    }
    check_id("--version", req.version)?;
    check_id("--channel", req.channel)?;
    authoring::validate_overrides(req.run, req.exec)?;
    if req.files.is_empty() {
        bail!("no asset files given");
    }

    let mut assets: Vec<Asset> = Vec::with_capacity(req.files.len());
    for path in req.files {
        let name = authoring::basename(path).to_owned();
        let meta = std::fs::metadata(path).map_err(|e| anyhow!("{path}: {e}"))?;
        if !meta.is_file() {
            bail!("not a file: {path}");
        }
        File::open(path).map_err(|e| anyhow!("not readable: {path}: {e}"))?;
        if !is_id(&name) {
            bail!("asset name {name:?} {ID_RULE}");
        }
        if assets.iter().any(|a| a.name == name) {
            bail!("asset name {name:?} is given twice");
        }
        let size = meta.len();
        if size == 0 {
            bail!("{name} is empty");
        }
        if size > env.asset_max {
            bail!(
                "{name} is {size} bytes; the registry takes at most {} per asset",
                env.asset_max
            );
        }
        assets.push(Asset {
            path: path.clone(),
            name,
            size,
        });
    }

    check_token(env.token)?;
    Ok(assets)
}

/// The signing key, when one was given. The registry verifies ed25519 only.
fn registry_key(key: Option<&str>, key_env: Option<&str>) -> Result<Option<SecretKey>> {
    if key.is_none() && key_env.is_none() {
        return Ok(None);
    }
    let key = authoring::resolve_sign_key(key, key_env)?;
    if key.algorithm() != Algorithm::Ed25519 {
        bail!(
            "the signing key is {}; the registry accepts ed25519 keys only",
            key.algorithm()
        );
    }
    Ok(Some(key))
}

// --- steps ------------------------------------------------------------------

fn register_key(
    client: &Client<'_>,
    package: &str,
    key: &SecretKey,
    out: &mut dyn io::Write,
) -> Result<()> {
    client.check_interrupt()?;
    let public = key.public();
    let body = json!({ "public_key": B64.encode(public.to_bytes()), "name": "ci" });
    let reply = client.post(
        &format!("{}/packages/{package}/keys", client.api),
        Some(&body),
    )?;
    if reply.ok() {
        writeln!(out, "publish: registered signing key {}", public.key_id())?;
    } else if reply.is(409, "KEY_EXISTS") {
        writeln!(out, "publish: signing key already registered")?;
    } else {
        return Err(reply.refused("registering the signing key"));
    }
    Ok(())
}

fn create_draft(
    client: &Client<'_>,
    req: &Request<'_>,
    drafts_url: &str,
    version_url: &str,
    out: &mut dyn io::Write,
) -> Result<()> {
    client.check_interrupt()?;
    let target = format!("{}@{}", req.package, req.version);
    let mut body = json!({ "version": req.version });
    if let Some(notes) = req.notes {
        body["notes"] = json!(notes);
    }
    let reply = client.post(drafts_url, Some(&body))?;
    if reply.ok() {
        writeln!(out, "publish: created draft {target}")?;
        return Ok(());
    }
    if !reply.is(409, "VERSION_EXISTS") {
        return Err(reply.refused(&format!("creation of the draft {target}")));
    }
    let found = client.get(version_url)?;
    found.accept(&format!("lookup of {target}"))?;
    let status = found.data()["version"]["status"]
        .as_str()
        .unwrap_or("unknown");
    if status != "draft" {
        bail!("{target} is already {status}; a released version cannot be changed");
    }
    writeln!(out, "publish: continuing into the existing draft {target}")?;
    Ok(())
}

/// One asset on its way up.
struct Upload<'a> {
    asset: &'a Asset,
    sha256: &'a str,
    sig: Option<&'a str>,
    run: Option<&'a str>,
    exec: Option<&'a str>,
    runtime: Option<&'a RuntimeSpec>,
}

impl Upload<'_> {
    /// The optional fields sent with the asset, by registry parameter name.
    fn fields(&self) -> [(&'static str, Option<&str>); 8] {
        let rt = self.runtime;
        [
            ("sig", self.sig),
            ("run", self.run),
            ("exec", self.exec),
            ("runtime", rt.map(|rt| rt.runtime.as_str())),
            ("runtime_version", rt.and_then(|rt| rt.version.as_deref())),
            (
                "runtime_version_check",
                rt.and_then(|rt| rt.version_check.as_deref()),
            ),
            ("runtime_download", rt.and_then(|rt| rt.download.as_deref())),
            ("runtime_sha256", rt.and_then(|rt| rt.sha256.as_deref())),
        ]
    }
}

fn upload_single(client: &Client<'_>, version_url: &str, upload: &Upload<'_>) -> Result<()> {
    let name = &upload.asset.name;
    let mut url = format!("{version_url}/assets/{name}?sha256={}", upload.sha256);
    for (key, value) in upload.fields() {
        if let Some(value) = value {
            let _ = write!(url, "&{key}={}", percent_encode(value));
        }
    }
    let step = format!("upload of {name}");
    client
        .put_range(&url, &upload.asset.path, 0, upload.asset.size, &step)?
        .accept(&step)
}

fn upload_multipart(
    client: &Client<'_>,
    version_url: &str,
    upload: &Upload<'_>,
    out: &mut dyn io::Write,
) -> Result<()> {
    let name = &upload.asset.name;
    let size = upload.asset.size;
    let mut body = json!({ "asset": name, "size": size, "sha256": upload.sha256 });
    for (key, value) in upload.fields() {
        if let Some(value) = value {
            body[key] = json!(value);
        }
    }
    let step = format!("start of the multipart upload of {name}");
    let reply = client.post(&format!("{version_url}/uploads"), Some(&body))?;
    reply.accept(&step)?;
    let data = reply.data();
    let id = data["uploadId"].as_str().filter(|id| is_id(id));
    let (Some(id), Some(part_size), Some(part_count)) =
        (id, data["partSize"].as_u64(), data["partCount"].as_u64())
    else {
        bail!("{step}: the registry's answer names no usable upload");
    };
    let upload_url = format!("{}/uploads/{id}", client.api);

    let sent = plan_parts(size, part_size, part_count)
        .and_then(|parts| send_parts(client, &upload_url, upload.asset, &parts, out))
        .and_then(|()| {
            client.check_interrupt()?;
            client
                .post(&format!("{upload_url}/complete"), None)?
                .accept(&format!("completion of the multipart upload of {name}"))
        });
    if sent.is_err() {
        // Give the upload up, best effort: its parts and reservation go.
        let _ = client.delete(&upload_url);
    }
    sent
}

/// The `(offset, length)` of each part, from what the registry dictated.
fn plan_parts(size: u64, part_size: u64, part_count: u64) -> Result<Vec<(u64, u64)>> {
    if part_size == 0 || part_count != size.div_ceil(part_size) {
        bail!("the registry's part plan ({part_count} x {part_size}) does not cover {size} bytes");
    }
    Ok((0..part_count)
        .map(|n| {
            let offset = n * part_size;
            (offset, part_size.min(size - offset))
        })
        .collect())
}

fn send_parts(
    client: &Client<'_>,
    upload_url: &str,
    asset: &Asset,
    parts: &[(u64, u64)],
    out: &mut dyn io::Write,
) -> Result<()> {
    let total = parts.len();
    for (index, &(offset, len)) in parts.iter().enumerate() {
        let n = index + 1;
        let url = format!("{upload_url}/parts/{n}");
        let step = format!("upload of {}, part {n}/{total}", asset.name);
        send_part(client, &url, asset, offset, len, &step)?;
        writeln!(out, "publish: {}: part {n}/{total}", asset.name)?;
    }
    Ok(())
}

/// Send one part. Parts are idempotent per number, so a network error or a 5xx
/// is retried a fixed number of times; a 4xx never is.
fn send_part(
    client: &Client<'_>,
    url: &str,
    asset: &Asset,
    offset: u64,
    len: u64,
    step: &str,
) -> Result<()> {
    let mut attempt = 1;
    loop {
        client.check_interrupt()?;
        let last = attempt == PART_ATTEMPTS;
        match client.put_range(url, &asset.path, offset, len, step) {
            Ok(reply) if reply.ok() => return Ok(()),
            Ok(reply) if reply.status < 500 || last => return Err(reply.refused(step)),
            Err(error) if last || client.interrupted() => return Err(error),
            Ok(_) | Err(_) => {}
        }
        attempt += 1;
        std::thread::sleep(client.retry_pause);
    }
}

/// Percent-encode a query value: everything but RFC 3986 unreserved characters.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

// --- wire -------------------------------------------------------------------

/// The registry's answer to one request.
struct Reply {
    status: u16,
    /// The JSON body, when there is one; gateway errors carry none.
    body: Value,
}

impl Reply {
    const fn ok(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    fn code(&self) -> Option<&str> {
        self.body["error"]["code"].as_str()
    }

    fn is(&self, status: u16, code: &str) -> bool {
        self.status == status && self.code() == Some(code)
    }

    fn data(&self) -> &Value {
        &self.body["data"]
    }

    /// The error for a refused step: the HTTP status and the API's code and message.
    fn refused(&self, step: &str) -> anyhow::Error {
        let mut text = format!("{step}: HTTP {}", self.status);
        if let Some(code) = self.code() {
            let _ = write!(text, " {code}");
        }
        if let Some(message) = self.body["error"]["message"].as_str() {
            let _ = write!(text, ": {message}");
        }
        anyhow!(text)
    }

    fn accept(&self, step: &str) -> Result<()> {
        if self.ok() {
            Ok(())
        } else {
            Err(self.refused(step))
        }
    }
}

/// A file range that stops reading once the run is interrupted.
struct Guarded<'a, R> {
    inner: R,
    interrupted: &'a AtomicBool,
}

impl<R: Read> Read for Guarded<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.interrupted.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
        }
        self.inner.read(buf)
    }
}

struct Client<'a> {
    agent: ureq::Agent,
    /// `<registry>/api/registry`.
    api: String,
    authorization: Zeroizing<String>,
    interrupted: &'a AtomicBool,
    retry_pause: Duration,
}

impl<'a> Client<'a> {
    fn new(registry: &str, env: &Env<'a>) -> Self {
        // Redirects are not followed: the token is for the registry host only.
        let agent = ureq::config::Config::builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .new_agent();
        Self {
            agent,
            api: format!("{}/api/registry", registry.trim_end_matches('/')),
            authorization: Zeroizing::new(format!("Bearer {}", env.token)),
            interrupted: env.interrupted,
            retry_pause: env.retry_pause,
        }
    }

    fn interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Relaxed)
    }

    fn check_interrupt(&self) -> Result<()> {
        if self.interrupted() {
            bail!("interrupted");
        }
        Ok(())
    }

    fn post(&self, url: &str, body: Option<&Value>) -> Result<Reply> {
        let request = self
            .agent
            .post(url)
            .header("Authorization", self.authorization.as_str())
            .config()
            .timeout_global(Some(CALL_CAP))
            .build();
        let result = match body {
            Some(body) => request
                .content_type("application/json")
                .send(serde_json::to_vec(body)?.as_slice()),
            None => request.send_empty(),
        };
        reply(result)
    }

    fn get(&self, url: &str) -> Result<Reply> {
        let result = self
            .agent
            .get(url)
            .header("Authorization", self.authorization.as_str())
            .config()
            .timeout_global(Some(CALL_CAP))
            .build()
            .call();
        reply(result)
    }

    fn delete(&self, url: &str) -> Result<Reply> {
        let result = self
            .agent
            .delete(url)
            .header("Authorization", self.authorization.as_str())
            .config()
            .timeout_global(Some(CALL_CAP))
            .build()
            .call();
        reply(result)
    }

    /// `PUT` `len` bytes of `path` from `offset`, streamed, with `Content-Length`.
    fn put_range(&self, url: &str, path: &str, offset: u64, len: u64, step: &str) -> Result<Reply> {
        let mut file = File::open(path).with_context(|| format!("open {path}"))?;
        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("seek {path}"))?;
        let mut body = Guarded {
            inner: file.take(len),
            interrupted: self.interrupted,
        };
        let result = self
            .agent
            .put(url)
            .header("Authorization", self.authorization.as_str())
            .header("Content-Length", len.to_string())
            .content_type("application/octet-stream")
            .config()
            .timeout_global(Some(UPLOAD_CAP))
            .build()
            .send(ureq::SendBody::from_reader(&mut body));
        if self.interrupted() {
            bail!("interrupted");
        }
        reply(result).with_context(|| step.to_owned())
    }
}

/// Turn a transport result into a [`Reply`]; a body that is not JSON reads as none.
fn reply(
    result: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<Reply> {
    let mut response = result.map_err(|e| anyhow!("no answer from the registry: {e}"))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .with_config()
        .limit(REPLY_LIMIT)
        .read_to_string()
        .unwrap_or_default();
    Ok(Reply {
        status,
        body: serde_json::from_str(&text).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests;
