use iced::advanced::layout::{Layout, Limits};
use iced::advanced::widget::Tree;
use iced::advanced::{renderer, Renderer as _};
use iced::{Element, Point, Rectangle, Size, Theme};
use iced_tiny_skia::Renderer;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub mod panels;
pub mod widgets;
use widgets::Widget as _;
use widgets::Canvas as _;
use phimakor::trace_span;
use winit::keyboard::ModifiersState;

use self::flow::{AreaId, AreaKind, ButtonState, PanelTransform, UIFlow};

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
pub mod eff_panel;
pub mod flow;
pub mod font;
pub mod menu;
pub mod model;
pub mod panel_ui;
pub mod primitives;
pub mod settings;
pub mod splash;
pub mod text;
pub mod timeline;
pub mod timeline_draw;

#[allow(unused_imports)] // re-export: 各 bin(phimakor/measure/ui_kit)按需使用
pub use font::font_mem_bytes;
#[allow(unused_imports)]
pub use model::{EventEntry, GameInfo, KfRow, NoteEntry};
#[allow(unused_imports)]
pub use primitives::fill_rect_clipped;
#[allow(unused_imports)]
pub use settings::{backend_cycle, SettingsData};
#[allow(unused_imports)]
pub use splash::{draw_splash, filter_charts, splash_hit_test, splash_new_input_rect, splash_search_rect, ChartEntry, NewChartDlg, SplashData, SplashHover};
#[allow(unused_imports)]
pub use timeline::{PANEL_W, QP_W};
use self::primitives::hline;
use self::timeline::{COL_GAP, COL_W, HEADER_H, NT_W, TL_W};
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
    /// Toggle the note-preview panel (quick toolbar button).
    ToggleNotes,
    /// 音符面板右键菜单"删除音符"(PMCORE-17)。
    DeleteNote,
    /// 事件时间轴右键菜单"删除事件"(PMCORE-20)。
    DeleteEvent,
    MenuSave,
    MenuLoad,
    MenuExport,
    MenuQuit,
    /// 音符面板空白单击 → seek 到该拍(PMCORE-22)。
    SeekToBeat(f64),
    /// 音符面板放置(mania 网格批次 1):双击空白 / 拖拽松手。start/end 已
    /// 吸附,end>start = hold(kind 2),否则 = 当前类型;x 已 clamp。经
    /// doc.add_note 单 undo op(main.rs 处理)。
    PlaceNote { start: f64, end: f64, x: f32, kind: u8 },
    /// 右键菜单:设 A 点(PMCORE-22)。
    SetLoopA,
    /// 右键菜单:设 B 点(PMCORE-22)。
    SetLoopB,
    /// 右键菜单:编辑注释(PMCORE-77)。目标由 main.rs 按 ctx_on_events /
    /// ctx_note_hit + selected_note 解析(音符注释或判定线注释)。
    EditComment,
}

// ── hover 浮层(PMCORE-76)──

/// 时间轴 hover 浮层的目标。仅用作文本重建缓存键:key 或数据指纹变化
/// 才重建 [`IcedOverlay::tooltip_lines`],不每帧分配字符串。
#[derive(Clone, Debug, PartialEq)]
pub enum TooltipKey {
    /// 音符(NoteEntry.index,线内索引)。
    Note(usize),
    /// 事件(events_cache 扁平下标,按 end_beats 排序)。
    Event(usize),
}

/// 音符类型名(tap/hold/flick/drag,浮层标题行)。
pub fn note_kind_name(kind: u8) -> &'static str {
    match kind { 1 => "tap", 2 => "hold", 3 => "flick", 4 => "drag", _ => "note" }
}

/// 事件缓动名(RPE easingType;编辑器事件编辑钳制 0..=5,PMCORE-20)。
pub fn easing_label(e: i32) -> &'static str {
    match e {
        0 | 1 => "linear",
        2 => "sine out",
        3 => "sine in",
        4 => "quad out",
        5 => "quad in",
        6 => "sine in-out",
        7 => "quad in-out",
        _ => "ease",
    }
}

/// 构建音符浮层文本:类型 / start / end beat / position_x(PMCORE-76)。
pub fn note_tooltip_lines(n: &NoteEntry) -> Vec<String> {
    vec![
        note_kind_name(n.kind).to_string(),
        format!("start: {:.3} beat", n.start_beats),
        format!("end: {:.3} beat", n.end_beats),
        format!("x: {:.1}", n.x),
    ]
}

/// 构建事件浮层文本:kind / start / end beat / easing(PMCORE-76)。
pub fn event_tooltip_lines(e: &EventEntry) -> Vec<String> {
    vec![
        e.kind.clone(),
        format!("start: {:.3} beat", e.start_beats),
        format!("end: {:.3} beat", e.end_beats),
        format!("easing: {} ({})", e.easing, easing_label(e.easing)),
    ]
}

/// 将 raw beat 吸附到 snap 网格并保证 >= min_beats(PMCORE-17 放置/拖拽/
/// hold 尾部共用;snap<=0 时退化为只做下限钳制)。
pub(crate) fn snap_beat(raw: f64, snap: f64, min_beats: f64) -> f64 {
    let s = snap.max(0.0001);
    ((raw / s).round() * s).max(min_beats)
}

/// 事件时间轴一次点击的解析结果(PMCORE-20):命中事件 / 空白列点击。
/// `hit` 为命中的扁平事件索引(按 end_beats 排序);`beat` 为点击处 raw
/// beat(未吸附);`kind` 为点击所在列(不在 5 列内为 None)。
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineClick {
    pub hit: Option<usize>,
    pub beat: f64,
    pub kind: Option<String>,
}

/// 音符面板内容区几何(命中/绘制/ghost 共用,铁律):返回 (play_x, play_y,
/// play_w, play_h)。play_x = 面板左缘 + 横向 pad(12s);play_y = 头部底;
/// play_h = 面板可视高。与 timeline.rs 的 draw_notes_timeline 同源,增删
/// 数字两侧同步(mania 网格批次 1)。
/// 委托 flow::PanelTransform(单一真源);progress=0 时 panel_x() 还原传入值。
pub(crate) fn notes_play_rect(panel_x: f32, vh: f32, s: f32) -> (f32, f32, f32, f32) {
    PanelTransform::new(0.0, 0.0, 1, s, panel_x, vh, 0.0, 0.0).notes_play_rect()
}

/// 像素 y → beat(tl_scroll = 窗口顶拍,tl_zoom = 窗口拍高)。越界 y 钳到
/// 可见范围。与绘制 to_y 互逆(mania 网格批次 1 共用映射)。
/// 委托 flow::PanelTransform(单一真源)。
#[cfg(test)] // 仅测试仍走旧签名;生产路径一律 PanelTransform
pub(crate) fn panel_y_to_beat(my: f32, play: (f32, f32, f32, f32), scroll: f32, zoom: f32) -> f64 {
    PanelTransform::from_play(play, scroll, zoom, 1).y_to_beat(my)
}

/// beat → 像素 y(与 panel_y_to_beat 互逆;不 clamp,调用方按需)。
pub(crate) fn beat_to_panel_y(beat: f64, play: (f32, f32, f32, f32), scroll: f32, zoom: f32) -> f32 {
    PanelTransform::from_play(play, scroll, zoom, 1).beat_to_y(beat)
}

/// 像素 x → 音符 position_x(±675 线性;命中/拖拽/ghost 共用,含 pad_x)。
/// 委托 flow::PanelTransform(单一真源)。
#[cfg(test)] // 仅测试仍走旧签名;生产路径一律 PanelTransform
pub(crate) fn panel_x_to_pos_x(mx: f32, play: (f32, f32, f32, f32)) -> f32 {
    PanelTransform::from_play(play, 0.0, 0.0, 1).x_to_pos_x(mx)
}

/// 音符 position_x → 像素 x(与 panel_x_to_pos_x 互逆)。
pub(crate) fn pos_x_to_panel_x(nx: f32, play: (f32, f32, f32, f32)) -> f32 {
    PanelTransform::from_play(play, 0.0, 0.0, 1).pos_x_to_x(nx)
}

/// 分栏列中心 x(vertical_split>1 时按列显示;命中 = 绘制)。
pub(crate) fn pos_x_to_col_x(nx: f32, play: (f32, f32, f32, f32), v_split: u32) -> f32 {
    PanelTransform::from_play(play, 0.0, 0.0, v_split).pos_x_to_col_x(nx)
}

/// 拖拽放置结果(mania 网格批次 1):起点/终点均吸附;时长 < snap 退化为
/// tap。返回 (start_beat, end_beat, kind);退化为 tap 时 end == start。
pub(crate) fn drag_placement(start_raw: f64, end_raw: f64, snap: f64) -> (f64, f64, u8) {
    let s = snap.max(0.0001);
    let a = snap_beat(start_raw, s, 0.0);
    let b = snap_beat(end_raw, s, 0.0);
    let (lo, hi) = (a.min(b), a.max(b));
    if hi - lo >= s {
        (lo, hi, 2)
    } else {
        (lo, lo, 1)
    }
}

/// 音符面板空白释放的动作(mania 网格批次 1,纯逻辑可单测)。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BlankAction {
    /// 单击:seek 到该拍(吸附后)。
    Seek(f64),
    /// 放置音符(start/end 已吸附,end>start = hold;kind 为最终类型)。
    Place { start: f64, end: f64, x: f32, kind: u8 },
}

/// 合成空白释放动作:拖拽优先(hold/tap 退化);双击用第一次点击的
/// (beat, x);否则单击 seek。hold 类型双击时长 = 一个 snap。
pub(crate) fn blank_release_action(
    drag: Option<(f64, f64)>,
    double: Option<(f64, f32)>,
    kind: u8,
    snap: f64,
    click: (f64, f32),
) -> BlankAction {
    if let Some((s_raw, e_raw)) = drag {
        let (s, e, k) = drag_placement(s_raw, e_raw, snap);
        BlankAction::Place { start: s, end: e, x: click.1, kind: k }
    } else if let Some((b0, x0)) = double {
        let k = kind;
        BlankAction::Place {
            start: b0,
            end: if k == 2 { b0 + snap.max(0.0001) } else { b0 },
            x: x0,
            kind: k,
        }
    } else {
        BlankAction::Seek(click.0)
    }
}

// ── notes 面板流控区域(单一真源,flow.rs)──

/// 空白兜底区域 id(注册在最底层,反向命中时最后才落到它)。
const AREA_BLANK: AreaId = AreaId(0);
/// 音符块/hold 尾的 id = 线内索引 + 1(0 保留给空白,内容派生跨帧稳定)。
const AREA_NOTE_BASE: u32 = 1;
/// 幽灵区域 id(disabled,仅布局树)。
const AREA_GHOST: AreaId = AreaId(u32::MAX - 1);
/// 分栏列线 id 基址(disabled)。
const AREA_COL_BASE: u32 = 1_000_000;

/// 音符点击容差矩形(像素):beat ±1.0 拍、x ±150 pos_x(与旧
/// find_nearest_note 的命中阈值一致)。越界钳到面板可见区;
/// 完全在可见区外时返回空矩形(不命中)。
pub(crate) fn note_hit_rect(n: &NoteEntry, pt: &PanelTransform) -> (f32, f32, f32, f32) {
    let play = pt.notes_play_rect();
    let (px, py, pw, ph) = play;
    // beat ±1.0 → y 区间(beat 越大 y 越小)。
    let y_lo = pt.beat_to_y(n.start_beats + 1.0);
    let y_hi = pt.beat_to_y(n.start_beats - 1.0);
    let y0 = y_lo.min(y_hi).clamp(py, py + ph);
    let y1 = y_lo.max(y_hi).clamp(py, py + ph);
    // x ±150 pos_x → 像素区间。
    let x0 = pt.pos_x_to_x(n.x - 150.0).clamp(px, px + pw);
    let x1 = pt.pos_x_to_x(n.x + 150.0).clamp(px, px + pw);
    (x0.min(x1), y0, (x0 - x1).abs(), (y1 - y0).max(0.0))
}

/// 每帧重建 notes 面板流控区域(布局 rect 树一次计算,单一真源)。
/// 顺序:空白(最底)→ 音符块 / hold 尾(hold 注册 HoldTail,tap/flick/drag
/// 注册 NoteBlock)→ 幽灵(disabled)→ 分栏列线(disabled)。反向命中时
/// 后注册者在上层;幽灵/列线永不命中(纯布局)。
pub(crate) fn build_notes_areas(
    pt: &PanelTransform,
    notes: &[NoteEntry],
    ghost: Option<(f64, f64, f32, u8)>,
    vertical_split: u32,
) -> Vec<flow::HotArea> {
    use flow::HotArea;
    let s = pt.s;
    let mut areas = Vec::with_capacity(notes.len() + 8);
    // 空白兜底:几何 = is_over_notes 区域(命中 = 绘制,mania 网格批次 1)。
    let px = pt.panel_x();
    let py = HEADER_H * s + 4.0 * s;
    let ly = pt.h - 48.0 * s - 26.0 * s;
    areas.push(HotArea { id: AREA_BLANK, rect: (px, py, NT_W * s, (ly - 2.0 - py).max(0.0)), kind: AreaKind::NoteBlank, disabled: false });
    // 可见音符(与绘制同窗口:±5% 缓冲,notes_cache 按 end_beats 排序)。
    let (min_b, max_b) = (pt.scroll as f64, (pt.scroll + pt.zoom) as f64);
    let lo = min_b - pt.zoom as f64 * 0.05;
    let hi = max_b + pt.zoom as f64 * 0.05;
    let start = notes.partition_point(|n| n.end_beats < lo);
    for n in &notes[start..] {
        if n.start_beats > hi { break; }
        let rect = note_hit_rect(n, pt);
        let kind = if n.kind == 2 { AreaKind::HoldTail } else { AreaKind::NoteBlock };
        areas.push(HotArea { id: AreaId(AREA_NOTE_BASE + n.index as u32), rect, kind, disabled: false });
    }
    // 幽灵(disabled:预览块,不参与命中)。
    if let Some((gb0, gb1, gx, _)) = ghost {
        let xn = pt.pos_x_to_x(gx);
        let gy0 = pt.beat_to_y(gb0.clamp(min_b, max_b));
        let gy1 = pt.beat_to_y(gb1.clamp(min_b, max_b));
        areas.push(HotArea {
            id: AREA_GHOST,
            rect: (xn - 32.0 * s, gy0.min(gy1) - 4.0 * s, 64.0 * s, 8.0 * s),
            kind: AreaKind::Ghost,
            disabled: true,
        });
    }
    // 分栏列线(disabled:视觉竖线,不命中)。
    if vertical_split > 1 {
        let play = pt.notes_play_rect();
        let v = vertical_split as f32;
        for ci in 1..vertical_split as usize {
            let x = play.0 + ci as f32 * play.2 / v;
            areas.push(HotArea { id: AreaId(AREA_COL_BASE + ci as u32), rect: (x, play.1, 1.0, play.3), kind: AreaKind::ColumnLine, disabled: true });
        }
    }
    areas
}

