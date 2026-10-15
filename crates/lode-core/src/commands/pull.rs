//! `lode pull` — fetch, verify and activate `[[resource]]` bundles.
//!
//! With a supervised instance running, each pull is requested through
//! `state.resource_target` so the supervisor applies it and notifies the app;
//! otherwise the resource is activated directly.

use std::io::Write;

use crate::commands::update::{live_holder, supervisor_running};
use crate::config::{Config, Resource};
use crate::error::{Error, Result};
use crate::resource::{self, Applied};
use crate::state;

/// Pull `name` (every configured resource when `None`) to `version` (the
/// channel latest when `None`; an older version is a rollback).
pub fn run(cfg: &Config, name: Option<&str>, version: Option<&str>) -> Result<()> {
    let resources: Vec<&Resource> = match name {
        Some(name) => vec![resource::find(cfg, name)?],
        None if version.is_some() => {
            return Err(Error::Config("--version needs --resource".to_owned()));
        }
        None => cfg.resources.iter().collect(),
    };
    if resources.is_empty() {
        return Err(Error::Config("no [[resource]] configured".to_owned()));
    }
    let mut out = std::io::stdout().lock();
    let state_path = cfg.global.dir.join("state.json");
    let st = state::read(&state_path)?.unwrap_or_default();
    if supervisor_running(&st, live_holder(cfg)) {
        let target = version.unwrap_or("latest");
        state::locked_update(&state_path, |st| {
            for res in &resources {
                st.resource_target
                    .insert(res.name.clone(), target.to_owned());
            }
        })?;
        for res in &resources {
            writeln!(
                out,
                "lode pull: {}: a service is running — requested {target}",
                res.name
            )?;
        }
        return Ok(());
    }
    for res in resources {
        match resource::update(cfg, res, version)? {
            Applied::Updated { from, to } => writeln!(
                out,
                "lode pull: {}: {} -> {to}",
                res.name,
                from.as_deref().unwrap_or("(none)")
            )?,
            Applied::Current(v) => writeln!(out, "lode pull: {}: already on {v}", res.name)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_in(dir: &std::path::Path) -> Config {
        Config::from_toml(&format!(
            "[global]\napp = \"myapp\"\ndir = \"{}\"\n\
             [[resource]]\nname = \"config\"\nmanifest = \"https://r.example/c.json\"\nasset = \"c.json\"\n\
             [[resource]]\nname = \"geo\"\nmanifest = \"https://r.example/g.json\"\nasset = \"g.db\"\n",
            dir.display()
        ))
        .unwrap()
    }

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lode-pull-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pull_rejects_a_version_without_a_resource_and_unknown_names() {
        let dir = scratch("reject");
        let cfg = cfg_in(&dir);
        assert!(run(&cfg, None, Some("1.0.0")).is_err());
        assert!(run(&cfg, Some("nope"), None).is_err());
    }

    #[test]
    fn pull_with_a_running_supervisor_requests_through_state() {
        let dir = scratch("request");
        let cfg = cfg_in(&dir);
        let path = dir.join("state.json");
        state::locked_update(&path, |st| st.pid = Some(std::process::id())).unwrap();

        run(&cfg, None, None).unwrap();
        run(&cfg, Some("geo"), Some("1.2.0")).unwrap();

        let st = state::read(&path).unwrap().unwrap();
        assert_eq!(
            st.resource_target.get("config").map(String::as_str),
            Some("latest")
        );
        assert_eq!(
            st.resource_target.get("geo").map(String::as_str),
            Some("1.2.0")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
