//! Shared fixtures for the golden tests. One `access_unit` instead of a copy
//! per test file (the bench keeps its own: benches can't reach `tests/`).

/// Build one Annex-B H.264 access unit. `keyframe` adds SPS+PPS before an IDR
/// slice; otherwise a single non-IDR slice. NAL payloads are minimal but carry
/// valid NAL headers so the avcC builder and muxer accept them.
pub fn access_unit(keyframe: bool) -> Vec<u8> {
    let sc = [0u8, 0, 0, 1];
    let mut d = Vec::new();
    if keyframe {
        // SPS (type 7): bytes after the header are profile/constraint/level.
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01]);
        // PPS (type 8).
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x68, 0xce, 0x3c, 0x80]);
        // IDR slice (type 5).
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x65, 0x88, 0x84, 0x00, 0x33, 0x44, 0x55]);
    } else {
        // Non-IDR slice (type 1).
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x41, 0x9a, 0x00, 0x10, 0x20]);
    }
    d
}
