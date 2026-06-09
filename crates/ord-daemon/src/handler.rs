//! Command handler — the testable heart of the daemon.
//!
//! Owns the capture [`Engine`] and the daemon's mutable state, and maps each
//! [`Command`] to an [`Event`]. It is generic over the [`CaptureBackend`] so
//! tests drive it with `MockBackend` (no GPU). Clip writing is injected via a
//! closure so tests assert the selected clip without invoking ffmpeg.

use ord_common::{Command, Event};
use ord_core::{CaptureBackend, ClipError, Engine, PreparedClip};

/// Daemon state + capture engine.
pub struct Handler<B: CaptureBackend> {
    engine: Engine<B>,
    buffer_enabled: bool,
    recording: bool,
}

impl<B: CaptureBackend> Handler<B> {
    /// Build a handler over `engine`. The engine should already be started if the
    /// buffer is meant to be live. Clip *writing* is owned by the server layer so
    /// the slow ffmpeg mux runs off the handler lock (see [`prepare_save`]).
    ///
    /// [`prepare_save`]: Handler::prepare_save
    pub fn new(engine: Engine<B>) -> Self {
        let buffer_enabled = engine.is_running();
        Self {
            engine,
            buffer_enabled,
            recording: false,
        }
    }

    /// Ingest any frames the backend has produced. Call before handling a save so
    /// the buffer is current.
    pub fn pump(&mut self) -> usize {
        self.engine.drain_available()
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
            Command::SetBuffer { enabled } => self.handle_set_buffer(enabled),
            Command::Status => self.handle_status(),
            // Subscribe is finalized at the server layer (it keeps the connection
            // open and pushes events). The handler replies with an initial status
            // snapshot so the subscriber has current state immediately.
            Command::Subscribe => self.handle_status(),
        }
    }

    /// Drain pending frames and select the last `seconds` into a [`PreparedClip`],
    /// ready for the server to write off-lock. On failure returns the user-facing
    /// [`Event::Error`] to send back. Runs under the handler lock, but only does
    /// the cheap selection + refcount-clone (no ffmpeg, no disk, no subprocess).
    pub fn prepare_save(&mut self, seconds: u32) -> Result<PreparedClip, Event> {
        self.engine.drain_available();
        self.engine.take_clip(seconds).map_err(|e| match e {
            ClipError::EmptyBuffer => Event::Error {
                message: "nothing buffered yet — is the replay buffer enabled?".into(),
            },
            ClipError::NoKeyframeInWindow => Event::Error {
                message: "no keyframe available to start a decodable clip".into(),
            },
        })
    }

    fn handle_toggle_record(&mut self) -> Event {
        self.recording = !self.recording;
        Event::RecordState {
            recording: self.recording,
        }
    }

    fn handle_set_buffer(&mut self, enabled: bool) -> Event {
        if enabled && !self.engine.is_running() {
            if let Err(e) = self.engine.start() {
                return Event::Error {
                    message: format!("failed to start capture: {e}"),
                };
            }
        } else if !enabled && self.engine.is_running() {
            if let Err(e) = self.engine.stop() {
                return Event::Error {
                    message: format!("failed to stop capture: {e}"),
                };
            }
            self.engine.clear();
        }
        self.buffer_enabled = enabled;
        Event::BufferState { enabled }
    }

    fn handle_status(&mut self) -> Event {
        self.engine.drain_available();
        Event::Status {
            buffer_enabled: self.buffer_enabled,
            recording: self.recording,
            buffered_seconds: self.engine.buffered_seconds(),
            buffered_frames: self.engine.buffered_frames() as u32,
            buffered_keyframes: self.engine.buffered_keyframes() as u32,
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
        Handler::new(engine)
    }

    #[test]
    fn prepare_save_selects_a_clip() {
        let mut h = handler_with(60, 600, 60, 60);
        let clip = h.prepare_save(3).expect("clip prepared");
        assert!(!clip.frames.is_empty());
        assert!(clip.frames.first().unwrap().is_keyframe);
    }

    #[test]
    fn prepare_save_on_empty_buffer_errors() {
        // 0 frames -> empty buffer.
        let mut h = handler_with(60, 0, 1, 60);
        let err = h.prepare_save(3).expect_err("empty buffer");
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

    #[test]
    fn toggle_record_flips_state() {
        let mut h = handler_with(60, 10, 1, 60);
        assert_eq!(
            h.handle(Command::ToggleRecord),
            Event::RecordState { recording: true }
        );
        assert_eq!(
            h.handle(Command::ToggleRecord),
            Event::RecordState { recording: false }
        );
    }

    #[test]
    fn set_buffer_off_then_on() {
        let mut h = handler_with(60, 120, 60, 60);
        assert_eq!(
            h.handle(Command::SetBuffer { enabled: false }),
            Event::BufferState { enabled: false }
        );
        // After disabling, the engine stopped and cleared.
        match h.handle(Command::Status) {
            Event::Status { buffer_enabled, .. } => assert!(!buffer_enabled),
            other => panic!("expected Status, got {other:?}"),
        }
        assert_eq!(
            h.handle(Command::SetBuffer { enabled: true }),
            Event::BufferState { enabled: true }
        );
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
}
