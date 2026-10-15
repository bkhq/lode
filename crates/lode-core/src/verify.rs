//! Integrity (sha256) + publisher identity (ed25519 / ECDSA) verification.
//!
//! These are the runtime primitives the loader uses to verify a downloaded
//! artifact. The publisher-side `keygen` / `sign` / `verify` / `manifest`
//! commands (exposed under `lode-cli`) reuse them — see `lode::authoring`.
//!
//! Keys are distributed as base64, tagged with their [`Algorithm`]; an untagged
//! key is ed25519 (the default). A `key_id` is the first 16 hex chars of
//! `sha256(public_key)` over the key's canonical encoding. The signed message
//! binds an artifact's identity to its content digest; see [`artifact_message`].

use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use ed25519_dalek::Verifier as _;
use sha2::{Digest as _, Sha256};

/// The encoding lode emits for keys and signatures: base64url (RFC 4648 §5)
/// without padding.
///
/// Its alphabet (`A-Z a-z 0-9 - _`) is safe in URLs, filenames and shells, unlike
/// standard base64's `+`, `/` and `=`. Decoding goes through [`decode_b64`], which
/// also accepts the standard alphabet with or without padding, so keys, signatures
/// and manifests published before this encoding was adopted keep verifying
/// unchanged.
pub const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Decode base64 in either alphabet — standard (`+/`) or URL-safe (`-_`) — with
/// or without `=` padding, and with surrounding whitespace ignored.
///
/// The input is normalised onto the standard alphabet and decoded strictly (no
/// stray bits, no interior whitespace), so the two spellings of one value decode
/// identically and anything else is rejected.
pub fn decode_b64(text: &str) -> Result<Vec<u8>> {
    let normalised: String = text
        .trim()
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(normalised)
        .context("base64 decode")
}

/// Signature algorithm of a publisher key.
///
/// The algorithm is a property of the **trusted-key entry** (and of the key files
/// `keygen` writes), never of the manifest: a signature is always checked with the
/// algorithm the operator pinned for that key, so a catalog cannot pick a weaker or
/// mismatched primitive for a key it does not own. Unstated = ed25519.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// Raw 32-byte public key, 64-byte signature. The default when unstated.
    Ed25519,
    /// ECDSA over NIST P-256 with SHA-256: SEC1 public key (33-byte compressed is
    /// canonical; uncompressed accepted), fixed-size 64-byte `r || s` signature.
    EcdsaP256,
    /// ECDSA over NIST P-384 with SHA-384: SEC1 public key (49-byte compressed is
    /// canonical; uncompressed accepted), fixed-size 96-byte `r || s` signature.
    EcdsaP384,
}

impl Algorithm {
    /// Every supported algorithm, in the order `keygen --help` lists them.
    pub const ALL: [Self; 3] = [Self::Ed25519, Self::EcdsaP256, Self::EcdsaP384];

    /// The wire name (`ed25519`, `ecdsa-p256`, `ecdsa-p384`) used in trusted-key
    /// entries, key files and the manifest `alg` field.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ed25519 => "ed25519",
            Self::EcdsaP256 => "ecdsa-p256",
            Self::EcdsaP384 => "ecdsa-p384",
        }
    }

    /// Parse a wire name; `None` when unknown.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.name() == name)
    }
}

impl fmt::Display for Algorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Algorithm {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s).ok_or_else(|| {
            let known: Vec<&str> = Self::ALL.iter().map(|a| a.name()).collect();
            anyhow::anyhow!(
                "unknown signature algorithm {s:?} (expected one of: {})",
                known.join(", ")
            )
        })
    }
}

/// A decoded publisher public key, tagged with its [`Algorithm`].
#[derive(Debug, Clone)]
pub enum PublicKey {
    /// An ed25519 key.
    Ed25519(ed25519_dalek::VerifyingKey),
    /// An ECDSA P-256 key.
    EcdsaP256(p256::ecdsa::VerifyingKey),
    /// An ECDSA P-384 key.
    EcdsaP384(p384::ecdsa::VerifyingKey),
}

impl PublicKey {
    /// Decode raw public-key bytes under `alg` (raw 32 bytes for ed25519, SEC1
    /// compressed or uncompressed for ECDSA). Bytes of the wrong shape for `alg`
    /// are an error — a key never silently changes algorithm.
    pub fn decode(alg: Algorithm, bytes: &[u8]) -> Result<Self> {
        Ok(match alg {
            Algorithm::Ed25519 => {
                let raw: &[u8; 32] = bytes.try_into().map_err(|_| {
                    anyhow::anyhow!("expected 32-byte ed25519 key, got {} bytes", bytes.len())
                })?;
                Self::Ed25519(
                    ed25519_dalek::VerifyingKey::from_bytes(raw)
                        .context("invalid ed25519 public key")?,
                )
            }
            Algorithm::EcdsaP256 => Self::EcdsaP256(
                p256::ecdsa::VerifyingKey::from_sec1_bytes(bytes)
                    .context("invalid ecdsa-p256 public key (expected SEC1 bytes)")?,
            ),
            Algorithm::EcdsaP384 => Self::EcdsaP384(
                p384::ecdsa::VerifyingKey::from_sec1_bytes(bytes)
                    .context("invalid ecdsa-p384 public key (expected SEC1 bytes)")?,
            ),
        })
    }

    /// The algorithm of this key.
    pub const fn algorithm(&self) -> Algorithm {
        match self {
            Self::Ed25519(_) => Algorithm::Ed25519,
            Self::EcdsaP256(_) => Algorithm::EcdsaP256,
            Self::EcdsaP384(_) => Algorithm::EcdsaP384,
        }
    }

    /// The canonical encoding — raw 32 bytes (ed25519) or the SEC1 *compressed*
    /// point (ECDSA): what [`key_id`] hashes and what `keygen` prints.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Ed25519(k) => k.to_bytes().to_vec(),
            Self::EcdsaP256(k) => k.to_sec1_point(true).as_bytes().to_vec(),
            Self::EcdsaP384(k) => k.to_sec1_point(true).as_bytes().to_vec(),
        }
    }

    /// `key_id` of this key (over its canonical encoding, so an ECDSA key pinned
    /// in uncompressed form derives the same id `keygen` printed).
    pub fn key_id(&self) -> String {
        key_id(&self.to_bytes())
    }

    /// Verify a raw signature over `message`. A signature of the wrong shape for
    /// this key's algorithm simply fails to verify.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        match self {
            Self::Ed25519(k) => ed25519_dalek::Signature::from_slice(signature)
                .is_ok_and(|sig| k.verify(message, &sig).is_ok()),
            Self::EcdsaP256(k) => p256::ecdsa::Signature::from_slice(signature)
                .is_ok_and(|sig| k.verify(message, &sig).is_ok()),
            Self::EcdsaP384(k) => p384::ecdsa::Signature::from_slice(signature)
                .is_ok_and(|sig| k.verify(message, &sig).is_ok()),
        }
    }
}

