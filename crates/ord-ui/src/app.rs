//! egui clip-library window (behind the `gui` feature).
//!
//! Renders the [`library`](crate::library) model as a dark, card-based grid:
//! each clip shows an ffmpeg thumbnail, its label and metadata
//! (duration · resolution · size · relative time), and actions — Open, Export
//! (via [`ord_export`] presets), Reveal, and Delete. Metadata and thumbnails are
//! loaded off the UI thread so the window stays responsive.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use ord_common::{lock_tolerant, Command, Event};
use ord_export::{export_with, ExportSummary, Preset, Trim};

use crate::editor::{EditorAction, EditorState};
use crate::format::{human_duration, human_size, relative_time, resolution};
use crate::library::{changed_paths, filter_sort, scan_dir, Clip, SortOrder};
use crate::meta::{self, ClipMeta};
use crate::settings_view::{SettingsAction, SettingsExtra, SettingsView};
use crate::theme;

/// Adaptive card sizing for the library grid (computed per layout pass).
#[derive(Clone, Copy)]
struct CardMetrics {
    inner: f32,
    thumb_w: f32,
    thumb_h: f32,
}

/// Per-card render inputs (keeps `card` under clippy's arg limit).
struct CardArgs<'a> {
    clip: &'a Clip,
    now: u64,
    is_export: bool,
    idx: usize,
    metrics: CardMetrics,
}

/// Message from the background loader thread.
enum Loaded {
    Meta {
        path: PathBuf,
        meta: Option<ClipMeta>,
    },
    Thumb {
        path: PathBuf,
        image: egui::ColorImage,
    },
}

/// Result of a background export.
struct ExportMsg {
    clip: PathBuf,
    result: Result<ExportSummary, String>,
}

/// A live export's shared progress (0.0..=1.0) and cancel flag.
struct ExportJob {
    progress: Arc<Mutex<f32>>,
    cancel: Arc<AtomicBool>,
}

/// Latest daemon state shown in the header (polled over the control socket).
#[derive(Debug, Clone, Copy)]
struct DaemonInfo {
    buffer_enabled: bool,
    recording: bool,
    buffered_seconds: u32,
}

/// Per-clip async-loaded state.
#[derive(Default)]
struct ClipState {
    meta: Option<ClipMeta>,
    meta_loaded: bool,
    texture: Option<egui::TextureHandle>,
    thumb_tried: bool,
}

/// The clip-library application state.
pub struct LibraryApp {
    clips_dir: PathBuf,
    clips: Vec<Clip>,
    /// Finished exports (`<clips_dir>/exports`), shown in their own section.
    export_clips: Vec<Clip>,
    /// Last-seen mtime per discovered file, so a refresh only re-probes
    /// new/changed clips instead of sweeping the whole library.
    mtimes: HashMap<PathBuf, Option<SystemTime>>,
    /// Bumped on every (re)scan; part of the view-cache key.
    library_gen: u64,
    /// Cached `filter_sort` results (recomputing + cloning the whole clip
    /// vector every repaint is wasted work at 60 fps).
    view_clips: Vec<Clip>,
    view_exports: Vec<Clip>,
    view_key: Option<(String, SortOrder, u64)>,
    /// Keyboard-focused card: an index into the current view (library cards
    /// first, then exports).
    focused: Option<usize>,
    /// The focus just moved via the keyboard: scroll the card into view once.
    focus_scroll: bool,
    states: HashMap<PathBuf, ClipState>,
    loader_rx: Receiver<Loaded>,
    loader_tx: Sender<Loaded>,
    export_rx: Receiver<ExportMsg>,
    export_tx: Sender<ExportMsg>,
    exports: HashMap<PathBuf, ExportJob>,
    confirm_delete: Option<PathBuf>,
    status: Option<(String, Instant)>,
    styled: bool,
    /// The initial scan+load has been kicked off.
    scanned: bool,
    /// When `Some`, the trim editor is shown instead of the library grid.
    editor: Option<EditorState>,
    /// A library rescan (new clip saved / recording stopped) arrived while the
    /// editor was open: deferred until it closes, so the full ffprobe sweep
    /// doesn't compete with the preview decoder mid-playback.
    pending_refresh: bool,
    watchdog: crate::diag::Watchdog,
    /// One-shot: honor `ORD_OPEN=<clip>` / `ORD_SETTINGS` (debug/QA launch aids).
    auto_open_tried: bool,
    /// Window focus tracking, to pause + idle when hidden (e.g. special workspace
    /// toggled away). `visible` is shared with the loader thread so it doesn't
    /// drive repaints while hidden.
    was_focused: bool,
    visible: Arc<AtomicBool>,
    /// Library search query (case-insensitive substring of the clip name).
    query: String,
    /// Library sort order.
    sort: SortOrder,
    /// Latest daemon status (`None` = unreachable/offline), polled off-thread.
    daemon: Option<DaemonInfo>,
    daemon_rx: Receiver<Option<DaemonInfo>>,
    daemon_tx: Sender<Option<DaemonInfo>>,
    daemon_poll_started: bool,
    /// Replies from one-shot daemon commands (save/record/buffer/config).
    ctl_rx: Receiver<Result<Event, String>>,
    ctl_tx: Sender<Result<Event, String>>,
    /// Pushed daemon events (a persistent `Subscribe` connection): new clips
    /// appear in the library the moment a hotkey saves them, record/buffer
    /// state updates instantly instead of on the next 2 s poll.
    events_rx: Receiver<Event>,
    events_tx: Sender<Event>,
    events_started: bool,
    /// When `Some`, the settings page is shown instead of the library grid.
    settings: Option<SettingsView>,
}

