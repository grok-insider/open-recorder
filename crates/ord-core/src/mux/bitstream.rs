//! Per-codec bitstream logic for muxing, keyed by [`Codec`].
//!
//! This is the **single** place that knows how an encoder's raw bitstream maps
//! onto container packets and codec-private extradata. Both the clip muxer
//! (`mux.rs`) and the streaming recorder (`record.rs`) consume it — never
//! duplicate this logic, and never branch on `is_h264`-style booleans.
//!
//! | Codec | Wire format from encoder | Packet transform | Extradata |
//! |-------|--------------------------|------------------|-----------|
//! | H.264 | Annex-B (start codes, in-band SPS/PPS) | 4-byte length prefixes, SPS/PPS stripped | `avcC` |
//! | HEVC  | Annex-B (start codes, in-band VPS/SPS/PPS) | 4-byte length prefixes, VPS/SPS/PPS stripped | `hvcC` |
//! | AV1   | Low-overhead OBU stream (no start codes) | passthrough (refcount bump) | `av1C` |
//!
//! Everything here is pure byte/bit manipulation, unit-tested without ffmpeg.

use bytes::Bytes;

use crate::backend::Codec;

/// Errors building codec-private extradata from a keyframe.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BitstreamError {
    #[error("{0} keyframe is missing required parameter sets")]
    MissingParameterSets(&'static str),
    #[error("malformed {0} bitstream: {1}")]
    Malformed(&'static str, &'static str),
}

/// Build the container codec-private blob (`avcC`/`hvcC`/`av1C`) from the first
/// keyframe of the stream.
pub fn extradata(codec: Codec, keyframe: &[u8]) -> Result<Vec<u8>, BitstreamError> {
    match codec {
        Codec::H264 => h264::build_avcc(keyframe)
            .ok_or(BitstreamError::MissingParameterSets("H.264 (SPS/PPS)")),
        Codec::Hevc => hevc::build_hvcc(keyframe),
        Codec::Av1 => av1::build_av1c(keyframe),
    }
}

/// Transform one encoded access unit into the container's packet payload.
///
/// H.264/HEVC allocate a fresh `Vec` (moved into `Bytes` O(1)); AV1 is a
/// refcount bump on the buffered frame, never a copy.
pub fn packet_payload(codec: Codec, data: &Bytes) -> Bytes {
    match codec {
        Codec::H264 => h264::to_length_prefixed(data).into(),
        Codec::Hevc => hevc::to_length_prefixed(data).into(),
        Codec::Av1 => data.clone(),
    }
}

/// Iterate the NAL units in an Annex-B buffer (payloads only, start codes
/// stripped). Handles both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) codes.
/// Shared by the H.264 and HEVC paths.
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

/// Length-prefix every NAL unit (4-byte big-endian), dropping the parameter-set
/// NALs identified by `is_parameter_set` (they live in extradata instead).
fn length_prefix(data: &[u8], is_parameter_set: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for nal in nal_units(data) {
        if is_parameter_set(nal) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// MSB-first bit reader over a byte slice, with Exp-Golomb support. Returns
/// `None` past the end so malformed input degrades to an error, never a panic.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    fn read_bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()?;
        }
        Some(v)
    }

    fn skip(&mut self, n: u32) -> Option<()> {
        self.pos = self.pos.checked_add(n as usize)?;
        (self.pos <= self.data.len() * 8).then_some(())
    }

    /// Unsigned Exp-Golomb (`ue(v)`).
    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.read_bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.read_bits(zeros)?;
        Some((1u32 << zeros) - 1 + rest)
    }
}

pub mod h264 {
    //! H.264: Annex-B handling and the `avcC` decoder configuration record.

    /// The NAL unit type (lower 5 bits of the first byte).
    pub fn nal_type(nal: &[u8]) -> u8 {
        nal.first().map(|b| b & 0x1f).unwrap_or(0)
    }

    pub const NAL_SPS: u8 = 7;
    pub const NAL_PPS: u8 = 8;

