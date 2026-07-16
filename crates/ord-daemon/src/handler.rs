//! Command handler — the testable heart of the daemon.
//!
//! Owns the capture [`Engine`] and the daemon's mutable state, and maps each
//! [`Command`] to an [`Event`]. It is generic over the [`CaptureBackend`] so
//! tests drive it with `MockBackend` (no GPU). Clip writing is injected via a
//! closure so tests assert the selected clip without invoking ffmpeg.

use std::path::PathBuf;

use ord_common::{BufferSeconds, ClipDuration, Command, Event};
use ord_core::{
    CaptureBackend, ClipError, EncodedFrame, Engine, FrameStore, PreparedClip, RecordingFault,
    RingBuffer, StreamParams,
};

/// Resolves where a new full-length recording should be written (game-named,
/// timestamped). Injected so the handler stays free of filename I/O and tests can
/// supply a temp path.
pub type RecordPath = Box<dyn FnMut() -> PathBuf + Send>;

/// Daemon state + capture engine.
///
/// Generic over the replay [`FrameStore`] so the daemon can pick RAM or disk at
/// runtime; defaults to [`RingBuffer`] so the mock tests (and any caller that
/// doesn't care) stay unchanged.
pub struct Handler<B: CaptureBackend, S: FrameStore = RingBuffer> {
    engine: Engine<B, S>,
    buffer: BufferSeconds,
    buffer_enabled: bool,
    record_path: RecordPath,
}

impl<B: CaptureBackend, S: FrameStore> Handler<B, S> {
    /// Build a handler over `engine`. The engine should already be started if the
    /// buffer is meant to be live. Clip *writing* is owned by the server layer so
    /// the slow ffmpeg mux runs off the handler lock (see [`prepare_save`]).
    /// `record_path` resolves the output path each time a recording is started.
    ///
    /// [`prepare_save`]: Handler::prepare_save
    pub fn new(engine: Engine<B, S>, record_path: RecordPath) -> Self {
        let buffer_enabled = engine.is_running();
        let buffer = BufferSeconds::new(engine.capacity_seconds())
            .unwrap_or_else(|| BufferSeconds::new(1).expect("1 is non-zero"));
        Self {
            engine,
            buffer,
            buffer_enabled,
            record_path,
        }
    }

    /// Ingest any frames the backend has produced. Call before handling a save so
    /// the buffer is current. After pumping, call [`take_recording_fault`] so a
    /// mid-stream recording abort is not left unreported.
    pub fn pump(&mut self) -> usize {
        self.engine.drain_available()
    }

    /// Take a pending encode-health alarm (CBR undershoot). The server
    /// broadcasts it as [`Event::Error`] so the HUD toasts.
    pub fn take_encode_alarm(&mut self) -> Option<String> {
        self.engine.take_encode_alarm()
    }

    /// Set the CBR target used for encode-health (call after building the engine).
    pub fn set_target_bitrate_kbps(&mut self, kbps: Option<u32>) {
        self.engine.set_target_bitrate_kbps(kbps);
    }

    /// Take a pending recording fault (write failure → finalized file).
    pub fn take_recording_fault(&mut self) -> Option<RecordingFault> {
        self.engine.take_recording_fault()
    }

    /// Whether the replay buffer is meant to be live (the watchdog only
    /// recovers a stalled capture when the user wants it running).
    pub fn is_buffer_enabled(&self) -> bool {
        self.buffer_enabled
    }

    /// Whether capture is actually running (as opposed to merely *armed*).
    pub fn is_capturing(&self) -> bool {
        self.engine.is_running()
    }

    /// Finalize any active full-length recording because capture is about to
    /// be replaced (encoder settings change is a clean cut). Returns the
    /// event(s) the caller must broadcast so HUD/CLI learn the recording ended.
    pub fn cut_active_recording(&mut self) -> Vec<Event> {
        let Some(result) = self.engine.stop_recording() else {
            return Vec::new();
        };
        match result {
            Ok(path) => vec![Event::RecordState {
                recording: false,
                path: Some(path.to_string_lossy().into_owned()),
            }],
            Err(e) => vec![
                Event::Error {
                    message: format!("recording failed to finalize on capture restart: {e}"),
                },
                Event::RecordState {
                    recording: false,
                    path: None,
                },
            ],
        }
    }

