//! Replace the running executable with a verified release of itself.
//!
//! This is the in-place update for a program that ships as one binary and links
//! lode as a library — and what `lode-cli self-update` runs for lode itself.
//! The update source and the trusted keys come from the caller, who compiles
//! them into the binary: [`SelfUpdate`] builds a self-contained [`Config`]
//! (`require_signature = enforce` against those keys, no `lode.toml`, no
//! `LODE_*` environment; a private source gets its credentials from
//! [`SelfUpdate::header`]) and then reuses the ordinary chain — source adapter →
//! download → sha256 + signature gate → unpack — that installs any app version.
//! The verified binary is staged beside the executable, probed (`--version`
//! must report the target), and moved into its place. Nothing changes for the
//! running process until the program is next started; restarting (or
//! re-executing) is the caller's decision.
//!
//! On Unix the move is one atomic `rename` and the running process keeps its
//! old inode. Windows does not let a running executable be overwritten or
//! deleted, only renamed: the old file is moved aside under a hidden name and
//! the new one takes its place. Call [`remove_replaced`] when the program
//! starts to delete what an earlier update left there.
//!
//! ```no_run
//! use lode_core::self_update::{Outcome, SelfUpdate, Source};
//!
//! # fn main() -> Result<(), lode_core::self_update::SelfUpdateError> {
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

use crate::config::{self, Config, Overrides, Policy, RequireManifest, RequireSignature};
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

/// A release an update would install, as [`SelfUpdate::check`] reports it.
#[derive(Debug, Clone)]
pub struct Available {
    /// The version id.
    pub version: String,
    /// Its manifest entry: `notes`, `published_at` and the assets. Notes and
    /// the publication time are not covered by any signature — show them as
    /// untrusted text.
    pub entry: manifest::VersionEntry,
}

/// A verified release, downloaded and unpacked, that [`SelfUpdate::install`]
/// puts in place. Whatever else the asset holds is under [`dir`](Self::dir)
/// until this value is dropped, which removes the scratch directory.
#[derive(Debug)]
pub struct Fetched {
    version: String,
    entry: manifest::VersionEntry,
    dir: PathBuf,
    binary: PathBuf,
    scratch: PathBuf,
}

impl Fetched {
    /// The version id.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The version's manifest entry; see [`Available::entry`].
    #[must_use]
    pub const fn entry(&self) -> &manifest::VersionEntry {
        &self.entry
    }

    /// The unpacked asset: every file of the archive whose digest and
    /// signature were verified, the executable included.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The new executable inside [`dir`](Self::dir), for a program that puts
    /// it in place itself instead of calling [`SelfUpdate::install`]. Copy it
    /// out before this value is dropped. It has not been run: `install` is
    /// what checks that it starts and reports [`version`](Self::version).
    #[must_use]
    pub fn binary(&self) -> &Path {
        &self.binary
    }
}

impl Drop for Fetched {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.scratch);
    }
}

/// Why an update did not go through. [`is_untouched`](Self::is_untouched) tells
/// a failure that left the installation as it was from one that did not.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SelfUpdateError {
    /// The update is not set up to run: no trusted key, no asset, a bad source.
    #[error(transparent)]
    Config(Error),
    /// The source could not be reached, or its answer could not be read.
    #[error(transparent)]
    Source(Error),
    /// The release was refused: a digest or signature that does not verify, or
    /// a manifest that is unsigned or expired where
    /// [`require_manifest`](SelfUpdate::require_manifest) asks for one.
    #[error(transparent)]
    Verify(Error),
    /// A local failure before anything was replaced: scratch space, unpacking.
    #[error(transparent)]
    Local(Error),
    /// The newest release is older than the running version.
    #[error(
        "the newest release ({latest}) is older than this binary ({current}); request that \
         version explicitly to install it deliberately"
    )]
    Older {
        /// The channel's latest version.
        latest: String,
        /// The running version.
        current: String,
    },
    /// The newest release and the running version are not both semver.
    #[error(
        "the newest release ({latest}) cannot be ordered against this binary ({current}): both \
         must be semver for an update to be followed; request that version explicitly to \
         install it deliberately"
    )]
    Unordered {
        /// The channel's latest version.
        latest: String,
        /// The running version.
        current: String,
    },
    /// The asset does not hold the executable.
    #[error("{asset} does not contain a `{binary}` binary")]
    MissingBinary {
        /// The asset filename.
        asset: String,
        /// The file that was looked for in it.
        binary: String,
    },
    /// The downloaded binary could not be run.
    #[error("run {} --version: {source} (is the release built for this platform?)", path.display())]
    NotRunnable {
        /// The staged copy that was run.
        path: PathBuf,
        /// What the operating system said.
        source: std::io::Error,
    },
    /// The downloaded binary reports another version than the release's.
    #[error("the downloaded binary reports {reported:?}, expected version {expected}")]
    WrongVersion {
        /// The release's version.
        expected: String,
        /// What `--version` printed.
        reported: String,
    },
    /// The executable could not be replaced; it is as it was.
    #[error("replace {}: {source}", path.display())]
    Replace {
        /// The executable.
        path: PathBuf,
        /// What the operating system said.
        source: std::io::Error,
    },
    /// The executable was replaced, an [`alias`](SelfUpdate::alias) of it was
    /// not: the two are now different versions.
    #[error("the executable was replaced, its copy {} was not: {source}", path.display())]
    Alias {
        /// The copy that still holds the old version.
        path: PathBuf,
        /// What the operating system said.
        source: std::io::Error,
    },
    /// Windows only: the executable was moved aside, the new one could not
    /// take its place and the old one could not be moved back. Nothing sits at
    /// `path`; the old executable is at `moved_to`.
    #[error(
        "{} is missing: the new executable could not be put in place ({source}) and the old one is left at {}",
        path.display(),
        moved_to.display()
    )]
    Displaced {
        /// Where the executable belongs.
        path: PathBuf,
        /// Where the old executable is now.
        moved_to: PathBuf,
        /// What the operating system said.
        source: std::io::Error,
    },
}

