//! Inline A/V preview player for the editor (gui-only).
//!
//! ffplay-style architecture: a single **decode thread** (ffmpeg-next) demuxes
//! the clip, decodes video into a bounded RGBA frame queue and audio into a
//! shared sample buffer; a **cpal** output stream drains the audio and drives the
//! master clock (samples played → seconds). The UI calls [`Player::frame`] each
//! repaint to advance the clock, enforce the loop range, and get the video frame
//! whose pts matches the clock.
//!
//! Clips with no audio (or no audio device) fall back to a wall-clock master so
//! playback still advances. Export stays CLI-based (`ord-export`); this player is
//! preview/playback only and decodes at a reduced width to stay light.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use eframe::egui;
use ffmpeg_next as ff;

/// Decode preview at most this wide (keeps memory/CPU sane; export is full-res).
/// 1440 is crisp on a 1440p display while keeping the frame queue bounded.
const PREVIEW_MAX_W: u32 = 1440;
/// Bounded look-ahead video queue (frames) ≈ 0.5s at 60fps. This is the demuxer
/// pacer: it must hold a comparable DURATION to the audio buffer below, otherwise
/// the demuxer races ahead to fill audio and the video queue overflows → video
/// freezes while audio plays. ~0.5s @1440p RGBA ≈ 140 MB transient (cheaper once
/// the NV12/GL path is active).
const VIDEO_QUEUE_MAX: usize = 30;
/// Audio look-ahead ceiling (interleaved f32 samples) ≈ 2s stereo @ 48k. In
/// practice the video queue fills first and paces the demuxer, so audio settles
/// well under this; it's just an upper bound.
const AUDIO_BUF_MAX: usize = 48_000 * 2 * 2;

/// Which video decoder the decode thread ended up using.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecKind {
    /// Not chosen yet (decode thread still starting up).
    Unknown,
    /// NVIDIA NVDEC via ffmpeg `*_cuvid` (GPU decode + GPU resize).
    Nvdec,
    /// Frame-threaded software decode (fallback / non-NVIDIA).
    Software,
}

impl DecKind {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => DecKind::Nvdec,
            2 => DecKind::Software,
            _ => DecKind::Unknown,
        }
    }
    fn as_u8(self) -> u8 {
        match self {
            DecKind::Unknown => 0,
            DecKind::Nvdec => 1,
            DecKind::Software => 2,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            DecKind::Unknown => "…",
            DecKind::Nvdec => "nvdec",
            DecKind::Software => "sw",
        }
    }
}

/// Live player diagnostics (debug overlay).
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub position: f64,
    pub has_audio: bool,
    pub playing: bool,
    pub audio_buf_ms: f64,
    pub frames_queued: usize,
    pub decoded: u64,
    pub dropped: u64,
    pub decoder: DecKind,
}

/// A decoded RGBA video frame tagged with its presentation time (seconds).
struct VideoFrame {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    pts: f64,
}

/// State shared between the UI, the decode thread, and the audio callback.
struct Shared {
    playing: AtomicBool,
    stop: AtomicBool,
    looping: AtomicBool,
    has_audio: AtomicBool,
    sample_rate: AtomicU64,
    channels: AtomicU64,
    /// Raw audio samples (all channels) actually consumed by cpal since the last
    /// seek — the audio master clock. Only *real* samples count (silence on
    /// underrun does not advance it), so video stays locked to real audio.
    samples_played: AtomicU64,
    /// Diagnostics (debug mode).
    decoded: AtomicU64,
    dropped: AtomicU64,
    /// Which decoder the decode thread selected ([`DecKind`] as u8).
    dec_kind: AtomicU8,
    seek_base: Mutex<f64>,
    seek_to: Mutex<Option<f64>>,
    in_secs: Mutex<f64>,
    out_secs: Mutex<f64>,
    volume: Mutex<f32>,
    /// Wall-clock anchor used as the master clock when there is no audio.
    play_anchor: Mutex<Option<Instant>>,
    audio_buf: Mutex<VecDeque<f32>>,
    frames: Mutex<VecDeque<VideoFrame>>,
}

