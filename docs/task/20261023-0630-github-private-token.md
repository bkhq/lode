# 20261023-0630-github-private-token Read private GitHub releases with a token

- **status**: completed
- **priority**: P2
- **owner**: agent/gh-token-20261023
- **createdAt**: 2026-10-23 06:30

## Description

Let the `[update].github` source read
releases of a private repository with a GitHub token. Today the documented way
is a bearer token in `[http].headers`, which authenticates the API call but not
the asset download: `browser_download_url` answers 404 for a private repo even
with a token. Assets must be fetched through the release-asset API with
`Accept: application/octet-stream`, and the token must stay on the API host.

## ActiveForm

Adding private GitHub repository support

## Notes

- Plan: [20261023-0630-github-private-token](../plan/20261023-0630-github-private-token.md)
