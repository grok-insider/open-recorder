//! `ordd` — the open-recorder background daemon.
//!
//! Owns the capture engine + replay buffer and serves the control socket. With
//! the `waycap` feature it records for real via NVENC; without it (dev/CI) it
//! runs the mock backend so the socket and control flow can be exercised.

use std::path::PathBuf;

use ord_common::config::Config;
use ord_core::{Engine, PreparedClip};
use ord_daemon::{serve, server::bind, socket_path, ClipWriter, Handler};

/// Load the user config, writing a default file on first run. `ord-common` is
/// I/O-free, so the file read/parse lives here in the binary.
fn load_config() -> Config {
    let path = ord_common::config::default_config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => match Config::from_toml_str(&s) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "ordd: invalid config at {} ({e}); using defaults",
                    path.display()
                );
                Config::default()
            }
        },
        Err(_) => {
            // No config yet: drop a default one so it is discoverable/editable.
            let c = Config::default();
            if let (Some(parent), Ok(toml)) = (path.parent(), c.to_toml_string()) {
                let _ = std::fs::create_dir_all(parent);
                let _ = std::fs::write(&path, toml);
            }
            c
        }
    }
}

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

/// Map the on-disk quality enum onto the waycap backend's preset.
#[cfg(feature = "waycap")]
fn map_quality(q: ord_common::config::Quality) -> ord_core::waycap_backend::Quality {
    use ord_common::config::Quality as C;
    use ord_core::waycap_backend::Quality as W;
    match q {
        C::Low => W::Low,
        C::Medium => W::Medium,
        C::High => W::High,
        C::Ultra => W::Ultra,
    }
}

#[cfg(feature = "waycap")]
fn make_engine(config: &Config) -> Engine<ord_core::waycap_backend::WaycapBackend> {
    use ord_core::waycap_backend::WaycapBackend;
    // desktop and mic are mixed into one Opus track on a shared PipeWire clock.
    // Enabling the mic implies audio; mic capture also includes desktop audio.
    let audio_any = config.audio.desktop || config.audio.mic;
    let backend = WaycapBackend::new(map_quality(config.capture.quality), config.capture.fps)
        .with_audio(audio_any)
        .with_mic(config.audio.mic)
        .with_restore_token_path(restore_token_path());
    Engine::new(backend, config.capture.buffer_seconds)
}

/// Where the XDG screencast restore token is cached, so the daemon skips the
/// "Select what to share" picker after the first authorized run.
#[cfg(feature = "waycap")]
fn restore_token_path() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".local/state")
        });
    base.join("open-recorder/portal-restore-token")
}

#[cfg(not(feature = "waycap"))]
fn make_engine(config: &Config) -> Engine<ord_core::MockBackend> {
    // Dev daemon: a long mock capture so the socket/CLI can be exercised.
    let fps = config.capture.fps;
    let buffer = config.capture.buffer_seconds;
    Engine::new(ord_core::MockBackend::new(fps, fps * buffer, fps), buffer)
}

fn main() {
    let path = socket_path();
    let config = load_config();

    let mut engine = make_engine(&config);
    if let Err(e) = engine.start() {
        eprintln!("ordd: failed to start capture: {e}");
        std::process::exit(1);
    }

    let handler = Handler::new(engine);

    let listener = match bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ordd: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("ordd: listening on {}", path.display());
    if let Err(e) = serve(listener, handler, make_writer()) {
        eprintln!("ordd: server error: {e}");
        std::process::exit(1);
    }
}
