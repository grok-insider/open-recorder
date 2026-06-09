//! Unix-socket control server. Accepts connections, reads length-prefixed
//! [`Command`] frames, dispatches them through the [`Handler`], and writes back
//! [`Event`] frames.
//!
//! Single-threaded accept loop with a per-connection request/response exchange;
//! the handler holds the engine, so commands are naturally serialized.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use ord_common::{read_frame, write_frame, Command, Event};
use ord_core::{CaptureBackend, PreparedClip};

use crate::handler::Handler;

/// Writes a prepared clip to disk and returns the path written. Injectable so the
/// real daemon uses the ffmpeg muxer and tests use a fake. The server invokes it
/// **off the handler lock** so a slow mux never starves capture-frame draining.
pub type ClipWriter = Box<dyn FnMut(&PreparedClip) -> Result<PathBuf, String> + Send>;

/// Shared list of subscriber connections that receive pushed events.
type Subscribers = Arc<Mutex<Vec<UnixStream>>>;

/// Lock a mutex, recovering from poisoning instead of panicking. A daemon must
/// not die because some other thread panicked while holding a lock — in
/// particular the capture-drain pump must keep running, or memory grows unbounded
/// (the OOM this project already fixed once).
fn lock_tolerant<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Broadcast an event to all live subscribers, dropping any that have closed.
fn broadcast(subs: &Subscribers, event: &Event) {
    let payload = match event.encode() {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut guard = lock_tolerant(subs);
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
    writer: ClipWriter,
) -> Result<(), ServerError> {
    let handler = Arc::new(Mutex::new(handler));
    let writer = Arc::new(Mutex::new(writer));
    let subs: Subscribers = Arc::new(Mutex::new(Vec::new()));

    // Continuously drain captured frames into the (evicting) ring buffer,
    // independent of client activity. The capture forwarder thread produces
    // encoded frames non-stop; `pump()` (drain_available) is the only consumer,
    // and it was previously called ONLY while handling a client command. After
    // the HUD subscribes it stops sending commands, so during idle/gaming nothing
    // drained the channel and it grew unbounded at the encode bitrate (~8 MB/s)
    // until the OOM killer fired. A ~250 ms periodic pump keeps the channel
    // drained and the ring bounded to `buffer_seconds`. `pump()` is pure
    // ingestion (no events/side effects), so this is safe to call on a timer.
    {
        let handler = Arc::clone(&handler);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(250));
            lock_tolerant(&handler).pump();
        });
    }

    for conn in listener.incoming() {
        // A transient accept error (e.g. EMFILE under fd pressure) must not kill
        // the daemon: log and keep serving.
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ordd: accept error: {e}");
                continue;
            }
        };
        let handler = Arc::clone(&handler);
        let writer = Arc::clone(&writer);
        let subs = Arc::clone(&subs);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &handler, &writer, &subs) {
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
    writer: &Arc<Mutex<ClipWriter>>,
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

        let event = match cmd {
            Command::SaveLast { duration } => {
                // Prepare the clip under the handler lock — cheap: keyframe
                // selection + a refcount clone of the encoded window. Then write
                // it OFF the lock so the ~hundreds-of-ms ffmpeg mux (and the
                // hyprctl game probe inside the writer) never block the 250 ms
                // capture-drain pump, which would otherwise fill the bounded
                // forward channel and drop freshly-captured frames after a save.
                let prepared = {
                    let mut h = lock_tolerant(handler);
                    h.pump();
                    h.prepare_save(duration.get())
                };
                match prepared {
                    Ok(clip) => {
                        let mut w = lock_tolerant(writer);
                        match w(&clip) {
                            Ok(path) => Event::ClipSaved {
                                path: path.to_string_lossy().into_owned(),
                                duration,
                            },
                            Err(e) => Event::Error {
                                message: format!("failed to write clip: {e}"),
                            },
                        }
                    }
                    Err(ev) => ev,
                }
            }
            other => {
                let mut h = lock_tolerant(handler);
                h.pump();
                h.handle(other)
            }
        };

        // Reply to the requester (for Subscribe this is the initial snapshot).
        write_event(&mut stream, &event)?;

        if is_subscribe {
            // Register this connection for future pushes and stop reading it
            // for commands (the client now only listens).
            match stream.try_clone() {
                Ok(clone) => lock_tolerant(subs).push(clone),
                Err(e) => return Err(e),
            }
            return Ok(());
        }

        // State-changing events are pushed to all subscribers too.
        if event.is_state_change() {
            broadcast(subs, &event);
        }
    }
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
    use crate::handler::Handler;
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
        Handler::new(engine)
    }

    /// A writer that "succeeds" instantly without touching disk.
    fn mock_writer() -> ClipWriter {
        Box::new(|_clip: &PreparedClip| Ok(PathBuf::from("/tmp/open-recorder/x.mkv")))
    }

    /// End-to-end over a real Unix socket against the mock backend: a client
    /// sends Status + SaveLast and gets well-formed events back.
    #[test]
    fn socket_request_response_roundtrip() {
        let path = temp_sock("roundtrip");
        let listener = bind(&path).unwrap();

        let server = thread::spawn(move || {
            // Serve exactly one client then return (the client closes the conn).
            serve(listener, mock_handler(), mock_writer()).unwrap();
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
            let _ = serve(listener, mock_handler(), mock_writer());
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
            let _ = serve(listener, mock_handler(), mock_writer());
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

    /// Regression: a slow clip write must NOT hold the handler lock, or it starves
    /// the capture-drain pump and blocks every other command. We simulate a slow
    /// ffmpeg mux with a writer that blocks mid-write, then assert a concurrent
    /// `Status` still returns promptly. On the old lock-across-write code this
    /// `Status` would block until the writer released — caught here via a read
    /// timeout so the regression fails fast instead of hanging.
    #[test]
    fn save_write_does_not_block_other_commands() {
        use std::sync::mpsc;
        use std::time::Duration;

        let path = temp_sock("nostarve");
        let listener = bind(&path).unwrap();

        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let writer: ClipWriter = Box::new(move |_clip: &PreparedClip| {
            // Signal that the write is in-flight (handler lock already released),
            // then block until the test releases us.
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            Ok(PathBuf::from("/tmp/open-recorder/x.mkv"))
        });

        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler(), writer);
        });

        // Client A kicks off a save; the writer blocks mid-write.
        let mut a = UnixStream::connect(&path).unwrap();
        let save = Command::SaveLast {
            duration: ClipDuration::new(2).unwrap(),
        };
        write_frame(&mut a, &save.encode().unwrap()).unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer should start (clip prepared off-lock)");

        // Client B asks for Status while A's write is still blocked. Must return.
        let mut b = UnixStream::connect(&path).unwrap();
        b.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        write_frame(&mut b, &Command::Status.encode().unwrap()).unwrap();
        let resp = read_frame(&mut b).expect("Status must return while a save is writing");
        assert!(matches!(
            Event::decode(&resp).unwrap(),
            Event::Status { .. }
        ));

        // Release the writer; A now gets its ClipSaved.
        let _ = release_tx.send(());
        a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let saved = Event::decode(&read_frame(&mut a).unwrap()).unwrap();
        assert!(matches!(saved, Event::ClipSaved { .. }));

        let _ = std::fs::remove_file(&path);
    }
}
