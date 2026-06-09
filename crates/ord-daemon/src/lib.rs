//! open-recorder daemon library: the command [`Handler`] and the Unix-socket
//! [`server`] loop. Split from `main.rs` so the handler and socket protocol are
//! integration-testable without spawning the real binary.

pub mod gamedetect;
pub mod handler;
pub mod server;

pub use gamedetect::{clip_stem, detect_foreground};
pub use handler::Handler;
pub use server::{serve, socket_path, ClipWriter, ServerError};
