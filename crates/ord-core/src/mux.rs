//! Clip muxing: write a [`PreparedClip`] to an `.mkv` via ffmpeg-next using a
//! pure **stream copy** (no re-encode), so saving is instant and lossless.
//!
//! Gated behind the `mux` feature because ffmpeg-next links the system ffmpeg
//! libraries. The pure logic crates build and test without it.

use std::path::Path;

#[cfg(feature = "mux")]
use crate::backend::Codec;
use crate::engine::PreparedClip;

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

    // Add a single video stream with parameters matching the encoded data.
    let codec = ffmpeg_next::encoder::find(codec_id(clip.params.codec))
        .ok_or_else(|| MuxError::Ffmpeg("codec not found".into()))?;
    let mut stream = octx.add_stream(codec)?;
    // Microsecond time base matches our pts/dts units.
    stream.set_time_base(ffmpeg_next::Rational(1, crate::MICROS_PER_SEC as i32));

    octx.write_header()?;

    let first_pts = clip.frames.first().map(|f| f.pts).unwrap_or(0);
    for frame in &clip.frames {
        let mut packet = ffmpeg_next::codec::packet::Packet::copy(&frame.data);
        packet.set_pts(Some(frame.pts - first_pts));
        packet.set_dts(Some(frame.dts - first_pts));
        packet.set_stream(0);
        if frame.is_keyframe {
            packet.set_flags(ffmpeg_next::codec::packet::Flags::KEY);
        }
        packet.write_interleaved(&mut octx)?;
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
            params: StreamParams {
                width: 1920,
                height: 1080,
                fps: 60,
                codec: Codec::H264,
            },
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
