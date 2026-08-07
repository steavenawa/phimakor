#![allow(dead_code)] // 数据模型/绘制库: 字段与函数多为面板模板备用
//! Display data models + iced UI builder.

use super::timeline::{PANEL_W, QP_W, TL_W, NT_W};

use crate::core::model::RPENote;
use iced::{Element, Length, Theme};
use std::sync::Arc;
use iced_tiny_skia::Renderer;

#[derive(Clone)]
pub struct EventEntry {
    pub layer: usize, pub kind: String, #[allow(dead_code)] pub index: usize,
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
    pub scale: f32,      // note size multiplier
    /// True when this note carries a comment (PMCORE-77, timeline marker).
    pub comment: bool,
}

impl NoteEntry {
    /// RPENote → 面板条目(索引 = 列表行号,由调用方给出)。
    /// 用 `crate::core` 而非 `phimakor::core`:measure bin 以 #[path]
    /// 引入本地 core 保持类型一致(见 measure.rs 头注释)。
    pub fn from_rpe_note(n: &RPENote, index: usize) -> Self {
        Self {
            index,
            kind: n.kind,
            start_beats: n.start_time.beats(),
            end_beats: n.end_time.beats(),
            x: n.position_x,
            scale: n.size,
            comment: n.comment.is_some(),
        }
    }
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
    // 选中线实时位姿/属性(Line 面板模板键,main.rs 每帧填)。
    m.insert("line_x", format!("{:.1}", info.line_x));
    m.insert("line_y", format!("{:.1}", info.line_y));
    m.insert("line_rot", format!("{:.0} deg", info.line_rot));
    m.insert("line_alpha", format!("{:.2}", info.line_alpha));
    m.insert("line_notes", format!("{}", info.line_notes_total));
    m.insert("line_parent", info.line_parent.map(|p| format!("L{p}")).unwrap_or_else(|| "—".into()));
    m.insert("line_cover", if info.line_cover { "ON" } else { "OFF" }.to_string());
    m.insert("line_below", if info.line_below { "ON" } else { "OFF" }.to_string());
    m
}

/// One keyframe row of an expanded Eff-panel uniform variable.
#[derive(Clone)]
pub struct KfRow {
    pub start_beats: f64,
    pub end_beats: f64,
    pub v1: f32,
    pub v2: f32,
    pub easing: i32,
}

#[derive(Clone, Default)]
pub struct GameInfo {
    pub chart_time: f64, pub chart_beat: f64, pub audio_time: f64, pub fps: f64,
    /// 主线程帧延迟(ms,性能提示用)。
    pub frame_latency_ms: f32,
    /// 是否正在播放(性能提示只在播放时显示)。
    pub playing: bool,
    pub combo: u32, pub hits: u32, pub note_count: usize, pub score: u32,
    pub lines: usize, pub visible_notes: usize, pub paused: bool, pub dim: f32,
    pub chart_name: String, pub composer: String, pub level: String, pub difficulty: f32,
    pub offset: f32, pub duration: f64,
    pub show_overlay: bool, pub show_properties: bool, pub show_events: bool,
    pub show_notes: bool, pub events_progress: f32, pub notes_progress: f32,
    pub has_custom_tex: bool, pub full_notes: bool,
    pub selected_line: usize, pub line_name: String, pub line_count: usize,
    /// 选中线实时位姿(Line 面板 Edit 区模板值,每帧从 frame 填)。
    pub line_x: f32,
    pub line_y: f32,
    /// 旋转(度)。
    pub line_rot: f32,
    pub line_alpha: f32,
    /// 选中线音符总数(doc 层)。
    pub line_notes_total: usize,
    /// 选中线父线(索引)。
    pub line_parent: Option<usize>,
    /// 选中线封面色/下排音符标记。
    pub line_cover: bool,
    pub line_below: bool,
    pub selected_layer: usize, pub max_layers: usize, pub events: Arc<Vec<EventEntry>>,
    pub notes: Arc<Vec<NoteEntry>>,
    /// 当前线选中音符(线内索引,PMCORE-18 高亮绘制用)。
    pub selected_notes: Arc<Vec<usize>>,
    /// 事件时间轴框选集合(扁平索引,PMCORE-20 批量高亮/删除/平移用)。
    pub selected_events: Arc<Vec<usize>>,
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
    /// Eff panel double-click numeric input (keyframe 行): (field id, typed
    /// buffer) or None. Eff 字段行的打字已迁移到 RealtimeForm(PMCORE-59)。
    pub num_edit: Option<(u8, String)>,
    /// Eff keyframe editor: expanded var index (into sorted var names),
    /// selected keyframe row, and the parsed rows for display.
    pub eff_kf_var: Option<usize>,
    pub eff_kf_sel: Option<usize>,
    pub eff_kf_rows: Vec<KfRow>,

    // ── A-B loop (PMCORE-22) ──
    /// 循环开关。
    pub loop_on: bool,
    /// A 点(拍,时间轴高亮带用)。
    pub loop_a: Option<f64>,
    /// B 点(拍)。
    pub loop_b: Option<f64>,
    /// A/B 对应的 chart 秒(seek bar 高亮带用)。
    pub loop_a_time: Option<f64>,
    pub loop_b_time: Option<f64>,
    /// 短暂提示 toast:(文本, 剩余秒)。过期后 main.rs 触发全量重绘擦除。
    pub loop_toast: Option<(String, f32)>,
    /// 当前选中线是否带注释(PMCORE-77,时间轴头部标记)。
    pub line_comment: bool,
    /// 进行中的注释编辑:(目标标签, 已输入文本);Some 时绘制输入框。
    pub comment_edit: Option<(String, String)>,
}

pub(crate) fn build_ui<'a>(info: &'a GameInfo, panel: f32) -> Element<'a, (), Theme, Renderer> {
    use iced::widget::{container, text};
    let s = info.gui_scale;

    let header_fmt = format!("Line #{} {}", info.selected_line, info.line_name);
    let _tl_w = TL_W * s;

    // Animated panel widths for smooth slide
    let ev_w = TL_W * s * info.events_progress;
    let nt_w = NT_W * s * info.notes_progress;
    let notes_panel: Element<'_, (), Theme, Renderer> = container(text(header_fmt.clone()).size(iced::Pixels(12.0 * s)))
        .width(nt_w).height(Length::Fill)
        .into();
    let events_panel: Element<'_, (), Theme, Renderer> = container(text("Events").size(iced::Pixels(12.0 * s)))
        .width(ev_w).height(Length::Fill)
        .into();

    let props_w = PANEL_W * s * panel;
    let props: Element<'_, (), Theme, Renderer> = container(iced::widget::Column::new()
        .push(container(iced::widget::Column::new()).height(Length::Fill))
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






