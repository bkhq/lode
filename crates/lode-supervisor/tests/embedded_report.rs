//! An embedding host turns on status reporting through the public
//! `Config::builder()` and gets the same exports as the `lode` binary: metrics
//! and the `lode.started` / `lode.stopping` events, with `[report].headers`.
//!
//! Helpers return `Result` rather than `unwrap`ing (see `api_smoke.rs`).
#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One request the stub endpoint received: path, raw headers, body.
type Request = (String, String, String);

/// Accept requests forever, answering each with `{}`.
fn stub(listener: TcpListener, seen: Arc<Mutex<Vec<Request>>>) {
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            if let Ok(request) = read_request(sock)
                && let Ok(mut seen) = seen.lock()
            {
                seen.push(request);
            }
        }
    });
}

fn read_request(sock: std::net::TcpStream) -> std::io::Result<Request> {
    let mut reader = BufReader::new(sock);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
    let mut headers = String::new();
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header.trim().is_empty() {
            break;
        }
        if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
        headers.push_str(&header);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    reader
        .get_mut()
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")?;
    Ok((path, headers, String::from_utf8_lossy(&body).into_owned()))
}

fn fresh_dir(tag: &str) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("lode-embed-report-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A worker script installed and activated as `1.0.0`.
fn seed(cfg: &lode_core::Config, dir: &std::path::Path) -> lode_core::Result<()> {
    let worker = dir.join("worker.sh");
    std::fs::write(
        &worker,
        "#!/bin/sh\ntrap 'exit 0' TERM\nwhile true; do sleep 1 & wait $!; done\n",
    )?;
    lode_core::install::seed_local(cfg, "1.0.0", &worker, true)
}

fn logged(seen: &Mutex<Vec<Request>>, event: &str) -> bool {
    seen.lock().is_ok_and(|seen| {
        seen.iter()
            .any(|(path, _, body)| path == "/otlp/v1/logs" && body.contains(event))
    })
}

#[test]
fn serve_embedded_reports_metrics_and_lifecycle_events() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    stub(listener, Arc::clone(&seen));

    let dir = fresh_dir("events").unwrap();
    let cfg = lode_core::Config::builder()
        .app("embedded")
        .dir(dir.join("data").display().to_string())
        .policy(lode_core::Policy::Off)
        .run("./worker.sh")
        .report_endpoint(format!("http://127.0.0.1:{port}/otlp"))
        .report_interval(1)
        .report_header("Authorization: Bearer host-token")
        .report_instance_id("host-01")
        .build()
        .unwrap();
    seed(&cfg, &dir).unwrap();

    let (signals, mut source) = lode_supervisor::signal_channel();
    let supervisor = std::thread::spawn(move || {
        lode_supervisor::serve_embedded(
            cfg,
            &mut source,
            lode_supervisor::SuperviseOptions::host_owned(),
        )
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    while !logged(&seen, "lode.started") {
        assert!(Instant::now() < deadline, "no lode.started export");
        std::thread::sleep(Duration::from_millis(100));
    }
    signals.send(15).unwrap();
    supervisor.join().unwrap().unwrap();

    assert!(logged(&seen, "lode.stopping"));
    let seen = seen.lock().unwrap();
    let metrics = seen
        .iter()
        .find(|(path, _, _)| path == "/otlp/v1/metrics")
        .unwrap();
    assert!(
        metrics
            .1
            .to_ascii_lowercase()
            .contains("authorization: bearer host-token")
    );
    for needle in [
        r#""service.name""#,
        r#""embedded""#,
        r#""host-01""#,
        "lode.app.status",
    ] {
        assert!(metrics.2.contains(needle), "metrics lack {needle}");
    }
    assert_eq!(seen.last().unwrap().0, "/otlp/v1/metrics", "a final export");
    drop(seen);
    let _ = std::fs::remove_dir_all(&dir);
}
