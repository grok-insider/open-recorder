//! Real wlr-layer-shell HUD surface (pure-Rust via smithay-client-toolkit).
//!
//! Creates an OVERLAY-layer, anchored, **click-through** surface that floats over
//! everything (including fullscreen games) and paints the [`Hud`] as simple
//! colored toast bars into an shm buffer. No GPU needed.
//!
//! Behind the `layershell` feature (needs a Wayland session). The text rendering
//! is intentionally minimal — solid bars colored by [`ToastKind`] plus a buffer
//! indicator dot — keeping the dependency surface small and robust. Glyph
//! rendering can be layered on later without changing the [`Overlay`] contract.

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
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

use crate::hud::{Hud, ToastKind};
use crate::{Overlay, OverlayError};

const WIDTH: u32 = 360;
const BAR_H: u32 = 36;
const PAD: u32 = 6;
const MAX_ROWS: u32 = 6;

/// A live wlr-layer-shell HUD surface.
pub struct LayerShellOverlay {
    state: Option<State>,
}

impl LayerShellOverlay {
    /// Construct (does not connect yet; call [`Overlay::create`]).
    pub fn new() -> Self {
        Self { state: None }
    }
}

impl Default for LayerShellOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for LayerShellOverlay {
    fn create(&mut self) -> Result<(), OverlayError> {
        let state = State::connect().map_err(OverlayError::Create)?;
        self.state = Some(state);
        Ok(())
    }

    fn set_visible(&mut self, visible: bool) {
        if let Some(s) = self.state.as_mut() {
            s.visible = visible;
        }
    }

    fn render(&mut self, hud: &Hud) {
        if let Some(s) = self.state.as_mut() {
            s.draw(hud);
        }
    }

    fn destroy(&mut self) {
        self.state = None;
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
    width: u32,
    height: u32,
    visible: bool,
    configured: bool,
}

impl State {
    fn connect() -> Result<State, String> {
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

        let height = PAD + (BAR_H + PAD) * MAX_ROWS;
        layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
        layer.set_size(WIDTH, height);
        layer.set_margin(12, 12, 0, 0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);

        // Click-through: an EMPTY input region means no pointer events are routed
        // to this surface — they pass to the game underneath. Combined with
        // KeyboardInteractivity::None, the HUD is fully non-interactive.
        let empty_region = compositor.wl_compositor().create_region(&qh, ());
        layer.wl_surface().set_input_region(Some(&empty_region));

        layer.commit();

        let pool = SlotPool::new((WIDTH * height * 4) as usize, &shm)
            .map_err(|e| format!("slot pool: {e}"))?;

        let mut state = State {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            shm,
            pool,
            layer,
            conn: conn.clone(),
            width: WIDTH,
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
        Ok(state)
    }

    fn color(kind: ToastKind) -> u32 {
        // 0xAARRGGBB premultiplied-ish; alpha kept high for legibility.
        match kind {
            ToastKind::Saved => 0xCC2E7D32,     // green
            ToastKind::Recording => 0xCCC62828, // red
            ToastKind::Stopped => 0xCC616161,   // grey
            ToastKind::Error => 0xCCB71C1C,     // dark red
        }
    }

    fn draw(&mut self, hud: &Hud) {
        if !self.configured {
            return;
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

        // Clear to transparent.
        for px in canvas.chunks_exact_mut(4) {
            px.copy_from_slice(&[0, 0, 0, 0]);
        }

        if self.visible && hud.has_content() {
            let mut row = 0u32;

            // Buffer-active indicator as the first slim bar.
            if hud.buffer_active {
                fill_bar(canvas, w, row, 0x66000000 | 0x004CAF50); // translucent green
                row += 1;
            }
            for toast in hud
                .toasts()
                .iter()
                .take((MAX_ROWS.saturating_sub(row)) as usize)
            {
                fill_bar(canvas, w, row, Self::color(toast.kind));
                row += 1;
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

/// Paint a single rounded-ish (plain rect) bar at row index `row`.
fn fill_bar(canvas: &mut [u8], width: u32, row: u32, argb: u32) {
    let y0 = PAD + row * (BAR_H + PAD);
    let a = ((argb >> 24) & 0xff) as u8;
    let r = ((argb >> 16) & 0xff) as u8;
    let g = ((argb >> 8) & 0xff) as u8;
    let b = (argb & 0xff) as u8;
    for y in y0..(y0 + BAR_H) {
        for x in PAD..(width - PAD) {
            let idx = ((y * width + x) * 4) as usize;
            if idx + 4 <= canvas.len() {
                // wl_shm Argb8888 is little-endian BGRA in memory.
                canvas[idx] = b;
                canvas[idx + 1] = g;
                canvas[idx + 2] = r;
                canvas[idx + 3] = a;
            }
        }
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
