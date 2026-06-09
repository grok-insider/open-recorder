//! `ord export` — local clip transcode (does not touch the daemon).
//!
//! Wraps [`ord_export`]: parse flags into an [`ExportProfile`] (+ optional trim),
//! then run the encode. The argument parsing is split into a pure
//! [`parse_export`] so it can be unit-tested without ffmpeg.

use std::path::{Path, PathBuf};

use ord_common::config::{Container, ExportCodec};
use ord_export::profile::{ExportProfile, FrameRate, Preset, RateControl, Scale};
use ord_export::{export, Trim};

pub fn usage() -> &'static str {
    "ord export — transcode/trim a clip\n\
     \n\
     usage:\n  \
       ord export <input> [output] [options]\n\
     \n\
     options:\n  \
       --preset high|discord|source   start from a preset (default: high)\n  \
       --codec av1|hevc|h264          video codec (default from preset)\n  \
       --container mp4|mkv            output container\n  \
       --cq N                         constant quality (lower = better)\n  \
       --bitrate N[k|M]               average video bitrate (VBR)\n  \
       --target-size N                aim for ~N MiB (hard cap)\n  \
       --max-height N                 downscale to at most N px tall\n  \
       --scale WxH                    force exact resolution\n  \
       --fps N                        force frame rate\n  \
       --start S --end S              trim window in seconds (both or neither)\n  \
       --no-hardware                  force software encoding\n"
}

/// The fully-resolved inputs to an export, produced by [`parse_export`].
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedExport {
    pub input: PathBuf,
    pub output: PathBuf,
    pub profile: ExportProfile,
    pub trim: Option<Trim>,
}

fn parse_codec(s: &str) -> Result<ExportCodec, String> {
    match s.to_ascii_lowercase().as_str() {
        "av1" => Ok(ExportCodec::Av1),
        "hevc" | "h265" => Ok(ExportCodec::Hevc),
        "h264" | "avc" => Ok(ExportCodec::H264),
        other => Err(format!("unknown codec: {other}")),
    }
}

fn parse_container(s: &str) -> Result<Container, String> {
    match s.to_ascii_lowercase().as_str() {
        "mp4" => Ok(Container::Mp4),
        "mkv" | "matroska" => Ok(Container::Mkv),
        other => Err(format!("unknown container: {other}")),
    }
}

/// Parse a bitrate like `8000`, `8000k`, or `8M` into kbps.
fn parse_bitrate_kbps(s: &str) -> Result<u32, String> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let (num, mult) = if let Some(n) = lower.strip_suffix('m') {
        (n, 1000.0)
    } else if let Some(n) = lower.strip_suffix('k') {
        (n, 1.0)
    } else {
        (lower.as_str(), 1.0)
    };
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("bad bitrate: {s}"))?;
    let kbps = (v * mult).round() as i64;
    if kbps <= 0 {
        return Err(format!("bitrate must be positive: {s}"));
    }
    Ok(kbps as u32)
}

fn parse_scale(s: &str) -> Result<Scale, String> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("--scale must be WxH, got {s}"))?;
    let width: u32 = w.trim().parse().map_err(|_| "bad scale width")?;
    let height: u32 = h.trim().parse().map_err(|_| "bad scale height")?;
    if width == 0 || height == 0 {
        return Err("--scale dimensions must be > 0".into());
    }
    Ok(Scale::Exact { width, height })
}

/// Derive a default output path: `<stem>-export.<ext>` next to the input. The
/// extension follows the profile's output kind (gif/audio-only override the
/// container).
fn default_output(input: &Path, profile: &ExportProfile) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "clip".to_string());
    let file = format!("{stem}-export.{}", profile.output_extension());
    match input.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(file),
        _ => PathBuf::from(file),
    }
}

fn need(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_f64(s: &str, flag: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("{flag} must be a number"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!("{flag} must be a non-negative number"));
    }
    Ok(v)
}

