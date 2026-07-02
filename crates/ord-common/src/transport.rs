//! Cross-platform transport for the daemon control plane.
//!
//! Every peer speaks the same length-prefixed [`frame`](crate::frame) stream;
//! only the underlying socket differs by OS, hidden behind the [`Stream`] /
//! [`Listener`] aliases and the [`connect`] / [`bind`] helpers so the client
//! ([`client`](crate::client)) and the daemon server share one seam:
//!
//! * **unix** (Linux, macOS): a Unix domain socket at the control-socket path.
//!   Filesystem permissions gate who can reach the daemon — the right default,
//!   and what every keybind/HUD already expects.
//! * **non-unix** (Windows, …): there is no `AF_UNIX` path socket, so the same
//!   frames run over a **loopback** TCP connection. The daemon binds an
//!   ephemeral `127.0.0.1` port and publishes it in a tiny rendezvous file at
//!   the control-socket path; the client reads the port back and dials it. The
//!   socket is bound to loopback only, so it is never exposed off the machine.
//!
//! This is the Phase 0 "compile everywhere" seam. Capture is still Linux-only;
//! off-Linux this carries the control protocol for the client and clip UI.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};

#[cfg(not(unix))]
pub use std::net::{TcpListener as Listener, TcpStream as Stream};

/// Connect a client [`Stream`] to the daemon addressed by the control-socket
/// `path`.
#[cfg(unix)]
pub fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(path)
}

#[cfg(not(unix))]
pub fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(loopback_addr(path)?)
}

/// Bind the daemon [`Listener`] for the control-socket `path`.
///
/// Stale-file recovery is the *caller's* job (the daemon's `bind` probes the
/// path with [`connect`] and unlinks it if nothing answers) — this seam only
/// binds. On non-unix targets this also publishes the chosen loopback port in
/// a rendezvous file at `path` (written atomically via a sibling temp file +
/// rename, so a crash mid-write can never leave a half-written port for a
/// client to dial).
#[cfg(unix)]
pub fn bind(path: &Path) -> io::Result<Listener> {
    Listener::bind(path)
}

#[cfg(not(unix))]
pub fn bind(path: &Path) -> io::Result<Listener> {
    let listener = Listener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let tmp = path.with_extension("port.tmp");
    std::fs::write(&tmp, port.to_string())?;
    std::fs::rename(&tmp, path)?;
    Ok(listener)
}

/// Resolve the loopback address a non-unix client should dial, from the port
/// published in the rendezvous file at `path`.
#[cfg(not(unix))]
fn loopback_addr(path: &Path) -> io::Result<std::net::SocketAddr> {
    let text = std::fs::read_to_string(path)?;
    let port = parse_port(&text)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid daemon port file"))?;
    Ok(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
}

/// Parse a TCP port from the rendezvous file contents. Pure, so the non-unix
/// transport's only fiddly bit is unit-tested on every platform (including the
/// unix host) even though the transport itself only runs off-unix.
#[cfg(any(not(unix), test))]
pub(crate) fn parse_port(text: &str) -> Option<u16> {
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_port;

    #[test]
    fn parses_trimmed_port() {
        assert_eq!(parse_port("49321\n"), Some(49321));
        assert_eq!(parse_port("  8080  "), Some(8080));
        assert_eq!(parse_port("0"), Some(0));
    }

    #[test]
    fn rejects_garbage_and_out_of_range() {
        assert_eq!(parse_port(""), None);
        assert_eq!(parse_port("notaport"), None);
        assert_eq!(parse_port("70000"), None); // > u16::MAX
        assert_eq!(parse_port("-1"), None);
    }
}