impl Shared {
    fn range(&self) -> (f64, f64) {
        (
            *self.in_secs.lock().unwrap(),
            *self.out_secs.lock().unwrap(),
        )
    }
}

/// An inline preview player for one clip.
pub struct Player {
    shared: Arc<Shared>,
    _stream: Option<cpal::Stream>,
    decode_thread: Option<JoinHandle<()>>,
    texture: Option<egui::TextureHandle>,
    shown_pts: f64,
    duration: f64,
    fps: f64,
}

impl Player {
    /// Open `path` and start the decode thread + audio output (paused).
    pub fn open(path: &Path) -> Result<Self, String> {
        ff::init().map_err(|e| format!("ffmpeg init: {e}"))?;
        let info = ord_export::probe::probe(path).map_err(|e| e.to_string())?;
        let duration = info.duration_secs.max(0.0);
        let fps = if info.fps > 1.0 { info.fps } else { 30.0 };

        let shared = Arc::new(Shared {
            playing: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            looping: AtomicBool::new(false),
            has_audio: AtomicBool::new(false),
            sample_rate: AtomicU64::new(48_000),
            channels: AtomicU64::new(2),
            samples_played: AtomicU64::new(0),
            decoded: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            dec_kind: AtomicU8::new(0),
            seek_base: Mutex::new(0.0),
            seek_to: Mutex::new(Some(0.0)),
            in_secs: Mutex::new(0.0),
            out_secs: Mutex::new(duration),
            volume: Mutex::new(1.0),
            play_anchor: Mutex::new(None),
            audio_buf: Mutex::new(VecDeque::new()),
            frames: Mutex::new(VecDeque::new()),
        });

        // Set up audio output (best-effort). Falls back to wall-clock if absent.
        let stream = if info.has_audio {
            match build_audio_stream(&shared) {
                Ok((stream, rate, channels)) => {
                    shared.sample_rate.store(rate as u64, Ordering::Relaxed);
                    shared.channels.store(channels as u64, Ordering::Relaxed);
                    shared.has_audio.store(true, Ordering::Release);
                    Some(stream)
                }
                Err(e) => {
                    eprintln!("ord-ui: audio unavailable ({e}); preview will be silent");
                    None
                }
            }
        } else {
            None
        };

        let decode_thread = {
            let shared = Arc::clone(&shared);
            let path = PathBuf::from(path);
            std::thread::Builder::new()
                .name("ord-preview-decode".into())
                .spawn(move || decode_loop(path, shared))
                .map_err(|e| e.to_string())?
        };

        Ok(Self {
            shared,
            _stream: stream,
            decode_thread: Some(decode_thread),
            texture: None,
            shown_pts: -1.0,
            duration,
            fps,
        })
    }

    pub fn duration(&self) -> f64 {
        self.duration
    }

    /// Source frame rate (for frame-accurate stepping).
    pub fn fps(&self) -> f64 {
        self.fps
    }

    /// Live diagnostics for the debug overlay.
    pub fn stats(&self) -> Stats {
        let sr = self.shared.sample_rate.load(Ordering::Relaxed).max(1) as f64;
        let ch = self.shared.channels.load(Ordering::Relaxed).max(1) as f64;
        let audio_samples = self.shared.audio_buf.lock().unwrap().len() as f64;
        Stats {
            position: self.position(),
            has_audio: self.has_audio(),
            playing: self.is_playing(),
            audio_buf_ms: (audio_samples / (sr * ch)) * 1000.0,
            frames_queued: self.shared.frames.lock().unwrap().len(),
            decoded: self.shared.decoded.load(Ordering::Relaxed),
            dropped: self.shared.dropped.load(Ordering::Relaxed),
            decoder: self.decoder_kind(),
        }
    }

