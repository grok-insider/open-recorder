//! Real wlr-layer-shell HUD surface (pure-Rust via smithay-client-toolkit).
//!
//! Creates an OVERLAY-layer, anchored, **click-through** surface that floats over
//! everything (including fullscreen games) and paints the [`Hud`]'s toasts as
//! dark rounded cards — soft drop shadow, per-kind anti-aliased icon, and real
//! text (fontdue) — into an ARGB shm buffer. No GPU needed.
//!
//! Behind the `layershell` feature (needs a Wayland session). All pixel work is
//! CPU SDF rasterization; glyph bitmaps are cached per character (the font size
//! is constant) so an on-screen toast costs a blit, not a re-rasterize, each
//! frame. The caller (`ord-hud`) only repaints while a toast is animating, so the
//! static buffer-on state spends no time here at all.

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, EventQueue, QueueHandle};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};

use std::collections::HashMap;

use fontdue::{Font, Metrics};
use ord_common::config::PressedKeysPosition;

use crate::hud::{Hud, PressedKeysLayout, ToastKind};
use crate::{Overlay, OverlayError};

/// A rasterized glyph (constant font size), cached so a visible toast blits the
/// same bitmap each frame instead of re-rasterizing every character every paint.
struct Glyph {
    metrics: Metrics,
    bitmap: Vec<u8>,
}

/// Glyph bitmap cache keyed by character (the font size is fixed at [`FONT_PX`]).
type GlyphCache = HashMap<char, Glyph>;
type KeyGlyphCache = HashMap<(u32, char), Glyph>;

/// Embedded UI font for the toast text (OFL, see `assets/fonts/LICENSES.md`).
const FONT_DATA: &[u8] = include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf");
const KEY_FONT_DATA: &[u8] = include_bytes!("../../ord-ui/assets/fonts/IBMPlexMono-Regular.ttf");

// Layout. The surface is a transparent canvas anchored top-right; cards are
// right-aligned within it, with `SHADOW` px of transparent margin for the soft
// drop shadow.
const SHADOW: u32 = 16;
const CARD_H: u32 = 46;
const CARD_GAP: u32 = 10;
const MAX_ROWS: u32 = 5;
const CARD_MAX_W: u32 = 420;
const CARD_MIN_W: u32 = 168;
const SURFACE_W: u32 = CARD_MAX_W + SHADOW * 2;
const SURFACE_H: u32 = SHADOW * 2 + (CARD_H + CARD_GAP) * MAX_ROWS;

const KEY_SURFACE_FALLBACK_W: u32 = 1280;
const KEY_SURFACE_FALLBACK_H: u32 = 720;
const KEYCAP_H: f32 = 58.0;
const KEYCAP_MIN_W: f32 = 72.0;
const KEYCAP_PAD_X: f32 = 20.0;
const KEYCAP_GAP: f32 = 10.0;
const KEYCAP_RADIUS: f32 = 10.0;
const KEY_FONT_PX: f32 = 22.0;
const KEY_SAFE_MARGIN: f32 = 54.0;
const KEY_SHADOW: f32 = 18.0;

const CARD_RADIUS: f32 = 12.0;
const FONT_PX: f32 = 15.0;
const PAD_X: f32 = 15.0;
const ICON_BOX: f32 = 20.0;
const ICON_GAP: f32 = 11.0;

// Animation.
const FADE_IN_MS: f32 = 150.0;
const FADE_OUT_MS: f32 = 240.0;
const SLIDE_PX: f32 = 18.0;

// Palette (straight, non-premultiplied; compositing premultiplies).
const CARD_BG: (f32, f32, f32) = (28.0, 29.0, 36.0); // #1C1D24
const TEXT_RGB: (f32, f32, f32) = (236.0, 237.0, 241.0); // #ECEDF1
const CARD_ALPHA: f32 = 0.94;
const KEY_TOP_RGB: (f32, f32, f32) = (89.0, 94.0, 97.0);
const KEY_BOTTOM_RGB: (f32, f32, f32) = (44.0, 47.0, 50.0);
const KEY_TEXT_RGB: (f32, f32, f32) = (244.0, 246.0, 249.0);

/// A live wlr-layer-shell HUD surface.
pub struct LayerShellOverlay {
    inner: Option<Inner>,
}

/// The connected surface plus its event queue. The queue must be pumped every
/// frame; otherwise `wl_buffer.release` events are never received and the shm
/// [`SlotPool`] allocates a fresh buffer on every draw (a steady memory leak).
struct Inner {
    state: State,
    event_queue: EventQueue<State>,
}

impl LayerShellOverlay {
    /// Construct (does not connect yet; call [`Overlay::create`]).
    pub fn new() -> Self {
        Self { inner: None }
    }
}

impl Default for LayerShellOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for LayerShellOverlay {
    fn create(&mut self) -> Result<(), OverlayError> {
        let (state, event_queue) = State::connect().map_err(OverlayError::Create)?;
        self.inner = Some(Inner { state, event_queue });
        Ok(())
    }

    fn render(&mut self, hud: &Hud, now_ms: u64) {
        if let Some(inner) = self.inner.as_mut() {
            // Process queued events (configure/close) before drawing.
            let _ = inner.event_queue.dispatch_pending(&mut inner.state);
            inner.state.draw(hud, now_ms);
            // Round-trip so the compositor's buffer-release events come back and
            // the SlotPool recycles buffers instead of growing every frame.
            let _ = inner.event_queue.roundtrip(&mut inner.state);
        }
    }