/// Parse `ord export` arguments (everything after the `export` subcommand).
pub fn parse_export(args: impl Iterator<Item = String>) -> Result<ParsedExport, String> {
    let mut input: Option<String> = None;
    let mut output: Option<String> = None;
    let mut preset: Option<Preset> = None;
    let mut codec: Option<ExportCodec> = None;
    let mut container: Option<Container> = None;
    let mut rate: Option<RateControl> = None;
    let mut scale: Option<Scale> = None;
    let mut fps: Option<FrameRate> = None;
    let mut hardware: Option<bool> = None;
    let mut normalize: Option<bool> = None;
    let mut start: Option<f64> = None;
    let mut end: Option<f64> = None;

    let mut it = args;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--preset" => {
                let v = need(&mut it, "--preset")?;
                preset = Some(Preset::parse(&v).ok_or_else(|| format!("unknown preset: {v}"))?);
            }
            "--codec" => codec = Some(parse_codec(&need(&mut it, "--codec")?)?),
            "--container" => container = Some(parse_container(&need(&mut it, "--container")?)?),
            "--cq" => {
                let v = need(&mut it, "--cq")?;
                let q: u8 = v.parse().map_err(|_| "--cq must be 0-255")?;
                rate = Some(RateControl::Quality(q));
            }
            "--bitrate" => {
                rate = Some(RateControl::Bitrate {
                    kbps: parse_bitrate_kbps(&need(&mut it, "--bitrate")?)?,
                });
            }
            "--target-size" => {
                let v = need(&mut it, "--target-size")?;
                let mib: f64 = v.parse().map_err(|_| "--target-size must be a number")?;
                if mib <= 0.0 {
                    return Err("--target-size must be > 0".into());
                }
                rate = Some(RateControl::TargetSize { mib });
            }
            "--max-height" => {
                let v = need(&mut it, "--max-height")?;
                let h: u32 = v.parse().map_err(|_| "--max-height must be a number")?;
                scale = Some(Scale::MaxHeight(h));
            }
            "--scale" => scale = Some(parse_scale(&need(&mut it, "--scale")?)?),
            "--fps" => {
                let v = need(&mut it, "--fps")?;
                let f: u32 = v.parse().map_err(|_| "--fps must be a number")?;
                fps = Some(FrameRate::Fixed(f));
            }
            "--start" => start = Some(parse_f64(&need(&mut it, "--start")?, "--start")?),
            "--end" => end = Some(parse_f64(&need(&mut it, "--end")?, "--end")?),
            "--no-hardware" => hardware = Some(false),
            "--hardware" => hardware = Some(true),
            "--normalize" => normalize = Some(true),
            "-h" | "--help" => return Err(usage().to_string()),
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            positional => {
                if input.is_none() {
                    input = Some(positional.to_string());
                } else if output.is_none() {
                    output = Some(positional.to_string());
                } else {
                    return Err(format!("unexpected argument: {positional}"));
                }
            }
        }
    }

    let input = input.ok_or("export needs an input file")?;
    let input = PathBuf::from(input);

    let mut profile = preset.map(Preset::profile).unwrap_or_default();
    if let Some(c) = codec {
        profile.codec = c;
    }
    if let Some(c) = container {
        profile.container = c;
    }
    if let Some(r) = rate {
        profile.rate_control = r;
    }
    if let Some(s) = scale {
        profile.scale = s;
    }
    if let Some(f) = fps {
        profile.fps = f;
    }
    if let Some(h) = hardware {
        profile.hardware = h;
    }
    if let Some(n) = normalize {
        profile.normalize_audio = n;
    }

    let output = output
        .map(PathBuf::from)
        .unwrap_or_else(|| default_output(&input, &profile));

    let trim = match (start, end) {
        (None, None) => None,
        (Some(s), Some(e)) => Some(Trim {
            start_secs: s,
            end_secs: e,
        }),
        _ => return Err("--start and --end must be given together".into()),
    };

    Ok(ParsedExport {
        input,
        output,
        profile,
        trim,
    })
}

