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

Nothing is published to crates.io (`git_only = true`).

**One-time setup:** enable *Settings → Actions → General → "Allow GitHub Actions
to create and approve pull requests"*. The `CACHIX_AUTH_TOKEN` secret and the
baseline `v*` tags already exist, so the next bump is computed automatically.

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
