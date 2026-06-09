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
        pub fn finish(self) -> Result<PathBuf, MuxError> {
            Ok(PathBuf::new())
        }
    }
}

#[cfg(feature = "mux")]
mod imp {
    use super::*;

    use ffmpeg_next as ff;

    use crate::mux::{bitstream, stream};

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
            Ok(())
        }

        /// Push one encoded audio frame (ignored before the video header is
        /// written, or if its rebased timestamp would precede the start).
        pub fn push_audio(&mut self, frame: &EncodedAudioFrame) -> Result<(), MuxError> {
            let Some(aidx) = self.audio_index else {
                return Ok(());
            };
            if !self.started || frame.data.is_empty() {
                return Ok(());
            }
            let ts = (frame.timestamp_micros - self.video_base_us) / 1000;
            if ts < 0 {
                return Ok(());
            }
            let ts = self.audio_ts.next(ts);

            let mut packet = ff::codec::packet::Packet::copy(&frame.data);
            packet.set_pts(Some(ts));
            packet.set_dts(Some(ts));
            packet.set_stream(aidx);
            packet.write_interleaved(&mut self.octx)?;
            Ok(())
        }

        /// Finalize the file and return its path. A recording that never saw a
        /// keyframe leaves an (empty) header-less file, which the caller removes.
        pub fn finish(mut self) -> Result<PathBuf, MuxError> {
            if self.started {
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
