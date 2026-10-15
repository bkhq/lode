# 20261015-0103-delegated-signing-keys Delegated signing keys with revocation

- **status**: completed
- **createdAt**: 2026-10-15 01:03
- **approvedAt**: 2026-10-15 01:20
- **relatedTask**: 20261015-0103-delegated-signing-keys

## Context

Trust today is a flat key set. There is no delegation.

- `[trust].trusted_keys` / `trusted_keys_file` (`install::trusted_keys`) is the only
  trust anchor. Entries are `[<alg>:][<key_id>:]<base64>`; the algorithm is pinned on
  the entry (`verify::decode_trusted_key`).
- Catalog signature: `install::verify_manifest_at` -> `verify::verify_manifest_sig`
  over `Manifest::signing_message` (`sig`) and `Manifest::expiry_signing_message`
  (`expires_sig`). Both must be made directly by a trusted key.
- Artifact signature: `install::install` -> `verify_download` -> `verify_identity` ->
  `check_sig` -> `verify::verify_artifact_sig`. It receives only `(cfg, version,
  asset, sha)`, **not the manifest**, and may run long after the manifest was
  fetched (supervisor staged update, `engine` fetch/install split).
- Every install path calls `install::verify_manifest_identity` right after
  `manifest::fetch`: `engine.rs:180,583`, `commands/update.rs:33`,
  `supervise.rs:1380,1823,1992`, `self_update.rs:538`. This is the single place
  where per-fetch network work (a revocation-list refresh) can hook in.
- `Asset` is `Deserialize + Clone`, all-`pub` fields, carried by value into install.
- Rotation today: list old and new keys together, then switch (`release-keys.txt`).
- GitHub source (`manifest::map_release`) has no manifest-level slot; `lode-cli
  publish` delegates manifest generation to lode-server.

## Proposal

### 1. Delegation certificate

A root key (offline, listed in `trusted_keys`) signs a certificate for a sub-key:

```json
"delegation": {
  "key": "ed25519:<sub_key_id>:<base64url>",
  "name": "myapp",
  "not_before": 1760000000,
  "not_after": 1762592000,
  "issuer": "<root_key_id>",
  "sig": "<root signature, base64url>"
}
```

Signed message (new domain prefix, so it cannot collide with artifact/manifest
messages):

```text
lode.delegation.v1\n{name}\n{alg}\n{sub_key_canonical_b64}\n{not_before}\n{not_after}
```

- `key` uses the existing trusted-key entry syntax; the algorithm is bound into the
  signed message, so the sub-key's algorithm is pinned by the root, not by the manifest.
- `name` must equal the manifest `name` (which the loader already requires to equal
  `[global].app`); a delegation for one manifest cannot sign another.
- `issuer` is advisory (selects the root key first; try-all fallback, like `key_id`).
- Validity: `not_before <= now < not_after`, checked against the host clock with no
  skew allowance (publisher backdates `not_before` slightly).
- Only one level: a sub-key cannot issue further certificates.

### 2. Manifest carriage and verification

- `Manifest` gains `delegation: Option<Delegation>` (top-level only). `manifest::parse`
  copies it into each `Asset` (`#[serde(skip)] pub delegation: Option<Delegation>`), so the install-time
  artifact check has it without threading the manifest through `install()`.
- New `verify::effective_keys(trusted, delegation, name, now, revoked) -> Vec<String>`:
  the trusted root entries plus, if a cert is present and valid (root signature, app
  scope, window, not revoked), the sub-key entry. An invalid cert is an error only if
  the signature then fails to verify with the root keys alone (the error names the
  cert failure reason).
- `verify_manifest_at` and `verify_identity` use `effective_keys` instead of
  `trusted_keys` for the actual signature checks. Policy decisions
  (`require_signature`, `require_manifest`, "no keys configured") still look only at
  root keys, so a cert never substitutes for an operator's trust configuration.
- Root-signed manifests are unaffected (root keys remain in the effective set).
- Already-installed versions are not re-verified at launch, so cert expiry only gates
  new installs.

GitHub-releases source:

- The delegation ships as a release asset named `lode-delegation.json` (same JSON as
  the manifest field). `manifest::fetch_github` / `map_release` fetch it when it is in
  the release's asset list, through the same host and credential rules as `.sig`
  sidecars, and set `Manifest.delegation`; `parse`-time propagation then applies.
