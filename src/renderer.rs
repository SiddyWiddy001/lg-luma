//! A small GLES2-compatible GPU painter. Rust owns vector glyphs and the draw list;
//! SDL only supplies the context. No browser, LVGL or Qt runtime.
use crate::{
    font::Font,
    raster::{Size, Viewport},
    state::Rect,
    vector,
};
use gl::types::*;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, CString},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

pub type Color = [f32; 4];
pub const WHITE: Color = [0.96, 0.96, 0.96, 1.];
const GLYPH_GEOMETRY_BUDGET: usize = 8 * 1024 * 1024;
const TEXT_GEOMETRY_BUDGET: usize = 32 * 1024 * 1024;
const TEXTURE_BUDGET: usize = 48 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct TextStyle {
    pub size: f32,
    pub weight: u16,
    /// CSS letter-spacing in em, not pixels.
    pub tracking: f32,
    pub line_height: f32,
    pub align_right: bool,
}
impl TextStyle {
    pub const fn new(size: f32) -> Self {
        Self {
            size,
            weight: 400,
            tracking: 0.,
            line_height: 1.2,
            align_right: false,
        }
    }
    pub const fn weight(mut self, weight: u16) -> Self {
        self.weight = weight;
        self
    }
    pub const fn tracking(mut self, tracking: f32) -> Self {
        self.tracking = tracking;
        self
    }
    pub const fn leading(mut self, height: f32) -> Self {
        self.line_height = height;
        self
    }
    pub const fn right(mut self) -> Self {
        self.align_right = true;
        self
    }
}

/// A smoked-glass tile plate painted in one analytic pass: the vertical tint, a soft
/// inner rim, a crisp rim at the edge and the top reflection. It used to be four full-face
/// draws (gradient, 3px border, 1px border, reflection), which cost four times the fragments.
#[derive(Clone, Copy, PartialEq)]
pub struct Glass {
    pub top: Color,
    pub bottom: Color,
    /// Crisp 1px rim right at the plate edge.
    pub rim: f32,
    /// Soft 3px inner rim, just inside the edge.
    pub inner: f32,
    /// Top reflection strength, fading to nothing at the bottom edge.
    pub reflection: f32,
}

#[derive(Clone)]
pub enum Draw {
    Shape(Rect, f32, f32, Color),
    RoundedGradient(Rect, f32, Color, Color),
    Glass(Rect, f32, Glass),
    Shadow(Rect, f32, f32, Color),
    /// Focus bloom: a continuous exponential halo around `rect`, fading over `blur` pixels.
    /// Replaces the stacked flat-alpha rings that banded on dark wallpapers.
    Glow(Rect, f32, f32, Color),
    Text(String, Rect, TextStyle, Color, bool),
    Image(String, Rect, f32, bool),
    TintedImage(String, Rect, f32, bool, Color),
    /// Installed icon and its backdrop, composited before opacity and edge fading.
    /// The glass plate paints the icon alone over its own gradient (`AppIcon`), so this
    /// composited path is unused here. It is kept, with its renderer support and tests, so the
    /// upstream look can be restored without rebuilding the artwork decode.
    #[allow(dead_code)]
    AppImage(Arc<crate::app_artwork::Artwork>, Rect, f32, Color),
    /// Installed icon alone, alpha preserved, drawn over a caller-painted plate.
    AppIcon(Arc<crate::app_artwork::Artwork>, Rect, Color),
    Clip(Option<Rect>),
    EdgeFade(Option<[f32; 4]>),
    /// Horizontal counterpart: [left fade-in start, end, right fade-out start, end].
    EdgeFadeX(Option<[f32; 4]>),
}

struct Texture {
    id: GLuint,
    width: u32,
    height: u32,
    frame: u64,
    /// The size cap this was uploaded under. A larger viewport later means a larger cap, and then
    /// the cached copy is dropped and uploaded again instead of staying over-shrunk.
    cap: (u32, u32),
}
impl Drop for Texture {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteTextures(1, &self.id);
        }
    }
}

struct Glyph {
    geometry: Vec<luma_vector_geometry::Vertex>,
    frame: u64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct TextKey {
    text: String,
    weight: u16,
    // Logical size, tracking, line height, available width and height.
    layout: [u32; 5],
    flags: u8,
}

struct TextRun {
    /// Raw nine-float vertices (position, offset, origin, coverage); colour is
    /// supplied per submission so shadow and foreground runs can share a batch.
    vertices: Vec<f32>,
    missing_glyphs: usize,
    failures: usize,
    frame: u64,
}

#[derive(Clone, Serialize)]
pub struct GraphicsInfo {
    pub vendor: String,
    pub renderer: String,
    pub version: String,
    pub max_texture_size: u32,
    pub max_viewport: [i32; 2],
    pub fragment_highp: bool,
}

#[derive(Clone, Serialize)]
pub struct RenderStats {
    pub target: Size,
    pub viewport: Viewport,
    pub font_antialiasing: &'static str,
    pub multisample_samples: u32,
    pub max_logical_font_px: f32,
    pub max_physical_font_px: f32,
    pub cached_glyphs: usize,
    pub glyph_tessellations: usize,
    /// Milliseconds this frame spent on first-use work: glyph outlines, logo meshes, images.
    pub glyph_ms: f64,
    pub vector_load_ms: f64,
    pub image_load_ms: f64,
    pub glyph_failures: usize,
    pub missing_glyphs: usize,
    pub missing_images: usize,
    pub glyph_geometry_bytes: usize,
    pub cached_text_runs: usize,
    pub text_geometry_bytes: usize,
    pub text_triangles: usize,
    pub artwork_bytes: usize,
    pub vector_meshes: usize,
    pub vector_bytes: usize,
    pub vector_triangles: usize,
    pub high_resolution_wallpaper: bool,
    pub draw_calls: usize,
    pub cpu_submission_ms: f64,
    /// Where `cpu_submission_ms` went: scene assembly, clear, buffer uploads, GL setup, the
    /// draw loop split by batch kind, and the tail (cache trimming and stats).
    pub build_ms: f64,
    pub clear_ms: f64,
    pub upload_ms: f64,
    pub upload_bytes: usize,
    pub setup_ms: f64,
    pub draw_paint_ms: f64,
    pub draw_vector_ms: f64,
    pub draw_text_ms: f64,
    pub draw_ms: f64,
    pub tail_ms: f64,
    pub paint_batches: usize,
    pub vector_batches: usize,
    pub text_batches: usize,
    /// Share of the drawable redrawn in the last frame (1 without partial redraw).
    pub redrawn_fraction: f64,
}
impl RenderStats {
    fn new(target: Size) -> Self {
        Self {
            target,
            viewport: Viewport::new(target),
            font_antialiasing: "vector grayscale coverage",
            multisample_samples: 0,
            max_logical_font_px: 0.,
            max_physical_font_px: 0.,
            cached_glyphs: 0,
            glyph_tessellations: 0,
            glyph_ms: 0.,
            vector_load_ms: 0.,
            image_load_ms: 0.,
            glyph_failures: 0,
            missing_glyphs: 0,
            missing_images: 0,
            glyph_geometry_bytes: 0,
            cached_text_runs: 0,
            text_geometry_bytes: 0,
            text_triangles: 0,
            artwork_bytes: 0,
            vector_meshes: 0,
            vector_bytes: 0,
            vector_triangles: 0,
            high_resolution_wallpaper: false,
            draw_calls: 0,
            cpu_submission_ms: 0.,
            build_ms: 0.,
            clear_ms: 0.,
            upload_ms: 0.,
            upload_bytes: 0,
            setup_ms: 0.,
            draw_paint_ms: 0.,
            draw_vector_ms: 0.,
            draw_text_ms: 0.,
            draw_ms: 0.,
            tail_ms: 0.,
            paint_batches: 0,
            vector_batches: 0,
            text_batches: 0,
            redrawn_fraction: 1.,
        }
    }
}

/// How the paint shader treats sampled texture colour. `Monochrome` turns installed app
/// icons into a cool dark-mode grey without touching shape or alpha.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PaintMode {
    Normal,
    Monochrome,
    /// Focus bloom: the shader draws a continuous exponential halo (see `Draw::Glow`).
    Glow,
    /// Analytic glass plate (see `Draw::Glass`). Alphas are stored as bits for exact merging.
    Glass {
        rim: u32,
        inner: u32,
        reflection: u32,
    },
}

enum BatchKind {
    Paint {
        texture: GLuint,
        start: usize,
        count: usize,
        backdrop: Option<[[u8; 3]; 2]>,
        mode: PaintMode,
    },
    Vector {
        start: usize,
        count: usize,
        /// Curated brand logos render as cool monochrome glyphs on the glass plate.
        mono: bool,
        /// Logos are merged into the final layer; UI glyphs keep their original order.
        deferred: bool,
    },
    Text {
        start: usize,
        count: usize,
        /// Physical emboldening for this batch, stored as bits for exact merging.
        embolden: u32,
    },
}

struct Batch {
    kind: BatchKind,
    clip: Option<Rect>,
    // [top fade-in, top end, bottom start, bottom fade-out, left ×2, right ×2]; 0 = off.
    fade: Option<[f32; 8]>,
}

struct DeferredVector {
    start: usize,
    count: usize,
    mono: bool,
    clip: Option<Rect>,
    fade: Option<[f32; 8]>,
}

struct DeferredText {
    start: usize,
    count: usize,
    embolden: u32,
    clip: Option<Rect>,
    fade: Option<[f32; 8]>,
}

/// Uniform locations are stable for a program, so they are looked up once instead of
/// once per batch (the Apps captions alone submit dozens of text batches per frame).
struct PaintUniforms {
    aa: GLint,
    texture: GLint,
    edge_fade: GLint,
    edge_fade_x: GLint,
    app: GLint,
    icon_tint: GLint,
    app_top: GLint,
    app_bottom: GLint,
    glass: GLint,
}
struct MeshUniforms {
    edge_fade: GLint,
    edge_fade_x: GLint,
    fringe: GLint,
    embolden: GLint,
    transform: GLint,
    pixel_scale: GLint,
    tint: GLint,
    mono: GLint,
}
struct Uniforms {
    paint: PaintUniforms,
    vector: MeshUniforms,
    text: MeshUniforms,
}

fn full_screen(rect: &Rect) -> bool {
    rect.x <= 0. && rect.y <= 0. && rect.x + rect.w >= 1920. && rect.y + rect.h >= 1080.
}

/// A full-screen paint command, such as the modal/Recents dim layer, can cover
/// text that was deferred earlier. Flush those deferred runs before it so the
/// parent interface stays beneath the overlay.
fn full_screen_paint(command: &Draw) -> bool {
    match command {
        Draw::Shape(rect, ..)
        | Draw::RoundedGradient(rect, ..)
        | Draw::Glass(rect, ..)
        | Draw::Image(_, rect, ..)
        | Draw::TintedImage(_, rect, ..) => full_screen(rect),
        _ => false,
    }
}