/// Identity of a single release asset, used to build the signed message.
///
/// `name`
/// is the **asset filename** — the field binding *which* artifact a signature
/// authorises (§1). The optional `run`/`exec` launch overrides a manifest may
/// publish are bound too (they decide what the loader executes, so a signature
/// must cover them); `format`/`url` are derived from the filename or are
/// operator-local, so they are deliberately not part of this struct.
#[derive(Debug)]
pub struct Artifact<'a> {
    /// The app the asset belongs to: the manifest `name` / `[global].app`.
    /// Signed, so an asset of one app is not valid for another under the same key.
    pub app: &'a str,
    /// The asset filename (selection key + signed identity).
    pub name: &'a str,
    /// The version id.
    pub version: &'a str,
    /// On-disk path to hash (authoring only); not part of the signed message.
    /// Read only by `authoring` (cli); under `--features engine` the artifact is
    /// built to verify a download, never to hash a local file, so it is
    /// constructed-but-unread there.
    pub path: &'a str,
    /// Manifest-published bare-run launch override (signed; empty when absent).
    pub run: Option<&'a str>,
    /// Manifest-published passthrough launch override (signed; empty when absent).
    pub exec: Option<&'a str>,
}

/// Lowercase hex encoding.
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// `key_id` = first 16 hex chars of `sha256(public_key)` (the key's canonical
/// encoding — see [`PublicKey::to_bytes`]).
pub fn key_id(public: &[u8]) -> String {
    let digest = Sha256::digest(public);
    to_hex(&digest)[..16].to_owned()
}

/// Canonical signed message for an asset (design §1).
///
/// Binds the asset's identity (`app` = the app name, `name` = the asset filename,
/// plus `version`) to its
/// content digest and its optional `run`/`exec` launch overrides (empty fields when
/// absent — they steer what the loader executes, so a tampered override must fail
/// verification).
///
/// Exact bytes (UTF-8,
/// `\n` separated, no trailing newline) — must match the loader. `format`/`url`
/// are *not* bound: the filename's extension fixes the format and `url` is a
/// runtime concern (§1/§3). `run`/`exec` may not contain control characters
/// (rejected at manifest parse), so the field framing is unambiguous. The same
/// bytes are signed under every [`Algorithm`].
pub fn artifact_message(a: &Artifact<'_>, sha256_hex: &str) -> Vec<u8> {
    format!(
        "lode.artifact.v2\n{}\n{}\n{}\n{}\n{}\n{}",
        a.app,
        a.name,
        a.version,
        sha256_hex,
        a.run.unwrap_or(""),
        a.exec.unwrap_or("")
    )
    .into_bytes()
}

/// Stream a file through sha256 and return the lowercase hex digest.
pub fn sha256_hex_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// Verify a base64 signature over an artifact message against a trusted-key entry.
///
/// `public` is any form [`decode_trusted_key`] accepts (a bare base64 key is
/// ed25519). Errors when the entry itself is malformed; `Ok(false)` when the
/// signature does not validate (including one of the wrong shape for the key).
pub fn verify_signature(public: &str, message: &[u8], sig_b64: &str) -> Result<bool> {
    let key = decode_trusted_key(public)?;
    let sig_bytes = decode_b64(sig_b64).context("decode signature")?;
    Ok(key.verify(message, &sig_bytes))
}

/// Lowercase-hex sha256 of an in-memory buffer. Companion to
/// [`sha256_hex_file`] for callers that already hold the bytes (the
/// runtime-download path and unit tests). Reuses the same `to_hex` encoding.
#[allow(dead_code)] // in-memory digest helper; used by the download path + tests
pub fn sha256_hex(bytes: &[u8]) -> String {
    to_hex(&Sha256::digest(bytes))
}

/// Verify an asset's signature against a set of trusted keys.
///
/// Uses the exact §1 canonical message
/// (`lode.artifact.v2\n{app}\n{name}\n{version}\n{sha256}\n{run}\n{exec}`, where
/// `app` is the app name, `name` is the asset filename and `run`/`exec` are the asset's optional launch
/// overrides, empty when absent).
///
/// Each entry in `trusted_keys` is a trusted-key entry (see
/// [`decode_trusted_key`]: `[<alg>:][<key_id>:]<base64>`, or the whitespace file
/// form); the signature is checked with **that entry's** algorithm. Succeeds as
/// soon as any trusted key validates the signature; errors if none do. The
/// integrity (sha256) check is the caller's responsibility (see [`crate::install`]).
#[allow(clippy::too_many_arguments)]
pub fn verify_artifact_sig(
    app: &str,
    name: &str,
    version: &str,
    sha256_hex: &str,
    run: Option<&str>,
    exec: Option<&str>,
    sig_b64: &str,
    trusted_keys: &[String],
) -> Result<()> {
    if trusted_keys.is_empty() {
        bail!("no trusted keys configured to verify the artifact signature");
    }
    let artifact = Artifact {
        app,
        name,
        version,
        path: "",
        run,
        exec,
    };
    let message = artifact_message(&artifact, sha256_hex);
    for key in trusted_keys {
        // A malformed key (bad base64 / wrong shape for its algorithm) is skipped,
        // not fatal — another configured key (e.g. during rotation) may still
        // validate.
        if matches!(verify_signature(key, &message, sig_b64), Ok(true)) {
            return Ok(());
        }
    }
    bail!("artifact signature did not match any trusted key");
}

/// Canonical signed message for a manifest catalog (design §6).
///
/// Binds the manifest's identity (`name` + `key_id`) to a deterministic,
/// `sig`-free serialization of its catalog (`canonical` — built by
/// [`crate::manifest::Manifest::signing_message`] from the sorted channel/version
/// maps).
///
/// Exact bytes (UTF-8, `\n`-separated, no trailing
/// newline beyond what `canonical` carries): `lode.manifest.v1\n{name}\n{key_id}\n{canonical}`.
/// The publisher (`lode-cli manifest-sign`) and the loader MUST produce identical
/// bytes; both go through `signing_message` so they always do.
pub fn manifest_message(name: &str, key_id: &str, canonical: &str) -> Vec<u8> {
    format!("lode.manifest.v1\n{name}\n{key_id}\n{canonical}").into_bytes()
}

