//! Clip muxing: write a [`PreparedClip`] to an `.mkv` via ffmpeg-next using a
//! pure **stream copy** (no re-encode), so saving is instant and lossless.
//!
//! Per-codec bitstream handling (extradata such as `avcC`/`hvcC`/`av1C`,
//! Annex-B→length-prefix packet transforms) lives in [`bitstream`], keyed by
//! [`Codec`](crate::backend::Codec). Stream setup shared with the streaming
//! recorder lives in `stream`. This file only sequences them: build extradata
//! from the first keyframe, add streams, then write the packets interleaved in
//! timestamp order.
//!
//! Gated behind the `mux` feature because ffmpeg-next links the system ffmpeg
//! libraries. The pure logic ([`bitstream`] included) builds and tests without
//! it.

use std::path::Path;

#[cfg(feature = "mux")]
use bytes::Bytes;

use crate::engine::PreparedClip;

pub mod bitstream;
#[cfg(feature = "mux")]
pub(crate) mod stream;

pub use bitstream::BitstreamError;

/// Errors writing a clip.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("clip has no frames to write")]
    EmptyClip,
    #[error("clip has no keyframe")]
    NoKeyframe,
    #[error(transparent)]
    Bitstream(#[from] BitstreamError),
    #[error("ffmpeg error: {0}")]
    Ffmpeg(String),
}

#[cfg(feature = "mux")]
impl From<ffmpeg_next::Error> for MuxError {
    fn from(e: ffmpeg_next::Error) -> Self {
        MuxError::Ffmpeg(e.to_string())
    }
}

/// ffmpeg codec id for our [`Codec`](crate::backend::Codec).
#[cfg(feature = "mux")]
pub(crate) fn codec_id(codec: crate::backend::Codec) -> ffmpeg_next::codec::Id {
    use ffmpeg_next::codec::Id;
    match codec {
        crate::backend::Codec::H264 => Id::H264,
        crate::backend::Codec::Hevc => Id::HEVC,
        crate::backend::Codec::Av1 => Id::AV1,
    }
}

/// Build a standard 19-byte `OpusHead` codec-private header (RFC 7845 §5.1).
///
/// Matroska and MP4 require this blob as the Opus stream's extradata. We use a
/// mapping family of 0 (mono/stereo), a conventional 3840-sample (80ms) pre-skip,
/// and zero output gain.
#[cfg(feature = "mux")]
pub(crate) fn build_opus_head(sample_rate: u32, channels: u16) -> Vec<u8> {
    let mut h = Vec::with_capacity(19);
    h.extend_from_slice(b"OpusHead");
    h.push(1); // version
    h.push(channels.clamp(1, 255) as u8);
    h.extend_from_slice(&3840u16.to_le_bytes()); // pre-skip
    h.extend_from_slice(&sample_rate.to_le_bytes()); // input sample rate
    h.extend_from_slice(&0u16.to_le_bytes()); // output gain
    h.push(0); // channel mapping family
    h
}

/// One container packet, normalized to the shared millisecond timeline.
#[cfg(feature = "mux")]
enum Pkt {
    Video {
        dts: i64,
        pts: i64,
        key: bool,
        data: Bytes,
    },
    Audio {
        ts: i64,
        data: Bytes,
    },
}

/// Write `clip` to `path` as Matroska, stream-copying the encoded packets.
///
/// pts/dts are rebased so the first frame starts at 0. The first frame must be a
/// keyframe (guaranteed by clip selection) for the result to be decodable.
#[cfg(feature = "mux")]
pub fn write_clip(clip: &PreparedClip, path: impl AsRef<Path>) -> Result<(), MuxError> {
    use ffmpeg_next::format;

    if clip.frames.is_empty() {
        return Err(MuxError::EmptyClip);
    }

    ffmpeg_next::init()?;
    let mut octx = format::output(&path.as_ref())?;

    let first_keyframe = clip
        .frames
        .iter()
        .find(|f| f.is_keyframe)
        .ok_or(MuxError::NoKeyframe)?;
    let extradata = bitstream::extradata(clip.params.codec, &first_keyframe.data)?;
    let video_index = stream::add_video_stream(&mut octx, clip.params, &extradata)?;
    let audio_index = match clip.audio_params {
        Some(ap) if clip.has_audio() => Some(stream::add_audio_stream(&mut octx, ap)?),
        _ => None,
    };
    if !clip.chapters.is_empty() {
        let end_ms = clip.span_ticks() * 1000 / clip.params.time_base_den.max(1);
        stream::set_chapters(&mut octx, &clip.chapters, end_ms)?;
    }

    octx.write_header()?;

    for p in collect_packets(clip, audio_index.is_some()) {
        match p {
            Pkt::Video {
                dts,
                pts,
                key,
                data,
            } => {
                let mut packet = ffmpeg_next::codec::packet::Packet::copy(&data);
                packet.set_pts(Some(pts));
                packet.set_dts(Some(dts));
                packet.set_stream(video_index);
                if key {
                    packet.set_flags(ffmpeg_next::codec::packet::Flags::KEY);
                }
                packet.write_interleaved(&mut octx)?;
            }
            Pkt::Audio { ts, data } => {
                let Some(aidx) = audio_index else { continue };
                let mut packet = ffmpeg_next::codec::packet::Packet::copy(&data);
                packet.set_pts(Some(ts));
                packet.set_dts(Some(ts));
                packet.set_stream(aidx);
                packet.write_interleaved(&mut octx)?;
            }
        }
    }

    octx.write_trailer()?;
    Ok(())
}

