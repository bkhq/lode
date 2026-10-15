# 20261015-0106-resource-distribution Distribute arbitrary resources

- **status**: completed
- **priority**: P2
- **owner**: agent/lode-20261015
- **createdAt**: 2026-10-15 01:06

## Description

Extend lode beyond app self-updates: publish and consume arbitrary signed resources
(config bundles, data sets, models, static assets) through the same manifest, channel,
signature and delegation machinery, without launching or supervising anything.

Current coupling (investigation pending):

- One `lode.toml` = one app (`[global].app`) = one manifest = one `current` version.
- The engine/supervisor path always ends in a launch (`[command]`, `run`/`exec`).
- Reusable pieces: `manifest::fetch`, `resolve_target`, `download::fetch_artifact`,
  `install::install` (verify + stage + atomic dir), `switch_current`, `prune`, and
  the fetch/install split used by `lode_core::self_update`.

Open questions before a plan: target use cases, delivery shape (`[[resource]]` in
`lode.toml` synced by the supervisor, a one-shot `lode-cli pull`, and/or a
`lode-core` library API), how the app is notified of a new resource version, and
whether lode-server `publish` must accept non-app resources.

## ActiveForm

Designing arbitrary resource distribution

## Dependencies

- **blocked by**: 20261015-0103-delegated-signing-keys
- **blocks**: (none)

## Notes

Plan: [20261015-0107-resource-distribution](../plan/20261015-0107-resource-distribution.md)

Delegation (20261015-0103-delegated-signing-keys) binds a delegation to the manifest
`name`, so it applies to resource manifests unchanged.
