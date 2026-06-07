//! Command handler — the testable heart of the daemon.
//!
//! Owns the capture [`Engine`] and the daemon's mutable state, and maps each
//! [`Command`] to an [`Event`]. It is generic over the [`CaptureBackend`] so
//! tests drive it with `MockBackend` (no GPU). Clip writing is injected via a
//! closure so tests assert the selected clip without invoking ffmpeg.

use std::path::PathBuf;

use ord_common::{Command, Event};
use ord_core::{CaptureBackend, ClipError, Engine, PreparedClip};

/// Writes a prepared clip somewhere and returns the path written. Injectable so
/// the real daemon uses the ffmpeg muxer and tests use a fake.
pub type ClipWriter = Box<dyn FnMut(&PreparedClip) -> Result<PathBuf, String> + Send>;

/// Daemon state + capture engine.
pub struct Handler<B: CaptureBackend> {
    engine: Engine<B>,
    buffer_enabled: bool,
    recording: bool,
    write_clip: ClipWriter,
}

impl<B: CaptureBackend> Handler<B> {
    /// Build a handler over `engine`, using `write_clip` to persist saves.
    /// The engine should already be started if the buffer is meant to be live.
    pub fn new(engine: Engine<B>, write_clip: ClipWriter) -> Self {
        let buffer_enabled = engine.is_running();
        Self {
            engine,
            buffer_enabled,
            recording: false,
            write_clip,
        }
    }

    /// Ingest any frames the backend has produced. Call before handling a save so
    /// the buffer is current.
    pub fn pump(&mut self) -> usize {
        self.engine.drain_available()
    }

    /// Handle one command, returning the event to send back.
    pub fn handle(&mut self, cmd: Command) -> Event {
        match cmd {
            Command::SaveLast { duration } => self.handle_save(duration.get()),
            Command::ToggleRecord => self.handle_toggle_record(),
            Command::SetBuffer { enabled } => self.handle_set_buffer(enabled),
            Command::Status => self.handle_status(),
        }
    }

    fn handle_save(&mut self, seconds: u32) -> Event {
        self.engine.drain_available();
        match self.engine.take_clip(seconds) {
            Ok(clip) => match (self.write_clip)(&clip) {
                Ok(path) => Event::ClipSaved {
                    path: path.to_string_lossy().into_owned(),
                    duration: ord_common::ClipDuration::new(seconds.max(1))
                        .expect("seconds.max(1) >= 1"),
                },
                Err(e) => Event::Error {
                    message: format!("failed to write clip: {e}"),
                },
            },
            Err(ClipError::EmptyBuffer) => Event::Error {
                message: "nothing buffered yet — is the replay buffer enabled?".into(),
            },
            Err(ClipError::NoKeyframeInWindow) => Event::Error {
                message: "no keyframe available to start a decodable clip".into(),
            },
        }
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ord_common::ClipDuration;
    use ord_core::{Engine, MockBackend};
    use std::sync::{Arc, Mutex};

    /// A handler over the mock backend with a recording clip-writer that captures
    /// the last prepared clip's frame count.
    fn handler_with(
        fps: u32,
        frames: u32,
        kf: u32,
        cap_secs: u32,
    ) -> (Handler<MockBackend>, Arc<Mutex<Vec<usize>>>) {
        let mut engine = Engine::new(MockBackend::new(fps, frames, kf), cap_secs);
        engine.start().unwrap();
        let saved = Arc::new(Mutex::new(Vec::new()));
        let saved_w = Arc::clone(&saved);
        let writer: ClipWriter = Box::new(move |clip: &PreparedClip| {
            saved_w.lock().unwrap().push(clip.frames.len());
            Ok(PathBuf::from("/tmp/open-recorder/clip.mkv"))
        });
        (Handler::new(engine, writer), saved)
    }

    fn save(n: u32) -> Command {
        Command::SaveLast {
            duration: ClipDuration::new(n).unwrap(),
        }
    }

    #[test]
    fn save_writes_a_clip_and_reports_path() {
        let (mut h, saved) = handler_with(60, 600, 60, 60);
        let ev = h.handle(save(3));
        match ev {
            Event::ClipSaved { path, duration } => {
                assert_eq!(path, "/tmp/open-recorder/clip.mkv");
                assert_eq!(duration.get(), 3);
            }
            other => panic!("expected ClipSaved, got {other:?}"),
        }
        assert_eq!(saved.lock().unwrap().len(), 1);
        assert!(saved.lock().unwrap()[0] > 0);
    }

    #[test]
    fn save_on_empty_buffer_errors() {
        // 0 frames -> empty buffer.
        let (mut h, _saved) = handler_with(60, 0, 1, 60);
        let ev = h.handle(save(3));
        assert!(matches!(ev, Event::Error { .. }));
    }

    #[test]
    fn toggle_record_flips_state() {
        let (mut h, _s) = handler_with(60, 10, 1, 60);
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
        let (mut h, _s) = handler_with(60, 120, 60, 60);
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
        let (mut h, _s) = handler_with(60, 600, 60, 60); // 10s captured
        h.pump();
        match h.handle(Command::Status) {
            Event::Status {
                buffer_enabled,
                recording,
                buffered_seconds,
            } => {
                assert!(buffer_enabled);
                assert!(!recording);
                assert!(buffered_seconds >= 9);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }
}
