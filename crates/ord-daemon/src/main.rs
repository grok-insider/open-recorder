//! `ordd` — the open-recorder background daemon.
//!
//! Owns the capture engine + replay buffer and serves the control socket. With
//! the `waycap` feature it records for real via NVENC; without it (dev/CI) it
//! runs the mock backend so the socket and control flow can be exercised.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ord_common::config::ReplayStorage;
use ord_common::{lock_tolerant, Config};
use ord_core::{CaptureBackend, Engine, FrameStore, PreparedClip};
use ord_daemon::storage::{self, ClipKind};
use ord_daemon::{serve, server::bind, server::ServerCtx, socket_path, ClipWriter, Handler};

/// Build the replay store the config asks for, sized to the buffer and the
/// backend's pts time base. Disk falls back to RAM if the spill file can't be
/// created so capture still starts.
fn make_store(config: &Config, ticks_per_sec: i64) -> Box<dyn FrameStore> {
    let secs = config.capture.buffer_seconds.max(1);
    let ram = || {
        Box::new(ord_core::RingBuffer::with_time_base(secs, ticks_per_sec)) as Box<dyn FrameStore>
    };
    match config.capture.replay_storage {
        ReplayStorage::Ram => ram(),
        // Disk-backed replay is unix-only (DiskFrameStore uses positioned I/O);
        // elsewhere it transparently degrades to the RAM ring.
        #[cfg(unix)]
        ReplayStorage::Disk => match ord_core::DiskFrameStore::create(
            ord_core::disk_store::default_spill_path(),
            secs,
            ticks_per_sec,
        ) {
            Ok(store) => {
                tracing::info!("replay buffer: disk-backed spill");
                Box::new(store)
            }
            Err(e) => {
                tracing::warn!(error = %e, "disk replay store unavailable; using RAM");
                ram()
            }
        },
        #[cfg(not(unix))]
        ReplayStorage::Disk => {
            tracing::warn!("disk replay storage is unix-only; using the RAM ring");
            ram()
        }
    }
}

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
        None => default_clips_dir(),
    }
}

/// Default clips directory: the platform Videos dir + `open-recorder`
/// (`~/Videos` on Linux, `~/Movies` on macOS, the Videos known folder on
/// Windows), falling back to `<home>/Videos` and finally the temp dir.
fn default_clips_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Videos")))
        .unwrap_or_else(std::env::temp_dir)
        .join("open-recorder")
}

/// Resolve the directory for full-length recordings (`~` expanded). Defaults to
/// the clips directory when `storage.recordings_dir` is unset, so simultaneous
/// replay + recording (gpu-screen-recorder's `-ro`) can land in their own
/// folder without colliding with replay clips.
fn recordings_dir(cfg: &Config) -> PathBuf {
    match cfg.storage.recordings_dir.as_deref() {
        Some(dir) => ord_daemon::hook::expand_home(dir),
        None => clips_dir(cfg),
    }
}

/// Install a SIGTERM/SIGINT handler that removes the control socket and exits
/// cleanly. systemd sends SIGTERM when the user service stops; without this the
/// process is killed mid-accept and leaves the socket file behind (the next start
/// recovers via `bind`, but a deterministic shutdown is cleaner). The handler runs
/// on its own thread — not in async-signal context — so the cleanup is safe.
#[cfg(unix)]
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

/// Off-unix there is no signal-hook; a stale rendezvous file is recovered by
/// `bind` on the next start, so shutdown cleanup is a no-op here.
#[cfg(not(unix))]
fn install_signal_handler(_socket: PathBuf) {}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Make `path` collision-free: the default template has 1-second granularity,
/// so a mark auto-save racing a manual save in the same second would silently
/// overwrite. Appends `-1`, `-2`, … to the stem until the name is unused.
fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path.file_stem().unwrap_or_default().to_os_string();
    let ext = path.extension().map(|e| e.to_os_string());
    for n in 1u32.. {
        let mut name = stem.clone();
        name.push(format!("-{n}"));
        let mut candidate = path.with_file_name(name);
        if let Some(ext) = &ext {
            candidate.set_extension(ext);
        }
        if !candidate.exists() {
            return candidate;
        }
    }
    path
}

/// Resolve a new output path from the storage template, creating parent dirs.
/// Recordings go to the (optionally separate) recordings directory; replay clips
/// go to the clips directory.
fn output_path(cfg: &Config, kind: ClipKind) -> PathBuf {
    let dir = match kind {
        ClipKind::Recording => recordings_dir(cfg),
        ClipKind::Clip => clips_dir(cfg),
    };
    templated_path(cfg, kind, dir, "mkv")
}

/// Resolve a screenshot output path (`.png`) in the clips directory.
#[cfg(feature = "mux")]
fn screenshot_path(cfg: &Config) -> PathBuf {
    templated_path(cfg, ClipKind::Clip, clips_dir(cfg), "png")
}

