use iced::advanced::layout::{Layout, Limits};
use iced::advanced::widget::Tree;
use iced::advanced::{renderer, Renderer as _};
use iced::{Element, Length, Point, Rectangle, Size, Theme};
use iced_tiny_skia::Renderer;

pub mod panels;

static UI_FONT: std::sync::OnceLock<fontdue::Font> = std::sync::OnceLock::new();

fn get_font() -> &'static fontdue::Font {
    UI_FONT.get_or_init(|| {
        std::fs::read("res/Exo2.ttf").ok()
            .and_then(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok())
            .unwrap_or_else(|| {
                // Fallback: built-in Exo2 or system font
                fontdue::Font::from_bytes(
                    include_bytes!("../../res/Exo2.ttf") as &[u8],
                    fontdue::FontSettings::default(),
                ).expect("Exo2.ttf bundled")
            })
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum OverlayMessage {
    ToggleEvents,
    SelectLayer(usize),
}

pub struct IcedOverlay {
    renderer: Renderer,
    tree: Tree,
    theme: Theme,
    pixmap: tiny_skia::Pixmap,
    clip_mask: tiny_skia::Mask,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    w: u32, h: u32,
    panel_progress: f32,
    events_progress: f32,
    notes_progress: f32,
    pub mouse_pos: Option<(f32, f32)>,
    show_overlay: bool,
    tl_visible: bool,
    tool_hover: Option<usize>,
    pub selected_tool: usize,
    tool_hover_progress: [f32; 3],
    panel_defs: Vec<panels::PanelDef>,
    pub messages: Vec<OverlayMessage>,
    timeline_click: Option<f32>,
    layer_click: Option<f32>,
    pub tl_scroll: f32,
    pub tl_zoom: f32,
    pub gui_scale: f32,
    select_start: Option<(f32, f32)>,
    select_end: Option<(f32, f32)>,
    pub selecting: bool,
    pub seek_dragging: bool,
    ctx_pos: Option<(f32, f32)>,
    ctx_progress: f32,
}

const KIND_COLORS: [(&str, [u8; 3]); 5] = [
    ("Alpha", [70, 130, 255]), ("MoveX", [70, 200, 100]),
    ("MoveY", [255, 170, 60]), ("Rotate", [255, 220, 60]), ("Speed", [255, 80, 80]),
];

impl IcedOverlay {
    pub fn new(device: &wgpu::Device, tex_bgl: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler, w: u32, h: u32) -> Self {
        let (texture, bind_group) = Self::make_texture(device, tex_bgl, sampler, w.max(1), h.max(1));
        let renderer = Renderer::new(iced::Font::default(), iced::Pixels(14.0));
        let root: Element<'_, (), Theme, Renderer> = iced::widget::Column::new().into();
        let tree = Tree::new(&root);
        Self { renderer, tree, theme: Theme::Dark, pixmap: tiny_skia::Pixmap::new(w.max(1), h.max(1)).unwrap(), clip_mask: tiny_skia::Mask::new(w.max(1), h.max(1)).unwrap(), texture, bind_group, w: w.max(1), h: h.max(1), panel_progress: 0.0, events_progress: 0.0, notes_progress: 0.0, mouse_pos: None, show_overlay: true, tl_visible: false, tool_hover: None, selected_tool: 0, tool_hover_progress: [0.0; 3], panel_defs: Vec::new(), messages: Vec::new(), timeline_click: None, layer_click: None, tl_scroll: 0.0, tl_zoom: 8.0, gui_scale: 1.0, select_start: None, select_end: None, selecting: false, seek_dragging: false, ctx_pos: None, ctx_progress: 0.0 }
    }

    fn make_texture(device: &wgpu::Device, tex_bgl: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler, w: u32, h: u32) -> (wgpu::Texture, wgpu::BindGroup) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("iced-ui"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("iced-ui-bg"), layout: tex_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            ],
        });
        (texture, bind_group)
    }

    pub fn resize(&mut self, device: &wgpu::Device, tex_bgl: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1)); if (w, h) == (self.w, self.h) { return; }
        (self.w, self.h) = (w, h);
        self.pixmap = tiny_skia::Pixmap::new(w, h).unwrap();
        self.clip_mask = tiny_skia::Mask::new(w, h).unwrap();
        (self.texture, self.bind_group) = Self::make_texture(device, tex_bgl, sampler, w, h);
    }

    pub fn bind_group(&self) -> &wgpu::BindGroup { &self.bind_group }
    pub fn props_progress(&self) -> f32 { self.panel_progress }
    pub fn handle_cursor(&mut self, x: f64, y: f64) {
        self.mouse_pos = Some((x as f32, y as f32));
        if self.selecting {
            self.select_end = Some((x as f32, y as f32));
        }
        self.tool_hover = None;
        let s = self.gui_scale;
        let btn_base = 34.0 * s;
        let gap = 6.0 * s;
        let mx = x as f32;
        let my = y as f32;
        // Match draw_quick_panel: left-aligned at 4*s, hover progress for width
        let max_hp = self.tool_hover_progress.iter().cloned().reduce(f32::max).unwrap_or(0.0);
        let bg_extra = max_hp * 100.0 * s;
        let bg_w = QP_W * s + bg_extra;
        for i in 0..3 {
            let y0 = gap + i as f32 * (btn_base + gap);
            let extra = self.tool_hover_progress[i] * 80.0 * s;
            let bw = btn_base + extra;
            let bx = 4.0 * s;
            let by = y0;
            if mx >= bx && mx <= bx + bw && my >= by && my <= by + btn_base {
                self.tool_hover = Some(i);
                break;
            }
        }
    }

    pub fn is_over_timeline(&self, props: f32) -> bool {
        self.is_over_events(props) || self.is_over_notes(props)
    }

    fn panel_x(&self, props: f32, panel_w: f32, progress: f32) -> f32 {
        let s = self.gui_scale;
        let pp = self.panel_progress;
        let props_x = self.w as f32 - pp * PANEL_W * s;
        props_x - progress * panel_w * s
    }

    fn is_over_events(&self, props: f32) -> bool {
        let s = self.gui_scale;
        let Some((mx, my)) = self.mouse_pos else { return false };
        let px = self.panel_x(props, TL_W, self.events_progress);
        let ly = self.h as f32 - 48.0 * s - 26.0 * s;
        mx >= px && mx <= px + TL_W * s && my >= HEADER_H * s + 4.0 * s && my <= ly - 2.0
    }

    fn is_over_notes(&self, props: f32) -> bool {
        let s = self.gui_scale;
        let Some((mx, my)) = self.mouse_pos else { return false };
        let px = self.panel_x(props, NT_W, self.notes_progress);
        let ly = self.h as f32 - 48.0 * s - 26.0 * s;
        mx >= px && mx <= px + NT_W * s && my >= HEADER_H * s + 4.0 * s && my <= ly - 2.0
    }

    /// Zoom the timeline (Ctrl+scroll). `delta` is the notch value.
    pub fn timeline_zoom_in(&mut self, delta: f32) {
        self.tl_zoom = (self.tl_zoom * (1.0 - delta * 0.1)).clamp(2.0, 64.0);
    }

    /// Scroll the timeline (mouse wheel). `delta` is the notch value.
    pub fn timeline_scroll(&mut self, delta: f32) {
        self.tl_scroll = (self.tl_scroll - delta * self.tl_zoom * 0.15).max(0.0);
    }

    pub fn handle_click(&mut self, pressed: bool, ctrl: bool) {
        let s = self.gui_scale;
        let Some((mx, my)) = self.mouse_pos else { return };
        // Left click always dismisses context menu
        self.ctx_pos = None;
        // Seek bar: press starts drag; release always stops (before any other handler)
        if pressed && self.is_over_seekbar() { self.seek_dragging = true; }
        if !pressed { self.seek_dragging = false; }
        if self.seek_dragging { return; }
        // Release: handle bottom bar buttons and selection end
        if !pressed {
            if self.selecting { self.selecting = false; }
            if my >= self.h as f32 - 52.0 * s && my <= self.h as f32 - 8.0 * s && self.show_overlay {
                let cx = self.w as f32 / 2.0;
                let btn_w = 100.0 * s;
                let gap = 8.0 * s;
                let total_w = btn_w * 2.0 + gap;
                let left_start = cx - total_w * 0.5;
                if mx >= left_start && mx < left_start + btn_w {
                    self.messages.push(OverlayMessage::SelectLayer(666)); return;
                }
                if mx >= left_start + btn_w + gap && mx < left_start + total_w {
                    self.messages.push(OverlayMessage::ToggleEvents); return;
                }
                return;
            }
        }
        // Ctrl+click on timeline: press starts drag, release ends
        if ctrl && self.is_over_timeline(0.0) {
            if pressed { self.select_start = Some((mx, my)); self.select_end = Some((mx, my)); self.selecting = true; }
            else { self.select_end = Some((mx, my)); }
            return;
        }
        // On release only: handle layer selector, tool click, timeline click
        if !pressed {
            if self.show_overlay && self.tl_visible {
                let lx = self.w as f32 - self.panel_progress * PANEL_W * s - TL_W * s - 2.0;
                if mx >= lx && mx <= lx + TL_W * s {
                    let ly = self.h as f32 - 48.0 * s - 26.0 * s;
                    if my >= ly && my <= ly + 22.0 * s {
                        self.layer_click = Some(mx); return;
                    }
                }
            }
            if let Some(i) = self.tool_hover {
                self.selected_tool = i; return;
            }
            self.timeline_click = Some(mx);
        }
    }

    /// Get seek ratio (0..1) from a position in the seek bar area (main.rs uses this).
    pub fn seek_from_pos(&self, mx: f32, info: &GameInfo) -> f64 {
        let s = self.gui_scale;
        let qp_w = QP_W * s;
        let sb_w = qp_w;
        ((mx / sb_w).clamp(0.0, 1.0) * info.duration as f32) as f64
    }

    pub fn is_over_seekbar(&self) -> bool {
        let s = self.gui_scale;
        let Some((mx, my)) = self.mouse_pos else { return false };
        let pp = self.panel_progress;
        let props_x = self.w as f32 - pp * PANEL_W * s;
        let sb_y = self.h as f32 - 50.0 * s;
        let sb_h = 16.0 * s;
        let sb_x = QP_W * s + 2.0 * s;
        let sb_w = (props_x - sb_x - 2.0 * s).max(20.0);
        mx >= sb_x && mx <= sb_x + sb_w && my >= sb_y && my <= sb_y + sb_h
    }

    /// Called when Ctrl is released: finalize box selection and clear the rect.
    pub fn finish_selection(&mut self) {
        if self.selecting {
            self.selecting = false;
            self.select_start = None;
            self.select_end = None;
        }
    }

    pub fn handle_right_click(&mut self, props: f32) {
        // Whitelist: only open context menu on timeline panels
        if !self.is_over_timeline(props) { return; }
        if let Some((mx, my)) = self.mouse_pos {
            self.ctx_pos = Some((mx, my));
        }
    }

    /// Resolve a pending layer bar click → layer index.
    pub fn take_layer_click(&mut self, props: f32, max_layers: usize) -> Option<usize> {
        let s = self.gui_scale;
        let mx = self.layer_click.take()?;
        let px = self.w as f32 - props * PANEL_W * s - TL_W * s - 2.0;
        let bx = px + 6.0 * s;
        let bw = 20.0 * s;
        let clicked = ((mx - bx) / (bw + 4.0 * s)).floor() as usize;
        if clicked < max_layers { Some(clicked) } else { None }
    }

    /// If there's a pending timeline click, resolve it against the current
    /// events and return the flattened event index.
    pub fn take_timeline_click(&mut self, info: &GameInfo, props: f32) -> Option<usize> {
        let mx = self.timeline_click.take()?;
        self.hit_test_timeline_impl(info, props, mx)
    }

    fn hit_test_timeline_impl(&self, info: &GameInfo, props: f32, mx: f32) -> Option<usize> {
        let s = self.gui_scale;
        let my = self.mouse_pos?.1;
        let px = self.w as f32 - props * PANEL_W * s - TL_W * s - 2.0;
        if mx < px || mx > px + TL_W * s { return None; }
        if my < HEADER_H * s + 4.0 * s { return None; }

        let py = (HEADER_H * s + 4.0 * s) as f64;
        let ph = (self.h as f32 - 48.0 * s - py as f32) as f64;
        if ph <= 0.0 { return None; }

        let scroll = self.tl_scroll as f64;
        let zoom = self.tl_zoom as f64;
        let click_beat = scroll + (1.0 - (my as f64 - py) / ph) * zoom;

        let col = ((mx - px - 4.0 * s) / (COL_W * s + COL_GAP * s)).floor() as usize;
        let kind = match col { 0 => "Alpha", 1 => "MoveX", 2 => "MoveY", 3 => "Rotate", 4 => "Speed", _ => return None };

        // Find the event that spans this beat in the clicked column
        let mut best: Option<(usize, f64)> = None;
        for (i, ev) in info.events.iter().enumerate() {
            if ev.kind != kind { continue; }
            if click_beat >= ev.start_beats && click_beat <= ev.end_beats {
                let mid = (ev.start_beats + ev.end_beats) / 2.0;
                let dist = (click_beat - mid).abs();
                if best.map_or(true, |(_, d)| dist < d) {
                    best = Some((i, dist));
                }
            }
        }
        best.map(|(i, _)| i)
    }

    fn approach(&self, current: f32, target: f32) -> f32 {
        let d = target - current; if d.abs() < 0.005 { target } else { current + d * 0.12 }
    }

    /// Rebuild the full Iced widget tree + draw everything (dirty-triggered, ~10fps).
    pub fn render_iced(&mut self, queue: &wgpu::Queue, info: &GameInfo) {
        self.panel_progress = self.approach(self.panel_progress, if info.show_properties { 1.0 } else { 0.0 });
        self.events_progress = self.approach(self.events_progress, if info.show_events { 1.0 } else { 0.0 });
        self.notes_progress = self.approach(self.notes_progress, if info.show_notes { 1.0 } else { 0.0 });
        self.show_overlay = info.show_overlay;
        self.tl_visible = info.show_events || info.show_notes;
        self.gui_scale = info.gui_scale;
        self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
        self.renderer.reset(Rectangle::new(Point::ORIGIN, Size::new(self.w as f32, self.h as f32)));
        let mut element = build_ui(info, self.panel_progress);
        self.tree.diff(&element);
        let size = Size::new(self.w as f32, self.h as f32);
        let limits = Limits::new(size, size);
        let node = element.as_widget_mut().layout(&mut self.tree, &self.renderer, &limits);
        let viewport = iced_tiny_skia::graphics::Viewport::with_physical_size(iced::Size::new(self.w, self.h), 1.0);
        let logical = viewport.logical_size();
        element.as_widget().draw(&self.tree, &mut self.renderer, &self.theme,
            &renderer::Style { text_color: iced::Color::WHITE }, Layout::new(&node),
            iced::advanced::mouse::Cursor::Unavailable, &Rectangle::new(Point::ORIGIN, logical));
        self.renderer.draw(&mut self.pixmap.as_mut(), &mut self.clip_mask, &viewport,
            &[Rectangle::new(Point::ORIGIN, logical)], iced::Color::TRANSPARENT);

        self.upload_timeline(queue, info)
    }

    pub fn render_progress(&self) -> (f32, f32) { (self.events_progress, self.notes_progress) }

    pub fn set_panels(&mut self, panels: Vec<panels::PanelDef>) {
        self.panel_defs = panels;
    }

    /// Timeline-only redraw (120fps): no clear, no Iced — just update the
    /// tiny_skia content and upload. Previous Iced content stays intact.
    pub fn redraw_timeline(&mut self, queue: &wgpu::Queue, info: &GameInfo) {
        self.tl_visible = info.show_events;
        self.upload_timeline(queue, info)
    }

    fn animate_all(&mut self) {
        self.animate_tool_hover();
        // Context menu animation
        let ctx_target = if self.ctx_pos.is_some() { 1.0 } else { 0.0 };
        let d = ctx_target - self.ctx_progress;
        if d.abs() > 0.005 { self.ctx_progress += d * 0.12; }
        else { self.ctx_progress = ctx_target; }
    }

    fn animate_tool_hover(&mut self) {
        for i in 0..3 {
            let target = if self.tool_hover == Some(i) { 1.0 } else { 0.0 };
            let d = target - self.tool_hover_progress[i];
            if d.abs() > 0.005 {
                self.tool_hover_progress[i] += d * 0.15;
            } else {
                self.tool_hover_progress[i] = target;
            }
        }
    }

    fn upload_timeline(&mut self, queue: &wgpu::Queue, info: &GameInfo) {
        self.animate_all();
        let s = self.gui_scale;
        let vw = self.w as f32;
        let vh = self.h as f32;
        let ep = self.events_progress;
        let np = self.notes_progress;
        let pp = self.panel_progress;
        let qp_w = QP_W * s;
        let pan_w = PANEL_W * s;
        let ev_w = TL_W * s;
        let nt_w = NT_W * s;

        // Left toolbar always visible (no animation)
        draw_quick_panel(&mut self.pixmap.as_mut(), self.tool_hover, self.selected_tool, self.tool_hover_progress, info, qp_w, vw, vh, s);

        // Layout: [QP] [Playfield] [Events→] [Notes→] [Properties→]
        let props_x = vw - pp * pan_w;
        let events_x = props_x - ep * ev_w;  // Events next to Properties
        let notes_x = events_x - np * nt_w;  // Notes left of Events

        if self.tl_visible {
            self.tl_scroll = (info.chart_beat as f32 - self.tl_zoom * 0.1).max(0.0);
            if info.show_notes {
                draw_notes_timeline(&mut self.pixmap.as_mut(), self.tl_scroll, self.tl_zoom, info, notes_x, vh, s);
            }
            if info.show_events {
                draw_5col_timeline(&mut self.pixmap.as_mut(), self.tl_scroll, self.tl_zoom, info, events_x, vh, s);
            }
        }
        // Selection rect (Ctrl+drag) + seek bar + context menu
        {
            let mut pm = self.pixmap.as_mut();
            if let (Some(s), Some(e)) = (self.select_start, self.select_end) {
                let mut sel = tiny_skia::Paint::default();
                sel.set_color_rgba8(100, 150, 255, 60);
                let rx = s.0.min(e.0);
                let ry = s.1.min(e.1);
                let rw = (s.0 - e.0).abs();
                let rh = (s.1 - e.1).abs();
                if rw > 2.0 && rh > 2.0 {
                    if let Some(r) = tiny_skia::Rect::from_xywh(rx, ry, rw, rh) {
                        pm.fill_rect(r, &sel, tiny_skia::Transform::default(), None);
                    }
                }
            }
            if self.ctx_progress > 0.01 {
                let (mx, my) = self.ctx_pos.unwrap_or((0.0, 0.0));
                let mw = 160.0 * s; let mh = 120.0 * s;
                let alpha = (self.ctx_progress * 230.0) as u8;
                let mut mp = tiny_skia::Paint::default();
                mp.set_color_rgba8(25, 25, 30, alpha);
                if let Some(r) = tiny_skia::Rect::from_xywh(mx.min(vw - mw), my.min(vh - mh), mw, mh) {
                    pm.fill_rect(r, &mp, tiny_skia::Transform::default(), None);
                }
            }
            // Seek bar: thin strip above bottom buttons, full playfield width
            if info.show_overlay {
                let sb_h = 16.0 * s;
                let sb_y = vh - 50.0 * s;
                let sb_x = qp_w;
                let sb_w = (props_x - sb_x).max(20.0);
                let mut sbg = tiny_skia::Paint::default();
                sbg.set_color_rgba8(40, 45, 55, 200);
                if let Some(r) = tiny_skia::Rect::from_xywh(sb_x, sb_y, sb_w, sb_h) {
                    pm.fill_rect(r, &sbg, tiny_skia::Transform::default(), None);
                }
                let prog = (info.chart_time / info.duration.max(0.01)) as f32;
                if prog > 0.01 {
                    let mut fp = tiny_skia::Paint::default();
                    fp.set_color_rgba8(100, 180, 255, 200);
                    if let Some(r) = tiny_skia::Rect::from_xywh(sb_x + 1.0 * s, sb_y + 1.0 * s, (sb_w - 2.0 * s) * prog.min(1.0), (sb_h - 2.0 * s).max(1.0)) {
                        pm.fill_rect(r, &fp, tiny_skia::Transform::default(), None);
                    }
                }
            }
        }

        // Render panel definition matching selected tool
        if info.show_properties && pp > 0.01 {
            let idx = self.selected_tool.min(self.panel_defs.len().max(1) - 1);
            if let Some(def) = self.panel_defs.get(idx) {
                draw_panel_def(&mut self.pixmap.as_mut(), def, info, props_x, vw, vh, s);
            }
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &self.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            self.pixmap.data(),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
            wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
        );
    }
}