    /// Map a [`RecordingFault`] into the events clients need to see.
    pub fn events_for_recording_fault(fault: RecordingFault) -> Vec<Event> {
        let path = fault.path.map(|p| p.to_string_lossy().into_owned());
        vec![
            Event::Error {
                message: fault.cause,
            },
            Event::RecordState {
                recording: false,
                path,
            },
        ]
    }

    /// Place a marker at the newest buffered frame (drains first so "now" is
    /// current). Returns `false` when nothing is buffered.
    pub fn mark(&mut self) -> bool {
        self.engine.drain_available();
        self.engine.mark()
    }

    /// Drop everything buffered (used by `clear_on_save`).
    pub fn clear_buffer(&mut self) {
        self.engine.clear();
    }

    /// Resize the replay window without touching capture.
    pub fn set_capacity(&mut self, buffer: BufferSeconds) {
        self.engine.set_capacity_seconds(buffer.get());
        self.buffer = buffer;
    }

    /// Restart the capture session in place (same parameters) — the watchdog's
    /// recovery for a stalled backend (suspend/resume, output change).
    pub fn restart_capture(&mut self) -> Result<(), String> {
        self.engine.restart().map_err(|e| e.to_string())
    }

    /// Install `engine` and return the previous one. Neither engine is
    /// stopped here — the capture supervisor stops the returned engine
    /// **off the handler lock** so a hung PipeWire/NVENC teardown cannot
    /// freeze the control plane. `armed` is the post-install buffer intent.
    ///
    /// Any active recording on the outgoing engine must already have been
    /// finalized via [`cut_active_recording`] (or moved via
    /// [`adopt_replay_from`](Engine::adopt_replay_from)).
    pub fn exchange_engine(&mut self, engine: Engine<B, S>, armed: bool) -> Engine<B, S> {
        if self.engine.is_recording() {
            // Defensive: never drop an open muxer with the outgoing engine.
            let _ = self.cut_active_recording();
        }
        self.buffer = BufferSeconds::new(engine.capacity_seconds())
            .unwrap_or_else(|| BufferSeconds::new(1).expect("1 is non-zero"));
        self.buffer_enabled = armed;
        std::mem::replace(&mut self.engine, engine)
    }

    /// Swap in a freshly built engine (encoder settings changed). Buffered
    /// footage from the old engine is dropped (clean cut). Stops the old
    /// capture **before** drop; prefer the supervisor's off-lock exchange
    /// path for long teardowns.
    pub fn replace_engine(&mut self, engine: Engine<B, S>) {
        let armed = engine.is_running();
        let mut old = self.exchange_engine(engine, armed);
        if let Err(e) = old.stop() {
            tracing::debug!(error = %e, "old engine stop before replace");
        }
    }

    /// Swap in a freshly built (and started) engine, transplanting the replay
    /// state — ring, audio, markers, active recording — from the old one.
    /// The capture supervisor's arm/restart path: recovery must never discard
    /// buffered footage. Returns the outgoing (stopped/empty) engine so the
    /// supervisor can finish teardown off-lock if needed.
    pub fn install_engine_preserving_replay(&mut self, mut engine: Engine<B, S>) -> Engine<B, S> {
        engine.adopt_replay_from(&mut self.engine);
        // Armed intent stays true on this path (restart/arm recovery).
        self.exchange_engine(engine, true)
    }

    /// Stop the current engine ahead of a supervisor restart (quick, never
    /// touches the portal). The armed *intent* is kept so `is_buffer_enabled`
    /// stays true while the replacement session starts.
    ///
    /// Prefer the supervisor's off-lock stop (exchange + stop outside the
    /// handler mutex) when the backend teardown may hang.
    pub fn stop_engine_for_restart(&mut self) {
        if let Err(e) = self.engine.stop() {
            tracing::debug!(error = %e, "old engine stop before restart");
        }
    }

