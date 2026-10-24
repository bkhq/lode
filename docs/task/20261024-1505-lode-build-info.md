# 20261024-1505-lode-build-info Expose lode's version, commit and features to the app

- **status**: completed
- **priority**: P2
- **owner**: agent/build-info-20261024
- **createdAt**: 2026-10-24 15:05

## Description

The app cannot tell which lode supervises it, from which commit, or which
parts of the `state.json` contract it understands. Embed the git commit at
build time and hand the app lode's version, commit and a feature list through
the environment and `state.json`, with SDK accessors.

## ActiveForm

Exposing lode's build info to the app

## Notes

- Plan: [20261024-1505-lode-build-info](../plan/20261024-1505-lode-build-info.md)
