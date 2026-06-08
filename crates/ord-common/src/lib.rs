//! Shared types and the IPC wire protocol for open-recorder.
//!
//! This crate has no I/O. It defines the domain newtypes, the daemon control
//! protocol (`Command`/`Event`), and their bincode (de)serialization. The CLI,
//! GUI, and daemon all speak this protocol over the Unix socket.

pub mod config;
pub mod frame;
pub mod ipc;
pub mod newtypes;

pub use config::{
    default_config_path, AudioConfig, CaptureConfig, Config, ConfigError, Container, ExportCodec,
    ExportConfig, Quality,
};
pub use frame::{read_frame, write_frame, MAX_FRAME};
pub use ipc::{Command, Event, ProtocolError};
pub use newtypes::{BufferSeconds, ClipDuration, MonitorId};
