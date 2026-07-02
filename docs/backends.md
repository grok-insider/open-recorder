# Backends: the platform/engine seams

open-recorder stays portable by isolating everything OS- or engine-specific
behind two traits. Code outside a backend module programs against the trait,
never a concrete implementation. Both traits are implemented and
hardware-verified; the signatures below are the current code
(`crates/ord-core/src/backend.rs`, `crates/ord-overlay/src/lib.rs`).

## `CaptureBackend` — capture + hardware encode

Produces a stream of **encoded** video packets (and audio) from a capture
target, using GPU hardware encode where available.

```rust
/// The encoded streams a backend delivers once started: always video, optionally
/// a mixed audio track.
pub struct CaptureStreams {
    pub video: Receiver<EncodedFrame>,
    pub audio: Option<Receiver<EncodedAudioFrame>>,
}

/// A source of hardware-encoded frames. Implementations own the capture -> encode
/// pipeline. The hot path (delivering frames) must not block or copy beyond the
/// encoded packet itself.
pub trait CaptureBackend: Send {
    /// Begin capturing. Encoded frames are delivered on the returned receivers
    /// until [`stop`](CaptureBackend::stop) is called.
    fn start(&mut self) -> Result<CaptureStreams, BackendError>;

    /// Stop capturing and release resources.
    fn stop(&mut self) -> Result<(), BackendError>;

    /// Negotiated video stream parameters.
    fn params(&self) -> StreamParams;

    /// Negotiated audio stream parameters, if audio capture is active.
    fn audio_params(&self) -> Option<AudioParams> {
        None
    }

    /// Whether capture is currently running.
    fn is_running(&self) -> bool;
}
```

`EncodedFrame` carries the encoded payload, `is_keyframe: bool`, and pts/dts in
the backend's time base (`StreamParams::time_base_den`). The ring buffer in
`ord-core` consumes these; the keyframe flag is what makes "save last N"
lossless.

| Implementation | Platform | Path | Status |
|----------------|----------|------|--------|
| `WaycapBackend` | Linux/Wayland | `waycap-rs`: PipeWire DMA-BUF → NVENC | **shipped, hardware-verified** |
| `MockBackend`   | any (tests) | deterministic synthetic frames, no GPU | **shipped, the CI default** |
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
/// A transparent, always-on-top, click-through HUD surface.
///
/// Deliberately minimal: the HUD renders every state from [`Hud`], so the trait
/// is create/render/destroy. (A `set_visible` toggle was removed as speculative
/// API — an empty `Hud` renders nothing, which is "hidden".)
pub trait Overlay {
    /// Create the surface on the active output.
    fn create(&mut self) -> Result<(), OverlayError>;
    /// Render the current HUD state (called each tick by the owner). `now_ms` is
    /// the same monotonic clock the toasts were created with, so the renderer can
    /// drive fade/slide animations.
    fn render(&mut self, hud: &Hud, now_ms: u64);
    /// Tear down the surface.
    fn destroy(&mut self);
}
```

| Implementation | Platform | Surface | Status |
|----------------|----------|---------|--------|
| `LayerShellOverlay` | Wayland | `wlr-layer-shell` OVERLAY layer, empty input region for click-through | **shipped, hardware-verified** (behind `layershell`) |
| `NoopOverlay`       | any | records calls; headless tests/dev | shipped |
| `X11Overlay`        | X11 | override-redirect, always-on-top, `cursor_hittest(false)` | future |
| `Win32Overlay`      | Windows | `WS_EX_LAYERED \| WS_EX_TRANSPARENT`, topmost | future |

## Platform cfg-gating (cross-platform Phase 0, v0.3.0)

The whole workspace compiles on non-Linux; the gates decide what is real:

- **`WaycapBackend`** is gated `#[cfg(all(feature = "waycap",
  target_os = "linux"))]`, and the `waycap-rs` dependency itself is
  target-gated in `ord-core`'s `Cargo.toml` — so `--features waycap` on
  another OS resolves to nothing instead of failing the build.
- **`DiskFrameStore`** is `#[cfg(unix)]` (it uses positioned I/O via
  `std::os::unix::fs::FileExt`); off-unix the engine falls back to the RAM
  ring, which loses nothing while capture is Linux-only.
- **`MockBackend`** is the default everywhere else — CI, non-Linux builds, and
  any build without `--features waycap`.

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
