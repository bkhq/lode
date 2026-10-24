use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use lode_core::verify::{artifact_message, verify_signature};

use super::*;

const TOKEN: &str = "pat_SECRETTOKEN";
const PACKAGE: &str = "acme/app";

/// One request the stub received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    /// Path and query, as sent.
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn line(&self) -> String {
        format!("{} {}", self.method, self.target)
    }
}

type Responder = dyn Fn(&Seen, usize) -> (u16, String) + Send + Sync;

/// Loopback registry: records every request and answers from `respond`, which
/// gets the request and how many came before it.
struct Registry {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Registry {
    fn start(respond: impl Fn(&Seen, usize) -> (u16, String) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let respond: Box<Responder> = Box::new(respond);
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { break };
                let Some(request) = read_request(&mut sock) else {
                    continue;
                };
                let before = {
                    let mut log = log.lock().unwrap();
                    log.push(request.clone());
                    log.len() - 1
                };
                let (status, body) = respond(&request, before);
                let _ = write!(
                    sock,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self { base, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn lines(&self) -> Vec<String> {
        self.seen().iter().map(Seen::line).collect()
    }
}

fn read_request(sock: &mut TcpStream) -> Option<Seen> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if sock.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let len = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).ok()?;
    Some(Seen {
        method,
        target,
        headers,
        body,
    })
}

fn ok(data: &Value) -> (u16, String) {
    (200, json!({ "success": true, "data": data }).to_string())
}

fn created(data: &Value) -> (u16, String) {
    (201, json!({ "success": true, "data": data }).to_string())
}

fn error(status: u16, code: &str, message: &str) -> (u16, String) {
    let body = json!({ "success": false, "error": { "code": code, "message": message } });
    (status, body.to_string())
}

/// A registry where every step succeeds; uploads are cut into 4-byte parts.
fn happy(request: &Seen, _: usize) -> (u16, String) {
    if request.target.ends_with("/uploads") {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let size = body["size"].as_u64().unwrap();
        return created(
            &json!({ "uploadId": "up1", "partSize": 4, "partCount": size.div_ceil(4) }),
        );
    }
    if request.target.ends_with("/commits/head") {
        return ok(&empty_head());
    }
    match request.method.as_str() {
        "POST" if request.target.ends_with("/keys") => created(&json!({ "key_id": "k" })),
        "POST" | "PUT" => created(&json!({})),
        _ => ok(&json!({})),
    }
}

/// The head of a package without commits or published versions.
fn empty_head() -> Value {
    json!({
        "commit": null,
        "generation": 0,
        "manifest": { "schema": "lode/v1", "name": "app", "channels": {}, "versions": {} },
    })
}

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lode-publish-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn file(dir: &Path, name: &str, contents: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path.to_str().unwrap().to_owned()
}

fn request<'a>(registry: &'a str, files: &'a [String]) -> Request<'a> {
    Request {
        registry,
        package: PACKAGE,
        version: "1.4.2",
        channel: "stable",
        notes: None,
        key: None,
        key_env: None,
        run: None,
        exec: None,
        runtime: None,
        allow_insecure: false,
        files,
    }
}

/// Run with the test token; `single_max` is where multipart starts.
fn publish_to(req: &Request<'_>, single_max: u64) -> (Result<()>, String) {
    let interrupted = AtomicBool::new(false);
    run_with(req, TOKEN, &interrupted, single_max)
}

fn run_with(
    req: &Request<'_>,
    token: &str,
    interrupted: &AtomicBool,
    single_max: u64,
) -> (Result<()>, String) {
    let env = Env {
        token,
        interrupted,
        retry_pause: Duration::ZERO,
        single_max,
        asset_max: 1024,
    };
    let mut out = Vec::new();
    let result = run(req, &env, &mut out);
    (result, String::from_utf8(out).unwrap())
}

fn failure(result: Result<()>) -> String {
    format!("{:#}", result.unwrap_err())
}

#[test]
fn publishes_an_unsigned_asset_in_one_request() {
    let registry = Registry::start(happy);
    let dir = scratch("single");
    let files = [file(&dir, "app.tar.gz", b"hello world")];
    let mut req = request(&registry.base, &files);
    req.notes = Some("first");

    let (result, out) = publish_to(&req, 100);
    result.unwrap();

    let sha256 = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
    assert_eq!(
        registry.lines(),
        [
            "POST /api/registry/packages/acme/app/versions".to_owned(),
            format!(
                "PUT /api/registry/packages/acme/app/versions/1.4.2/assets/app.tar.gz?sha256={sha256}"
            ),
            "POST /api/registry/packages/acme/app/versions/1.4.2/publish".to_owned(),
        ]
    );
    let seen = registry.seen();
    let draft: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(draft, json!({ "version": "1.4.2", "notes": "first" }));
    assert_eq!(seen[0].header("content-type"), Some("application/json"));
    assert_eq!(seen[1].body, b"hello world");
    assert_eq!(seen[1].header("content-length"), Some("11"));
    assert_eq!(seen[1].header("transfer-encoding"), None);
    assert_eq!(
        seen[1].header("content-type"),
        Some("application/octet-stream")
    );
    let publish: Value = serde_json::from_slice(&seen[2].body).unwrap();
    assert_eq!(publish, json!({ "channel": "stable" }));
    for request in &seen {
        assert_eq!(
            request.header("authorization"),
            Some("Bearer pat_SECRETTOKEN")
        );
    }
    assert_eq!(
        out,
        "publish: created draft acme/app@1.4.2\n\
         publish: uploading app.tar.gz (11 bytes)\n\
         publish: published acme/app@1.4.2 to stable\n"
    );
    assert!(!out.contains(TOKEN));
}

#[test]
fn a_registry_base_path_is_kept() {
    let registry = Registry::start(happy);
    let dir = scratch("base-path");
    let files = [file(&dir, "a.bin", b"x")];
    let base = format!("{}/u/", registry.base);

    publish_to(&request(&base, &files), 100).0.unwrap();
    assert_eq!(
        registry.lines()[0],
        "POST /u/api/registry/packages/acme/app/versions"
    );
}

#[test]
fn signs_every_asset_and_registers_the_key_first() {
    let registry = Registry::start(happy);
    let dir = scratch("signed");
    let files = [file(&dir, "app.tar.gz", b"hello world")];
    let secret = [7u8; 32];
    let key_path = file(&dir, "signer.key", B64.encode(secret).as_bytes());
    let mut req = request(&registry.base, &files);
    req.key = Some(&key_path);
    req.run = Some("./app --serve");

    let (result, out) = publish_to(&req, 100);
    result.unwrap();

    let seen = registry.seen();
    assert_eq!(seen[0].line(), "POST /api/registry/packages/acme/app/keys");
    let public_key = B64.encode(
        ed25519_dalek::SigningKey::from_bytes(&secret)
            .verifying_key()
            .to_bytes(),
    );
    let registered: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(
        registered,
        json!({ "public_key": public_key, "name": "ci" })
    );

    // The signature in the query is the one `lode-cli sign` would produce.
    let query = seen[2].target.split_once('?').unwrap().1;
    let param = |name: &str| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .unwrap()
            .to_owned()
    };
    assert_eq!(param("run"), ".%2Fapp%20--serve");
    let artifact = Artifact {
        app: "app",
        name: "app.tar.gz",
        version: "1.4.2",
        path: &files[0],
        run: Some("./app --serve"),
        exec: None,
        runtime: None,
    };
    let message = artifact_message(&artifact, &param("sha256"));
    assert!(verify_signature(&public_key, &message, &param("sig")).unwrap());
    assert!(out.contains("publish: registered signing key "));
    assert!(out.contains("(11 bytes, signed)"));
}

#[test]
fn an_asset_runtime_is_signed_and_sent() {
    let registry = Registry::start(happy);
    let dir = scratch("runtime");
    let files = [file(&dir, "app.tar.gz", b"hello world")];
    let secret = [7u8; 32];
    let key_path = file(&dir, "signer.key", B64.encode(secret).as_bytes());
    let rt = lode_core::manifest::RuntimeSpec {
        runtime: "node".to_owned(),
        version: Some("v22.".to_owned()),
        version_check: None,
        download: Some("https://x/node.tar.gz".to_owned()),
        sha256: Some("c".repeat(64)),
    };
    let mut req = request(&registry.base, &files);
    req.key = Some(&key_path);
    req.runtime = Some(&rt);

    publish_to(&req, 100).0.unwrap();

    let seen = registry.seen();
    let query = seen[2].target.split_once('?').unwrap().1;
    let param = |name: &str| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .map(ToOwned::to_owned)
    };
    assert_eq!(param("runtime").as_deref(), Some("node"));
    assert_eq!(param("runtime_version").as_deref(), Some("v22."));
    assert_eq!(param("runtime_version_check"), None);
    assert_eq!(
        param("runtime_download").as_deref(),
        Some("https%3A%2F%2Fx%2Fnode.tar.gz")
    );
    assert_eq!(param("runtime_sha256"), rt.sha256);
    let public_key = B64.encode(
        ed25519_dalek::SigningKey::from_bytes(&secret)
            .verifying_key()
            .to_bytes(),
    );
    let artifact = Artifact {
        app: "app",
        name: "app.tar.gz",
        version: "1.4.2",
        path: &files[0],
        run: None,
        exec: None,
        runtime: Some(&rt),
    };
    let message = artifact_message(&artifact, &param("sha256").unwrap());
    let sig = param("sig").unwrap();
    assert!(verify_signature(&public_key, &message, &sig).unwrap());
}