/// AreaId → 线内音符索引(音符块 / hold 尾区域;空白/幽灵/列线返回 None)。
pub(crate) fn note_index_from_area(id: AreaId) -> Option<usize> {
    (id.0 >= AREA_NOTE_BASE && id.0 < AREA_COL_BASE).then(|| (id.0 - AREA_NOTE_BASE) as usize)
}

/// 当前流控区域列表中 id 对应的 kind(事件消费时区分 NoteBlock / HoldTail)。
pub(crate) fn flow_area_kind(areas: &[flow::HotArea], id: AreaId) -> Option<AreaKind> {
    areas.iter().find(|a| a.id == id).map(|a| a.kind)
}

/// 编辑器主菜单项(Menu 按钮打开;行为与旧 draw_menu 的 Save/Load/Export/
/// Quit 一致,命令名经 [`menu_command_message`] 映射回 OverlayMessage)。
pub fn editor_menu_items() -> Vec<menu::MenuItem> {
    vec![
        menu::MenuItem::new("Save (Ctrl+S)", menu::MenuAction::Command("save")),
        menu::MenuItem::new("Load", menu::MenuAction::Command("load")),
        menu::MenuItem::new("Export", menu::MenuAction::Command("export")),
        menu::MenuItem::new("Quit to Menu (Ctrl+Q)", menu::MenuAction::Command("quit")),
    ]
}

/// 音符面板/事件面板右键菜单项(PMCORE-17/20/22/77,行为 1:1)。
pub fn context_menu_items(on_events: bool) -> Vec<menu::MenuItem> {
    vec![
        menu::MenuItem::new(
            if on_events { "删除事件" } else { "删除音符" },
            menu::MenuAction::Command(if on_events { "delete_event" } else { "delete_note" }),
        ),
        menu::MenuItem::new("设 A 点 (I)", menu::MenuAction::Command("loop_a")),
        menu::MenuItem::new("设 B 点 (O)", menu::MenuAction::Command("loop_b")),
        menu::MenuItem::new("注释", menu::MenuAction::Command("comment")),
    ]
}

