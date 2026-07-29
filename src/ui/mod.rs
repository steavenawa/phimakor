//! Iced (tiny-skia) debug overlay, rendered into a REUSED wgpu texture.
//!
//! Pipeline per frame: pixmap.fill(TRANSPARENT) → iced diff/layout/draw →
//! queue.write_texture into the persistent texture → caller composites the
//! bind group as an ordinary DrawCmd (see render/mod.rs, `draw_to_view`).
//!
//! Pitfalls this design avoids (learned the hard way):
//! - per-frame texture + bind group creation (fps killer) → created once,
//!   resized on demand only
//! - tiny-skia composites OVER the pixmap → must clear each frame or the
//!   semi-transparent panel accumulates into opaque black
//! - premultiplied RGBA: the quad pipeline's straight-alpha blending is fine
//!   with tiny-skia output (transparent = all-zero; white text is identical
//!   in both conventions)

use iced::advanced::layout::{Layout, Limits};
use iced::advanced::widget::{Widget, Tree};
use iced::advanced::{mouse, renderer, Renderer as _};
use iced::{Element, Point, Rectangle, Size, Theme};
use iced_tiny_skia::Renderer;

pub struct IcedOverlay {
    renderer: Renderer,
    tree: Tree,
    theme: Theme,
    pixmap: tiny_skia::Pixmap,
    clip_mask: tiny_skia::Mask,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    w: u32,
    h: u32,
}

impl IcedOverlay {
    pub fn new(
        device: &wgpu::Device,
        tex_bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        w: u32,
        h: u32,
    ) -> Self {
        let (texture, bind_group) = Self::make_texture(device, tex_bgl, sampler, w.max(1), h.max(1));
        let renderer = Renderer::new(iced::Font::default(), iced::Pixels(14.0));
        let root: Element<'_, (), Theme, Renderer> = iced::widget::Column::new().into();
        let tree = Tree::new(&root);
        Self {
            renderer,
            tree,
            theme: Theme::Dark,
            pixmap: tiny_skia::Pixmap::new(w.max(1), h.max(1)).unwrap(),
            clip_mask: tiny_skia::Mask::new(w.max(1), h.max(1)).unwrap(),
            texture,
            bind_group,
            w: w.max(1),
            h: h.max(1),
        }
    }

    fn make_texture(
        device: &wgpu::Device,
        tex_bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        w: u32,
        h: u32,
    ) -> (wgpu::Texture, wgpu::BindGroup) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("iced-ui"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("iced-ui-bg"),
            layout: tex_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            ],
        });
        (texture, bind_group)
    }

    pub fn resize(
        &mut self,
        device: &wgpu::Device,
        tex_bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        w: u32,
        h: u32,
    ) {
        let (w, h) = (w.max(1), h.max(1));
        if (w, h) == (self.w, self.h) {
            return;
        }
        (self.w, self.h) = (w, h);
        self.pixmap = tiny_skia::Pixmap::new(w, h).unwrap();
        self.clip_mask = tiny_skia::Mask::new(w, h).unwrap();
        (self.texture, self.bind_group) = Self::make_texture(device, tex_bgl, sampler, w, h);
    }

    pub fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }

    /// Rasterize the debug HUD into the persistent texture. Cheap: one
    /// pixmap clear + iced draw + one write_texture.
    pub fn render(&mut self, queue: &wgpu::Queue, info: &GameInfo) {
        // tiny-skia composites over existing pixels — clear first.
        self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
        // iced's runtime calls Renderer::reset every frame to clear the
        // primitive layer stack; we drive the renderer manually, so we must
        // do it ourselves or layers accumulate (~200 ms slower per frame).
        self.renderer.reset(Rectangle::new(Point::ORIGIN, Size::new(self.w as f32, self.h as f32)));

        let mut element: Element<'_, (), Theme, Renderer> = build_ui(info);
        self.tree.diff(&element);

        let size = Size::new(self.w as f32, self.h as f32);
        let limits = Limits::new(size, size);
        let node = element.as_widget_mut().layout(&mut self.tree, &self.renderer, &limits);

        let viewport = iced_tiny_skia::graphics::Viewport::with_physical_size(iced::Size::new(self.w, self.h), 1.0);
        let logical = viewport.logical_size();
        let viewport_rect = Rectangle::new(Point::ORIGIN, logical);

        element.as_widget().draw(
            &self.tree,
            &mut self.renderer,
            &self.theme,
            &renderer::Style { text_color: iced::Color::WHITE },
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport_rect,
        );

        let damage = vec![Rectangle::new(Point::ORIGIN, logical)];
        self.renderer.draw(
            &mut self.pixmap.as_mut(),
            &mut self.clip_mask,
            &viewport,
            &damage,
            iced::Color::TRANSPARENT,
        );

        // (engine caches are trimmed inside Renderer::draw itself)

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            self.pixmap.data(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * self.w),
                rows_per_image: Some(self.h),
            },
            wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
        );
    }
}

pub struct GameInfo {
    pub chart_time: f64,
    pub audio_time: f64,
    pub fps: f64,
    pub combo: u32,
    pub hits: u32,
    pub note_count: usize,
    pub score: u32,
    pub lines: usize,
    pub visible_notes: usize,
    pub paused: bool,
    pub dim: f32,
}

fn build_ui<'a>(info: &GameInfo) -> Element<'a, (), Theme, Renderer> {
    use iced::widget::{column, container, text};

    let dbg = column![
        text(format!("time   {:8.3} (audio {:8.3})", info.chart_time, info.audio_time)),
        text(format!("fps    {:.1}", info.fps)),
        text(format!("combo  {}", info.combo)),
        text(format!("hits   {}/{}", info.hits, info.note_count)),
        text(format!("score  {:07}", info.score)),
        text(format!("lines  {}", info.lines)),
        text(format!("notes  {}", info.visible_notes)),
        text(format!("paused {}", info.paused)),
        text(format!("dim    {:.2}", info.dim)),
    ]
    .spacing(2);

    container(dbg)
        .padding(8)
        .style(move |_: &Theme| container::Style::default().background(iced::Color::from_rgba(0.0, 0.0, 0.0, 0.5)))
        .into()
}
