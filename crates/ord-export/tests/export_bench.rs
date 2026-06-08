//! Export benchmark: time + output size per preset, so encoder quality/perf
//! changes are quantifiable. `#[ignore]`d (needs ffmpeg + NVENC). Run:
//!
//! ```sh
//! nix develop -c cargo test -p ord-export --test export_bench -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use ord_export::export;
use ord_export::profile::ExportProfile;

fn gen_clip(secs: u32) -> PathBuf {
    let path = std::env::temp_dir().join(format!("ord-export-bench-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size=2560x1440:rate=60:duration={secs}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={secs}"),
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "60",
            "-c:a",
            "libopus",
        ])
        .arg(&path)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
    path
}

#[test]
#[ignore = "needs ffmpeg + NVENC; run in devshell with --ignored --nocapture"]
fn bench_export_presets() {
    let secs = 10;
    let clip = gen_clip(secs);

    println!("\n=== ord-export benchmark (2560x1440x60, {secs}s source) ===");
    println!(
        "{:<14} {:>9} {:>10} {:>10} {:>9}",
        "preset", "time", "size", "encoder", "x-rt"
    );

    let cases: &[(&str, ExportProfile)] = &[
        ("high-quality", ExportProfile::high_quality()),
        ("discord", ExportProfile::discord()),
        (
            "source",
            ExportProfile::source(ord_common::config::Container::Mkv),
        ),
    ];

    for (name, profile) in cases {
        let out = std::env::temp_dir().join(format!("ord-export-bench-out-{name}.mp4"));
        let _ = std::fs::remove_file(&out);
        let t0 = Instant::now();
        match export(&clip, &out, profile, None) {
            Ok(s) => {
                let secs_el = t0.elapsed().as_secs_f64();
                let mib = s.size_bytes as f64 / (1024.0 * 1024.0);
                let xrt = secs as f64 / secs_el.max(1e-6);
                let enc = if s.encoder.is_empty() {
                    "copy".to_string()
                } else {
                    s.encoder.clone()
                };
                println!(
                    "{:<14} {:>7.2}s {:>8.1}MiB {:>10} {:>7.1}x",
                    name, secs_el, mib, enc, xrt
                );
            }
            Err(e) => println!("{name:<14} FAILED: {e}"),
        }
        let _ = std::fs::remove_file(&out);
    }
    let _ = std::fs::remove_file(&clip);
    println!("(x-rt = encode speed vs real-time; higher is faster)\n");
}
