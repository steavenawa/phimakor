//! Surfaceless offscreen preview renderer: the same quad pipeline as the
//! window path, drawn into an Rgba8Unorm target and read back to CPU memory
//! (embedding path for the iced editor / mobile readback).

use crate::core::FrameState;

use super::{Renderer, TextAnchor};

/// Offscreen [`Renderer`] + triple-buffered readback.
/// Returns RGBA8 pixel data for embedding in the iced editor or mobile readback.
///
/// Triple buffering: frame N renders into slot `N % 3` while the pixels of
/// slot `(N+1) % 3` (rendered two frames ago, guaranteed complete) are being
/// read back. The GPU copy of the current frame stays in flight while the CPU
/// consumes the previous result — a fixed 1-frame latency, zero GPU stalls.
pub struct PreviewEngine {
    renderer: Renderer,
    /// Offscreen color targets (RENDER_ATTACHMENT | COPY_SRC), one per slot.
    targets: [wgpu::Texture; 3],
    /// Readback staging buffers (COPY_DST | MAP_READ), one per slot.
    readbacks: [wgpu::Buffer; 3],
    /// Readback row stride: 4·width rounded up to `COPY_BYTES_PER_ROW_ALIGNMENT`.
    padded_bpr: u32,
    /// Depadded frame bytes returned by [`render_frame`](Self::render_frame).
    pixels: Vec<u8>,
    /// Monotonic frame counter; slots rotate `frame_idx % 3`.
    frame_idx: u64,
    width: u32,
    height: u32,
}