    fn destroy(&mut self) {
        self.inner = None;
    }
}

/// Internal Wayland client state + delegates.
struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    key_pool: SlotPool,
    key_layer: LayerSurface,
    conn: Connection,
    font: Font,
    key_font: Font,
    glyph_cache: GlyphCache,
    key_glyph_cache: KeyGlyphCache,
    width: u32,
    height: u32,
    key_width: u32,
    key_height: u32,
    /// Integer output scale (wl_surface preferred buffer scale). The shm
    /// buffer is allocated at `logical × scale` and all rasterization runs in
    /// physical pixels, so the HUD stays crisp on HiDPI outputs.
    scale: i32,
    configured: bool,
    key_configured: bool,
}

impl State {
    fn connect() -> Result<(State, EventQueue<State>), String> {
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let (globals, mut event_queue) =
            registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        let qh = event_queue.handle();

        let compositor =
            CompositorState::bind(&globals, &qh).map_err(|e| format!("wl_compositor: {e}"))?;
        let layer_shell =
            LayerShell::bind(&globals, &qh).map_err(|e| format!("layer_shell: {e}"))?;
        let shm = Shm::bind(&globals, &qh).map_err(|e| format!("wl_shm: {e}"))?;

        let surface = compositor.create_surface(&qh);
        let layer = layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some("open-recorder"),
            None,
        );

        let (width, height) = (SURFACE_W, SURFACE_H);
        layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
        layer.set_size(width, height);
        layer.set_margin(8, 8, 0, 0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);

        // Click-through: an EMPTY input region means no pointer events are routed
        // to this surface — they pass to the game underneath. Combined with
        // KeyboardInteractivity::None, the HUD is fully non-interactive.
        let empty_region = compositor.wl_compositor().create_region(&qh, ());
        layer.wl_surface().set_input_region(Some(&empty_region));

        layer.commit();

        let key_layer = layer_shell.create_layer_surface(
            &qh,
            compositor.create_surface(&qh),
            Layer::Overlay,
            Some("open-recorder-keys"),
            None,
        );
        key_layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        key_layer.set_size(0, 0);
        key_layer.set_margin(0, 0, 0, 0);
        key_layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        key_layer.wl_surface().set_input_region(Some(&empty_region));
        key_layer.commit();

        let pool = SlotPool::new((width * height * 4) as usize, &shm)
            .map_err(|e| format!("slot pool: {e}"))?;
        let key_pool = SlotPool::new(
            (KEY_SURFACE_FALLBACK_W * KEY_SURFACE_FALLBACK_H * 4) as usize,
            &shm,
        )
        .map_err(|e| format!("key slot pool: {e}"))?;

        let font = Font::from_bytes(FONT_DATA, fontdue::FontSettings::default())
            .map_err(|e| format!("font load: {e}"))?;
        let key_font = Font::from_bytes(KEY_FONT_DATA, fontdue::FontSettings::default())
            .map_err(|e| format!("key font load: {e}"))?;

        let mut state = State {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            shm,
            pool,
            layer,
            key_pool,
            key_layer,
            conn: conn.clone(),
            font,
            key_font,
            glyph_cache: GlyphCache::new(),
            key_glyph_cache: KeyGlyphCache::new(),
            width,
            height,
            key_width: KEY_SURFACE_FALLBACK_W,
            key_height: KEY_SURFACE_FALLBACK_H,
            scale: 1,
            configured: false,
            key_configured: false,
        };