#[test]
fn a_key_that_is_already_registered_is_fine() {
    let registry = Registry::start(|request, n| {
        if request.target.ends_with("/keys") {
            error(409, "KEY_EXISTS", "already there")
        } else {
            happy(request, n)
        }
    });
    let dir = scratch("key-exists");
    let files = [file(&dir, "a.bin", b"x")];
    let key_path = file(&dir, "signer.key", B64.encode([7u8; 32]).as_bytes());
    let mut req = request(&registry.base, &files);
    req.key = Some(&key_path);

    let (result, out) = publish_to(&req, 100);
    result.unwrap();
    assert!(out.starts_with("publish: signing key already registered\n"));
    assert_eq!(registry.lines().len(), 5);
}

#[test]
fn an_ecdsa_key_is_refused_before_any_request() {
    let registry = Registry::start(happy);
    let dir = scratch("ecdsa");
    let files = [file(&dir, "a.bin", b"x")];
    let key = format!("ecdsa-p256:{}", B64.encode([7u8; 32]));
    let key_path = file(&dir, "signer.key", key.as_bytes());
    let mut req = request(&registry.base, &files);
    req.key = Some(&key_path);

    let text = failure(publish_to(&req, 100).0);
    assert!(
        text.contains("the registry accepts ed25519 keys only"),
        "{text}"
    );
    assert!(registry.seen().is_empty());
}

