//! Post-processing pipeline: ping-pong render targets + per-effect GPU passes.
//! Each effect reads the previous pass's output and writes a new texture.
//! After all effects, the final result is composited into the scene.

use crate::render::shaders::{EffectDef, EFFECTS};
use std::collections::{HashMap, HashSet};

/// 判断源是否为 GLSL(而非 WGSL):版本指令 / GLSL 内置变量。
fn is_glsl_source(body: &str) -> bool {
    let t = body.trim_start();
    t.starts_with("#version")
        || body.contains("gl_FragColor")
        || body.contains("gl_Position")
        || body.contains("gl_FragCoord")
        || body.contains("attribute ")
        || body.contains("varying ")
}

/// std140 对齐/大小(GLSL uniform block 布局):(align, size)。
fn std140_layout(ty: &str) -> (u32, u32) {
    match ty.trim() {
        "float" | "int" | "uint" | "bool" => (4, 4),
        "vec2" | "ivec2" | "uvec2" | "bvec2" => (8, 8),
        "vec3" | "ivec3" | "uvec3" | "bvec3" => (16, 16),
        "vec4" | "ivec4" | "uvec4" | "bvec4" => (16, 16),
        "mat2" => (16, 32),
        "mat3" => (16, 48),
        "mat4" => (16, 64),
        _ => (4, 4),
    }
}

/// GLSL → glslang 预转换(ES100 → GLSL 450 + separate sampler + 绑定注入):
/// - `#version 100` → `#version 450`
/// - `precision ...;` 行删除(desktop 无 precision)
/// - `varying` → `in`、`attribute` → `in`(注入 location)
/// - `uniform sampler2D NAME;` → 分离声明
///   `layout(binding=0) uniform texture2D NAME;` + `layout(binding=1)
///   uniform sampler NAME_smp;`(glslang 生成分离 SPIR-V,wgpu 要求分离)
/// - 调用 `texture2D(NAME, ...)` → `texture(sampler2D(NAME, NAME_smp), ...)`
/// - 其余 uniform 变量合并进 `layout(binding=2) uniform Params { ... };`
///   (Vulkan 禁止 block 外非 opaque uniform),按 std140 计算 offsets
/// - `gl_FragColor` → 注入的 `fragColor_out`(450 已移除内置,补 out 声明)
/// 返回 (转换后的源, uniform buffer 数(0 或 1), 变量 std140 (offset, size))。
fn glsl_for_glslang(src: &str) -> (String, usize, Vec<(u32, u32)>) {
    let re_tex_call = regex::Regex::new(r"texture2D\(\s*([A-Za-z_]\w*)\s*,").unwrap();
    let re_sampler = regex::Regex::new(
        r"uniform\s+(sampler2D|sampler3D|samplerCube|sampler2DArray|samplerCubeArray)\s+(\w+)\s*;",
    )
    .unwrap();
    let re_uniform_var =
        regex::Regex::new(r"uniform\s+(float|int|uint|bool|vec[234]|ivec[234]|uvec[234]|bvec[234]|mat[234])\s+(\w+)\s*;").unwrap();
    // 第一遍:收集非 sampler uniform 变量(声明顺序 = uniform_values 顺序)。
    let mut vars: Vec<(String, String)> = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        if let Some(caps) = re_uniform_var.captures(t) {
            vars.push((caps[2].to_string(), caps[1].to_string()));
        }
    }
    // 第二遍:生成。
    let mut out = String::with_capacity(src.len() + 256);
    let mut has_version = false;
    let mut next_in_location = 0u32;
    let mut var_idx = 0usize;
    for line in src.lines() {
        let t = line.trim_start();
        if t.starts_with("#version") {
            if !has_version {
                out.push_str("#version 450\n");
                has_version = true;
            }
            continue;
        }
        if t.starts_with("precision ") {
            continue;
        }
        if re_uniform_var.is_match(t) {
            // uniform 变量由 block 统一声明,原行删除。
            var_idx += 1;
            continue;
        }
        let mut l = line
            .replace("varying", "in")
            .replace("attribute", "in")
            .replace("gl_FragColor", "fragColor_out");
        if t.starts_with("uniform ") {
            if let Some(caps) = re_sampler.captures(t) {
                let (ty, tex_name) = (caps[1].to_string(), caps[2].to_string());
                // sampler2D → texture2D(separate image 类型,GLSL 400+)。
                let tex_ty = ty.replacen("sampler", "texture", 1);
                out.push_str(&format!("layout(binding = 0) uniform {tex_ty} {tex_name};\n"));
                out.push_str(&format!("layout(binding = 1) uniform sampler {tex_name}_smp;\n"));
                continue;
            }
            continue; // 其他 uniform 变量:原行删除(block 统一声明)
        } else if l.trim_start().starts_with("in ") && !l.contains("layout(") {
            // SPIR-V 要求输入/输出带 location。判断用替换后的 l
            // (varying → in 之后)。
            l = l.replacen("in", &format!("layout(location = {next_in_location}) in"), 1);
            next_in_location += 1;
        } else {
            // 调用行:texture2D(NAME, ...) → texture(sampler2D(NAME, NAME_smp), ...)
            l = re_tex_call
                .replace_all(&l, "texture(sampler2D($1, ${1}_smp),")
                .into_owned();
        }
        out.push_str(&l);
        out.push('\n');
    }
    if !has_version {
        out.insert_str(0, "#version 450\n");
    }
    if src.contains("gl_FragColor") {
        let ver = "#version 450\n";
        out.insert_str(ver.len(), "layout(location = 0) out vec4 fragColor_out;\n");
    }
    // uniform block(std140):Vulkan 禁止 block 外非 opaque uniform。
    let mut layout: Vec<(u32, u32)> = Vec::with_capacity(vars.len());
    let mut block = String::new();
    let mut cursor = 0u32;
    for (name, ty) in &vars {
        let (align, size) = std140_layout(ty);
        cursor = (cursor + align - 1) & !(align - 1);
        layout.push((cursor, size));
        block.push_str(&format!("    {ty} {name};\n"));
        cursor += size;
    }
    let uniform_count = usize::from(!vars.is_empty());
    if !vars.is_empty() {
        // 插到 #version 之后(变量须在使用前声明——main 之前的声明区)。
        let block_src = format!("layout(binding = 2) uniform Params {{\n{block}}};\n");
        let ver = "#version 450\n";
        out.insert_str(ver.len(), &block_src);
    }
    (out, uniform_count, layout)
}

