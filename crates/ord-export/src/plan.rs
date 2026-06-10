//! Pure ffmpeg-invocation planner.
//!
//! [`build_plan`] turns an [`ExportProfile`] plus probed [`SourceInfo`] (and an
//! optional [`Trim`]) into the exact `ffmpeg` argument vector to run. It performs
//! no I/O and spawns nothing, so every policy decision — encoder selection, rate
//! control, target-size bitrate math, scaling, audio handling — is unit-tested
//! here. The runner ([`crate::run`]) just executes the returned args.

use ord_common::config::{Container, ExportCodec};

use crate::profile::{ExportProfile, FrameRate, Output, RateControl, Scale};
use crate::ExportError;

/// Properties of the input file, normally filled in by [`crate::probe`].
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInfo {
    pub duration_secs: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub has_audio: bool,
    /// Source audio codec name as ffprobe reports it (e.g. `"opus"`, `"aac"`).
    pub audio_codec: Option<String>,
}

/// An inclusive-start, exclusive-end trim window in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trim {
    pub start_secs: f64,
    pub end_secs: f64,
}

impl Trim {
    pub fn duration(&self) -> f64 {
        (self.end_secs - self.start_secs).max(0.0)
    }

    /// Reject nonsensical windows (negative, zero-length, or reversed).
    pub fn validate(&self) -> Result<(), ExportError> {
        if !self.start_secs.is_finite() || !self.end_secs.is_finite() || self.start_secs < 0.0 {
            return Err(ExportError::InvalidTrim);
        }
        if self.end_secs <= self.start_secs {
            return Err(ExportError::InvalidTrim);
        }
        Ok(())
    }
}

/// The planned ffmpeg invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct FfmpegPlan {
    /// Arguments to pass to `ffmpeg` (not including the program name).
    pub args: Vec<String>,
    /// The chosen video encoder (e.g. `"av1_nvenc"`). Empty for a stream copy.
    pub encoder: String,
    /// Whether this plan uses a hardware (NVENC) encoder.
    pub uses_hardware: bool,
}

/// Resolve the ffmpeg encoder name for a codec on hardware vs. software.
fn encoder_name(codec: ExportCodec, hardware: bool) -> &'static str {
    match (codec, hardware) {
        (ExportCodec::Av1, true) => "av1_nvenc",
        (ExportCodec::Av1, false) => "libsvtav1",
        (ExportCodec::Hevc, true) => "hevc_nvenc",
        (ExportCodec::Hevc, false) => "libx265",
        (ExportCodec::H264, true) => "h264_nvenc",
        (ExportCodec::H264, false) => "libx264",
    }
}

/// Speed/efficiency preset flag for the chosen encoder.
fn preset_args(encoder: &str) -> Vec<String> {
    match encoder {
        e if e.ends_with("_nvenc") => vec!["-preset".into(), "p5".into()],
        "libsvtav1" => vec!["-preset".into(), "6".into()],
        "libx264" | "libx265" => vec!["-preset".into(), "medium".into()],
        _ => vec![],
    }
}

/// Compute a video bitrate (kbps) to hit a target file size over `duration`.
///
/// The 8% headroom covers container overhead and rate-control slack; combined
/// with a hard `maxrate` cap (see [`bitrate_args`]) this keeps the finished file
/// under the requested size even on high-bitrate content.
fn target_size_kbps(mib: f64, duration: f64, has_audio: bool, audio_kbps: u32) -> u32 {
    if duration <= 0.0 {
        return 100;
    }
    let budget_bits = mib * 1024.0 * 1024.0 * 8.0 * 0.92;
    let audio_bits = if has_audio {
        audio_kbps as f64 * 1000.0 * duration
    } else {
        0.0
    };
    let video_bits = budget_bits - audio_bits;
    let kbps = (video_bits / duration) / 1000.0;
    kbps.floor().max(100.0) as u32
}