/// An opaque, untinted, square full-screen image: the wallpaper that scrims can fold into.
fn plain_wallpaper(command: &Draw) -> Option<(&str, Rect)> {
    match command {
        Draw::Image(path, rect, radius, false) if full_screen(rect) && *radius <= 0. => {
            Some((path.as_str(), *rect))
        }
        Draw::TintedImage(path, rect, radius, false, tint)
            if full_screen(rect) && *radius <= 0. && *tint == [1.; 4] =>
        {
            Some((path.as_str(), *rect))
        }
        _ => None,
    }
}

/// A full-screen translucent fill without corners or border: a flat colour or a vertical
/// gradient (straight alpha, interpolated like the paint shader's vertex colours).
enum Scrim {
    Flat(Color),
    Gradient(Color, Color),
}
impl Scrim {
    fn of(command: &Draw) -> Option<Self> {
        match command {
            Draw::Shape(rect, radius, border, color)
                if full_screen(rect) && *radius <= 0. && *border <= 0. =>
            {
                Some(Self::Flat(*color))
            }
            Draw::RoundedGradient(rect, radius, top, bottom)
                if full_screen(rect) && *radius <= 0. =>
            {
                Some(Self::Gradient(*top, *bottom))
            }
            _ => None,
        }
    }
    fn at(&self, t: f32) -> Color {
        match self {
            Self::Flat(color) => *color,
            Self::Gradient(top, bottom) => {
                std::array::from_fn(|i| top[i] + (bottom[i] - top[i]) * t)
            }
        }
    }
}

fn same_clip(a: Option<Rect>, b: Option<Rect>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.x == b.x && a.y == b.y && a.w == b.w && a.h == b.h,
        _ => false,
    }
}

fn intersect(a: [i32; 4], b: [i32; 4]) -> [i32; 4] {
    let x = a[0].max(b[0]);
    let y = a[1].max(b[1]);
    let right = (a[0] + a[2]).min(b[0] + b[2]);
    let top = (a[1] + a[3]).min(b[1] + b[3]);
    [x, y, (right - x).max(0), (top - y).max(0)]
}

/// Square images skip the rounded-rectangle distance field: a negative radius selects the
/// shader's plain texture path. Per pixel that field cost as much as the rest of the Home scene
/// on a webOS 5 Mali for the full-screen wallpaper.
fn plain_radius(radius: f32) -> f32 {
    if radius > 0. { radius } else { -1. }
}

/// Measurement aid: LUMA_PROFILE_SKIP=wallpaper,image,app,shape,gradient,shadow,text or all
/// leaves those draws out, to find which part of the scene costs the GPU time.
fn profile_skip(command: &Draw) -> bool {
    static SKIP: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let skip = SKIP.get_or_init(|| {
        std::env::var("LUMA_PROFILE_SKIP")
            .map(|value| value.split(',').map(str::to_owned).collect())
            .unwrap_or_default()
    });
    if skip.is_empty() {
        return false;
    }
    let kind = match command {
        Draw::Image(_, rect, ..) | Draw::TintedImage(_, rect, ..)
            if rect.w >= 1920. && rect.h >= 1080. =>
        {
            "wallpaper"
        }
        Draw::Image(..) | Draw::TintedImage(..) => "image",
        Draw::AppImage(..) => "app",
        Draw::AppIcon(..) => "app",
        Draw::Shape(..) => "shape",
        Draw::RoundedGradient(..) | Draw::Glass(..) => "gradient",
        Draw::Shadow(..) => "shadow",
        Draw::Glow(..) => "glow",
        Draw::Text(..) => "text",
        Draw::Clip(_) | Draw::EdgeFade(_) | Draw::EdgeFadeX(_) => return false,
    };
    skip.iter().any(|entry| entry == kind || entry == "all")
}

/// Stem emboldening for small text, in physical pixels per edge: thin stems get an opaque
/// core instead of only grey edge pixels, which a TV's 2x scale-up would soften further.
/// Full strength up to 16 physical pixels, none from 28. Zero in the default MSAA mode.
fn embolden(physical_size: f32, strength: f32) -> f32 {
    strength * ((28. - physical_size) / 12.).clamp(0., 1.)
}

fn snap_rect(rect: Rect, scale: f32) -> Rect {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var("LUMA_SNAP").as_deref() != Ok("0")) {
        return rect;
    }
    let scale = scale.max(0.01);
    let snap = |value: f32| (value * scale).round() / scale;
    let (x0, y0) = (snap(rect.x), snap(rect.y));
    // Never collapse a visible hairline.
    let x1 = snap(rect.x + rect.w).max(x0 + 1. / scale);
    let y1 = snap(rect.y + rect.h).max(y0 + 1. / scale);
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

fn uniform(program: GLuint, name: &CStr) -> GLint {
    unsafe { gl::GetUniformLocation(program, name.as_ptr()) }
}

/// Appends one vector mesh into a merged dynamic buffer with its transform and
/// tint baked into the vertices. The shader then uses a fixed [1, 1, 0, 0]
/// transform, so many logos can share one draw call without changing their
/// per-logo position, size or colour.
fn append_vector_run(
    vertices: &mut Vec<f32>,
    mesh: &vector::Mesh,
    rect: Rect,
    tint: Color,
) -> usize {
    let start = vertices.len() / 9;
    let sx = (rect.w / mesh.width).abs().max(1e-6);
    let sy = (rect.h / mesh.height).abs().max(1e-6);
    for chunk in mesh.data.chunks_exact(9) {
        let (px, py, ox, oy) = (chunk[0], chunk[1], chunk[2], chunk[3]);
        let dx = ox / sx;
        let dy = oy / sy;
        let direction = (dx * dx + dy * dy).sqrt();
        let length = (ox * ox + oy * oy).sqrt();
        let offset = if direction > 1e-6 {
            [dx / direction * length, dy / direction * length]
        } else {
            [0., 0.]
        };
        vertices.extend_from_slice(&[
            px * sx + rect.x,
            py * sy + rect.y,
            offset[0],
            offset[1],
            chunk[4] * tint[0],
            chunk[5] * tint[1],
            chunk[6] * tint[2],
            chunk[7] * tint[3],
            chunk[8],
        ]);
    }
    start
}

/// Appends one cached text run into the merged text buffer. The layout is
/// position (2), offset (2), origin (4), colour (4), coverage (1): the final
/// vertex colour is baked here so shadow and foreground runs can share a batch.
fn append_text_run(
    vertices: &mut Vec<f32>,
    run: &TextRun,
    origin: [f32; 2],
    color: Color,
    y_offset: f32,
) -> usize {
    let start = vertices.len() / 13;
    for chunk in run.vertices.chunks_exact(9) {
        vertices.extend_from_slice(&[
            chunk[0],
            chunk[1],
            chunk[2],
            chunk[3],
            chunk[4] + origin[0],
            chunk[5] + origin[1] + y_offset,
            0.,
            0.,
            color[0],
            color[1],
            color[2],
            color[3],
            chunk[8],
        ]);
    }
    start
}

pub struct Renderer {
    program: GLuint,
    vector_program: GLuint,
    text_program: GLuint,
    text_embolden: f32,
    buffer: GLuint,
    text_buffer: GLuint,
    vector_buffer: GLuint,
    logo_buffer: GLuint,
    vao: GLuint,
    white: Texture,
    font_directory: PathBuf,
    fonts: HashMap<u16, Font>,
    failed_fonts: HashSet<u16>,
    glyphs: HashMap<(char, u16), Glyph>,
    text_runs: HashMap<TextKey, TextRun>,
    textures: HashMap<String, Option<Texture>>,
    vectors: HashMap<String, Option<vector::Mesh>>,
    artwork_variants: HashMap<String, String>,
    vertices: Vec<f32>,
    text_vertices: Vec<f32>,
    vector_vertices: Vec<f32>,
    logo_vertices: Vec<f32>,
    pending_text: Vec<DeferredText>,
    pending_logo: Vec<DeferredVector>,
    batches: Vec<Batch>,
    clip: Option<Rect>,
    // [top fade-in, top end, bottom start, bottom fade-out, left ×2, right ×2]; 0 = off.
    fade: Option<[f32; 8]>,
    uniforms: Uniforms,
    frame: u64,
    stats: RenderStats,
    pub info: GraphicsInfo,
}

impl Renderer {
    fn set_fade(&mut self, offset: usize, fade: Option<[f32; 4]>) {
        let mut next = self.fade.unwrap_or([0.; 8]);
        next[offset..offset + 4].copy_from_slice(&fade.unwrap_or([0.; 4]));
        self.fade = (next != [0.; 8]).then_some(next);
    }

