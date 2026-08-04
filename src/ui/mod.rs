use iced::advanced::layout::{Layout, Limits};
use iced::advanced::widget::Tree;
use iced::advanced::{renderer, Renderer as _};
use iced::{Element, Point, Rectangle, Size, Theme};
use iced_tiny_skia::Renderer;
use std::sync::Arc;

pub mod panels;
pub mod widgets;
use widgets::Widget as _;
use widgets::Canvas as _;
use phimakor::trace_span;

// Global font fallback chain: the first font that has a glyph for a given
// character wins, so Latin text renders in the primary UI font while CJK /
// symbols fall through to system fonts (Microsoft YaHei, SimHei, PingFang,
// Noto CJK, ...).
//
// Latin fonts load eagerly at startup (~2 MB). CJK fonts are heavy to parse
// (fontdue expands the GSUB tables — hundreds of MB for a full CJK set), so
// they load LAZILY, one at a time, and only up to the first font that covers
// the requested glyph: a Chinese chart name touches msyh.ttc and stops there.

pub mod bpm_panel;
pub mod font;
pub mod model;
pub mod panel_ui;
pub mod primitives;
pub mod settings;
pub mod splash;
pub mod text;
pub mod timeline;
pub mod timeline_draw;

pub use font::font_mem_bytes;
pub use model::{gameinfo_values, EffectRow, EventEntry, GameInfo, KfRow, NoteEntry};
pub use panel_ui::draw_panel_def;
pub use primitives::fill_rect_clipped;
pub use settings::{backend_cycle, backend_label, SettingsData};
pub use splash::{draw_splash, filter_charts, splash_detail_x, splash_hit_test, splash_list_right, ChartEntry, SplashData, SplashHover};
pub use timeline::{PANEL_W, QP_W};
use self::text::draw_text_on_pixmap;
use self::font::get_font;
use self::primitives::hline;
use self::timeline::{draw_5col_timeline, draw_notes_timeline, COL_GAP, COL_W, HEADER_H, NT_W, TL_W};
use self::panel_ui::{draw_effects_panel, draw_quick_panel};
pub(crate) use self::panel_ui::{effects_hit_test, EffHit};
use self::model::build_ui;

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
    /// Toggle the note-preview panel (quick toolbar button).
    ToggleNotes,
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
    tool_hover_progress: [f32; 5],
    panel_defs: Vec<panels::PanelDef>,
    /// BPM 面板组件(组件库试点,tool 4)。由 main.rs 每帧构建/更新。
    pub bpm_form: Option<widgets::RealtimeForm>,
    pub bpm_hover: Option<widgets::Area>,
    /// 设置面板组件(组件库,tool 2)。由 main.rs 每帧构建/更新。
    pub settings_form: Option<widgets::RealtimeForm>,
    pub settings_hover: Option<widgets::Area>,
    /// Line 面板的实时线数据滚动列表(tool 1)。main.rs 每帧构建。
    pub line_list: Option<widgets::ScrollList>,
    pub line_list_hover: Option<widgets::Area>,
    /// Chart 面板的元数据网格(tool 0)。main.rs 每帧构建。
    pub chart_grid: Option<widgets::KeyValueGrid>,
    pub chart_grid_hover: Option<widgets::Area>,
    /// 时间轴绘制 worker(PMCORE-55):后台画,主线程只上传。
    pub tl_worker: Option<timeline_draw::TimelineWorker>,
    /// 右上角性能提示开关(设置里开启,播放帧延迟大时显示)。
    pub perf_hint: bool,
    /// 自定义 GPU 光标(系统光标隐藏,worker 管线画动态光标)。
    pub custom_cursor: bool,
    /// 光标移动强度 0..1(移动时顶点外扩,静止回落)。
    pub cursor_move: f32,
    /// 光标点击强度 0..1(点击时菱形收缩变色)。
    pub cursor_click: f32,
    /// 光标位置历史(延迟轨迹,新→旧)。
    cursor_trail: Vec<(f32, f32)>,
    /// 光标动画时间。
    cursor_time: f32,
    /// 光标刚移动/动画未衰减完:即使 ui_dirty 未置位也强制 render_iced,
    /// 否则暂停时(画面静止)光标会冻结在帧里。
    pub cursor_dirty: bool,
    /// 面板进度动画中已重绘 iced(避免重复重绘)。
    last_anim_iced: bool,
    pub messages: Vec<OverlayMessage>,
    timeline_click: Option<f32>,
    layer_click: Option<f32>,
    pub tl_scroll: f32,
    pub tl_zoom: f32,
    /// 时间轴是否跟随播放头滚动。手动滚轮滚动会置 false(视图停住),
    /// seek 时重新置 true。
    pub tl_follow: bool,
    /// 时间轴视图参数(tl_scroll/tl_zoom)被手动改动后置位,强制下一帧
    /// 全量重绘面板(否则 fast path 只画 playhead,网格/内容停留在旧视图)。
    timeline_dirty: bool,
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
            tool_hover: None, selected_tool: 0, tool_hover_progress: [0.0; 5],
            panel_defs: Vec::new(), bpm_form: None, bpm_hover: None, settings_form: None, settings_hover: None, line_list: None, line_list_hover: None, chart_grid: None, chart_grid_hover: None, tl_worker: Some(timeline_draw::TimelineWorker::new(w.max(1), h.max(1))), perf_hint: false, custom_cursor: false, cursor_move: 0.0, cursor_click: 0.0, cursor_trail: Vec::new(), cursor_time: 0.0, cursor_dirty: false, last_anim_iced: false, messages: Vec::new(), timeline_click: None,
            layer_click: None, tl_scroll: 0.0, tl_zoom: 8.0, tl_follow: true, gui_scale: 1.0,
            timeline_dirty: false,
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
        // 尺寸变化 → 重建 worker(像素缓冲大小跟随)。
        self.tl_worker = Some(timeline_draw::TimelineWorker::new(w, h));
    }

    pub fn bind_group(&self) -> &wgpu::BindGroup { &self.timeline_bg }
    pub fn iced_bind_group(&self) -> &wgpu::BindGroup { &self.iced_bg }
    pub fn props_progress(&self) -> f32 { self.panel_progress }
    pub fn handle_cursor(&mut self, x: f64, y: f64) {
        let _s = trace_span!("handle_cursor");
        self.mouse_pos = Some((x as f32, y as f32));
        // 光标移动:置 1(animate_all 衰减,实现"移动外扩→静止回落")。
        self.cursor_move = 1.0;
        // 光标刚动过:暂停时也强制下一帧 render_iced(光标画在帧里)。
        self.cursor_dirty = self.custom_cursor;
        // 光标轨迹(延迟跟随):推新位置,保留最近 30 个。
        self.cursor_trail.insert(0, (x as f32, y as f32));
        if self.cursor_trail.len() > 30 {
            self.cursor_trail.pop();
        }
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
        let _bg_w = QP_W * s + bg_extra;
        for i in 0..5 {
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

    fn panel_x(&self, _props: f32, panel_w: f32, progress: f32) -> f32 {
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
        self.timeline_dirty = true;
    }

    /// Scroll the timeline (mouse wheel). `delta` is the notch value.
    /// Manual scrolling stops the playhead auto-follow (`tl_follow = false`)
    /// so the view doesn't snap back next frame.
    pub fn timeline_scroll(&mut self, delta: f32) {
        self.tl_follow = false;
        self.tl_scroll = (self.tl_scroll - delta * self.tl_zoom * 0.15).max(0.0);
        self.timeline_dirty = true;
    }

    /// Snap the timeline scroll position to the beat grid (`snap` in beats,
    /// e.g. 0.25) so the window top aligns with a snap boundary after wheel
    /// scrolling.
    pub fn snap_timeline_scroll(&mut self, snap: f32) {
        let s = snap.max(0.0001);
        self.tl_scroll = (self.tl_scroll / s).round() * s;
        self.timeline_dirty = true;
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
                if mx >= bx(1) && mx < bx(1) + btn_w { self.messages.push(OverlayMessage::ToggleNotes); return; }
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
        let (_panel_px, _panel_w, _progress) = if self.is_over_events(props) {
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
        self.timeline_dirty = false; // full redraw covers any pending view change
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
        if std::env::var("PHIMAKOR_ICED_CHECK").is_ok() {
            // 诊断:统计非透明像素(排除面板区域,面板在右侧)。
            let opaque = iced_data.chunks_exact(4)
                .filter(|p| p[3] > 0).count();
            let w = self.w as usize;
            // 左侧区域(非面板)的透明情况
            let left_opaque = iced_data.chunks_exact(4).enumerate()
                .filter(|(i, p)| (i % w) < (w / 3) && p[3] > 0).count();
            eprintln!("iced cache: opaque={opaque} (left-third opaque={left_opaque})");
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &self.iced_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &iced_data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
            wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
        );
        // Clear working pixmap and draw timeline → overlay texture
        self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
        self.upload_timeline_to(queue, info);
        // 光标动画(move/click 衰减)未结束前保持 dirty,下一帧继续渲染,
        // 否则暂停时动画只衰减不绘制,光标冻结在旧帧里。
        self.cursor_dirty = self.custom_cursor && (self.cursor_move > 0.01 || self.cursor_click > 0.01);
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
        // 标题界面也画自定义光标(设置开启时)。
        if self.custom_cursor {
            if let Some((mx, my)) = self.mouse_pos {
                timeline_draw::draw_custom_cursor(
                    &mut self.pixmap.as_mut(), mx, my, gui_scale,
                    self.cursor_move, self.cursor_click, &self.cursor_trail, self.cursor_time,
                );
            }
        }
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

    /// 加载界面:黑底 + 谱名 + 组件库 ProgressBar 动画。切谱面后台加载
    /// 期间渲染(PMCORE 加载屏)。
    pub fn render_loading(&mut self, queue: &wgpu::Queue, name: &str, progress: f32, gui_scale: f32) {
        let _s = trace_span!("render_loading");
        self.pixmap.fill(tiny_skia::Color::BLACK);
        self.iced_cache.fill(tiny_skia::Color::TRANSPARENT);
        let vw = self.w as f32;
        let vh = self.h as f32;
        let s = gui_scale;
        let theme = widgets::Theme::default().scaled(s);
        let mut cv = bpm_panel::SkiaCanvas { pm: &mut self.pixmap.as_mut() };
        // 谱名(居中)
        let name = if name.is_empty() { "Loading…" } else { name };
        let name_w = cv.text_width(name, 22.0 * s);
        cv.text(name, (vw - name_w) * 0.5, vh * 0.5 - 30.0 * s, 22.0 * s, [230, 230, 235]);
        // 进度条(组件库)
        let bw = 360.0 * s;
        let bx = (vw - bw) * 0.5;
        let by = vh * 0.5 - 10.0 * s;
        let bar = widgets::ProgressBar::new(bx, by, bw, 20.0 * s, "loading", progress);
        bar.draw(&mut cv, &theme, None);
        bar.draw_overlay(&mut cv, &theme, None);
        // 百分比文本
        cv.text(&format!("{:.0}%", progress * 100.0), (vw - 30.0 * s) * 0.5 + bw * 0.5, by + 24.0 * s, 12.0 * s, [130, 130, 140]);
        self.iced_cache.data_mut().copy_from_slice(self.pixmap.data());
        for (tex, data) in [(&self.iced_tex, self.iced_cache.data()), (&self.timeline_tex, self.pixmap.data())] {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
                wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
            );
        }
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
            if self.tl_follow {
                self.tl_scroll = (info.chart_beat as f32 - self.tl_zoom * 0.1).max(0.0);
            } else {
                // 手动滚动后播放头跑出可视窗口 → 重新跟随播放头。
                let b = info.chart_beat as f32;
                if b < self.tl_scroll || b > self.tl_scroll + self.tl_zoom {
                    self.tl_follow = true;
                }
            }
            let (scroll, zoom) = (self.tl_scroll as f64, self.tl_zoom as f64);
            let (min_b, _max_b) = (scroll, scroll + zoom);
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
        if playing || anim_moving || self.timeline_dirty {
            self.timeline_dirty = false;
            // 面板进度动画中:iced 布局随 progress 变化,需同步重绘,
            // 否则面板关闭后内容定格残留(透明区留存面板名)。
            if anim_moving && !self.last_anim_iced {
                self.render_iced(queue, info);
                self.last_anim_iced = true;
                return;
            }
            if !anim_moving {
                self.last_anim_iced = false;
            }
            self.upload_timeline_to(queue, info);
            self.last_drawn_beat = info.chart_beat;
            return;
        }
        self.last_anim_iced = false;
        // Restore base content (no playhead) — memcpy, much cheaper than redraw.
        // 若 worker 有新帧(播放→静态切换),同步到 base_pixmap 一次。
        if let Some(worker) = &mut self.tl_worker {
            worker.poll();
            if worker.has_frame() {
                let data = worker.pixels();
                self.base_pixmap.data_mut().copy_from_slice(data);
            }
        }
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
            if self.tl_follow {
                self.tl_scroll = (info.chart_beat as f32 - self.tl_zoom * 0.1).max(0.0);
            } else {
                // 手动滚动后播放头跑出可视窗口 → 重新跟随播放头。
                let b = info.chart_beat as f32;
                if b < self.tl_scroll || b > self.tl_scroll + self.tl_zoom {
                    self.tl_follow = true;
                }
            }
            let (scroll, zoom) = (self.tl_scroll as f64, self.tl_zoom as f64);
            let (min_b, _max_b) = (scroll, scroll + zoom);
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
        // 光标移动/点击强度:handle_cursor 置 1,mouse 按下置 1,这里衰减。
        self.cursor_move *= 0.94;
        if self.cursor_move < 0.01 { self.cursor_move = 0.0; }
        self.cursor_click *= 0.90;
        if self.cursor_click < 0.01 { self.cursor_click = 0.0; }
        self.cursor_time += 1.0 / 60.0;
    }

    fn animate_tool_hover(&mut self) {
        for i in 0..5 {
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
        // PMCORE-55:绘制挪到 worker 线程(纯函数 draw_timeline_pixmap)。
        // 主线程:animate + 快照 → 发 job → 收上一帧结果 → 上传 GPU。
        // 像素零拷贝:直接上传 worker 的 pending,不走 pixmap 中转。
        self.animate_all();
        self.notes_cache = info.notes.clone();
        if self.tl_visible && self.tl_follow {
            self.tl_scroll = (info.chart_beat as f32 - self.tl_zoom * 0.1).max(0.0);
        }
        let st = timeline_draw::TimelineDrawState::from_overlay(self);
        // 手动滚动后的重新跟随由 worker 内快照处理,主线程同步一次。
        if !st.tl_follow {
            let b = info.chart_beat as f32;
            if b < st.tl_scroll || b > st.tl_scroll + st.tl_zoom {
                self.tl_follow = true;
            }
        }
        let Some(worker) = &mut self.tl_worker else { return };
        // 发最新帧给 worker;收 worker 完成的上一帧(零拷贝,移动 Vec)。
        worker.submit(st, info.clone());
        worker.poll();
        if worker.has_frame() {
            // 播放中 base_pixmap 不需要同步(静态 fast path 才用,
            // 由 redraw_timeline 在切静态时同步一次)。
            let data = worker.pixels();
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &self.timeline_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
                wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
            );
        } else {
            // 首帧 worker 未完成:上传空白(避免显示旧谱残留)。
            self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
            let data = self.pixmap.data();
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &self.timeline_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
                wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
            );
        }
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