    /// Convert an Annex-B access unit to AVCC: each NAL prefixed by its 4-byte
    /// big-endian length. SPS/PPS NALs are dropped (they live in extradata).
    pub fn to_length_prefixed(data: &[u8]) -> Vec<u8> {
        super::length_prefix(data, |nal| matches!(nal_type(nal), NAL_SPS | NAL_PPS))
    }

    /// Build an `avcC` extradata box from the SPS and PPS found in `keyframe`.
    /// Returns `None` if either is missing.
    pub fn build_avcc(keyframe: &[u8]) -> Option<Vec<u8>> {
        let mut sps: Option<&[u8]> = None;
        let mut pps: Option<&[u8]> = None;
        for nal in super::nal_units(keyframe) {
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
}

pub mod hevc {
    //! HEVC: Annex-B handling and the `hvcC` decoder configuration record
    //! (ISO/IEC 14496-15 §8.3.3.1).

    use super::{BitReader, BitstreamError};

    /// The NAL unit type: bits 1–6 of the first header byte (H.265 §7.3.1.2).
    pub fn nal_type(nal: &[u8]) -> u8 {
        nal.first().map(|b| (b >> 1) & 0x3f).unwrap_or(0)
    }

    pub const NAL_VPS: u8 = 32;
    pub const NAL_SPS: u8 = 33;
    pub const NAL_PPS: u8 = 34;

    /// Convert an Annex-B access unit to length-prefixed form, dropping
    /// VPS/SPS/PPS (they live in the `hvcC` extradata).
    pub fn to_length_prefixed(data: &[u8]) -> Vec<u8> {
        super::length_prefix(data, |nal| {
            matches!(nal_type(nal), NAL_VPS | NAL_SPS | NAL_PPS)
        })
    }

    /// Strip emulation-prevention bytes (`00 00 03` → `00 00`) so fixed offsets
    /// into the RBSP are valid.
    fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(nal.len());
        let mut zeros = 0u32;
        for &b in nal {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        out
    }

    /// Fields parsed from the SPS that `hvcC` needs.
    struct SpsInfo {
        max_sub_layers_minus1: u8,
        temporal_id_nesting: u8,
        /// The 12-byte general profile_tier_level block.
        ptl: [u8; 12],
        chroma_format_idc: u8,
        bit_depth_luma_minus8: u8,
        bit_depth_chroma_minus8: u8,
    }

    /// Parse the leading fields of an (escaped) SPS NAL. Only reads up to the
    /// bit depths — everything `hvcC` mirrors.
    fn parse_sps(sps_nal: &[u8]) -> Result<SpsInfo, BitstreamError> {
        let err = |what: &'static str| BitstreamError::Malformed("HEVC", what);
        let rbsp = unescape_rbsp(sps_nal);
        // 2-byte NAL header, then sps_video_parameter_set_id(4) +
        // sps_max_sub_layers_minus1(3) + sps_temporal_id_nesting_flag(1).
        if rbsp.len() < 15 {
            return Err(err("SPS too short"));
        }
        let max_sub_layers_minus1 = (rbsp[2] >> 1) & 0x7;
        let temporal_id_nesting = rbsp[2] & 1;
        let mut ptl = [0u8; 12];
        ptl.copy_from_slice(&rbsp[3..15]);

        let mut r = BitReader::new(&rbsp);
        r.skip(3 * 8 + 12 * 8).ok_or(err("SPS too short"))?;
        // profile_tier_level sub-layer part (only when sub-layers exist).
        if max_sub_layers_minus1 > 0 {
            let mut profile_present = [false; 8];
            let mut level_present = [false; 8];
            for i in 0..max_sub_layers_minus1 as usize {
                profile_present[i] = r.read_bit().ok_or(err("PTL truncated"))? == 1;
                level_present[i] = r.read_bit().ok_or(err("PTL truncated"))? == 1;
            }
            r.skip(2 * (8 - max_sub_layers_minus1 as u32))
                .ok_or(err("PTL truncated"))?;
            for i in 0..max_sub_layers_minus1 as usize {
                if profile_present[i] {
                    r.skip(88).ok_or(err("PTL truncated"))?;
                }
                if level_present[i] {
                    r.skip(8).ok_or(err("PTL truncated"))?;
                }
            }
        }
        let _sps_id = r.read_ue().ok_or(err("missing sps id"))?;
        let chroma_format_idc = r.read_ue().ok_or(err("missing chroma_format_idc"))? as u8;
        if chroma_format_idc == 3 {
            r.read_bit().ok_or(err("missing separate_colour_plane"))?;
        }
        let _width = r.read_ue().ok_or(err("missing width"))?;
        let _height = r.read_ue().ok_or(err("missing height"))?;
        if r.read_bit().ok_or(err("missing conformance flag"))? == 1 {
            for _ in 0..4 {
                r.read_ue().ok_or(err("conformance window truncated"))?;
            }
        }
        let bit_depth_luma_minus8 = r.read_ue().ok_or(err("missing bit depth"))? as u8;
        let bit_depth_chroma_minus8 = r.read_ue().ok_or(err("missing bit depth"))? as u8;

        Ok(SpsInfo {
            max_sub_layers_minus1,
            temporal_id_nesting,
            ptl,
            chroma_format_idc,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
        })
    }

    /// Build an `hvcC` extradata box from the VPS/SPS/PPS in `keyframe`.
    pub fn build_hvcc(keyframe: &[u8]) -> Result<Vec<u8>, BitstreamError> {
        let mut vps: Vec<&[u8]> = Vec::new();
        let mut sps: Vec<&[u8]> = Vec::new();
        let mut pps: Vec<&[u8]> = Vec::new();
        for nal in super::nal_units(keyframe) {
            match nal_type(nal) {
                NAL_VPS => vps.push(nal),
                NAL_SPS => sps.push(nal),
                NAL_PPS => pps.push(nal),
                _ => {}
            }
        }
        if sps.is_empty() || pps.is_empty() {
            return Err(BitstreamError::MissingParameterSets("HEVC (SPS/PPS)"));
        }
        let info = parse_sps(sps[0])?;

        let mut h = Vec::with_capacity(64);
        h.push(1); // configurationVersion
        h.extend_from_slice(&info.ptl); // general profile/compat/constraints/level
        h.extend_from_slice(&0xf000u16.to_be_bytes()); // reserved + min_spatial_segmentation_idc(0)
        h.push(0xfc); // reserved + parallelismType(0 = unknown)
        h.push(0xfc | (info.chroma_format_idc & 0x3));
        h.push(0xf8 | (info.bit_depth_luma_minus8 & 0x7));
        h.push(0xf8 | (info.bit_depth_chroma_minus8 & 0x7));
        h.extend_from_slice(&0u16.to_be_bytes()); // avgFrameRate (unspecified)
                                                  // constantFrameRate(0) | numTemporalLayers | temporalIdNested | lengthSizeMinusOne(3)
        h.push(((info.max_sub_layers_minus1 + 1) << 3) | (info.temporal_id_nesting << 2) | 3);

        let arrays: Vec<(u8, &Vec<&[u8]>)> = [(NAL_VPS, &vps), (NAL_SPS, &sps), (NAL_PPS, &pps)]
            .into_iter()
            .filter(|(_, nals)| !nals.is_empty())
            .collect();
        h.push(arrays.len() as u8);
        for (ty, nals) in arrays {
            h.push(0x80 | ty); // array_completeness(1) + reserved(0) + NAL_unit_type
            h.extend_from_slice(&(nals.len() as u16).to_be_bytes());
            for nal in nals {
                h.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                h.extend_from_slice(nal);
            }
        }
        Ok(h)
    }
}

pub mod av1 {
    //! AV1: OBU stream handling and the `av1C` configuration record
    //! (AV1-ISOBMFF §2.3). AV1 has no start codes; packets pass through
    //! untouched and only the extradata needs building.

    use super::{BitReader, BitstreamError};

    const OBU_SEQUENCE_HEADER: u8 = 1;

    /// One OBU: (type, full bytes including header).
    fn obus(data: &[u8]) -> Vec<(u8, &[u8])> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let start = i;
            let header = data[i];
            let obu_type = (header >> 3) & 0xf;
            let has_extension = header & 0x4 != 0;
            let has_size = header & 0x2 != 0;
            i += 1;
            if has_extension {
                i += 1;
            }
            if !has_size {
                // Size-less OBUs are only legal as the last OBU of a frame.
                out.push((obu_type, &data[start..]));
                break;
            }
            // leb128 payload size.
            let mut size = 0u64;
            let mut shift = 0u32;
            loop {
                let Some(&b) = data.get(i) else { return out };
                i += 1;
                size |= ((b & 0x7f) as u64) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
                if shift > 56 {
                    return out;
                }
            }
            let end = (i + size as usize).min(data.len());
            out.push((obu_type, &data[start..end]));
            i = end;
        }
        out
    }