        // Pump until the surface is configured so it's ready to draw.
        for _ in 0..50 {
            event_queue
                .blocking_dispatch(&mut state)
                .map_err(|e| e.to_string())?;
            if state.configured && state.key_configured {
                break;
            }
        }
        if !state.configured || !state.key_configured {
            return Err("layer surface was not configured".into());
        }
        Ok((state, event_queue))
    }

    fn draw(&mut self, hud: &Hud, now_ms: u64) {
        if !self.configured {
            return;
        }
        let scale = self.scale.max(1);
        let s = scale as f32;

        // Rasterize any not-yet-cached glyphs first, while `font` and
        // `glyph_cache` can be borrowed together (before the canvas borrows the
        // pool). The font size is constant per scale (the cache is cleared on
        // a scale change), so each glyph is rasterized at most once;
        // thereafter a visible toast only blits. Glyphs are rasterized at the
        // PHYSICAL pixel size so text is sharp on HiDPI outputs.
        for toast in hud.toasts().iter().take(MAX_ROWS as usize) {
            for ch in toast.text.chars() {
                if !self.glyph_cache.contains_key(&ch) {
                    let (metrics, bitmap) = self.font.rasterize(ch, FONT_PX * s);
                    self.glyph_cache.insert(ch, Glyph { metrics, bitmap });
                }
            }
        }
        // The buffer is physical-size; every coordinate below is physical.
        let (w, h) = physical_size(self.width, self.height, scale);
        let stride = (w * 4) as i32;
        let (buffer, canvas) =
            match self
                .pool
                .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
            {
                Ok(b) => b,
                Err(_) => return,
            };

        // Clear to fully transparent (premultiplied BGRA).
        canvas.fill(0);

        let cache = &self.glyph_cache;
        let right = w as f32 - SHADOW as f32 * s;
        let mut y = SHADOW as f32 * s;
        for toast in hud.toasts().iter().take(MAX_ROWS as usize) {
            // Fade + slide on appear and just before expiry.
            let age = now_ms.saturating_sub(toast.created_at_ms) as f32;
            let remaining = toast.expires_at_ms.saturating_sub(now_ms) as f32;
            let fade_in = ease_out_cubic(age / FADE_IN_MS);
            let fade_out = ease_out_cubic(remaining / FADE_OUT_MS);
            let alpha = fade_in.min(fade_out);
            if alpha > 0.001 {
                let slide = ((1.0 - fade_in) * SLIDE_PX + (1.0 - fade_out) * SLIDE_PX) * s;
                let text_w = measure_text(cache, &toast.text);
                let card_w = ((PAD_X + ICON_BOX + ICON_GAP + PAD_X) * s + text_w)
                    .clamp(CARD_MIN_W as f32 * s, CARD_MAX_W as f32 * s);
                let x0 = right - card_w + slide;
                draw_card(
                    canvas,
                    w,
                    h,
                    cache,
                    x0,
                    y,
                    card_w,
                    toast.kind,
                    &toast.text,
                    alpha,
                    s,
                );
            }
            y += (CARD_H + CARD_GAP) as f32 * s;
        }

        // Persistent replay-buffer indicator: a small dot in the top-right
        // corner (within the shadow margin, clear of the cards) whenever the
        // buffer is armed. Static, so with the caller's dirty-tracking it
        // costs nothing while idle. When the daemon is unreachable the dot
        // turns grey — visibly distinct from "armed", never silently absent.
        // `overlay.show_status_dot = false` suppresses it entirely.
        if hud.status_dot_visible() {
            let color = if hud.daemon_offline {
                accent(ToastKind::Stopped)
            } else {
                accent(ToastKind::Recording)
            };
            draw_dot(
                canvas,
                w,
                h,
                w as f32 - 11.0 * s,
                11.0 * s,
                4.5 * s,
                color,
                s,
            );
        }

        let surface = self.layer.wl_surface();
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if let Err(e) = buffer.attach_to(surface) {
            eprintln!("ord-overlay: attach failed: {e}");
            return;
        }
        surface.commit();
        let _ = self.conn.flush();

        self.draw_keys(hud);
    }

    fn draw_keys(&mut self, hud: &Hud) {
        if !self.key_configured {
            return;
        }

        let scale = self.scale.max(1);
        let s = scale as f32;
        let layout = hud.pressed_keys_layout();
        let font_px = key_font_px(layout, s);
        let font_key = font_px.round().max(1.0) as u32;
        for label in hud.pressed_key_labels() {
            for ch in label.chars() {
                self.key_glyph_cache
                    .entry((font_key, ch))
                    .or_insert_with(|| {
                        let (metrics, bitmap) = self.key_font.rasterize(ch, font_px);
                        Glyph { metrics, bitmap }
                    });
            }
        }

        let (w, h) = physical_size(self.key_width, self.key_height, scale);
        let stride = (w * 4) as i32;
        let (buffer, canvas) =
            match self
                .key_pool
                .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
            {
                Ok(b) => b,
                Err(_) => return,
            };
        canvas.fill(0);

        if !hud.pressed_key_labels().is_empty() {
            draw_keycaps(
                canvas,
                w,
                h,
                &self.key_glyph_cache,
                hud.pressed_key_labels(),
                layout,
                s,
            );
        }

        let surface = self.key_layer.wl_surface();
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if let Err(e) = buffer.attach_to(surface) {
            eprintln!("ord-overlay: key attach failed: {e}");
            return;
        }
        surface.commit();
        let _ = self.conn.flush();
    }
}

/// Physical shm-buffer size for a logical surface size at an integer scale.
fn physical_size(logical_w: u32, logical_h: u32, scale: i32) -> (u32, u32) {
    let s = scale.max(1) as u32;
    (logical_w * s, logical_h * s)
}

#[inline]
fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Accent colour (straight RGB) per toast kind.
#[inline]
fn accent(kind: ToastKind) -> (f32, f32, f32) {
    match kind {
        ToastKind::Saved => (63.0, 185.0, 80.0),     // #3FB950
        ToastKind::Recording => (248.0, 81.0, 73.0), // #F85149
        ToastKind::Stopped => (139.0, 148.0, 158.0), // #8B949E
        ToastKind::Error => (248.0, 81.0, 73.0),     // #F85149
        ToastKind::Marked => (217.0, 164.0, 65.0),   // #D9A441
    }
}

/// Composite a straight-colour source with coverage `a` (0..1) over a
/// premultiplied-alpha BGRA destination pixel at byte `idx`.
#[inline]
fn blend(canvas: &mut [u8], idx: usize, r: f32, g: f32, b: f32, a: f32) {
    if a <= 0.0 || idx + 4 > canvas.len() {
        return;
    }
    let a = a.min(1.0);
    let inv = 1.0 - a;
    let db = canvas[idx] as f32;
    let dg = canvas[idx + 1] as f32;
    let dr = canvas[idx + 2] as f32;
    let da = canvas[idx + 3] as f32;
    canvas[idx] = (b * a + db * inv) as u8;
    canvas[idx + 1] = (g * a + dg * inv) as u8;
    canvas[idx + 2] = (r * a + dr * inv) as u8;
    canvas[idx + 3] = (255.0 * a + da * inv) as u8;
}