/// [`menu::MenuAction::Command`] 命令名 → 触发消息(菜单/键盘共用的映射表)。
pub fn menu_command_message(c: &str) -> Option<OverlayMessage> {
    Some(match c {
        "save" => OverlayMessage::MenuSave,
        "load" => OverlayMessage::MenuLoad,
        "export" => OverlayMessage::MenuExport,
        "quit" => OverlayMessage::MenuQuit,
        "delete_note" => OverlayMessage::DeleteNote,
        "delete_event" => OverlayMessage::DeleteEvent,
        "loop_a" => OverlayMessage::SetLoopA,
        "loop_b" => OverlayMessage::SetLoopB,
        "comment" => OverlayMessage::EditComment,
        _ => return None,
    })
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
    /// UI 流控层(单一真源,flow.rs):notes 面板命中/拖拽/放置统一走它。
    pub flow: UIFlow,
    /// 左键是否按下(handle_click 写入,handle_cursor 组装 buttons 快照用)。
    btn_left: bool,
    /// 最近一次修饰键(handle_click 写入;光标移动时复用)。
    flow_mods: ModifiersState,
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
    /// Eff 面板组件(组件库,tool 3,PMCORE-59)。由 main.rs 每帧构建/更新。
    pub eff_form: Option<widgets::RealtimeForm>,
    pub eff_form_hover: Option<widgets::Area>,
    /// Line 面板的实时线数据滚动列表(tool 1)。main.rs 每帧构建。
    pub line_list: Option<widgets::ScrollList>,
    pub line_list_hover: Option<widgets::Area>,
    /// Chart 面板的元数据网格(tool 0)。main.rs 每帧构建。
    pub chart_grid: Option<widgets::KeyValueGrid>,
    pub chart_grid_hover: Option<widgets::Area>,
    /// Eff 面板展开的 keyframed var(每帧由 render_iced 从 GameInfo 同步;
    /// 面板流控区域据此注册 keyframe 手写区,命中 = 绘制)。
    eff_kf_var: Option<usize>,
    /// Eff 面板展开的 keyframe 行数(同上,GameInfo.eff_kf_rows.len())。
    eff_kf_rows_n: usize,
    /// 时间轴绘制 worker(PMCORE-55):后台画,主线程只上传。
    pub tl_worker: Option<timeline_draw::TimelineWorker>,
    /// 右上角性能提示开关(设置里开启,播放帧延迟大时显示)。
    pub perf_hint: bool,
    /// 右上角常驻帧时间叠层(设置里开启,恒显示 frame ms / fps)。
    pub fps_overlay: bool,
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
    /// 当前选中音符(selected_line 内 note 索引,PMCORE-17)。
    /// 点击音符面板命中时设置,Delete/Backspace/右键菜单删除用。
    pub selected_note: Option<usize>,
    /// 多选集合(selected_line 内 note 索引,PMCORE-18)。框选/Shift+点击维护,
    /// 空 = 无多选;`selected_note` 恒为集合首项(单选时的主选中)。
    pub selected_notes: Vec<usize>,
    /// Ctrl+框选是否起手于音符面板:是才参与音符命中(事件面板起手只画
    /// 矩形,PMCORE-18 限音符)。
    select_notes: bool,
    /// 音符面板纵向分栏数(框选命中 to_col_x 用,与 timeline.rs 绘制几何
    /// 一致;每帧由 main.rs 同步)。
    pub vertical_split: u32,
    /// 全谱预览模式(每帧由 main.rs 同步):框选限当前线,full_notes 下
    /// notes_cache 的 index 跨线会重号,不参与命中(PMCORE-18)。
    pub full_notes: bool,
    /// Ctrl+拖拽 hold 尾部:进行中的 hold 索引(PMCORE-17)。
    pub drag_hold: Option<usize>,
    /// hold 尾部拖拽预览:(note_index, 吸附后的 end_beats)。
    pub hold_preview: Option<(usize, f64)>,
    /// hold 尾部拖拽松手:(note_index, 最终 end_beats),main.rs 一次提交。
    pub hold_updated: Option<(usize, f64)>,
    /// 吸附网格步长(每帧由 main.rs 同步 self.snap)。
    pub snap: f32,
    /// 当前放置音符类型(1=tap 2=hold 3=flick 4=drag;Q/W/E/R 选型,mania 批次 1)。
    pub place_kind: u8,
    /// 放置幽灵(start_beat, end_beat, position_x, kind):悬停显示当前类型
    /// 将落位置,拖拽中显示起点→终点。None = 不显示。绘制走 worker
    /// (TimelineDrawState.ghost),与放置共用同一映射(mania 批次 1)。
    pub ghost: Option<(f64, f64, f32, u8)>,
    /// 音符面板空白按下:(mx, my, 起点 raw beat)。释放时按移动距离区分
    /// 单击/双击/拖拽(mania 批次 1)。
    place_drag: Option<(f32, f32, f64)>,
    /// 上次音符面板空白单击:(时刻, mx, my, 吸附 beat, 吸附 x)。300ms 内
    /// + 8px 内再次空白单击 = 双击放置(mania 批次 1)。
    last_blank_click: Option<(Instant, f32, f32, f64, f32)>,
    pub     mouse_beat: f64,
    notes_cache: Arc<Vec<NoteEntry>>,
    /// 事件时间轴命中/框选用(与 notes_cache 同模式,每帧由 main.rs 经
    /// GameInfo 同步,PMCORE-20)。
    events_cache: Arc<Vec<EventEntry>>,
    /// 事件框选集合(扁平索引,按 end_beats 排序;批量删除/平移用,
    /// PMCORE-20)。
    pub selected_events: Vec<usize>,
    /// Ctrl+拖拽起手列的事件 kind(框选只收同列同类事件)。
    select_events_kind: Option<String>,
    /// 右键菜单是否开在事件时间轴上(菜单项"删除事件",main.rs 右键处理读)。
    pub ctx_on_events: bool,
    /// 上次右键是否命中音符(PMCORE-77:注释目标解析用,音符面板命中
    /// 音符 → 音符注释,否则 → 判定线注释)。
    pub ctx_note_hit: bool,
    last_drawn_beat: f64,
    /// 菜单宿主(P3):顶栏菜单 / 右键菜单 / 全屏菜单统一走它,命中与绘制
    /// 同源(menu.rs,几何单源);打开时独占 flow 命中。
    pub menu: menu::MenuHost,
    /// 上次上传后 GPU 上播放头水平线的 y(像素)。fast path 只重传播放头
    /// 新旧两条水平条(全宽 × ~10px),替代每帧 8MB 全屏上传(PMCORE-69)。
    last_playhead_y: Option<f32>,
    /// PMCORE-76:hover 浮层开关(每帧由 main.rs 从 SettingsData 同步)。
    pub hover_tooltip: bool,
    /// 浮层缓存:(目标 key, 数据指纹)。key/指纹变化才重建文本。
    tooltip_cache: Option<(TooltipKey, u64)>,
    /// 浮层文本缓存(首行为标题)。分配集中在重建那一刻。
    tooltip_lines: Vec<String>,
    /// 上次绘制的浮层矩形(fast path 脏区用它擦除 GPU 上的旧浮层)。
    last_tooltip_rect: Option<(f32, f32, f32, f32)>,
    /// GPU 上 iced_tex 的内容镜像(PMCORE-69 diff 上传的比对基准)。
    /// 空 = 纹理未初始化(首帧/重建),必须全量上传。
    iced_last: Vec<u8>,
    /// GPU 上 timeline_tex 的内容镜像(PMCORE-69)。与 upload_rect 同步,
    /// 保证 diff 基准恒等于 GPU 现状。
    tl_last: Vec<u8>,
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
            notes_progress: 0.0, mouse_pos: None, flow: UIFlow::new(), btn_left: false, flow_mods: ModifiersState::default(), show_overlay: true, tl_visible: false,
            tool_hover: None, selected_tool: 0, tool_hover_progress: [0.0; 5],
            panel_defs: Vec::new(), bpm_form: None, bpm_hover: None, settings_form: None, settings_hover: None, eff_form: None, eff_form_hover: None, eff_kf_var: None, eff_kf_rows_n: 0, line_list: None, line_list_hover: None, chart_grid: None, chart_grid_hover: None, tl_worker: Some(timeline_draw::TimelineWorker::new(w.max(1), h.max(1))), perf_hint: false, fps_overlay: true, custom_cursor: false, cursor_move: 0.0, cursor_click: 0.0, cursor_trail: Vec::new(), cursor_time: 0.0, cursor_dirty: false, last_anim_iced: false, messages: Vec::new(), timeline_click: None,
            layer_click: None, tl_scroll: 0.0, tl_zoom: 8.0, tl_follow: true, gui_scale: 1.0,
            timeline_dirty: false,
            select_start: None, select_end: None, selecting: false, seek_dragging: false,
            drag_note: None, drag_updated: None, selected_note: None,
            selected_notes: Vec::new(), select_notes: false, vertical_split: 1, full_notes: false,
            drag_hold: None, hold_preview: None, hold_updated: None, snap: 0.25,
            place_kind: 1, ghost: None, place_drag: None, last_blank_click: None,
            mouse_beat: 0.0, notes_cache: Arc::new(Vec::new()), events_cache: Arc::new(Vec::new()),
            selected_events: Vec::new(), select_events_kind: None, ctx_on_events: false, ctx_note_hit: false,
            last_drawn_beat: 0.0, menu: menu::MenuHost::new(),
            last_playhead_y: None,
            hover_tooltip: true, tooltip_cache: None, tooltip_lines: Vec::new(), last_tooltip_rect: None,
            iced_last: Vec::new(), tl_last: Vec::new(),
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
        // 纹理已重建:diff 快照作废,下帧必须全量上传(PMCORE-69)。
        self.iced_last.clear();
        self.tl_last.clear();
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
        // 事件层写入流控(单一真源):拖拽中 captured 钉住 hover;未按下则
        // 只更新 hover(命中几何 = flow.areas,绘制高亮不再自己重算)。
        self.rebuild_notes_flow();
        let buttons = [
            if self.btn_left { ButtonState::Pressed } else { ButtonState::Up },
            ButtonState::Up,
            ButtonState::Up,
        ];
        self.flow.on_mouse(x as f32, y as f32, &buttons, self.flow_mods);
        self.flow.resolve();
        if self.tl_visible {
            self.mouse_beat = self.y_to_beat(y as f32, 0.0);
        }
        // Track note drag
        if self.drag_note.is_some() {
            let beat = self.y_to_beat(y as f32, 0.0); // 面板外回退中间拍(原行为)
            let rel_x = self.panel_transform().x_to_pos_x(x as f32);
            if let Some((ni, ..)) = self.drag_note {
                self.drag_updated = Some((ni, beat, rel_x));
            }
        }
        // Hold 尾部拖拽(Ctrl+拖 hold):实时吸附预览 end_beats。
        if let Some(ni) = self.drag_hold {
            if let Some(start) = self.notes_cache.iter().find(|e| e.index == ni).map(|e| e.start_beats) {
                let end = snap_beat(self.y_to_beat(y as f32, 0.0), self.snap as f64, start + self.snap as f64);
                self.hold_preview = Some((ni, end));
            }
        }
        // 放置幽灵:悬停/拖拽中跟随鼠标(变化才置 timeline_dirty)。
        self.update_ghost();
        let s = self.gui_scale;
        let btn_base = 34.0 * s;
        let gap = 6.0 * s;
        let mx = x as f32;
        let my = y as f32;
        // Match draw_quick_panel: left-aligned at 4*s, hover progress for width
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

    pub(crate) fn is_over_events(&self, props: f32) -> bool {
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
    // ponytail: 与 snap_beat(f64) 同一套四舍五入逻辑;类型不同(f32 vs f64)
    // 未提取公共函数,改动任一处时保持两边一致。
    pub fn snap_timeline_scroll(&mut self, snap: f32) {
        let s = snap.max(0.0001);
        self.tl_scroll = (self.tl_scroll / s).round() * s;
        self.timeline_dirty = true;
    }

    pub fn handle_click(&mut self, pressed: bool, ctrl: bool, shift: bool) {
        let _s = trace_span!("handle_click");
        let s = self.gui_scale;
        let Some((mx, my)) = self.mouse_pos else { return };
        // 任何新按下都作废旧的空白拖拽记录(避免释放落在面板外未消费的
        // 陈旧 place_drag 污染下一次释放,mania 批次 1)。
        if pressed {
            self.place_drag = None;
        }
        // 丢释放自愈:物理上已抬起(收到新 Pressed)而流控仍认为按下 →
        // 上一次释放被吞(拖拽出窗/失焦,OS 不补发 Released)。先重置
        // 边沿并作废残留拖拽,本次按下照常产生 press 边沿 —— 否则点击
        // 静默丢失(表现为"要双击才有反应")、hover 被陈旧 captured 钉死。
        if pressed && self.flow.buttons[0] == ButtonState::Pressed {
            self.flow.force_release_all();
            self.drag_note = None;
            self.drag_hold = None;
            self.hold_preview = None;
        }
        // 菜单打开(全屏/右键/菜单条):独占处理。命中 = flow.areas(菜单区
        // 注册在 notes 之上,外部点击由最底层全窗 mask 承接 → 关闭)。
        if self.menu.is_open() {
            self.handle_menu_click(pressed, ctrl, shift, mx, my);
            return;
        }
        // Seek bar: press starts drag; release always stops (before any other handler)
        if pressed && self.is_over_seekbar() { self.seek_dragging = true; }
        if !pressed { self.seek_dragging = false; }
        if self.seek_dragging { return; }
        // Release: handle bottom bar buttons and selection end
        if !pressed {
            if self.selecting {
                self.selecting = false;
                self.finalize_box_selection();
            }
            if my >= self.h as f32 - 48.0 * s && my <= self.h as f32 && self.show_overlay {
                // 拖拽中释放落在底栏(含三个按钮):补喂流控,否则 prev_buttons
                // 停在 Pressed,下一次按下无边沿(拖拽/选择失灵)。底栏非 flow
                // 区域,Released 从 Up 触发也解析为空,无条件调用安全。
                self.feed_swallowed_release();
                let btn_w = 90.0 * s;
                let right = self.w as f32 - 10.0 * s;
                let bx = |i: usize| right - (3 - i) as f32 * (btn_w + 6.0 * s);
                if mx >= bx(0) && mx < bx(0) + btn_w { self.messages.push(OverlayMessage::ToggleEvents); return; }
                if mx >= bx(1) && mx < bx(1) + btn_w { self.messages.push(OverlayMessage::ToggleNotes); return; }
                if mx >= bx(2) && mx < bx(2) + btn_w { self.toggle_main_menu(); return; }
                return;
            }
        }
        // 事件层写入流控(单一真源):命中几何 = flow.areas。modal 分支(ctx
        // 菜单/主菜单/seekbar/底栏)已在上面 return,不消费 flow 事件。
        self.rebuild_notes_flow();
        self.btn_left = pressed;
        let mut mods = ModifiersState::empty();
        if ctrl { mods |= ModifiersState::CONTROL; }
        if shift { mods |= ModifiersState::SHIFT; }
        self.flow_mods = mods;
        let buttons = [
            if pressed { ButtonState::Pressed } else { ButtonState::Released },
            ButtonState::Up,
            ButtonState::Up,
        ];
        self.flow.on_mouse(mx, my, &buttons, mods);
        let ev = self.flow.resolve();
        // Note press(PMCORE-18 多选):命中几何 = flow.areas(单一真源)。
        // - Shift+点击:toggle 增选/减选,不启动拖拽。
        // - 点击已选中音符:保持多选集合,启动整组拖拽。
        // - 点击未选中音符:清空多选,单选该音符并启动拖拽。
        // - 点击空白:清空选择。
        if pressed && self.tl_visible {
            for &id in &ev.pressed {
                let kind = flow_area_kind(&self.flow.areas, id);
                let ni = self.note_from_flow(id);
                match (kind, ni, ctrl) {
                    // Ctrl+hold 音符 → 拖尾部(PMCORE-17);Ctrl+tap → 不消费,
                    // 落入下方 Ctrl+框选。
                    (Some(AreaKind::HoldTail), Some(ni), true) => {
                        self.drag_hold = Some(ni);
                        self.selected_note = Some(ni);
                        self.selected_notes = vec![ni];
                        // 初始化预览(按下即显示吸附位置)。
                        if let Some(start) = self.notes_cache.iter().find(|e| e.index == ni).map(|e| e.start_beats) {
                            let end = snap_beat(self.y_to_beat(my, 0.0), self.snap as f64, start + self.snap as f64);
                            self.hold_preview = Some((ni, end));
                        }
                        return;
                    }
                    // 普通点击音符块 / hold 尾(非 ctrl):选择 + 拖拽整组。
                    (Some(AreaKind::NoteBlock | AreaKind::HoldTail), Some(ni), false) => {
                        if shift {
                            // toggle:在选中集里则移除,否则加入。
                            if let Some(pos) = self.selected_notes.iter().position(|&i| i == ni) {
                                self.selected_notes.remove(pos);
                                self.selected_note = self.selected_notes.first().copied();
                            } else {
                                self.selected_notes.push(ni);
                                self.selected_notes.sort_unstable();
                                self.selected_note = Some(ni);
                            }
                            return;
                        }
                        let beat = self.y_to_beat(my, 0.0);
                        self.drag_note = Some((ni, beat, mx, my));
                        if !self.selected_notes.contains(&ni) {
                            self.selected_notes.clear();
                            self.selected_notes.push(ni);
                        }
                        self.selected_note = Some(ni);
                        return;
                    }
                    // 空白按下(非 ctrl):清空选择,记录放置起点(mania 批次 1)。
                    (Some(AreaKind::NoteBlank), _, false) => {
                        self.selected_notes.clear();
                        self.selected_note = None;
                        let beat = self.y_to_beat(my, 0.0);
                        self.place_drag = Some((mx, my, beat));
                        self.update_ghost();
                        return;
                    }
                    _ => {} // ctrl+空白 / ctrl+tap → 框选;幽灵/列线 disabled 不命中。
                }
            }
        }
        if !pressed && self.drag_note.is_some() {
            // Release: store updated position(释放解析到 captured,同一 area;
            // 越界拖出面板也提交,原行为)。y 走 y_to_beat(面板外回退中间拍)。
            let beat = self.y_to_beat(my, 0.0);
            let rel_x = self.mouse_note_x();
            if let Some((ni, ..)) = self.drag_note {
                self.drag_updated = Some((ni, beat, rel_x));
            }
            self.drag_note = None;
            return;
        }
        if !pressed && self.drag_hold.is_some() {
            // Release: 一次提交最终 end_beats(吸附 + 下限钳制)。
            if let Some(ni) = self.drag_hold {
                let end = self.hold_preview.map(|(_, e)| e).unwrap_or_else(|| {
                    let start = self.notes_cache.iter().find(|e| e.index == ni).map(|e| e.start_beats).unwrap_or(0.0);
                    snap_beat(self.y_to_beat(my, 0.0), self.snap as f64, start + self.snap as f64)
                });
                self.hold_updated = Some((ni, end));
            }
            self.drag_hold = None;
            self.hold_preview = None;
            return;
        }
        // Ctrl+click on timeline: press starts drag, release ends.
        // 音符面板起手的框选参与音符命中(select_notes);事件面板起手框选
        // 同列同类事件(select_events_kind,PMCORE-20 批量删除/平移)。
        if ctrl && self.is_over_timeline(0.0) {
            if pressed {
                self.select_start = Some((mx, my));
                self.select_end = Some((mx, my));
                self.selecting = true;
                self.select_notes = self.is_over_notes(0.0);
                self.select_events_kind = if self.is_over_events(0.0) {
                    self.event_geo(mx, my).map(|(k, _)| k)
                } else {
                    None
                };
            } else if self.selecting {
                self.select_end = Some((mx, my));
            }
            return;
        }
        // On release only: handle layer selector, tool click, timeline click
        if !pressed {
            // mania 批次 1:音符面板空白释放 → 单击 seek / 双击放置 / 拖拽放
            // hold(拖出面板外也按拖拽处理,beat/x 钳到可见范围)。
            if self.show_overlay && self.tl_visible && !ctrl {
                if let Some((mx0, my0, start_raw)) = self.place_drag.take() {
                    let s2 = self.gui_scale;
                    let cur_beat = self.y_to_beat(my, 0.0);
                    let cur_x = self.mouse_note_x().clamp(-675.0, 675.0);
                    let moved = (mx - mx0).abs() + (my - my0).abs();
                    let drag = (moved >= 6.0 * s2).then_some((start_raw, cur_beat));
                    // 双击判定:300ms 内 + 8px 内再次空白单击(像素判据,seek
                    // 滚动视图后吸附 beat 可能漂移,不能用 beat 判据)。
                    let double = if drag.is_none() {
                        match self.last_blank_click {
                            Some((t0, pmx, pmy, b0, x0))
                                if t0.elapsed() < Duration::from_millis(300)
                                    && (mx - pmx).abs() <= 8.0 * s2
                                    && (my - pmy).abs() <= 8.0 * s2 =>
                            {
                                Some((b0, x0))
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    let snap = self.snap as f64;
                    let b = snap_beat(cur_beat, snap, 0.0);
                    match blank_release_action(drag, double, self.place_kind, snap, (b, cur_x)) {
                        BlankAction::Seek(beat) => {
                            // 单击:记下位置(下次双击判定),seek 到该拍(PMCORE-22)。
                            self.last_blank_click = Some((Instant::now(), mx, my, b, cur_x));
                            self.messages.push(OverlayMessage::SeekToBeat(beat));
                        }
                        BlankAction::Place { start, end, x, kind } => {
                            self.last_blank_click = None;
                            self.messages.push(OverlayMessage::PlaceNote { start, end, x, kind });
                        }
                    }
                    self.ghost = None;
                    self.timeline_dirty = true;
                    return;
                }
                // 非面板起手的释放:释放落在 notes 区域(flow 命中)→ 原 seek
                // 行为(PMCORE-22)。命中几何 = flow.areas。
                if ev.released.iter().any(|&id| self.is_notes_flow_area(id)) {
                    let beat = snap_beat(self.y_to_beat(my, 0.0), self.snap as f64, 0.0);
                    self.messages.push(OverlayMessage::SeekToBeat(beat));
                    return;
                }
            }
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
                self.selected_tool = i;
                // 面板切换:清空旧面板 hover(flow 下一帧重算前避免残留高亮)。
                self.bpm_hover = None;
                self.settings_hover = None;
                self.eff_form_hover = None;
                self.chart_grid_hover = None;
                self.line_list_hover = None;
                return;
            }
            self.timeline_click = Some(mx);
        }
    }

    /// 菜单打开时的左键处理:经 flow 命中(单一真源)派发动作;按下即触发
    /// (与旧右键菜单一致);外部按下关闭;释放在外关闭后继续下行(拖拽提交)。
    fn handle_menu_click(&mut self, pressed: bool, ctrl: bool, shift: bool, mx: f32, my: f32) {
        self.menu.set_viewport(self.w as f32, self.h as f32, self.gui_scale);
        self.rebuild_notes_flow();
        self.btn_left = pressed;
        let mut mods = ModifiersState::empty();
        if ctrl { mods |= ModifiersState::CONTROL; }
        if shift { mods |= ModifiersState::SHIFT; }
        self.flow_mods = mods;
        let buttons = [
            if pressed { ButtonState::Pressed } else { ButtonState::Released },
            ButtonState::Up,
            ButtonState::Up,
        ];
        self.flow.on_mouse(mx, my, &buttons, mods);
        let ev = self.flow.resolve();
        let mut actions = Vec::new();
        self.menu.dispatch(&ev, &mut actions);
        let mut close = false;
        for a in actions {
            match a {
                menu::MenuAction::Close => close = true,
                menu::MenuAction::Command(c) => {
                    if let Some(m) = menu_command_message(c) {
                        self.messages.push(m);
                    }
                    close = true; // 命令执行后菜单关闭(与旧菜单一致)。
                }
                menu::MenuAction::Submenu(_) => {}
            }
        }
        if close {
            self.menu.close();
            self.timeline_dirty = true;
        }
        if !pressed {
            // 菜单吞掉的释放:结束进行中的拖拽(提交语义同正常释放路径),
            // 否则 drag_note 残留 → 松手后音符仍跟随鼠标(幽灵跟随)。
            self.commit_pending_drags();
        }
        if pressed {
            return; // 按下已处理(含关闭);吞掉,不落入下方拖拽/选择分支。
        }
        if !close {
            return; // 释放在菜单行上:吞掉。
        }
        // 释放在菜单外:菜单已关,继续下行(拖拽中的释放仍提交,旧行为)。
    }

    /// 底栏 Menu 按钮:打开/关闭编辑器主菜单(bar 形态)。
    pub fn toggle_main_menu(&mut self) {
        self.menu.set_viewport(self.w as f32, self.h as f32, self.gui_scale);
        if self.menu.is_open() {
            self.menu.close();
        } else {
            self.menu.open_bar(0.0, &editor_menu_items());
        }
        self.timeline_dirty = true;
    }

    /// 菜单键盘(宿主在 main.rs 其它键盘路由前拦截时调用):返回要派发的
    /// 动作(Close/Command),调用方据此关闭菜单/发消息。
    pub fn menu_key(&mut self, k: widgets::WidgetKey) -> Vec<menu::MenuAction> {
        self.menu.set_viewport(self.w as f32, self.h as f32, self.gui_scale);
        let out = self.menu.on_key(k);
        if !out.is_empty() || !self.menu.is_open() {
            self.timeline_dirty = true;
        }
        out
    }

    /// 鼠标横坐标 → 音符面板相对 x(-675..675)。复用拖拽/命中/ghost 的
    /// 同一换算(含 pad_x,与绘制一致)。
    pub fn mouse_note_x(&self) -> f32 {
        let Some((mx, _)) = self.mouse_pos else { return 0.0 };
        self.panel_transform().x_to_pos_x(mx)
    }

    /// 当前视图的 notes 面板映射链(单一真源,flow.rs)。
    fn panel_transform(&self) -> PanelTransform {
        PanelTransform::new(
            self.tl_scroll, self.tl_zoom, self.vertical_split, self.gui_scale,
            self.w as f32, self.h as f32, self.notes_progress, self.panel_progress,
        )
    }

    /// 每帧重建 notes 面板流控区域(布局 rect 树一次计算)并 resolve(hover
    /// 跟随布局变化;事件边沿在 handle_click/handle_cursor 里消费)。
    /// 区域构建在 `build_notes_areas`(纯函数,可单测),id 内容派生跨帧稳定。
    /// 属性面板打开时追加面板区域(命中 = 绘制,与 notes 区域不重叠);
    /// 菜单打开时在其上追加菜单区(最底层全窗 mask 承接外部点击,独占)。
    fn rebuild_notes_flow(&mut self) {
        let prev_hover = self.flow.hover;
        let pt = self.panel_transform();
        self.flow.areas = build_notes_areas(&pt, &self.notes_cache, self.ghost, self.vertical_split);
        self.flow.areas.extend(self.panel_flow_areas());
        if self.menu.is_open() {
            let menu_areas = self.menu.areas();
            self.flow.areas.extend(menu_areas);
        }
        self.flow.resolve();
        // hover 变化 → 重绘:面板行高亮等 hover 驱动的绘制在 fast path
        // (只传播放头+浮层)下不刷新,必须置 timeline_dirty 走全量路径。
        if self.flow.hover != prev_hover {
            self.timeline_dirty = true;
        }
        // 悬停高亮回写菜单(键盘导航中鼠标不覆盖,menu.rs 内部判定)。
        self.menu.apply_hover(self.flow.hover);
    }

    /// 属性面板(tool 0/2/3/4)的流控区域:几何委托各面板模块
    /// `form_areas`/`grid.areas()`(组件库 Area → HotArea 转换,单一真源)。
    /// 面板可见性门限 = panel_progress > 0.01(与 timeline_draw 绘制门限一致)。
    fn panel_flow_areas(&self) -> Vec<flow::HotArea> {
        if self.panel_progress <= 0.01 {
            return Vec::new();
        }
        let mut out = Vec::new();
        match self.selected_tool {
            // Chart 面板:KeyValueGrid 行(id = 行索引;可编辑行点击/键盘保留)。
            0 => {
                if let Some(grid) = &self.chart_grid {
                    out.extend(grid.areas().into_iter().map(|a| flow::HotArea {
                        id: AreaId(a.id),
                        rect: (a.rect.x(), a.rect.y(), a.rect.width(), a.rect.height()),
                        kind: AreaKind::Widget(a.kind),
                        disabled: false,
                    }));
                }
            }
            2 => {
                if let Some(f) = &self.settings_form {
                    out.extend(settings::form_areas(f));
                }
            }
            3 => {
                if let Some(f) = &self.eff_form {
                    let kf_rows = if self.eff_kf_var.is_some() { self.eff_kf_rows_n } else { 0 };
                    out.extend(eff_panel::form_areas(f, self.gui_scale, kf_rows));
                }
            }
            4 => {
                if let Some(f) = &self.bpm_form {
                    out.extend(bpm_panel::form_areas(f));
                }
            }
            _ => {}
        }
        out
    }

    /// 当前面板 hover(组件库 Area,绘制高亮用):从 [`UIFlow::hover`] 推导
    /// (单一真源)。仅当 hover 落在面板区域(Widget kind)时返回对应组件
    /// Area;notes 区域与面板行 id 可能同值(如 AreaId(2) = 音符 1 或
    /// BPM 第 1 行),按 kind 区分,非面板命中返回 None。
    pub fn panel_hover(&self, widget_areas: &[widgets::Area]) -> Option<widgets::Area> {
        let id = self.flow.hover?;
        if !matches!(flow_area_kind(&self.flow.areas, id), Some(AreaKind::Widget(_))) {
            return None;
        }
        widget_areas.iter().find(|a| a.id == id.0).cloned()
    }

    /// 点击/释放事件里的区域 id → 线内音符索引(音符块或 hold 尾)。
    fn note_from_flow(&self, id: AreaId) -> Option<usize> {
        note_index_from_area(id).filter(|&ni| {
            self.notes_cache.iter().any(|e| e.index == ni)
        })
    }

    /// 被 modal 分支(ctx 菜单/主菜单/底栏)吞掉的左键释放:补喂给流控,
    /// 使 prev_buttons 回到 Released/Up —— 否则下一次按下检测不到边沿,
    /// 音符选择/拖拽全部失效(拖拽中释放落在底栏是常见路径)。
    /// 释放事件本身不消费(命中/点击已由 modal 分支处理)。
    pub(crate) fn feed_swallowed_release(&mut self) {
        self.btn_left = false;
        let buttons = [ButtonState::Released, ButtonState::Up, ButtonState::Up];
        self.flow.on_mouse(self.flow.mouse.0, self.flow.mouse.1, &buttons, self.flow_mods);
        self.flow.resolve();
        // 吞掉的释放同样结束进行中的拖拽(提交语义同正常释放路径),
        // 否则 drag_note 残留 → 松手后音符仍跟随鼠标(幽灵跟随)。
        self.commit_pending_drags();
    }

    /// 结束进行中的音符/hold 拖拽。提交语义与正常释放路径
    /// (handle_click 的 `!pressed && drag_note.is_some()` 分支)一致:
    /// 拖到哪提交到哪。释放被 modal 分支(底栏/菜单)吞掉时调用,
    /// 防 drag_note/drag_hold 残留导致"松手后音符仍跟随鼠标"。
    fn commit_pending_drags(&mut self) {
        if let Some((ni, ..)) = self.drag_note {
            let (_, my) = self.mouse_pos.unwrap_or((0.0, 0.0));
            self.drag_updated = Some((ni, self.y_to_beat(my, 0.0), self.mouse_note_x()));
        }
        self.drag_note = None;
        if let Some(ni) = self.drag_hold {
            let end = self.hold_preview.map(|(_, e)| e).unwrap_or_else(|| {
                let start = self.notes_cache.iter().find(|e| e.index == ni).map(|e| e.start_beats).unwrap_or(0.0);
                snap_beat(self.y_to_beat(self.mouse_pos.map_or(0.0, |(_, my)| my), 0.0), self.snap as f64, start + self.snap as f64)
            });
            self.hold_updated = Some((ni, end));
        }
        self.drag_hold = None;
        self.hold_preview = None;
    }

    /// 窗口失焦 / 光标离开窗口:释放事件可能被 OS 吞掉(Alt-Tab、拖拽出窗),
    /// 流控 prev_buttons 停在 Pressed → 下一次按下无边沿(点击丢失)、hover
    /// 被陈旧 captured 钉死、seek 拖拽吞掉后续按下。重置边沿并取消(而非
    /// 提交)进行中的拖拽/框选/seek。
    pub(crate) fn on_focus_lost(&mut self) {
        self.btn_left = false;
        self.flow.force_release_all();
        self.drag_note = None;
        self.drag_hold = None;
        self.hold_preview = None;
        self.place_drag = None;
        self.selecting = false;
        self.select_start = None;
        self.select_end = None;
        self.seek_dragging = false;
    }

    /// 事件 id 是否为 notes 面板的区域(空白/音符块/hold 尾;幽灵/列线 excluded)。
    fn is_notes_flow_area(&self, id: AreaId) -> bool {
        matches!(
            flow_area_kind(&self.flow.areas, id),
            Some(AreaKind::NoteBlock | AreaKind::HoldTail | AreaKind::NoteBlank)
        )
    }

    /// 放置音符类型(1=tap 2=hold 3=flick 4=drag);Q/W/E/R 设置(mania 批次 1)。
    /// 变化时刷新幽灵并置 timeline_dirty。
    pub fn set_place_kind(&mut self, k: u8) {
        if self.place_kind != k {
            self.place_kind = k;
            self.update_ghost();
        }
    }

    /// 重算放置幽灵(悬停/拖拽);变化才置 timeline_dirty(避免每帧全量重绘)。
    fn update_ghost(&mut self) {
        let new = self.compute_ghost();
        if new != self.ghost {
            self.ghost = new;
            self.timeline_dirty = true;
        }
    }

    /// 当前应显示的放置幽灵(与放置共用同一映射,铁律)。拖拽中 = 拖拽判定
    /// 的 hold/tap;悬停 = 当前类型(hold 时长 = 一个 snap)。拖已有音符/
    /// hold 尾部时不显示。
    fn compute_ghost(&self) -> Option<(f64, f64, f32, u8)> {
        if !self.tl_visible || !self.is_over_notes(0.0) {
            return None;
        }
        if self.drag_note.is_some() || self.drag_hold.is_some() {
            return None;
        }
        let (_, my) = self.mouse_pos?;
        let snap = self.snap as f64;
        let x = self.mouse_note_x().clamp(-675.0, 675.0);
        if let Some((_, _, start_raw)) = self.place_drag {
            let (s, e, k) = drag_placement(start_raw, self.y_to_beat(my, 0.0), snap);
            Some((s, e, x, k))
        } else {
            let b = snap_beat(self.y_to_beat(my, 0.0), snap, 0.0);
            let k = self.place_kind;
            Some((b, if k == 2 { b + snap } else { b }, x, k))
        }
    }

    /// 鼠标当前位置(音符面板上方)命中的最近音符,返回线内 note 索引。
    /// 命中来源 = flow.hover(单一真源;handle_cursor 每帧 resolve)。
    pub fn hit_note_at_mouse(&self) -> Option<usize> {
        if !self.is_over_notes(0.0) { return None; }
        self.flow.hover.and_then(|id| self.note_from_flow(id))
    }

    /// Convert a Y position to beat value on the timeline (whichever panel is visible).
    fn y_to_beat(&self, my: f32, props: f32) -> f64 {
        if !self.is_over_events(props) && !self.is_over_notes(props) {
            return self.tl_scroll as f64 + self.tl_zoom as f64 * 0.5;
        }
        // notes/events 面板几何相同(56s 底边),共用同一映射(单一真源)。
        self.panel_transform().y_to_beat(my)
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
            self.finalize_box_selection();
        }
    }

    /// 清空全部选择与残留框选状态(切谱面/切线时调用,PMCORE-18)。
    pub fn clear_selection(&mut self) {
        self.selected_note = None;
        self.selected_notes.clear();
        self.selected_events.clear();
        self.select_events_kind = None;
        self.ctx_on_events = false;
        self.ctx_note_hit = false;
        self.selecting = false;
        self.select_notes = false;
        self.select_start = None;
        self.select_end = None;
    }

    /// 框选收尾(PMCORE-18):矩形与音符相交即选中,然后清矩形。
    /// 矩形过小(≤ 绘制阈值 2px)视为点击,不改动现有选择。
    /// 空矩形(未框住任何音符)清空选择。
    fn finalize_box_selection(&mut self) {
        if self.select_notes && !self.full_notes {
            if let (Some(s0), Some(e0)) = (self.select_start, self.select_end) {
                let rx = s0.0.min(e0.0);
                let ry = s0.1.min(e0.1);
                let rw = (s0.0 - e0.0).abs();
                let rh = (s0.1 - e0.1).abs();
                if rw > 2.0 && rh > 2.0 {
                    let sel: Vec<usize> = self
                        .notes_cache
                        .iter()
                        .filter(|n| self.note_in_rect(n, rx, ry, rw, rh))
                        .map(|n| n.index)
                        .collect();
                    if sel.is_empty() {
                        self.selected_notes.clear();
                        self.selected_note = None;
                    } else {
                        self.selected_notes = sel;
                        self.selected_note = Some(self.selected_notes[0]);
                    }
                }
            }
        } else if let Some(kind) = self.select_events_kind.clone() {
            // 事件面板框选:矩形内同列同类事件 → 扁平索引集合(PMCORE-20)。
            if let (Some(s0), Some(e0)) = (self.select_start, self.select_end) {
                let rx = s0.0.min(e0.0);
                let ry = s0.1.min(e0.1);
                let rw = (s0.0 - e0.0).abs();
                let rh = (s0.1 - e0.1).abs();
                if rw > 2.0 && rh > 2.0 {
                    let sel: Vec<usize> = self
                        .events_cache
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.kind == kind && self.event_in_rect(e, rx, ry, rw, rh))
                        .map(|(fi, _)| fi)
                        .collect();
                    if sel.is_empty() {
                        self.selected_events.clear();
                    } else {
                        self.selected_events = sel;
                    }
                }
            }
        }
        self.select_notes = false;
        self.select_events_kind = None;
        self.select_start = None;
        self.select_end = None;
    }

    /// 事件在屏幕上的绘制矩形(列 x + beat y)。几何与 timeline.rs 的
    /// draw_5col_timeline 完全一致——命中几何 = 绘制几何(PMCORE-20)。
    fn event_screen_rect(&self, e: &EventEntry) -> (f32, f32, f32, f32) {
        let s = self.gui_scale;
        let px = self.w as f32 - self.props_progress() * PANEL_W * s - TL_W * s - 2.0;
        let ci = match e.kind.as_str() {
            "Alpha" => 0usize, "MoveX" => 1, "MoveY" => 2, "Rotate" => 3, "Speed" => 4, _ => return (0.0, 0.0, 0.0, 0.0),
        };
        let cx = px + 4.0 * s + ci as f32 * (COL_W * s + COL_GAP * s);
        let py = (HEADER_H * s + 4.0 * s) as f64;
        let ph = (self.h as f32 - 48.0 * s - py as f32) as f64;
        let to_y = |b: f64| py + ph - (b - self.tl_scroll as f64) / self.tl_zoom as f64 * ph;
        let y0 = to_y(e.start_beats).clamp(py, py + ph) as f32;
        let y1 = to_y(e.end_beats).clamp(py, py + ph) as f32;
        (cx, y0.min(y1), COL_W * s, (y1 - y0).abs().max(3.0))
    }

    /// 框选矩形(屏幕坐标)与事件绘制矩形是否相交。
    fn event_in_rect(&self, e: &EventEntry, rx: f32, ry: f32, rw: f32, rh: f32) -> bool {
        let (cx, cy, cw, ch) = self.event_screen_rect(e);
        rx < cx + cw && rx + rw > cx && ry < cy + ch && ry + rh > cy
    }

    /// 音符在屏幕上的绘制矩形(中心 + 宽高)。几何与 timeline.rs 的
    /// draw_notes_timeline 完全一致(to_y / to_col_x / nw / nh)——
    /// 命中几何 = 绘制几何(PMCORE-18)。
    fn note_screen_rect(&self, n: &NoteEntry) -> (f32, f32, f32, f32) {
        let s = self.gui_scale;
        let play = notes_play_rect(self.panel_x(0.0, NT_W, self.notes_progress), self.h as f32, s);
        let cy = beat_to_panel_y(n.start_beats, play, self.tl_scroll, self.tl_zoom)
            .clamp(play.1, play.1 + play.3);
        let cx = pos_x_to_col_x(n.x, play, self.vertical_split);
        let sc = n.scale.max(0.1);
        let nw = (64.0 * s * sc).max(4.0);
        let nh = (8.0 * s * sc).max(2.0);
        (cx, cy, nw, nh)
    }

    /// 选择矩形(屏幕坐标)与音符绘制矩形是否相交。
    fn note_in_rect(&self, n: &NoteEntry, rx: f32, ry: f32, rw: f32, rh: f32) -> bool {
        let (cx, cy, nw, nh) = self.note_screen_rect(n);
        rx < cx + nw * 0.5
            && rx + rw > cx - nw * 0.5
            && ry < cy + nh * 0.5
            && ry + rh > cy - nh * 0.5
    }

    pub fn handle_right_click(&mut self, props: f32) {
        // Whitelist: only open context menu on timeline panels
        if !self.is_over_timeline(props) { return; }
        self.ctx_on_events = false;
        self.ctx_note_hit = false;
        // 事件面板右键:菜单项为"删除事件"(PMCORE-20)。实际命中/选中由
        // main.rs 在右键处理时用 events_cache 完成(这里只记面板归属)。
        if self.is_over_events(props) {
            if let Some((mx, my)) = self.mouse_pos {
                if self.event_geo(mx, my).is_some() {
                    self.ctx_on_events = true;
                }
            }
        }
        // 音符面板右键:先命中并选中音符,菜单"删除音符"作用于它(PMCORE-17)。
        // 同时把多选集合收敛为该音符(PMCORE-18)。ctx_note_hit 记录本次右键
        // 是否命中音符(PMCORE-77 注释目标解析用)。命中来源 = flow.hover
        // (此刻重建一次:滚动/缩放可能发生在最后一次光标移动之后)。
        if self.is_over_notes(props) {
            let hit = { self.rebuild_notes_flow(); self.flow.hover }.and_then(|id| self.note_from_flow(id));
            self.ctx_note_hit = hit.is_some();
            if let Some(ni) = hit {
                self.selected_note = Some(ni);
                self.selected_notes = vec![ni];
            }
        }
        if let Some((mx, my)) = self.mouse_pos {
            // 右键菜单迁为 MenuHost context 形态(几何/命中同源,menu.rs)。
            self.menu.set_viewport(self.w as f32, self.h as f32, self.gui_scale);
            self.menu.open_context((mx, my), &context_menu_items(self.ctx_on_events));
            self.timeline_dirty = true;
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
    /// events and return the click info (hit event / empty-column beat+kind).
    pub fn take_timeline_click(&mut self) -> Option<TimelineClick> {
        let mx = self.timeline_click.take()?;
        self.hit_test_timeline_impl(mx)
    }

    /// 事件面板坐标 → (列 kind, raw beat)。不在事件面板/有效列上返回
    /// None。几何与 draw_5col_timeline 一致(列宽/列间距/头部/高度)。
    fn event_geo(&self, mx: f32, my: f32) -> Option<(String, f64)> {
        let s = self.gui_scale;
        let px = self.w as f32 - self.props_progress() * PANEL_W * s - TL_W * s - 2.0;
        if mx < px || mx > px + TL_W * s { return None; }
        if my < HEADER_H * s + 4.0 * s { return None; }
        let py = (HEADER_H * s + 4.0 * s) as f64;
        let ph = (self.h as f32 - 48.0 * s - py as f32) as f64;
        if ph <= 0.0 { return None; }
        let col = ((mx - px - 4.0 * s) / (COL_W * s + COL_GAP * s)).floor() as usize;
        let kind = match col { 0 => "Alpha", 1 => "MoveX", 2 => "MoveY", 3 => "Rotate", 4 => "Speed", _ => return None };
        let beat = self.tl_scroll as f64 + (1.0 - (my as f64 - py) / ph) * self.tl_zoom as f64;
        Some((kind.to_string(), beat))
    }

    /// 指定列 + beat 命中的事件扁平索引(events_cache 按 end_beats 排序)。
    fn best_event_hit(&self, kind: &str, click_beat: f64) -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        for (i, ev) in self.events_cache.iter().enumerate() {
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

    fn hit_test_timeline_impl(&self, mx: f32) -> Option<TimelineClick> {
        let my = self.mouse_pos?.1;
        let (kind, click_beat) = self.event_geo(mx, my)?;
        Some(TimelineClick { hit: self.best_event_hit(&kind, click_beat), beat: click_beat, kind: Some(kind) })
    }

    /// 事件面板右键/Insert:命中鼠标位置的事件(扁平索引),未命中返回 None。
    pub fn hit_event_at_mouse(&self) -> Option<usize> {
        if !self.is_over_events(0.0) { return None; }
        let (mx, my) = self.mouse_pos?;
        let (kind, click_beat) = self.event_geo(mx, my)?;
        self.best_event_hit(&kind, click_beat)
    }

    /// 鼠标在事件面板上时:所在列 kind + raw beat(Insert 创建事件用)。
    pub fn event_geo_at_mouse(&self) -> Option<(String, f64)> {
        if !self.is_over_events(0.0) { return None; }
        let (mx, my) = self.mouse_pos?;
        self.event_geo(mx, my)
    }

    // ── hover 浮层(PMCORE-76)──

    /// 当前鼠标位置命中的浮层目标。命中几何 = 绘制几何:复用
    /// [`note_screen_rect`]/[`event_screen_rect`](与框选同一矩形,无第二套
    /// 魔法数字)。候选按 end_beats 排序窗口扫描(±1 拍),屏幕矩形精确过滤。
    fn hover_key(&self) -> Option<TooltipKey> {
        if !self.hover_tooltip || !self.tl_visible { return None; }
        let (mx, my) = self.mouse_pos?;
        if self.is_over_notes(0.0) {
            // 命中来源 = flow.hover(单一真源);浮层仍要求精确落在绘制矩形内
            // (note_screen_rect),保持 PMCORE-76 行为不变。
            let ni = self.flow.hover.and_then(|id| self.note_from_flow(id))?;
            let n = self.notes_cache.iter().find(|n| n.index == ni)?;
            let (cx, cy, nw, nh) = self.note_screen_rect(n);
            if mx >= cx - nw * 0.5 && mx <= cx + nw * 0.5 && my >= cy - nh * 0.5 && my <= cy + nh * 0.5 {
                return Some(TooltipKey::Note(ni));
            }
        } else if self.is_over_events(0.0) {
            let beat = self.y_to_beat(my, 0.0);
            let lo = self.events_cache.partition_point(|e| e.end_beats < beat - 1.0);
            for (i, e) in self.events_cache[lo..].iter().enumerate() {
                let i = lo + i;
                if e.start_beats > beat + 1.0 { break; }
                let (cx, cy, cw, ch) = self.event_screen_rect(e);
                if mx >= cx && mx <= cx + cw && my >= cy && my <= cy + ch {
                    return Some(TooltipKey::Event(i));
                }
            }
        }
        None
    }

    /// 浮层目标数据的轻量指纹:拖拽落盘后缓存刷新时 key 相同但数据变了,
    /// 指纹变化同样触发重建(其余帧零分配)。
    fn tooltip_fp(&self, key: &TooltipKey) -> u64 {
        match key {
            TooltipKey::Note(i) => self.notes_cache.iter()
                .find(|n| n.index == *i)
                .map(|n| n.start_beats.to_bits() ^ n.end_beats.to_bits() ^ ((n.x as f64).to_bits() << 1) ^ ((n.kind as u64) << 56))
                .unwrap_or(0),
            TooltipKey::Event(i) => self.events_cache.get(*i)
                .map(|e| e.start_beats.to_bits() ^ e.end_beats.to_bits() ^ ((e.easing as u64) << 56))
                .unwrap_or(0),
        }
    }

    /// 构建浮层文本(分配集中在 key/指纹变化那一刻)。
    fn tooltip_lines_for(&self, key: &TooltipKey) -> Vec<String> {
        match key {
            TooltipKey::Note(i) => self.notes_cache.iter()
                .find(|n| n.index == *i)
                .map(note_tooltip_lines)
                .unwrap_or_default(),
            TooltipKey::Event(i) => self.events_cache.get(*i)
                .map(event_tooltip_lines)
                .unwrap_or_default(),
        }
    }

    /// 每帧更新 hover 浮层缓存(命中 + 文本重建)。返回本帧是否有浮层
    /// 要绘制;调用方随后用 [`timeline_draw::draw_tooltip`] 把文本画进
    /// pixmap(分开两步避免 pixmap 与 self 的双重可变借用)。
    fn update_tooltip(&mut self) -> bool {
        let key = self.hover_key();
        match &key {
            Some(k) => {
                let fp = self.tooltip_fp(k);
                let changed = match &self.tooltip_cache {
                    Some((ck, cfp)) => ck != k || *cfp != fp,
                    None => true,
                };
                if changed {
                    self.tooltip_lines = self.tooltip_lines_for(k);
                    self.tooltip_cache = Some((k.clone(), fp));
                }
                true
            }
            None => {
                self.tooltip_cache = None;
                self.tooltip_lines.clear();
                false
            }
        }
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
        self.tl_visible = info.show_events || info.show_notes;
        self.gui_scale = info.gui_scale;
        self.notes_cache = info.notes.clone();
        self.events_cache = info.events.clone();
        self.eff_kf_var = info.eff_kf_var;
        self.eff_kf_rows_n = info.eff_kf_rows.len();
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
        // PMCORE-69:iced_tex 按脏区上传(对比上一帧 GPU 镜像)。面板动画只
        // 动顶层文本条(~20KB),光标动画完全不动 iced(0B),不再每帧 4MB。
        if std::env::var("PHIMAKOR_ICED_CHECK").is_ok() {
            // 诊断:统计非透明像素(排除面板区域,面板在右侧)。
            let opaque = self.iced_cache.data().chunks_exact(4)
                .filter(|p| p[3] > 0).count();
            let w = self.w as usize;
            // 左侧区域(非面板)的透明情况
            let left_opaque = self.iced_cache.data().chunks_exact(4).enumerate()
                .filter(|(i, p)| (i % w) < (w / 3) && p[3] > 0).count();
            eprintln!("iced cache: opaque={opaque} (left-third opaque={left_opaque})");
        }
        let _ = upload_diffed(queue, &self.iced_tex, self.w, self.h, self.iced_cache.data(), &mut self.iced_last, "iced");
        // Clear working pixmap and draw timeline → overlay texture
        self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
        self.upload_timeline_diff(queue, info);
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
        // 标题界面无播放头:fast path 的脏条基准清零。
        self.last_playhead_y = None;
        // PMCORE-69:splash 每帧全量上传;上传后作废 diff 快照,切回编辑器
        // 时首帧全量上传重建(避免以 splash 内容做 diff 基准)。
        self.iced_last.clear();
        self.tl_last.clear();
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

    /// Timeline-only per-frame redraw: restore the playhead-free base pixmap,
    /// draw the current-time lines, upload. No tiny_skia content rebuild.
    pub fn redraw_timeline(&mut self, queue: &wgpu::Queue, info: &GameInfo) {
        let _s = trace_span!("redraw_timeline");
        self.show_overlay = info.show_overlay;
        self.tl_visible = info.show_events || info.show_notes;
        self.gui_scale = info.gui_scale;
        let prev_anim = (
            self.panel_progress,
            self.events_progress,
            self.notes_progress,
            self.tool_hover_progress,
        );
        self.panel_progress = self.approach(self.panel_progress, if info.show_properties { 1.0 } else { 0.0 });
        self.events_progress = self.approach(self.events_progress, if info.show_events { 1.0 } else { 0.0 });
        self.notes_progress = self.approach(self.notes_progress, if info.show_notes { 1.0 } else { 0.0 });
        self.animate_all();
        // 全量路径条件:
        // - timeline_dirty(视图/内容变化,必须重绘)
        // - anim_moving(面板动画,iced 同步重绘)
        // - playing 且跟随滚动(tl_follow:时间轴内容每帧平移,base 失效)
        // 播放但视图固定(tl_follow=false,手动滚动后)时内容静止,
        // 只有播放头在动 → 走 fast path(只传播放头脏条,省 8MB/帧)。
        let playing = (info.chart_beat - self.last_drawn_beat).abs() > 1e-4;
        let anim_moving = prev_anim != (
            self.panel_progress,
            self.events_progress,
            self.notes_progress,
            self.tool_hover_progress,
        );
        let scrolling = playing && self.tl_follow;
        if scrolling || anim_moving || self.timeline_dirty {
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
            // PMCORE-69:面板动画期间(非滚动)按脏区上传(面板/光标条带),
            // 滚动播放内容每帧平移 → diff 退化全屏,维持全量上传不变。
            if anim_moving && !scrolling {
                self.upload_timeline_diff(queue, info);
            } else {
                self.upload_timeline_to(queue, info);
            }
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
        // PMCORE-69:只重传播放头新旧两条水平条(全宽 × ~10px),
        // 替代每帧 8MB 全屏上传。播放中视图固定时也走这里:
        // seek bar 进度每帧变,整条重画(背景覆盖旧进度)+ 并入脏区。
        let s = self.gui_scale;
        let vw = self.w as f32;
        let vh = self.h as f32;
        let pan_w = PANEL_W * s;
        let props_x = vw - self.panel_progress * pan_w;
        if self.show_overlay {
            timeline_draw::draw_seek_bar(&mut self.pixmap.as_mut(), info, QP_W * s, props_x, vh, s);
        }
        let y_new = self.draw_playhead(info);
        let y_old = self.last_playhead_y;
        self.last_playhead_y = y_new;
        // PMCORE-76:hover 浮层:本帧绘制后把新旧矩形并入脏条(base 恢复
        // 已擦除自绘 pixmap 上的旧浮层,必须上传该区域才能擦除 GPU 上的
        // 旧浮层——与播放头新旧两条脏条的思路一致)。
        let tt_new = if self.update_tooltip() {
            let (mx, my) = self.mouse_pos.unwrap_or((0.0, 0.0));
            Some(timeline_draw::draw_tooltip(&mut self.pixmap.as_mut(), mx, my, &self.tooltip_lines, s))
        } else {
            None
        };
        let tt_old = self.last_tooltip_rect;
        self.last_tooltip_rect = tt_new;
        // 脏区:播放头条 + seek bar 条(合并为竖直范围,避免多次上传)。
        let mut y0 = if self.show_overlay { vh - 56.0 * s - 2.0 } else { f32::MAX };
        let mut y1 = if self.show_overlay { vh - 56.0 * s + 14.0 * s + 2.0 } else { f32::MIN };
        if let Some(yn) = y_new {
            let (a, b) = match y_old {
                Some(yo) => (yo.min(yn) - 2.0, yo.max(yn) + 3.0),
                None => (yn - 2.0, yn + 3.0),
            };
            y0 = y0.min(a);
            y1 = y1.max(b);
        }
        for (_, ty, _, th) in [tt_new, tt_old].into_iter().flatten() {
            y0 = y0.min(ty);
            y1 = y1.max(ty + th);
        }
        if y1 >= y0 {
            let y0 = y0.max(0.0) as u32;
            let y1 = (y1.max(0.0) as u32).min(self.h - 1);
            if y1 >= y0 {
                self.upload_rect(queue, 0, y0, vw as u32, y1 - y0 + 1);
            }
        }
        self.last_drawn_beat = info.chart_beat;
    }

    /// Draw the current-time playhead lines on `pixmap` (assumes base content).
    /// Returns the playhead y (pixels), or `None` when the timeline is hidden.
    fn draw_playhead(&mut self, info: &GameInfo) -> Option<f32> {
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
            return Some(ct_y);
        }
        None
    }

    /// 播放头水平线 y(像素),供全量上传后同步 last_playhead_y。
    fn playhead_y(&self, info: &GameInfo) -> Option<f32> {
        if !self.tl_visible {
            return None;
        }
        let s = self.gui_scale;
        let vh = self.h as f32;
        let head_h = HEADER_H * s;
        let py = head_h + 4.0 * s;
        let ph = (vh - 56.0 * s - py) as f64;
        let (scroll, zoom) = (self.tl_scroll as f64, self.tl_zoom as f64);
        let to_y = |b: f64| py as f64 + ph - (b - scroll) / zoom * ph;
        Some(to_y(info.chart_beat).clamp(py as f64, py as f64 + ph) as f32)
    }

    /// 上传 pixmap 的一个矩形区域到 timeline 纹理(PMCORE-69)。
    /// 同时同步 tl_last 快照,保证后续 diff 上传的基准恒等于 GPU 现状。
    /// 返回实际上传字节数(供诊断/量测)。
    fn upload_rect(&mut self, queue: &wgpu::Queue, x: u32, y: u32, w: u32, h: u32) -> usize {
        let n = upload_rect_from(queue, &self.timeline_tex, self.w, self.h, self.pixmap.data(), x, y, w, h);
        // 同步 GPU 镜像(仅上传过的矩形区域)。
        if n > 0 && !self.tl_last.is_empty() {
            copy_rect_into(&mut self.tl_last, self.pixmap.data(), self.w, self.h, x, y, w, h);
        }
        n
    }

    fn animate_all(&mut self) {
        self.animate_tool_hover();
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

    /// 全量上传时间轴(播放滚动等全量变化路径,PMCORE-69 保持全量)。
    fn upload_timeline_to(&mut self, queue: &wgpu::Queue, info: &GameInfo) {
        self.upload_timeline_impl(queue, info, false);
    }

    /// 脏区上传时间轴(面板动画/光标动画路径,PMCORE-69):worker 照常
    /// 重绘,但只上传与上一帧镜像不同的矩形,避免动画期间每帧全屏上传。
    fn upload_timeline_diff(&mut self, queue: &wgpu::Queue, info: &GameInfo) {
        self.upload_timeline_impl(queue, info, true);
    }

    fn upload_timeline_impl(&mut self, queue: &wgpu::Queue, info: &GameInfo, diff: bool) {
        let _s = trace_span!("upload_timeline");
        // PMCORE-55:绘制挪到 worker 线程(纯函数 draw_timeline_pixmap)。
        // 主线程:animate + 快照 → 发 job → 收上一帧结果 → 上传 GPU。
        // 像素零拷贝:直接上传 worker 的 pending,不走 pixmap 中转。
        self.animate_all();
        self.notes_cache = info.notes.clone();
        self.events_cache = info.events.clone();
        self.eff_kf_var = info.eff_kf_var;
        self.eff_kf_rows_n = info.eff_kf_rows.len();
        if self.tl_visible && self.tl_follow {
            self.tl_scroll = (info.chart_beat as f32 - self.tl_zoom * 0.1).max(0.0);
        }
        // 布局 rect 树每帧重建 + resolve:hover 跟随布局变化(滚动/缩放/
        // 面板滑入),绘制高亮不再自己重算命中。
        self.rebuild_notes_flow();
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
            if diff {
                let _ = upload_diffed(queue, &self.timeline_tex, self.w, self.h, data, &mut self.tl_last, "tl");
            } else {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo { texture: &self.timeline_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                    data,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * self.w), rows_per_image: Some(self.h) },
                    wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
                );
                // 同步 GPU 镜像(全量路径,diff 基准恒等于 GPU 现状)。
                if self.tl_last.is_empty() {
                    self.tl_last.extend_from_slice(data);
                } else {
                    self.tl_last.copy_from_slice(data);
                }
            }
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
            if self.tl_last.is_empty() {
                self.tl_last.extend_from_slice(data);
            } else {
                self.tl_last.copy_from_slice(data);
            }
        }
        // 全量上传后同步播放头 y(fast path 脏条擦除依赖它)。
        // 注意:worker 帧可能比本帧 info 旧一帧,但 beat 差 <1 帧窗口内
        // 播放头只动几像素,多擦/少擦 1-2px 的残影由下一帧脏条自愈。
        self.last_playhead_y = self.playhead_y(info);
        // PMCORE-76:hover 浮层:整帧 worker 上传已擦除 GPU 上的旧浮层,
        // 这里画新浮层并只补传其矩形区域(不触发全屏 Iced 重建)。
        self.last_tooltip_rect = if self.update_tooltip() {
            let (mx, my) = self.mouse_pos.unwrap_or((0.0, 0.0));
            let rect = timeline_draw::draw_tooltip(&mut self.pixmap.as_mut(), mx, my, &self.tooltip_lines, self.gui_scale);
            self.upload_rect(queue, rect.0 as u32, rect.1 as u32, rect.2 as u32, rect.3 as u32);
            Some(rect)
        } else {
            None
        };
    }
}

/// 拷贝 `src` 中 (x, y, w, h) 矩形到 `dst`(同为 w×h RGBA 缓冲)。
/// 上传后同步 GPU 镜像用(PMCORE-69)。
fn copy_rect_into(dst: &mut [u8], src: &[u8], w: u32, h: u32, x: u32, y: u32, rect_w: u32, rect_h: u32) {
    if rect_w == 0 || rect_h == 0 || y >= h || x >= w {
        return;
    }
    let (rect_w, rect_h) = (rect_w.min(w - x), rect_h.min(h - y));
    let stride = 4 * w as usize;
    for row in 0..rect_h as usize {
        let start = (y as usize + row) * stride + x as usize * 4;
        let len = rect_w as usize * 4;
        dst[start..start + len].copy_from_slice(&src[start..start + len]);
    }
}

/// 计算两个 RGBA 缓冲(w×h)的差异包围矩形;无差异返回 None。
/// PMCORE-69 diff 上传的脏区来源(逐行 u64 比较,变化行才逐像素扫)。
fn diff_bbox(a: &[u8], b: &[u8], w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
    let wu = w as usize;
    let n = a.len().min(b.len());
    let mut min_x = wu;
    let mut max_x = 0usize;
    let mut min_y = h as usize;
    let mut max_y = 0usize;
    for y in 0..(n / (wu * 4)).min(h as usize) {
        let row_a = &a[y * wu * 4..(y + 1) * wu * 4];
        let row_b = &b[y * wu * 4..(y + 1) * wu * 4];
        let words = wu / 2;
        let mut changed = false;
        for i in 0..words {
            let pa = u64::from_ne_bytes(row_a[i * 8..i * 8 + 8].try_into().unwrap());
            let pb = u64::from_ne_bytes(row_b[i * 8..i * 8 + 8].try_into().unwrap());
            if pa != pb {
                changed = true;
                break;
            }
        }
        if !changed && wu % 2 != 0 {
            changed = row_a[wu * 4 - 4..wu * 4] != row_b[wu * 4 - 4..wu * 4];
        }
        if changed {
            min_y = min_y.min(y);
            max_y = y + 1;
            for x in 0..wu {
                if row_a[x * 4..x * 4 + 4] != row_b[x * 4..x * 4 + 4] {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x + 1);
                }
            }
        }
    }
    if max_y > min_y && max_x > min_x {
        Some((min_x as u32, min_y as u32, (max_x - min_x) as u32, (max_y - min_y) as u32))
    } else {
        None
    }
}

/// 从任意 RGBA 源缓冲上传矩形区域到纹理(wgpu 256 字节行对齐)。
/// 返回实际上传字节数;w==0/h==0/越界返回 0。
fn upload_rect_from(
    queue: &wgpu::Queue, tex: &wgpu::Texture, tex_w: u32, tex_h: u32,
    src: &[u8], x: u32, y: u32, w: u32, h: u32,
) -> usize {
    if w == 0 || h == 0 || y >= tex_h || x >= tex_w {
        return 0;
    }
    let (w, h) = (w.min(tex_w - x), h.min(tex_h - y));
    let stride = 4 * tex_w as usize;
    let aligned = (stride + 255) & !255;
    let start = (y as usize * stride) + x as usize * 4;
    if aligned == stride {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
            &src[start..],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride as u32), rows_per_image: Some(tex_h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    } else {
        let mut buf = vec![0u8; aligned * h as usize];
        for row in 0..h as usize {
            let s = start + row * stride;
            buf[row * aligned..row * aligned + w as usize * 4]
                .copy_from_slice(&src[s..s + w as usize * 4]);
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
            &buf,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(aligned as u32), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }
    w as usize * h as usize * 4
}

/// 脏区上传(PMCORE-69):比较 `new` 与 `snapshot`(GPU 现状镜像)的差异
/// bbox,只上传该矩形并同步 snapshot;快照为空 → 全量上传。
/// 返回 (bbox, 实际上传字节数)。`label` 仅用于 PHIMAKOR_DIRTY_CHECK 诊断。
fn upload_diffed(
    queue: &wgpu::Queue, tex: &wgpu::Texture, tex_w: u32, tex_h: u32,
    new: &[u8], snapshot: &mut Vec<u8>, label: &str,
) -> (Option<(u32, u32, u32, u32)>, usize) {
    if snapshot.is_empty() {
        // 首帧/重建:全量上传并建立镜像。
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            new,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * tex_w), rows_per_image: Some(tex_h) },
            wgpu::Extent3d { width: tex_w, height: tex_h, depth_or_array_layers: 1 },
        );
        snapshot.clear();
        snapshot.extend_from_slice(new);
        let n = new.len();
        if std::env::var("PHIMAKOR_DIRTY_CHECK").is_ok() {
            eprintln!("[dirty:{label}] FULL {n}B");
        }
        return (Some((0, 0, tex_w, tex_h)), n);
    }
    let Some((x, y, w, h)) = diff_bbox(snapshot, new, tex_w, tex_h) else {
        if std::env::var("PHIMAKOR_DIRTY_CHECK").is_ok() {
            eprintln!("[dirty:{label}] none 0B");
        }
        return (None, 0);
    };
    let n = upload_rect_from(queue, tex, tex_w, tex_h, new, x, y, w, h);
    copy_rect_into(snapshot, new, tex_w, tex_h, x, y, w, h);
    if std::env::var("PHIMAKOR_DIRTY_CHECK").is_ok() {
        eprintln!("[dirty:{label}] rect ({x},{y} {w}x{h}) {n}B (full={}B)", 4 * tex_w * tex_h);
    }
    (Some((x, y, w, h)), n)
}

