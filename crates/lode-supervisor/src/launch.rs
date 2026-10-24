//! Launching the app: the effective command, its argv and environment, and the
//! `lode <args>` passthrough. Shared by the supervise loop and the passthrough.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::process::Command;

use lode_core::config::Config;
use lode_core::engine::{ensure_target_runtime, resolve_target};
use lode_core::{Error, Result};

/// CLI passthrough (`lode <args>`): `exec`-replace into `[command].exec` + `args`.
///
/// Validates the version, then `exec`-replaces this process. On success it never
/// returns (the process image is replaced); any failure surfaces as
/// [`Error::Process`]. Part of the embeddable surface — Globals-free, so a host
/// can drive the passthrough directly.
///
/// # Errors
/// Returns [`Error::Process`] (or a propagated resolve/runtime error) when the
/// version cannot be resolved/prepared or the `exec` itself fails.
pub fn exec_passthrough(cfg: &Config, args: &[String]) -> Result<Infallible> {
    let target = resolve_target(cfg)?;
    let runtime_dir = ensure_target_runtime(cfg, &target)?;
    let instance = format!("{}-exec", std::process::id());

    let dir = target.dir.to_string_lossy();
    let exec = effective_command(target.exec.as_deref(), cfg.command.exec.as_deref(), "exec")?;
    let command_line = build_exec_argv(&exec, &dir, args)?;
    let mut env = child_env(
        std::env::vars(),
        &cfg.env,
        &target.version,
        &cfg.global.dir,
        &instance,
        runtime_dir.as_deref(),
    );
    let workdir = PathBuf::from(expand_token(&cfg.command.workdir, &dir));
    set_env(&mut env, "LODE_WORKDIR", &workdir.display().to_string());
    if let Some(p) = cfg.config_path.as_deref() {
        set_env(&mut env, "LODE_CONFIG", &p.display().to_string());
    }

    let (program, rest) = command_line
        .split_first()
        .ok_or_else(|| Error::Process("empty exec command".to_owned()))?;
    let mut cmd = Command::new(program);
    cmd.args(rest).current_dir(&workdir).env_clear();
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    hand_over(cmd, program)
}

/// `exec` only returns on failure; on success this process is replaced.
#[cfg(unix)]
fn hand_over(mut cmd: Command, program: &str) -> Result<Infallible> {
    use std::os::unix::process::CommandExt as _;
    let err = cmd.exec();
    Err(Error::Process(format!("exec {program}: {err}")))
}

/// There is no `exec` off unix: run the app as a child sharing this console,
/// wait for it, and exit with its status.
#[cfg(not(unix))]
fn hand_over(mut cmd: Command, program: &str) -> Result<Infallible> {
    let status = cmd
        .status()
        .map_err(|e| Error::Process(format!("run {program}: {e}")))?;
    std::process::exit(status.code().unwrap_or(1))
}

// --- argv + environment ---

/// The launch command actually in force: the manifest asset's signed override
/// (from the version marker) wins over the operator's `[command]` value; blank
/// strings count as unset on both sides. When neither supplies one, launching is
/// impossible — a clear, actionable hard error (design: entry abolition).
pub(crate) fn effective_command(
    override_cmd: Option<&str>,
    configured: Option<&str>,
    kind: &str,
) -> Result<String> {
    override_cmd
        .filter(|c| !c.trim().is_empty())
        .or_else(|| configured.filter(|c| !c.trim().is_empty()))
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::Process(format!(
                "no {kind} command: set [command].{kind} or publish `{kind}` in the manifest asset"
            ))
        })
}

/// Expand the `{dir}` placeholder (the running version dir) in one token. The
/// braces are a literal placeholder to substitute, not Rust format args.
#[allow(clippy::literal_string_with_formatting_args)]
pub(crate) fn expand_token(token: &str, dir: &str) -> String {
    token.replace("{dir}", dir)
}

/// Absolutize a relative program path against the version dir: when the argv's
/// first token is a relative path naming an existing file in `dir` (e.g.
/// `./myapp`, the install-time chmod target), replace it with the absolute path —
/// launch then works regardless of an operator-overridden `workdir`. Any other
/// token (a PATH command like `bun`, or an absolute path) is left untouched.
pub(crate) fn resolve_program(argv: &mut [String], dir: &str) {
    if let Some(first) = argv.first_mut() {
        let path = Path::new(first.as_str());
        if path.is_relative() {
            // Drop a leading `./` so the joined path is clean in logs/argv[0].
            let rel = path.strip_prefix(".").unwrap_or(path);
            let candidate = Path::new(dir).join(rel);
            // On Windows `./myapp` names `myapp.exe`.
            let with_exe = candidate.with_extension("exe");
            if candidate.is_file() {
                *first = candidate.to_string_lossy().into_owned();
            } else if cfg!(windows) && candidate.extension().is_none() && with_exe.is_file() {
                *first = with_exe.to_string_lossy().into_owned();
            }
        }
    }
}

