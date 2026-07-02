//! Streaming recorder for "real" full-length recording.
//!
//! Where [`write_clip`](crate::mux::write_clip) is a one-shot mux of a finished
//! [`PreparedClip`](crate::engine::PreparedClip), a [`Recorder`] is **stateful**:
//! you `start` it, `push_video`/`push_audio` encoded frames as they arrive from
//! capture, and `finish` to finalize the file. The header is written from the
//! first keyframe (so the recording always starts decodably), and timestamps are
//! rebased to that keyframe — exactly like the clip muxer, just incremental.
//!
//! The real implementation needs ffmpeg (the `mux` feature). Without it,
//! [`Recorder::start`] errors so the pure engine still builds and tests.

use std::path::{Path, PathBuf};

use crate::audio::{AudioParams, EncodedAudioFrame};
use crate::backend::StreamParams;
use crate::mux::MuxError;
use crate::ring::EncodedFrame;

#[cfg(feature = "mux")]
pub use imp::Recorder;

#[cfg(not(feature = "mux"))]
pub use stub::Recorder;

#[cfg(not(feature = "mux"))]
mod stub {
    use super::*;

    /// Stub recorder for builds without the `mux` feature: `start` fails so the
    /// daemon reports that recording needs the real (ffmpeg) build.
    pub struct Recorder;

    impl Recorder {
        pub fn start(
            _path: &Path,
            _params: StreamParams,
            _audio: Option<AudioParams>,
        ) -> Result<Self, MuxError> {
            Err(MuxError::Ffmpeg(
                "recording requires the `mux` feature".into(),
            ))
        }
        pub fn push_video(&mut self, _f: &EncodedFrame) -> Result<(), MuxError> {
            Ok(())
        }
        pub fn push_audio(&mut self, _f: &EncodedAudioFrame) -> Result<(), MuxError> {
            Ok(())
        }
        pub fn held_audio_frames(&self) -> usize {
            0
        }
        pub fn finish(self) -> Result<PathBuf, MuxError> {
            Ok(PathBuf::new())
        }
    }
}

#[cfg(feature = "mux")]
mod imp {
    use super::*;

    use std::collections::VecDeque;

    use ffmpeg_next as ff;

    use crate::mux::{bitstream, stream};

    /// How much encoded audio (µs) may be held back at any time. Before the
    /// header it bounds the preroll: NVENC emits frames with encode latency, so
    /// in pump order the audio for the recording's first moments arrives BEFORE
    /// the keyframe carrying the matching timestamp — dropping it left
    /// recordings with a silent (and player-confusing) audio hole at the start.
    /// After the header it bounds the backlog held while waiting for video to
    /// advance: if video stalls (encoder hang, frozen compositor) while audio
    /// keeps flowing, the queue must not grow for the life of the recording.
    const AUDIO_PREROLL_US: i64 = 5_000_000;

    /// An open recording: a live ffmpeg output context fed encoded packets.
    pub struct Recorder {
        octx: ff::format::context::Output,
        path: PathBuf,
        params: StreamParams,
        audio_params: Option<AudioParams>,
        video_index: usize,
        audio_index: Option<usize>,
        /// Whether the header has been written (waits for the first keyframe).
        started: bool,
        /// Timestamp (ticks) of the first frame, rebased to 0.
        base: i64,
        video_base_us: i64,
        video_dts: stream::MonotonicMs,
        audio_ts: stream::MonotonicMs,
        /// Audio held until video catches up: before the header it is the
        /// (bounded) preroll; afterwards it absorbs the encoder's video
        /// latency so audio is only written up to the newest video pts —
        /// `finish` then drops whatever trails the last frame, keeping the
        /// audio track from out-running the video by a second of frozen tail.
        pending_audio: VecDeque<EncodedAudioFrame>,
        /// Rebased pts (ms) of the newest written video packet.
        last_video_ms: i64,
    }

    impl Recorder {
        /// Open `path` for recording. The header is deferred until the first
        /// keyframe arrives via [`push_video`](Recorder::push_video).
        pub fn start(
            path: &Path,
            params: StreamParams,
            audio_params: Option<AudioParams>,
        ) -> Result<Self, MuxError> {
            ff::init()?;
            let octx = ff::format::output(&path)?;
            Ok(Self {
                octx,
                path: path.to_path_buf(),
                params,
                audio_params,
                video_index: 0,
                audio_index: None,
                started: false,
                base: 0,
                video_base_us: 0,
                video_dts: stream::MonotonicMs::new(),
                audio_ts: stream::MonotonicMs::new(),
                pending_audio: VecDeque::new(),
                last_video_ms: 0,
            })
        }