#[cfg(test)]
mod tests {
    use super::{blank_release_action, drag_placement, panel_x_to_pos_x, pos_x_to_panel_x, snap_beat};
    use super::{beat_to_panel_y, notes_play_rect, panel_y_to_beat};
    use super::{build_notes_areas, note_hit_rect, note_index_from_area, flow_area_kind, AREA_BLANK, AREA_COL_BASE, AREA_GHOST, AREA_NOTE_BASE};
    use super::flow::{AreaId, AreaKind, ButtonState, PanelTransform, UIFlow};
    use super::NoteEntry;
    use winit::keyboard::ModifiersState;

    #[test]
    fn snap_beat_rounds_to_grid_and_clamps_min() {
        // 吸附:1.3 → 最近网格点(0.25 步进)。
        assert_eq!(snap_beat(1.3, 0.25, 0.0), 1.25);
        assert_eq!(snap_beat(0.6, 0.5, 0.0), 0.5);
        assert_eq!(snap_beat(0.6, 1.0, 0.0), 1.0);
        // 下限钳制:end >= start + min(hold 尾部,PMCORE-17)。
        assert_eq!(snap_beat(0.1, 0.25, 0.5), 0.5);
        // 负值钳到 0。
        assert_eq!(snap_beat(-1.0, 1.0, 0.0), 0.0);
    }

