//! 时间轴 + 面板的纯绘制函数(PMCORE-55 第一阶段)。
//!
//! 把 `IcedOverlay::upload_timeline_to` 的 tiny_skia 绘制拆成纯函数:
//! 输入 = [`TimelineDrawState`](绘制状态快照)+ [`GameInfo`](帧数据),
//! 输出 = 完成的 Pixmap。不依赖 overlay 的 self 状态 → 可单测、可跨线程
//! (PMCORE-55 第二阶段 worker 线程直接调用)。

use tiny_skia::{Paint, Pixmap, PixmapMut, Rect, Transform};

use super::model::GameInfo;
use super::panels::PanelDef;
use super::panel_ui::{draw_effects_panel, draw_panel_def, draw_quick_panel};
use super::primitives::fill_rect_clipped;
use super::timeline::{draw_5col_timeline, draw_notes_timeline, PANEL_W, QP_W, TL_W, NT_W};
use super::widgets::{Area, KeyValueGrid, RealtimeForm, ScrollList, Theme, Widget, Canvas as _};
use super::{bpm_panel, draw_menu};
use phimakor::trace_span;

/// 绘制状态快照(从 IcedOverlay 提取,跨线程安全)。
#[derive(Clone)]
pub struct TimelineDrawState {
    pub w: u32,
    pub h: u32,
    pub gui_scale: f32,
    pub tl_scroll: f32,
    pub tl_zoom: f32,
    pub tl_follow: bool,
    pub tl_visible: bool,
    pub tool_hover: Option<usize>,
    pub selected_tool: usize,
    pub tool_hover_progress: [f32; 5],
    pub panel_progress: f32,
    pub events_progress: f32,
    pub notes_progress: f32,
    pub select_start: Option<(f32, f32)>,
    pub select_end: Option<(f32, f32)>,
    pub ctx_pos: Option<(f32, f32)>,
    pub ctx_progress: f32,
    /// 面板定义(tool 0/1 的配置信息)。
    pub panel_defs: Vec<PanelDef>,
    /// 组件库面板实例(每帧由 main.rs 构建)。
    pub bpm_form: Option<RealtimeForm>,
    pub bpm_hover: Option<Area>,
    pub settings_form: Option<RealtimeForm>,
    pub settings_hover: Option<Area>,
    pub chart_grid: Option<KeyValueGrid>,
    pub chart_grid_hover: Option<Area>,
    pub line_list: Option<ScrollList>,
    pub line_list_hover: Option<Area>,
}

impl TimelineDrawState {
    /// 从 overlay 提取快照(替代 upload_timeline_to 里逐字段读取)。
    pub fn from_overlay(o: &super::IcedOverlay) -> Self {
        Self {
            w: o.w, h: o.h, gui_scale: o.gui_scale,
            tl_scroll: o.tl_scroll, tl_zoom: o.tl_zoom, tl_follow: o.tl_follow,
            tl_visible: o.tl_visible,
            tool_hover: o.tool_hover, selected_tool: o.selected_tool,
            tool_hover_progress: o.tool_hover_progress,
            panel_progress: o.panel_progress,
            events_progress: o.events_progress,
            notes_progress: o.notes_progress,
            select_start: o.select_start, select_end: o.select_end,
            ctx_pos: o.ctx_pos, ctx_progress: o.ctx_progress,
            panel_defs: o.panel_defs.clone(),
            bpm_form: o.bpm_form.clone(), bpm_hover: o.bpm_hover,
            settings_form: o.settings_form.clone(), settings_hover: o.settings_hover,
            chart_grid: o.chart_grid.clone(), chart_grid_hover: o.chart_grid_hover,
            line_list: o.line_list.clone(), line_list_hover: o.line_list_hover,
        }
    }
}

/// 纯绘制:时间轴 + 面板 → Pixmap(不依赖任何 self 状态)。
/// 调用方负责把结果上传 GPU。
pub fn draw_timeline_pixmap(state: &TimelineDrawState, info: &GameInfo) -> Pixmap {
    let _s = trace_span!("draw_timeline_pixmap");
    let mut pm = Pixmap::new(state.w.max(1), state.h.max(1)).unwrap();
    pm.fill(tiny_skia::Color::TRANSPARENT);
    let mut st = state.clone();
    st.draw_into(&mut pm, info);
    pm
}

