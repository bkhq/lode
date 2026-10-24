# 20261024-1505-lode-build-info Expose lode's version, commit and features to the app

- **status**: completed
- **createdAt**: 2026-10-24 15:05
- **approvedAt**: 2026-10-24 15:20
- **relatedTask**: 20261024-1505-lode-build-info

## Context

- The app receives `LODE_ACTIVE_VERSION`, `LODE_DIR`, `LODE_WORKDIR`,
  `LODE_CONFIG`, `LODE_INSTANCE`, `LODE_READINESS`; nothing says which lode it
  is. `state.json` has no lode identity either.
- The version is `CARGO_PKG_VERSION` only; no build embeds a commit. Release
  and docker builds run `cargo build` in a full checkout, so `git` is there.
- Contract features were added over releases (`check_nonce`, staged cut-over,
  `resource_target`, `hold`, …); an app cannot detect them except by version
  comparison.

## Proposal

1. Build info in `lode-core` (`build.rs`): the commit from `LODE_BUILD_COMMIT`
   when set, else `git rev-parse HEAD` when the repository top level is this
   workspace, else none. Exposed as `lode_core::build_info::{VERSION, COMMIT,
   LONG_VERSION, FEATURES}`.
2. Features: a fixed list of stable, append-only names for what this lode
   supports in the app contract: `target`, `restart`, `check`, `hold`,
   `readiness-state`, `staged-cutover`, `config-generation`, `resources`,
   `resource-target`, `report`.
3. To the app:
   - env at every spawn: `LODE_VERSION`, `LODE_COMMIT` (when known),
     `LODE_FEATURES` (comma-separated);
   - `state.json`: a lode-owned `lode` object
     `{ "version", "commit", "features": [...] }` written when the loop starts.
4. Elsewhere: `lode --version` / `lode-cli --version` print
   `lode 0.x.y (abcdef123456)`; reports carry a `lode.commit` resource attribute.
5. SDKs (TS, Go, Rust): `State.lode` field, env accessors and
   `hasFeature(name)`.
6. Tests: build-info unit tests, child-env test, `state.json` test, an e2e
   scenario reading env + `state.json` from the app; SDK tests where they exist.
7. Docs: integration (+zh) environment table and `state.json` contract, SDK
   README, architecture note, CHANGELOG.

## Risks

- A git dependency build (embedders) has no `.git` in the checkout → commit
  absent; the version and features are still there.
- The feature list becomes contract: names are never removed or renamed.

## Scope

lode-core (build.rs, build info, state), lode-supervisor (env, state write),
lode CLI (version), report attribute, SDKs, tests, docs.

## Alternatives

- Env only: simplest, but `lode-cli` and non-child readers cannot see it.
- `state.json` only: env is what an app reads at boot without parsing files.

## Annotations

Implementation notes (2026-10-24):

- No `-dirty` suffix: the build script only reruns when `HEAD` moves, so a
  dirty flag would go stale after an edit.
- The e2e fixture app matched state keys with `grep '"key"'`, which also hit
  the `"target"` feature name; it now matches `"key":`. Apps should parse
  `state.json` as JSON (the SDKs do).
- SDKs have no test suite; each was checked with a throwaway program (TS via
  bun, Go via a temporary test, Rust built as an edition-2021 crate).
