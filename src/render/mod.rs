//! wgpu 30 renderer for M0. Architecture:
//!
//! - **Quad pipeline**: instanced `TriangleStrip` quads with a shared WGSL shader
//!   (`SHADER`). Each instance encodes a 2D affine transform, RGBA tint, and UV rect.
//! - **Coordinate model**: centre-origin world space (x ∈ ±1, y ∈ ±1/ASPECT),
//!   letterboxed for non-16:9 windows. Canvas-to-world: 675 px → 1.0 world unit.
//! - **Draw pipeline**: `draw_to_view` builds a `DrawCmd` list
//!   → converts to `Instance` records → vertex‑buffer upload → scene render pass
//!   → post‑processing (see `post`) → UI overlay pass (iced + timeline).
//! - **Note rendering**: sprite‑based (textured) with coloured‑quad fallback.
//!   Holds use atlas textures (head + stretchable body + tail); _mh variants for
//!   multi‑press hints. Z‑order sorted by judge line, then kind priority.
//! - **Post‑processing**: ping‑pong render targets with per‑effect pipelines.
//!   Effects loaded from `extra.json` (grayscale, chromatic, glitch, vignette, etc.).
//! - **Hit effects**: sprite‑sheet burst particles (`fx.rs`).
//! - **Consumes** [`crate::core::FrameState`]; never touches serde models or IO.

use std::{borrow::Cow, collections::HashMap, mem::size_of, sync::Arc};

use anyhow::Context as _;
use winit::window::Window;

use crate::core::FrameState;

mod fx;
pub mod post;
pub mod preview;
pub mod shaders;
mod text;

pub use text::TextAnchor;

/// Playfield aspect ratio (w/h). 3:2 = native RPE canvas (1350×900).
const ASPECT: f32 = 3.0 / 2.0;

// Contract render-semantics constants, in RPE CANVAS pixels (1350×900).
const CANVAS_W: f32 = 675.0; // world x ±1 ↔ ±675 canvas px
const CANVAS_H: f32 = 450.0; // world y ±1 ↔ ±450 canvas px
/// Positions (not sizes!) are stretched ×1.5 vertically so the RPE canvas
/// (900 px tall) fills the playfield at any aspect, while sprites keep a
/// uniform pixel scale (no texture distortion).
const Y_STRETCH: f32 = CANVAS_W / CANVAS_H; // = 1.5
const LINE_LEN: f32 = 6.0 * CANVAS_W; // built-in line quad length
const LINE_THICK: f32 = 0.01 * CANVAS_H; // built-in line quad thickness
const NOTE_W: f32 = 0.22 * CANVAS_W; // note quad width factor (× note.scale)
const NOTE_H: f32 = 0.03 * CANVAS_H; // note quad height factor (× note.scale)
const HOLD_BODY_W: f32 = 0.1 * CANVAS_W; // hold body width factor (× note.scale)
/// prpr note sprite width: 2 × NOTE_WIDTH_RATIO_BASE (0.13175016).
const NOTE_SPRITE_W: f32 = 0.2635 * CANVAS_W; // 2×0.13175016 world ≈ 177.8 canvas px
/// Hold atlas cap height in px (official respack: 50px head at top, 50px tail at bottom).
const HOLD_CAP_PX: f32 = 50.0;

// Contract note colors (kind: u8 follows RPE numbering).
const TAP_COLOR: [f32; 3] = [0.2, 0.6, 1.0]; // kind 1
const HOLD_COLOR: [f32; 3] = [0.3, 0.7, 1.0]; // kind 2
const FLICK_COLOR: [f32; 3] = [1.0, 0.3, 0.5]; // kind 3
const DRAG_COLOR: [f32; 3] = [1.0, 0.85, 0.2]; // kind 4

const INITIAL_DRAW_CAPACITY: usize = 4096;

// ---------------------------------------------------------------------------
// 2D affine matrix, column-major, padded to WGSL uniform layout (3 × vec4).
// Columns: (a, b, 0), (c, d, 0), (tx, ty, 1) — i.e. x' = a·x + c·y + tx.
// ---------------------------------------------------------------------------

type Mat3 = [[f32; 4]; 3];

fn mat_translate(x: f32, y: f32) -> Mat3 {
    [[1., 0., 0., 0.], [0., 1., 0., 0.], [x, y, 1., 0.]]
}

fn mat_scale(x: f32, y: f32) -> Mat3 {
    [[x, 0., 0., 0.], [0., y, 0., 0.], [0., 0., 1., 0.]]
}

fn mat_rotate(rad: f32) -> Mat3 {
    let (s, c) = rad.sin_cos();
    [[c, s, 0., 0.], [-s, c, 0., 0.], [0., 0., 1., 0.]]
}

/// `a * b` (apply `b` first, then `a`).
fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut r = [[0.; 4]; 3];
    for j in 0..3 {
        for i in 0..3 {
            r[j][i] = a[0][i] * b[j][0] + a[1][i] * b[j][1] + a[2][i] * b[j][2];
        }
    }
    r
}

/// CPU-side per-quad assembly record; converted to [`Instance`] at upload.
/// `uv_rect`: xy = offset, zw = scale — `uv = xy + quad_uv * zw`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct DrawUniform {
    /// Column-major 2D affine transform (Mat3 padded to 3×vec4).
    model: Mat3,
    /// RGBA tint, pre-multiplied alpha in scene pass.
    color: [f32; 4],
    /// UV rectangle: [u0, v0, du, dv]; final UV = xy + quad_uv * zw.
    uv_rect: [f32; 4],
}

struct DrawCmd<'a> {
    uniform: DrawUniform,
    tex: &'a wgpu::BindGroup,
}