impl TimelineDrawState {
    /// 把当前快照绘制进 pixmap(与旧 upload_timeline_to 逐行等价,
    /// 只是所有 self 读取都换成快照字段)。
    fn draw_into(&mut self, pm: &mut Pixmap, info: &GameInfo) {
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

        draw_quick_panel(&mut pm.as_mut(), self.tool_hover, self.selected_tool, self.tool_hover_progress, info, qp_w, vw, vh, s);

        let props_x = vw - pp * pan_w;
        let events_x = props_x - ep * ev_w;
        let notes_x = events_x - np * nt_w;

        if self.tl_visible {
            if self.tl_follow {
                self.tl_scroll = (info.chart_beat as f32 - self.tl_zoom * 0.1).max(0.0);
            } else {
                let b = info.chart_beat as f32;
                if b < self.tl_scroll || b > self.tl_scroll + self.tl_zoom {
                    self.tl_follow = true;
                }
            }
            if info.show_notes {
                draw_notes_timeline(&mut pm.as_mut(), self.tl_scroll, self.tl_zoom, info, notes_x, vh, s);
            }
            if info.show_events {
                draw_5col_timeline(&mut pm.as_mut(), self.tl_scroll, self.tl_zoom, info, events_x, vh, s);
            }
        }
        // Selection rect + seek bar + context menu
        {

            if let (Some(s0), Some(e0)) = (self.select_start, self.select_end) {
                let mut sel = Paint::default();
                sel.set_color_rgba8(100, 150, 255, 60);
                let rx = s0.0.min(e0.0);
                let ry = s0.1.min(e0.1);
                let rw = (s0.0 - e0.0).abs();
                let rh = (s0.1 - e0.1).abs();
                if rw > 2.0 && rh > 2.0 {
                    if let Some(r) = Rect::from_xywh(rx, ry, rw, rh) {
                        fill_rect_clipped(&mut pm.as_mut(), r, &sel);
                    }
                }
            }
            if self.ctx_progress > 0.01 {
                let (mx, my) = self.ctx_pos.unwrap_or((0.0, 0.0));
                let mw = 160.0 * s; let mh = 120.0 * s;
                let alpha = (self.ctx_progress * 230.0) as u8;
                let mut mp = Paint::default();
                mp.set_color_rgba8(25, 25, 30, alpha);
                if let Some(r) = Rect::from_xywh(mx.min(vw - mw), my.min(vh - mh), mw, mh) {
                    fill_rect_clipped(&mut pm.as_mut(), r, &mp);
                }
            }
            if info.show_overlay {
                let sb_h = 12.0 * s;
                let sb_y = vh - 56.0 * s;
                let sb_x = qp_w;
                let sb_w = (props_x - sb_x).max(20.0);
                let mut sbg = Paint::default();
                sbg.set_color_rgba8(40, 45, 55, 200);
                if let Some(r) = Rect::from_xywh(sb_x, sb_y, sb_w, sb_h) {
                    fill_rect_clipped(&mut pm.as_mut(), r, &sbg);
                }
                let prog = (info.chart_time / info.duration.max(0.01)) as f32;
                if prog > 0.01 {
                    let mut fp = Paint::default();
                    fp.set_color_rgba8(100, 180, 255, 200);
                    if let Some(r) = Rect::from_xywh(sb_x + 1.0 * s, sb_y + 1.0 * s, (sb_w - 2.0 * s) * prog.min(1.0), (sb_h - 2.0 * s).max(1.0)) {
                        fill_rect_clipped(&mut pm.as_mut(), r, &fp);
                    }
                }
            }
        }
        // Panel definition matching selected tool
        if info.show_properties && pp > 0.01 {
            if self.selected_tool == 3 {
                draw_effects_panel(&mut pm.as_mut(), info, props_x, vw, vh, s);
            } else if self.selected_tool == 4 {
                if let Some(form) = &self.bpm_form {
                    bpm_panel::draw_bpm_panel(&mut pm.as_mut(), form, self.bpm_hover.as_ref(), s);
                }
            } else if self.selected_tool == 2 {
                if let Some(form) = &self.settings_form {
                    bpm_panel::draw_bpm_panel(&mut pm.as_mut(), form, self.settings_hover.as_ref(), s);
                }
            } else if self.selected_tool == 0 {
                if let Some(grid) = &self.chart_grid {
                    let mut cv = bpm_panel::SkiaCanvas { pm: &mut pm.as_mut() };
                    let theme = Theme::default().scaled(s);
                    grid.draw(&mut cv, &theme, self.chart_grid_hover.as_ref());
                    grid.draw_overlay(&mut cv, &theme, self.chart_grid_hover.as_ref());
                }
            } else if self.selected_tool == 1 {
                let idx = self.selected_tool.min(self.panel_defs.len().max(1) - 1);
                if let Some(def) = self.panel_defs.get(idx) {
                    draw_panel_def(&mut pm.as_mut(), def, info, props_x, vw, vh, s);
                }
                if let Some(list) = &self.line_list {
                    let mut cv = bpm_panel::SkiaCanvas { pm: &mut pm.as_mut() };
                    let theme = Theme::default().scaled(s);
                    list.draw(&mut cv, &theme, self.line_list_hover.as_ref());
                    list.draw_overlay(&mut cv, &theme, self.line_list_hover.as_ref());
                }
            } else {
                let idx = self.selected_tool.min(self.panel_defs.len().max(1) - 1);
                if let Some(def) = self.panel_defs.get(idx) {
                    draw_panel_def(&mut pm.as_mut(), def, info, props_x, vw, vh, s);
                }
            }
        }
        if info.show_menu {
            draw_menu(&mut pm.as_mut(), vw, vh, s);
        }
    }
}












