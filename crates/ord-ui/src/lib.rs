//! open-recorder clip library GUI.
//!
//! The pure [`library`] model (discover + sort clips) is always available and
//! tested. The egui view (`app`, behind the `gui` feature) renders from it.

pub mod library;

pub use library::{parse_clip, scan_dir, sort_newest_first, Clip};

#[cfg(feature = "gui")]
pub mod app;
