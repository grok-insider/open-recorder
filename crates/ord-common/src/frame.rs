//! Length-prefixed framing for the Unix socket: a 4-byte big-endian length
//! followed by that many bytes of bincode payload. Shared by the daemon and CLI
//! so both sides agree on the wire format.

use std::io::{self, Read, Write};

/// Maximum accepted frame size (1 MiB) — a guard against a malformed/hostile
/// length prefix. Control messages are tiny; this is generous.
pub const MAX_FRAME: u32 = 1024 * 1024;

/// Write `payload` as a length-prefixed frame.
pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = payload.len();
    if len as u64 > MAX_FRAME as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame exceeds MAX_FRAME",
        ));
    }
    w.write_all(&(len as u32).to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one length-prefixed frame. Returns the payload bytes.
pub fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
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

    #[test]
    fn oversized_length_is_rejected() {
        // Craft a header claiming > MAX_FRAME bytes.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME + 1).to_be_bytes());
        let mut cur = Cursor::new(buf);
        assert!(read_frame(&mut cur).is_err());
    }

    #[test]
    fn truncated_payload_errors() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&10u32.to_be_bytes());
        buf.extend_from_slice(b"abc"); // fewer than 10 bytes
        let mut cur = Cursor::new(buf);
        assert!(read_frame(&mut cur).is_err());
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