impl LibraryApp {
    /// Build the app; the first `update` scans `clips_dir` before painting.
    pub fn new(clips_dir: PathBuf) -> Self {
        let (loader_tx, loader_rx) = channel();
        let (export_tx, export_rx) = channel();
        let (daemon_tx, daemon_rx) = channel();
        let (ctl_tx, ctl_rx) = channel();
        let (events_tx, events_rx) = channel();
        Self {
            clips_dir,
            clips: Vec::new(),
            export_clips: Vec::new(),
            mtimes: HashMap::new(),
            library_gen: 0,
            view_clips: Vec::new(),
            view_exports: Vec::new(),
            view_key: None,
            focused: None,
            focus_scroll: false,
            states: HashMap::new(),
            loader_rx,
            loader_tx,
            export_rx,
            export_tx,
            exports: HashMap::new(),
            confirm_delete: None,
            status: None,
            styled: false,
            scanned: false,
            editor: None,
            pending_refresh: false,
            watchdog: crate::diag::Watchdog::start(std::time::Duration::from_secs(4)),
            auto_open_tried: false,
            was_focused: true,
            visible: Arc::new(AtomicBool::new(true)),
            query: String::new(),
            sort: SortOrder::default(),
            daemon: None,
            daemon_rx,
            daemon_tx,
            daemon_poll_started: false,
            ctl_rx,
            ctl_tx,
            events_rx,
            events_tx,
            events_started: false,
            settings: None,
        }
    }

    /// Fire one daemon command on a worker thread; the reply (or the connect
    /// error) lands in the control channel and is routed on the next repaint.
    fn send_ctl(&self, cmd: Command, ctx: &egui::Context) {
        let tx = self.ctl_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = ord_common::connect(ord_common::socket_path())
                .map_err(|e| e.to_string())
                .and_then(|mut c| c.request(&cmd).map_err(|e| e.to_string()));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    /// Route replies from one-shot daemon commands: config snapshots feed the
    /// settings page, state changes become status-bar text.
    fn drain_ctl(&mut self, ctx: &egui::Context) {
        while let Ok(reply) = self.ctl_rx.try_recv() {
            match reply {
                Ok(Event::Config { effective, base }) => {
                    if let Some(s) = self.settings.as_mut() {
                        s.on_config(*effective, *base);
                    }
                }
                Ok(Event::Outputs { outputs }) => {
                    if let Some(s) = self.settings.as_mut() {
                        s.on_outputs(outputs);
                    }
                }
                Ok(Event::ClipSaved { path, duration }) => {
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or(path);
                    self.set_status(format!("Saved {}s → {name}", duration.get()));
                    self.request_refresh(ctx);
                }
                Ok(Event::BufferState { enabled }) => {
                    if let Some(d) = self.daemon.as_mut() {
                        d.buffer_enabled = enabled;
                    }
                    self.set_status(if enabled {
                        "Replay buffer armed"
                    } else {
                        "Replay buffer off"
                    });
                }
                Ok(Event::RecordState { recording, .. }) => {
                    if let Some(d) = self.daemon.as_mut() {
                        d.recording = recording;
                    }
                    if recording {
                        self.set_status("Recording started — writing until you press Stop");
                    } else {
                        self.set_status("Recording stopped — added to the library");
                        self.request_refresh(ctx);
                    }
                }
                Ok(Event::Error { message }) => {
                    if let Some(s) = self.settings.as_mut() {
                        if s.busy {
                            s.on_error(message);
                            continue;
                        }
                    }
                    self.set_status(format!("Daemon: {message}"));
                }
                Ok(_) => {}
                Err(e) => {
                    if let Some(s) = self.settings.as_mut() {
                        if s.busy || s.model.is_none() {
                            s.on_error(e);
                            continue;
                        }
                    }
                    self.set_status(format!("Daemon unreachable: {e}"));
                }
            }
        }
    }

    /// Keep a persistent `Subscribe` connection to the daemon so pushed events
    /// (clip saved via hotkey, record/buffer toggles from any client) update
    /// the UI immediately — no manual refresh. Reconnects when the daemon
    /// restarts.
    fn start_event_stream(&mut self, ctx: &egui::Context) {
        if self.events_started {
            return;
        }
        self.events_started = true;
        let tx = self.events_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || loop {
            let stream = ord_common::connect(ord_common::socket_path())
                .ok()
                .and_then(|c| c.subscribe().ok());
            if let Some(events) = stream {
                for ev in events {
                    if tx.send(ev).is_err() {
                        return; // app gone
                    }
                    ctx.request_repaint();
                }
            }
            // Daemon unreachable or disconnected (restart): retry shortly.
            std::thread::sleep(Duration::from_secs(1));
        });
    }