impl SelfUpdateError {
    /// Whether the failure left the installation exactly as it was. `false`
    /// for [`Alias`](Self::Alias) and [`Displaced`](Self::Displaced), whose
    /// fields say what has to be put right.
    #[must_use]
    pub const fn is_untouched(&self) -> bool {
        !matches!(self, Self::Alias { .. } | Self::Displaced { .. })
    }
}

/// File a lode error under the stage of an update it belongs to.
impl From<Error> for SelfUpdateError {
    fn from(error: Error) -> Self {
        match error {
            Error::Config(_) | Error::Toml(_) | Error::ParseInt(_) => Self::Config(error),
            Error::Http(_) | Error::Manifest(_) | Error::Download(_) | Error::Json(_) => {
                Self::Source(error)
            }
            Error::Verify(_) => Self::Verify(error),
            _ => Self::Local(error),
        }
    }
}

impl From<SelfUpdateError> for Error {
    fn from(error: SelfUpdateError) -> Self {
        match error {
            SelfUpdateError::Config(inner)
            | SelfUpdateError::Source(inner)
            | SelfUpdateError::Verify(inner)
            | SelfUpdateError::Local(inner) => inner,
            SelfUpdateError::Older { .. } | SelfUpdateError::Unordered { .. } => {
                Self::Manifest(error.to_string())
            }
            other => Self::Install(other.to_string()),
        }
    }
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
    channel: Option<String>,
    headers: Vec<String>,
    require_manifest: Option<RequireManifest>,
    exe: Option<PathBuf>,
    aliases: Vec<PathBuf>,
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
            channel: None,
            headers: Vec::new(),
            require_manifest: None,
            exe: None,
            aliases: Vec::new(),
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
    /// the app name, with `.exe` on Windows.
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

    /// The channel whose latest release is followed. Defaults to `stable`; with
    /// a GitHub source any other channel follows the newest pre-release.
    #[must_use]
    pub fn channel(mut self, name: impl Into<String>) -> Self {
        self.channel = Some(name.into());
        self
    }

    /// A request header, as a `Name: value` line, for a source that needs
    /// credentials. `${VAR}` in the value is replaced from the environment when
    /// the request is made, so a token need not be compiled in. The header is
    /// sent to the source only — the manifest's host, or GitHub's API and
    /// asset hosts — never to another host an asset url may name.
    #[must_use]
    pub fn header(mut self, line: impl Into<String>) -> Self {
        self.headers.push(line.into());
        self
    }