/// Estimate the finished file size (MiB) for `profile` over `duration` seconds.
/// `None` when the size isn't predictable (stream copy / quality-based modes).
/// Pure planner math — mirrors exactly how the bitrates are chosen, including
/// the 100 kbps video floor that makes very long size-targeted clips overshoot.
pub fn estimated_output_mib(profile: &ExportProfile, duration: f64) -> Option<f64> {
    if duration <= 0.0 {
        return Some(0.0);
    }
    let audio_kbps = if profile.mute { 0 } else { profile.audio_kbps };
    let video_kbps = match profile.rate_control {
        RateControl::TargetSize { mib } => {
            target_size_kbps(mib, duration, !profile.mute, profile.audio_kbps)
        }
        RateControl::Bitrate { kbps } => kbps,
        _ => return None,
    };
    let bits = (video_kbps + audio_kbps) as f64 * 1000.0 * duration;
    Some(bits / 8.0 / (1024.0 * 1024.0))
}

/// Rate-control flags for the encoder.
fn rate_control_args(
    encoder: &str,
    rc: RateControl,
    duration: f64,
    has_audio: bool,
    audio_kbps: u32,
) -> Vec<String> {
    let is_nvenc = encoder.ends_with("_nvenc");
    match rc {
        RateControl::Quality(q) => {
            if is_nvenc {
                // Constant-quality VBR: -cq with -b:v 0.
                vec![
                    "-rc".into(),
                    "vbr".into(),
                    "-cq".into(),
                    q.to_string(),
                    "-b:v".into(),
                    "0".into(),
                ]
            } else {
                vec!["-crf".into(), q.to_string()]
            }
        }
        // An explicit average bitrate: VBR with headroom so bursts look good.
        RateControl::Bitrate { kbps } => bitrate_args(is_nvenc, kbps, false),
        // A size target: a hard cap (maxrate == bitrate) so the file fits.
        RateControl::TargetSize { mib } => {
            let kbps = target_size_kbps(mib, duration, has_audio, audio_kbps);
            bitrate_args(is_nvenc, kbps, true)
        }
        // Copy is handled before we ever pick an encoder.
        RateControl::Copy => vec![],
    }
}

/// Bitrate flags. When `capped`, `maxrate == bitrate` and a 1-second VBV buffer
/// (NVENC in CBR) hold the output to the target size; otherwise it is VBR with
/// 1.5× burst headroom for a quality-oriented average bitrate.
fn bitrate_args(is_nvenc: bool, kbps: u32, capped: bool) -> Vec<String> {
    let (maxrate, bufsize, nvenc_rc) = if capped {
        (kbps, kbps, "cbr")
    } else {
        (kbps.saturating_mul(3) / 2, kbps.saturating_mul(2), "vbr")
    };
    let mut a = vec![
        "-b:v".into(),
        format!("{kbps}k"),
        "-maxrate".into(),
        format!("{maxrate}k"),
        "-bufsize".into(),
        format!("{bufsize}k"),
    ];
    if is_nvenc {
        a.splice(0..0, ["-rc".to_string(), nvenc_rc.to_string()]);
        if capped {
            // Two-pass within NVENC: tighter size adherence + better quality at
            // the target bitrate (cheap on the GPU).
            a.extend(["-multipass".to_string(), "fullres".to_string()]);
        }
    }
    a
}

/// Build the `-vf` scale filter, if any, for the requested [`Scale`].
fn scale_filter(scale: Scale, src: &SourceInfo) -> Option<String> {
    match scale {
        Scale::Source => None,
        Scale::MaxHeight(h) => {
            if src.height > h {
                // -2 keeps the width even and preserves the aspect ratio.
                Some(format!("scale=-2:{h}:flags=lanczos"))
            } else {
                None
            }
        }
        Scale::Exact { width, height } => Some(format!("scale={width}:{height}:flags=lanczos")),
        Scale::Vertical { height } => {
            // Center-crop the source to 9:16 (full height, 9/16·h wide — every
            // gameplay source is wider than that), then scale to the target.
            // `min(iw,…)` keeps the crop legal for already-narrow sources, and
            // `floor(/2)*2` keeps the encoder's even-dimension requirement.
            let width = (height * 9 / 16) & !1;
            Some(format!(
                "crop=min(iw\\,floor(ih*9/32)*2):ih,scale={width}:{height}:flags=lanczos"
            ))
        }
    }
}

