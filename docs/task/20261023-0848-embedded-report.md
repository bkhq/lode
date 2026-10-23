# 20261023-0848-embedded-report Full status reporting for embedded hosts

- **status**: completed
- **priority**: P2
- **owner**: agent/embedded-report-20261023
- **createdAt**: 2026-10-23 08:48

## Description

`serve_embedded` already runs the same reporter as the `lode` binary, but an
embedding host can only turn it on through `Config::from_toml`: the builder has
no `[report]` setters, the embedding guide never mentions reporting, and no
test exercises reporting through `serve_embedded`.

## ActiveForm

Completing status reporting for embedded hosts

## Notes

- Plan: [20261023-0848-embedded-report](../plan/20261023-0848-embedded-report.md)