/// Shared stem/render/mkdir logic behind [`output_path`] / [`screenshot_path`].
fn templated_path(cfg: &Config, kind: ClipKind, dir: PathBuf, ext: &str) -> PathBuf {
    let game = ord_daemon::detect_foreground();
    let stem = ord_daemon::clip_stem(game.as_deref());
    let name = storage::render_name(&cfg.storage.template, Some(&stem), kind, epoch_secs());
    let path = dir.join(name).with_extension(ext);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    unique_path(path)
}

/// Decode the newest GOP to a PNG: mux the GOP to a temp file, then have ffmpeg
/// extract its last frame. Subprocess ffmpeg (like `ord-export`) keeps this
/// robust without a hand-rolled decoder.
#[cfg(feature = "mux")]
fn write_screenshot(
    frames: &[ord_core::EncodedFrame],
    params: ord_core::StreamParams,
    png: &std::path::Path,
) -> Result<(), String> {
    let tmp = png.with_extension("gop.mkv");
    let clip = PreparedClip {
        frames: frames.to_vec(),
        audio: Vec::new(),
        params,
        audio_params: None,
        chapters: Vec::new(),
    };
    ord_core::write_clip(&clip, &tmp).map_err(|e| e.to_string())?;
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-sseof", "-0.2", "-i"])
        .arg(&tmp)
        .args(["-update", "1", "-frames:v", "1"])
        .arg(png)
        .status();
    let _ = std::fs::remove_file(&tmp);
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("ffmpeg screenshot failed: {s}")),
        Err(e) => Err(format!("ffmpeg not available: {e}")),
    }
}

/// Build the screenshot writer (reads live config). With `mux` it decodes via
/// ffmpeg; without it, screenshots report a clear error.
#[cfg(feature = "mux")]
fn make_shot_writer(config: Arc<Mutex<Config>>) -> ord_daemon::server::ShotWriter {
    Box::new(move |frames, params| {
        let cfg = lock_tolerant(&config).clone();
        let path = screenshot_path(&cfg);
        write_screenshot(frames, params, &path)?;
        Ok(path)
    })
}

#[cfg(not(feature = "mux"))]
fn make_shot_writer(_config: Arc<Mutex<Config>>) -> ord_daemon::server::ShotWriter {
    Box::new(|_frames, _params| Err("screenshots require the `mux` build".into()))
}

/// Build the recording-path provider for the handler (reads live config).
fn make_record_path(config: Arc<Mutex<Config>>) -> ord_daemon::RecordPath {
    Box::new(move || {
        let cfg = lock_tolerant(&config).clone();
        output_path(&cfg, ClipKind::Recording)
    })
}

/// Files younger than this are never pruned: a full-length recording that is
/// still being written has a fresh mtime, and deleting it out from under the
/// muxer corrupts it.
const PRUNE_GRACE_SECS: u64 = 300;