    /// Which video decoder is active (NVDEC / software), for the debug overlay.
    pub fn decoder_kind(&self) -> DecKind {
        DecKind::from_u8(self.shared.dec_kind.load(Ordering::Relaxed))
    }

    pub fn has_audio(&self) -> bool {
        self.shared.has_audio.load(Ordering::Acquire)
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Acquire)
    }

    pub fn looping(&self) -> bool {
        self.shared.looping.load(Ordering::Acquire)
    }

    pub fn set_loop(&self, on: bool) {
        self.shared.looping.store(on, Ordering::Release);
    }

    pub fn volume(&self) -> f32 {
        *self.shared.volume.lock().unwrap()
    }

    pub fn set_volume(&self, v: f32) {
        *self.shared.volume.lock().unwrap() = v.clamp(0.0, 1.0);
    }

    pub fn set_range(&self, in_s: f64, out_s: f64) {
        *self.shared.in_secs.lock().unwrap() = in_s;
        *self.shared.out_secs.lock().unwrap() = out_s;
    }

    /// Current playback position in seconds (the master clock).
    pub fn position(&self) -> f64 {
        let base = *self.shared.seek_base.lock().unwrap();
        if self.has_audio() {
            let sr = self.shared.sample_rate.load(Ordering::Relaxed).max(1) as f64;
            let ch = self.shared.channels.load(Ordering::Relaxed).max(1) as f64;
            let played = self.shared.samples_played.load(Ordering::Relaxed) as f64;
            (base + played / (sr * ch)).min(self.duration)
        } else {
            match *self.shared.play_anchor.lock().unwrap() {
                Some(t0) => (base + t0.elapsed().as_secs_f64()).min(self.duration),
                None => base,
            }
        }
    }

    pub fn seek(&mut self, t: f64) {
        let t = t.clamp(0.0, self.duration);
        *self.shared.seek_base.lock().unwrap() = t;
        self.shared.samples_played.store(0, Ordering::Relaxed);
        *self.shared.seek_to.lock().unwrap() = Some(t);
        if !self.has_audio() {
            let playing = self.is_playing();
            *self.shared.play_anchor.lock().unwrap() = playing.then(Instant::now);
        }
    }

    pub fn play(&mut self) {
        // Always (re)start within the selection so we "only play the clip".
        let pos = self.position();
        let (i, o) = self.shared.range();
        if pos < i || pos >= o - 0.05 {
            self.seek(i);
        }
        self.shared.playing.store(true, Ordering::Release);
        if self.has_audio() {
            if let Some(s) = &self._stream {
                let _ = s.play();
            }
        } else {
            *self.shared.play_anchor.lock().unwrap() = Some(Instant::now());
        }
    }

    pub fn pause(&mut self) {
        if self.has_audio() {
            if let Some(s) = &self._stream {
                let _ = s.pause();
            }
        } else {
            // Freeze the wall clock at the current position.
            let pos = self.position();
            *self.shared.seek_base.lock().unwrap() = pos;
            *self.shared.play_anchor.lock().unwrap() = None;
        }
        self.shared.playing.store(false, Ordering::Release);
    }

    pub fn toggle(&mut self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Advance one UI frame: enforce the loop range, pick the video frame for the
    /// current clock, and return the texture to draw. Call every repaint.
    pub fn frame(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        // Loop / stop at the out-point when playing the selection.
        if self.is_playing() {
            let (in_s, out_s) = self.shared.range();
            if self.position() >= out_s.min(self.duration) - 0.001 {
                if self.looping() {
                    self.seek(in_s);
                } else {
                    self.pause();
                    self.seek(out_s);
                }
            }
        }

        let pos = self.position();
        // Pick the newest decoded frame at or before the clock.
        let mut chosen: Option<VideoFrame> = None;
        {
            let mut q = self.shared.frames.lock().unwrap();
            while q.front().map(|f| f.pts <= pos + 0.005).unwrap_or(false) {
                chosen = q.pop_front();
            }
            // Right after a seek the queue may only hold frames slightly ahead;
            // show the closest upcoming one so a paused scrub isn't blank.
            if chosen.is_none() && self.texture.is_none() {
                if let Some(f) = q.pop_front() {
                    chosen = Some(f);
                }
            }
        }
        if let Some(f) = chosen {
            if (f.pts - self.shown_pts).abs() > f64::EPSILON {
                let img = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
                self.texture =
                    Some(ctx.load_texture("editor-preview", img, egui::TextureOptions::LINEAR));
                self.shown_pts = f.pts;
            }
        }

        // Keep polling so playback advances and post-seek frames appear.
        ctx.request_repaint_after(Duration::from_millis(16));
        self.texture.as_ref()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // Signal stop and pause audio, but DON'T join the decode thread on the UI
        // thread — joining could block the UI for the time a slow decode call
        // takes to return (this was the ~4s "UI STALL" after closing the editor
        // in the watchdog log). The thread observes `stop` and exits on its own,
        // dropping its ffmpeg context; detaching keeps window close instant.
        self.shared.stop.store(true, Ordering::Release);
        if let Some(s) = &self._stream {
            let _ = s.pause();
        }
        drop(self.decode_thread.take());
    }
}

