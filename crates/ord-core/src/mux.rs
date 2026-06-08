//! Clip muxing: write a [`PreparedClip`] to an `.mkv` via ffmpeg-next using a
//! pure **stream copy** (no re-encode), so saving is instant and lossless.
//!
//! NVENC (via waycap-rs) emits **Annex-B** H.264/HEVC (NAL units separated by
//! `00 00 00 01` start codes, with in-band SPS/PPS). Matroska/MP4 want **AVCC**
//! (length-prefixed NALs + an `avcC`/`hvcC` extradata blob). So the muxer:
//!
//! 1. extracts SPS/PPS from the first keyframe to build `avcC` extradata, and
//! 2. converts every packet's start codes to 4-byte big-endian length prefixes.
//!
//! This keeps it a true stream-copy (no re-encode) while producing a valid file.
//!
//! Gated behind the `mux` feature because ffmpeg-next links the system ffmpeg
//! libraries. The pure logic crates build and test without it.

use std::path::Path;

#[cfg(feature = "mux")]
use crate::backend::Codec;
use crate::engine::PreparedClip;

#[cfg(feature = "mux")]
mod annexb;

/// Errors writing a clip.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("clip has no frames to write")]
    EmptyClip,
    #[error("ffmpeg error: {0}")]
    Ffmpeg(String),
}

#[cfg(feature = "mux")]
impl From<ffmpeg_next::Error> for MuxError {
    fn from(e: ffmpeg_next::Error) -> Self {
        MuxError::Ffmpeg(e.to_string())
    }
}

/// ffmpeg codec id for our [`Codec`].
#[cfg(feature = "mux")]
fn codec_id(codec: Codec) -> ffmpeg_next::codec::Id {
    use ffmpeg_next::codec::Id;
    match codec {
        Codec::H264 => Id::H264,
        Codec::Hevc => Id::HEVC,
        Codec::Av1 => Id::AV1,
    }
}

