//! The daemon control protocol. `Command`s flow client -> daemon; `Event`s flow
//! daemon -> client. Both are length-prefixed bincode frames on the Unix socket
//! (framing lives in the daemon/CLI; this crate owns the types + (de)serde).

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::newtypes::ClipDuration;

/// A request sent from a client (CLI/GUI) to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    /// Save the last `duration` seconds from the replay buffer to a clip.
    SaveLast { duration: ClipDuration },
    /// Toggle a manual full recording (start if stopped, stop if running).
    ToggleRecord,
    /// Enable/disable the always-on replay buffer.
    SetBuffer { enabled: bool },
    /// Ask the daemon for its current state.
    Status,
    /// Turn this connection into an event stream: the daemon pushes every
    /// subsequent [`Event`] (e.g. `ClipSaved`, `BufferState`) until the client
    /// disconnects. Used by the HUD overlay.
    Subscribe,
    /// Ask for the effective configuration (base + runtime overrides).
    GetConfig,
    /// Replace the runtime configuration. The daemon persists the sparse diff
    /// against the base config as overrides and applies it: storage/hooks/
    /// markers/export apply instantly; buffer length resizes the ring; encoder
    /// fields (fps/quality/codec/bitrate/audio) restart the capture session.
    SetConfig { config: Box<Config> },
    /// Place a marker ("clip that" bookmark) at the current buffer position.
    /// Markers inside a later save's window become MKV chapters. May also
    /// auto-save (see `markers.auto_save_seconds` in the config).
    Mark,
    /// Grab a still image of the most recent buffered frame (decodes the newest
    /// GOP and writes a PNG). The replay buffer must be armed.
    Screenshot,
}

/// A message sent from the daemon to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// A clip was written to disk.
    ClipSaved {
        path: String,
        duration: ClipDuration,
    },
    /// The replay buffer changed state.
    BufferState { enabled: bool },
    /// A manual recording changed state.
    RecordState { recording: bool },
    /// Current daemon status snapshot (reply to `Command::Status`).
    Status {
        buffer_enabled: bool,
        recording: bool,
        buffered_seconds: u32,
        buffered_frames: u32,
        buffered_keyframes: u32,
    },
    /// An error occurred handling a command. User-facing, actionable text.
    Error { message: String },
    /// Reply to [`Command::GetConfig`]: the effective configuration and the
    /// base layer it was derived from (so UIs can show which fields carry a
    /// runtime override).
    Config {
        effective: Box<Config>,
        base: Box<Config>,
    },
    /// A marker was placed. `auto_saving` is true when the daemon will also
    /// save a clip because of it (`markers.auto_save_seconds`).
    Marked { auto_saving: bool },
    /// The capture session was restarted (watchdog recovery after a stall —
    /// e.g. suspend/resume — or a settings change that requires it).
    CaptureRestarted,
    /// A screenshot was written to disk.
    ScreenshotSaved { path: String },
}

/// Errors encoding/decoding protocol frames.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("failed to encode message: {0}")]
    Encode(String),
    #[error("failed to decode message: {0}")]
    Decode(String),
}

impl Command {
    /// Encode to a bincode byte buffer.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        bincode::serialize(self).map_err(|e| ProtocolError::Encode(e.to_string()))
    }

    /// Decode from a bincode byte buffer.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        bincode::deserialize(bytes).map_err(|e| ProtocolError::Decode(e.to_string()))
    }
}

impl Event {
    /// Whether this event is a state change worth pushing to subscribers (the
    /// HUD). Lives on the type so the daemon's broadcast filter can't drift out
    /// of sync with the protocol as variants are added.
    pub fn is_state_change(&self) -> bool {
        matches!(
            self,
            Event::ClipSaved { .. }
                | Event::BufferState { .. }
                | Event::RecordState { .. }
                | Event::Marked { .. }
                | Event::CaptureRestarted
                | Event::ScreenshotSaved { .. }
        )
    }

    /// Encode to a bincode byte buffer.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        bincode::serialize(self).map_err(|e| ProtocolError::Encode(e.to_string()))
    }

    /// Decode from a bincode byte buffer.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        bincode::deserialize(bytes).map_err(|e| ProtocolError::Decode(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(n: u32) -> ClipDuration {
        ClipDuration::new(n).unwrap()
    }

    #[test]
    fn command_round_trips() {
        let cases = [
            Command::SaveLast { duration: clip(30) },
            Command::ToggleRecord,
            Command::SetBuffer { enabled: true },
            Command::SetBuffer { enabled: false },
            Command::Status,
            Command::Subscribe,
            Command::GetConfig,
            Command::Mark,
            Command::Screenshot,
            Command::SetConfig {
                config: Box::new(Config::default()),
            },
        ];
        for cmd in cases {
            let bytes = cmd.encode().unwrap();
            let back = Command::decode(&bytes).unwrap();
            assert_eq!(cmd, back);
        }
    }

    #[test]
    fn event_round_trips() {
        let cases = [
            Event::ClipSaved {
                path: "/home/friend/Videos/open-recorder/clip.mkv".to_string(),
                duration: clip(30),
            },
            Event::BufferState { enabled: true },
            Event::RecordState { recording: false },
            Event::Status {
                buffer_enabled: true,
                recording: false,
                buffered_seconds: 42,
                buffered_frames: 2520,
                buffered_keyframes: 84,
            },
            Event::Error {
                message: "no keyframe in window".to_string(),
            },
            Event::Config {
                effective: Box::new(Config::default()),
                base: Box::new(Config::default()),
            },
            Event::Marked { auto_saving: true },
            Event::CaptureRestarted,
            Event::ScreenshotSaved {
                path: "/home/friend/Videos/open-recorder/shot.png".to_string(),
            },
        ];
        for ev in cases {
            let bytes = ev.encode().unwrap();
            let back = Event::decode(&bytes).unwrap();
            assert_eq!(ev, back);
        }
    }

    #[test]
    fn broadcast_filter_covers_new_events() {
        assert!(Event::Marked { auto_saving: false }.is_state_change());
        assert!(Event::CaptureRestarted.is_state_change());
        // Config replies are point-to-point, never broadcast.
        assert!(!Event::Config {
            effective: Box::new(Config::default()),
            base: Box::new(Config::default()),
        }
        .is_state_change());
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(Command::decode(&[0xff, 0xff, 0xff, 0xff, 0xff]).is_err());
    }
}
