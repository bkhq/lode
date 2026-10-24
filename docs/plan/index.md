# lode - Plan Index

> Updated: 2026-10-15

## Usage

Each plan is a single line linking to its detail file. All detailed information lives in `docs/plan/<timestamp>-<feature-slug>.md`.

### Format

- [ ] [**20260907-1440-add-endpoint Add endpoint**](20260907-1440-add-endpoint.md) `YYYY-MM-DD`

### Status Markers

| Marker | Meaning |
|--------|---------|
| `[ ]`  | Draft / Pending review |
| `[-]`  | Approved / Implementing |
| `[x]`  | Completed |
| `[~]`  | Rejected / Abandoned |
| `[d]`  | Deleted detail file; index entry retained |

### Rules

- Only update the checkbox marker; never delete the line or change its other content. If the detail file is deleted, mark the entry `[d]`.
- Record change history and deletion reasons in `docs/changelog.md`; update affected task and plan references.
- New plans append to the end.
- See each `<timestamp>-<feature-slug>.md` for full details, except `[d]` entries whose files have been deleted; consult `docs/changelog.md` for their history.

---

## Plans

- [x] [**20261015-0103-delegated-signing-keys Delegated signing keys with revocation**](20261015-0103-delegated-signing-keys.md) `2026-10-15`
- [x] [**20261015-0107-resource-distribution Distribute arbitrary resources**](20261015-0107-resource-distribution.md) `2026-10-15`
- [x] [**20261017-0251-rust-sdk-no-unsafe Remove unsafe from the Rust SDK**](20261017-0251-rust-sdk-no-unsafe.md) `2026-10-17`
- [x] [**20261022-1915-status-report Report host status over OTLP/HTTP**](20261022-1915-status-report.md) `2026-10-22`
- [x] [**20261023-0630-github-private-token Read private GitHub releases with a token**](20261023-0630-github-private-token.md) `2026-10-23`
- [x] [**20261023-0848-embedded-report Full status reporting for embedded hosts**](20261023-0848-embedded-report.md) `2026-10-23`
- [x] [**20261024-1505-lode-build-info Expose lode's version, commit and features to the app**](20261024-1505-lode-build-info.md) `2026-10-24`
- [x] [**20261024-1559-manifest-runtime Deliver the runtime through the signed manifest**](20261024-1559-manifest-runtime.md) `2026-10-24`
