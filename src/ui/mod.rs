use iced::advanced::layout::{Layout, Limits};
use iced::advanced::widget::Tree;
use iced::advanced::{renderer, Renderer as _};
use iced::{Element, Length, Point, Rectangle, Size, Theme};
use iced_tiny_skia::Renderer;
use std::sync::Arc;

pub mod panels;
use phimakor::trace_span;

// Global font fallback chain: the first font that has a glyph for a given
// character wins, so Latin text renders in the primary UI font while CJK /
// symbols fall through to system fonts (Microsoft YaHei, SimHei, PingFang,
// Noto CJK, ...). Loaded lazily once, in order.
static UI_FONTS: std::sync::OnceLock<Vec<fontdue::Font>> = std::sync::OnceLock::new();

const FONT_CANDIDATES: [&str; 18] = [
    // Primary UI font (shipped with the app).
    "res/Exo2.ttf",
    "res.dis/Exo2.ttf",
    // Latin fallbacks.
    "C:\\Windows\\Fonts\\arial.ttf",
    "C:\\Windows\\Fonts\\segoeui.ttf",
    "/System/Library/Fonts/Helvetica.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    // CJK fallbacks (Windows).
    "C:\\Windows\\Fonts\\msyh.ttc",
    "C:\\Windows\\Fonts\\msyhbd.ttc",
    "C:\\Windows\\Fonts\\simhei.ttf",
    "C:\\Windows\\Fonts\\simsun.ttc",
    "C:\\Windows\\Fonts\\Deng.ttf",
    // CJK fallbacks (macOS).
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    // CJK fallbacks (Linux).
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
];

fn load_font_from(path: &str) -> Option<fontdue::Font> {
    std::fs::read(path).ok().and_then(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok())
}

/// All loaded fonts, primary first.
fn get_fonts() -> &'static Vec<fontdue::Font> {
    UI_FONTS.get_or_init(|| {
        FONT_CANDIDATES.iter().filter_map(|p| load_font_from(p)).collect()
    })
}

/// The primary font (or None if nothing loaded).
fn get_font() -> Option<&'static fontdue::Font> {
    get_fonts().first()
}

/// First font in the chain that contains `ch`; falls back to the primary
/// font when nothing has the glyph (renders the font's default glyph).
fn font_for(ch: char) -> Option<&'static fontdue::Font> {
    let fonts = get_fonts();
    fonts.iter().find(|f| f.has_glyph(ch)).or_else(|| fonts.first())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok()
}

// Debug toggles are read once at startup — probing env vars on every frame
// costs a syscall + allocation per call.
static SKIP_GRID: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
static SKIP_CENTER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

#[derive(Debug, Clone, PartialEq)]
pub enum OverlayMessage {
    ToggleEvents,
    SelectLayer(usize),
    ToggleMenu,
    MenuSave,
    MenuQuit,
    ToggleVsync,
}

pub struct IcedOverlay {
    renderer: Renderer,
    tree: Tree,
    theme: Theme,
    pixmap: tiny_skia::Pixmap,          // timeline + overlay working pixmap
    base_pixmap: tiny_skia::Pixmap,     // timeline content WITHOUT playhead (per-frame redraw source)
    iced_cache: tiny_skia::Pixmap,      // cached Iced UI, same size
    iced_tex: wgpu::Texture,            // GPU texture for Iced cache
    iced_bg: wgpu::BindGroup,           // bind group for Iced cache
    timeline_tex: wgpu::Texture,        // GPU texture for per-frame overlay
    timeline_bg: wgpu::BindGroup,       // bind group for per-frame overlay
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
    tool_hover_progress: [f32; 4],
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
    pub drag_note: Option<(usize, f64, f32, f32)>, // (note_index, start_beat, mouse_x_start, mouse_y_start)
    pub drag_updated: Option<(usize, f64, f32)>, // (note_index, new_beat, new_x)
    ctx_pos: Option<(f32, f32)>,
    ctx_progress: f32,
    pub     mouse_beat: f64,
    notes_cache: Arc<Vec<NoteEntry>>,
    last_drawn_beat: f64,
    show_menu: bool,
}

const KIND_COLORS: [(&str, [u8; 3]); 5] = [
    ("Alpha", [70, 130, 255]), ("MoveX", [70, 200, 100]),
    ("MoveY", [255, 170, 60]), ("Rotate", [255, 220, 60]), ("Speed", [255, 80, 80]),
];

impl IcedOverlay {
    pub fn new(device: &wgpu::Device, tex_bgl: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler, w: u32, h: u32) -> Self {
        let (texture, bind_group) = Self::make_texture(device, tex_bgl, sampler, w.max(1), h.max(1), "iced");
        let (iced_tex, iced_bg) = Self::make_texture(device, tex_bgl, sampler, w.max(1), h.max(1), "iced-cache");
        let (timeline_tex, timeline_bg) = Self::make_texture(device, tex_bgl, sampler, w.max(1), h.max(1), "timeline");
        let renderer = Renderer::new(iced::Font::default(), iced::Pixels(14.0));
        let root: Element<'_, (), Theme, Renderer> = iced::widget::Column::new().into();
        let tree = Tree::new(&root);
        Self { renderer, tree, theme: Theme::Dark,
            pixmap: tiny_skia::Pixmap::new(w.max(1), h.max(1)).unwrap(),
            base_pixmap: tiny_skia::Pixmap::new(w.max(1), h.max(1)).unwrap(),
            iced_cache: tiny_skia::Pixmap::new(w.max(1), h.max(1)).unwrap(),
            texture, bind_group, iced_tex, iced_bg, timeline_tex, timeline_bg,
            clip_mask: tiny_skia::Mask::new(w.max(1), h.max(1)).unwrap(),
            w: w.max(1), h: h.max(1), panel_progress: 0.0, events_progress: 0.0,
            notes_progress: 0.0, mouse_pos: None, show_overlay: true, tl_visible: false,
            tool_hover: None, selected_tool: 0, tool_hover_progress: [0.0; 4],
            panel_defs: Vec::new(), messages: Vec::new(), timeline_click: None,
            layer_click: None, tl_scroll: 0.0, tl_zoom: 8.0, gui_scale: 1.0,
            select_start: None, select_end: None, selecting: false, seek_dragging: false,
            drag_note: None, drag_updated: None, ctx_pos: None, ctx_progress: 0.0,
            mouse_beat: 0.0, notes_cache: Arc::new(Vec::new()), last_drawn_beat: 0.0, show_menu: false,
        }
    }