- A missing or unreachable `lode-delegation.json` leaves the release root-only (the
  sub-key signature then fails like any untrusted signature).
- GitHub releases have no catalog signature, so only per-asset `sig` (inline label or
  `<asset>.sig` sidecar) uses the delegation.
- `lode_core::self_update` reuses the same adapter, so embedders get delegation for
  their own self-update; lode's own `release-keys.txt` process is unchanged.

### 3. Revocation list (optional subscription)

Config: `[trust].revocation_url` (env `LODE_REVOCATION_URL`, flag
`--revocation-url`). Absent -> revocation relies on cert expiry alone.

Format, signed by a trusted root key **or by a valid delegated sub-key** (same rule as
an update):

```json
{
  "schema": "lode-revocations/v1",
  "issued_at": 1760500000,
  "revoked": ["<sub_key_id>", "..."],
  "key_id": "<signer_key_id>",
  "delegation": { "...": "optional, required when signed by a sub-key" },
  "sig": "<base64url>"
}
```

Message: `lode.revocations.v1\n{key_id}\n{issued_at}\n{sorted revoked ids joined by \n}`.

Behaviour:

- Refreshed inside `verify_manifest_identity` (all install paths pass through it),
  fetched via the existing `http` module (HTTPS rule, `allow_insecure`, credential
  host rules unchanged).
- Signer check = `effective_keys` with the list's own `delegation`, whose `name` must
  equal `[global].app`. A list signed by a sub-key that is already in the cached revoked
  set is rejected (a revoked key is not valid).
- The local cache at `<dir>/revocations.json` (temp + rename) is **grow-only**: a newer
  valid list (`issued_at > cached.issued_at`) is merged as a union, never replacing
  entries. A compromised sub-key can therefore add revocations (denial of service,
  fixed by revoking it from root) but can never un-revoke itself or another key.
  Un-revocation is not supported; a wrongly revoked key is replaced by a new sub-key.
- An older or equal `issued_at` is ignored (anti-rollback).
- Fetch failure, bad signature, or parse error -> `warn!` and keep using the cache.
  Never fails an update (not mandatory, per requirement).
- `verify_identity` reads the cached set, so staged installs see the latest known
  revocations.
- Revoking a root key is out of scope (remove it from `trusted_keys`).

### 4. Scheme detection and loader compatibility

Detection is by field presence, no schema bump:

- `delegation` present -> delegated path (§2); absent -> direct root signing, exactly
  today's behaviour. `schema` stays `lode/v1`.
- The loader logs the scheme once per fetch and `lode-cli status` prints it:
  `trust: delegated (sub <key_id>, issuer <key_id>, expires in 12d)` or
  `trust: direct (<key_id>)`.
- `lode-cli status` also lists the loader's trust capabilities
  (`features: delegation, revocation`), so operators can audit which hosts can consume
  a delegated manifest before the publisher switches.

Behaviour of existing loaders (<= 0.0.3), from code reading (`Manifest` has no
`deny_unknown_fields`; `manifest::parse` only checks `schema`):

| Loader trust config | Delegated manifest on an old loader |
|---|---|
| trusted keys configured (`auto`/`enforce`) | parses, ignores `delegation`, sub-key `sig` matches no root key -> **fail closed** (`signature did not match any trusted key`) |
| no keys, `auto` | accepted UNVERIFIED, same as any signed manifest today |
| `require_signature = off` | accepted, integrity only |

So an old loader never accepts a sub-key signature as valid; the cost is a confusing
error. Publisher migration rule: upgrade the fleet first (verify with `status`), then
switch to delegated signing. A schema bump (`lode/v2`) would give old loaders a clearer
error but also lock out unsigned/`off` consumers that work today, so it is not proposed.

New loaders fail with an explicit message when a manifest has `delegation` but the
delegation does not verify (reason: bad root signature / name / window / revoked),
instead of the generic key-mismatch error.

### 5. Publisher tooling (`lode-cli`)

- `lode-cli delegate --key root.key --sub <entry|pubkey file> --name myapp
  --valid-for 30d [--not-before <unix>]` -> prints the cert JSON (or `--out delegation.json`).
- `lode-cli manifest --delegation delegation.json` and `manifest-sign --delegation delegation.json` embed the
  cert; `manifest-sign` refuses a cert whose sub-key does not match the signing key.
