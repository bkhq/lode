//! Unix process and signal primitives: process groups, `kill`, `waitpid`,
//! signal-hook.

use std::os::raw::c_int;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use nix::errno::Errno;
pub(crate) use nix::sys::signal::Signal;
use nix::sys::signal::kill;
pub(crate) use nix::sys::wait::WaitStatus;
use nix::sys::wait::{WaitPidFlag, waitpid};
pub(crate) use nix::unistd::Pid;
use signal_hook::iterator::Signals;

use lode_core::config::Config;
use lode_core::{Error, Result};

use super::STOP_POLL;
use crate::supervise::{SignalSource, forward_signals};

/// One non-blocking reap of any child (the supervised one or a re-parented
/// grandchild): its pid and status, or `None` when nothing changed state.
pub(crate) fn reap_next() -> Option<(Option<Pid>, WaitStatus)> {
    match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
        // No more children have changed state, or none remain (ECHILD).
        Ok(WaitStatus::StillAlive) | Err(_) => None,
        Ok(status) => Some((status.pid(), status)),
    }
}

/// OWNED signal source: lode installs the process-global signal dispositions via
/// signal-hook. This is the bare-`lode` binary path — identical registration to
/// the pre-seam loader.
#[derive(Debug)]
pub struct OwnedSignalSource(Signals);

impl OwnedSignalSource {
    /// Install lode's standard supervise set (termination + the config's
    /// forward/restart sets) via signal-hook, exactly as the loader does.
    ///
    /// # Errors
    /// Returns [`Error::Process`] if signal-hook cannot install the handlers.
    pub fn new(cfg: &Config) -> Result<Self> {
        let signals = Signals::new(registration_set(
            &forward_signals(&cfg.signals.forward),
            cfg.signals.restart.as_deref().and_then(parse_signal),
        ))
        .map_err(|e| Error::Process(format!("install signal handlers: {e}")))?;
        Ok(Self(signals))
    }
}

impl SignalSource for OwnedSignalSource {
    fn poll_signals(&mut self) -> Vec<i32> {
        self.0.pending().collect()
    }

    fn add_signal(&mut self, signum: i32) -> Result<()> {
        self.0
            .add_signal(signum)
            .map_err(|e| Error::Process(format!("install signal handlers: {e}")))
    }
}

/// lode's standard forward set when `[signals].forward` is unset (design §8).
pub(crate) fn default_forward() -> Vec<Signal> {
    vec![
        Signal::SIGHUP,
        Signal::SIGUSR1,
        Signal::SIGUSR2,
        Signal::SIGWINCH,
        Signal::SIGCONT,
        Signal::SIGTSTP,
    ]
}

/// Parse a signal name, accepting both `SIGHUP` and `HUP` (any case).
pub(crate) fn parse_signal(name: &str) -> Option<Signal> {
    let upper = name.trim().to_ascii_uppercase();
    let canonical = if upper.starts_with("SIG") {
        upper
    } else {
        format!("SIG{upper}")
    };
    canonical.parse().ok()
}

/// signal-hook refuses to register these (they cannot be caught or trigger UB).
pub(crate) const fn is_forbidden(sig: Signal) -> bool {
    matches!(
        sig,
        Signal::SIGKILL | Signal::SIGSTOP | Signal::SIGILL | Signal::SIGFPE | Signal::SIGSEGV
    )
}

/// The signals to register: termination set + forward set + restart signal,
/// minus the forbidden ones, deduplicated. A free function so [`crate::serve_core`] can
/// install the set before any [`crate::Supervisor`] exists (bootstrap must stay
/// interruptible) and [`crate::Supervisor::run`] can re-add a reloaded config's set.
pub(crate) fn registration_set(forward: &[Signal], restart: Option<Signal>) -> Vec<c_int> {
    let mut wanted = vec![Signal::SIGTERM, Signal::SIGINT, Signal::SIGQUIT];
    wanted.extend(forward.iter().copied());
    if let Some(sig) = restart {
        wanted.push(sig);
    }
    let mut ints: Vec<c_int> = Vec::new();
    for sig in wanted {
        if is_forbidden(sig) {
            tracing::warn!(
                signal = sig.as_str(),
                "refusing to register forbidden signal"
            );
            continue;
        }
        let raw = sig as c_int;
        if !ints.contains(&raw) {
            ints.push(raw);
        }
    }
    ints
}

