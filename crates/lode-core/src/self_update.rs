//! Replace the running executable with a verified release of itself.
//!
//! This is the in-place update for a program that ships as one binary and links
//! lode as a library — and what `lode-cli self-update` runs for lode itself.
//! The update source and the trusted keys come from the caller, who compiles
//! them into the binary: [`SelfUpdate`] builds a self-contained [`Config`]
//! (`require_signature = enforce` against those keys, no `lode.toml`, no
//! `LODE_*` environment) and then reuses the ordinary chain — source adapter →
//! download → sha256 + signature gate → unpack — that installs any app version.
//! The verified binary is staged beside the executable, probed (`--version`
//! must report the target), and atomically renamed over it. The running process
//! keeps its old inode, so nothing changes until the program is next started;
//! restarting (or re-executing) is the caller's decision.
//!
//! ```no_run
//! use lode_core::self_update::{Outcome, SelfUpdate, Source};
//!
//! # fn main() -> lode_core::Result<()> {
//! // Resolve the executable once at startup: after an update the kernel
//! // reports the running (replaced) file as deleted.
//! let exe = lode_core::self_update::current_exe()?;
//! lode_core::InitOptions::new().crypto_provider(true).install();
//!
//! let update = SelfUpdate::new(
//!     "myapp",
//!     Source::GitHub("owner/myapp".to_owned()),
//!     env!("CARGO_PKG_VERSION"),
//! )
//! .asset("myapp-linux-x64.tar.gz")
//! .trusted_keys(["<key_id>:<base64-public-key>"])
//! .exe(exe);
//!
//! if let Outcome::Installed { version, .. } = update.apply()? {
//!     println!("installed {version}; restart to run it");
//! }
//! # Ok(())
//! # }
//! ```

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, Config, Overrides, Policy, RequireSignature};
use crate::error::{Error, Result};
use crate::{download, engine, install, manifest};

/// Where the releases of the program come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// GitHub releases of the `owner/name` repository (`[update].github`).
    GitHub(String),
    /// A native lode manifest URL (`[update].manifest`).
    Manifest(String),
}

/// What [`SelfUpdate::apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The running version is already the newest release; nothing was touched.
    UpToDate,
    /// `version` now sits at `exe`. The running process is still the old
    /// binary until it is restarted.
    Installed {
        /// The installed version id.
        version: String,
        /// The executable that was replaced.
        exe: PathBuf,
    },
}

/// An in-place update of the running executable; see the [module docs](self).
#[derive(Debug, Clone)]
pub struct SelfUpdate {
    app: String,
    source: Source,
    current_version: String,
    asset: Option<String>,
    binary: Option<String>,
    trusted_keys: Vec<String>,
    version: Option<String>,
    exe: Option<PathBuf>,
}

impl SelfUpdate {
    /// An update of `app` from `source`, for a binary that is currently
    /// `current_version` (the "already up to date" reference and the downgrade
    /// guard — normally `env!("CARGO_PKG_VERSION")`). Following the newest
    /// release needs semver on both sides; any other version id is only
    /// installed when [`version`](Self::version) names it.
    #[must_use]
    pub fn new(app: impl Into<String>, source: Source, current_version: impl Into<String>) -> Self {
        Self {
            app: app.into(),
            source,
            current_version: current_version.into(),
            asset: None,
            binary: None,
            trusted_keys: Vec::new(),
            version: None,
            exe: None,
        }
    }

    /// The release asset built for this host, e.g. `myapp-linux-x64.tar.gz`.
    /// Required; its extension selects the packaging format.
    #[must_use]
    pub fn asset(mut self, name: impl Into<String>) -> Self {
        self.asset = Some(name.into());
        self
    }

    /// The file inside the unpacked asset that is the executable. Defaults to
    /// the app name.
    #[must_use]
    pub fn binary(mut self, name: impl Into<String>) -> Self {
        self.binary = Some(name.into());
        self
    }