    /// Route pushed daemon events into the model. A `ClipSaved` (from any
    /// client — hotkey, CLI, auto-save-on-mark) refreshes the library, so new
    /// clips appear without touching anything.
    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.events_rx.try_recv() {
            match ev {
                Event::ClipSaved { path, duration } => {
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or(path);
                    self.set_status(format!("Saved {}s → {name}", duration.get()));
                    self.request_refresh(ctx);
                }
                Event::RecordState { recording, .. } => {
                    if let Some(d) = self.daemon.as_mut() {
                        d.recording = recording;
                    }
                    if recording {
                        self.set_status("Recording started — writing until you press Stop");
                    } else {
                        self.set_status("Recording stopped — added to the library");
                        // The finished recording is a new file on disk.
                        self.request_refresh(ctx);
                    }
                }
                Event::BufferState { enabled } => {
                    if let Some(d) = self.daemon.as_mut() {
                        d.buffer_enabled = enabled;
                    }
                }
                Event::Status {
                    buffer_enabled,
                    recording,
                    buffered_seconds,
                    ..
                } => {
                    // The initial snapshot right after subscribing.
                    self.daemon = Some(DaemonInfo {
                        buffer_enabled,
                        recording,
                        buffered_seconds,
                    });
                }
                Event::Marked { auto_saving } => {
                    self.set_status(if auto_saving {
                        "Marked — saving clip"
                    } else {
                        "Marked"
                    });
                }
                Event::CaptureRestarted => self.set_status("Capture restarted"),
                Event::ScreenshotSaved { path } => {
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or(path);
                    self.set_status(format!("Screenshot → {name}"));
                }
                // One-shot Config/Outputs replies drive the settings page via
                // drain_ctl; a pushed Config mid-edit must not clobber drafts.
                Event::Config { .. } | Event::Outputs { .. } | Event::Error { .. } => {}
            }
        }
    }

    /// Poll `ordd` for status every couple of seconds (skipped while the window
    /// is hidden), reporting into the daemon channel. Offline is a state, not an
    /// error: the header just shows the daemon as unreachable.
    fn start_daemon_poll(&mut self, ctx: &egui::Context) {
        if self.daemon_poll_started {
            return;
        }
        self.daemon_poll_started = true;
        let tx = self.daemon_tx.clone();
        let ctx = ctx.clone();
        let visible = Arc::clone(&self.visible);
        std::thread::spawn(move || loop {
            if visible.load(Ordering::Relaxed) {
                let info = ord_common::connect(ord_common::socket_path())
                    .ok()
                    .and_then(|mut c| c.request(&Command::Status).ok())
                    .and_then(|ev| match ev {
                        Event::Status {
                            buffer_enabled,
                            recording,
                            buffered_seconds,
                            ..
                        } => Some(DaemonInfo {
                            buffer_enabled,
                            recording,
                            buffered_seconds,
                        }),
                        _ => None,
                    });
                if tx.send(info).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            std::thread::sleep(Duration::from_secs(2));
        });
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// (Re)scan the library + exports and kick off background loading — but
    /// only for new/changed files: entries whose path+mtime are unchanged
    /// keep their probed metadata and thumbnail texture, and removed paths
    /// are dropped.
    fn refresh(&mut self, ctx: &egui::Context) {
        let clips = scan_dir(&self.clips_dir);
        let export_clips = scan_dir(&meta::exports_dir(&self.clips_dir));
        let fresh: Vec<(PathBuf, Option<SystemTime>)> = clips
            .iter()
            .chain(export_clips.iter())
            .map(|c| (c.path.clone(), file_mtime(&c.path)))
            .collect();
        let changed: HashSet<PathBuf> = changed_paths(&self.mtimes, &fresh).into_iter().collect();
        for path in &changed {
            self.states.remove(path);
        }
        self.mtimes = fresh.into_iter().collect();
        let live = &self.mtimes;
        self.states.retain(|path, _| live.contains_key(path));
        let to_load: Vec<Clip> = clips
            .iter()
            .chain(export_clips.iter())
            .filter(|c| changed.contains(&c.path))
            .cloned()
            .collect();
        self.clips = clips;
        self.export_clips = export_clips;
        self.library_gen = self.library_gen.wrapping_add(1);
        self.confirm_delete = None;
        if !to_load.is_empty() {
            self.start_loading(to_load, ctx);
        }
    }

    /// Rescan now — unless the editor is open, in which case defer until it
    /// closes: a refresh runs one ffprobe per clip plus thumbnail decodes, and
    /// that sweep competing with the preview decoder is audible/visible.
    fn request_refresh(&mut self, ctx: &egui::Context) {
        if self.editor.is_some() {
            self.pending_refresh = true;
        } else {
            self.refresh(ctx);
        }
    }

    /// Recompute the cached query/sort views only when their inputs (query,
    /// sort order, library generation) changed since the last repaint.
    fn rebuild_views(&mut self) {
        let dirty = match &self.view_key {
            Some((q, s, g)) => q != &self.query || *s != self.sort || *g != self.library_gen,
            None => true,
        };
        if !dirty {
            return;
        }
        self.view_clips = filter_sort(&self.clips, &self.query, self.sort);
        self.view_exports = filter_sort(&self.export_clips, &self.query, self.sort);
        self.view_key = Some((self.query.clone(), self.sort, self.library_gen));
        let total = self.view_clips.len() + self.view_exports.len();
        if self.focused.is_some_and(|i| i >= total) {
            self.focused = total.checked_sub(1);
        }
    }

    /// Keyboard navigation over the card grid: arrows move the focused card
    /// (grid-aware via `cols`), Enter opens it, Delete asks the usual
    /// confirmation. Inert while a widget (the search box) has focus.
    fn grid_keys(&mut self, ctx: &egui::Context, cols: usize, clips: &[Clip], exports: &[Clip]) {
        let total = clips.len() + exports.len();
        if total == 0 || ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let (left, right, up, down, enter, delete) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Delete),
            )
        });
        if left || right || up || down {
            let next = match self.focused {
                None => 0,
                Some(i) => {
                    let i = i.min(total - 1);
                    if left {
                        i.saturating_sub(1)
                    } else if right {
                        (i + 1).min(total - 1)
                    } else if up {
                        i.saturating_sub(cols)
                    } else {
                        (i + cols).min(total - 1)
                    }
                }
            };
            self.focused = Some(next);
            self.focus_scroll = true;
        }
        let Some(i) = self.focused.filter(|i| *i < total) else {
            return;
        };
        let (clip, is_export) = if i < clips.len() {
            (&clips[i], false)
        } else {
            (&exports[i - clips.len()], true)
        };
        if enter {
            if is_export {
                open_clip(&clip.path);
            } else {
                self.open_editor(clip, ctx);
            }
        }
        if delete {
            self.confirm_delete = Some(clip.path.clone());
        }
    }

    /// Spawn a loader thread for `clips` (metadata probe + thumbnail decode).
    fn start_loading(&mut self, clips: Vec<Clip>, ctx: &egui::Context) {
        let tx = self.loader_tx.clone();
        let ctx = ctx.clone();
        let visible = Arc::clone(&self.visible);
        std::thread::spawn(move || {
            // Only nudge a repaint when the window is visible; while hidden the
            // data is still queued and gets drained/shown on the next re-show, so
            // a hidden, loading library doesn't drive (blocking-on-hidden) renders.
            let repaint = |ctx: &egui::Context| {
                if visible.load(Ordering::Relaxed) {
                    ctx.request_repaint();
                }
            };
            for clip in clips {
                let meta = meta::load_meta(&clip.path);
                let _ = tx.send(Loaded::Meta {
                    path: clip.path.clone(),
                    meta,
                });
                repaint(&ctx);
                if let Some(thumb) = meta::ensure_thumbnail(&clip.path) {
                    if let Some(image) = decode_image(&thumb) {
                        let _ = tx.send(Loaded::Thumb {
                            path: clip.path.clone(),
                            image,
                        });
                        repaint(&ctx);
                    }
                }
            }
        });
    }

    fn drain_channels(&mut self, ctx: &egui::Context) {
        while let Ok(info) = self.daemon_rx.try_recv() {
            self.daemon = info;
        }
        while let Ok(msg) = self.loader_rx.try_recv() {
            match msg {
                Loaded::Meta { path, meta } => {
                    let st = self.states.entry(path).or_default();
                    st.meta = meta;
                    st.meta_loaded = true;
                }
                Loaded::Thumb { path, image } => {
                    let name = format!("thumb:{}", path.display());
                    let tex = ctx.load_texture(name, image, egui::TextureOptions::LINEAR);
                    let st = self.states.entry(path).or_default();
                    st.texture = Some(tex);
                    st.thumb_tried = true;
                }
            }
        }
        while let Ok(msg) = self.export_rx.try_recv() {
            self.exports.remove(&msg.clip);
            match msg.result {
                Ok(s) => {
                    let name = s
                        .output
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    self.set_status(format!("Exported → {name}  ({})", human_size(s.size_bytes)));
                    // The new file must appear in the Exports section.
                    self.request_refresh(ctx);
                }
                Err(e) if e == ord_export::ExportError::Cancelled.to_string() => {
                    self.set_status("Export cancelled");
                }
                Err(e) => self.set_status(format!("Export failed: {e}")),
            }
        }
    }

    fn start_export(&mut self, clip: &Clip, preset: Preset, ctx: &egui::Context) {
        self.run_export(
            &clip.path,
            &clip.stem,
            clip.label(),
            preset,
            None,
            None,
            false,
            1.0,
            false,
            ctx,
        );
    }

    /// Export `input` with `preset`, optionally trimmed/muted — or, when
    /// `segments` is set, the editor's kept pieces concatenated into one file.
    /// Runs off-thread and reports via the export channel; ignores a duplicate
    /// in-flight export. When `to_library` is true the file lands next to
    /// clips (not under `exports/`).
    #[allow(clippy::too_many_arguments)]
    fn run_export(
        &mut self,
        input: &Path,
        stem: &str,
        label: &str,
        preset: Preset,
        trim: Option<Trim>,
        segments: Option<Vec<Trim>>,
        mute: bool,
        speed: f32,
        to_library: bool,
        ctx: &egui::Context,
    ) {
        if self.exports.contains_key(input) {
            return;
        }
        let mut profile = preset.profile();
        profile.mute = mute;
        profile.speed = speed.clamp(0.25, 2.0) as f64;
        let ext = profile.output_extension();
        let preset_name = preset.slug();
        let suffix = if segments.is_some() {
            "-cut"
        } else if trim.is_some() {
            "-trim"
        } else if to_library {
            "-edit"
        } else {
            ""
        };
        let out = if to_library {
            self.clips_dir
                .join(format!("{stem}-{preset_name}{suffix}.{ext}"))
        } else {
            meta::exports_dir(&self.clips_dir).join(format!("{stem}-{preset_name}{suffix}.{ext}"))
        };
        let input = input.to_path_buf();
        let tx = self.export_tx.clone();
        let ctx = ctx.clone();

        let progress = Arc::new(Mutex::new(0.0f32));
        let cancel = Arc::new(AtomicBool::new(false));
        self.exports.insert(
            input.clone(),
            ExportJob {
                progress: Arc::clone(&progress),
                cancel: Arc::clone(&cancel),
            },
        );
        self.set_status(format!("Exporting {label} as {preset_name}…"));
        std::thread::spawn(move || {
            let mut on_progress = |p: f64| {
                *lock_tolerant(&progress) = p as f32;
                ctx.request_repaint();
            };
            let result = match &segments {
                Some(segs) => ord_export::export_segments_with(
                    &input,
                    &out,
                    &profile,
                    segs,
                    &mut on_progress,
                    &cancel,
                ),
                None => export_with(&input, &out, &profile, trim, &mut on_progress, &cancel),
            }
            .map_err(|e| e.to_string());
            let _ = tx.send(ExportMsg {
                clip: input,
                result,
            });
            ctx.request_repaint();
        });
    }

    fn delete_clip(&mut self, path: &Path, ctx: &egui::Context) {
        match std::fs::remove_file(path) {
            Ok(()) => {
                self.set_status("Clip deleted");
                self.refresh(ctx);
            }
            Err(e) => self.set_status(format!("Delete failed: {e}")),
        }
    }
}

