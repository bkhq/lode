# 20261015-0103-delegated-signing-keys Delegated signing keys with revocation

- **status**: completed
- **priority**: P1
- **owner**: agent/lode-20261015
- **createdAt**: 2026-10-15 01:03

## Description

Support a two-level trust chain: an offline root key (the operator's `trusted_keys`)
signs a short-lived delegation certificate for an online sub-key; a manifest carries
the certificate and its catalog and assets are signed by the sub-key.

Acceptance criteria:

- A manifest signed by a sub-key whose certificate is signed by a trusted root key,
  scoped to the app and inside its validity window, verifies (catalog `sig`,
  `expires_sig`, and per-asset `sig`).
- An expired, not-yet-valid, wrongly-scoped, or root-unsigned certificate is rejected.
- An optional subscribed revocation list (root-signed) revokes sub-keys by `key_id`;
  an unreachable or invalid list never blocks an update, and a cached list cannot be
  rolled back to an older one.
- Manifests signed directly by a root key keep verifying unchanged.
- `lode-cli` can issue a certificate, embed it in a manifest, and issue/update a
  revocation list.

## ActiveForm

Adding delegated signing keys with revocation

## Dependencies

- **blocked by**: (none)
- **blocks**: 20261015-0106-resource-distribution

## Notes

Plan: [20261015-0103-delegated-signing-keys](../plan/20261015-0103-delegated-signing-keys.md)

- complete: Unit, doc, clippy, rustdoc and full bun e2e suites pass; e2e 30-delegation covers the root-only trust and revocation paths.