    /// Payload offset within a full OBU (header + optional extension + leb128).
    fn payload_offset(obu: &[u8]) -> usize {
        let mut i = 1;
        if obu[0] & 0x4 != 0 {
            i += 1;
        }
        if obu[0] & 0x2 != 0 {
            while let Some(&b) = obu.get(i) {
                i += 1;
                if b & 0x80 == 0 {
                    break;
                }
            }
        }
        i
    }

    struct SeqInfo {
        profile: u8,
        level: u8,
        tier: u8,
    }

    /// Parse seq_profile / seq_level_idx_0 / seq_tier_0 from a sequence header
    /// OBU payload (AV1 §5.5.1).
    fn parse_sequence_header(payload: &[u8]) -> Result<SeqInfo, BitstreamError> {
        let err = |what: &'static str| BitstreamError::Malformed("AV1", what);
        let mut r = BitReader::new(payload);
        let profile = r.read_bits(3).ok_or(err("truncated seq header"))? as u8;
        let _still_picture = r.read_bit().ok_or(err("truncated seq header"))?;
        let reduced = r.read_bit().ok_or(err("truncated seq header"))? == 1;
        if reduced {
            let level = r.read_bits(5).ok_or(err("truncated seq header"))? as u8;
            return Ok(SeqInfo {
                profile,
                level,
                tier: 0,
            });
        }
        let mut decoder_model_info = false;
        let mut buffer_delay_length = 0u32;
        if r.read_bit().ok_or(err("truncated timing info"))? == 1 {
            // timing_info(): num_units_in_display_tick(32) + time_scale(32) +
            // equal_picture_interval(1) [+ uvlc num_ticks_per_picture_minus_1].
            r.skip(64).ok_or(err("truncated timing info"))?;
            if r.read_bit().ok_or(err("truncated timing info"))? == 1 {
                // uvlc: leading zeros, then a 1, then that many value bits.
                let mut zeros = 0u32;
                while r.read_bit().ok_or(err("truncated uvlc"))? == 0 {
                    zeros += 1;
                    if zeros >= 32 {
                        break;
                    }
                }
                if zeros < 32 {
                    r.skip(zeros).ok_or(err("truncated uvlc"))?;
                }
            }
            decoder_model_info = r.read_bit().ok_or(err("truncated decoder model"))? == 1;
            if decoder_model_info {
                buffer_delay_length = r.read_bits(5).ok_or(err("truncated decoder model"))? + 1;
                r.skip(32 + 5 + 5).ok_or(err("truncated decoder model"))?;
            }
        }
        let initial_display_delay = r.read_bit().ok_or(err("truncated seq header"))? == 1;
        let _op_cnt_minus1 = r.read_bits(5).ok_or(err("truncated operating points"))?;
        // Only operating point 0 matters for av1C.
        let _op_idc = r.read_bits(12).ok_or(err("truncated operating points"))?;
        let level = r.read_bits(5).ok_or(err("truncated operating points"))? as u8;
        let tier = if level > 7 {
            r.read_bits(1).ok_or(err("truncated operating points"))? as u8
        } else {
            0
        };
        // Remaining op-0 fields exist but av1C doesn't need them; parsing stops
        // here, which is safe because we never read past this point.
        let _ = (
            decoder_model_info,
            buffer_delay_length,
            initial_display_delay,
        );
        Ok(SeqInfo {
            profile,
            level,
            tier,
        })
    }