    /// The `<key_id>:<base64-public-key>` entries a release must be signed by.
    /// Required: an update is always verified under `enforce`. Compile them
    /// into the binary rather than reading them from configuration, so the
    /// update cannot be steered by whoever can edit that configuration.
    #[must_use]
    pub fn trusted_keys<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.trusted_keys = keys.into_iter().map(Into::into).collect();
        self
    }

    /// Install exactly this version (a GitHub tag or a manifest version id)
    /// instead of following the channel latest. A named version is installed as
    /// is — this is also the downgrade path.
    #[must_use]
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// The executable to replace. Defaults to [`current_exe`] at the time of the
    /// call; a long-running program should resolve it once at startup and pass
    /// it here, because the kernel reports a replaced running file as deleted.
    #[must_use]
    pub fn exe(mut self, path: impl Into<PathBuf>) -> Self {
        self.exe = Some(path.into());
        self
    }

    /// Look for an update without installing it: `Some(version)` when
    /// [`apply`](Self::apply) would install `version`, `None` when the running
    /// version is already the newest release.
    ///
    /// # Errors
    /// As [`apply`](Self::apply), short of the download and the replacement.
    pub fn check(&self) -> Result<Option<String>> {
        self.with_scratch(|cfg| Ok(resolve(cfg, &self.current_version)?.1))
    }

    /// Install the newest release — or the [`version`](Self::version) named —
    /// over the executable.
    ///
    /// # Errors
    /// Returns an error when the source cannot be reached, the release is
    /// unsigned or signed by another key, the newest release is older than the
    /// running version or cannot be ordered against it (ids that are not both
    /// semver), or the downloaded binary does not run and report the
    /// target version. The executable is untouched in every error case.
    pub fn apply(&self) -> Result<Outcome> {
        let exe = match &self.exe {
            Some(exe) => exe.clone(),
            None => current_exe()?,
        };
        let binary = self.binary.as_deref().unwrap_or(&self.app);
        self.with_scratch(|cfg| run_with(cfg, &exe, binary, &self.current_version))
    }

    /// Run `f` against the self-contained config, in a scratch directory that
    /// is removed afterwards.
    fn with_scratch<T>(&self, f: impl FnOnce(&Config) -> Result<T>) -> Result<T> {
        if self.trusted_keys.is_empty() {
            return Err(Error::Config(
                "self-update needs at least one trusted key to verify a release against".to_owned(),
            ));
        }
        let dir = scratch_dir()?;
        let result = self.config(&dir).and_then(|cfg| f(&cfg));
        let _ = fs::remove_dir_all(&dir);
        result
    }

    /// The self-contained config an update runs under: the source, the release
    /// asset, the named version as the pin, `enforce` against the trusted keys,
    /// and `dir` as the download scratch space. Built from an [`Overrides`]
    /// layer with no `lode.toml`, so nothing configured elsewhere leaks in.
    fn config(&self, dir: &Path) -> Result<Config> {
        let dir = dir
            .to_str()
            .ok_or_else(|| Error::Config(format!("scratch dir {} is not UTF-8", dir.display())))?;
        let (github, manifest) = match &self.source {
            Source::GitHub(repo) => (Some(repo.clone()), None),
            Source::Manifest(url) => (None, Some(url.clone())),
        };
        let overrides = Overrides {
            app: Some(self.app.clone()),
            dir: Some(dir.to_owned()),
            github,
            manifest,
            asset: self.asset.clone(),
            pin: self.version.clone(),
            policy: Some(Policy::Off),
            require_signature: Some(RequireSignature::Enforce),
            trusted_keys: Some(self.trusted_keys.join(",")),
            ..Overrides::default()
        };
        config::resolve_with(&overrides, None)
    }
}

