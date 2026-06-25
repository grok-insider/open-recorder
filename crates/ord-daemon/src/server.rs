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
use std::time::{Duration, Instant};

use ord_common::{lock_tolerant, read_frame, write_frame, BufferSeconds, Command, Config, Event};
use ord_core::{CaptureBackend, Engine, FrameStore, PreparedClip, RingBuffer};

use crate::handler::Handler;

/// Writes a prepared clip to disk and returns the path written. Injectable so the
/// real daemon uses the ffmpeg muxer and tests use a fake. The server invokes it
/// **off the handler lock** so a slow mux never starves capture-frame draining.
pub type ClipWriter = Box<dyn FnMut(&PreparedClip) -> Result<PathBuf, String> + Send>;

/// Builds a fresh (not yet started) engine from a configuration — how a
/// `SetConfig` with changed encoder settings restarts capture. Injected so the
/// daemon supplies the real backend and tests a mock. Generic over the replay
/// [`FrameStore`] (defaults to [`RingBuffer`]).
pub type EngineFactory<B, S = RingBuffer> = Box<dyn Fn(&Config) -> Engine<B, S> + Send + Sync>;

/// Persists the sparse overrides document (the diff against the base config).
/// Injected so tests capture writes instead of touching the state dir.
pub type OverridesWriter = Box<dyn FnMut(&str) -> Result<(), String> + Send>;

/// Decodes the newest buffered GOP into a still image on disk, returning the
/// path. Injected so the daemon uses ffmpeg and tests use a fake (no decode).
pub type ShotWriter = Box<
    dyn FnMut(&[ord_core::EncodedFrame], ord_core::StreamParams) -> Result<PathBuf, String> + Send,
>;

/// Everything the server needs beyond the handler: the configuration store and
/// the apply machinery for `SetConfig`, plus the capture watchdog policy.
pub struct ServerCtx<B: CaptureBackend, S: FrameStore = RingBuffer> {
    /// Effective configuration (base + overrides), shared with the writer.
    pub config: Arc<Mutex<Config>>,
    /// The immutable base layer (user/HM config file at startup).
    pub base: Config,
    pub engine_factory: EngineFactory<B, S>,
    pub write_overrides: OverridesWriter,
    /// Decodes the newest GOP to a still image (screenshot). Off the hot path.
    pub shot_writer: ShotWriter,
    /// Restart capture when the buffer is enabled but no frames arrived for
    /// this long (suspend/resume, output change). `None` disables the watchdog
    /// (tests; the mock emits a finite burst and would otherwise restart
    /// forever).
    pub watchdog: Option<Duration>,
}

/// Shared list of subscriber connections that receive pushed events.
type Subscribers = Arc<Mutex<Vec<UnixStream>>>;