#[test]
fn a_rerun_continues_into_the_draft() {
    let registry = Registry::start(|request, n| match request.method.as_str() {
        "POST" if request.target.ends_with("/versions") => error(409, "VERSION_EXISTS", "exists"),
        "GET" => ok(&json!({ "version": { "status": "draft" }, "channels": [] })),
        _ => happy(request, n),
    });
    let dir = scratch("rerun");
    let files = [file(&dir, "a.bin", b"x")];

    let (result, out) = publish_to(&request(&registry.base, &files), 100);
    result.unwrap();
    assert!(out.starts_with("publish: continuing into the existing draft acme/app@1.4.2\n"));
    assert_eq!(
        registry.lines()[1],
        "GET /api/registry/packages/acme/app/versions/1.4.2"
    );
}

#[test]
fn a_released_version_is_left_alone() {
    let registry = Registry::start(|request, n| match request.method.as_str() {
        "POST" => error(409, "VERSION_EXISTS", "exists"),
        "GET" => ok(&json!({ "version": { "status": "published" } })),
        _ => happy(request, n),
    });
    let dir = scratch("released");
    let files = [file(&dir, "a.bin", b"x")];

    let text = failure(publish_to(&request(&registry.base, &files), 100).0);
    assert!(
        text.contains("acme/app@1.4.2 is already published"),
        "{text}"
    );
    assert_eq!(registry.lines().len(), 2);
}

