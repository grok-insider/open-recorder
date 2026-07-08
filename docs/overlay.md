# Overlay strategy

"Overlay" means two different UI surfaces in this project. They use different
techniques, and the cross-platform story differs. This doc records the design.

## Two surfaces

### 1. Clip library / manager — a normal window

Browsing, playing, trimming, deleting clips is a regular app window shown on
demand. It does **not** need true overlay rendering.

On tiling Wayland/X11 window managers the cleanest "overlay" behavior is a
**special workspace** — the compositor floats the window over the current view
on a keypress and hides it again. This is already the pattern used on the dev
box for Discord/Spotify:

```ini
# Hyprland (already in use for Discord/Spotify):
workspace = special:social
windowrule = workspace special:social, match:class ^([Dd]iscord|[Vv]esktop)$
bind = $mainMod, O, togglespecialworkspace, social

# open-recorder library window (planned):
workspace = special:clips
windowrule = workspace special:clips, match:class ^(open-recorder)$
bind = $mainMod, C, togglespecialworkspace, clips
```

No overlay code — a window rule plus a keybind. The compositor does the work.
i3 has the equivalent via the scratchpad.

### 2. HUD / on-screen feedback — a true overlay

"Buffer active" indicators and "Clip saved!" toasts must float **over fullscreen
games**, always on top, transparent, and click-through. A special workspace
can't do this (it replaces the view rather than floating over it). This needs a
real overlay surface, and here the platform split is unavoidable:

| Platform | Technique |
|----------|-----------|
| Wayland (Hyprland/Sway/KDE) | `wlr-layer-shell` `OVERLAY` layer — the same protocol waybar/swaync use. Composites over fullscreen, click-through via empty input region. |
| X11 (i3) | Transparent, undecorated, always-on-top window + `set_cursor_hittest(false)`. Requires a compositor (e.g. picom) for transparency. |
| Windows | `WS_EX_LAYERED \| WS_EX_TRANSPARENT` + topmost. |

These live behind the `Overlay` trait in `ord-overlay` (see `docs/backends.md`).

The pressed-key demo overlay is part of the same HUD process, not the recorder
engine. When `[overlay.pressed_keys].enabled = true`, `ord-hud` reads raw Linux
keyboard events and draws individual IBM Plex Mono keycaps on a click-through,
output-sized layer-shell surface so PipeWire captures them naturally. The
position can be one of the presets or a custom normalized point edited in
`ord-ui` Settings, with live size, opacity, and 2D rotation controls. It is off
by default because raw input access is effectively key-capture permission.

## GUI toolkit: egui

One toolkit for both surfaces. Chosen over `iced` because:

- egui has the mature, proven **cross-platform overlay + click-through** story
  (`egui_overlay`, `egui_window_glfw_passthrough`, `screen_overlay`); iced's
  layer-shell/overlay story is less mature.
- egui is what the closest analog (Lapse) ships with.
- One toolkit keeps the library window and the HUD consistent and reduces deps.

Tradeoff accepted: iced looks slightly more "native" for the library window, but
that window lives in a special workspace and egui renders it fine. The overlay
requirement dominates the choice.

## Cross-platform stance

- The **UI/overlay layer is cross-platform from day one** (egui runs on Linux
  and Windows; the `Overlay` trait abstracts the surface).
- The **capture/encode engine is Linux-only for now** (`waycap-rs` =
  PipeWire/Wayland + NVENC/VAAPI). Windows needs a separate DXGI→NVENC
  `CaptureBackend`. That is a future milestone, designed-for but not promised in
  v1.

Net: **ship Linux-first**, keep the seams clean so Windows is an additive
backend, not a rewrite.
