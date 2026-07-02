//! Shared ffmpeg stream setup for the clip muxer and the streaming recorder.
//!
//! Both muxers write the same kind of file: a stream-copied video track (codec
//! private built by [`bitstream`](super::bitstream)) plus an optional Opus
//! track, on a 1/1000 time base. This module owns that setup so the two can
//! never drift apart; it is the only place outside backends allowed to touch
//! `codecpar` via FFI.

use ffmpeg_next as ff;

use super::{build_opus_head, codec_id, MuxError};
use crate::audio::AudioParams;
use crate::backend::StreamParams;

/// Milliseconds time base shared by every stream we write. We normalize
/// timestamps to milliseconds ourselves and declare 1/1000, which Matroska uses
/// natively — avoiding rescale ambiguity between the backend's base (waycap:
/// ns) and the container.
const MS_TIME_BASE: ff::Rational = ff::Rational(1, 1000);

/// Add the stream-copied video stream, returning its index. `extradata` is the
/// codec-private blob from [`bitstream::extradata`](super::bitstream::extradata)
/// (may be empty for codecs that need none).
pub(crate) fn add_video_stream(
    octx: &mut ff::format::context::Output,
    params: StreamParams,
    extradata: &[u8],
) -> Result<usize, MuxError> {
    let codec = ff::encoder::find(codec_id(params.codec))
        .ok_or_else(|| MuxError::Ffmpeg("codec not found".into()))?;
    let mut stream = octx.add_stream(codec)?;
    stream.set_time_base(MS_TIME_BASE);
    let index = stream.index();

    // SAFETY: codecpar is a valid AVCodecParameters owned by the stream. We set
    // the fields a copy muxer requires plus the codec-private extradata, which
    // is allocated with av_malloc so ffmpeg can free it.
    unsafe {
        let par = (*stream.as_ptr()).codecpar;
        if par.is_null() {
            return Err(MuxError::Ffmpeg("stream has no codecpar".into()));
        }
        (*par).codec_type = ff::ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
        (*par).codec_id = codec_id(params.codec).into();
        (*par).width = params.width as i32;
        (*par).height = params.height as i32;
        set_extradata(par, extradata)?;
    }
    Ok(index)
}

/// Add the Opus passthrough audio stream, returning its index. Builds the
/// `OpusHead` codec-private blob Matroska/MP4 require.
pub(crate) fn add_audio_stream(
    octx: &mut ff::format::context::Output,
    ap: AudioParams,
) -> Result<usize, MuxError> {
    let acodec = ff::encoder::find(ff::codec::Id::OPUS)
        .ok_or_else(|| MuxError::Ffmpeg("opus codec not found".into()))?;
    let mut astream = octx.add_stream(acodec)?;
    astream.set_time_base(MS_TIME_BASE);
    let index = astream.index();
    let opus_head = build_opus_head(ap.sample_rate, ap.channels);

    // SAFETY: codecpar is valid for the lifetime of the stream; we set the
    // fields the Opus muxer needs for a copy stream plus OpusHead extradata.
    unsafe {
        let par = (*astream.as_ptr()).codecpar;
        if par.is_null() {
            return Err(MuxError::Ffmpeg("audio stream has no codecpar".into()));
        }
        (*par).codec_type = ff::ffi::AVMediaType::AVMEDIA_TYPE_AUDIO;
        (*par).codec_id = ff::ffi::AVCodecID::AV_CODEC_ID_OPUS;
        (*par).sample_rate = ap.sample_rate as i32;
        (*par).ch_layout.nb_channels = ap.channels as i32;
        set_extradata(par, &opus_head)?;
    }
    Ok(index)
}

/// Copy `data` into an av_malloc'd, zero-padded buffer and attach it as the
/// parameter set's extradata. No-op when `data` is empty.
///
/// SAFETY: caller guarantees `par` points at a valid AVCodecParameters.
unsafe fn set_extradata(par: *mut ff::ffi::AVCodecParameters, data: &[u8]) -> Result<(), MuxError> {
    if data.is_empty() {
        return Ok(());
    }
    let size = data.len();
    let buf = ff::ffi::av_malloc(size + ff::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize) as *mut u8;
    if buf.is_null() {
        return Err(MuxError::Ffmpeg("av_malloc failed for extradata".into()));
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), buf, size);
    std::ptr::write_bytes(
        buf.add(size),
        0,
        ff::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize,
    );
    (*par).extradata = buf;
    (*par).extradata_size = size as i32;
    Ok(())
}