    /// Build an `av1C` configuration record: the parsed profile/level/tier plus
    /// the sequence header OBU as configOBUs.
    ///
    /// The chroma/bit-depth flags are fixed to 8-bit 4:2:0 (`sub_x=sub_y=1`),
    /// which is what NVENC AV1 (Main profile) emits; revisit if a 10-bit/HDR
    /// capture path is added.
    pub fn build_av1c(keyframe: &[u8]) -> Result<Vec<u8>, BitstreamError> {
        let seq = obus(keyframe)
            .into_iter()
            .find(|(ty, _)| *ty == OBU_SEQUENCE_HEADER)
            .map(|(_, bytes)| bytes)
            .ok_or(BitstreamError::MissingParameterSets(
                "AV1 (sequence header)",
            ))?;
        let info = parse_sequence_header(&seq[payload_offset(seq)..])?;

        let mut c = Vec::with_capacity(4 + seq.len());
        c.push(0x81); // marker(1) + version(7)
        c.push((info.profile << 5) | (info.level & 0x1f));
        c.push((info.tier << 7) | 0x0c); // 8-bit, not mono, 4:2:0 (sub_x=1, sub_y=1)
        c.push(0); // no initial_presentation_delay
        c.extend_from_slice(seq);
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal synthetic H.264 access unit: SPS(7) + PPS(8) + IDR(5).
    fn h264_keyframe() -> Vec<u8> {
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
        let kf = h264_keyframe();
        let nals = nal_units(&kf);
        assert_eq!(nals.len(), 3);
        assert_eq!(h264::nal_type(nals[0]), h264::NAL_SPS);
        assert_eq!(h264::nal_type(nals[1]), h264::NAL_PPS);
        assert_eq!(h264::nal_type(nals[2]), 5);
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
    fn h264_payload_drops_sps_pps_and_length_prefixes() {
        let avcc = packet_payload(Codec::H264, &h264_keyframe().into());
        // Only the IDR NAL remains: 4-byte length + 6 bytes payload.
        assert_eq!(avcc.len(), 4 + 6);
        let len = u32::from_be_bytes([avcc[0], avcc[1], avcc[2], avcc[3]]);
        assert_eq!(len, 6);
        assert_eq!(h264::nal_type(&avcc[4..]), 5);
    }

    #[test]
    fn h264_extradata_is_avcc() {
        let avcc = extradata(Codec::H264, &h264_keyframe()).unwrap();
        assert_eq!(avcc[0], 1); // version
        assert_eq!(avcc[1], 0x4d); // profile from SPS[1]
        assert_eq!(avcc[4], 0xff);
        assert_eq!(avcc[5], 0xe1);
    }

    #[test]
    fn h264_missing_pps_is_error() {
        let mut d = Vec::new();
        d.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x4d, 0x40, 0x32, 0xaa]); // SPS only
        assert!(matches!(
            extradata(Codec::H264, &d),
            Err(BitstreamError::MissingParameterSets(_))
        ));
    }