/// Audio output flags for a re-encode (the copy path sets `-c copy` globally).
fn audio_args(profile: &ExportProfile, src: &SourceInfo) -> Vec<String> {
    if profile.mute || !src.has_audio {
        return vec!["-an".into()];
    }
    // Target-size must control audio size, and loudnorm is a filter, so both
    // force a transcode (you can't filter or re-bitrate a copied stream).
    let force_transcode =
        matches!(profile.rate_control, RateControl::TargetSize { .. }) || profile.normalize_audio;
    let kbps = profile.audio_kbps.max(1);
    let mut a = Vec::new();
    if profile.normalize_audio {
        a.push("-af".into());
        a.push("loudnorm".into());
    }
    match profile.container {
        Container::Mkv => {
            let is_opus = src.audio_codec.as_deref() == Some("opus");
            if is_opus && !force_transcode {
                a.push("-c:a".into());
                a.push("copy".into());
            } else {
                a.extend([
                    "-c:a".into(),
                    "libopus".into(),
                    "-b:a".into(),
                    format!("{kbps}k"),
                ]);
            }
        }
        // MP4: AAC is the universally-playable choice (Discord inline, QuickTime).
        Container::Mp4 => {
            a.extend([
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                format!("{kbps}k"),
            ]);
        }
    }
    a
}

/// The audio codec name for an audio-only extraction into `container`.
fn audio_codec_for(container: Container) -> &'static str {
    match container {
        Container::Mp4 => "aac",
        Container::Mkv => "libopus",
    }
}

/// Args for an audio-only extraction (`-vn` + transcode to the container codec).
fn audio_only_args(args: &mut Vec<String>, profile: &ExportProfile, output: &str) {
    args.push("-vn".into());
    if profile.normalize_audio {
        args.push("-af".into());
        args.push("loudnorm".into());
    }
    let kbps = profile.audio_kbps.max(1);
    args.push("-c:a".into());
    args.push(audio_codec_for(profile.container).into());
    args.push("-b:a".into());
    args.push(format!("{kbps}k"));
    if profile.container == Container::Mp4 {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }
    args.push(output.into());
}

/// Args for a palette-based animated GIF (`scale`/`fps` size it; no audio).
fn gif_args(args: &mut Vec<String>, profile: &ExportProfile, output: &str) {
    let fps = match profile.fps {
        FrameRate::Fixed(f) => f.max(1),
        FrameRate::Source => 15,
    };
    let height = match profile.scale {
        Scale::MaxHeight(h) => h,
        Scale::Exact { height, .. } | Scale::Vertical { height } => height,
        Scale::Source => 480,
    };
    // One pass: generate a palette from the frames and apply it (Bayer dithering
    // keeps gradients clean without ballooning the file).
    let vf = format!(
        "fps={fps},scale=-2:{height}:flags=lanczos,split[s0][s1];\
         [s0]palettegen=stats_mode=diff[p];\
         [s1][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle"
    );
    args.push("-vf".into());
    args.push(vf);
    args.push("-loop".into());
    args.push("0".into());
    args.push("-an".into());
    args.push(output.into());
}