/// Build the bare-run argv from the effective run command: split on whitespace
/// (the command is LITERAL — no shell), expand `{dir}`, and absolutize a
/// version-dir-relative program. Never empty.
// Used by the supervise loop, which is Unix-only.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn build_run_argv(command: &str, dir: &str) -> Result<Vec<String>> {
    let mut argv: Vec<String> = command
        .split_whitespace()
        .map(|t| expand_token(t, dir))
        .collect();
    if argv.is_empty() {
        return Err(Error::Process("empty run command".to_owned()));
    }
    resolve_program(&mut argv, dir);
    Ok(argv)
}

/// Build the passthrough argv from the effective exec command + `args`: split on
/// whitespace, expand `{dir}`, absolutize a version-dir-relative program, then
/// append the user args verbatim.
pub(crate) fn build_exec_argv(command: &str, dir: &str, args: &[String]) -> Result<Vec<String>> {
    let mut parts: Vec<String> = command
        .split_whitespace()
        .map(|t| expand_token(t, dir))
        .collect();
    if parts.is_empty() {
        return Err(Error::Process("empty exec command".to_owned()));
    }
    resolve_program(&mut parts, dir);
    parts.extend(args.iter().cloned());
    Ok(parts)
}

/// Build the child environment: inherit the host env minus all config `LODE_*`
/// vars, apply the operator's `[env]` overrides, optionally prepend the runtime dir
/// to PATH, then inject the read-only introspection vars (design §10). Precedence
/// (low → high): operator `[env]` defaults < inherited host env < runtime
/// PATH-prepend < lode's `LODE_*` vars.
pub(crate) fn child_env(
    host: impl IntoIterator<Item = (String, String)>,
    defined: &BTreeMap<String, String>,
    version: &str,
    dir: &Path,
    instance: &str,
    runtime_dir: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = host
        .into_iter()
        .filter(|(key, _)| !key.starts_with("LODE_"))
        .collect();
    // The `[env]` table is DEFAULTS: applied only for keys the inherited host env
    // doesn't already provide, so a per-deploy `-e KEY=…` (any inherited env var)
    // overrides `[env]`.
    for (key, value) in defined {
        if !env.iter().any(|(k, _)| k == key) {
            env.push((key.to_owned(), value.to_owned()));
        }
    }
    if let Some(dir) = runtime_dir {
        prepend_path(&mut env, dir);
    }
    // lode's introspection vars always win — set (not push) so a `[env]` entry of
    // the same name can't leave a duplicate behind.
    set_env(&mut env, "LODE_ACTIVE_VERSION", version);
    set_env(&mut env, "LODE_DIR", &dir.display().to_string());
    set_env(&mut env, "LODE_INSTANCE", instance);
    set_env(&mut env, "LODE_VERSION", lode_core::build_info::VERSION);
    if let Some(commit) = lode_core::build_info::COMMIT {
        set_env(&mut env, "LODE_COMMIT", commit);
    }
    set_env(
        &mut env,
        "LODE_FEATURES",
        &lode_core::build_info::FEATURES.join(","),
    );
    env
}

/// Set `key` to `value` in `env`, replacing an existing entry or appending a new
/// one (so the result never holds a duplicate key).
pub(crate) fn set_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, slot)) = env.iter_mut().find(|(k, _)| k == key) {
        value.clone_into(slot);
    } else {
        env.push((key.to_owned(), value.to_owned()));
    }
}

/// Prepend `dir` to the PATH entry in `env` (or create PATH if absent).
pub(crate) fn prepend_path(env: &mut Vec<(String, String)>, dir: &Path) {
    let dir = dir.display().to_string();
    // The variable is `Path` on Windows, where names compare case-insensitively.
    let is_path = |key: &str| {
        if cfg!(windows) {
            key.eq_ignore_ascii_case("PATH")
        } else {
            key == "PATH"
        }
    };
    let separator = if cfg!(windows) { ';' } else { ':' };
    if let Some((_, value)) = env.iter_mut().find(|(key, _)| is_path(key)) {
        *value = format!("{dir}{separator}{value}");
    } else {
        env.push(("PATH".to_owned(), dir));
    }
}
