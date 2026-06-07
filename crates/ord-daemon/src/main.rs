//! `ordd` — the open-recorder background daemon.
//!
//! Owns the capture engine + replay buffer and serves the control socket. With
//! the `waycap` feature it records for real via NVENC; without it (dev/CI) it
//! runs the mock backend so the socket and control flow can be exercised.

use std::path::PathBuf;

use ord_core::{Engine, PreparedClip};
use ord_daemon::handler::ClipWriter;
use ord_daemon::{serve, server::bind, socket_path, Handler};

const BUFFER_SECONDS: u32 = 60;
const FPS: u32 = 60;

fn clips_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join("Videos/open-recorder")
}

fn clip_filename() -> PathBuf {
    // <game-or-clip>-<epoch>.mkv: sortable, unique, and labelled by the
    // foreground app when detectable.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let game = ord_daemon::detect_foreground();
    let stem = ord_daemon::clip_stem(game.as_deref());
    clips_dir().join(format!("{stem}-{secs}.mkv"))
}

#[cfg(feature = "mux")]
fn make_writer() -> ClipWriter {
    Box::new(|clip: &PreparedClip| {
        std::fs::create_dir_all(clips_dir()).map_err(|e| e.to_string())?;
        let path = clip_filename();
        ord_core::write_clip(clip, &path).map_err(|e| e.to_string())?;
        Ok(path)
    })
}

#[cfg(not(feature = "mux"))]
fn make_writer() -> ClipWriter {
    // Without the muxer the daemon still runs (dev mode); saves report where a
    // clip *would* go but write nothing.
    Box::new(|_clip: &PreparedClip| {
        eprintln!("ordd: built without `mux`; clip not written");
        Ok(clip_filename())
    })
}

#[cfg(feature = "waycap")]
fn make_engine() -> Engine<ord_core::waycap_backend::WaycapBackend> {
    use ord_core::waycap_backend::{Quality, WaycapBackend};
    Engine::new(WaycapBackend::new(Quality::High, FPS), BUFFER_SECONDS)
}

#[cfg(not(feature = "waycap"))]
fn make_engine() -> Engine<ord_core::MockBackend> {
    // Dev daemon: a long mock capture so the socket/CLI can be exercised.
    Engine::new(
        ord_core::MockBackend::new(FPS, FPS * BUFFER_SECONDS, FPS),
        BUFFER_SECONDS,
    )
}

fn main() {
    let path = socket_path();

    let mut engine = make_engine();
    if let Err(e) = engine.start() {
        eprintln!("ordd: failed to start capture: {e}");
        std::process::exit(1);
    }

    let handler = Handler::new(engine, make_writer());

    let listener = match bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ordd: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("ordd: listening on {}", path.display());
    if let Err(e) = serve(listener, handler) {
        eprintln!("ordd: server error: {e}");
        std::process::exit(1);
    }
}
