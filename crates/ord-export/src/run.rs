//! Execute an export: probe the input, plan the ffmpeg invocation, and run it.
//!
//! This is the only module that touches the filesystem and spawns processes.
//! On an NVENC failure for a re-encode, it transparently retries once with the
//! software encoder so an export never hard-fails just because the GPU session
//! could not be acquired.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::plan::{build_plan, build_segments_plan, FfmpegPlan, Trim};
use crate::probe::probe;
use crate::profile::ExportProfile;
use crate::{ffmpeg_bin, ExportError};

/// A cancel flag that never trips (for the simple [`export`] path).
fn never_cancel() -> &'static AtomicBool {
    static NEVER: AtomicBool = AtomicBool::new(false);
    &NEVER
}

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

/// Export `input` to `output` per `profile`, optionally trimmed (blocking, no
/// progress). Convenience wrapper over [`export_with`].
pub fn export(
    input: &Path,
    output: &Path,
    profile: &ExportProfile,
    trim: Option<Trim>,
) -> Result<ExportSummary, ExportError> {
    export_with(input, output, profile, trim, &mut |_| {}, never_cancel())
}

/// Export with live progress reporting and cancellation.
///
/// `on_progress` is called with a fraction in `0.0..=1.0` as ffmpeg makes
/// progress (parsed from `-progress pipe:1`). Setting `cancel` kills the running
/// ffmpeg and yields [`ExportError::Cancelled`] (no partial file is left behind).
pub fn export_with(
    input: &Path,
    output: &Path,
    profile: &ExportProfile,
    trim: Option<Trim>,
    on_progress: &mut dyn FnMut(f64),
    cancel: &AtomicBool,
) -> Result<ExportSummary, ExportError> {
    let src = probe(input)?;
    let duration = trim.map(|t| t.duration()).unwrap_or(src.duration_secs);
    let input_s = input.to_string_lossy().to_string();
    let output_s = output.to_string_lossy().to_string();
    execute(
        output,
        profile,
        duration,
        &|hw| build_plan(&input_s, &output_s, profile, &src, trim, hw),
        on_progress,
        cancel,
    )
}

/// Export several windows of `input` concatenated into one `output` (the
/// editor's multi-segment cuts), with the same progress/cancel/fallback
/// behavior as [`export_with`]. Segments must be ascending, non-overlapping.
pub fn export_segments_with(
    input: &Path,
    output: &Path,
    profile: &ExportProfile,
    segments: &[Trim],
    on_progress: &mut dyn FnMut(f64),
    cancel: &AtomicBool,
) -> Result<ExportSummary, ExportError> {
    let src = probe(input)?;
    let duration = segments.iter().map(|t| t.duration()).sum();
    let input_s = input.to_string_lossy().to_string();
    let output_s = output.to_string_lossy().to_string();
    execute(
        output,
        profile,
        duration,
        &|hw| build_segments_plan(&input_s, &output_s, profile, &src, segments, hw),
        on_progress,
        cancel,
    )
}