/// A single active effect instance (from extra.json).
#[derive(Clone)]
pub struct ActiveEffect {
    /// Index into the built-in [`EFFECTS`] array.
    /// `usize::MAX` signals a custom shader loaded from the chart directory.
    pub shader_idx: usize,
    /// Custom shader filename used when `shader_idx` is `usize::MAX`.
    pub custom_name: Option<String>,
    /// Execution order within the effect chain (lower = earlier).
    #[allow(dead_code)] // 排序由 effects_chain 保证,字段保留供调试
    pub priority: u32,
    /// Float values written into the effect's uniform buffer each frame.
    pub uniform_values: Vec<f32>,
    /// How many leading elements of `uniform_values` are meaningful.
    pub uniform_count: usize,
}

/// Ping-pong pair + per-effect GPU pipelines.
pub struct PostPipe {
    /// Ping-pong colour targets (alternate read/write for multi-pass effects).
    pub(crate) targets: [Option<wgpu::Texture>; 2],
    /// Texture views for the ping-pong targets.
    pub(crate) target_views: [Option<wgpu::TextureView>; 2],
    /// Index of the target holding the last effect output (set by apply()).
    last_output: usize,

    /// Half-resolution ping-pong targets for bandwidth-heavy effects
    /// (blur/glitch-style: ~75% fewer pixels per pass).
    half_targets: [Option<wgpu::Texture>; 2],
    half_views: [Option<wgpu::TextureView>; 2],

    /// Bind group layout for screen texture (shared by all effects, needed by Renderer too).
    pub screen_bgl: wgpu::BindGroupLayout,
    /// Sampler shared by all effects.
    pub sampler: wgpu::Sampler,
    /// Linear/linear sampler for down/upscaling to/from half-resolution
    /// (the shared sampler has min=Nearest, which would shimmer when
    /// downsample-filtering the source into a half-res pass).
    half_sampler: wgpu::Sampler,

    /// Per-effect pipelines (lazily created).
    pipelines: HashMap<String, EffPipe>,
    /// Chart directory path for loading custom shader files.
    pub chart_dir: Option<std::path::PathBuf>,
    /// Blit pipeline: simple passthrough to copy final result to surface.
    pub blit_pipeline: Option<wgpu::RenderPipeline>,
    /// Cached blit bind groups keyed by (SrcTag, use_half_sampler).
    blit_bgs: HashMap<(SrcTag, bool), wgpu::BindGroup>,

    /// Active effects for the current frame.
    pub active: Vec<ActiveEffect>,

    /// 半分辨率特效降采样开关(设置里可关)。关闭时所有特效全分辨率跑,
    /// 用于排查特效质量问题(如尺寸参数型特效在 half 下变味)。
    pub half_res_enabled: bool,

    /// 预热队列:启动/切谱时预编译内置特效 pipeline,消除"特效首次出现
    /// 时编译卡顿"(ensure_effect 是惰性的,第一次用某个特效会卡一帧)。
    warmup_pending: Vec<String>,
    /// 自定义 shader 加载失败的缓存:失败不重试(否则每帧读磁盘 + 刷屏)。
    /// `warmup_custom` 重新扫描时会覆盖(文件修复后可恢复)。
    failed_custom: HashSet<String>,

    /// Viewport width in pixels.
    pub width: u32,
    /// Viewport height in pixels.
    pub height: u32,
    /// Colour format of all render targets.
    pub tex_format: wgpu::TextureFormat,
}

