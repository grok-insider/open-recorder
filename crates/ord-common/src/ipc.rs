//! The daemon control protocol. `Command`s flow client -> daemon; `Event`s flow
//! daemon -> client. Both are length-prefixed bincode frames on the Unix socket
//! (framing lives in the daemon/CLI; this crate owns the types + (de)serde).

use serde::{Deserialize, Serialize};

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
    },
    /// An error occurred handling a command. User-facing, actionable text.
    Error { message: String },
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
            },
            Event::Error {
                message: "no keyframe in window".to_string(),
            },
        ];
        for ev in cases {
            let bytes = ev.encode().unwrap();
            let back = Event::decode(&bytes).unwrap();
            assert_eq!(ev, back);
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(Command::decode(&[0xff, 0xff, 0xff, 0xff, 0xff]).is_err());
    }
}