/// The exact bytes signed/verified for a manifest's `expires_sig`.
///
/// The catalog framing of [`manifest_message`] under its own domain tag, with the
/// expiry (Unix seconds) bound in:
/// `lode.manifest-expiry.v1\n{name}\n{key_id}\n{expires}\n{canonical}`.
///
/// It is a separate signature rather than a field of [`manifest_message`] so that
/// a loader predating expiry still verifies the unchanged `sig` of the same file.
pub fn manifest_expiry_message(name: &str, key_id: &str, expires: u64, canonical: &str) -> Vec<u8> {
    format!("lode.manifest-expiry.v1\n{name}\n{key_id}\n{expires}\n{canonical}").into_bytes()
}

/// Verify a manifest's top-level signature over its canonical message against a
/// set of trusted keys.
///
/// The manifest's declared `key_id` selects the
/// preferred key; when it is `None` or matches no trusted entry, every trusted key
/// is tried (covering an absent id or a rotation where the id differs). Succeeds as
/// soon as any key validates the signature; errors if none do. Entry forms are the
/// same as for [`verify_artifact_sig`]; each is checked with its own algorithm.
pub fn verify_manifest_sig(
    trusted_keys: &[String],
    key_id: Option<&str>,
    message: &[u8],
    sig_b64: &str,
) -> Result<()> {
    if trusted_keys.is_empty() {
        bail!("no trusted keys configured to verify the manifest signature");
    }
    // Prefer the key whose id matches the manifest's declared `key_id`.
    if let Some(want) = key_id
        && let Some(entry) = trusted_keys
            .iter()
            .find(|e| trusted_key_id(e).as_deref() == Some(want))
        && matches!(verify_signature(entry, message, sig_b64), Ok(true))
    {
        return Ok(());
    }
    // Fall back to trying every trusted key (a missing/unmatched id, or rotation).
    for entry in trusted_keys {
        if matches!(verify_signature(entry, message, sig_b64), Ok(true)) {
            return Ok(());
        }
    }
    bail!("manifest signature did not match any trusted key");
}

/// The `key_id` of a trusted-key entry, derived from its public component, or
/// `None` when the entry is malformed (bad base64 / wrong shape for its algorithm).
fn trusted_key_id(entry: &str) -> Option<String> {
    decode_trusted_key(entry).ok().map(|key| key.key_id())
}

/// Split a trusted-key entry into its algorithm and base64 public component.
///
/// Fields are separated by `:` or whitespace (neither occurs in base64, so the
/// split is unambiguous):
/// - `<base64>` — ed25519;
/// - `<key_id>:<base64>` / `<key_id> <base64>` — ed25519 (the pre-`alg` forms);
/// - `<alg>:<base64>` — `alg` without an id;
/// - `<alg>:<key_id>:<base64>` / `<alg>:<key_id> <base64>` — the full form
///   `keygen` prints for non-ed25519 keys.
///
/// The `key_id` field is informational: the id used for matching is always
/// re-derived from the public component.
fn parse_trusted_key(entry: &str) -> Result<(Algorithm, &str)> {
    let mut fields = entry
        .split(|c: char| c == ':' || c.is_whitespace())
        .filter(|f| !f.is_empty());
    let parsed = match (fields.next(), fields.next(), fields.next()) {
        (Some(key), None, None) => (Algorithm::Ed25519, key),
        // Two fields: `<alg>:<key>` when the first names an algorithm, else the
        // legacy `<key_id>:<key>`.
        (Some(first), Some(key), None) => {
            (Algorithm::parse(first).unwrap_or(Algorithm::Ed25519), key)
        }
        (Some(alg), Some(_key_id), Some(key)) => (alg.parse::<Algorithm>()?, key),
        _ => bail!("empty trusted-key entry"),
    };
    if fields.next().is_some() {
        bail!("malformed trusted-key entry (expected `[<alg>:][<key_id>:]<base64>`)");
    }
    Ok(parsed)
}

/// Decode a trusted-key entry into a [`PublicKey`] carrying the algorithm the
/// operator pinned for it.
///
/// Accepted forms (fields split on `:` or whitespace, neither of which occurs in
/// base64): `<base64>` and `<key_id>:<base64>` / `<key_id> <base64>` (ed25519, the
/// pre-`alg` forms); `<alg>:<base64>`; `<alg>:<key_id>:<base64>` /
/// `<alg>:<key_id> <base64>` (what `keygen` prints for non-ed25519 keys). The
/// `key_id` field is informational — the id used for matching is re-derived from
/// the public component.
pub fn decode_trusted_key(entry: &str) -> Result<PublicKey> {
    let (alg, b64) = parse_trusted_key(entry)?;
    let bytes = decode_b64(b64)?;
    PublicKey::decode(alg, &bytes)
}

/// Decode a base64 32-byte key (an ed25519 public key or private seed).
pub fn decode_key(b64: &str) -> Result<[u8; 32]> {
    let bytes = decode_b64(b64)?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 32-byte key, got {} bytes", bytes.len()))?;
    Ok(arr)
}

/// The full trusted-key entry for `key`: `<alg>:<key_id>:<base64url>`.
pub fn key_entry(key: &PublicKey) -> String {
    format!(
        "{}:{}:{}",
        key.algorithm(),
        key.key_id(),
        B64.encode(key.to_bytes())
    )
}

/// Canonical signed message for a [`Delegation`]:
/// `lode.delegation.v1\n{name}\n{alg}\n{sub_key_base64url}\n{not_before}\n{not_after}`.
///
/// The sub-key's algorithm is bound, so the root pins it, not the manifest.
pub fn delegation_message(
    name: &str,
    alg: Algorithm,
    key_b64: &str,
    not_before: u64,
    not_after: u64,
) -> Vec<u8> {
    format!("lode.delegation.v1\n{name}\n{alg}\n{key_b64}\n{not_before}\n{not_after}").into_bytes()
}

/// A root-signed certificate that lets a sub-key sign for one manifest `name`
/// during `[not_before, not_after)` (Unix seconds).
///
/// Only trusted (root) keys can issue one; a sub-key cannot delegate further.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Delegation {
    /// The sub-key, as a trusted-key entry (`[<alg>:][<key_id>:]<base64>`).
    pub key: String,
    /// The manifest `name` the sub-key may sign for.
    pub name: String,
    /// Start of validity (inclusive).
    pub not_before: u64,
    /// End of validity (exclusive).
    pub not_after: u64,
    /// Advisory `key_id` of the issuing root key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// Root signature over [`Delegation::signing_message`].
    #[serde(default)]
    pub sig: String,
}

impl Delegation {
    /// The decoded sub-key.
    pub fn sub_key(&self) -> Result<PublicKey> {
        decode_trusted_key(&self.key).context("delegation key")
    }

