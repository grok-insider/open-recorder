//! Capture bitrate policy: recommended / minimum rates for res × fps × codec.
//!
//! Pure helpers shared by the daemon (auto-floor CBR that would mush), the UI
//! (seed the CBR field, show warnings), and the CLI. Tuned against known-good
//! 1440p60 H.264 game clips at ~60 Mbps on an RTX 5070 Ti.

use crate::config::{CaptureCodec, Quality};

/// Quality tier used when recommending a constant bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitrateTier {
    /// Smallest acceptable for the resolution (still playable, soft under motion).
    Minimum,
    /// Balanced / "High" capture quality.
    High,
    /// Near-visually-lossless for local archives.
    Ultra,
}

impl From<Quality> for BitrateTier {
    fn from(q: Quality) -> Self {
        match q {
            Quality::Low | Quality::Medium => BitrateTier::Minimum,
            Quality::High => BitrateTier::High,
            Quality::Ultra => BitrateTier::Ultra,
        }
    }
}

/// Bits-per-pixel targets for game content (high motion, UI text).
///
/// Calibrated so H.264 High @ 1440p60 lands near the known-good ~50–65 Mbps
/// NVENC clips observed on RTX 50-series hardware.
fn bpp(codec: CaptureCodec, tier: BitrateTier) -> f64 {
    match (codec, tier) {
        (CaptureCodec::H264, BitrateTier::Minimum) => 0.10,
        (CaptureCodec::H264, BitrateTier::High) => 0.22,
        (CaptureCodec::H264, BitrateTier::Ultra) => 0.28,
        (CaptureCodec::Hevc, BitrateTier::Minimum) => 0.07,
        (CaptureCodec::Hevc, BitrateTier::High) => 0.14,
        (CaptureCodec::Hevc, BitrateTier::Ultra) => 0.20,
        (CaptureCodec::Av1, BitrateTier::Minimum) => 0.05,
        (CaptureCodec::Av1, BitrateTier::High) => 0.11,
        (CaptureCodec::Av1, BitrateTier::Ultra) => 0.16,
    }
}

/// Absolute floor / ceiling so tiny or absurd resolutions stay sane.
const FLOOR_KBPS: u32 = 2_000;
const CEIL_KBPS: u32 = 200_000;

fn pixels_per_sec(width: u32, height: u32, fps: u32) -> f64 {
    let w = width.max(1) as f64;
    let h = height.max(1) as f64;
    let f = fps.max(1) as f64;
    w * h * f
}

fn kbps_from_bpp(width: u32, height: u32, fps: u32, bpp: f64) -> u32 {
    let raw = pixels_per_sec(width, height, fps) * bpp / 1000.0;
    // Round to nearest 500 kbps so UI values look intentional.
    let rounded = ((raw / 500.0).round() as u32).saturating_mul(500);
    rounded.clamp(FLOOR_KBPS, CEIL_KBPS)
}

/// Recommended constant bitrate (kbps) for the given capture geometry.
pub fn recommended_bitrate_kbps(
    width: u32,
    height: u32,
    fps: u32,
    codec: CaptureCodec,
    tier: BitrateTier,
) -> u32 {
    kbps_from_bpp(width, height, fps, bpp(codec, tier))
}

/// Minimum constant bitrate (kbps) below which clips are expected to look
/// blocky / mushy for game content. Daemon auto-raises CBR below this.
pub fn minimum_bitrate_kbps(width: u32, height: u32, fps: u32, codec: CaptureCodec) -> u32 {
    kbps_from_bpp(width, height, fps, bpp(codec, BitrateTier::Minimum))
}

/// Rough RAM estimate for an encoded replay buffer at a constant bitrate.
pub fn estimate_buffer_mib(bitrate_kbps: u32, buffer_seconds: u32) -> u32 {
    // bits → bytes → MiB; +10% container/audio overhead.
    let bits = (bitrate_kbps as u64)
        .saturating_mul(buffer_seconds as u64)
        .saturating_mul(1000);
    let bytes = bits / 8;
    let with_overhead = bytes.saturating_mul(110) / 100;
    ((with_overhead + (1024 * 1024) - 1) / (1024 * 1024)) as u32
}