/// Build a cpal F32 output stream that drains the shared audio buffer and counts
/// frames played (the audio clock). Returns (stream, sample_rate, channels).
fn build_audio_stream(shared: &Arc<Shared>) -> Result<(cpal::Stream, u32, u16), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no default audio output device")?;

    // Prefer an F32 config (PipeWire/Pulse default); pick ~48k and stereo if
    // offered, otherwise the first F32 range we see.
    let mut chosen: Option<cpal::SupportedStreamConfig> = None;
    if let Ok(configs) = device.supported_output_configs() {
        for c in configs {
            if c.sample_format() != cpal::SampleFormat::F32 {
                continue;
            }
            let want_rate = cpal::SampleRate(48_000);
            let cfg = if c.min_sample_rate() <= want_rate && want_rate <= c.max_sample_rate() {
                c.with_sample_rate(want_rate)
            } else {
                c.with_max_sample_rate()
            };
            let better = match &chosen {
                None => true,
                Some(p) => p.channels() != 2 && cfg.channels() == 2,
            };
            if better {
                chosen = Some(cfg);
            }
        }
    }
    let supported = match chosen {
        Some(c) => c,
        None => device
            .default_output_config()
            .map_err(|e| e.to_string())
            .and_then(|c| {
                if c.sample_format() == cpal::SampleFormat::F32 {
                    Ok(c)
                } else {
                    Err("no F32 output config".to_string())
                }
            })?,
    };

    let rate = supported.sample_rate().0;
    let channels = supported.channels();
    let config: cpal::StreamConfig = supported.into();
    let shared_cb = Arc::clone(shared);
    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let vol = *shared_cb.volume.lock().unwrap();
                let mut buf = shared_cb.audio_buf.lock().unwrap();
                let mut consumed = 0u64;
                for s in data.iter_mut() {
                    match buf.pop_front() {
                        Some(v) => {
                            *s = v * vol;
                            consumed += 1;
                        }
                        None => *s = 0.0, // underrun: silence, do NOT advance the clock
                    }
                }
                drop(buf);
                shared_cb
                    .samples_played
                    .fetch_add(consumed, Ordering::Relaxed);
            },
            |e| eprintln!("ord-ui: audio stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    // Built paused; Player::play() starts it.
    let _ = stream.pause();
    Ok((stream, rate, channels))
}