    /// The exact bytes the root signs ([`delegation_message`]).
    pub fn signing_message(&self) -> Result<Vec<u8>> {
        let key = self.sub_key()?;
        Ok(delegation_message(
            &self.name,
            key.algorithm(),
            &B64.encode(key.to_bytes()),
            self.not_before,
            self.not_after,
        ))
    }

    /// Check the delegation for `name` at `now` against the root keys and the
    /// revoked sub-key ids; returns the sub-key's trusted-key entry.
    pub fn validate(
        &self,
        roots: &[String],
        name: &str,
        now: u64,
        revoked: &std::collections::BTreeSet<String>,
    ) -> Result<String> {
        if self.name != name {
            bail!("delegation is for {:?}, not {name:?}", self.name);
        }
        let message = self.signing_message()?;
        verify_manifest_sig(roots, self.issuer.as_deref(), &message, &self.sig)
            .map_err(|_| anyhow::anyhow!("delegation is not signed by a trusted key"))?;
        if now < self.not_before {
            bail!(
                "delegation is not valid yet (not_before={}, now={now})",
                self.not_before
            );
        }
        if now >= self.not_after {
            bail!(
                "delegation expired (not_after={}, now={now})",
                self.not_after
            );
        }
        let key = self.sub_key()?;
        let id = key.key_id();
        if revoked.contains(&id) {
            bail!("delegated key {id} is revoked");
        }
        Ok(key_entry(&key))
    }
}

/// The keys a signature may verify against: the trusted `roots`, plus the
/// delegated sub-key when `delegation` validates for `name` at `now`.
///
/// The second value is why a present delegation was rejected, for error messages.
/// With no roots nothing is trusted, delegation included.
pub fn effective_keys(
    roots: &[String],
    delegation: Option<&Delegation>,
    name: &str,
    now: u64,
    revoked: &std::collections::BTreeSet<String>,
) -> (Vec<String>, Option<String>) {
    let mut keys = roots.to_vec();
    let Some(delegation) = delegation else {
        return (keys, None);
    };
    if roots.is_empty() {
        return (keys, None);
    }
    match delegation.validate(roots, name, now, revoked) {
        Ok(entry) => {
            keys.push(entry);
            (keys, None)
        }
        Err(e) => (keys, Some(format!("{e:#}"))),
    }
}

/// Schema id of a [`RevocationList`].
pub const REVOCATIONS_SCHEMA: &str = "lode-revocations/v1";

/// Canonical signed message for a [`RevocationList`]:
/// `lode.revocations.v1\n{key_id}\n{issued_at}` then each revoked id (sorted,
/// deduplicated) on its own line.
pub fn revocation_message(key_id: &str, issued_at: u64, revoked: &[String]) -> Vec<u8> {
    let ids: std::collections::BTreeSet<&str> = revoked.iter().map(String::as_str).collect();
    let mut out = format!("lode.revocations.v1\n{key_id}\n{issued_at}");
    for id in ids {
        out.push('\n');
        out.push_str(id);
    }
    out.into_bytes()
}

/// A signed list of revoked delegated sub-key ids.
///
/// Signed by a trusted key, or by a valid delegated sub-key that carries its
/// [`Delegation`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RevocationList {
    /// [`REVOCATIONS_SCHEMA`].
    pub schema: String,
    /// Issue time (Unix seconds); a list never replaces a newer one.
    pub issued_at: u64,
    /// Revoked sub-key ids.
    #[serde(default)]
    pub revoked: Vec<String>,
    /// Id of the signing key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// The signer's delegation, when signed by a sub-key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Delegation>,
    /// Signature over [`RevocationList::signing_message`].
    #[serde(default)]
    pub sig: String,
}

impl RevocationList {
    /// The exact bytes signed ([`revocation_message`]).
    pub fn signing_message(&self) -> Vec<u8> {
        revocation_message(
            self.key_id.as_deref().unwrap_or(""),
            self.issued_at,
            &self.revoked,
        )
    }