    pub fn new(font: &Path, gles: bool, text_embolden: f32) -> Result<Self, String> {
        let directory = font.parent().ok_or("Invalid font path")?;
        let mut fonts = HashMap::new();
        // Open only the common face. Metrics and outlines stay lazy even when
        // the much larger CJK fallback is needed later.
        fonts.insert(400, load_font(directory, 400)?);
        let mut max_texture_size = 0;
        let mut max_viewport = [0; 2];
        let mut precision = 0;
        unsafe {
            gl::GetIntegerv(gl::MAX_TEXTURE_SIZE, &mut max_texture_size);
            gl::GetIntegerv(gl::MAX_VIEWPORT_DIMS, max_viewport.as_mut_ptr());
            if gles && gl::GetShaderPrecisionFormat::is_loaded() {
                let mut range = [0; 2];
                gl::GetShaderPrecisionFormat(
                    gl::FRAGMENT_SHADER,
                    gl::HIGH_FLOAT,
                    range.as_mut_ptr(),
                    &mut precision,
                );
            }
        }
        let fragment_highp = !gles || precision > 0;
        let qualifier = if fragment_highp { "highp" } else { "mediump" };
        let vertex_prefix = if gles {
            format!("#version 100\nprecision highp float;\n#define PAINT_PRECISION {qualifier}\n")
        } else {
            "#version 150\n#define attribute in\n#define varying out\n#define PAINT_PRECISION\n"
                .into()
        };
        let fragment_prefix = if gles {
            format!(
                "#version 100\nprecision {qualifier} float;\n#define PAINT_PRECISION {qualifier}\n"
            )
        } else {
            "#version 150\n#define varying in\n#define texture2D texture\nout vec4 out_color;\n#define gl_FragColor out_color\n#define PAINT_PRECISION\n".into()
        };
        let program = link(
            &(vertex_prefix.clone() + include_str!("shaders/paint.vert")),
            &(fragment_prefix.clone() + include_str!("shaders/paint.frag")),
            &[c"a_position", c"a_uv", c"a_color", c"a_shape", c"a_params"],
        )?;
        let vector_program = match link(
            &(vertex_prefix.clone() + include_str!("shaders/vector.vert")),
            &(fragment_prefix.clone() + include_str!("shaders/vector.frag")),
            &[c"a_position", c"a_offset", c"a_color", c"a_coverage"],
        ) {
            Ok(program) => program,
            Err(error) => {
                unsafe { gl::DeleteProgram(program) };
                return Err(error);
            }
        };
        let text_program = match link(
            &(vertex_prefix + include_str!("shaders/text.vert")),
            &(fragment_prefix + include_str!("shaders/vector.frag")),
            &[
                c"a_position",
                c"a_offset",
                c"a_origin",
                c"a_color",
                c"a_coverage",
            ],
        ) {
            Ok(program) => program,
            Err(error) => {
                unsafe {
                    gl::DeleteProgram(program);
                    gl::DeleteProgram(vector_program);
                }
                return Err(error);
            }
        };
        let uniforms = Uniforms {
            paint: PaintUniforms {
                aa: uniform(program, c"u_aa"),
                texture: uniform(program, c"u_texture"),
                edge_fade: uniform(program, c"u_edge_fade"),
                edge_fade_x: uniform(program, c"u_edge_fade_x"),
                app: uniform(program, c"u_app"),
                icon_tint: uniform(program, c"u_icon_tint"),
                app_top: uniform(program, c"u_app_top"),
                app_bottom: uniform(program, c"u_app_bottom"),
                glass: uniform(program, c"u_glass"),
            },
            vector: MeshUniforms {
                edge_fade: uniform(vector_program, c"u_edge_fade"),
                edge_fade_x: uniform(vector_program, c"u_edge_fade_x"),
                fringe: uniform(vector_program, c"u_fringe"),
                embolden: uniform(vector_program, c"u_embolden"),
                transform: uniform(vector_program, c"u_transform"),
                pixel_scale: uniform(vector_program, c"u_pixel_scale"),
                tint: uniform(vector_program, c"u_tint"),
                mono: uniform(vector_program, c"u_mono"),
            },
            text: MeshUniforms {
                edge_fade: uniform(text_program, c"u_edge_fade"),
                edge_fade_x: uniform(text_program, c"u_edge_fade_x"),
                fringe: uniform(text_program, c"u_fringe"),
                embolden: uniform(text_program, c"u_embolden"),
                transform: uniform(text_program, c"u_transform"),
                pixel_scale: uniform(text_program, c"u_pixel_scale"),
                tint: uniform(text_program, c"u_tint"),
                mono: uniform(text_program, c"u_mono"),
            },
        };
        let mut buffer = 0;
        let mut text_buffer = 0;
        let mut vector_buffer = 0;
        let mut logo_buffer = 0;
        let mut vao = 0;
        unsafe {
            if !gles {
                gl::GenVertexArrays(1, &mut vao);
                gl::BindVertexArray(vao);
            }
            gl::GenBuffers(1, &mut buffer);
            gl::GenBuffers(1, &mut text_buffer);
            gl::GenBuffers(1, &mut vector_buffer);
            gl::GenBuffers(1, &mut logo_buffer);
            gl::Enable(gl::BLEND);
            // Preserve an opaque destination alpha. Using BlendFunc for both
            // channels makes translucent UI darken again in PNG/window compositing.
            gl::BlendFuncSeparate(
                gl::SRC_ALPHA,
                gl::ONE_MINUS_SRC_ALPHA,
                gl::ONE,
                gl::ONE_MINUS_SRC_ALPHA,
            );
            gl::Disable(gl::DEPTH_TEST);
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
        }
        let white = texture(1, 1, &[255; 4], NO_CAP);
        Ok(Self {
            program,
            vector_program,
            text_program,
            text_embolden,
            buffer,
            text_buffer,
            vector_buffer,
            logo_buffer,
            vao,
            white,
            font_directory: directory.into(),
            fonts,
            failed_fonts: HashSet::new(),
            glyphs: HashMap::new(),
            text_runs: HashMap::new(),
            textures: HashMap::new(),
            vectors: HashMap::new(),
            artwork_variants: HashMap::new(),
            vertices: Vec::with_capacity(65536),
            text_vertices: Vec::with_capacity(65536),
            vector_vertices: Vec::with_capacity(65536),
            logo_vertices: Vec::with_capacity(65536),
            pending_text: Vec::new(),
            pending_logo: Vec::new(),
            batches: Vec::new(),
            clip: None,
            fade: None,
            uniforms,
            frame: 0,
            stats: RenderStats::new(Size::HD),
            info: GraphicsInfo {
                vendor: gl_string(gl::VENDOR),
                renderer: gl_string(gl::RENDERER),
                version: gl_string(gl::VERSION),
                max_texture_size: max_texture_size.max(0) as u32,
                max_viewport,
                fragment_highp,
            },
        })
    }

    fn ensure_font(&mut self, id: u16) -> bool {
        if self.fonts.contains_key(&id) {
            return true;
        }
        if self.failed_fonts.contains(&id) {
            return false;
        }
        match load_font(&self.font_directory, id) {
            Ok(font) => {
                self.fonts.insert(id, font);
                true
            }
            Err(error) => {
                eprintln!("{error}");
                self.failed_fonts.insert(id);
                false
            }
        }
    }

    fn glyph(&mut self, c: char, font: u16) -> Result<&[luma_vector_geometry::Vertex], String> {
        let key = (c, font);
        if !self.glyphs.contains_key(&key) {
            let started = Instant::now();
            let geometry = self.fonts[&font].geometry(c)?;
            self.stats.glyph_ms += started.elapsed().as_secs_f64() * 1000.;
            self.glyphs.insert(
                key,
                Glyph {
                    geometry,
                    frame: self.frame,
                },
            );
            self.stats.glyph_tessellations += 1;
        }
        let glyph = self.glyphs.get_mut(&key).unwrap();
        glyph.frame = self.frame;
        Ok(&glyph.geometry)
    }

    fn trim_text_geometry(&mut self) {
        let glyph_bytes =
            |glyph: &Glyph| glyph.geometry.len() * size_of::<luma_vector_geometry::Vertex>();
        let mut bytes: usize = self.glyphs.values().map(glyph_bytes).sum();
        if bytes > GLYPH_GEOMETRY_BUDGET || self.glyphs.len() > 4096 {
            let mut old: Vec<_> = self
                .glyphs
                .iter()
                .filter(|(_, glyph)| glyph.frame != self.frame)
                .map(|(key, glyph)| (*key, glyph.frame))
                .collect();
            old.sort_by_key(|(_, frame)| *frame);
            for (key, _) in old {
                if bytes <= GLYPH_GEOMETRY_BUDGET && self.glyphs.len() <= 4096 {
                    break;
                }
                if let Some(glyph) = self.glyphs.remove(&key) {
                    bytes -= glyph_bytes(&glyph);
                }
            }
        }
        let run_bytes = |run: &TextRun| run.vertices.len() * size_of::<f32>();
        let mut bytes: usize = self.text_runs.values().map(run_bytes).sum();
        if bytes > TEXT_GEOMETRY_BUDGET || self.text_runs.len() > 512 {
            let mut old: Vec<_> = self
                .text_runs
                .iter()
                .filter(|(_, run)| run.frame != self.frame)
                .map(|(key, run)| (key.clone(), run.frame))
                .collect();
            old.sort_by_key(|(_, frame)| *frame);
            for (key, _) in old {
                if bytes <= TEXT_GEOMETRY_BUDGET && self.text_runs.len() <= 512 {
                    break;
                }
                if let Some(run) = self.text_runs.remove(&key) {
                    bytes -= run_bytes(&run);
                }
            }
        }
        self.stats.cached_glyphs = self.glyphs.len();
        self.stats.glyph_geometry_bytes = self.glyphs.values().map(glyph_bytes).sum();
        self.stats.cached_text_runs = self.text_runs.len();
        self.stats.text_geometry_bytes = self.text_runs.values().map(run_bytes).sum();
    }

    fn quad(
        &mut self,
        rect: Rect,
        uv: [f32; 4],
        radius: f32,
        border: f32,
        color: Color,
        texture: GLuint,
    ) {
        self.gradient_quad(rect, uv, radius, border, [color, color], texture, None);
    }

    /// A single-colour quad whose texture is desaturated and darkened by the paint shader.
    fn mono_quad(
        &mut self,
        rect: Rect,
        uv: [f32; 4],
        radius: f32,
        border: f32,
        color: Color,
        texture: GLuint,
    ) {
        self.paint_quad(
            rect,
            uv,
            radius,
            border,
            [color, color],
            texture,
            None,
            PaintMode::Monochrome,
        );
    }

