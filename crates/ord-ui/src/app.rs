//! egui clip-library window (behind the `gui` feature).
//!
//! Renders the [`library`](crate::library) model as a dark, card-based grid:
//! each clip shows an ffmpeg thumbnail, its label and metadata
//! (duration · resolution · size · relative time), and actions — Open, Export
//! (via [`ord_export`] presets), Reveal, and Delete. Metadata and thumbnails are
//! loaded off the UI thread so the window stays responsive.

use std::collections::HashMap;
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
use crate::library::{filter_sort, scan_dir, Clip, SortOrder};
use crate::meta::{self, ClipMeta};
use crate::settings_view::{SettingsAction, SettingsView};
use crate::theme;

const CARD_INNER_W: f32 = 300.0;
const THUMB_W: f32 = 300.0;
const THUMB_H: f32 = 169.0; // 16:9

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
    states: HashMap<PathBuf, ClipState>,
    loader_rx: Receiver<Loaded>,
    loader_tx: Sender<Loaded>,
    export_rx: Receiver<ExportMsg>,
    export_tx: Sender<ExportMsg>,
    exports: HashMap<PathBuf, ExportJob>,
    confirm_delete: Option<PathBuf>,
    status: Option<(String, Instant)>,
    styled: bool,
    loading: bool,
    /// When `Some`, the trim editor is shown instead of the library grid.
    editor: Option<EditorState>,
    /// A library rescan (new clip saved / recording stopped) arrived while the
    /// editor was open: deferred until it closes, so the full ffprobe sweep
    /// doesn't compete with the preview decoder mid-playback.
    pending_refresh: bool,
    watchdog: crate::diag::Watchdog,
    /// One-shot: honor `ORD_OPEN=<clip>` (debug) to open straight into the editor.
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
    /// Build the app, scanning `clips_dir` immediately.
    pub fn new(clips_dir: PathBuf) -> Self {
        let clips = scan_dir(&clips_dir);
        let (loader_tx, loader_rx) = channel();
        let (export_tx, export_rx) = channel();
        let (daemon_tx, daemon_rx) = channel();
        let (ctl_tx, ctl_rx) = channel();
        let (events_tx, events_rx) = channel();
        Self {
            clips_dir,
            clips,
            states: HashMap::new(),
            loader_rx,
            loader_tx,
            export_rx,
            export_tx,
            exports: HashMap::new(),
            confirm_delete: None,
            status: None,
            styled: false,
            loading: false,
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
                Ok(Event::RecordState { recording }) => {
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
                Event::RecordState { recording } => {
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
                // One-shot Config replies drive the settings page; a pushed
                // Config mid-edit must not clobber the user's draft.
                Event::Config { .. } | Event::Error { .. } => {}
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

    /// (Re)scan the directory and kick off background loading.
    fn refresh(&mut self, ctx: &egui::Context) {
        self.clips = scan_dir(&self.clips_dir);
        self.states.clear();
        self.confirm_delete = None;
        self.start_loading(ctx);
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

    /// Spawn the loader thread for the current clip set.
    fn start_loading(&mut self, ctx: &egui::Context) {
        let clips = self.clips.clone();
        let tx = self.loader_tx.clone();
        let ctx = ctx.clone();
        let visible = Arc::clone(&self.visible);
        self.loading = true;
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
            ctx,
        );
    }

    /// Export `input` with `preset`, optionally trimmed/muted — or, when
    /// `segments` is set, the editor's kept pieces concatenated into one file.
    /// Runs off-thread and reports via the export channel; ignores a duplicate
    /// in-flight export.
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
        ctx: &egui::Context,
    ) {
        if self.exports.contains_key(input) {
            return;
        }
        let mut profile = preset.profile();
        profile.mute = mute;
        let ext = profile.output_extension();
        let preset_name = preset.slug();
        let suffix = if segments.is_some() {
            "-cut"
        } else if trim.is_some() {
            "-trim"
        } else {
            ""
        };
        let out =
            meta::exports_dir(&self.clips_dir).join(format!("{stem}-{preset_name}{suffix}.{ext}"));
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

        // Debug: ORD_OPEN=<path> opens straight into the editor (skips the grid),
        // for screenshotting/validating the preview render path.
        if !self.auto_open_tried {
            self.auto_open_tried = true;
            if let Some(path) = crate::tuning::auto_open() {
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
            match view.ui(ctx) {
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
                } => {
                    let clip = ed.clip().clone();
                    let stem = clip
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "clip".to_string());
                    self.run_export(&clip, &stem, &stem, preset, trim, segments, mute, ctx);
                    self.editor = None;
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
        if !self.loading {
            self.start_loading(ctx);
        }
        self.start_daemon_poll(ctx);

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

                    // Search-as-you-type over clip names; Esc clears.
                    let search = egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Search clips…")
                        .desired_width(180.0);
                    let resp = ui.add(search);
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
                        if ui
                            .button("Settings")
                            .on_hover_text("Daemon configuration (applies live)")
                            .clicked()
                        {
                            self.settings = Some(SettingsView::new());
                            self.send_ctl(Command::GetConfig, ctx);
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
                if self.clips.is_empty() {
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

                // The query/sort view of the model (also the owned snapshot that
                // lets the card closures mutate `self`).
                let clips = filter_sort(&self.clips, &self.query, self.sort);
                if clips.is_empty() {
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
                const CARD_OUTER_W: f32 = CARD_INNER_W + 2.0 * theme::SP_3 + 8.0;
                let spacing = theme::SP_3;
                let cols = (((avail + spacing) / (CARD_OUTER_W + spacing)).floor() as usize).max(1);
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        ui.spacing_mut().item_spacing = egui::vec2(spacing, spacing);
                        for row in clips.chunks(cols) {
                            ui.horizontal(|ui| {
                                for clip in row {
                                    self.card(ui, clip, now, ctx);
                                }
                            });
                        }
                        ui.add_space(8.0);
                    });
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

    fn card(&mut self, ui: &mut egui::Ui, clip: &Clip, now: u64, ctx: &egui::Context) {
        theme::card().show(ui, |ui| {
            ui.set_width(CARD_INNER_W);
            ui.vertical(|ui| {
                if self.thumbnail(ui, clip) {
                    self.open_editor(clip, ctx);
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
                self.actions(ui, clip, ctx);
            });
        });
    }

    /// Render the thumbnail; returns true if it was clicked (opens the editor).
    fn thumbnail(&self, ui: &mut egui::Ui, clip: &Clip) -> bool {
        let size = egui::vec2(THUMB_W, THUMB_H);
        match self.states.get(&clip.path).and_then(|s| s.texture.as_ref()) {
            Some(tex) => ui
                .add(
                    egui::Image::new(tex)
                        .fit_to_exact_size(size)
                        .rounding(8.0)
                        .sense(egui::Sense::click()),
                )
                .on_hover_text("Edit / trim")
                .clicked(),
            None => {
                let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                ui.painter()
                    .rect_filled(rect, theme::RADIUS, egui::Color32::from_rgb(10, 11, 13));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "▶",
                    egui::FontId::proportional(26.0),
                    theme::INK_3,
                );
                resp.on_hover_text("Edit / trim").clicked()
            }
        }
    }

    fn open_editor(&mut self, clip: &Clip, ctx: &egui::Context) {
        match EditorState::new(clip.path.clone(), clip.label().to_string(), ctx) {
            Ok(ed) => self.editor = Some(ed),
            Err(e) => self.set_status(format!("Can't open editor: {e}")),
        }
    }

    fn actions(&mut self, ui: &mut egui::Ui, clip: &Clip, ctx: &egui::Context) {
        // One quiet row: primary actions inline, the rest behind "⋯". A delete
        // confirmation temporarily replaces the row (no accidental deletes,
        // no modal).
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
            .with_inner_size([920.0, 640.0])
            .with_min_inner_size([420.0, 320.0]),
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
