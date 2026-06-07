//! Unix-socket control server. Accepts connections, reads length-prefixed
//! [`Command`] frames, dispatches them through the [`Handler`], and writes back
//! [`Event`] frames.
//!
//! Single-threaded accept loop with a per-connection request/response exchange;
//! the handler holds the engine, so commands are naturally serialized.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use ord_common::{read_frame, write_frame, Command, Event};
use ord_core::CaptureBackend;

use crate::handler::Handler;

/// Server errors.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("socket already in use at {0}")]
    InUse(String),
}

/// Default control socket path: `$XDG_RUNTIME_DIR/open-recorder.sock`, falling
/// back to `/tmp` if the runtime dir is unset.
pub fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join("open-recorder.sock")
}

/// Bind the listener, removing a stale socket file if present.
pub fn bind(path: &PathBuf) -> Result<UnixListener, ServerError> {
    if path.exists() {
        // If we can connect, another daemon owns it; otherwise it's stale.
        if UnixStream::connect(path).is_ok() {
            return Err(ServerError::InUse(path.display().to_string()));
        }
        let _ = std::fs::remove_file(path);
    }
    Ok(UnixListener::bind(path)?)
}

/// Serve commands on `listener` using `handler` until the listener closes.
/// `pump` is called before each command so the buffer is current (the real
/// daemon also pumps on a timer; here we pump on demand for determinism).
pub fn serve<B: CaptureBackend>(
    listener: UnixListener,
    mut handler: Handler<B>,
) -> Result<(), ServerError> {
    for conn in listener.incoming() {
        let mut stream = conn?;
        if let Err(e) = handle_connection(&mut stream, &mut handler) {
            // A broken client connection is not fatal to the daemon.
            if e.kind() == io::ErrorKind::UnexpectedEof {
                continue;
            }
            eprintln!("ordd: connection error: {e}");
        }
    }
    Ok(())
}

/// Handle one client: read commands, reply with events, until the client closes.
fn handle_connection<B: CaptureBackend>(
    stream: &mut UnixStream,
    handler: &mut Handler<B>,
) -> io::Result<()> {
    loop {
        let bytes = match read_frame(stream) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let event = match Command::decode(&bytes) {
            Ok(cmd) => {
                handler.pump();
                handler.handle(cmd)
            }
            Err(e) => Event::Error {
                message: format!("bad command: {e}"),
            },
        };
        let payload = event
            .encode()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        write_frame(stream, &payload)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{ClipWriter, Handler};
    use ord_common::ClipDuration;
    use ord_core::{Engine, MockBackend, PreparedClip};
    use std::path::PathBuf;
    use std::thread;

    fn temp_sock(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ord-test-{}-{}.sock", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn mock_handler() -> Handler<MockBackend> {
        let mut engine = Engine::new(MockBackend::new(60, 600, 60), 60);
        engine.start().unwrap();
        let writer: ClipWriter =
            Box::new(|_clip: &PreparedClip| Ok(PathBuf::from("/tmp/open-recorder/x.mkv")));
        Handler::new(engine, writer)
    }

    /// End-to-end over a real Unix socket against the mock backend: a client
    /// sends Status + SaveLast and gets well-formed events back.
    #[test]
    fn socket_request_response_roundtrip() {
        let path = temp_sock("roundtrip");
        let listener = bind(&path).unwrap();

        let server = thread::spawn(move || {
            // Serve exactly one client then return (the client closes the conn).
            serve(listener, mock_handler()).unwrap();
        });

        // Client.
        let mut client = UnixStream::connect(&path).unwrap();

        // Status.
        write_frame(&mut client, &Command::Status.encode().unwrap()).unwrap();
        let resp = Event::decode(&read_frame(&mut client).unwrap()).unwrap();
        assert!(matches!(
            resp,
            Event::Status {
                buffer_enabled: true,
                ..
            }
        ));

        // SaveLast(3).
        let save = Command::SaveLast {
            duration: ClipDuration::new(3).unwrap(),
        };
        write_frame(&mut client, &save.encode().unwrap()).unwrap();
        let resp = Event::decode(&read_frame(&mut client).unwrap()).unwrap();
        assert!(matches!(resp, Event::ClipSaved { .. }));

        // Close client -> server's accept loop continues; drop listener via
        // ending the test. Detach the server thread.
        drop(client);
        let _ = std::fs::remove_file(&path);
        // The server thread is blocked on accept; we don't join it (it would
        // block). Dropping the process at test end cleans it up.
        let _ = &server;
    }

    #[test]
    fn bad_command_bytes_yield_error_event() {
        let path = temp_sock("badcmd");
        let listener = bind(&path).unwrap();
        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler());
        });

        let mut client = UnixStream::connect(&path).unwrap();
        write_frame(&mut client, &[0xff, 0xff, 0xff, 0xff]).unwrap();
        let resp = Event::decode(&read_frame(&mut client).unwrap()).unwrap();
        assert!(matches!(resp, Event::Error { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bind_clears_stale_socket_file() {
        // A leftover socket file with no listener behind it must not block bind:
        // bind() should remove the stale file and succeed.
        let path = temp_sock("stale");
        std::fs::write(&path, b"").unwrap();
        let listener = bind(&path).expect("stale socket file should be cleared");
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bind_rejects_live_socket() {
        // A live listener already owns the path -> a second bind must error.
        let path = temp_sock("inuse");
        let l1 = bind(&path).unwrap();
        let result = bind(&path);
        assert!(matches!(result, Err(ServerError::InUse(_))));
        drop(l1);
        let _ = std::fs::remove_file(&path);
    }
}