    /// The whole tile plate in one pass: tint, both rims and the reflection.
    fn glass_quad(&mut self, rect: Rect, radius: f32, glass: Glass) {
        self.paint_quad(
            rect,
            [0., 0., 1., 1.],
            radius,
            0.,
            [glass.top, glass.bottom],
            self.white.id,
            None,
            PaintMode::Glass {
                rim: glass.rim.to_bits(),
                inner: glass.inner.to_bits(),
                reflection: glass.reflection.to_bits(),
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn gradient_quad(
        &mut self,
        rect: Rect,
        uv: [f32; 4],
        radius: f32,
        border: f32,
        colors: [Color; 2],
        texture: GLuint,
        backdrop: Option<[[u8; 3]; 2]>,
    ) {
        self.paint_quad(
            rect,
            uv,
            radius,
            border,
            colors,
            texture,
            backdrop,
            PaintMode::Normal,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_quad(
        &mut self,
        rect: Rect,
        uv: [f32; 4],
        radius: f32,
        border: f32,
        colors: [Color; 2],
        texture: GLuint,
        backdrop: Option<[[u8; 3]; 2]>,
        mode: PaintMode,
    ) {
        if rect.w <= 0. || rect.h <= 0. {
            return;
        }
        let rect = self.snap(rect);
        let start = self.vertices.len() / 14;
        // A glow's half sizes are negative: paint.frag uses that as the halo flag.
        let half = if matches!(mode, PaintMode::Glow) {
            -0.5
        } else {
            0.5
        };
        for (x, y) in [(0., 0.), (1., 0.), (0., 1.), (0., 1.), (1., 0.), (1., 1.)] {
            let color = colors[y as usize];
            self.vertices.extend_from_slice(&[
                rect.x + x * rect.w,
                rect.y + y * rect.h,
                uv[0] + x * (uv[2] - uv[0]),
                uv[1] + y * (uv[3] - uv[1]),
                color[0],
                color[1],
                color[2],
                color[3],
                (x - 0.5) * rect.w,
                (y - 0.5) * rect.h,
                half * rect.w,
                half * rect.h,
                radius,
                border,
            ]);
        }
        self.paint_batch(start, 6, texture, backdrop, mode);
    }

    /// Appends `count` paint vertices from `start`, merged into the previous batch when it uses
    /// the same texture, clip, fade and paint mode.
    fn paint_batch(
        &mut self,
        start: usize,
        count: usize,
        texture: GLuint,
        backdrop: Option<[[u8; 3]; 2]>,
        mode: PaintMode,
    ) {
        if let Some(batch) = self.batches.last_mut() {
            let same_clip = match (batch.clip, self.clip) {
                (None, None) => true,
                (Some(a), Some(b)) => a.x == b.x && a.y == b.y && a.w == b.w && a.h == b.h,
                _ => false,
            };
            if same_clip
                && batch.fade == self.fade
                && let BatchKind::Paint {
                    texture: previous,
                    count: batch_count,
                    backdrop: previous_backdrop,
                    mode: previous_mode,
                    ..
                } = &mut batch.kind
                && *previous == texture
                && *previous_backdrop == backdrop
                && *previous_mode == mode
            {
                *batch_count += count;
                return;
            }
        }
        self.batches.push(Batch {
            kind: BatchKind::Paint {
                texture,
                start,
                count,
                backdrop,
                mode,
            },
            clip: self.clip,
            fade: self.fade,
        });
    }

    /// Appends a merged vector run, joining the previous vector batch when it has
    /// the same clip, fade, mono mode and deferred layer.
    fn push_vector_batch(&mut self, start: usize, count: usize, mono: bool, deferred: bool) {
        if let Some(batch) = self.batches.last_mut()
            && same_clip(batch.clip, self.clip)
            && batch.fade == self.fade
            && let BatchKind::Vector {
                start: previous_start,
                count: previous_count,
                mono: previous_mono,
                deferred: previous_deferred,
            } = &mut batch.kind
            && *previous_start + *previous_count == start
            && *previous_mono == mono
            && *previous_deferred == deferred
        {
            *previous_count += count;
            return;
        }
        self.batches.push(Batch {
            kind: BatchKind::Vector {
                start,
                count,
                mono,
                deferred,
            },
            clip: self.clip,
            fade: self.fade,
        });
    }

    /// Text is painted last, after every shape, image and UI icon, so all captions and
    /// labels can merge into the few batches that share a clip and edge fade. Shadow and
    /// foreground vertices stay in their original order inside the merged buffer.
    /// Brand logos are deferred to the layer just below text; UI glyphs keep their order.
    fn flush_deferred(&mut self) {
        let mut logo_batches: Vec<Batch> = Vec::new();
        for entry in self.pending_logo.drain(..) {
            if let Some(last) = logo_batches.last_mut()
                && same_clip(last.clip, entry.clip)
                && last.fade == entry.fade
                && let BatchKind::Vector {
                    start,
                    count,
                    mono,
                    deferred,
                } = &mut last.kind
                && *start + *count == entry.start
                && *mono == entry.mono
                && *deferred
            {
                *count += entry.count;
                continue;
            }
            logo_batches.push(Batch {
                kind: BatchKind::Vector {
                    start: entry.start,
                    count: entry.count,
                    mono: entry.mono,
                    deferred: true,
                },
                clip: entry.clip,
                fade: entry.fade,
            });
        }
        self.batches.extend(logo_batches);

        let mut text_batches: Vec<Batch> = Vec::new();
        for entry in self.pending_text.drain(..) {
            if let Some(last) = text_batches.last_mut()
                && same_clip(last.clip, entry.clip)
                && last.fade == entry.fade
                && let BatchKind::Text {
                    start,
                    count,
                    embolden,
                } = &mut last.kind
                && *start + *count == entry.start
                && *embolden == entry.embolden
            {
                *count += entry.count;
                continue;
            }
            text_batches.push(Batch {
                kind: BatchKind::Text {
                    start: entry.start,
                    count: entry.count,
                    embolden: entry.embolden,
                },
                clip: entry.clip,
                fade: entry.fade,
            });
        }
        self.batches.extend(text_batches);
    }

    /// The wallpaper with full-screen translucent fills folded in: each fill maps a pixel p to
    /// p * (1 - a) + c * a, so together they are p * m + k, drawn in one pass instead of one
    /// full-screen layer each. Strips keep gradients' products of colour and alpha exact.
    fn folded_quad(&mut self, rect: Rect, texture: GLuint, scrims: &[Scrim]) {
        const STRIPS: usize = 16;
        let fold = |t: f32| {
            let (mut m, mut k) = ([1f32; 3], [0f32; 3]);
            for scrim in scrims {
                let color = scrim.at(t);
                for i in 0..3 {
                    m[i] *= 1. - color[3];
                    k[i] = k[i] * (1. - color[3]) + color[i] * color[3];
                }
            }
            (m, k)
        };
        // Scrims span the visible screen, so map the (possibly zoomed) wallpaper rect onto it.
        let screen_t = |t: f32| ((rect.y + t * rect.h) / 1080.).clamp(0., 1.);
        let start = self.vertices.len() / 14;
        for strip in 0..STRIPS {
            let rows = [
                strip as f32 / STRIPS as f32,
                (strip + 1) as f32 / STRIPS as f32,
            ];
            let folds = rows.map(|row| fold(screen_t(row)));
            for (x, row) in [(0., 0), (1., 0), (0., 1), (0., 1), (1., 0), (1., 1)] {
                let (m, k) = folds[row];
                let t = rows[row];
                self.vertices.extend_from_slice(&[
                    rect.x + x * rect.w,
                    rect.y + t * rect.h,
                    x,
                    t,
                    m[0],
                    m[1],
                    m[2],
                    1.,
                    k[0],
                    k[1],
                    k[2],
                    0.,
                    -2.,
                    0.,
                ]);
            }
        }
        self.paint_batch(start, STRIPS * 6, texture, None, PaintMode::Normal);
    }

    fn text(&mut self, text: &str, rect: Rect, style: TextStyle, color: Color, centered: bool) {
        // Layout remains logical. Reuse the same run geometry at every density;
        // its shader snaps insertion origins and computes pixel-sized coverage.
        let size_f = style.size;
        let viewport = self.stats.viewport;
        self.stats.max_logical_font_px = self.stats.max_logical_font_px.max(size_f);
        self.stats.max_physical_font_px = self
            .stats
            .max_physical_font_px
            .max(size_f * viewport.text_scale());
        let padded = rect.inset(-size_f);
        if padded.x + padded.w < 0.
            || padded.y + padded.h < 0.
            || padded.x > 1920.
            || padded.y > 1080.
            || self.clip.is_some_and(|clip| {
                padded.x + padded.w < clip.x
                    || padded.y + padded.h < clip.y
                    || padded.x > clip.x + clip.w
                    || padded.y > clip.y + clip.h
            })
        {
            return;
        }
        let origin = [rect.x, rect.y];
        let key = TextKey {
            text: text.into(),
            weight: style.weight,
            layout: [
                size_f.to_bits(),
                style.tracking.to_bits(),
                style.line_height.to_bits(),
                rect.w.to_bits(),
                rect.h.to_bits(),
            ],
            flags: u8::from(style.align_right)
                | (u8::from(centered) << 1)
                | (u8::from(self.stats.multisample_samples > 1) << 2),
        };
        if self.text_runs.contains_key(&key) {
            self.submit_text_shadow(&key, origin, color, size_f);
            self.submit_text(key, origin, color);
            return;
        }
        let rect = Rect::new(0., 0., rect.w, rect.h);
        let font_id = style.weight + if style.size >= 32. { 1000 } else { 0 };
        let cjk_id = if style.weight >= 500 { 2500 } else { 2400 };
        let hangul_id = cjk_id + 1000;
        if !self.ensure_font(font_id) {
            self.stats.glyph_failures += text.chars().count();
            return;
        }
        let (mut needs_cjk, mut needs_hangul) = (false, false);
        for c in text.chars() {
            if !c.is_ascii_control() && self.fonts[&font_id].lookup_glyph_index(c) == 0 {
                if is_hangul(c) {
                    needs_hangul = true;
                } else {
                    needs_cjk = true;
                }
            }
        }
        // Each fallback loads only when a run needs it, so Korean text never
        // pulls in the larger Chinese font and vice versa.
        let cjk_ready = needs_cjk && self.ensure_font(cjk_id);
        let hangul_ready = needs_hangul && self.ensure_font(hangul_id);
        let fonts = &self.fonts;
        let choose_font = |c: char| {
            if fonts[&font_id].lookup_glyph_index(c) != 0 {
                font_id
            } else if is_hangul(c) {
                if hangul_ready { hangul_id } else { font_id }
            } else if cjk_ready {
                cjk_id
            } else {
                font_id
            }
        };
        let font = &fonts[&font_id];
        let line_height = size_f * style.line_height;
        let baseline = font.horizontal_line_metrics(size_f).map_or(size_f, |m| {
            (line_height - m.ascent + m.descent) / 2. + m.ascent
        });
        let tracking = style.tracking * size_f;
        let advance = |c| {
            if c == '\n' {
                0.
            } else {
                fonts[&choose_font(c)].metrics(c, size_f).advance_width + tracking
            }
        };
        let mut lines: Vec<String> = Vec::new();
        let mut line = String::new();
        let mut width = 0.;
        // Word wrapping, with character fallback for a title wider than the tile.
        for word in text.split_inclusive(char::is_whitespace) {
            let word_width: f32 = word.chars().map(advance).sum();
            if width > 0. && width + word_width > rect.w {
                lines.push(line.trim_end().into());
                line.clear();
                width = 0.;
            }
            for c in word.chars() {
                if c == '\n' {
                    lines.push(line.trim_end().into());
                    line.clear();
                    width = 0.;
                    continue;
                }
                let w = advance(c);
                if width > 0. && width + w > rect.w {
                    lines.push(line.trim_end().into());
                    line.clear();
                    width = 0.;
                }
                line.push(c);
                width += w;
            }
        }
        if !line.is_empty() {
            lines.push(line.trim_end().into());
        }
        let mut placed = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let top = rect.y + index as f32 * line_height;
            if index > 0 && top + line_height > rect.y + rect.h + 1. {
                break;
            }
            let mut previous = None;
            let width: f32 = line
                .chars()
                .map(|c| {
                    let kern = previous
                        .and_then(|p| font.horizontal_kern(p, c, size_f))
                        .unwrap_or(0.);
                    previous = Some(c);
                    advance(c) + kern
                })
                .sum();
            let mut x = rect.x
                + if style.align_right {
                    rect.w - width
                } else if centered {
                    (rect.w - width) / 2.
                } else {
                    0.
                };
            let mut previous = None;
            for c in line.chars() {
                if let Some(previous) = previous {
                    x += font.horizontal_kern(previous, c, size_f).unwrap_or(0.);
                }
                let selected = choose_font(c);
                let baseline = if selected == font_id {
                    baseline
                } else {
                    fonts[&selected]
                        .horizontal_line_metrics(size_f)
                        .map_or(baseline, |m| {
                            (line_height - m.ascent + m.descent) / 2. + m.ascent
                        })
                };
                placed.push((c, x, top + baseline, selected));
                x += advance(c);
                previous = Some(c);
            }
        }
        let mut vertices = Vec::new();
        let mut missing_glyphs = 0;
        let mut failures = 0;
        let multisampled = self.stats.multisample_samples > 1;
        for (c, x, baseline, selected) in placed {
            if self.fonts[&selected].lookup_glyph_index(c) == 0 && !c.is_whitespace() {
                missing_glyphs += 1;
            }
            match self.glyph(c, selected) {
                Ok(geometry) => {
                    for triangle in geometry.chunks_exact(3) {
                        // True sample coverage needs only the original fill.
                        // Exclude the boundary fringe, not just its alpha.
                        if multisampled && triangle.iter().any(|vertex| vertex.coverage < 1.) {
                            continue;
                        }
                        for vertex in triangle {
                            vertices.extend_from_slice(&[
                                vertex.position.x * size_f,
                                vertex.position.y * size_f,
                                if multisampled { 0. } else { vertex.offset[0] },
                                if multisampled { 0. } else { vertex.offset[1] },
                                x,
                                baseline,
                                0.,
                                0.,
                                vertex.coverage,
                            ]);
                        }
                    }
                }
                Err(error) => {
                    eprintln!("Luma glyph {c:?}: {error}");
                    failures += 1;
                }
            }
        }
        self.text_runs.insert(
            key.clone(),
            TextRun {
                vertices,
                missing_glyphs,
                failures,
                frame: self.frame,
            },
        );
        self.submit_text_shadow(&key, origin, color, size_f);
        self.submit_text(key, origin, color);
    }

    /// A dark offset copy of a text run, submitted before the real one. It reuses the same
    /// cached outline, so the only cost is one more tiny draw; the result reads as type
    /// floating above the glass rather than painted onto it.
    fn submit_text_shadow(&mut self, key: &TextKey, origin: [f32; 2], color: Color, size: f32) {
        let Some(run) = self.text_runs.get(key) else {
            return;
        };
        if run.vertices.is_empty() || color[3] <= 0. {
            return;
        }
        let offset = (size * 0.045).clamp(1., 4.);
        let shadow = [0., 0., 0., color[3] * 0.42];
        let start = append_text_run(&mut self.text_vertices, run, origin, shadow, offset);
        let count = self.text_vertices.len() / 13 - start;
        let embolden = embolden(
            f32::from_bits(key.layout[0]) * self.stats.viewport.height as f32 / 1080.,
            self.text_embolden,
        );
        self.pending_text.push(DeferredText {
            start,
            count,
            embolden: embolden.to_bits(),
            clip: self.clip,
            fade: self.fade,
        });
    }

    fn submit_text(&mut self, key: TextKey, origin: [f32; 2], tint: Color) {
        let run = self.text_runs.get(&key).unwrap();
        self.stats.missing_glyphs += run.missing_glyphs;
        self.stats.glyph_failures += run.failures;
        if run.vertices.is_empty() {
            return;
        }
        self.stats.text_triangles += run.vertices.len() / 27;
        let start = append_text_run(&mut self.text_vertices, run, origin, tint, 0.);
        let count = self.text_vertices.len() / 13 - start;
        let embolden = embolden(
            f32::from_bits(key.layout[0]) * self.stats.viewport.height as f32 / 1080.,
            self.text_embolden,
        );
        self.pending_text.push(DeferredText {
            start,
            count,
            embolden: embolden.to_bits(),
            clip: self.clip,
            fade: self.fade,
        });
        self.text_runs.get_mut(&key).unwrap().frame = self.frame;
    }

    fn image(&mut self, path: &str, rect: Rect, radius: f32, contain: bool, tint: Color) {
        self.folded_image(path, rect, radius, contain, tint, &[]);
    }

    fn folded_image(
        &mut self,
        path: &str,
        mut rect: Rect,
        radius: f32,
        contain: bool,
        tint: Color,
        scrims: &[Scrim],
    ) {
        if path.ends_with(".lvec") {
            self.vector_image(path, rect, contain, tint);
            return;
        }
        let path = if self.stats.viewport.scale > 1.
            && self.info.max_texture_size >= 3840
            && Path::new(path)
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|p| p == "wallpapers")
        {
            self.artwork_variants
                .entry(path.into())
                .or_insert_with(|| {
                    let original = Path::new(path);
                    let variant = original.with_file_name(format!(
                        "{}@2x.jpg",
                        original.file_stem().unwrap_or_default().to_string_lossy()
                    ));
                    if variant.is_file() {
                        variant.to_string_lossy().into_owned()
                    } else {
                        path.into()
                    }
                })
                .clone()
        } else {
            path.into()
        };
        if path.ends_with("@2x.jpg") {
            self.stats.high_resolution_wallpaper = true;
        }
        let frame = self.frame;
        let started = Instant::now();
        let cap = self.wallpaper_limit();
        self.forget_mismatched_upload(&path, cap);
        let loaded = !self.textures.contains_key(&path);
        let entry = self
            .textures
            .entry(path.clone())
            .or_insert_with(|| match image::open(&path) {
                Ok(image) => {
                    let image = image.into_rgba8();
                    let target =
                        crate::resample::screen_upload_size(image.width(), image.height(), cap);
                    Some(upload(
                        image.width(),
                        image.height(),
                        image.as_raw(),
                        target,
                        cap,
                    ))
                }
                Err(error) => {
                    eprintln!("Artwork {path}: {error}");
                    None
                }
            });
        if loaded {
            self.stats.image_load_ms += started.elapsed().as_secs_f64() * 1000.;
        }
        let Some(texture) = entry else {
            self.stats.missing_images += 1;
            return;
        };
        texture.frame = frame;
        let id = texture.id;
        if contain {
            let scale = (rect.w / texture.width as f32).min(rect.h / texture.height as f32);
            let w = texture.width as f32 * scale;
            let h = texture.height as f32 * scale;
            rect = Rect::new(rect.x + (rect.w - w) / 2., rect.y + (rect.h - h) / 2., w, h);
        }
        if scrims.is_empty() {
            self.quad(rect, [0., 0., 1., 1.], plain_radius(radius), 0., tint, id);
        } else {
            self.folded_quad(rect, id, scrims);
        }
    }

    /// The largest physical size a full-screen image can be displayed at: the drawable, enlarged
    /// by the Ken Burns zoom that scales the wallpaper up behind the plates, and never beyond what
    /// this GL driver can hold as a texture.
    fn wallpaper_limit(&self) -> (u32, u32) {
        let zoom = 1. + crate::view::KEN_BURNS_ZOOM;
        let (width, height) = crate::resample::largest_display(self.stats.target);
        let ceiling = self.info.max_texture_size.max(1);
        (
            ((width as f32 * zoom).ceil() as u32).min(ceiling),
            ((height as f32 * zoom).ceil() as u32).min(ceiling),
        )
    }

    /// The largest physical box app artwork is ever drawn in, including the launch zoom, never
    /// beyond what this GL driver can hold as a texture.
    fn artwork_limit(&self) -> (u32, u32) {
        let (side, _) = crate::resample::largest_artwork_display(self.stats.target);
        let ceiling = self.info.max_texture_size.max(1);
        (side.min(ceiling), side.min(ceiling))
    }

    /// Drops a cached upload whose cap no longer matches the viewport, so a larger display later
    /// gets a correspondingly larger copy instead of keeping one shrunk for a smaller screen.
    /// Removing it runs `Texture`'s `Drop`, which deletes the GL texture.
    fn forget_mismatched_upload(&mut self, key: &str, cap: (u32, u32)) {
        let stale = self
            .textures
            .get(key)
            .and_then(|entry| entry.as_ref())
            .is_some_and(|texture| texture.cap != cap);
        if stale {
            self.textures.remove(key);
        }
    }

    /// Edges on whole drawable pixels. The TV scales its 1080p graphics plane up 2x, which
    /// turns a half-covered edge pixel into a soft 2-pixel band; whole pixels stay crisp.
    /// LUMA_SNAP=0 keeps logical positions (for comparison).
    fn snap(&self, rect: Rect) -> Rect {
        snap_rect(rect, self.stats.viewport.scale)
    }

    fn vector_image(&mut self, path: &str, mut rect: Rect, contain: bool, tint: Color) {
        if rect.w <= 0. || rect.h <= 0. {
            return;
        }
        let scale = self.stats.viewport.scale;
        let started = Instant::now();
        let loaded = !self.vectors.contains_key(path);
        let mesh = self.vectors.entry(path.into()).or_insert_with(|| {
            match vector::Mesh::load(Path::new(path)) {
                Ok(mesh) => Some(mesh),
                Err(error) => {
                    eprintln!("{error}");
                    None
                }
            }
        });
        if loaded {
            self.stats.vector_load_ms += started.elapsed().as_secs_f64() * 1000.;
        }
        let Some(mesh) = mesh else {
            self.stats.missing_images += 1;
            return;
        };
        if contain {
            let scale = (rect.w / mesh.width).min(rect.h / mesh.height);
            let width = mesh.width * scale;
            let height = mesh.height * scale;
            rect = Rect::new(
                rect.x + (rect.w - width) / 2.,
                rect.y + (rect.h - height) / 2.,
                width,
                height,
            );
        }
        // Logos keep their size; only their placement moves onto the pixel grid.
        let snapped = snap_rect(rect, scale);
        rect = Rect::new(snapped.x, snapped.y, rect.w, rect.h);
        if let Some(clip) = self.clip {
            let x = rect.x.max(clip.x);
            let y = rect.y.max(clip.y);
            let visible = Rect::new(
                x,
                y,
                (rect.x + rect.w).min(clip.x + clip.w).max(x) - x,
                (rect.y + rect.h).min(clip.y + clip.h).max(y) - y,
            );
            if visible.w <= 0. || visible.h <= 0. {
                return;
            }
        }
        self.stats.vector_triangles += mesh.vertices as usize / 3;
        // Curated brand artwork lives under logos/ and is tinted like an installed
        // icon; UI glyphs (ui-icons/, clock, weather) keep their own colour.
        let mono = path.contains("logos/") || path.contains("logos\\");
        if mono {
            let start = append_vector_run(&mut self.logo_vertices, mesh, rect, tint);
            let count = self.logo_vertices.len() / 9 - start;
            self.pending_logo.push(DeferredVector {
                start,
                count,
                mono: true,
                clip: self.clip,
                fade: self.fade,
            });
        } else {
            let start = append_vector_run(&mut self.vector_vertices, mesh, rect, tint);
            let count = self.vector_vertices.len() / 9 - start;
            self.push_vector_batch(start, count, false, false);
        }
    }

    fn app_image(
        &mut self,
        image: &crate::app_artwork::Artwork,
        rect: Rect,
        radius: f32,
        tint: Color,
    ) {
        let cap = self.artwork_limit();
        self.forget_mismatched_upload(&image.key, cap);
        let entry = self.textures.entry(image.key.clone()).or_insert_with(|| {
            let (source_width, source_height) = (image.pixels.width(), image.pixels.height());
            let target = crate::resample::artwork_upload_size(source_width, source_height, cap);
            Some(upload(
                source_width,
                source_height,
                image.pixels.as_raw(),
                target,
                cap,
            ))
        });
        let Some(texture) = entry else {
            return;
        };
        texture.frame = self.frame;
        let scale =
            (rect.w * 0.68 / texture.width as f32).min(rect.h * 0.68 / texture.height as f32);
        let w = texture.width as f32 * scale;
        let h = texture.height as f32 * scale;
        let id = texture.id;
        self.gradient_quad(
            rect,
            [
                0.5 - rect.w / (2. * w),
                0.5 - rect.h / (2. * h),
                0.5 + rect.w / (2. * w),
                0.5 + rect.h / (2. * h),
            ],
            radius,
            0.,
            [tint; 2],
            id,
            Some(image.backdrop),
        );
    }

    /// An installed icon alone, contain-fitted inside `rect` and keeping its real alpha, so a
    /// translucent plate painted underneath stays visible around it. Unlike `app_image` this
    /// does not take the shader's opaque app path, so no backdrop is filled behind it.
    fn app_icon(&mut self, image: &crate::app_artwork::Artwork, rect: Rect, tint: Color) {
        let cap = self.artwork_limit();
        self.forget_mismatched_upload(&image.key, cap);
        let entry = self.textures.entry(image.key.clone()).or_insert_with(|| {
            let target = crate::resample::artwork_upload_size(
                image.pixels.width(),
                image.pixels.height(),
                cap,
            );
            Some(upload(
                image.pixels.width(),
                image.pixels.height(),
                image.pixels.as_raw(),
                target,
                cap,
            ))
        });
        let Some(texture) = entry else {
            return;
        };
        texture.frame = self.frame;
        let scale = (rect.w / texture.width as f32).min(rect.h / texture.height as f32);
        let w = texture.width as f32 * scale;
        let h = texture.height as f32 * scale;
        let id = texture.id;
        // A squircle, like the plate it sits on: the shader clamps the radius to half the
        // shorter side, so this stays a rounded square rather than a circle.
        let radius = w.min(h) * 0.30;
        self.mono_quad(
            Rect::new(rect.x + (rect.w - w) / 2., rect.y + (rect.h - h) / 2., w, h),
            [0., 0., 1., 1.],
            radius,
            0.,
            tint,
            id,
        );
    }

    /// A logical damage rect in drawable pixels ([x, y, width, height], origin bottom-left),
    /// grown to whole pixels.
    pub fn damage_pixels(rect: Rect, width: u32, height: u32) -> [i32; 4] {
        let viewport = Viewport::new(Size::new(width, height));
        let sx = viewport.width as f32 / 1920.;
        let sy = viewport.height as f32 / 1080.;
        let x0 = (viewport.x as f32 + rect.x.max(0.) * sx).floor() as i32;
        let x1 = (viewport.x as f32 + (rect.x + rect.w).min(1920.) * sx).ceil() as i32;
        let y0 = (viewport.y as f32 + (1080. - (rect.y + rect.h).min(1080.)) * sy).floor() as i32;
        let y1 = (viewport.y as f32 + (1080. - rect.y.max(0.)) * sy).ceil() as i32;
        if x1 <= x0 || y1 <= y0 {
            // No change still names, clears and redraws one pixel inside the drawable. An empty
            // or off-surface region left the driver nothing to preserve: Mali presented black.
            return [viewport.x, viewport.y, 1, 1];
        }
        [x0, y0, x1 - x0, y1 - y0]
    }

    /// The logical area drawable pixels from `damage_pixels` cover. Grown to whole pixels it
    /// can exceed the original logical damage; commands are culled against this area, so none
    /// that paints into the cleared pixels is skipped.
    pub fn damage_area(pixels: [i32; 4], width: u32, height: u32) -> Rect {
        let viewport = Viewport::new(Size::new(width, height));
        let sx = viewport.width as f32 / 1920.;
        let sy = viewport.height as f32 / 1080.;
        let [x, y, w, h] = pixels;
        Rect::new(
            (x - viewport.x) as f32 / sx,
            1080. - (y + h - viewport.y) as f32 / sy,
            w as f32 / sx,
            h as f32 / sy,
        )
    }

    /// Prepares first-use work for a scene without drawing it (text layout and glyph
    /// outlines, logo meshes, images), e.g. another page while the screen is idle, so its
    /// first appearance does not pay for it. The visible frame's statistics stay unchanged.
    pub fn prepare(&mut self, commands: &[Draw], width: u32, height: u32) {
        let stats = self.stats.clone();
        self.build(commands, width, height, None);
        self.vertices.clear();
        self.batches.clear();
        self.stats = stats;
    }

    /// Lays out the scene into vertices and batches; commands entirely outside `area` are
    /// skipped.
    fn build(&mut self, commands: &[Draw], width: u32, height: u32, area: Option<Rect>) {
        self.stats = RenderStats::new(Size::new(width, height));
        // Captures can bind a single-sample FBO even when the actual TV window
        // is multisampled, so query the active target rather than assume.
        let mut samples = 0;
        unsafe { gl::GetIntegerv(gl::SAMPLES, &mut samples) };
        self.stats.multisample_samples = samples.max(0) as u32;
        if samples > 1 {
            self.stats.font_antialiasing = "multisample vector coverage";
        }
        self.frame += 1;
        self.vertices.clear();
        self.text_vertices.clear();
        self.vector_vertices.clear();
        self.logo_vertices.clear();
        self.pending_text.clear();
        self.pending_logo.clear();
        self.batches.clear();
        self.clip = None;
        self.fade = None;
        let mut index = 0;
        while index < commands.len() {
            let command = &commands[index];
            index += 1;
            if profile_skip(command) {
                continue;
            }
            if full_screen_paint(command) {
                self.flush_deferred();
            }
            if let Some((path, wallpaper)) = plain_wallpaper(command)
                && self.clip.is_none()
                && self.fade.is_none()
            {
                let mut scrims = Vec::new();
                while let Some(next) = commands.get(index) {
                    if profile_skip(next) {
                        index += 1;
                    } else if let Some(scrim) = Scrim::of(next) {
                        scrims.push(scrim);
                        index += 1;
                    } else {
                        break;
                    }
                }
                if !scrims.is_empty() {
                    self.folded_image(path, wallpaper, 0., false, [1.; 4], &scrims);
                    continue;
                }
            }
            // Outside the damage the scissor would discard it anyway; skip its layout too.
            if let (Some(area), Some(bounds)) = (area, crate::damage::bounds(command))
                && !crate::damage::intersects(area, bounds)
            {
                continue;
            }
            match command {
                Draw::Shape(rect, radius, border, color) => self.quad(
                    *rect,
                    [0., 0., 1., 1.],
                    *radius,
                    *border,
                    *color,
                    self.white.id,
                ),
                Draw::RoundedGradient(rect, radius, top, bottom) => self.gradient_quad(
                    *rect,
                    [0., 0., 1., 1.],
                    *radius,
                    0.,
                    [*top, *bottom],
                    self.white.id,
                    None,
                ),
                Draw::Glass(rect, radius, glass) => self.glass_quad(*rect, *radius, *glass),
                Draw::Shadow(rect, radius, blur, color) => self.quad(
                    rect.inset(-*blur),
                    [0., 0., 1., 1.],
                    *radius,
                    -*blur,
                    *color,
                    self.white.id,
                ),
                // A glow marks its half sizes negative so paint.frag can tell the halo branch
                // apart from a plain shadow (see PaintMode::Glow).
                Draw::Glow(rect, radius, blur, color) => self.paint_quad(
                    rect.inset(-*blur),
                    [0., 0., 1., 1.],
                    *radius,
                    -*blur,
                    [*color, *color],
                    self.white.id,
                    None,
                    PaintMode::Glow,
                ),
                Draw::Text(text, rect, size, color, centered) => {
                    self.text(text, *rect, *size, *color, *centered)
                }
                Draw::Image(path, rect, radius, contain) => {
                    self.image(path, *rect, *radius, *contain, [1.; 4])
                }
                Draw::TintedImage(path, rect, radius, contain, tint) => {
                    self.image(path, *rect, *radius, *contain, *tint)
                }
                Draw::AppImage(image, rect, radius, tint) => {
                    self.app_image(image, *rect, *radius, *tint)
                }
                Draw::AppIcon(image, rect, tint) => self.app_icon(image, *rect, *tint),
                Draw::Clip(clip) => self.clip = *clip,
                Draw::EdgeFade(fade) => self.set_fade(0, *fade),
                Draw::EdgeFadeX(fade) => self.set_fade(4, *fade),
            }
        }
        self.flush_deferred();
    }

    /// Draws the scene. With `damage` (drawable pixels from `damage_pixels`), only those pixels
    /// are cleared and drawn; commands entirely outside them are skipped, and the rest of the
    /// back buffer keeps its earlier contents.
    pub fn render(&mut self, commands: &[Draw], width: u32, height: u32, damage: Option<[i32; 4]>) {
        let area = damage.map(|pixels| Self::damage_area(pixels, width, height));
        let started = Instant::now();
        self.build(commands, width, height, area);
        let built = Instant::now();
        let Viewport {
            x: vx,
            y: vy,
            width: vw,
            height: vh,
            scale,
        } = self.stats.viewport;
        if let Some([_, _, w, h]) = damage {
            self.stats.redrawn_fraction = f64::from(w * h) / f64::from((width * height).max(1));
        }
        let scissor = |area: Option<[i32; 4]>| unsafe {
            match area {
                Some([x, y, w, h]) => {
                    gl::Enable(gl::SCISSOR_TEST);
                    gl::Scissor(x, y, w.max(0), h.max(0));
                }
                None => gl::Disable(gl::SCISSOR_TEST),
            }
        };
        let clear_ms;
        let upload_ms;
        let upload_bytes;
        let setup_ms;
        let draw_ms;
        let mut draw_paint_ms = 0.;
        let mut draw_vector_ms = 0.;
        let mut draw_text_ms = 0.;
        unsafe {
            gl::Viewport(0, 0, width as i32, height as i32);
            scissor(damage);
            let clear_started = Instant::now();
            gl::ClearColor(0.04, 0.04, 0.05, 1.);
            gl::Clear(gl::COLOR_BUFFER_BIT);
            clear_ms = clear_started.elapsed().as_secs_f64() * 1000.;
            gl::Viewport(vx, vy, vw, vh);
            let upload_started = Instant::now();
            if self.vao != 0 {
                gl::BindVertexArray(self.vao);
            }
            gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (self.vertices.len() * size_of::<f32>()) as isize,
                self.vertices.as_ptr().cast(),
                gl::DYNAMIC_DRAW,
            );
            if !self.text_vertices.is_empty() {
                gl::BindBuffer(gl::ARRAY_BUFFER, self.text_buffer);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    (self.text_vertices.len() * size_of::<f32>()) as isize,
                    self.text_vertices.as_ptr().cast(),
                    gl::DYNAMIC_DRAW,
                );
            }
            if !self.vector_vertices.is_empty() {
                gl::BindBuffer(gl::ARRAY_BUFFER, self.vector_buffer);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    (self.vector_vertices.len() * size_of::<f32>()) as isize,
                    self.vector_vertices.as_ptr().cast(),
                    gl::DYNAMIC_DRAW,
                );
            }
            if !self.logo_vertices.is_empty() {
                gl::BindBuffer(gl::ARRAY_BUFFER, self.logo_buffer);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    (self.logo_vertices.len() * size_of::<f32>()) as isize,
                    self.logo_vertices.as_ptr().cast(),
                    gl::DYNAMIC_DRAW,
                );
            }
            upload_ms = upload_started.elapsed().as_secs_f64() * 1000.;
            upload_bytes = (self.vertices.len()
                + self.text_vertices.len()
                + self.vector_vertices.len()
                + self.logo_vertices.len())
                * size_of::<f32>();
            let setup_started = Instant::now();
            gl::UseProgram(self.program);
            // One drawable pixel in logical units: the paint shader's edge filter width.
            gl::Uniform1f(self.uniforms.paint.aa, 1. / scale.max(0.01));
            gl::Uniform1i(self.uniforms.paint.texture, 0);
            gl::ActiveTexture(gl::TEXTURE0);
            setup_ms = setup_started.elapsed().as_secs_f64() * 1000.;
            let draw_started = Instant::now();
            let mut paint_bound = false;
            let mut current_fade = None;
            for batch in &self.batches {
                if current_fade != Some(batch.fade) {
                    // The same cheap alpha mask applies to shapes, textures,
                    // icons and text; the unmasked wallpaper remains beneath.
                    let fade = batch.fade.unwrap_or([0.; 8]);
                    for program in [self.program, self.vector_program, self.text_program] {
                        gl::UseProgram(program);
                        let (edge_fade, edge_fade_x) = if program == self.vector_program {
                            (
                                self.uniforms.vector.edge_fade,
                                self.uniforms.vector.edge_fade_x,
                            )
                        } else if program == self.text_program {
                            (self.uniforms.text.edge_fade, self.uniforms.text.edge_fade_x)
                        } else {
                            (
                                self.uniforms.paint.edge_fade,
                                self.uniforms.paint.edge_fade_x,
                            )
                        };
                        gl::Uniform4f(edge_fade, fade[0], fade[1], fade[2], fade[3]);
                        gl::Uniform4f(edge_fade_x, fade[4], fade[5], fade[6], fade[7]);
                    }
                    current_fade = Some(batch.fade);
                    paint_bound = false;
                }
                let clip = batch.clip.map(|clip| {
                    [
                        vx + (clip.x * vw as f32 / 1920.).round() as i32,
                        vy + ((1080. - clip.y - clip.h) * vh as f32 / 1080.).round() as i32,
                        (clip.w * vw as f32 / 1920.).round() as i32,
                        (clip.h * vh as f32 / 1080.).round() as i32,
                    ]
                });
                scissor(match (clip, damage) {
                    (Some(a), Some(b)) => Some(intersect(a, b)),
                    (a, b) => a.or(b),
                });
                let batch_started = Instant::now();
                match &batch.kind {
                    BatchKind::Paint {
                        texture,
                        start,
                        count,
                        backdrop,
                        mode,
                    } => {
                        if !paint_bound {
                            gl::UseProgram(self.program);
                            gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
                            let mut offset = 0;
                            for (index, size) in [2, 2, 4, 4, 2].into_iter().enumerate() {
                                gl::EnableVertexAttribArray(index as u32);
                                gl::VertexAttribPointer(
                                    index as u32,
                                    size,
                                    gl::FLOAT,
                                    gl::FALSE,
                                    56,
                                    (offset * size_of::<f32>()) as *const _,
                                );
                                offset += size as usize;
                            }
                            paint_bound = true;
                        }
                        gl::BindTexture(gl::TEXTURE_2D, *texture);
                        gl::Uniform1i(self.uniforms.paint.app, i32::from(backdrop.is_some()));
                        gl::Uniform1f(
                            self.uniforms.paint.icon_tint,
                            if matches!(mode, PaintMode::Monochrome) {
                                1.
                            } else {
                                0.
                            },
                        );
                        if let PaintMode::Glass {
                            rim,
                            inner,
                            reflection,
                        } = *mode
                        {
                            gl::Uniform4f(
                                self.uniforms.paint.glass,
                                f32::from_bits(rim),
                                f32::from_bits(inner),
                                f32::from_bits(reflection),
                                1.,
                            );
                        } else {
                            gl::Uniform4f(self.uniforms.paint.glass, 0., 0., 0., 0.);
                        }
                        if let Some(stops) = backdrop {
                            for (location, rgb) in [
                                (self.uniforms.paint.app_top, stops[0]),
                                (self.uniforms.paint.app_bottom, stops[1]),
                            ] {
                                gl::Uniform3f(
                                    location,
                                    rgb[0] as f32 / 255.,
                                    rgb[1] as f32 / 255.,
                                    rgb[2] as f32 / 255.,
                                );
                            }
                        }
                        gl::DrawArrays(gl::TRIANGLES, *start as i32, *count as i32);
                    }
                    BatchKind::Vector {
                        start,
                        count,
                        mono,
                        deferred,
                    } => {
                        self.draw_vector_batch(*start, *count, *mono, *deferred);
                        paint_bound = false;
                    }
                    BatchKind::Text {
                        start,
                        count,
                        embolden,
                    } => {
                        self.draw_text_batch(*start, *count, f32::from_bits(*embolden));
                        paint_bound = false;
                    }
                }
                let batch_ms = batch_started.elapsed().as_secs_f64() * 1000.;
                match &batch.kind {
                    BatchKind::Paint { .. } => draw_paint_ms += batch_ms,
                    BatchKind::Vector { .. } => draw_vector_ms += batch_ms,
                    BatchKind::Text { .. } => draw_text_ms += batch_ms,
                }
            }
            draw_ms = draw_started.elapsed().as_secs_f64() * 1000.;
            gl::Disable(gl::SCISSOR_TEST);
        }
        let tail_started = Instant::now();
        // Keep the current scene, evict old full-resolution wallpapers before memory grows.
        let mut bytes: usize = self
            .textures
            .values()
            .flatten()
            .map(|t| t.width as usize * t.height as usize * 4)
            .sum();
        if bytes > TEXTURE_BUDGET {
            let mut old: Vec<_> = self
                .textures
                .iter()
                .filter_map(|(path, t)| {
                    t.as_ref()
                        .filter(|t| t.frame != self.frame)
                        .map(|t| (path.clone(), t.frame))
                })
                .collect();
            old.sort_by_key(|(_, frame)| *frame);
            for (path, _) in old {
                if bytes <= TEXTURE_BUDGET {
                    break;
                }
                if let Some(Some(t)) = self.textures.remove(&path) {
                    bytes -= t.width as usize * t.height as usize * 4;
                }
            }
        }
        self.trim_text_geometry();
        self.stats.vector_meshes = self.vectors.values().flatten().count();
        self.stats.vector_bytes = self.vectors.values().flatten().map(|mesh| mesh.bytes).sum();
        self.stats.artwork_bytes = bytes + self.stats.vector_bytes;
        self.stats.draw_calls = self.batches.len();
        self.stats.paint_batches = self
            .batches
            .iter()
            .filter(|batch| matches!(batch.kind, BatchKind::Paint { .. }))
            .count();
        self.stats.vector_batches = self
            .batches
            .iter()
            .filter(|batch| matches!(batch.kind, BatchKind::Vector { .. }))
            .count();
        self.stats.text_batches = self
            .batches
            .iter()
            .filter(|batch| matches!(batch.kind, BatchKind::Text { .. }))
            .count();
        self.stats.build_ms = built.duration_since(started).as_secs_f64() * 1000.;
        self.stats.clear_ms = clear_ms;
        self.stats.upload_ms = upload_ms;
        self.stats.upload_bytes = upload_bytes;
        self.stats.setup_ms = setup_ms;
        self.stats.draw_paint_ms = draw_paint_ms;
        self.stats.draw_vector_ms = draw_vector_ms;
        self.stats.draw_text_ms = draw_text_ms;
        self.stats.draw_ms = draw_ms;
        self.stats.tail_ms = tail_started.elapsed().as_secs_f64() * 1000.;
        self.stats.cpu_submission_ms = started.elapsed().as_secs_f64() * 1000.;
    }

    fn draw_text_batch(&self, start: usize, count: usize, embolden: f32) {
        let viewport = self.stats.viewport;
        let uniforms = &self.uniforms.text;
        unsafe {
            gl::UseProgram(self.text_program);
            gl::Uniform1f(
                uniforms.fringe,
                if self.stats.multisample_samples > 1 {
                    0.
                } else {
                    0.5
                },
            );
            gl::Uniform1f(uniforms.embolden, embolden);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.text_buffer);
            let mut offset = 0;
            for (index, size) in [2, 2, 4, 4, 1].into_iter().enumerate() {
                gl::EnableVertexAttribArray(index as u32);
                gl::VertexAttribPointer(
                    index as u32,
                    size,
                    gl::FLOAT,
                    gl::FALSE,
                    52,
                    (offset * size_of::<f32>()) as *const _,
                );
                offset += size as usize;
            }
            gl::Uniform4f(uniforms.transform, 1., 1., 0., 0.);
            gl::Uniform2f(
                uniforms.pixel_scale,
                viewport.width as f32 / 1920.,
                viewport.height as f32 / 1080.,
            );
            gl::Uniform1f(uniforms.mono, 0.);
            gl::DrawArrays(gl::TRIANGLES, start as i32, count as i32);
        }
    }

    fn draw_vector_batch(&self, start: usize, count: usize, mono: bool, deferred: bool) {
        let viewport = self.stats.viewport;
        let uniforms = &self.uniforms.vector;
        unsafe {
            gl::UseProgram(self.vector_program);
            gl::BindBuffer(
                gl::ARRAY_BUFFER,
                if deferred {
                    self.logo_buffer
                } else {
                    self.vector_buffer
                },
            );
            gl::DisableVertexAttribArray(4);
            let mut offset = 0;
            for (index, size) in [2, 2, 4, 1].into_iter().enumerate() {
                gl::EnableVertexAttribArray(index as u32);
                gl::VertexAttribPointer(
                    index as u32,
                    size,
                    gl::FLOAT,
                    gl::FALSE,
                    36,
                    (offset * size_of::<f32>()) as *const _,
                );
                offset += size as usize;
            }
            gl::Uniform4f(uniforms.transform, 1., 1., 0., 0.);
            gl::Uniform2f(
                uniforms.pixel_scale,
                viewport.width as f32 / 1920.,
                viewport.height as f32 / 1080.,
            );
            gl::Uniform4f(uniforms.tint, 1., 1., 1., 1.);
            gl::Uniform1f(uniforms.mono, if mono { 1. } else { 0. });
            gl::DrawArrays(gl::TRIANGLES, start as i32, count as i32);
        }
    }

    pub fn stats(&self) -> &RenderStats {
        &self.stats
    }

    pub fn screenshot(&self, path: &Path, width: u32, height: u32) -> Result<(), String> {
        let mut pixels = vec![0; width as usize * height as usize * 4];
        unsafe {
            gl::ReadPixels(
                0,
                0,
                width as i32,
                height as i32,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                pixels.as_mut_ptr().cast(),
            );
        }
        let mut image = image::RgbaImage::from_raw(width, height, pixels)
            .ok_or("Invalid framebuffer dimensions")?;
        image::imageops::flip_vertical_in_place(&mut image);
        image.save(path).map_err(|e| e.to_string())
    }

    /// Capture the authored TV surface, independent of desktop HiDPI/window limits.
    pub fn capture(
        &mut self,
        commands: &[Draw],
        path: &Path,
        size: Size,
    ) -> Result<RenderStats, String> {
        if size.width > self.info.max_texture_size
            || size.height > self.info.max_texture_size
            || size.width > self.info.max_viewport[0] as u32
            || size.height > self.info.max_viewport[1] as u32
        {
            return Err(format!(
                "Capture {}x{} exceeds GPU limits: {}px texture, {:?} viewport",
                size.width, size.height, self.info.max_texture_size, self.info.max_viewport
            ));
        }
        let image = texture(size.width, size.height, &[], NO_CAP);
        let mut framebuffer = 0;
        unsafe {
            gl::GenFramebuffers(1, &mut framebuffer);
            gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffer);
            gl::FramebufferTexture2D(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                image.id,
                0,
            );
            if gl::CheckFramebufferStatus(gl::FRAMEBUFFER) != gl::FRAMEBUFFER_COMPLETE {
                gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
                gl::DeleteFramebuffers(1, &framebuffer);
                return Err("Screenshot framebuffer is incomplete".into());
            }
        }
        self.render(commands, size.width, size.height, None);
        let stats = self.stats.clone();
        let result = self.screenshot(path, size.width, size.height);
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::DeleteFramebuffers(1, &framebuffer);
        }
        result.map(|_| stats)
    }

    /// Verification aid (LUMA_DAMAGE_VERIFY=1): compares the window's back buffer after a
    /// partial redraw with the same scene drawn in full offscreen at the window's sample count.
    /// Returns the pixels differing by more than 16 in a channel, and the largest difference.
    pub fn verify_window(
        &mut self,
        commands: &[Draw],
        width: u32,
        height: u32,
        save: Option<&Path>,
    ) -> (usize, u8) {
        let (w, h) = (width as i32, height as i32);
        let length = width as usize * height as usize * 4;
        let mut window = vec![0u8; length];
        let mut full = vec![0u8; length];
        let samples = self.stats.multisample_samples as i32;
        let stats = self.stats.clone();
        let mut framebuffers = [0; 2];
        let mut renderbuffers = [0; 2];
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::ReadPixels(
                0,
                0,
                w,
                h,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                window.as_mut_ptr().cast(),
            );
            gl::GenFramebuffers(2, framebuffers.as_mut_ptr());
            gl::GenRenderbuffers(2, renderbuffers.as_mut_ptr());
            gl::BindRenderbuffer(gl::RENDERBUFFER, renderbuffers[0]);
            if samples > 1 {
                gl::RenderbufferStorageMultisample(gl::RENDERBUFFER, samples, gl::RGBA8, w, h);
            } else {
                gl::RenderbufferStorage(gl::RENDERBUFFER, gl::RGBA8, w, h);
            }
            gl::BindRenderbuffer(gl::RENDERBUFFER, renderbuffers[1]);
            gl::RenderbufferStorage(gl::RENDERBUFFER, gl::RGBA8, w, h);
            for (framebuffer, renderbuffer) in framebuffers.into_iter().zip(renderbuffers) {
                gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffer);
                gl::FramebufferRenderbuffer(
                    gl::FRAMEBUFFER,
                    gl::COLOR_ATTACHMENT0,
                    gl::RENDERBUFFER,
                    renderbuffer,
                );
            }
            gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffers[0]);
        }
        self.render(commands, width, height, None);
        unsafe {
            gl::BindFramebuffer(gl::READ_FRAMEBUFFER, framebuffers[0]);
            gl::BindFramebuffer(gl::DRAW_FRAMEBUFFER, framebuffers[1]);
            gl::BlitFramebuffer(0, 0, w, h, 0, 0, w, h, gl::COLOR_BUFFER_BIT, gl::NEAREST);
            gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffers[1]);
            gl::ReadPixels(
                0,
                0,
                w,
                h,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                full.as_mut_ptr().cast(),
            );
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::DeleteFramebuffers(2, framebuffers.as_ptr());
            gl::DeleteRenderbuffers(2, renderbuffers.as_ptr());
        }
        self.stats = stats;
        let mut differing = 0;
        let mut largest = 0;
        for (a, b) in window.chunks_exact(4).zip(full.chunks_exact(4)) {
            let difference = (0..3).map(|i| a[i].abs_diff(b[i])).max().unwrap_or(0);
            largest = largest.max(difference);
            differing += usize::from(difference > 16);
        }
        if let Some(stem) = save.filter(|_| differing > 0) {
            for (name, pixels) in [("window", window), ("full", full)] {
                if let Some(image) = image::RgbaImage::from_raw(width, height, pixels) {
                    let image = image::imageops::flip_vertical(&image);
                    let _ = image.save(stem.with_extension(format!("{name}.png")));
                }
            }
        }
        (differing, largest)
    }
}