impl PreviewEngine {
    /// Create an offscreen preview renderer at the given size.
    /// Spawns a surfaceless wgpu device and allocates the readback staging buffers.
    pub async fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        let (width, height) = (width.max(1), height.max(1));
        let renderer = Renderer::new_surfaceless(width, height).await?;
        let (targets, readbacks, padded_bpr) = Self::make_slots(renderer.device(), width, height);
        Ok(Self {
            renderer,
            targets,
            readbacks,
            padded_bpr,
            pixels: vec![0; (width * height * 4) as usize],
            frame_idx: 0,
            width,
            height,
        })
    }

    fn make_slots(device: &wgpu::Device, width: u32, height: u32) -> ([wgpu::Texture; 3], [wgpu::Buffer; 3], u32) {
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bpr = (4 * width).next_multiple_of(align);
        let mut targets = Vec::with_capacity(3);
        let mut readbacks = Vec::with_capacity(3);
        for i in 0..3 {
            let target = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(&format!("preview-target-{i}")),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(&format!("preview-readback-{i}")),
                size: (padded_bpr * height) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            targets.push(target);
            readbacks.push(readback);
        }
        (
            [targets.pop().unwrap(), targets.pop().unwrap(), targets.pop().unwrap()],
            [readbacks.pop().unwrap(), readbacks.pop().unwrap(), readbacks.pop().unwrap()],
            padded_bpr,
        )
    }

    /// Resize the offscreen targets and readback staging buffers.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 || (width == self.width && height == self.height) {
            return;
        }
        let (targets, readbacks, padded_bpr) = Self::make_slots(self.renderer.device(), width, height);
        self.targets = targets;
        self.readbacks = readbacks;
        self.padded_bpr = padded_bpr;
        self.pixels.resize((width * height * 4) as usize, 0);
        self.width = width;
        self.height = height;
        self.frame_idx = 0;
        self.renderer.resize(width, height);
    }

    /// Current output dimensions in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Borrow the underlying renderer (device/queue/tex_bgl/sampler access).
    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    /// Render one frame offscreen and read back RGBA8, row-major, top-down.
    /// Returns pixel bytes (len = w*h*4, row padding already removed).
    ///
    /// The returned bytes are from the frame rendered two calls ago (1-frame
    /// latency); the current frame's GPU work is still in flight when this
    /// returns, so rendering and readback pipeline across frames.
    pub fn render_frame(&mut self, frame: &FrameState, window_aspect: f32, dim: f32) -> &[u8] {
        let cur = (self.frame_idx % 3) as usize;
        let prev = ((self.frame_idx + 1) % 3) as usize;

        // 1) Render + copy current frame into slot `cur`.
        let view = self.targets[cur].create_view(&wgpu::TextureViewDescriptor::default());
        self.renderer.draw_to_view(&view, frame, window_aspect, dim, None, None);

        let mut encoder = self
            .renderer
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("preview-readback") });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.targets[cur],
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readbacks[cur],
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_bpr),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
        self.renderer.queue().submit([encoder.finish()]);

        // 2) Read back slot `prev` — its copy was submitted two frames ago and
        //    is (nearly always) already complete. Poll non-blockingly so the
        //    CPU never stalls on the current frame's GPU work.
        let slice = self.readbacks[prev].slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(8);
        let mut mapped = false;
        loop {
            if let Ok(Ok(())) = rx.try_recv() {
                mapped = true;
                break;
            }
            if std::time::Instant::now() > deadline {
                // Rare: previous frame still in flight. Poll until the map
                // resolves (must unmap to keep the slot reusable) but skip
                // the pixel copy — keep last frame's pixels.
                mapped = false;
                loop {
                    if let Ok(Ok(())) = rx.try_recv() { break; }
                    self.renderer.device().poll(wgpu::PollType::Wait {
                        submission_index: None,
                        timeout: Some(std::time::Duration::from_millis(1)),
                    });
                }
                break;
            }
            self.renderer.device().poll(wgpu::PollType::Poll);
            std::thread::yield_now();
        }
        if mapped {
            if let Ok(data) = slice.get_mapped_range() {
                let row = (self.width * 4) as usize;
                let padded = self.padded_bpr as usize;
                let len = self.pixels.len();
                if padded == row {
                    self.pixels.copy_from_slice(&data[..len]);
                } else {
                    for (dst, src) in self.pixels.chunks_exact_mut(row).zip(data.chunks(padded)) {
                        dst.copy_from_slice(&src[..row]);
                    }
                }
            }
        }
        // Always unmap so the slot is ready for the next map_async (wgpu
        // forbids mapping a buffer while a previous map is pending).
        self.readbacks[prev].unmap();

        self.frame_idx += 1;
        &self.pixels
    }

    /// Copy the last rendered frame into `out`, reusing its allocation.
    /// Avoids the per-frame `Vec::new` + page-fault cost of [`pixels`](Self::pixels)`.
    pub fn copy_pixels_to(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&self.pixels);
    }

    /// Copy the last rendered frame into `out` (reused buffer) and return it.
    pub fn pixels_reused<'a>(&self, out: &'a mut Vec<u8>) -> &'a [u8] {
        out.clear();
        out.extend_from_slice(&self.pixels);
        out
    }

    /// Set the background image (delegates to [`Renderer::set_background`]).
    pub fn set_background(&mut self, img_bytes: &[u8], dim: f32) -> anyhow::Result<()> {
        self.renderer.set_background(img_bytes, dim)
    }

    /// Load a named texture (delegates to [`Renderer::load_texture`]).
    pub fn load_texture(&mut self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.renderer.load_texture(name, bytes)
    }

    /// Switch playfield canvas aspect ratio (delegates to [`Renderer::set_playfield_aspect`]).
    pub fn set_playfield_aspect(&mut self, aspect: f32) {
        self.renderer.set_playfield_aspect(aspect);
    }

    /// Current playfield canvas aspect ratio.
    pub fn playfield_aspect(&self) -> f32 {
        self.renderer.playfield_aspect()
    }

    /// Update playback progress for the top progress bar (delegates to [`Renderer::set_progress`]).
    pub fn set_progress(&mut self, progress: f32) {
        self.renderer.set_progress(progress);
    }

    /// Set the Phigros-style HUD (delegates to [`Renderer::set_hud`]).
    pub fn set_hud(&mut self, hud: crate::render::HudData) {
        self.renderer.set_hud(hud);
    }

    /// Set the judge-line length multiplier (delegates to [`Renderer::set_line_length`]).
    pub fn set_line_length(&mut self, length: f32) {
        self.renderer.set_line_length(length);
    }

    /// Read-only access to the last rendered RGBA frame.
    pub fn pixels(&self) -> &[u8] { &self.pixels }

    /// Spawn a hit-effect burst at the given canvas position.
    pub fn spawn_hit_fx(&mut self, pos_canvas: [f32; 2]) {
        self.renderer.spawn_hit_fx(pos_canvas);
    }

    /// Queue a text overlay for the next frame.
    pub fn draw_text(&mut self, text: &str, anchor: TextAnchor, color: [f32; 4]) {
        self.renderer.draw_text(text, anchor, color);
    }

    /// Evaluate and apply post-processing effects from extra.json for the
    /// current chart beat. Must be called before [`render_frame`].
    pub fn set_effects_from_extra(&mut self, extra: &crate::core::extra::ExtraRoot, chart_beat: f64, chart_time: f32) {
        use crate::render::shaders::EFFECTS;
        use crate::render::post::ActiveEffect;
        self.renderer.post.active.clear();
        let evals = crate::core::extra::evaluate_effects(extra, chart_beat);
        let size = self.renderer.size();
        let (sw, sh) = (size[0] as f32, size[1] as f32);
        for e in &evals {
            let si = EFFECTS.iter().position(|d| d.name == e.shader_name).unwrap_or(usize::MAX);
            let (uv, count) = if si == usize::MAX {
                (e.uniforms.clone(), e.uniforms.len())
            } else {
                let def = &EFFECTS[si];
                let mut uv: Vec<f32> = def.defaults.iter().map(|(_, v)| *v).collect();
                let norm = |s: &str| s.to_lowercase().replace("_", "").replace("-", "");
                for (i, (dname, _)) in def.defaults.iter().enumerate() {
                    let base = dname.trim_end_matches("_r").trim_end_matches("_g")
                        .trim_end_matches("_b").trim_end_matches("_a")
                        .trim_end_matches("_x").trim_end_matches("_y");
                    let nbase = norm(base);
                    if let Some(pos) = e.uniforms_names.iter().position(|n| norm(n) == nbase) {
                        uv[i] = e.uniforms[pos];
                    }
                    if dname.contains("screen_size") {
                        uv[i] = if dname.ends_with('x') { sw } else { sh };
                    }
                    if *dname == "time" {
                        uv[i] = chart_time;
                    }
                }
                let l = uv.len();
                (uv, l)
            };
            self.renderer.post.active.push(ActiveEffect {
                shader_idx: si,
                custom_name: if si == usize::MAX { Some(e.shader_name.clone()) } else { None },
                priority: e.priority,
                uniform_values: uv,
                uniform_count: count,
            });
        }
    }
}
