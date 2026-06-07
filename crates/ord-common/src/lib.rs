//! Shared types and the IPC wire protocol for open-recorder.
//!
//! This crate has no I/O. It defines the domain newtypes, the daemon control
//! protocol (`Command`/`Event`), and their bincode (de)serialization. The CLI,
//! GUI, and daemon all speak this protocol over the Unix socket.

pub mod ipc;
pub mod newtypes;

pub use ipc::{Command, Event, ProtocolError};
pub use newtypes::{BufferSeconds, ClipDuration, MonitorId};