fn decode_image(path: &Path) -> Option<egui::ColorImage> {
    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        img.as_raw(),
    ))
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Copy a clip onto the Wayland clipboard as a file (`text/uri-list` via
/// `wl-copy`), so it can be pasted straight into Discord/Slack/a file manager.
fn copy_clip_to_clipboard(path: &Path) -> bool {
    let uri = format!("file://{}", path.display());
    std::process::Command::new("wl-copy")
        .args(["-t", "text/uri-list"])
        .arg(&uri)
        .spawn()
        .is_ok()
}

impl eframe::App for LibraryApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.watchdog.beat("update");
        if !self.styled {
            crate::fonts::install(ctx);
            theme::apply(ctx);
            self.styled = true;
        }
        self.drain_channels(ctx);
        self.drain_ctl(ctx);
        self.drain_events(ctx);
        self.start_event_stream(ctx);

        // Pause + idle when our window isn't focused (e.g. its Hyprland special
        // workspace was toggled away). Pausing stops audio playing into a hidden
        // window; going reactive (no repaint requests here) lets eframe answer
        // compositor pings so it can't be flagged "not responding". `visible`
        // also stops the loader thread from driving repaints while hidden.
        let focused = ctx.input(|i| i.focused);
        self.visible.store(focused, Ordering::Relaxed);
        // Keep the stall watchdog honest: only expect frames while focused, and
        // when focused tick at least once a second so a *paused* editor (which
        // legitimately stops painting) isn't misread as a hang. A true hang
        // (the update loop blocked past the threshold) still trips it.
        self.watchdog.set_active(focused);
        if focused {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if focused != self.was_focused {
            self.was_focused = focused;
            if !focused {
                if let Some(ed) = self.editor.as_mut() {
                    ed.pause_player();
                }
            } else {
                ctx.request_repaint(); // redraw once on re-show
            }
        }

        // Debug/QA launch aids (one-shot): ORD_OPEN=<path> → editor, or
        // ORD_SETTINGS → settings page (for wisp without pointer hit-testing).
        if !self.auto_open_tried {
            self.auto_open_tried = true;
            if crate::tuning::auto_settings() {
                self.settings = Some(SettingsView::new());
                self.send_ctl(Command::GetConfig, ctx);
                self.send_ctl(Command::ListOutputs, ctx);
            } else if let Some(path) = crate::tuning::auto_open() {
                let path = PathBuf::from(path);
                if path.is_file() {
                    let label = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if let Ok(ed) = EditorState::new(path, label, ctx) {
                        self.editor = Some(ed);
                    }
                }
            }
        }

        // Settings page takes over the whole window when open.
        if let Some(mut view) = self.settings.take() {
            self.watchdog.beat("settings");
            let (action, extra) = view.ui(ctx);
            if let Some(SettingsExtra::RefreshOutputs) = extra {
                self.send_ctl(Command::ListOutputs, ctx);
            }
            match action {
                SettingsAction::Back => {} // dropped
                SettingsAction::None => self.settings = Some(view),
                SettingsAction::Apply(config) => {
                    self.send_ctl(Command::SetConfig { config }, ctx);
                    self.settings = Some(view);
                }
            }
            return;
        }

        // Trim editor takes over the whole window when open.
        if let Some(ed) = self.editor.as_mut() {
            // Surface export progress inside the editor chrome.
            let prog = self
                .exports
                .get(ed.clip())
                .map(|j| *lock_tolerant(&j.progress));
            ed.set_export_progress(prog);
            self.watchdog.beat("editor");
            let wd = self.watchdog.clone();
            match ed.ui(ctx, &wd) {
                EditorAction::None => {}
                EditorAction::Back => self.editor = None,
                EditorAction::Export {
                    preset,
                    trim,
                    segments,
                    mute,
                    speed,
                } => {
                    let clip = ed.clip().clone();
                    let stem = clip
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "clip".to_string());
                    self.run_export(
                        &clip, &stem, &stem, preset, trim, segments, mute, speed, false, ctx,
                    );
                    // Keep the editor open so progress shows on the export bar.
                }
                EditorAction::SaveAsClip {
                    trim,
                    segments,
                    mute,
                    speed,
                } => {
                    let clip = ed.clip().clone();
                    let stem = clip
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "clip".to_string());
                    // Prefer stream-copy when possible; re-encode when cuts or
                    // non-1× speed force it.
                    let needs_reencode = segments.is_some() || (speed - 1.0).abs() > 0.01;
                    let preset = if needs_reencode {
                        Preset::HighQuality
                    } else {
                        Preset::Source
                    };
                    self.run_export(
                        &clip, &stem, &stem, preset, trim, segments, mute, speed,
                        true, // land in the main clips library
                        ctx,
                    );
                }
            }
            // Clips saved while editing were deferred; rescan now it's closed.
            if self.editor.is_none() && self.pending_refresh {
                self.pending_refresh = false;
                self.refresh(ctx);
            }
            return;
        }

        self.watchdog.beat("library");
        if !self.scanned {
            self.scanned = true;
            self.refresh(ctx);
        }
        self.start_daemon_poll(ctx);
        self.rebuild_views();

        let now = now_epoch();
        let total_size: u64 = self
            .states
            .values()
            .filter_map(|s| s.meta.as_ref().map(|m| m.size_bytes))
            .sum();

        egui::TopBottomPanel::top("top")
            .frame(theme::chrome())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    theme::brand(ui);
                    ui.add_space(theme::SP_2);
                    self.daemon_badge(ui);
                    ui.add_space(theme::SP_3);

                    // Search-as-you-type over clip names; Esc clears; Ctrl+F
                    // focuses it from anywhere in the library.
                    let search = egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Search clips…")
                        .desired_width(180.0);
                    let resp = ui.add(search);
                    if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
                        resp.request_focus();
                    }
                    if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        self.query.clear();
                    }
                    if !self.query.is_empty() && ui.small_button("✕").clicked() {
                        self.query.clear();
                    }

                    egui::ComboBox::from_id_salt("sort")
                        .selected_text(self.sort.label())
                        .width(90.0)
                        .show_ui(ui, |ui| {
                            for order in SortOrder::ALL {
                                ui.selectable_value(&mut self.sort, order, order.label());
                            }
                        });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let settings_btn = ui
                            .add(egui::Button::new("Settings").min_size(egui::vec2(72.0, 0.0)))
                            .on_hover_text("Daemon configuration (applies live)");
                        // AccessKit: stable name for screen readers / wisp marks.
                        crate::a11y::button(&settings_btn, "Settings");
                        if settings_btn.clicked() {
                            self.settings = Some(SettingsView::new());
                            self.send_ctl(Command::GetConfig, ctx);
                            self.send_ctl(Command::ListOutputs, ctx);
                        }
                        ui.add_space(theme::SP_1);
                        self.daemon_controls(ui, ctx);
                        ui.add_space(theme::SP_2);
                        let summary = if total_size > 0 {
                            format!("{} clips · {}", self.clips.len(), human_size(total_size))
                        } else {
                            format!("{} clips", self.clips.len())
                        };
                        ui.label(
                            egui::RichText::new(summary)
                                .size(theme::TEXT_LABEL)
                                .color(theme::INK_3),
                        );
                    });
                });
            });

        if let Some((msg, at)) = self.status.clone() {
            if at.elapsed() < Duration::from_secs(6) {
                egui::TopBottomPanel::bottom("status")
                    .frame(theme::chrome())
                    .show(ctx, |ui| {
                        theme::status_dot(ui, theme::AI, &msg, "");
                    });
                if focused {
                    ctx.request_repaint_after(Duration::from_millis(500));
                }
            } else {
                self.status = None;
            }
        }

        let panel_frame =
            egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(16.0));
        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                if self.clips.is_empty() && self.export_clips.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(96.0);
                        ui.label(
                            egui::RichText::new("No clips yet")
                                .size(theme::TEXT_TITLE)
                                .strong()
                                .color(theme::INK),
                        );
                        ui.add_space(theme::SP_2);
                        ui.label(
                            egui::RichText::new(
                                "Press ALT+R in a game to save the last 30 seconds.",
                            )
                            .size(theme::TEXT_BODY)
                            .color(theme::INK_2),
                        );
                    });
                    return;
                }

                if self.view_clips.is_empty() && self.view_exports.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(96.0);
                        ui.label(
                            egui::RichText::new(format!("No clips match “{}”", self.query))
                                .size(theme::TEXT_BODY)
                                .color(theme::INK_2),
                        );
                    });
                    return;
                }
                // Card grid. `horizontal_wrapped` inside a vertical ScrollArea
                // sees unbounded width and never wraps — every clip lands in one
                // off-screen row. Instead, compute the column count from the
                // panel width (finite, captured here before the scroll area) and
                // lay out fixed rows of fixed-width cards: a real grid, no
                // reliance on egui's wrap inference.
                let avail = ui.available_width();
                let spacing = theme::SP_3;
                // Frame pad ≈ horizontal card margin + rounding chrome.
                let frame_pad = 2.0 * theme::SP_3 + 8.0;
                let (cols, card_inner) = crate::layout::library_grid(avail, spacing, frame_pad);
                let metrics = CardMetrics {
                    inner: card_inner,
                    thumb_w: card_inner,
                    thumb_h: crate::layout::thumb_height(card_inner),
                };
                // Owned snapshots of the cached views, so the card closures can
                // mutate `self`; restored below (no per-repaint clone).
                let clips = std::mem::take(&mut self.view_clips);
                let exports = std::mem::take(&mut self.view_exports);
                self.grid_keys(ctx, cols, &clips, &exports);
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        ui.spacing_mut().item_spacing = egui::vec2(spacing, spacing);
                        let mut idx = 0usize;
                        for row in clips.chunks(cols) {
                            ui.horizontal(|ui| {
                                for clip in row {
                                    self.card(
                                        ui,
                                        ctx,
                                        CardArgs {
                                            clip,
                                            now,
                                            is_export: false,
                                            idx,
                                            metrics,
                                        },
                                    );
                                    idx += 1;
                                }
                            });
                        }
                        if !exports.is_empty() {
                            theme::section(ui, "Exports");
                            for row in exports.chunks(cols) {
                                ui.horizontal(|ui| {
                                    for clip in row {
                                        self.card(
                                            ui,
                                            ctx,
                                            CardArgs {
                                                clip,
                                                now,
                                                is_export: true,
                                                idx,
                                                metrics,
                                            },
                                        );
                                        idx += 1;
                                    }
                                });
                            }
                        }
                        ui.add_space(8.0);
                    });
                self.focus_scroll = false;
                self.view_clips = clips;
                self.view_exports = exports;
            });
    }
}