#[test]
fn a_large_asset_goes_up_in_the_parts_the_registry_dictates() {
    let registry = Registry::start(happy);
    let dir = scratch("multipart");
    let files = [file(&dir, "big.bin", b"0123456789")];
    let mut req = request(&registry.base, &files);
    req.exec = Some("./big");

    let (result, out) = publish_to(&req, 9);
    result.unwrap();

    let seen = registry.seen();
    let lines = registry.lines();
    assert_eq!(
        lines[1..],
        [
            "POST /api/registry/packages/acme/app/versions/1.4.2/uploads",
            "PUT /api/registry/uploads/up1/parts/1",
            "PUT /api/registry/uploads/up1/parts/2",
            "PUT /api/registry/uploads/up1/parts/3",
            "POST /api/registry/uploads/up1/complete",
            "POST /api/registry/packages/acme/app/versions/1.4.2/publish",
        ]
    );
    let start: Value = serde_json::from_slice(&seen[1].body).unwrap();
    assert_eq!(start["asset"], "big.bin");
    assert_eq!(start["size"], 10);
    assert_eq!(start["exec"], "./big");
    assert_eq!(start.get("sig"), None);
    assert_eq!(seen[2].body, b"0123");
    assert_eq!(seen[3].body, b"4567");
    assert_eq!(seen[4].body, b"89");
    assert_eq!(seen[4].header("content-length"), Some("2"));
    assert!(seen[5].body.is_empty());
    assert!(out.contains("publish: big.bin: part 3/3\n"));
}

#[test]
fn plans_parts_from_the_registry_answer() {
    assert_eq!(plan_parts(10, 4, 3).unwrap(), [(0, 4), (4, 4), (8, 2)]);
    assert_eq!(plan_parts(8, 4, 2).unwrap(), [(0, 4), (4, 4)]);
    assert_eq!(plan_parts(3, 64, 1).unwrap(), [(0, 3)]);
    assert!(plan_parts(10, 4, 2).is_err());
    assert!(plan_parts(10, 0, 3).is_err());
}

#[test]
fn a_part_is_retried_on_a_server_error() {
    let failed_once = AtomicBool::new(false);
    let registry = Registry::start(move |request, n| {
        if request.target.ends_with("/parts/2") && !failed_once.swap(true, Ordering::Relaxed) {
            (502, "<html>bad gateway</html>".to_owned())
        } else {
            happy(request, n)
        }
    });
    let dir = scratch("retry");
    let files = [file(&dir, "big.bin", b"0123456789")];

    publish_to(&request(&registry.base, &files), 9).0.unwrap();
    let parts = registry
        .lines()
        .iter()
        .filter(|l| l.ends_with("/parts/2"))
        .count();
    assert_eq!(parts, 2);
}

#[test]
fn a_part_is_given_up_after_a_fixed_number_of_tries() {
    let registry = Registry::start(|request, n| {
        if request.target.contains("/parts/") {
            (503, String::new())
        } else {
            happy(request, n)
        }
    });
    let dir = scratch("retry-cap");
    let files = [file(&dir, "big.bin", b"0123456789")];

    let text = failure(publish_to(&request(&registry.base, &files), 9).0);
    assert_eq!(text, "upload of big.bin, part 1/3: HTTP 503");
    let lines = registry.lines();
    assert_eq!(lines.iter().filter(|l| l.contains("/parts/1")).count(), 3);
    assert_eq!(lines.last().unwrap(), "DELETE /api/registry/uploads/up1");
}

