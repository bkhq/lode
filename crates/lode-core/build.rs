//! Embeds the git commit lode is built from as `LODE_BUILD_COMMIT`, and the
//! `--version` line as `LODE_BUILD_LONG_VERSION`.
//!
//! The commit comes from `LODE_BUILD_COMMIT` when set, else from `git` when
//! this crate sits in a lode checkout (never from a repository that merely
//! contains it, such as a host project vendoring lode).

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=LODE_BUILD_COMMIT");
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let commit = std::env::var("LODE_BUILD_COMMIT")
        .ok()
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .or_else(|| git_commit(&manifest_dir))
        .filter(|c| (7..=64).contains(&c.len()) && c.chars().all(|ch| ch.is_ascii_hexdigit()));
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let long = match &commit {
        Some(c) => {
            println!("cargo:rustc-env=LODE_BUILD_COMMIT={c}");
            format!("{version} ({})", &c[..c.len().min(12)])
        }
        None => version,
    };
    println!("cargo:rustc-env=LODE_BUILD_LONG_VERSION={long}");
}

/// `HEAD` of the lode checkout holding `manifest_dir` (`<root>/crates/lode-core`).
fn git_commit(manifest_dir: &Path) -> Option<String> {
    let root = manifest_dir.parent()?.parent()?;
    let top = git(manifest_dir, &["rev-parse", "--show-toplevel"])?;
    if std::fs::canonicalize(top).ok()? != std::fs::canonicalize(root).ok()? {
        return None;
    }
    // Rebuild when HEAD moves: the HEAD file, the branch it points at, packed refs.
    let git_dir = PathBuf::from(git(manifest_dir, &["rev-parse", "--absolute-git-dir"])?);
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );
    if let Some(branch) = git(manifest_dir, &["symbolic-ref", "-q", "HEAD"]) {
        println!("cargo:rerun-if-changed={}", git_dir.join(branch).display());
    }
    git(manifest_dir, &["rev-parse", "HEAD"])
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}