/// Draw splash screen (chart picker) when no chart is loaded.
pub fn draw_splash(pm: &mut tiny_skia::PixmapMut, charts: &[String], hover: Option<usize>, vw: f32, vh: f32, s: f32) {
    let bar_h = 48.0 * s;
    // Background
    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(15, 15, 20, 255);
    if let Some(r) = tiny_skia::Rect::from_xywh(0.0, 0.0, vw, vh - bar_h) {
        pm.fill_rect(r, &bg, tiny_skia::Transform::default(), None);
    }
    // Title
    let font = get_font();
    draw_text_on_pixmap(pm, "PhiMakor - Select Chart", vw * 0.5 - 100.0 * s, 40.0 * s, 18.0 * s, font);
    // Chart list
    let mut y = 70.0 * s;
    let cell_h = 28.0 * s;
    for (i, name) in charts.iter().enumerate() {
        if y > vh - 60.0 * s { break; }
        let is_hover = hover == Some(i);
        let mut cp = tiny_skia::Paint::default();
        cp.set_color_rgba8(if is_hover { 40 } else { 25 }, if is_hover { 45 } else { 28 }, if is_hover { 55 } else { 32 }, 220);
        if let Some(r) = tiny_skia::Rect::from_xywh(40.0 * s, y, vw - 80.0 * s, cell_h) {
            pm.fill_rect(r, &cp, tiny_skia::Transform::default(), None);
        }
        draw_text_on_pixmap(pm, name, 48.0 * s, y + cell_h * 0.5, 14.0 * s, font);
        y += cell_h + 4.0 * s;
    }
}

