//! Post-processing pipeline: ping-pong render targets + per-effect GPU passes.
//! Each effect reads the previous pass's output and writes a new texture.
//! After all effects, the final result is composited into the scene.

use crate::render::shaders::{EffectDef, EFFECTS};
use std::collections::HashMap;

/// A single active effect instance (from extra.json).
pub struct ActiveEffect {
    /// Index into the built-in [`EFFECTS`] array.
    /// `usize::MAX` signals a custom shader loaded from the chart directory.
    pub shader_idx: usize,
    /// Custom shader filename used when `shader_idx` is `usize::MAX`.
    pub custom_name: Option<String>,
    /// Execution order within the effect chain (lower = earlier).
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
    /// Index of the current write target (toggles 0/1 each pass).
    idx: usize,

    /// Bind group layout for screen texture (shared by all effects, needed by Renderer too).
    pub screen_bgl: wgpu::BindGroupLayout,
    /// Sampler shared by all effects.
    pub sampler: wgpu::Sampler,

    /// Per-effect pipelines (lazily created).
    pipelines: HashMap<String, EffPipe>,
    /// Chart directory path for loading custom shader files.
    pub chart_dir: Option<std::path::PathBuf>,
    /// Blit pipeline: simple passthrough to copy final result to surface.
    pub blit_pipeline: Option<wgpu::RenderPipeline>,
    /// Cached blit bind groups keyed by texture-view pointer.
    pub(crate) blit_bgs: HashMap<usize, wgpu::BindGroup>,

    /// Active effects for the current frame.
    pub active: Vec<ActiveEffect>,

    /// Viewport width in pixels.
    pub width: u32,
    /// Viewport height in pixels.
    pub height: u32,
    /// Colour format of all render targets.
    pub tex_format: wgpu::TextureFormat,
}