// `lock_tolerant` comes from ord-common: a daemon must not die because some
// other thread panicked while holding a lock — in particular the capture-drain
// pump must keep running, or memory grows unbounded (the OOM this project
// already fixed once).

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
pub fn serve<B: CaptureBackend + 'static, S: FrameStore + 'static>(
    listener: UnixListener,
    handler: Handler<B, S>,
    writer: ClipWriter,
    ctx: ServerCtx<B, S>,
) -> Result<(), ServerError> {
    let handler = Arc::new(Mutex::new(handler));
    let writer = Arc::new(Mutex::new(writer));
    let subs: Subscribers = Arc::new(Mutex::new(Vec::new()));
    let watchdog = ctx.watchdog;
    let ctx = Arc::new(Mutex::new(ctx));

    // Continuously drain captured frames into the (evicting) ring buffer,
    // independent of client activity. The capture forwarder thread produces
    // encoded frames non-stop; `pump()` (drain_available) is the only consumer,
    // and it was previously called ONLY while handling a client command. After
    // the HUD subscribes it stops sending commands, so during idle/gaming nothing
    // drained the channel and it grew unbounded at the encode bitrate (~8 MB/s)
    // until the OOM killer fired. A ~250 ms periodic pump keeps the channel
    // drained and the ring bounded to `buffer_seconds`.
    //
    // The same thread runs the capture WATCHDOG: if the buffer is enabled but
    // no frames have arrived for `watchdog` (NVENC dies on suspend/resume, the
    // portal session ends on output changes), restart the capture session and
    // tell subscribers — the answer to the "it silently stopped recording"
    // failure mode every incumbent suffers from.
    {
        let handler = Arc::clone(&handler);
        let subs = Arc::clone(&subs);
        let ctx = Arc::clone(&ctx);
        std::thread::spawn(move || {
            let mut last_frames = Instant::now();
            let mut tick: u64 = 0;
            loop {
                std::thread::sleep(Duration::from_millis(250));
                tick = tick.wrapping_add(1);

                // AUTO-ARM (~every 3 s): when configured, start the replay buffer
                // as soon as a game takes the foreground (Steam app or fullscreen).
                // The hyprctl probe runs lock-free; we only lock to flip state.
                if tick.is_multiple_of(12) {
                    let auto = {
                        let c = lock_tolerant(&ctx);
                        let cfg = lock_tolerant(&c.config);
                        cfg.capture.auto_arm
                    };
                    if auto
                        && !lock_tolerant(&handler).is_buffer_enabled()
                        && crate::gamedetect::foreground_is_game()
                    {
                        let ev =
                            lock_tolerant(&handler).handle(Command::SetBuffer { enabled: true });
                        if ev.is_state_change() {
                            broadcast(&subs, &ev);
                        }
                        last_frames = Instant::now();
                    }
                }

                let mut h = lock_tolerant(&handler);
                if h.pump() > 0 {
                    last_frames = Instant::now();
                    continue;
                }
                let Some(timeout) = watchdog else { continue };
                if !h.is_buffer_enabled() || last_frames.elapsed() < timeout {
                    continue;
                }
                tracing::warn!(
                    stalled_for = ?last_frames.elapsed(),
                    "no frames from capture; restarting the session"
                );
                let event = match h.restart_capture() {
                    Ok(()) => Event::CaptureRestarted,
                    Err(e) => Event::Error {
                        message: format!("capture stalled and restart failed: {e}"),
                    },
                };
                drop(h);
                // Either way, wait a full window before the next attempt.
                last_frames = Instant::now();
                broadcast(&subs, &event);
            }
        });
    }

    for conn in listener.incoming() {
        // A transient accept error (e.g. EMFILE under fd pressure) must not kill
        // the daemon: log and keep serving.
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "accept error; continuing");
                continue;
            }
        };
        let handler = Arc::clone(&handler);
        let writer = Arc::clone(&writer);
        let subs = Arc::clone(&subs);
        let ctx = Arc::clone(&ctx);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &handler, &writer, &subs, &ctx) {
                if e.kind() != io::ErrorKind::UnexpectedEof {
                    tracing::warn!(error = %e, "connection error");
                }
            }
        });
    }
    Ok(())
}

