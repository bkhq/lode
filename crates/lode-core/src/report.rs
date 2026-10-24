//! Status reporting over OpenTelemetry OTLP/HTTP (JSON encoding).
//!
//! When `[report].endpoint` is set, a background thread posts an
//! `ExportMetricsServiceRequest` to `{endpoint}/v1/metrics` every
//! `[report].interval` seconds — the app's status, version, restarts and
//! update state, read from `state.json` — and an `ExportLogsServiceRequest` to
//! `{endpoint}/v1/logs` for each lifecycle [`Reporter::event`]. Any OTLP/HTTP
//! receiver works (an OpenTelemetry Collector, a vendor backend, lode-server's
//! `/r/otlp`). The thread owns all network I/O, so a slow or dead endpoint never
//! blocks supervision; a failed export is logged and dropped (no disk queue, no
//! replay), and a `429`/`503` pauses exports for its `Retry-After`.
//!
//! `[report].headers` go to the endpoint only, `${ENV}` expanded per request
//! like `[http].headers`; `[http].headers` are never sent there.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::config::Config;
use crate::http;
use crate::state::{self, State, Status};

/// Events queued for the reporter thread; further ones are dropped until it
/// catches up.
const QUEUE: usize = 100;
/// OTLP instrumentation scope name.
const SCOPE: &str = "lode";
const LODE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// OTLP `SeverityNumber` values.
const SEVERITY_INFO: u8 = 9;
const SEVERITY_WARN: u8 = 13;

/// Who is reporting: the resource attributes that do not change between
/// exports. The version is read from `state.json` per export.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    app: String,
    instance_id: Option<String>,
    host_name: Option<String>,
    channel: String,
    interval: u64,
    pid: u32,
    attributes: BTreeMap<String, String>,
}

impl Identity {
    fn from_config(cfg: &Config) -> Self {
        let host_name = host_name();
        Self {
            app: cfg.global.app.clone(),
            instance_id: cfg.report.instance_id.clone().or_else(|| host_name.clone()),
            host_name,
            channel: cfg.update.channel.clone(),
            interval: cfg.report.interval,
            pid: std::process::id(),
            attributes: cfg.report.attributes.clone(),
        }
    }
}

/// A lifecycle event, exported as one OTLP log record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Event {
    name: String,
    body: String,
    attributes: Vec<(String, String)>,
    at_nanos: u128,
}

/// What the supervisor knows that `state.json` does not.
#[derive(Debug, Default)]
struct Live {
    /// App processes spawned by this lode.
    spawns: AtomicU32,
    /// Unix seconds the current app process was spawned at.
    spawned_at: AtomicU64,
}

enum Msg {
    Event(Event),
    Stop(mpsc::Sender<()>),
}

/// Handle to the reporter thread. Cheap to clone; every clone feeds the same
/// thread.
#[derive(Debug, Clone)]
pub struct Reporter {
    tx: SyncSender<Msg>,
    live: Arc<Live>,
}

impl Reporter {
    /// Start reporting for `cfg`, or `None` when `[report].endpoint` is unset
    /// (or the thread cannot be spawned, which is logged).
    #[must_use]
    pub fn start(cfg: &Config) -> Option<Self> {
        let endpoint = cfg.report.endpoint.clone()?;
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let live = Arc::new(Live::default());
        let worker = Worker {
            endpoint: endpoint.trim_end_matches('/').to_owned(),
            headers: cfg.report.headers.clone(),
            allow_insecure: cfg.http.allow_insecure,
            interval: Duration::from_secs(cfg.report.interval),
            state_path: cfg.global.dir.join("state.json"),
            identity: Identity::from_config(cfg),
            started_nanos: now_nanos(),
            live: Arc::clone(&live),
            quiet_until: None,
        };
        match std::thread::Builder::new()
            .name("lode-report".to_owned())
            .spawn(move || worker.run(&rx))
        {
            Ok(_) => {
                tracing::info!(interval = cfg.report.interval, "status reporting on");
                Some(Self { tx, live })
            }
            Err(e) => {
                tracing::warn!(error = %e, "status reporting: cannot start the reporter thread");
                None
            }
        }
    }

