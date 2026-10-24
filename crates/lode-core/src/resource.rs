//! `[[resource]]` — separately published bundles (config files, data, assets),
//! verified like the app and installed under `$LODE_DIR/resources/<name>`.
//!
//! A resource is driven through a config [`scoped`] to it: its `[global].app` is
//! the identity `<app>/<name>`, which the manifest `name`, every artifact
//! signature and any delegation must carry, and which
//! [`crate::install::install_root`] maps to the resource's own directory.

use std::path::PathBuf;

use crate::config::{Config, Resource};
use crate::error::{Error, Result};
use crate::manifest::{self, Manifest};
use crate::{download, install, state};

/// The identity a resource's manifest and signatures carry: `<app>/<name>`.
pub fn identity(cfg: &Config, name: &str) -> String {
    format!("{}/{name}", cfg.global.app)
}

/// The configured `[[resource]]` called `name`.
pub fn find<'a>(cfg: &'a Config, name: &str) -> Result<&'a Resource> {
    cfg.resources
        .iter()
        .find(|r| r.name == name)
        .ok_or_else(|| Error::Config(format!("no [[resource]] named {name:?}")))
}

/// `cfg` scoped to `res`: its identity, source, asset, channel and policy.
pub fn scoped(cfg: &Config, res: &Resource) -> Config {
    let mut scoped = cfg.clone();
    scoped.global.app = identity(cfg, &res.name);
    scoped.update.manifest = Some(res.manifest.clone());
    scoped.update.github = None;
    scoped.update.asset = Some(res.asset.clone());
    scoped.update.channel.clone_from(&res.channel);
    scoped.update.policy = res.policy;
    scoped.update.pin = None;
    scoped.command.run = None;
    scoped.command.exec = None;
    scoped.resources = Vec::new();
    scoped
}

/// The path handed to the app for the active `version`: the stable `current`
/// link on Unix, the version directory elsewhere.
pub fn active_path(cfg: &Config, name: &str, version: &str) -> PathBuf {
    let root = cfg.global.dir.join("resources").join(name);
    if cfg!(unix) {
        root.join("current")
    } else {
        root.join("versions").join(version)
    }
}

/// The active version recorded in `state.json`.
pub fn current(cfg: &Config, name: &str) -> Option<String> {
    state::read_lenient(&cfg.global.dir.join("state.json"))?
        .resources
        .get(name)?
        .current
        .clone()
}

/// Fetch `res`'s manifest and verify it: its `name` is the resource identity, no
/// asset publishes a launch command, and its signature and expiry hold.
pub fn fetch(cfg: &Config, res: &Resource) -> Result<Manifest> {
    let scoped = scoped(cfg, res);
    let mut m = manifest::fetch(&scoped)?;
    if m.name != scoped.global.app {
        return Err(Error::Manifest(format!(
            "resource manifest is for {:?}, not {:?}",
            m.name, scoped.global.app
        )));
    }
    if let Some(asset) = m
        .versions
        .values()
        .flat_map(|e| &e.assets)
        .find(|a| a.run.is_some() || a.exec.is_some())
    {
        return Err(Error::Manifest(format!(
            "resource asset {:?} publishes a run/exec launch command",
            asset.name
        )));
    }
    install::verify_manifest_identity(&scoped, &mut m)?;
    Ok(m)
}

/// The channel latest of a fetched resource manifest.
pub fn latest(m: &Manifest, res: &Resource) -> Result<String> {
    manifest::resolve_target(m, &res.channel, None, Some("latest"), None)
}