/// Fetch and verify the catalog, then decide what to install: the version id,
/// or `None` when `current_version` is already the channel latest.
fn resolve(cfg: &Config, current_version: &str) -> Result<(manifest::Manifest, Option<String>)> {
    let manifest = manifest::fetch(cfg)?;
    install::verify_manifest_identity(cfg, &manifest)?;
    // No floor: the plan below decides against the running binary's version, with
    // a self-update-specific message; a named version is the caller's deliberate
    // choice (also the downgrade path) and is installed as is.
    let target = manifest::resolve_target(
        &manifest,
        &cfg.update.channel,
        cfg.update.pin.as_deref(),
        None,
        None,
    )?;
    if cfg.update.pin.is_none() {
        match plan(&target, current_version) {
            Plan::UpToDate => return Ok((manifest, None)),
            Plan::Older => {
                return Err(Error::Manifest(format!(
                    "the newest release ({target}) is older than this binary \
                     ({current_version}); request that version explicitly to install it \
                     deliberately"
                )));
            }
            Plan::Unordered => {
                return Err(Error::Manifest(format!(
                    "the newest release ({target}) cannot be ordered against this binary \
                     ({current_version}): both must be semver for an update to be followed; \
                     request that version explicitly to install it deliberately"
                )));
            }
            Plan::Install => {}
        }
    }
    Ok((manifest, Some(target)))
}

/// The update proper, against `cfg` and the executable at `exe` (split from
/// [`SelfUpdate::apply`] so tests can drive it with a stub source and a scratch
/// executable). `binary` is the executable's name inside the unpacked asset.
fn run_with(cfg: &Config, exe: &Path, binary: &str, current_version: &str) -> Result<Outcome> {
    let (manifest, Some(target)) = resolve(cfg, current_version)? else {
        return Ok(Outcome::UpToDate);
    };

    let entry = manifest::version_entry(&manifest, &target)?;
    let asset = manifest::select_asset(entry, engine::required_asset(cfg)?)?;
    let (artifact, sha256) =
        download::fetch_artifact(cfg, asset, &target, &manifest::allowed_hosts(cfg))?;
    install::verify_download(cfg, &target, asset, &sha256)?;

    let unpack = cfg.global.dir.join("unpack");
    fs::create_dir_all(&unpack)?;
    install::extract(
        asset,
        manifest::format_from_name(&asset.name),
        &artifact,
        &unpack,
    )?;
    let unpacked = unpack.join(binary);
    if !unpacked.is_file() {
        return Err(Error::Install(format!(
            "{} does not contain a `{binary}` binary",
            asset.name
        )));
    }
    replace_executable(exe, &unpacked, &target)?;

    Ok(Outcome::Installed {
        version: target,
        exe: exe.to_path_buf(),
    })
}

/// What following the channel `latest` should do relative to the running binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// `latest` is the running version.
    UpToDate,
    /// `latest` is older than the running version — refused unless a tag is named.
    Older,
    /// `latest` differs and the two are not both semver: it could be a
    /// downgrade — refused unless a tag is named.
    Unordered,
    /// Newer — install it.
    Install,
}

/// Compare `target` (the channel latest) with `current` by semver precedence;
/// ids that don't parse fall back to plain equality.
fn plan(target: &str, current: &str) -> Plan {
    match (
        semver::Version::parse(target),
        semver::Version::parse(current),
    ) {
        (Ok(t), Ok(c)) => match t.cmp(&c) {
            Ordering::Equal => Plan::UpToDate,
            Ordering::Less => Plan::Older,
            Ordering::Greater => Plan::Install,
        },
        _ if target == current => Plan::UpToDate,
        _ => Plan::Unordered,
    }
}