    /// Queue a lifecycle event (`name` such as `lode.updated`, a human `body`,
    /// and attributes). Never blocks: when the queue is full the event is dropped.
    pub fn event(&self, name: &str, body: &str, attributes: &[(&str, &str)]) {
        let event = Event {
            name: name.to_owned(),
            body: body.to_owned(),
            attributes: attributes
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            at_nanos: now_nanos(),
        };
        if let Err(TrySendError::Full(_)) = self.tx.try_send(Msg::Event(event)) {
            tracing::debug!(event = name, "status reporting: queue full, event dropped");
        }
    }

    /// Record that an app process was just spawned (restarts and uptime).
    pub fn app_spawned(&self) {
        self.live.spawns.fetch_add(1, Ordering::Relaxed);
        self.live
            .spawned_at
            .store(now_nanos_u64() / 1_000_000_000, Ordering::Relaxed);
    }

    /// Ask the thread to send what is queued and a final metrics export, then
    /// stop; waits up to `wait`.
    pub fn stop(&self, wait: Duration) {
        let (ack, done) = mpsc::channel();
        if self.tx.send(Msg::Stop(ack)).is_ok() {
            let _ = done.recv_timeout(wait);
        }
    }
}

/// The reporter thread's state.
struct Worker {
    endpoint: String,
    headers: Vec<String>,
    allow_insecure: bool,
    interval: Duration,
    state_path: PathBuf,
    identity: Identity,
    started_nanos: u128,
    live: Arc<Live>,
    /// No exports before this instant (`Retry-After`).
    quiet_until: Option<Instant>,
}

impl Worker {
    fn run(mut self, rx: &Receiver<Msg>) {
        self.export_metrics();
        let mut next_metrics = Instant::now() + self.interval;
        loop {
            match rx.recv_timeout(next_metrics.saturating_duration_since(Instant::now())) {
                Ok(Msg::Event(first)) => {
                    let mut events = vec![first];
                    let mut stop = None;
                    while let Ok(msg) = rx.try_recv() {
                        match msg {
                            Msg::Event(e) => events.push(e),
                            Msg::Stop(ack) => {
                                stop = Some(ack);
                                break;
                            }
                        }
                    }
                    let st = state::read_lenient(&self.state_path).unwrap_or_default();
                    let body = logs_body(&self.identity, st.current.as_deref(), &events);
                    self.post("/v1/logs", &body);
                    if let Some(ack) = stop {
                        self.export_metrics();
                        let _ = ack.send(());
                        return;
                    }
                }
                Ok(Msg::Stop(ack)) => {
                    self.export_metrics();
                    let _ = ack.send(());
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.export_metrics();
                    next_metrics = Instant::now() + self.interval;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn export_metrics(&mut self) {
        let st = state::read_lenient(&self.state_path).unwrap_or_default();
        let now = now_nanos();
        let body = metrics_body(
            &self.identity,
            &st,
            self.snapshot(now),
            self.started_nanos,
            now,
        );
        self.post("/v1/metrics", &body);
    }

    fn snapshot(&self, now: u128) -> AppCounters {
        let spawned_at = self.live.spawned_at.load(Ordering::Relaxed);
        let now_secs = u64::try_from(now / 1_000_000_000).unwrap_or(u64::MAX);
        AppCounters {
            restarts: self.live.spawns.load(Ordering::Relaxed).saturating_sub(1),
            uptime: if spawned_at == 0 {
                0
            } else {
                now_secs.saturating_sub(spawned_at)
            },
        }
    }

    fn post(&mut self, path: &str, body: &Value) {
        if self.quiet_until.is_some_and(|t| Instant::now() < t) {
            tracing::debug!(
                path,
                "status reporting: server asked to wait; export skipped"
            );
            return;
        }
        let headers = match http::expand_headers(&self.headers) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "status reporting: [report].headers");
                return;
            }
        };
        let url = format!("{}{path}", self.endpoint);
        let bytes = body.to_string().into_bytes();
        match http::post_json(&url, &headers, &bytes, self.allow_insecure) {
            Ok(r) if (200..300).contains(&r.status) => {
                if let Some(partial) = rejected(&r.body) {
                    tracing::warn!(path, detail = %partial, "status reporting: export partly rejected");
                }
            }
            Ok(r) if r.status == 429 || r.status == 503 => {
                let wait = r.retry_after.map_or(self.interval, Duration::from_secs);
                self.quiet_until = Some(Instant::now() + wait);
                tracing::warn!(
                    path,
                    status = r.status,
                    wait_secs = wait.as_secs(),
                    "status reporting: server busy"
                );
            }
            Ok(r) => tracing::warn!(path, status = r.status, "status reporting: export refused"),
            Err(e) => tracing::warn!(path, error = %e, "status reporting: export failed"),
        }
    }
}

/// The `partialSuccess` of an OTLP answer, when it rejected anything.
fn rejected(body: &[u8]) -> Option<String> {
    let answer: Value = serde_json::from_slice(body).ok()?;
    let partial = answer.get("partialSuccess")?;
    let count = |key: &str| {
        partial
            .get(key)
            .and_then(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
            .filter(|n| n != "0")
    };
    let n = count("rejectedDataPoints").or_else(|| count("rejectedLogRecords"))?;
    let message = partial
        .get("errorMessage")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(format!("{n} rejected: {message}"))
}

/// Restarts and uptime of the app process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AppCounters {
    restarts: u32,
    uptime: u64,
}

fn string(value: &str) -> Value {
    json!({ "stringValue": value })
}

fn int(value: u64) -> Value {
    json!({ "intValue": value.to_string() })
}

fn kv<'a>(pairs: impl IntoIterator<Item = (&'a str, Value)>) -> Value {
    Value::Array(
        pairs
            .into_iter()
            .map(|(key, value)| json!({ "key": key, "value": value }))
            .collect(),
    )
}

