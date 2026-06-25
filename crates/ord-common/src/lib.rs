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
pub mod version;

pub use client::{connect, Client, ClientError};
pub use config::{
    default_config_path, overrides_path, AudioConfig, CaptureCodec, CaptureConfig, Config,
    ConfigError, Container, ExportCodec, ExportConfig, HooksConfig, MarkersConfig, Quality,
    StorageConfig,
};
pub use frame::{read_frame, write_frame, MAX_FRAME, PROTOCOL_VERSION};
pub use ipc::{Command, Event, ProtocolError};
pub use newtypes::{BufferSeconds, ClipDuration, MonitorId};
pub use sync::lock_tolerant;

use std::path::PathBuf;

/// Path to the daemon control socket: `$XDG_RUNTIME_DIR/open-recorder.sock`,
/// falling back to `/tmp` when the runtime dir is unset. The single source of
/// truth shared by the daemon, the CLI, and the HUD so all three agree on the
/// location (pure path construction — no filesystem access).
pub fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join("open-recorder.sock")
}