struct EffPipe {
    pipeline: wgpu::RenderPipeline,
    pl: wgpu::PipelineLayout,
    bgl: wgpu::BindGroupLayout,     // group 1 (uniforms)
    uniform_buf: wgpu::Buffer,
    uniform_size: u64,
    /// Cached uniform bind group (buffer is reused; content rewritten per frame).
    uniform_bg: Option<wgpu::BindGroup>,
    /// Cached screen bind groups keyed by texture-view pointer.
    screen_bgs: HashMap<usize, wgpu::BindGroup>,
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
            idx: 0,
            screen_bgl,
            sampler,
            blit_pipeline,
            blit_bgs: HashMap::new(),
            pipelines: HashMap::new(),
            chart_dir: None,
            active: Vec::new(),
            width: 0, height: 0, tex_format: tex_fmt,
        };
        pipe.resize(device, width, height);
        pipe
    }

    /// Recreate ping-pong targets at a new viewport size.
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        // Old texture views are being dropped — cached bind groups that
        // reference them must go too, otherwise the old targets can never
        // be freed (leak) and stale views could be bound.
        self.blit_bgs.clear();
        for ep in self.pipelines.values_mut() {
            ep.uniform_bg = None;
            ep.screen_bgs.clear();
        }
        for (i, (t, v)) in self.targets.iter_mut().zip(self.target_views.iter_mut()).enumerate() {
            *t = Some(Self::make_target(device, self.width, self.height, &format!("post-{i}"), self.tex_format));
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

    /// Ensure the pipeline + resources exist for a given effect.
    pub fn ensure_effect(&mut self, device: &wgpu::Device, def: &EffectDef) {
        if self.pipelines.contains_key(def.name) { return; }
        let shader_src = String::from(crate::render::shaders::VERT) + def.frag;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(&format!("shader-{}", def.name)),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Owned(shader_src)),
        });

        // Group 1: uniform buffer — generous 256 bytes covers any effect struct
        let uniform_size: u64 = 256;
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(&format!("bgl-{}", def.name)),
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
            label: Some(&format!("pl-{}", def.name)),
            bind_group_layouts: &[Some(&self.screen_bgl), Some(&bgl)],
            ..Default::default()
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(&format!("pipe-{}", def.name)),
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
            label: Some(&format!("ubuf-{}", def.name)),
            size: uniform_size,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        self.pipelines.insert(def.name.to_string(), EffPipe { pipeline, pl, bgl, uniform_buf, uniform_size, uniform_bg: None, screen_bgs: HashMap::new() });
    }

    /// Load and compile a custom WGSL shader from the chart directory.
    /// Failures are reported (stderr) instead of silently dropping the effect.
    fn ensure_custom_effect(&mut self, device: &wgpu::Device, name: String) {
        if self.pipelines.contains_key(&name) { return; }
        let Some(ref chart_dir) = self.chart_dir else { return };
        let path = chart_dir.join(&name);
        let wgsl = match std::fs::read_to_string(&path) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("warning: custom effect {name}: cannot read {}: {e}", path.display());
                return;
            }
        };

        let combined = String::from(crate::render::shaders::VERT) + &wgsl;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(&format!("custom-shader-{name}")),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Owned(combined)),
        });

        // Create uniform bind group layout (256 bytes)
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(&format!("custom-bgl-{name}")),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: Some(wgpu::BufferSize::new(256).unwrap()),
                },
                count: None,
            }],
        });

        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(&format!("custom-pl-{name}")),
            bind_group_layouts: &[Some(&self.screen_bgl), Some(&bgl)],
            ..Default::default()
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(&format!("custom-pipe-{name}")),
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
            label: Some(&format!("custom-ubuf-{name}")),
            size: 256,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        self.pipelines.insert(name, EffPipe { pipeline, pl, bgl, uniform_buf, uniform_size: 256, uniform_bg: None, screen_bgs: HashMap::new() });
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

        self.idx = 0;

        // Phase 1: ensure all pipelines exist
        // Copy active descriptors: (pipeline_key, uniform_values, uniform_count)
        let descriptors: Vec<(String, Vec<f32>, usize)> = self.active.iter().map(|ae| {
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

        // Phase 2: execute effects
        let mut read_view: &wgpu::TextureView = src;
        let mut idx = self.idx;
        for (key, uv, _) in &descriptors {
            let Some(ep) = self.pipelines.get_mut(key.as_str()) else { continue };
            let write_view = self.target_views[idx].as_ref().unwrap();
            idx = 1 - idx;
            // Write uniform buffer (256 bytes)
            let mut uniform_data = vec![0u8; 256];
            for (i, &val) in uv.iter().enumerate() {
                let offset = i * 4;
                if offset + 4 <= uniform_data.len() {
                    uniform_data[offset..offset+4].copy_from_slice(&val.to_le_bytes());
                }
            }
            queue.write_buffer(&ep.uniform_buf, 0, &uniform_data);
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
                                buffer: &ep.uniform_buf,
                                offset: 0,
                                size: wgpu::BufferSize::new(ep.uniform_size),
                            }),
                        }],
                    });
                    ep.uniform_bg = Some(bg);
                    ep.uniform_bg.as_ref().unwrap()
                }
            };
            // Cache the screen bind group per view; views only change on
            // resize (ping-pong pair + scene/surface targets are stable).
            let view_key = read_view as *const wgpu::TextureView as usize;
            let screen_bg = match ep.screen_bgs.get(&view_key) {
                Some(bg) => bg,
                None => {
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some(&format!("screen-bg-{key}")),
                        layout: &self.screen_bgl,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(read_view) },
                            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                        ],
                    });
                    ep.screen_bgs.insert(view_key, bg);
                    ep.screen_bgs.get(&view_key).unwrap()
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
            pass.set_bind_group(0, screen_bg, &[]);
            pass.set_bind_group(1, uniform_bg, &[]);
            pass.draw(0..3, 0..1);
            read_view = write_view;
        }
        self.idx = idx;
    }

    /// After `apply()`, returns the texture view containing the final result.
    pub fn last_view(&self) -> &wgpu::TextureView {
        self.target_views[1 - self.idx].as_ref().unwrap()
    }
}
