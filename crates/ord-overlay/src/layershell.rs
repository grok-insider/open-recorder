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

use crate::hud::{Hud, ToastKind};
use crate::{Overlay, OverlayError};

/// A rasterized glyph (constant font size), cached so a visible toast blits the
/// same bitmap each frame instead of re-rasterizing every character every paint.
struct Glyph {
    metrics: Metrics,
    bitmap: Vec<u8>,
}

/// Glyph bitmap cache keyed by character (the font size is fixed at [`FONT_PX`]).
type GlyphCache = HashMap<char, Glyph>;

/// Embedded UI font for the toast text (OFL, see `assets/fonts/LICENSES.md`).
const FONT_DATA: &[u8] = include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf");

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

    fn set_visible(&mut self, visible: bool) {
        if let Some(inner) = self.inner.as_mut() {
            inner.state.visible = visible;
        }
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
    conn: Connection,
    font: Font,
    glyph_cache: GlyphCache,
    width: u32,
    height: u32,
    visible: bool,
    configured: bool,
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

        let pool = SlotPool::new((width * height * 4) as usize, &shm)
            .map_err(|e| format!("slot pool: {e}"))?;

        let font = Font::from_bytes(FONT_DATA, fontdue::FontSettings::default())
            .map_err(|e| format!("font load: {e}"))?;

        let mut state = State {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            shm,
            pool,
            layer,
            conn: conn.clone(),
            font,
            glyph_cache: GlyphCache::new(),
            width,
            height,
            visible: true,
            configured: false,
        };

        // Pump until the surface is configured so it's ready to draw.
        for _ in 0..50 {
            event_queue
                .blocking_dispatch(&mut state)
                .map_err(|e| e.to_string())?;
            if state.configured {
                break;
            }
        }
        if !state.configured {
            return Err("layer surface was not configured".into());
        }
        Ok((state, event_queue))
    }

    fn draw(&mut self, hud: &Hud, now_ms: u64) {
        if !self.configured {
            return;
        }

        // Rasterize any not-yet-cached glyphs first, while `font` and
        // `glyph_cache` can be borrowed together (before the canvas borrows the
        // pool). The font size is constant, so each glyph is rasterized at most
        // once per process; thereafter a visible toast only blits.
        if self.visible {
            for toast in hud.toasts().iter().take(MAX_ROWS as usize) {
                for ch in toast.text.chars() {
                    if !self.glyph_cache.contains_key(&ch) {
                        let (metrics, bitmap) = self.font.rasterize(ch, FONT_PX);
                        self.glyph_cache.insert(ch, Glyph { metrics, bitmap });
                    }
                }
            }
        }

        let (w, h) = (self.width, self.height);
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

        if self.visible {
            let cache = &self.glyph_cache;
            let right = w as f32 - SHADOW as f32;
            let mut y = SHADOW as f32;
            for toast in hud.toasts().iter().take(MAX_ROWS as usize) {
                // Fade + slide on appear and just before expiry.
                let age = now_ms.saturating_sub(toast.created_at_ms) as f32;
                let remaining = toast.expires_at_ms.saturating_sub(now_ms) as f32;
                let fade_in = ease_out_cubic(age / FADE_IN_MS);
                let fade_out = ease_out_cubic(remaining / FADE_OUT_MS);
                let alpha = fade_in.min(fade_out);
                if alpha > 0.001 {
                    let slide = (1.0 - fade_in) * SLIDE_PX + (1.0 - fade_out) * SLIDE_PX;
                    let text_w = measure_text(cache, &toast.text);
                    let card_w = (PAD_X + ICON_BOX + ICON_GAP + text_w + PAD_X)
                        .clamp(CARD_MIN_W as f32, CARD_MAX_W as f32);
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
                    );
                }
                y += (CARD_H + CARD_GAP) as f32;
            }

            // Persistent replay-buffer indicator: a small dot in the top-right
            // corner (within the shadow margin, clear of the cards) whenever the
            // buffer is armed. Static, so with the caller's dirty-tracking it
            // costs nothing while idle.
            if hud.buffer_active {
                draw_dot(
                    canvas,
                    w,
                    h,
                    w as f32 - 11.0,
                    11.0,
                    4.5,
                    accent(ToastKind::Recording),
                );
            }
        }

        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if let Err(e) = buffer.attach_to(surface) {
            eprintln!("ord-overlay: attach failed: {e}");
            return;
        }
        surface.commit();
        let _ = self.conn.flush();
    }
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
) {
    let h = CARD_H as f32;
    let (cx, cy) = (x0 + w / 2.0, y0 + h / 2.0);
    let (hw, hh) = (w / 2.0, h / 2.0);

    let pad = SHADOW as f32;
    let xs = (x0 - pad).floor().max(0.0) as u32;
    let xe = ((x0 + w + pad).ceil() as u32).min(width);
    let ys = (y0 - pad).floor().max(0.0) as u32;
    let ye = ((y0 + h + pad).ceil() as u32).min(height);
    let feather = 13.0;
    let sh_cy = cy + 4.0;
    for py in ys..ye {
        for px in xs..xe {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            let idx = ((py * width + px) * 4) as usize;
            // Soft drop shadow (outside the card body).
            let ds = sd_round_box(fx, fy, cx, sh_cy, hw, hh, CARD_RADIUS);
            if ds > 0.0 && ds < feather {
                let s = 1.0 - ds / feather;
                blend(canvas, idx, 0.0, 0.0, 0.0, 0.40 * s * s * alpha);
            }
            // Card body with AA edge.
            let d = sd_round_box(fx, fy, cx, cy, hw, hh, CARD_RADIUS);
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
                let ring = (1.0 - (d + 1.2).abs()).clamp(0.0, 1.0);
                if ring > 0.0 {
                    blend(canvas, idx, 255.0, 255.0, 255.0, 0.05 * ring * alpha);
                }
            }
        }
    }

    let icx = x0 + PAD_X + ICON_BOX / 2.0;
    let icy = y0 + h / 2.0;
    draw_icon(canvas, width, height, kind, icx, icy, accent(kind), alpha);

    let tx = x0 + PAD_X + ICON_BOX + ICON_GAP;
    let baseline = y0 + h / 2.0 + FONT_PX * 0.34;
    draw_text(
        canvas, width, height, cache, text, tx, baseline, TEXT_RGB, alpha,
    );
}

