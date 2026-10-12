//! `lode-cli self-update` — replace THIS lode binary with a release of lode
//! itself.
//!
//! lode's own update source is fixed — its GitHub releases — and is never the
//! app's `lode.toml` / `LODE_*` source: the command drives [`SelfUpdate`] with
//! `bkhq/lode`, the release asset for this platform and the release keys
//! compiled into the binary. A running supervisor is never disturbed: it keeps
//! the old binary until lode is next started.

use std::io::Write as _;

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

use crate::self_update::{self, Outcome, SelfUpdate, Source};

/// lode's own release repository (`owner/name`).
pub const RELEASE_REPO: &str = "bkhq/lode";
/// The app name the update runs under, and the archive member that is the lode
/// binary (`lode-cli` is a symlink to it, or a second copy on Windows).
const APP: &str = "lode";
/// The names lode is installed under.
const NAMES: [&str; 2] = ["lode", "lode-cli"];

/// Run `self-update`: install the newest stable lode release — or the release
/// `tag` — over the running executable.
///
/// `trusted_keys` are the release-key entries the download must be signed by;
/// `current_version` is this binary's version (the "already up to date"
/// reference and the downgrade guard).
pub fn run(tag: Option<&str>, trusted_keys: &[String], current_version: &str) -> Result<()> {
    if trusted_keys.is_empty() {
        return Err(Error::Config(
            "no lode release key is compiled into this binary; pass --release-key <entry>"
                .to_owned(),
        ));
    }
    let mut update = SelfUpdate::new(
        APP,
        Source::GitHub(RELEASE_REPO.to_owned()),
        current_version,
    )
    .asset(release_asset()?)
    .trusted_keys(trusted_keys.iter().cloned());
    if cfg!(windows) {
        let exe = self_update::current_exe()?;
        for copy in other_copies(&exe) {
            update = update.alias(copy);
        }
        update = update.exe(exe);
    }
    if let Some(tag) = tag {
        update = update.version(tag);
    }

    let mut out = std::io::stdout().lock();
    match update.apply()? {
        Outcome::UpToDate => writeln!(
            out,
            "lode self-update: {current_version} is already the newest release"
        )?,
        Outcome::Installed { version, exe } => {
            writeln!(
                out,
                "lode self-update: installed {version} at {} (was {current_version})",
                exe.display()
            )?;
            writeln!(
                out,
                "  restart lode to run it — a running supervisor keeps the old binary until then"
            )?;
        }
    }
    Ok(())
}

/// The other names lode sits under beside `exe`. Windows has no symlink to
/// ship, so `lode.exe` and `lode-cli.exe` are two copies of one program and an
/// update has to replace both.
fn other_copies(exe: &Path) -> Vec<PathBuf> {
    NAMES
        .iter()
        .map(|name| exe.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .filter(|path| path != exe)
        .collect()
}

/// The release asset built for this host — `lode-<os>-<arch>.tar.gz`, or
/// `.zip` on Windows, as `.github/workflows/release.yml` names it.
fn release_asset() -> Result<String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    asset_for(os, arch)
        .ok_or_else(|| Error::Config(format!("no prebuilt lode release for {os}/{arch}")))
}

/// The release asset for a Rust `target_os`/`target_arch` pair, using the
/// release workflow's names (`darwin`/`linux`/`windows`, `x64`/`arm64`); `None`
/// when no release is built for it.
fn asset_for(os: &str, arch: &str) -> Option<String> {
    let (os, ext) = match os {
        "linux" => ("linux", "tar.gz"),
        "macos" => ("darwin", "tar.gz"),
        "windows" => ("windows", "zip"),
        _ => return None,
    };
    let arch = match (os, arch) {
        (_, "x86_64") => "x64",
        ("linux" | "darwin", "aarch64") => "arm64",
        _ => return None,
    };
    Some(format!("lode-{os}-{arch}.{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_for_maps_the_released_platforms_only() {
        assert_eq!(
            asset_for("linux", "x86_64").as_deref(),
            Some("lode-linux-x64.tar.gz")
        );
        assert_eq!(
            asset_for("linux", "aarch64").as_deref(),
            Some("lode-linux-arm64.tar.gz")
        );
        assert_eq!(
            asset_for("macos", "x86_64").as_deref(),
            Some("lode-darwin-x64.tar.gz")
        );
        assert_eq!(
            asset_for("macos", "aarch64").as_deref(),
            Some("lode-darwin-arm64.tar.gz")
        );
        assert_eq!(
            asset_for("windows", "x86_64").as_deref(),
            Some("lode-windows-x64.zip")
        );
        assert!(asset_for("windows", "aarch64").is_none());
        assert!(asset_for("linux", "riscv64").is_none());
    }

    #[test]
    fn other_copies_are_the_remaining_names_beside_the_executable() {
        let suffix = std::env::consts::EXE_SUFFIX;
        let dir = Path::new("bin");
        let lode = dir.join(format!("lode{suffix}"));
        let cli = dir.join(format!("lode-cli{suffix}"));
        assert_eq!(other_copies(&cli), vec![lode.clone()]);
        assert_eq!(other_copies(&lode), vec![cli]);
    }
}