// ── 5-column timeline ──

const TL_W: f32 = 260.0;
const NT_W: f32 = 520.0;
pub const QP_W: f32 = 44.0;   // left quick toolbar width
const COL_W: f32 = 36.0;
const COL_GAP: f32 = 4.0;
const HEADER_H: f32 = 20.0;
pub const PANEL_W: f32 = 280.0;

fn draw_5col_timeline(pixmap: &mut tiny_skia::PixmapMut, scroll: f32, zoom: f32, info: &GameInfo, px: f32, vh: f32, s: f32) {
    if !info.show_overlay || !info.show_events || info.events.is_empty() { return; }
    let tl_w = TL_W * s;
    let col_w = COL_W * s;
    let col_g = COL_GAP * s;
    let head_h = HEADER_H * s;

    let py = head_h + 4.0 * s;
    let ph = (vh - 50.0 * s - py) as f64;
    if ph <= 0.0 { return; }

    let (scroll, zoom) = (scroll as f64, zoom as f64);
    let (min_b, max_b) = (scroll, scroll + zoom);

    let to_y = |b: f64| py as f64 + ph - (b - min_b) / zoom * ph;

    let n_cols: usize = if info.show_notes && !info.notes.is_empty() { 6 } else { 5 };
    let hdr_h = head_h - 4.0 * s;

    // Draw column header labels
    for ci in 0..n_cols {
        let cx = px + 4.0 * s + ci as f32 * (col_w + col_g);
        let (r, g, b) = if ci < 5 { let c = KIND_COLORS[ci].1; (c[0], c[1], c[2]) }
            else { (140, 200, 255) };
        let mut hp = tiny_skia::Paint::default();
        hp.set_color_rgba8(r, g, b, 180);
        if let Some(hr) = tiny_skia::Rect::from_xywh(cx, 2.0 * s, col_w, hdr_h) {
            pixmap.fill_rect(hr, &hp, tiny_skia::Transform::default(), None);
        }
    }

    // Beat lines across all columns
    let b_start = (min_b.ceil() as i32).max(0);
    let b_end = max_b.floor() as i32;
    for b in b_start..=b_end {
        let y = to_y(b as f64) as f32;
        let mut lp = tiny_skia::Paint::default();
        lp.set_color_rgba8(60, 60, 70, 90);
        for ci in 0..n_cols {
            let cx = px + 4.0 * s + ci as f32 * (col_w + col_g);
            if let Some(r) = tiny_skia::Rect::from_xywh(cx, y, col_w, 1.0) {
                pixmap.fill_rect(r, &lp, tiny_skia::Transform::default(), None);
            }
        }
    }

    // Current time line across all columns (beat-aligned)
    let ct_y = to_y(info.chart_beat).clamp(py as f64, py as f64 + ph) as f32;
    let mut cp = tiny_skia::Paint::default();
    cp.set_color_rgba8(255, 200, 80, 230);
    if let Some(r) = tiny_skia::Rect::from_xywh(px + 2.0 * s, ct_y - 1.0, tl_w - 4.0 * s, 3.0) {
        pixmap.fill_rect(r, &cp, tiny_skia::Transform::default(), None);
    }

    // Layer selector at bottom of timeline panel
    let layer_bar_y = (vh - 48.0 * s) as f32 - 26.0 * s;
    let mut lbg = tiny_skia::Paint::default();
    lbg.set_color_rgba8(30, 30, 35, 200);
    if let Some(r) = tiny_skia::Rect::from_xywh(px + 2.0 * s, layer_bar_y, tl_w - 4.0 * s, 22.0 * s) {
        pixmap.fill_rect(r, &lbg, tiny_skia::Transform::default(), None);
    }
    for li in 0..info.max_layers.min(10) {
        let bx = px + 6.0 * s + li as f32 * (20.0 * s + 4.0 * s);
        let mut lp = tiny_skia::Paint::default();
        if li == info.selected_layer { lp.set_color_rgba8(200, 200, 220, 220); }
        else { lp.set_color_rgba8(100, 100, 120, 160); }
        if let Some(r) = tiny_skia::Rect::from_xywh(bx, layer_bar_y + 3.0 * s, 20.0 * s, 16.0 * s) {
            pixmap.fill_rect(r, &lp, tiny_skia::Transform::default(), None);
        }
    }

    // Event blocks per column (cols 0-4), skip outside visible range
    let ev_min = min_b - zoom * 0.05;
    let ev_max = max_b + zoom * 0.05;
    for ev in &info.events {
        if ev.end_beats < ev_min || ev.start_beats > ev_max { continue; }
        let ci = match ev.kind.as_str() {
            "Alpha" => Some(0), "MoveX" => Some(1), "MoveY" => Some(2),
            "Rotate" => Some(3), "Speed" => Some(4), _ => None,
        };
        let Some(ci) = ci else { continue };
        let c = KIND_COLORS[ci].1;
        let y0 = to_y(ev.start_beats).clamp(py as f64, py as f64 + ph) as f32;
        let y1 = to_y(ev.end_beats).clamp(py as f64, py as f64 + ph) as f32;
        let bh = (y1 - y0).abs().max(3.0);
        let mut ep = tiny_skia::Paint::default();
        ep.set_color_rgba8(c[0], c[1], c[2], 200);
        let cx = px + 4.0 * s + ci as f32 * (col_w + col_g);
        if let Some(r) = tiny_skia::Rect::from_xywh(cx, y0.min(y1), col_w, bh) {
            pixmap.fill_rect(r, &ep, tiny_skia::Transform::default(), None);
        }
    }

    // Note blocks per column (col 5, only when show_notes)
    if n_cols >= 6 {
        for note in &info.notes {
            let c = match note.kind { 1 => [50, 150, 255], 2 => [100, 200, 255], 3 => [255, 80, 150], 4 => [255, 220, 60], _ => [180; 3] };
            let y0 = to_y(note.start_beats).clamp(py as f64, py as f64 + ph) as f32;
            let y1 = to_y(note.end_beats).clamp(py as f64, py as f64 + ph) as f32;
            let bh = (y1 - y0).abs().max(3.0);
            let mut np = tiny_skia::Paint::default();
            np.set_color_rgba8(c[0], c[1], c[2], 200);
            let cx = px + 4.0 * s + 5.0 * (col_w + col_g);
            if let Some(r) = tiny_skia::Rect::from_xywh(cx + 1.0, y0.min(y1), col_w - 2.0, bh) {
                pixmap.fill_rect(r, &np, tiny_skia::Transform::default(), None);
            }
        }
    }
}