    /// Take the engine out for an off-lock stop, leaving `placeholder` installed
    /// so the control plane stays responsive. Preserves armed intent.
    pub fn detach_engine_for_stop(&mut self, placeholder: Engine<B, S>) -> Engine<B, S> {
        let armed = self.buffer_enabled;
        self.exchange_engine(placeholder, armed)
    }

    /// Disarm the replay buffer under the handler lock **without** stopping
    /// capture: the supervisor stops the returned engine off-lock. Clears
    /// buffered footage on the detached engine. Returns `(old_engine, reply)`.
    pub fn disable_capture_detach(&mut self, placeholder: Engine<B, S>) -> (Engine<B, S>, Event) {
        self.buffer_enabled = false;
        let mut old = self.exchange_engine(placeholder, false);
        old.clear();
        (old, Event::BufferState { enabled: false })
    }

    /// Disarm the replay buffer: stop capture, drop buffered footage, clear
    /// the armed flag. Idempotent; returns the reply event.
    ///
    /// Prefer [`disable_capture_detach`] from the supervisor so stop runs
    /// off-lock; this remains for tests and simple call sites.
    pub fn disable_capture(&mut self) -> Event {
        if self.engine.is_running() {
            if let Err(e) = self.engine.stop() {
                return Event::Error {
                    message: format!("failed to stop capture: {e}"),
                };
            }
            self.engine.clear();
        }
        self.buffer_enabled = false;
        Event::BufferState { enabled: false }
    }

    /// Handle one command, returning the event to send back.
    ///
    /// [`Command::SaveLast`] is intentionally **not** handled here: the server
    /// intercepts it, calls [`prepare_save`](Handler::prepare_save) under the
    /// lock, then writes the clip off the lock so the capture-drain pump is never
    /// starved by a multi-hundred-millisecond mux. Reaching the `SaveLast` arm
    /// means a dispatch wiring bug.
    pub fn handle(&mut self, cmd: Command) -> Event {
        match cmd {
            Command::SaveLast { .. } => Event::Error {
                message: "internal: save was not dispatched off-lock".into(),
            },
            Command::ToggleRecord => self.handle_toggle_record(),
            // SetBuffer is server-dispatched through the capture supervisor:
            // starting capture can block on the screen-share portal, which
            // must never happen under the handler lock. Reaching this arm
            // means a dispatch wiring bug, same as SaveLast.
            Command::SetBuffer { .. } => Event::Error {
                message: "internal: buffer toggle was not dispatched to the capture supervisor"
                    .into(),
            },
            Command::Status => self.handle_status(),
            // Subscribe is finalized at the server layer (it keeps the connection
            // open and pushes events). The handler replies with an initial status
            // snapshot so the subscriber has current state immediately.
            Command::Subscribe => self.handle_status(),
            // Config and marker commands are server-dispatched (they touch the
            // config store / run save flows off-lock). Reaching these arms means
            // a dispatch wiring bug, same as SaveLast.
            Command::GetConfig
            | Command::SetConfig { .. }
            | Command::Mark
            | Command::Screenshot
            | Command::ListOutputs => Event::Error {
                message: "internal: command was not dispatched by the server".into(),
            },
        }
    }

    /// Drain pending frames and select the newest decodable GOP for a
    /// screenshot, leaving the buffer intact. `None` when nothing is buffered.
    pub fn prepare_screenshot(&mut self) -> Option<(Vec<EncodedFrame>, StreamParams)> {
        self.engine.drain_available();
        self.engine.take_latest_gop()
    }

    /// Drain pending frames and select the last `duration` into a [`PreparedClip`],
    /// ready for the server to write off-lock. The request is clamped to the
    /// configured buffer length *and* to what is actually buffered, so a save
    /// right after arming reports the real clip length instead of the capacity.
    /// On failure returns the user-facing [`Event::Error`] to send back. Runs
    /// under the handler lock, but only does the cheap selection +
    /// refcount-clone (no ffmpeg, no disk, no subprocess).
    pub fn prepare_save(
        &mut self,
        duration: ClipDuration,
    ) -> Result<(PreparedClip, ClipDuration), Event> {
        let clamped = duration.clamped_to(self.buffer);
        self.engine.drain_available();
        let clip = self.engine.take_clip(clamped).map_err(|e| match e {
            ClipError::EmptyBuffer => Event::Error {
                message: "nothing buffered yet — is the replay buffer enabled?".into(),
            },
            ClipError::NoKeyframeInWindow => Event::Error {
                message: "no keyframe available to start a decodable clip".into(),
            },
        })?;
        let buffered = self.engine.buffered_seconds().max(1);
        let reported = ClipDuration::new(clamped.get().min(buffered)).unwrap_or(clamped);
        Ok((clip, reported))
    }