    /// What the manifest itself has to prove. By default its signature and
    /// expiry are verified when present and never required.
    /// [`RequireManifest::Signed`] refuses a manifest without a signature
    /// ([`RequireManifest::Fresh`] is a deprecated alias). It needs a
    /// [`Source::Manifest`] whose publisher signs it; with a GitHub source,
    /// which has no manifest to sign, the update is a configuration error.
    #[must_use]
    pub const fn require_manifest(mut self, policy: RequireManifest) -> Self {
        self.require_manifest = Some(policy);
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

    /// Another path that holds a copy of the same executable and is replaced
    /// along with it — a second name the program ships under where there is no
    /// symlink to do it (lode's own `lode-cli.exe` on Windows). A path that does
    /// not exist is left out.
    #[must_use]
    pub fn alias(mut self, path: impl Into<PathBuf>) -> Self {
        self.aliases.push(path.into());
        self
    }

    /// Look for an update without installing it: the release
    /// [`apply`](Self::apply) would install, `None` when the running version
    /// is already the newest.
    ///
    /// # Errors
    /// As [`fetch`](Self::fetch), short of the download.
    pub fn check(&self) -> std::result::Result<Option<Available>, SelfUpdateError> {
        let scratch = self.scratch()?;
        let result = self
            .config(&scratch)
            .map_err(SelfUpdateError::from)
            .and_then(|cfg| resolve(&cfg, &self.current_version));
        let _ = fs::remove_dir_all(&scratch);
        let (manifest, target) = result?;
        let Some(version) = target else {
            return Ok(None);
        };
        let entry = manifest::version_entry(&manifest, &version)?.clone();
        Ok(Some(Available { version, entry }))
    }

    /// Download the newest release — or the [`version`](Self::version) named —
    /// verify it and unpack it, without installing anything. `None` when the
    /// running version is already the newest. For a program that takes more
    /// from the asset than the executable: read the other files from
    /// [`Fetched::dir`], then call [`install`](Self::install).
    ///
    /// # Errors
    /// Returns an error when the source cannot be reached, the manifest or the
    /// release does not verify, the newest release is older than the running
    /// version or cannot be ordered against it, or the asset does not hold
    /// the executable. Nothing outside the scratch directory is touched.
    pub fn fetch(&self) -> std::result::Result<Option<Fetched>, SelfUpdateError> {
        let scratch = self.scratch()?;
        let fetched = self
            .config(&scratch)
            .map_err(SelfUpdateError::from)
            .and_then(|cfg| fetch_with(&cfg, &self.binary_name(), &self.current_version));
        match fetched {
            // It owns the scratch directory from here on.
            Ok(Some(fetched)) => Ok(Some(fetched)),
            other => {
                let _ = fs::remove_dir_all(&scratch);
                other
            }
        }
    }

    /// Put a [`fetch`](Self::fetch)ed release in place: prove the binary runs
    /// and reports its version, then replace the executable and its aliases.
    ///
    /// # Errors
    /// Returns an error when the binary does not run or reports another
    /// version, or a file cannot be replaced.
    /// [`SelfUpdateError::is_untouched`] tells whether anything was changed.
    pub fn install(&self, fetched: &Fetched) -> std::result::Result<Outcome, SelfUpdateError> {
        let exe = match &self.exe {
            Some(exe) => exe.clone(),
            None => current_exe()?,
        };
        install_with(&exe, &self.aliases, fetched)
    }

    /// [`fetch`](Self::fetch) and [`install`](Self::install) in one step.
    ///
    /// # Errors
    /// As those two.
    pub fn apply(&self) -> std::result::Result<Outcome, SelfUpdateError> {
        self.fetch()?
            .map_or(Ok(Outcome::UpToDate), |fetched| self.install(&fetched))
    }

    fn binary_name(&self) -> String {
        self.binary
            .clone()
            .unwrap_or_else(|| format!("{}{}", self.app, std::env::consts::EXE_SUFFIX))
    }

    /// A fresh scratch directory for one update; the caller removes it.
    fn scratch(&self) -> Result<PathBuf> {
        if self.trusted_keys.is_empty() {
            return Err(Error::Config(
                "self-update needs at least one trusted key to verify a release against".to_owned(),
            ));
        }
        scratch_dir()
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
            channel: self.channel.clone(),
            header: self.headers.clone(),
            pin: self.version.clone(),
            policy: Some(Policy::Off),
            require_signature: Some(RequireSignature::Enforce),
            require_manifest: self.require_manifest,
            trusted_keys: Some(self.trusted_keys.join(",")),
            ..Overrides::default()
        };
        config::resolve_with(&overrides, None)
    }
}

/// Fetch and verify the catalog, then decide what to install: the version id,
/// or `None` when `current_version` is already the channel latest.
fn resolve(
    cfg: &Config,
    current_version: &str,
) -> std::result::Result<(manifest::Manifest, Option<String>), SelfUpdateError> {
    let mut manifest = manifest::fetch(cfg)?;
    install::verify_manifest_identity(cfg, &mut manifest)?;
    // No floor: the plan below decides against the running binary's version;
    // a named version is the caller's deliberate choice (also the downgrade
    // path) and is installed as is.
    let target = manifest::resolve_target(
        &manifest,
        &cfg.update.channel,
        cfg.update.pin.as_deref(),
        None,
        None,
    )?;
    if cfg.update.pin.is_none() {
        let versions = || (target.clone(), current_version.to_owned());
        match plan(&target, current_version) {
            Plan::UpToDate => return Ok((manifest, None)),
            Plan::Older => {
                let (latest, current) = versions();
                return Err(SelfUpdateError::Older { latest, current });
            }
            Plan::Unordered => {
                let (latest, current) = versions();
                return Err(SelfUpdateError::Unordered { latest, current });
            }
            Plan::Install => {}
        }
    }
    Ok((manifest, Some(target)))
}