/// Parse args, run the export, and print a summary.
pub fn run_export(args: impl Iterator<Item = String>) -> Result<(), String> {
    let p = parse_export(args)?;
    let summary = export(&p.input, &p.output, &p.profile, p.trim)
        .map_err(|e| format!("export failed: {e}"))?;

    let mib = summary.size_bytes as f64 / (1024.0 * 1024.0);
    let how = if summary.encoder.is_empty() {
        "stream copy".to_string()
    } else {
        format!(
            "{} ({})",
            summary.encoder,
            if summary.used_hardware {
                "hardware"
            } else {
                "software"
            }
        )
    };
    println!(
        "exported {} -> {} | {:.1} MiB, {:.1}s, {}",
        p.input.display(),
        summary.output.display(),
        mib,
        summary.duration_secs,
        how
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<ParsedExport, String> {
        parse_export(s.split_whitespace().map(String::from))
    }

    #[test]
    fn input_required() {
        assert!(parse("").is_err());
    }

    #[test]
    fn defaults_to_high_quality_and_derived_output() {
        let p = parse("clip.mkv").unwrap();
        assert_eq!(p.profile, ExportProfile::high_quality());
        assert_eq!(p.output, PathBuf::from("clip-export.mp4"));
        assert!(p.trim.is_none());
    }

    #[test]
    fn derived_output_keeps_input_dir_and_container_ext() {
        let p = parse("/v/game.mkv --container mkv").unwrap();
        assert_eq!(p.output, PathBuf::from("/v/game-export.mkv"));
    }

    #[test]
    fn explicit_output_positional() {
        let p = parse("in.mkv out.mp4").unwrap();
        assert_eq!(p.output, PathBuf::from("out.mp4"));
    }

    #[test]
    fn preset_then_overrides() {
        let p = parse("in.mkv --preset discord --codec hevc --max-height 720").unwrap();
        assert_eq!(p.profile.codec, ExportCodec::Hevc);
        assert_eq!(p.profile.scale, Scale::MaxHeight(720));
        // Discord's target-size rate control is retained (not overridden here).
        assert!(matches!(
            p.profile.rate_control,
            RateControl::TargetSize { .. }
        ));
    }

    #[test]
    fn bitrate_units() {
        assert_eq!(parse_bitrate_kbps("8000").unwrap(), 8000);
        assert_eq!(parse_bitrate_kbps("8000k").unwrap(), 8000);
        assert_eq!(parse_bitrate_kbps("8M").unwrap(), 8000);
        assert!(parse_bitrate_kbps("-5").is_err());
        assert!(parse_bitrate_kbps("abc").is_err());
    }

    #[test]
    fn rate_control_flags() {
        assert!(matches!(
            parse("i.mkv --cq 24").unwrap().profile.rate_control,
            RateControl::Quality(24)
        ));
        assert!(matches!(
            parse("i.mkv --bitrate 12M").unwrap().profile.rate_control,
            RateControl::Bitrate { kbps: 12000 }
        ));
        assert!(matches!(
            parse("i.mkv --target-size 8").unwrap().profile.rate_control,
            RateControl::TargetSize { .. }
        ));
    }

    #[test]
    fn scale_exact_and_fps() {
        let p = parse("i.mkv --scale 1920x1080 --fps 30").unwrap();
        assert_eq!(
            p.profile.scale,
            Scale::Exact {
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(p.profile.fps, FrameRate::Fixed(30));
    }

    #[test]
    fn trim_requires_both_ends() {
        assert!(parse("i.mkv --start 5").is_err());
        assert!(parse("i.mkv --end 10").is_err());
        let p = parse("i.mkv --start 5 --end 10").unwrap();
        assert_eq!(
            p.trim,
            Some(Trim {
                start_secs: 5.0,
                end_secs: 10.0
            })
        );
    }

    #[test]
    fn no_hardware_flag() {
        assert!(!parse("i.mkv --no-hardware").unwrap().profile.hardware);
    }

    #[test]
    fn unknown_flag_errors() {
        assert!(parse("i.mkv --bogus").is_err());
    }

    #[test]
    fn too_many_positionals_error() {
        assert!(parse("a b c").is_err());
    }
}