/// Swap `binary` (the verified, unpacked release) in over `exe`: copy it beside
/// `exe` (same filesystem, so the final step is an atomic `rename`), carry over
/// `exe`'s permission bits, prove it runs and reports `version`, then rename it
/// over `exe`. On any failure the staged copy is removed and `exe` is untouched.
/// A running `exe` keeps its old inode (Unix never lets a mapped executable be
/// written — `ETXTBSY`), which is why this is a rename, not a write.
fn replace_executable(exe: &Path, binary: &Path, version: &str) -> Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| Error::Install(format!("{} has no parent directory", exe.display())))?;
    let name = exe
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| Error::Install(format!("{} has no usable filename", exe.display())))?;
    let staged = dir.join(format!(".{name}.self-update-{}", std::process::id()));
    fs::copy(binary, &staged)
        .map_err(|e| Error::Install(format!("stage {}: {e}", staged.display())))?;
    let result = install_staged(exe, &staged, version);
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

/// The steps after staging (see [`replace_executable`]), separated so a failure
/// in any of them cleans up the staged copy in one place.
fn install_staged(exe: &Path, staged: &Path, version: &str) -> Result<()> {
    let perms = fs::metadata(exe)?.permissions();
    fs::set_permissions(staged, perms)?;
    probe_version(staged, version)?;
    fs::rename(staged, exe).map_err(|e| Error::Install(format!("replace {}: {e}", exe.display())))
}

/// Run `staged --version` and require it to report `version`: catches a download
/// built for another platform (fails to exec) or a mislabelled release before it
/// replaces a working binary.
fn probe_version(staged: &Path, version: &str) -> Result<()> {
    let output = spawn_probe(staged).map_err(|e| {
        Error::Install(format!(
            "run {} --version: {e} (is the release built for this platform?)",
            staged.display()
        ))
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && stdout.split_whitespace().any(|word| word == version) {
        return Ok(());
    }
    Err(Error::Install(format!(
        "the downloaded binary reports {:?}, expected version {version}",
        stdout.trim()
    )))
}

/// `staged --version`, retried while the file is busy: a process another
/// thread is spawning holds the just-written file open for writing between its
/// `fork` and `exec`, and the kernel refuses to execute it until then.
fn spawn_probe(staged: &Path) -> std::io::Result<std::process::Output> {
    const ATTEMPTS: u32 = 100;
    const PAUSE: std::time::Duration = std::time::Duration::from_millis(20);

    let mut attempt = 1;
    loop {
        match Command::new(staged).arg("--version").output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(PAUSE);
            }
            result => return result,
        }
    }
}

/// The running executable, symlinks resolved — so a run via a symlink replaces
/// the file it points at, never the link.
///
/// # Errors
/// Returns [`Error::Install`] when the executable cannot be located or resolved
/// — which is the case once the running file has been replaced, so call this
/// before the first update.
pub fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::Install(format!("locate the running executable: {e}")))?;
    exe.canonicalize()
        .map_err(|e| Error::Install(format!("resolve {}: {e}", exe.display())))
}

/// A private scratch directory under the system temp dir for the download
/// cache and the unpacked archive; [`SelfUpdate::with_scratch`] removes it when done.
fn scratch_dir() -> Result<PathBuf> {
    scratch_dir_in(&std::env::temp_dir())
}

