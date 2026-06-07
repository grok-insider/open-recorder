//! Domain newtypes. Wrapping these quantities keeps units and meanings from
//! being mixed up (e.g. a buffer length can never be passed where a clip length
//! is expected), per AGENTS.md "newtypes over primitives".

use serde::{Deserialize, Serialize};

/// Length of the replay ring buffer, in whole seconds. Always >= 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BufferSeconds(u32);

impl BufferSeconds {
    /// Construct a buffer length. Returns `None` for zero (a zero-length replay
    /// buffer is meaningless).
    pub fn new(seconds: u32) -> Option<Self> {
        if seconds == 0 {
            None
        } else {
            Some(Self(seconds))
        }
    }

    /// The value in seconds.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// A requested clip length ("save the last N seconds"), in whole seconds.
/// Always >= 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClipDuration(u32);

impl ClipDuration {
    /// Construct a clip duration. Returns `None` for zero.
    pub fn new(seconds: u32) -> Option<Self> {
        if seconds == 0 {
            None
        } else {
            Some(Self(seconds))
        }
    }

    /// The value in seconds.
    pub fn get(self) -> u32 {
        self.0
    }

    /// Clamp this duration to a buffer length: you can never save more than the
    /// buffer holds.
    pub fn clamped_to(self, buffer: BufferSeconds) -> ClipDuration {
        ClipDuration(self.0.min(buffer.get()))
    }
}

/// Identifier for a capture target monitor (compositor connector name, e.g.
/// "DP-1"), or the portal sentinel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorId(String);

impl MonitorId {
    /// A specific monitor by connector name.
    pub fn named(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The "ask the portal to pick" sentinel.
    pub fn portal() -> Self {
        Self("portal".to_string())
    }

    /// The underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the portal sentinel.
    pub fn is_portal(&self) -> bool {
        self.0 == "portal"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_seconds_rejects_zero() {
        assert!(BufferSeconds::new(0).is_none());
        assert_eq!(BufferSeconds::new(60).unwrap().get(), 60);
    }

    #[test]
    fn clip_duration_rejects_zero() {
        assert!(ClipDuration::new(0).is_none());
        assert_eq!(ClipDuration::new(30).unwrap().get(), 30);
    }

    #[test]
    fn clip_clamps_to_buffer() {
        let buffer = BufferSeconds::new(60).unwrap();
        // Requesting more than the buffer holds clamps down.
        assert_eq!(ClipDuration::new(120).unwrap().clamped_to(buffer).get(), 60);
        // Requesting less is unchanged.
        assert_eq!(ClipDuration::new(30).unwrap().clamped_to(buffer).get(), 30);
        // Requesting exactly the buffer length is unchanged.
        assert_eq!(ClipDuration::new(60).unwrap().clamped_to(buffer).get(), 60);
    }

    #[test]
    fn monitor_id_portal_sentinel() {
        assert!(MonitorId::portal().is_portal());
        assert!(!MonitorId::named("DP-1").is_portal());
        assert_eq!(MonitorId::named("DP-1").as_str(), "DP-1");
    }
}