// Noto Sans SC (font-cjk) has no Hangul syllables; these go to font-kr instead.
// Keep in sync with is_hangul() in tools/font-weights.py, which builds font-kr.
fn is_hangul(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{11FF}'
            | '\u{3130}'..='\u{318F}'
            | '\u{A960}'..='\u{A97F}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{D7B0}'..='\u{D7FF}'
    )
}

fn load_font(directory: &Path, id: u16) -> Result<Font, String> {
    let name = if id >= 3000 {
        format!("font-kr-{}.ttf", id - 3000)
    } else if id >= 2000 {
        format!("font-cjk-{}.ttf", id - 2000)
    } else if id >= 1000 {
        format!("font-display-{}.ttf", id - 1000)
    } else {
        format!("font-{id}.ttf")
    };
    let path = directory.join(&name);
    let started = Instant::now();
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("Font {}: {e}. Regenerate native assets.", path.display()))?;
    let font = Font::from_bytes(bytes).map_err(|e| format!("Font {}: {e}", path.display()))?;
    if cfg!(feature = "webos") {
        eprintln!(
            "Luma font: {name} loaded in {} ms",
            started.elapsed().as_millis()
        );
    }
    Ok(font)
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteBuffers(1, &self.buffer);
            gl::DeleteProgram(self.program);
            gl::DeleteProgram(self.vector_program);
            gl::DeleteProgram(self.text_program);
            if self.vao != 0 {
                gl::DeleteVertexArrays(1, &self.vao);
            }
        }
    }
}

