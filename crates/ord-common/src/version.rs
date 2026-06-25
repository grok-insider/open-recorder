//! Build-version reporting, shared by `ord` and `ordd` so `--version` is
//! consistent across the binaries.
//!
//! The SemVer comes from the workspace `Cargo.toml` (`CARGO_PKG_VERSION`); the
//! optional short git revision is captured at build time by this crate's
//! `build.rs`. The revision is intentionally **absent in pure Nix builds** (the
//! flake source has no `.git`), which keeps those builds reproducible — a Nix
//! install reports a clean `0.2.0`, while a local `cargo` build also shows the
//! commit it was built from.

/// The workspace SemVer, e.g. `"0.2.0"`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The wire protocol version this build speaks (mirrors
/// [`crate::frame::PROTOCOL_VERSION`]). Surfaced in `--version` so a peer-skew
/// diagnosis is a glance away.
pub const PROTOCOL: u8 = crate::frame::PROTOCOL_VERSION;

/// A one-line version string for `--version`:
/// `"0.2.0 (a1b2c3d) [protocol 4]"`, or `"0.2.0 [protocol 4]"` in a clean/Nix
/// build with no embedded revision.
pub fn long() -> String {
    match option_env!("ORD_BUILD_REV") {
        Some(rev) if !rev.is_empty() => format!("{VERSION} ({rev}) [protocol {PROTOCOL}]"),
        _ => format!("{VERSION} [protocol {PROTOCOL}]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_cargo() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn long_contains_version_and_protocol() {
        let s = long();
        assert!(s.starts_with(VERSION), "{s}");
        assert!(s.contains(&format!("protocol {PROTOCOL}")), "{s}");
    }
}
