// Phase-1 spike: validate that waycap-rs delivers zero-copy DMA-BUF capture +
// NVENC encoded frames on the NVIDIA 610 open driver, and that ord-core's muxer
// turns them into a playable file. Exercises the REAL ord-core code paths
// (EncodedFrame, PreparedClip, write_clip) so the spike validates production code.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use ord_core::backend::{Codec, StreamParams};
use ord_core::engine::PreparedClip;
use ord_core::ring::EncodedFrame;
use waycap_rs::{
    pipeline::builder::CaptureBuilder,
    types::config::{QualityPreset, VideoEncoder},
};

const SECONDS: u64 = 8;
const OUT: &str = "spike_out.mkv";

fn main() -> waycap_rs::types::error::Result<()> {
    simple_logging::log_to_stderr(log::LevelFilter::Info);

    eprintln!("== open-recorder spike: waycap-rs NVENC + ord-core mux validation ==");

    let mut capture = CaptureBuilder::new()
        .with_video_encoder(VideoEncoder::H264Nvenc)
        .with_quality_preset(QualityPreset::High)
        .with_cursor_shown()
        .build()?;

    let video_recv = capture.get_video_receiver();
    let frames = Arc::new(Mutex::new(Vec::<EncodedFrame>::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let frames_w = Arc::clone(&frames);
    let stop_w = Arc::clone(&stop);
    let collector = std::thread::spawn(move || {
        while !stop_w.load(Ordering::Acquire) {
            match video_recv.recv_timeout(Duration::from_millis(100)) {
                Ok(f) => frames_w
                    .lock()
                    .unwrap()
                    .push(EncodedFrame::new(f.data, f.is_keyframe, f.pts, f.dts)),
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
    let _ = capture.finish();

    let frames = Arc::try_unwrap(frames).unwrap().into_inner().unwrap();
    let keyframes = frames.iter().filter(|f| f.is_keyframe).count();
    let bytes: usize = frames.iter().map(|f| f.len()).sum();
    eprintln!("frames: {}  keyframes: {}  bytes: {}", frames.len(), keyframes, bytes);

    if frames.is_empty() {
        eprintln!("FAIL: no frames captured");
        std::process::exit(2);
    }

    // Trim to start at the first keyframe (mirrors clip selection) so the clip is
    // decodable, then mux via the real ord-core writer.
    let first_kf = frames.iter().position(|f| f.is_keyframe).unwrap_or(0);
    let clip = PreparedClip {
        frames: frames[first_kf..].to_vec(),
        params: StreamParams {
            width: 2560,
            height: 1440,
            fps: 60,
            codec: Codec::H264,
            time_base_den: ord_core::backend::NANOS_PER_SEC, // waycap pts are nanos
        },
    };

    eprintln!("Muxing {OUT} via ord_core::write_clip ...");
    ord_core::write_clip(&clip, OUT).map_err(|e| {
        eprintln!("FAIL: mux error: {e}");
        std::process::exit(3);
    }).ok();
    eprintln!("OK: wrote {OUT}");
    Ok(())
}
