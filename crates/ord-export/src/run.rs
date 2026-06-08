//! Execute an export: probe the input, plan the ffmpeg invocation, and run it.
//!
//! This is the only module that touches the filesystem and spawns processes.
//! On an NVENC failure for a re-encode, it transparently retries once with the
//! software encoder so an export never hard-fails just because the GPU session
//! could not be acquired.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::plan::{build_plan, Trim};
use crate::probe::probe;
use crate::profile::ExportProfile;
use crate::ExportError;

/// Result of a successful export.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportSummary {
    pub output: PathBuf,
    pub size_bytes: u64,
    /// The video encoder used (empty for a stream copy).
    pub encoder: String,
    pub used_hardware: bool,
    /// Duration of the exported clip in seconds.
    pub duration_secs: f64,
}

/// The `ffmpeg` binary, overridable via `ORD_FFMPEG` (e.g. a Nix store path).
fn ffmpeg_bin() -> String {
    std::env::var("ORD_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string())
}

/// Export `input` to `output` per `profile`, optionally trimmed.
pub fn export(
    input: &Path,
    output: &Path,
    profile: &ExportProfile,
    trim: Option<Trim>,
) -> Result<ExportSummary, ExportError> {
    let src = probe(input)?;
    let duration = trim.map(|t| t.duration()).unwrap_or(src.duration_secs);

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| ExportError::Io(e.to_string()))?;
        }
    }

    let input_s = input.to_string_lossy().to_string();
    let output_s = output.to_string_lossy().to_string();

    // Attempt the profile as configured (hardware if requested).
    let plan = build_plan(&input_s, &output_s, profile, &src, trim, true)?;
    let first = run_ffmpeg(&plan.args);

    let (encoder, used_hardware) = match first {
        Ok(()) => (plan.encoder, plan.uses_hardware),
        Err(e) => {
            // Retry in software only if the failed attempt actually used the GPU
            // for a re-encode; a copy or already-software run has nothing to gain.
            if plan.uses_hardware && profile.reencodes() {
                let sw = build_plan(&input_s, &output_s, profile, &src, trim, false)?;
                run_ffmpeg(&sw.args)?;
                (sw.encoder, sw.uses_hardware)
            } else {
                return Err(e);
            }
        }
    };

    let size_bytes = std::fs::metadata(output)
        .map_err(|e| ExportError::Io(format!("output not written: {e}")))?
        .len();

    Ok(ExportSummary {
        output: output.to_path_buf(),
        size_bytes,
        encoder,
        used_hardware,
        duration_secs: duration,
    })
}

/// Run ffmpeg with `args`, capturing stderr for diagnostics.
fn run_ffmpeg(args: &[String]) -> Result<(), ExportError> {
    let out = Command::new(ffmpeg_bin())
        .args(args)
        .output()
        .map_err(|e| ExportError::Spawn(e.to_string()))?;

    if out.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: String = {
        let trimmed = stderr.trim_end();
        let start = trimmed.len().saturating_sub(2000);
        trimmed[start..].to_string()
    };
    Err(ExportError::Ffmpeg {
        code: out.status.code(),
        stderr_tail: tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_ffmpeg_binary_is_a_spawn_error() {
        std::env::set_var("ORD_FFMPEG", "/nonexistent/ffmpeg-xyz");
        let err = run_ffmpeg(&["-version".to_string()]).unwrap_err();
        std::env::remove_var("ORD_FFMPEG");
        assert!(matches!(err, ExportError::Spawn(_)));
    }
}
