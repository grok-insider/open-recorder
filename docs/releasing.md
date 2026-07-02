# Versioning & releasing

open-recorder uses **SemVer**, with the workspace `Cargo.toml` as the single
source of truth and **git tags** (`vX.Y.Z`) as the release markers. While the
project is pre-1.0, the minor version moves on feature drops and the patch on
fixes; breaking wire/config changes are called out via the protocol version.

## The three version surfaces

| Surface | Where | Notes |
|---|---|---|
| **Package version** | `[workspace.package] version` in `Cargo.toml` | Every crate inherits it (`version.workspace = true`). The flake reads it (`flake.nix`: `version = importTOML ./Cargo.toml`), so package + store-path names track it automatically. |
| **Binary `--version`** | `ord --version`, `ordd --version` | Prints `X.Y.Z [protocol N]`, plus the short git rev `(abc1234)` for local `cargo` builds. Nix builds omit the rev (no `.git` in the flake source) so they stay reproducible. Implemented in `ord-common` (`version.rs` + `build.rs`). |
| **Wire protocol** | `PROTOCOL_VERSION` in `ord-common/src/frame.rs` | Bumped on any incompatible `Command`/`Event` change. The framing layer rejects peer skew loudly (a stale `ord`/`ordd`/`ord-hud`), so `ord` and `ordd` from different releases refuse to talk instead of mis-decoding. |

## Cutting a release (automated with release-plz)

Releases are driven by [release-plz](https://release-plz.dev)
(`release-plz.toml` + `.github/workflows/release.yml`). You do **not** bump the
version, edit `CHANGELOG.md`, or tag by hand — a clear [Conventional
Commit](https://www.conventionalcommits.org) drives all three.

1. Land `feat:`/`fix:`/… PRs on `master` as usual. If `Command`/`Event` shapes
   changed incompatibly, bump `PROTOCOL_VERSION` (`ord-common/src/frame.rs`) in
   that same commit.
2. release-plz keeps a **release PR** open (`chore: release vX.Y.Z`) that bumps
   the single `[workspace.package].version` (every crate inherits it via
   `version.workspace = true`), refreshes `Cargo.lock`, and regenerates
   `CHANGELOG.md` from the commits since the last `v*` tag. `ord-cli` is the
   release unit; the other six crates fold in via `changelog_include`.
3. **Merge the release PR to ship.** `release-plz-release` tags `vX.Y.Z` and
   creates the GitHub Release, then two artifact jobs attach prebuilt binaries:
   - `upload-ord` — the portable **static `ord`** client (`x86_64` musl), for
     PATH installs / compositor keybinds.
   - `upload-appimages` — **`ordd` / `ord-hud` / `ord-ui` AppImages**, bundled
     from the flake (`nix bundle`), for non-Nix Linux users.
4. The same master push makes `ci.yml`'s `build` job push the closures to
   `grok-insider.cachix.org` (it reads the version from `Cargo.toml`), so flake
   consumers substitute `open-recorder-X.Y.Z` instead of compiling. Cachix is
   deliberately handled in `ci.yml`, not `release.yml`.

Nothing is published to crates.io (`git_only = true` + `publish = false` in
`release-plz.toml`). Do **not** set `publish = false` in the Cargo manifests:
release-plz skips manifest-unpublishable packages entirely — including their
git tag + GitHub Release — so a manifest-level flag silently turns
`release-plz release` into "nothing to release" (verified against 0.3.159).

The pipeline is live: the repo's *"Allow GitHub Actions to create and approve
pull requests"* setting is enabled, and release-plz cut **v0.3.0** through it
(release PRs #2/#3). The `CACHIX_AUTH_TOKEN` secret and the `v*` tags exist.

**AI-changelog enrichment.** After release-plz opens/updates the release PR,
`release.yml` runs `grok-insider/release-changelog-action`, which rewrites that
PR's `CHANGELOG.md` entry with user-facing, AI-written notes (overwriting the
git-cliff baseline) and commits it back to the PR branch.

## Manual release PR (current procedure)

`release-plz release-pr` is **broken upstream for this workspace**
(release-plz/release-plz#2651-adjacent): in git-only mode it reconstructs the
last tag's worktree and runs `cargo package` on it, which tries to resolve the
git-pinned `waycap-rs` fork against crates.io (`^3.0.0` does not exist there)
and fails. The `release-plz-pr` job in `release.yml` is therefore
`continue-on-error`, and release PRs are opened by hand until the upstream
source-diff fallback lands. The **release step is unaffected**:
`release-plz release` only checks the current workspace version against the
`v*` tags and cuts the tag + GitHub Release (proven by v0.3.0 and v0.4.0).

To cut a release:

1. Branch from master, bump `[workspace.package].version` in the root
   `Cargo.toml` (Conventional-Commits semantics: `feat!`/`BREAKING CHANGE` →
   minor while 0.x, `feat` → minor, `fix` → patch).
2. `cargo update --workspace` to refresh `Cargo.lock`.
3. Add the `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md` (Keep a
   Changelog form, summarized from the Conventional Commits since the last
   tag). This is the one sanctioned hand-edit of the changelog.
4. Open the PR titled `chore: release vX.Y.Z`, let CI pass, merge.
5. The master push runs `release-plz release`: tag + GitHub Release; the
   `upload-ord`/`upload-appimages` jobs attach the binaries and ci.yml's
   `build` job pushes the closures to cachix.

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
