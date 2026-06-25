//! Best-effort embedding of the git revision into the build, surfaced by
//! `ord --version` / `ordd --version` via `ord_common::version`.
//!
//! This MUST never fail the build: in pure Nix builds the flake source has no
//! `.git` and `git` may be absent, so any error simply leaves `ORD_BUILD_REV`
//! unset and the version reports a clean SemVer. An optional `ORD_BUILD_REV`
//! env at build time wins (so packagers can inject a revision explicitly).

use std::process::Command;

fn main() {
    // Honor an explicit override (e.g. a packager passing the rev).
    if let Ok(rev) = std::env::var("ORD_BUILD_REV") {
        if !rev.is_empty() {
            println!("cargo:rustc-env=ORD_BUILD_REV={rev}");
            return;
        }
    }

    // Otherwise try git, best-effort. Re-run when HEAD moves.
    if std::path::Path::new("../../.git/HEAD").exists() {
        println!("cargo:rerun-if-changed=../../.git/HEAD");
    }
    println!("cargo:rerun-if-env-changed=ORD_BUILD_REV");

    if let Some(rev) = git_rev() {
        println!("cargo:rustc-env=ORD_BUILD_REV={rev}");
    }
}

/// Short HEAD revision plus a `-dirty` suffix when the tree has uncommitted
/// changes. `None` on any failure (no git, no repo, command error).
fn git_rev() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut rev = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if rev.is_empty() {
        return None;
    }
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    if dirty {
        rev.push_str("-dirty");
    }
    Some(rev)
}