/// A cap that never shrinks anything: used for textures that are render targets or solid fills
/// rather than decoded image files.
const NO_CAP: (u32, u32) = (u32::MAX, u32::MAX);

fn texture(width: u32, height: u32, pixels: &[u8], cap: (u32, u32)) -> Texture {
    let mut id = 0;
    unsafe {
        gl::GenTextures(1, &mut id);
        gl::BindTexture(gl::TEXTURE_2D, id);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
        gl::TexImage2D(
            gl::TEXTURE_2D,
            0,
            gl::RGBA as i32,
            width as i32,
            height as i32,
            0,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            if pixels.is_empty() {
                std::ptr::null()
            } else {
                pixels.as_ptr().cast()
            },
        );
    }
    Texture {
        id,
        width,
        height,
        frame: 0,
        cap,
    }
}

/// Uploads RGBA8 as a texture, resampling it to `target` with Lanczos3 first when the decoded file
/// is bigger than the largest size this image is ever drawn at. The pixel format stays RGBA8, so
/// colours are not re-quantised and edges keep the same look; only the texel count - and the GPU's
/// per-frame downscaling work - goes away.
fn upload(width: u32, height: u32, pixels: &[u8], target: (u32, u32), cap: (u32, u32)) -> Texture {
    if target == (width, height) {
        return texture(width, height, pixels, cap);
    }
    let resized = crate::resample::lanczos3_rgba(pixels, width, height, target.0, target.1);
    texture(target.0, target.1, &resized, cap)
}

