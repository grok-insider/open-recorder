//! GPU NV12 → RGB renderer for the editor preview ("Tier B").
//!
//! The decode thread produces NV12 frames (native output of the NVDEC `*_cuvid`
//! decoder; software frames are converted to NV12 cheaply). Instead of a CPU
//! `swscale` NV12 → RGBA per frame, we upload the two NV12 planes (Y as `R8`,
//! interleaved UV as `RG8`) to GL textures and convert to RGB **in a fragment
//! shader**, with the GPU sampler doing the final scale to the preview rect for
//! free. This removes the per-frame CPU colour-convert and cuts upload bandwidth
//! ~2.7× (NV12 is 1.5 B/px vs RGBA 4 B/px).
//!
//! Rendering happens inside an egui [`egui::PaintCallback`] where the glow
//! context is current. GL resources are created lazily and kept in a process
//! global (one shader + two textures, reused across clips — only one preview is
//! visible at a time), so opening/closing clips never leaks or needs GL teardown
//! on the UI thread.

use std::sync::{Arc, Mutex, OnceLock};

use eframe::{egui, egui_glow, glow};
use glow::HasContext;

/// One NV12 frame ready to upload. Planes are tight-packed (no row padding).
pub struct Nv12 {
    /// Luma width / height (UV plane is `w/2 × h/2`, interleaved).
    pub w: usize,
    pub h: usize,
    /// Y plane, `w * h` bytes.
    pub y: Vec<u8>,
    /// Interleaved Cb/Cr plane, `w * (h/2)` bytes.
    pub uv: Vec<u8>,
    /// Presentation time (seconds) — used to skip re-uploading an unchanged frame.
    pub pts: f64,
    /// Full-range (JPEG) vs limited-range (MPEG) luma/chroma.
    pub full_range: bool,
    /// BT.601 matrix (SD) vs BT.709 (HD, the default for our captures).
    pub bt601: bool,
}

/// Process-global GL resources (lazily created in the first paint callback).
static GL: OnceLock<Mutex<Option<GlVideo>>> = OnceLock::new();

struct GlVideo {
    program: glow::Program,
    vao: glow::VertexArray,
    tex_y: glow::Texture,
    tex_uv: glow::Texture,
    dims: (usize, usize),
    uploaded_pts: f64,
    u_tex_y: Option<glow::UniformLocation>,
    u_tex_uv: Option<glow::UniformLocation>,
    u_full: Option<glow::UniformLocation>,
    u_bt601: Option<glow::UniformLocation>,
}

/// Build an egui paint callback that draws `pending` (the latest NV12 frame) into
/// `rect` on the GPU. Cheap to call every frame; the heavy GL objects are global.
pub fn paint_callback(rect: egui::Rect, pending: Arc<Mutex<Option<Nv12>>>) -> egui::PaintCallback {
    let cb = egui_glow::CallbackFn::new(move |_info, painter| {
        let gl = painter.gl();
        let cell = GL.get_or_init(|| Mutex::new(None));
        let mut guard = cell.lock().unwrap();
        if guard.is_none() {
            *guard = unsafe { GlVideo::new(gl) };
        }
        if let Some(video) = guard.as_mut() {
            if let Some(frame) = pending.lock().unwrap().as_ref() {
                unsafe { video.draw(gl, frame) };
            }
        }
    });
    egui::PaintCallback {
        rect,
        callback: Arc::new(cb),
    }
}

impl GlVideo {
    unsafe fn new(gl: &glow::Context) -> Option<Self> {
        let es = gl.version().is_embedded;
        let header = if es {
            "#version 300 es\nprecision highp float;\nprecision highp sampler2D;\n"
        } else {
            "#version 330 core\n"
        };
        let vs = format!("{header}{VERT}");
        let fs = format!("{header}{FRAG}");
        let program = link_program(gl, &vs, &fs)?;
        let vao = gl.create_vertex_array().ok()?;
        let tex_y = make_tex(gl)?;
        let tex_uv = make_tex(gl)?;
        Some(GlVideo {
            u_tex_y: gl.get_uniform_location(program, "tex_y"),
            u_tex_uv: gl.get_uniform_location(program, "tex_uv"),
            u_full: gl.get_uniform_location(program, "u_full"),
            u_bt601: gl.get_uniform_location(program, "u_bt601"),
            program,
            vao,
            tex_y,
            tex_uv,
            dims: (0, 0),
            uploaded_pts: f64::NAN,
        })
    }