    #[test]
    fn x_mapping_roundtrips_and_edges() {
        // 内容区左缘 → -675,右缘 → +675,中点 → 0(含 pad_x,与绘制一致)。
        let play = notes_play_rect(100.0, 800.0, 1.0);
        assert_eq!(panel_x_to_pos_x(play.0, play), -675.0);
        assert_eq!(panel_x_to_pos_x(play.0 + play.2, play), 675.0);
        assert_eq!(panel_x_to_pos_x(play.0 + play.2 * 0.5, play), 0.0);
        // 往返:pos_x → x → pos_x 一致。
        for nx in [-675.0, -200.0, 0.0, 333.0, 675.0] {
            let mx = pos_x_to_panel_x(nx, play);
            let back = panel_x_to_pos_x(mx, play);
            assert!((back - nx).abs() < 1e-3, "pos_x {nx} → x {mx} → {back}");
        }
    }

    #[test]
    fn beat_mapping_roundtrips_and_clamp() {
        let play = notes_play_rect(100.0, 800.0, 1.0);
        let (scroll, zoom) = (4.0f32, 8.0f32);
        // 窗口顶 = scroll+zoom,底 = scroll(可见范围 clamp)。
        assert!((panel_y_to_beat(play.1, play, scroll, zoom) - (scroll + zoom) as f64).abs() < 1e-4);
        assert!((panel_y_to_beat(play.1 + play.3, play, scroll, zoom) - scroll as f64).abs() < 1e-4);
        // 面板外 y 钳到可见范围。
        assert_eq!(panel_y_to_beat(-500.0, play, scroll, zoom), (scroll + zoom) as f64);
        assert_eq!(panel_y_to_beat(1e9, play, scroll, zoom), scroll as f64);
        // 可见范围内 beat → y → beat 往返一致。
        for b in [4.0, 5.5, 8.0, 11.9, 12.0] {
            let y = beat_to_panel_y(b, play, scroll, zoom);
            let back = panel_y_to_beat(y, play, scroll, zoom);
            assert!((back - b).abs() < 1e-4, "beat {b} → y {y} → {back}");
        }
        // y → beat → y 往返一致(可见范围内)。
        for my in [play.1, play.1 + play.3 * 0.25, play.1 + play.3 * 0.5, play.1 + play.3] {
            let b = panel_y_to_beat(my, play, scroll, zoom);
            let back = beat_to_panel_y(b, play, scroll, zoom);
            assert!((back - my).abs() < 0.01, "y {my} → beat {b} → {back}");
        }
    }

