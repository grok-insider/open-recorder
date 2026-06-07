//! open-recorder daemon library: the command [`Handler`] and the Unix-socket
//! [`server`] loop. Split from `main.rs` so the handler and socket protocol are
//! integration-testable without spawning the real binary.

pub mod handler;
pub mod server;

pub use handler::{ClipWriter, Handler};
pub use server::{serve, socket_path, ServerError};