/// The OTLP `Resource` for one export.
fn resource(id: &Identity, version: Option<&str>) -> Value {
    let mut attrs: Vec<(&str, Value)> = vec![("service.name", string(&id.app))];
    if let Some(v) = version {
        attrs.push(("service.version", string(v)));
    }
    if let Some(i) = &id.instance_id {
        attrs.push(("service.instance.id", string(i)));
    }
    if let Some(h) = &id.host_name {
        attrs.push(("host.name", string(h)));
    }
    attrs.extend([
        ("host.arch", string(host_arch())),
        ("os.type", string(os_type())),
        ("process.pid", int(u64::from(id.pid))),
        ("lode.version", string(LODE_VERSION)),
        ("lode.channel", string(&id.channel)),
        ("lode.report.interval", int(id.interval)),
    ]);
    if let Some(commit) = crate::build_info::COMMIT {
        attrs.push(("lode.commit", string(commit)));
    }
    for (key, value) in &id.attributes {
        if !attrs.iter().any(|(k, _)| k == key) {
            attrs.push((key, string(value)));
        }
    }
    json!({ "attributes": kv(attrs) })
}

fn status_name(status: Option<Status>) -> String {
    status
        .and_then(|s| serde_json::to_value(s).ok())
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn point(value: u64, attributes: Vec<(&str, Value)>, now: u128) -> Value {
    json!({ "timeUnixNano": now.to_string(), "asInt": value.to_string(), "attributes": kv(attributes) })
}

#[allow(clippy::needless_pass_by_value)] // built in place at every call site
fn gauge(name: &str, unit: &str, points: Vec<Value>) -> Value {
    json!({ "name": name, "unit": unit, "gauge": { "dataPoints": points } })
}

/// The `ExportMetricsServiceRequest` for the current state.
fn metrics_body(id: &Identity, st: &State, app: AppCounters, started: u128, now: u128) -> Value {
    let status = status_name(st.status);
    let running = st.status == Some(Status::Running);
    let mut metrics = vec![
        gauge(
            "lode.app.up",
            "1",
            vec![point(u64::from(running), vec![], now)],
        ),
        gauge(
            "lode.app.status",
            "1",
            vec![point(1, vec![("status", string(&status))], now)],
        ),
        json!({
            "name": "lode.app.restarts",
            "unit": "1",
            "sum": {
                "aggregationTemporality": 2,
                "isMonotonic": true,
                "dataPoints": [{
                    "startTimeUnixNano": started.to_string(),
                    "timeUnixNano": now.to_string(),
                    "asInt": app.restarts.to_string(),
                }],
            },
        }),
        gauge(
            "lode.app.uptime",
            "s",
            vec![point(if running { app.uptime } else { 0 }, vec![], now)],
        ),
    ];
    let available = st
        .available
        .as_deref()
        .filter(|a| st.current.as_deref() != Some(*a));
    metrics.push(gauge(
        "lode.update.available",
        "1",
        vec![point(
            u64::from(available.is_some()),
            available
                .map(|v| vec![("version", string(v))])
                .unwrap_or_default(),
            now,
        )],
    ));
    if let Some(good) = &st.last_good {
        metrics.push(gauge(
            "lode.app.last_good",
            "1",
            vec![point(1, vec![("version", string(good))], now)],
        ));
    }
    let resources: Vec<Value> = st
        .resources
        .iter()
        .filter_map(|(name, r)| {
            let version = r.current.as_deref()?;
            Some(point(
                1,
                vec![("resource", string(name)), ("version", string(version))],
                now,
            ))
        })
        .collect();
    if !resources.is_empty() {
        metrics.push(gauge("lode.resource.version", "1", resources));
    }
    json!({
        "resourceMetrics": [{
            "resource": resource(id, st.current.as_deref()),
            "scopeMetrics": [{ "scope": { "name": SCOPE, "version": LODE_VERSION }, "metrics": metrics }],
        }],
    })
}

/// Failures and fallbacks are warnings; everything else is informational.
fn severity(name: &str) -> (u8, &'static str) {
    if name.ends_with(".failed") || name == "lode.rolled_back" || name == "lode.paused" {
        (SEVERITY_WARN, "WARN")
    } else {
        (SEVERITY_INFO, "INFO")
    }
}

/// The `ExportLogsServiceRequest` for `events`.
fn logs_body(id: &Identity, version: Option<&str>, events: &[Event]) -> Value {
    let records: Vec<Value> = events
        .iter()
        .map(|e| {
            let (number, text) = severity(&e.name);
            let mut attrs = vec![("event.name", string(&e.name))];
            attrs.extend(e.attributes.iter().map(|(k, v)| (k.as_str(), string(v))));
            json!({
                "timeUnixNano": e.at_nanos.to_string(),
                "observedTimeUnixNano": e.at_nanos.to_string(),
                "severityNumber": number,
                "severityText": text,
                "eventName": e.name,
                "body": string(&e.body),
                "attributes": kv(attrs),
            })
        })
        .collect();
    json!({
        "resourceLogs": [{
            "resource": resource(id, version),
            "scopeLogs": [{ "scope": { "name": SCOPE, "version": LODE_VERSION }, "logRecords": records }],
        }],
    })
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

fn now_nanos_u64() -> u64 {
    u64::try_from(now_nanos()).unwrap_or(u64::MAX)
}

/// The machine's host name, when it can be read.
fn host_name() -> Option<String> {
    #[cfg(unix)]
    let name = nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok());
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").ok();
    #[cfg(not(any(unix, windows)))]
    let name = None;
    name.filter(|n| !n.is_empty())
}

/// OpenTelemetry `host.arch` for this build.
fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm32",
        "powerpc64" => "ppc64",
        other => other,
    }
}

