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

## Cutting a release

1. Decide the bump (SemVer): feature → minor, fix → patch (pre-1.0).
2. If `Command`/`Event` shapes changed incompatibly, bump `PROTOCOL_VERSION`.
3. Set `[workspace.package] version` in `Cargo.toml`; run a build so `Cargo.lock`
   updates.
4. `cargo fmt --all` · `cargo clippy --all-targets --all-features -- -D warnings`
   (devshell) · `cargo test --all` must be green.
5. Commit (`chore(release): vX.Y.Z`), then tag: `git tag -a vX.Y.Z -m vX.Y.Z`.
6. `git push && git push --tags`. CI builds on both the `master` push and the
   `v*` tag (`.github/workflows/ci.yml`) and pushes the closures to
   `0xfell.cachix.org`, so flake consumers substitute instead of compiling.

## How a consumer updates

The NixOS/Home-Manager config pins `open-recorder.url = "github:0xfell/open-recorder"`.
To move to a new release:

```sh
nix flake update open-recorder        # or: nix flake lock --update-input open-recorder
nixos-rebuild switch --flake .         # (or home-manager switch)
ord --version                          # verify the installed version
```

Pinning to a tag instead of `master` is `github:0xfell/open-recorder/vX.Y.Z`.

## Future automation (not yet wired)

The repo already uses Conventional Commits, so `git-cliff` (CHANGELOG) or
`release-please` (version-bump PRs from commit history) could be added later. For
now the bump + tag is manual, which keeps the release surface small.
