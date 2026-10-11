//! Windows process primitives: a Job Object contains the app's process tree, a
//! `CTRL_BREAK` console event asks it to stop, and console events to lode itself
//! arrive through `ctrlc`.
//!
//! Windows has no process groups to signal and no `waitpid(-1)`, so the spawned
//! children are kept in a table keyed by pid; the supervise loop keeps
//! addressing them by pid, as it does on Unix.

use std::collections::HashMap;
use std::os::raw::c_int;
use std::os::windows::io::AsRawHandle as _;
use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use win32job::{ExtendedLimitInfo, Job};

use lode_core::config::Config;
use lode_core::{Error, Result};

use super::STOP_POLL;
use crate::supervise::SignalSource;

/// The child leads its own process group, which is what a `CTRL_BREAK` targets.
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// A process id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pid(i32);

impl Pid {
    pub(crate) const fn as_raw(self) -> i32 {
        self.0
    }
}

/// The requests the supervise loop understands. Windows delivers none of them
/// as signals: console events map onto `SIGTERM`, and a host may feed any of
/// them through a channel signal source.
// The variant names are the ones the Unix `Signal` type uses.
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    SIGINT = 2,
    SIGQUIT = 3,
    SIGTERM = 15,
}

impl TryFrom<i32> for Signal {
    type Error = ();

    fn try_from(raw: i32) -> std::result::Result<Self, ()> {
        match raw {
            2 => Ok(Self::SIGINT),
            3 => Ok(Self::SIGQUIT),
            15 => Ok(Self::SIGTERM),
            _ => Err(()),
        }
    }
}

/// How a child ended: its exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WaitStatus(i32);

impl From<ExitStatus> for WaitStatus {
    fn from(status: ExitStatus) -> Self {
        Self(status.code().unwrap_or(1))
    }
}

/// A spawned child and the job that contains its process tree. Dropping the job
/// closes its handle, which terminates whatever is still running in it.
struct Running {
    child: Child,
    job: Option<Job>,
}

static CHILDREN: LazyLock<Mutex<HashMap<i32, Running>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn children() -> MutexGuard<'static, HashMap<i32, Running>> {
    CHILDREN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Spawn `argv` in `workdir` with exactly `env` (stdio inherited), as the leader
/// of its own process group and inside a kill-on-close Job Object: the whole
/// tree dies with the job, and the job dies with lode.
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
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    let child = cmd
        .spawn()
        .map_err(|e| Error::Process(format!("spawn {program}: {e}")))?;
    let pid = i32::try_from(child.id())
        .map_err(|_| Error::Process("child pid out of range".to_owned()))?;
    let job = contain(&child);
    let mut table = children();
    table.insert(pid, Running { child, job });
    drop(table);
    Ok(Pid(pid))
}

/// Put `child` in a kill-on-close job. Best-effort: without the job the child
/// still runs, but its descendants can outlive it.
fn contain(child: &Child) -> Option<Job> {
    let mut limits = ExtendedLimitInfo::new();
    limits.limit_kill_on_job_close();
    let job = Job::create_with_limit_info(&limits)
        .and_then(|job| {
            // win32job takes the process handle as an integer.
            job.assign_process(child.as_raw_handle() as isize)?;
            Ok(job)
        })
        .map_err(|e| tracing::warn!(error = %e, "could not put the app in a job object"));
    job.ok()
}

/// One non-blocking reap: the pid and status of a child that has exited, or
/// `None` when all are still running.
pub(crate) fn reap_next() -> Option<(Option<Pid>, WaitStatus)> {
    let mut table = children();
    let (pid, status) = table.iter_mut().find_map(|(pid, running)| {
        let status = running.child.try_wait().ok()??;
        Some((*pid, WaitStatus::from(status)))
    })?;
    let exited = table.remove(&pid);
    drop(table);
    // Dropping the entry closes its job: anything the child left running goes.
    drop(exited);
    Some((Some(Pid(pid)), status))
}

/// Ask the child's process group to stop. Every request is delivered as a
/// `CTRL_BREAK`, the one event a process group can be sent.
pub(crate) fn signal_child(pid: Pid, _sig: Signal) -> std::io::Result<()> {
    let group = u32::try_from(pid.0).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    lode_winproc::send_ctrl_break(group)
}