fn draw_notes_timeline(pixmap: &mut tiny_skia::PixmapMut, scroll: f32, zoom: f32, info: &GameInfo, px: f32, vh: f32, s: f32) {
    if !info.show_overlay || !info.show_notes || info.notes.is_empty() { return; }
    let nt_w = NT_W * s;
    let head_h = HEADER_H * s;
    let pad_x = 12.0 * s;
    let play_w = nt_w - pad_x * 2.0;
    let py = head_h + 4.0 * s;
    let ph = (vh - 50.0 * s - py) as f64;
    if ph <= 0.0 { return; }

    let (scroll, zoom) = (scroll as f64, zoom as f64);
    let (min_b, max_b) = (scroll, scroll + zoom);
    let to_y = |b: f64| py as f64 + ph - (b - min_b) / zoom * ph;
    // X position: note.position_x / 675 → -1..1 → panel center ±play_w/2
    let to_x = |nx: f32| px + pad_x + (nx / 675.0 + 1.0) * play_w * 0.5;

    // Background
    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(20, 25, 35, 215);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, nt_w, vh - 48.0 * s) {
        pixmap.fill_rect(r, &bg, tiny_skia::Transform::default(), None);
    }

    // Header (full preview mode = different color)
    let mut hp = tiny_skia::Paint::default();
    if info.full_notes { hp.set_color_rgba8(200, 140, 255, 200); }
    else { hp.set_color_rgba8(140, 200, 255, 180); }
    if let Some(r) = tiny_skia::Rect::from_xywh(px + pad_x, 2.0 * s, play_w, head_h - 4.0 * s) {
        pixmap.fill_rect(r, &hp, tiny_skia::Transform::default(), None);
    }

    // Beat lines
    for b in ((min_b.ceil() as i32).max(0))..=(max_b.floor() as i32) {
        let y = to_y(b as f64) as f32;
        let mut lp = tiny_skia::Paint::default();
        lp.set_color_rgba8(60, 60, 70, 70);
        if let Some(r) = tiny_skia::Rect::from_xywh(px + pad_x, y, play_w, 1.0) {
            pixmap.fill_rect(r, &lp, tiny_skia::Transform::default(), None);
        }
    }

    // Center line (x = 0)
    let cx = to_x(0.0);
    let mut cl = tiny_skia::Paint::default();
    cl.set_color_rgba8(100, 100, 120, 120);
    if let Some(r) = tiny_skia::Rect::from_xywh(cx, py, 1.0, ph as f32) {
        pixmap.fill_rect(r, &cl, tiny_skia::Transform::default(), None);
    }

    // Current time (beat-aligned)
    let ct_y = to_y(info.chart_beat).clamp(py as f64, py as f64 + ph) as f32;
    let mut cp = tiny_skia::Paint::default();
    cp.set_color_rgba8(255, 200, 80, 230);
    if let Some(r) = tiny_skia::Rect::from_xywh(px + pad_x, ct_y - 1.0, play_w, 3.0) {
        pixmap.fill_rect(r, &cp, tiny_skia::Transform::default(), None);
    }

    // Notes, skip outside visible range
    let nt_min = min_b - zoom * 0.05;
    let nt_max = max_b + zoom * 0.05;
    for note in &info.notes {
        if note.end_beats < nt_min || note.start_beats > nt_max { continue; }
        let c = match note.kind { 1 => [50, 150, 255], 2 => [100, 200, 255], 3 => [255, 80, 150], 4 => [255, 220, 60], _ => [180; 3] };
        let xn = to_x(note.x);
        let y0 = to_y(note.start_beats).clamp(py as f64, py as f64 + ph) as f32;
        let y1 = to_y(note.end_beats).clamp(py as f64, py as f64 + ph) as f32;
        let bh = (y1 - y0).abs().max(3.0);
        let sc = note.scale.max(0.1);
        let nw = (64.0 * s * sc).max(4.0);
        let nh = (8.0 * s * sc).max(2.0);
        let has_tex = info.has_custom_tex;
        let mut np = tiny_skia::Paint::default();
        if has_tex { np.set_color_rgba8(c[0]/2+128, c[1]/2+128, c[2]/2+128, 220); }
        else { np.set_color_rgba8(c[0], c[1], c[2], 220); }

        if note.kind == 2 && bh > nh * 2.0 {
            if let Some(r) = tiny_skia::Rect::from_xywh(xn - 2.0 * s, y0.min(y1), 4.0 * s, bh) {
                pixmap.fill_rect(r, &np, tiny_skia::Transform::default(), None);
            }
        }
        if let Some(r) = tiny_skia::Rect::from_xywh(xn - nw * 0.5, y0 - nh * 0.5, nw, nh) {
            pixmap.fill_rect(r, &np, tiny_skia::Transform::default(), None);
        }
    }
}

