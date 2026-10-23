# lode - Task List

> Updated: 2026-10-15

## Usage

Each task is a single line linking to its detail file. All detailed information lives in `docs/task/<timestamp>-<feature-slug>.md`.

### Format

- [ ] [**20260907-1428-add-endpoint Add endpoint**](20260907-1428-add-endpoint.md) `P1`

### Status Markers

| Marker | Meaning |
|--------|---------|
| `[ ]`  | Pending |
| `[-]`  | In progress |
| `[x]`  | Completed |
| `[~]`  | Closed / Won't do |
| `[d]`  | Deleted detail file; index entry retained |

### Priority: P0 (blocking) > P1 (high) > P2 (medium) > P3 (low)

### Rules

- Only update the checkbox marker; never delete the line or change its other content. If the detail file is deleted, mark the entry `[d]`.
- Record change history and deletion reasons in `docs/changelog.md`; update affected dependency and plan references.
- New tasks append to the end.
- See each `<timestamp>-<feature-slug>.md` for full details, except `[d]` entries whose files have been deleted; consult `docs/changelog.md` for their history.

---

## Tasks

- [x] [**20261015-0103-delegated-signing-keys Delegated signing keys with revocation**](20261015-0103-delegated-signing-keys.md) `P1`
- [x] [**20261015-0106-resource-distribution Distribute arbitrary resources**](20261015-0106-resource-distribution.md) `P2`
- [x] [**20261017-0251-rust-sdk-no-unsafe Remove unsafe from the Rust SDK**](20261017-0251-rust-sdk-no-unsafe.md) `P2`
- [x] [**20261022-1915-status-report Report host status over OTLP/HTTP**](20261022-1915-status-report.md) `P2`
- [x] [**20261023-0630-github-private-token Read private GitHub releases with a token**](20261023-0630-github-private-token.md) `P2`