/// Install `version` from `m` (unless already installed), make it active,
/// record it in `state.json` and prune old versions.
pub fn activate(cfg: &Config, res: &Resource, m: &Manifest, version: &str) -> Result<()> {
    let scoped = scoped(cfg, res);
    install::gc(&scoped)?;
    let entry = manifest::version_entry(m, version)?;
    let asset = manifest::select_asset(entry, &res.asset)?;
    if !install::install_root(&scoped)
        .join("versions")
        .join(version)
        .is_dir()
    {
        let (artifact, sha256) =
            download::fetch_artifact(&scoped, asset, version, &manifest::allowed_hosts(&scoped))?;
        install::install(&scoped, version, asset, &artifact, &sha256)?;
    }
    install::switch_current(&scoped, version)?;
    let previous = current(cfg, &res.name);
    let path = active_path(cfg, &res.name, version);
    state::locked_update(&cfg.global.dir.join("state.json"), |st| {
        let entry = st.resources.entry(res.name.clone()).or_default();
        entry.current = Some(version.to_owned());
        entry.path = Some(path.display().to_string());
        entry.error = None;
        if entry.available.as_deref() == Some(version) {
            entry.available = None;
        }
    })?;
    install::prune(&scoped, Some(version), previous.as_deref())
}

/// What [`update`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Switched to `to`.
    Updated {
        /// The previously active version.
        from: Option<String>,
        /// The now active version.
        to: String,
    },
    /// Already on this version.
    Current(String),
}

/// Fetch `res`, resolve `requested` (`None` or `"latest"` = the channel latest,
/// never below the active version; anything else = that version) and activate it.
pub fn update(cfg: &Config, res: &Resource, requested: Option<&str>) -> Result<Applied> {
    let m = fetch(cfg, res)?;
    let from = current(cfg, &res.name);
    let explicit = requested.filter(|r| *r != "latest");
    let to = manifest::resolve_target(&m, &res.channel, None, explicit, from.as_deref())?;
    if from.as_deref() == Some(to.as_str()) {
        return Ok(Applied::Current(to));
    }
    activate(cfg, res, &m, &to)?;
    Ok(Applied::Updated { from, to })
}

