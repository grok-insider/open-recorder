//! `ord doctor` — diagnose and fix the NVIDIA CUDA/NVENC "P2 state" downclock.
//!
//! When any process uses CUDA (NVENC uses CUDA) the NVIDIA driver can pin the
//! GPU to the P2 performance state, downclocking VRAM and throttling *both* the
//! game and the encoder. NVIDIA whitelists ShadowPlay in-driver; on Linux the
//! remedy is an application profile that clears the stable-perf limit for our
//! process — exactly what gpu-screen-recorder ships. Requires driver >= 580.
//!
//! The profile is the only thing `ord` writes under `~/.nv`, and `--fix` prints
//! precisely what it changes. The parsing/JSON building is pure and unit-tested;
//! only `run` shells out to `nvidia-smi` and touches disk.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Process name the profile rule matches — the daemon binary.
pub const APP_PROCNAME: &str = "ordd";

/// Profile name inside the JSON document.
const PROFILE_NAME: &str = "ordd-cuda-no-stable-perf-limit";

/// Installed filename. The `10-` prefix orders it among other `*-rc.d` entries.
const PROFILE_FILE: &str = "10-ordd-cuda-no-stable-perf-limit";

/// Minimum NVIDIA driver major version that honors the `CudaNoStablePerfLimit`
/// key (`0x166c5e`). Older drivers need the Vulkan-encode path instead.
pub const MIN_DRIVER_MAJOR: u32 = 580;

/// Build the NVIDIA application-profile JSON that lets `procname` reach P0 under
/// CUDA/NVENC. Mirrors gpu-screen-recorder / CachyOS's profile (raw key
/// `0x166c5e` = 0, i.e. "do not force the CUDA P2 state"), scoped to our process
/// by a `procname` rule. Pure.
pub fn profile_json(procname: &str) -> String {
    let doc = serde_json::json!({
        "profiles": [
            {
                "name": PROFILE_NAME,
                "settings": [
                    { "key": "0x166c5e", "value": 0 }
                ]
            }
        ],
        "rules": [
            {
                "pattern": { "feature": "procname", "matches": procname },
                "profile": PROFILE_NAME
            }
        ]
    });
    // Pretty-print with a trailing newline so the installed file is tidy.
    let mut s = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| doc.to_string());
    s.push('\n');
    s
}

/// Directory NVIDIA scans for per-user application profiles.
pub fn profile_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".nv/nvidia-application-profiles-rc.d")
}

/// Full path of the profile file `ord doctor --fix` writes.
pub fn profile_path() -> PathBuf {
    profile_dir().join(PROFILE_FILE)
}

/// Parse the NVIDIA driver major version from `nvidia-smi
/// --query-gpu=driver_version --format=csv,noheader` output (e.g. `"580.65.06"`
/// -> `Some(580)`). Pure.
pub fn parse_driver_major(s: &str) -> Option<u32> {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty())?;
    line.split('.').next()?.parse().ok()
}

/// Parse the performance state (`P0`..`P12`) from `nvidia-smi -q -d PERFORMANCE`
/// output, returning the numeric state (`P2` -> `Some(2)`). Pure.
pub fn parse_perf_state(s: &str) -> Option<u8> {
    for line in s.lines() {
        // Lines look like: "        Performance State          : P2"
        let lower = line.to_ascii_lowercase();
        if lower.contains("performance state") {
            let val = line.rsplit(':').next()?.trim();
            let digits = val.strip_prefix(['P', 'p'])?;
            return digits.parse().ok();
        }
    }
    None
}

/// A snapshot of the NVIDIA state relevant to the P2 downclock. Pure data so the
/// reporting can be unit-tested without a GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    /// Driver major version, if `nvidia-smi` was available.
    pub driver_major: Option<u32>,
    /// Current performance state, if queryable (`2` = the throttled P2 state).
    pub perf_state: Option<u8>,
    /// Whether the profile file is already installed.
    pub profile_installed: bool,
}

impl Diagnosis {
    /// Whether the driver is new enough for the profile key to take effect.
    pub fn driver_supported(&self) -> bool {
        self.driver_major.is_some_and(|v| v >= MIN_DRIVER_MAJOR)
    }

    /// Whether the GPU currently looks pinned to the throttled P2 state.
    pub fn p2_throttled(&self) -> bool {
        self.perf_state == Some(2)
    }
}

/// Render the human-readable report for a diagnosis. Pure.
pub fn render(d: &Diagnosis, profile_path: &Path) -> String {
    let mut out = String::from("open-recorder doctor — NVIDIA CUDA/NVENC P2 downclock\n\n");

    match d.driver_major {
        Some(v) if v >= MIN_DRIVER_MAJOR => {
            out.push_str(&format!("  driver        : {v}.x (>= {MIN_DRIVER_MAJOR}, supported)\n"));
        }
        Some(v) => out.push_str(&format!(
            "  driver        : {v}.x (< {MIN_DRIVER_MAJOR}; profile won't apply — use Vulkan encode)\n"
        )),
        None => out.push_str("  driver        : nvidia-smi not found (no NVIDIA GPU?)\n"),
    }

    match d.perf_state {
        Some(2) => {
            out.push_str("  perf state    : P2 (throttled — VRAM downclocked under CUDA/NVENC)\n")
        }
        Some(s) => out.push_str(&format!("  perf state    : P{s}\n")),
        None => out.push_str("  perf state    : unknown\n"),
    }

    out.push_str(&format!(
        "  profile       : {} at {}\n",
        if d.profile_installed {
            "installed"
        } else {
            "NOT installed"
        },
        profile_path.display()
    ));

    out.push('\n');
    if !d.profile_installed && d.driver_supported() {
        out.push_str("  -> run `ord doctor --fix` to install the profile so ordd reaches P0.\n");
    } else if d.profile_installed {
        if d.p2_throttled() {
            out.push_str(
                "  -> profile present but GPU still in P2; restart ordd (and reboot once) to apply.\n",
            );
        } else {
            out.push_str("  -> profile present and GPU not pinned to P2. All good.\n");
        }
    } else if !d.driver_supported() {
        out.push_str(
            "  -> driver too old for the profile; record with the Vulkan encoder instead.\n",
        );
    }
    out
}