/// GPU instance record (64B): 2D affine + pad, tint, uv rect. One per quad,
/// Instance data consumed by the vertex shader (`SHADER → fn vs`).
///
/// The vertex shader treats this as four `vec4<f32>` instance attributes
/// (`VsIn` struct) and applies the affine parts as:
///
/// ```wgsl
/// // @location(0): m01 = (a, b, c, d)   — linear part of Mat3
/// // @location(1): m2p = (tx, ty, 0, 0)  — translation + padding
/// // @location(2): color                  — rgba tint
/// // @location(3): uv_rect = (u0, v0, du, dv) — UV rectangle
/// let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
/// let p = corner - 0.5; // [-0.5, +0.5]
/// world.x = a * p.x + c * p.y + tx;
/// world.y = b * p.x + d * p.y + ty;
/// uv = uv_rect.xy + corner * uv_rect.zw;
/// ```
///
/// `model` encodes a 2D affine transform `x' = M × x` in column-major order:
/// ```text
/// [a  c  tx]   [0   2   4]
/// [b  d  ty] = [1   3   5]
/// [0  0  1 ]   [6=0 7=0 /]
/// ```
///
/// Three draw types share this same layout, distinguished at construction:
///
/// | Type | `model` encodes | Consumer |
/// |------|----------------|----------|
/// | **Scene** (lines, notes, bg) | `letterbox × translate × rotate × scale(...)` | `line_m`, `note_m`, `bg_m` |
/// | **Texture** (custom judge line) | pixel→clip mapping | `tex_m` (see `Some(name)` case) |
/// | **UI overlay** (iced, timeline) | fullscreen NDC stretch | `mat_scale(2, 2)` + UV flip |
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    /// Affine matrix: [a, b, c, d, tx, ty, 0, 0].
    /// a,b,c,d = linear; tx,ty = translation; last 2 always 0.
    model: [f32; 8],
    /// RGBA tint (pre-multiplied alpha in scene pass).
    color: [f32; 4],
    /// UV rect: [u0, v0, du, dv]. Final UV = (u0, v0) + corner * (du, dv).
    /// V⵨ is standard GPU bottom-up; overlay textures pass (du=1, dv=-1) to flip.
    uv_rect: [f32; 4],
}

/// Pack the affine part of a column-major Mat3 into Instance.model format:
/// `[m00, m01, m10, m11, m20, m21, 0, 0]`.
fn instance_model(m: &Mat3) -> [f32; 8] {
    [m[0][0], m[0][1], m[1][0], m[1][1], m[2][0], m[2][1], 0., 0.]
}

/// Zero-overhead draw type tag — resolved at Instance construction, no
/// runtime dispatch.
enum DrawTag {
    /// 3D scene element: full letterbox × T × R × S transform.
    Scene,
    /// Custom judge-line texture: pixel–world mapping via `RPE_HEIGHT / win_h`.
    Texture { tex_w: f32, tex_h: f32, win_w: f32, win_h: f32 },
    /// UI overlay: fullscreen NDC quad with optional V-flip.
    Overlay,
}

const INSTANCE_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: size_of::<Instance>() as u64,
    step_mode: wgpu::VertexStepMode::Instance,
    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4],
};

struct TexEntry {
    bind_group: wgpu::BindGroup,
    /// Pixel size of the decoded image. prpr draws textured lines at natural
    /// pixel size × object scale (CoreEval's scale includes the ×2/1350 factor).
    size: [f32; 2],
}

const SHADER: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsIn {
    @location(0) m01: vec4<f32>,  // a, b, c, d
    @location(1) m2p: vec4<f32>,  // tx, ty, pad, pad
    @location(2) color: vec4<f32>,
    @location(3) uv_rect: vec4<f32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
};

// Unit quad (±0.5) as a triangle strip; one instance per quad.
@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: VsIn) -> VsOut {
    let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let p = corner - 0.5;
    let world = vec2<f32>(
        inst.m01.x * p.x + inst.m01.z * p.y + inst.m2p.x,
        inst.m01.y * p.x + inst.m01.w * p.y + inst.m2p.y,
    );
    var out: VsOut;
    out.pos = vec4<f32>(world, 0.0, 1.0);
    out.uv = inst.uv_rect.xy + corner * inst.uv_rect.zw;
    out.color = inst.color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv) * in.color;
}
"#;

/// Shared pipeline setup for the window and offscreen paths: quad shader,
/// texture bind-group layout + sampler, alpha-blended TriangleStrip instanced
/// quads. `format` is the color target (surface format or offscreen Rgba8Unorm).
///
/// The vertex shader expects per-instance attributes matching [`Instance`] layout:
/// `@location(0)`: `m01` (linear part `[a, b, c, d]`),
/// `@location(1)`: `m2p` (translation `[tx, ty, 0, 0]`),
/// `@location(2)`: `color` (RGBA),
/// `@location(3)`: `uv_rect` (`[u0, v0, du, dv]`).
/// See the `VsIn` struct inside `SHADER` for the WGSL definition.
pub(crate) fn create_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout, wgpu::Sampler) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("phimakor-quad"),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SHADER)),
    });

    let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&tex_bgl)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("phimakor-quad"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[Some(INSTANCE_LAYOUT)],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: None,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    (pipeline, tex_bgl, sampler)
}

/// wgpu-based renderer driving the main window (or offscreen preview).
/// Holds the device, queue, pipelines, double-buffered instance storage,
/// texture cache, background image, post-processing pipe, and UI overlay state.
pub struct Renderer {
    /// Window surface; `None` for surfaceless (offscreen preview) renderers.
    surface: Option<wgpu::Surface<'static>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Present configuration; `None` when surfaceless.
    config: Option<wgpu::SurfaceConfiguration>,
    /// Viewport size in px (drives the text overlay layout).
    size: [u32; 2],