    unsafe fn upload(&mut self, gl: &glow::Context, f: &Nv12) {
        if self.dims == (f.w, f.h) && self.uploaded_pts == f.pts {
            return; // same frame already on the GPU
        }
        // CRITICAL: egui_glow may leave a PIXEL_UNPACK_BUFFER (PBO) bound from its
        // own texture uploads. With a PBO bound, glTexImage2D treats our slice
        // pointer as a byte OFFSET into the PBO, reading out of bounds in VRAM →
        // GPU fault. Unbind it so our client-memory uploads are read correctly.
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
        // R8/RG8 rows aren't 4-byte aligned for odd widths; tight rows.
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        let resized = self.dims != (f.w, f.h);

        gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_y));
        if resized {
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::R8 as i32,
                f.w as i32,
                f.h as i32,
                0,
                glow::RED,
                glow::UNSIGNED_BYTE,
                Some(&f.y),
            );
        } else {
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                f.w as i32,
                f.h as i32,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(&f.y),
            );
        }
        gl.generate_mipmap(glow::TEXTURE_2D);

        let cw = (f.w / 2) as i32;
        let ch = (f.h / 2) as i32;
        gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_uv));
        if resized {
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RG8 as i32,
                cw,
                ch,
                0,
                glow::RG,
                glow::UNSIGNED_BYTE,
                Some(&f.uv),
            );
        } else {
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                cw,
                ch,
                glow::RG,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(&f.uv),
            );
        }
        gl.generate_mipmap(glow::TEXTURE_2D);

        self.dims = (f.w, f.h);
        self.uploaded_pts = f.pts;
    }

    unsafe fn draw(&mut self, gl: &glow::Context, f: &Nv12) {
        if f.w == 0 || f.h == 0 {
            return;
        }
        self.upload(gl, f);

        gl.use_program(Some(self.program));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_y));
        gl.uniform_1_i32(self.u_tex_y.as_ref(), 0);
        gl.active_texture(glow::TEXTURE1);
        gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_uv));
        gl.uniform_1_i32(self.u_tex_uv.as_ref(), 1);
        gl.uniform_1_f32(self.u_full.as_ref(), if f.full_range { 1.0 } else { 0.0 });
        gl.uniform_1_f32(self.u_bt601.as_ref(), if f.bt601 { 1.0 } else { 0.0 });

        // Opaque video: don't blend (egui leaves blending on for its meshes).
        gl.disable(glow::BLEND);
        gl.bind_vertex_array(Some(self.vao));
        gl.draw_arrays(glow::TRIANGLES, 0, 3);

        // Restore the state egui expects for its subsequent meshes.
        gl.bind_vertex_array(None);
        gl.enable(glow::BLEND);
        gl.active_texture(glow::TEXTURE0);
    }
}

unsafe fn make_tex(gl: &glow::Context) -> Option<glow::Texture> {
    let tex = gl.create_texture().ok()?;
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    // Trilinear min filter: with mipmaps this makes downscaling the full-res
    // frame to the (smaller) preview widget crisp + alias-free at any window size.
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MIN_FILTER,
        glow::LINEAR_MIPMAP_LINEAR as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MAG_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_S,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_T,
        glow::CLAMP_TO_EDGE as i32,
    );
    Some(tex)
}

unsafe fn link_program(gl: &glow::Context, vs: &str, fs: &str) -> Option<glow::Program> {
    let program = gl.create_program().ok()?;
    let shaders = [(glow::VERTEX_SHADER, vs), (glow::FRAGMENT_SHADER, fs)];
    let mut handles = Vec::new();
    for (kind, src) in shaders {
        let sh = gl.create_shader(kind).ok()?;
        gl.shader_source(sh, src);
        gl.compile_shader(sh);
        if !gl.get_shader_compile_status(sh) {
            eprintln!(
                "ord-ui: NV12 shader compile error: {}",
                gl.get_shader_info_log(sh)
            );
            return None;
        }
        gl.attach_shader(program, sh);
        handles.push(sh);
    }
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        eprintln!(
            "ord-ui: NV12 program link error: {}",
            gl.get_program_info_log(program)
        );
        return None;
    }
    for sh in handles {
        gl.detach_shader(program, sh);
        gl.delete_shader(sh);
    }
    Some(program)
}

/// Full-screen triangle; `v_uv` flipped so image row 0 (top) maps to screen top.
const VERT: &str = r#"
out vec2 v_uv;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_uv = vec2(p.x, 1.0 - p.y);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

/// NV12 → RGB with selectable range (limited/full) and matrix (BT.709/BT.601).
const FRAG: &str = r#"
in vec2 v_uv;
out vec4 frag;
uniform sampler2D tex_y;
uniform sampler2D tex_uv;
uniform float u_full;   // 1 = full range (JPEG), 0 = limited (MPEG)
uniform float u_bt601;  // 1 = BT.601, 0 = BT.709
void main() {
    float Y = texture(tex_y, v_uv).r;
    vec2 C = texture(tex_uv, v_uv).rg;
    float y, cb, cr;
    if (u_full > 0.5) {
        y = Y;
        cb = C.x - 0.5;
        cr = C.y - 0.5;
    } else {
        y = (Y - 0.0625) * 1.164384;
        cb = (C.x - 0.5) * 1.138393;
        cr = (C.y - 0.5) * 1.138393;
    }
    vec3 rgb;
    if (u_bt601 > 0.5) {
        rgb = vec3(y + 1.402 * cr,
                   y - 0.344136 * cb - 0.714136 * cr,
                   y + 1.772 * cb);
    } else {
        rgb = vec3(y + 1.5748 * cr,
                   y - 0.187324 * cb - 0.468124 * cr,
                   y + 1.8556 * cb);
    }
    frag = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
"#;