    #[test]
    fn empty_input_yields_no_nals() {
        assert!(nal_units(&[]).is_empty());
        assert!(extradata(Codec::H264, &[]).is_err());
    }

    // --- HEVC ---

    /// Synthetic HEVC SPS: NAL header (type 33) + sps fields for a 0-sub-layer,
    /// 4:2:0, 8-bit stream. Bit-packed by hand:
    ///   byte2: vps_id(4)=0, max_sub_layers_minus1(3)=0, nesting(1)=1 -> 0x01
    ///   bytes 3..15: PTL (profile_space 0, tier 0, profile_idc 1 "Main", ...)
    ///   then: sps_id ue(0)='1', chroma ue(1)='010', w ue(...), h ue(...),
    ///   conformance(0), bit_depth_luma ue(0), bit_depth_chroma ue(0).
    fn hevc_sps() -> Vec<u8> {
        let mut sps = vec![NAL_HEADER_SPS, 0x01, 0x01];
        sps.extend_from_slice(&[
            0x01, // profile_space(2)=0 tier(1)=0 profile_idc(5)=1
            0x60, 0x00, 0x00, 0x00, // compat flags
            0x90, 0x00, 0x00, 0x00, 0x00, 0x00, // constraint flags
            0x5d, // level_idc = 93
        ]);
        // Trailing bit fields: sps_id=0 (1), chroma=1 (010), width ue(63)
        // (0000001000000), height ue(35) (00000100100), conformance 0,
        // bit_depths 1,1 — just pack enough valid bits; values past chroma and
        // depths are not asserted, only that parsing succeeds.
        // Bits: 1 010 0000001000000 00000100100 0 1 1  + padding
        let bits: &[u8] = &[
            1, 0, 1, 0, // sps_id=0, chroma starts
            0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, // width ue(63)
            0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, // height ue(35)
            0, // conformance_window_flag
            1, // bit_depth_luma ue(0)
            1, // bit_depth_chroma ue(0)
        ];
        let mut acc = 0u8;
        let mut n = 0;
        for &b in bits {
            acc = (acc << 1) | b;
            n += 1;
            if n == 8 {
                sps.push(acc);
                acc = 0;
                n = 0;
            }
        }
        if n > 0 {
            sps.push(acc << (8 - n));
        }
        sps
    }