/// Run `nvidia-smi` with the given args and return stdout, or `None` if the
/// command is missing or fails.
fn nvidia_smi(args: &[&str]) -> Option<String> {
    let out = Command::new("nvidia-smi").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Gather a [`Diagnosis`] from the live system (best-effort; missing tools just
/// leave fields `None`).
fn diagnose() -> Diagnosis {
    let driver_major = nvidia_smi(&["--query-gpu=driver_version", "--format=csv,noheader"])
        .as_deref()
        .and_then(parse_driver_major);
    let perf_state = nvidia_smi(&["-q", "-d", "PERFORMANCE"])
        .as_deref()
        .and_then(parse_perf_state);
    Diagnosis {
        driver_major,
        perf_state,
        profile_installed: profile_path().exists(),
    }
}

/// Write the profile file, creating `~/.nv/nvidia-application-profiles-rc.d`.
fn install_profile() -> Result<PathBuf, String> {
    let dir = profile_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let path = profile_path();
    std::fs::write(&path, profile_json(APP_PROCNAME))
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path)
}

/// Entry point for `ord doctor [--fix]`.
pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut fix = false;
    for arg in args.by_ref() {
        match arg.as_str() {
            "--fix" => fix = true,
            "-h" | "--help" => {
                println!("ord doctor [--fix]\n\n  Diagnose the NVIDIA CUDA/NVENC P2 downclock.\n  --fix  install the application profile that lets ordd reach P0.");
                return Ok(());
            }
            other => return Err(format!("unknown doctor flag: {other}")),
        }
    }

    let diag = diagnose();
    print!("{}", render(&diag, &profile_path()));

    if fix {
        if !diag.driver_supported() {
            // Still install (harmless) but warn it won't help on this driver.
            eprintln!("warning: driver < {MIN_DRIVER_MAJOR}; the profile won't take effect here.");
        }
        let path = install_profile()?;
        println!("\ninstalled profile -> {}", path.display());
        println!("restart ordd (and reboot once) for the NVIDIA driver to pick it up.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_json_is_valid_and_scoped() {
        let json = profile_json("ordd");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        // The setting clears the forced P2 state.
        assert_eq!(v["profiles"][0]["settings"][0]["key"], "0x166c5e");
        assert_eq!(v["profiles"][0]["settings"][0]["value"], 0);
        // The rule matches our process by name.
        assert_eq!(v["rules"][0]["pattern"]["feature"], "procname");
        assert_eq!(v["rules"][0]["pattern"]["matches"], "ordd");
        // Profile name links rule -> profile.
        assert_eq!(v["rules"][0]["profile"], v["profiles"][0]["name"]);
    }

    #[test]
    fn parse_driver_major_extracts_first_component() {
        assert_eq!(parse_driver_major("580.65.06\n"), Some(580));
        assert_eq!(parse_driver_major("  610.43.02  "), Some(610));
        assert_eq!(parse_driver_major("\n\n550.120\n"), Some(550));
        assert_eq!(parse_driver_major(""), None);
        assert_eq!(parse_driver_major("garbage"), None);
    }

    #[test]
    fn parse_perf_state_reads_pstate() {
        let q = "    Performance State          : P2\n";
        assert_eq!(parse_perf_state(q), Some(2));
        assert_eq!(parse_perf_state("Performance State : P0"), Some(0));
        assert_eq!(parse_perf_state("Performance State : P12"), Some(12));
        assert_eq!(parse_perf_state("nothing here"), None);
    }

    #[test]
    fn driver_support_threshold() {
        let mk = |maj| Diagnosis {
            driver_major: maj,
            perf_state: None,
            profile_installed: false,
        };
        assert!(mk(Some(580)).driver_supported());
        assert!(mk(Some(610)).driver_supported());
        assert!(!mk(Some(550)).driver_supported());
        assert!(!mk(None).driver_supported());
    }

    #[test]
    fn p2_detection() {
        let mk = |ps| Diagnosis {
            driver_major: Some(610),
            perf_state: ps,
            profile_installed: false,
        };
        assert!(mk(Some(2)).p2_throttled());
        assert!(!mk(Some(0)).p2_throttled());
        assert!(!mk(None).p2_throttled());
    }

    #[test]
    fn render_recommends_fix_when_missing_and_supported() {
        let d = Diagnosis {
            driver_major: Some(610),
            perf_state: Some(2),
            profile_installed: false,
        };
        let r = render(&d, &PathBuf::from("/tmp/p"));
        assert!(r.contains("--fix"), "{r}");
        assert!(r.contains("P2"), "{r}");
    }

    #[test]
    fn render_notes_old_driver() {
        let d = Diagnosis {
            driver_major: Some(550),
            perf_state: None,
            profile_installed: false,
        };
        let r = render(&d, &PathBuf::from("/tmp/p"));
        assert!(r.contains("Vulkan"), "{r}");
    }
}