impl LibraryApp {
    /// Live daemon state in the header: buffer armed (matcha) / recording
    /// (shu) / buffer off (grey) / daemon unreachable (dim).
    fn daemon_badge(&self, ui: &mut egui::Ui) {
        let (dot, text, hover) = match self.daemon {
            Some(d) if d.recording => (
                theme::SHU,
                "recording".to_string(),
                "ordd is writing a full-length recording".to_string(),
            ),
            Some(d) if d.buffer_enabled => (
                theme::OK,
                format!("buffer {}s", d.buffered_seconds),
                "Replay buffer armed — seconds currently held in RAM".to_string(),
            ),
            Some(_) => (
                theme::INK_3,
                "buffer off".to_string(),
                "ordd is running but the replay buffer is disabled".to_string(),
            ),
            None => (
                theme::INK_3,
                "daemon offline".to_string(),
                "Cannot reach ordd on its control socket".to_string(),
            ),
        };
        theme::status_dot(ui, dot, &text, &hover);
    }

    /// One-click daemon actions in the header (right-to-left layout): buffer
    /// toggle, Save ▾ (15/30/60 s — the "multiple buffer lengths" everyone
    /// asks OBS for), record toggle. Hidden while the daemon is unreachable.
    fn daemon_controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(d) = self.daemon else { return };

