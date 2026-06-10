//! `ordd` — the open-recorder background daemon.
//!
//! Owns the capture engine + replay buffer and serves the control socket. With
//! the `waycap` feature it records for real via NVENC; without it (dev/CI) it
//! runs the mock backend so the socket and control flow can be exercised.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ord_common::{lock_tolerant, Config};
use ord_core::{Engine, PreparedClip};
use ord_daemon::storage::{self, ClipKind};
use ord_daemon::{serve, server::bind, server::ServerCtx, socket_path, ClipWriter, Handler};

/// Initialize `tracing` with an `RUST_LOG` env filter (default `info`), so the
/// daemon emits levelled, filterable logs to the journal instead of bare prints.
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

/// Load the layered user config: the base file (written with defaults on first
/// run so it is discoverable) plus the daemon-owned runtime overrides. Returns
/// `(effective, base)` — both are needed so settings UIs can show which fields
/// carry an override. `ord-common` is I/O-free, so the file reads live here.
fn load_config() -> (Config, Config) {
    let base_path = ord_common::config::default_config_path();
    let base_text = match std::fs::read_to_string(&base_path) {
        Ok(s) => s,
        Err(_) => {
            // No config yet: drop a default one so it is discoverable/editable.
            let c = Config::default();
            if let (Some(parent), Ok(toml)) = (base_path.parent(), c.to_toml_string()) {
                let _ = std::fs::create_dir_all(parent);
                let _ = std::fs::write(&base_path, toml);
            }
            String::new()
        }
    };
    let base = match Config::from_toml_str(&base_text) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(path = %base_path.display(), error = %e, "invalid config; using defaults");
            Config::default()
        }
    };

    let over_text = std::fs::read_to_string(ord_common::overrides_path()).unwrap_or_default();
    let effective = match Config::from_layers(&base_text, &over_text) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "invalid overrides; using base config only");
            base.clone()
        }
    };
    (effective, base)
}

/// Resolve the clips directory from config (`~` expanded), defaulting to
/// `~/Videos/open-recorder`.
fn clips_dir(cfg: &Config) -> PathBuf {
    match cfg.storage.clips_dir.as_deref() {
        Some(dir) => ord_daemon::hook::expand_home(dir),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join("Videos/open-recorder")
        }
    }
}

/// Install a SIGTERM/SIGINT handler that removes the control socket and exits
/// cleanly. systemd sends SIGTERM when the user service stops; without this the
/// process is killed mid-accept and leaves the socket file behind (the next start
/// recovers via `bind`, but a deterministic shutdown is cleaner). The handler runs
/// on its own thread — not in async-signal context — so the cleanup is safe.
fn install_signal_handler(socket: PathBuf) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;
    match Signals::new([SIGINT, SIGTERM]) {
        Ok(mut signals) => {
            std::thread::spawn(move || {
                if signals.forever().next().is_some() {
                    let _ = std::fs::remove_file(&socket);
                    // `_exit`, NOT `std::process::exit`: the latter runs C atexit
                    // handlers, which deadlock against waycap's EGL/PipeWire/CUDA
                    // background threads during teardown (observed as a 90 s hang
                    // then SIGKILL). This terminates immediately — like the default
                    // SIGTERM disposition — but only after the socket is removed.
                    signal_hook::low_level::exit(0);
                }
            });
        }
        Err(e) => tracing::warn!(error = %e, "could not install signal handler"),
    }
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve a new output path from the storage template, creating parent dirs.
fn output_path(cfg: &Config, kind: ClipKind) -> PathBuf {
    let game = ord_daemon::detect_foreground();
    let stem = ord_daemon::clip_stem(game.as_deref());
    let name = storage::render_name(&cfg.storage.template, Some(&stem), kind, epoch_secs());
    let path = clips_dir(cfg).join(name).with_extension("mkv");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    path
}

/// Build the recording-path provider for the handler (reads live config).
fn make_record_path(config: Arc<Mutex<Config>>) -> ord_daemon::RecordPath {
    Box::new(move || {
        let cfg = lock_tolerant(&config).clone();
        output_path(&cfg, ClipKind::Recording)
    })
}