    /// mania 批次 1:拖拽放置——hold 生成与 <snap 退化 tap。
    #[test]
    fn drag_placement_hold_and_tap_degradation() {
        // 时长 >= snap → hold(两端均吸附)。
        assert_eq!(drag_placement(4.0, 4.5, 0.25), (4.0, 4.5, 2));
        assert_eq!(drag_placement(4.3, 4.6, 0.25), (4.25, 4.5, 2));
        // 反拖(终点在起点前)→ 区间取 min..max。
        assert_eq!(drag_placement(4.5, 4.0, 0.25), (4.0, 4.5, 2));
        // 时长 < snap → 退化 tap(起点,end == start)。
        assert_eq!(drag_placement(4.0, 4.1, 0.25), (4.0, 4.0, 1));
        // 两端吸附到同一点 → tap。
        assert_eq!(drag_placement(4.1, 4.12, 0.25), (4.0, 4.0, 1));
    }

    /// mania 批次 1:空白释放动作——单击 seek / 双击放置(用第一次点击位置)/
    /// 拖拽 hold / 拖拽退化 tap。
    #[test]
    fn blank_release_action_click_double_drag() {
        // 拖拽 → hold。
        let a = blank_release_action(Some((4.0, 4.5)), None, 2, 0.25, (4.0, 10.0));
        assert_eq!(a, super::BlankAction::Place { start: 4.0, end: 4.5, x: 10.0, kind: 2 });
        // 拖拽太短 → 退化 tap(当前类型是 hold 也退化)。
        let a = blank_release_action(Some((4.0, 4.1)), None, 2, 0.25, (4.0, 10.0));
        assert_eq!(a, super::BlankAction::Place { start: 4.0, end: 4.0, x: 10.0, kind: 1 });
        // 双击 → 用第一次点击的 (beat, x) 放置;tap 类型 end == start。
        let a = blank_release_action(None, Some((3.75, 55.0)), 1, 0.25, (9.0, 9.0));
        assert_eq!(a, super::BlankAction::Place { start: 3.75, end: 3.75, x: 55.0, kind: 1 });
        // 双击 + hold 类型 → 时长 = 一个 snap。
        let a = blank_release_action(None, Some((3.75, 55.0)), 2, 0.25, (9.0, 9.0));
        assert_eq!(a, super::BlankAction::Place { start: 3.75, end: 4.0, x: 55.0, kind: 2 });
        // 单击 → seek(吸附后的 beat)。
        let a = blank_release_action(None, None, 1, 0.25, (8.0, 0.0));
        assert_eq!(a, super::BlankAction::Seek(8.0));
    }