#[test]
fn a_refused_part_is_not_retried_and_the_upload_is_given_up() {
    let registry = Registry::start(|request, n| {
        if request.target.contains("/parts/") {
            error(422, "INVALID_PART_SIZE", "wrong size")
        } else {
            happy(request, n)
        }
    });
    let dir = scratch("refused-part");
    let files = [file(&dir, "big.bin", b"0123456789")];

    let text = failure(publish_to(&request(&registry.base, &files), 9).0);
    assert_eq!(
        text,
        "upload of big.bin, part 1/3: HTTP 422 INVALID_PART_SIZE: wrong size"
    );
    let lines = registry.lines();
    assert_eq!(lines.iter().filter(|l| l.contains("/parts/")).count(), 1);
    assert_eq!(lines.last().unwrap(), "DELETE /api/registry/uploads/up1");
    assert!(!lines.iter().any(|l| l.ends_with("/publish")));
}

#[test]
fn a_failed_completion_gives_the_upload_up() {
    let registry = Registry::start(|request, n| {
        if request.target.ends_with("/complete") {
            error(422, "DIGEST_MISMATCH", "digest")
        } else {
            happy(request, n)
        }
    });
    let dir = scratch("complete");
    let files = [file(&dir, "big.bin", b"0123456789")];

    let text = failure(publish_to(&request(&registry.base, &files), 9).0);
    assert!(text.contains("HTTP 422 DIGEST_MISMATCH: digest"), "{text}");
    assert_eq!(
        registry.lines().last().unwrap(),
        "DELETE /api/registry/uploads/up1"
    );
}

#[test]
fn an_interrupted_multipart_upload_is_given_up() {
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupted);
    let registry = Registry::start(move |request, n| {
        if request.target.ends_with("/parts/1") {
            flag.store(true, Ordering::Relaxed);
        }
        happy(request, n)
    });
    let dir = scratch("interrupt");
    let files = [file(&dir, "big.bin", b"0123456789")];

    let (result, _) = run_with(&request(&registry.base, &files), TOKEN, &interrupted, 9);
    assert_eq!(failure(result), "interrupted");
    let lines = registry.lines();
    assert!(!lines.iter().any(|l| l.ends_with("/parts/2")));
    assert_eq!(lines.last().unwrap(), "DELETE /api/registry/uploads/up1");
}

#[test]
fn a_refusal_reports_the_step_status_code_and_message() {
    let registry = Registry::start(|_, _| error(403, "FORBIDDEN", "no write grant"));
    let dir = scratch("refused");
    let files = [file(&dir, "a.bin", b"x")];

    let text = failure(publish_to(&request(&registry.base, &files), 100).0);
    assert_eq!(
        text,
        "creation of the draft acme/app@1.4.2: HTTP 403 FORBIDDEN: no write grant"
    );
    assert!(!text.contains(TOKEN));
    assert_eq!(registry.lines().len(), 1);
}

#[test]
fn a_gateway_error_without_json_still_reports_the_status() {
    let registry = Registry::start(|_, _| (502, "<html>bad gateway</html>".to_owned()));
    let dir = scratch("gateway");
    let files = [file(&dir, "a.bin", b"x")];

    let text = failure(publish_to(&request(&registry.base, &files), 100).0);
    assert_eq!(text, "creation of the draft acme/app@1.4.2: HTTP 502");
}

#[test]
fn a_redirect_is_not_followed() {
    let registry = Registry::start(|_, _| (302, String::new()));
    let dir = scratch("redirect");
    let files = [file(&dir, "a.bin", b"x")];

    let text = failure(publish_to(&request(&registry.base, &files), 100).0);
    assert!(text.ends_with("HTTP 302"), "{text}");
    assert_eq!(registry.lines().len(), 1);
}

