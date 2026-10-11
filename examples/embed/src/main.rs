//! A host program that embeds lode as a library instead of running the `lode`
//! binary.
//!
//! The host keeps control of its own process: it installs its own signal
//! handlers, decides when to switch versions, and runs lode's supervise loop on
//! a thread. lode gets no global signal handlers, no subreaper and no lock.
//!
//! The demo is offline. It seeds two versions of a worker script, starts on
//! 1.1.0, then walks through what a real host would do:
//!
//!   1. supervise the worker            (`serve_embedded`)
//!   2. switch it to another version    (`Engine::rollback`)
//!   3. restart it                      (`Engine::restart`)
//!   4. stop it and collect the status  (a SIGTERM fed through the channel)
//!
//! Press Ctrl-C at any time: the host's own handler forwards it to the loop.
//!
//! ```bash
//! cargo run --manifest-path examples/embed/Cargo.toml
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use lode_core::{Config, Engine, InitOptions};
use lode_supervisor::{SuperviseOptions, serve_embedded, signal_channel};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("host: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    // Process-wide effects are opt-in for a library user. This host wants lode's
    // log lines; it would add `.crypto_provider(true)` before any HTTPS fetch.
    InitOptions::new().log_level("info").install();

    let dir = std::env::temp_dir().join(format!("lode-demo-embed-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let cfg = config(&dir)?;

    // Two installed versions, the newer one active. A real host would let lode
    // download and verify them from `[update].manifest` or `[update].github`.
    seed(&cfg, &dir, "1.0.0", false)?;
    seed(&cfg, &dir, "1.1.0", true)?;

    // The channel is the only way signals reach the supervise loop.
    let (signals_tx, mut signals_rx) = signal_channel();

    // The host's own signal handling: Ctrl-C and SIGTERM become a graceful stop.
    let forward = signals_tx.clone();
    let mut host_signals = Signals::new([SIGINT, SIGTERM])?;
    thread::spawn(move || {
        for signal in host_signals.forever() {
            eprintln!("host: got signal {signal}, stopping the worker");
            let _ = forward.send(SIGTERM);
        }
    });

    // The supervise loop blocks until the worker is stopped, so it gets a thread.
    // `host_owned()`: no subreaper, no single-instance lock.
    let supervised = cfg.clone();
    let supervisor = thread::spawn(move || {
        serve_embedded(supervised, &mut signals_rx, SuperviseOptions::host_owned())
    });

    // The host's own logic. `Engine` is the same facade `lode-cli` uses: these
    // calls write a request into state.json, and the loop picks it up.
    let engine = Engine::new(cfg);
    thread::sleep(Duration::from_secs(3));
    eprintln!("host: switching the worker to 1.0.0");
    engine.rollback(Some("1.0.0"))?;

    thread::sleep(Duration::from_secs(4));
    eprintln!("host: restarting the worker");
    engine.restart()?;

    thread::sleep(Duration::from_secs(3));
    eprintln!("host: shutting down");
    let _ = signals_tx.send(SIGTERM);

    let code = supervisor
        .join()
        .map_err(|_| "the supervise thread panicked")??;
    eprintln!("host: the worker stopped with {code:?}");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(code)
}

/// The configuration a `lode.toml` would hold, built in code.
fn config(dir: &Path) -> lode_core::Result<Config> {
    Config::from_toml(&format!(
        r#"
[global]
app = "worker"
dir = "{dir}"

[update]
policy = "off"            # this demo never fetches

[command]
run = "./worker.sh"

[supervise]
health_grace = 2          # seconds a switched-to version must survive
stop_timeout = 5
"#,
        dir = dir.join("data").display()
    ))
}

/// Install a local file as `version`, the way `lode-cli seed` does.
fn seed(cfg: &Config, dir: &Path, version: &str, activate: bool) -> lode_core::Result<()> {
    let staging: PathBuf = dir.join("build").join(version);
    std::fs::create_dir_all(&staging)?;
    let worker = staging.join("worker.sh");
    std::fs::write(
        &worker,
        format!(
            "#!/bin/sh\n\
             trap 'echo \"worker {version}: stopping\"; exit 0' TERM\n\
             echo \"worker {version}: started (LODE_ACTIVE_VERSION=$LODE_ACTIVE_VERSION)\"\n\
             while true; do sleep 1 & wait $!; done\n"
        ),
    )?;
    lode_core::install::seed_local(cfg, version, &worker, activate)
}