/// Handle one client: read commands, reply with events, broadcasting state
/// changes to subscribers. A `Subscribe` command converts the connection into a
/// pushed event stream.
fn handle_connection<B: CaptureBackend, S: FrameStore>(
    mut stream: UnixStream,
    handler: &Arc<Mutex<Handler<B, S>>>,
    writer: &Arc<Mutex<ClipWriter>>,
    subs: &Subscribers,
    ctx: &Arc<Mutex<ServerCtx<B, S>>>,
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
            Command::SaveLast { duration } => save_flow(handler, writer, ctx, duration),
            Command::GetConfig => {
                let c = lock_tolerant(ctx);
                let effective = lock_tolerant(&c.config).clone();
                Event::Config {
                    effective: Box::new(effective),
                    base: Box::new(c.base.clone()),
                }
            }
            Command::SetConfig { config } => apply_config(handler, ctx, subs, *config),
            Command::Screenshot => screenshot_flow(handler, ctx),
            Command::Mark => {
                let marked = lock_tolerant(handler).mark();
                if !marked {
                    Event::Error {
                        message: "nothing buffered yet — is the replay buffer enabled?".into(),
                    }
                } else {
                    let auto = {
                        let c = lock_tolerant(ctx);
                        let cfg = lock_tolerant(&c.config);
                        cfg.markers.auto_save_seconds
                    };
                    if let Some(secs) = auto.and_then(ord_common::ClipDuration::new) {
                        // Marker doubles as "clip that": run the normal save
                        // flow (broadcasts ClipSaved to subscribers as usual).
                        let saved = save_flow(handler, writer, ctx, secs);
                        if saved.is_state_change() {
                            broadcast(subs, &saved);
                        }
                        Event::Marked { auto_saving: true }
                    } else {
                        Event::Marked { auto_saving: false }
                    }
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

/// The save pipeline: prepare under the handler lock (cheap selection +
/// refcount clone), then write OFF the lock so the ~hundreds-of-ms ffmpeg mux
/// (and the hyprctl game probe inside the writer) never block the 250 ms
/// capture-drain pump, which would otherwise fill the bounded forward channel
/// and drop freshly-captured frames after a save. Honors `clear_on_save`.
fn save_flow<B: CaptureBackend, S: FrameStore>(
    handler: &Arc<Mutex<Handler<B, S>>>,
    writer: &Arc<Mutex<ClipWriter>>,
    ctx: &Arc<Mutex<ServerCtx<B, S>>>,
    duration: ord_common::ClipDuration,
) -> Event {
    let prepared = {
        let mut h = lock_tolerant(handler);
        h.pump();
        h.prepare_save(duration)
    };
    match prepared {
        Ok((clip, clamped)) => {
            let written = {
                let mut w = lock_tolerant(writer);
                w(&clip)
            };
            match written {
                Ok(path) => {
                    let clear = {
                        let c = lock_tolerant(ctx);
                        let cfg = lock_tolerant(&c.config);
                        cfg.capture.clear_on_save
                    };
                    if clear {
                        lock_tolerant(handler).clear_buffer();
                    }
                    Event::ClipSaved {
                        path: path.to_string_lossy().into_owned(),
                        duration: clamped,
                    }
                }
                Err(e) => Event::Error {
                    message: format!("failed to write clip: {e}"),
                },
            }
        }
        Err(ev) => ev,
    }
}

/// Take a screenshot: select the newest decodable GOP under the handler lock
/// (cheap), then decode+encode the image off it. The decode runs under the ctx
/// lock (not the handler lock), so it never starves the capture-drain pump.
fn screenshot_flow<B: CaptureBackend, S: FrameStore>(
    handler: &Arc<Mutex<Handler<B, S>>>,
    ctx: &Arc<Mutex<ServerCtx<B, S>>>,
) -> Event {
    let prepared = {
        let mut h = lock_tolerant(handler);
        h.prepare_screenshot()
    };
    let Some((frames, params)) = prepared else {
        return Event::Error {
            message: "nothing buffered yet — is the replay buffer enabled?".into(),
        };
    };
    let written = {
        let mut c = lock_tolerant(ctx);
        (c.shot_writer)(&frames, params)
    };
    match written {
        Ok(path) => Event::ScreenshotSaved {
            path: path.to_string_lossy().into_owned(),
        },
        Err(e) => Event::Error {
            message: format!("failed to write screenshot: {e}"),
        },
    }
}

/// Apply a new configuration: persist the sparse overrides, swap the shared
/// config, then apply by tier — encoder/audio changes rebuild and restart the
/// capture engine, a buffer-length change resizes the ring in place, and
/// everything else (storage, hooks, markers, export) is read live by its
/// consumer. Replies with the new effective config.
fn apply_config<B: CaptureBackend, S: FrameStore>(
    handler: &Arc<Mutex<Handler<B, S>>>,
    ctx: &Arc<Mutex<ServerCtx<B, S>>>,
    subs: &Subscribers,
    new: Config,
) -> Event {
    if new.capture.fps == 0 || new.capture.fps > 240 {
        return Event::Error {
            message: "capture.fps must be between 1 and 240".into(),
        };
    }
    if !(100..=10_000).contains(&new.capture.keyframe_interval_ms) {
        return Event::Error {
            message: "capture.keyframe_interval_ms must be between 100 and 10000".into(),
        };
    }
    if new.capture.target.trim().is_empty() {
        return Event::Error {
            message: "capture.target must be 'portal' or a monitor name".into(),
        };
    }
    if new.capture.hdr && matches!(new.capture.codec, ord_common::config::CaptureCodec::H264) {
        return Event::Error {
            message: "capture.hdr requires an HEVC or AV1 codec".into(),
        };
    }
    if let Some(res) = new.capture.resolution {
        let bad = res.width < 16
            || res.height < 16
            || res.width > 16384
            || res.height > 16384
            || res.width % 2 != 0
            || res.height % 2 != 0;
        if bad {
            return Event::Error {
                message: "capture.resolution must be even and between 16 and 16384".into(),
            };
        }
    }
    let Some(buffer) = BufferSeconds::new(new.capture.buffer_seconds) else {
        return Event::Error {
            message: "capture.buffer_seconds must be at least 1".into(),
        };
    };

    let mut c = lock_tolerant(ctx);
    let overrides = match Config::diff_overrides(&c.base, &new) {
        Ok(o) => o,
        Err(e) => {
            return Event::Error {
                message: format!("could not compute settings overrides: {e}"),
            }
        }
    };
    if let Err(e) = (c.write_overrides)(&overrides) {
        return Event::Error {
            message: format!("could not persist settings: {e}"),
        };
    }

    let old = lock_tolerant(&c.config).clone();
    *lock_tolerant(&c.config) = new.clone();

    let capture_restart = old.capture.fps != new.capture.fps
        || old.capture.quality != new.capture.quality
        || old.capture.codec != new.capture.codec
        || old.capture.bitrate_kbps != new.capture.bitrate_kbps
        || old.capture.resolution != new.capture.resolution
        || old.capture.keyframe_interval_ms != new.capture.keyframe_interval_ms
        || old.capture.framerate_mode != new.capture.framerate_mode
        || old.capture.color_range != new.capture.color_range
        || old.capture.tune != new.capture.tune
        || old.capture.replay_storage != new.capture.replay_storage
        || old.capture.target != new.capture.target
        || old.capture.hdr != new.capture.hdr
        || old.audio != new.audio;
    let resize = old.capture.buffer_seconds != new.capture.buffer_seconds;

    if capture_restart {
        let mut engine = (c.engine_factory)(&new);
        let mut h = lock_tolerant(handler);
        if h.is_buffer_enabled() {
            if let Err(e) = engine.start() {
                // The old engine keeps running; the persisted overrides will
                // be retried on the next daemon start.
                return Event::Error {
                    message: format!("new capture settings failed to start: {e}"),
                };
            }
        }
        h.replace_engine(engine);
        drop(h);
        broadcast(subs, &Event::CaptureRestarted);
    } else if resize {
        lock_tolerant(handler).set_capacity(buffer);
    }

    let event = Event::Config {
        effective: Box::new(new),
        base: Box::new(c.base.clone()),
    };
    // Config replies are point-to-point, but subscribers (the HUD, an open
    // settings UI) still need to learn that the effective config changed —
    // e.g. `overlay.show_status_dot` applies live. Push it explicitly.
    broadcast(subs, &event);
    event
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
        Handler::new(
            engine,
            Box::new(|| std::env::temp_dir().join("ord-test-rec.mkv")),
        )
    }

    /// A writer that "succeeds" instantly without touching disk.
    fn mock_writer() -> ClipWriter {
        Box::new(|_clip: &PreparedClip| Ok(PathBuf::from("/tmp/open-recorder/x.mkv")))
    }

    /// A server context over the mock backend: default config, factory that
    /// builds a mock engine from the requested settings, overrides discarded,
    /// watchdog off (the mock's finite frame burst would trip it forever).
    fn mock_ctx() -> ServerCtx<MockBackend> {
        ServerCtx {
            config: Arc::new(Mutex::new(Config::default())),
            base: Config::default(),
            engine_factory: Box::new(|cfg| {
                Engine::new(
                    MockBackend::new(cfg.capture.fps, 600, 60),
                    cfg.capture.buffer_seconds,
                )
            }),
            write_overrides: Box::new(|_| Ok(())),
            shot_writer: Box::new(|_frames, _params| {
                Ok(PathBuf::from("/tmp/open-recorder/shot.png"))
            }),
            watchdog: None,
        }
    }

    fn request(client: &mut UnixStream, cmd: &Command) -> Event {
        write_frame(client, &cmd.encode().unwrap()).unwrap();
        Event::decode(&read_frame(client).unwrap()).unwrap()
    }

    /// End-to-end over a real Unix socket against the mock backend: a client
    /// sends Status + SaveLast and gets well-formed events back.
    #[test]
    fn socket_request_response_roundtrip() {
        let path = temp_sock("roundtrip");
        let listener = bind(&path).unwrap();

        let server = thread::spawn(move || {
            // Serve exactly one client then return (the client closes the conn).
            serve(listener, mock_handler(), mock_writer(), mock_ctx()).unwrap();
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
            let _ = serve(listener, mock_handler(), mock_writer(), mock_ctx());
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
            let _ = serve(listener, mock_handler(), mock_writer(), mock_ctx());
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
            let _ = serve(listener, mock_handler(), writer, mock_ctx());
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

    #[test]
    fn get_and_set_config_round_trip() {
        let path = temp_sock("config");
        let listener = bind(&path).unwrap();

        // Capture what the daemon persists as overrides.
        let written: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&written);
        let mut ctx = mock_ctx();
        ctx.write_overrides = Box::new(move |s| {
            lock_tolerant(&sink).push(s.to_string());
            Ok(())
        });
        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler(), mock_writer(), ctx);
        });

        let mut client = UnixStream::connect(&path).unwrap();

        // GetConfig returns the defaults.
        let Event::Config { effective, base } = request(&mut client, &Command::GetConfig) else {
            panic!("expected Config");
        };
        assert_eq!(*effective, Config::default());
        assert_eq!(*base, Config::default());

        // SetConfig with a changed buffer length: applied live + persisted.
        let mut desired = Config::default();
        desired.capture.buffer_seconds = 17;
        desired.hooks.on_clip_saved = Some("/bin/true".into());
        let reply = request(
            &mut client,
            &Command::SetConfig {
                config: Box::new(desired.clone()),
            },
        );
        let Event::Config { effective, .. } = reply else {
            panic!("expected Config reply, got {reply:?}");
        };
        assert_eq!(*effective, desired);

        // The persisted overrides are sparse (only the changed leaves).
        let writes = lock_tolerant(&written);
        assert_eq!(writes.len(), 1);
        assert!(writes[0].contains("buffer_seconds"), "{}", writes[0]);
        assert!(!writes[0].contains("fps"), "{}", writes[0]);

        // GetConfig now reflects the override.
        let Event::Config { effective, .. } = request(&mut client, &Command::GetConfig) else {
            panic!("expected Config");
        };
        assert_eq!(effective.capture.buffer_seconds, 17);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_config_pushes_config_to_subscribers() {
        let path = temp_sock("cfgpush");
        let listener = bind(&path).unwrap();
        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler(), mock_writer(), mock_ctx());
        });

        let mut sub = UnixStream::connect(&path).unwrap();
        write_frame(&mut sub, &Command::Subscribe.encode().unwrap()).unwrap();
        let _snapshot = read_frame(&mut sub).unwrap();

        // A live-tier change (no capture restart): subscribers still see it.
        let mut client = UnixStream::connect(&path).unwrap();
        let mut desired = Config::default();
        desired.overlay.show_status_dot = false;
        let reply = request(
            &mut client,
            &Command::SetConfig {
                config: Box::new(desired.clone()),
            },
        );
        assert!(matches!(reply, Event::Config { .. }), "{reply:?}");

        sub.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let pushed = Event::decode(&read_frame(&mut sub).unwrap()).unwrap();
        let Event::Config { effective, .. } = pushed else {
            panic!("expected pushed Config, got {pushed:?}");
        };
        assert!(!effective.overlay.show_status_dot);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_config_rejects_invalid_fps() {
        let path = temp_sock("badcfg");
        let listener = bind(&path).unwrap();
        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler(), mock_writer(), mock_ctx());
        });
        let mut client = UnixStream::connect(&path).unwrap();
        let mut bad = Config::default();
        bad.capture.fps = 0;
        let reply = request(
            &mut client,
            &Command::SetConfig {
                config: Box::new(bad),
            },
        );
        assert!(matches!(reply, Event::Error { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_config_rejects_bad_capture_knobs() {
        let path = temp_sock("badknobs");
        let listener = bind(&path).unwrap();
        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler(), mock_writer(), mock_ctx());
        });
        let mut client = UnixStream::connect(&path).unwrap();

        // Keyframe interval out of range.
        let mut bad = Config::default();
        bad.capture.keyframe_interval_ms = 50;
        assert!(matches!(
            request(
                &mut client,
                &Command::SetConfig {
                    config: Box::new(bad)
                }
            ),
            Event::Error { .. }
        ));

        // Odd capture dimensions (NVENC needs even).
        let mut bad = Config::default();
        bad.capture.resolution = Some(ord_common::config::Resolution {
            width: 1921,
            height: 1080,
        });
        assert!(matches!(
            request(
                &mut client,
                &Command::SetConfig {
                    config: Box::new(bad)
                }
            ),
            Event::Error { .. }
        ));

        // HDR with H.264 is rejected (needs HEVC/AV1).
        let mut bad = Config::default();
        bad.capture.hdr = true;
        bad.capture.codec = ord_common::config::CaptureCodec::H264;
        assert!(matches!(
            request(
                &mut client,
                &Command::SetConfig {
                    config: Box::new(bad)
                }
            ),
            Event::Error { .. }
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_config_encoder_change_restarts_capture() {
        let path = temp_sock("restartcfg");
        let listener = bind(&path).unwrap();
        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler(), mock_writer(), mock_ctx());
        });

        // A subscriber should observe the CaptureRestarted broadcast.
        let mut sub = UnixStream::connect(&path).unwrap();
        write_frame(&mut sub, &Command::Subscribe.encode().unwrap()).unwrap();
        let _snapshot = read_frame(&mut sub).unwrap();

        let mut client = UnixStream::connect(&path).unwrap();
        let mut desired = Config::default();
        desired.capture.fps = 30; // encoder-tier change
        let reply = request(
            &mut client,
            &Command::SetConfig {
                config: Box::new(desired),
            },
        );
        assert!(matches!(reply, Event::Config { .. }), "{reply:?}");

        sub.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let pushed = Event::decode(&read_frame(&mut sub).unwrap()).unwrap();
        assert_eq!(pushed, Event::CaptureRestarted);

        // The replacement engine works: a save still succeeds.
        let saved = request(
            &mut client,
            &Command::SaveLast {
                duration: ClipDuration::new(2).unwrap(),
            },
        );
        assert!(matches!(saved, Event::ClipSaved { .. }), "{saved:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mark_replies_and_auto_saves_when_configured() {
        let path = temp_sock("mark");
        let listener = bind(&path).unwrap();
        let ctx = mock_ctx();
        {
            let mut cfg = lock_tolerant(&ctx.config);
            cfg.markers.auto_save_seconds = Some(5);
        }
        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler(), mock_writer(), ctx);
        });

        // Subscriber sees the ClipSaved triggered by the auto-saving mark.
        let mut sub = UnixStream::connect(&path).unwrap();
        write_frame(&mut sub, &Command::Subscribe.encode().unwrap()).unwrap();
        let _snapshot = read_frame(&mut sub).unwrap();

        let mut client = UnixStream::connect(&path).unwrap();
        let reply = request(&mut client, &Command::Mark);
        assert_eq!(reply, Event::Marked { auto_saving: true });

        sub.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let pushed = Event::decode(&read_frame(&mut sub).unwrap()).unwrap();
        assert!(matches!(pushed, Event::ClipSaved { .. }), "{pushed:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn screenshot_returns_saved_event() {
        let path = temp_sock("shot");
        let listener = bind(&path).unwrap();
        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler(), mock_writer(), mock_ctx());
        });
        let mut client = UnixStream::connect(&path).unwrap();
        let reply = request(&mut client, &Command::Screenshot);
        assert!(matches!(reply, Event::ScreenshotSaved { .. }), "{reply:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clear_on_save_empties_the_buffer() {
        let path = temp_sock("clearsave");
        let listener = bind(&path).unwrap();
        let ctx = mock_ctx();
        {
            let mut cfg = lock_tolerant(&ctx.config);
            cfg.capture.clear_on_save = true;
        }
        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler(), mock_writer(), ctx);
        });

        let mut client = UnixStream::connect(&path).unwrap();
        let saved = request(
            &mut client,
            &Command::SaveLast {
                duration: ClipDuration::new(2).unwrap(),
            },
        );
        assert!(matches!(saved, Event::ClipSaved { .. }));

        // The buffer is now empty: a second immediate save has nothing.
        let again = request(
            &mut client,
            &Command::SaveLast {
                duration: ClipDuration::new(2).unwrap(),
            },
        );
        assert!(matches!(again, Event::Error { .. }), "{again:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn watchdog_restarts_stalled_capture() {
        let path = temp_sock("watchdog");
        let listener = bind(&path).unwrap();
        let mut ctx = mock_ctx();
        // The mock emits its whole burst up-front; after the first pump drains
        // it the stream is "stalled", so a short watchdog must fire.
        ctx.watchdog = Some(Duration::from_millis(600));
        let _server = thread::spawn(move || {
            let _ = serve(listener, mock_handler(), mock_writer(), ctx);
        });

        let mut sub = UnixStream::connect(&path).unwrap();
        write_frame(&mut sub, &Command::Subscribe.encode().unwrap()).unwrap();
        let _snapshot = read_frame(&mut sub).unwrap();

        sub.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let pushed = Event::decode(&read_frame(&mut sub).unwrap()).unwrap();
        assert_eq!(pushed, Event::CaptureRestarted);
        let _ = std::fs::remove_file(&path);
    }
}
