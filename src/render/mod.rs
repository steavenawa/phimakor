//! wgpu 30 renderer for M0: procedural colored quads + named textures.
//!
//! Consumes [`crate::core::FrameState`] only; never touches serde models or IO.
//!
//! Internal coordinates (see `crate::core`): center origin, x ∈ [-1, 1],
//! y down-positive (same convention as prpr's world space). The playfield
//! (16:9, letterboxed with black bars) spans world x ±1, world y ±1/ASPECT;
//! the letterbox scale is pixel-uniform so rotated quads stay rectangular.

use std::{borrow::Cow, collections::HashMap, mem::size_of, sync::Arc};

use anyhow::Context as _;
use winit::window::Window;

use crate::core::FrameState;

mod fx;
pub mod preview;
mod text;

pub use text::TextAnchor;

/// Playfield aspect ratio (w/h). 3:2 = native RPE canvas (1350×900).
const ASPECT: f32 = 3.0 / 2.0;

// Contract render-semantics constants, in RPE CANVAS pixels (1350×900).
const CANVAS_W: f32 = 675.0; // world x ±1 ↔ ±675 canvas px
const CANVAS_H: f32 = 450.0; // world y ±1 ↔ ±450 canvas px
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
    model: Mat3,
    color: [f32; 4],
    uv_rect: [f32; 4],
}

struct DrawCmd<'a> {
    uniform: DrawUniform,
    tex: &'a wgpu::BindGroup,
}

/// GPU instance record (64B): 2D affine + pad, tint, uv rect. One per quad,
/// fed to the vertex shader as instance attributes (no uniforms).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    /// a, b, c, d, tx, ty (from column-major Mat3) + 2 pad floats.
    model: [f32; 8],
    color: [f32; 4],
    uv_rect: [f32; 4],
}

/// Pack the affine part of a column-major Mat3: x' = a·x + c·y + tx.
fn instance_model(m: &Mat3) -> [f32; 8] {
    [m[0][0], m[0][1], m[1][0], m[1][1], m[2][0], m[2][1], 0., 0.]
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
    /// Live hit-effect bursts (see `fx.rs`).
    hit_fx: Vec<fx::HitFx>,
    /// Text overlay state (see `text.rs`).
    text: text::TextState,
}

impl Renderer {
    pub fn device(&self) -> &wgpu::Device { &self.device }
    pub fn queue(&self) -> &wgpu::Queue { &self.queue }
    pub fn tex_bgl(&self) -> &wgpu::BindGroupLayout { &self.tex_bgl }
    pub fn sampler(&self) -> &wgpu::Sampler { &self.sampler }

