# 20261024-1559-manifest-runtime Deliver the runtime through the signed manifest

- **status**: completed
- **createdAt**: 2026-10-24 15:59
- **approvedAt**: 2026-10-24 16:40
- **relatedTask**: 20261024-1559-manifest-runtime

## Context

- `[runtime]` (`runtime`, `download`, `sha256`, `version`, `version_check`) is
  local config only. `ensure_runtime` runs once per supervise-loop start, and
  the resulting dir is reused for every spawn: a hot update to a release that
  needs another runtime keeps the old one. `[runtime].download`/`sha256` are
  pinned across reloads.
- Precedent: an asset's `run`/`exec` override `[command]`. They are bound into
  the `lode.artifact.v2` message and the catalog signature, and stored per
  version in the `.lode.json` marker.
- GitHub releases carry no `run`/`exec` today (no place to put them). The
  lode-server registry stores `run`/`exec` per asset, verifies
  `lode.artifact.v2` signatures and builds the catalog.

## Proposal

1. Manifest: an optional asset `runtime` object
   `{ runtime, version, version_check, download, sha256 }` — same fields as
   `[runtime]`; `download` requires `sha256`.
2. Signature: an asset with `runtime` is signed over `lode.artifact.v3`
   = the v2 fields plus `runtime`, `version`, `version_check`, `sha256` (empty
   when absent, fixed order; the download URL is not signed);
   an asset without it keeps v2, so existing releases and older loaders are
   unaffected. A loader that predates v3 cannot verify a v3 asset and refuses
   it rather than starting it with the wrong runtime. The catalog signature
   covers the object as part of the asset.
3. Precedence: a manifest `runtime` replaces the local `[runtime]` as a whole
   (no field merge) when a verified signature covers it — the asset's v3
   signature, or the manifest's top-level signature (which covers the whole
   catalog). Otherwise (unsigned asset and unsigned or stripped manifest,
   `require_signature = "off"`) the override is ignored and logged: only
   signed data may override local config, and nothing in the manifest can
   change `[trust]`. A `download` without `sha256` is rejected.
4. Per version: the effective runtime is stored in the version's `.lode.json`
   marker; every spawn (bootstrap, hot update, rollback, restart) resolves the
   runtime for the version it starts, through the existing cache keyed by
   version + URL. A new version whose runtime cannot be fetched or fails its
   version probe is treated like a failed install (no switch).
5. Tooling: `lode-cli manifest`/`sign`/`publish` accept `--runtime-*` flags
   (or a `runtime` table) and sign v3; `lode-cli verify` understands v3.
6. Sources: native manifest and lode-server. GitHub releases stay without
   overrides (unchanged). lode-server (separate change): store the `runtime`
   object per asset, accept it on upload, verify v3, include it in the
   catalog; schema migration.
7. Tests: message v2/v3 golden bytes, override precedence (signed vs unsigned),
   marker round-trip, spawn picks the target version's runtime, e2e: release
   0.0.2 with a new runtime → hot update uses it, rollback returns to the old.
8. Docs: integration/architecture (+zh), lode.example.toml, publishing,
   CHANGELOG.

## Risks

- v3 is a new signed message: publisher tools, lode-server and loaders must
  agree on the bytes; golden tests on both sides.
- A runtime download per version adds a fetch to updates (cached by version).

## Scope

lode-core (manifest, verify, install marker, engine runtime), lode-supervisor
(per-spawn runtime), lode CLI (authoring flags), tests, docs; lode-server
follow-up.

## Alternatives

- Runtime as a separate signed `[[resource]]`: updates independently of the
  app, but app and runtime versions drift and a rollback does not restore the
  runtime the old version was built for.
- Manifest-level (not per-asset) runtime: one entry cannot serve several
  platforms' downloads.

## Annotations

Decisions (2026-10-24): accept the runtime override under either signature —
the asset's `lode.artifact.v3` or the manifest's top-level signature. The
download URL stays out of the signed bytes (the signed `sha256` pins the
content; mirrors stay possible), like an asset's `url`.