/// Create a fresh owner-only directory under `base`. The name is unpredictable
/// and creation fails when the path exists, so another local user cannot plant
/// the directory and swap the verified binary before it is installed.
fn scratch_dir_in(base: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;

    let nonce = getrandom::u64()
        .map_err(|e| Error::Install(format!("self-update: no randomness for scratch dir: {e}")))?;
    let dir = base.join(format!("lode-self-update-{nonce:016x}"));
    fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use ed25519_dalek::{Signer as _, SigningKey};
    use serde_json::json;

    use super::*;

    #[test]
    fn scratch_dir_is_fresh_and_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = scratch("scratch-dir");
        let first = scratch_dir_in(&base).unwrap();
        let second = scratch_dir_in(&base).unwrap();
        assert_ne!(first, second, "each call gets its own directory");
        for dir in [&first, &second] {
            let mode = fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let _ = fs::remove_dir_all(&base);
    }
    use crate::stub::Stub;
    use crate::verify::{Artifact, artifact_message, key_id, sha256_hex};

    const B64: base64::engine::general_purpose::GeneralPurpose =
        base64::engine::general_purpose::STANDARD;
    const ASSET: &str = "lode-linux-x64.tar.gz";
    const REPO: &str = "bkhq/lode";
    const BINARY: &str = "lode";
    const LATEST: &str = "/repos/bkhq/lode/releases/latest";

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lode-self-update-test-{}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn plan_compares_by_semver_then_equality() {
        assert_eq!(plan("0.3.0", "0.2.0"), Plan::Install);
        assert_eq!(plan("0.2.0", "0.2.0"), Plan::UpToDate);
        assert_eq!(plan("0.1.9", "0.2.0"), Plan::Older);
        assert_eq!(plan("0.3.0-rc.1", "0.2.0"), Plan::Install);
        // Not semver: only literal equality counts as up to date, and a
        // different id cannot be ordered — it could be a downgrade.
        assert_eq!(plan("nightly", "nightly"), Plan::UpToDate);
        assert_eq!(plan("nightly", "0.2.0"), Plan::Unordered);
        assert_eq!(plan("0.3.0", "nightly"), Plan::Unordered);
        assert_eq!(plan("nightly-1", "nightly-2"), Plan::Unordered);
    }

    /// The updater the tests drive: lode's own shape, from GitHub `REPO`.
    fn updater(tag: Option<&str>, keys: &[String]) -> SelfUpdate {
        let update = SelfUpdate::new(BINARY, Source::GitHub(REPO.to_owned()), "0.1.0")
            .asset(ASSET)
            .trusted_keys(keys.iter().cloned());
        match tag {
            Some(tag) => update.version(tag),
            None => update,
        }
    }

    #[test]
    fn config_is_self_contained_and_enforcing() {
        let dir = scratch("config");
        let keys = vec!["k1:AAAA".to_owned(), "k2:BBBB".to_owned()];
        let cfg = updater(Some("v1.2.3"), &keys).config(&dir).unwrap();
        assert_eq!(cfg.global.app, BINARY);
        assert_eq!(cfg.global.dir, dir);
        assert_eq!(cfg.update.github.as_deref(), Some(REPO));
        assert!(cfg.update.manifest.is_none());
        assert_eq!(cfg.update.asset.as_deref(), Some(ASSET));
        assert_eq!(cfg.update.pin.as_deref(), Some("v1.2.3"));
        assert_eq!(cfg.trust.require_signature, RequireSignature::Enforce);
        assert_eq!(cfg.trust.trusted_keys, keys);
        assert!(cfg.trust.trusted_keys_file.is_none());
        assert!(cfg.http.headers.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manifest_source_configures_the_native_adapter() {
        let dir = scratch("config-manifest");
        let url = "https://releases.example.com/myapp/manifest.json";
        let cfg = SelfUpdate::new("myapp", Source::Manifest(url.to_owned()), "1.0.0")
            .asset("myapp-linux-x64.tar.gz")
            .trusted_keys(["k1:AAAA"])
            .config(&dir)
            .unwrap();
        assert_eq!(cfg.global.app, "myapp");
        assert_eq!(cfg.update.manifest.as_deref(), Some(url));
        assert!(cfg.update.github.is_none());
        assert_eq!(cfg.trust.require_signature, RequireSignature::Enforce);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_to_run_without_a_trusted_key() {
        let update = SelfUpdate::new("myapp", Source::GitHub("o/r".to_owned()), "1.0.0")
            .asset("myapp-linux-x64.tar.gz");
        let err = update.check().unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");
        let err = update.exe("/nonexistent/myapp").apply().unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");
    }

    // --- the executable swap + the end-to-end flow need `/bin/sh` -------------

    /// A stand-in `lode`: a shell script whose `--version` prints `lode <version>`.
    #[cfg(unix)]
    fn write_fake_lode(path: &Path, version: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::write(path, fake_lode(version)).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn fake_lode(version: &str) -> String {
        format!("#!/bin/sh\necho \"lode {version}\"\n")
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn reported_version(exe: &Path) -> String {
        let out = Command::new(exe).arg("--version").output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// No `.<name>.self-update-*` staging file was left behind in `dir`.
    fn no_staging_leftovers(dir: &Path) -> bool {
        !fs::read_dir(dir).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".self-update-")
        })
    }

    #[cfg(unix)]
    #[test]
    fn replace_executable_swaps_in_the_probed_binary_and_keeps_its_mode() {
        let dir = scratch("swap");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "1.0.0", 0o750);
        let new = dir.join("unpacked-lode");
        write_fake_lode(&new, "9.9.9", 0o755);

        replace_executable(&exe, &new, "9.9.9").unwrap();

        assert_eq!(reported_version(&exe), "lode 9.9.9");
        assert_eq!(mode_of(&exe), 0o750, "the installed mode is carried over");
        assert!(no_staging_leftovers(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn replace_executable_keeps_the_old_binary_when_the_probe_fails() {
        let dir = scratch("probe-fail");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "1.0.0", 0o755);
        // Reports the wrong version (a mislabelled release)...
        let wrong = dir.join("wrong-lode");
        write_fake_lode(&wrong, "1.2.3", 0o755);
        let err = replace_executable(&exe, &wrong, "9.9.9").unwrap_err();
        assert!(err.to_string().contains("expected version 9.9.9"), "{err}");
        // ...and one that cannot run at all (a foreign-platform binary).
        let broken = dir.join("broken-lode");
        fs::write(&broken, b"\x7fELF not really").unwrap();
        let err = replace_executable(&exe, &broken, "9.9.9").unwrap_err();
        assert!(err.to_string().contains("--version"), "{err}");

        assert_eq!(reported_version(&exe), "lode 1.0.0");
        assert!(no_staging_leftovers(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A release tarball holding `lode` (a fake reporting `version`) plus the
    /// `lode-cli -> lode` symlink the real packaging ships.
    fn release_tarball(version: &str) -> Vec<u8> {
        use std::io::Write as _;
        let script = fake_lode(version);
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, BINARY, script.as_bytes())
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        link.set_cksum();
        builder.append_link(&mut link, "lode-cli", BINARY).unwrap();
        let tar_bytes = builder.into_inner().unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap()
    }

    /// Sign `bytes` as `ASSET` of `version` with a fresh ed25519 key: returns the
    /// sha256, the base64 signature and the matching trusted-key entry.
    fn sign(bytes: &[u8], version: &str) -> (String, String, String) {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).unwrap();
        let key = SigningKey::from_bytes(&seed);
        let sha = sha256_hex(bytes);
        let artifact = Artifact {
            app: BINARY,
            name: ASSET,
            version,
            path: "",
            run: None,
            exec: None,
        };
        let sig = B64.encode(key.sign(&artifact_message(&artifact, &sha)).to_bytes());
        let public = key.verifying_key().to_bytes();
        let entry = format!("{}:{}", key_id(&public), B64.encode(public));
        (sha, sig, entry)
    }

    /// A stub GitHub whose `latest` and `tags/<tag>` both answer one release
    /// carrying `ASSET` (digest `sha`) and — when given — its `.sig` sidecar asset.
    fn github_stub(tag: &str, tarball: Vec<u8>, sha: &str, sig: Option<&str>) -> Stub {
        let tag = tag.to_owned();
        let sha = sha.to_owned();
        let sig = sig.map(ToOwned::to_owned);
        Stub::start(move |base| {
            let mut assets = vec![json!({
                "name": ASSET,
                "browser_download_url": format!("{base}/dl/{ASSET}"),
                "digest": format!("sha256:{sha}"),
            })];
            let mut routes = vec![(format!("/dl/{ASSET}"), tarball)];
            if let Some(sig) = sig {
                assets.push(json!({
                    "name": format!("{ASSET}.sig"),
                    "browser_download_url": format!("{base}/dl/{ASSET}.sig"),
                }));
                routes.push((format!("/dl/{ASSET}.sig"), format!("{sig}\n").into_bytes()));
            }
            let release = json!({ "tag_name": tag, "assets": assets })
                .to_string()
                .into_bytes();
            routes.push((LATEST.to_owned(), release.clone()));
            routes.push((format!("/repos/bkhq/lode/releases/tags/{tag}"), release));
            routes
        })
    }

    /// The self-update config pointed at `stub` (loopback http is always allowed).
    fn stub_config(stub: &Stub, tag: Option<&str>, keys: &[String], dir: &Path) -> Config {
        let mut cfg = updater(tag, keys).config(dir).unwrap();
        cfg.update.github_api = stub.base.clone();
        cfg
    }

    #[cfg(unix)]
    #[test]
    fn installs_a_signed_release_over_the_running_binary() {
        let dir = scratch("install");
        let exe = dir.join("bin").join("lode");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        write_fake_lode(&exe, "0.1.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, entry) = sign(&tarball, "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));
        let cfg = stub_config(&stub, None, &[entry], &dir.join("scratch"));

        run_with(&cfg, &exe, BINARY, "0.1.0").unwrap();

        assert_eq!(reported_version(&exe), "lode 9.9.9");
        assert!(no_staging_leftovers(exe.parent().unwrap()));
        // API → sidecar (during the manifest fetch) → the artifact itself.
        assert_eq!(
            stub.served(),
            vec![
                LATEST.to_owned(),
                format!("/dl/{ASSET}.sig"),
                format!("/dl/{ASSET}")
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_an_unsigned_release() {
        let dir = scratch("unsigned");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "0.1.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, _sig, entry) = sign(&tarball, "9.9.9");
        // No `.sig` sidecar asset and no label: unsigned under `enforce`.
        let stub = github_stub("v9.9.9", tarball, &sha, None);
        let cfg = stub_config(&stub, None, &[entry], &dir.join("scratch"));

        let err = run_with(&cfg, &exe, BINARY, "0.1.0").unwrap_err();
        assert!(matches!(err, Error::Verify(_)), "{err}");
        assert_eq!(reported_version(&exe), "lode 0.1.0");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_release_signed_by_another_key() {
        let dir = scratch("wrong-key");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "0.1.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, _entry) = sign(&tarball, "9.9.9");
        let (_, _, other) = sign(b"another key", "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));
        let cfg = stub_config(&stub, None, &[other], &dir.join("scratch"));

        let err = run_with(&cfg, &exe, BINARY, "0.1.0").unwrap_err();
        assert!(matches!(err, Error::Verify(_)), "{err}");
        assert_eq!(reported_version(&exe), "lode 0.1.0");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn is_a_no_op_when_already_on_the_newest_release() {
        let dir = scratch("up-to-date");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "9.9.9", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, entry) = sign(&tarball, "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));
        let cfg = stub_config(&stub, None, &[entry], &dir.join("scratch"));

        run_with(&cfg, &exe, BINARY, "9.9.9").unwrap();

        assert_eq!(reported_version(&exe), "lode 9.9.9");
        assert!(!stub.served().iter().any(|p| p == &format!("/dl/{ASSET}")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_an_older_latest_unless_the_tag_is_named() {
        let dir = scratch("older");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "10.0.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, entry) = sign(&tarball, "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));

        // Following `latest` onto an older release is refused...
        let cfg = stub_config(
            &stub,
            None,
            std::slice::from_ref(&entry),
            &dir.join("scratch"),
        );
        let err = run_with(&cfg, &exe, BINARY, "10.0.0").unwrap_err();
        assert!(err.to_string().contains("older than this binary"), "{err}");
        assert_eq!(reported_version(&exe), "lode 10.0.0");

        // ...while naming the raw tag installs it: the pin resolves to the same
        // `9.9.9` id the signature was made over, not the raw `v9.9.9`.
        let cfg = stub_config(&stub, Some("v9.9.9"), &[entry], &dir.join("scratch"));
        run_with(&cfg, &exe, BINARY, "10.0.0").unwrap();
        assert_eq!(reported_version(&exe), "lode 9.9.9");
        assert!(
            stub.served()
                .iter()
                .any(|p| p == "/repos/bkhq/lode/releases/tags/v9.9.9")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_an_unorderable_latest_unless_the_tag_is_named() {
        let dir = scratch("unordered");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "nightly-2", 0o755);
        let tarball = release_tarball("nightly-1");
        let (sha, sig, entry) = sign(&tarball, "nightly-1");
        let stub = github_stub("nightly-1", tarball, &sha, Some(&sig));

        let cfg = stub_config(
            &stub,
            None,
            std::slice::from_ref(&entry),
            &dir.join("scratch"),
        );
        let err = run_with(&cfg, &exe, BINARY, "nightly-2").unwrap_err();
        assert!(err.to_string().contains("cannot be ordered"), "{err}");
        assert_eq!(reported_version(&exe), "lode nightly-2");

        let cfg = stub_config(&stub, Some("nightly-1"), &[entry], &dir.join("scratch"));
        run_with(&cfg, &exe, BINARY, "nightly-2").unwrap();
        assert_eq!(reported_version(&exe), "lode nightly-1");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Another thread spawning a process holds the staged file's write handle
    /// for a moment in its forked child; the probe has to outlast that.
    #[cfg(unix)]
    #[test]
    fn probe_waits_out_a_staged_file_that_is_still_open_for_writing() {
        let dir = scratch("probe-busy");
        let staged = dir.join("staged");
        write_fake_lode(&staged, "9.9.9", 0o755);
        let writer = fs::OpenOptions::new().append(true).open(&staged).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(writer);
        });
        probe_version(&staged, "9.9.9").unwrap();
        release.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn installs_the_named_binary_and_reports_the_outcome() {
        let dir = scratch("named-binary");
        let exe = dir.join("myapp");
        write_fake_lode(&exe, "0.1.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, entry) = sign(&tarball, "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));
        let cfg = stub_config(&stub, None, &[entry], &dir.join("scratch"));

        // The archive holds `lode`; asking for a member it lacks changes nothing.
        let err = run_with(&cfg, &exe, "myapp", "0.1.0").unwrap_err();
        assert!(err.to_string().contains("`myapp` binary"), "{err}");
        assert_eq!(reported_version(&exe), "lode 0.1.0");

        // The member name is independent of the installed executable's name.
        let outcome = run_with(&cfg, &exe, BINARY, "0.1.0").unwrap();
        assert_eq!(
            outcome,
            Outcome::Installed {
                version: "9.9.9".to_owned(),
                exe: exe.clone(),
            }
        );
        assert_eq!(reported_version(&exe), "lode 9.9.9");
        assert_eq!(
            run_with(&cfg, &exe, BINARY, "9.9.9").unwrap(),
            Outcome::UpToDate
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_reports_the_version_an_update_would_install() {
        let dir = scratch("resolve");
        let tarball = release_tarball("9.9.9");
        let (sha, sig, entry) = sign(&tarball, "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));
        let cfg = stub_config(&stub, None, &[entry], &dir.join("scratch"));

        assert_eq!(resolve(&cfg, "0.1.0").unwrap().1.as_deref(), Some("9.9.9"));
        assert_eq!(resolve(&cfg, "9.9.9").unwrap().1, None);
        assert!(resolve(&cfg, "10.0.0").is_err());
        // Looking never downloads the artifact.
        assert!(!stub.served().iter().any(|p| p == &format!("/dl/{ASSET}")));
        let _ = fs::remove_dir_all(&dir);
    }
}