/// 屏幕采样源标签:bind group 缓存的稳定 key。
/// 不用纹理指针(栈地址会碰撞),用链路中的固定角色。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum SrcTag {
    /// 输入场景(scene 纹理或 surface 上游)。
    Scene,
    /// 全分辨率 ping-pong target。
    Full(u8),
    /// 半分辨率 ping-pong target。
    Half(u8),
}

struct EffPipe {
    pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,     // group 1 (uniforms); GLSL 路径:单 group 三绑定
    /// Uniform buffers:WGSL 路径 1 个;GLSL 路径每个 uniform 变量一个。
    uniform_bufs: Vec<wgpu::Buffer>,
    uniform_size: u64,
    /// Cached uniform bind group (buffer is reused; content rewritten per frame).
    uniform_bg: Option<wgpu::BindGroup>,
    /// Cached screen bind groups keyed by (SrcTag, use_half_sampler).
    /// `SrcTag` is a stable role (Scene/Full0/Full1/Half0/Half1) — unlike a
    /// texture pointer, it never collides when the same local variable
    /// carries different textures within one frame.
    screen_bgs: HashMap<(SrcTag, bool), wgpu::BindGroup>,
    /// GLSL 源(自定义 GLSL 特效):单 bind group(tex+sampler+uniforms),
    /// 与 WGSL 路径的 bind group 构建不同。
    glsl: bool,
    /// GLSL 路径:uniform block 内各变量的 std140 (offset, size)。
    uniform_layout: Vec<(u32, u32)>,
}

