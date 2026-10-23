# 20261023-0630-github-private-token Read private GitHub releases with a token

- **status**: completed
- **createdAt**: 2026-10-23 06:30
- **approvedAt**: 2026-10-23 06:40
- **relatedTask**: 20261023-0630-github-private-token

## Context

- `manifest::fetch_github` selects the release through the API with
  `[http].headers`, then maps each asset's `browser_download_url`
  (`github.com/<o>/<r>/releases/download/...`) into the manifest.
- For a private repository that URL is 404 with a token: only the asset API
  `GET {github_api}/repos/<o>/<r>/releases/assets/<id>` with
  `Accept: application/octet-stream` works; it answers 302 to a short-lived
  signed URL on GitHub's storage host, which must be fetched **without** the
  token.
- `allowed_hosts` for the github source today sends `[http].headers` to
  `github.com`, `objects.githubusercontent.com` and `codeload.github.com` as
  well — wider than needed for a token.
- The same path serves the `<asset>.sig` sidecar and the
  `lode-delegation.json` asset.

## Proposal

### 1. Config

```toml
[update]
github       = "owner/repo"
github_token = "${GITHUB_TOKEN}"   # ${ENV} expanded per request; never logged
```

- `[update].github_token`, `--github-token`, `LODE_GITHUB_TOKEN`. Only used
  with `github`; set together with `manifest` it is a config error.
- No implicit `GITHUB_TOKEN` fallback (the app's environment often has one).
- Pinned across reloads with the update source.

### 2. Behaviour with a token

- Every API call carries `Authorization: Bearer <token>` and
  `X-GitHub-Api-Version: 2022-11-28`.
- Release assets (app asset, `.sig` sidecar, delegation) are fetched from the
  asset API URL with `Accept: application/octet-stream`; the token goes to the
  `github_api` host only, and the redirect to the signed storage URL is
  followed without it (the existing per-hop credential check).
- Clear errors: 401 → token rejected; 404 → repository/release not found or
  the token lacks access (fine-grained PAT: *Contents: read*; classic: `repo`).
- Without a token nothing changes (public repos keep `browser_download_url`).
  A token in `[http].headers` keeps working for the API call as today; docs
  move to `github_token`.

### 3. Docs

integration (+zh) GitHub section, architecture (+zh) §10 table and §12,
`lode.example.toml`, README private-sources line, CHANGELOG.

### 4. Tests

- Unit (stub servers): asset fetched via the API URL with `Accept` and
  `Authorization`; the redirect hop to another host gets no `Authorization`;
  sidecar via the same path; 401/404 messages; config validation.
- e2e: `githubServer.ts` gains a private mode (requires the token, serves
  assets only through `/releases/assets/<id>` with a redirect); scenario:
  install and update from a private repo.

## Risks

- GitHub's storage redirect host changes over time; the token never goes there,
  so only the per-hop rule matters.
- Asset downloads through the API count toward the authenticated rate limit
  (5000/h), ample for one check per `check_interval`.

## Scope

lode-core (config, manifest GitHub adapter, download headers), lode CLI flag,
docs, tests. No change to signing or the manifest contract.

## Alternatives

- Keep `[http].headers` only and switch to the asset API when headers are set:
  cannot tell a GitHub token from other headers, and would keep sending them to
  every GitHub host.
- `codeload`/archive downloads: not release assets.

## Annotations

Implementation notes (2026-10-23):

- `manifest::credentials` builds the download credentials for every asset,
  sidecar and delegation fetch; with a token the github `allowed_hosts` is the
  API host only (plus `[http].credential_hosts`).
- Verified against a real private repository (`bkhq/lode-private-test`, two
  signed releases): without a token the 404 names `github_token`; with one,
  v0.0.1 installed under `enforce` and v0.0.2 was auto-applied. GitHub's
  storage host is now `release-assets.githubusercontent.com`; the token never
  reached it.
- Credentials stripped on a redirect hop are logged at debug instead of warn
  (every private download redirects to storage).
