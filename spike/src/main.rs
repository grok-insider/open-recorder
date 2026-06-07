// Phase-1 spike: validate that waycap-rs delivers zero-copy DMA-BUF capture +
// NVENC hardware-encoded frames on the NVIDIA 610 open driver, and that we can
// mux them to a playable file. This is the gate for the whole project.
//
// It is intentionally throwaway: no error handling polish, no abstractions.
// What it must prove:
//   1. A NVENC capture session builds and starts (no CPU/VAAPI fallback).
//   2. Encoded video frames arrive over the channel.
//   3. Keyframes are flagged (is_keyframe) — required for "save last N".
//   4. The frames mux into a valid file (checked afterwards with ffprobe).
//
// Note: waycap-rs 3.0 only exposes H264Nvenc / H264Vaapi (no HEVC). We validate
// the real NVENC path here; HEVC/AV1 is a later waycap-rs fork concern.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use waycap_rs::{
    pipeline::builder::CaptureBuilder,
    types::{
        config::{QualityPreset, VideoEncoder},
        video_frame::EncodedVideoFrame,
    },
    Capture, DynamicEncoder,
};

const SECONDS: u64 = 8;
const OUT: &str = "spike_out.mkv";

fn main() -> waycap_rs::types::error::Result<()> {
    simple_logging::log_to_stderr(log::LevelFilter::Info);

    eprintln!("== open-recorder spike: waycap-rs NVENC validation ==");
    eprintln!("Building NVENC (H264) capture session...");

    let mut capture = CaptureBuilder::new()
        .with_video_encoder(VideoEncoder::H264Nvenc)
        .with_quality_preset(QualityPreset::High)
        .with_cursor_shown()
        .build()?;

    let video_recv = capture.get_video_receiver();

    let frames = Arc::new(Mutex::new(BTreeMap::<i64, EncodedVideoFrame>::new()));
    let keyframes = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let frames_w = Arc::clone(&frames);
    let kf_w = Arc::clone(&keyframes);
    let tot_w = Arc::clone(&total);
    let by_w = Arc::clone(&bytes);
    let stop_w = Arc::clone(&stop);

    let collector = std::thread::spawn(move || {
        while !stop_w.load(Ordering::Acquire) {
            match video_recv.recv_timeout(Duration::from_millis(100)) {
                Ok(frame) => {
                    tot_w.fetch_add(1, Ordering::Relaxed);
                    by_w.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                    if frame.is_keyframe {
                        kf_w.fetch_add(1, Ordering::Relaxed);
                    }
                    frames_w.lock().unwrap().insert(frame.dts, frame);
                }
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    eprintln!("Starting capture for {SECONDS}s (a portal prompt may appear)...");
    capture.start()?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(SECONDS) {
        std::thread::sleep(Duration::from_millis(100));
    }

    stop.store(true, Ordering::Release);
    let _ = collector.join();

    let total = total.load(Ordering::Relaxed);
    let keyframes = keyframes.load(Ordering::Relaxed);
    let bytes = bytes.load(Ordering::Relaxed);

    eprintln!("---");
    eprintln!("frames captured : {total}");
    eprintln!("keyframes       : {keyframes}");
    eprintln!("encoded bytes   : {bytes} ({:.1} MiB)", bytes as f64 / 1048576.0);
    eprintln!("avg fps         : {:.1}", total as f64 / SECONDS as f64);

    if total == 0 {
        eprintln!("FAIL: no encoded frames arrived — capture/encode path is broken.");
        std::process::exit(2);
    }
    if keyframes == 0 {
        eprintln!("FAIL: no keyframes flagged — save-last-N would be impossible.");
        std::process::exit(3);
    }

    eprintln!("Muxing {OUT}...");
    let guard = frames.lock().unwrap();
    save(OUT, &guard, &capture)?;
    eprintln!("OK: wrote {OUT}. Validate with: ffprobe -hide_banner {OUT}");

    Ok(())
}

fn save(
    filename: &str,
    video: &BTreeMap<i64, EncodedVideoFrame>,
    capture: &Capture<DynamicEncoder>,
) -> waycap_rs::types::error::Result<()> {
    let mut output = ffmpeg_next::format::output(&filename)?;

    capture.with_video_encoder(|enc| {
        if let Some(encoder) = enc {
            let codec = encoder.codec().unwrap();
            let mut stream = output.add_stream(codec).unwrap();
            stream.set_time_base(encoder.time_base());
            stream.set_parameters(encoder);
        }
    });

    output.write_header()?;

    let first_pts = video.values().next().map(|f| f.pts).unwrap_or(0);
    for frame in video.values() {
        let mut packet = ffmpeg_next::codec::packet::Packet::copy(&frame.data);
        packet.set_pts(Some(frame.pts - first_pts));
        packet.set_dts(Some(frame.dts - first_pts));
        packet.set_stream(0);
        packet.write_interleaved(&mut output)?;
    }

    output.write_trailer()?;
    Ok(())
}