/// Attach chapter marks (millisecond offsets from the clip start) to the
/// output before `write_header`. Each chapter is titled "Marker N" and runs to
/// the next mark (the last one to `end_ms`). Markers are how "clip that"
/// bookmarks survive into the saved file — players and editors show them as a
/// chapter list.
pub(crate) fn set_chapters(
    octx: &mut ff::format::context::Output,
    marks_ms: &[i64],
    end_ms: i64,
) -> Result<(), MuxError> {
    if marks_ms.is_empty() {
        return Ok(());
    }
    // SAFETY: we build an av_malloc'd AVChapter array exactly as ffmpeg's own
    // demuxers do and hand ownership to the AVFormatContext, which frees the
    // chapters, their metadata, and the array in avformat_free_context.
    unsafe {
        let n = marks_ms.len();
        let arr = ff::ffi::av_calloc(n, std::mem::size_of::<*mut ff::ffi::AVChapter>())
            as *mut *mut ff::ffi::AVChapter;
        if arr.is_null() {
            return Err(MuxError::Ffmpeg("av_calloc failed for chapters".into()));
        }
        for (i, &start) in marks_ms.iter().enumerate() {
            let ch = ff::ffi::av_mallocz(std::mem::size_of::<ff::ffi::AVChapter>())
                as *mut ff::ffi::AVChapter;
            if ch.is_null() {
                return Err(MuxError::Ffmpeg("av_mallocz failed for chapter".into()));
            }
            (*ch).id = i as i64 + 1;
            (*ch).time_base = ff::ffi::AVRational { num: 1, den: 1000 };
            (*ch).start = start.max(0);
            let end = marks_ms.get(i + 1).copied().unwrap_or(end_ms).max(start);
            (*ch).end = end;
            let title = std::ffi::CString::new(format!("Marker {}", i + 1))
                .unwrap_or_else(|_| std::ffi::CString::default());
            ff::ffi::av_dict_set(&mut (*ch).metadata, c"title".as_ptr(), title.as_ptr(), 0);
            *arr.add(i) = ch;
        }
        let raw = octx.as_mut_ptr();
        (*raw).chapters = arr;
        (*raw).nb_chapters = n as u32;
    }
    Ok(())
}

/// Rebases capture-clock timestamps (ticks at `den`/sec, anchored at `base`)
/// onto the shared millisecond stream timeline. Both muxers must use this —
/// hand-rolled `(t - base) * 1000 / den` arithmetic in one of them is exactly
/// the codec/stream-setup drift AGENTS.md forbids.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rebaser {
    base: i64,
    den: i64,
}

impl Rebaser {
    /// Anchor the timeline at `base` ticks with `den` ticks per second.
    pub(crate) fn new(base: i64, den: i64) -> Self {
        Self {
            base,
            den: den.max(1),
        }
    }

    /// Convert `t` ticks to milliseconds on the rebased timeline.
    pub(crate) fn ms(&self, t: i64) -> i64 {
        (t - self.base) * 1000 / self.den
    }
}

/// Keeps a millisecond timestamp sequence strictly increasing (the muxer
/// requires monotonic DTS): equal-or-earlier values are bumped 1 ms past the
/// previous one.
pub(crate) struct MonotonicMs {
    last: i64,
}

impl MonotonicMs {
    pub(crate) fn new() -> Self {
        Self { last: i64::MIN }
    }

    pub(crate) fn next(&mut self, t: i64) -> i64 {
        let t = if t <= self.last { self.last + 1 } else { t };
        self.last = t;
        t
    }
}

#[cfg(test)]
mod tests {
    use super::{MonotonicMs, Rebaser};

    #[test]
    fn monotonic_bumps_ties_and_regressions() {
        let mut m = MonotonicMs::new();
        assert_eq!(m.next(0), 0);
        assert_eq!(m.next(0), 1);
        assert_eq!(m.next(1), 2);
        assert_eq!(m.next(10), 10);
        assert_eq!(m.next(5), 11);
    }

    #[test]
    fn rebaser_anchors_and_converts_to_ms() {
        // Nanosecond base (waycap): 1 s past the anchor is 1000 ms.
        let r = Rebaser::new(5_000_000_000, 1_000_000_000);
        assert_eq!(r.ms(5_000_000_000), 0);
        assert_eq!(r.ms(6_000_000_000), 1000);
        // A zero den is clamped, never a divide-by-zero.
        assert_eq!(Rebaser::new(0, 0).ms(7), 7000);
    }
}