- `lode-cli sign` is unchanged (the sub-key signs artifacts like any key).
- `lode-cli verify --delegation delegation.json` accepts a sub-key signature through a cert.
- `lode-cli revoke --key <root or sub key> [--delegation delegation.json] --list
  revocations.json --add <key_id>` creates or updates the list, bumps `issued_at`,
  re-signs.
- `lode-cli status` prints the cert (sub key_id, issuer, `not_after` remaining) and the
  cached revocation list's `issued_at`.

### 6. Tests (RED first)

- `verify.rs`: cert round-trip; tampered field, wrong app, outside window, unknown
  issuer, revoked sub-key, sub-key-issued cert all rejected; algorithm bound.
- `install.rs`: manifest + asset signed by a sub-key verify under `enforce` and
  `require_manifest = fresh`; root-signed manifests still pass; cert expired between
  fetch and install is rejected.
- Revocation: root- and sub-key-signed lists accepted; newer list merges into cache,
  older ignored; list omitting a cached entry does not un-revoke it; list signed by a
  revoked sub-key rejected; unreachable/bad-signature list warns and keeps cache;
  revoked key rejected at install.
- GitHub: release with `lode-delegation.json` + sub-key `.sig` verifies; without it fails.
- Detection: manifest with/without `delegation` reports the right scheme in `status`.
- CLI: `delegate` -> `manifest-sign --delegation` -> loader verify end-to-end; `revoke`.

### 7. Docs

`SECURITY.md`, `docs/architecture.md` (trust section), `docs/integration.md`
(publisher flow: root offline, sub-key in CI, renewal cadence), `docs/operations.md`
(`revocation_url`), `docs/manifest.example.json`, the `.zh-CN.md` mirrors,
`CHANGELOG.md`.

## Risks

- **Sub-key-signed revocation lists**: a leaked sub-key can revoke other sub-keys
  (DoS) until it is itself revoked or expires; it cannot un-revoke anything.
- **Revocation is soft by design**: whoever can block the revocation URL keeps a
  revoked-but-unexpired sub-key usable until `not_after`. Short cert lifetimes are the
  real bound; docs must say so.
- **Clock dependency**: a wrong host clock rejects valid certs (same class as
  `expires` today). Error message must name the window and `now`.
- **Delayed installs**: a staged update whose cert expires before cut-over fails and
  is retried on the next fetch with the renewed cert.
- **API change**: `Asset` and `Manifest` gain a field; embedders constructing them
  by struct literal break. Acceptable at 0.0.x; noted in `CHANGELOG.md`.
- **Root key handling becomes routine**: every renewal needs the offline root. Renewal
  cadence must be documented (e.g. 30-day certs renewed weekly).

## Scope

- `crates/lode-core`: `verify.rs` (cert + revocation types and checks),
  `manifest.rs` (field, parse propagation, GitHub `lode-delegation.json`), `install.rs` (effective keys, revocation
  refresh/cache), `config.rs` (`revocation_url`), `commands/status.rs`.
- `crates/lode`: `cli.rs`, `authoring.rs`, `lib.rs` (new `delegate`, `revoke`;
  `--delegation` on `manifest`, `manifest-sign`, `verify`), `config_cli.rs`.
- Docs listed above. Estimated ~900-1200 lines including tests.
- Out of scope: lode-server `publish`
  flow, lode's own `self-update` release keys (`release-keys.txt`), multi-level
  chains, threshold signatures, root-key revocation.

## Alternatives

- **Status quo (flat keys + rotation)**: root offline is possible only if the root
  signs every release; an online key must be listed in every client.
- **Per-asset delegation**: allows different sub-keys per asset but duplicates the cert in
  every asset; top-level cert with parse-time propagation is simpler.
- **Re-verify the cert against the manifest at install time (thread `&Manifest`
  into `install`)**: cleaner data flow but changes the public `install()` signature
  and every caller; carrying the cert on `Asset` keeps call sites unchanged.
- **TUF**: full role/threshold/snapshot model; far heavier than needed here.
- **Revocation list signed only by root**: strongest, but every revocation needs the
  offline root. Rejected: a valid sub-key may sign it, with the grow-only cache as the
  safeguard.

## Annotations

Decisions (2026-10-15):

1. GitHub-releases source is supported via a `lode-delegation.json` release asset.
2. No client-side cap on delegation lifetime; the publisher's `not_after` is trusted.
3. The revocation list follows the update rule: root or any valid delegated sub-key may
   sign it.
