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
use crate::self_update::{Outcome, SelfUpdate, Source};

/// lode's own release repository (`owner/name`).
pub const RELEASE_REPO: &str = "bkhq/lode";
/// The app name the update runs under, and the archive member that is the lode
/// binary (`lode-cli` is a symlink to it).
const APP: &str = "lode";

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

/// The release asset built for this host — `lode-<os>-<arch>.tar.gz` as
/// `.github/workflows/release.yml` names it.
fn release_asset() -> Result<String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    asset_for(os, arch)
        .ok_or_else(|| Error::Config(format!("no prebuilt lode release for {os}/{arch}")))
}

/// `lode-<os>-<arch>.tar.gz` for a Rust `target_os`/`target_arch` pair, using the
/// release workflow's names (`darwin`/`linux`, `x64`/`arm64`); `None` when no
/// release is built for it.
fn asset_for(os: &str, arch: &str) -> Option<String> {
    let os = match os {
        "linux" => "linux",
        "macos" => "darwin",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some(format!("lode-{os}-{arch}.tar.gz"))
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
        assert!(asset_for("windows", "x86_64").is_none());
        assert!(asset_for("linux", "riscv64").is_none());
    }
}
