//! Shared types and the IPC wire protocol for open-recorder.
//!
//! This crate has no I/O. It defines the domain newtypes, the daemon control
//! protocol (`Command`/`Event`), and their bincode (de)serialization. The CLI,
//! GUI, and daemon all speak this protocol over the Unix socket.

pub mod client;
pub mod config;
pub mod frame;
pub mod ipc;
pub mod newtypes;
pub mod sync;
pub mod transport;
pub mod version;

pub use client::{connect, Client, ClientError};
pub use config::{
    default_config_path, overrides_path, AudioConfig, CaptureCodec, CaptureConfig, Config,
    ConfigError, Container, ExportCodec, ExportConfig, FpsMode, HooksConfig, MarkersConfig,
    PressedKeysConfig, PressedKeysPosition, Quality, StorageConfig,
};
pub use frame::{read_frame, write_frame, MAX_FRAME, PROTOCOL_VERSION};
pub use ipc::{Command, Event, OutputInfo, ProtocolError};
pub use newtypes::{BufferSeconds, ClipDuration};
pub use sync::lock_tolerant;

use std::path::PathBuf;

/// Path to the daemon control socket (unix) or loopback rendezvous file
/// (non-unix): `<runtime dir>/open-recorder.sock`, where the runtime dir is the
/// XDG runtime dir on Linux and the temp dir as a fallback (and on platforms
/// without one). The single source of truth shared by the daemon, the CLI, and
/// the HUD so all agree on the location (pure path construction — no I/O).
pub fn socket_path() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("open-recorder.sock")
}