/// Apply the storage prune policy after a save (off the handler lock; deleting
/// a few files is cheap but still doesn't belong on the capture path).
fn prune_library(cfg: &Config) {
    if cfg.storage.max_gib.is_none() && cfg.storage.max_age_days.is_none() {
        return;
    }
    let dir = clips_dir(cfg);
    let doomed = storage::plan_prune(
        storage::prune_candidates(&dir),
        cfg.storage.max_gib,
        cfg.storage.max_age_days,
        epoch_secs(),
    );
    for path in doomed {
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!(clip = %path.display(), "pruned by storage policy"),
            Err(e) => tracing::warn!(clip = %path.display(), error = %e, "prune failed"),
        }
    }
}

#[cfg(feature = "mux")]
fn make_writer(config: Arc<Mutex<Config>>) -> ClipWriter {
    Box::new(move |clip: &PreparedClip| {
        let cfg = lock_tolerant(&config).clone();
        let path = output_path(&cfg, ClipKind::Clip);
        ord_core::write_clip(clip, &path).map_err(|e| e.to_string())?;
        // Verify the container before declaring success: catches the classic
        // "saved an empty/corrupt file" failure mode at the moment it happens
        // instead of when the user opens the clip later.
        if let Err(e) = ord_core::verify_clip(&path) {
            return Err(format!(
                "clip written to {} but failed verification: {e}",
                path.display()
            ));
        }
        // The hook runs detached AFTER a verified write, still off the handler
        // lock, so it can never stall capture or fail the save.
        if let Some(hook) = cfg.hooks.on_clip_saved.as_deref() {
            ord_daemon::spawn_clip_hook(hook, &path);
        }
        prune_library(&cfg);
        Ok(path)
    })
}

#[cfg(not(feature = "mux"))]
fn make_writer(config: Arc<Mutex<Config>>) -> ClipWriter {
    // Without the muxer the daemon still runs (dev mode); saves report where a
    // clip *would* go but write nothing (verification and hooks are skipped).
    Box::new(move |_clip: &PreparedClip| {
        tracing::warn!("built without `mux`; clip not written");
        let cfg = lock_tolerant(&config).clone();
        let path = output_path(&cfg, ClipKind::Clip);
        prune_library(&cfg);
        Ok(path)
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

/// Map the on-disk capture codec enum onto the engine's [`ord_core::Codec`].
#[cfg(feature = "waycap")]
fn map_codec(c: ord_common::config::CaptureCodec) -> ord_core::Codec {
    use ord_common::config::CaptureCodec as C;
    match c {
        C::H264 => ord_core::Codec::H264,
        C::Hevc => ord_core::Codec::Hevc,
        C::Av1 => ord_core::Codec::Av1,
    }
}

#[cfg(feature = "waycap")]
fn make_engine(config: &Config) -> Engine<ord_core::waycap_backend::WaycapBackend> {
    use ord_core::waycap_backend::WaycapBackend;
    // desktop and mic are mixed into one Opus track on a shared PipeWire clock.
    // Enabling the mic implies audio; mic capture also includes desktop audio.
    let audio_any = config.audio.desktop || config.audio.mic;
    let backend = WaycapBackend::new(map_quality(config.capture.quality), config.capture.fps)
        .with_codec(map_codec(config.capture.codec))
        .with_bitrate_kbps(config.capture.bitrate_kbps)
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

/// Persist the overrides document atomically-ish; an empty diff removes the
/// file so the base config is authoritative again.
fn write_overrides_file(contents: &str) -> Result<(), String> {
    let path = ord_common::overrides_path();
    if contents.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, contents).map_err(|e| e.to_string())
    }
}

fn main() {
    init_tracing();
    let path = socket_path();
    let (config, base) = load_config();

    let mut engine = make_engine(&config);
    if let Err(e) = engine.start() {
        tracing::error!(error = %e, "failed to start capture");
        std::process::exit(1);
    }

    let shared_config = Arc::new(Mutex::new(config));
    let handler = Handler::new(engine, make_record_path(Arc::clone(&shared_config)));

    let listener = match bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "failed to bind control socket");
            std::process::exit(1);
        }
    };

    install_signal_handler(path.clone());

    let ctx = ServerCtx {
        config: Arc::clone(&shared_config),
        base,
        engine_factory: Box::new(make_engine),
        write_overrides: Box::new(write_overrides_file),
        // No frames for 5 s while the buffer is armed -> restart capture
        // (suspend/resume kills NVENC; monitor changes end the portal session).
        watchdog: Some(std::time::Duration::from_secs(5)),
    };

    tracing::info!(socket = %path.display(), "ordd listening");
    if let Err(e) = serve(listener, handler, make_writer(shared_config), ctx) {
        tracing::error!(error = %e, "server error");
        std::process::exit(1);
    }
}