    pipeline: wgpu::RenderPipeline,
    tex_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,

    /// Double-buffered instance storage; frames alternate so an in-flight
    /// frame's buffer is never overwritten by `queue.write_buffer`.
    instance_bufs: [wgpu::Buffer; 2],
    instance_capacity: usize, // in instances
    frame_idx: usize,         // ping-pong 0/1

    white: wgpu::BindGroup, // 1×1 white, for all solid quads
    textures: HashMap<String, TexEntry>,
    background: Option<(wgpu::BindGroup, [f32; 2])>,
    background_dim: f32,
    /// Playfield canvas aspect (w/h), hotkey-switchable.
    playfield_aspect: f32,
    /// Playback progress 0..1 for the top progress bar.
    progress: f32,
    pub vsync: bool,
    /// Post-processing pipeline (effects from extra.json).
    pub post: post::PostPipe,
    /// Intermediate scene texture (for post-processing).
    scene_tex: Option<wgpu::Texture>,
    scene_view: Option<wgpu::TextureView>,

    /// Live hit-effect bursts (see `fx.rs`).
    hit_fx: Vec<fx::HitFx>,
    /// Text overlay state (see `text.rs`).
    text: text::TextState,
    /// Persistent single-instance buffer for UI overlay draws.
    ui_inst_buf: wgpu::Buffer,
}

impl Renderer {
    /// Borrow the wgpu device.
    pub fn device(&self) -> &wgpu::Device { &self.device }
    /// Borrow the wgpu queue.
    pub fn queue(&self) -> &wgpu::Queue { &self.queue }
    /// Borrow the texture bind-group layout (shared by all textures).
    pub fn tex_bgl(&self) -> &wgpu::BindGroupLayout { &self.tex_bgl }
    /// Borrow the shared linear sampler.
    pub fn sampler(&self) -> &wgpu::Sampler { &self.sampler }