    const NAL_HEADER_SPS: u8 = hevc::NAL_SPS << 1;
    const NAL_HEADER_PPS: u8 = hevc::NAL_PPS << 1;
    const NAL_HEADER_VPS: u8 = hevc::NAL_VPS << 1;
    const NAL_HEADER_IDR: u8 = 19 << 1; // IDR_W_RADL

    fn hevc_keyframe() -> Vec<u8> {
        let mut d = Vec::new();
        let vps = [NAL_HEADER_VPS, 0x01, 0xaa, 0xbb];
        let pps = [NAL_HEADER_PPS, 0x01, 0xcc];
        let idr = [NAL_HEADER_IDR, 0x01, 0x11, 0x22, 0x33];
        d.extend_from_slice(&[0, 0, 0, 1]);
        d.extend_from_slice(&vps);
        d.extend_from_slice(&[0, 0, 0, 1]);
        d.extend_from_slice(&hevc_sps());
        d.extend_from_slice(&[0, 0, 0, 1]);
        d.extend_from_slice(&pps);
        d.extend_from_slice(&[0, 0, 0, 1]);
        d.extend_from_slice(&idr);
        d
    }

    #[test]
    fn hevc_nal_types_use_high_bits() {
        assert_eq!(hevc::nal_type(&[NAL_HEADER_SPS, 0x01]), hevc::NAL_SPS);
        assert_eq!(hevc::nal_type(&[NAL_HEADER_VPS, 0x01]), hevc::NAL_VPS);
    }

    #[test]
    fn hevc_extradata_is_hvcc() {
        let hvcc = extradata(Codec::Hevc, &hevc_keyframe()).unwrap();
        assert_eq!(hvcc[0], 1); // configurationVersion
        assert_eq!(hvcc[1], 0x01); // general_profile: space 0, tier 0, idc 1 (Main)
        assert_eq!(hvcc[12], 0x5d); // general_level_idc
        assert_eq!(hvcc[16], 0xfc | 1); // chromaFormat 4:2:0
        assert_eq!(hvcc[17], 0xf8); // 8-bit luma
        assert_eq!(hvcc[21] & 0x3, 3); // lengthSizeMinusOne = 3
        assert_eq!(hvcc[22], 3); // three arrays: VPS, SPS, PPS
        assert_eq!(hvcc[23], 0x80 | hevc::NAL_VPS);
    }

    #[test]
    fn hevc_payload_drops_parameter_sets() {
        let out = packet_payload(Codec::Hevc, &hevc_keyframe().into());
        // Only the IDR remains: 4-byte length + 5 bytes payload.
        assert_eq!(out.len(), 4 + 5);
        assert_eq!(hevc::nal_type(&out[4..]), 19);
    }