    /// Verify the list for app `name` at `now` against the root keys; a sub-key
    /// signer must hold a valid, unrevoked delegation.
    pub fn verify(
        &self,
        roots: &[String],
        name: &str,
        now: u64,
        revoked: &std::collections::BTreeSet<String>,
    ) -> Result<()> {
        if self.schema != REVOCATIONS_SCHEMA {
            bail!(
                "unsupported revocation list schema {:?} (expected {REVOCATIONS_SCHEMA:?})",
                self.schema
            );
        }
        let (keys, rejected) = effective_keys(roots, self.delegation.as_ref(), name, now, revoked);
        verify_manifest_sig(
            &keys,
            self.key_id.as_deref(),
            &self.signing_message(),
            &self.sig,
        )
        .map_err(|e| {
            rejected.map_or_else(
                || anyhow::anyhow!("revocation list: {e}"),
                |why| anyhow::anyhow!("revocation list: {e} (delegation rejected: {why})"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    #[test]
    fn sign_then_verify_roundtrip() {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).unwrap();
        let signing = SigningKey::from_bytes(&seed);
        let public_b64 = B64.encode(signing.verifying_key().to_bytes());

        let artifact = Artifact {
            app: "myapp",
            name: "app-linux-x86_64.tar.gz",
            version: "1.0.0",
            path: "",
            run: None,
            exec: None,
        };
        let msg = artifact_message(&artifact, "abc123");
        let sig_b64 = B64.encode(signing.sign(&msg).to_bytes());

        assert!(verify_signature(&public_b64, &msg, &sig_b64).unwrap());

        // Tampered message must fail.
        let bad = artifact_message(&artifact, "deadbeef");
        assert!(!verify_signature(&public_b64, &bad, &sig_b64).unwrap());

        // The run/exec launch overrides are bound: adding either changes the bytes.
        let with_run = artifact_message(
            &Artifact {
                run: Some("./app serve"),
                ..artifact
            },
            "abc123",
        );
        assert!(!verify_signature(&public_b64, &with_run, &sig_b64).unwrap());
    }

    #[test]
    fn artifact_message_binds_run_and_exec_unambiguously() {
        let base = Artifact {
            app: "myapp",
            name: "app.tar.gz",
            version: "1.0.0",
            path: "",
            run: None,
            exec: None,
        };
        // Absent overrides serialize as empty fields (stable framing).
        assert_eq!(
            artifact_message(&base, "abc"),
            b"lode.artifact.v2\nmyapp\napp.tar.gz\n1.0.0\nabc\n\n".to_vec()
        );
        // run vs exec occupy distinct fields: the same string in the other slot
        // yields different bytes (no swap confusion).
        let run_only = artifact_message(
            &Artifact {
                run: Some("./app"),
                ..base
            },
            "abc",
        );
        let exec_only = artifact_message(
            &Artifact {
                exec: Some("./app"),
                ..base
            },
            "abc",
        );
        assert_ne!(run_only, exec_only);
        assert_eq!(
            run_only,
            b"lode.artifact.v2\nmyapp\napp.tar.gz\n1.0.0\nabc\n./app\n".to_vec()
        );
    }

    #[test]
    fn key_id_is_stable_16_hex() {
        let id = key_id(&[7u8; 32]);
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // The empty-input sha256 is a well-known vector.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn verify_artifact_sig_happy_and_tampered() {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).unwrap();
        let signing = SigningKey::from_bytes(&seed);
        let public = signing.verifying_key().to_bytes();
        let id = key_id(&public);
        let public_b64 = B64.encode(public);

        let sha = "a97ad2265ae84cdeff1219b1c83db8e6f096e444c81f733bc93355f0fff368a1";
        let name = "myapp-linux-x86_64.tar.gz";
        let artifact = Artifact {
            app: "myapp",
            name,
            version: "1.5.0",
            path: "",
            run: None,
            exec: None,
        };
        let sig = B64.encode(signing.sign(&artifact_message(&artifact, sha)).to_bytes());

        // Fixed identity (name + version, no overrides); vary digest + key set.
        let check = |sha: &str, keys: &[String]| {
            verify_artifact_sig("myapp", name, "1.5.0", sha, None, None, &sig, keys)
        };

        // CLI/TOML form `key_id:base64` and bare base64 both validate.
        let keys_colon = vec![format!("{id}:{public_b64}")];
        let keys_bare = vec![public_b64.clone()];
        let keys_space = vec![format!("{id} {public_b64}")]; // file form
        let keys_tagged = vec![format!("ed25519:{id}:{public_b64}")]; // explicit alg
        for keys in [&keys_colon, &keys_bare, &keys_space, &keys_tagged] {
            assert!(check(sha, keys).is_ok());
        }

        // The digest is part of the message: changing it must fail.
        let bad_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef0";
        assert!(check(bad_sha, &keys_colon).is_err());

        // A signature is bound to the asset filename: the same bytes under a
        // different asset name must fail.
        assert!(
            verify_artifact_sig(
                "myapp",
                "other-asset.tar.gz",
                "1.5.0",
                sha,
                None,
                None,
                &sig,
                &keys_colon
            )
            .is_err()
        );
        // …and to the app: the same asset under another app name must fail.
        assert!(
            verify_artifact_sig(
                "other-app",
                name,
                "1.5.0",
                sha,
                None,
                None,
                &sig,
                &keys_colon
            )
            .is_err()
        );
        // …and under a different version.
        assert!(
            verify_artifact_sig("myapp", name, "9.9.9", sha, None, None, &sig, &keys_colon)
                .is_err()
        );
        // …and with an injected run/exec override the signer never saw.
        assert!(
            verify_artifact_sig(
                "myapp",
                name,
                "1.5.0",
                sha,
                Some("./evil"),
                None,
                &sig,
                &keys_colon
            )
            .is_err()
        );
        assert!(
            verify_artifact_sig(
                "myapp",
                name,
                "1.5.0",
                sha,
                None,
                Some("./evil"),
                &sig,
                &keys_colon
            )
            .is_err()
        );

        // No trusted keys → error (cannot establish identity).
        assert!(check(sha, &[]).is_err());

        // An untrusted key alone → error.
        let other = B64.encode(
            SigningKey::from_bytes(&[9u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        assert!(check(sha, &[other]).is_err());
    }

    /// One ECDSA key per curve, with its canonical (compressed) public key and
    /// trusted-key entry.
    fn ecdsa_p256() -> (p256::ecdsa::SigningKey, String) {
        let signing = p256::ecdsa::SigningKey::from_slice(&[7u8; 32]).unwrap();
        let public = PublicKey::EcdsaP256(*signing.verifying_key());
        let entry = format!(
            "ecdsa-p256:{}:{}",
            public.key_id(),
            B64.encode(public.to_bytes())
        );
        (signing, entry)
    }

    fn ecdsa_p384() -> (p384::ecdsa::SigningKey, String) {
        let signing = p384::ecdsa::SigningKey::from_slice(&[7u8; 48]).unwrap();
        let public = PublicKey::EcdsaP384(*signing.verifying_key());
        let entry = format!(
            "ecdsa-p384:{}:{}",
            public.key_id(),
            B64.encode(public.to_bytes())
        );
        (signing, entry)
    }

    #[test]
    fn ecdsa_artifact_sig_roundtrip_both_curves() {
        let sha = "a97ad2265ae84cdeff1219b1c83db8e6f096e444c81f733bc93355f0fff368a1";
        let name = "myapp-linux-x86_64.tar.gz";
        let artifact = Artifact {
            app: "myapp",
            name,
            version: "1.5.0",
            path: "",
            run: Some("./myapp serve"),
            exec: None,
        };
        let message = artifact_message(&artifact, sha);

        let (p256_key, p256_entry) = ecdsa_p256();
        let p256_sig: p256::ecdsa::Signature = p256_key.sign(&message);
        let p256_sig = B64.encode(p256_sig.to_bytes());
        let (p384_key, p384_entry) = ecdsa_p384();
        let p384_sig: p384::ecdsa::Signature = p384_key.sign(&message);
        let p384_sig = B64.encode(p384_sig.to_bytes());

        for (sig, entry) in [(&p256_sig, &p256_entry), (&p384_sig, &p384_entry)] {
            let keys = vec![entry.clone()];
            let check = |sha: &str, run: Option<&str>| {
                verify_artifact_sig("myapp", name, "1.5.0", sha, run, None, sig, &keys)
            };
            assert!(check(sha, Some("./myapp serve")).is_ok());
            // The same bindings as ed25519: digest and launch override are covered.
            assert!(check("00", Some("./myapp serve")).is_err());
            assert!(check(sha, Some("./evil")).is_err());
        }

        // Signatures do not cross curves, even though both are `r || s`.
        assert!(
            verify_artifact_sig(
                "myapp",
                name,
                "1.5.0",
                sha,
                Some("./myapp serve"),
                None,
                &p384_sig,
                std::slice::from_ref(&p256_entry)
            )
            .is_err()
        );
        // A mixed trust set (ed25519 + ECDSA) verifies with whichever key signed.
        let ed = B64.encode(
            SigningKey::from_bytes(&[9u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        assert!(
            verify_artifact_sig(
                "myapp",
                name,
                "1.5.0",
                sha,
                Some("./myapp serve"),
                None,
                &p256_sig,
                &[ed, p384_entry, p256_entry]
            )
            .is_ok()
        );
    }

    #[test]
    fn ecdsa_manifest_sig_roundtrip() {
        let (signing, entry) = ecdsa_p256();
        let id = decode_trusted_key(&entry).unwrap().key_id();
        let canonical = "channel\tstable\t1.0.0\nversion\t1.0.0\nasset\tapp.tar.gz\tabc\t\t\n";
        let message = manifest_message("myapp", &id, canonical);
        let sig: p256::ecdsa::Signature = signing.sign(&message);
        let sig = B64.encode(sig.to_bytes());

        let trusted = vec![entry];
        assert!(verify_manifest_sig(&trusted, Some(&id), &message, &sig).is_ok());
        assert!(verify_manifest_sig(&trusted, None, &message, &sig).is_ok());
        let tampered = manifest_message("myapp", &id, "channel\tstable\t2.0.0\n");
        assert!(verify_manifest_sig(&trusted, Some(&id), &tampered, &sig).is_err());
    }

    /// The algorithm comes from the trusted entry, never from the signature: key
    /// bytes relabelled under another algorithm are rejected at decode, so a
    /// signature can only ever be checked with the primitive the operator pinned.
    #[test]
    fn algorithm_is_bound_to_the_trusted_entry() {
        let ed_public = B64.encode(
            SigningKey::from_bytes(&[9u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        let (_, p256_entry) = ecdsa_p256();
        let (_, p256_b64) = parse_trusted_key(&p256_entry).unwrap();

        // 32 raw ed25519 bytes are not a SEC1 point; 33 SEC1 bytes are not an
        // ed25519 key. Both relabellings fail to decode (and so never verify).
        assert!(decode_trusted_key(&format!("ecdsa-p256:{ed_public}")).is_err());
        assert!(decode_trusted_key(&format!("ecdsa-p384:{ed_public}")).is_err());
        assert!(decode_trusted_key(&format!("ed25519:{p256_b64}")).is_err());
        assert!(decode_trusted_key(&format!("ecdsa-p384:{p256_b64}")).is_err());
        // The untagged form is ed25519 — an ECDSA key needs its tag.
        assert!(decode_trusted_key(p256_b64).is_err());
        assert!(
            verify_signature(
                &format!("ecdsa-p256:{ed_public}"),
                b"m",
                &B64.encode([0u8; 64])
            )
            .is_err()
        );
    }

    #[test]
    fn parse_trusted_key_accepts_all_forms() {
        let ok = |entry: &str, alg: Algorithm| {
            assert_eq!(
                parse_trusted_key(entry).unwrap(),
                (alg, "KEYDATA"),
                "{entry}"
            );
        };
        ok("KEYDATA", Algorithm::Ed25519);
        ok("  KEYDATA  ", Algorithm::Ed25519);
        ok("abc123:KEYDATA", Algorithm::Ed25519);
        ok("abc123 KEYDATA", Algorithm::Ed25519);
        ok("ed25519:abc123:KEYDATA", Algorithm::Ed25519);
        ok("ed25519:KEYDATA", Algorithm::Ed25519);
        ok("ecdsa-p256:abc123:KEYDATA", Algorithm::EcdsaP256);
        ok("ecdsa-p256:abc123 KEYDATA", Algorithm::EcdsaP256);
        ok("ecdsa-p256:KEYDATA", Algorithm::EcdsaP256);
        ok("ecdsa-p384:abc123:KEYDATA", Algorithm::EcdsaP384);
        // A three-field entry must name a known algorithm; more fields is malformed.
        assert!(parse_trusted_key("rsa-pss:abc123:KEYDATA").is_err());
        assert!(parse_trusted_key("ed25519:abc123:KEYDATA:extra").is_err());
        assert!(parse_trusted_key("").is_err());
    }

    #[test]
    fn ecdsa_key_id_is_canonical_over_compressed_point() {
        let (signing, entry) = ecdsa_p256();
        let compressed = decode_trusted_key(&entry).unwrap();
        let uncompressed = B64.encode(signing.verifying_key().to_sec1_point(false).as_bytes());
        let from_uncompressed = decode_trusted_key(&format!("ecdsa-p256:{uncompressed}")).unwrap();
        assert_eq!(compressed.to_bytes().len(), 33);
        assert_eq!(compressed.key_id(), from_uncompressed.key_id());
        assert_eq!(compressed.to_bytes(), from_uncompressed.to_bytes());
    }

    #[test]
    fn algorithm_names_roundtrip() {
        for alg in Algorithm::ALL {
            assert_eq!(alg.name().parse::<Algorithm>().unwrap(), alg);
            assert_eq!(alg.to_string(), alg.name());
        }
        assert!("ECDSA-P256".parse::<Algorithm>().is_err());
        assert!(Algorithm::parse("hmac-sha256").is_none());
    }

    #[test]
    fn manifest_expiry_message_binds_the_expiry_under_its_own_domain() {
        let msg = manifest_expiry_message("myapp", "deadbeefdeadbeef", 1_800_000_000, "c\n");
        assert_eq!(
            msg,
            b"lode.manifest-expiry.v1\nmyapp\ndeadbeefdeadbeef\n1800000000\nc\n".to_vec()
        );
        // Never the bytes of the catalog signature, whatever the expiry.
        assert_ne!(msg, manifest_message("myapp", "deadbeefdeadbeef", "c\n"));
        assert_ne!(
            msg,
            manifest_expiry_message("myapp", "deadbeefdeadbeef", 1_800_000_001, "c\n")
        );
    }

    #[test]
    fn manifest_message_is_stable_and_excludes_sig() {
        // The bytes are a fixed, reproducible function of (name, key_id, canonical).
        let msg = manifest_message("myapp", "deadbeefdeadbeef", "channel\tstable\t1.0.0\n");
        assert_eq!(
            msg,
            b"lode.manifest.v1\nmyapp\ndeadbeefdeadbeef\nchannel\tstable\t1.0.0\n".to_vec()
        );
        // A different catalog produces different bytes (so a tampered catalog fails).
        let other = manifest_message("myapp", "deadbeefdeadbeef", "channel\tstable\t9.9.9\n");
        assert_ne!(msg, other);
    }

    #[test]
    fn verify_manifest_sig_happy_tampered_and_wrong_key() {
        let signing = SigningKey::from_bytes(&[11u8; 32]);
        let public = signing.verifying_key().to_bytes();
        let id = key_id(&public);
        let public_b64 = B64.encode(public);

        let canonical = "channel\tstable\t1.0.0\nversion\t1.0.0\nasset\tapp-linux.tar.gz\tabc\n";
        let message = manifest_message("myapp", &id, canonical);
        let sig = B64.encode(signing.sign(&message).to_bytes());

        let trusted = vec![format!("{id}:{public_b64}")];
        // Happy path: id selects the matching key and it validates.
        assert!(verify_manifest_sig(&trusted, Some(&id), &message, &sig).is_ok());
        // key_id None still succeeds via the try-all fallback.
        assert!(verify_manifest_sig(&trusted, None, &message, &sig).is_ok());

        // Tampered message (catalog changed) → fail.
        let tampered = manifest_message("myapp", &id, "channel\tstable\t2.0.0\n");
        assert!(verify_manifest_sig(&trusted, Some(&id), &tampered, &sig).is_err());

        // Wrong key (an untrusted signer) → fail, even though its id is advertised.
        let attacker = SigningKey::from_bytes(&[12u8; 32]);
        let attacker_pub = attacker.verifying_key().to_bytes();
        let attacker_id = key_id(&attacker_pub);
        let attacker_msg = manifest_message("myapp", &attacker_id, canonical);
        let attacker_sig = B64.encode(attacker.sign(&attacker_msg).to_bytes());
        assert!(
            verify_manifest_sig(&trusted, Some(&attacker_id), &attacker_msg, &attacker_sig)
                .is_err()
        );

        // No trusted keys → error (cannot establish identity).
        assert!(verify_manifest_sig(&[], Some(&id), &message, &sig).is_err());
    }

    #[test]
    fn trusted_key_id_derives_from_public() {
        let public = SigningKey::from_bytes(&[7u8; 32])
            .verifying_key()
            .to_bytes();
        let id = key_id(&public);
        let public_b64 = B64.encode(public);
        // All entry forms resolve to the same derived id.
        assert_eq!(
            trusted_key_id(&format!("ignored:{public_b64}")).as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            trusted_key_id(&format!("ignored {public_b64}")).as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            trusted_key_id(&format!("ed25519:ignored:{public_b64}")).as_deref(),
            Some(id.as_str())
        );
        assert_eq!(trusted_key_id(&public_b64).as_deref(), Some(id.as_str()));
        // A malformed entry yields None rather than erroring.
        assert!(trusted_key_id("not-base64-!!!").is_none());
    }

    #[test]
    fn decode_b64_accepts_both_alphabets_with_or_without_padding() {
        // 256 bytes: every 6-bit group occurs, so both `+`/`/` (standard) and
        // `-`/`_` (url-safe) appear, and the length forces `==` padding.
        let bytes: Vec<u8> = (0..=255).collect();
        let standard = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert!(standard.contains('+') && standard.contains('/') && standard.ends_with("=="));
        let urlsafe = B64.encode(&bytes);
        assert!(!urlsafe.contains(['+', '/', '=']));
        assert!(urlsafe.contains('-') && urlsafe.contains('_'));

        assert_eq!(decode_b64(&standard).unwrap(), bytes);
        assert_eq!(decode_b64(standard.trim_end_matches('=')).unwrap(), bytes);
        assert_eq!(decode_b64(&urlsafe).unwrap(), bytes);
        assert_eq!(decode_b64(&format!("  {urlsafe}==\n")).unwrap(), bytes);
        // Garbage and interior whitespace are still rejected.
        assert!(decode_b64("not-base64-!!!").is_err());
        assert!(decode_b64("AB CD").is_err());
    }

    #[test]
    fn standard_base64_keys_and_signatures_still_verify() {
        // Entries, seeds and signatures produced before base64url was adopted
        // (standard alphabet, padded) verify exactly like the url-safe form, in
        // any mix.
        let standard = base64::engine::general_purpose::STANDARD;
        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let public = signing.verifying_key().to_bytes();
        let message = b"lode.artifact.v2\nmyapp\na.tar.gz\n1.0.0\nabc\n\n";
        let sig_standard = standard.encode(signing.sign(message).to_bytes());
        let sig_urlsafe = B64.encode(signing.sign(message).to_bytes());
        let entry_standard = format!("{}:{}", key_id(&public), standard.encode(public));
        let entry_urlsafe = format!("{}:{}", key_id(&public), B64.encode(public));
        for entry in [&entry_standard, &entry_urlsafe] {
            for sig in [&sig_standard, &sig_urlsafe] {
                assert!(
                    verify_signature(entry, message, sig).unwrap(),
                    "{entry} / {sig}"
                );
            }
        }
        assert_eq!(decode_key(&standard.encode(public)).unwrap(), public);
        assert_eq!(decode_key(&B64.encode(public)).unwrap(), public);
    }

    // --- delegation -------------------------------------------------------

    fn ed_entry(signing: &SigningKey) -> String {
        let public = PublicKey::Ed25519(signing.verifying_key());
        key_entry(&public)
    }

    /// A delegation for `name` of `sub` signed by `root`, valid over `[nb, na)`.
    fn delegate(root: &SigningKey, sub: &SigningKey, name: &str, nb: u64, na: u64) -> Delegation {
        let mut d = Delegation {
            key: ed_entry(sub),
            name: name.to_owned(),
            not_before: nb,
            not_after: na,
            issuer: Some(PublicKey::Ed25519(root.verifying_key()).key_id()),
            sig: String::new(),
        };
        d.sig = B64.encode(root.sign(&d.signing_message().unwrap()).to_bytes());
        d
    }

    #[test]
    fn delegation_message_binds_every_field() {
        let sub = SigningKey::from_bytes(&[2u8; 32]);
        let b64 = B64.encode(sub.verifying_key().to_bytes());
        assert_eq!(
            delegation_message("myapp", Algorithm::Ed25519, &b64, 10, 20),
            format!("lode.delegation.v1\nmyapp\ned25519\n{b64}\n10\n20").into_bytes()
        );
    }

    #[test]
    fn delegation_validates_only_when_root_signed_scoped_and_current() {
        let root = SigningKey::from_bytes(&[1u8; 32]);
        let sub = SigningKey::from_bytes(&[2u8; 32]);
        let roots = vec![ed_entry(&root)];
        let none = BTreeSet::new();
        let d = delegate(&root, &sub, "myapp", 100, 200);

        let entry = d.validate(&roots, "myapp", 150, &none).unwrap();
        assert_eq!(entry, ed_entry(&sub));
        // Window: not_before inclusive, not_after exclusive.
        assert!(d.validate(&roots, "myapp", 100, &none).is_ok());
        assert!(d.validate(&roots, "myapp", 99, &none).is_err());
        assert!(d.validate(&roots, "myapp", 200, &none).is_err());
        // Scope: another manifest name is refused.
        assert!(d.validate(&roots, "other", 150, &none).is_err());
        // An untrusted issuer (the sub-key itself, or any other key) is refused.
        assert!(d.validate(&[ed_entry(&sub)], "myapp", 150, &none).is_err());
        let self_signed = delegate(&sub, &sub, "myapp", 100, 200);
        assert!(self_signed.validate(&roots, "myapp", 150, &none).is_err());
        // Revoked sub-key.
        let revoked = BTreeSet::from([PublicKey::Ed25519(sub.verifying_key()).key_id()]);
        assert!(d.validate(&roots, "myapp", 150, &revoked).is_err());
        // Any tampered field breaks the root signature.
        for tampered in [
            Delegation {
                not_after: 900,
                ..d.clone()
            },
            Delegation {
                not_before: 0,
                ..d.clone()
            },
            Delegation {
                name: "x".to_owned(),
                ..d.clone()
            },
            Delegation {
                key: ed_entry(&SigningKey::from_bytes(&[5u8; 32])),
                ..d.clone()
            },
        ] {
            assert!(
                tampered
                    .validate(&roots, &tampered.name, 150, &none)
                    .is_err()
            );
        }
        // A wrong `issuer` hint still verifies through the try-all fallback.
        let hinted = Delegation {
            issuer: Some("0000000000000000".to_owned()),
            ..d
        };
        assert!(hinted.validate(&roots, "myapp", 150, &none).is_ok());
    }

    #[test]
    fn delegation_binds_the_sub_key_algorithm() {
        let root = SigningKey::from_bytes(&[1u8; 32]);
        let roots = vec![ed_entry(&root)];
        let (p256_signing, p256_entry) = ecdsa_p256();
        let mut d = Delegation {
            key: p256_entry,
            name: "myapp".to_owned(),
            not_before: 0,
            not_after: 10,
            issuer: None,
            sig: String::new(),
        };
        d.sig = B64.encode(root.sign(&d.signing_message().unwrap()).to_bytes());
        let entry = d.validate(&roots, "myapp", 5, &BTreeSet::new()).unwrap();
        assert_eq!(
            decode_trusted_key(&entry).unwrap().algorithm(),
            Algorithm::EcdsaP256
        );
        // The sub-key then verifies its own signatures under its pinned algorithm.
        let msg = b"payload";
        let sig: p256::ecdsa::Signature = p256_signing.sign(msg);
        assert!(verify_signature(&entry, msg, &B64.encode(sig.to_bytes())).unwrap());
    }

    #[test]
    fn effective_keys_adds_a_valid_sub_key_and_reports_a_bad_one() {
        let root = SigningKey::from_bytes(&[1u8; 32]);
        let sub = SigningKey::from_bytes(&[2u8; 32]);
        let roots = vec![ed_entry(&root)];
        let none = BTreeSet::new();
        let d = delegate(&root, &sub, "myapp", 100, 200);

        let (keys, err) = effective_keys(&roots, None, "myapp", 150, &none);
        assert_eq!(keys, roots);
        assert!(err.is_none());

        let (keys, err) = effective_keys(&roots, Some(&d), "myapp", 150, &none);
        assert_eq!(keys, vec![ed_entry(&root), ed_entry(&sub)]);
        assert!(err.is_none());

        let (keys, err) = effective_keys(&roots, Some(&d), "myapp", 250, &none);
        assert_eq!(keys, roots);
        assert!(err.unwrap().contains("expired"));

        // No roots configured: a delegation can never stand in for them.
        let (keys, _) = effective_keys(&[], Some(&d), "myapp", 150, &none);
        assert!(keys.is_empty());
    }

    // --- revocation list --------------------------------------------------

    fn revocation_list(
        signer: &SigningKey,
        delegation: Option<Delegation>,
        issued_at: u64,
        revoked: &[&str],
    ) -> RevocationList {
        let mut list = RevocationList {
            schema: REVOCATIONS_SCHEMA.to_owned(),
            issued_at,
            revoked: revoked.iter().map(|s| (*s).to_owned()).collect(),
            key_id: Some(PublicKey::Ed25519(signer.verifying_key()).key_id()),
            delegation,
            sig: String::new(),
        };
        list.sig = B64.encode(signer.sign(&list.signing_message()).to_bytes());
        list
    }

    #[test]
    fn revocation_message_is_order_independent() {
        let a = revocation_message("k", 7, &["b".to_owned(), "a".to_owned(), "b".to_owned()]);
        assert_eq!(a, b"lode.revocations.v1\nk\n7\na\nb".to_vec());
        assert_eq!(
            a,
            revocation_message("k", 7, &["a".to_owned(), "b".to_owned()])
        );
    }

    #[test]
    fn revocation_list_verifies_by_root_or_valid_sub_key() {
        let root = SigningKey::from_bytes(&[1u8; 32]);
        let sub = SigningKey::from_bytes(&[2u8; 32]);
        let other = SigningKey::from_bytes(&[3u8; 32]);
        let roots = vec![ed_entry(&root)];
        let none = BTreeSet::new();
        let sub_id = PublicKey::Ed25519(sub.verifying_key()).key_id();

        // Root-signed.
        let list = revocation_list(&root, None, 5, &["aaaa"]);
        assert!(list.verify(&roots, "myapp", 150, &none).is_ok());
        // Sub-key-signed, carrying its delegation.
        let d = delegate(&root, &sub, "myapp", 100, 200);
        let list = revocation_list(&sub, Some(d), 5, &["aaaa"]);
        assert!(list.verify(&roots, "myapp", 150, &none).is_ok());
        // ... but not once the delegation expired, or for another app.
        assert!(list.verify(&roots, "myapp", 250, &none).is_err());
        assert!(list.verify(&roots, "other", 150, &none).is_err());
        // ... and not by an already-revoked sub-key.
        let revoked = BTreeSet::from([sub_id]);
        assert!(list.verify(&roots, "myapp", 150, &revoked).is_err());
        // Untrusted signer, tampered content, wrong schema.
        assert!(
            revocation_list(&other, None, 5, &[])
                .verify(&roots, "myapp", 150, &none)
                .is_err()
        );
        let mut tampered = revocation_list(&root, None, 5, &["aaaa"]);
        tampered.revoked.clear();
        assert!(tampered.verify(&roots, "myapp", 150, &none).is_err());
        let mut schema = revocation_list(&root, None, 5, &[]);
        schema.schema = "lode-revocations/v9".to_owned();
        assert!(schema.verify(&roots, "myapp", 150, &none).is_err());
    }
}