        let rec_label = if d.recording {
            "■ Stop rec"
        } else {
            "● Record"
        };
        let rec = if d.recording {
            theme::danger_button(ui, rec_label)
        } else {
            ui.button(rec_label)
        };
        if rec
            .on_hover_text(
                "Toggle a full-length recording that writes to disk continuously \
                 until stopped (separate from the replay buffer). The file shows \
                 up in the library when you stop.",
            )
            .clicked()
        {
            self.send_ctl(Command::ToggleRecord, ctx);
            self.set_status(if d.recording {
                "Stopping recording…"
            } else {
                "Starting recording…"
            });
        }

        if d.buffer_enabled {
            ui.menu_button("Save ▾", |ui| {
                ui.label(
                    egui::RichText::new(format!("buffered: {} s", d.buffered_seconds))
                        .size(theme::TEXT_MICRO)
                        .color(theme::INK_3),
                );
                ui.separator();
                for secs in [15u32, 30, 60, 120] {
                    if ui
                        .button(format!("Save last {secs} s"))
                        .on_hover_text(if secs > d.buffered_seconds.max(1) {
                            format!(
                                "Only ~{} s are buffered right now — the clip will be that long",
                                d.buffered_seconds
                            )
                        } else {
                            format!("Write the last {secs} s of the buffer to a clip")
                        })
                        .clicked()
                    {
                        if let Some(duration) = ord_common::ClipDuration::new(secs) {
                            self.send_ctl(Command::SaveLast { duration }, ctx);
                            self.set_status(format!("Saving the last {secs} s…"));
                        }
                        ui.close_menu();
                    }
                }
            });
        }

