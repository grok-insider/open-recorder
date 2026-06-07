//! Unix-socket control server. Accepts connections, reads length-prefixed
//! [`Command`] frames, dispatches them through the [`Handler`], and writes back
//! [`Event`] frames.
//!
//! Single-threaded accept loop with a per-connection request/response exchange;
//! the handler holds the engine, so commands are naturally serialized.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ord_common::{read_frame, write_frame, Command, Event};
use ord_core::CaptureBackend;

use crate::handler::Handler;

/// Shared list of subscriber connections that receive pushed events.
type Subscribers = Arc<Mutex<Vec<UnixStream>>>;

/// Broadcast an event to all live subscribers, dropping any that have closed.
fn broadcast(subs: &Subscribers, event: &Event) {
    let payload = match event.encode() {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut guard = subs.lock().expect("subscribers mutex");
    guard.retain_mut(|s| write_frame(s, &payload).is_ok());
}

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
///
/// Each connection is handled on its own thread; the handler is shared behind a
/// mutex so commands serialize naturally. A `Subscribe` connection is moved into
/// the subscriber registry and receives every event produced by state-changing
/// commands (e.g. `ClipSaved`, `BufferState`) until it disconnects.
pub fn serve<B: CaptureBackend + 'static>(
    listener: UnixListener,
    handler: Handler<B>,
) -> Result<(), ServerError> {
    let handler = Arc::new(Mutex::new(handler));
    let subs: Subscribers = Arc::new(Mutex::new(Vec::new()));

    for conn in listener.incoming() {
        let stream = conn?;
        let handler = Arc::clone(&handler);
        let subs = Arc::clone(&subs);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &handler, &subs) {
                if e.kind() != io::ErrorKind::UnexpectedEof {
                    eprintln!("ordd: connection error: {e}");
                }
            }
        });
    }
    Ok(())
}

/// Handle one client: read commands, reply with events, broadcasting state
/// changes to subscribers. A `Subscribe` command converts the connection into a
/// pushed event stream.
fn handle_connection<B: CaptureBackend>(
    mut stream: UnixStream,
    handler: &Arc<Mutex<Handler<B>>>,
    subs: &Subscribers,
) -> io::Result<()> {
    loop {
        let bytes = match read_frame(&mut stream) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };

        let cmd = match Command::decode(&bytes) {
            Ok(c) => c,
            Err(e) => {
                let ev = Event::Error {
                    message: format!("bad command: {e}"),
                };
                write_event(&mut stream, &ev)?;
                continue;
            }
        };

        let is_subscribe = matches!(cmd, Command::Subscribe);

        let event = {
            let mut h = handler.lock().expect("handler mutex");
            h.pump();
            h.handle(cmd)
        };

        // Reply to the requester (for Subscribe this is the initial snapshot).
        write_event(&mut stream, &event)?;

        if is_subscribe {
            // Register this connection for future pushes and stop reading it
            // for commands (the client now only listens).
            match stream.try_clone() {
                Ok(clone) => subs.lock().expect("subscribers mutex").push(clone),
                Err(e) => return Err(e),
            }
            return Ok(());
        }

        // State-changing events are pushed to all subscribers too.
        if should_broadcast(&event) {
            broadcast(subs, &event);
        }
    }
}

/// Events worth pushing to subscribers (HUD-relevant state changes).
fn should_broadcast(event: &Event) -> bool {
    matches!(
        event,
        Event::ClipSaved { .. } | Event::BufferState { .. } | Event::RecordState { .. }
    )
}

fn write_event(stream: &mut UnixStream, event: &Event) -> io::Result<()> {
    let payload = event
        .encode()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    write_frame(stream, &payload)
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
    fn subscriber_receives_pushed_events() {
        let path = temp_sock("subscribe");
        let listener = bind(&path).unwrap();
        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler());
        });

        // Subscriber connects and subscribes -> gets an initial Status snapshot.
        let mut sub = UnixStream::connect(&path).unwrap();
        write_frame(&mut sub, &Command::Subscribe.encode().unwrap()).unwrap();
        let snap = Event::decode(&read_frame(&mut sub).unwrap()).unwrap();
        assert!(matches!(snap, Event::Status { .. }));

        // A separate client triggers a state change.
        let mut client = UnixStream::connect(&path).unwrap();
        let save = Command::SaveLast {
            duration: ClipDuration::new(2).unwrap(),
        };
        write_frame(&mut client, &save.encode().unwrap()).unwrap();
        let _reply = read_frame(&mut client).unwrap();

        // The subscriber should now receive the pushed ClipSaved event.
        let pushed = Event::decode(&read_frame(&mut sub).unwrap()).unwrap();
        assert!(matches!(pushed, Event::ClipSaved { .. }));
        let _ = std::fs::remove_file(&path);
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
