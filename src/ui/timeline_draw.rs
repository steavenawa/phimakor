//! 时间轴 + 面板的纯绘制函数(PMCORE-55 第一阶段)。
//!
//! 把 `IcedOverlay::upload_timeline_to` 的 tiny_skia 绘制拆成纯函数:
//! 输入 = [`TimelineDrawState`](绘制状态快照)+ [`GameInfo`](帧数据),
//! 输出 = 完成的 Pixmap。不依赖 overlay 的 self 状态 → 可单测、可跨线程
//! (PMCORE-55 第二阶段 worker 线程直接调用)。

use tiny_skia::{Paint, Pixmap, PixmapMut, Rect, Transform};

use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread::JoinHandle;

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
    /// 右上角性能提示开关(设置里开启)。
    pub perf_hint: bool,
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
            perf_hint: o.perf_hint,
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

/// 画进已有 Pixmap(worker 复用缓冲用,不新建)。
pub fn draw_timeline_pixmap_into(state: &TimelineDrawState, info: &GameInfo, pm: &mut Pixmap) {
    let _s = trace_span!("draw_timeline_pixmap");
    pm.fill(tiny_skia::Color::TRANSPARENT);
    let mut st = state.clone();
    st.draw_into(pm, info);
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
        // 右上角性能提示(PMCORE-68):设置开启 + 播放中 + 帧延迟过大才显示。
        // 极小英文文字,画在时间轴/面板之上(worker 管线内,非阻塞)。
        if self.perf_hint && info.playing && info.frame_latency_ms > 25.0 {
            let txt = format!("frame {:.0}ms / {:.0}fps", info.frame_latency_ms, info.fps);
            let size = 9.0 * s;
            let tw = {
                let mut cv = bpm_panel::SkiaCanvas { pm: &mut pm.as_mut() };
                cv.text_width(&txt, size)
            };
            let x = (vw - tw - 6.0 * s).max(4.0);
            let y = 14.0 * s;
            // 半透明黑底,保证可读
            let mut bgp = Paint::default();
            bgp.set_color_rgba8(0, 0, 0, 140);
            if let Some(r) = Rect::from_xywh(x - 2.0 * s, y - size, tw + 4.0 * s, size + 4.0 * s) {
                fill_rect_clipped(&mut pm.as_mut(), r, &bgp);
            }
            // 帧延迟大 → 红色,轻微 → 黄
            let color = if info.frame_latency_ms > 50.0 { [255, 90, 90] } else { [255, 210, 90] };
            let mut cv = bpm_panel::SkiaCanvas { pm: &mut pm.as_mut() };
            cv.text(&txt, x, y, size, color);
        }
    }
}

// ── 绘制 worker(PMCORE-55 第二阶段)──

/// 绘制请求:状态快照 + 帧数据(均为 Send)。
struct DrawJob {
    state: TimelineDrawState,
    info: GameInfo,
}

/// 时间轴绘制 worker:后台线程跑 [`draw_timeline_pixmap`](最重的 CPU 活),
/// 主线程只收完成的像素并上传 GPU。双缓冲:worker 画 N+1 帧时主线程上传 N 帧。
pub struct TimelineWorker {
    tx: SyncSender<DrawJob>,
    rx: Receiver<Vec<u8>>,
    /// 上一帧完成的像素(主线程上传源)。
    pending: Vec<u8>,
    w: u32,
    h: u32,
    handle: Option<JoinHandle<()>>,
}

impl TimelineWorker {
    /// 启动 worker(持有一个线程 + 两条 channel)。
    pub fn new(w: u32, h: u32) -> Self {
        // 容量 1:worker 忙时 submit 丢弃旧帧(try_send),保持最新。
        let (job_tx, job_rx) = sync_channel::<DrawJob>(1);
        let (out_tx, out_rx) = channel::<Vec<u8>>();
        let handle = std::thread::Builder::new()
            .name("timeline-draw".into())
            .spawn(move || {
                // 复用 Pixmap(避免每帧 new/drop 的对齐分配尖峰),
                // 发送时 to_vec(唯一分配,普通分配器)。
                let mut pms = [
                    Pixmap::new(w, h).unwrap(),
                    Pixmap::new(w, h).unwrap(),
                ];
                let mut idx = 0usize;
                while let Ok(job) = job_rx.recv() {
                    draw_timeline_pixmap_into(&job.state, &job.info, &mut pms[idx]);
                    if out_tx.send(pms[idx].data().to_vec()).is_err() {
                        break;
                    }
                    idx = 1 - idx;
                }
            })
            .ok();
        Self {
            tx: job_tx,
            rx: out_rx,
            pending: Vec::new(),
            w,
            h,
            handle,
        }
    }

    /// 提交一帧绘制请求(最新快照 + 帧数据)。若 worker 忙(channel 满),
    /// 丢弃旧 job(只保留最新)。
    pub fn submit(&self, state: TimelineDrawState, info: GameInfo) {
        // try_send:worker 忙时丢弃(双缓冲最多滞后一帧,保持最新)。
        let _ = self.tx.try_send(DrawJob { state, info });
    }

    /// 取回 worker 完成的像素(若可用),存为 pending。返回是否有新结果。
    pub fn poll(&mut self) -> bool {
        if let Ok(raw) = self.rx.try_recv() {
            if raw.len() == (self.w as usize * self.h as usize * 4) {
                self.pending = raw;
                return true;
            }
        }
        false
    }

    /// 主线程上传源:上一帧完成的像素(无结果时为全透明)。
    pub fn pixels(&self) -> &[u8] {
        &self.pending
    }

    /// 是否已有可上传的像素。
    pub fn has_frame(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 停止 worker。
    pub fn shutdown(mut self) {
        drop(self.tx);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}