/// Draw a small anti-aliased filled dot with a faint halo (the buffer indicator).
fn draw_dot(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    cx: f32,
    cy: f32,
    r: f32,
    color: (f32, f32, f32),
) {
    let half = r + 3.0;
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
            } else if d < 3.0 {
                let halo = (1.0 - d / 3.0).clamp(0.0, 1.0) * 0.18;
                blend(canvas, idx, color.0, color.1, color.2, halo);
            }
        }
    }
}

/// Draw the per-kind status icon (anti-aliased, procedural — no glyph needed).
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
) {
    let half = ICON_BOX / 2.0 + 1.0;
    let xs = (cx - half).floor().max(0.0) as u32;
    let xe = ((cx + half).ceil() as u32).min(width);
    let ys = (cy - half).floor().max(0.0) as u32;
    let ye = ((cy + half).ceil() as u32).min(height);
    let stroke = 1.7;
    for py in ys..ye {
        for px in xs..xe {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            let cov = match kind {
                ToastKind::Recording => {
                    let r = ICON_BOX * 0.30;
                    let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() - r;
                    (0.5 - d).clamp(0.0, 1.0)
                }
                ToastKind::Saved => {
                    let r = ICON_BOX * 0.36;
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
                    let s = ICON_BOX * 0.26;
                    let d = sd_round_box(fx, fy, cx, cy, s, s, 2.5);
                    (0.5 - d).clamp(0.0, 1.0)
                }
                ToastKind::Error => {
                    let r = ICON_BOX * 0.28;
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
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
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
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.configured = false;
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if configure.new_size.0 != 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 != 0 {
            self.height = configure.new_size.1;
        }
        self.configured = true;
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