    fn handle_toggle_record(&mut self) -> Event {
        if self.engine.is_recording() {
            match self.engine.stop_recording() {
                Some(Err(e)) => Event::Error {
                    message: format!("recording failed to finalize: {e}"),
                },
                Some(Ok(path)) => Event::RecordState {
                    recording: false,
                    path: Some(path.to_string_lossy().into_owned()),
                },
                None => Event::RecordState {
                    recording: false,
                    path: None,
                },
            }
        } else {
            let path = (self.record_path)();
            match self.engine.start_recording(path.clone()) {
                Ok(()) => Event::RecordState {
                    recording: true,
                    path: Some(path.to_string_lossy().into_owned()),
                },
                Err(e) => Event::Error {
                    message: format!("could not start recording: {e}"),
                },
            }
        }
    }

    fn handle_status(&mut self) -> Event {
        self.engine.drain_available();
        Event::Status {
            buffer_enabled: self.buffer_enabled,
            recording: self.engine.is_recording(),
            buffered_seconds: self.engine.buffered_seconds(),
            buffered_frames: self.engine.buffered_frames() as u32,
            buffered_keyframes: self.engine.buffered_keyframes() as u32,
            encode_bitrate_kbps: self.engine.encode_bitrate_kbps(),
            target_bitrate_kbps: self.engine.target_bitrate_kbps(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ord_core::{Engine, MockBackend};

    /// A handler over the mock backend (no clip writer — writing is the server's
    /// job now; the handler only *prepares* clips).
    fn handler_with(fps: u32, frames: u32, kf: u32, cap_secs: u32) -> Handler<MockBackend> {
        let mut engine = Engine::new(MockBackend::new(fps, frames, kf), cap_secs);
        engine.start().unwrap();
        Handler::new(
            engine,
            Box::new(|| std::env::temp_dir().join("ord-test-rec.mkv")),
        )
    }

    fn cd(seconds: u32) -> ClipDuration {
        ClipDuration::new(seconds).unwrap()
    }

    #[test]
    fn prepare_save_selects_a_clip() {
        let mut h = handler_with(60, 600, 60, 60);
        let (clip, dur) = h.prepare_save(cd(3)).expect("clip prepared");
        assert!(!clip.frames.is_empty());
        assert!(clip.frames.first().unwrap().is_keyframe);
        assert_eq!(dur.get(), 3);
    }

    #[test]
    fn prepare_save_clamps_to_what_is_buffered() {
        // Capacity is 60 s but only ~10 s were captured; requesting 120 s must
        // report the *actual* clip length, not the configured capacity.
        let mut h = handler_with(60, 600, 60, 60);
        let (_clip, dur) = h.prepare_save(cd(120)).expect("clip prepared");
        assert!((9..=10).contains(&dur.get()), "reported {}", dur.get());
    }

    #[test]
    fn prepare_save_on_empty_buffer_errors() {
        // 0 frames -> empty buffer.
        let mut h = handler_with(60, 0, 1, 60);
        let err = h.prepare_save(cd(3)).expect_err("empty buffer");
        assert!(matches!(err, Event::Error { .. }));
    }

    #[test]
    fn save_command_is_not_handled_inline() {
        // The handler must refuse to write inline; the server dispatches saves
        // off-lock. Reaching `handle` with SaveLast yields an internal error.
        let mut h = handler_with(60, 600, 60, 60);
        let ev = h.handle(Command::SaveLast {
            duration: ord_common::ClipDuration::new(3).unwrap(),
        });
        assert!(matches!(ev, Event::Error { .. }));
    }

    #[cfg(not(feature = "mux"))]
    #[test]
    fn toggle_record_without_mux_errors() {
        // The no-`mux` build has no streaming muxer, so starting a recording fails
        // with a clear error instead of silently "succeeding" (the old stub that
        // flipped a bool and wrote nothing). Status stays not recording.
        let mut h = handler_with(60, 10, 1, 60);
        assert!(matches!(
            h.handle(Command::ToggleRecord),
            Event::Error { .. }
        ));
        match h.handle(Command::Status) {
            Event::Status { recording, .. } => assert!(!recording),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[cfg(feature = "mux")]
    #[test]
    fn toggle_record_flips_state() {
        // With the muxer present, ToggleRecord opens then closes a real recording.
        // We don't drain frames here: the mock's synthetic packets aren't valid
        // H.264, so the muxer would (correctly) reject them and auto-stop — real
        // frame streaming is covered by record_golden.rs.
        let mut h = handler_with(60, 10, 1, 60);
        assert!(matches!(
            h.handle(Command::ToggleRecord),
            Event::RecordState {
                recording: true,
                path: Some(_)
            }
        ));
        assert!(matches!(
            h.handle(Command::ToggleRecord),
            Event::RecordState {
                recording: false,
                path: Some(_)
            }
        ));
    }

    #[test]
    fn set_buffer_is_not_handled_inline() {
        // Buffer toggles are server-dispatched through the capture supervisor
        // (a capture start can block on the portal, never under the handler
        // lock). Reaching `handle` with SetBuffer yields an internal error.
        let mut h = handler_with(60, 120, 60, 60);
        assert!(matches!(
            h.handle(Command::SetBuffer { enabled: true }),
            Event::Error { .. }
        ));
    }

    #[test]
    fn disable_then_supervisor_style_arm() {
        let mut h = handler_with(60, 120, 60, 60);
        assert_eq!(h.disable_capture(), Event::BufferState { enabled: false });
        // After disabling, the engine stopped, cleared, and disarmed.
        match h.handle(Command::Status) {
            Event::Status { buffer_enabled, .. } => assert!(!buffer_enabled),
            other => panic!("expected Status, got {other:?}"),
        }
        // Disable is idempotent.
        assert_eq!(h.disable_capture(), Event::BufferState { enabled: false });

        // The supervisor arms by installing a freshly started engine that
        // adopts the (cleared) replay state; the armed flag re-derives.
        let mut fresh = Engine::new(MockBackend::new(60, 120, 60), 60);
        fresh.start().unwrap();
        let _stale = h.install_engine_preserving_replay(fresh);
        assert!(h.is_buffer_enabled());
        match h.handle(Command::Status) {
            Event::Status { buffer_enabled, .. } => assert!(buffer_enabled),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn status_reports_buffered_seconds() {
        let mut h = handler_with(60, 600, 60, 60); // 10s captured
        h.pump();
        match h.handle(Command::Status) {
            Event::Status {
                buffer_enabled,
                recording,
                buffered_seconds,
                buffered_frames,
                buffered_keyframes,
                ..
            } => {
                assert!(buffer_enabled);
                assert!(!recording);
                assert!(buffered_seconds >= 9);
                assert!(buffered_frames > 0);
                assert!(buffered_keyframes > 0);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[cfg(feature = "mux")]
    #[test]
    fn cut_active_recording_finalizes_and_reports() {
        let mut h = handler_with(60, 10, 1, 60);
        assert!(matches!(
            h.handle(Command::ToggleRecord),
            Event::RecordState {
                recording: true,
                path: Some(_)
            }
        ));
        let events = h.cut_active_recording();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            Event::RecordState {
                recording: false,
                path: Some(_)
            }
        ));
        match h.handle(Command::Status) {
            Event::Status { recording, .. } => assert!(!recording),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn events_for_recording_fault_are_error_then_record_state() {
        let fault = RecordingFault {
            cause: "recording video write failed: test".into(),
            path: Some(PathBuf::from("/tmp/clip.mkv")),
        };
        let events = Handler::<MockBackend>::events_for_recording_fault(fault);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            Event::Error { message } if message.contains("video write failed")
        ));
        assert!(matches!(
            &events[1],
            Event::RecordState {
                recording: false,
                path: Some(p)
            } if p == "/tmp/clip.mkv"
        ));
    }
}