/// Draw a panel definition into the right-side area.
pub fn draw_panel_def(pixmap: &mut tiny_skia::PixmapMut, def: &panels::PanelDef, info: &GameInfo, px: f32, vw: f32, vh: f32, s: f32) {
    if !info.show_overlay { return; }
    if vh < 10.0 { return; }
    let font = get_font();
    let vals = gameinfo_values(info);
    let rows = def.resolve(&vals);
    let bar_h = 48.0 * s;
    let pan_w = def.width * s;
    let bg_h = (vh - bar_h).max(1.0);

    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(12, 12, 14, 200);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, pan_w, bg_h) {
        pixmap.fill_rect(r, &bg, tiny_skia::Transform::default(), None);
    }

    let mut hp = tiny_skia::Paint::default();
    hp.set_color_rgba8(60, 60, 70, 180);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, pan_w, 24.0 * s) {
        pixmap.fill_rect(r, &hp, tiny_skia::Transform::default(), None);
    }

    // Header text
    draw_text_on_pixmap(pixmap, &def.name, px + 6.0 * s, 16.0 * s, 12.0 * s, font);

    let cell_h = 22.0 * s;
    let mut y = 28.0 * s;

    for (label, val, span, kind) in &rows {
        if *kind == 1 {
            // Regular separator line
            y += cell_h * 0.3;
            let mut sp = tiny_skia::Paint::default();
            sp.set_color_rgba8(50, 50, 60, 100);
            if let Some(r) = tiny_skia::Rect::from_xywh(px + 4.0 * s, y, pan_w - 8.0 * s, 1.0) {
                pixmap.fill_rect(r, &sp, tiny_skia::Transform::default(), None);
            }
            y += cell_h * 0.3;
            continue;
        }
        if *kind == 2 {
            // Section split: thicker divider + extra spacing
            y += cell_h * 0.4;
            let mut sp = tiny_skia::Paint::default();
            sp.set_color_rgba8(60, 60, 75, 160);
            if let Some(r) = tiny_skia::Rect::from_xywh(px + 2.0 * s, y, pan_w - 4.0 * s, 2.0 * s) {
                pixmap.fill_rect(r, &sp, tiny_skia::Transform::default(), None);
            }
            y += cell_h * 0.4;
            continue;
        }
        let row_h = cell_h * (*span as f32).max(1.0);
        let mut lp = tiny_skia::Paint::default();
        lp.set_color_rgba8(20, 20, 25, 120);
        if let Some(r) = tiny_skia::Rect::from_xywh(px, y, pan_w, row_h) {
            pixmap.fill_rect(r, &lp, tiny_skia::Transform::default(), None);
        }
        // Label text
        draw_text_on_pixmap(pixmap, label, px + 8.0 * s, y + cell_h * 0.5, 10.0 * s, font);
        // Value text
        let vx = px + pan_w * 0.5 + 4.0 * s;
        draw_text_on_pixmap(pixmap, val, vx, y + cell_h * 0.5, 10.0 * s, font);
        y += row_h;
    }
}