/// Signed distance to a rounded box centred at `(cx,cy)`, half-size `(hw,hh)`.
#[inline]
fn sd_round_box(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let dx = (px - cx).abs() - (hw - r);
    let dy = (py - cy).abs() - (hh - r);
    let ax = dx.max(0.0);
    let ay = dy.max(0.0);
    (ax * ax + ay * ay).sqrt() + dx.max(dy).min(0.0) - r
}

/// Distance from `(px,py)` to the segment `a`-`b`.
#[inline]
fn sd_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (pax, pay) = (px - ax, py - ay);
    let (bax, bay) = (bx - ax, by - ay);
    let denom = bax * bax + bay * bay;
    let h = if denom > 0.0 {
        ((pax * bax + pay * bay) / denom).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (dx, dy) = (pax - bax * h, pay - bay * h);
    (dx * dx + dy * dy).sqrt()
}

/// Sum of cached glyph advance widths (px) — for auto-sizing the card. Any glyph
/// in `text` is expected to be in `cache` already (the renderer pre-populates it).
fn measure_text(cache: &GlyphCache, text: &str) -> f32 {
    text.chars()
        .map(|c| cache.get(&c).map_or(0.0, |g| g.metrics.advance_width))
        .sum()
}

/// Draw one toast card: soft shadow, rounded body + hairline, icon, and text.
/// `s` is the integer output scale as f32; all inputs/outputs are physical px.
#[allow(clippy::too_many_arguments)]
fn draw_card(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    cache: &GlyphCache,
    x0: f32,
    y0: f32,
    w: f32,
    kind: ToastKind,
    text: &str,
    alpha: f32,
    s: f32,
) {
    let h = CARD_H as f32 * s;
    let (cx, cy) = (x0 + w / 2.0, y0 + h / 2.0);
    let (hw, hh) = (w / 2.0, h / 2.0);

    let pad = SHADOW as f32 * s;
    let xs = (x0 - pad).floor().max(0.0) as u32;
    let xe = ((x0 + w + pad).ceil() as u32).min(width);
    let ys = (y0 - pad).floor().max(0.0) as u32;
    let ye = ((y0 + h + pad).ceil() as u32).min(height);
    let feather = 13.0 * s;
    let sh_cy = cy + 4.0 * s;
    let radius = CARD_RADIUS * s;
    for py in ys..ye {
        for px in xs..xe {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            let idx = ((py * width + px) * 4) as usize;
            // Soft drop shadow (outside the card body).
            let ds = sd_round_box(fx, fy, cx, sh_cy, hw, hh, radius);
            if ds > 0.0 && ds < feather {
                let t = 1.0 - ds / feather;
                blend(canvas, idx, 0.0, 0.0, 0.0, 0.40 * t * t * alpha);
            }
            // Card body with AA edge.
            let d = sd_round_box(fx, fy, cx, cy, hw, hh, radius);
            let cov = (0.5 - d).clamp(0.0, 1.0);
            if cov > 0.0 {
                blend(
                    canvas,
                    idx,
                    CARD_BG.0,
                    CARD_BG.1,
                    CARD_BG.2,
                    CARD_ALPHA * cov * alpha,
                );
                // Faint light hairline just inside the edge for definition.
                let ring = (1.0 - (d + 1.2 * s).abs()).clamp(0.0, 1.0);
                if ring > 0.0 {
                    blend(canvas, idx, 255.0, 255.0, 255.0, 0.05 * ring * alpha);
                }
            }
        }
    }

    let icx = x0 + (PAD_X + ICON_BOX / 2.0) * s;
    let icy = y0 + h / 2.0;
    draw_icon(
        canvas,
        width,
        height,
        kind,
        icx,
        icy,
        accent(kind),
        alpha,
        s,
    );

    let tx = x0 + (PAD_X + ICON_BOX + ICON_GAP) * s;
    let baseline = y0 + h / 2.0 + FONT_PX * s * 0.34;
    draw_text(
        canvas, width, height, cache, text, tx, baseline, TEXT_RGB, alpha,
    );
}

fn key_font_px(layout: PressedKeysLayout, s: f32) -> f32 {
    KEY_FONT_PX * s * key_layout_scale(layout)
}

fn key_layout_scale(layout: PressedKeysLayout) -> f32 {
    layout.scale_percent.clamp(50, 250) as f32 / 100.0
}

fn key_opacity(layout: PressedKeysLayout) -> f32 {
    layout.opacity_percent.clamp(35, 100) as f32 / 100.0
}

fn measure_key_text(cache: &KeyGlyphCache, label: &str, font_key: u32) -> f32 {
    label
        .chars()
        .map(|c| {
            cache
                .get(&(font_key, c))
                .map_or(0.0, |g| g.metrics.advance_width)
        })
        .sum()
}

fn key_row_width(labels: &[String], widths: &[f32], gap: f32) -> f32 {
    widths.iter().sum::<f32>() + gap * labels.len().saturating_sub(1) as f32
}