/// Download, verify and unpack the release under `cfg`'s scratch directory
/// (split from [`SelfUpdate::fetch`] so tests can drive it with a stub source).
/// `binary` is the executable's name inside the unpacked asset.
fn fetch_with(
    cfg: &Config,
    binary: &str,
    current_version: &str,
) -> std::result::Result<Option<Fetched>, SelfUpdateError> {
    let (manifest, Some(target)) = resolve(cfg, current_version)? else {
        return Ok(None);
    };

    let entry = manifest::version_entry(&manifest, &target)?;
    let asset = manifest::select_asset(entry, engine::required_asset(cfg)?)?;
    let (artifact, sha256) =
        download::fetch_artifact(cfg, asset, &target, &manifest::allowed_hosts(cfg))?;
    install::verify_download(cfg, &target, asset, &sha256)?;

    let dir = cfg.global.dir.join("unpack");
    fs::create_dir_all(&dir).map_err(Error::from)?;
    install::extract(
        asset,
        manifest::format_from_name(&asset.name),
        &artifact,
        &dir,
    )?;
    let unpacked = dir.join(binary);
    if !unpacked.is_file() {
        return Err(SelfUpdateError::MissingBinary {
            asset: asset.name.clone(),
            binary: binary.to_owned(),
        });
    }
    Ok(Some(Fetched {
        version: target,
        entry: entry.clone(),
        dir,
        binary: unpacked,
        scratch: cfg.global.dir.clone(),
    }))
}