#[test]
fn an_unsigned_release_refused_for_its_signature_publishes_nothing() {
    let registry = Registry::start(|request, n| {
        if request.method == "PUT" {
            error(
                422,
                "SIGNATURE_REQUIRED",
                "this package requires signatures",
            )
        } else {
            happy(request, n)
        }
    });
    let dir = scratch("sig-required");
    let files = [file(&dir, "a.bin", b"x")];

    let text = failure(publish_to(&request(&registry.base, &files), 100).0);
    assert!(text.contains("HTTP 422 SIGNATURE_REQUIRED"), "{text}");
    assert!(!registry.lines().iter().any(|l| l.ends_with("/publish")));
}

#[test]
fn bad_arguments_fail_before_the_first_request() {
    let registry = Registry::start(happy);
    let dir = scratch("invalid-args");
    let good = file(&dir, "a.bin", b"x");

    let files = [good];
    let cases: Vec<(Request<'_>, &str)> = vec![
        (
            Request {
                package: "Acme/app",
                ..request(&registry.base, &files)
            },
            "--package must be",
        ),
        (
            Request {
                package: "acme",
                ..request(&registry.base, &files)
            },
            "--package must be",
        ),
        (
            Request {
                package: "acme/-app",
                ..request(&registry.base, &files)
            },
            "--package must be",
        ),
        (
            Request {
                version: "-1",
                ..request(&registry.base, &files)
            },
            "--version may only",
        ),
        (
            Request {
                version: "1/2",
                ..request(&registry.base, &files)
            },
            "--version may only",
        ),
        (
            Request {
                channel: ".x",
                ..request(&registry.base, &files)
            },
            "--channel may only",
        ),
        (
            Request {
                run: Some(" "),
                ..request(&registry.base, &files)
            },
            "run",
        ),
        (
            Request {
                registry: "ftp://x",
                ..request(&registry.base, &files)
            },
            "--registry must be",
        ),
        (
            Request {
                registry: "http://registry.example",
                ..request(&registry.base, &files)
            },
            "refusing a non-HTTPS registry",
        ),
        (
            Request {
                key: Some("a"),
                key_env: Some("B"),
                ..request(&registry.base, &files)
            },
            "exactly one",
        ),
    ];
    for (req, expected) in cases {
        let text = failure(publish_to(&req, 100).0);
        assert!(text.contains(expected), "{text}");
    }
    assert!(registry.seen().is_empty(), "nothing may reach the registry");
}

#[test]
fn bad_files_and_tokens_fail_before_the_first_request() {
    let registry = Registry::start(happy);
    let dir = scratch("invalid-files");
    let good = file(&dir, "a.bin", b"x");
    let empty = file(&dir, "empty.bin", b"");
    let big = file(&dir, "big.bin", &[0u8; 2048]);
    let odd = file(&dir, "has space.bin", b"x");
    std::fs::create_dir_all(dir.join("other")).unwrap();
    let twin = file(&dir.join("other"), "a.bin", b"y");
    let missing = dir.join("nope.bin").to_str().unwrap().to_owned();
    let directory = dir.to_str().unwrap().to_owned();

    let file_cases: Vec<(Vec<String>, &str)> = vec![
        (vec![], "no asset files given"),
        (vec![missing], "nope.bin"),
        (vec![directory], "not a file"),
        (vec![empty], "empty.bin is empty"),
        (vec![big], "at most 1024 per asset"),
        (vec![odd], "asset name \"has space.bin\" may only"),
        (vec![good.clone(), twin], "is given twice"),
    ];
    for (files, expected) in file_cases {
        let text = failure(publish_to(&request(&registry.base, &files), 100).0);
        assert!(text.contains(expected), "{text}");
    }

    let interrupted = AtomicBool::new(false);
    let files = [good];
    let req = request(&registry.base, &files);
    let text = failure(run_with(&req, "", &interrupted, 100).0);
    assert_eq!(text, "LODE_REGISTRY_TOKEN is not set");
    let text = failure(run_with(&req, "pat_x\r\nX-Evil: 1", &interrupted, 100).0);
    assert_eq!(
        text,
        "LODE_REGISTRY_TOKEN contains characters a token never has"
    );

    assert!(registry.seen().is_empty(), "nothing may reach the registry");
}