/// Gracefully stop a child: `CTRL_BREAK` to its process group, wait up to
/// `timeout`, then terminate the whole job. Returns its exit status.
pub(crate) fn graceful_stop(pid: Pid, timeout: Duration) -> Option<WaitStatus> {
    let mut running = children().remove(&pid.0)?;
    if let Err(e) = signal_child(pid, Signal::SIGTERM) {
        // No shared console: the app can still see `status = stopping` in
        // state.json and exit on its own within the timeout.
        tracing::warn!(error = %e, "could not send CTRL_BREAK to the app");
    }
    let deadline = Instant::now() + timeout;
    loop {
        match running.child.try_wait() {
            Ok(Some(status)) => return Some(status.into()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(STOP_POLL),
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    // Closing the job terminates the tree; `kill` covers a child without a job.
    drop(running.job.take());
    let _ = running.child.kill();
    running.child.wait().ok().map(WaitStatus::from)
}

/// Nothing to do: the job is closed when lode ends, however it ends, so a
/// previous lode cannot leave an app behind.
pub(crate) const fn terminate_external(_pid: Pid, _timeout: Duration) {}

/// Is the child still running?
pub(crate) fn process_alive(pid: Pid) -> bool {
    children()
        .get_mut(&pid.0)
        .is_some_and(|running| matches!(running.child.try_wait(), Ok(None)))
}

/// Translate an exit status into a process exit code. Windows codes are 32-bit;
/// one that does not fit (an NTSTATUS such as a crash) is reported as `1`.
pub(crate) fn exit_code_from(status: WaitStatus) -> u8 {
    u8::try_from(status.0).unwrap_or(1)
}

/// Did the child *fail*? Anything but a clean `exit(0)`.
pub(crate) const fn is_failure(status: WaitStatus) -> bool {
    status.0 != 0
}

/// There are no orphans to adopt on Windows.
pub(crate) const fn set_subreaper() {}

/// A pid recorded by a previous lode is never acted on: its job closed with it,
/// and the pid may since belong to an unrelated process.
pub(crate) const fn orphan_pid(_st_pid: Option<u32>) -> Option<Pid> {
    None
}

/// No signals are forwarded on Windows.
pub(crate) const fn default_forward() -> Vec<Signal> {
    Vec::new()
}

/// Parse a signal name, accepting both `SIGTERM` and `TERM` (any case). Only
/// the termination set exists here.
pub(crate) fn parse_signal(name: &str) -> Option<Signal> {
    let upper = name.trim().to_ascii_uppercase();
    match upper.strip_prefix("SIG").unwrap_or(&upper) {
        "INT" => Some(Signal::SIGINT),
        "QUIT" => Some(Signal::SIGQUIT),
        "TERM" => Some(Signal::SIGTERM),
        _ => None,
    }
}

/// The signals the loop wants registered: the termination set plus the
/// configured ones, deduplicated.
pub(crate) fn registration_set(forward: &[Signal], restart: Option<Signal>) -> Vec<c_int> {
    let mut wanted = vec![Signal::SIGTERM, Signal::SIGINT, Signal::SIGQUIT];
    wanted.extend(forward.iter().copied());
    wanted.extend(restart);
    let mut ints: Vec<c_int> = Vec::new();
    for sig in wanted {
        let raw = sig as c_int;
        if !ints.contains(&raw) {
            ints.push(raw);
        }
    }
    ints
}

/// OWNED signal source: lode installs the process-wide console control handler.
/// Ctrl-C, Ctrl-Break, closing the console, logoff and shutdown all ask for a
/// graceful stop.
#[derive(Debug)]
pub struct OwnedSignalSource(mpsc::Receiver<i32>);

impl OwnedSignalSource {
    /// Install the console control handler.
    ///
    /// # Errors
    /// Returns [`Error::Process`] if a handler is already installed.
    pub fn new(_cfg: &Config) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        ctrlc::set_handler(move || {
            let _ = tx.send(Signal::SIGTERM as i32);
        })
        .map_err(|e| Error::Process(format!("install the console control handler: {e}")))?;
        Ok(Self(rx))
    }
}

impl SignalSource for OwnedSignalSource {
    fn poll_signals(&mut self) -> Vec<i32> {
        self.0.try_iter().collect()
    }

    /// No-op: every console event is already delivered.
    fn add_signal(&mut self, _signum: i32) -> Result<()> {
        Ok(())
    }
}