/// Build a standard 19-byte `OpusHead` codec-private header (RFC 7845 §5.1).
///
/// Matroska and MP4 require this blob as the Opus stream's extradata. We use a
/// mapping family of 0 (mono/stereo), a conventional 3840-sample (80ms) pre-skip,
/// and zero output gain.
#[cfg(feature = "mux")]
fn build_opus_head(sample_rate: u32, channels: u16) -> Vec<u8> {
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

    let path = path.as_ref();
    let mut octx = format::output(&path)?;

    // NVENC emits Annex-B; mkv/mp4 need AVCC + avcC extradata. Build the avcC
    // from the first keyframe's SPS/PPS. (H.264 path; other codecs pass through.)
    let is_h264 = matches!(clip.params.codec, Codec::H264);
    let first_keyframe = clip
        .frames
        .iter()
        .find(|f| f.is_keyframe)
        .ok_or_else(|| MuxError::Ffmpeg("clip has no keyframe".into()))?;
    let extradata = if is_h264 {
        annexb::build_avcc(&first_keyframe.data)
            .ok_or_else(|| MuxError::Ffmpeg("could not build avcC (missing SPS/PPS)".into()))?
    } else {
        Vec::new()
    };

    let codec = ffmpeg_next::encoder::find(codec_id(clip.params.codec))
        .ok_or_else(|| MuxError::Ffmpeg("codec not found".into()))?;
    let stream_index;
    {
        let mut stream = octx.add_stream(codec)?;
        // We normalize timestamps to milliseconds ourselves (below) and declare a
        // 1/1000 time base, which Matroska uses natively — avoiding rescale
        // ambiguity between the backend's base (waycap: ns) and the container.
        stream.set_time_base(ffmpeg_next::Rational(1, 1000));
        stream_index = stream.index();

        // SAFETY: codecpar is a valid AVCodecParameters owned by the stream. We
        // set the fields a copy muxer requires plus the avcC extradata, which we
        // allocate with av_malloc so ffmpeg can free it.
        unsafe {
            let par = (*stream.as_ptr()).codecpar;
            if par.is_null() {
                return Err(MuxError::Ffmpeg("stream has no codecpar".into()));
            }
            (*par).codec_type = ffmpeg_next::ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
            (*par).codec_id = codec_id(clip.params.codec).into();
            (*par).width = clip.params.width as i32;
            (*par).height = clip.params.height as i32;
            if !extradata.is_empty() {
                let size = extradata.len();
                let buf = ffmpeg_next::ffi::av_malloc(
                    size + ffmpeg_next::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize,
                ) as *mut u8;
                if buf.is_null() {
                    return Err(MuxError::Ffmpeg("av_malloc failed for extradata".into()));
                }
                std::ptr::copy_nonoverlapping(extradata.as_ptr(), buf, size);
                std::ptr::write_bytes(
                    buf.add(size),
                    0,
                    ffmpeg_next::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize,
                );
                (*par).extradata = buf;
                (*par).extradata_size = size as i32;
            }
        }
    }

    // Optional audio stream (Opus passthrough). Added before write_header so the
    // muxer knows about both streams. The audio packets carry a microsecond
    // capture timestamp which we rebase to the same millisecond timeline as
    // video, keeping A/V in sync.
    let audio_stream_index = if clip.has_audio() {
        let ap = clip.audio_params.expect("has_audio checked");
        let acodec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::OPUS)
            .ok_or_else(|| MuxError::Ffmpeg("opus codec not found".into()))?;
        let mut astream = octx.add_stream(acodec)?;
        astream.set_time_base(ffmpeg_next::Rational(1, 1000));
        let idx = astream.index();
        // Matroska/MP4 Opus needs an `OpusHead` codec-private blob; build a
        // standard 19-byte header from the negotiated sample rate / channels.
        let opus_head = build_opus_head(ap.sample_rate, ap.channels);
        // SAFETY: codecpar is valid for the lifetime of the stream; we set the
        // fields the Opus muxer needs for a copy stream plus OpusHead extradata
        // (allocated with av_malloc so ffmpeg frees it).
        unsafe {
            let par = (*astream.as_ptr()).codecpar;
            if par.is_null() {
                return Err(MuxError::Ffmpeg("audio stream has no codecpar".into()));
            }
            (*par).codec_type = ffmpeg_next::ffi::AVMediaType::AVMEDIA_TYPE_AUDIO;
            (*par).codec_id = ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_OPUS;
            (*par).sample_rate = ap.sample_rate as i32;
            (*par).ch_layout.nb_channels = ap.channels as i32;
            let size = opus_head.len();
            let buf = ffmpeg_next::ffi::av_malloc(
                size + ffmpeg_next::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize,
            ) as *mut u8;
            if buf.is_null() {
                return Err(MuxError::Ffmpeg("av_malloc failed for OpusHead".into()));
            }
            std::ptr::copy_nonoverlapping(opus_head.as_ptr(), buf, size);
            std::ptr::write_bytes(
                buf.add(size),
                0,
                ffmpeg_next::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize,
            );
            (*par).extradata = buf;
            (*par).extradata_size = size as i32;
        }
        Some(idx)
    } else {
        None
    };

    octx.write_header()?;

    // Order packets by DTS (the muxer requires monotonic DTS) and rebase by the
    // minimum timestamp so the clip starts at 0. waycap-rs can deliver frames
    // slightly out of order, so we never assume insertion order. Timestamps are
    // converted from the backend's time base into milliseconds (1/1000), the
    // declared stream base.
    let mut ordered: Vec<&crate::ring::EncodedFrame> = clip.frames.iter().collect();
    ordered.sort_by_key(|f| f.dts);
    let base = ordered.iter().map(|f| f.dts.min(f.pts)).min().unwrap_or(0);
    let den = clip.params.time_base_den.max(1);
    let to_ms = |t: i64| -> i64 { (t - base) * 1000 / den };

    // Audio rebasing: audio frames carry a microsecond capture timestamp. Rebase
    // by the clip's first video frame (converted to microseconds, 128-bit-safe)
    // so audio and video share t=0, then express in milliseconds (the stream
    // time base) to match the video packets.
    let video_base_us = crate::ticks_to_micros(base, den);
    let audio_to_ms = |ts_us: i64| -> i64 { (ts_us - video_base_us) / 1000 };

    // Build every packet with a per-stream-monotonic millisecond timestamp, then
    // write them MERGED in timestamp order. Writing an entire stream before the
    // other defeats av_interleaved_write_frame and yields a [all video][all
    // audio] file: players must read the whole video block before reaching any
    // audio, so audio appears delayed (and seeking/streaming suffer).
    enum Pkt {
        Video {
            dts: i64,
            pts: i64,
            key: bool,
            data: Vec<u8>,
        },
        Audio {
            ts: i64,
            data: Vec<u8>,
        },
    }

    let mut packets: Vec<Pkt> = Vec::with_capacity(ordered.len() + clip.audio.len());

    // Video: keep DTS strictly increasing (bump ties by 1ms).
    let mut last_dts = i64::MIN;
    for frame in ordered {
        // Convert Annex-B -> AVCC (length-prefixed, SPS/PPS stripped) for H.264.
        let payload = if is_h264 {
            annexb::to_avcc(&frame.data)
        } else {
            frame.data.clone()
        };
        if payload.is_empty() {
            continue;
        }
        let mut dts = to_ms(frame.dts);
        if dts <= last_dts {
            dts = last_dts + 1;
        }
        last_dts = dts;
        let pts = to_ms(frame.pts).max(dts);
        packets.push(Pkt::Video {
            dts,
            pts,
            key: frame.is_keyframe,
            data: payload,
        });
    }

    // Audio (Opus passthrough), if present.
    if audio_stream_index.is_some() {
        let mut audio: Vec<&crate::audio::EncodedAudioFrame> = clip.audio.iter().collect();
        audio.sort_by_key(|f| f.timestamp_micros);
        let mut last_audio_ts = i64::MIN;
        for frame in audio {
            if frame.data.is_empty() {
                continue;
            }
            let mut ts = audio_to_ms(frame.timestamp_micros);
            if ts <= last_audio_ts {
                ts = last_audio_ts + 1;
            }
            last_audio_ts = ts;
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

    for p in packets {
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
                packet.set_stream(stream_index);
                if key {
                    packet.set_flags(ffmpeg_next::codec::packet::Flags::KEY);
                }
                packet.write_interleaved(&mut octx)?;
            }
            Pkt::Audio { ts, data } => {
                let astream =
                    audio_stream_index.expect("audio packets only built when the stream exists");
                let mut packet = ffmpeg_next::codec::packet::Packet::copy(&data);
                packet.set_pts(Some(ts));
                packet.set_dts(Some(ts));
                packet.set_stream(astream);
                packet.write_interleaved(&mut octx)?;
            }
        }
    }

    octx.write_trailer()?;
    Ok(())
}

/// Stub used when the `mux` feature is disabled, so callers compile in CI.
#[cfg(not(feature = "mux"))]
pub fn write_clip(_clip: &PreparedClip, _path: impl AsRef<Path>) -> Result<(), MuxError> {
    Err(MuxError::Ffmpeg(
        "open-recorder built without the `mux` feature".into(),
    ))
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