fn draw_text_on_pixmap(pixmap: &mut tiny_skia::PixmapMut, text: &str, x: f32, y: f32, size: f32, font: &fontdue::Font) {
    let w = pixmap.width() as i32;
    let h = pixmap.height() as i32;
    // fontdue baseline: y is the baseline in image space (y down).
    // Bitmap top in image space = baseline - ymin - height.
    let mut pen_x = x;
    for ch in text.chars() {
        let (m, bitmap) = font.rasterize(ch, size);
        let gx0 = pen_x.round() as i32 + m.xmin;
        let gy0 = (y - m.ymin as f32 - m.height as f32).round() as i32;
        for row in 0..m.height {
            let py = gy0 + row as i32;
            if py < 0 || py >= h { continue; }
            for col in 0..m.width {
                let a = bitmap[row * m.width + col];
                if a < 160 { continue; }
                let px = gx0 + col as i32;
                if px < 0 || px >= w { continue; }
                let idx = (py * w + px) as usize * 4;
                let data = pixmap.data_mut();
                data[idx] = 255; data[idx+1] = 255; data[idx+2] = 255; data[idx+3] = 255;
            }
        }
        pen_x += m.advance_width;
    }
}

fn draw_quick_panel(pixmap: &mut tiny_skia::PixmapMut, tool_hover: Option<usize>, selected_tool: usize, hp: [f32; 3], info: &GameInfo, qp_w: f32, vw: f32, vh: f32, s: f32) {
    if !info.show_overlay { return; }
    if vh < 10.0 || vw < 10.0 { return; }
    let font = get_font();
    let bar_h = 48.0 * s;
    let max_hp = hp.iter().cloned().reduce(f32::max).unwrap_or(0.0);
    let bg_extra = max_hp * 100.0 * s;
    let bg_w = qp_w + bg_extra;
    let bg_h = (vh - bar_h).max(1.0);

    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(18, 18, 22, 230);
    if let Some(r) = tiny_skia::Rect::from_xywh(0.0, 0.0, bg_w, bg_h) {
        pixmap.fill_rect(r, &bg, tiny_skia::Transform::default(), None);
    }

    let tools: [(&str, [u8; 3]); 3] = [
        ("Chart",  [100, 200, 255]),
        ("Line",   [100, 220, 100]),
        ("Settings", [180, 180, 190]),
    ];

    let btn_base = 34.0 * s;
    let gap = 6.0 * s;

    for (i, (name, color)) in tools.iter().enumerate() {
        let p = hp[i];
        let y = gap + i as f32 * (btn_base + gap);
        let extra = p * 80.0 * s;
        let bw = btn_base + extra;
        let bx = 4.0 * s; // left-align within panel

        let mut bp = tiny_skia::Paint::default();
        let a = if i == selected_tool { 230 } else { (80.0 + p * 100.0) as u8 };
        bp.set_color_rgba8(color[0], color[1], color[2], a);
        if let Some(rect) = tiny_skia::Rect::from_xywh(bx, y, bw, btn_base) {
            pixmap.fill_rect(rect, &bp, tiny_skia::Transform::default(), None);
        }

        // Label text on button when hovered
        if p > 0.3 {
            let lx = bx + 6.0 * s;
            let ly = y + btn_base * 0.5 + 4.0 * s;
            draw_text_on_pixmap(pixmap, name, lx, ly, 11.0 * s, font);
        }
    }
}