fn key_layout_center(
    layout: PressedKeysLayout,
    width: u32,
    height: u32,
    row_w: f32,
    row_h: f32,
    unit: f32,
) -> (f32, f32) {
    let w = width as f32;
    let h = height as f32;
    let margin = KEY_SAFE_MARGIN * unit;
    match layout.position {
        PressedKeysPosition::BottomCenter => (w / 2.0, h - margin - row_h / 2.0),
        PressedKeysPosition::BottomLeft => (margin + row_w / 2.0, h - margin - row_h / 2.0),
        PressedKeysPosition::BottomRight => (w - margin - row_w / 2.0, h - margin - row_h / 2.0),
        PressedKeysPosition::TopCenter => (w / 2.0, margin + row_h / 2.0),
        PressedKeysPosition::Custom => (
            w * layout.x_ppm.min(1000) as f32 / 1000.0,
            h * layout.y_ppm.min(1000) as f32 / 1000.0,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn clamp_key_center(
    cx: f32,
    cy: f32,
    width: u32,
    height: u32,
    row_w: f32,
    row_h: f32,
    angle: f32,
    unit: f32,
) -> (f32, f32) {
    let (sin, cos) = angle.sin_cos();
    let half_w = (cos.abs() * row_w + sin.abs() * row_h) / 2.0 + KEY_SHADOW * unit;
    let half_h = (sin.abs() * row_w + cos.abs() * row_h) / 2.0 + KEY_SHADOW * unit;
    let w = width as f32;
    let h = height as f32;
    let min_x = half_w.min(w / 2.0);
    let max_x = (w - half_w).max(min_x);
    let min_y = half_h.min(h / 2.0);
    let max_y = (h - half_h).max(min_y);
    (cx.clamp(min_x, max_x), cy.clamp(min_y, max_y))
}

fn rotated_local(
    px: f32,
    py: f32,
    cx: f32,
    cy: f32,
    row_w: f32,
    row_h: f32,
    angle: f32,
) -> (f32, f32) {
    let (sin, cos) = angle.sin_cos();
    let dx = px - cx;
    let dy = py - cy;
    (
        cos * dx + sin * dy + row_w / 2.0,
        -sin * dx + cos * dy + row_h / 2.0,
    )
}

#[allow(clippy::too_many_arguments)]
fn rotated_screen(
    lx: f32,
    ly: f32,
    cx: f32,
    cy: f32,
    row_w: f32,
    row_h: f32,
    angle: f32,
) -> (f32, f32) {
    let (sin, cos) = angle.sin_cos();
    let dx = lx - row_w / 2.0;
    let dy = ly - row_h / 2.0;
    (cx + cos * dx - sin * dy, cy + sin * dx + cos * dy)
}

fn draw_keycaps(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    cache: &KeyGlyphCache,
    labels: &[String],
    layout: PressedKeysLayout,
    s: f32,
) {
    let unit = s * key_layout_scale(layout);
    let font_key = key_font_px(layout, s).round().max(1.0) as u32;
    let gap = KEYCAP_GAP * unit;
    let key_h = KEYCAP_H * unit;
    let widths: Vec<f32> = labels
        .iter()
        .map(|label| {
            (measure_key_text(cache, label, font_key) + KEYCAP_PAD_X * 2.0 * unit)
                .max(KEYCAP_MIN_W * unit)
        })
        .collect();
    let row_w = key_row_width(labels, &widths, gap);
    let row_h = key_h;
    let angle = (layout.rotation_degrees.clamp(-30, 30) as f32).to_radians();
    let (cx, cy) = key_layout_center(layout, width, height, row_w, row_h, unit);
    let (cx, cy) = clamp_key_center(cx, cy, width, height, row_w, row_h, angle, unit);

    let (sin, cos) = angle.sin_cos();
    let bbox_w = cos.abs() * row_w + sin.abs() * row_h + KEY_SHADOW * unit * 2.0;
    let bbox_h = sin.abs() * row_w + cos.abs() * row_h + KEY_SHADOW * unit * 2.0;
    let xs = (cx - bbox_w / 2.0).floor().max(0.0) as u32;
    let xe = ((cx + bbox_w / 2.0).ceil() as u32).min(width);
    let ys = (cy - bbox_h / 2.0).floor().max(0.0) as u32;
    let ye = ((cy + bbox_h / 2.0).ceil() as u32).min(height);
    let radius = KEYCAP_RADIUS * unit;
    let feather = 14.0 * unit;
    let opacity = key_opacity(layout);
    for py in ys..ye {
        for px in xs..xe {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            let idx = ((py * width + px) * 4) as usize;
            let (lx, ly) = rotated_local(fx, fy, cx, cy, row_w, row_h, angle);
            let mut x = 0.0;
            for w in &widths {
                let kcx = x + *w / 2.0;
                let kcy = row_h / 2.0;
                let ds = sd_round_box(lx, ly, kcx, kcy + 5.0 * unit, *w / 2.0, key_h / 2.0, radius);
                if ds > 0.0 && ds < feather {
                    let t = 1.0 - ds / feather;
                    blend(canvas, idx, 0.0, 0.0, 0.0, 0.40 * t * t * opacity);
                }
                let d = sd_round_box(lx, ly, kcx, kcy, *w / 2.0, key_h / 2.0, radius);
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let shade = (ly / key_h).clamp(0.0, 1.0);
                    let r = KEY_TOP_RGB.0 * (1.0 - shade) + KEY_BOTTOM_RGB.0 * shade;
                    let g = KEY_TOP_RGB.1 * (1.0 - shade) + KEY_BOTTOM_RGB.1 * shade;
                    let b = KEY_TOP_RGB.2 * (1.0 - shade) + KEY_BOTTOM_RGB.2 * shade;
                    blend(canvas, idx, r, g, b, cov * opacity);
                    let ring = (1.0 - (d + 1.1 * unit).abs()).clamp(0.0, 1.0);
                    if ring > 0.0 {
                        blend(canvas, idx, 255.0, 255.0, 255.0, 0.10 * ring * opacity);
                    }
                    let top_line = (1.0 - (ly - 6.0 * unit).abs()).clamp(0.0, 1.0);
                    if top_line > 0.0 && lx > x + radius && lx < x + *w - radius {
                        blend(canvas, idx, 255.0, 255.0, 255.0, 0.08 * top_line * opacity);
                    }
                }
                x += *w + gap;
            }
        }
    }

    let mut x = 0.0;
    for (label, key_w) in labels.iter().zip(widths.iter()) {
        let text_w = measure_key_text(cache, label, font_key);
        let tx = x + (*key_w - text_w) / 2.0;
        let baseline = row_h / 2.0 + key_font_px(layout, s) * 0.34;
        draw_rotated_key_text(
            canvas, width, height, cache, font_key, label, tx, baseline, cx, cy, row_w, row_h,
            angle, opacity,
        );
        x += *key_w + gap;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_rotated_key_text(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    cache: &KeyGlyphCache,
    font_key: u32,
    text: &str,
    x: f32,
    baseline: f32,
    cx: f32,
    cy: f32,
    row_w: f32,
    row_h: f32,
    angle: f32,
    alpha: f32,
) {
    let mut pen = x;
    for ch in text.chars() {
        let Some(glyph) = cache.get(&(font_key, ch)) else {
            continue;
        };
        let m = &glyph.metrics;
        if m.width > 0 && m.height > 0 {
            let gx = pen + m.xmin as f32;
            let gy = baseline - m.height as f32 - m.ymin as f32;
            for row in 0..m.height {
                for col in 0..m.width {
                    let cov = glyph.bitmap[row * m.width + col] as f32 / 255.0;
                    if cov <= 0.0 {
                        continue;
                    }
                    let (sx, sy) = rotated_screen(
                        gx + col as f32,
                        gy + row as f32,
                        cx,
                        cy,
                        row_w,
                        row_h,
                        angle,
                    );
                    let px = sx.round() as i32;
                    let py = sy.round() as i32;
                    if px < 0 || py < 0 || px as u32 >= width || py as u32 >= height {
                        continue;
                    }
                    let idx = ((py as u32 * width + px as u32) * 4) as usize;
                    blend(
                        canvas,
                        idx,
                        KEY_TEXT_RGB.0,
                        KEY_TEXT_RGB.1,
                        KEY_TEXT_RGB.2,
                        cov * alpha,
                    );
                }
            }
        }
        pen += m.advance_width;
    }
}

/// Draw a small anti-aliased filled dot with a faint halo (the buffer
/// indicator). `cx`/`cy`/`r` are physical px; `s` scales the halo.
#[allow(clippy::too_many_arguments)]
fn draw_dot(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    cx: f32,
    cy: f32,
    r: f32,
    color: (f32, f32, f32),
    s: f32,
) {
    let halo_w = 3.0 * s;
    let half = r + halo_w;
    let xs = (cx - half).floor().max(0.0) as u32;
    let xe = ((cx + half).ceil() as u32).min(width);
    let ys = (cy - half).floor().max(0.0) as u32;
    let ye = ((cy + half).ceil() as u32).min(height);
    for py in ys..ye {
        for px in xs..xe {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() - r;
            let idx = ((py * width + px) * 4) as usize;
            let cov = (0.5 - d).clamp(0.0, 1.0);
            if cov > 0.0 {
                blend(canvas, idx, color.0, color.1, color.2, cov * 0.9);
            } else if d < halo_w {
                let halo = (1.0 - d / halo_w).clamp(0.0, 1.0) * 0.18;
                blend(canvas, idx, color.0, color.1, color.2, halo);
            }
        }
    }
}

/// Draw the per-kind status icon (anti-aliased, procedural — no glyph
/// needed). `cx`/`cy` are physical px; `s` scales the icon geometry.
#[allow(clippy::too_many_arguments)]
fn draw_icon(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    kind: ToastKind,
    cx: f32,
    cy: f32,
    color: (f32, f32, f32),
    alpha: f32,
    s: f32,
) {
    let icon_box = ICON_BOX * s;
    let half = icon_box / 2.0 + s;
    let xs = (cx - half).floor().max(0.0) as u32;
    let xe = ((cx + half).ceil() as u32).min(width);
    let ys = (cy - half).floor().max(0.0) as u32;
    let ye = ((cy + half).ceil() as u32).min(height);
    let stroke = 1.7 * s;
    for py in ys..ye {
        for px in xs..xe {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            let cov = match kind {
                ToastKind::Recording => {
                    let r = icon_box * 0.30;
                    let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() - r;
                    (0.5 - d).clamp(0.0, 1.0)
                }
                ToastKind::Saved => {
                    let r = icon_box * 0.36;
                    let d = sd_segment(
                        fx,
                        fy,
                        cx - r * 0.62,
                        cy + r * 0.06,
                        cx - r * 0.16,
                        cy + r * 0.5,
                    )
                    .min(sd_segment(
                        fx,
                        fy,
                        cx - r * 0.16,
                        cy + r * 0.5,
                        cx + r * 0.64,
                        cy - r * 0.46,
                    )) - stroke;
                    (0.5 - d).clamp(0.0, 1.0)
                }
                ToastKind::Stopped => {
                    let sq = icon_box * 0.26;
                    let d = sd_round_box(fx, fy, cx, cy, sq, sq, 2.5 * s);
                    (0.5 - d).clamp(0.0, 1.0)
                }
                ToastKind::Error => {
                    let r = icon_box * 0.28;
                    let d = sd_segment(fx, fy, cx - r, cy - r, cx + r, cy + r).min(sd_segment(
                        fx,
                        fy,
                        cx - r,
                        cy + r,
                        cx + r,
                        cy - r,
                    )) - stroke;
                    (0.5 - d).clamp(0.0, 1.0)
                }
                ToastKind::Marked => {
                    // A bookmark flag: vertical pole + a short top stroke.
                    let r = icon_box * 0.30;
                    let d = sd_segment(fx, fy, cx - r * 0.4, cy - r, cx - r * 0.4, cy + r).min(
                        sd_segment(fx, fy, cx - r * 0.4, cy - r, cx + r * 0.8, cy - r * 0.35),
                    ) - stroke;
                    (0.5 - d).clamp(0.0, 1.0)
                }
            };
            if cov > 0.0 {
                let idx = ((py * width + px) * 4) as usize;
                blend(canvas, idx, color.0, color.1, color.2, cov * alpha);
            }
        }
    }
}

/// Blit `text` from the cached glyph bitmaps (alpha = glyph coverage). Glyphs are
/// rasterized once into `cache` by the renderer; here we only copy pixels.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    cache: &GlyphCache,
    text: &str,
    x: f32,
    baseline: f32,
    color: (f32, f32, f32),
    alpha: f32,
) {
    let mut pen = x;
    for ch in text.chars() {
        let Some(glyph) = cache.get(&ch) else {
            continue;
        };
        let m = &glyph.metrics;
        if m.width > 0 && m.height > 0 {
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (baseline - m.height as f32 - m.ymin as f32).round() as i32;
            for row in 0..m.height {
                let py = gy + row as i32;
                if py < 0 || py as u32 >= height {
                    continue;
                }
                for col in 0..m.width {
                    let px = gx + col as i32;
                    if px < 0 || px as u32 >= width {
                        continue;
                    }
                    let cov = glyph.bitmap[row * m.width + col] as f32 / 255.0;
                    if cov > 0.0 {
                        let idx = ((py as u32 * width + px as u32) * 4) as usize;
                        blend(canvas, idx, color.0, color.1, color.2, cov * alpha);
                    }
                }
            }
        }
        pen += m.advance_width;
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let known_surface =
            surface == self.layer.wl_surface() || surface == self.key_layer.wl_surface();
        if !known_surface || new_factor < 1 || new_factor == self.scale {
            return;
        }
        self.scale = new_factor;
        // Glyphs are rasterized at the physical pixel size, so a scale change
        // invalidates every cached bitmap.
        self.glyph_cache.clear();
        self.key_glyph_cache.clear();
        self.layer.wl_surface().set_buffer_scale(new_factor);
        self.key_layer.wl_surface().set_buffer_scale(new_factor);
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if layer.wl_surface() == self.layer.wl_surface() {
            self.configured = false;
        } else if layer.wl_surface() == self.key_layer.wl_surface() {
            self.key_configured = false;
        }
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if layer.wl_surface() == self.layer.wl_surface() {
            if configure.new_size.0 != 0 {
                self.width = configure.new_size.0;
            }
            if configure.new_size.1 != 0 {
                self.height = configure.new_size.1;
            }
            self.configured = true;
        } else if layer.wl_surface() == self.key_layer.wl_surface() {
            if configure.new_size.0 != 0 {
                self.key_width = configure.new_size.0;
            }
            if configure.new_size.1 != 0 {
                self.key_height = configure.new_size.1;
            }
            self.key_configured = true;
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_layer!(State);
delegate_registry!(State);

// wl_region carries no state; a trivial Dispatch lets us create the empty
// input region used for click-through.
impl
    smithay_client_toolkit::reexports::client::Dispatch<
        smithay_client_toolkit::reexports::client::protocol::wl_region::WlRegion,
        (),
    > for State
{
    fn event(
        _: &mut Self,
        _: &smithay_client_toolkit::reexports::client::protocol::wl_region::WlRegion,
        _: <smithay_client_toolkit::reexports::client::protocol::wl_region::WlRegion as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_full_alpha_writes_source_bgra() {
        let mut c = vec![0u8; 8];
        blend(&mut c, 0, 255.0, 128.0, 10.0, 1.0);
        assert_eq!(&c[..4], &[10, 128, 255, 255]);
        // Zero alpha is a no-op.
        blend(&mut c, 4, 255.0, 255.0, 255.0, 0.0);
        assert_eq!(&c[4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn blend_composites_over_destination() {
        // Opaque black dest + 50% white source -> mid grey, still opaque.
        let mut c = vec![0u8, 0, 0, 255];
        blend(&mut c, 0, 255.0, 255.0, 255.0, 0.5);
        assert_eq!(&c[..3], &[127, 127, 127]);
        assert_eq!(c[3], 255);
    }

    #[test]
    fn blend_out_of_bounds_is_a_noop() {
        let mut c = vec![7u8; 6];
        blend(&mut c, 4, 255.0, 255.0, 255.0, 1.0); // idx + 4 > len
        assert_eq!(c, vec![7u8; 6]);
        // Coverage above 1 clamps rather than overflowing.
        let mut c = vec![0u8; 4];
        blend(&mut c, 0, 255.0, 255.0, 255.0, 2.0);
        assert_eq!(c, vec![255u8; 4]);
    }

    #[test]
    fn sd_round_box_signs_and_distances() {
        // Center is inside (negative), a point on the straight edge is ~0,
        // and far outside the distance is euclidean.
        assert!(sd_round_box(50.0, 50.0, 50.0, 50.0, 10.0, 10.0, 2.0) < 0.0);
        assert!(sd_round_box(60.0, 50.0, 50.0, 50.0, 10.0, 10.0, 2.0).abs() < 1e-4);
        let d = sd_round_box(80.0, 50.0, 50.0, 50.0, 10.0, 10.0, 2.0);
        assert!((d - 20.0).abs() < 1e-4, "{d}");
        // A sharp corner (r=0) measures the diagonal distance.
        let d = sd_round_box(13.0, 14.0, 0.0, 0.0, 10.0, 10.0, 0.0);
        assert!((d - 5.0).abs() < 1e-4, "{d}");
    }

    #[test]
    fn sd_segment_distances() {
        // On the segment.
        assert!(sd_segment(5.0, 0.0, 0.0, 0.0, 10.0, 0.0) < 1e-6);
        // Perpendicular offset.
        assert!((sd_segment(5.0, 3.0, 0.0, 0.0, 10.0, 0.0) - 3.0).abs() < 1e-6);
        // Beyond an endpoint clamps to the endpoint distance.
        assert!((sd_segment(13.0, 4.0, 0.0, 0.0, 10.0, 0.0) - 5.0).abs() < 1e-6);
        // Degenerate zero-length segment is distance-to-point.
        assert!((sd_segment(3.0, 4.0, 0.0, 0.0, 0.0, 0.0) - 5.0).abs() < 1e-6);
    }

    #[test]
    fn measure_text_sums_cached_advances() {
        let font = match Font::from_bytes(FONT_DATA, fontdue::FontSettings::default()) {
            Ok(f) => f,
            Err(e) => panic!("embedded font must load: {e}"),
        };
        let mut cache = GlyphCache::new();
        for ch in "ab".chars() {
            let (metrics, bitmap) = font.rasterize(ch, FONT_PX);
            cache.insert(ch, Glyph { metrics, bitmap });
        }
        let a = cache[&'a'].metrics.advance_width;
        let b = cache[&'b'].metrics.advance_width;
        assert!(a > 0.0 && b > 0.0);
        assert!((measure_text(&cache, "ab") - (a + b)).abs() < 1e-4);
        assert_eq!(measure_text(&cache, ""), 0.0);
        // Glyphs missing from the cache measure zero instead of panicking.
        assert_eq!(measure_text(&cache, "zz"), 0.0);
    }

    #[test]
    fn ease_out_cubic_clamps_and_eases() {
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
        let mid = ease_out_cubic(0.5);
        assert!(mid > 0.5 && mid < 1.0);
    }

    #[test]
    fn physical_size_scales_and_guards() {
        assert_eq!(physical_size(420, 100, 1), (420, 100));
        assert_eq!(physical_size(420, 100, 2), (840, 200));
        assert_eq!(physical_size(420, 100, 3), (1260, 300));
        // Degenerate scales clamp to 1 instead of zeroing the buffer.
        assert_eq!(physical_size(420, 100, 0), (420, 100));
        assert_eq!(physical_size(420, 100, -2), (420, 100));
    }

    #[test]
    fn pressed_key_layout_scale_and_opacity_are_clamped() {
        let mut layout = PressedKeysLayout {
            position: PressedKeysPosition::Custom,
            x_ppm: 500,
            y_ppm: 500,
            scale_percent: 20,
            opacity_percent: 10,
            rotation_degrees: 0,
        };
        assert_eq!(key_layout_scale(layout), 0.5);
        assert_eq!(key_opacity(layout), 0.35);
        layout.scale_percent = 300;
        layout.opacity_percent = 120;
        assert_eq!(key_layout_scale(layout), 2.5);
        assert_eq!(key_opacity(layout), 1.0);
    }

    #[test]
    fn pressed_key_custom_center_uses_normalized_coordinates() {
        let layout = PressedKeysLayout {
            position: PressedKeysPosition::Custom,
            x_ppm: 250,
            y_ppm: 750,
            scale_percent: 100,
            opacity_percent: 92,
            rotation_degrees: 0,
        };
        let (x, y) = key_layout_center(layout, 1000, 800, 100.0, 50.0, 1.0);
        assert_eq!((x, y), (250.0, 600.0));
    }
}