impl PostPipe {
    /// Create a new post-processing pipeline. Builds the blit pipeline, sampler,
    /// screen bind-group layout, and initial ping-pong targets at the given size.
    pub fn new(device: &wgpu::Device, width: u32, height: u32, tex_fmt: wgpu::TextureFormat) -> Self {
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("post-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let half_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("post-half-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        // Bind group layout for group 0: texture + sampler
        let screen_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post-screen-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
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
        // Blit pipeline: simple texture pass-through
        let blit_frag = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
@fragment fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    return textureSample(screen_tex, screen_sampler, uv);
}";
        let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Owned(
                String::from(crate::render::shaders::VERT) + blit_frag
            )),
        });
        let blit_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit-pl"),
            bind_group_layouts: &[Some(&screen_bgl)],
            ..Default::default()
        });
        let blit_pipeline = Some(device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit-pipe"),
            layout: Some(&blit_pl),
            vertex: wgpu::VertexState {
                module: &blit_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &blit_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: tex_fmt,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        }));
        let mut pipe = PostPipe {
            targets: [None, None],
            target_views: [None, None],
            last_output: 0,
            half_targets: [None, None],
            half_views: [None, None],
            screen_bgl,
            sampler,
            half_sampler,
            blit_pipeline,
            blit_bgs: HashMap::new(),
            pipelines: HashMap::new(),
            chart_dir: None,
            active: Vec::new(),
            half_res_enabled: true,
            warmup_pending: Vec::new(),
            failed_custom: HashSet::new(),
            width: 0, height: 0, tex_format: tex_fmt,
        };
        pipe.resize(device, width, height);
        pipe
    }

    /// Recreate ping-pong targets at a new viewport size.
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        // Old texture views are dropped — cached bind groups referencing
        // them must go too, otherwise the old targets can never be freed.
        self.blit_bgs.clear();
        for ep in self.pipelines.values_mut() {
            ep.screen_bgs.clear();
        }
        for (i, (t, v)) in self.targets.iter_mut().zip(self.target_views.iter_mut()).enumerate() {
            *t = Some(Self::make_target(device, self.width, self.height, &format!("post-{i}"), self.tex_format));
            *v = Some(t.as_ref().unwrap().create_view(&wgpu::TextureViewDescriptor::default()));
        }
        let hw = (self.width / 2).max(1);
        let hh = (self.height / 2).max(1);
        for (i, (t, v)) in self.half_targets.iter_mut().zip(self.half_views.iter_mut()).enumerate() {
            *t = Some(Self::make_target(device, hw, hh, &format!("post-half-{i}"), self.tex_format));
            *v = Some(t.as_ref().unwrap().create_view(&wgpu::TextureViewDescriptor::default()));
        }
    }

    /// Create a renderable + readable texture (also used by Renderer for the scene target).
    pub fn make_target2(device: &wgpu::Device, w: u32, h: u32, label: &str, tex_fmt: wgpu::TextureFormat) -> wgpu::Texture {
        Self::make_target(device, w, h, label, tex_fmt)
    }

    fn make_target(device: &wgpu::Device, w: u32, h: u32, label: &str, tex_fmt: wgpu::TextureFormat) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: tex_fmt,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    /// Shared pipeline construction for built-in and custom effects (the two
    /// paths used to duplicate ~70 lines). `body` is the fragment shader;
    /// WGSL 路径:共享 vertex 前置拼接;GLSL 路径(naga 转译)用 GLSL
    /// vertex 模板 + 剥离 #version 的用户 fragment。
    ///
    /// 失败(编译/校验错误)返回 `None` 并报错——wgpu 默认把校验错误当
    /// fatal panic,这里用 error scope 捕获后优雅跳过。
    fn build_eff_pipe(&self, device: &wgpu::Device, name: &str, body: &str) -> Option<EffPipe> {
        let is_glsl = is_glsl_source(body);
        if is_glsl {
            return self.build_eff_pipe_glsl(device, name, body);
        }
        let shader_src = String::from(crate::render::shaders::VERT) + body;
        // 编译/校验失败优雅降级:error scope 捕获,不 panic。
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(&format!("shader-{name}")),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Owned(shader_src)),
        });

        // Group 1: uniform buffer — generous 256 bytes covers any effect struct
        let uniform_size: u64 = 256;
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(&format!("bgl-{name}")),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: Some(wgpu::BufferSize::new(uniform_size).unwrap()),
                },
                count: None,
            }],
        });

        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(&format!("pl-{name}")),
            bind_group_layouts: &[Some(&self.screen_bgl), Some(&bgl)],
            ..Default::default()
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(&format!("pipe-{name}")),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: self.tex_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(&format!("ubuf-{name}")),
            size: uniform_size,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // 收 scope:shader/pipeline 创建期若有校验错误,报错跳过。
        let err = pollster::block_on(scope.pop());
        if err.is_some() {
            eprintln!("warning: custom effect {name}: shader 编译/校验失败,已跳过: {err:?}");
            return None;
        }

        Some(EffPipe { pipeline, bgl, uniform_bufs: vec![uniform_buf], uniform_size, uniform_bg: None, screen_bgs: HashMap::new(), glsl: false, uniform_layout: Vec::new() })
    }

    /// GLSL 自定义特效路径:glslang 编译 GLSL → SPIR-V(完整 GLSL 支持,
    /// 含 sampler 采样;naga glsl-in 的 texture 内建残缺不可用)。绑定:
    ///   group 0: binding0 = screen 纹理, binding1 = sampler,
    ///            binding 2.. = 每个 uniform 变量一个 buffer(256B)
    fn build_eff_pipe_glsl(&self, device: &wgpu::Device, name: &str, body: &str) -> Option<EffPipe> {
        let (frag, uniform_count, uniform_layout) = glsl_for_glslang(body);
        // glslang 纯 CPU 编译(完整 GLSL 支持,含 sampler 采样)。
        let compiler = glslang::Compiler::acquire()?;
        let compile = |src: String, stage: glslang::ShaderStage| -> Option<Vec<u32>> {
            let source = glslang::ShaderSource::from(src);
            let input = glslang::ShaderInput::new(
                &source,
                stage,
                &glslang::CompilerOptions::default(),
                None::<&[(&str, Option<&str>)]>,
                None,
            )
            .ok()?;
            let shader = compiler.create_shader(input).ok()?;
            shader.compile().ok()
        };
        let vs_spv = compile(crate::render::shaders::GLSL_VERT.to_string(), glslang::ShaderStage::Vertex)
            .unwrap_or_else(|| {
                eprintln!("warning: custom effect {name}: GLSL vertex 编译失败,已跳过");
                return Vec::new();
            });
        if vs_spv.is_empty() {
            return None;
        }
        let fs_spv = match compile(frag, glslang::ShaderStage::Fragment) {
            Some(v) => v,
            None => {
                eprintln!("warning: custom effect {name}: GLSL 编译失败,已跳过");
                return None;
            }
        };
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let vs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(&format!("vs-{name}")),
            source: wgpu::ShaderSource::SpirV(std::borrow::Cow::Owned(vs_spv)),
        });
        let fs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(&format!("fs-{name}")),
            source: wgpu::ShaderSource::SpirV(std::borrow::Cow::Owned(fs_spv)),
        });

        let uniform_size: u64 = 256;
        let mut bgl_entries = vec![
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
        ];
        for i in 0..uniform_count {
            bgl_entries.push(wgpu::BindGroupLayoutEntry {
                binding: 2 + i as u32,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: Some(wgpu::BufferSize::new(uniform_size).unwrap()),
                },
                count: None,
            });
        }
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(&format!("bgl-{name}")),
            entries: &bgl_entries,
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(&format!("pl-{name}")),
            bind_group_layouts: &[Some(&bgl)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(&format!("pipe-{name}")),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &vs,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &fs,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: self.tex_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let mut uniform_bufs = Vec::with_capacity(uniform_count);
        for i in 0..uniform_count {
            uniform_bufs.push(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(&format!("ubuf-{name}-{i}")),
                size: uniform_size,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let err = pollster::block_on(scope.pop());
        if err.is_some() {
            eprintln!("warning: custom effect {name}: GLSL 编译/校验失败,已跳过: {err:?}");
            return None;
        }
        Some(EffPipe { pipeline, bgl, uniform_bufs, uniform_size, uniform_bg: None, screen_bgs: HashMap::new(), glsl: true, uniform_layout })
    }

    /// Ensure the pipeline + resources exist for a given effect.
    pub fn ensure_effect(&mut self, device: &wgpu::Device, def: &EffectDef) {
        if self.pipelines.contains_key(def.name) { return; }
        if let Some(pipe) = self.build_eff_pipe(device, def.name, def.frag) {
            self.pipelines.insert(def.name.to_string(), pipe);
        }
    }

    /// Queue ALL built-in effect pipelines for pre-compilation.
    /// Call during startup / chart-switch (loading screen covers the cost);
    /// afterwards `tick_warmup` compiles them in batches.
    pub fn start_warmup(&mut self) {
        if self.warmup_pending.is_empty() {
            self.warmup_pending = EFFECTS.iter().map(|d| d.name.to_string()).collect();
        }
    }

    /// Compile up to `budget` pending pipelines. Returns `true` when done.
    /// A pipeline compile is a blocking GPU call (~tens of ms each) — call
    /// this only in contexts where a stall is acceptable (startup, loading
    /// screen), never in the steady-state frame.
    pub fn tick_warmup(&mut self, device: &wgpu::Device, budget: usize) -> bool {
        for _ in 0..budget {
            let Some(name) = self.warmup_pending.pop() else { return true };
            if let Some(def) = EFFECTS.iter().find(|d| d.name == name.as_str()) {
                self.ensure_effect(device, def);
            }
        }
        self.warmup_pending.is_empty()
    }

    /// Load and compile a custom WGSL shader from the chart directory.
    /// Failures are reported (stderr) instead of silently dropping the effect.
    /// Failed names are cached: retried only by a later `warmup_custom` scan
    /// (otherwise every frame re-reads the disk and re-prints the warning).
    fn ensure_custom_effect(&mut self, device: &wgpu::Device, name: String) {
        if self.pipelines.contains_key(&name) || self.failed_custom.contains(&name) {
            return;
        }
        let Some(ref chart_dir) = self.chart_dir else { return };
        // extra.json 的 shader 名常带前导 '/'——PathBuf::join 遇绝对路径
        // 会丢弃 chart_dir,直接拼成 D:/xxx.glsl。去前导分隔符再拼。
        let rel = name.trim_start_matches(['/', '\\']);
        let path = chart_dir.join(rel);
        let wgsl = match std::fs::read_to_string(&path) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("warning: custom effect {name}: cannot read {}: {e}", path.display());
                self.failed_custom.insert(name);
                return;
            }
        };
        let pipe = self.build_eff_pipe(device, &name, &wgsl);
        match pipe {
            Some(pipe) => { self.pipelines.insert(name, pipe); }
            None => { self.failed_custom.insert(name); }
        }
    }

    /// 预热当前谱目录下的全部自定义 WGSL shader(切谱加载完成时调用,
    /// 一次性编译——运行中首次使用某个自定义特效不再阻塞卡帧)。
    pub fn warmup_custom(&mut self, device: &wgpu::Device) {
        let Some(ref chart_dir) = self.chart_dir else { return };
        let Ok(entries) = std::fs::read_dir(chart_dir) else { return };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "wgsl") {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if self.pipelines.contains_key(&name) {
                    continue;
                }
                let wgsl = match std::fs::read_to_string(&p) {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("warning: custom effect {name}: cannot read {}: {e}", p.display());
                        self.failed_custom.insert(name);
                        continue;
                    }
                };
                let pipe = self.build_eff_pipe(device, &name, &wgsl);
                match pipe {
                    Some(pipe) => {
                        self.pipelines.insert(name.clone(), pipe);
                        self.failed_custom.remove(&name);
                    }
                    None => { self.failed_custom.insert(name); }
                }
            }
        }
    }

    /// Run all active effects. Reads from `src`, applies each in order.
    /// After this call, `last_view()` returns the output texture view.
    pub fn apply(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        src: &wgpu::TextureView,
    ) {
        let _s = crate::trace_span!("post_apply");
        if self.active.is_empty() { return; }

        // 优化(PMCORE-53/54):裁剪无效 pass。
        // 零强度特效(主参数 = 0 时输出与输入相同)直接跳过。
        // 注意:不裁剪"重复特效"——两个参数相同的特效是用户刻意叠加的
        // (double glitch / double noise),去重会让第二个静默不渲染。
        let mut active: Vec<&ActiveEffect> = Vec::with_capacity(self.active.len());
        for ae in &self.active {
            if is_effect_noop(ae) {
                continue;
            }
            active.push(ae);
        }
        if active.is_empty() {
            // 全部裁剪:输出 = 输入。做一次 src→target blit 保持
            // last_view() 语义(调用方随后 blit 到 surface)。
            let out = self.target_views[0].as_ref().unwrap().clone();
            self.blit(encoder, device, src, &out, SrcTag::Scene);
            self.last_output = 0;
            return;
        }

        // Phase 1: ensure all pipelines exist
        // Copy active descriptors: (pipeline_key, uniform_values, uniform_count)
        let descriptors: Vec<(String, Vec<f32>, usize)> = active.iter().map(|ae| {
            let key = if ae.shader_idx < usize::MAX {
                EFFECTS.get(ae.shader_idx).map(|d| d.name.to_string()).unwrap_or_default()
            } else {
                ae.custom_name.clone().unwrap_or_default()
            };
            (key, ae.uniform_values.clone(), ae.uniform_count)
        }).collect();
        for (key, _, _) in &descriptors {
            if let Some(def) = EFFECTS.iter().find(|d| d.name == key.as_str()) {
                self.ensure_effect(device, def);
            } else if !key.is_empty() {
                self.ensure_custom_effect(device, key.clone());
            }
        }

        // Phase 2: execute effects. `write_idx` toggles 0/1 between effects
        // (local only — no persistent state to get out of sync).
        //
        // 半分辨率段:带宽型特效(glitch/chromatic/blur 类)在 W/2×H/2 target
        // 上跑,省 ~75% 像素带宽;连续 half-res 特效只做一次降采样和一次
        // 升采样(段内直接 ping-pong 半分辨率 target)。
        // `read_view` 是 owned clone(wgpu::TextureView 内部 Arc,clone 廉价),
        // 避免跨迭代的借用链。
        let mut read_view: wgpu::TextureView = src.clone();
        let mut read_tag = SrcTag::Scene;
        let half_res = self.half_res_enabled;
        let mut write_idx = 0usize;
        let mut i = 0usize;
        while i < descriptors.len() {
            if !(half_res && effect_is_half_res(&descriptors[i].0)) {
                let (key, uv, _) = &descriptors[i];
                let write_view = self.target_views[write_idx].as_ref().unwrap().clone();
                write_idx = 1 - write_idx;
                if self.run_effect(encoder, device, queue, key, uv, &read_view, &write_view, read_tag) {
                    read_view = write_view;
                    read_tag = SrcTag::Full((1 - write_idx) as u8);
                } else {
                    write_idx = 1 - write_idx; // roll back; keep read_view
                }
                i += 1;
            } else {
                let mut j = i + 1;
                while j < descriptors.len() && half_res && effect_is_half_res(&descriptors[j].0) {
                    j += 1;
                }
                // Downscale: current full-res output → half-res ping-pong[0].
                let h0 = self.half_views[0].as_ref().unwrap().clone();
                self.blit(encoder, device, &read_view, &h0, read_tag);
                let mut h_read: wgpu::TextureView = h0;
                let mut h_read_tag = SrcTag::Half(0);
                let mut h_write = 1usize;
                for (key, uv, _) in descriptors.iter().take(j).skip(i) {
                    let wv = self.half_views[h_write].as_ref().unwrap().clone();
                    h_write = 1 - h_write;
                    if self.run_effect(encoder, device, queue, key, uv, &h_read, &wv, h_read_tag) {
                        h_read = wv;
                        h_read_tag = SrcTag::Half((1 - h_write) as u8);
                    } else {
                        h_write = 1 - h_write; // roll back; keep h_read
                    }
                }
                // Upscale: last half-res output → full-res ping-pong slot.
                let fw = self.target_views[write_idx].as_ref().unwrap().clone();
                write_idx = 1 - write_idx;
                self.blit(encoder, device, &h_read, &fw, h_read_tag);
                read_view = fw;
                read_tag = SrcTag::Full((1 - write_idx) as u8);
                i = j;
            }
        }
        // After the loop, `write_idx` points at the slot that was NOT written
        // last (it toggled after the final write); the last output is the
        // other one.
        self.last_output = 1 - write_idx;
    }

    /// Run one effect pass: bind screen + uniform groups, draw the full-screen
    /// triangle into `write_view`, sampling from `read_view`.
    /// Returns `false` when the pipeline is missing (custom shader failed to
    /// load) — the caller must then keep `read_view`/indices untouched so the
    /// chain doesn't sample an unwritten target.
    fn run_effect(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: &str,
        uv: &[f32],
        read_view: &wgpu::TextureView,
        write_view: &wgpu::TextureView,
        src_tag: SrcTag,
    ) -> bool {
        let Some(ep) = self.pipelines.get_mut(key) else { return false };
        // Write uniform buffer (256 bytes, stack array — no per-frame alloc)
        let mut uniform_data = [0u8; 256];
        for (i, &val) in uv.iter().enumerate() {
            let offset = i * 4;
            if offset + 4 <= uniform_data.len() {
                uniform_data[offset..offset+4].copy_from_slice(&val.to_le_bytes());
            }
        }
        if ep.glsl {
            // GLSL 路径:uniform block 内按 std140 (offset, size) 写入。
            for (i, &(off, size)) in ep.uniform_layout.iter().enumerate() {
                let start = i * 4;
                let n = (size as usize).min(4).max(1);
                let mut v = [0u8; 16];
                if start + n <= uniform_data.len() {
                    v[..n].copy_from_slice(&uniform_data[start..start + n]);
                }
                queue.write_buffer(&ep.uniform_bufs[0], off as u64, &v[..n]);
            }
        } else {
            queue.write_buffer(&ep.uniform_bufs[0], 0, &uniform_data);
        }
        let use_half = matches!(src_tag, SrcTag::Half(_));
        let sampler = if use_half { &self.half_sampler } else { &self.sampler };
        // GLSL 路径:单 bind group(binding0 纹理 + binding1 sampler +
        // binding 2.. uniform);WGSL 路径:uniform 与 screen 分离缓存。
        let screen_bg = if ep.glsl {
            match ep.screen_bgs.get(&(src_tag, use_half)) {
                Some(bg) => bg.clone(),
                None => {
                    let mut entries = vec![
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(read_view) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
                    ];
                    for (i, buf) in ep.uniform_bufs.iter().enumerate() {
                        entries.push(wgpu::BindGroupEntry {
                            binding: 2 + i as u32,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: buf,
                                offset: 0,
                                size: wgpu::BufferSize::new(ep.uniform_size),
                            }),
                        });
                    }
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some(&format!("glsl-bg-{key}")),
                        layout: &ep.bgl,
                        entries: &entries,
                    });
                    ep.screen_bgs.insert((src_tag, use_half), bg.clone());
                    bg
                }
            }
        } else {
            // Reuse the uniform bind group — the buffer is stable, only its
            // contents change per frame.
            let uniform_bg = match &ep.uniform_bg {
                Some(bg) => bg,
                None => {
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some(&format!("ubg-{key}")),
                        layout: &ep.bgl,
                        entries: &[wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &ep.uniform_bufs[0],
                                offset: 0,
                                size: wgpu::BufferSize::new(ep.uniform_size),
                            }),
                        }],
                    });
                    ep.uniform_bg = Some(bg);
                    ep.uniform_bg.as_ref().unwrap()
                }
            };
            // Screen bind group: cached by (SrcTag, half-sampler). SrcTag is a
            // stable role (Scene/Full0/1/Half0/1), so the cache is safe — the
            // same stack local may carry different textures, but the TAG changes
            // with it. Half-res passes sample with the linear half_sampler to
            // avoid downscale shimmer.
            match ep.screen_bgs.get(&(src_tag, use_half)) {
                Some(bg) => bg.clone(),
                None => {
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some(&format!("screen-bg-{key}")),
                        layout: &self.screen_bgl,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(read_view) },
                            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
                        ],
                    });
                    ep.screen_bgs.insert((src_tag, use_half), bg.clone());
                    bg
                }
            }
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(&format!("effect-{key}")),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: write_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&ep.pipeline);
        pass.set_bind_group(0, &screen_bg, &[]);
        // WGSL 路径才有独立的 uniform bind group(group 1)。
        if !ep.glsl {
            let ubg = ep.uniform_bg.as_ref().unwrap();
            pass.set_bind_group(1, ubg, &[]);
        }
        pass.draw(0..3, 0..1);
        true
    }

    /// Copy `src` into `dst` with the passthrough blit pipeline. Used for the
    /// no-op shortcut and for down/upscaling around half-resolution effect
    /// runs. Bind groups are cached per (SrcTag, half-sampler) pair.
    fn blit(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        device: &wgpu::Device,
        src: &wgpu::TextureView,
        dst: &wgpu::TextureView,
        src_tag: SrcTag,
    ) {
        let Some(blit) = &self.blit_pipeline else { return };
        let use_half = matches!(src_tag, SrcTag::Half(_));
        let sampler = if use_half { &self.half_sampler } else { &self.sampler };
        let bg = match self.blit_bgs.get(&(src_tag, use_half)) {
            Some(bg) => bg.clone(),
            None => {
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("blit-bg"),
                    layout: &self.screen_bgl,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(src) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
                    ],
                });
                self.blit_bgs.insert((src_tag, use_half), bg.clone());
                bg
            }
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("effect-blit"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: dst,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        });
        pass.set_pipeline(blit);
        pass.set_bind_group(0, &bg, &[]);
        pass.draw(0..3, 0..1);
    }

    /// After `apply()`, returns the texture view containing the final result.
    pub fn last_view(&self) -> &wgpu::TextureView {
        self.target_views[self.last_output].as_ref().unwrap()
    }
}

