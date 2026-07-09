//! Framing for the Unix socket: a 3-byte magic + 1-byte protocol version + a
//! 4-byte big-endian length, followed by that many bytes of bincode payload.
//! Shared by the daemon, CLI, and HUD so all peers agree on the wire format.
//!
//! The magic + version make a peer skew (e.g. a stale `ord` binary in `$PATH`
//! talking to a newer `ordd`) fail **loudly** at the framing layer instead of
//! silently mis-decoding shifted bincode enum discriminants. Bump
//! [`PROTOCOL_VERSION`] whenever the `Command`/`Event` shapes or any nested
//! bincode payload they carry change incompatibly.

use std::io::{self, Read, Write};

/// Wire protocol version. Bump on any incompatible control-payload change.
///
/// v7: added `overlay.pressed_keys` layout transform fields, which cross the
/// wire in `Event::Config` and `Command::SetConfig`.
pub const PROTOCOL_VERSION: u8 = 8;

/// Frame magic identifying an open-recorder control message.
const MAGIC: [u8; 3] = *b"ORD";

/// Maximum accepted frame size (1 MiB) — a guard against a malformed/hostile
/// length prefix. Control messages are tiny; this is generous.
pub const MAX_FRAME: u32 = 1024 * 1024;

/// Write `payload` as a versioned, length-prefixed frame.
pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = payload.len();
    if len as u64 > MAX_FRAME as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame exceeds MAX_FRAME",
        ));
    }
    w.write_all(&MAGIC)?;
    w.write_all(&[PROTOCOL_VERSION])?;
    w.write_all(&(len as u32).to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one versioned, length-prefixed frame. Returns the payload bytes. Errors
/// if the magic or protocol version does not match this build (peer skew).
pub fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 4]; // 3-byte magic + 1-byte version
    r.read_exact(&mut header)?;
    if header[..3] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an open-recorder frame (bad magic)",
        ));
    }
    if header[3] != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "open-recorder protocol mismatch: peer v{}, this build v{} \
                 (a stale ord/ordd/ord-hud binary?)",
                header[3], PROTOCOL_VERSION
            ),
        ));
    }
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame length exceeds MAX_FRAME",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_round_trips() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello world").unwrap();
        let mut cur = Cursor::new(buf);
        let out = read_frame(&mut cur).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn empty_frame_round_trips() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"").unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap(), Vec::<u8>::new());
    }

    /// A valid magic + version header prefix for hand-crafted frames.
    fn header() -> Vec<u8> {
        let mut h = b"ORD".to_vec();
        h.push(PROTOCOL_VERSION);
        h
    }

    #[test]
    fn oversized_length_is_rejected() {
        // Craft a header claiming > MAX_FRAME bytes.
        let mut buf = header();
        buf.extend_from_slice(&(MAX_FRAME + 1).to_be_bytes());
        let mut cur = Cursor::new(buf);
        assert!(read_frame(&mut cur).is_err());
    }

    #[test]
    fn truncated_payload_errors() {
        let mut buf = header();
        buf.extend_from_slice(&10u32.to_be_bytes());
        buf.extend_from_slice(b"abc"); // fewer than 10 bytes
        let mut cur = Cursor::new(buf);
        assert!(read_frame(&mut cur).is_err());
    }

    #[test]
    fn protocol_version_mismatch_is_rejected() {
        let mut buf = b"ORD".to_vec();
        buf.push(PROTOCOL_VERSION.wrapping_add(1)); // a peer on a different version
        buf.extend_from_slice(&3u32.to_be_bytes());
        buf.extend_from_slice(b"abc");
        let err = read_frame(&mut Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut buf = b"XXX".to_vec();
        buf.push(PROTOCOL_VERSION);
        buf.extend_from_slice(&0u32.to_be_bytes());
        assert!(read_frame(&mut Cursor::new(buf)).is_err());
    }

    #[test]
    fn multiple_frames_sequential() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"one").unwrap();
        write_frame(&mut buf, b"two").unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap(), b"one");
        assert_eq!(read_frame(&mut cur).unwrap(), b"two");
    }
}
