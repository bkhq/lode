# 20261017-0251-rust-sdk-no-unsafe Remove unsafe from the Rust SDK

- **status**: completed
- **createdAt**: 2026-10-17 02:51
- **approvedAt**: 2026-10-17 03:00
- **relatedTask**: 20261017-0251-rust-sdk-no-unsafe

## Context

`sdks/lode.rs` (single-file SDK, included by apps as `mod lode`) has two
`unsafe` call sites, present since 0.0.1:

- `LockGuard` calls `flock(2)` through an `extern "C"` declaration to
  serialise `state.json` read-modify-writes on `state.json.lock`.
- `install_term_handler` installs a SIGTERM/SIGINT handler with `signal(3)`.

It also imports `std::os::unix`, so it builds on Unix only. lode itself takes
the same lock with std's safe `File::lock()` (`lode-core/src/state.rs`), which
is `flock(2)` on Unix and `LockFileEx` on Windows.

## Proposal

1. Lock: replace the `flock` FFI with `File::lock()` / `File::unlock()` (std,
   Rust 1.89+). Same lock as lode takes, so the contract is unchanged; a failed
   open or lock still degrades to the lock-free RMW.
2. Signals: replace `signal(3)` with `signal_hook::flag::register` (safe) for
   SIGTERM and SIGINT, keeping `install_term_handler()` / `terminating()` as
   they are. Adds `signal-hook` (0.4.5, latest stable) as the SDK's third
   dependency.
3. Drop the `extern "C"` block, the `c_int` constants and `std::os::unix`.
4. Guard: the example app
   (`examples/rust/src/main.rs`) gets `#![forbid(unsafe_code)]` so CI proves the
   SDK compiles under it.
5. Docs: `sdks/README.md` dependency row (`serde` + `serde_json` + `signal-hook`,
   Rust 1.89+), `examples/rust/Cargo.toml` dependency and comment, CHANGELOG.

## Risks

- Apps that vendor `lode.rs` must add `signal-hook`; called out in CHANGELOG.
- MSRV becomes 1.89 (for `File::lock`).

## Scope

`sdks/lode.rs`, `sdks/README.md`, `examples/rust/{Cargo.toml,Cargo.lock,src/main.rs}`,
`CHANGELOG.md`. No change to lode's crates or the state.json contract.

## Alternatives

- Keep the FFI behind a documented `// SAFETY:` exception: allowed for FFI
  crates by pma-rust, but this SDK is app code, so it does not qualify.
- `ctrlc` instead of `signal-hook`: also safe, but handles SIGINT only unless
  its `termination` feature is enabled, and runs a handler thread.
- Drop the signal helper and let apps bring their own: breaks the SDK API.

## Annotations

Implementation notes (2026-10-17):

1. CI did not build `examples/rust`, so the `forbid(unsafe_code)` guard had no
   gate; added a `cargo check (examples/rust)` step next to the
   `examples/embed` one. Verified that an injected `unsafe` block fails it.
2. The SDK file itself also carries `#![forbid(unsafe_code)]`.
3. Runtime check: an SDK write waits on an external `flock` of
   `state.json.lock` (~1.7 s against a 2 s hold, 11 ms uncontended), and
   SIGTERM still exits the demo with 0.