// ── Display data ──

pub struct EventEntry {
    pub layer: usize, pub kind: String, pub index: usize,
    pub start_beats: f64, pub end_beats: f64,
    pub start: f32, pub end: f32, pub easing: i32,
}

pub struct NoteEntry {
    pub index: usize,
    pub kind: u8,        // 1=tap 2=hold 3=flick 4=drag
    pub start_beats: f64,
    pub end_beats: f64,
    pub x: f32,
    pub speed: f32,
    pub scale: f32,      // note size multiplier
    pub texture: String, // custom texture name, empty for default
}

/// Helper: build a value map from GameInfo for panel template resolution.
pub fn gameinfo_values(info: &GameInfo) -> std::collections::HashMap<&str, String> {
    let mut m = std::collections::HashMap::new();
    m.insert("chart_time", format!("{:.3}s", info.chart_time));
    m.insert("chart_beat", format!("{:.2}", info.chart_beat));
    m.insert("fps", format!("{:.0}", info.fps));
    m.insert("combo", format!("{}", info.combo));
    m.insert("score", format!("{:07}", info.score));
    m.insert("note_count", format!("{}", info.note_count));
    m.insert("visible_notes", format!("{}", info.visible_notes));
    m.insert("line_count", format!("{}", info.line_count));
    m.insert("selected_line", format!("{}", info.selected_line));
    m.insert("line_name", info.line_name.clone());
    m.insert("selected_layer", format!("{}", info.selected_layer));
    m.insert("event_count", format!("{}", info.events.len()));
    m.insert("note_count_line", format!("{}", info.notes.len()));
    m.insert("chart_name", info.chart_name.clone());
    m.insert("composer", info.composer.clone());
    m.insert("level", info.level.clone());
    m.insert("difficulty", format!("{:.1}", info.difficulty));
    m.insert("offset", format!("{:.3}", info.offset));
    m.insert("duration", format!("{:.2}s", info.duration));
    m.insert("gui_scale", format!("{:.1}", info.gui_scale));
    m.insert("show_overlay", if info.show_overlay { "ON" } else { "OFF" }.to_string());
    m
}