/// Build the full ffmpeg argument vector for an export.
///
/// `use_hardware` lets the runner retry with software encoding after an NVENC
/// failure; it is ANDed with the profile's `hardware` flag.
pub fn build_plan(
    input: &str,
    output: &str,
    profile: &ExportProfile,
    src: &SourceInfo,
    trim: Option<Trim>,
    use_hardware: bool,
) -> Result<FfmpegPlan, ExportError> {
    if let Some(t) = trim {
        t.validate()?;
    }
    let duration = trim.map(|t| t.duration()).unwrap_or(src.duration_secs);
    if duration <= 0.0 {
        return Err(ExportError::ZeroDuration);
    }

    let mut args: Vec<String> = vec!["-y".into(), "-hide_banner".into()];

    // Fast, accurate input seek: -ss before -i.
    if let Some(t) = trim {
        args.push("-ss".into());
        args.push(format!("{:.3}", t.start_secs));
    }
    args.push("-i".into());
    args.push(input.into());
    if let Some(t) = trim {
        args.push("-t".into());
        args.push(format!("{:.3}", t.duration()));
    }

    // Non-video outputs ignore the video codec/rate-control entirely.
    match profile.output {
        Output::Gif => {
            gif_args(&mut args, profile, output);
            return Ok(FfmpegPlan {
                args,
                encoder: "gif".to_string(),
                uses_hardware: false,
            });
        }
        Output::AudioOnly => {
            if !src.has_audio {
                return Err(ExportError::Io(
                    "source has no audio track for an audio-only export".into(),
                ));
            }
            audio_only_args(&mut args, profile, output);
            return Ok(FfmpegPlan {
                args,
                encoder: String::new(),
                uses_hardware: false,
            });
        }
        Output::Video => {}
    }

    let hardware = profile.hardware && use_hardware;

    if !profile.reencodes() {
        // Stream copy: instant, lossless remux.
        if profile.mute {
            args.push("-c:v".into());
            args.push("copy".into());
            args.push("-an".into());
        } else {
            args.push("-c".into());
            args.push("copy".into());
        }
        if profile.container == Container::Mp4 {
            args.push("-movflags".into());
            args.push("+faststart".into());
        }
        args.push(output.into());
        return Ok(FfmpegPlan {
            args,
            encoder: String::new(),
            uses_hardware: false,
        });
    }

    let encoder = encoder_name(profile.codec, hardware);

    if let Some(vf) = scale_filter(profile.scale, src) {
        args.push("-vf".into());
        args.push(vf);
    }
    if let FrameRate::Fixed(f) = profile.fps {
        args.push("-r".into());
        args.push(f.to_string());
    }

    args.push("-c:v".into());
    args.push(encoder.into());
    args.extend(preset_args(encoder));
    args.extend(rate_control_args(
        encoder,
        profile.rate_control,
        duration,
        src.has_audio,
        profile.audio_kbps,
    ));
    args.push("-pix_fmt".into());
    args.push("yuv420p".into());

    args.extend(audio_args(profile, src));

    if profile.container == Container::Mp4 {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }

    args.push(output.into());

    Ok(FfmpegPlan {
        args,
        encoder: encoder.to_string(),
        uses_hardware: hardware,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ExportProfile;

    fn src() -> SourceInfo {
        SourceInfo {
            duration_secs: 30.0,
            width: 2560,
            height: 1440,
            fps: 60.0,
            has_audio: true,
            audio_codec: Some("opus".into()),
        }
    }

    fn joined(plan: &FfmpegPlan) -> String {
        plan.args.join(" ")
    }

    #[test]
    fn vertical_preset_crops_to_9x16_and_scales() {
        let p = ExportProfile::vertical();
        let plan = build_plan("in.mkv", "out.mp4", &p, &src(), None, true).unwrap();
        let s = joined(&plan);
        assert!(s.contains("crop=min(iw\\,floor(ih*9/32)*2):ih"), "{s}");
        assert!(s.contains("scale=1080:1920"), "{s}");
        assert_eq!(plan.encoder, "h264_nvenc");
    }

    #[test]
    fn estimated_size_tracks_the_target_budget() {
        let p = ExportProfile::discord(); // TargetSize { mib: 9.0 }
                                          // A normal clip lands just under the budget (0.92 container overhead).
        let est = estimated_output_mib(&p, 30.0).unwrap();
        assert!((7.5..=9.0).contains(&est), "got {est}");
        // A very long clip hits the 100 kbps video floor and overshoots.
        let long = estimated_output_mib(&p, 3600.0).unwrap();
        assert!(long > 9.0, "got {long}");
        // Short selections shrink toward the budget, never balloon.
        assert_eq!(estimated_output_mib(&p, 0.0), Some(0.0));
        // Quality-based profiles are not predictable.
        assert_eq!(
            estimated_output_mib(&ExportProfile::high_quality(), 30.0),
            None
        );
    }

    #[test]
    fn high_quality_uses_av1_nvenc_cq() {
        let p = ExportProfile::high_quality();
        let plan = build_plan("in.mkv", "out.mp4", &p, &src(), None, true).unwrap();
        assert_eq!(plan.encoder, "av1_nvenc");
        assert!(plan.uses_hardware);
        let s = joined(&plan);
        assert!(s.contains("-c:v av1_nvenc"));
        assert!(s.contains("-cq 20"));
        assert!(s.contains("-movflags +faststart"));
        assert!(s.contains("-pix_fmt yuv420p"));
    }

    #[test]
    fn software_fallback_swaps_encoder() {
        let p = ExportProfile::high_quality();
        let plan = build_plan("in.mkv", "out.mp4", &p, &src(), None, false).unwrap();
        assert_eq!(plan.encoder, "libsvtav1");
        assert!(!plan.uses_hardware);
        assert!(joined(&plan).contains("-crf 20"));
    }

    #[test]
    fn source_preset_is_stream_copy() {
        let p = ExportProfile::source(Container::Mkv);
        let plan = build_plan("in.mkv", "out.mkv", &p, &src(), None, true).unwrap();
        assert!(plan.encoder.is_empty());
        let s = joined(&plan);
        assert!(s.contains("-c copy"));
        assert!(!s.contains("av1_nvenc"));
        assert!(!s.contains("faststart")); // mkv
    }

    #[test]
    fn discord_caps_height_and_targets_size() {
        let p = ExportProfile::discord();
        let plan = build_plan("in.mkv", "out.mp4", &p, &src(), None, true).unwrap();
        let s = joined(&plan);
        // 1440p source -> downscaled to 1080.
        assert!(s.contains("-vf scale=-2:1080:flags=lanczos"));
        // Target size -> a concrete, hard-capped CBR bitrate.
        assert!(s.contains("-rc cbr"));
        assert!(s.contains("-b:v "));
        assert!(s.contains("-maxrate "));
        // MP4 -> AAC audio for compatibility.
        assert!(s.contains("-c:a aac"));
    }

    #[test]
    fn no_downscale_when_source_below_cap() {
        let mut s = src();
        s.height = 720;
        s.width = 1280;
        let plan =
            build_plan("in.mkv", "o.mp4", &ExportProfile::discord(), &s, None, true).unwrap();
        assert!(!joined(&plan).contains("scale="));
    }

    #[test]
    fn mkv_copies_opus_audio_on_quality() {
        let mut p = ExportProfile::high_quality();
        p.container = Container::Mkv;
        let plan = build_plan("in.mkv", "o.mkv", &p, &src(), None, true).unwrap();
        assert!(joined(&plan).contains("-c:a copy"));
    }

    #[test]
    fn mkv_transcodes_opus_when_target_size() {
        let mut p = ExportProfile::discord();
        p.container = Container::Mkv;
        let plan = build_plan("in.mkv", "o.mkv", &p, &src(), None, true).unwrap();
        let s = joined(&plan);
        assert!(s.contains("-c:a libopus"));
        assert!(!s.contains("-c:a copy"));
    }

    #[test]
    fn no_audio_source_yields_an() {
        let mut s = src();
        s.has_audio = false;
        s.audio_codec = None;
        let plan = build_plan(
            "in.mkv",
            "o.mp4",
            &ExportProfile::high_quality(),
            &s,
            None,
            true,
        )
        .unwrap();
        assert!(joined(&plan).contains("-an"));
    }

    #[test]
    fn trim_sets_ss_before_input_and_t_after() {
        let t = Trim {
            start_secs: 5.0,
            end_secs: 12.5,
        };
        let plan = build_plan(
            "in.mkv",
            "o.mp4",
            &ExportProfile::high_quality(),
            &src(),
            Some(t),
            true,
        )
        .unwrap();
        let pos_ss = plan.args.iter().position(|a| a == "-ss").unwrap();
        let pos_i = plan.args.iter().position(|a| a == "-i").unwrap();
        let pos_t = plan.args.iter().position(|a| a == "-t").unwrap();
        assert!(pos_ss < pos_i, "-ss must precede -i for fast seek");
        assert!(pos_t > pos_i, "-t must follow -i");
        assert!(plan.args.contains(&"7.500".to_string()));
    }

    #[test]
    fn invalid_trim_is_rejected() {
        let bad = Trim {
            start_secs: 10.0,
            end_secs: 5.0,
        };
        assert!(build_plan(
            "i",
            "o",
            &ExportProfile::high_quality(),
            &src(),
            Some(bad),
            true
        )
        .is_err());
    }

    #[test]
    fn target_size_bitrate_is_under_budget() {
        // 9 MiB over 30s with 128 kbps audio.
        let kbps = target_size_kbps(9.0, 30.0, true, 128);
        // Sanity: resulting total stays roughly within 9 MiB.
        let total_bytes = ((kbps as f64 + 128.0) * 1000.0 * 30.0) / 8.0;
        assert!(
            total_bytes <= 9.0 * 1024.0 * 1024.0,
            "over budget: {total_bytes}"
        );
        assert!(kbps >= 100);
    }

    #[test]
    fn gif_uses_palette_filter() {
        let plan = build_plan(
            "in.mkv",
            "out.gif",
            &ExportProfile::gif(),
            &src(),
            None,
            true,
        )
        .unwrap();
        let s = joined(&plan);
        assert!(s.contains("palettegen") && s.contains("paletteuse"));
        assert!(s.contains("fps=15"));
        assert!(s.contains("-an"));
        assert_eq!(plan.encoder, "gif");
    }

    #[test]
    fn audio_only_drops_video() {
        let p = ExportProfile::audio_only(Container::Mp4);
        let plan = build_plan("in.mkv", "out.m4a", &p, &src(), None, true).unwrap();
        let s = joined(&plan);
        assert!(s.contains("-vn"));
        assert!(s.contains("-c:a aac"));
        assert!(!s.contains("-c:v"));
    }

    #[test]
    fn audio_only_errors_without_audio() {
        let mut s = src();
        s.has_audio = false;
        let p = ExportProfile::audio_only(Container::Mkv);
        assert!(build_plan("i", "o", &p, &s, None, true).is_err());
    }

    #[test]
    fn loudnorm_forces_transcode_and_filter() {
        let mut p = ExportProfile::high_quality();
        p.container = Container::Mkv;
        p.normalize_audio = true;
        let s = joined(&build_plan("in.mkv", "o.mkv", &p, &src(), None, true).unwrap());
        assert!(s.contains("-af loudnorm"));
        assert!(s.contains("-c:a libopus") && !s.contains("-c:a copy"));
    }

    #[test]
    fn target_size_uses_nvenc_multipass() {
        let plan = build_plan(
            "in.mkv",
            "o.mp4",
            &ExportProfile::discord(),
            &src(),
            None,
            true,
        )
        .unwrap();
        assert!(joined(&plan).contains("-multipass fullres"));
    }

    #[test]
    fn x_twitter_is_h264_1080p() {
        let plan = build_plan(
            "in.mkv",
            "o.mp4",
            &ExportProfile::x_twitter(),
            &src(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(plan.encoder, "h264_nvenc");
        assert!(joined(&plan).contains("scale=-2:1080"));
    }

    #[test]
    fn zero_duration_errors() {
        let mut s = src();
        s.duration_secs = 0.0;
        assert!(build_plan("i", "o", &ExportProfile::high_quality(), &s, None, true).is_err());
    }

    #[test]
    fn explicit_bitrate_rate_control() {
        let mut p = ExportProfile::high_quality();
        p.rate_control = RateControl::Bitrate { kbps: 8000 };
        let plan = build_plan("i.mkv", "o.mp4", &p, &src(), None, true).unwrap();
        let s = joined(&plan);
        assert!(s.contains("-rc vbr"));
        assert!(s.contains("-b:v 8000k"));
        assert!(s.contains("-maxrate 12000k"));
    }
}
