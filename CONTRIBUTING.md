# Contributing to open-recorder

Thanks for hacking on open-recorder. This is a latency-sensitive, Linux-native
zero-copy clipper; it optimizes for **clean, correct, real-time-safe** code. Read
[`AGENTS.md`](./AGENTS.md) and [`docs/architecture.md`](./docs/architecture.md)
before a first change.

## Dev setup

```bash
git clone https://github.com/grok-insider/open-recorder && cd open-recorder

# Pure logic (no GPU, no Wayland): builds + tests anywhere.
cargo build
cargo test --workspace
```

The **real recorder** (NVENC capture, the layer-shell HUD, the egui UI) is
feature-gated behind native libraries (ffmpeg, PipeWire, CUDA, Wayland, GL). The
flake devshell is canonical for that:

```bash
nix develop                      # CUDA + ffmpeg + PipeWire + clang toolchain
cargo build --release -p ord-daemon --features waycap   # ordd (real NVENC)
cargo build --release -p ord-ui --features gui          # clip library window
cargo build --release -p ord-overlay --features layershell  # ord-hud overlay
```

## The golden rules

These come straight from `AGENTS.md`; a change that breaks one needs a deliberate
design reason, not a shortcut.

1. **Traits at the platform/engine seams.** Code against `CaptureBackend` and
   `Overlay`, never a concrete backend, outside that backend's own module. A new
   OS/capture path is a new trait impl, not an `if cfg!`.
2. **The hot path never panics and never copies.** Encoded frames move into the
   ring buffer over a bounded channel — no per-frame allocation beyond the packet,
   no `unwrap`/`expect` on the capture→ring→mux path.
3. **Errors are values.** `Result` + `thiserror` enums. `unwrap`/`expect` only in
   tests and documented `main()` startup wiring.
4. **`unsafe` only in FFI shims**, small and isolated, with a `// SAFETY:` note.
5. **Newtypes over primitives** (`BufferSeconds`, `ClipDuration`, `MonitorId`,
   `Keyframe`) — no bare `u64` seconds through APIs.
6. **One bitstream module, two muxers.** Per-codec logic lives in
   `ord-core/src/mux/bitstream.rs` keyed by `Codec`, shared by the clip muxer and
   the streaming recorder. Match on `Codec`; never branch on `is_h264` booleans.
7. **All `Mutex` access is lock-tolerant** (the shared poisoned-lock-recovery
   helper) — a panicked worker degrades, it does not cascade `lock().unwrap()`.
8. **Config is layered; only the daemon writes overrides.** Never modify the base
   `config.toml` at runtime; settings changes persist as a sparse diff in
   `$XDG_STATE_HOME/open-recorder/overrides.toml` via `ordd`'s `SetConfig`.
9. **UI follows the design system.** Colors/spacing/radii/type come from
   `ord-ui/src/theme.rs`. No hardcoded `Color32`s in views.

## Before you push

All three must be clean (the `-D warnings` gate is enforced in CI):

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings   # in `nix develop`
cargo test --all
```

GPU/Wayland end-to-end tests are `#[ignore]`d (CI has no GPU). Run them on a real
NVIDIA box from the devshell:

```bash
cargo test --features waycap -- --ignored
```

- **Tests:** drive the daemon/core against the **mock `CaptureBackend`** — never
  let real capture leak into unit/integration tests. The "save last N seconds"
  keyframe-boundary math is the highest-risk logic; cover the edges.
- **Comments:** explain *why* (a timing quirk, a codec invariant), not *what*.
- Update the relevant doc under `docs/` and any new `config.toml` keys (plus the
  Home Manager `settings` example in `flake.nix`) when you add a knob.

## Commit & PR style

This repo uses [Conventional Commits](https://www.conventionalcommits.org). The
history drives automated versioning + the changelog (see [Releases](#releases)),
so prefix every commit subject with a type:

- `feat: …` — a user-visible feature → **minor** bump (pre-1.0: `0.x.0`).
- `fix: …` — a bug fix → **patch** bump (`0.0.x`).
- `feat!: …` (or a `BREAKING CHANGE:` footer) — a breaking change. Pre-1.0 this
  bumps the **minor**. Also bump `PROTOCOL_VERSION` (`ord-common/src/frame.rs`) on
  any incompatible `Command`/`Event` change.
- `docs:`, `refactor:`, `perf:`, `test:`, `chore:`, `ci:` — don't trigger a
  release on their own; grouped into the changelog where relevant.

Keep subjects short and imperative; add a scope when it helps
(`fix(ord-core): …`, `feat(ord-ui): …`). A PR should leave `master` green
(fmt + clippy + test).

## Releases

Releasing is automated with [release-plz](https://release-plz.dev)
(`release-plz.toml` + `.github/workflows/release.yml`). Don't bump versions or
hand-edit `CHANGELOG.md` — a clear Conventional Commit *is* the changelog entry.

1. Merge Conventional-Commit PRs to `master` as usual.
2. release-plz keeps a **release PR** open (`chore: release v…`) that bumps the
   single `[workspace.package].version` (every crate inherits it via
   `version.workspace = true`), refreshes `Cargo.lock`, and regenerates
   `CHANGELOG.md` from the commits since the last tag. An **AI-changelog step**
   (`grok-insider/release-changelog-action` in `release.yml`) then rewrites the
   PR's changelog entry into user-facing notes — one more reason to never
   hand-edit `CHANGELOG.md`.
3. **Merge the release PR to ship.** It tags `vX.Y.Z` and creates the GitHub
   Release, which gets:
   - the **static `ord` client** (`x86_64` musl) for PATH installs, and
   - **`ordd` / `ord-hud` / `ord-ui` AppImages** (bundled from the flake) for
     non-Nix Linux users.

   The same master push makes `ci.yml` build and push the closures to the
   `grok-insider` cachix cache (`flake.nix` reads the version from `Cargo.toml`), so
   NixOS consumers substitute `open-recorder-X.Y.Z` instead of compiling.

Nothing is published to crates.io.

**Repo setup (done).** *Settings → Actions → General → "Allow GitHub Actions to
create and approve pull requests"* is enabled — release-plz opened the v0.3.0
release PRs (#2/#3) through it. Secrets in place: `CACHIX_AUTH_TOKEN` (cachix
push) and `RELEASE_PLZ_TOKEN` (so the release PR triggers required CI); the
`v*` tags exist, so release-plz computes the next bump automatically.

See [`docs/releasing.md`](./docs/releasing.md) for the version surfaces (package
version, binary `--version`, wire `PROTOCOL_VERSION`) and how consumers update.