    /// Acquire the next surface texture. Returns `Err` variants that the caller
    /// should handle (Timeout/Occluded → skip frame, Validation → propagate).
    pub fn surface_acquire(&mut self) -> Result<wgpu::SurfaceTexture, wgpu::CurrentSurfaceTexture> {
        let surface = self.surface.as_ref().expect("surface_acquire on a surfaceless renderer");
        match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(st) | wgpu::CurrentSurfaceTexture::Suboptimal(st) => Ok(st),
            other => Err(other),
        }
    }

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
            hit_fx: Vec::new(),
            text: text::TextState::new(),
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

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return; // minimized; keep last valid config
        }
        self.size = [width, height];
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
        self.draw_to_view(&view, frame, window_aspect, dim, None);
        self.queue.present(st);
        Ok(())
    }

    /// Build this frame's draw list and execute the render pass into `view`
    /// (window surface view or offscreen target). Submits on the internal
    /// queue; the caller presents (window path) or copies out (preview path).
    /// `dim`: global brightness multiplied into every quad's rgb (alpha kept).
    /// If `ui_overlay` is `Some`, a fullscreen textured quad is drawn last
    /// (useful for compositing a CPU-rendered Iced/tiny-skia overlay).
    pub(crate) fn draw_to_view(
        &mut self,
        view: &wgpu::TextureView,
        frame: &FrameState,
        window_aspect: f32,
        dim: f32,
        ui_overlay: Option<&wgpu::BindGroup>,
    ) {
        // RPE canvas model: world x ±1 = canvas ±675px, world y ±1 = ±450px
        // (1350×900, SQUARE canvas pixels). Letterbox the 3:2 canvas into the
        // window; canvas px stay uniform on every window, so rotated quads
        // never shear and sprites never distort.
        let aspect = self.playfield_aspect;
        let (kx, ky) = if window_aspect >= aspect {
            (aspect / window_aspect, 1.0)
        } else {
            (1.0, window_aspect / aspect)
        };
        // Square canvas px on screen: canvas x ±675 spans the playfield width;
        // visible canvas height = 1350/aspect px (y range ±675/aspect).
        let letterbox = mat_scale(kx / CANVAS_W, ky * aspect / CANVAS_W);

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
                [(1.0 - w) * 0.5, 0.0, w, 1.0]
            } else {
                let h = r / aspect;
                [0.0, (1.0 - h) * 0.5, 1.0, h]
            };
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

        let mut lines: Vec<&crate::core::LineState> = frame.lines.iter().collect();
        lines.sort_by_key(|l| l.z_order);

        for line in lines {
            // translate → rotate → scale
            let line_m = mat_mul(
                &letterbox,
                &mat_mul(
                    &mat_translate(line.position[0] * CANVAS_W, line.position[1] * CANVAS_H),
                    &mat_mul(
                        &mat_rotate(line.rotation), // radians (confirmed w/ CoreEval)
                        &mat_scale(line.scale[0], line.scale[1]),
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
                            color: [c[0], c[1], c[2], c[3] * line.alpha],
                            uv_rect: [0., 0., 1., 1.],
                        },
                        tex: &self.white,
                    });
                }
                Some(name) => {
                    if let Some(t) = self.textures.get(name) {
                        // prpr: natural pixel size × object scale.
                        let c = line.color;
                        cmds.push(DrawCmd {
                            uniform: DrawUniform {
                                model: mat_mul(&line_m, &mat_scale(t.size[0], t.size[1])),
                                color: [c[0], c[1], c[2], c[3] * line.alpha],
                                uv_rect: [0., 0., 1., 1.],
                            },
                            tex: &t.bind_group,
                        });
                    }
                }
            }

            // Notes follow the line's translation AND rotation, but NOT its
            // scale or alpha (locked product decision).
            let note_m = mat_mul(
                &letterbox,
                &mat_mul(
                    &mat_translate(line.position[0] * CANVAS_W, line.position[1] * CANVAS_H),
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
                    if note.fake {
                        alpha *= 0.5;
                    }
                    if alpha <= 0.0 {
                        continue;
                    }
                    // prpr mirrors below notes under a scale(1, -1); CoreEval
                    // already carries that mirror in `relative[1]` / `hold_end_y`
                    // (core/chart.rs), so use the values as-is.
                    let x = note.relative[0] * CANVAS_W;
                    let y = note.relative[1] * CANVAS_H;

                    match (note.kind, sprite) {
                        (1 | 3 | 4, Some(t)) => {
                            let w = NOTE_SPRITE_W * note.scale * mh_factor;
                            let h = w * t.size[1] / t.size[0];
                            cmds.push(DrawCmd {
                                uniform: DrawUniform {
                                    model: mat_mul(
                                        &note_m,
                                        &mat_mul(&mat_translate(x, y), &mat_scale(w, h)),
                                    ),
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
                            let w = NOTE_SPRITE_W * note.scale * mh_factor;
                            let (tail_px, head_px) = if is_mh { (50.0, 95.0) } else { (HOLD_CAP_PX, HOLD_CAP_PX) };
                            let head_uv = head_px / t.size[1]; // v0-fraction for head
                            let tail_uv = tail_px / t.size[1];
                            let head_h = w * head_px / t.size[0]; // quad heights
                            let tail_h = w * tail_px / t.size[0];
                            let tint = [1.0, 1.0, 1.0, alpha];
                            // Below-notes are position-mirrored in core; mirror the
                            // TEXTURE too (negative v scale) or head/tail swap and
                            // the body gradient runs the wrong way.
                            let v_at = |v0: f32, len: f32| -> [f32; 4] {
                                if note.above { [0., v0, 1., len] } else { [0., v0 + len, 1., -len] }
                            };
                            if let Some(end_y) = note.hold_end_y {
                                let y1 = end_y as f32 * CANVAS_H;
                                let h = (y1 - y).abs();
                                if h > 1e-5 {
                                    cmds.push(DrawCmd {
                                        uniform: DrawUniform {
                                            model: mat_mul(
                                                &note_m,
                                                &mat_mul(
                                                    &mat_translate(x, (y + y1) * 0.5),
                                                    &mat_scale(w, h),
                                                ),
                                            ),
                                            color: tint,
                                            // v ∈ [head/H, 1-tail/H]
                                            uv_rect: v_at(head_uv, 1. - head_uv - tail_uv),
                                        },
                                        tex: &t.bind_group,
                                    });
                                }
                                // head at y (near line), tail at y1 (far end).
                                for (cy, v0, len, quad_h) in [
                                    (y, 0.0, head_uv, head_h),
                                    (y1, 1.0 - tail_uv, tail_uv, tail_h),
                                ] {
                                    cmds.push(DrawCmd {
                                        uniform: DrawUniform {
                                            model: mat_mul(
                                                &note_m,
                                                &mat_mul(
                                                    &mat_translate(x, cy),
                                                    &mat_scale(w, quad_h),
                                                ),
                                            ),
                                            color: tint,
                                            uv_rect: v_at(v0, len),
                                        },
                                        tex: &t.bind_group,
                                    });
                                }
                            } else {
                                // No end (shouldn't happen for RPE holds): head only.
                                cmds.push(DrawCmd {
                                    uniform: DrawUniform {
                                        model: mat_mul(
                                            &note_m,
                                            &mat_mul(&mat_translate(x, y), &mat_scale(w, head_h)),
                                        ),
                                        color: tint,
                                        uv_rect: v_at(0., head_uv),
                                    },
                                    tex: &t.bind_group,
                                });
                            }
                        }
                        // Colored-quad fallback (no sprite loaded).
                        _ => {
                            if note.kind == 2 {
                                if let Some(end_y) = note.hold_end_y {
                                    let y1 = end_y as f32 * CANVAS_H;
                                    let h = (y1 - y).abs();
                                    if h > 1e-5 {
                                        cmds.push(DrawCmd {
                                            uniform: DrawUniform {
                                                model: mat_mul(
                                                    &note_m,
                                                    &mat_mul(
                                                        &mat_translate(x, (y + y1) * 0.5),
                                                        &mat_scale(HOLD_BODY_W * note.scale, h),
                                                    ),
                                                ),
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
                                    model: mat_mul(
                                        &note_m,
                                        &mat_mul(
                                            &mat_translate(x, y),
                                            &mat_scale(NOTE_W * note.scale, NOTE_H * note.scale),
                                        ),
                                    ),
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

        // Hit effects: sprite-sheet bursts in canvas-pixel space, after notes.
        // Top progress bar (Phigros-style): thin white strip hugging the
        // visible canvas top edge, grows left → right with `progress`.
        if self.progress > 0.0 {
            let top = CANVAS_W / aspect; // visible canvas y at the top (+y = up)
            let bar_h = 5.0;
            let bar_w = 1350.0 * self.progress;
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
        // UI overlay: just the last DrawCmd in the main pass (fullscreen NDC
        // quad). uv flipped — CPU-rendered pixmaps are top-down row-major,
        // our texture convention is v0 = bottom.
        if let Some(bg) = ui_overlay {
            cmds.push(DrawCmd {
                uniform: DrawUniform {
                    model: mat_scale(2.0, 2.0),
                    color: [1.0, 1.0, 1.0, 1.0],
                    uv_rect: [0., 1., 1., -1.],
                },
                tex: bg,
            });
        }

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
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
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
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.instance_bufs[self.frame_idx].slice(..));
            // Painter's algorithm: draw order is cmd order. Batch maximal
            // runs sharing one texture bind group (pointer identity).
            let mut start = 0usize;
            for i in 1..=cmds.len() {
                if i == cmds.len() || !std::ptr::eq(cmds[i].tex, cmds[start].tex) {
                    pass.set_bind_group(0, cmds[start].tex, &[]);
                    pass.draw(0..4, start as u32..i as u32);
                    start = i;
                }
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