    fn make_texture(device: &wgpu::Device, tex_bgl: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler, w: u32, h: u32, label: &str) -> (wgpu::Texture, wgpu::BindGroup) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(&format!("{label}-bg")), layout: tex_bgl,
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
        self.base_pixmap = tiny_skia::Pixmap::new(w, h).unwrap();
        self.iced_cache = tiny_skia::Pixmap::new(w, h).unwrap();
        self.clip_mask = tiny_skia::Mask::new(w, h).unwrap();
        (self.texture, self.bind_group) = Self::make_texture(device, tex_bgl, sampler, w, h, "overlay");
        (self.iced_tex, self.iced_bg) = Self::make_texture(device, tex_bgl, sampler, w, h, "iced-cache");
        (self.timeline_tex, self.timeline_bg) = Self::make_texture(device, tex_bgl, sampler, w, h, "timeline");
    }

    pub fn bind_group(&self) -> &wgpu::BindGroup { &self.timeline_bg }
    pub fn iced_bind_group(&self) -> &wgpu::BindGroup { &self.iced_bg }
    pub fn props_progress(&self) -> f32 { self.panel_progress }
    pub fn handle_cursor(&mut self, x: f64, y: f64) {
        let _s = trace_span!("handle_cursor");
        self.mouse_pos = Some((x as f32, y as f32));
        if self.selecting {
            self.select_end = Some((x as f32, y as f32));
        }
        self.tool_hover = None;
        if self.tl_visible {
            self.mouse_beat = self.y_to_beat(y as f32, 0.0);
        }
        // Track note drag
        if self.drag_note.is_some() {
            let beat = self.y_to_beat(y as f32, 0.0);
            let px = self.panel_x(0.0, NT_W, self.notes_progress);
            let rel_x = ((x as f32 - px) / (NT_W * self.gui_scale) * 2.0 - 1.0) * 675.0;
            if let Some((ni, ..)) = self.drag_note {
                self.drag_updated = Some((ni, beat, rel_x));
            }
        }
        let s = self.gui_scale;
        let btn_base = 34.0 * s;
        let gap = 6.0 * s;
        let mx = x as f32;
        let my = y as f32;
        // Match draw_quick_panel: left-aligned at 4*s, hover progress for width
        let max_hp = self.tool_hover_progress.iter().cloned().reduce(f32::max).unwrap_or(0.0);
        let bg_extra = max_hp * 100.0 * s;
        let bg_w = QP_W * s + bg_extra;
        for i in 0..4 {
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
        let _s = trace_span!("handle_click");
        let s = self.gui_scale;
        let Some((mx, my)) = self.mouse_pos else { return };
        // Left click always dismisses context menu
        self.ctx_pos = None;
        // Overlay menu (modal): clicking an item activates it, clicking
        // outside closes it. Geometry matches draw_menu.
        if self.show_menu {
            let pw = 240.0 * s;
            let ph = 150.0 * s;
            let px = (self.w as f32 - pw) * 0.5;
            let py = (self.h as f32 - ph) * 0.5;
            let in_panel = mx >= px && mx <= px + pw && my >= py && my <= py + ph;
            if pressed {
                if !in_panel { self.messages.push(OverlayMessage::ToggleMenu); }
                return;
            }
            if in_panel {
                let item_h = 32.0 * s;
                let iy = py + 50.0 * s;
                match ((my - iy) / item_h) as usize {
                    0 => self.messages.push(OverlayMessage::MenuSave),
                    1 => self.messages.push(OverlayMessage::MenuQuit),
                    _ => {}
                }
            } else {
                self.messages.push(OverlayMessage::ToggleMenu);
            }
            return;
        }
        // Seek bar: press starts drag; release always stops (before any other handler)
        if pressed && self.is_over_seekbar() { self.seek_dragging = true; }
        if !pressed { self.seek_dragging = false; }
        if self.seek_dragging { return; }
        // Release: handle bottom bar buttons and selection end
        if !pressed {
            if self.selecting { self.selecting = false; }
            if my >= self.h as f32 - 48.0 * s && my <= self.h as f32 && self.show_overlay {
                let btn_w = 90.0 * s;
                let right = self.w as f32 - 10.0 * s;
                let bx = |i: usize| right - (3 - i) as f32 * (btn_w + 6.0 * s);
                if mx >= bx(0) && mx < bx(0) + btn_w { self.messages.push(OverlayMessage::ToggleEvents); return; }
                if mx >= bx(1) && mx < bx(1) + btn_w { self.messages.push(OverlayMessage::SelectLayer(666)); return; }
                if mx >= bx(2) && mx < bx(2) + btn_w { self.messages.push(OverlayMessage::ToggleMenu); return; }
                return;
            }
        }
        // Note drag: press on notes panel → start drag
        if pressed && self.tl_visible && self.is_over_notes(0.0) && !ctrl {
            let beat = self.y_to_beat(my, 0.0);
            if let Some(ni) = self.find_nearest_note(beat, mx) {
                self.drag_note = Some((ni, beat, mx, my));
                return;
            }
        }
        if !pressed && self.drag_note.is_some() {
            // Release: store updated position
            let beat = self.y_to_beat(my, 0.0);
            let px = self.panel_x(0.0, NT_W, self.notes_progress);
            let rel_x = ((mx - px) / (NT_W * self.gui_scale) * 2.0 - 1.0) * 675.0;
            if let Some((ni, ..)) = self.drag_note {
                self.drag_updated = Some((ni, beat, rel_x));
            }
            self.drag_note = None;
            return;
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
            // Settings panel: toggle vsync on click
            if !pressed && self.selected_tool == 2 && self.panel_progress > 0.5 {
                let pp = self.panel_progress;
                let pan_w = PANEL_W * s;
                let pan_x = self.w as f32 - pp * pan_w;
                if mx >= pan_x && mx <= pan_x + pan_w {
                    let cell_h = 22.0 * s;
                    let vsync_y = 28.0 * s + cell_h * (2.0 + 2.0 + 1.0 + 1.0 + 1.6 + 2.0 + 1.6);
                    if my >= vsync_y && my <= vsync_y + cell_h * 2.0 {
                        self.messages.push(OverlayMessage::ToggleVsync); return;
                    }
                }
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

    fn find_nearest_note(&self, beat: f64, mx: f32) -> Option<usize> {
        let notes = self.notes_cache.as_slice();
        if notes.is_empty() { return None; }
        let px = self.panel_x(0.0, NT_W, self.notes_progress);
        let rel_x = ((mx - px) / (NT_W * self.gui_scale) * 2.0 - 1.0) * 675.0;
        notes.iter().enumerate()
            .filter(|(_, n)| (n.start_beats - beat).abs() < 1.0 && (n.x - rel_x).abs() < 150.0)
            .min_by(|(_, a), (_, b)| {
                let da = (a.start_beats - beat).abs() + (a.x - rel_x).abs() as f64 * 0.01;
                let db = (b.start_beats - beat).abs() + (b.x - rel_x).abs() as f64 * 0.01;
                da.partial_cmp(&db).unwrap()
            })
            .map(|(i, _)| i)
    }

    /// Convert a Y position to beat value on the timeline (whichever panel is visible).
    fn y_to_beat(&self, my: f32, props: f32) -> f64 {
        let s = self.gui_scale;
        // Find which panel the mouse is on
        let (panel_px, panel_w, _progress) = if self.is_over_events(props) {
            (self.panel_x(props, TL_W, self.events_progress), TL_W, self.events_progress)
        } else if self.is_over_notes(props) {
            (self.panel_x(props, NT_W, self.notes_progress), NT_W, self.notes_progress)
        } else { return self.tl_scroll as f64 + self.tl_zoom as f64 * 0.5; };

        let py = HEADER_H * s + 4.0 * s;
        let ph = (self.h as f32 - 56.0 * s - py) as f64;
        if ph <= 0.0 { return self.tl_scroll as f64; }
        let ratio = 1.0 - ((my - py) / ph as f32).clamp(0.0, 1.0) as f64;
        self.tl_scroll as f64 + ratio * self.tl_zoom as f64
    }

    pub fn is_over_seekbar(&self) -> bool {
        let s = self.gui_scale;
        let Some((mx, my)) = self.mouse_pos else { return false };
        let pp = self.panel_progress;
        let props_x = self.w as f32 - pp * PANEL_W * s;
        let sb_y = self.h as f32 - 56.0 * s;
        let sb_h = 12.0 * s;
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
        let _s = trace_span!("render_iced");
        self.panel_progress = self.approach(self.panel_progress, if info.show_properties { 1.0 } else { 0.0 });
        self.events_progress = self.approach(self.events_progress, if info.show_events { 1.0 } else { 0.0 });
        self.notes_progress = self.approach(self.notes_progress, if info.show_notes { 1.0 } else { 0.0 });
        self.show_overlay = info.show_overlay;
        self.show_menu = info.show_menu;
        self.tl_visible = info.show_events || info.show_notes;
        self.gui_scale = info.gui_scale;
        self.notes_cache = info.notes.clone();
        // Render Iced UI to cache pixmap, upload to dedicated GPU texture
        self.iced_cache.fill(tiny_skia::Color::TRANSPARENT);
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
        self.renderer.draw(&mut self.iced_cache.as_mut(), &mut self.clip_mask, &viewport,
            &[Rectangle::new(Point::ORIGIN, logical)], iced::Color::TRANSPARENT);
        // Upload Iced cache to GPU — capture data before mutable borrow
        let iced_data = self.iced_cache.data().to_vec();
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &self.iced_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &iced_data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
            wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
        );
        // Clear working pixmap and draw timeline → overlay texture
        self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
        self.upload_timeline_to(queue, info)
    }

    pub fn render_progress(&self) -> (f32, f32) { (self.events_progress, self.notes_progress) }

    /// Render splash screen (chart picker / settings) into the overlay texture.
    /// `gui_scale` is applied to the splash layout each frame, so live
    /// settings changes take effect immediately (and hit-testing stays in
    /// sync with what's drawn).
    pub fn render_splash(&mut self, queue: &wgpu::Queue, data: &SplashData, gui_scale: f32, settings: Option<&SettingsData>) {
        let _s = trace_span!("render_splash");
        self.gui_scale = gui_scale;
        self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
        self.iced_cache.fill(tiny_skia::Color::TRANSPARENT);
        let vw = self.w as f32;
        let vh = self.h as f32;
        draw_splash(&mut self.pixmap.as_mut(), data, vw, vh, gui_scale, settings);
        self.iced_cache.data_mut().copy_from_slice(self.pixmap.data());
        // Upload to both textures
        for (tex, data) in [(&self.iced_tex, self.iced_cache.data()), (&self.timeline_tex, self.pixmap.data())] {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
                wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
            );
        }
    }

    pub fn set_panels(&mut self, panels: Vec<panels::PanelDef>) {
        self.panel_defs = panels;
    }

    /// Lightweight playhead-only redraw: draw current-time lines on the
    /// existing pixmap and upload. No Iced rebuild, no full clear.
    pub fn redraw_playhead(&mut self, queue: &wgpu::Queue, info: &GameInfo) {
        let _s = trace_span!("redraw_playhead");
        self.tl_visible = info.show_events;
        let s = self.gui_scale;
        let vw = self.w as f32;
        let vh = self.h as f32;
        let ep = self.events_progress;
        let np = self.notes_progress;
        let pp = self.panel_progress;
        let pan_w = PANEL_W * s;
        let props_x = vw - pp * pan_w;
        let events_x = props_x - ep * TL_W * s;
        let notes_x = events_x - np * NT_W * s;
        if self.tl_visible {
            self.tl_scroll = (info.chart_beat as f32 - self.tl_zoom * 0.1).max(0.0);
            let (scroll, zoom) = (self.tl_scroll as f64, self.tl_zoom as f64);
            let (min_b, max_b) = (scroll, scroll + zoom);
            let head_h = HEADER_H * s;
            let py = head_h + 4.0 * s;
            let ph = (vh - 56.0 * s - py) as f64;
            let to_y = |b: f64| py as f64 + ph - (b - min_b) / zoom * ph;
            let ct_y = to_y(info.chart_beat).clamp(py as f64, py as f64 + ph) as f32;
            let mut cp = tiny_skia::Paint::default();
            cp.set_color_rgba8(255, 200, 80, 230);
            // Events timeline playhead
            if info.show_events {
                let tl_w = TL_W * s;
                if let Some(r) = tiny_skia::Rect::from_xywh(events_x + 2.0 * s, ct_y - 1.0, tl_w - 4.0 * s, 3.0) {
                    fill_rect_clipped(&mut self.pixmap.as_mut(), r, &cp);
                }
            }
            // Notes timeline playhead
            if info.show_notes {
                let pad_x = 12.0 * s;
                let play_w = NT_W * s - pad_x * 2.0;
                if let Some(r) = tiny_skia::Rect::from_xywh(notes_x + pad_x, ct_y - 1.0, play_w, 3.0) {
                    fill_rect_clipped(&mut self.pixmap.as_mut(), r, &cp);
                }
            }
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &self.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            self.pixmap.data(),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
            wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
        );
    }

    /// Timeline-only per-frame redraw: restore the playhead-free base pixmap,
    /// draw the current-time lines, upload. No tiny_skia content rebuild.
    pub fn redraw_timeline(&mut self, queue: &wgpu::Queue, info: &GameInfo) {
        let _s = trace_span!("redraw_timeline");
        self.show_overlay = info.show_overlay;
        self.show_menu = info.show_menu;
        self.tl_visible = info.show_events || info.show_notes;
        self.gui_scale = info.gui_scale;
        let prev_anim = (
            self.panel_progress,
            self.events_progress,
            self.notes_progress,
            self.ctx_progress,
            self.tool_hover_progress,
        );
        self.panel_progress = self.approach(self.panel_progress, if info.show_properties { 1.0 } else { 0.0 });
        self.events_progress = self.approach(self.events_progress, if info.show_events { 1.0 } else { 0.0 });
        self.notes_progress = self.approach(self.notes_progress, if info.show_notes { 1.0 } else { 0.0 });
        self.animate_all();
        // During playback the timeline scrolls and the seek bar advances every
        // frame — the base pixmap is stale. Fall back to a full redraw then;
        // the fast path (base copy + playhead) is only valid while static.
        // Also include the tool-hover / context-menu animations: their values
        // keep moving while the panels don't, and the fast path would freeze
        // the hover/menu at its last-synced state (stuck highlight residue).
        let playing = (info.chart_beat - self.last_drawn_beat).abs() > 1e-4;
        let anim_moving = prev_anim != (
            self.panel_progress,
            self.events_progress,
            self.notes_progress,
            self.ctx_progress,
            self.tool_hover_progress,
        );
        if playing || anim_moving {
            self.upload_timeline_to(queue, info);
            self.last_drawn_beat = info.chart_beat;
            return;
        }
        // Restore base content (no playhead) — memcpy, much cheaper than redraw.
        self.pixmap.data_mut().copy_from_slice(self.base_pixmap.data());
        self.draw_playhead(info);
        self.last_drawn_beat = info.chart_beat;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &self.timeline_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            self.pixmap.data(),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
            wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
        );
    }

    /// Draw the current-time playhead lines on `pixmap` (assumes base content).
    fn draw_playhead(&mut self, info: &GameInfo) {
        let s = self.gui_scale;
        let vw = self.w as f32;
        let vh = self.h as f32;
        let ep = self.events_progress;
        let np = self.notes_progress;
        let pp = self.panel_progress;
        let pan_w = PANEL_W * s;
        let props_x = vw - pp * pan_w;
        let events_x = props_x - ep * TL_W * s;
        let notes_x = events_x - np * NT_W * s;
        if self.tl_visible {
            self.tl_scroll = (info.chart_beat as f32 - self.tl_zoom * 0.1).max(0.0);
            let (scroll, zoom) = (self.tl_scroll as f64, self.tl_zoom as f64);
            let (min_b, max_b) = (scroll, scroll + zoom);
            let head_h = HEADER_H * s;
            let py = head_h + 4.0 * s;
            let ph = (vh - 56.0 * s - py) as f64;
            let to_y = |b: f64| py as f64 + ph - (b - min_b) / zoom * ph;
            let ct_y = to_y(info.chart_beat).clamp(py as f64, py as f64 + ph) as f32;
            if info.show_events {
                let tl_w = TL_W * s;
                hline(&mut self.pixmap.as_mut(), ct_y - 1.0, events_x + 2.0 * s, events_x + tl_w - 2.0 * s, [255, 200, 80, 230]);
                hline(&mut self.pixmap.as_mut(), ct_y + 1.0, events_x + 2.0 * s, events_x + tl_w - 2.0 * s, [255, 200, 80, 230]);
            }
            if info.show_notes {
                let pad_x = 12.0 * s;
                let play_w = NT_W * s - pad_x * 2.0;
                hline(&mut self.pixmap.as_mut(), ct_y - 1.0, notes_x + pad_x, notes_x + pad_x + play_w, [255, 200, 80, 230]);
                hline(&mut self.pixmap.as_mut(), ct_y + 1.0, notes_x + pad_x, notes_x + pad_x + play_w, [255, 200, 80, 230]);
            }
        }
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
        for i in 0..4 {
            let target = if self.tool_hover == Some(i) { 1.0 } else { 0.0 };
            let d = target - self.tool_hover_progress[i];
            if d.abs() > 0.005 {
                self.tool_hover_progress[i] += d * 0.15;
            } else {
                self.tool_hover_progress[i] = target;
            }
        }
    }

    fn upload_iced(&mut self, queue: &wgpu::Queue) {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &self.iced_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            self.iced_cache.data(),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
            wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
        );
    }

    fn upload_timeline_to(&mut self, queue: &wgpu::Queue, info: &GameInfo) {
        let _s = trace_span!("upload_timeline");
        // MUST clear first: this runs both from redraw_iced (pre-cleared) and
        // directly from redraw_timeline's playing/animation path — without the
        // fill, the previous frame's playhead lines/notes/seek fill would
        // remain and ghost under the new content.
        self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
        self.animate_all();
        self.notes_cache = info.notes.clone();
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
            let _s2 = trace_span!("tl_notes_draw");
            if info.show_notes {
                draw_notes_timeline(&mut self.pixmap.as_mut(), self.tl_scroll, self.tl_zoom, info, notes_x, vh, s);
            }
            drop(_s2);
            let _s3 = trace_span!("tl_events_draw");
            if info.show_events {
                draw_5col_timeline(&mut self.pixmap.as_mut(), self.tl_scroll, self.tl_zoom, info, events_x, vh, s);
            }
            drop(_s3);
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
                        fill_rect_clipped(&mut pm, r, &sel);
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
                    fill_rect_clipped(&mut pm, r, &mp);
                }
            }
            // Seek bar: thin strip above bottom buttons, full playfield width
            if info.show_overlay {
                let sb_h = 12.0 * s;
                let sb_y = vh - 56.0 * s;
                let sb_x = qp_w;
                let sb_w = (props_x - sb_x).max(20.0);
                let mut sbg = tiny_skia::Paint::default();
                sbg.set_color_rgba8(40, 45, 55, 200);
                if let Some(r) = tiny_skia::Rect::from_xywh(sb_x, sb_y, sb_w, sb_h) {
                    fill_rect_clipped(&mut pm, r, &sbg);
                }
                let prog = (info.chart_time / info.duration.max(0.01)) as f32;
                if prog > 0.01 {
                    let mut fp = tiny_skia::Paint::default();
                    fp.set_color_rgba8(100, 180, 255, 200);
                    if let Some(r) = tiny_skia::Rect::from_xywh(sb_x + 1.0 * s, sb_y + 1.0 * s, (sb_w - 2.0 * s) * prog.min(1.0), (sb_h - 2.0 * s).max(1.0)) {
                        fill_rect_clipped(&mut pm, r, &fp);
                    }
                }
            }
        }

        // Render panel definition matching selected tool
        if info.show_properties && pp > 0.01 {
            if self.selected_tool == 3 {
                draw_effects_panel(&mut self.pixmap.as_mut(), info, props_x, vw, vh, s);
            } else {
                let idx = self.selected_tool.min(self.panel_defs.len().max(1) - 1);
                if let Some(def) = self.panel_defs.get(idx) {
                    draw_panel_def(&mut self.pixmap.as_mut(), def, info, props_x, vw, vh, s);
                }
            }
        }
        // Floating menu (topmost)
        if info.show_menu {
            draw_menu(&mut self.pixmap.as_mut(), vw, vh, s);
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &self.timeline_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            self.pixmap.data(),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
            wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
        );
        // Sync the playhead-free base for per-frame redraw_timeline.
        self.base_pixmap.data_mut().copy_from_slice(self.pixmap.data());
    }
}