        let buf_label = if d.buffer_enabled {
            "Buffer: on"
        } else {
            "Buffer: off"
        };
        if ui
            .button(buf_label)
            .on_hover_text(if d.buffer_enabled {
                "Replay buffer is armed — click to disable it (stops capturing; \
                 `ord save` will have nothing to save)"
            } else {
                "Replay buffer is off — click to arm it"
            })
            .clicked()
        {
            self.send_ctl(
                Command::SetBuffer {
                    enabled: !d.buffer_enabled,
                },
                ctx,
            );
            self.set_status(if d.buffer_enabled {
                "Disabling the replay buffer…"
            } else {
                "Arming the replay buffer…"
            });
        }
    }

    fn card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, args: CardArgs<'_>) {
        let CardArgs {
            clip,
            now,
            is_export,
            idx,
            metrics,
        } = args;
        let focused = self.focused == Some(idx);
        let frame = if focused {
            theme::card_focused()
        } else {
            theme::card()
        };
        let resp = frame.show(ui, |ui| {
            ui.set_width(metrics.inner);
            ui.vertical(|ui| {
                if self.thumbnail(ui, clip, is_export, metrics) {
                    if is_export {
                        open_clip(&clip.path);
                    } else {
                        self.open_editor(clip, ctx);
                    }
                }
                ui.add_space(theme::SP_2);

                // Title row: name left, relative age right (one line of *ma*
                // instead of two stacked metadata lines).
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(clip.label())
                            .strong()
                            .size(14.5)
                            .color(theme::INK),
                    );
                    if let Some(epoch) = clip.epoch {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(relative_time(epoch, now))
                                    .color(theme::INK_3)
                                    .size(theme::TEXT_MICRO),
                            );
                        });
                    }
                });

                let st = self.states.get(&clip.path);
                let meta_line = match st.and_then(|s| s.meta.as_ref()) {
                    Some(m) => format!(
                        "{}   ·   {}   ·   {}",
                        human_duration(m.duration_secs),
                        resolution(m.width, m.height),
                        human_size(m.size_bytes),
                    ),
                    None if st.map(|s| s.meta_loaded).unwrap_or(false) => "—".to_string(),
                    None => "loading…".to_string(),
                };
                ui.label(
                    egui::RichText::new(meta_line)
                        .color(theme::INK_2)
                        .size(theme::TEXT_LABEL),
                );

                ui.add_space(theme::SP_2);
                self.actions(ui, clip, ctx, is_export);
            });
        });
        if focused && self.focus_scroll {
            resp.response.scroll_to_me(None);
        }
    }

    /// Render the thumbnail; returns true if it was clicked (opens the editor,
    /// or plays the file for an export).
    fn thumbnail(
        &self,
        ui: &mut egui::Ui,
        clip: &Clip,
        is_export: bool,
        metrics: CardMetrics,
    ) -> bool {
        let size = egui::vec2(metrics.thumb_w, metrics.thumb_h);
        let hover = if is_export { "Play" } else { "Edit / trim" };
        match self.states.get(&clip.path).and_then(|s| s.texture.as_ref()) {
            Some(tex) => ui
                .add(
                    egui::Image::new(tex)
                        .fit_to_exact_size(size)
                        .rounding(8.0)
                        .sense(egui::Sense::click()),
                )
                .on_hover_text(hover)
                .clicked(),
            None => {
                let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                ui.painter()
                    .rect_filled(rect, theme::RADIUS, theme::THUMB_BG);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "▶",
                    egui::FontId::proportional(26.0),
                    theme::INK_3,
                );
                resp.on_hover_text(hover).clicked()
            }
        }
    }

    fn open_editor(&mut self, clip: &Clip, ctx: &egui::Context) {
        match EditorState::new(clip.path.clone(), clip.label().to_string(), ctx) {
            Ok(ed) => self.editor = Some(ed),
            Err(e) => self.set_status(format!("Can't open editor: {e}")),
        }
    }

    fn actions(&mut self, ui: &mut egui::Ui, clip: &Clip, ctx: &egui::Context, is_export: bool) {
        // One quiet row: primary actions inline, the rest behind "⋯". A delete
        // confirmation temporarily replaces the row (no accidental deletes,
        // no modal). Exports are finished artifacts: they play, copy, reveal,
        // and delete, but re-editing/re-exporting an export is not offered.
        let confirming = self.confirm_delete.as_deref() == Some(clip.path.as_path());
        if confirming {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Delete this clip?")
                        .size(theme::TEXT_LABEL)
                        .color(theme::INK_2),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::danger_button(ui, "Delete").clicked() {
                        let path = clip.path.clone();
                        self.confirm_delete = None;
                        self.delete_clip(&path, ctx);
                    }
                    if ui.button("Keep").clicked() {
                        self.confirm_delete = None;
                    }
                });
            });
            return;
        }

        ui.horizontal(|ui| {
            if ui.button("Open").clicked() {
                open_clip(&clip.path);
            }
            if !is_export {
                if ui.button("Edit").clicked() {
                    self.open_editor(clip, ctx);
                }
                // Owned snapshot so the menu closure can still borrow `self` mutably.
                let job = self
                    .exports
                    .get(&clip.path)
                    .map(|j| (*lock_tolerant(&j.progress), Arc::clone(&j.cancel)));
                if let Some((prog, cancel)) = job {
                    ui.label(
                        egui::RichText::new(format!("Exporting… {}%", (prog * 100.0) as u32))
                            .size(theme::TEXT_LABEL)
                            .color(theme::AI),
                    );
                    if ui.button("Cancel").clicked() {
                        cancel.store(true, Ordering::Relaxed);
                    }
                } else {
                    ui.menu_button("Export", |ui| {
                        for preset in Preset::ALL {
                            if ui.button(preset.label()).clicked() {
                                self.start_export(clip, preset, ctx);
                                ui.close_menu();
                            }
                        }
                    });
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("⋯", |ui| {
                    if ui
                        .button("Copy as file")
                        .on_hover_text("Clipboard as text/uri-list — paste into Discord")
                        .clicked()
                    {
                        if copy_clip_to_clipboard(&clip.path) {
                            self.set_status("Copied — paste into Discord/your chat");
                        } else {
                            self.set_status("wl-copy not found; cannot copy");
                        }
                        ui.close_menu();
                    }
                    if ui.button("Reveal in file manager").clicked() {
                        reveal(&clip.path);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui
                        .button(egui::RichText::new("Delete…").color(theme::SHU))
                        .clicked()
                    {
                        self.confirm_delete = Some(clip.path.clone());
                        ui.close_menu();
                    }
                });
            });
        });
    }
}