/// The decode thread: demux + decode video/audio, honoring seek/stop/loop.
fn decode_loop(path: PathBuf, shared: Arc<Shared>) {
    let Ok(mut ictx) = ff::format::input(&path) else {
        return;
    };

    let video_stream = match ictx.streams().best(ff::media::Type::Video) {
        Some(s) => s,
        None => return,
    };
    let video_index = video_stream.index();
    let video_tb = f64::from(video_stream.time_base());
    let video_params = video_stream.parameters();

    let audio_info = ictx
        .streams()
        .best(ff::media::Type::Audio)
        .map(|s| (s.index(), s.parameters()));

    // Choose the video decoder: NVDEC (GPU decode + GPU downscale) when available,
    // else frame-threaded software decode. NVDEC moves the expensive 1440p60
    // decode off the CPU — the software path pegging cores was the stutter + the
    // "UI STALL" ANR seen in the watchdog log.
    let (mut vdec, kind) = match open_video_decoder(video_params) {
        Some(v) => v,
        None => return,
    };
    shared.dec_kind.store(kind.as_u8(), Ordering::Relaxed);

    // Preview RGBA scaler, built lazily from the FIRST decoded frame: cuvid only
    // reports its real output format/size after the first frame, and software
    // frames are source-sized so we cap width here. Lazy init handles both.
    let mut scaler: Option<ff::software::scaling::context::Context> = None;

    // Audio decoder + resampler to the device's F32 format.
    let out_rate = shared.sample_rate.load(Ordering::Relaxed) as u32;
    let out_ch = shared.channels.load(Ordering::Relaxed) as u16;
    let out_layout = if out_ch >= 2 {
        ff::channel_layout::ChannelLayout::STEREO
    } else {
        ff::channel_layout::ChannelLayout::MONO
    };
    // Audio decoder only; the resampler is built lazily from the FIRST decoded
    // frame, because a decoder's channel layout / format can be unset until then
    // (building it upfront produced a mis-configured resampler → silent audio).
    let mut audio: Option<(usize, ff::decoder::Audio)> = None;
    let mut resampler: Option<ff::software::resampling::context::Context> = None;
    if shared.has_audio.load(Ordering::Acquire) {
        if let Some((aidx, aparams)) = audio_info {
            if let Ok(adec) = ff::codec::context::Context::from_parameters(aparams)
                .and_then(|c| c.decoder().audio())
            {
                audio = Some((aidx, adec));
            }
        }
    }

    let mut packet = ff::codec::packet::Packet::empty();
    loop {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }

        // Handle a pending seek.
        if let Some(t) = shared.seek_to.lock().unwrap().take() {
            let ts = (t * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
            let _ = ictx.seek(ts, ..ts);
            vdec.flush();
            if let Some((_, adec)) = audio.as_mut() {
                adec.flush();
            }
            shared.frames.lock().unwrap().clear();
            shared.audio_buf.lock().unwrap().clear();
        }

        // Backpressure: pace the demuxer so it stays only a small, BALANCED
        // amount ahead of the master clock — sleep when EITHER buffer is full.
        //
        // The previous "sleep only when audio is full" let the demuxer read ~2s
        // ahead to fill the audio buffer while the small video queue overflowed
        // and dropped most frames; video then starved (the same frame held for
        // seconds) while audio kept playing. Because video frames arrive at the
        // frame rate, the video queue fills first and becomes the pacer, so the
        // demuxer stays ~0.5s ahead, audio settles at a similar depth, and no
        // video is dropped in steady state.
        let video_full = shared.frames.lock().unwrap().len() >= VIDEO_QUEUE_MAX;
        let audio_full = audio.is_some() && shared.audio_buf.lock().unwrap().len() >= AUDIO_BUF_MAX;
        if video_full || audio_full {
            std::thread::sleep(Duration::from_millis(3));
            continue;
        }

        match packet.read(&mut ictx) {
            Ok(()) => {}
            Err(ff::Error::Eof) => {
                if shared.looping.load(Ordering::Acquire) && shared.playing.load(Ordering::Acquire)
                {
                    let (in_s, _) = shared.range();
                    *shared.seek_to.lock().unwrap() = Some(in_s);
                } else {
                    std::thread::sleep(Duration::from_millis(15));
                }
                continue;
            }
            Err(_) => continue,
        }

        let idx = packet.stream();
        if idx == video_index {
            if vdec.send_packet(&packet).is_ok() {
                let mut frame = ff::frame::Video::empty();
                while vdec.receive_frame(&mut frame).is_ok() {
                    let pts =
                        frame.pts().or_else(|| frame.timestamp()).unwrap_or(0) as f64 * video_tb;

                    // Build the preview scaler from the first real frame (cap to
                    // preview width, keep aspect, even dims).
                    if scaler.is_none() {
                        let inw = frame.width();
                        let inh = frame.height();
                        let out_w = inw.clamp(2, PREVIEW_MAX_W) & !1;
                        let out_h =
                            (((inh as u64 * out_w as u64) / inw.max(1) as u64) as u32).max(2) & !1;
                        scaler = ff::software::scaling::context::Context::get(
                            frame.format(),
                            inw,
                            inh,
                            ff::format::Pixel::RGBA,
                            out_w,
                            out_h,
                            ff::software::scaling::flag::Flags::LANCZOS,
                        )
                        .ok();
                    }
                    let Some(sc) = scaler.as_mut() else { continue };

                    // Skip the scale+pack entirely when the queue is full — we'd
                    // only drop the result. (We still drained `receive_frame`, so
                    // the decoder keeps flowing for audio.) This removes the
                    // wasted CPU that showed up as a huge `drop` count.
                    if shared.frames.lock().unwrap().len() >= VIDEO_QUEUE_MAX {
                        shared.dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    let mut rgba = ff::frame::Video::empty();
                    if sc.run(&frame, &mut rgba).is_ok() {
                        let vf = pack_rgba(&rgba, pts);
                        shared.decoded.fetch_add(1, Ordering::Relaxed);
                        shared.frames.lock().unwrap().push_back(vf);
                    }
                }
            }
        } else if let Some((aidx, adec)) = audio.as_mut() {
            if idx == *aidx && adec.send_packet(&packet).is_ok() {
                let mut frame = ff::frame::Audio::empty();
                while adec.receive_frame(&mut frame).is_ok() {
                    // Build the resampler from the actual frame format the first
                    // time we see one (layout/rate are reliable post-decode).
                    if resampler.is_none() {
                        resampler = ff::software::resampling::context::Context::get(
                            frame.format(),
                            frame.channel_layout(),
                            frame.rate(),
                            ff::format::Sample::F32(ff::format::sample::Type::Packed),
                            out_layout,
                            out_rate,
                        )
                        .ok();
                    }
                    if let Some(res) = resampler.as_mut() {
                        let mut out = ff::frame::Audio::empty();
                        if res.run(&frame, &mut out).is_ok() {
                            push_audio(&shared, &out, out_ch);
                        }
                    }
                }
            }
        }
    }
}

/// Map a codec id to its NVIDIA `*_cuvid` (NVDEC) decoder name, if one exists.
fn cuvid_name(id: ff::codec::Id) -> Option<&'static str> {
    use ff::codec::Id;
    Some(match id {
        Id::H264 => "h264_cuvid",
        Id::HEVC => "hevc_cuvid",
        Id::AV1 => "av1_cuvid",
        Id::VP9 => "vp9_cuvid",
        Id::VP8 => "vp8_cuvid",
        Id::MPEG2VIDEO => "mpeg2_cuvid",
        Id::MPEG4 => "mpeg4_cuvid",
        Id::VC1 => "vc1_cuvid",
        _ => return None,
    })
}