/// Draw splash screen (chart picker) when no chart is loaded.
/// Editor settings surfaced on the splash screen. Persisted to `config.json`.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct SettingsData {
    pub vsync: bool,
    pub gui_scale: f32,
    pub fullscreen: bool,
}

impl Default for SettingsData {
    fn default() -> Self {
        Self { vsync: true, gui_scale: 1.0, fullscreen: false }
    }
}

// ── Splash screen: chart library picker ──

/// One chart in the library, with metadata for the splash list / preview.
#[derive(Clone)]
pub struct ChartEntry {
    pub name: String,
    pub path: std::path::PathBuf,
    pub composer: String,
    pub charter: String,
    pub level: String,
    pub difficulty: f32,
    /// Last-modified time (unix seconds), for "Newest" sorting.
    pub modified: u64,
    /// Downscaled illustration thumbnail (fits a 200px box).
    pub thumb: Option<image::RgbaImage>,
}

/// What the splash cursor is currently over. Indexes are positions in the
/// *filtered* chart list (same convention as [`SplashData::filtered`]).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SplashHover {
    None,
    Chart(usize),
    Delete(usize),
    Search,
    Sort,
    Refresh,
    OpenFolder,
    Settings,
    // Settings page rows
    Vsync,
    Fullscreen,
    ScaleRow,
    ScaleMinus,
    ScalePlus,
    Back,
}

/// Everything the splash screen needs to draw a frame.
pub struct SplashData<'a> {
    pub charts: &'a [ChartEntry],
    pub filtered: &'a [usize],
    pub filter: &'a str,
    pub hover: SplashHover,
    pub sel: Option<usize>,
    pub sort: u8,
    pub lib_path: &'a str,
    /// List scroll offset in px (clamped to the content height).
    pub scroll: f32,
}