#[test]
fn an_unknown_option_does_not_echo_its_value() {
    use clap::Parser as _;

    let error = crate::cli::ToolCli::try_parse_from([
        "lode-cli",
        "publish",
        "--package",
        PACKAGE,
        "--version",
        "1.0.0",
        "--token=pat_SECRETTOKEN",
        "a.bin",
    ])
    .unwrap_err()
    .to_string();
    assert!(error.contains("--token"), "{error}");
    assert!(!error.contains("SECRETTOKEN"), "{error}");
}

#[test]
fn percent_encodes_query_values() {
    assert_eq!(percent_encode("aZ0-._~"), "aZ0-._~");
    assert_eq!(percent_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
}

// --- commits ----------------------------------------------------------------

const SECRET: [u8; 32] = [7u8; 32];

fn public_key() -> String {
    B64.encode(
        ed25519_dalek::SigningKey::from_bytes(&SECRET)
            .verifying_key()
            .to_bytes(),
    )
}

/// The commits the stub received, parsed.
fn commits(registry: &Registry) -> Vec<(Value, lode_core::manifest::Manifest)> {
    registry
        .seen()
        .iter()
        .filter(|r| r.method == "POST" && r.target.ends_with("/commits"))
        .map(|r| {
            (
                serde_json::from_slice(&r.body).unwrap(),
                lode_core::manifest::parse(&r.body).unwrap(),
            )
        })
        .collect()
}

/// Both signatures of a commit verify under the test key.
fn assert_signed(manifest: &lode_core::manifest::Manifest) {
    let key = public_key();
    let sig = manifest.sig.as_deref().unwrap();
    assert!(verify_signature(&key, &manifest.signing_message(), sig).unwrap());
    let commit_sig = manifest.commit_sig.as_deref().unwrap();
    let message = manifest.commit_signing_message().unwrap();
    assert!(verify_signature(&key, &message, commit_sig).unwrap());
}

#[test]
fn a_signed_release_is_a_commit() {
    let registry = Registry::start(happy);
    let dir = scratch("commit");
    let files = [file(&dir, "app.tar.gz", b"hello world")];
    let key_path = file(&dir, "signer.key", B64.encode(SECRET).as_bytes());
    let mut req = request(&registry.base, &files);
    req.key = Some(&key_path);
    req.notes = Some("first");

    let (result, out) = publish_to(&req, 100);
    result.unwrap();

    let lines = registry.lines();
    assert_eq!(
        lines[3..],
        [
            "GET /api/registry/packages/acme/app/commits/head",
            "POST /api/registry/packages/acme/app/commits",
        ]
    );
    let commits = commits(&registry);
    let (json, manifest) = &commits[0];
    assert_eq!(manifest.generation, Some(1));
    assert_eq!(manifest.parent, None);
    assert_eq!(manifest.channels["stable"].latest, "1.4.2");
    let version = &manifest.versions["1.4.2"];
    assert_eq!(version.notes.as_deref(), Some("first"));
    assert_eq!(version.assets[0].name, "app.tar.gz");
    assert!(version.assets[0].sig.is_some());
    assert_eq!(json["versions"]["1.4.2"]["assets"][0]["size"], 11);
    assert_signed(manifest);
    assert!(out.contains("publish: committed "));
    assert!(out.ends_with("publish: published acme/app@1.4.2 to stable\n"));
}

#[test]
fn a_conflict_builds_on_the_new_head() {
    let parent = "b".repeat(64);
    let head = json!({
        "commit": parent,
        "generation": 3,
        "manifest": {
            "schema": "lode/v1", "name": "app", "key_id": "old", "sig": "c2ln",
            "channels": { "beta": { "latest": "1.0.0" } },
            "versions": { "1.0.0": { "assets": [
                { "name": "a.bin", "url": "https://r/a.bin", "sha256": "a".repeat(64) },
            ] } },
        },
    });
    let commit_posts = AtomicUsize::new(0);
    let registry = Registry::start(move |request, n| {
        if request.target.ends_with("/commits/head") {
            // Empty until the first commit is refused.
            return if commit_posts.load(Ordering::SeqCst) == 0 {
                ok(&empty_head())
            } else {
                ok(&head)
            };
        }
        if request.target.ends_with("/commits") && commit_posts.fetch_add(1, Ordering::SeqCst) == 0
        {
            return error(409, "COMMIT_CONFLICT", "the head moved");
        }
        happy(request, n)
    });
    let dir = scratch("conflict");
    let files = [file(&dir, "app.tar.gz", b"hello world")];
    let key_path = file(&dir, "signer.key", B64.encode(SECRET).as_bytes());
    let mut req = request(&registry.base, &files);
    req.key = Some(&key_path);

    let (result, out) = publish_to(&req, 100);
    result.unwrap();
    assert!(out.contains("the manifest moved on"));
    let commits = commits(&registry);
    assert_eq!(commits.len(), 2);
    let (_, manifest) = &commits[1];
    assert_eq!(manifest.generation, Some(4));
    assert_eq!(manifest.parent.as_deref(), Some(parent.as_str()));
    assert_eq!(manifest.channels["beta"].latest, "1.0.0");
    assert_eq!(manifest.channels["stable"].latest, "1.4.2");
    assert!(manifest.versions.contains_key("1.0.0"));
    assert_signed(manifest);
}

fn two_versions_head() -> Value {
    let asset = |name: &str| json!({ "name": name, "url": "", "sha256": "a".repeat(64) });
    json!({
        "commit": "c".repeat(64),
        "generation": 1,
        "manifest": {
            "schema": "lode/v1", "name": "app",
            "channels": { "stable": { "latest": "2.0.0" } },
            "versions": {
                "1.0.0": { "assets": [asset("a.bin")] },
                "2.0.0": { "assets": [asset("a.bin")] },
            },
        },
    })
}

fn commit_to_stub(change: &commit::Change<'_>, dir: &Path) -> (Registry, Result<()>) {
    let registry = Registry::start(|request, n| {
        if request.target.ends_with("/commits/head") {
            return ok(&two_versions_head());
        }
        happy(request, n)
    });
    let key_path = file(dir, "signer.key", B64.encode(SECRET).as_bytes());
    let target = CommitTarget {
        registry: registry.base.clone(),
        package: PACKAGE.to_owned(),
        key: Some(key_path),
        key_env: None,
        allow_insecure: false,
    };
    let interrupted = AtomicBool::new(false);
    let env = Env {
        token: TOKEN,
        interrupted: &interrupted,
        retry_pause: Duration::ZERO,
        single_max: SINGLE_REQUEST_MAX,
        asset_max: ASSET_MAX,
    };
    let result = commit_with(&target, change, &env, &mut Vec::new());
    (registry, result)
}

#[test]
fn yank_and_channel_commit_their_change() {
    let dir = scratch("yank");
    let (registry, result) = commit_to_stub(&commit::Change::Yank { version: "1.0.0" }, &dir);
    result.unwrap();
    let (_, manifest) = &commits(&registry)[0];
    assert!(!manifest.versions.contains_key("1.0.0"));
    assert_eq!(manifest.generation, Some(2));
    assert_signed(manifest);

    let (registry, result) = commit_to_stub(&commit::Change::Yank { version: "2.0.0" }, &dir);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("channel stable points at 2.0.0"), "{err}");
    assert!(commits(&registry).is_empty());

    let change = commit::Change::Channel {
        channel: "stable",
        version: "1.0.0",
    };
    let (registry, result) = commit_to_stub(&change, &dir);
    result.unwrap();
    assert_eq!(commits(&registry)[0].1.channels["stable"].latest, "1.0.0");

    let change = commit::Change::Channel {
        channel: "stable",
        version: "9.9.9",
    };
    let (_, result) = commit_to_stub(&change, &dir);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("not in the manifest")
    );
}