    /// PMCORE-76:浮层文本内容——音符:类型/start/end beat/position_x;
    /// 事件:kind/start/end beat/easing。
    #[test]
    fn tooltip_lines_cover_spec_fields() {
        let n = super::NoteEntry { index: 3, kind: 2, start_beats: 4.0, end_beats: 7.5, x: 123.4, scale: 1.0, comment: false };
        let nl = super::note_tooltip_lines(&n);
        assert_eq!(nl[0], "hold");
        assert!(nl.iter().any(|l| l.starts_with("start: 4.000")));
        assert!(nl.iter().any(|l| l.starts_with("end: 7.500")));
        assert!(nl.iter().any(|l| l.starts_with("x: 123.4")));
        assert_eq!(super::note_tooltip_lines(&super::NoteEntry { index: 0, kind: 1, start_beats: 0.0, end_beats: 0.0, x: 0.0, scale: 1.0, comment: false })[0], "tap");

        let e = super::EventEntry { layer: 0, kind: "Alpha".into(), index: 0, start_beats: 1.0, end_beats: 2.0, start: 0.0, end: 1.0, easing: 2 };
        let el = super::event_tooltip_lines(&e);
        assert_eq!(el[0], "Alpha");
        assert!(el.iter().any(|l| l.starts_with("start: 1.000")));
        assert!(el.iter().any(|l| l.starts_with("end: 2.000")));
        assert!(el.iter().any(|l| l.starts_with("easing: 2 (sine out)")));
    }

    /// PMCORE-76:easing_label 覆盖事件编辑钳制范围 0..=5(PMCORE-20)。
    #[test]
    fn easing_label_covers_editor_range() {
        for i in 0..=5 {
            assert!(!super::easing_label(i).is_empty(), "easing {i} 应有名称");
        }
        assert_eq!(super::easing_label(2), "sine out");
    }

    // ── PMCORE-69:diff 上传 ──
    #[test]
    fn diff_bbox_detects_regions_and_none() {
        let w = 4u32; let h = 4u32;
        let a = vec![0u8; (w * h * 4) as usize];
        let mut b = a.clone();
        // 完全相同 → None(不上传)。
        assert_eq!(super::diff_bbox(&a, &b, w, h), None);
        // 单像素变化 → 1×1 bbox。
        let i = ((1 * w + 2) * 4) as usize;
        b[i] = 255;
        assert_eq!(super::diff_bbox(&a, &b, w, h), Some((2, 1, 1, 1)));
        // 两个角变化 → 包围矩形。
        b[0] = 1;
        let last = ((3 * w + 3) * 4) as usize;
        b[last] = 1;
        assert_eq!(super::diff_bbox(&a, &b, w, h), Some((0, 0, 4, 4)));
        // 整帧不同 → 全屏 bbox。
        let c = vec![9u8; (w * h * 4) as usize];
        assert_eq!(super::diff_bbox(&a, &c, w, h), Some((0, 0, 4, 4)));
    }

    fn make_info() -> super::GameInfo {
        use std::sync::Arc;
        super::GameInfo {
            chart_time: 10.0, chart_beat: 20.0, audio_time: 10.0, fps: 60.0,
            frame_latency_ms: 16.0, playing: true,
            combo: 100, hits: 100, note_count: 656, score: 500_000,
            lines: 90, visible_notes: 300, paused: false, dim: 1.0,
            chart_name: "Test".into(), composer: "T".into(), level: "IN 15".into(), difficulty: 15.0,
            offset: 0.0, duration: 224.0,
            show_overlay: true, show_properties: true, show_events: true,
            show_notes: true, events_progress: 1.0, notes_progress: 1.0,
            has_custom_tex: false, full_notes: false,
            selected_line: 0, line_name: "line0".into(), line_count: 90,
            selected_layer: 0, max_layers: 1,
            events: Arc::new(vec![]), notes: Arc::new(vec![]),
            selected_notes: Arc::new(vec![]), selected_events: Arc::new(vec![]),
            gui_scale: 1.0, snap: 0.25, vsync: true, vertical_split: 1,
            selected_tool: 0, show_menu: false, selected_event_idx: None,
            event_edit_target: 0, ev_kind: String::new(),
            ev_start_beats: 0.0, ev_end_beats: 0.0, ev_start_val: 0.0,
            ev_end_val: 0.0, ev_easing: 0, effect_names: vec![], num_edit: None,
            eff_kf_var: None, eff_kf_sel: None, eff_kf_rows: vec![],
            loop_on: false, loop_a: None, loop_b: None, loop_a_time: None, loop_b_time: None, loop_toast: None,
            line_comment: false, comment_edit: None,
            ..Default::default()
        }
    }