/// Run a planned export with the NVENC→software fallback and corrupt-output
/// cleanup. `build` produces the plan for hardware (`true`) or software.
fn execute(
    output: &Path,
    profile: &ExportProfile,
    duration: f64,
    build: &dyn Fn(bool) -> Result<FfmpegPlan, ExportError>,
    on_progress: &mut dyn FnMut(f64),
    cancel: &AtomicBool,
) -> Result<ExportSummary, ExportError> {
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| ExportError::Io(e.to_string()))?;
        }
    }

    // From here ffmpeg (run with `-y`) may create `output`. If any step in this
    // phase fails, the file on disk is truncated/corrupt — remove it so a caller
    // (the UI library) never lists a broken clip.
    let phase = (|| -> Result<(String, bool, u64), ExportError> {
        // Attempt the profile as configured (hardware if requested).
        let plan = build(true)?;
        let (encoder, used_hardware) = match run_ffmpeg(&plan.args, duration, on_progress, cancel) {
            Ok(()) => (plan.encoder, plan.uses_hardware),
            // A cancellation is final — never retry it in software.
            Err(ExportError::Cancelled) => return Err(ExportError::Cancelled),
            Err(e) => {
                // Retry in software only if the failed attempt actually used the
                // GPU for a re-encode AND the failure looks like a hardware-encoder
                // problem; a disk-full or bad-path error would fail identically in
                // software, so re-encoding the whole clip first just wastes time.
                let hw_failure = matches!(
                    &e,
                    ExportError::Ffmpeg { stderr_tail, .. } if is_hw_encoder_failure(stderr_tail)
                );
                if plan.uses_hardware && profile.reencodes() && hw_failure {
                    let sw = build(false)?;
                    run_ffmpeg(&sw.args, duration, on_progress, cancel)?;
                    (sw.encoder, sw.uses_hardware)
                } else {
                    return Err(e);
                }
            }
        };
        let size_bytes = std::fs::metadata(output)
            .map_err(|e| ExportError::Io(format!("output not written: {e}")))?
            .len();
        Ok((encoder, used_hardware, size_bytes))
    })();

    let (encoder, used_hardware, size_bytes) = match phase {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(output);
            return Err(e);
        }
    };

    Ok(ExportSummary {
        output: output.to_path_buf(),
        size_bytes,
        encoder,
        used_hardware,
        duration_secs: duration,
    })
}

/// Whether an ffmpeg stderr tail points at a hardware-encoder (NVENC/CUDA)
/// failure worth retrying in software. Signatures cover the real NVENC failure
/// modes: missing driver libraries, no capable GPU, exhausted encode sessions,
/// driver/API version mismatches, CUDA device/context errors, and unsupported
/// encoder features (e.g. `b_ref_mode` on older GPUs). Anything else — disk
/// full, bad paths, corrupt input — fails identically in software.
fn is_hw_encoder_failure(stderr_tail: &str) -> bool {
    const SIGNATURES: &[&str] = &[
        "nvenc",
        "cuda",
        "cannot load libnvidia-encode",
        "no capable devices",
        "device creation failed",
        "hwaccel",
        "b_ref_mode",
        "openencodesessionex",
        "nvml",
        "no nvidia devices",
    ];
    let lower = stderr_tail.to_ascii_lowercase();
    SIGNATURES.iter().any(|s| lower.contains(s))
}

