//! Surfaceless offscreen preview renderer: the same quad pipeline as the
//! window path, drawn into an Rgba8Unorm target and read back to CPU memory
//! (embedding path for the iced editor / mobile readback).

use crate::core::FrameState;

use super::{Renderer, TextAnchor};

/// Offscreen [`Renderer`] + target texture + readback buffer.
/// Returns RGBA8 pixel data for embedding in the iced editor or mobile readback.
pub struct PreviewEngine {
    renderer: Renderer,
    /// Offscreen color target (RENDER_ATTACHMENT | COPY_SRC).
    target: wgpu::Texture,
    /// Readback staging buffer (COPY_DST | MAP_READ), reused every frame.
    readback: wgpu::Buffer,
    /// Readback row stride: 4·width rounded up to `COPY_BYTES_PER_ROW_ALIGNMENT`.
    padded_bpr: u32,
    /// Depadded frame bytes returned by [`render_frame`](Self::render_frame).
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

impl PreviewEngine {
    /// Create an offscreen preview renderer at the given size.
    /// Spawns a surfaceless wgpu device and allocates the readback staging buffer.
    pub async fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        let (width, height) = (width.max(1), height.max(1));
        let renderer = Renderer::new_surfaceless(width, height).await?;
        let (target, readback, padded_bpr) = Self::make_target(renderer.device(), width, height);
        Ok(Self {
            renderer,
            target,
            readback,
            padded_bpr,
            pixels: vec![0; (width * height * 4) as usize],
            width,
            height,
        })
    }

    fn make_target(device: &wgpu::Device, width: u32, height: u32) -> (wgpu::Texture, wgpu::Buffer, u32) {
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bpr = (4 * width).next_multiple_of(align);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview-target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview-readback"),
            size: (padded_bpr * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        (target, readback, padded_bpr)
    }

    /// Resize the offscreen target and readback staging buffer.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 || (width == self.width && height == self.height) {
            return;
        }
        let (target, readback, padded_bpr) = Self::make_target(self.renderer.device(), width, height);
        self.target = target;
        self.readback = readback;
        self.padded_bpr = padded_bpr;
        self.pixels.resize((width * height * 4) as usize, 0);
        self.width = width;
        self.height = height;
        self.renderer.resize(width, height);
    }

    /// Current output dimensions in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Render one frame offscreen and read back RGBA8, row-major, top-down.
    /// Returns pixel bytes (len = w*h*4, row padding already removed).
    pub fn render_frame(&mut self, frame: &FrameState, window_aspect: f32, dim: f32) -> &[u8] {
        let view = self.target.create_view(&wgpu::TextureViewDescriptor::default());
        self.renderer.draw_to_view(&view, frame, window_aspect, dim, None, None);

        let mut encoder = self
            .renderer
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("preview-readback") });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_bpr),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
        self.renderer.queue().submit([encoder.finish()]);

        // Blocking readback: the map callback fires once the copy lands;
        // poll(Wait) pumps the device until then.
        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        let _ = self.renderer.device().poll(wgpu::PollType::wait_indefinitely());
        if matches!(rx.recv(), Ok(Ok(()))) {
            // ponytail: on map failure (device lost) return the previous frame's pixels
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
        self.readback.unmap();
        &self.pixels
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

    /// Update playback progress for the top progress bar (delegates to [`Renderer::set_progress`]).
    pub fn set_progress(&mut self, progress: f32) {
        self.renderer.set_progress(progress);
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
}
