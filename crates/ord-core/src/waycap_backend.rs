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
    width: u32,
    height: u32,
    restore_token_path: Option<std::path::PathBuf>,
    capture: Option<Capture<waycap_rs::DynamicEncoder>>,
    forwarder: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    running: bool,
}

impl WaycapBackend {
    /// Create a backend (does not start capture or prompt the portal yet). The
    /// width/height are container hints; actual dimensions come from the H.264
    /// SPS in the stream.
    pub fn new(quality: Quality, fps: u32) -> Self {
        Self {
            quality,
            fps,
            width: 2560,
            height: 1440,
            restore_token_path: None,
            capture: None,
            forwarder: None,
            stop: Arc::new(AtomicBool::new(false)),
            running: false,
        }
    }

    /// Set the container dimension hints.
    pub fn with_dimensions(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Persist/reuse the XDG screencast restore token at `path`. When a token
    /// exists it is passed to the portal so the "Select what to share" picker is
    /// skipped; after a successful start the (possibly refreshed) token is saved
    /// back. This is what stops the picker appearing on every daemon start.
    pub fn with_restore_token_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.restore_token_path = Some(path.into());
        self
    }
}

impl CaptureBackend for WaycapBackend {
    fn start(&mut self) -> Result<Receiver<EncodedFrame>, BackendError> {
        if self.running {
            return Err(BackendError::AlreadyRunning);
        }

        // Load a previously saved restore token so the portal can skip the
        // interactive picker. A stale/invalid token just makes the portal prompt
        // again, so this is safe.
        let saved_token = self
            .restore_token_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let mut builder = CaptureBuilder::new()
            .with_video_encoder(VideoEncoder::H264Nvenc)
            .with_quality_preset(self.quality.into())
            .with_target_fps(self.fps as u64)
            .with_cursor_shown();
        if let Some(token) = saved_token {
            builder = builder.with_restore_token(token);
        }
        let mut capture = builder
            .build()
            .map_err(|e| BackendError::Init(format!("{e:?}")))?;

        // Persist the token the portal granted (present only if the user ticked
        // "Allow a restore token"), so the next start skips the picker.
        if let (Some(path), Some(token)) = (
            self.restore_token_path.as_ref(),
            capture.restore_token.as_ref(),
        ) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, token);
        }

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
            // Dimensions are carried in the H.264 SPS (in each keyframe), so the
            // muxer/decoder recover them even though waycap-rs does not surface
            // them here. These act only as a container hint.
            width: self.width,
            height: self.height,
            fps: self.fps,
            codec: Codec::H264,
            time_base_den: crate::backend::NANOS_PER_SEC, // waycap pts are nanoseconds
        }
    }

    fn is_running(&self) -> bool {
        self.running
    }
}
