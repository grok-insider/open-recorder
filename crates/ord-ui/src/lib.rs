//! open-recorder clip library GUI.
//!
//! The pure models ([`library`] discover/sort, [`timeline`] trim math,
//! [`settings`] config editing, [`pace`] player demux pacing) are always
//! available and tested. The egui views (`app`, `editor`, `settings_view`,
//! behind the `gui` feature) render from them through the [`theme`] design
//! system.

pub mod format;
pub mod library;
pub mod pace;
pub mod settings;
pub mod timeline;

pub use library::{parse_clip, scan_dir, sort_newest_first, Clip};
pub use settings::{ApplyTier, SettingsModel};

#[cfg(feature = "gui")]
pub mod app;
#[cfg(feature = "gui")]
pub mod diag;
#[cfg(feature = "gui")]
pub mod editor;
#[cfg(feature = "gui")]
pub mod fonts;
#[cfg(feature = "gui")]
pub mod glvideo;
#[cfg(feature = "gui")]
pub mod meta;
#[cfg(feature = "gui")]
pub mod player;
#[cfg(feature = "gui")]
pub mod settings_view;
#[cfg(feature = "gui")]
pub mod theme;
#[cfg(feature = "gui")]
pub mod tuning;
