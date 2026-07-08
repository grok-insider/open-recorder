# Versioning & releasing

open-recorder uses **SemVer**, with the workspace `Cargo.toml` as the single
source of truth and **git tags** (`vX.Y.Z`) as the release markers. The
**automatic stream stays on the patch line** (`z` bumps: `0.4.0 → 0.4.1` on
`feat:`/`fix:` commits, open-media's model); deliberate **minor/major
milestones** (feature drops, IPC protocol bumps) are cut by a repo admin via
the *Manual Version Bump* workflow.

## The three version surfaces

| Surface | Where | Notes |
|---|---|---|
| **Package version** | `[workspace.package] version` in `Cargo.toml` | Every crate inherits it (`version.workspace = true`). The flake reads it (`flake.nix`: `version = importTOML ./Cargo.toml`), so package + store-path names track it automatically. |
| **Binary `--version`** | `ord --version`, `ordd --version` | Prints `X.Y.Z [protocol N]`, plus the short git rev `(abc1234)` for local `cargo` builds. Nix builds omit the rev (no `.git` in the flake source) so they stay reproducible. Implemented in `ord-common` (`version.rs` + `build.rs`). |
| **Wire protocol** | `PROTOCOL_VERSION` in `ord-common/src/frame.rs` | Bumped on any incompatible `Command`/`Event` or nested bincode payload change. The framing layer rejects peer skew loudly (a stale `ord`/`ordd`/`ord-hud`), so `ord` and `ordd` from different releases refuse to talk instead of mis-decoding. |

## Cutting a release (the automated patch line)

You do **not** bump the version, edit `CHANGELOG.md`, or tag by hand — a clear
[Conventional Commit](https://www.conventionalcommits.org) drives all three.

1. Land `feat:`/`fix:`/… PRs on `master` as usual. If `Command`/`Event` shapes
   or nested bincode-carried types such as `Config` changed incompatibly, bump
   `PROTOCOL_VERSION` (`ord-common/src/frame.rs`) in that same commit — and
   prefer shipping it via a manual **minor** bump (below).
2. The **`release-pr` job** (`.github/workflows/release.yml` — our own job, see
   "Why not `release-plz release-pr`") keeps a release PR open
   (`chore: release vX.Y.Z+1`) whenever `feat`/`fix` commits have landed since
   the last tag: it bumps the single `[workspace.package].version` to the next
   **patch**, refreshes `Cargo.lock`, and writes the `CHANGELOG.md` section via
   `grok-insider/release-changelog-action` (AI notes over the commit range,
   Keep-a-Changelog heading enforced). The PR regenerates from master on every
   push; chore/docs/ci-only pushes don't churn it but still ride into the notes.
   Breaking commits since the tag are flagged in the PR body as a nudge toward
   a manual minor instead.
3. **Merge the release PR to ship.** `release-plz-release` (release-plz with
   `release_always = true`) sees the untagged version, tags `vX.Y.Z` and
   creates the GitHub Release from the changelog, then two artifact jobs attach
   prebuilt binaries:
   - `upload-ord` — the portable **static `ord`** client (`x86_64` musl), for
     PATH installs / compositor keybinds.
   - `upload-appimages` — **`ordd` / `ord-hud` / `ord-ui` AppImages**, bundled
     from the flake (`nix bundle`), for non-Nix Linux users.
4. The same master push makes `ci.yml`'s `build` job push the closures to
   `grok-insider.cachix.org` (it reads the version from `Cargo.toml`), so flake
   consumers substitute `open-recorder-X.Y.Z` instead of compiling. Cachix is
   deliberately handled in `ci.yml`, not `release.yml`.

   **Cache lag:** the GitHub Release appears within seconds of the merge, but
   the `build` job needs ~10–12 minutes to compile and upload the closures. A
   `nix flake update` + rebuild inside that window compiles the `ord-*`
   packages locally (harmless, just slow). If you want the substitutes, wait
   for the master push's `build + cache` job to finish first. The master merge
   commit and the `vX.Y.Z` tag have identical trees, so either ref hits the
   same store paths.

The `release-pr` job also has a `workflow_dispatch` handle with a `force` input
that bypasses the feat/fix filter — useful to kick a release PR on demand.

## Minor/major milestones (Manual Version Bump)

`.github/workflows/manual-version-bump.yml` (`workflow_dispatch`, repo-admin
gated, with a dry-run option) opens a `chore: release vX.Y.0` / `vX+1.0.0` PR:
version bump + `Cargo.lock` + AI changelog section, exactly like the automatic
PR but for a deliberate milestone. Merging it releases through the same
`release_always` path. Use it for feature drops and **IPC protocol bumps** —
the automatic stream never leaves the patch line.

## Why not `release-plz release-pr`

`release-plz release-pr` is broken upstream for this workspace
(release-plz/release-plz#2651-adjacent): in git-only mode its change detection
reconstructs the last tag's worktree and runs `cargo package` on it, which
tries to resolve the git-pinned `waycap-rs` fork against crates.io (`^3.0.0`
does not exist there) and fails. open-media hit the same wall and only escaped
by publishing its git dependency to crates.io — not an option for a fork of
someone else's crate (whose own deps are forked git branches). The **release
step is unaffected**: `release-plz release` never packages; it only checks the
workspace version against the `v*` tags and cuts the tag + GitHub Release
(proven by v0.3.0 and v0.4.0). Hence the split: our own `release-pr` job +
release-plz for the release.

Nothing is published to crates.io (`git_only = true` + `publish = false` in
`release-plz.toml`). Do **not** set `publish = false` in the Cargo manifests:
release-plz skips manifest-unpublishable packages entirely — including their
git tag + GitHub Release — so a manifest-level flag silently turns
`release-plz release` into "nothing to release" (verified against 0.3.159).

The pipeline is live: the repo's *"Allow GitHub Actions to create and approve
pull requests"* setting is enabled. Secrets: `RELEASE_PLZ_TOKEN` (PAT so
release-PR branches trigger CI), `OPENROUTER_API_KEY` (AI changelog; falls back
to a plain commit list without it), `CACHIX_AUTH_TOKEN`.

## How a consumer updates

The NixOS/Home-Manager config pins `open-recorder.url = "github:grok-insider/open-recorder"`.
To move to a new release:

```sh
nix flake update open-recorder        # or: nix flake lock --update-input open-recorder
nixos-rebuild switch --flake .         # (or home-manager switch)
ord --version                          # verify the installed version
```

Pinning to a tag instead of `master` is `github:grok-insider/open-recorder/vX.Y.Z`.

## Non-Nix Linux consumers

Users not on Nix install from the GitHub Release assets instead of the cache:

- **`ord` client** — `ord-X.Y.Z-x86_64-linux-musl.tar.gz`: a static binary; drop
  it on `PATH` and point compositor keybinds at it.
- **`ordd` / `ord-hud` / `ord-ui`** — `*-X.Y.Z-x86_64.AppImage`: self-contained,
  need only FUSE2 (or run with `--appimage-extract-and-run`) and the host NVIDIA
  driver (the AppImage resolves `libcuda.so.1` / `libnvidia-encode.so.1` from the
  FHS driver dirs; see `flake.nix` `postFixup`).

A Flathub Flatpak of `ord-ui` (driver-matched GL via the `GL.nvidia` extension +
PipeWire portal) is the planned next step for the GUI; the daemon, HUD, and CLI
stay out of a sandbox by design.
