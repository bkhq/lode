# 20261023-0848-embedded-report Full status reporting for embedded hosts

- **status**: completed
- **createdAt**: 2026-10-23 08:48
- **approvedAt**: 2026-10-23 09:10
- **relatedTask**: 20261023-0848-embedded-report

## Context

- `serve_embedded` → `serve_core`, which starts the reporter
  (`ReportGuard(Reporter::start(&cfg))`) and wires every lifecycle hook, so an
  embedded host already gets the metrics and the `lode.*` events once
  `[report].endpoint` is set.
- Gaps: `ConfigBuilder` has no `[report]` setters (only `from_toml` works);
  embedded hosts read no `LODE_*` env, so there is no other way in;
  `docs/embedding(.zh-CN).md` say nothing about reporting; no test covers
  reporting through `serve_embedded`.

## Proposal

1. `ConfigBuilder`: `report_endpoint`, `report_interval`, `report_header`
   (repeatable, `${ENV}` expanded as in `[report].headers`),
   `report_attribute(key, value)`, `report_instance_id`. `build()` runs the
   existing `validate_report`.
2. Tests: builder unit tests (setters, validation); a `lode-supervisor`
   integration test that runs `serve_embedded` host-owned against a local stub
   endpoint and sees metrics, `lode.started` and `lode.stopping`.
3. Docs: a "Status reporting" subsection in `docs/embedding.md` and
   `docs/embedding.zh-CN.md` §3; CHANGELOG Unreleased.

## Risks

- New public API surface (builder setters) is a semver addition only.

## Scope

lode-core (config builder), lode-supervisor (test), docs, CHANGELOG.

## Alternatives

- Docs only (point hosts at `from_toml`): no code, but the builder stays
  incomplete.
- A host event handle (`ReportHandle`) for host-defined events: dropped;
  lode's own metrics and events cover the supervised app.

## Annotations

Decisions (2026-10-23): `SelfUpdate` stays without reporting — a one-shot
library call has no loop, no `state.json` and no supervised child; an app that
updates itself reports through its own telemetry. No host event handle.
