//! Offline tests for the export runner: `ORD_FFMPEG`/`ORD_FFPROBE` point at
//! shell scripts, so the fallback/cancel/cleanup logic runs in plain CI with
//! no ffmpeg, GPU, or network.
//!
//! Env vars are process-global and cargo runs tests on parallel threads, so
//! every test serializes on [`ENV_LOCK`] and the overrides are set exactly
//! once (same value for the whole binary) by [`fake_tools`]. Per-test behavior
//! is selected by a `mode` file next to the output path, never by re-pointing
//! the env vars.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ord_export::profile::ExportProfile;
use ord_export::{export_with, ExportError};

static ENV_LOCK: Mutex<()> = Mutex::new(());
static TOOLS: OnceLock<PathBuf> = OnceLock::new();

const FAKE_FFPROBE: &str = r#"#!/bin/sh
cat <<'EOF'
{"streams":[{"codec_type":"video","codec_name":"h264","width":1920,"height":1080,"r_frame_rate":"60/1"},{"codec_type":"audio","codec_name":"opus"}],"format":{"duration":"3.0"}}
EOF
"#;

const FAKE_FFMPEG: &str = r#"#!/bin/sh
for last; do :; done
out="$last"
dir=$(dirname "$out")
echo "call $*" >> "$dir/calls"
mode=$(cat "$dir/mode" 2>/dev/null || echo ok)
case "$mode" in
  hw_retry)
    case "$*" in
      *libsvtav1*)
        echo "out_time_us=1500000"
        printf 'sw-encoded' > "$out"
        exit 0
        ;;
      *)
        printf 'hw-garbage' > "$out"
        echo "[av1_nvenc @ 0x5602] OpenEncodeSessionEx failed: out of memory (10)" >&2
        exit 1
        ;;
    esac
    ;;
  disk_full)
    printf 'partial' > "$out"
    echo "av_interleaved_write_frame(): No space left on device" >&2
    exit 1
    ;;
  hang)
    printf 'partial' > "$out"
    sleep 30
    exit 0
    ;;
  progress)
    echo "out_time_us=600000"
    echo "out_time_us=1500000"
    echo "out_time_us=3000000"
    printf 'encoded' > "$out"
    exit 0
    ;;
  *)
    printf 'encoded' > "$out"
    exit 0
    ;;
esac
"#;

fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn fake_tools() -> &'static Path {
    TOOLS.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("ord-export-fake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_script(&dir.join("ffmpeg"), FAKE_FFMPEG);
        write_script(&dir.join("ffprobe"), FAKE_FFPROBE);
        std::env::set_var("ORD_FFMPEG", dir.join("ffmpeg"));
        std::env::set_var("ORD_FFPROBE", dir.join("ffprobe"));
        dir
    })
}

struct Case {
    dir: PathBuf,
    input: PathBuf,
    output: PathBuf,
}

impl Case {
    fn new(name: &str, mode: &str) -> Self {
        fake_tools();
        let dir =
            std::env::temp_dir().join(format!("ord-export-case-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mode"), mode).unwrap();
        let input = dir.join("in.mkv");
        std::fs::write(&input, b"fake clip").unwrap();
        Self {
            output: dir.join("out.mp4"),
            dir,
            input,
        }
    }

    fn calls(&self) -> usize {
        std::fs::read_to_string(self.dir.join("calls"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

#[test]
fn nvenc_failure_retries_in_software_and_succeeds() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let case = Case::new("hw-retry", "hw_retry");
    let summary = export_with(
        &case.input,
        &case.output,
        &ExportProfile::high_quality(),
        None,
        &mut |_| {},
        &no_cancel(),
    )
    .unwrap();
    assert!(!summary.used_hardware);
    assert_eq!(summary.encoder, "libsvtav1");
    assert_eq!(case.calls(), 2, "hardware attempt then software retry");
    assert_eq!(std::fs::read(&case.output).unwrap(), b"sw-encoded");
}

#[test]
fn non_encoder_failure_surfaces_without_retry_and_deletes_output() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let case = Case::new("disk-full", "disk_full");
    let err = export_with(
        &case.input,
        &case.output,
        &ExportProfile::high_quality(),
        None,
        &mut |_| {},
        &no_cancel(),
    )
    .unwrap_err();
    match err {
        ExportError::Ffmpeg { stderr_tail, .. } => {
            assert!(
                stderr_tail.contains("No space left on device"),
                "{stderr_tail}"
            );
        }
        other => panic!("expected Ffmpeg error, got {other:?}"),
    }
    assert_eq!(case.calls(), 1, "must not retry a non-encoder failure");
    assert!(!case.output.exists(), "partial output must be deleted");
}

#[test]
fn cancel_kills_ffmpeg_that_emits_no_progress() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let case = Case::new("cancel", "hang");
    let cancel = AtomicBool::new(true);
    let started = Instant::now();
    let err = export_with(
        &case.input,
        &case.output,
        &ExportProfile::high_quality(),
        None,
        &mut |_| {},
        &cancel,
    )
    .unwrap_err();
    assert!(matches!(err, ExportError::Cancelled));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a wedged child must be killed promptly, took {:?}",
        started.elapsed()
    );
    assert!(
        case.calls() <= 1,
        "a cancellation is final, never retried (got {} calls)",
        case.calls()
    );
    assert!(!case.output.exists(), "cancelled output must be deleted");
}

#[test]
fn cancel_soon_after_start_kills_ffmpeg() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let case = Case::new("cancel-late", "hang");
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let err = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            cancel.store(true, Ordering::Relaxed);
        });
        export_with(
            &case.input,
            &case.output,
            &ExportProfile::high_quality(),
            None,
            &mut |_| {},
            &cancel,
        )
        .unwrap_err()
    });
    assert!(matches!(err, ExportError::Cancelled));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    assert!(!case.output.exists());
}

#[test]
fn progress_lines_drive_the_callback_monotonically() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let case = Case::new("progress", "progress");
    let mut fractions = Vec::new();
    export_with(
        &case.input,
        &case.output,
        &ExportProfile::high_quality(),
        None,
        &mut |f| fractions.push(f),
        &no_cancel(),
    )
    .unwrap();
    // duration 3.0s -> 0.6s/1.5s/3.0s land at 0.2/0.5/1.0, plus the final 1.0.
    assert!(fractions.len() >= 4, "got {fractions:?}");
    assert!(
        fractions.windows(2).all(|w| w[0] <= w[1]),
        "not monotonic: {fractions:?}"
    );
    assert!((fractions[0] - 0.2).abs() < 1e-9, "got {fractions:?}");
    assert_eq!(*fractions.last().unwrap(), 1.0, "got {fractions:?}");
}