/// Apply the storage prune policy after a save (off the handler lock; deleting
/// a few files is cheap but still doesn't belong on the capture path). Covers
/// both the clips and the (optionally separate) recordings directory under one
/// size budget.
fn prune_library(cfg: &Config) {
    if cfg.storage.max_gib.is_none() && cfg.storage.max_age_days.is_none() {
        return;
    }
    let clips = clips_dir(cfg);
    let recs = recordings_dir(cfg);
    let mut candidates = storage::prune_candidates(&clips);
    if recs != clips {
        candidates.extend(storage::prune_candidates(&recs));
    }
    let now = epoch_secs();
    candidates.retain(|e| e.mtime_epoch.saturating_add(PRUNE_GRACE_SECS) < now);
    let doomed = storage::plan_prune(
        candidates,
        cfg.storage.max_gib,
        cfg.storage.max_age_days,
        now,
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
#[cfg(all(feature = "waycap", target_os = "linux"))]
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
#[cfg(all(feature = "waycap", target_os = "linux"))]
fn map_codec(c: ord_common::config::CaptureCodec) -> ord_core::Codec {
    use ord_common::config::CaptureCodec as C;
    match c {
        C::H264 => ord_core::Codec::H264,
        C::Hevc => ord_core::Codec::Hevc,
        C::Av1 => ord_core::Codec::Av1,
    }
}

#[cfg(all(feature = "waycap", target_os = "linux"))]
fn make_engine(
    config: &Config,
) -> Engine<ord_core::waycap_backend::WaycapBackend, Box<dyn FrameStore>> {
    use ord_core::waycap_backend::WaycapBackend;
    // desktop and mic are mixed into one Opus track on a shared PipeWire clock.
    // Enabling the mic implies audio; mic capture also includes desktop audio.
    // Derive the (single-track) waycap audio flags from the effective track
    // model. The waycap path still mixes into one Opus track; the multi-track /
    // per-app subsystem (ord-core PipeWire capture) is the follow-on that
    // consumes the full `effective_tracks()` plan.
    let tracks = config.audio.effective_tracks();
    let audio_any = config.audio.any();
    let mic = tracks.iter().any(|t| {
        t.sources
            .iter()
            .any(|s| matches!(s, ord_common::config::AudioSource::DefaultInput))
    });
    let mut backend = WaycapBackend::new(map_quality(config.capture.quality), config.capture.fps)
        .with_codec(map_codec(config.capture.codec))
        .with_bitrate_kbps(config.capture.bitrate_kbps)
        .with_keyframe_interval_ms(config.capture.keyframe_interval_ms)
        .with_framerate_mode(config.capture.framerate_mode)
        .with_color_range(config.capture.color_range)
        .with_tune(config.capture.tune)
        .with_target(config.capture.target.clone())
        .with_hdr(config.capture.hdr)
        .with_audio(audio_any)
        .with_mic(mic)
        .with_restore_token_path(restore_token_path());
    if let Some(res) = config.capture.resolution {
        backend = backend.with_dimensions(res.width, res.height);
    }
    let ticks = backend.params().time_base_den;
    Engine::with_store(
        backend,
        make_store(config, ticks),
        config.capture.buffer_seconds,
    )
}

/// Where the XDG screencast restore token is cached, so the daemon skips the
/// "Select what to share" picker after the first authorized run.
#[cfg(all(feature = "waycap", target_os = "linux"))]
fn restore_token_path() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("open-recorder/portal-restore-token")
}

#[cfg(not(all(feature = "waycap", target_os = "linux")))]
fn make_engine(config: &Config) -> Engine<ord_core::MockBackend, Box<dyn FrameStore>> {
    // Dev daemon (and every non-Linux build): a long mock capture so the
    // socket/CLI can be exercised. Honors the configured replay storage (RAM or
    // disk) so that path is exercisable without a GPU.
    let fps = config.capture.fps;
    let buffer = config.capture.buffer_seconds;
    let backend = ord_core::MockBackend::new(fps, fps * buffer, fps);
    let ticks = backend.params().time_base_den;
    Engine::with_store(backend, make_store(config, ticks), buffer)
}

/// Persist the overrides document atomically (write a sibling temp file, then
/// rename over the target — a crash mid-write can never leave truncated TOML);
/// an empty diff removes the file so the base config is authoritative again.
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
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, contents).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }
}

fn main() {
    // `ordd --version` (and `-V`) report and exit before any setup, so version
    // checks work even when a session/socket isn't available.
    if std::env::args()
        .nth(1)
        .is_some_and(|a| a == "--version" || a == "-V")
    {
        println!("ordd {}", ord_common::version::long());
        return;
    }

    init_tracing();
    let path = socket_path();
    let (config, base) = load_config();

    // The control socket binds BEFORE capture starts: the daemon must always
    // be reachable (`ord status`, the settings UI) even while the screen-share
    // portal is slow, not ready yet at session login, or hung. The capture
    // supervisor inside `serve` arms the buffer asynchronously (with retries);
    // only a failure to bind is fatal — a daemon that can't serve is useless.
    let listener = match bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "failed to bind control socket");
            std::process::exit(1);
        }
    };
    install_signal_handler(path.clone());

    let shared_config = Arc::new(Mutex::new(config));
    let engine = {
        let cfg = lock_tolerant(&shared_config).clone();
        make_engine(&cfg)
    };
    let handler = Handler::new(engine, make_record_path(Arc::clone(&shared_config)));

    let ctx = ServerCtx {
        config: Arc::clone(&shared_config),
        base,
        engine_factory: Box::new(make_engine),
        write_overrides: Box::new(write_overrides_file),
        shot_writer: Arc::new(Mutex::new(make_shot_writer(Arc::clone(&shared_config)))),
        // No frames for 5 s while the buffer is armed -> restart capture
        // (suspend/resume kills NVENC; monitor changes end the portal session).
        watchdog: Some(std::time::Duration::from_secs(5)),
        game_probe: Box::new(ord_daemon::foreground_is_game),
        // The portal is often not ready right at session login: retry the
        // initial arm every 5 s for ~1 min (non-interactive via the restore
        // token), then stay reachable-but-degraded until armed manually or by
        // auto-arm.
        capture_retry_attempts: 12,
        capture_retry_delay: std::time::Duration::from_secs(5),
        arm_wait: std::time::Duration::from_secs(15),
    };

    tracing::info!(socket = %path.display(), "ordd listening");
    if let Err(e) = serve(listener, handler, make_writer(shared_config), ctx) {
        tracing::error!(error = %e, "server error");
        std::process::exit(1);
    }
}
