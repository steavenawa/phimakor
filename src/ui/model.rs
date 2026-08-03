//! Display data models + iced UI builder.

use super::timeline::{PANEL_W, QP_W, TL_W, NT_W};

use iced::{Element, Length, Theme};
use std::sync::Arc;
use iced_tiny_skia::Renderer;

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

/// One row in the Eff panel's effect list (all effects, sorted by start beat).
#[derive(Clone)]
pub struct EffectRow {
    /// Index of this effect in `ExtraRoot::effects` (edits map back through it).
    pub index: usize,
    /// Shader name (built-in or custom file name).
    pub shader: String,
    /// Start/end position in beats.
    pub start_beats: f64,
    pub end_beats: f64,
    /// Whether the effect applies to the whole frame.
    pub global: bool,
    /// Whether the effect is active at the current playhead beat.
    pub active: bool,
    /// Uniform variables: (name, display value). A plain number is editable
    /// with the wheel; keyframed values show "N kf" (read-only).
    pub vars: Vec<(String, String)>,
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
    /// All post-processing effects, sorted by start beat (Eff panel list).
    pub effects: Arc<Vec<EffectRow>>,
    /// Selected row index into `effects`, plus the edit field under the
    /// wheel: 0 = shader, 1 = start, 2 = end, 3 = global.
    pub selected_effect: Option<usize>,
    pub eff_edit_field: u8,
    /// Eff panel double-click numeric input: (field id, typed buffer) or None.
    pub num_edit: Option<(u8, String)>,
    /// Eff keyframe editor: expanded var index (into sorted var names),
    /// selected keyframe row, and the parsed rows for display.
    pub eff_kf_var: Option<usize>,
    pub eff_kf_sel: Option<usize>,
    pub eff_kf_rows: Vec<KfRow>,
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