/// Build every packet on a per-stream-monotonic millisecond timeline, MERGED in
/// timestamp order. Writing an entire stream before the other defeats
/// av_interleaved_write_frame and yields a [all video][all audio] file: players
/// must read the whole video block before reaching any audio, so audio appears
/// delayed (and seeking/streaming suffer).
#[cfg(feature = "mux")]
fn collect_packets(clip: &PreparedClip, with_audio: bool) -> Vec<Pkt> {
    // Order frames by DTS and rebase by the minimum timestamp so the clip starts
    // at 0. waycap-rs can deliver frames slightly out of order, so we never
    // assume insertion order. Timestamps are converted from the backend's time
    // base into milliseconds (1/1000), the declared stream base.
    let mut ordered: Vec<&crate::ring::EncodedFrame> = clip.frames.iter().collect();
    ordered.sort_by_key(|f| f.dts);
    let base = ordered.iter().map(|f| f.dts.min(f.pts)).min().unwrap_or(0);
    let rebase = stream::Rebaser::new(base, clip.params.time_base_den);

    let mut packets: Vec<Pkt> = Vec::with_capacity(ordered.len() + clip.audio.len());

    let mut video_dts = stream::MonotonicMs::new();
    for frame in ordered {
        let payload = bitstream::packet_payload(clip.params.codec, &frame.data);
        if payload.is_empty() {
            continue;
        }
        let dts = video_dts.next(rebase.ms(frame.dts));
        let pts = rebase.ms(frame.pts).max(dts);
        packets.push(Pkt::Video {
            dts,
            pts,
            key: frame.is_keyframe,
            data: payload,
        });
    }

    if with_audio {
        // Audio frames carry a microsecond capture timestamp. Rebase by the
        // clip's first video frame (converted to microseconds, 128-bit-safe) so
        // audio and video share t=0, then express in milliseconds.
        let video_base_us = crate::ticks_to_micros(base, clip.params.time_base_den);
        let mut audio: Vec<&crate::audio::EncodedAudioFrame> = clip.audio.iter().collect();
        audio.sort_by_key(|f| f.timestamp_micros);
        let mut audio_ts = stream::MonotonicMs::new();
        for frame in audio {
            if frame.data.is_empty() {
                continue;
            }
            let ts = audio_ts.next((frame.timestamp_micros - video_base_us) / 1000);
            // (Bytes clone below is a refcount bump, not a packet copy.)
            packets.push(Pkt::Audio {
                ts,
                data: frame.data.clone(),
            });
        }
    }

    // Merge by timestamp (video before audio on ties) for interleaved output.
    packets.sort_by_key(|p| match p {
        Pkt::Video { dts, .. } => (*dts, 0u8),
        Pkt::Audio { ts, .. } => (*ts, 1u8),
    });
    packets
}

/// Stub used when the `mux` feature is disabled, so callers compile in CI.
#[cfg(not(feature = "mux"))]
pub fn write_clip(_clip: &PreparedClip, _path: impl AsRef<Path>) -> Result<(), MuxError> {
    Err(MuxError::Ffmpeg(
        "open-recorder built without the `mux` feature".into(),
    ))
}

/// What a quick post-save verification found in the written file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipCheck {
    /// Container duration in milliseconds (0 when the container reports none).
    pub duration_ms: i64,
    pub has_video: bool,
    pub has_audio: bool,
}

/// Open a just-written clip and confirm it is a readable container with a
/// video stream and a positive duration. Runs in-process (no ffprobe child)
/// and reads only headers, so it adds ~ms to the save path — cheap insurance
/// against the classic "Saving… produced an empty file" failure mode.
#[cfg(feature = "mux")]
pub fn verify_clip(path: impl AsRef<Path>) -> Result<ClipCheck, MuxError> {
    use ffmpeg_next::media::Type;
    ffmpeg_next::init()?;
    let ictx = ffmpeg_next::format::input(&path.as_ref())?;
    let has_video = ictx.streams().best(Type::Video).is_some();
    let has_audio = ictx.streams().best(Type::Audio).is_some();
    let duration_ms = if ictx.duration() > 0 {
        // AVFormatContext duration is in AV_TIME_BASE (microseconds).
        ictx.duration() / 1000
    } else {
        0
    };
    if !has_video {
        return Err(MuxError::Ffmpeg("written clip has no video stream".into()));
    }
    if duration_ms <= 0 {
        return Err(MuxError::Ffmpeg("written clip has zero duration".into()));
    }
    Ok(ClipCheck {
        duration_ms,
        has_video,
        has_audio,
    })
}

/// Stub used when the `mux` feature is disabled: verification is skipped (the
/// stub writer never produces files to verify anyway).
#[cfg(not(feature = "mux"))]
pub fn verify_clip(_path: impl AsRef<Path>) -> Result<ClipCheck, MuxError> {
    Ok(ClipCheck {
        duration_ms: 0,
        has_video: true,
        has_audio: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Codec, StreamParams};

    fn empty_clip() -> PreparedClip {
        PreparedClip {
            frames: vec![],
            audio: vec![],
            params: StreamParams {
                width: 1920,
                height: 1080,
                fps: 60,
                codec: Codec::H264,
                time_base_den: crate::MICROS_PER_SEC,
            },
            audio_params: None,
            chapters: vec![],
        }
    }

    #[test]
    fn empty_clip_is_rejected() {
        // Both feature states reject an empty clip (one as EmptyClip, the other
        // as the no-feature stub error). Either way it does not succeed.
        let r = write_clip(&empty_clip(), "/tmp/ord-should-not-exist.mkv");
        assert!(r.is_err());
    }
}
