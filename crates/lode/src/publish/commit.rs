//! Signed manifest commits: the registry serves the newest commit (its head)
//! as the package manifest. A change is applied to the head, signed with the
//! catalog `sig` and the `commit_sig`, and uploaded; when another commit got
//! there first (409 `COMMIT_CONFLICT`) it is applied again to the new head.

use std::io;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Map, Value, json};

use lode_core::verify::B64;

use super::Client;
use crate::authoring::SecretKey;

/// Uploads tried per change, counting the first.
const ATTEMPTS: u32 = 5;

/// What a commit changes in the head manifest.
pub(super) enum Change<'a> {
    /// Add `version` with its `assets` (manifest asset objects) and point
    /// `channel` at it.
    Publish {
        version: &'a str,
        channel: &'a str,
        notes: Option<&'a str>,
        assets: Vec<Value>,
    },
    /// Drop `version` from the manifest.
    Yank { version: &'a str },
    /// Point `channel` at `version`.
    Channel { channel: &'a str, version: &'a str },
}

impl Change<'_> {
    fn apply(
        &self,
        versions: &mut Map<String, Value>,
        channels: &mut Map<String, Value>,
    ) -> Result<()> {
        match self {
            Self::Publish {
                version,
                channel,
                notes,
                assets,
            } => {
                let mut entry = json!({ "assets": assets });
                if let Some(notes) = notes {
                    entry["notes"] = json!(notes);
                }
                versions.insert((*version).to_owned(), entry);
                channels.insert((*channel).to_owned(), json!({ "latest": version }));
            }
            Self::Yank { version } => {
                if versions.remove(*version).is_none() {
                    bail!("version {version} is not in the manifest");
                }
                let pointing: Vec<&str> = channels
                    .iter()
                    .filter(|(_, c)| c["latest"].as_str() == Some(version))
                    .map(|(name, _)| name.as_str())
                    .collect();
                if !pointing.is_empty() {
                    bail!(
                        "channel {} points at {version}; move it first",
                        pointing.join(", ")
                    );
                }
            }
            Self::Channel { channel, version } => {
                if !versions.contains_key(*version) {
                    bail!("version {version} is not in the manifest");
                }
                channels.insert((*channel).to_owned(), json!({ "latest": version }));
            }
        }
        Ok(())
    }
}

/// `head` with `change` applied, signed as the commit after `parent`
/// (generation `generation`). Returns the manifest and its commit id.
pub(super) fn build(
    head: &Value,
    change: &Change<'_>,
    key: &SecretKey,
    parent: Option<&str>,
    generation: u64,
    created: &str,
) -> Result<(Value, String)> {
    let mut root = head.clone();
    let obj = root
        .as_object_mut()
        .context("the head manifest is not a JSON object")?;
    // Signatures of the head do not cover the new catalog; the registry serves
    // its own delegation.
    for stale in [
        "sig",
        "key_id",
        "alg",
        "expires",
        "expires_sig",
        "commit_sig",
        "delegation",
        "parent",
    ] {
        obj.remove(stale);
    }
    let mut versions = obj
        .remove("versions")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut channels = obj
        .remove("channels")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    change.apply(&mut versions, &mut channels)?;
    obj.insert("versions".to_owned(), Value::Object(versions));
    obj.insert("channels".to_owned(), Value::Object(channels));
    if let Some(parent) = parent {
        obj.insert("parent".to_owned(), json!(parent));
    }
    obj.insert("generation".to_owned(), json!(generation));
    obj.insert("created".to_owned(), json!(created));
    obj.insert("key_id".to_owned(), json!(key.public().key_id()));

    let manifest = lode_core::manifest::parse(&serde_json::to_vec(&root)?)
        .map_err(|e| anyhow!("the new manifest is not valid: {e}"))?;
    let message = manifest
        .commit_signing_message()
        .context("the new manifest has no commit")?;
    let id = lode_core::verify::sha256_hex(&message);
    let obj = root.as_object_mut().context("unreachable")?;
    obj.insert(
        "sig".to_owned(),
        json!(B64.encode(key.sign(&manifest.signing_message()))),
    );
    obj.insert(
        "commit_sig".to_owned(),
        json!(B64.encode(key.sign(&message))),
    );
    Ok((root, id))
}

/// Apply `change` to the package's head as a signed commit, building again on
/// a newer head after a conflict.
pub(super) fn commit(
    client: &Client<'_>,
    package: &str,
    key: &SecretKey,
    change: &Change<'_>,
    out: &mut dyn io::Write,
) -> Result<()> {
    let commits = format!("{}/packages/{package}/commits", client.api);
    for attempt in 1..=ATTEMPTS {
        client.check_interrupt()?;
        let head = client.get(&format!("{commits}/head"))?;
        head.accept(&format!("lookup of the manifest of {package}"))?;
        let data = head.data();
        let parent = data["commit"].as_str();
        let generation = data["generation"].as_u64().unwrap_or(0) + 1;
        let (manifest, id) = build(
            &data["manifest"],
            change,
            key,
            parent,
            generation,
            &lode_supervisor::now_timestamp(),
        )?;
        let reply = client.post(&commits, Some(&manifest))?;
        if reply.ok() {
            writeln!(
                out,
                "publish: committed {} (generation {generation})",
                &id[..12]
            )?;
            return Ok(());
        }
        if !reply.is(409, "COMMIT_CONFLICT") || attempt == ATTEMPTS {
            return Err(reply.refused(&format!("commit to {package}")));
        }
        writeln!(
            out,
            "publish: the manifest moved on; building on the new head"
        )?;
        std::thread::sleep(jitter(client.retry_pause, attempt));
    }
    unreachable!("the last attempt returns")
}

/// A pause that grows with `attempt`, spread so racing publishers part ways.
fn jitter(base: Duration, attempt: u32) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    base.mul_f64(f64::from(attempt) * (0.5 + f64::from(nanos % 1000) / 1000.0))
}