pub struct GameInfo {
    pub chart_time: f64, pub chart_beat: f64, pub audio_time: f64, pub fps: f64,
    pub combo: u32, pub hits: u32, pub note_count: usize, pub score: u32,
    pub lines: usize, pub visible_notes: usize, pub paused: bool, pub dim: f32,
    pub chart_name: String, pub composer: String, pub level: String, pub difficulty: f32,
    pub offset: f32, pub duration: f64,
    pub show_overlay: bool, pub show_properties: bool, pub show_events: bool,
    pub show_notes: bool, pub events_progress: f32, pub notes_progress: f32,
    pub has_custom_tex: bool, pub full_notes: bool,
    pub selected_line: usize, pub line_name: String, pub line_count: usize,
    pub selected_layer: usize, pub max_layers: usize, pub events: Vec<EventEntry>,
    pub notes: Vec<NoteEntry>,
    pub gui_scale: f32,
    pub selected_tool: usize,
}

fn build_ui<'a>(info: &'a GameInfo, panel: f32) -> Element<'a, (), Theme, Renderer> {
    use iced::widget::{container, text};
    let s = info.gui_scale;

    let header_fmt = format!("Line #{} {}", info.selected_line, info.line_name);
    let tl_w = TL_W * s;

    // Animated panel widths for smooth slide
    let ev_w = TL_W * s * info.events_progress;
    let nt_w = NT_W * s * info.notes_progress;
    let notes_panel: Element<'_, (), Theme, Renderer> = container(text(header_fmt.clone()).size(iced::Pixels(12.0 * s)))
        .width(nt_w).height(Length::Fill)
        .into();
    let events_panel: Element<'_, (), Theme, Renderer> = container(text("Events").size(iced::Pixels(12.0 * s)))
        .width(ev_w).height(Length::Fill)
        .into();

    // Old Iced properties panel (bottom of properties column)
    let header_fmt2 = header_fmt.clone();
    let props_w = PANEL_W * s * panel;
    let old_props: Element<'_, (), Theme, Renderer> = if props_w > 1.0 {
        container(iced::widget::Column::new()
            .push(text("Properties").size(iced::Pixels(14.0 * s)))
            .push(text("---"))
            .push(text(header_fmt2))
            .spacing(2.0 * s)
        ).padding(8.0 * s)
            .style(|_: &Theme| container::Style::default().background(iced::Color::from_rgba(0.12, 0.12, 0.14, 0.92)))
            .width(props_w).into()
    } else { container(iced::widget::Column::new()).width(0.0).into() };
    let props: Element<'_, (), Theme, Renderer> = container(iced::widget::Column::new()
        .push(container(iced::widget::Column::new()).height(Length::Fill))
        .push(old_props)
    ).width(props_w).height(Length::Fill).into();

    let show_ev = container(text("Show Event").size(iced::Pixels(13.0 * s))).padding([6.0 * s, 14.0 * s])
        .style(|_: &Theme| container::Style::default().background(iced::Color::from_rgba(0.25, 0.25, 0.28, 0.85)));
    let show_nt = container(text("Show Notes").size(iced::Pixels(13.0 * s))).padding([6.0 * s, 14.0 * s])
        .style(|_: &Theme| container::Style::default().background(iced::Color::from_rgba(0.25, 0.25, 0.28, 0.85)));
    let bar: Element<'_, (), Theme, Renderer> = container(
        iced::widget::Row::new().push(show_ev).push(show_nt).spacing(8.0 * s),
    ).padding([6.0 * s, 10.0 * s])
        .style(|_: &Theme| container::Style::default().background(iced::Color::from_rgba(0.15, 0.15, 0.17, 0.88)))
        .into();

    let qp_w = QP_W * s;
    let quick_panel: Element<'_, (), Theme, Renderer> = container(iced::widget::Column::new())
        .width(qp_w).height(Length::Fill)
        .into();

    iced::widget::Column::new()
        .push(container(iced::widget::Row::new()
            .push(quick_panel)
            .push(container(iced::widget::Column::new()).width(Length::Fill))
            .push(notes_panel)
            .push(events_panel)
            .push(props)
        ).height(Length::Fill))
        .push(bar)
        .into()
}