/// Filter (case-insensitive name match) and sort (0 = name, 1 = newest)
/// the chart list, returning indices into `charts`.
pub fn filter_charts(charts: &[ChartEntry], query: &str, sort: u8) -> Vec<usize> {
    let q = query.to_lowercase();
    let mut idx: Vec<usize> = charts.iter().enumerate()
        .filter(|(_, c)| q.is_empty() || c.name.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect();
    match sort {
        1 => idx.sort_by(|&a, &b| {
            charts[b].modified.cmp(&charts[a].modified)
                .then_with(|| charts[a].name.cmp(&charts[b].name))
        }),
        _ => idx.sort_by(|&a, &b| charts[a].name.cmp(&charts[b].name)),
    }
    idx
}

// ── Splash layout constants (draw + hit-test share these) ──

const SPLASH_M: f32 = 40.0;          // left/right margin
const SPLASH_BAR_H: f32 = 48.0;      // bottom bar height
const SPLASH_CTRL_Y: f32 = 56.0;     // search box top
const SPLASH_CTRL_H: f32 = 32.0;     // search box height
const SPLASH_LIST_TOP: f32 = 96.0;   // first chart row top
const SPLASH_ROW_H: f32 = 36.0;      // row height
const SPLASH_ROW_GAP: f32 = 4.0;     // row spacing
const SPLASH_DETAIL_W: f32 = 280.0;  // preview panel width
const SPLASH_DETAIL_GAP: f32 = 24.0; // list ↔ preview gap
const SPLASH_BTN_H: f32 = 28.0;      // bottom buttons
const SPLASH_HDR_BTN_Y: f32 = 10.0;  // header buttons top
const SPLASH_HDR_BTN_H: f32 = 28.0;  // header buttons height

pub fn splash_detail_x(vw: f32, s: f32) -> f32 { vw - SPLASH_M * s - SPLASH_DETAIL_W * s }
pub fn splash_list_right(vw: f32, s: f32) -> f32 { splash_detail_x(vw, s) - SPLASH_DETAIL_GAP * s }

/// Resolve the splash cursor to a [`SplashHover`]. `filtered_len` is the
/// number of currently visible chart rows (used to clamp row indices),
/// `scroll` the list scroll offset in px.
pub fn splash_hit_test(mx: f32, my: f32, vw: f32, vh: f32, s: f32, filtered_len: usize, settings: bool, scroll: f32) -> SplashHover {
    if settings {
        let row_h = 34.0 * s;
        let lx = 60.0 * s;
        let rx = vw - 60.0 * s;
        if mx < lx || mx > rx { return SplashHover::None; }
        let hit = |my: f32, y: f32| my >= y && my <= y + row_h;
        let mut y = 90.0 * s;
        if hit(my, y) { return SplashHover::Vsync; }
        y += row_h + 6.0 * s;
        if hit(my, y) { return SplashHover::Fullscreen; }
        y += row_h + 6.0 * s;
        if hit(my, y) { return SplashHover::ScaleRow; }
        y += row_h + 6.0 * s;
        let bw = 60.0 * s;
        if mx >= lx && mx <= lx + bw && hit(my, y) { return SplashHover::ScaleMinus; }
        if mx >= lx + bw + 8.0 * s && mx <= lx + bw * 2.0 + 8.0 * s && hit(my, y) { return SplashHover::ScalePlus; }
        let by = vh - SPLASH_BAR_H * s - row_h - 10.0 * s;
        if hit(my, by) && mx <= lx + 100.0 * s { return SplashHover::Back; }
        return SplashHover::None;
    }
    // Header buttons (top-right: sort / refresh / open folder)
    let ty = SPLASH_HDR_BTN_Y * s;
    if my >= ty && my <= ty + SPLASH_HDR_BTN_H * s {
        let right = vw - SPLASH_M * s;
        let x_dir = right - 104.0 * s;
        let x_ref = x_dir - 8.0 * s - 88.0 * s;
        let x_sort = x_ref - 8.0 * s - 100.0 * s;
        if mx >= x_sort && mx <= x_sort + 100.0 * s { return SplashHover::Sort; }
        if mx >= x_ref && mx <= x_ref + 88.0 * s { return SplashHover::Refresh; }
        if mx >= x_dir && mx <= x_dir + 104.0 * s { return SplashHover::OpenFolder; }
    }
    // Search box
    let lr = splash_list_right(vw, s);
    if mx >= SPLASH_M * s && mx <= lr && my >= SPLASH_CTRL_Y * s && my <= (SPLASH_CTRL_Y + SPLASH_CTRL_H) * s {
        return SplashHover::Search;
    }
    // Chart rows (delete button at the right edge). The list scrolls: row
    // index maps through the scroll offset, rows above/below the viewport
    // are unreachable.
    if mx >= SPLASH_M * s && mx <= lr && my >= SPLASH_LIST_TOP * s && my < vh - 96.0 * s {
        let ri = ((my - (SPLASH_LIST_TOP * s - scroll)) / ((SPLASH_ROW_H + SPLASH_ROW_GAP) * s)) as usize;
        if ri < filtered_len {
            if mx >= lr - 26.0 * s && mx <= lr - 8.0 * s { return SplashHover::Delete(ri); }
            return SplashHover::Chart(ri);
        }
    }
    // Settings button (bottom-left)
    let by = vh - SPLASH_BAR_H * s - SPLASH_BTN_H * s - 10.0 * s;
    if mx >= SPLASH_M * s && mx <= 160.0 * s && my >= by && my <= by + SPLASH_BTN_H * s { return SplashHover::Settings; }
    SplashHover::None
}

/// Render the splash screen (chart picker or settings page) into the overlay
/// texture. `settings == Some(_)` shows the settings page (with a Back button).
pub fn draw_splash(pm: &mut tiny_skia::PixmapMut, data: &SplashData, vw: f32, vh: f32, s: f32, settings: Option<&SettingsData>) {
    let _s = trace_span!("draw_splash");
    let font = get_font();
    // Background
    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(13, 13, 18, 255);
    if let Some(r) = tiny_skia::Rect::from_xywh(0.0, 0.0, vw, vh) {
        fill_rect_clipped(pm, r, &bg);
    }
    // Header buttons
    if settings.is_none() {
        draw_splash_header_buttons(pm, data.hover, data.sort, vw, s);
    }
    if let Some(font) = font {
        draw_text_on_pixmap(pm, "PhiMakor", SPLASH_M * s, 32.0 * s, 20.0 * s, font);
    }
    if let Some(cfg) = settings {
        if let Some(font) = font {
            draw_text_on_pixmap(pm, "Settings", SPLASH_M * s, 50.0 * s, 14.0 * s, font);
        }
        draw_splash_settings(pm, cfg, data.hover, vw, vh, s);
        return;
    }
    let detail_x = splash_detail_x(vw, s);
    let list_right = splash_list_right(vw, s);
    // Search box
    let sx = SPLASH_M * s;
    let sw = (list_right - sx).max(20.0);
    let mut sbp = tiny_skia::Paint::default();
    let searching = !data.filter.is_empty();
    sbp.set_color_rgba8(if searching { 38 } else { 26 }, if searching { 44 } else { 29 }, if searching { 58 } else { 34 }, 230);
    if let Some(r) = tiny_skia::Rect::from_xywh(sx, SPLASH_CTRL_Y * s, sw, SPLASH_CTRL_H * s) {
        fill_rect_clipped(pm, r, &sbp);
    }
    if let Some(font) = font {
        if searching {
            let txt = data.filter;
            draw_text_on_pixmap(pm, txt, sx + 10.0 * s, SPLASH_CTRL_Y * s + SPLASH_CTRL_H * 0.5 * s + 4.0 * s, 13.0 * s, font);
            let tw = text_width(txt, 13.0 * s);
            draw_text_on_pixmap(pm, "|", sx + 10.0 * s + tw + 2.0 * s, SPLASH_CTRL_Y * s + SPLASH_CTRL_H * 0.5 * s + 4.0 * s, 13.0 * s, font);
        } else {
            draw_text_c(pm, "Search charts…", sx + 10.0 * s, SPLASH_CTRL_Y * s + SPLASH_CTRL_H * 0.5 * s + 4.0 * s, 13.0 * s, font, [130, 130, 140]);
        }
    }
    // Bottom bar: settings button
    let by = vh - SPLASH_BAR_H * s - SPLASH_BTN_H * s - 10.0 * s;
    let is_set = data.hover == SplashHover::Settings;
    let mut bp = tiny_skia::Paint::default();
    bp.set_color_rgba8(if is_set { 60 } else { 40 }, if is_set { 65 } else { 45 }, if is_set { 75 } else { 52 }, 220);
    if let Some(r) = tiny_skia::Rect::from_xywh(SPLASH_M * s, by, 120.0 * s, SPLASH_BTN_H * s) {
        fill_rect_clipped(pm, r, &bp);
    }
    if let Some(font) = font {
        draw_text_on_pixmap(pm, "Settings", 48.0 * s, by + SPLASH_BTN_H * 0.5 * s + 4.0 * s, 13.0 * s, font);
        // Library path + chart count
        draw_text_c(pm, data.lib_path, SPLASH_M * s, vh - 18.0 * s, 11.0 * s, font, [120, 120, 130]);
        let count = format!("{} chart(s)", data.charts.len());
        let cw = text_width(&count, 11.0 * s);
        draw_text_c(pm, &count, vw - SPLASH_M * s - cw, vh - 18.0 * s, 11.0 * s, font, [150, 150, 160]);
    }
    // Empty library: big hint instead of the list.
    if data.charts.is_empty() {
        if let Some(font) = font {
            let msg = "No charts found in the library";
            let msg2 = "Drop a chart folder or .zip file anywhere in this window";
            let msg3 = data.lib_path;
            let cy = vh * 0.42;
            draw_text_on_pixmap(pm, msg, (vw - text_width(msg, 16.0 * s)) * 0.5, cy, 16.0 * s, font);
            draw_text_c(pm, msg2, (vw - text_width(msg2, 12.0 * s)) * 0.5, cy + 24.0 * s, 12.0 * s, font, [140, 140, 152]);
            draw_text_c(pm, msg3, (vw - text_width(msg3, 11.0 * s)) * 0.5, cy + 44.0 * s, 11.0 * s, font, [110, 110, 122]);
        }
        return;
    }
    // Chart rows (scrollable). Rows above the viewport are skipped, rows
    // below are cut off by `list_bottom`; the scrollbar appears when the
    // content overflows.
    let list_bottom = vh - 96.0 * s;
    let list_top = SPLASH_LIST_TOP * s;
    let row_step = (SPLASH_ROW_H + SPLASH_ROW_GAP) * s;
    let view_h = (list_bottom - list_top).max(1.0);
    let content_h = data.filtered.len() as f32 * row_step;
    let max_scroll = (content_h - view_h).max(0.0);
    let scroll = data.scroll.clamp(0.0, max_scroll);
    let mut y = list_top - scroll;
    for (ri, &ci) in data.filtered.iter().enumerate() {
        if y >= list_bottom { break; }
        if y + SPLASH_ROW_H * s > list_top {
            draw_splash_row(pm, &data.charts[ci], data.hover, data.sel, ri, list_right, y, s);
        }
        y += row_step;
    }
    // Scrollbar
    if max_scroll > 0.0 {
        let sw = 3.0 * s;
        let sx = list_right - 2.0 * s;
        let mut track = tiny_skia::Paint::default();
        track.set_color_rgba8(60, 60, 70, 160);
        if let Some(r) = tiny_skia::Rect::from_xywh(sx, list_top, sw, view_h) {
            fill_rect_clipped(pm, r, &track);
        }
        let thumb_h = (view_h * view_h / content_h).max(18.0 * s);
        let thumb_y = list_top + (view_h - thumb_h) * (scroll / max_scroll);
        let mut thumb = tiny_skia::Paint::default();
        thumb.set_color_rgba8(180, 180, 195, 200);
        if let Some(r) = tiny_skia::Rect::from_xywh(sx, thumb_y, sw, thumb_h) {
            fill_rect_clipped(pm, r, &thumb);
        }
    }
    // No search results
    if data.filtered.is_empty() {
        if let Some(font) = font {
            let msg = format!("No results for \"{}\"", data.filter);
            let cy = SPLASH_LIST_TOP * s + 40.0 * s;
            draw_text_c(pm, &msg, (vw - text_width(&msg, 13.0 * s)) * 0.5, cy, 13.0 * s, font, [150, 150, 160]);
            let msg2 = "Press Esc to clear the search";
            draw_text_c(pm, msg2, (vw - text_width(msg2, 11.0 * s)) * 0.5, cy + 20.0 * s, 11.0 * s, font, [120, 120, 132]);
        }
    }
    // Detail preview: hovered chart wins over keyboard selection.
    let preview = match data.hover {
        SplashHover::Chart(i) | SplashHover::Delete(i) => data.filtered.get(i).copied(),
        _ => data.sel,
    };
    draw_splash_detail(pm, data, preview, detail_x, vh, s);
}

/// Top-right utility buttons: sort toggle / refresh / open folder.
fn draw_splash_header_buttons(pm: &mut tiny_skia::PixmapMut, hover: SplashHover, sort: u8, vw: f32, s: f32) {
    let ty = SPLASH_HDR_BTN_Y * s;
    let bh = SPLASH_HDR_BTN_H * s;
    let right = vw - SPLASH_M * s;
    let mut x = right;
    x -= 104.0 * s;
    let dir = (x, 104.0 * s);
    x -= 8.0 * s;
    x -= 88.0 * s;
    let refresh = (x, 88.0 * s);
    x -= 8.0 * s;
    x -= 100.0 * s;
    let sort_b = (x, 100.0 * s);
    let sort_label = if sort == 1 { "Sort: Newest" } else { "Sort: Name" };
    draw_splash_btn(pm, sort_b.0, ty, sort_b.1, bh, hover == SplashHover::Sort, sort_label, 11.0, s);
    draw_splash_btn(pm, refresh.0, ty, refresh.1, bh, hover == SplashHover::Refresh, "Refresh", 11.0, s);
    draw_splash_btn(pm, dir.0, ty, dir.1, bh, hover == SplashHover::OpenFolder, "Open Folder", 11.0, s);
}

/// One gray button with centered text.
fn draw_splash_btn(pm: &mut tiny_skia::PixmapMut, x: f32, y: f32, w: f32, h: f32, hovered: bool, label: &str, size: f32, s: f32) {
    let mut bp = tiny_skia::Paint::default();
    bp.set_color_rgba8(if hovered { 55 } else { 34 }, if hovered { 60 } else { 38 }, if hovered { 72 } else { 46 }, 230);
    if let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) {
        fill_rect_clipped(pm, r, &bp);
    }
    if let Some(font) = get_font() {
        let tw = text_width(label, size * s);
        draw_text_c(pm, label, x + (w - tw) * 0.5, y + h * 0.5 + 4.0 * s, size * s, font, [225, 225, 230]);
    }
}

