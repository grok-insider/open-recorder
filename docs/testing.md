# Testing strategy

How open-recorder guarantees high-quality, regression-resistant code. This is
the detail behind the testing table in `AGENTS.md`. The quality gate is:
**`cargo fmt --check` + `cargo clippy -D warnings` + `cargo test --all` must all
pass before any merge.**

## Tiers

### Unit tests — pure logic, no I/O

In `#[cfg(test)]` modules next to the code. The high-value targets:

- **Ring buffer** (`ord-core`): capacity/eviction (oldest packets drop as the
  buffer fills), byte/duration accounting, behavior at empty / single-packet /
  exactly-full states.
- **Save-last-N keyframe math** (`ord-core`): **the highest-risk logic.** The
  saved clip must begin on the newest keyframe ≤ N seconds back. Exhaustive
  cases:
  - N smaller than, equal to, and larger than the buffered span,
  - no keyframe within the window,
  - buffer with a single keyframe,
  - empty buffer,
  - keyframe exactly at the N-second boundary.
- **IPC protocol** (`ord-common`): every command and event round-trips through
  bincode encode→decode unchanged.
- **Newtypes**: `BufferSeconds`, `ClipDuration`, `MonitorId`, etc. reject
  invalid construction and never silently mix units.

### Integration tests — daemon behavior, no GPU

Inline in `crates/ord-daemon/src/server.rs` under `#[cfg(all(test, unix))]`.
Drive `ordd` against the **`MockBackend`** (`docs/backends.md`) over a real
Unix socket:

- `SaveLast(n)` produces a `ClipSaved` event with a plausible path/duration.
- `ToggleRecord`, `BufferOn/Off`, `Status` transition state correctly.
- Backend errors surface as `Error` events, never panics.
- Concurrent commands are serialized safely.

The mock emits a scripted frame sequence (controlled keyframe cadence + PTS), so
these tests are fully deterministic — **no sleeps, no real time, no GPU.**

### Golden tests — output file structure

In `crates/ord-core/tests/`. After a save (driven by the mock), assert on the
produced `.mkv` with `ffprobe`:

- container + codec are as configured (e.g. HEVC),
- duration ≈ requested N (within a tolerance),
- the first video packet is a keyframe,
- an audio track is present when audio is enabled.

These catch muxing regressions that unit tests can't see.

### Benchmarks — hot-path performance

`criterion` benches in `crates/ord-core/benches/`:

- ring-buffer push throughput,
- save-path mux latency for a fixed clip length.

Run locally to catch performance regressions on the capture hot path. Not a
merge gate, but tracked.

### GPU / real-hardware lane

End-to-end capture→encode→save against actual NVENC on the dev box. These tests
are `#[ignore]` by default (CI has no GPU and no Wayland session) and run
explicitly:

```sh
cargo test --features waycap -- --ignored
```

This lane is the only place real `waycap-rs` capture runs in tests; everything
else uses `MockBackend`.

## Determinism rules

- No `thread::sleep` to "wait for frames." Drive frame arrival and time through
  the mock.
- No reliance on a real compositor, GPU, or wall-clock in unit/integration
  tests.
- Tests must pass identically on CI (no GPU) and on the dev box.

## What CI runs

Four jobs (`.github/workflows/ci.yml`):

- **`rust`** (every push/PR, fast): `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings` (default features
  only), `cargo test --workspace` (GPU tests excluded via `#[ignore]`).
- **`cross-check`** (every push/PR): `cargo check --workspace` for
  `x86_64-pc-windows-gnu` and `aarch64-apple-darwin` — protects the Phase 0
  "compiles everywhere with the mock backend" guarantee without linking.
- **`nix-checks`** (every push/PR, devshell): `nix flake check --no-build`,
  `cargo clippy --all-features -- -D warnings`, and tests for every feature
  **except `waycap`** (the waycap/cust binaries dlopen `libcuda.so.1` and
  cannot load on a driverless runner; that lane runs on the dev box).
- **`build`** (master/tags/dispatch only): `nix build` of every package,
  pushing closures to `grok-insider.cachix.org` so flake
  consumers pull prebuilt instead of compiling.

## Coverage expectations

New logic ships with tests in the matching tier. The save-last-N boundary math
and the IPC protocol are considered critical and must stay at high coverage —
a change there without corresponding tests is incomplete.