/// Replace `exe` and those of `aliases` that exist with the fetched binary.
fn install_with(
    exe: &Path,
    aliases: &[PathBuf],
    fetched: &Fetched,
) -> std::result::Result<Outcome, SelfUpdateError> {
    replace_executable(exe, &fetched.binary, &fetched.version)?;
    for alias in aliases {
        if alias != exe && alias.is_file() {
            replace_alias(alias, &fetched.binary)?;
        }
    }
    Ok(Outcome::Installed {
        version: fetched.version.clone(),
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
/// `exe` (same filesystem, so it moves in with a `rename`), carry over `exe`'s
/// permission bits, prove it runs and reports `version`, then put it in `exe`'s
/// place ([`swap`]). On any failure the staged copy is removed and `exe` is
/// untouched.
fn replace_executable(
    exe: &Path,
    binary: &Path,
    version: &str,
) -> std::result::Result<(), SelfUpdateError> {
    let staged = stage(exe, binary).map_err(|e| SelfUpdateError::Local(e.into()))?;
    let result = probe_version(&staged, version).and_then(|()| {
        swap(&staged, exe).map_err(|e| match e {
            SwapError::Untouched(source) => SelfUpdateError::Replace {
                path: exe.to_path_buf(),
                source,
            },
            SwapError::Displaced { moved_to, source } => SelfUpdateError::Displaced {
                path: exe.to_path_buf(),
                moved_to,
                source,
            },
        })
    });
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

/// Replace `alias`, a second copy of the executable, with `binary`. The binary
/// was probed when it replaced the executable itself, which by now it has.
fn replace_alias(alias: &Path, binary: &Path) -> std::result::Result<(), SelfUpdateError> {
    let failed = |source| SelfUpdateError::Alias {
        path: alias.to_path_buf(),
        source,
    };
    let staged = stage(alias, binary).map_err(failed)?;
    swap(&staged, alias).map_err(|e| {
        let _ = fs::remove_file(&staged);
        match e {
            SwapError::Untouched(source) | SwapError::Displaced { source, .. } => failed(source),
        }
    })
}

/// Copy `binary` beside `exe` under a name that keeps `exe`'s extension, with
/// `exe`'s permission bits.
fn stage(exe: &Path, binary: &Path) -> std::io::Result<PathBuf> {
    let name = exe.file_name().and_then(OsStr::to_str).ok_or_else(|| {
        std::io::Error::other(format!("{} has no usable filename", exe.display()))
    })?;
    let staged = exe.with_file_name(format!(".self-update-{}-{name}", std::process::id()));
    let staging = fs::copy(binary, &staged).map(drop);
    // Windows has no mode bits to carry, and a read-only flag would only get
    // in the way of the next update.
    #[cfg(unix)]
    let staging =
        staging.and_then(|()| fs::set_permissions(&staged, fs::metadata(exe)?.permissions()));
    if let Err(e) = staging {
        let _ = fs::remove_file(&staged);
        return Err(std::io::Error::new(
            e.kind(),
            format!("stage {}: {e}", staged.display()),
        ));
    }
    Ok(staged)
}

/// How putting a file in an executable's place failed.
#[derive(Debug)]
enum SwapError {
    /// The executable is as it was.
    Untouched(std::io::Error),
    /// The executable was moved to `moved_to` and nothing took its place.
    #[cfg_attr(not(any(windows, test)), allow(dead_code))]
    Displaced {
        moved_to: PathBuf,
        source: std::io::Error,
    },
}

/// Put `staged` in `exe`'s place. On Unix one `rename` does it: the running
/// process keeps the old inode (a mapped executable cannot be written —
/// `ETXTBSY` — which is why this is a rename, not a write).
#[cfg(not(windows))]
fn swap(staged: &Path, exe: &Path) -> std::result::Result<(), SwapError> {
    fs::rename(staged, exe).map_err(SwapError::Untouched)
}

/// Put `staged` in `exe`'s place. Windows refuses to overwrite or delete a
/// running executable but lets it be renamed, so the old file moves aside
/// first; see `swap_keeping_old`.
#[cfg(windows)]
fn swap(staged: &Path, exe: &Path) -> std::result::Result<(), SwapError> {
    swap_keeping_old(staged, exe)
}

/// Rename `exe` aside, rename `staged` into its place, and put `exe` back when
/// that fails. The file moved aside is deleted when nothing runs it any more:
/// here if possible, else by [`remove_replaced`] on a later start.
#[cfg(any(windows, test))]
fn swap_keeping_old(staged: &Path, exe: &Path) -> std::result::Result<(), SwapError> {
    let nonce = getrandom::u64().map_err(|e| SwapError::Untouched(std::io::Error::other(e)))?;
    let old = exe.with_file_name(format!("{}{nonce:016x}", old_prefix(exe)));
    fs::rename(exe, &old).map_err(SwapError::Untouched)?;
    if let Err(source) = fs::rename(staged, exe) {
        return Err(match fs::rename(&old, exe) {
            Ok(()) => SwapError::Untouched(source),
            Err(_) => SwapError::Displaced {
                moved_to: old,
                source,
            },
        });
    }
    let _ = fs::remove_file(&old);
    Ok(())
}

/// What the files `swap_keeping_old` moves aside for `exe` are named with.
fn old_prefix(exe: &Path) -> String {
    let name = exe.file_name().and_then(OsStr::to_str).unwrap_or_default();
    format!(".{name}.old-")
}

/// The files earlier updates moved aside for `exe` that are still there.
fn replaced_copies(exe: &Path) -> Vec<PathBuf> {
    let prefix = old_prefix(exe);
    let Some(entries) = exe.parent().and_then(|dir| fs::read_dir(dir).ok()) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect()
}

/// Delete what earlier updates left of `exe`.
///
/// On Windows the replaced binary stays beside it for as long as a process
/// still runs it. Best effort — a file still in use stays for the next call. A
/// program that updates itself calls this when it starts.
pub fn remove_replaced(exe: &Path) {
    for old in replaced_copies(exe) {
        let _ = fs::remove_file(old);
    }
}

/// Run `staged --version` and require it to report `version`: catches a download
/// built for another platform (fails to exec) or a mislabelled release before it
/// replaces a working binary.
fn probe_version(staged: &Path, version: &str) -> std::result::Result<(), SelfUpdateError> {
    let output = spawn_probe(staged).map_err(|source| SelfUpdateError::NotRunnable {
        path: staged.to_path_buf(),
        source,
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && stdout.split_whitespace().any(|word| word == version) {
        return Ok(());
    }
    Err(SelfUpdateError::WrongVersion {
        expected: version.to_owned(),
        reported: stdout.trim().to_owned(),
    })
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
/// cache and the unpacked archive; whoever asked for it removes it.
fn scratch_dir() -> Result<PathBuf> {
    scratch_dir_in(&std::env::temp_dir())
}

/// Create a fresh owner-only directory under `base`. The name is unpredictable
/// and creation fails when the path exists, so another local user cannot plant
/// the directory and swap the verified binary before it is installed.
fn scratch_dir_in(base: &Path) -> Result<PathBuf> {
    let nonce = getrandom::u64()
        .map_err(|e| Error::Install(format!("self-update: no randomness for scratch dir: {e}")))?;
    let dir = base.join(format!("lode-self-update-{nonce:016x}"));
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700).create(&dir)?;
    // On Windows the user's temp directory is already private to the user.
    #[cfg(not(unix))]
    fs::create_dir(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use ed25519_dalek::{Signer as _, SigningKey};
    use serde_json::json;

    use super::*;

    /// Fetch and install in one go, as [`SelfUpdate::apply`] does.
    #[cfg(unix)]
    fn run_with(
        cfg: &Config,
        exe: &Path,
        aliases: &[PathBuf],
        binary: &str,
        current_version: &str,
    ) -> std::result::Result<Outcome, SelfUpdateError> {
        fetch_with(cfg, binary, current_version)?.map_or(Ok(Outcome::UpToDate), |fetched| {
            install_with(exe, aliases, &fetched)
        })
    }

    #[test]
    fn scratch_dir_is_fresh_and_owner_only() {
        let base = scratch("scratch-dir");
        let first = scratch_dir_in(&base).unwrap();
        let second = scratch_dir_in(&base).unwrap();
        assert_ne!(first, second, "each call gets its own directory");
        #[cfg(unix)]
        for dir in [&first, &second] {
            use std::os::unix::fs::PermissionsExt as _;
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
    fn follows_the_stable_channel_with_no_headers_unless_told_otherwise() {
        let dir = scratch("config-defaults");
        let update = SelfUpdate::new("myapp", Source::GitHub("o/r".to_owned()), "1.0.0")
            .asset("myapp-linux-x64.tar.gz")
            .trusted_keys(["k1:AAAA"]);
        let cfg = update.config(&dir).unwrap();
        assert_eq!(cfg.update.channel, "stable");
        assert!(cfg.http.headers.is_empty());

        let cfg = update
            .channel("beta")
            .header("Authorization: Bearer ${MYAPP_TOKEN}")
            .header("X-Client: myapp")
            .config(&dir)
            .unwrap();
        assert_eq!(cfg.update.channel, "beta");
        assert_eq!(
            cfg.http.headers,
            ["Authorization: Bearer ${MYAPP_TOKEN}", "X-Client: myapp"]
        );
        // Still nothing but the source itself may receive them.
        assert!(cfg.http.credential_hosts.is_empty());
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
        assert!(matches!(err, SelfUpdateError::Config(_)), "{err}");
        assert!(err.is_untouched());
        let err = update.exe("/nonexistent/myapp").apply().unwrap_err();
        assert!(matches!(err, SelfUpdateError::Config(_)), "{err}");
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

    #[cfg(unix)]
    fn reported_version(exe: &Path) -> String {
        let out = Command::new(exe).arg("--version").output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// No `.self-update-*` staging file was left behind in `dir`.
    #[cfg(unix)]
    fn no_staging_leftovers(dir: &Path) -> bool {
        !fs::read_dir(dir).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".self-update-")
        })
    }

    #[test]
    fn swap_keeping_old_replaces_the_file_and_puts_the_old_one_aside() {
        let dir = scratch("swap-old");
        let exe = dir.join("app.exe");
        fs::write(&exe, b"old").unwrap();
        let staged = dir.join(".self-update-1-app.exe");
        fs::write(&staged, b"new").unwrap();

        swap_keeping_old(&staged, &exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert!(!staged.exists());
        // Nothing runs the old file here, so it could be removed at once.
        assert!(replaced_copies(&exe).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn swap_keeping_old_puts_the_file_back_when_the_new_one_cannot_move_in() {
        let dir = scratch("swap-back");
        let exe = dir.join("app.exe");
        fs::write(&exe, b"old").unwrap();

        let err = swap_keeping_old(&dir.join("missing"), &exe).unwrap_err();
        assert!(
            matches!(&err, SwapError::Untouched(e) if e.kind() == std::io::ErrorKind::NotFound),
            "{err:?}"
        );
        assert_eq!(fs::read(&exe).unwrap(), b"old");
        assert!(replaced_copies(&exe).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_replaced_clears_what_earlier_updates_left_of_this_executable_only() {
        let dir = scratch("old-copies");
        let exe = dir.join("app.exe");
        fs::write(&exe, b"current").unwrap();
        let mine = dir.join(".app.exe.old-00000000000000aa");
        let other = dir.join(".tool.exe.old-00000000000000bb");
        fs::write(&mine, b"x").unwrap();
        fs::write(&other, b"x").unwrap();

        remove_replaced(&exe);
        assert!(!mine.exists());
        assert!(other.exists());
        assert!(exe.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Stays alive while `swap_keeping_old_replaces_a_running_executable` swaps
    /// the file it runs from. Not a test of its own.
    #[cfg(windows)]
    #[test]
    #[ignore = "helper process for swap_keeping_old_replaces_a_running_executable"]
    fn stay_alive_for_the_swap_test() {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    /// The point of the Windows path: a running executable cannot be overwritten
    /// or deleted, but it can be renamed aside.
    #[cfg(windows)]
    #[test]
    fn swap_keeping_old_replaces_a_running_executable() {
        let dir = scratch("swap-running");
        let exe = dir.join("app.exe");
        fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let mut running = Command::new(&exe)
            .args([
                "--exact",
                "self_update::tests::stay_alive_for_the_swap_test",
                "--ignored",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(fs::write(&exe, b"new").is_err(), "a running exe is locked");

        let staged = dir.join(".self-update-1-app.exe");
        fs::write(&staged, b"new").unwrap();
        swap_keeping_old(&staged, &exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        // The old file is still mapped by the running process: it stays, aside.
        assert_eq!(replaced_copies(&exe).len(), 1);

        running.kill().unwrap();
        running.wait().unwrap();
        remove_replaced(&exe);
        assert!(replaced_copies(&exe).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn an_alias_of_the_executable_gets_the_same_binary() {
        let dir = scratch("alias");
        let exe = dir.join("lode");
        let alias = dir.join("lode-cli");
        write_fake_lode(&exe, "0.1.0", 0o755);
        write_fake_lode(&alias, "0.1.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, entry) = sign(&tarball, "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));
        let cfg = stub_config(&stub, None, &[entry], &dir.join("scratch"));

        // An alias that does not exist is left out; the executable itself is not done twice.
        let aliases = [alias.clone(), dir.join("absent"), exe.clone()];
        run_with(&cfg, &exe, &aliases, BINARY, "0.1.0").unwrap();
        assert_eq!(reported_version(&exe), "lode 9.9.9");
        assert_eq!(reported_version(&alias), "lode 9.9.9");
        assert!(!dir.join("absent").exists());
        assert!(no_staging_leftovers(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_binary_defaults_to_the_app_name_with_the_platform_suffix() {
        let update = SelfUpdate::new("myapp", Source::GitHub("o/r".to_owned()), "1.0.0");
        assert_eq!(
            update.binary_name(),
            format!("myapp{}", std::env::consts::EXE_SUFFIX)
        );
        assert_eq!(update.binary("tool.bin").binary_name(), "tool.bin");
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
        assert!(
            matches!(&err, SelfUpdateError::WrongVersion { expected, reported }
                if expected == "9.9.9" && reported == "lode 1.2.3"),
            "{err:?}"
        );
        assert!(err.to_string().contains("expected version 9.9.9"), "{err}");
        // ...and one that cannot run at all (a foreign-platform binary).
        let broken = dir.join("broken-lode");
        fs::write(&broken, b"\x7fELF not really").unwrap();
        let err = replace_executable(&exe, &broken, "9.9.9").unwrap_err();
        assert!(
            matches!(err, SelfUpdateError::NotRunnable { .. }),
            "{err:?}"
        );
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
            runtime: None,
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
            let release = json!({
                "tag_name": tag,
                "body": "What changed.",
                "published_at": "2026-10-12T10:00:00Z",
                "assets": assets,
            })
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

        run_with(&cfg, &exe, &[], BINARY, "0.1.0").unwrap();

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

        let err = run_with(&cfg, &exe, &[], BINARY, "0.1.0").unwrap_err();
        assert!(matches!(err, SelfUpdateError::Verify(_)), "{err}");
        assert!(err.is_untouched());
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

        let err = run_with(&cfg, &exe, &[], BINARY, "0.1.0").unwrap_err();
        assert!(matches!(err, SelfUpdateError::Verify(_)), "{err}");
        assert!(err.is_untouched());
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

        run_with(&cfg, &exe, &[], BINARY, "9.9.9").unwrap();

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
        let err = run_with(&cfg, &exe, &[], BINARY, "10.0.0").unwrap_err();
        assert!(
            matches!(&err, SelfUpdateError::Older { latest, current }
                if latest == "9.9.9" && current == "10.0.0"),
            "{err:?}"
        );
        assert!(err.to_string().contains("older than this binary"), "{err}");
        assert_eq!(reported_version(&exe), "lode 10.0.0");

        // ...while naming the raw tag installs it: the pin resolves to the same
        // `9.9.9` id the signature was made over, not the raw `v9.9.9`.
        let cfg = stub_config(&stub, Some("v9.9.9"), &[entry], &dir.join("scratch"));
        run_with(&cfg, &exe, &[], BINARY, "10.0.0").unwrap();
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
        let err = run_with(&cfg, &exe, &[], BINARY, "nightly-2").unwrap_err();
        assert!(matches!(err, SelfUpdateError::Unordered { .. }), "{err:?}");
        assert!(err.to_string().contains("cannot be ordered"), "{err}");
        assert_eq!(reported_version(&exe), "lode nightly-2");

        let cfg = stub_config(&stub, Some("nightly-1"), &[entry], &dir.join("scratch"));
        run_with(&cfg, &exe, &[], BINARY, "nightly-2").unwrap();
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
        let err = run_with(&cfg, &exe, &[], "myapp", "0.1.0").unwrap_err();
        assert!(
            matches!(&err, SelfUpdateError::MissingBinary { asset, binary }
                if asset == ASSET && binary == "myapp"),
            "{err:?}"
        );
        assert_eq!(reported_version(&exe), "lode 0.1.0");

        // The member name is independent of the installed executable's name.
        let outcome = run_with(&cfg, &exe, &[], BINARY, "0.1.0").unwrap();
        assert_eq!(
            outcome,
            Outcome::Installed {
                version: "9.9.9".to_owned(),
                exe: exe.clone(),
            }
        );
        assert_eq!(reported_version(&exe), "lode 9.9.9");
        assert_eq!(
            run_with(&cfg, &exe, &[], BINARY, "9.9.9").unwrap(),
            Outcome::UpToDate
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_fetched_release_is_unpacked_for_the_caller_before_anything_is_installed() {
        let dir = scratch("fetch");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "0.1.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, entry) = sign(&tarball, "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));
        let scratch_dir = dir.join("scratch");
        let cfg = stub_config(&stub, None, &[entry], &scratch_dir);

        let fetched = fetch_with(&cfg, BINARY, "0.1.0").unwrap().unwrap();
        assert_eq!(fetched.version(), "9.9.9");
        assert_eq!(fetched.entry().notes.as_deref(), Some("What changed."));
        assert_eq!(
            fetched.entry().published_at.as_deref(),
            Some("2026-10-12T10:00:00Z")
        );
        // Every file of the verified asset is there to take, and nothing is installed yet.
        assert!(fetched.dir().join(BINARY).is_file());
        assert_eq!(fetched.binary(), fetched.dir().join(BINARY));
        assert!(
            fs::read_to_string(fetched.binary())
                .unwrap()
                .contains("9.9.9")
        );
        assert_eq!(reported_version(&exe), "lode 0.1.0");

        install_with(&exe, &[], &fetched).unwrap();
        assert_eq!(reported_version(&exe), "lode 9.9.9");
        drop(fetched);
        assert!(!scratch_dir.exists(), "dropping it removes the scratch dir");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_required_manifest_signature_refuses_an_unsigned_manifest() {
        let dir = scratch("require-manifest");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "0.1.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, entry) = sign(&tarball, "9.9.9");
        // A native manifest whose asset is signed and whose catalog is not.
        let stub = Stub::start(move |base| {
            let manifest = json!({
                "schema": "lode/v1",
                "name": BINARY,
                "channels": { "stable": { "latest": "9.9.9" } },
                "versions": { "9.9.9": {
                    "notes": "What changed.",
                    "published_at": "2026-10-12T10:00:00Z",
                    "assets": [{
                        "name": ASSET,
                        "url": format!("{base}/dl/{ASSET}"),
                        "sha256": sha,
                        "sig": sig,
                    }],
                } },
            });
            vec![
                (
                    "/manifest.json".to_owned(),
                    manifest.to_string().into_bytes(),
                ),
                (format!("/dl/{ASSET}"), tarball),
            ]
        });
        let update = SelfUpdate::new(
            BINARY,
            Source::Manifest(format!("{}/manifest.json", stub.base)),
            "0.1.0",
        )
        .asset(ASSET)
        .trusted_keys([entry]);

        let strict = update
            .clone()
            .require_manifest(RequireManifest::Signed)
            .config(&dir.join("scratch"))
            .unwrap();
        let err = run_with(&strict, &exe, &[], BINARY, "0.1.0").unwrap_err();
        assert!(matches!(err, SelfUpdateError::Verify(_)), "{err:?}");
        assert!(err.is_untouched());
        assert_eq!(reported_version(&exe), "lode 0.1.0");
        assert!(!stub.served().iter().any(|path| path.starts_with("/dl/")));

        // Left at the default the same manifest is followed, and its entry is
        // what `check` hands on.
        let lenient = update.config(&dir.join("scratch")).unwrap();
        let (manifest, target) = resolve(&lenient, "0.1.0").unwrap();
        let entry = manifest::version_entry(&manifest, &target.unwrap()).unwrap();
        assert_eq!(entry.notes.as_deref(), Some("What changed."));
        assert_eq!(entry.published_at.as_deref(), Some("2026-10-12T10:00:00Z"));
        run_with(&lenient, &exe, &[], BINARY, "0.1.0").unwrap();
        assert_eq!(reported_version(&exe), "lode 9.9.9");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_github_source_cannot_be_asked_for_a_signed_manifest() {
        let dir = scratch("require-manifest-github");
        let err = updater(None, &["k1:AAAA".to_owned()])
            .require_manifest(RequireManifest::Signed)
            .config(&dir)
            .unwrap_err();
        assert!(matches!(
            SelfUpdateError::from(err),
            SelfUpdateError::Config(_)
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_errors_that_changed_the_installation_say_so() {
        let io = || std::io::Error::other("denied");
        let alias = SelfUpdateError::Alias {
            path: PathBuf::from("lode-cli.exe"),
            source: io(),
        };
        let displaced = SelfUpdateError::Displaced {
            path: PathBuf::from("lode.exe"),
            moved_to: PathBuf::from(".lode.exe.old-1"),
            source: io(),
        };
        let replace = SelfUpdateError::Replace {
            path: PathBuf::from("lode"),
            source: io(),
        };
        assert!(!alias.is_untouched());
        assert!(!displaced.is_untouched());
        assert!(replace.is_untouched());
        // The command-line caller still gets lode's own error.
        assert!(matches!(Error::from(replace), Error::Install(_)));
        assert!(matches!(
            Error::from(SelfUpdateError::from(Error::Verify("bad".to_owned()))),
            Error::Verify(_)
        ));
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
