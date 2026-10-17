# 20261017-0251-rust-sdk-no-unsafe Remove unsafe from the Rust SDK

- **status**: completed
- **priority**: P2
- **owner**: agent/lode-20261017
- **createdAt**: 2026-10-17 02:51

## Description

`sdks/lode.rs` declares `extern "C"` `flock` and `signal` and calls them in
`unsafe` blocks (the `state.json.lock` guard and `install_term_handler`), which
breaks the pma-rust rule that app code forbids `unsafe`. Replace both with safe
APIs without changing the SDK's public surface or the lock contract.

## ActiveForm

Removing unsafe from the Rust SDK

## Notes

- Plan: [20261017-0251-rust-sdk-no-unsafe](../plan/20261017-0251-rust-sdk-no-unsafe.md)
