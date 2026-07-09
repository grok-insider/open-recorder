//! open-recorder daemon library: the command [`Handler`] and the Unix-socket
//! [`server`] loop. Split from `main.rs` so the handler and socket protocol are
//! integration-testable without spawning the real binary.

pub mod gamedetect;
pub mod handler;
pub mod hook;
pub mod outputs;
pub mod server;
pub mod storage;
pub mod supervisor;

pub use gamedetect::{clip_stem, detect_foreground, foreground_is_game};
pub use handler::{Handler, RecordPath};
pub use hook::spawn_clip_hook;
pub use ord_common::socket_path;
pub use server::{serve, ClipWriter, ServerError};