/// One chart row: name, composer · charter, level badge, hover delete button.
fn draw_splash_row(pm: &mut tiny_skia::PixmapMut, ch: &ChartEntry, hover: SplashHover, sel: Option<usize>, ri: usize, list_right: f32, y: f32, s: f32) {
    let row_w = list_right - SPLASH_M * s;
    let is_hover = hover == SplashHover::Chart(ri) || hover == SplashHover::Delete(ri);
    let is_sel = sel == Some(ri);
    let mut rp = tiny_skia::Paint::default();
    let (r, g, b) = if is_sel { (58, 66, 88) } else if is_hover { (42, 46, 58) } else { (24, 27, 33) };
    rp.set_color_rgba8(r, g, b, 240);
    if let Some(rect) = tiny_skia::Rect::from_xywh(SPLASH_M * s, y, row_w, SPLASH_ROW_H * s) {
        fill_rect_clipped(pm, rect, &rp);
    }
    let Some(font) = get_font() else { return };
    let text_w = row_w - 170.0 * s;
    // Name
    let name = fit_text(&ch.name, text_w, 14.0 * s);
    draw_text_on_pixmap(pm, &name, SPLASH_M * s + 8.0 * s, y + 12.0 * s, 14.0 * s, font);
    // Composer · charter
    let meta = if ch.composer.is_empty() && ch.charter.is_empty() {
        String::new()
    } else if ch.composer.is_empty() {
        ch.charter.clone()
    } else if ch.charter.is_empty() {
        ch.composer.clone()
    } else {
        format!("{} · {}", ch.composer, ch.charter)
    };
    if !meta.is_empty() {
        let meta = fit_text(&meta, text_w, 11.0 * s);
        draw_text_c(pm, &meta, SPLASH_M * s + 8.0 * s, y + 27.0 * s, 11.0 * s, font, [140, 140, 152]);
    }
    // Level badge
    if !ch.level.is_empty() || ch.difficulty > 0.0 {
        let label = if ch.level.is_empty() { format!("{:.1}", ch.difficulty) } else { ch.level.clone() };
        let badge_right = if is_hover { list_right - 30.0 * s } else { list_right - 8.0 * s };
        draw_badge(pm, &label, badge_right, y + 9.0 * s, 11.0 * s, level_color(&ch.level));
    }
    // Delete button (hover only)
    if is_hover {
        let dbx = list_right - 26.0 * s;
        let dby = y + 9.0 * s;
        let mut dp = tiny_skia::Paint::default();
        dp.set_color_rgba8(70, 30, 35, 240);
        if let Some(r) = tiny_skia::Rect::from_xywh(dbx, dby, 18.0 * s, 18.0 * s) {
            fill_rect_clipped(pm, r, &dp);
        }
        draw_text_c(pm, "x", dbx + 6.0 * s, dby + 14.0 * s, 14.0 * s, font, [255, 150, 150]);
    }
}

/// Right detail panel: thumbnail + metadata of the hovered/selected chart.
fn draw_splash_detail(pm: &mut tiny_skia::PixmapMut, data: &SplashData, preview: Option<usize>, x: f32, vh: f32, s: f32) {
    let w = SPLASH_DETAIL_W * s;
    let y = SPLASH_CTRL_Y * s;
    let h = (vh - 60.0 * s - y).max(80.0);
    let mut bp = tiny_skia::Paint::default();
    bp.set_color_rgba8(22, 24, 30, 240);
    if let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) {
        fill_rect_clipped(pm, r, &bp);
    }
    let Some(font) = get_font() else { return };
    let Some(ci) = preview else {
        draw_text_c(pm, "Select a chart to preview", x + 16.0 * s, y + 24.0 * s, 13.0 * s, font, [140, 140, 152]);
        return;
    };
    let ch = &data.charts[ci];
    // Illustration thumbnail
    let ty = y + 12.0 * s;
    let th = 170.0 * s;
    let tw = w - 24.0 * s;
    let mut tbg = tiny_skia::Paint::default();
    tbg.set_color_rgba8(12, 12, 17, 255);
    if let Some(r) = tiny_skia::Rect::from_xywh(x + 12.0 * s, ty, tw, th) {
        fill_rect_clipped(pm, r, &tbg);
    }
    if let Some(img) = &ch.thumb {
        draw_thumb(pm, img, x + 12.0 * s, ty, tw, th);
    }
    // Name
    let ny = ty + th + 14.0 * s;
    let name = fit_text(&ch.name, w - 32.0 * s, 16.0 * s);
    draw_text_on_pixmap(pm, &name, x + 16.0 * s, ny, 16.0 * s, font);
    // Metadata rows
    let mut my = ny + 24.0 * s;
    for (label, value) in [("Composer", &ch.composer), ("Charter", &ch.charter)] {
        if value.is_empty() { continue; }
        let val = fit_text(value, w - 16.0 * s - 100.0 * s, 12.0 * s);
        draw_text_c(pm, label, x + 16.0 * s, my, 11.0 * s, font, [130, 130, 142]);
        draw_text_c(pm, &val, x + 100.0 * s, my, 12.0 * s, font, [215, 215, 220]);
        my += 18.0 * s;
    }
    // Level / difficulty badge
    if !ch.level.is_empty() || ch.difficulty > 0.0 {
        let label = if ch.level.is_empty() { format!("Lv. {:.1}", ch.difficulty) } else { ch.level.clone() };
        draw_text_c(pm, "Level", x + 16.0 * s, my, 11.0 * s, font, [130, 130, 142]);
        draw_badge(pm, &label, x + w - 16.0 * s, my - 13.0 * s, 12.0 * s, level_color(&ch.level));
        my += 22.0 * s;
    }
    // Hint
    draw_text_c(pm, "Click to open  ·  Enter", x + 16.0 * s, y + h - 14.0 * s, 11.0 * s, font, [120, 120, 132]);
}

/// Difficulty tag color: AT/IN/HD/EZ/SP get distinctive hues, else gray.
fn level_color(level: &str) -> [u8; 3] {
    let tag = level.split_whitespace().next().unwrap_or("").to_uppercase();
    match tag.as_str() {
        "AT" => [190, 120, 255],
        "IN" => [255, 90, 130],
        "HD" => [255, 160, 60],
        "EZ" => [90, 210, 120],
        "SP" => [90, 200, 230],
        _ => [150, 150, 165],
    }
}

/// Colored pill badge, right-aligned at `right`.
fn draw_badge(pm: &mut tiny_skia::PixmapMut, text: &str, right: f32, y: f32, size: f32, color: [u8; 3]) {
    let Some(font) = get_font() else { return };
    let w = text_width(text, size) + 2.4 * size;
    let h = size * 1.7;
    let mut bp = tiny_skia::Paint::default();
    bp.set_color_rgba8(color[0], color[1], color[2], 210);
    if let Some(r) = tiny_skia::Rect::from_xywh(right - w, y, w, h) {
        fill_rect_clipped(pm, r, &bp);
    }
    draw_text_c(pm, text, right - w + 1.2 * size, y + h * 0.72, size, font, [255, 255, 255]);
}

/// Scale an image to *cover* the `maxw`×`maxh` box (fill, center-crop the
/// overflow — no letterboxing) and blit it onto the pixmap, premultiplying
/// alpha. Only the box region is written.
fn draw_thumb(pm: &mut tiny_skia::PixmapMut, img: &image::RgbaImage, x: f32, y: f32, maxw: f32, maxh: f32) {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    if iw <= 1.0 || ih <= 1.0 { return; }
    let boxw = maxw.round() as i32;
    let boxh = maxh.round() as i32;
    if boxw <= 0 || boxh <= 0 { return; }
    // Cover: scale so the smaller dimension fills the box, crop the rest.
    let scale = (maxw / iw).max(maxh / ih);
    let (tw, th) = ((iw * scale).round() as u32, (ih * scale).round() as u32);
    let r = image::imageops::resize(img, tw.max(1), th.max(1), image::imageops::FilterType::Triangle);
    let ox = ((tw as f32 - maxw) * 0.5).round() as i32;
    let oy = ((th as f32 - maxh) * 0.5).round() as i32;
    let pm_w = pm.width() as i32;
    let pm_h = pm.height() as i32;
    let dx0 = x.round() as i32;
    let dy0 = y.round() as i32;
    let data = pm.data_mut();
    let src = r.as_raw();
    for row in 0..boxh {
        let sy = oy + row;
        if sy < 0 || sy >= r.height() as i32 { continue; }
        let py = dy0 + row;
        if py < 0 || py >= pm_h { continue; }
        for col in 0..boxw {
            let sx = ox + col;
            if sx < 0 || sx >= r.width() as i32 { continue; }
            let px = dx0 + col;
            if px < 0 || px >= pm_w { continue; }
            let si = ((sy as u32) * r.width() + sx as u32) as usize * 4;
            let a = src[si + 3] as u32;
            if a == 0 { continue; }
            let di = ((py * pm_w + px) as usize) * 4;
            if a == 255 {
                data[di..di + 4].copy_from_slice(&src[si..si + 4]);
            } else {
                data[di] = (src[si] as u32 * a / 255) as u8;
                data[di + 1] = (src[si + 1] as u32 * a / 255) as u8;
                data[di + 2] = (src[si + 2] as u32 * a / 255) as u8;
                data[di + 3] = a as u8;
            }
        }
    }
}