/// Spawn `argv` in `workdir` with exactly `env` (stdio inherited). The child is
/// made the leader of its OWN process group (`process_group(0)`), so stop/forward
/// signals can reach the whole tree — a fork-model app's workers must die with it,
/// or the old version's workers would hold the port across an update (P2-16).
/// Returns the child pid; the [`std::process::Child`] is dropped (its `Drop`
/// neither waits nor kills) because the supervisor reaps via `waitpid` to also
/// harvest grandchildren.
pub(crate) fn spawn_process(
    argv: &[String],
    workdir: &Path,
    env: &[(String, String)],
) -> Result<Pid> {
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| Error::Process("empty command".to_owned()))?;
    let mut cmd = Command::new(program);
    cmd.args(rest).current_dir(workdir).env_clear();
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    cmd.process_group(0); // child leads its own group (pgid = its pid)
    let child = cmd
        .spawn()
        .map_err(|e| Error::Process(format!("spawn {program}: {e}")))?;
    i32::try_from(child.id())
        .map(Pid::from_raw)
        .map_err(|_| Error::Process("child pid out of range".to_owned()))
}

/// The process-group target for `pid` (its negation, per `kill(2)`), or `None`
/// unless `pid` is a plausible child (raw > 1). Negating 0 or 1 turns a group
/// signal into `kill(0, …)` (lode's OWN process group) or `kill(-1, …)` (every
/// process on the system when lode runs as PID 1) — a torn/stale `state.json`
/// carrying `"pid": 0|1` must never broadcast-kill the container (R2-1).
pub(crate) fn group_target(pid: Pid) -> Option<Pid> {
    (pid.as_raw() > 1).then(|| Pid::from_raw(-pid.as_raw()))
}

/// Signal the child's whole process group (the child leads its own group — see
/// [`spawn_process`]), falling back to the single pid when the group signal fails
/// (e.g. an orphan recorded by an older lode that did not set a process group).
/// Refuses pid <= 1 outright with `ESRCH` — the bare-pid fallback would be just
/// as destructive there (`kill(1, …)` signals init / lode itself) (R2-1).
pub(crate) fn signal_child(pid: Pid, sig: Signal) -> std::result::Result<(), Errno> {
    let group = group_target(pid).ok_or(Errno::ESRCH)?;
    kill(group, sig).or_else(|_| kill(pid, sig))
}

/// Gracefully stop a specific child: `SIGTERM` to its process group, wait up to
/// `timeout` (never killing early), then `SIGKILL` the group. Reaps the child and
/// returns its exit status (`waitpid` still targets the child pid alone).
pub(crate) fn graceful_stop(pid: Pid, timeout: Duration) -> Option<WaitStatus> {
    let _ = signal_child(pid, Signal::SIGTERM);
    let deadline = Instant::now() + timeout;
    loop {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(STOP_POLL);
            }
            Ok(status) => return Some(status),
            Err(_) => return None,
        }
    }
    let _ = signal_child(pid, Signal::SIGKILL);
    waitpid(pid, None).ok()
}

/// Terminate an external process we cannot reap (an orphan re-parented to init):
/// `SIGTERM`, poll liveness up to `timeout`, then `SIGKILL`. Best-effort group
/// first (an orphan from this lode leads its own group), then the bare pid.
pub(crate) fn terminate_external(pid: Pid, timeout: Duration) {
    if signal_child(pid, Signal::SIGTERM).is_err() {
        return; // already gone
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return;
        }
        std::thread::sleep(STOP_POLL);
    }
    let _ = signal_child(pid, Signal::SIGKILL);
}

/// Liveness probe via signal 0: alive unless `kill` reports `ESRCH`.
pub(crate) fn process_alive(pid: Pid) -> bool {
    !matches!(kill(pid, None), Err(Errno::ESRCH))
}

/// Translate a child wait status into a process exit code (`128 + signal` for a
/// signalled child, mirroring the shell convention).
pub(crate) fn exit_code_from(status: WaitStatus) -> u8 {
    match status {
        WaitStatus::Exited(_, code) => u8::try_from(code).unwrap_or(0),
        WaitStatus::Signaled(_, sig, _) => u8::try_from(128 + (sig as i32)).unwrap_or(255),
        _ => 0,
    }
}

/// Did the child *fail*? Any outcome other than a clean `exit(0)` (a non-zero
/// exit or a fatal signal) counts as a failure for `restart=on-failure`.
pub(crate) const fn is_failure(status: WaitStatus) -> bool {
    !matches!(status, WaitStatus::Exited(_, 0))
}

/// Become a child subreaper so re-parented grandchildren are reaped by us (PID 1
/// init duty). Best-effort: a failure is logged, not fatal.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn set_subreaper() {
    if let Err(e) = nix::sys::prctl::set_child_subreaper(true) {
        tracing::warn!(error = %e, "could not set child subreaper");
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn set_subreaper() {}

/// Validate a `state.pid` for orphan reaping: `None` when it does not fit an
/// i32 or is <= 1 — pid 0/1 is never a real app child, and signalling either
/// would hit lode's own process group / broadcast system-wide as PID 1 (R2-1).
pub(crate) fn orphan_pid(st_pid: Option<u32>) -> Option<Pid> {
    let raw = i32::try_from(st_pid?).ok()?;
    (raw > 1).then(|| Pid::from_raw(raw))
}