        /// Push one encoded video frame. Frames before the first keyframe are
        /// dropped so the recording starts cleanly; the first keyframe writes the
        /// header (building avcC from its SPS/PPS) and anchors the timeline.
        pub fn push_video(&mut self, frame: &EncodedFrame) -> Result<(), MuxError> {
            if !self.started {
                if !frame.is_keyframe {
                    return Ok(());
                }
                self.write_header(frame)?;
            }

            let den = self.params.time_base_den.max(1);
            let payload = bitstream::packet_payload(self.params.codec, &frame.data);
            if payload.is_empty() {
                return Ok(());
            }
            let dts = self.video_dts.next((frame.dts - self.base) * 1000 / den);
            let pts = ((frame.pts - self.base) * 1000 / den).max(dts);

            let mut packet = ff::codec::packet::Packet::copy(&payload);
            packet.set_pts(Some(pts));
            packet.set_dts(Some(dts));
            packet.set_stream(self.video_index);
            if frame.is_keyframe {
                packet.set_flags(ff::codec::packet::Flags::KEY);
            }
            packet.write_interleaved(&mut self.octx)?;

            self.last_video_ms = self.last_video_ms.max(pts);
            self.flush_audio()
        }

        /// Push one encoded audio frame. Audio is buffered, not written
        /// directly: it flushes as video advances (see `pending_audio`).
        pub fn push_audio(&mut self, frame: &EncodedAudioFrame) -> Result<(), MuxError> {
            if self.audio_params.is_none() || frame.data.is_empty() {
                return Ok(());
            }
            self.pending_audio.push_back(frame.clone());
            if !self.started {
                self.bound_pending_audio();
                return Ok(());
            }
            self.flush_audio()?;
            self.bound_pending_audio();
            Ok(())
        }

        /// Keep only the newest [`AUDIO_PREROLL_US`] of held-back audio. Before
        /// the header this trims the preroll silently; after it, dropping means
        /// video has stalled for seconds while audio kept arriving — worth a
        /// warning, and strictly better than unbounded growth.
        fn bound_pending_audio(&mut self) {
            let mut dropped = 0usize;
            while let (Some(front), Some(back)) =
                (self.pending_audio.front(), self.pending_audio.back())
            {
                if back.timestamp_micros - front.timestamp_micros > AUDIO_PREROLL_US {
                    self.pending_audio.pop_front();
                    dropped += 1;
                } else {
                    break;
                }
            }
            if dropped > 0 && self.started {
                tracing::warn!(
                    dropped,
                    "video stalled during recording; dropping oldest held-back audio"
                );
            }
        }

        /// Write buffered audio up to the newest video pts; drop anything from
        /// before the recording start.
        fn flush_audio(&mut self) -> Result<(), MuxError> {
            let Some(aidx) = self.audio_index else {
                self.pending_audio.clear();
                return Ok(());
            };
            while let Some(front) = self.pending_audio.front() {
                let ts = (front.timestamp_micros - self.video_base_us) / 1000;
                if ts > self.last_video_ms {
                    break; // hold until video catches up
                }
                let Some(frame) = self.pending_audio.pop_front() else {
                    break;
                };
                if ts < 0 {
                    continue; // audio from before the first keyframe
                }
                let ts = self.audio_ts.next(ts);
                let mut packet = ff::codec::packet::Packet::copy(&frame.data);
                packet.set_pts(Some(ts));
                packet.set_dts(Some(ts));
                packet.set_stream(aidx);
                packet.write_interleaved(&mut self.octx)?;
            }
            Ok(())
        }

        /// How many audio frames are currently held back waiting for video.
        /// Diagnostics/testing: bounded by [`AUDIO_PREROLL_US`] worth of audio.
        pub fn held_audio_frames(&self) -> usize {
            self.pending_audio.len()
        }

        /// Finalize the file and return its path. Buffered audio past the last
        /// video frame is dropped so both tracks end together. A recording that
        /// never saw a keyframe leaves an (empty) header-less file, which the
        /// caller removes.
        pub fn finish(mut self) -> Result<PathBuf, MuxError> {
            if self.started {
                self.flush_audio()?;
                self.octx.write_trailer()?;
            }
            Ok(self.path)
        }

        fn write_header(&mut self, first_keyframe: &EncodedFrame) -> Result<(), MuxError> {
            let extradata = bitstream::extradata(self.params.codec, &first_keyframe.data)?;
            self.video_index = stream::add_video_stream(&mut self.octx, self.params, &extradata)?;
            if let Some(ap) = self.audio_params {
                self.audio_index = Some(stream::add_audio_stream(&mut self.octx, ap)?);
            }
            self.octx.write_header()?;
            self.base = first_keyframe.dts.min(first_keyframe.pts);
            self.video_base_us =
                crate::ticks_to_micros(self.base, self.params.time_base_den.max(1));
            self.started = true;
            Ok(())
        }
    }
}
