# Backends: the platform/engine seams

open-recorder stays portable by isolating everything OS- or engine-specific
behind two traits. Code outside a backend module programs against the trait,
never a concrete implementation. These signatures are a design target for the
implementation phases, not yet code.

## `CaptureBackend` — capture + hardware encode

Produces a stream of **encoded** video packets (and audio) from a capture
target, using GPU hardware encode where available.

```rust
/// A source of hardware-encoded frames. Implementations own the
/// capture → encode pipeline and must not copy frames on the hot path.
pub trait CaptureBackend: Send {
    /// Begin capturing the configured target. Encoded packets are delivered
    /// to the returned receivers until `stop` is called.
    fn start(&mut self, cfg: &CaptureConfig) -> Result<(), CaptureError>;

    /// Stop capturing and release GPU/portal resources.
    fn stop(&mut self) -> Result<(), CaptureError>;

    /// Encoded video packets, each tagged with keyframe state and PTS.
    fn video(&self) -> Receiver<EncodedFrame>;

    /// Encoded audio packets (Opus), if audio capture is enabled.
    fn audio(&self) -> Option<Receiver<EncodedAudio>>;

    /// Negotiated stream parameters (resolution, fps, codec, time base).
    fn params(&self) -> StreamParams;
}
```

`EncodedFrame` carries `data: Bytes`, `is_keyframe: bool`, `pts`. The ring
buffer in `ord-core` consumes these; the keyframe flag is what makes
"save last N" lossless.

| Implementation | Platform | Path | Status |
|----------------|----------|------|--------|
| `WaycapBackend` | Linux/Wayland | `waycap-rs`: PipeWire DMA-BUF → NVENC/VAAPI | **v1 target** |
| `MockBackend`   | any (tests) | deterministic synthetic frames, no GPU | **required for CI** |
| `DxgiBackend`   | Windows | DXGI/WGC capture → NVENC | future |

`MockBackend` is not optional: the daemon and core logic must be testable
without a GPU or a live Wayland session. It emits a scripted frame sequence
(controlled keyframe cadence, PTS) so ring-buffer and save-boundary tests are
deterministic.

## `Overlay` — on-screen HUD surface

A transparent, always-on-top, click-through surface for HUD feedback. The clip
**library** window does NOT use this (it's a normal window in a special
workspace); only the HUD does.

```rust
/// A click-through, always-on-top surface for HUD rendering.
pub trait Overlay {
    /// Create the overlay surface on the active output.
    fn create(&mut self, cfg: &OverlayConfig) -> Result<(), OverlayError>;

    /// Show/hide without destroying the surface (for transient toasts).
    fn set_visible(&mut self, visible: bool);

    /// Provide the egui frame to render this tick.
    fn render(&mut self, ui: impl FnOnce(&egui::Context));

    /// Tear down the surface and release compositor resources.
    fn destroy(&mut self);
}
```

| Implementation | Platform | Surface |
|----------------|----------|---------|
| `LayerShellOverlay` | Wayland | `wlr-layer-shell` OVERLAY layer, empty input region for click-through | **v1 target** |
| `X11Overlay`        | X11 | override-redirect, always-on-top, `cursor_hittest(false)` | future |
| `Win32Overlay`      | Windows | `WS_EX_LAYERED \| WS_EX_TRANSPARENT`, topmost | future |

## Why two traits, not one

Capture and presentation vary independently: a Windows port needs a new
`CaptureBackend` but could reuse much of the egui UI; a new compositor quirk
touches `Overlay` without touching capture. Keeping them separate keeps each
small and independently testable.

## Selection

Backends are selected at startup from config + runtime detection
(`WAYLAND_DISPLAY` / `DISPLAY` / OS). Selection logic is the only place allowed
to name concrete backend types; everything downstream holds a
`Box<dyn CaptureBackend>` / `Box<dyn Overlay>`.
