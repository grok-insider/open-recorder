//! Annex-B (start-code) <-> AVCC (length-prefixed) helpers for H.264.
//!
//! NVENC emits Annex-B; mp4/mkv want AVCC + an `avcC` extradata box. These
//! helpers are pure byte manipulation and unit-tested without ffmpeg.

/// Iterate the NAL units in an Annex-B buffer (payloads only, start codes
/// stripped). Handles both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) codes.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let starts = start_code_positions(data);
    for (i, &(pos, sc_len)) in starts.iter().enumerate() {
        let payload_start = pos + sc_len;
        let payload_end = if i + 1 < starts.len() {
            starts[i + 1].0
        } else {
            data.len()
        };
        if payload_start < payload_end {
            units.push(&data[payload_start..payload_end]);
        }
    }
    units
}

/// Positions of start codes as (offset, start_code_length).
fn start_code_positions(data: &[u8]) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            positions.push((i, 3));
            i += 3;
        } else if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            positions.push((i, 4));
            i += 4;
        } else {
            i += 1;
        }
    }
    positions
}

/// The NAL unit type (lower 5 bits of the first byte) for H.264.
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1f).unwrap_or(0)
}

pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// Convert an Annex-B access unit to AVCC: each NAL prefixed by its 4-byte
/// big-endian length. SPS/PPS NALs are dropped (they live in extradata).
pub fn to_avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for nal in nal_units(data) {
        let t = nal_type(nal);
        if t == NAL_SPS || t == NAL_PPS {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Build an `avcC` extradata box from the SPS and PPS found in `keyframe`.
/// Returns `None` if either is missing.
pub fn build_avcc(keyframe: &[u8]) -> Option<Vec<u8>> {
    let mut sps: Option<&[u8]> = None;
    let mut pps: Option<&[u8]> = None;
    for nal in nal_units(keyframe) {
        match nal_type(nal) {
            NAL_SPS if sps.is_none() => sps = Some(nal),
            NAL_PPS if pps.is_none() => pps = Some(nal),
            _ => {}
        }
    }
    let sps = sps?;
    let pps = pps?;
    if sps.len() < 4 {
        return None;
    }

    let mut avcc = vec![
        1,      // configurationVersion
        sps[1], // AVCProfileIndication
        sps[2], // profile_compatibility
        sps[3], // AVCLevelIndication
        0xff,   // 6 bits reserved + lengthSizeMinusOne (3 => 4-byte NAL length)
        0xe1,   // 3 bits reserved + numOfSPS (1)
    ];
    avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(sps);
    avcc.push(1); // numOfPPS
    avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(pps);
    Some(avcc)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal synthetic access unit: SPS(7) + PPS(8) + IDR(5), 4-byte codes.
    fn sample_keyframe() -> Vec<u8> {
        let mut d = Vec::new();
        let sps = [0x67u8, 0x4d, 0x40, 0x32, 0xaa];
        let pps = [0x68u8, 0xee, 0x3c, 0x80];
        let idr = [0x65u8, 0x88, 0x84, 0x00, 0x11, 0x22];
        for nal in [&sps[..], &pps[..], &idr[..]] {
            d.extend_from_slice(&[0, 0, 0, 1]);
            d.extend_from_slice(nal);
        }
        d
    }

    #[test]
    fn splits_nal_units() {
        let kf = sample_keyframe();
        let nals = nal_units(&kf);
        assert_eq!(nals.len(), 3);
        assert_eq!(nal_type(nals[0]), NAL_SPS);
        assert_eq!(nal_type(nals[1]), NAL_PPS);
        assert_eq!(nal_type(nals[2]), 5);
    }

    #[test]
    fn handles_3byte_start_codes() {
        let mut d = Vec::new();
        d.extend_from_slice(&[0, 0, 1, 0x67, 0xaa]);
        d.extend_from_slice(&[0, 0, 1, 0x65, 0xbb]);
        let nals = nal_units(&d);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x67, 0xaa]);
    }

    #[test]
    fn to_avcc_drops_sps_pps_and_length_prefixes() {
        let avcc = to_avcc(&sample_keyframe());
        // Only the IDR NAL remains: 4-byte length + 6 bytes payload.
        assert_eq!(avcc.len(), 4 + 6);
        let len = u32::from_be_bytes([avcc[0], avcc[1], avcc[2], avcc[3]]);
        assert_eq!(len, 6);
        assert_eq!(nal_type(&avcc[4..]), 5);
    }

    #[test]
    fn build_avcc_from_keyframe() {
        let avcc = build_avcc(&sample_keyframe()).unwrap();
        assert_eq!(avcc[0], 1); // version
        assert_eq!(avcc[1], 0x4d); // profile from SPS[1]
        assert_eq!(avcc[4], 0xff);
        assert_eq!(avcc[5], 0xe1);
    }

    #[test]
    fn build_avcc_missing_pps_is_none() {
        let mut d = Vec::new();
        d.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x4d, 0x40, 0x32, 0xaa]); // SPS only
        assert!(build_avcc(&d).is_none());
    }

    #[test]
    fn empty_input_yields_no_nals() {
        assert!(nal_units(&[]).is_empty());
        assert!(build_avcc(&[]).is_none());
    }
}