    /// PMCORE-69 验收量测:面板动画/光标动画期间 diff 路径上传字节显著
    /// 小于全屏(iced 顶层文本 ~20KB、光标 ~4KB)。需要 GPU 适配器,默认忽略。
    #[test]
    #[ignore]
    fn dirty_upload_bytes_well_below_fullscreen() {
        use std::thread;
        use std::time::Duration;
        let (w, h) = (1280u32, 800u32);
        // measure bin 的 crate root 没有 render 模块,用库路径 phimakor::render。
        let rt = pollster::block_on(phimakor::render::preview::PreviewEngine::new(w, h)).unwrap();
        let r = rt.renderer();
        let mut ov = super::IcedOverlay::new(r.device(), r.tex_bgl(), r.sampler(), w, h);
        let mut info = make_info();
        let notes: Vec<super::NoteEntry> = (0..200).map(|i| super::NoteEntry {
            index: i, kind: if i % 4 == 0 { 2 } else { 1 },
            start_beats: i as f64 * 0.2, end_beats: i as f64 * 0.2 + 0.5,
            x: ((i % 10) as f32 - 5.0) * 60.0, scale: 1.0,
            comment: false,
        }).collect();
        info.notes = std::sync::Arc::new(notes);
        info.events = std::sync::Arc::new(vec![]);
        let rows: Vec<(String, String)> = vec![
            ("chart_name".into(), "Test Chart".into()),
            ("composer".into(), "Composer".into()),
            ("level".into(), "IN 15".into()),
            ("difficulty".into(), "15.0".into()),
        ];
        ov.chart_grid = Some(super::widgets::KeyValueGrid::new(0.0, 60.0, 300.0, rows));
        let full = (w * h * 4) as u64;
        ov.render_iced(r.queue(), &info);
        thread::sleep(Duration::from_millis(10));
        ov.render_iced(r.queue(), &info);
        thread::sleep(Duration::from_millis(10));
        // 首帧后快照必须已建立(全量上传)。
        assert_eq!(ov.iced_last.len(), full as usize);
        assert_eq!(ov.tl_last.len(), full as usize);

        // 面板打开动画:iced 只动顶层文本条(透明面板容器不产生像素差)。
        ov.panel_progress = 0.0;
        let mut iced_bytes = 0u64;
        let mut frames = 0u32;
        for _ in 0..60 {
            let prev = ov.iced_last.clone();
            ov.render_iced(r.queue(), &info);
            thread::sleep(Duration::from_millis(2));
            frames += 1;
            if let Some((_, _, bw, bh)) = super::diff_bbox(&prev, &ov.iced_last, w, h) {
                iced_bytes += bw as u64 * bh as u64 * 4;
            }
            if ov.panel_progress >= 1.0 { break; }
        }
        eprintln!("[dirty-test] panel anim iced bytes = {iced_bytes}B over {frames}f (full {full}B, avg {}/f)",
            iced_bytes / frames.max(1) as u64);
        // 每帧平均远小于全屏(量测 ~20KB ≈ 0.5% of 4MB)。
        assert!((iced_bytes / frames.max(1) as u64) < full / 20,
            "panel anim iced avg {}/f should be < 5% of full", iced_bytes / frames.max(1) as u64);

        // 光标动画:tl 只动光标区,iced 完全不动。先让面板动画完全收敛。
        ov.panel_progress = 1.0;
        ov.events_progress = 1.0;
        ov.notes_progress = 1.0;
        ov.render_iced(r.queue(), &info);
        thread::sleep(Duration::from_millis(5));
        ov.render_iced(r.queue(), &info);
        thread::sleep(Duration::from_millis(5));
        ov.custom_cursor = true;
        ov.cursor_trail = vec![(300.0, 300.0), (304.0, 302.0)];
        info.playing = false;
        info.chart_beat = 20.0;
        ov.last_drawn_beat = 20.0;
        let mut tl_bytes = 0u64;
        let mut iced_bytes = 0u64;
        for i in 0..10 {
            ov.mouse_pos = Some((316.0 + i as f32, 308.0 + i as f32));
            ov.cursor_move = 1.0;
            ov.cursor_dirty = true;
            let prev_iced = ov.iced_last.clone();
            let prev_tl = ov.tl_last.clone();
            ov.render_iced(r.queue(), &info);
            thread::sleep(Duration::from_millis(2));
            if let Some((_, _, bw, bh)) = super::diff_bbox(&prev_tl, &ov.tl_last, w, h) {
                tl_bytes += bw as u64 * bh as u64 * 4;
            }
            if let Some((_, _, bw, bh)) = super::diff_bbox(&prev_iced, &ov.iced_last, w, h) {
                iced_bytes += bw as u64 * bh as u64 * 4;
            }
        }
        eprintln!("[dirty-test] cursor anim tl bytes = {tl_bytes}B, iced bytes = {iced_bytes}B (full {full}B)");
        assert!(tl_bytes < full / 10, "cursor anim tl upload {tl_bytes}B should be < 10% of full");
        assert_eq!(iced_bytes, 0, "cursor anim must not touch iced_tex");
    }

    // ── Task B:notes 面板流控区域(单一真源,flow.rs)──

    fn sample_pt() -> PanelTransform {
        PanelTransform::new(4.0, 8.0, 1, 1.0, 1280.0, 800.0, 1.0, 1.0)
    }

    fn sample_notes() -> Vec<NoteEntry> {
        vec![
            NoteEntry { index: 0, kind: 1, start_beats: 5.0, end_beats: 5.0, x: 0.0, scale: 1.0, comment: false },
            NoteEntry { index: 1, kind: 2, start_beats: 6.0, end_beats: 9.0, x: 100.0, scale: 1.0, comment: false },
        ]
    }

    /// 区域注册:空白在最底,音符块按索引 id,hold 注册 HoldTail 在上层,
    /// 幽灵/列线 disabled(不命中)。
    #[test]
    fn notes_areas_blank_bottom_notes_top() {
        let pt = sample_pt();
        let notes = sample_notes();
        let areas = build_notes_areas(&pt, &notes, None, 1);
        // 空白在最底层。
        assert_eq!(areas.first().unwrap().kind, AreaKind::NoteBlank);
        assert_eq!(areas.first().unwrap().id, AREA_BLANK);
        assert!(!areas.first().unwrap().disabled);
        // 音符块 id = 索引 + 1;hold 是 HoldTail。
        let block = areas.iter().find(|a| a.id == AreaId(AREA_NOTE_BASE + 0)).unwrap();
        assert_eq!(block.kind, AreaKind::NoteBlock);
        let hold = areas.iter().find(|a| a.id == AreaId(AREA_NOTE_BASE + 1)).unwrap();
        assert_eq!(hold.kind, AreaKind::HoldTail);
        // 命中几何 = 绘制几何:空白覆盖整个面板内容区。
        let play = pt.notes_play_rect();
        let blank = areas.first().unwrap().rect;
        assert!(blank.0 <= play.0 && blank.1 <= play.1);
        assert!(blank.0 + blank.2 >= play.0 + play.2);
        // 幽灵 + 列线(v_split=2):disabled。
        let areas = build_notes_areas(&pt, &notes, Some((5.0, 5.5, 0.0, 1)), 2);
        assert!(areas.iter().any(|a| a.kind == AreaKind::Ghost && a.disabled));
        assert!(areas.iter().any(|a| a.kind == AreaKind::ColumnLine && a.disabled));
    }

    /// 命中:鼠标在音符容差矩形内 → 命中音符块;不在任何区域 → 空白。
    #[test]
    fn notes_areas_hit_resolves_note_and_blank() {
        let pt = sample_pt();
        let notes = sample_notes();
        let areas = build_notes_areas(&pt, &notes, None, 1);
        let mut f = UIFlow::new();
        f.areas = areas;
        // 音符 0 中心像素 → hover 命中音符块。
        let (bx, by) = (pt.pos_x_to_col_x(0.0), pt.beat_to_y(5.0));
        f.on_mouse(bx, by, &[ButtonState::Up; 3], ModifiersState::default());
        f.resolve();
        assert_eq!(f.hover, Some(AreaId(AREA_NOTE_BASE + 0)));
        // 面板空白角落 → hover 落到空白兜底。
        let play = pt.notes_play_rect();
        f.on_mouse(play.0 + 2.0, play.1 + 2.0, &[ButtonState::Up; 3], ModifiersState::default());
        f.resolve();
        assert_eq!(f.hover, Some(AREA_BLANK));
    }

    /// 点击判定:同 area 按下+释放 → clicked;音符块按下进入拖拽(captured 钉住)。
    #[test]
    fn flow_click_and_drag_pin() {
        let pt = sample_pt();
        let notes = sample_notes();
        let areas = build_notes_areas(&pt, &notes, None, 1);
        let (bx, by) = (pt.pos_x_to_col_x(0.0), pt.beat_to_y(5.0));
        let mut f = UIFlow::new();
        f.areas = areas.clone();
        // 按下音符 0。
        f.on_mouse(bx, by, &[ButtonState::Pressed, ButtonState::Up, ButtonState::Up], ModifiersState::default());
        let ev = f.resolve();
        assert!(ev.pressed.contains(&AreaId(AREA_NOTE_BASE + 0)));
        assert_eq!(f.captured, Some(AreaId(AREA_NOTE_BASE + 0)));
        // 拖动中 hover 钉在 captured(不随鼠标漂移)。
        f.on_mouse(bx + 30.0, by + 20.0, &[ButtonState::Pressed, ButtonState::Up, ButtonState::Up], ModifiersState::default());
        let ev = f.resolve();
        assert!(ev.pressed.is_empty() && ev.released.is_empty());
        assert_eq!(f.hover, Some(AreaId(AREA_NOTE_BASE + 0)));
        // 释放 → clicked(按下+释放同 area)。
        f.on_mouse(bx + 30.0, by + 20.0, &[ButtonState::Released, ButtonState::Up, ButtonState::Up], ModifiersState::default());
        let ev = f.resolve();
        assert!(ev.released.contains(&AreaId(AREA_NOTE_BASE + 0)));
        assert!(ev.clicked.contains(&AreaId(AREA_NOTE_BASE + 0)));
        assert_eq!(f.captured, None);
    }

    /// 覆盖顺序:hold 的 HoldTail 与音符块同容差矩形;反向命中先取后注册。
    #[test]
    fn flow_reverse_order_top_wins() {
        let pt = sample_pt();
        let notes = vec![
            NoteEntry { index: 0, kind: 1, start_beats: 5.0, end_beats: 5.0, x: 0.0, scale: 1.0, comment: false },
            NoteEntry { index: 1, kind: 1, start_beats: 5.0, end_beats: 5.0, x: 0.0, scale: 1.0, comment: false },
        ];
        let areas = build_notes_areas(&pt, &notes, None, 1);
        let mut f = UIFlow::new();
        f.areas = areas;
        let (bx, by) = (pt.pos_x_to_col_x(0.0), pt.beat_to_y(5.0));
        // 同点两个音符:后注册(索引大)者胜出(上层覆盖)。
        f.on_mouse(bx, by, &[ButtonState::Up; 3], ModifiersState::default());
        f.resolve();
        assert_eq!(f.hover, Some(AreaId(AREA_NOTE_BASE + 1)));
    }

    /// disabled(幽灵)不命中:鼠标在幽灵矩形上落回下层(空白/音符)。
    #[test]
    fn flow_disabled_ghost_never_hits() {
        let pt = sample_pt();
        let notes = vec![];
        let areas = build_notes_areas(&pt, &notes, Some((5.0, 5.5, 0.0, 1)), 1);
        let mut f = UIFlow::new();
        f.areas = areas;
        let (gx, gy) = (pt.pos_x_to_x(0.0), pt.beat_to_y(5.25));
        // 幽灵块中心:disabled → 命中空白(兜底),不是幽灵。
        f.on_mouse(gx, gy, &[ButtonState::Up; 3], ModifiersState::default());
        f.resolve();
        assert_eq!(f.hover, Some(AREA_BLANK));
    }

    /// note_index_from_area 往返:音符块/hold 尾 → 索引;空白/幽灵/列线 → None。
    #[test]
    fn note_index_from_area_roundtrip() {
        assert_eq!(note_index_from_area(AreaId(AREA_NOTE_BASE + 0)), Some(0));
        assert_eq!(note_index_from_area(AreaId(AREA_NOTE_BASE + 7)), Some(7));
        assert_eq!(note_index_from_area(AREA_BLANK), None);
        assert_eq!(note_index_from_area(AREA_GHOST), None);
        assert_eq!(note_index_from_area(AreaId(AREA_COL_BASE + 1)), None);
    }

    /// 拖拽/选择不因"释放被 modal 分支吞掉"而失灵:释放补喂后,
    /// 下一次按下必须重新检测到边沿(prev_buttons 回到 Released/Up)。
    #[test]
    fn flow_swallowed_release_then_next_press_still_edges() {
        let pt = sample_pt();
        let notes = sample_notes();
        let areas = build_notes_areas(&pt, &notes, None, 1);
        let (bx, by) = (pt.pos_x_to_col_x(0.0), pt.beat_to_y(5.0));
        let mut f = UIFlow::new();
        f.areas = areas.clone();
        // 按下音符 0(拖拽开始,底栏吞掉释放前)。
        f.on_mouse(bx, by, &[ButtonState::Pressed, ButtonState::Up, ButtonState::Up], ModifiersState::default());
        let ev = f.resolve();
        assert!(ev.pressed.contains(&AreaId(AREA_NOTE_BASE + 0)));
        assert_eq!(f.captured, Some(AreaId(AREA_NOTE_BASE + 0)));
        // 释放(模拟 feed_swallowed_release:底栏非 flow 区域,解析为空但仍消费边沿)。
        f.on_mouse(2000.0, 780.0, &[ButtonState::Released, ButtonState::Up, ButtonState::Up], ModifiersState::default());
        let ev = f.resolve();
        assert!(ev.released.contains(&AreaId(AREA_NOTE_BASE + 0)), "captured 释放解析到 captured");
        assert_eq!(f.captured, None);
        // 下一次按下音符 1:必须重新产生 press 边沿(否则选择/拖拽整体失灵)。
        let (bx1, by1) = (pt.pos_x_to_col_x(100.0), pt.beat_to_y(6.0));
        f.on_mouse(bx1, by1, &[ButtonState::Pressed, ButtonState::Up, ButtonState::Up], ModifiersState::default());
        let ev = f.resolve();
        assert!(ev.pressed.contains(&AreaId(AREA_NOTE_BASE + 1)), "吞掉释放后的下一次按下必须命中");
    }

    /// 容差矩形:±1 拍 / ±150 pos_x,与旧 find_nearest_note 阈值一致。
    #[test]
    fn note_hit_rect_covers_tolerance() {
        let pt = sample_pt();
        let n = NoteEntry { index: 0, kind: 1, start_beats: 5.0, end_beats: 5.0, x: 0.0, scale: 1.0, comment: false };
        let (x, y, w, h) = note_hit_rect(&n, &pt);
        assert!(w > 0.0 && h > 0.0);
        // 中心在矩形内。
        let (cx, cy) = (pt.pos_x_to_x(0.0), pt.beat_to_y(5.0));
        assert!(x <= cx && cx <= x + w && y <= cy && cy <= y + h);
        // ±1 拍边界点仍在容差内(beat 5±1)。
        let (y_lo, y_hi) = (pt.beat_to_y(6.0), pt.beat_to_y(4.0));
        assert!(y <= y_lo.min(y_hi) && y_hi.max(y_lo) <= y + h);
        // flow_area_kind 查 kind。
        let areas = build_notes_areas(&pt, &[n], None, 1);
        assert_eq!(flow_area_kind(&areas, AreaId(AREA_NOTE_BASE + 0)), Some(AreaKind::NoteBlock));
        assert_eq!(flow_area_kind(&areas, AREA_BLANK), Some(AreaKind::NoteBlank));
    }
}

// ── 5-column timeline ──