    /// Acquire the next surface texture. Returns `Err` variants that the caller
    /// should handle (Timeout/Occluded → skip frame, Validation → propagate).
    pub fn surface_acquire(&mut self) -> Result<wgpu::SurfaceTexture, wgpu::CurrentSurfaceTexture> {
        let _s = crate::trace_span!("surface_acquire");
        let surface = self.surface.as_ref().expect("surface_acquire on a surfaceless renderer");
        match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(st) | wgpu::CurrentSurfaceTexture::Suboptimal(st) => Ok(st),
            other => Err(other),
        }
    }

    /// Create a new windowed renderer: surface, adapter, device, pipelines,
    /// double-buffered instance storage, white texture, and post-processing pipe.
    pub async fn new(window: Arc<Window>) -> anyhow::Result<Self> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(window)
            .context("failed to create surface")?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .context("no suitable GPU adapter")?;

        let caps = surface.get_capabilities(&adapter);
        eprintln!("present modes available: {:?}", caps.present_modes);
        // Prefer a non-sRGB format so authored sRGB values pass through
        // unchanged (prpr colors are display-ready sRGB).
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 4,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
        };
        let (width, height) = (config.width, config.height);
        Self::init(Some((surface, config)), adapter, format, width, height).await
    }

    /// Surfaceless constructor for offscreen rendering (preview / embedding).
    /// No compatible surface on the adapter request; the target format is
    /// fixed to non-sRGB Rgba8Unorm, matching the window path's pass-through
    /// color choice.
    pub(crate) async fn new_surfaceless(width: u32, height: u32) -> anyhow::Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .context("no suitable GPU adapter")?;
        Self::init(None, adapter, wgpu::TextureFormat::Rgba8Unorm, width.max(1), height.max(1)).await
    }

    /// Shared construction for the window and surfaceless paths: device/queue,
    /// pipeline (via [`create_pipeline`]), instance double-buffer, white
    /// texture. A passed-in surface is configured with its config here.
    async fn init(
        surface: Option<(wgpu::Surface<'static>, wgpu::SurfaceConfiguration)>,
        adapter: wgpu::Adapter,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Self> {
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: None,
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                experimental_features: wgpu::ExperimentalFeatures::default(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::Off,
            })
            .await
            .context("failed to create device")?;

        let (surface, config) = match surface {
            Some((surface, config)) => {
                surface.configure(&device, &config);
                (Some(surface), Some(config))
            }
            None => (None, None),
        };

        let (pipeline, tex_bgl, sampler) = create_pipeline(&device, format);

        let instance_bufs = [
            Self::make_instance_buf(&device, INITIAL_DRAW_CAPACITY),
            Self::make_instance_buf(&device, INITIAL_DRAW_CAPACITY),
        ];

        let white_tex = Self::create_texture(&device, &queue, &[255; 4], 1, 1);
        let white = Self::texture_bind_group(&device, &tex_bgl, &sampler, &white_tex);

        let post = post::PostPipe::new(&device, width, height, format);
        let scene_tex = Some(post::PostPipe::make_target2(&device, width, height, "scene", format));
        let scene_view = Some(scene_tex.as_ref().unwrap().create_view(&wgpu::TextureViewDescriptor::default()));
        let ui_inst_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-inst"),
            size: std::mem::size_of::<Instance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self {
            surface,
            device,
            queue,
            config,
            size: [width, height],
            pipeline,
            tex_bgl,
            sampler,
            instance_bufs,
            instance_capacity: INITIAL_DRAW_CAPACITY,
            frame_idx: 0,
            white,
            textures: HashMap::new(),
            background: None,
            background_dim: 1.0,
            playfield_aspect: ASPECT,
            progress: 0.0,
            post,
            scene_tex,
            scene_view,
            hit_fx: Vec::new(),
            text: text::TextState::new(),
            vsync: true,
            ui_inst_buf,
        })
    }

    fn make_instance_buf(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (capacity * size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn create_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> wgpu::Texture {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        texture
    }

    fn texture_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        texture: &wgpu::Texture,
    ) -> wgpu::BindGroup {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }

    fn upload_image(&self, bytes: &[u8]) -> anyhow::Result<(wgpu::BindGroup, [f32; 2])> {
        let mut img = image::load_from_memory(bytes)
            .context("failed to decode image")?
            .to_rgba8();
        // wgpu v=0 is the first uploaded row; flip so v=0 is the image TOP,
        // keeping shader UV math (incl. hold atlas) in image space.
        image::imageops::flip_vertical_in_place(&mut img);
        let (w, h) = (img.width().max(1), img.height().max(1));
        let texture = Self::create_texture(&self.device, &self.queue, img.as_raw(), w, h);
        let bind_group = Self::texture_bind_group(&self.device, &self.tex_bgl, &self.sampler, &texture);
        Ok((bind_group, [w as f32, h as f32]))
    }

    fn reconfigure(&self) {
        if let (Some(surface), Some(config)) = (&self.surface, &self.config) {
            surface.configure(&self.device, config);
        }
    }

    /// Clear all pending hit-effect bursts.
    pub fn clear_hit_fx(&mut self) { self.hit_fx.clear(); }

    /// Viewport size in pixels.
    pub fn size(&self) -> [u32; 2] { self.size }

    /// Enable or disable V-sync (reconfigures the surface present mode).
    pub fn set_vsync(&mut self, enabled: bool) {
        self.vsync = enabled;
        if let Some(config) = &mut self.config {
            config.present_mode = if enabled { wgpu::PresentMode::AutoVsync } else { wgpu::PresentMode::AutoNoVsync };
            self.reconfigure();
        }
    }

    /// Resize the viewport, surface, and post-processing targets.
    /// Ignores zero dimensions (minimised window).
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return; // minimized; keep last valid config
        }
        self.size = [width, height];
        self.post.resize(&self.device, width, height);
        // Recreate scene texture at new size
        let fmt = self.config.as_ref().map(|c| c.format).unwrap_or(wgpu::TextureFormat::Rgba8Unorm);
        self.scene_tex = Some(post::PostPipe::make_target2(&self.device, width, height, "scene", fmt));
        self.scene_view = Some(self.scene_tex.as_ref().unwrap().create_view(&wgpu::TextureViewDescriptor::default()));
        if let Some(config) = &mut self.config {
            config.width = width;
            config.height = height;
            self.reconfigure();
        }
    }

    pub fn set_background(&mut self, img_bytes: &[u8], dim: f32) -> anyhow::Result<()> {
        // Static Gaussian blur at load time.
        let img = image::load_from_memory(img_bytes)
            .context("background decode")?
            .to_rgba8();
        let img = image::imageops::blur(&img, 8.0); // σ = 8 px, returns new image
        let (w, h) = (img.width().max(1), img.height().max(1));
        let texture = Self::create_texture(&self.device, &self.queue, img.as_raw(), w, h);
        let bind_group = Self::texture_bind_group(&self.device, &self.tex_bgl, &self.sampler, &texture);
        self.background = Some((bind_group, [w as f32, h as f32]));
        self.background_dim = dim;
        Ok(())
    }

    /// Switch the playfield canvas aspect (w/h) at runtime.
    pub fn set_playfield_aspect(&mut self, aspect: f32) {
        self.playfield_aspect = aspect;
    }

    /// Update the top progress bar (0..1).
    pub fn set_progress(&mut self, progress: f32) {
        self.progress = progress.clamp(0.0, 1.0);
    }

    pub fn load_texture(&mut self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let (bind_group, size) = self.upload_image(bytes).with_context(|| format!("texture {name:?}"))?;
        self.textures.insert(name.to_string(), TexEntry { bind_group, size });
        Ok(())
    }

    /// Draw one frame. wgpu 30 removed `wgpu::SurfaceError`; acquisition
    /// failures are reported via `wgpu::CurrentSurfaceTexture`. Lost/Outdated
    /// are handled here by reconfiguring with the stored size (frame skipped);
    /// Timeout/Occluded skip silently; only Validation is returned as `Err`.
    pub fn render(
        &mut self,
        frame: &FrameState,
        window_aspect: f32,
        dim: f32,
    ) -> Result<(), wgpu::CurrentSurfaceTexture> {
        let surface = self.surface.as_ref().expect("render() requires a window surface");
        let st = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(st) | wgpu::CurrentSurfaceTexture::Suboptimal(st) => st,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => return Ok(()),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.reconfigure();
                return Ok(());
            }
            other => return Err(other), // Validation
        };
        let view = st.texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.draw_to_view(&view, frame, window_aspect, dim, None, None);
        self.queue.present(st);
        Ok(())
    }

    /// Build this frame's draw list and execute the render pass into `view`
    /// (window surface view or offscreen target). Submits on the internal
    /// queue; the caller presents (window path) or copies out (preview path).
    /// `dim`: global brightness multiplied into every quad's rgb (alpha kept).
    /// If `ui_overlay` is `Some`, a fullscreen textured quad is drawn last.
    /// If `ui_iced` is also `Some`, it is drawn BEFORE `ui_overlay` (so the
    /// iced background sits under the per-frame overlay — no CPU copy needed).
    pub(crate) fn draw_to_view(
        &mut self,
        view: &wgpu::TextureView,
        frame: &FrameState,
        window_aspect: f32,
        dim: f32,
        ui_overlay: Option<&wgpu::BindGroup>,
        ui_iced: Option<&wgpu::BindGroup>,
    ) {
        let _s = crate::trace_span!("draw_to_view");
        // ── Coordinate model ──────────────────────────────────────────────
        // The RPE canvas is 1350×900 px (x ±675, y ±450). The playfield fills
        // the window at ANY playfield aspect (Tab cycles 3:2 → 16:9 → 4:3 →
        // 1:1): the RPE canvas stretches to fill the playfield box, so y=1
        // (450 px) is always at the visible top. Canvas px → world:
        //
        //   world_x = canvas_x / 675
        //   world_y = canvas_y / 450
        //
        // kx/ky then letterbox the playfield into the window, preserving the
        // playfield's aspect ratio (no shear, uniform pixel scale per axis).
        // ──────────────────────────────────────────────────────────────────
        let aspect = self.playfield_aspect;
        let (kx, ky) = if window_aspect >= aspect {
            (aspect / window_aspect, 1.0)
        } else {
            (1.0, window_aspect / aspect)
        };
        // The RPE canvas (1350×900) is scaled UNIFORMLY by S to fit the
        // playfield box (1350 × 1350/aspect px): y=1 (450 px) maps to the
        // box's visible top at any aspect (16:9 shrinks the canvas with side
        // bars; 3:2 fills it), sprites never distort, and nothing clips.
        // kx/ky then letterbox the playfield into the window.
        let fit = (1350.0 / aspect).min(900.0) / 900.0; // uniform canvas scale
        let letterbox = mat_scale(kx * fit / CANVAS_W, ky * aspect * fit / CANVAS_W);

        // Rough pre-estimate for the cmds vec capacity.
        let needed = 2 + frame
            .lines
            .iter()
            .map(|l| 1 + 3 * l.notes.len())
            .sum::<usize>();
        let mut cmds: Vec<DrawCmd> = Vec::with_capacity(needed);

        // Background: cover-fill with Gaussian-blurred illustration + 30 % black
        // overlay so judge lines / notes pop against any image.
        if let Some((bg, size)) = &self.background {
            let d = self.background_dim;
            let r = size[0] / size[1];
            let uv = if r > aspect {
                let w = aspect / r;
                [(1.0 - w) * 0.5, 1.0, w, -1.0]
            } else {
                let h = r / aspect;
                [0.0, (1.0 + h) * 0.5, 1.0, -h]
            };
            // Cover the full RPE canvas (1350×900): it stretches to the
            // playfield box at any aspect.
            let bg_m = mat_mul(&letterbox, &mat_scale(1350.0, 1350.0 / aspect));
            cmds.push(DrawCmd {
                uniform: DrawUniform { model: bg_m, color: [d, d, d, 1.0], uv_rect: uv },
                tex: bg,
            });
            // 30 % black overlay
            cmds.push(DrawCmd {
                uniform: DrawUniform { model: bg_m, color: [0., 0., 0., 0.3], uv_rect: [0., 0., 1., 1.] },
                tex: &self.white,
            });
        }

        // Track original index before z-sort for selection highlight
        let mut lines: Vec<(usize, &crate::core::LineState)> = frame.lines.iter().enumerate().collect();
        lines.sort_by_key(|(_, l)| l.z_order);

        for (_orig_i, line) in &lines {
            if line.pe_hide || line.attach_ui.is_some() { continue; }
            // [E] CtrlObject: ctrl_alpha scales the line's final alpha.
            let line_alpha = line.alpha * line.ctrl_alpha;
            // T * R * S: translate to position, rotate around self, scale
            // [E] CtrlObject: ctrl_pos is a multiplier (phira applies to incline);
            // ctrl_size scales the line.
            let ctrl_px = line.position[0] * CANVAS_W;
            let ctrl_py = line.position[1] * CANVAS_H;
            let line_m = mat_mul(
                &letterbox,
                &mat_mul(
                    &mat_translate(ctrl_px, ctrl_py),
                    &mat_mul(
                        &mat_rotate(line.rotation),
                        &mat_scale(line.scale[0] * line.ctrl_size_x, line.scale[1] * line.ctrl_size_y),
                    ),
                ),
            );

            // Line quad.
            match &line.texture {
                None => {
                    let c = line.color;
                    cmds.push(DrawCmd {
                        uniform: DrawUniform {
                            model: mat_mul(&line_m, &mat_scale(LINE_LEN, LINE_THICK)),
                            color: [c[0], c[1], c[2], c[3] * line_alpha],
                            uv_rect: [0., 0., 1., 1.],
                        },
                        tex: &self.white,
                    });
                }
                Some(name) => {
                    let c = line.color;
                    if let Some(t) = self.textures.get(name) {
                        // [Texture] raw_scale = line.scale / (2/1350). For lines without
                        // scale events, line.scale = 1.0 (identity) but the implicit factor
                        // 2/RPE_WIDTH must still apply → fall back to 2.0.
                        let raw_sx = if (line.scale[0] - 1.0).abs() < 1e-6 { 2.0 } else { line.scale[0] * 675.0 };
                        let raw_sy = if (line.scale[1] - 1.0).abs() < 1e-6 { 2.0 } else { line.scale[1] * 675.0 };
                        let tw = t.size[0] * raw_sx / kx;
                        let th = t.size[1] * raw_sy / kx;
                        let tex_m = mat_mul(
                            &letterbox,
                            &mat_mul(
                                &mat_translate(ctrl_px, ctrl_py),
                                &mat_mul(&mat_rotate(line.rotation), &mat_scale(tw, th)),
                            ),
                        );
                        cmds.push(DrawCmd {
                            uniform: DrawUniform {
                                model: tex_m,
                                color: [c[0], c[1], c[2], c[3] * line_alpha],
                                uv_rect: [0., 0., 1., 1.],
                            },
                            tex: &t.bind_group,
                        });
                    } else {
                        // Fallback: draw default white bar if texture not found
                        cmds.push(DrawCmd {
                            uniform: DrawUniform {
                                model: mat_mul(&line_m, &mat_scale(LINE_LEN, LINE_THICK)),
                                color: [c[0], c[1], c[2], c[3] * line_alpha],
                                uv_rect: [0., 0., 1., 1.],
                            },
                            tex: &self.white,
                        });
                    }
                }
            }



            // Notes follow the line's translation AND rotation, but NOT its
            // scale or alpha (locked product decision).
            let note_m = mat_mul(
                &letterbox,
                &mat_mul(
                    &mat_translate(ctrl_px, ctrl_py),
                    &mat_rotate(line.rotation),
                ),
            );
            // Notes: below-notes first, above-notes after (contract).
            // Within each group draw by kind priority (prpr NoteKind::order):
            // hold at the bottom, then drag, tap, flick on top.
            for &above in &[false, true] {
                for kind in [2u8, 4, 1, 3] {
                    for note in line.notes.iter().filter(|n| n.above == above && n.kind == kind) {
                    let rgb = match note.kind {
                        1 => TAP_COLOR,
                        2 => HOLD_COLOR,
                        3 => FLICK_COLOR,
                        4 => DRAG_COLOR,
                        _ => continue,
                    };
                    // Multi-press hint: prefer the _mh sprite when flagged and
                    // loaded, else the normal sprite, else colored quads.
                    let (base, mh) = match note.kind {
                        1 => ("note:click", "note:click_mh"),
                        2 => ("note:hold", "note:hold_mh"),
                        3 => ("note:flick", "note:flick_mh"),
                        4 => ("note:drag", "note:drag_mh"),
                        _ => continue,
                    };
                    // prpr: _mh sprites render wider by mh_width/normal_width
                    // (note.rs scale factor), e.g. 1089/989 ≈ 1.10 for click.
                    let (sprite, mh_factor, is_mh) = if note.multiple_hint {
                        match (self.textures.get(mh), self.textures.get(base)) {
                            (Some(m), Some(b)) => (Some(m), m.size[0] / b.size[0], true),
                            (m, b) => (m.or(b), 1.0, false),
                        }
                    } else {
                        (self.textures.get(base), 1.0, false)
                    };
                    let mut alpha = note.alpha;
                    // fake notes render at full opacity (character art etc.)
                    if alpha <= 0.0 {
                        continue;
                    }
                    // [F] Incline: perspective X distortion based on note Y position
                    let incline_factor = 1.0 - line.incline_sin * note.relative[1] * 0.5;
                    // [E] CtrlObject from LineState (evaluated in state_at)
                    let ctrl_y = line.ctrl_y;
                    let x = note.relative[0] * CANVAS_W;
                    // [E] ctrl_y scales the note's relative Y position
                    let y = note.relative[1] * CANVAS_H * ctrl_y;
                    let note_base = mat_mul(&note_m, &mat_translate(x, y));

                    match (note.kind, sprite) {
                        (1 | 3 | 4, Some(t)) => {
                            let w = NOTE_SPRITE_W * note.scale * mh_factor * incline_factor;
                            let h = w * t.size[1] / t.size[0];
                            cmds.push(DrawCmd {
                                uniform: DrawUniform {
                                    model: mat_mul(&note_base, &mat_scale(w, h)),
                                    color: [1.0, 1.0, 1.0, alpha],
                                    uv_rect: [0., 0., 1., 1.],
                                },
                                tex: &t.bind_group,
                            });
                        }
                        // Textured hold: atlas = TAIL (image top) + stretchable
                        // body + HEAD (image bottom). _mh atlas has a taller
                        // head (holdAtlasMH = [50 tail, 95 head]). Textures are
                        // flipped at upload: head = v0, tail = v1. body→head→tail.
                        (2, Some(t)) => {
                            let w = NOTE_SPRITE_W * note.scale * mh_factor * incline_factor;
                            let (tail_px, head_px) = if is_mh { (50.0, 95.0) } else { (HOLD_CAP_PX, HOLD_CAP_PX) };
                            let head_uv = head_px / t.size[1]; // v0-fraction for head
                            let tail_uv = tail_px / t.size[1];
                            let head_h = w * head_px / t.size[0]; // quad heights
                            let tail_h = w * tail_px / t.size[0];
                            let tint = [1.0, 1.0, 1.0, alpha];
                            // [B] chart.rs negates relative[1] for below notes; mirror UV
                            // so the hold body gradient (head→tail) stays correct.
                            let v_at = |v0: f32, len: f32| -> [f32; 4] {
                                if note.above { [0., v0, 1., len] } else { [0., v0 + len, 1., -len] }
                            };
                            // [B] Hold body/head/tail each get below_rot at their position
                            let hd = |cy: f32| mat_mul(&note_m, &mat_mul(
                                &mat_translate(x, cy), &mat_scale(w, w),
                            ));
                            if let Some(end_y) = note.hold_end_y {
                                let y1 = end_y as f32 * CANVAS_H * ctrl_y;
                                let (head_y, tail_y) = (y, y1);
                                let h = (tail_y - head_y).abs();
                                if h > 1e-5 {
                                    cmds.push(DrawCmd {
                                        uniform: DrawUniform {
                                            model: mat_mul(&hd((head_y + tail_y) * 0.5), &mat_scale(1.0, h / w)),
                                            color: tint,
                                            uv_rect: v_at(head_uv, 1. - head_uv - tail_uv),
                                        },
                                        tex: &t.bind_group,
                                    });
                                }
                                for (cy, v0, len, quad_h) in [
                                    (head_y, 0.0, head_uv, head_h),
                                    (tail_y, 1.0 - tail_uv, tail_uv, tail_h),
                                ] {
                                    cmds.push(DrawCmd {
                                        uniform: DrawUniform {
                                            model: mat_mul(&hd(cy), &mat_scale(1.0, quad_h / w)),
                                            color: tint,
                                            uv_rect: v_at(v0, len),
                                        },
                                        tex: &t.bind_group,
                                    });
                                }
                            } else {
                                cmds.push(DrawCmd {
                                    uniform: DrawUniform {
                                        model: mat_mul(&hd(y), &mat_scale(1.0, head_h / w)),
                                        color: tint,
                                        uv_rect: v_at(0., head_uv),
                                    },
                                    tex: &t.bind_group,
                                });
                            }
                        }
                        // Colored-quad fallback (no sprite loaded).
                        _ => {
                            // [B] below rotation applies to all fallback quads too
                            let nb = || mat_mul(&note_m, &mat_translate(x, y));
                            if note.kind == 2 {
                                if let Some(end_y) = note.hold_end_y {
                                let y1 = end_y as f32 * CANVAS_H * ctrl_y;
                                    let h = (y1 - y).abs();
                                    if h > 1e-5 {
                                        cmds.push(DrawCmd {
                                            uniform: DrawUniform {
                                                model: mat_mul(&nb(), &mat_scale(HOLD_BODY_W * note.scale * incline_factor, h)),
                                                color: [rgb[0], rgb[1], rgb[2], alpha],
                                                uv_rect: [0., 0., 1., 1.],
                                            },
                                            tex: &self.white,
                                        });
                                    }
                                }
                            }
                            cmds.push(DrawCmd {
                                uniform: DrawUniform {
                                    model: mat_mul(&nb(), &mat_scale(NOTE_W * note.scale * incline_factor, NOTE_H * note.scale)),
                                    color: [rgb[0], rgb[1], rgb[2], alpha],
                                    uv_rect: [0., 0., 1., 1.],
                                },
                                tex: &self.white,
                            });
                        }
                    }
                }
            }
        }
        }

        // attachUI labels: show where the game UI elements (score/combo/pause/
        // name/level) sit, following each line's transform. "bar" is drawn as
        // the bound progress bar above instead. Field borrows only — `cmds`
        // still borrows self.background/self.white/self.textures.
        for line in frame.lines.iter() {
            let Some(ui) = line.attach_ui.as_deref() else { continue };
            if ui == "bar" || line.pe_hide || line.alpha <= 0.0 { continue; }
            let pos = [line.position[0] * CANVAS_W, line.position[1] * CANVAS_H];
            let a = line.alpha * line.ctrl_alpha;
            text::draw_text_world(
                &mut self.text,
                &self.device,
                &self.queue,
                &self.tex_bgl,
                &self.sampler,
                aspect,
                &ui.to_uppercase(),
                pos,
                line.rotation,
                [1.0, 1.0, 1.0, a],
            );
        }

        // Hit effects: sprite-sheet bursts in canvas-pixel space, after notes.
        // Progress bar: bound to the attachUI "bar" line when present (phira
        // UIElement::Bar follows the line transform); falls back to the visible
        // canvas top edge otherwise.
        if self.progress > 0.0 {
            let bar_h = 5.0;
            let top = CANVAS_H; // canvas top: y=450 maps to the playfield top at any aspect
            let bar_w = 1350.0 * self.progress;
            if let Some(bl) = frame.lines.iter().find(|l| l.attach_ui.as_deref() == Some("bar")) {
                if !bl.pe_hide && bl.alpha > 0.0 {
                    let bl_alpha = bl.alpha * bl.ctrl_alpha;
                    // Bar rect lives in viewport space (anchored at the visible
                    // canvas top-left, like phira's UIElement::Bar Rect(-1, top, ...));
                    // the attachUI line's transform then moves/rotates/scales it.
                    let bar_local = mat_mul(
                        &mat_translate(-CANVAS_W + bar_w * 0.5, top - bar_h * 0.5),
                        &mat_scale(bar_w, bar_h),
                    );
                    let bar_m = mat_mul(
                        &letterbox,
                        &mat_mul(
                            &mat_mul(
                                &mat_translate(bl.position[0] * CANVAS_W, bl.position[1] * CANVAS_H),
                                &mat_rotate(bl.rotation),
                            ),
                            &mat_mul(
                                &mat_scale(bl.scale[0] * bl.ctrl_size_x, bl.scale[1] * bl.ctrl_size_y),
                                &bar_local,
                            ),
                        ),
                    );
                    cmds.push(DrawCmd {
                        uniform: DrawUniform {
                            model: bar_m,
                            color: [1.0, 1.0, 1.0, 0.9 * bl_alpha],
                            uv_rect: [0., 0., 1., 1.],
                        },
                        tex: &self.white,
                    });
                }
            } else {
                cmds.push(DrawCmd {
                    uniform: DrawUniform {
                        model: mat_mul(
                            &letterbox,
                            &mat_mul(
                                &mat_translate(-CANVAS_W + bar_w * 0.5, top - bar_h * 0.5),
                                &mat_scale(bar_w, bar_h),
                            ),
                        ),
                        color: [1.0, 1.0, 1.0, 0.9],
                        uv_rect: [0., 0., 1., 1.],
                    },
                    tex: &self.white,
                });
            }
        }

        // Field-split call: `cmds` already borrows `self.textures`/`self.white`.
        Self::push_hit_fx(&mut self.hit_fx, &self.textures, &mut cmds, &letterbox);

        // Text overlay (Phaser UI), on top of everything; queue is per-frame.
        text::push_text(
            &mut self.text.pending,
            &self.text.cache,
            &mut cmds,
            &letterbox,
            [self.size[0] as f32, self.size[1] as f32],
        );

        // Convert cmds to instances (applying the global rgb dim on the CPU),
        // growing both instance buffers together when capacity falls short.
        if cmds.len() > self.instance_capacity {
            // Smooth growth: 1.5× current, minimum +512. Avoids the double-
            // capacity burst that causes visible frame spikes on D3D12.
            self.instance_capacity = (cmds.len() as f64 * 1.5) as usize + 512;
            self.instance_bufs = [
                Self::make_instance_buf(&self.device, self.instance_capacity),
                Self::make_instance_buf(&self.device, self.instance_capacity),
            ];
        }
        // UI overlay is NOT pushed to cmds here — it's drawn on the surface
        // AFTER effects, so that post-processing doesn't affect the UI.
        // The overlay is drawn in a separate render pass on the surface view
        // (see Step 4 below).

        let _s1 = crate::trace_span!("draw_cmds_build");
        let instances: Vec<Instance> = cmds
            .iter()
            .map(|cmd| {
                let u = &cmd.uniform;
                Instance {
                    model: instance_model(&u.model),
                    color: [u.color[0] * dim, u.color[1] * dim, u.color[2] * dim, u.color[3]],
                    uv_rect: u.uv_rect,
                }
            })
            .collect();
        drop(_s1);

        let _s2 = crate::trace_span!("draw_upload_submit");
        if !instances.is_empty() {
            self.queue.write_buffer(
                &self.instance_bufs[self.frame_idx],
                0,
                bytemuck::cast_slice(&instances),
            );
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        // Render 3D scene to intermediate (if effects active) or directly to surface
        let has_effects = { let p = &self.post; !p.active.is_empty() };
        let scene_view: &wgpu::TextureView = if has_effects {
            self.scene_view.as_ref().unwrap()
        } else {
            &view
        };
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: scene_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.instance_bufs[self.frame_idx].slice(..));
            let mut start = 0usize;
            for i in 1..=cmds.len() {
                if i == cmds.len() || !std::ptr::eq(cmds[i].tex, cmds[start].tex) {
                    pass.set_bind_group(0, cmds[start].tex, &[]);
                    pass.draw(0..4, start as u32..i as u32);
                    start = i;
                }
            }
        }
        // Step 2: Run post-processing effects (if any)
        if has_effects {
            self.post.apply(&mut encoder, &self.device, &self.queue, scene_view);
        }
        // Step 3: Blit final to surface (only when using intermediate texture)
        if has_effects {
            let (blit_pipe, screen_bgl, sampler, final_view) = {
                let p = &self.post;
                (p.blit_pipeline.as_ref().unwrap(), &p.screen_bgl, &p.sampler, p.last_view())
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(blit_pipe);
            let screen_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("blit-bg"),
                layout: screen_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(final_view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
                ],
            });
            pass.set_bind_group(0, &screen_bg, &[]);
            pass.draw(0..3, 0..1);
        }
        // Step 4: UI overlay (iced + timeline) on the surface, never post-processed
        let ui_instance = Instance {
            model: [2.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0],
            color: [1.0, 1.0, 1.0, 1.0],
            uv_rect: [0., 1., 1., -1.],
        };
        self.queue.write_buffer(&self.ui_inst_buf, 0, bytemuck::cast_slice(&[ui_instance]));
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui-overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.ui_inst_buf.slice(..));
            if let Some(bg) = ui_iced {
                pass.set_bind_group(0, bg, &[]);
                pass.draw(0..4, 0..1);
            }
            if let Some(bg) = ui_overlay {
                pass.set_bind_group(0, bg, &[]);
                pass.draw(0..4, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.frame_idx ^= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: &Mat3, x: f32, y: f32) -> (f32, f32) {
        (
            m[0][0] * x + m[1][0] * y + m[2][0],
            m[0][1] * x + m[1][1] * y + m[2][1],
        )
    }

    #[test]
    fn line_transform_composes_translate_rotate_scale() {
        // T(0.3,-0.2) · R(90°) · S(1,1) applied to (0, 0.5): rotate first
        // (→ (-0.5, 0)), then translate (→ (-0.2, -0.2)).
        let m = mat_mul(
            &mat_translate(0.3, -0.2),
            &mat_mul(&mat_rotate(std::f32::consts::FRAC_PI_2), &mat_scale(1., 1.)),
        );
        let (x, y) = apply(&m, 0., 0.5);
        assert!((x + 0.2).abs() < 1e-6 && (y + 0.2).abs() < 1e-6, "({x}, {y})");
    }

    #[test]
    fn instance_model_packs_column_major_affine() {
        // T(3,-2) · R(90°): a=cos=0, b=sin=1, c=-sin=-1, d=cos=0, tx=3, ty=-2.
        // (1,0) rotates to (0,1), then translates to (3,-1).
        let m = mat_mul(&mat_translate(3., -2.), &mat_rotate(std::f32::consts::FRAC_PI_2));
        let p = instance_model(&m);
        let apply = |x: f32, y: f32| (p[0] * x + p[2] * y + p[4], p[1] * x + p[3] * y + p[5]);
        let (x, y) = apply(1., 0.);
        assert!((x - 3.).abs() < 1e-6 && (y + 1.).abs() < 1e-6, "({x}, {y})");
        assert!(p[6] == 0. && p[7] == 0., "pad must stay zero");
        assert_eq!(size_of::<Instance>(), 64);
    }

    #[test]
    fn letterbox_maps_internal_rect_to_playfield() {
        // 4:3 window (1.333 < 3:2): canvas x ±675px fills width, y letterboxed.
        let ky = (4. / 3.) / super::ASPECT;
        let l = mat_scale(1. / 675., ky / 450.);
        let (x, y) = apply(&l, 675., 450.);
        assert!((x - 1.).abs() < 1e-4 && (y - ky).abs() < 1e-4, "({x}, {y})");
    }
}
