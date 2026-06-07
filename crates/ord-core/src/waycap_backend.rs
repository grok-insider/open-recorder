//! Real capture backend: PipeWire DMA-BUF -> NVENC via waycap-rs.
//!
//! Gated behind the `waycap` feature (needs the GPU/CUDA toolchain). It adapts
//! waycap-rs's crossbeam stream of `EncodedVideoFrame` into our `CaptureBackend`
//! contract by forwarding frames onto an `std::mpsc` channel on a background
//! thread, converting each frame into [`EncodedFrame`].
//!
//! waycap-rs must be built with the `nvidia` + `egl` features (see the spike and
//! docs/spike-results.md). The interactive screencast portal pick happens inside
//! `CaptureBuilder::build()`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use waycap_rs::pipeline::builder::CaptureBuilder;
use waycap_rs::types::config::{QualityPreset, VideoEncoder};
use waycap_rs::Capture;

use crate::backend::{BackendError, CaptureBackend, Codec, StreamParams};
use crate::ring::EncodedFrame;

/// Quality knob mapped onto waycap-rs presets.
#[derive(Debug, Clone, Copy)]
pub enum Quality {
    Low,
    Medium,
    High,
    Ultra,
}

impl From<Quality> for QualityPreset {
    fn from(q: Quality) -> Self {
        match q {
            Quality::Low => QualityPreset::Low,
            Quality::Medium => QualityPreset::Medium,
            Quality::High => QualityPreset::High,
            Quality::Ultra => QualityPreset::Ultra,
        }
    }
}

/// NVENC capture backend.
pub struct WaycapBackend {
    quality: Quality,
    fps: u32,
    capture: Option<Capture<waycap_rs::DynamicEncoder>>,
    forwarder: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    running: bool,
}

impl WaycapBackend {
    /// Create a backend (does not start capture or prompt the portal yet).
    pub fn new(quality: Quality, fps: u32) -> Self {
        Self {
            quality,
            fps,
            capture: None,
            forwarder: None,
            stop: Arc::new(AtomicBool::new(false)),
            running: false,
        }
    }
}

impl CaptureBackend for WaycapBackend {
    fn start(&mut self) -> Result<Receiver<EncodedFrame>, BackendError> {
        if self.running {
            return Err(BackendError::AlreadyRunning);
        }

        let mut capture = CaptureBuilder::new()
            .with_video_encoder(VideoEncoder::H264Nvenc)
            .with_quality_preset(self.quality.into())
            .with_target_fps(self.fps as u64)
            .with_cursor_shown()
            .build()
            .map_err(|e| BackendError::Init(format!("{e:?}")))?;

        let video_recv = capture.get_video_receiver();
        capture
            .start()
            .map_err(|e| BackendError::Init(format!("{e:?}")))?;

        let (tx, rx) = mpsc::channel();
        let stop = Arc::clone(&self.stop);
        stop.store(false, Ordering::Release);

        // Forward waycap-rs (crossbeam) frames onto our mpsc channel, converting
        // each into our EncodedFrame. Exits when stop is set or either channel
        // closes.
        let forwarder = std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                match video_recv.recv_timeout(Duration::from_millis(100)) {
                    Ok(f) => {
                        let frame = EncodedFrame::new(f.data, f.is_keyframe, f.pts, f.dts);
                        if tx.send(frame).is_err() {
                            break;
                        }
                    }
                    Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        self.capture = Some(capture);
        self.forwarder = Some(forwarder);
        self.running = true;
        Ok(rx)
    }

    fn stop(&mut self) -> Result<(), BackendError> {
        if !self.running {
            return Err(BackendError::NotRunning);
        }
        self.stop.store(true, Ordering::Release);
        if let Some(cap) = self.capture.as_mut() {
            let _ = cap.finish();
        }
        if let Some(handle) = self.forwarder.take() {
            let _ = handle.join();
        }
        self.capture = None;
        self.running = false;
        Ok(())
    }

    fn params(&self) -> StreamParams {
        StreamParams {
            width: 0,
            height: 0,
            fps: self.fps,
            codec: Codec::H264,
        }
    }

    fn is_running(&self) -> bool {
        self.running
    }
}