/// 特效是否无效(输出与输入相同,可安全跳过)。
/// 各内置特效的主强度参数 = 0 时视为 no-op;自定义 shader 无法判定,保留。
fn is_effect_noop(ae: &ActiveEffect) -> bool {
    let v = ae.uniform_values.as_slice();
    if ae.shader_idx == usize::MAX {
        return false; // 自定义:无法判定,保守执行
    }
    let name = EFFECTS.get(ae.shader_idx).map(|d| d.name).unwrap_or("");
    match name {
        "grayscale" => v.first().is_some_and(|f| *f == 0.0),   // factor
        "chromatic" => v.first().is_some_and(|f| *f == 0.0),   // power
        "glitch" => v.first().is_some_and(|f| *f == 0.0),      // power
        "fisheye" => v.first().is_some_and(|f| *f == 0.0),     // power
        "noise" => v.get(1).is_some_and(|f| *f == 0.0),        // power(第二个)
        "radialBlur" => v.first().is_some_and(|f| *f == 0.0),  // power
        "pixel" => v.first().is_some_and(|f| *f == 0.0),       // size
        "circleBlur" => v.first().is_some_and(|f| *f == 0.0),  // size
        "vignette" => v.get(3).is_some_and(|f| *f == 0.0),     // color_a
        _ => false,
    }
}