/// Open the video decoder for `params`: prefer NVDEC (GPU decode + GPU resize to
/// preview width), fall back to frame-threaded software decode. Honors
/// `ORD_DECODE`: `sw` forces software; `nvdec`/`gl`/`zerocopy` force-or-warn
/// hardware; unset = auto (NVDEC if available).
fn open_video_decoder(params: ff::codec::Parameters) -> Option<(ff::decoder::Video, DecKind)> {
    let want = std::env::var("ORD_DECODE").unwrap_or_default();
    let force_sw = want == "sw";
    let force_hw = matches!(want.as_str(), "nvdec" | "gl" | "zerocopy" | "hw");

    if !force_sw {
        if let Some(name) = cuvid_name(params.id()) {
            // Source dims from AVCodecParameters (for the GPU resize target).
            let (src_w, src_h) = unsafe {
                let p = params.as_ptr();
                ((*p).width, (*p).height)
            };
            if let Some(dec) = open_cuvid(&params, name, src_w, src_h) {
                return Some((dec, DecKind::Nvdec));
            }
            if force_hw {
                eprintln!("ord-ui: ORD_DECODE={want} but NVDEC unavailable; using software");
            }
        } else if force_hw {
            eprintln!("ord-ui: no NVDEC decoder for this codec; using software");
        }
    }

    // Frame-threaded software decode, capped so the UI + audio threads keep cores.
    let threads = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(2, 8))
        .unwrap_or(4);
    let dec = ff::codec::context::Context::from_parameters(params)
        .ok()
        .and_then(|mut c| {
            c.set_threading(ff::codec::threading::Config {
                kind: ff::codec::threading::Type::Frame,
                count: threads,
            });
            c.decoder().video().ok()
        })?;
    Some((dec, DecKind::Software))
}