/// Approximate rendered text width (per-char fallback metrics, matching
/// [`draw_text_c`] so measured width always matches the drawn width).
fn text_width(text: &str, size: f32) -> f32 {
    let fonts = get_fonts();
    text.chars().map(|ch| {
        let f = fonts.iter().find(|f| f.has_glyph(ch)).or_else(|| fonts.first());
        f.map(|f| f.metrics(ch, size).advance_width).unwrap_or(0.0)
    }).sum()
}

/// Truncate `text` with a trailing "..." to fit `max_w`.
fn fit_text(text: &str, max_w: f32, size: f32) -> String {
    if text_width(text, size) <= max_w { return text.to_string(); }
    let mut out = String::new();
    for ch in text.chars() {
        if text_width(&format!("{out}{ch}"), size) > max_w { break; }
        out.push(ch);
    }
    out.push_str("...");
    out
}

/// Settings page: toggles for vsync / fullscreen and a gui-scale slider.
/// Hover targets: Vsync / Fullscreen / ScaleRow / ScaleMinus / ScalePlus /
/// Back (hit-testing in [`splash_hit_test`]).
fn draw_splash_settings(pm: &mut tiny_skia::PixmapMut, cfg: &SettingsData, hover: SplashHover, vw: f32, vh: f32, s: f32) {
    let font = get_font();
    let bar_h = SPLASH_BAR_H * s;
    let mut y = 90.0 * s;
    let row_h = 34.0 * s;
    let lx = 60.0 * s;
    let rx = vw - 60.0 * s;
    let row_bg = |hover: bool| -> tiny_skia::Paint {
        let mut p = tiny_skia::Paint::default();
        p.set_color_rgba8(if hover { 45 } else { 30 }, if hover { 50 } else { 33 }, if hover { 60 } else { 38 }, 220);
        p
    };

    let rows: [(&str, String, SplashHover); 3] = [
        ("Vsync", format!("{}", if cfg.vsync { "ON" } else { "OFF" }), SplashHover::Vsync),
        ("Fullscreen", format!("{}", if cfg.fullscreen { "ON" } else { "OFF" }), SplashHover::Fullscreen),
        ("GUI Scale", format!("{:.1}", cfg.gui_scale), SplashHover::ScaleRow),
    ];
    for (label, value, target) in rows {
        let hovered = hover == target;
        if let Some(r) = tiny_skia::Rect::from_xywh(lx, y, rx - lx, row_h) {
            fill_rect_clipped(pm, r, &row_bg(hovered));
        }
        if let Some(font) = font {
            draw_text_on_pixmap(pm, label, lx + 12.0 * s, y + row_h * 0.5, 14.0 * s, font);
            draw_text_on_pixmap(pm, &value, rx - (value.len() as f32) * 8.0 * s - 12.0 * s, y + row_h * 0.5, 14.0 * s, font);
        }
        y += row_h + 6.0 * s;
    }
    // Scale +/- buttons row
    let scale_row = y;
    let bw = 60.0 * s;
    for (idx, (target, label)) in [(0usize, (SplashHover::ScaleMinus, "-")), (1usize, (SplashHover::ScalePlus, "+"))] {
        let hovered = hover == target;
        let bx = lx + idx as f32 * (bw + 8.0 * s);
        if let Some(r) = tiny_skia::Rect::from_xywh(bx, scale_row, bw, row_h) {
            fill_rect_clipped(pm, r, &row_bg(hovered));
        }
        if let Some(font) = font {
            draw_text_on_pixmap(pm, label, bx + bw * 0.5 - 4.0 * s, scale_row + row_h * 0.5, 16.0 * s, font);
        }
    }
    // Back button
    let by = vh - bar_h - row_h - 10.0 * s;
    let hovered = hover == SplashHover::Back;
    if let Some(r) = tiny_skia::Rect::from_xywh(lx, by, 100.0 * s, row_h) {
        fill_rect_clipped(pm, r, &row_bg(hovered));
    }
    if let Some(font) = font {
        draw_text_on_pixmap(pm, "← Back", lx + 12.0 * s, by + row_h * 0.5, 14.0 * s, font);
    }
}

pub fn draw_menu(pm: &mut tiny_skia::PixmapMut, vw: f32, vh: f32, s: f32) {
    let _s = trace_span!("draw_menu");
    // Dim background
    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(0, 0, 0, 160);
    if let Some(r) = tiny_skia::Rect::from_xywh(0.0, 0.0, vw, vh) {
        fill_rect_clipped(pm, r, &bg);
    }
    // Panel
    let pw = 240.0 * s;
    let ph = 150.0 * s;
    let px = (vw - pw) / 2.0;
    let py = (vh - ph) / 2.0;
    let mut panel = tiny_skia::Paint::default();
    panel.set_color_rgba8(30, 32, 38, 240);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, py, pw, ph) {
        fill_rect_clipped(pm, r, &panel);
    }
    // Title
    let font = get_font();
    if let Some(font) = font {
        draw_text_on_pixmap(pm, "Menu", px + 12.0 * s, py + 28.0 * s, 16.0 * s, font);
    }
    // Menu items (geometry must match the hit-test in handle_click)
    let items = ["Save (Ctrl+S)", "Quit to Menu (Ctrl+Q)"];
    let item_h = 32.0 * s;
    let iy = py + 50.0 * s;
    for (i, label) in items.iter().enumerate() {
        let y = iy + i as f32 * item_h;
        let mut ip = tiny_skia::Paint::default();
        ip.set_color_rgba8(45, 48, 55, 255);
        if let Some(r) = tiny_skia::Rect::from_xywh(px + 8.0 * s, y, pw - 16.0 * s, item_h - 4.0 * s) {
            fill_rect_clipped(pm, r, &ip);
        }
        if let Some(font) = font {
            draw_text_on_pixmap(pm, label, px + 16.0 * s, y + item_h * 0.5, 14.0 * s, font);
        }
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
    let _s = trace_span!("draw_5col");
        if !info.show_overlay || !info.show_events || info.events.is_empty() { return; }
    let tl_w = TL_W * s;
    let col_w = COL_W * s;
    let col_g = COL_GAP * s;
    let head_h = HEADER_H * s;

    let py = head_h + 4.0 * s;
    let ph = (vh - 56.0 * s - py) as f64;
    if ph <= 0.0 { return; }

    // Background
    let mut ebg = tiny_skia::Paint::default();
    ebg.set_color_rgba8(20, 25, 35, 200);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, tl_w, vh - 48.0 * s) {
        fill_rect_clipped(pixmap, r, &ebg);
    }

    let (scroll, zoom) = (scroll as f64, zoom as f64);
    let (min_b, max_b) = (scroll, scroll + zoom);

    let to_y = |b: f64| py as f64 + ph - (b - min_b) / zoom * ph;

    let n_cols: usize = if info.show_notes && !info.notes.is_empty() { 6 } else { 5 };
    let hdr_h = head_h - 4.0 * s;

    // Column header background (single rect)
    let hdr_x = px + 4.0 * s;
    let hdr_w = n_cols as f32 * col_w + (n_cols as f32 - 1.0) * col_g;
    let mut hbg = tiny_skia::Paint::default();
    hbg.set_color_rgba8(40, 50, 65, 200);
    if let Some(hr) = tiny_skia::Rect::from_xywh(hdr_x, 2.0 * s, hdr_w, hdr_h) {
        fill_rect_clipped(pixmap, hr, &hbg);
    }

    // Grid lines at snap divisions (single rect spanning all columns)
    let grid = (info.snap as f64).max(0.125);
    let g_start = (min_b / grid).ceil() as i32;
    let g_end = (max_b / grid).floor() as i32;
    let grid_x = px + 4.0 * s;
    let grid_w = n_cols as f32 * col_w + (n_cols as f32 - 1.0) * col_g;
    for gi in g_start..=g_end {
        let b = gi as f64 * grid;
        let y = to_y(b) as f32;
        let is_whole = (b.round() - b).abs() < 0.001;
        let is_half = !is_whole && ((b * 2.0).round() - b * 2.0).abs() < 0.001;
        let a = if is_whole { 100 } else if is_half { 70 } else { 40 };
        let h = if is_whole { 2.0 } else { 1.0 };
        for dy in 0..h as i32 {
            hline(pixmap, y + dy as f32, grid_x, grid_x + grid_w, [60, 60, 70, a]);
        }
    }

    // Current time line across all columns (beat-aligned)
    let ct_y = to_y(info.chart_beat).clamp(py as f64, py as f64 + ph) as f32;
    hline(pixmap, ct_y - 1.0, px + 2.0 * s, px + tl_w - 2.0 * s, [255, 200, 80, 230]);
    hline(pixmap, ct_y + 1.0, px + 2.0 * s, px + tl_w - 2.0 * s, [255, 200, 80, 230]);

    // Layer selector at bottom of timeline panel
    let layer_bar_y = (vh - 48.0 * s) as f32 - 26.0 * s;
    let mut lbg = tiny_skia::Paint::default();
    lbg.set_color_rgba8(30, 30, 35, 200);
    if let Some(r) = tiny_skia::Rect::from_xywh(px + 2.0 * s, layer_bar_y, tl_w - 4.0 * s, 22.0 * s) {
        fill_rect_clipped(pixmap, r, &lbg);
    }
    for li in 0..info.max_layers.min(10) {
        let bx = px + 6.0 * s + li as f32 * (20.0 * s + 4.0 * s);
        let mut lp = tiny_skia::Paint::default();
        if li == info.selected_layer { lp.set_color_rgba8(200, 200, 220, 220); }
        else { lp.set_color_rgba8(100, 100, 120, 160); }
        if let Some(r) = tiny_skia::Rect::from_xywh(bx, layer_bar_y + 3.0 * s, 20.0 * s, 16.0 * s) {
            fill_rect_clipped(pixmap, r, &lp);
        }
    }

    // Event blocks per column (cols 0-4), skip outside visible range.
    // info.events is sorted by end_beats (extract_line_events) → binary search
    // the visible window instead of iterating every event.
    let ev_min = min_b - zoom * 0.05;
    let ev_max = max_b + zoom * 0.05;
    let ev_start = info.events.partition_point(|e| e.end_beats < ev_min);
    for (ei, ev) in info.events[ev_start..].iter().enumerate() {
        if ev.start_beats > ev_max { break; }
        let ei = ev_start + ei;
        let ci = match ev.kind.as_str() {
            "Alpha" => Some(0), "MoveX" => Some(1), "MoveY" => Some(2),
            "Rotate" => Some(3), "Speed" => Some(4), _ => None,
        };
        let Some(ci) = ci else { continue };
        let c = KIND_COLORS[ci].1;
        let y0 = to_y(ev.start_beats).clamp(py as f64, py as f64 + ph) as f32;
        let y1 = to_y(ev.end_beats).clamp(py as f64, py as f64 + ph) as f32;
        let bh = (y1 - y0).abs().max(3.0);
        let selected = info.selected_event_idx == Some(ei);
        if selected {
            let mut sp = tiny_skia::Paint::default();
            sp.set_color_rgba8(255, 255, 255, 220);
            let cx = px + 4.0 * s + ci as f32 * (col_w + col_g);
            if let Some(r) = tiny_skia::Rect::from_xywh(cx - 2.0, y0.min(y1) - 2.0, col_w + 4.0, bh + 4.0) {
                fill_rect_clipped(pixmap, r, &sp);
            }
        }
        let mut ep = tiny_skia::Paint::default();
        ep.set_color_rgba8(c[0], c[1], c[2], if selected { 255 } else { 200 });
        let cx = px + 4.0 * s + ci as f32 * (col_w + col_g);
        if let Some(r) = tiny_skia::Rect::from_xywh(cx, y0.min(y1), col_w, bh) {
            fill_rect_clipped(pixmap, r, &ep);
        }
    }

    // Note blocks per column (col 5, only when show_notes)
    if n_cols >= 6 {
        for note in info.notes.iter() {
            let c = match note.kind { 1 => [50, 150, 255], 2 => [100, 200, 255], 3 => [255, 80, 150], 4 => [255, 220, 60], _ => [180; 3] };
            let y0 = to_y(note.start_beats).clamp(py as f64, py as f64 + ph) as f32;
            let y1 = to_y(note.end_beats).clamp(py as f64, py as f64 + ph) as f32;
            let bh = (y1 - y0).abs().max(3.0);
            let mut np = tiny_skia::Paint::default();
            np.set_color_rgba8(c[0], c[1], c[2], 200);
            let cx = px + 4.0 * s + 5.0 * (col_w + col_g);
            if let Some(r) = tiny_skia::Rect::from_xywh(cx + 1.0, y0.min(y1), col_w - 2.0, bh) {
                fill_rect_clipped(pixmap, r, &np);
            }
        }
    }
}