/// Run ffmpeg with `args`, streaming progress and honoring cancellation.
///
/// Prepends `-progress pipe:1 -nostats` and parses the progress stream from
/// stdout on a dedicated thread (fraction = `out_time_us` / `duration`), so the
/// wait loop can poll `cancel` every ~100ms and kill a wedged child even when
/// no progress lines flow. Stderr is drained on its own thread to avoid a
/// pipe-full deadlock.
fn run_ffmpeg(
    args: &[String],
    duration: f64,
    on_progress: &mut dyn FnMut(f64),
    cancel: &AtomicBool,
) -> Result<(), ExportError> {
    let mut full: Vec<String> = vec!["-progress".into(), "pipe:1".into(), "-nostats".into()];
    full.extend_from_slice(args);

    let mut child = Command::new(ffmpeg_bin())
        .args(&full)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ExportError::Spawn(e.to_string()))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExportError::Spawn("ffmpeg stdout was not piped".into()))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ExportError::Spawn("ffmpeg stderr was not piped".into()))?;
    // Drain stderr on a thread so a full stderr pipe can't deadlock the export.
    let stderr_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });

    // Parse progress on its own thread; the channel disconnects when the pipe
    // closes. Never joined: the pipe can outlive a killed child (inherited by
    // an orphaned grandchild), and a blocked read must not wedge the export.
    let (progress_tx, progress_rx) = mpsc::channel::<i64>();
    let _progress_thread = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(us) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = us.trim().parse::<i64>() {
                    if progress_tx.send(us).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut cancelled = false;
    let mut exit_seen: Option<Instant> = None;
    loop {
        if cancel.load(Ordering::Relaxed) && !cancelled {
            let _ = child.kill();
            cancelled = true;
        }
        match progress_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(us) => {
                if !cancelled && duration > 0.0 {
                    on_progress((us as f64 / 1_000_000.0 / duration).clamp(0.0, 1.0));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Disconnection alone can't end the loop: the stdout pipe may
                // stay open past the child's death. Child exit ends it too,
                // after a short grace so in-flight progress still lands.
                if exit_seen.is_none() && matches!(child.try_wait(), Ok(Some(_))) {
                    exit_seen = Some(Instant::now());
                }
                if exit_seen.is_some_and(|t| t.elapsed() > Duration::from_millis(300)) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Wait for exit, still honoring a late cancel (e.g. progress stopped).
    let status = loop {
        if cancel.load(Ordering::Relaxed) && !cancelled {
            let _ = child.kill();
            cancelled = true;
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(ExportError::Spawn(e.to_string())),
        }
    };

    if cancelled {
        // Skip the stderr join: an orphan holding the pipe would block it, and
        // a cancellation needs no diagnostics.
        return Err(ExportError::Cancelled);
    }
    let stderr = stderr_thread.join().unwrap_or_default();
    if status.success() {
        on_progress(1.0);
        return Ok(());
    }
    Err(ExportError::Ffmpeg {
        code: status.code(),
        stderr_tail: stderr_tail(&stderr),
    })
}

/// The last ~2000 bytes of ffmpeg's stderr for diagnostics, snapped **forward** to
/// a UTF-8 char boundary. A naive byte slice panics when the cut lands inside a
/// multibyte sequence (e.g. a non-ASCII filename echoed in the error).
fn stderr_tail(stderr: &str) -> String {
    let trimmed = stderr.trim_end();
    let mut start = trimmed.len().saturating_sub(2000);
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    trimmed[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_ffmpeg_binary_is_a_spawn_error() {
        std::env::set_var("ORD_FFMPEG", "/nonexistent/ffmpeg-xyz");
        let err =
            run_ffmpeg(&["-version".to_string()], 0.0, &mut |_| {}, never_cancel()).unwrap_err();
        std::env::remove_var("ORD_FFMPEG");
        assert!(matches!(err, ExportError::Spawn(_)));
    }

    #[test]
    fn stderr_tail_snaps_multibyte_boundary() {
        // 3000 bytes of 3-byte chars: the "last 2000 bytes" cut lands inside a
        // character. The old naive byte slice panicked here.
        let s = "日".repeat(1000);
        let tail = stderr_tail(&s);
        assert!(!tail.is_empty());
        assert!(tail.len() <= 2000);
        assert!(tail.chars().all(|c| c == '日'));
    }

    #[test]
    fn stderr_tail_trims_and_keeps_short_input() {
        assert_eq!(stderr_tail("  boom\n\n"), "  boom");
    }

    #[test]
    fn hw_encoder_failures_match() {
        for tail in [
            "[av1_nvenc @ 0x5602] OpenEncodeSessionEx failed: out of memory (10): (no details)",
            "Cannot load libnvidia-encode.so.1",
            "[h264_nvenc @ 0x55aa] No capable devices found",
            "Device creation failed: -542398533.",
            "[AVHWDeviceContext @ 0x56] Could not initialize the CUDA driver API",
            "Error while opening encoder - maybe incorrect parameters such as bit_rate, rate, width or height. hwaccel initialisation returned error.",
            "[hevc_nvenc @ 0x55] B frames as references are not supported. b_ref_mode 2 is not supported.",
            "The minimum required Nvidia driver for nvenc is 570.0 or newer",
            "driver does not support the required nvenc API version. Required: 13.0 Found: 12.2",
        ] {
            assert!(is_hw_encoder_failure(tail), "should match: {tail}");
        }
    }

    #[test]
    fn non_encoder_failures_do_not_match() {
        for tail in [
            "av_interleaved_write_frame(): No space left on device",
            "out/clip.mp4: Permission denied",
            "in.mkv: No such file or directory",
            "Invalid data found when processing input",
            "Error writing trailer of out.mp4: I/O error",
            "",
        ] {
            assert!(!is_hw_encoder_failure(tail), "should not match: {tail}");
        }
    }
}
