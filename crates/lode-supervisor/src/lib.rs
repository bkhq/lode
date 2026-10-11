#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! lode-supervisor — the embeddable supervised-service runtime + exec passthrough
//! (design §5/§8/§9).
//!
//! [`serve_embedded`] drives the same loop bare `lode` runs: acquire the single-instance lock, clean up
//! orphans/garbage from a previous run, decide which version to launch (bootstrap
//! the latest only when nothing is installed), then spawn the app as a child and
//! supervise it. By default (`supervise.restart=on-failure`) lode keeps the app
//! alive: a failing app is retried with exponential backoff and, after the retry
//! cap, lode *pauses* — it stays alive (never crash-loops the container / exits as
//! PID 1) until a recovery trigger (an edited `lode.toml`, a bumped `restart_nonce`,
//! or a new `target`). `off` opts back into mirror-the-child (lode exits with it); a
//! clean `exit(0)` exits lode (use `always` to retry that too). lode also relaunches
//! the child for lode-initiated transitions — an update, a single-strike rollback,
//! or an explicit restart — does signal forwarding, graceful stop, and (as
//! PID 1) child-subreaping so re-parented grandchildren never become zombies. On
//! the same short-interval tick the loop also drives the C2 update
//! machinery: it polls `state.json`'s mtime for app-written `target` /
//! `restart_nonce` requests (§7), runs the `[update].policy` check (§5), and — when
//! a target is applied — performs the stop-start hot-update with the readiness/stop
//! handshake (§8) and automatic rollback to `last_good` on failure.
//!
//! [`serve_core`] is the same loop with a caller-supplied config *reloader*, which
//! is what the `lode` binary's CLI wrapper uses to re-resolve from its parsed
//! globals on a `lode.toml`-change reload.
//!
//! [`exec_passthrough`] is the `lode <args>` path: validate the version (bootstrap
//! if none), prepare the same argv/env/runtime, then hand over to the app — no
//! lock, no supervision, no polling. On Unix the process is replaced with the safe
//! `CommandExt::exec`; on Windows the app runs as a child
//! and lode exits with its status.
//!
//! The process and signal primitives under the loop are per platform: process
//! groups and signals on Unix, a Job Object and console events on Windows.

mod launch;
mod supervise;
mod sys;

pub use launch::exec_passthrough;
pub use supervise::*;
pub use sys::OwnedSignalSource;