/// Edit `name`'s entry in `state.json`.
pub fn record(cfg: &Config, name: &str, edit: impl FnOnce(&mut state::ResourceState)) {
    state::locked_update_lenient(&cfg.global.dir.join("state.json"), |st| {
        edit(st.resources.entry(name.to_owned()).or_default());
    });
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use base64::Engine as _;
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;
    use crate::stub::Stub;
    use crate::verify::{Artifact, B64, PublicKey, artifact_message, key_entry, sha256_hex};

    fn scratch(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lode-resource-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn root_key() -> SigningKey {
        SigningKey::from_bytes(&[40u8; 32])
    }

    /// One asset entry signed by `key` for `identity`.
    fn asset(
        base: &str,
        identity: &str,
        version: &str,
        body: &[u8],
        key: &SigningKey,
    ) -> serde_json::Value {
        let sha = sha256_hex(body);
        let message = artifact_message(
            &Artifact {
                app: identity,
                name: "settings.json",
                version,
                path: "",
                run: None,
                exec: None,
                runtime: None,
            },
            &sha,
        );
        serde_json::json!({
            "name": "settings.json",
            "url": format!("{base}/a/{version}"),
            "sha256": sha,
            "sig": B64.encode(key.sign(&message).to_bytes()),
        })
    }

    /// A stub serving a resource catalog named `name` (versions 1.0.0 and 2.0.0,
    /// latest 2.0.0) whose assets are signed for `identity`; `edit` adjusts it.
    fn catalog(name: &str, identity: &str, edit: impl FnOnce(&mut serde_json::Value)) -> Stub {
        let name = name.to_owned();
        let identity = identity.to_owned();
        Stub::start(move |base| {
            let key = root_key();
            let mut m = serde_json::json!({
                "schema": "lode/v1",
                "name": name,
                "channels": { "stable": { "latest": "2.0.0" } },
                "versions": {
                    "1.0.0": { "assets": [asset(base, &identity, "1.0.0", b"{\"v\":1}", &key)] },
                    "2.0.0": { "assets": [asset(base, &identity, "2.0.0", b"{\"v\":2}", &key)] },
                },
            });
            edit(&mut m);
            vec![
                ("/config.json".to_owned(), m.to_string().into_bytes()),
                ("/a/1.0.0".to_owned(), b"{\"v\":1}".to_vec()),
                ("/a/2.0.0".to_owned(), b"{\"v\":2}".to_vec()),
            ]
        })
    }

    fn cfg_for(dir: &std::path::Path, base: &str) -> Config {
        let root = key_entry(&PublicKey::Ed25519(root_key().verifying_key()));
        Config::from_toml(&format!(
            "[global]\napp = \"myapp\"\ndir = \"{}\"\n[update]\nmanifest = \"{base}/app.json\"\n\
             [trust]\nrequire_signature = \"enforce\"\ntrusted_keys = [\"{root}\"]\n\
             [[resource]]\nname = \"config\"\nmanifest = \"{base}/config.json\"\nasset = \"settings.json\"\n",
            dir.display()
        ))
        .unwrap()
    }

    #[test]
    fn update_installs_activates_and_records_the_resource() {
        let dir = scratch("update");
        let stub = catalog("myapp/config", "myapp/config", |_| {});
        let cfg = cfg_for(&dir, &stub.base);
        let res = find(&cfg, "config").unwrap();

        assert_eq!(
            update(&cfg, res, None).unwrap(),
            Applied::Updated {
                from: None,
                to: "2.0.0".to_owned()
            }
        );
        let installed = dir.join("resources/config/versions/2.0.0/settings.json");
        assert_eq!(fs::read(&installed).unwrap(), b"{\"v\":2}");
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(dir.join("resources/config/current")).unwrap(),
            PathBuf::from("versions/2.0.0")
        );
        let st = crate::state::read(&dir.join("state.json"))
            .unwrap()
            .unwrap();
        let entry = &st.resources["config"];
        assert_eq!(entry.current.as_deref(), Some("2.0.0"));
        assert_eq!(
            entry.path.as_deref(),
            Some(active_path(&cfg, "config", "2.0.0").to_str().unwrap())
        );
        assert_eq!(current(&cfg, "config").as_deref(), Some("2.0.0"));
        // The app's own data dir is untouched.
        assert!(!dir.join("versions").exists());

        // Already on the channel latest.
        assert_eq!(
            update(&cfg, res, None).unwrap(),
            Applied::Current("2.0.0".to_owned())
        );
        // An explicit version is the operator's choice, even a lower one.
        assert_eq!(
            update(&cfg, res, Some("1.0.0")).unwrap(),
            Applied::Updated {
                from: Some("2.0.0".to_owned()),
                to: "1.0.0".to_owned()
            }
        );
        // Following `latest` from 1.0.0 goes back up.
        assert_eq!(
            update(&cfg, res, Some("latest")).unwrap(),
            Applied::Updated {
                from: Some("1.0.0".to_owned()),
                to: "2.0.0".to_owned()
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_resource_catalog_must_carry_the_resource_identity() {
        // The app's own catalog (name `myapp`) is not a resource catalog.
        let dir = scratch("identity");
        let stub = catalog("myapp", "myapp", |_| {});
        let cfg = cfg_for(&dir, &stub.base);
        let err = update(&cfg, find(&cfg, "config").unwrap(), None).unwrap_err();
        assert!(err.to_string().contains("myapp/config"), "{err}");

        // Right name, but assets signed for the app: the signature does not cover them.
        let stub = catalog("myapp/config", "myapp", |_| {});
        let cfg = cfg_for(&dir, &stub.base);
        assert!(update(&cfg, find(&cfg, "config").unwrap(), None).is_err());
        assert!(current(&cfg, "config").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_resource_cannot_publish_launch_commands() {
        let dir = scratch("launch");
        let stub = catalog("myapp/config", "myapp/config", |m| {
            m["versions"]["2.0.0"]["assets"][0]["run"] = serde_json::json!("./evil");
        });
        let cfg = cfg_for(&dir, &stub.base);
        let err = update(&cfg, find(&cfg, "config").unwrap(), None).unwrap_err();
        assert!(err.to_string().contains("run"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_names_the_unknown_resource() {
        let cfg = Config::from_toml("").unwrap();
        assert!(find(&cfg, "nope").unwrap_err().to_string().contains("nope"));
    }
}