fn draw_notes_timeline(pixmap: &mut tiny_skia::PixmapMut, scroll: f32, zoom: f32, info: &GameInfo, px: f32, vh: f32, s: f32) {
    let _s = trace_span!("draw_notes");
        if !info.show_overlay || !info.show_notes || info.notes.is_empty() { return; }
    let nt_w = NT_W * s;
    let head_h = HEADER_H * s;
    let pad_x = 12.0 * s;
    let play_w = nt_w - pad_x * 2.0;
    let py = head_h + 4.0 * s;
    let ph = (vh - 56.0 * s - py) as f64;
    if ph <= 0.0 { return; }

    let v_split = info.vertical_split.max(1) as f32;

    let (scroll, zoom) = (scroll as f64, zoom as f64);
    let (min_b, max_b) = (scroll, scroll + zoom);
    let to_y = |b: f64| py as f64 + ph - (b - min_b) / zoom * ph;
    // X position: note.position_x / 675 → -1..1 → panel center ±play_w/2
    let to_x = |nx: f32| px + pad_x + (nx / 675.0 + 1.0) * play_w * 0.5;
    // Column-aware x: map position_x to column center when split > 1
    let to_col_x = |nx: f32| {
        if v_split <= 1.0 { to_x(nx) }
        else {
            let col_w = play_w / v_split;
            let half = 675.0;
            let col = ((nx / half + 1.0) * v_split * 0.5).clamp(0.0, v_split - 1.0).round() as usize;
            let col = col.min(v_split as usize - 1);
            px + pad_x + (col as f32 + 0.5) * col_w
        }
    };

    // Background
    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(20, 25, 35, 215);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, nt_w, vh - 48.0 * s) {
        fill_rect_clipped(pixmap, r, &bg);
    }

    // Header (full preview mode = different color)
    let mut hp = tiny_skia::Paint::default();
    if info.full_notes { hp.set_color_rgba8(200, 140, 255, 200); }
    else { hp.set_color_rgba8(140, 200, 255, 180); }
    if let Some(r) = tiny_skia::Rect::from_xywh(px + pad_x, 2.0 * s, play_w, head_h - 4.0 * s) {
        fill_rect_clipped(pixmap, r, &hp);
    }
    // Header split indicator (top-right)
    if let Some(font) = get_font() {
        let label = format!("横:{:.0}b 纵:{:.0}", info.snap * 4.0, v_split);
        draw_text_on_pixmap(pixmap, &label, px + nt_w - pad_x - 120.0 * s, 2.0 * s + head_h * 0.4, 10.0 * s, font);
    }

    // Vertical column dividers (thin lines only, no fills)
    if v_split > 1.0 {
        let col_w = play_w / v_split;
        for ci in 1..v_split as usize {
            let x = px + pad_x + ci as f32 * col_w;
            let mut vp = tiny_skia::Paint::default();
            vp.set_color_rgba8(80, 100, 140, 80);
            if let Some(r) = tiny_skia::Rect::from_xywh(x, py, 1.0, ph as f32) {
                fill_rect_clipped(pixmap, r, &vp);
            }
        }
    }

    // Grid lines at snap divisions
    let grid = (info.snap as f64).max(0.125);
    let g_start = (min_b / grid).ceil() as i32;
    let g_end = (max_b / grid).floor() as i32;
    if !*SKIP_GRID.get_or_init(|| env_flag("PHIMAKOR_SKIP_GRID")) {
    for gi in g_start..=g_end {
        let b = gi as f64 * grid;
        let y = to_y(b) as f32;
        let is_whole = (b.round() - b).abs() < 0.001;
        let a = if is_whole { 90 } else { 50 };
        hline(pixmap, y, px + pad_x, px + pad_x + play_w, [60, 60, 70, a]);
    }
    }

    // Center line
    if !*SKIP_CENTER.get_or_init(|| env_flag("PHIMAKOR_SKIP_CENTER")) {
    let cx = to_x(0.0);
    // 1px vertical line: write pixels directly
    {
        let h = pixmap.height() as i32;
        let w = pixmap.width() as i32;
        let x = (cx.round() as i32).clamp(0, w - 1);
        let y0 = (py.round() as i32).max(0);
        let y1 = ((ph as f32 + py).round() as i32).min(h - 1);
        let data = pixmap.data_mut();
        for yy in y0..=y1 {
            let i = (yy as usize * w as usize + x as usize) * 4;
            data[i] = 100; data[i+1] = 100; data[i+2] = 120; data[i+3] = 120;
        }
    }
    }

    // Current time (beat-aligned)
    let ct_y = to_y(info.chart_beat).clamp(py as f64, py as f64 + ph) as f32;
    hline(pixmap, ct_y - 1.0, px + pad_x, px + pad_x + play_w, [255, 200, 80, 230]);
    hline(pixmap, ct_y + 1.0, px + pad_x, px + pad_x + play_w, [255, 200, 80, 230]);

    // Notes, skip outside visible range (info.notes sorted by end_beats)
    let nt_min = min_b - zoom * 0.05;
    let nt_max = max_b + zoom * 0.05;
    if std::env::var("PHIMAKOR_SKIP_NOTES").is_err() {
    let nt_start = info.notes.partition_point(|n| n.end_beats < nt_min);
    for note in &info.notes[nt_start..] {
        if note.start_beats > nt_max { break; }
        let c = match note.kind { 1 => [50, 150, 255], 2 => [100, 200, 255], 3 => [255, 80, 150], 4 => [255, 220, 60], _ => [180; 3] };
        let xn = to_col_x(note.x);
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
                fill_rect_clipped(pixmap, r, &np);
            }
        }
        if let Some(r) = tiny_skia::Rect::from_xywh(xn - nw * 0.5, y0 - nh * 0.5, nw, nh) {
            fill_rect_clipped(pixmap, r, &np);
        }
    }
    }
}

/// Draw a panel definition into the right-side area.
pub fn draw_panel_def(pixmap: &mut tiny_skia::PixmapMut, def: &panels::PanelDef, info: &GameInfo, px: f32, vw: f32, vh: f32, s: f32) {
    let _s = trace_span!("draw_panel");
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
        fill_rect_clipped(pixmap, r, &bg);
    }

    let mut hp = tiny_skia::Paint::default();
    hp.set_color_rgba8(60, 60, 70, 180);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, pan_w, 24.0 * s) {
        fill_rect_clipped(pixmap, r, &hp);
    }

    if let Some(font) = &font {
        draw_text_on_pixmap(pixmap, &def.name, px + 6.0 * s, 16.0 * s, 12.0 * s, font);
    }

    let cell_h = 22.0 * s;
    let mut y = 28.0 * s;

    for (label, val, span, kind) in &rows {
        if *kind == 1 {
            // Regular separator line
            y += cell_h * 0.3;
            let mut sp = tiny_skia::Paint::default();
            sp.set_color_rgba8(50, 50, 60, 100);
            if let Some(r) = tiny_skia::Rect::from_xywh(px + 4.0 * s, y, pan_w - 8.0 * s, 1.0) {
                fill_rect_clipped(pixmap, r, &sp);
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
                fill_rect_clipped(pixmap, r, &sp);
            }
            y += cell_h * 0.4;
            continue;
        }
        let row_h = cell_h * (*span as f32).max(1.0);
        let mut lp = tiny_skia::Paint::default();
        lp.set_color_rgba8(20, 20, 25, 120);
        if let Some(r) = tiny_skia::Rect::from_xywh(px, y, pan_w, row_h) {
            fill_rect_clipped(pixmap, r, &lp);
        }
        if let Some(font) = &font {
            draw_text_on_pixmap(pixmap, label, px + 8.0 * s, y + cell_h * 0.5, 10.0 * s, font);
            let vx = px + pan_w * 0.5 + 4.0 * s;
            draw_text_on_pixmap(pixmap, val, vx, y + cell_h * 0.5, 10.0 * s, font);
        }
        y += row_h;
    }
    // Selected event details (overlay at bottom of panel)
    if let Some(ei) = info.selected_event_idx {
        let event_h = 28.0 * s;
        let ey = bg_h - event_h * 7.0 - 8.0 * s;
        if ey > y {
            let mut eb = tiny_skia::Paint::default();
            eb.set_color_rgba8(40, 42, 50, 230);
            if let Some(r) = tiny_skia::Rect::from_xywh(px, ey, pan_w, event_h * 7.0 + 8.0 * s) {
                fill_rect_clipped(pixmap, r, &eb);
            }
            let rows: [(&str, &str, u8); 6] = [
                ("Event", &info.ev_kind, 255),
                ("Start", &format!("{:.3}b", info.ev_start_beats), 0),
                ("End", &format!("{:.3}b", info.ev_end_beats), 1),
                ("From", &format!("{:.4}", info.ev_start_val), 2),
                ("To", &format!("{:.4}", info.ev_end_val), 3),
                ("Easing", &format!("{}", info.ev_easing), 4),
            ];
            if let Some(font) = &font {
                let mut ry = ey + 4.0 * s;
                for (label, val, tgt) in &rows {
                    let highlight = info.event_edit_target == *tgt;
                    if highlight {
                        let mut hp = tiny_skia::Paint::default();
                        hp.set_color_rgba8(60, 80, 120, 60);
                        if let Some(r) = tiny_skia::Rect::from_xywh(px + 4.0 * s, ry, pan_w - 8.0 * s, event_h) {
                            fill_rect_clipped(pixmap, r, &hp);
                        }
                    }
                    draw_text_on_pixmap(pixmap, label, px + 8.0 * s, ry + cell_h * 0.5, 10.0 * s, font);
                    draw_text_on_pixmap(pixmap, val, px + pan_w * 0.5 + 4.0 * s, ry + cell_h * 0.5, 10.0 * s, font);
                    ry += event_h;
                }
            }
        }
    }
}