fn open_clip(path: &Path) {
    if std::process::Command::new("mpv").arg(path).spawn().is_err() {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

fn reveal(path: &Path) {
    let dir = path.parent().unwrap_or(path);
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

/// Launch the library window.
pub fn run(clips_dir: PathBuf) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        // Set the Wayland app_id (X11 WM_CLASS) so compositors can match the
        // window — e.g. a Hyprland rule that moves it to a `special:clips`
        // workspace. Without this the class is empty and rules never match.
        viewport: eframe::egui::ViewportBuilder::default()
            .with_app_id("open-recorder")
            .with_title("open-recorder")
            // Default open size suits 1440p/ultrawide desktops; the layout
            // grows with available width (see `layout::`). Min keeps the grid
            // usable on small windows.
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([480.0, 360.0]),
        // Disable vsync so eframe's swap_buffers never BLOCKS waiting for a frame
        // callback. On Wayland a hidden surface (e.g. this window's Hyprland
        // special workspace toggled away) stops getting frame callbacks, so a
        // blocking swap freezes the winit thread → it can't answer the xdg ping →
        // the compositor shows "Application Not Responding". DontWait avoids the
        // block. No tearing results: Hyprland composites windowed surfaces with
        // its own vsync, and we still self-cap repaints to ~60fps.
        vsync: false,
        ..Default::default()
    };
    eframe::run_native(
        "open-recorder",
        options,
        Box::new(move |_cc| Ok(Box::new(LibraryApp::new(clips_dir)))),
    )
}