/// OpenTelemetry `os.type` for this build.
fn os_type() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ResourceState;

    fn identity() -> Identity {
        Identity {
            app: "myapp".to_owned(),
            instance_id: Some("web-01".to_owned()),
            host_name: Some("web-01.internal".to_owned()),
            channel: "stable".to_owned(),
            interval: 300,
            pid: 42,
            attributes: BTreeMap::from([
                ("region".to_owned(), "hk".to_owned()),
                // A configured attribute never replaces one lode sets.
                ("service.name".to_owned(), "spoofed".to_owned()),
            ]),
        }
    }

    fn attrs(value: &Value) -> BTreeMap<String, Value> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|kv| (kv["key"].as_str().unwrap().to_owned(), kv["value"].clone()))
            .collect()
    }

    #[test]
    fn resource_carries_semantic_conventions_and_extra_attributes() {
        let r = resource(&identity(), Some("1.5.0"));
        let a = attrs(&r["attributes"]);
        assert_eq!(a["service.name"], string("myapp"));
        assert_eq!(a["service.version"], string("1.5.0"));
        assert_eq!(a["service.instance.id"], string("web-01"));
        assert_eq!(a["host.name"], string("web-01.internal"));
        assert_eq!(a["host.arch"], string(host_arch()));
        assert_eq!(a["os.type"], string(os_type()));
        assert_eq!(a["process.pid"], int(42));
        assert_eq!(a["lode.version"], string(LODE_VERSION));
        assert_eq!(
            a.get("lode.commit"),
            crate::build_info::COMMIT.map(string).as_ref()
        );
        assert_eq!(a["lode.channel"], string("stable"));
        assert_eq!(a["lode.report.interval"], json!({ "intValue": "300" }));
        assert_eq!(a["region"], string("hk"));
        assert_eq!(
            r["attributes"].as_array().unwrap().len(),
            a.len(),
            "keys are unique"
        );
    }

    #[test]
    fn metrics_body_reports_status_restarts_updates_and_resources() {
        let st = State {
            current: Some("1.5.0".to_owned()),
            last_good: Some("1.4.0".to_owned()),
            available: Some("1.6.0".to_owned()),
            status: Some(Status::Running),
            resources: BTreeMap::from([(
                "conf".to_owned(),
                ResourceState {
                    current: Some("7".to_owned()),
                    ..ResourceState::default()
                },
            )]),
            ..State::default()
        };
        let body = metrics_body(
            &identity(),
            &st,
            AppCounters {
                restarts: 2,
                uptime: 90,
            },
            1_000,
            2_000,
        );
        let scope = &body["resourceMetrics"][0]["scopeMetrics"][0];
        assert_eq!(
            scope["scope"],
            json!({ "name": "lode", "version": LODE_VERSION })
        );
        let metrics: BTreeMap<&str, &Value> = scope["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| (m["name"].as_str().unwrap(), m))
            .collect();
        let point = |name: &str| &metrics[name]["gauge"]["dataPoints"][0];
        assert_eq!(point("lode.app.up")["asInt"], "1");
        assert_eq!(point("lode.app.up")["timeUnixNano"], "2000");
        assert_eq!(point("lode.app.status")["asInt"], "1");
        assert_eq!(
            attrs(&point("lode.app.status")["attributes"])["status"],
            string("running")
        );
        assert_eq!(point("lode.app.uptime")["asInt"], "90");
        assert_eq!(point("lode.update.available")["asInt"], "1");
        assert_eq!(
            attrs(&point("lode.update.available")["attributes"])["version"],
            string("1.6.0")
        );
        assert_eq!(
            attrs(&point("lode.app.last_good")["attributes"])["version"],
            string("1.4.0")
        );
        let res = attrs(&point("lode.resource.version")["attributes"]);
        assert_eq!(
            (&res["resource"], &res["version"]),
            (&string("conf"), &string("7"))
        );
        assert_eq!(
            metrics["lode.app.restarts"]["sum"],
            json!({
                "aggregationTemporality": 2,
                "isMonotonic": true,
                "dataPoints": [{ "startTimeUnixNano": "1000", "timeUnixNano": "2000", "asInt": "2" }],
            })
        );
    }

    #[test]
    fn metrics_body_for_a_stopped_app_without_update() {
        let st = State {
            current: Some("1.5.0".to_owned()),
            available: Some("1.5.0".to_owned()),
            status: Some(Status::Stopped),
            ..State::default()
        };
        let body = metrics_body(
            &identity(),
            &st,
            AppCounters {
                restarts: 0,
                uptime: 90,
            },
            0,
            1,
        );
        let metrics: BTreeMap<&str, &Value> =
            body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| (m["name"].as_str().unwrap(), m))
                .collect();
        let point = |name: &str| &metrics[name]["gauge"]["dataPoints"][0];
        assert_eq!(point("lode.app.up")["asInt"], "0");
        assert_eq!(point("lode.app.uptime")["asInt"], "0");
        assert_eq!(point("lode.update.available")["asInt"], "0");
        assert!(!metrics.contains_key("lode.app.last_good"));
        assert!(!metrics.contains_key("lode.resource.version"));
        assert_eq!(status_name(None), "unknown");
    }

    #[test]
    fn logs_body_names_each_event() {
        let events = [
            Event {
                name: "lode.updated".to_owned(),
                body: "1.4.0 -> 1.5.0".to_owned(),
                attributes: vec![("from".to_owned(), "1.4.0".to_owned())],
                at_nanos: 5,
            },
            Event {
                name: "lode.rolled_back".to_owned(),
                body: "not ready".to_owned(),
                attributes: vec![],
                at_nanos: 6,
            },
        ];
        let body = logs_body(&identity(), Some("1.5.0"), &events);
        let rl = &body["resourceLogs"][0];
        assert_eq!(
            attrs(&rl["resource"]["attributes"])["service.version"],
            string("1.5.0")
        );
        let records = rl["scopeLogs"][0]["logRecords"].as_array().unwrap();
        assert_eq!(
            records[0],
            json!({
                "timeUnixNano": "5",
                "observedTimeUnixNano": "5",
                "severityNumber": 9,
                "severityText": "INFO",
                "eventName": "lode.updated",
                "body": { "stringValue": "1.4.0 -> 1.5.0" },
                "attributes": [
                    { "key": "event.name", "value": { "stringValue": "lode.updated" } },
                    { "key": "from", "value": { "stringValue": "1.4.0" } },
                ],
            })
        );
        assert_eq!(records[1]["severityText"], "WARN");
    }

    #[test]
    fn rejected_reads_partial_success() {
        assert_eq!(rejected(b"{}"), None);
        assert_eq!(rejected(b"not json"), None);
        assert_eq!(
            rejected(br#"{"partialSuccess":{"rejectedDataPoints":"0"}}"#),
            None
        );
        assert_eq!(
            rejected(
                br#"{"partialSuccess":{"rejectedLogRecords":"2","errorMessage":"no package"}}"#
            )
            .as_deref(),
            Some("2 rejected: no package")
        );
    }

    /// Accept `n` requests on `listener`, answering each with `{}`; return
    /// each request's path and body.
    fn serve(
        listener: std::net::TcpListener,
        n: usize,
    ) -> std::thread::JoinHandle<Vec<(String, Value)>> {
        use std::io::{BufRead as _, BufReader, Read as _, Write as _};
        std::thread::spawn(move || {
            (0..n)
                .map(|_| {
                    let (sock, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(sock);
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let path = line.split(' ').nth(1).unwrap().to_owned();
                    let mut length = 0;
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).unwrap();
                        if header.trim().is_empty() {
                            break;
                        }
                        if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = v.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    reader
                        .get_mut()
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .unwrap();
                    (path, serde_json::from_slice(&body).unwrap())
                })
                .collect()
        })
    }

    #[test]
    fn reporter_exports_metrics_at_start_then_events() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = serve(listener, 3);
        let dir = std::env::temp_dir().join(format!("lode-report-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config::from_toml(&format!(
            "[global]\napp = \"myapp\"\ndir = {:?}\n[update]\npolicy = \"off\"\n\
             [report]\nendpoint = \"http://127.0.0.1:{port}/r/otlp/\"\ninterval = 3600\n",
            dir.display().to_string()
        ))
        .unwrap();
        assert!(
            Reporter::start(&Config::from_toml("").unwrap()).is_none(),
            "off without an endpoint"
        );

        let reporter = Reporter::start(&cfg).unwrap();
        reporter.app_spawned();
        reporter.event("lode.started", "lode started", &[("version", "1.0.0")]);
        reporter.stop(Duration::from_secs(10));

        let requests = server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(requests[0].0, "/r/otlp/v1/metrics");
        assert_eq!(requests[1].0, "/r/otlp/v1/logs");
        assert_eq!(
            requests[2].0, "/r/otlp/v1/metrics",
            "a final export on stop"
        );
        let record = &requests[1].1["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(record["eventName"], "lode.started");
        let resource = attrs(&requests[0].1["resourceMetrics"][0]["resource"]["attributes"]);
        assert_eq!(resource["service.name"], string("myapp"));
    }
}
