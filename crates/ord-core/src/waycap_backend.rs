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
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use waycap_rs::pipeline::builder::CaptureBuilder;
use waycap_rs::types::config::{AudioEncoder, QualityPreset, VideoEncoder};
use waycap_rs::Capture;

use crate::audio::{AudioCodec, AudioParams, EncodedAudioFrame};
use crate::backend::{BackendError, CaptureBackend, CaptureStreams, Codec, StreamParams};
use crate::ring::EncodedFrame;

/// waycap-rs emits Opus at 48 kHz stereo (see its `OpusEncoder`).
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u16 = 2;

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
    audio_enabled: bool,
    mic_enabled: bool,
    restore_token_path: Option<std::path::PathBuf>,
    capture: Option<Capture<waycap_rs::DynamicEncoder>>,
    forwarder: Option<JoinHandle<()>>,
    audio_forwarder: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    running: bool,
}

impl WaycapBackend {
    /// Create a backend (does not start capture or prompt the portal yet). The
    /// width/height are container hints; actual dimensions come from the H.264
    /// SPS in the stream. Desktop audio (the default sink monitor) is captured by
    /// default; toggle with [`with_audio`](Self::with_audio).
    pub fn new(quality: Quality, fps: u32) -> Self {
        Self {
            quality,
            fps,
            width: 2560,
            height: 1440,
            audio_enabled: true,
            mic_enabled: false,
            restore_token_path: None,
            capture: None,
            forwarder: None,
            audio_forwarder: None,
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

    /// Enable or disable audio capture (default: enabled). When enabled,
    /// waycap-rs captures the default sink monitor (game + voice chat playback)
    /// and encodes it to Opus.
    pub fn with_audio(mut self, enabled: bool) -> Self {
        self.audio_enabled = enabled;
        self
    }

    /// Enable or disable mixing the default microphone into the audio track
    /// (default: disabled). Mic capture rides the same PipeWire clock as the
    /// desktop monitor, so it stays in A/V sync; the two are summed into one
    /// Opus track. Enabling the mic implies audio capture.
    pub fn with_mic(mut self, enabled: bool) -> Self {
        self.mic_enabled = enabled;
        if enabled {
            self.audio_enabled = true;
        }
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
    fn start(&mut self) -> Result<CaptureStreams, BackendError> {
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
        if self.mic_enabled {
            // with_microphone() implies audio; mixes mic + desktop into one track.
            builder = builder
                .with_microphone()
                .with_audio_encoder(AudioEncoder::Opus);
        } else if self.audio_enabled {
            builder = builder.with_audio().with_audio_encoder(AudioEncoder::Opus);
        }
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
        // Grab the audio receiver before starting so we never miss early frames.
        let audio_recv = if self.audio_enabled {
            Some(
                capture
                    .get_audio_receiver()
                    .map_err(|e| BackendError::Init(format!("audio receiver: {e:?}")))?,
            )
        } else {
            None
        };
        capture
            .start()
            .map_err(|e| BackendError::Init(format!("{e:?}")))?;

        // BOUND the forwarding channels so a stalled consumer (the daemon's
        // periodic `pump`) can never grow them without bound. The daemon drains
        // every ~250 ms, so occupancy stays near zero; these caps (~4 s of video,
        // and plenty of audio packets) are a memory backstop, not a normal limit.
        const VIDEO_CHANNEL_CAP: usize = 240;
        const AUDIO_CHANNEL_CAP: usize = 2000;

        let (tx, rx) = mpsc::sync_channel(VIDEO_CHANNEL_CAP);
        let stop = Arc::clone(&self.stop);
        stop.store(false, Ordering::Release);

        // Forward waycap-rs (crossbeam) frames onto our bounded mpsc channel,
        // converting each into our EncodedFrame. Exits when stop is set or the
        // channel closes. If the channel is full (consumer stalled), drop the
        // frame rather than grow memory — a saved clip may glitch there, but the
        // periodic pump means this should never happen.
        let video_stop = Arc::clone(&stop);
        let forwarder = std::thread::spawn(move || {
            let mut dropped = 0u64;
            while !video_stop.load(Ordering::Acquire) {
                match video_recv.recv_timeout(Duration::from_millis(100)) {
                    Ok(f) => {
                        let frame = EncodedFrame::new(f.data, f.is_keyframe, f.pts, f.dts);
                        match tx.try_send(frame) {
                            Ok(()) => {}
                            Err(mpsc::TrySendError::Full(_)) => {
                                dropped += 1;
                                if dropped % 300 == 1 {
                                    eprintln!(
                                        "ordd: video forward channel full ({VIDEO_CHANNEL_CAP}); \
                                         dropping frames (consumer stalled, {dropped} total)"
                                    );
                                }
                            }
                            Err(mpsc::TrySendError::Disconnected(_)) => break,
                        }
                    }
                    Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        // Forward + convert audio. waycap-rs stamps audio frames with a
        // CLOCK_MONOTONIC **nanosecond** capture time (its field doc saying
        // "micro seconds" is wrong: it comes from `pw_stream_get_nsec`), the same
        // clock as the video pts. Our engine correlates A/V in microseconds, so
        // divide by 1000 here to land in that domain.
        let audio_out = if let Some(audio_recv) = audio_recv {
            let (atx, arx) = mpsc::sync_channel(AUDIO_CHANNEL_CAP);
            let audio_stop = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                while !audio_stop.load(Ordering::Acquire) {
                    match audio_recv.recv_timeout(Duration::from_millis(100)) {
                        Ok(f) => {
                            let frame = EncodedAudioFrame::new(f.data, f.pts, f.timestamp / 1000);
                            match atx.try_send(frame) {
                                Ok(()) => {}
                                Err(mpsc::TrySendError::Full(_)) => {} // backstop only
                                Err(mpsc::TrySendError::Disconnected(_)) => break,
                            }
                        }
                        Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
                        Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
            self.audio_forwarder = Some(handle);
            Some(arx)
        } else {
            None
        };

        self.capture = Some(capture);
        self.forwarder = Some(forwarder);
        self.running = true;
        Ok(CaptureStreams {
            video: rx,
            audio: audio_out,
        })
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
        if let Some(handle) = self.audio_forwarder.take() {
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

    fn audio_params(&self) -> Option<AudioParams> {
        self.audio_enabled.then_some(AudioParams {
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS,
            codec: AudioCodec::Opus,
        })
    }

    fn is_running(&self) -> bool {
        self.running
    }
}