/// [Eff] panel: show currently active post-processing effects.
fn draw_effects_panel(pixmap: &mut tiny_skia::PixmapMut, info: &GameInfo, px: f32, vw: f32, vh: f32, s: f32) {
    if !info.show_overlay { return; }
    if vh < 10.0 { return; }
    let font = get_font();
    let pan_w = 280.0 * s;
    let bar_h = 48.0 * s;
    let bg_h = (vh - bar_h).max(1.0);
    let cell_h = 22.0 * s;

    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(12, 12, 14, 200);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, pan_w, bg_h) {
        fill_rect_clipped(pixmap, r, &bg);
    }

    let mut y = 28.0 * s;
    if let Some(font) = font {
        draw_text_on_pixmap(pixmap, "Active Effects", px + 8.0 * s, y, 12.0 * s, font);
        y += cell_h * 2.0;

        if info.effect_names.is_empty() {
            draw_text_on_pixmap(pixmap, "(none)", px + 8.0 * s, y, 10.0 * s, font);
        } else {
            for name in &info.effect_names {
                draw_text_on_pixmap(pixmap, name, px + 12.0 * s, y, 10.0 * s, font);
                y += cell_h;
            }
        }
    }
}

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

// Keyed by (char, size, font) — the same char can come from a different
// fallback font, and its metrics/bitmap differ per font.
static GLYPH_CACHE: LazyLock<Mutex<HashMap<(char, u32, usize), (fontdue::Metrics, Vec<u8>)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn draw_text_on_pixmap(pixmap: &mut tiny_skia::PixmapMut, text: &str, x: f32, y: f32, size: f32, font: &fontdue::Font) {
    draw_text_c(pixmap, text, x, y, size, font, [255, 255, 255]);
}

/// Draw text with per-character global font fallback: each char is rasterized
/// from the first loaded font that contains its glyph (CJK → system fonts).
/// `font` is only a hint for metrics consistency — glyphs fall through.
fn draw_text_c(pixmap: &mut tiny_skia::PixmapMut, text: &str, x: f32, y: f32, size: f32, font: &fontdue::Font, rgb: [u8; 3]) {
    let _s = trace_span!("draw_text");
    let w = pixmap.width() as i32;
    let h = pixmap.height() as i32;
    let size_key = (size * 100.0) as u32;
    let mut pen_x = x;
    for ch in text.chars() {
        let f = font_for(ch).unwrap_or(font);
        let font_id = f as *const fontdue::Font as usize;
        let (m, bitmap) = {
            let mut cache = GLYPH_CACHE.lock().unwrap();
            if let Some(entry) = cache.get(&(ch, size_key, font_id)) {
                (entry.0.clone(), entry.1.clone())
            } else {
                let result = f.rasterize(ch, size);
                cache.insert((ch, size_key, font_id), (result.0.clone(), result.1.clone()));
                result
            }
        };
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
                data[idx] = rgb[0]; data[idx+1] = rgb[1]; data[idx+2] = rgb[2]; data[idx+3] = 255;
            }
        }
        pen_x += m.advance_width;
    }
}

fn draw_quick_panel(pixmap: &mut tiny_skia::PixmapMut, tool_hover: Option<usize>, selected_tool: usize, hp: [f32; 4], info: &GameInfo, qp_w: f32, vw: f32, vh: f32, s: f32) {
    let _s = trace_span!("draw_quick_panel");
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
        fill_rect_clipped(pixmap, r, &bg);
    }

    let tools: [(&str, [u8; 3]); 4] = [
        ("Chart",  [100, 200, 255]),
        ("Line",   [100, 220, 100]),
        ("Settings", [180, 180, 190]),
        ("Eff",    [255, 150, 100]),
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
            fill_rect_clipped(pixmap, rect, &bp);
        }

        // Label text on button when hovered (skip if no font)
        if p > 0.3 {
            if let Some(font) = font {
                let lx = bx + 6.0 * s;
                let ly = y + btn_base * 0.5 + 4.0 * s;
                draw_text_on_pixmap(pixmap, name, lx, ly, 11.0 * s, font);
            }
        }
    }
}

// ── Display data ──

/// 1px horizontal line via direct pixel writes (tiny_skia fill_rect is
/// ~300µs/call in debug — grid lines are the hot path).
fn hline(pm: &mut tiny_skia::PixmapMut, y: f32, x0: f32, x1: f32, rgba: [u8; 4]) {
    let h = pm.height() as i32;
    let w = pm.width() as i32;
    let y = y.round() as i32;
    if y < 0 || y >= h { return; }
    let x0 = (x0.round() as i32).max(0);
    let x1 = (x1.round() as i32).min(w - 1);
    if x0 > x1 { return; }
    let row = (y as usize) * (w as usize);
    let data = pm.data_mut();
    for x in x0..=x1 {
        let i = (row + x as usize) * 4;
        data[i] = rgba[0]; data[i + 1] = rgba[1]; data[i + 2] = rgba[2]; data[i + 3] = rgba[3];
    }
}

/// fill_rect clipped to the pixmap bounds. Panels slide in from off-screen
/// (x > w during animations), and tiny_skia asserts on out-of-bounds rects.
/// Edges are rounded OUTWARD to whole pixels: tiny_skia 0.11.4's hairline AA
/// asserts on 1px-wide rects with fractional origins (inner width → 0).
/// Solid colors are written directly via `slice::fill` — tiny_skia's
/// per-pixel pipeline costs ~5ms for a large rect in debug builds.
pub fn fill_rect_clipped(pm: &mut tiny_skia::PixmapMut, rect: tiny_skia::Rect, paint: &tiny_skia::Paint) {
    let w = pm.width() as i32;
    let h = pm.height() as i32;
    let l = (rect.left().floor() as i32).max(0);
    let t = (rect.top().floor() as i32).max(0);
    let r = (rect.right().ceil() as i32).min(w);
    let b = (rect.bottom().ceil() as i32).min(h);
    if r <= l || b <= t { return; }
    // Fast path: solid color → direct u32 fill.
    if let tiny_skia::Shader::SolidColor(c) = &paint.shader {
        let rgba = c.premultiply().to_color_u8();
        let color = u32::from_le_bytes([rgba.red(), rgba.green(), rgba.blue(), rgba.alpha()]);
        let data = pm.data_mut();
        for y in t..b {
            let row = (y as usize) * (w as usize) * 4;
            let start = row + (l as usize) * 4;
            let end = row + (r as usize) * 4;
            let bytes = &mut data[start..end];
            let words = bytemuck::cast_slice_mut::<u8, u32>(bytes);
            words.fill(color);
        }
        return;
    }
    if let Some(cr) = tiny_skia::Rect::from_ltrb(l as f32, t as f32, r as f32, b as f32) {
        let mut p = paint.clone();
        p.anti_alias = false;
        pm.fill_rect(cr, &p, tiny_skia::Transform::default(), None);
    }
}

#[derive(Clone)]
pub struct EventEntry {
    pub layer: usize, pub kind: String, pub index: usize,
    pub start_beats: f64, pub end_beats: f64,
    pub start: f32, pub end: f32, pub easing: i32,
}

#[derive(Clone)]
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
    m.insert("snap", format!("{}", info.snap));
    m.insert("vsync", if info.vsync { "ON" } else { "OFF" }.to_string());
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
    pub selected_layer: usize, pub max_layers: usize, pub events: Arc<Vec<EventEntry>>,
    pub notes: Arc<Vec<NoteEntry>>,
    pub gui_scale: f32,
    pub snap: f32,
    pub vsync: bool,
    pub vertical_split: u32,
    pub selected_tool: usize,
    pub show_menu: bool,
    pub selected_event_idx: Option<usize>,
    pub event_edit_target: u8,
    pub ev_kind: String,
    pub ev_start_beats: f64,
    pub ev_end_beats: f64,
    pub ev_start_val: f32,
    pub ev_end_val: f32,
    pub ev_easing: i32,
    pub effect_names: Vec<String>,
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

    fn btn(label: &str, s: f32) -> Element<'static, (), Theme, Renderer> {
        container(text(label.to_owned()).size(iced::Pixels(13.0 * s))).padding([6.0 * s, 14.0 * s])
            .style(|_: &Theme| container::Style::default().background(iced::Color::from_rgba(0.25, 0.25, 0.28, 0.85))).into()
    }
    let bar: Element<'_, (), Theme, Renderer> = container(
        iced::widget::Row::new()
            .push(iced::widget::Row::new().width(Length::Fill))
            .push(btn("Events", s))
            .push(btn("Notes", s))
            .push(btn("Menu", s))
            .spacing(6.0 * s)
            .padding([0.0, 10.0 * s]),
    ).height(48.0 * s).width(Length::Fill)
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
