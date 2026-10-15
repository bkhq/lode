//! `lode update` — fetch the manifest, resolve a target, then download, verify
//! and install it (design §5/§13).
//!
//! If a supervised instance is running — a live holder of the single-instance
//! `lode.pid` lock, or a live `state.pid` — the new version is installed and
//! requested as a hot-update by writing `state.target`: the running lode polls
//! `state.json` and applies it (§7), and a paused or crash-backoff supervisor
//! (whose `state.pid` is cleared or dead, P2-15) picks it up as a recovery
//! target. Otherwise this command activates the version directly: flip the
//! `current` symlink and record it in `state.json`. Either way old versions are
//! pruned per `keep_versions`.

use std::io::Write;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::{download, install, manifest, state};

/// Run `update`, installing `version` (or the channel latest when `None`).
pub fn run(cfg: &Config, version: Option<&str>) -> Result<()> {
    // Clear any interrupted downloads/staging before we begin.
    install::gc(cfg)?;

    let manifest = manifest::fetch(cfg)?;
    if manifest.name != cfg.global.app {
        return Err(Error::Manifest(format!(
            "manifest name {:?} does not match configured app {:?}",
            manifest.name, cfg.global.app
        )));
    }
    // Verify the catalog signature if it carries one (verify-if-present); a missing
    // catalog signature is fine — the per-artifact check below binds the download.
    install::verify_manifest_identity(cfg, &manifest)?;

    // Anti-downgrade floor: the highest version we've already committed to. It gates
    // only the channel-`latest`-following resolution, so a tampered or replayed
    // catalog cannot silently roll us back; an explicit `--version` or `pin` is the
    // operator's deliberate choice and is never blocked.
    let state_path = cfg.global.dir.join("state.json");
    let prior = state::read(&state_path)?.unwrap_or_default();
    let floor = install::version_floor(prior.current.as_deref(), prior.last_good.as_deref());

    let target = manifest::resolve_target(
        &manifest,
        &cfg.update.channel,
        cfg.update.pin.as_deref(),
        version,
        floor.as_deref(),
    )?;
    let entry = manifest::version_entry(&manifest, &target)?;

    let asset_name = cfg.update.asset.as_deref().ok_or_else(|| {
        Error::Config(
            "no [update].asset configured — set the asset filename to install (source-adapters §3)"
                .to_owned(),
        )
    })?;
    let asset = manifest::select_asset(entry, asset_name)?;

    let (temp, sha256) =
        download::fetch_artifact(cfg, asset, &target, &manifest::allowed_hosts(cfg))?;
    install::install(cfg, &target, asset, &temp, &sha256)?;

    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "lode update: installed {target} ({})",
        manifest::format_from_name(&asset.name)
    )?;
    if let Some(notes) = entry.notes.as_deref() {
        writeln!(out, "  notes: {}", printable(notes))?;
    }

    // Liveness (P2-15): `state.pid` alone misses a paused supervisor (`pid` is
    // cleared) and a crash-backoff one (its recorded pid is dead), yet both stay
    // alive holding the single-instance lock and still poll `state.json`. Probe
    // the lock too, so they receive the update as a recovery `target` request
    // instead of having `current` flipped under them. The RMWs below re-read
    // under the shared `state.json.lock` flock (P2-14), so a concurrent
    // supervisor/app write is never clobbered.
    let holder = live_holder(cfg);
    let st = state::read(&state_path)?.unwrap_or_default();
    let st = if supervisor_running(&st, holder) {
        // Hand off to the running supervisor via the app-owned request channel
        // (a paused supervisor honours `target` as a recovery trigger, §8).
        let st = state::locked_update(&state_path, |st| {
            st.channel = Some(cfg.update.channel.clone());
            st.available = Some(target.clone());
            st.target = Some(target.clone());
        })?;
        writeln!(
            out,
            "  a service is running — requested hot-update to {target}"
        )?;
        st
    } else {
        install::switch_current(cfg, &target)?;
        let st = state::locked_update(&state_path, |st| {
            st.channel = Some(cfg.update.channel.clone());
            st.current = Some(target.clone());
            if st.last_good.is_none() {
                st.last_good = Some(target.clone());
            }
            st.available = None;
        })?;
        writeln!(out, "  activated {target} (current)")?;
        st
    };

    install::prune(cfg, st.current.as_deref(), st.last_good.as_deref())?;
    Ok(())
}

/// Release notes come from the network: drop every control character except the
/// line break before they reach the operator's terminal, so a hostile catalog
/// cannot smuggle terminal escape sequences through `lode-cli update`.
fn printable(notes: &str) -> String {
    notes
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

/// Is a supervising lode currently running? True when a live process holds the
/// single-instance `lode.pid` lock — which catches a paused (`pid: None`) or
/// crash-backoff (dead `pid`) supervisor (P2-15) — or, belt-and-braces, when
/// `state.pid` itself names a live process (a no-signal `kill` succeeds, §13).
pub(crate) fn supervisor_running(st: &state::State, lock_holder: Option<u32>) -> bool {
    lock_holder.is_some() || st.pid.is_some_and(process_alive)
}

/// The pid of a live supervisor holding the single-instance lock, if any.
pub(crate) fn live_holder(cfg: &Config) -> Option<u32> {
    crate::lock::live_holder(&cfg.global.dir)
}

/// Liveness probe via signal 0 (`kill(pid, None)`): `Ok` if the process exists.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None).is_ok()
}

/// Off Unix the lock alone tells whether a supervisor is running.
#[cfg(not(unix))]
const fn process_alive(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_alive_for_self_and_dead_for_unused_pid() {
        // Our own pid is alive.
        assert!(process_alive(std::process::id()));
        // A pid that overflows i32 is rejected by the guard.
        assert!(!process_alive(u32::MAX));
        // A high but in-range pid is almost certainly unused → kill reports ESRCH.
        assert!(!process_alive(2_000_000_000));
    }

    #[test]
    fn printable_strips_control_characters_but_keeps_line_breaks() {
        assert_eq!(
            printable("fixed \x1b[31mred\x1b[0m\nsecond\r\tline\x07"),
            // The escape byte is gone, so the leftover "[31m" is inert text.
            "fixed [31mred[0m\nsecondline"
        );
        assert_eq!(printable("plain"), "plain");
    }

    #[test]
    fn supervisor_running_false_without_pid_or_lock_holder() {
        assert!(!supervisor_running(&state::State::default(), None));
        // A dead recorded pid alone is not "running" either.
        let st = state::State {
            pid: Some(2_000_000_000),
            ..state::State::default()
        };
        assert!(!supervisor_running(&st, None));
    }

    #[test]
    fn supervisor_running_via_lock_holder_despite_missing_or_dead_pid() {
        // Paused supervisor: `pid` cleared in state.json, but the lock is held.
        assert!(supervisor_running(&state::State::default(), Some(4242)));
        // Crash-backoff supervisor: a dead pid recorded, lock still held.
        let st = state::State {
            pid: Some(2_000_000_000),
            ..state::State::default()
        };
        assert!(supervisor_running(&st, Some(4242)));
    }

    #[test]
    fn supervisor_running_via_live_state_pid_alone() {
        let st = state::State {
            pid: Some(std::process::id()),
            ..state::State::default()
        };
        assert!(supervisor_running(&st, None));
    }
}