fn gl_string(name: GLenum) -> String {
    unsafe {
        let value = gl::GetString(name);
        if value.is_null() {
            "unknown".into()
        } else {
            CStr::from_ptr(value.cast()).to_string_lossy().into_owned()
        }
    }
}

fn compile(kind: GLenum, source: &str) -> Result<GLuint, String> {
    let source = CString::new(source).map_err(|e| e.to_string())?;
    unsafe {
        let shader = gl::CreateShader(kind);
        gl::ShaderSource(shader, 1, &source.as_ptr(), std::ptr::null());
        gl::CompileShader(shader);
        let mut success = 0;
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut success);
        if success == 0 {
            let mut log = vec![0u8; 4096];
            let mut len = 0;
            gl::GetShaderInfoLog(shader, log.len() as i32, &mut len, log.as_mut_ptr().cast());
            gl::DeleteShader(shader);
            return Err(format!(
                "Shader compilation: {}",
                String::from_utf8_lossy(&log[..len as usize])
            ));
        }
        Ok(shader)
    }
}

fn link(vertex: &str, fragment: &str, attributes: &[&CStr]) -> Result<GLuint, String> {
    let vertex = compile(gl::VERTEX_SHADER, vertex)?;
    let fragment = match compile(gl::FRAGMENT_SHADER, fragment) {
        Ok(fragment) => fragment,
        Err(error) => {
            unsafe {
                gl::DeleteShader(vertex);
            }
            return Err(error);
        }
    };
    unsafe {
        let program = gl::CreateProgram();
        gl::AttachShader(program, vertex);
        gl::AttachShader(program, fragment);
        for (index, name) in attributes.iter().enumerate() {
            gl::BindAttribLocation(program, index as u32, name.as_ptr());
        }
        gl::LinkProgram(program);
        gl::DeleteShader(vertex);
        gl::DeleteShader(fragment);
        let mut success = 0;
        gl::GetProgramiv(program, gl::LINK_STATUS, &mut success);
        if success == 0 {
            let mut log = vec![0u8; 4096];
            let mut len = 0;
            gl::GetProgramInfoLog(program, log.len() as i32, &mut len, log.as_mut_ptr().cast());
            gl::DeleteProgram(program);
            return Err(format!(
                "Shader linking: {}",
                String::from_utf8_lossy(&log[..len as usize])
            ));
        }
        Ok(program)
    }
}