/// If the user-requested CBR is below the mush floor for this geometry, return
/// the recommended High-tier rate (and the floor used for the decision).
///
/// `None` means the request is acceptable as-is.
pub fn raise_bitrate_if_too_low(
    requested_kbps: u32,
    width: u32,
    height: u32,
    fps: u32,
    codec: CaptureCodec,
) -> Option<RaiseBitrate> {
    let min = minimum_bitrate_kbps(width, height, fps, codec);
    if requested_kbps >= min {
        return None;
    }
    let recommended = recommended_bitrate_kbps(width, height, fps, codec, BitrateTier::High);
    Some(RaiseBitrate {
        requested_kbps,
        minimum_kbps: min,
        raised_to_kbps: recommended.max(min),
        width,
        height,
        fps,
        codec,
    })
}

/// Result of an automatic CBR raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaiseBitrate {
    pub requested_kbps: u32,
    pub minimum_kbps: u32,
    pub raised_to_kbps: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: CaptureCodec,
}

impl RaiseBitrate {
    /// One-line log / toast message.
    pub fn message(self) -> String {
        format!(
            "capture.bitrate_kbps={} too low for {}x{}@{} {:?} (min={}); using {}",
            self.requested_kbps,
            self.width,
            self.height,
            self.fps,
            self.codec,
            self.minimum_kbps,
            self.raised_to_kbps
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_1440p60_high_near_known_good() {
        // Known-good clips were ~62 Mbps H.264 @ 1440p ~60 fps.
        let kbps = recommended_bitrate_kbps(2560, 1440, 60, CaptureCodec::H264, BitrateTier::High);
        assert!(
            (45_000..=80_000).contains(&kbps),
            "expected ~50–70 Mbps H.264 1440p60, got {kbps}"
        );
    }

    #[test]
    fn av1_1440p120_min_above_broken_12m() {
        let min = minimum_bitrate_kbps(2560, 1440, 120, CaptureCodec::Av1);
        assert!(
            min > 12_000,
            "12 Mbps must be below the AV1 1440p120 floor, got min={min}"
        );
        let rec = recommended_bitrate_kbps(2560, 1440, 120, CaptureCodec::Av1, BitrateTier::High);
        assert!(
            (35_000..=70_000).contains(&rec),
            "expected ~40–55 Mbps AV1 1440p120, got {rec}"
        );
    }

    #[test]
    fn raise_12m_at_1440p120_av1() {
        let r = raise_bitrate_if_too_low(12_000, 2560, 1440, 120, CaptureCodec::Av1)
            .expect("12M must be raised");
        assert_eq!(r.requested_kbps, 12_000);
        assert!(r.raised_to_kbps >= r.minimum_kbps);
        assert!(r.raised_to_kbps > 12_000);
        assert!(r.message().contains("12000"));
    }

    #[test]
    fn high_enough_cbr_not_raised() {
        assert!(
            raise_bitrate_if_too_low(80_000, 2560, 1440, 60, CaptureCodec::H264).is_none()
        );
    }

    #[test]
    fn hevc_more_efficient_than_h264() {
        let h = recommended_bitrate_kbps(1920, 1080, 60, CaptureCodec::H264, BitrateTier::High);
        let e = recommended_bitrate_kbps(1920, 1080, 60, CaptureCodec::Hevc, BitrateTier::High);
        assert!(e < h, "HEVC {e} should be below H.264 {h}");
    }

    #[test]
    fn buffer_mib_scales() {
        // 50 Mbps × 60 s ≈ 375 MB raw → ~412 MiB with 10% overhead
        let mib = estimate_buffer_mib(50_000, 60);
        assert!((350..=500).contains(&mib), "got {mib}");
    }

    #[test]
    fn quality_tier_from_enum() {
        assert_eq!(BitrateTier::from(Quality::Ultra), BitrateTier::Ultra);
        assert_eq!(BitrateTier::from(Quality::High), BitrateTier::High);
        assert_eq!(BitrateTier::from(Quality::Low), BitrateTier::Minimum);
    }
}
