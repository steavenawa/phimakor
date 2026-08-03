//! Post-processing pipeline: ping-pong render targets + per-effect GPU passes.
//! Each effect reads the previous pass's output and writes a new texture.
//! After all effects, the final result is composited into the scene.

use crate::render::shaders::{EffectDef, EFFECTS};
use std::collections::HashMap;

/// A single active effect instance (from extra.json).
#[derive(Clone)]
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
    /// Index of the target holding the last effect output (set by apply()).
    last_output: usize,

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
            last_output: 0,
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

    /// Shared pipeline construction for built-in and custom effects (the two
    /// paths used to duplicate ~70 lines). `wgsl_body` is the fragment shader;
    /// the shared vertex shader is prepended.
    fn build_eff_pipe(&self, device: &wgpu::Device, name: &str, wgsl_body: &str) -> EffPipe {
        let shader_src = String::from(crate::render::shaders::VERT) + wgsl_body;
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

        EffPipe { pipeline, pl, bgl, uniform_buf, uniform_size, uniform_bg: None, screen_bgs: HashMap::new() }
    }

    /// Ensure the pipeline + resources exist for a given effect.
    pub fn ensure_effect(&mut self, device: &wgpu::Device, def: &EffectDef) {
        if self.pipelines.contains_key(def.name) { return; }
        let pipe = self.build_eff_pipe(device, &def.name, def.frag);
        self.pipelines.insert(def.name.to_string(), pipe);
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
        let pipe = self.build_eff_pipe(device, &name, &wgsl);
        self.pipelines.insert(name, pipe);
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
        // 1) 零强度特效(主参数 = 0 时输出与输入相同)直接跳过;
        // 2) 连续完全相同的实例只保留一个。
        let mut active: Vec<&ActiveEffect> = Vec::with_capacity(self.active.len());
        let mut prev_key: Option<(usize, Option<&str>, &[f32])> = None;
        for ae in &self.active {
            if is_effect_noop(ae) {
                continue;
            }
            let key = (ae.shader_idx, ae.custom_name.as_deref(), ae.uniform_values.as_slice());
            if prev_key == Some(key) {
                continue; // 连续重复实例去重
            }
            prev_key = Some(key);
            active.push(ae);
        }
        if active.is_empty() {
            // 全部裁剪:输出 = 输入。做一次 src→target blit 保持
            // last_view() 语义(调用方随后 blit 到 surface)。
            let Some(blit) = &self.blit_pipeline else { self.last_output = 0; return };
            let bg = {
                let view_key = src as *const wgpu::TextureView as usize;
                match self.blit_bgs.get(&view_key) {
                    Some(bg) => bg.clone(),
                    None => {
                        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("blit-bg"),
                            layout: &self.screen_bgl,
                            entries: &[
                                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(src) },
                                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                            ],
                        });
                        self.blit_bgs.insert(view_key, bg.clone());
                        bg
                    }
                }
            };
            let out = self.target_views[0].as_ref().unwrap();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("effect-noop-blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: out,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(blit);
            pass.set_bind_group(0, &bg, &[]);
            pass.draw(0..3, 0..1);
            drop(pass);
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
        let mut read_view: &wgpu::TextureView = src;
        let mut write_idx = 0usize;
        for (key, uv, _) in &descriptors {
            let Some(ep) = self.pipelines.get_mut(key.as_str()) else { continue };
            let write_view = self.target_views[write_idx].as_ref().unwrap();
            write_idx = 1 - write_idx;
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
        // After the loop, `write_idx` points at the slot that was NOT written
        // last (it toggled after the final write); the last output is the
        // other one.
        self.last_output = 1 - write_idx;
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
}

