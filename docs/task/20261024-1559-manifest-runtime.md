# 20261024-1559-manifest-runtime Deliver the runtime through the signed manifest

- **status**: completed
- **priority**: P2
- **owner**: agent/manifest-runtime-20261024
- **createdAt**: 2026-10-24 15:59

## Description

`[runtime]` lives only in the local `lode.toml`, so changing the runtime (for
example a new bun) means editing every host. Let a release carry its runtime in
the manifest asset, signed like `run`/`exec`: it overrides the local
`[runtime]`, is downloaded and verified like the app, and follows the version
through updates and rollbacks.

## ActiveForm

Delivering the runtime through the signed manifest

## Notes

- Plan: [20261024-1559-manifest-runtime](../plan/20261024-1559-manifest-runtime.md)

- complete: Runtime override via signed manifest asset (v3 + catalog), per-version marker, per-spawn resolution, CLI flags, tests, docs