/// 特效是否在 W/2×H/2 target 上执行(且开关 [`PostPipe::half_res_enabled`]
/// 打开时)。
///
/// 采样型/带宽型特效(glitch/chromatic/noise/模糊类)对分辨率不敏感,
/// 半分辨率省 ~75% 像素带宽;grayscale/vignette 纯色调/柔边也无感。
/// **尺寸参数型特效排除**:circleBlur/pixel 的 size 是绝对像素值
/// (`ps = 1/screen_size`),半分辨率下半径/块大小在屏幕上翻倍——视觉
/// 错误;fisheye 几何畸变同理防糊。自定义 shader 保守全分辨率。
fn effect_is_half_res(key: &str) -> bool {
    matches!(
        key,
        "glitch" | "chromatic" | "noise" | "radialBlur"
            | "grayscale" | "vignette"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(name: &str, vals: &[f32]) -> ActiveEffect {
        let si = EFFECTS.iter().position(|d| d.name == name).unwrap();
        ActiveEffect {
            shader_idx: si,
            custom_name: None,
            priority: 0,
            uniform_values: vals.to_vec(),
            uniform_count: vals.len(),
        }
    }

    #[test]
    fn noop_detection() {
        // grayscale factor=0 → no-op
        assert!(is_effect_noop(&fx("grayscale", &[0.0])));
        assert!(!is_effect_noop(&fx("grayscale", &[0.5])));
        // chromatic power=0 → no-op
        assert!(is_effect_noop(&fx("chromatic", &[0.0, 3.0, 0.0, 0.0])));
        // vignette color_a=0 → no-op
        assert!(is_effect_noop(&fx("vignette", &[0.0, 0.0, 0.0, 0.0, 0.25, 15.0])));
        assert!(!is_effect_noop(&fx("vignette", &[0.0, 0.0, 0.0, 1.0, 0.25, 15.0])));
        // 自定义 shader 不跳过
        let custom = ActiveEffect {
            shader_idx: usize::MAX,
            custom_name: Some("x.frag".into()),
            priority: 0,
            uniform_values: vec![0.0],
            uniform_count: 1,
        };
        assert!(!is_effect_noop(&custom));
    }

    #[test]
    fn glsl_es100_fragment_compiles_via_glslang() {
        // 与 build_eff_pipe_glsl 相同的路径:ES 100 源预转换(separate
        // sampler + 绑定注入)后 glslang 编译 SPIR-V。glslang 完整支持
        // GLSL(含 sampler 采样),纯 CPU 不依赖 GPU。
        let frag = "#version 100
precision mediump float;
varying vec2 v_uv;
void main() {
    vec3 c = vec3(v_uv.x, v_uv.y, 1.0 - v_uv.x);
    gl_FragColor = vec4(c, 1.0);
}
";
        let (converted, count, layout) = glsl_for_glslang(frag);
        assert_eq!(count, 0);
        assert!(converted.starts_with("#version 450"));
        assert!(converted.contains("in vec2 v_uv"));
        assert!(!converted.contains("precision"));
        assert!(converted.contains("layout(location = 0) out vec4 fragColor_out"));
        let compiler = glslang::Compiler::acquire().unwrap();
        let compile = |src: String, stage: glslang::ShaderStage| {
            let source = glslang::ShaderSource::from(src);
            let input = glslang::ShaderInput::new(
                &source,
                stage,
                &glslang::CompilerOptions::default(),
                None::<&[(&str, Option<&str>)]>,
                None,
            )
            .unwrap();
            compiler.create_shader(input).unwrap().compile().unwrap()
        };
        let _ = compile(converted, glslang::ShaderStage::Fragment);
        // 共享 vertex 模板同样可编译。
        let _ = compile(
            crate::render::shaders::GLSL_VERT.to_string(),
            glslang::ShaderStage::Vertex,
        );
        // 带纹理采样 + 多 uniform 的用户形态(RPE shader 常见结构)。
        let with_tex = "#version 100
precision mediump float;
uniform sampler2D u_tex;
uniform float time;
uniform float power;
varying vec2 v_uv;
void main() {
    vec4 color = texture2D(u_tex, v_uv);
    gl_FragColor = color + vec4(vec3(power), 0.0);
}
";
        let (converted, count, layout) = glsl_for_glslang(with_tex);
        assert_eq!(count, 1); // 单 uniform block
        assert_eq!(layout, vec![(0, 4), (4, 4)]); // std140:time@0, power@4
        assert!(converted.contains("layout(binding = 0) uniform texture2D u_tex"));
        assert!(converted.contains("layout(binding = 1) uniform sampler u_tex_smp"));
        assert!(converted.contains("layout(binding = 2) uniform Params"));
        assert!(converted.contains("    float time;"));
        assert!(converted.contains("    float power;"));
        assert!(converted.contains("texture(sampler2D(u_tex, u_tex_smp), v_uv)"));
        let _ = compile(converted, glslang::ShaderStage::Fragment);
    }
}