/// Open an NVDEC (`*_cuvid`) decoder, asking the GPU to also downscale to preview
/// width (`resize`) and bounding VRAM (`surfaces`). Returns None if NVDEC is
/// unavailable (no driver / unsupported codec) so the caller can fall back. The
/// decoder outputs NV12 in system memory (no `hw_device_ctx`), which the lazy
/// swscale converts to RGBA like any software frame.
fn open_cuvid(
    params: &ff::codec::Parameters,
    name: &str,
    src_w: i32,
    src_h: i32,
) -> Option<ff::decoder::Video> {
    let codec = ff::codec::decoder::find_by_name(name)?;
    let ctx = ff::codec::context::Context::from_parameters(params.clone()).ok()?;

    let mut opts = ff::Dictionary::new();
    // GPU downscale to preview width only when the source is larger (never upscale).
    if src_w > PREVIEW_MAX_W as i32 && src_h > 0 {
        let out_w = (PREVIEW_MAX_W as i32) & !1;
        let out_h = (((src_h as i64 * out_w as i64) / src_w.max(1) as i64) as i32).max(2) & !1;
        opts.set("resize", &format!("{out_w}x{out_h}"));
    }
    // Small surface pool: bounds VRAM, ample for preview look-ahead.
    opts.set("surfaces", "8");

    let opened = ctx.decoder().open_as_with(codec, opts).ok()?;
    opened.video().ok()
}

/// Copy a scaled RGBA frame into a tight (no row padding) buffer.
fn pack_rgba(rgba: &ff::frame::Video, pts: f64) -> VideoFrame {
    let w = rgba.width() as usize;
    let h = rgba.height() as usize;
    let stride = rgba.stride(0);
    let data = rgba.data(0);
    let row = w * 4;
    let mut buf = vec![0u8; row * h];
    for y in 0..h {
        let src = &data[y * stride..y * stride + row];
        buf[y * row..(y + 1) * row].copy_from_slice(src);
    }
    VideoFrame {
        width: w,
        height: h,
        rgba: buf,
        pts,
    }
}

/// Push interleaved F32 samples from a resampled audio frame into the buffer.
fn push_audio(shared: &Arc<Shared>, out: &ff::frame::Audio, channels: u16) {
    let n = out.samples() * channels as usize;
    let bytes = out.data(0);
    let mut buf = shared.audio_buf.lock().unwrap();
    for chunk in bytes.chunks_exact(4).take(n) {
        buf.push_back(f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
}
