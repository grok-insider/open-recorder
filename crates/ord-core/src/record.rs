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

    use crate::backend::Codec;
    use crate::mux::{annexb, build_opus_head, codec_id};

    /// An open recording: a live ffmpeg output context fed encoded packets.
    pub struct Recorder {
        octx: ff::format::context::Output,
        path: PathBuf,
        params: StreamParams,
        audio_params: Option<AudioParams>,
        is_h264: bool,
        video_index: usize,
        audio_index: Option<usize>,
        /// Whether the header has been written (waits for the first keyframe).
        started: bool,
        /// Timestamp (ticks) of the first frame, rebased to 0.
        base: i64,
        video_base_us: i64,
        last_dts_ms: i64,
        last_audio_ms: i64,
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
                is_h264: matches!(params.codec, Codec::H264),
                params,
                audio_params,
                video_index: 0,
                audio_index: None,
                started: false,
                base: 0,
                video_base_us: 0,
                last_dts_ms: i64::MIN,
                last_audio_ms: i64::MIN,
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
            let payload = if self.is_h264 {
                annexb::to_avcc(&frame.data)
            } else {
                frame.data.to_vec()
            };
            if payload.is_empty() {
                return Ok(());
            }
            let mut dts = (frame.dts - self.base) * 1000 / den;
            if dts <= self.last_dts_ms {
                dts = self.last_dts_ms + 1;
            }
            self.last_dts_ms = dts;
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
            let mut ts = (frame.timestamp_micros - self.video_base_us) / 1000;
            if ts < 0 {
                return Ok(());
            }
            if ts <= self.last_audio_ms {
                ts = self.last_audio_ms + 1;
            }
            self.last_audio_ms = ts;

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
            let extradata = if self.is_h264 {
                annexb::build_avcc(&first_keyframe.data).ok_or_else(|| {
                    MuxError::Ffmpeg("could not build avcC (missing SPS/PPS)".into())
                })?
            } else {
                Vec::new()
            };

            let codec = ff::encoder::find(codec_id(self.params.codec))
                .ok_or_else(|| MuxError::Ffmpeg("codec not found".into()))?;
            {
                let mut stream = self.octx.add_stream(codec)?;
                stream.set_time_base(ff::Rational(1, 1000));
                self.video_index = stream.index();
                // SAFETY: codecpar is a valid AVCodecParameters owned by the stream;
                // we set the copy-mux fields + avcC extradata (av_malloc'd so ffmpeg
                // frees it), mirroring the clip muxer.
                unsafe {
                    let par = (*stream.as_ptr()).codecpar;
                    if par.is_null() {
                        return Err(MuxError::Ffmpeg("stream has no codecpar".into()));
                    }
                    (*par).codec_type = ff::ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
                    (*par).codec_id = codec_id(self.params.codec).into();
                    (*par).width = self.params.width as i32;
                    (*par).height = self.params.height as i32;
                    if !extradata.is_empty() {
                        let size = extradata.len();
                        let buf = ff::ffi::av_malloc(
                            size + ff::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize,
                        ) as *mut u8;
                        if buf.is_null() {
                            return Err(MuxError::Ffmpeg("av_malloc failed for extradata".into()));
                        }
                        std::ptr::copy_nonoverlapping(extradata.as_ptr(), buf, size);
                        std::ptr::write_bytes(
                            buf.add(size),
                            0,
                            ff::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize,
                        );
                        (*par).extradata = buf;
                        (*par).extradata_size = size as i32;
                    }
                }
            }

            if let Some(ap) = self.audio_params {
                let acodec = ff::encoder::find(ff::codec::Id::OPUS)
                    .ok_or_else(|| MuxError::Ffmpeg("opus codec not found".into()))?;
                let mut astream = self.octx.add_stream(acodec)?;
                astream.set_time_base(ff::Rational(1, 1000));
                self.audio_index = Some(astream.index());
                let opus_head = build_opus_head(ap.sample_rate, ap.channels);
                // SAFETY: as above, for the Opus copy stream + OpusHead extradata.
                unsafe {
                    let par = (*astream.as_ptr()).codecpar;
                    if par.is_null() {
                        return Err(MuxError::Ffmpeg("audio stream has no codecpar".into()));
                    }
                    (*par).codec_type = ff::ffi::AVMediaType::AVMEDIA_TYPE_AUDIO;
                    (*par).codec_id = ff::ffi::AVCodecID::AV_CODEC_ID_OPUS;
                    (*par).sample_rate = ap.sample_rate as i32;
                    (*par).ch_layout.nb_channels = ap.channels as i32;
                    let size = opus_head.len();
                    let buf =
                        ff::ffi::av_malloc(size + ff::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize)
                            as *mut u8;
                    if buf.is_null() {
                        return Err(MuxError::Ffmpeg("av_malloc failed for OpusHead".into()));
                    }
                    std::ptr::copy_nonoverlapping(opus_head.as_ptr(), buf, size);
                    std::ptr::write_bytes(
                        buf.add(size),
                        0,
                        ff::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize,
                    );
                    (*par).extradata = buf;
                    (*par).extradata_size = size as i32;
                }
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