    #[test]
    fn hevc_missing_sps_is_error() {
        let d = [0, 0, 0, 1, NAL_HEADER_PPS, 0x01, 0xcc];
        assert!(matches!(
            extradata(Codec::Hevc, &d),
            Err(BitstreamError::MissingParameterSets(_))
        ));
    }

    // --- AV1 ---

    /// Synthetic sequence header OBU: reduced_still_picture_header=1 keeps the
    /// payload trivial: profile(3)=0, still(1)=1, reduced(1)=1, level(5)=8.
    fn av1_seq_header_obu() -> Vec<u8> {
        // header: type=1 (<<3) | has_size (0x2)
        // payload bits: 000 1 1 01000 -> 0001_1010 0000_0000
        vec![(1 << 3) | 0x2, 2, 0b0001_1010, 0b0000_0000]
    }

    fn av1_keyframe() -> Vec<u8> {
        let mut d = av1_seq_header_obu();
        // A fake frame OBU (type 6) with size.
        d.extend_from_slice(&[(6 << 3) | 0x2, 3, 0xaa, 0xbb, 0xcc]);
        d
    }

    #[test]
    fn av1_extradata_is_av1c() {
        let c = extradata(Codec::Av1, &av1_keyframe()).unwrap();
        assert_eq!(c[0], 0x81);
        assert_eq!(c[1], 8); // profile 0, level 8
        assert_eq!(c[2], 0x0c); // tier 0, 8-bit 4:2:0
        assert_eq!(c[3], 0);
        assert_eq!(&c[4..], &av1_seq_header_obu()[..]); // configOBUs
    }

    #[test]
    fn av1_payload_is_refcount_passthrough() {
        let data: Bytes = av1_keyframe().into();
        let out = packet_payload(Codec::Av1, &data);
        assert_eq!(out, data);
        // Same allocation: passthrough must be a refcount bump, not a copy.
        assert_eq!(out.as_ptr(), data.as_ptr());
    }

    #[test]
    fn av1_missing_sequence_header_is_error() {
        let d = [(6u8 << 3) | 0x2, 2, 0xaa, 0xbb];
        assert!(matches!(
            extradata(Codec::Av1, &d),
            Err(BitstreamError::MissingParameterSets(_))
        ));
    }

    #[test]
    fn av1_non_reduced_header_parses_level_and_tier() {
        // profile(3)=0, still(1)=0, reduced(1)=0, timing_info_present(1)=0,
        // initial_display_delay(1)=0, op_cnt_minus1(5)=0, op_idc(12)=0,
        // level(5)=12, tier: level>7 -> tier(1)=1.
        // Bits: 000 0 0 0 0 00000 000000000000 01100 1 -> pad to bytes.
        let payload_bits: &[u8] = &[
            0, 0, 0, 0, 0, 0, 0, // profile + flags
            0, 0, 0, 0, 0, // op_cnt
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // op_idc
            0, 1, 1, 0, 0, // level = 12
            1, // tier
        ];
        let mut payload = Vec::new();
        let mut acc = 0u8;
        let mut n = 0;
        for &b in payload_bits {
            acc = (acc << 1) | b;
            n += 1;
            if n == 8 {
                payload.push(acc);
                acc = 0;
                n = 0;
            }
        }
        if n > 0 {
            payload.push(acc << (8 - n));
        }
        let mut obu = vec![(1u8 << 3) | 0x2, payload.len() as u8];
        obu.extend_from_slice(&payload);
        let c = extradata(Codec::Av1, &obu).unwrap();
        assert_eq!(c[1], 12); // profile 0, level 12
        assert_eq!(c[2], 0x80 | 0x0c); // tier 1
    }

    #[test]
    fn hevc_rbsp_unescape() {
        // 00 00 03 01 -> 00 00 01 after unescaping.
        let nal = [NAL_HEADER_SPS, 0x01, 0x00, 0x00, 0x03, 0x01];
        // Just exercise the path via a too-short SPS error (no panic).
        assert!(extradata(Codec::Hevc, &{
            let mut d = vec![0, 0, 0, 1];
            d.extend_from_slice(&nal);
            d
        })
        .is_err());
    }
}
