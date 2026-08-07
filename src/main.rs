mod audio;
mod core;
mod render;
mod ui;

// mimalloc 全局分配器(PMCORE-67):tiny_skia/iced/JSON 每帧大量小分配,
// Windows 系统分配器是隐藏瓶颈,mimalloc 减少分配抖动。
// profiling feature 下由 dhat 独占 #[global_allocator](bug 94e4675d),此处互斥。
#[cfg(not(feature = "profiling"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use phimakor::trace_span;
use core::edit::{ChartDocument, EventKind, InfoField};
use ui::panels::LayoutDef;
use ui::widgets::Widget;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes, WindowId};

struct App {
    dir: Option<PathBuf>,
    state: Option<State>,
    /// 菜单 Load/Export 的 rfd 对话框线程回传结果(PMCORE-7)。
    dialog_rx: Option<std::sync::mpsc::Receiver<DialogChoice>>,
}

/// 菜单 Load/Export 对话框线程的结果(PMCORE-7)。
/// rfd 阻塞对话框放到后台线程运行,避免冻结 winit 事件循环。
enum DialogChoice {
    Load(Option<PathBuf>),
    Export(Option<PathBuf>),
}

// ── Core state ──
struct State {
    window: Arc<Window>,
    chart_dir: PathBuf,
    renderer: render::Renderer,
    overlay: ui::IcedOverlay,
    doc: ChartDocument,
    chart: core::chart::Chart,
    info: core::model::ChartInfo,
    audio: Option<audio::AudioHandle>,

    // ── Playback ──
    started: Instant,
    fps_since: Instant,
    fps: f64,
    frame_latency: f64,
    device_latency: f64,
    pending_seek: Option<f64>,
    scroll_target: Option<f64>,
    chart_time_last: f64,
    aspect_idx: usize,
    combo: u32,
    hits: u32,
    note_count: usize,
    /// PMCORE-21 谱面内容校验告警(加载时收集,存内存;Chart 面板显示条数)。
    chart_warnings: Vec<String>,
    seek_dim_until: Instant,
    /// Set when dragging the seek bar auto-paused playback; restored on release.
    drag_was_playing: bool,
    focused: bool,
    ctrl: bool,
    /// Shift 键状态(Shift+点击增选/减选,PMCORE-18)。
    shift: bool,
    /// Alt 键状态(Alt+方向键 nudge,PMCORE-18)。
    alt: bool,
    /// PMCORE-8:IME 组合中(splash 搜索框中文输入)。组合期间按键交给输入法,
    /// 应用不处理键盘输入,Commit 后一次性写入 splash_search。
    ime_active: bool,

    // ── UI state ──
    show_overlay: bool,
    show_properties: bool,
    show_events: bool,
    show_notes: bool,
    full_notes: bool,
    gui_scale: f32,
    /// 系统 DPI scale factor(高分屏缩放)。预乘进 gui_scale:UI 在 150%/200%
    /// 缩放下保持逻辑尺寸显示,不需要用户手动调大设置里的 gui scale。
    dpi_scale: f32,
    snap: f32, // snap interval in beats: 0.25, 0.5, 1.0
    vertical_split: u32, // number of vertical columns in notes panel (1 = no split)
    ui_dirty: bool,

    // ── Selection ──
    selected_line: usize,
    selected_layer: usize,
    selected_event_idx: Option<usize>,
    /// Note 拖拽:开始时的 (note 索引, 原 note) 快照;松手时一次
    /// replace_note 提交(PMCORE-19:拖拽期间不再每帧 remove+add 刷 undo)。
    /// PMCORE-18:多选拖拽时快照整组 (索引, 原 note),松手一次
    /// replace_notes_multi 提交(单 undo op)。
    drag_origin: Option<Vec<(usize, core::model::RPENote)>>,
    /// 拖拽锚点 = 鼠标实际抓住的音符(线内索引)。
    drag_anchor: Option<usize>,
    /// Note 拖拽:当前预览位置列表 (note 索引, start_beats, x, end_beats),
    /// 拖拽中面板显示用(PMCORE-18 整组预览)。
    drag_preview: Option<Vec<(usize, f64, f32, f64)>>,
    event_edit_target: u8, // 0=start_beats, 1=end_beats, 2=start_val, 3=end_val, 4=easing
    /// 事件时间轴空白列上次点击 (时刻, kind):同列 300ms 内再次点击 =
    /// 双击创建事件(PMCORE-20)。
    last_event_click: Option<(Instant, String)>,
    /// 双击/Insert 待创建事件:render_frame 顶部应用(彼时无 chart 借用,
    /// 避免与 frame 借用冲突,PMCORE-20)。
    pending_event_create: Option<(EventKind, f64)>,
    cache_valid: bool,
    cached_events: Arc<Vec<ui::EventEntry>>,
    cached_notes: Arc<Vec<ui::NoteEntry>>,

    // ── Clipboard(复制粘贴,PMCORE-XX)──
    /// 复制的音符全字段快照(Ctrl+C 写入)。决策:切谱/切线时**保留**——
    /// 与系统剪贴板同语义,可跨线/跨谱粘贴,粘贴目标恒为当前选中线。
    clipboard: Vec<core::model::RPENote>,

    // ── Layout / panels ──
    splash_mode: bool,
    splash_charts: Vec<ui::ChartEntry>,
    splash_search: String,
    splash_sel: Option<usize>,
    splash_sort: u8,
    splash_scroll: f32,
    splash_lib_path: String,
    /// PMCORE-24:上次异常退出时待恢复的 .bak 路径(splash 顶部提示条)。
    splash_recover: Option<String>,
    /// PMCORE-21:最近一次加载失败的可读错误(目录+原因+位置),splash 顶部显示。
    splash_error: Option<String>,
    splash_hover: ui::SplashHover,
    show_settings: bool,
    settings: ui::SettingsData,

    // ── 多难度(PMCORE-26)──
    /// 同曲多难度的歌曲根目录(IN/HD/EZ 子目录的父目录)。None = 单 chart 目录。
    song_root: Option<PathBuf>,
    /// 同曲难度列表(显示名, 难度子目录),按难度升序。单 chart 目录时为空。
    /// 编辑器内 N/B 键切换难度时据此定位目标目录。
    difficulties: Vec<(String, PathBuf)>,
    /// PMCORE-36:新建谱面命名对话框(None = 未打开)。
    splash_new: Option<ui::NewChartDlg>,

    // ── Post-processing effects ──
    extra: Option<core::extra::ExtraRoot>,
    /// Selected effect list row。字段行由 eff_form 组件承载(PMCORE-59)。
    selected_effect: Option<usize>,
    /// Double-click numeric input on the Eff panel keyframe 行(field + buffer)。
    num_edit: Option<NumEdit>,
    /// In-progress comment edit (PMCORE-77):右键菜单"注释"→ 输入框。
    comment_edit: Option<CommentEdit>,
    /// Last Eff keyframe-row click for double-click detection (time, row, 200)。
    last_eff_click: (std::time::Instant, Option<usize>, u8),
    /// Keyframe editor: expanded var index (into sorted var names) + selected
    /// keyframe row.
    eff_kf_var: Option<usize>,
    eff_kf_sel: Option<usize>,

    // ── BPM panel (widgets 组件库试点,tool 4)──
    /// 每帧从 ChartDocument 重建的 BPM 表单(持有交互焦点/拖拽状态)。
    bpm_form: Option<ui::widgets::RealtimeForm>,
    /// BPM 表单的交互焦点行(跨帧保留)。
    bpm_focus: Option<usize>,
    /// BPM 表单是否正在拖拽(滚轮/拖动期间不重建,避免状态丢失)。
    bpm_dragging: bool,

    // ── Settings panel (widgets 组件库,tool 2)──
    /// 每帧从 SettingsData 重建的设置表单。
    settings_form: Option<ui::widgets::RealtimeForm>,
    /// 设置表单是否正在拖拽。
    settings_dragging: bool,

    // ── Line panel (widgets 组件库,tool 1)──
    /// 线列表滚动条是否正在拖拽。
    line_dragging: bool,

    // ── Loading screen (切谱面后台加载)──
    /// 正在加载的谱面名(显示在加载屏)。
    loading_name: Option<String>,
    /// 后台加载线程(读+解析 chart,不碰 renderer)。
    loading_thread: Option<std::thread::JoinHandle<anyhow::Result<LoadedChart>>>,
    /// 加载开始时间(进度动画)。
    loading_start: Instant,

    // ── Preload (PMCORE-71: splash 悬停/键盘选中预加载)──
    /// 期望预载目标(悬停行或键盘选中行)+ 稳定计时起点。目标稳定
    /// >200ms 才 spawn 后台线程;改选/移开即重置(键盘快速切换去抖)。
    preload_want: Option<(PathBuf, Instant)>,
    /// 在途预载目标目录(None = 无在途线程)。
    preload_target: Option<PathBuf>,
    /// 预载 generation:每次 spawn/丢弃 +1;线程完成时带回,比对防错应用。
    preload_gen: u64,
    /// 预载完成结果接收槽(主线程每帧 poll)。内存上限:1 份在途 + 1 份结果。
    preload_rx: Option<std::sync::mpsc::Receiver<(u64, anyhow::Result<LoadedChart>)>>,
    /// 已完成的预载结果 (目标目录, 结果)。点击命中直接 apply,跳过 loading 屏。
    preload_slot: Option<(PathBuf, LoadedChart)>,

    // ── A-B loop (PMCORE-22) ──
    /// A 点(拍)。None = 未设。
    loop_a: Option<f64>,
    /// B 点(拍)。
    loop_b: Option<f64>,
    /// 循环开关(P 键)。
    loop_on: bool,
    /// 上一帧音频时间(循环回跳的"自然前向穿过 B"判定基准)。
    loop_prev_audio: f64,
    /// 短暂提示 toast(文本 + 触发时刻,2 秒后过期擦除)。
    loop_toast: Option<(String, std::time::Instant)>,
}


/// Target of an in-progress numeric edit (double-clicked value field).
#[derive(Clone, Copy)]
enum NumTarget {
    /// Keyframe row field: (keyframe index, sub-field: 0 = start, 1 = end).
    /// Eff 字段行的打字编辑已迁移到 RealtimeForm(PMCORE-59),不再走 num_edit。
    Kf(usize, u8),
}

/// Target of an in-progress comment edit (PMCORE-77).
#[derive(Clone, Copy)]
enum CommentTarget {
    /// (line index, note index)
    Note(usize, usize),
    /// judge line index
    Line(usize),
}

/// In-progress comment edit started from the right-click menu "注释" row.
struct CommentEdit {
    target: CommentTarget,
    /// Text buffer being typed.
    buf: String,
}

/// In-progress numeric edit started by double-clicking an Eff panel field.
struct NumEdit {
    target: NumTarget,
    /// Text buffer being typed.
    buf: String,
}

impl State {
}

/// Map the `backend` setting to wgpu backends (`None` = all / auto).
fn backends_from_settings(settings: &ui::SettingsData) -> wgpu::Backends {
    match settings.backend.as_deref() {
        Some("dx12") => wgpu::Backends::DX12,
        Some("vulkan") => wgpu::Backends::VULKAN,
        Some("gl") => wgpu::Backends::GL,
        _ => wgpu::Backends::all(),
    }
}

impl App {
    fn create_splash_state(&self, event_loop: &ActiveEventLoop, charts: Vec<ui::ChartEntry>) -> Option<State> {
        // Load persisted settings so the splash respects (and doesn't
        // clobber) the saved config: scale applies to the splash itself,
        // fullscreen/vsync apply to the splash window too.
        let settings = load_settings();
        let window = Arc::new(event_loop.create_window(
            WindowAttributes::default().with_title("phimakor").with_inner_size(LogicalSize::new(800.0, 600.0)),
        ).ok()?);
        // PMCORE-8:splash 搜索框需要系统 IME(winit 0.30 Windows 默认关闭)。
        window.set_ime_allowed(true);
        if settings.fullscreen {
            window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        }
        let mut renderer = pollster::block_on(render::Renderer::new(window.clone(), backends_from_settings(&settings))).ok()?;
        renderer.set_vsync(settings.vsync);
        let overlay = ui::IcedOverlay::new(renderer.device(), renderer.tex_bgl(), renderer.sampler(), 800, 600);
        let tmp = std::env::temp_dir().join("phimakor-splash");
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("info.json"), r#"{"chart":"chart.json","name":"splash"}"#).ok();
        std::fs::write(tmp.join("chart.json"), r#"{"META":{"offset":0},"BPMList":[{"bpm":120,"startTime":[0,0,1]}],"judgeLineList":[]}"#).ok();
        let doc = ChartDocument::open(&tmp).ok()?;
        let chart = core::chart::Chart::from_rpe_chart(doc.chart(), false).ok()?;
        let info = doc.info().clone();
        let dpi = window.scale_factor() as f32;
        let splash_recover = find_recover_hint(&charts);
        Some(State {
            window, chart_dir: PathBuf::new(), renderer, overlay, doc, chart, info,
            audio: None, started: Instant::now(), fps_since: Instant::now(),
            aspect_idx: 0, show_overlay: true,
            show_properties: false, show_events: false, show_notes: false, full_notes: false,
            combo: 0, hits: 0, note_count: 0,
            chart_warnings: Vec::new(),
            seek_dim_until: Instant::now(), fps: 0.0, frame_latency: 0.016,
            device_latency: std::env::var("PHIMAKOR_AUDIO_LATENCY_MS")
                .ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(15.0) / 1000.0,
            drag_was_playing: false,
            selected_line: 0, selected_event_idx: None, drag_origin: None, drag_anchor: None, drag_preview: None, scroll_target: None,
            pending_seek: None, chart_time_last: 0.0, focused: true, ctrl: false, shift: false, alt: false, ime_active: false,
            gui_scale: settings.gui_scale * dpi, dpi_scale: dpi, snap: 0.25, selected_layer: 0, event_edit_target: 0,
            last_event_click: None, pending_event_create: None,
            vertical_split: 14, ui_dirty: true, splash_mode: true, splash_charts: charts,
            splash_search: String::new(), splash_sel: None, splash_sort: 0, splash_scroll: 0.0,
            splash_lib_path: charts_dir().display().to_string(),
            splash_recover,
            splash_error: None,
            splash_hover: ui::SplashHover::None, show_settings: false, settings,
            splash_new: None,
            song_root: None, difficulties: Vec::new(),
            cache_valid: false, cached_events: Arc::new(Vec::new()), cached_notes: Arc::new(Vec::new()),
            clipboard: Vec::new(),
            extra: None, selected_effect: None, num_edit: None, comment_edit: None, last_eff_click: (std::time::Instant::now(), None, 0), eff_kf_var: None, eff_kf_sel: None,
            bpm_form: None, bpm_focus: None, bpm_dragging: false,
            settings_form: None, settings_dragging: false,
            line_dragging: false,
            loading_name: None, loading_thread: None, loading_start: Instant::now(),
            preload_want: None, preload_target: None, preload_gen: 0, preload_rx: None, preload_slot: None,
            loop_a: None, loop_b: None, loop_on: false, loop_prev_audio: 0.0, loop_toast: None,
        })
    }

    fn create_state(&mut self, event_loop: &ActiveEventLoop, dir: &PathBuf) -> anyhow::Result<State> {
        // PMCORE-26:歌曲根目录(无 info、含同曲难度子目录)→ 首个难度子目录。
        let dir = resolve_open_dir(dir);
        let window = Arc::new(event_loop.create_window(
            WindowAttributes::default().with_title("phimakor").with_inner_size(LogicalSize::new(1200.0, 800.0)),
        )?);
        // 编辑器模式不处理 IME(PMCORE-8 scope 仅 splash 搜索框)。
        window.set_ime_allowed(false);
        // Apply persisted settings (vsync, fullscreen, backend) to the fresh window.
        let settings = load_settings();
        let mut renderer = pollster::block_on(render::Renderer::new(window.clone(), backends_from_settings(&settings)))?;
        renderer.set_vsync(settings.vsync);
        if settings.fullscreen {
            window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            // Borderless fullscreen can hide the OS cursor — force it back.
            window.set_cursor_visible(true);
        }
        let res_dir = PathBuf::from("res");

        let mut doc = ChartDocument::open(&dir)?;
        // PMCORE-24:按设置开启自动保存(编辑后防抖落盘)。
        doc.set_autosave(settings.autosave, (settings.autosave_interval * 1000.0) as u64);
        let info = doc.info().clone();
        renderer.set_line_length(info.line_length);
        let chart = core::chart::Chart::from_rpe_chart(doc.chart(), info.use_rpe_170_speed == Some(true))?;
        renderer.post.chart_dir = Some(dir.clone());

        for name in chart.textures() {
            if let Ok(bytes) = std::fs::read(dir.join(&name)) {
                if let Err(e) = renderer.load_texture(&name, &bytes) { eprintln!("warning: texture {name}: {e:#}"); }
            }
        }
        for kind in ["click", "drag", "flick", "hold", "click_mh", "drag_mh", "flick_mh", "hold_mh", "hit_fx"] {
            let path = res_dir.join(format!("{kind}.png"));
            let key = if kind == "hit_fx" { "note:hitfx".to_string() } else { format!("note:{kind}") };
            if let Ok(bytes) = std::fs::read(&path) {
                if let Err(e) = renderer.load_texture(&key, &bytes) { eprintln!("warning: {kind}: {e:#}"); }
            }
        }
        let tex_dir = dir.join("Texture2D");
        if tex_dir.is_dir() {
            let custom_map: [(&str, &str); 4] = [("Tap", "click"), ("Drag", "drag"), ("Flick", "flick"), ("Hold", "hold")];
            for (file, key_suffix) in &custom_map {
                for ext in &[".png", ".jpg"] {
                    let path = tex_dir.join(format!("{file}{ext}"));
                    if let Ok(bytes) = std::fs::read(&path) {
                        let key = format!("note:{key_suffix}");
                        if let Err(e) = renderer.load_texture(&key, &bytes) { eprintln!("warning: custom {file}: {e}"); }
                        break;
                    }
                }
            }
        }
        if let Ok(bytes) = std::fs::read(dir.join(&info.illustration)) {
            if let Err(e) = renderer.set_background(&bytes, info.background_dim) { eprintln!("warning: bg: {e:#}"); }
        }

        // Load post-processing effects
        let extra = std::fs::read(dir.join("extra.json")).ok()
            .and_then(|b| core::extra::parse_extra(&b).ok());

        let audio = audio::spawn_audio_thread(
            res_dir.as_path(),
            &dir.join(&info.music),
            (chart.offset() + info.offset) as f64,
            chart.fire_events(),
        )
        .ok();
        let note_count = chart.max_combo();
        let layout = LayoutDef::load(&res_dir.join("panels.json"))
            .or_else(|_| LayoutDef::load(&PathBuf::from("res.dis/panels.json")))
            .unwrap_or(LayoutDef { panels: vec![] });
        let mut overlay = ui::IcedOverlay::new(renderer.device(), renderer.tex_bgl(), renderer.sampler(), 1200, 800);
        overlay.set_panels(layout.panels.clone());
        overlay.perf_hint = settings.perf_hint;
        overlay.fps_overlay = settings.fps_overlay;
        overlay.custom_cursor = settings.custom_cursor;
        overlay.hover_tooltip = settings.hover_tooltip;
        renderer.aggressive = settings.aggressive;
        renderer.post.half_res_enabled = settings.half_res_fx;
        // 启动时同步预热全部内置特效 pipeline(一次性编译 ~15 个 shader,
        // 启动多花 1-3s;之后特效首次出现不再卡帧)。切谱加载屏也会预热。
        renderer.warmup_effects();
        if settings.custom_cursor {
            window.set_cursor_visible(false);
        }
        let dpi = window.scale_factor() as f32;
        // PMCORE-26:按加载目录推导多难度上下文(同曲难度列表)。
        let (song_root, difficulties) = resolve_difficulty_context(&dir);
        Ok(State {
            window, chart_dir: dir, renderer, overlay, doc, chart, info, audio,
            song_root, difficulties,
            started: Instant::now(), fps_since: Instant::now(), aspect_idx: 0,
            show_overlay: true, show_properties: false, show_events: false, show_notes: false,
            full_notes: false, combo: 0, hits: 0, note_count,
            chart_warnings: Vec::new(),
            seek_dim_until: Instant::now(), fps: 0.0, frame_latency: 0.016,
            device_latency: std::env::var("PHIMAKOR_AUDIO_LATENCY_MS")
                .ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(15.0) / 1000.0,
            drag_was_playing: false,
            selected_line: 0, selected_event_idx: None, drag_origin: None, drag_anchor: None, drag_preview: None, event_edit_target: 0,
            scroll_target: None, pending_seek: None, chart_time_last: 0.0,
            focused: true, ctrl: false, shift: false, alt: false, ime_active: false, gui_scale: settings.gui_scale * dpi, dpi_scale: dpi, snap: 0.25, selected_layer: 0,
            last_event_click: None, pending_event_create: None,
            vertical_split: 14, ui_dirty: true, splash_mode: false,
            splash_charts: vec![], splash_search: String::new(), splash_sel: None, splash_sort: 0, splash_scroll: 0.0,
            splash_lib_path: String::new(), splash_recover: None, splash_error: None, splash_hover: ui::SplashHover::None,
            show_settings: false, settings,
            splash_new: None,
            cache_valid: false, cached_events: Arc::new(Vec::new()), cached_notes: Arc::new(Vec::new()), extra,
            clipboard: Vec::new(),
            selected_effect: None, num_edit: None, comment_edit: None,
            last_eff_click: (std::time::Instant::now(), None, 0), eff_kf_var: None, eff_kf_sel: None,
            bpm_form: None, bpm_focus: None, bpm_dragging: false,
            settings_form: None, settings_dragging: false,
            line_dragging: false,
            loading_name: None, loading_thread: None, loading_start: Instant::now(),
            preload_want: None, preload_target: None, preload_gen: 0, preload_rx: None, preload_slot: None,
            loop_a: None, loop_b: None, loop_on: false, loop_prev_audio: 0.0, loop_toast: None,
        })
    }

    /// Leave the editor and return to the splash screen: stop the audio
    /// thread, rescan the library, and swap in a fresh splash state.
    fn back_to_splash(&mut self, event_loop: &ActiveEventLoop) {
        // 复用当前 State 的 window/renderer/overlay,只切到 splash 数据
        // (PMCORE-63,避免 create_window 重建)。
        if let Some(state) = &mut self.state {
            // PMCORE-24:离开编辑器前把 saver 线程待写的快照落盘(阻塞,
            // 防止切到 splash 后打开别的谱面把未保存修改丢掉)。
            if let Err(e) = state.doc.flush() {
                eprintln!("flush on exit failed: {e:#}");
            }
            if let Some(a) = &state.audio { a.quit(); }
            state.audio = None;
            let charts = scan_charts();
            state.splash_mode = true;
            state.window.set_ime_allowed(true);
            state.ime_active = false;
            state.splash_charts = charts;
            state.splash_recover = find_recover_hint(&state.splash_charts);
            // PMCORE-21:回 splash 清掉上次加载错误(下一次失败会重新设置)。
            state.splash_error = None;
            state.splash_search.clear();
            state.splash_sel = None;
            state.splash_sort = 0;
            state.splash_scroll = 0.0;
            state.show_settings = false;
            // PMCORE-36:回 splash 时关闭可能遗留的新建对话框。
            state.splash_new = None;
            // PMCORE-71:回 splash 丢弃预载状态(悬停同一谱面会重新预载,不缓存复用)。
            state.preload_want = None;
            state.preload_gen += 1;
            state.preload_rx = None;
            state.preload_target = None;
            state.preload_slot = None;
            // IME 候选窗定位到搜索框(show_settings 已复位,PMCORE-8)。
            state.update_ime_area();
            state.show_properties = false;
            state.show_events = false;
            state.show_notes = false;
            state.ui_dirty = true;
            return;
        }
        let charts = scan_charts();
        if let Some(s) = self.create_splash_state(event_loop, charts) {
            self.state = Some(s);
        }
    }

    /// Reuse the current editor state and reload a chart directory, keeping
    /// the window/renderer/overlay alive (no create_window on switch).
    fn open_chart(&mut self, event_loop: &ActiveEventLoop, path: &std::path::Path) {
        // PMCORE-26:歌曲根目录(无 info、含同曲难度子目录)→ 首个难度子目录。
        let path = resolve_open_dir(path);
        // 已有 State(编辑器或 splash):复用窗口重载谱面(PMCORE-63)。
        // splash 模式的 doc/chart 是临时空谱,reload 直接替换。
        if let Some(state) = &mut self.state {
            // PMCORE-71:预载命中 → 直接 apply_loaded_chart(仅 GPU 上传,
            // 跳过 loading 屏)。无论是否命中都先清空预载状态:在途线程
            // 经 channel send 失败自然退出,不 double-load。
            let preloaded = state.preload_slot.take();
            state.preload_want = None;
            state.preload_gen += 1;
            state.preload_rx = None;
            state.preload_target = None;
            if let Some((slot_path, loaded)) = preloaded {
                if slot_path == path {
                    state.splash_mode = false;
                    // 进入编辑器:关闭 IME(splash 搜索框专用,PMCORE-8)。
                    state.window.set_ime_allowed(false);
                    state.ime_active = false;
                    // 与 reload_chart 相同的前置:停旧音频 + 清 chart 纹理,
                    // 再设 chart_dir(apply_loaded_chart 用它填 post.chart_dir)。
                    if let Some(a) = &state.audio { a.quit(); }
                    state.audio = None;
                    state.renderer.clear_chart_textures();
                    state.chart_dir = path.to_path_buf();
                    // PMCORE-26:预载直通路径不经过 reload_chart,补设难度上下文。
                    let (song_root, difficulties) = resolve_difficulty_context(&path);
                    state.song_root = song_root;
                    state.difficulties = difficulties;
                    match state.apply_loaded_chart(loaded) {
                        Ok(()) => return,
                        // 预载结果损坏等:落到正常 reload 路径(走错误提示)。
                        Err(e) => { eprintln!("failed to apply preloaded {path:?}: {e:#}"); }
                    }
                }
            }
            match state.reload_chart(&path) {
                Ok(()) => {
                    state.splash_mode = false;
                    // 进入编辑器:关闭 IME(splash 搜索框专用,PMCORE-8)。
                    state.window.set_ime_allowed(false);
                    state.ime_active = false;
                    return;
                }
                Err(e) => { eprintln!("failed to reload {path:?}: {e:#}"); }
            }
        }
        // 首次启动(无 State):创建完整状态。
        self.state = None;
        match self.create_state(event_loop, &path.to_path_buf()) {
            Ok(st) => self.state = Some(st),
            Err(e) => { eprintln!("failed to load {path:?}: {e:#}"); }
        }
    }

    // ── 菜单 Load/Export 文件对话框(PMCORE-7)──

    /// 菜单 Load:弹 rfd 文件选择(谱面包/谱面文件),取消则回退选目录。
    /// 对话框在后台线程运行,结果经 channel 回传,由 [`App::poll_dialog`] 消费。
    fn start_chart_dialog(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("打开谱面")
                .add_filter("谱面", &["zip", "json", "yml", "yaml", "txt", "pec"])
                .pick_file()
                .or_else(|| rfd::FileDialog::new().set_title("打开谱面目录").pick_folder());
            let _ = tx.send(DialogChoice::Load(picked));
        });
        self.dialog_rx = Some(rx);
    }

    /// 菜单 Export:弹 rfd 目录选择,选中的目录作为导出位置。
    fn start_export_dialog(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let dir = rfd::FileDialog::new().set_title("选择导出目录").pick_folder();
            let _ = tx.send(DialogChoice::Export(dir));
        });
        self.dialog_rx = Some(rx);
    }

    /// 每帧轮询对话框结果(由 about_to_wait 调用;ControlFlow::Poll 下循环
    /// 持续唤醒,channel 就绪即被消费)。
    fn poll_dialog(&mut self, event_loop: &ActiveEventLoop) {
        let Some(rx) = &self.dialog_rx else { return };
        match rx.try_recv() {
            Ok(DialogChoice::Load(picked)) => {
                self.dialog_rx = None;
                if let Some(path) = picked {
                    self.open_chart_from_pick(event_loop, &path);
                }
            }
            Ok(DialogChoice::Export(dir)) => {
                self.dialog_rx = None;
                if let Some(dir) = dir {
                    self.export_current_chart(&dir);
                }
            }
            Err(_) => {} // 对话框仍在运行,下一帧再查
        }
    }

    /// 菜单 Load 结果:目录 → 直接加载;.zip 谱面包 → import_chart_zip 解包
    /// 后加载;单文件(chart.json/info.json/…) → 按其所在目录加载。
    /// 任何失败仅 eprintln 提示,保持当前谱面不变。
    fn open_chart_from_pick(&mut self, event_loop: &ActiveEventLoop, path: &Path) {
        if path.is_dir() {
            self.open_chart(event_loop, path);
            return;
        }
        if probe_chart_zip(path).is_ok() {
            match import_chart_zip(path) {
                Ok(dir) => self.open_chart(event_loop, &dir),
                Err(e) => eprintln!("import zip failed: {e:#}"),
            }
            return;
        }
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                self.open_chart(event_loop, dir);
            }
        }
    }

    /// 菜单 Export 结果:先同步保存 chart 到磁盘,再导出 .zip 谱面包。
    fn export_current_chart(&mut self, out_dir: &Path) {
        let Some(state) = &mut self.state else { return };
        if state.splash_mode {
            eprintln!("export: 当前没有打开的谱面");
            return;
        }
        // 导出前把最新编辑落盘(tmp+rename),打包内容与内存一致(PMCORE-25)。
        if let Err(e) = state.doc.save() {
            eprintln!("save before export failed: {e:#}");
        }
        let chart_dir = state.chart_dir.clone();
        let info = state.doc.info().clone();
        match export_chart_zip(&chart_dir, out_dir, &info) {
            Ok(zip_path) => eprintln!("exported chart: {}", zip_path.display()),
            Err(e) => eprintln!("export failed: {e:#}"),
        }
    }
}
/// 后台预解码的图片(RGBA + 尺寸,主线程直接建纹理)。
struct DecodedImage {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

/// 后台线程加载的谱面数据(纯 IO + 解析 + 图片解码,不碰 GPU)。
/// 注意:core::chart::Chart 含 `Rc<dyn TweenFunction>`(easing 缓存),不是
/// Send,不能跨线程传回——主线程在 apply 时构建 Chart。
struct LoadedChart {
    doc: ChartDocument,
    /// 谱面目录名(加载屏显示)。
    name: String,
    /// chart 纹理: (名字, 预解码)。从 RPEChart 的 line texture 提取。
    textures: Vec<(String, DecodedImage)>,
    /// 自定义 note 纹理: (key, 预解码)。
    custom_textures: Vec<(String, DecodedImage)>,
    /// 背景图(预解码 + 已高斯模糊,σ=8)。
    bg: Option<DecodedImage>,
    /// 背景 dim。
    bg_dim: f32,
    /// extra.json 解析结果。
    extra: Option<core::extra::ExtraRoot>,
    /// 音频句柄(在后台线程等 ready,不阻塞主线程)。
    audio: Option<audio::AudioHandle>,
    /// PMCORE-21 谱面内容校验告警(存内存,编辑器 UI 展示用)。
    chart_warnings: Vec<String>,
}

/// 解码图片 + 垂直翻转(wgpu v=0 是顶行,与 upload_image 一致)。
fn decode_image(bytes: &[u8]) -> anyhow::Result<DecodedImage> {
    let mut img = image::load_from_memory(bytes)
        .map_err(|e| anyhow::anyhow!("failed to decode image: {e}"))?
        .to_rgba8();
    image::imageops::flip_vertical_in_place(&mut img);
    let (w, h) = (img.width().max(1), img.height().max(1));
    Ok(DecodedImage { rgba: img.into_raw(), w, h })
}

/// Path → 目录名(空路径回退 ""):加载屏/回退显示共用。
fn dir_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 后台加载:读 + 解析谱面 + 预解码纹理/背景 + 音频就绪。
fn load_chart_async(dir: PathBuf) -> anyhow::Result<LoadedChart> {
    let doc = ChartDocument::open(&dir)
        .map_err(|e| anyhow::anyhow!("load chart {}: {e:#}", dir.display()))?;
    let info = doc.info().clone();
    let name = dir_name(&dir);
    // PMCORE-21:内容级校验(负拍/越界/反序/BPM≤0/重复),默认放行+告警不阻断
    // 加载;结构级问题(解析失败)已在 open 的 Err 里硬失败。
    // 警告存内存(LoadedChart.chart_warnings)供编辑器 UI 展示;终端打印
    // 仅 PHIMAKOR_CHART_WARNINGS=1(避免每次加载刷屏)。
    let chart_warnings: Vec<String> = doc.chart().validate()
        .iter()
        .map(|issue| format!("line {}: {}", issue.line.map_or(0, |l| l + 1), issue.message))
        .collect();
    if std::env::var("PHIMAKOR_CHART_WARNINGS").is_ok() {
        for w in &chart_warnings {
            eprintln!("chart warning [{}]: {}", dir.display(), w);
        }
    }

    // 纹理清单:各线的 texture 字段(与 Chart::textures() 同源)。
    let mut textures = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in &doc.chart().judge_line_list {
        if line.texture.is_empty() || !seen.insert(line.texture.clone()) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(dir.join(&line.texture)) {
            if let Ok(img) = decode_image(&bytes) {
                textures.push((line.texture.clone(), img));
            } else {
                eprintln!("warning: decode texture {}", line.texture);
            }
        }
    }
    let mut custom_textures = Vec::new();
    let tex_dir = dir.join("Texture2D");
    if tex_dir.is_dir() {
        let custom_map: [(&str, &str); 4] = [("Tap", "click"), ("Drag", "drag"), ("Flick", "flick"), ("Hold", "hold")];
        for (file, key_suffix) in &custom_map {
            for ext in &[".png", ".jpg"] {
                let path = tex_dir.join(format!("{file}{ext}"));
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(img) = decode_image(&bytes) {
                        custom_textures.push((format!("note:{key_suffix}"), img));
                    }
                    break;
                }
            }
        }
    }
    // 背景:解码 + 高斯模糊(重活,放后台)。与 set_background 共用
    // Renderer::blur_background_rgba(fastblur SIMD,σ=8,PMCORE-66)。
    // 注意:背景不翻转(原 set_background 无 flip,与 upload_image 不同)。
    let (bg, bg_dim) = match std::fs::read(dir.join(&info.illustration)) {
        Ok(bytes) => {
            let bg = render::Renderer::blur_background_rgba(&bytes)
                .ok()
                .map(|(rgba, w, h)| DecodedImage { rgba, w, h });
            (bg, info.background_dim)
        }
        Err(_) => (None, info.background_dim),
    };
    let extra = std::fs::read(dir.join("extra.json")).ok()
        .and_then(|b| core::extra::parse_extra(&b).ok());
    // 音频就绪等待也放后台(音乐解码可能慢)。音频统一从暂停启动:
    // 预载结果在 splash 悬停期间不能播音乐;正常加载路径的 loading 屏
    // 也保持静音。进入编辑器由 apply_loaded_chart 恢复播放(play-on-open)。
    let audio = audio::spawn_audio_thread(
        Path::new("res"),
        &dir.join(&info.music),
        doc.chart().meta.offset as f64 / 1000.0 + info.offset as f64,
        core::chart::Chart::fire_events_from_rpe(doc.chart()),
    )
    .ok();
    if let Some(a) = &audio { a.set_paused(true); }

    Ok(LoadedChart { doc, name, textures, custom_textures, bg, bg_dim, extra, audio, chart_warnings })
}

impl State {
    /// Sorted (effect-index, start-beat) pairs — same ordering as the Eff
    /// panel list, so a list row maps back to `ExtraRoot::effects`.
    fn eff_sorted(&self) -> Vec<(usize, f64)> {
        let mut idx: Vec<(usize, f64)> = self.extra.as_ref().map_or(Vec::new(), |extra| {
            extra.effects.iter().enumerate().map(|(i, e)| (i, e.start.beats())).collect()
        });
        idx.sort_by(|a, b| a.1.total_cmp(&b.1));
        idx
    }

    /// Persist the current ExtraRoot to `extra.json` and mark the UI dirty.
    fn eff_save(&mut self) {
        if let Some(extra) = &self.extra {
            if let Err(e) = extra.save(&self.chart_dir.join("extra.json")) {
                eprintln!("extra.json save: {e}");
            }
        }
        self.ui_dirty = true;
    }

    /// Add a built-in effect spanning ±2 beats around the playhead, snapped to
    /// the beat grid (`snap`, e.g. 0.25) so start/end land on grid lines.
    fn eff_add(&mut self) {
        let beat = self.chart.time_to_beat(self.chart_time_last);
        let snap = self.snap.max(0.01) as f64;
        let beat = (beat / snap).round() * snap;
        let (name, defaults) = crate::render::shaders::EFFECTS.first()
            .map(|d| (d.name.to_string(), d.defaults.to_vec()))
            .unwrap_or_else(|| ("grayscale".to_string(), Vec::new()));
        let vars: std::collections::HashMap<String, serde_json::Value> = defaults
            .into_iter()
            .map(|(k, _, v)| (k.to_string(), serde_json::json!(v)))
            .collect();
        let extra = self.extra.get_or_insert_with(|| core::extra::ExtraRoot { bpm: vec![], effects: vec![] });
        extra.effects.push(core::extra::ExtraEffect {
            start: core::bpm::Triple::from_beats((beat - 2.0).max(0.0)),
            end: core::bpm::Triple::from_beats(beat + 2.0),
            shader: name,
            global: true,
            priority: 0,
            vars,
        });
        // Select the freshly added effect (its row after the start-beat sort).
        let new_idx = extra.effects.len() - 1;
        self.selected_effect = self.eff_sorted().iter().position(|(i, _)| *i == new_idx);
        self.overlay.eff_form = None; // 下一帧重建,清焦点/缓冲残留
        self.eff_save();
    }

    /// Remove the effect at the selected list row.
    fn eff_remove_selected(&mut self) {
        let Some(sel) = self.selected_effect else { return };
        let idx = self.eff_sorted();
        let Some((orig, _)) = idx.get(sel).copied() else { return };
        if let Some(extra) = &mut self.extra {
            extra.effects.remove(orig);
        }
        self.selected_effect = None;
        self.overlay.eff_form = None; // 下一帧重建,清焦点/缓冲残留
        self.eff_save();
    }

    /// Start numeric input for a keyframe row field (double-click).
    fn start_kf_num_edit(&mut self, kf: usize, sub: u8) {
        let Some(sel) = self.selected_effect else { return };
        let sorted = self.eff_sorted();
        let Some((orig, _)) = sorted.get(sel).copied() else { return };
        let Some(extra) = &self.extra else { return };
        let Some(e) = extra.effects.get(orig) else { return };
        let Some(kv) = self.eff_kf_var else { return };
        let mut keys: Vec<&String> = e.vars.keys().collect();
        keys.sort();
        let Some(key) = keys.get(kv) else { return };
        let Some(serde_json::Value::Array(kfs)) = e.vars.get(*key) else { return };
        let Some(kf_obj) = kfs.get(kf).and_then(|v| v.as_object()) else { return };
        let buf = match sub {
            0 => kf_obj.get("startTime")
                .and_then(|t| core::bpm::triple_to_beats(t))
                .map(|b| format!("{b:.3}")).unwrap_or_default(),
            1 => kf_obj.get("endTime")
                .and_then(|t| core::bpm::triple_to_beats(t))
                .map(|b| format!("{b:.3}")).unwrap_or_default(),
            _ => return,
        };
        self.num_edit = Some(NumEdit { target: NumTarget::Kf(kf, sub), buf });
        self.ui_dirty = true;
    }

    /// Commit the numeric edit (Enter): parse and write back to the effect.
    fn commit_num_edit(&mut self) {
        let Some(edit) = self.num_edit.take() else { return };
        let Ok(value) = edit.buf.parse::<f64>() else { return };
        let Some(sel) = self.selected_effect else { return };
        let sorted = self.eff_sorted();
        let Some((orig, _)) = sorted.get(sel).copied() else { return };
        let Some(extra) = &mut self.extra else { return };
        let Some(e) = extra.effects.get_mut(orig) else { return };
        // 只剩 keyframe 行编辑(Eff 字段行的打字已迁移到 RealtimeForm,PMCORE-59)。
        let NumTarget::Kf(kf, sub) = edit.target;
        let Some(kv) = self.eff_kf_var else { return };
        let mut keys: Vec<&String> = e.vars.keys().collect();
        keys.sort();
        let Some(key) = keys.get(kv).map(|k| (*k).clone()) else { return };
        let Some(serde_json::Value::Array(kfs)) = e.vars.get_mut(&key) else { return };
        let Some(kf_obj) = kfs.get_mut(kf).and_then(|v| v.as_object_mut()) else { return };
        let field = match sub {
            0 => "startTime",
            1 => "endTime",
            _ => return,
        };
        kf_obj.insert(field.to_string(), serde_json::json!([value, 0, 1]));
        self.eff_save();
    }

    /// Wheel over the expanded keyframe list: cycle the selected keyframe's
    /// easing (0..=29, RPE_TWEEN_MAP indices).
    fn eff_kf_wheel(&mut self, delta: f32) {
        if delta == 0.0 { return; }
        let (Some(kv), Some(ks)) = (self.eff_kf_var, self.eff_kf_sel) else { return };
        let Some(sel) = self.selected_effect else { return };
        let sorted = self.eff_sorted();
        let Some((orig, _)) = sorted.get(sel).copied() else { return };
        let Some(extra) = &mut self.extra else { return };
        let Some(e) = extra.effects.get_mut(orig) else { return };
        let mut keys: Vec<&String> = e.vars.keys().collect();
        keys.sort();
        let Some(key) = keys.get(kv).map(|k| (*k).clone()) else { return };
        let Some(serde_json::Value::Array(kfs)) = e.vars.get_mut(&key) else { return };
        let Some(kf_obj) = kfs.get_mut(ks).and_then(|v| v.as_object_mut()) else { return };
        let cur = kf_obj.get("easingType").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let next = (cur + delta.signum() as i32).rem_euclid(30);
        kf_obj.insert("easingType".to_string(), serde_json::json!(next));
        self.eff_save();
    }

    /// 每帧重建 Eff 面板表单(组件库 RealtimeForm,PMCORE-59)。
    /// 保留 prev 表单的焦点行 / Number 打字缓冲 / Combo 展开(跨帧持有)。
    /// 与 bpm_refresh_form 同:render_frame 的 frame 借用期间字段级操作。
    fn eff_refresh_form(&mut self) {
        let s = self.gui_scale;
        let pp = self.overlay.props_progress();
        let pan_w = ui::PANEL_W * s;
        let px = self.window.inner_size().width as f32 - pp * pan_w;
        let py = 56.0 * s;
        // 效果列表行(按 start beat 排序,同 eff_sorted)。
        let mut sorted: Vec<(usize, f64)> = self.extra.as_ref().map_or(Vec::new(), |extra| {
            extra.effects.iter().enumerate().map(|(i, e)| (i, e.start.beats())).collect()
        });
        sorted.sort_by(|a, b| a.1.total_cmp(&b.1));
        let effects: Vec<ui::eff_panel::EffListRow> = sorted.iter().filter_map(|(orig, _)| {
            self.extra.as_ref()?.effects.get(*orig).map(|e| ui::eff_panel::EffListRow {
                shader: e.shader.clone(),
                start: e.start.beats(),
                end: e.end.beats(),
                global: e.global,
            })
        }).collect();
        // 选中效果的 uniform 变量行(按键名排序;行索引 = eff_apply 的 vars 偏移)。
        let mut vars: Vec<ui::eff_panel::VarRow> = Vec::new();
        if let (Some(sel), Some(extra)) = (self.selected_effect, self.extra.as_ref()) {
            if let Some((orig, _)) = sorted.get(sel) {
                if let Some(e) = extra.effects.get(*orig) {
                    let mut keys: Vec<&String> = e.vars.keys().collect();
                    keys.sort();
                    for key in keys {
                        match e.vars.get(key) {
                            Some(serde_json::Value::Number(n)) => vars.push(ui::eff_panel::VarRow::Number {
                                name: key.clone(),
                                value: n.as_f64().unwrap_or(0.0),
                            }),
                            Some(serde_json::Value::Array(a)) => vars.push(ui::eff_panel::VarRow::Keyframed {
                                name: key.clone(),
                                n: a.len(),
                            }),
                            _ => vars.push(ui::eff_panel::VarRow::Other { name: key.clone() }),
                        }
                    }
                }
            }
        }
        // 自定义 shader 名(extra 中引用但非内置)。
        let customs: Vec<String> = self.extra.as_ref().map_or(Vec::new(), |extra| {
            let builtin: std::collections::HashSet<&str> =
                render::shaders::EFFECTS.iter().map(|d| d.name).collect();
            extra.effects.iter()
                .filter(|e| !builtin.contains(e.shader.as_str()))
                .map(|e| e.shader.clone())
                .collect()
        });
        let prev = self.overlay.eff_form.clone();
        let form = ui::eff_panel::build_form(
            px, py, pan_w, s,
            &effects,
            self.selected_effect,
            &vars,
            self.eff_kf_var,
            self.snap as f64,
            &customs,
            prev.as_ref(),
        );
        self.overlay.eff_form = Some(form);
    }

    /// 把 Eff 表单的当前值写回 extra(Shader/Start/End/Global/uniform 变量)。
    /// 仅在有实际变化时 eff_save(避免每次点击都写盘)。行布局与 build_form
    /// 一致:0..n 列表行,n=Shader,n+1=Start,n+2=End,n+3=Global,n+4+=vars。
    fn eff_apply(&mut self) {
        let Some(form) = self.overlay.eff_form.as_ref().map(|f| f.clone()) else { return };
        let Some(sel) = self.selected_effect else { return };
        let sorted = self.eff_sorted();
        let Some((orig, _)) = sorted.get(sel).copied() else { return };
        let Some(extra) = &mut self.extra else { return };
        let Some(e) = extra.effects.get_mut(orig) else { return };
        let n = sorted.len();
        let mut changed = false;
        // Shader(Combo 选择即写,等价旧 eff_wheel field 0 的循环语义)。
        if let Some((_, ui::widgets::RTControl::Combo { items, selected, .. })) = form.rows.get(n) {
            if let Some(name) = items.get(*selected) {
                if e.shader != *name {
                    e.shader = name.clone();
                    changed = true;
                }
            }
        }
        // Start/End:数值行,写回时保持旧语义(Start ≥ 0;End > Start+0.01)。
        let (start0, end0) = (e.start.beats(), e.end.beats());
        let mut start = start0;
        let mut end = end0;
        if let Some((_, ui::widgets::RTControl::Number { value, .. })) = form.rows.get(n + 1) {
            start = (*value).max(0.0);
        }
        if let Some((_, ui::widgets::RTControl::Number { value, .. })) = form.rows.get(n + 2) {
            end = (*value).max(start + 0.01);
        }
        if (start - start0).abs() > 1e-9 || (end - end0).abs() > 1e-9 {
            e.start = core::bpm::Triple::from_beats(start);
            e.end = core::bpm::Triple::from_beats(end);
            changed = true;
        }
        // Global 开关。
        if let Some((_, ui::widgets::RTControl::Toggle { on, .. })) = form.rows.get(n + 3) {
            if *on != e.global {
                e.global = *on;
                changed = true;
            }
        }
        // uniform 变量(Number 行;keyframed 数组行是只读 Text,跳过)。
        let mut keys: Vec<String> = e.vars.keys().cloned().collect();
        keys.sort();
        for (vi, key) in keys.iter().enumerate() {
            let Some((_, ui::widgets::RTControl::Number { value, .. })) = form.rows.get(n + 4 + vi) else { continue };
            let cur = match e.vars.get(key) {
                Some(serde_json::Value::Number(num)) => num.as_f64().unwrap_or(0.0),
                _ => continue,
            };
            let v = (*value * 1000.0).round() / 1000.0;
            if (v - cur).abs() > 1e-9 {
                e.vars.insert(key.clone(), serde_json::json!(v));
                changed = true;
            }
        }
        if changed {
            self.eff_save();
        }
    }

    /// var 行 vi(按排序键)是否为 keyframed 数组(点击 = 展开/收起,PMCORE-59)。
    fn eff_var_is_keyframed(&self, vi: usize) -> bool {
        let Some(sel) = self.selected_effect else { return false };
        let sorted = self.eff_sorted();
        let Some((orig, _)) = sorted.get(sel).copied() else { return false };
        let Some(extra) = &self.extra else { return false };
        let Some(e) = extra.effects.get(orig) else { return false };
        let mut keys: Vec<&String> = e.vars.keys().collect();
        keys.sort();
        keys.get(vi).is_some_and(|k| matches!(e.vars.get(*k), Some(serde_json::Value::Array(_))))
    }

    /// 滚轮路由命中检测:属性面板可见、指定 tool、且鼠标悬停在面板区域上。
    /// 命中来源 = flow.hover(单一真源,命中 = 绘制),不再自己算面板几何。
    /// 注意:仅控件行(含标题栏外的行/Add)命中,面板列内的空白缝隙/表单
    /// 下方区域不命中(滚轮落入下方 timeline/seek 分支)。
    /// tool 1(Line 面板)未迁入流控,保留旧 x 范围几何检查(防回归)。
    fn is_mouse_over_props(&self, tool: usize) -> bool {
        if self.splash_mode || !self.show_properties || self.overlay.selected_tool != tool {
            return false;
        }
        if tool == 1 {
            // Line 面板(未迁移):旧几何检查 = 鼠标 x 在面板列内。
            return self.overlay.mouse_pos.is_some_and(|(mx, _)| {
                let s = self.overlay.gui_scale;
                let pp = self.overlay.props_progress();
                let pan_w = ui::PANEL_W * s;
                let props_x = self.window.inner_size().width as f32 - pp * pan_w;
                mx >= props_x && mx <= props_x + pan_w
            });
        }
        self.overlay.flow.hover.is_some_and(|id| {
            matches!(
                ui::flow_area_kind(&self.overlay.flow.areas, id),
                Some(ui::flow::AreaKind::Widget(_))
            )
        })
    }

    fn rebuild_chart(&mut self) {
        let cur_time = self.chart_time_last;
        if let Ok(c) = core::chart::Chart::from_rpe_chart(self.doc.chart(), self.info.use_rpe_170_speed == Some(true)) {
            self.chart = c;
            self.note_count = self.chart.max_combo();
            // 编辑后刷新音频 hitsound 调度:调度是 spawn 时静态传入的,不刷
            // 新则新加的 note 永远不响(旧表里没有它们的时刻)。
            if let Some(a) = &self.audio {
                a.set_events(self.chart.fire_events());
            }
        }
        // Advance past current time so state_at doesn't re-fire all notes
        self.chart.state_at(cur_time);
        self.cache_valid = false;
        self.selected_event_idx = None;
    }

    /// 切谱面:启动后台加载线程 + 显示加载屏。
    /// 完成后由 render_frame 每帧轮询调用 [`State::apply_loaded_chart`]。
    fn reload_chart(&mut self, dir: &std::path::Path) -> anyhow::Result<()> {
        if let Some(a) = &self.audio { a.quit(); }
        self.audio = None;
        self.renderer.clear_chart_textures();
        self.chart_dir = dir.to_path_buf();
        // PMCORE-26:按新目录推导多难度上下文(同曲难度切换用)。
        let (song_root, difficulties) = resolve_difficulty_context(dir);
        self.song_root = song_root;
        self.difficulties = difficulties;
        let dir = dir.to_path_buf();
        let name = dir_name(&dir);
        self.loading_name = Some(name);
        // PMCORE-21:新加载开始,清掉上次错误提示。
        self.splash_error = None;
        self.loading_start = Instant::now();
        self.loading_thread = Some(std::thread::spawn(move || load_chart_async(dir)));
        self.ui_dirty = true;
        Ok(())
    }

    /// PMCORE-71: splash 预载状态机,每帧调用(render_frame 顶部)。
    /// 1) 期望目标(悬停行优先,否则键盘选中行)稳定 >200ms → spawn
    ///    load_chart_async 后台线程(复用现状加载函数,不卡主线程);
    /// 2) 线程完成经 channel 回主线程,gen 比对后存 preload_slot。
    /// 改选/移开即丢弃:gen++ + 丢 rx,在途线程 send 失败自然退出,无泄漏。
    fn poll_preload(&mut self) {
        if !self.splash_mode { return; }
        // 期望目标:悬停行优先(点击即开该行),否则键盘选中行。
        let filtered = ui::filter_charts(&self.splash_charts, &self.splash_search, self.splash_sort);
        let want = match self.splash_hover {
            ui::SplashHover::Chart(i) | ui::SplashHover::Delete(i) =>
                filtered.get(i).and_then(|&ci| self.splash_charts.get(ci)).map(|c| c.path.clone()),
            _ => self.splash_sel.and_then(|i| filtered.get(i))
                .and_then(|&ci| self.splash_charts.get(ci)).map(|c| c.path.clone()),
        };
        // 目标变化:丢弃已存结果(按拍板:改选不保留缓存),重置稳定计时;
        // 在途线程作废(目标路径不同才需作废)。
        let changed = match (&self.preload_want, &want) {
            (Some((p, _)), Some(w)) => p != w,
            (None, Some(_)) | (Some(_), None) => true,
            (None, None) => false,
        };
        if changed {
            self.preload_slot = None;
            self.preload_want = want.clone().map(|p| (p, Instant::now()));
            if self.preload_target.as_ref() != want.as_ref() {
                self.preload_gen += 1;
                self.preload_rx = None;
                self.preload_target = None;
            }
        }
        // 稳定 >200ms、无在途线程、且未命中已存结果 → spawn。
        if self.preload_target.is_none() {
            if let Some((p, since)) = self.preload_want.clone() {
                if since.elapsed() >= Duration::from_millis(200)
                    && self.preload_slot.as_ref().map_or(true, |(sp, _)| sp != &p) {
                    self.preload_gen += 1;
                    let gen = self.preload_gen;
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.preload_rx = Some(rx);
                    self.preload_target = Some(p.clone());
                    std::thread::spawn(move || { let _ = tx.send((gen, load_chart_async(p))); });
                }
            }
        }
        // 完成结果 → 存接收槽(gen 比对防错应用)。
        if let Some(rx) = self.preload_rx.take() {
            match rx.try_recv() {
                Ok((gen, result)) => {
                    if gen == self.preload_gen {
                        if let Some(p) = self.preload_target.take() {
                            match result {
                                Ok(loaded) => self.preload_slot = Some((p, loaded)),
                                // 预载失败静默:点击时走正常 loading 错误路径。
                                Err(e) => eprintln!("preload {p:?} failed (silent): {e:#}"),
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => self.preload_rx = Some(rx),
                // 线程退出但没发结果(panic):丢弃在途状态。
                Err(std::sync::mpsc::TryRecvError::Disconnected) => { self.preload_target = None; }
            }
        }
    }

    /// 主线程应用后台加载结果(renderer 相关的上传/创建)。
    /// 纹理已预解码(RGBA),音频已就绪——这里只做 GPU 创建/上传,轻量。
    fn apply_loaded_chart(&mut self, loaded: LoadedChart) -> anyhow::Result<()> {
        let LoadedChart { mut doc, name: _name, textures, custom_textures, bg, bg_dim, extra, audio, chart_warnings } = loaded;
        self.chart_warnings = chart_warnings;
        let info = doc.info().clone();
        self.renderer.set_line_length(info.line_length);
        let chart = core::chart::Chart::from_rpe_chart(doc.chart(), info.use_rpe_170_speed == Some(true))?;
        self.renderer.post.chart_dir = Some(self.chart_dir.clone());
        // 自定义特效预热:谱目录 *.wgsl 全部预编译——运行中首次激活某个
        // 自定义特效会同步读盘+编译(tens of ms),挪到切谱瞬间一次完成。
        self.renderer.warmup_custom_effects();
        for (k, img) in &textures {
            self.renderer.load_texture_rgba(k, &img.rgba, img.w, img.h);
        }
        for (k, img) in &custom_textures {
            self.renderer.load_texture_rgba(k, &img.rgba, img.w, img.h);
        }
        // res 内置 note 纹理(音符/命中特效)。splash 首次进谱面也走此路径,
        // 必须在这里加载,否则音符贴图缺失。体积小,主线程解码可接受。
        let res_dir = PathBuf::from("res");
        for kind in ["click", "drag", "flick", "hold", "click_mh", "drag_mh", "flick_mh", "hold_mh", "hit_fx"] {
            let path = res_dir.join(format!("{kind}.png"));
            let key = if kind == "hit_fx" { "note:hitfx".to_string() } else { format!("note:{kind}") };
            if let Ok(bytes) = std::fs::read(&path) {
                if let Err(e) = self.renderer.load_texture(&key, &bytes) { eprintln!("warning: {kind}: {e:#}"); }
            }
        }
        if let Some(img) = &bg {
            self.renderer.set_background_rgba(&img.rgba, img.w, img.h, bg_dim);
        }
        // 音频已在后台线程就绪(spawn_audio_thread 的 ready 等待不阻塞主线程)。
        // 加载/预载期间保持暂停(PMCORE-71:悬停不播音乐),进入编辑器恢复播放。
        self.audio = audio;
        if let Some(a) = &self.audio { a.set_paused(false); }

        // PMCORE-24:切谱后的新文档按设置开启自动保存。
        doc.set_autosave(self.settings.autosave, (self.settings.autosave_interval * 1000.0) as u64);
        self.doc = doc;
        self.info = info;
        self.chart = chart;
        self.extra = extra;
        self.note_count = self.chart.max_combo();
        self.combo = 0;
        self.hits = 0;
        self.selected_line = 0;
        self.selected_layer = 0;
        self.selected_event_idx = None;
        self.overlay.clear_selection(); // 切谱后 note 索引失效,清选中/多选/框选状态(PMCORE-18)
        self.drag_origin = None;
        self.drag_anchor = None;
        self.drag_preview = None;
        self.event_edit_target = 0;
        self.scroll_target = None;
        self.pending_seek = None;
        self.chart_time_last = 0.0;
        // A-B 循环点位属于谱面,切谱后重置(PMCORE-22)。
        self.loop_a = None;
        self.loop_b = None;
        self.loop_on = false;
        self.loop_prev_audio = 0.0;
        self.loop_toast = None;
        self.cache_valid = false;
        self.cached_events = Arc::new(Vec::new());
        self.cached_notes = Arc::new(Vec::new());
        self.selected_effect = None;
        self.eff_kf_var = None;
        self.eff_kf_sel = None;
        self.num_edit = None;
        self.comment_edit = None;
        self.bpm_form = None;
        self.bpm_focus = None;
        self.bpm_dragging = false;
        self.settings_form = None;
        self.settings_dragging = false;
        self.overlay.bpm_form = None;
        self.overlay.settings_form = None;
        self.overlay.eff_form = None;
        self.overlay.eff_form_hover = None;
        self.overlay.line_list = None;
        self.overlay.chart_grid = None;
        self.chart.state_at(0.0);
        self.loading_name = None;
        self.loading_thread = None;
        self.ui_dirty = true;
        Ok(())
    }

    /// 每帧轮询:后台加载完成则应用结果。返回是否仍在加载。
    fn poll_loading(&mut self) -> bool {
        let Some(handle) = self.loading_thread.take() else {
            return self.loading_name.is_some();
        };
        if !handle.is_finished() {
            self.loading_thread = Some(handle);
            return self.loading_name.is_some();
        }
        match handle.join() {
            Ok(Ok(loaded)) => {
                if let Err(e) = self.apply_loaded_chart(loaded) {
                    eprintln!("apply loaded chart: {e:#}");
                    self.loading_name = None;
                }
            }
            Ok(Err(e)) => {
                // PMCORE-21:加载失败 → splash 顶部显示可读错误(目录+原因+位置),
                // 不再只 eprintln 后静默回 splash。
                self.splash_error = Some(format!("{e:#}"));
                eprintln!("load chart: {e:#}");
                self.loading_name = None;
                // 加载失败:清掉空状态,回 splash(由 App 层检测 loading 结束
                // 后 splash_mode 仍为 false 时触发)。
                self.splash_mode = true;
            }
            Err(_) => {
                eprintln!("load thread panicked");
                self.splash_error = Some("加载线程异常终止(panic),详情见日志".into());
                self.loading_name = None;
                self.splash_mode = true;
            }
        }
        self.loading_name.is_some()
    }

    /// 每帧重建 BPM 面板表单(从 ChartDocument),保留焦点/拖拽状态。
    /// 若 overlay 已有表单(拖拽/滚轮中间态),保留其已编辑的值。
    /// 注意:这里只构建用于绘制的表单,不写回文档(写回在 bpm_apply,
    /// 由鼠标释放/滚轮事件调用,避开 render_frame 的 frame 借用)。
    /// 调用时机:render_frame 内 frame 借用期间 → 拆成字段级操作,
    /// 只用 `self.doc` 与 `self.overlay`(与 `self.chart` 借用不冲突)。
    fn bpm_refresh_form(&mut self) {
        // 注意:render_frame 里 `self.chart.state_at()` 持有对 self.chart
        // 的可变借用,这里不能拿 `&mut self` 全量。字段级借用即可。
        let s = self.gui_scale;
        let pp = self.overlay.props_progress();
        let pan_w = ui::PANEL_W * s;
        let px = self.window.inner_size().width as f32 - pp * pan_w;
        let py = 56.0 * s;
        let focus = if self.bpm_dragging {
            self.bpm_form.as_ref().and_then(|f| f.focus_row)
        } else {
            self.bpm_focus
        };
        // 拖拽/滚轮中:保留 overlay 表单的已编辑值(不重建)。
        if self.bpm_dragging {
            if let Some(form) = self.overlay.bpm_form.as_mut() {
                form.x = px;
                form.y = py;
                form.w = pan_w;
                return;
            }
        }
        let rows = self.doc.bpm_pairs();
        let form = ui::bpm_panel::build_form(px, py, pan_w, &rows, focus, s);
        self.overlay.bpm_form = Some(form);
    }

    /// 提交 BPM 表单改动(拖拽/滚轮结束、或面板切换时)。
    fn bpm_apply(&mut self) {
        let Some(form) = self.overlay.bpm_form.as_ref().map(|f| f.clone()) else { return };
        let new_rows = ui::bpm_panel::rows_of(&form);
        let old_rows = self.doc.bpm_pairs();
        if new_rows == old_rows {
            return;
        }
        // 增:行数变多 → 末尾新增(Add 按钮的 "var{n}" 标签解析为 beat=0/bpm=0,
        // 沿用最后一行)。
        if new_rows.len() > old_rows.len() {
            let n = old_rows.len();
            let (last_beat, last_bpm) = old_rows.last().copied().unwrap_or((0.0, 120.0));
            for (beat, bpm) in new_rows.iter().skip(n) {
                let beat = if *beat == 0.0 { last_beat + 0.01 } else { *beat };
                let bpm = if *bpm == 0.0 { last_bpm } else { *bpm };
                let _ = self.doc.add_bpm(bpm, beat);
            }
        }
        // 减:行数变少 → 删末尾(保留至少一行)。
        if new_rows.len() < old_rows.len() {
            while self.doc.chart().bpm_list.len() > new_rows.len().max(1) {
                let last = self.doc.chart().bpm_list.len() - 1;
                if self.doc.chart().bpm_list.len() <= 1 {
                    break;
                }
                let _ = self.doc.remove_bpm(last);
            }
        }
        // 替换:逐行 diff(值或起始拍变化)。
        for (i, (beat, bpm)) in new_rows.iter().enumerate() {
            let Some(item) = self.doc.chart().bpm_list.get(i) else { break };
            if (bpm - item.bpm).abs() > 1e-9 || (beat - item.start_time.beats()).abs() > 1e-9 {
                let _ = self.doc.replace_bpm(i, *bpm, *beat);
            }
        }
        self.rebuild_chart();
        self.cache_valid = false;
        self.ui_dirty = true;
    }

    /// 每帧重建设置面板表单(从 SettingsData),保留拖拽状态。
    /// 与 bpm_refresh_form 同:frame 借用期间字段级操作。
    fn settings_refresh_form(&mut self) {
        let s = self.gui_scale;
        let pp = self.overlay.props_progress();
        let pan_w = ui::PANEL_W * s;
        let px = self.window.inner_size().width as f32 - pp * pan_w;
        let py = 56.0 * s;
        // 拖拽中:保留 overlay 表单的已编辑值(不重建)。
        if self.settings_dragging {
            if let Some(form) = self.overlay.settings_form.as_mut() {
                form.x = px;
                form.y = py;
                form.w = pan_w;
                return;
            }
        }
        let mut form = ui::settings::build_settings_form(px, py, pan_w, s, &self.settings);
        // 保留上次表单的 Combo open 状态(下拉展开期间不能被重建收起)。
        if let Some(prev) = &self.overlay.settings_form {
            for (a, b) in form.rows.iter_mut().zip(prev.rows.iter()) {
                if let (ui::widgets::RTControl::Combo { open, .. }, ui::widgets::RTControl::Combo { open: prev_open, .. }) =
                    (&mut a.1, &b.1)
                {
                    *open = *prev_open;
                }
            }
        }
        self.overlay.settings_form = Some(form);
    }

    /// 提交设置表单改动 → SettingsData + 应用(vsync/fullscreen/scale)+ 持久化。
    fn settings_apply(&mut self) {
        let Some(form) = self.overlay.settings_form.as_ref().map(|f| f.clone()) else { return };
        if !ui::settings::apply_settings_form(&form, &mut self.settings) {
            return;
        }
        // 即时生效的设置。gui_scale 预乘系统 DPI scale(高分屏保持逻辑尺寸)。
        self.renderer.set_vsync(self.settings.vsync);
        self.renderer.aggressive = self.settings.aggressive;
        self.renderer.post.half_res_enabled = self.settings.half_res_fx;
        self.renderer.texture_compress = self.settings.texture_compress;
        self.dpi_scale = self.window.scale_factor() as f32;
        self.gui_scale = self.settings.gui_scale * self.dpi_scale;
        self.overlay.perf_hint = self.settings.perf_hint;
        self.overlay.fps_overlay = self.settings.fps_overlay;
        self.overlay.hover_tooltip = self.settings.hover_tooltip;
        // 自定义 GPU 光标:隐藏系统光标。
        if self.settings.custom_cursor != self.overlay.custom_cursor {
            self.overlay.custom_cursor = self.settings.custom_cursor;
            self.window.set_cursor_visible(!self.settings.custom_cursor);
        }        if self.settings.fullscreen {
            self.window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        } else {
            self.window.set_fullscreen(None);
        }
        // PMCORE-24:自动保存开关/间隔实时同步到当前文档(saver 线程共享 Arc)。
        self.doc.set_autosave(self.settings.autosave, (self.settings.autosave_interval * 1000.0) as u64);
        save_settings(&self.settings);
        self.ui_dirty = true;
    }

    /// Print a memory breakdown to the console (F7, or PHIMAKOR_MEMLOG=1
    /// every 5 s). Tracks the allocations the app itself controls.
    /// `include_gpu`: the wgpu registry report walks internal registries under
    /// locks; it is fine on an explicit F7 but was stuttering playback when
    /// the automatic 5 s MEMLOG report ran on the render thread — skip it
    /// there.
    fn debug_memory(&self, include_gpu: bool) {
        let (text_entries, digit_bytes, font_bytes) = self.renderer.text_mem();
        let mb = |b: usize| b as f64 / 1048576.0;
        eprintln!("──── memory report ────");
        if let Some((rss, peak, commit)) = process_mem() {
            let mb = |b: usize| b as f64 / 1048576.0;
            eprintln!("RSS {:.1} MB | peak {:.1} MB | commit {:.1} MB", mb(rss), mb(peak), mb(commit));
        }
        eprintln!("HUD text cache entries : {} (dynamic digits bypass it)", text_entries);
        eprintln!("digit glyph bitmaps    : {:.2} MB", mb(digit_bytes));
        eprintln!("renderer fonts (raw)   : {:.2} MB", mb(font_bytes));
        eprintln!("UI font chain (raw)    : {:.2} MB", mb(ui::font_mem_bytes()));
        eprintln!("audio hitsounds        : {:.2} MB", mb(self.audio.as_ref().map(|a| a.mem_bytes()).unwrap_or(0)));
        let thumbs = self.splash_charts.iter().filter(|c| c.thumb.is_some()).count();
        eprintln!("splash thumbnails      : {} (≤200 px each)", thumbs);
        let notes = self.doc.chart().judge_line_list.iter().map(|l| l.notes.as_ref().map_or(0, |n| n.len())).sum::<usize>();
        eprintln!("chart notes            : {} (parsed JSON kept in RAM)", notes);
        if include_gpu {
            if let Some(gpu) = self.renderer.gpu_mem() {
                eprintln!("wgpu live resources    : {gpu}");
            }
        }
    }

    fn seek(&mut self, t: f64) {
        let _s = trace_span!("seek");
        let t = t.clamp(0.0, self.chart.duration());
        if let Some(a) = &self.audio { a.seek(t); }
        // Seek 回到播放头跟随模式(手动滚动时间轴后 seek 会重新吸附)。
        self.overlay.tl_follow = true;
        self.pending_seek = Some(t);
        let off = (self.chart.offset() + self.info.offset) as f64;
        let ct = (t - off).max(0.0);
        // 前向与后向 seek 都同步图表命中游标(reposition):前向时跳过的 note
        // 不再补触发(hits_before 已计);后向时重放精确从目标开始——若只靠
        // state_at 的 seek-back 隐式重放,起点是下一帧的 chart_time(含 predict
        // 偏移),(目标, 目标+δ] 窗口的 note 会漏计(combo 重建后追不上
        // hits_before)。chart_time_last 同步到 ct(chart 秒,与 render_frame
        // 同单位),作为暂停归零钳制的高水位(combo 精度不变量,见 render_frame)。
        self.chart.reposition(ct);
        self.chart_time_last = ct;
        self.hits = self.chart.hits_before(ct) as u32;
        self.combo = self.hits;
        self.seek_dim_until = Instant::now() + Duration::from_millis(400);
    }

    /// 硬 seek:立即跳到 `t`,不走平滑动画(空格重播等场景)。
    /// 平滑 seek 会在滚动动画里逐帧给音频发 seek,导致重播时音频/显示
    /// 卡在中间时间。
    fn hard_seek(&mut self, t: f64) {
        let _s = trace_span!("hard_seek");
        let t = t.clamp(0.0, self.chart.duration());
        if let Some(a) = &self.audio { a.seek(t); }
        self.overlay.tl_follow = true;
        self.pending_seek = None;
        self.scroll_target = None;
        let off = (self.chart.offset() + self.info.offset) as f64;
        let ct = (t - off).max(0.0);
        // 同 seek():前后向统一 reposition;chart_time_last 用 chart 秒单位
        // (旧代码存的是含 offset 的绝对时间,单位不一致会让暂停钳制高水位错位)。
        self.chart.reposition(ct);
        self.chart_time_last = ct;
        self.hits = self.chart.hits_before(ct) as u32;
        self.combo = self.hits;
        self.seek_dim_until = Instant::now() + Duration::from_millis(400);
    }

    /// A-B 循环提示 toast(PMCORE-22):2 秒后 render_frame 过期擦除。
    fn loop_notice(&mut self, msg: impl Into<String>) {
        self.loop_toast = Some((msg.into(), Instant::now()));
        self.ui_dirty = true;
    }

    /// 设/清 A 点(I)或 B 点(O):同键再次按下清除对应点;A>=B 时拒绝并提示。
    fn set_loop_point(&mut self, is_a: bool) {
        // 当前播放头 beat,吸附 snap 网格(A=0 / 暂停态均正常)。
        let beat = ui::snap_beat(self.chart.time_to_beat(self.chart_time_last), self.snap as f64, 0.0);
        if is_a {
            if self.loop_a.is_some() {
                self.loop_a = None;
                self.loop_on = false;
                self.loop_notice(format!("A 点已清除"));
                return;
            }
            if self.loop_b.is_some_and(|b| beat >= b) {
                self.loop_notice("A-B: A 必须小于 B(先清 B)");
                return;
            }
            self.loop_a = Some(beat);
        } else {
            if self.loop_b.is_some() {
                self.loop_b = None;
                self.loop_on = false;
                self.loop_notice(format!("B 点已清除"));
                return;
            }
            if self.loop_a.is_some_and(|a| beat <= a) {
                self.loop_notice("A-B: B 必须大于 A(先清 A)");
                return;
            }
            self.loop_b = Some(beat);
        }
        self.loop_notice(format!("{} = {beat:.2} 拍", if is_a { "A" } else { "B" }));
    }

    /// 循环开关(P):A/B 任一未设或 A>=B 时提示并拒绝生效。
    fn toggle_loop(&mut self) {
        if self.loop_on {
            self.loop_on = false;
            self.loop_notice("循环关");
            return;
        }
        match loop_points_valid(self.loop_a, self.loop_b) {
            Ok(_) => {
                self.loop_on = true;
                self.loop_notice("循环开:播放到 B 自动回 A");
            }
            Err(e) => self.loop_notice(format!("循环无法开启:{e}")),
        }
    }

    /// 时间轴点击定位(PMCORE-22):beat 吸附 snap 后 seek(notes/events 面板空白单击共用)。
    fn seek_to_beat(&mut self, raw_beat: f64) {
        let beat = ui::snap_beat(raw_beat, self.snap as f64, 0.0);
        let off = (self.chart.offset() + self.info.offset) as f64;
        let t = self.chart.beat_to_time(beat) + off;
        self.seek(t);
    }

    /// 删除音符(PMCORE-17):走 doc.remove_note 入 undo 栈,单步 Ctrl+Z 还原。
    fn delete_note(&mut self, ni: usize) {
        if let Err(e) = self.doc.remove_note(self.selected_line, ni) {
            eprintln!("remove note: {e}");
            return;
        }
        self.overlay.selected_note = None;
        self.overlay.selected_notes.clear();
        self.rebuild_chart();
        self.ui_dirty = true;
    }

    /// 粘贴目标拍位:播放头 > 最近选中块 > 0(解析逻辑见 `resolve_target_beat`,
    /// PMCORE-XX)。
    fn paste_target_beat(&mut self) -> f64 {
        let playhead = self.chart.time_to_beat(self.chart_time_last);
        let sel_start = self.overlay.selected_note.and_then(|ni| {
            self.doc
                .chart()
                .judge_line_list
                .get(self.selected_line)
                .and_then(|l| l.notes.as_ref())
                .and_then(|ns| ns.get(ni))
                .map(|n| n.start_time.beats())
        });
        resolve_target_beat(playhead, self.snap as f64, sel_start)
    }

    /// Ctrl+C:复制选中音符(单选中或框选整组)为全字段快照。不修改文档、
    /// 不入 undo 栈。剪贴板跨线/跨谱保留(决策:与系统剪贴板同语义,粘贴
    /// 目标恒为当前选中线)。
    fn copy_selected_notes(&mut self) {
        let idxs: Vec<usize> = if !self.overlay.selected_notes.is_empty() {
            self.overlay.selected_notes.clone()
        } else if let Some(ni) = self.overlay.selected_note {
            vec![ni]
        } else {
            return;
        };
        let Some(notes) = self
            .doc
            .chart()
            .judge_line_list
            .get(self.selected_line)
            .and_then(|l| l.notes.as_ref())
        else {
            return;
        };
        let snap: Vec<core::model::RPENote> =
            idxs.iter().filter_map(|&i| notes.get(i).cloned()).collect();
        // 选中索引全部失效(理论不出现)时保持上次剪贴板内容不动。
        if !snap.is_empty() {
            self.clipboard = snap;
        }
    }

    /// Ctrl+V:粘贴剪贴板到当前线目标拍位(整组相对偏移保留,锚点吸附
    /// snap),经 doc.add_notes_multi 单 undo op。剪贴板跨线/跨谱保留。
    fn paste_clipboard(&mut self) {
        if self.clipboard.is_empty() {
            return;
        }
        let anchor = ui::snap_beat(self.paste_target_beat(), self.snap as f64, 0.0);
        let notes = paste_notes_transform(&self.clipboard, anchor);
        if let Err(e) = self.doc.add_notes_multi(self.selected_line, &notes) {
            eprintln!("paste notes: {e}");
            return;
        }
        self.overlay.selected_note = None;
        self.overlay.selected_notes.clear();
        self.rebuild_chart();
        self.ui_dirty = true;
    }

    /// Enter:在目标拍位放一个当前类型音符(吸附 snap,单 undo op)。
    /// "当前类型" = 单选音符的类型;无选中时默认 Tap。x 取鼠标在音符面板
    /// 的换算位置(与 Q/W/E/R 放置一致,DevManiaGrid 共享映射)。
    fn place_note_at_playhead(&mut self) {
        let kind: u8 = if self.overlay.selected_notes.len() == 1 {
            self.overlay
                .selected_note
                .and_then(|ni| {
                    self.doc
                        .chart()
                        .judge_line_list
                        .get(self.selected_line)
                        .and_then(|l| l.notes.as_ref())
                        .and_then(|ns| ns.get(ni))
                })
                .map(|n| n.kind)
                .unwrap_or(1)
        } else {
            1
        };
        use core::model::RPENote;
        let snap = self.snap as f64;
        let beats = ui::snap_beat(self.paste_target_beat(), snap, 0.0);
        let note = RPENote {
            kind,
            above: 1,
            start_time: core::bpm::Triple::from_beats(beats),
            // hold 时长 = 一个 snap 步长(与 Q/E 放置一致)。
            end_time: core::bpm::Triple::from_beats(beats + if kind == 2 { snap } else { 0.0 }),
            position_x: self.overlay.mouse_note_x().clamp(-675.0, 675.0),
            y_offset: 0.,
            alpha: 255,
            hitsound: None,
            size: 1.0,
            speed: 1.0,
            is_fake: 0,
            visible_time: 999999.,
            tint: None,
            tint_hit_effects: None,
            judge_area: None,
            comment: None,
        };
        if let Err(e) = self.doc.add_note(self.selected_line, note) {
            eprintln!("place note: {e}");
            return;
        }
        self.rebuild_chart();
        self.ui_dirty = true;
    }

    /// 打开注释编辑(PMCORE-77):预填现有注释(若有)。空提交 = 删除注释。
    fn start_comment_edit(&mut self, target: CommentTarget) {
        let existing = match target {
            CommentTarget::Note(li, ni) => self.doc.note_comment(li, ni).map(str::to_string),
            CommentTarget::Line(li) => self.doc.line_comment(li).map(str::to_string),
        };
        self.comment_edit = Some(CommentEdit {
            target,
            buf: existing.unwrap_or_default(),
        });
        self.ui_dirty = true;
    }

    /// 提交注释编辑(Enter):空文本 = 删除注释,否则写回 doc(PMCORE-77)。
    fn commit_comment_edit(&mut self) {
        let Some(edit) = self.comment_edit.take() else { return };
        let text = edit.buf.trim().to_string();
        let comment = if text.is_empty() { None } else { Some(text) };
        let res = match edit.target {
            CommentTarget::Note(li, ni) => self.doc.set_note_comment(li, ni, comment),
            CommentTarget::Line(li) => self.doc.set_line_comment(li, comment),
        };
        if let Err(e) = res {
            eprintln!("set comment: {e}");
        }
        self.cache_valid = false; // 注释标记立即刷新
        self.ui_dirty = true;
    }

    /// Alt+方向键微调选中音符(PMCORE-18):beat 步进 snap,x 步进 5 单位。
    /// 多选整组平移,单次 replace_notes_multi 提交(单 undo op)。
    fn nudge_selected(&mut self, dbeat: f64, dx: f32) {
        let idxs: Vec<usize> = if !self.overlay.selected_notes.is_empty() {
            self.overlay.selected_notes.clone()
        } else if let Some(ni) = self.overlay.selected_note {
            vec![ni]
        } else { return };
        let line = self.selected_line;
        let Some(notes) = self.doc.chart().judge_line_list.get(line).and_then(|l| l.notes.as_ref()) else { return };
        let mut changes: Vec<(usize, core::model::RPENote)> = Vec::with_capacity(idxs.len());
        for i in idxs {
            let Some(n) = notes.get(i) else { continue };
            let mut nn = n.clone();
            let nb = (nn.start_time.beats() + dbeat).max(0.0);
            nn.start_time = core::bpm::Triple::from_beats(nb);
            nn.end_time = core::bpm::Triple::from_beats((nn.end_time.beats() + dbeat).max(nb));
            nn.position_x = (nn.position_x + dx).clamp(-675.0, 675.0);
            changes.push((i, nn));
        }
        if changes.is_empty() { return; }
        if self.doc.replace_notes_multi(line, &changes).is_ok() {
            self.rebuild_chart();
            self.ui_dirty = true;
        }
    }

    fn edit_selected_event(&mut self, f: impl Fn(&mut core::model::RPEEvent<f32>)) {
        let _s = trace_span!("edit_selected_event");
        let Some(ev_idx) = self.selected_event_idx else { return };
        let line_events = extract_line_events(&self.doc, self.selected_line, self.selected_layer);
        let Some(entry) = line_events.get(ev_idx) else { return };
        let Some(kind) = event_kind_of(&entry.kind) else { return };
        // 读旧事件 → f 修改 → replace_event 单 op 写回(PMCORE-19:
        // remove+add 会往 undo 栈塞两个 op,一次按键占两步撤销)。
        let Some(mut ev) = self.event_at(self.selected_line, self.selected_layer, kind, entry.index) else { return };
        f(&mut ev);
        if self.doc.replace_event(self.selected_line, self.selected_layer, kind, entry.index, ev).is_ok() {
            self.rebuild_chart(); // 清选中 + 失效缓存
            // 替换不改 per-kind 索引,但 end_beats 排序可能变 → 按
            // (kind, per-kind index) 重定位扁平索引(PMCORE-20 同步要求)。
            let entries = extract_line_events(&self.doc, self.selected_line, self.selected_layer);
            if let Some(pos) = entries.iter().position(|e| e.kind == entry.kind && e.index == entry.index) {
                self.selected_event_idx = Some(pos);
            }
            self.ui_dirty = true;
        }
    }

    /// 事件时间轴双击/Insert 创建事件(PMCORE-20):beat 吸附 snap 网格,
    /// 值取默认 0→1,时长一个 snap 步长;经 doc.add_event 单 op 入 undo 栈。
    /// 创建后选中新事件(per-kind 索引 → 扁平索引重定位)。
    fn add_event_at(&mut self, kind: EventKind, raw_beat: f64) {
        let line = self.selected_line;
        let layer = self.selected_layer;
        let snap = self.snap as f64;
        let beats = ui::snap_beat(raw_beat, snap, 0.0);
        let ev = core::model::RPEEvent {
            easing_left: 0.0, easing_right: 1.0, bezier: 0, bezier_points: [0.0; 4],
            easing_type: 1,
            start: 0.0, end: 1.0,
            start_time: core::bpm::Triple::from_beats(beats),
            end_time: core::bpm::Triple::from_beats(beats + snap),
        };
        let inserted = match self.doc.add_event(line, layer, kind, ev) {
            Ok(i) => i,
            Err(e) => { eprintln!("add event: {e}"); return; }
        };
        self.rebuild_chart(); // 清选中 + 失效缓存
        let entries = extract_line_events(&self.doc, line, layer);
        if let Some(pos) = entries.iter().position(|e| e.kind == format!("{kind:?}") && e.index == inserted) {
            self.selected_event_idx = Some(pos);
        }
        self.overlay.selected_events.clear();
        self.last_event_click = None;
        self.ui_dirty = true;
    }

    /// 删除选中事件(单个或框选批量,PMCORE-20):按 kind 分组经
    /// remove_events_multi 提交,单 undo op;框选集合同列同类 → 单次调用。
    fn delete_selected_events(&mut self) {
        let idxs: Vec<usize> = if !self.overlay.selected_events.is_empty() {
            self.overlay.selected_events.clone()
        } else if let Some(i) = self.selected_event_idx {
            vec![i]
        } else { return };
        let line = self.selected_line;
        let layer = self.selected_layer;
        let line_events = extract_line_events(&self.doc, line, layer);
        let mut by_kind: Vec<(EventKind, Vec<usize>)> = Vec::new();
        for fi in idxs {
            let Some(entry) = line_events.get(fi) else { continue };
            let Some(kind) = event_kind_of(&entry.kind) else { continue };
            match by_kind.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, v)) => v.push(entry.index),
                None => by_kind.push((kind, vec![entry.index])),
            }
        }
        if by_kind.is_empty() { return; }
        for (kind, indices) in &by_kind {
            if self.doc.remove_events_multi(line, layer, *kind, indices).is_err() {
                return; // 原子校验失败 → 整体放弃
            }
        }
        self.selected_event_idx = None;
        self.overlay.selected_events.clear();
        self.last_event_click = None;
        self.rebuild_chart();
        self.ui_dirty = true;
    }

    /// 框选事件整组平移(Ctrl+方向键,PMCORE-20):同列同类按 kind 分组经
    /// replace_events_multi 提交,单 undo op;平移后吸附 snap 网格,并按
    /// (kind, per-kind index) 重定位选中(排序可能变化)。
    fn nudge_selected_events(&mut self, dbeat: f64) {
        let idxs: Vec<usize> = self.overlay.selected_events.clone();
        if idxs.len() < 2 { return; }
        let line = self.selected_line;
        let layer = self.selected_layer;
        let snap = self.snap as f64;
        let line_events = extract_line_events(&self.doc, line, layer);
        let mut by_kind: Vec<(EventKind, Vec<(usize, core::model::RPEEvent<f32>)>)> = Vec::new();
        let mut sel_pairs: Vec<(String, usize)> = Vec::with_capacity(idxs.len());
        for fi in idxs {
            let Some(entry) = line_events.get(fi) else { continue };
            let Some(kind) = event_kind_of(&entry.kind) else { continue };
            let Some(mut ev) = self.event_at(line, layer, kind, entry.index) else { continue };
            ev.start_time = core::bpm::Triple::from_beats(ui::snap_beat(ev.start_time.beats() + dbeat, snap, 0.0));
            ev.end_time = core::bpm::Triple::from_beats(ui::snap_beat(ev.end_time.beats() + dbeat, snap, 0.0));
            match by_kind.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, v)) => v.push((entry.index, ev)),
                None => by_kind.push((kind, vec![(entry.index, ev)])),
            }
            sel_pairs.push((entry.kind.clone(), entry.index));
        }
        if by_kind.is_empty() { return; }
        for (kind, changes) in &by_kind {
            if self.doc.replace_events_multi(line, layer, *kind, changes).is_err() {
                return;
            }
        }
        self.rebuild_chart();
        let entries = extract_line_events(&self.doc, line, layer);
        let new_sel: Vec<usize> = sel_pairs.iter().filter_map(|(k, i)| {
            entries.iter().position(|e| e.kind == *k && e.index == *i)
        }).collect();
        self.overlay.selected_events = new_sel;
        self.selected_event_idx = self.overlay.selected_events.first().copied();
        self.ui_dirty = true;
    }

    /// 框选结束后把事件主选中同步为框选集合首项(集合空则清空)。
    /// 只在鼠标位于事件面板上的释放/Ctrl 释放时调用(PMCORE-20)。
    fn sync_event_sel(&mut self) {
        self.selected_event_idx = self.overlay.selected_events.first().copied();
        self.ui_dirty = true;
    }

    /// 当前选中事件的 kind(Insert 无鼠标列时的回退用)。
    fn selected_event_kind(&self) -> Option<EventKind> {
        let i = self.selected_event_idx?;
        extract_line_events(&self.doc, self.selected_line, self.selected_layer)
            .get(i)
            .and_then(|e| event_kind_of(&e.kind))
    }

    /// (line, layer, kind, per-kind index) 处的旧事件克隆。
    fn event_at(&self, line: usize, layer: usize, kind: EventKind, index: usize) -> Option<core::model::RPEEvent<f32>> {
        self.doc.chart().judge_line_list.get(line)
            .and_then(|l| l.event_layers.get(layer))
            .and_then(|l| l.as_ref())
            .and_then(|ly| {
                let slot = match kind {
                    EventKind::Alpha => &ly.alpha_events,
                    EventKind::MoveX => &ly.move_x_events,
                    EventKind::MoveY => &ly.move_y_events,
                    EventKind::Rotate => &ly.rotate_events,
                    EventKind::Speed => &ly.speed_events,
                };
                slot.as_ref().and_then(|l| l.get(index)).cloned()
            })
    }

    /// 把 IME 候选窗定位到 splash 搜索框或新建对话框输入框(PMCORE-8/36)。
    /// 仅 splash 搜索可见时有效;gui_scale 已预乘系统 DPI,
    /// set_ime_cursor_area 收物理像素。
    fn update_ime_area(&self) {
        if !self.splash_mode || self.show_settings { return; }
        let vw = self.window.inner_size().width as f32;
        let vh = self.window.inner_size().height as f32;
        let (x, y, w, h) = if self.splash_new.is_some() {
            ui::splash_new_input_rect(vw, vh, self.gui_scale)
        } else {
            ui::splash_search_rect(vw, self.gui_scale)
        };
        self.window.set_ime_cursor_area(
            winit::dpi::PhysicalPosition::new(x as i32, y as i32),
            winit::dpi::PhysicalSize::new(w.max(1.0) as u32, h.max(1.0) as u32),
        );
    }

    fn render_frame(&mut self) {
        let _span = trace_span!("render_frame");
        // PMCORE-71:每帧推进 splash 预载状态机(悬停计时/spawn/结果接收)。
        self.poll_preload();
        // 双击/Insert 待创建事件:帧顶应用(此时无 chart 借用,PMCORE-20)。
        if let Some((kind, beat)) = self.pending_event_create.take() {
            self.add_event_at(kind, beat);
        }
        // 吸附步长/分栏/全谱预览同步到 overlay(hold 尾部拖拽/框选命中共用,PMCORE-17/18)。
        self.overlay.snap = self.snap;
        self.overlay.vertical_split = self.vertical_split;
        self.overlay.full_notes = self.full_notes;
        // Note drag (PMCORE-17/18): 拖拽期间只更新预览(不碰 doc/undo 栈),
        // 松手时一次 replace_notes_multi 提交整组(单 undo op)。旧实现每帧
        // remove+add,一次拖拽刷几十个 undo op(PMCORE-19)。
        if let Some((ni, beat, nx)) = self.overlay.drag_updated.take() {
            // 拖拽开始:快照全部参与音符(单选=1 项,多选=整组)。
            if self.drag_origin.is_none() {
                let notes = self.doc.chart().judge_line_list.get(self.selected_line)
                    .and_then(|l| l.notes.as_ref());
                let mut set: Vec<(usize, core::model::RPENote)> = self.overlay.selected_notes.iter()
                    .filter_map(|&i| notes.and_then(|ns| ns.get(i)).cloned().map(|n| (i, n)))
                    .collect();
                if !set.iter().any(|(i, _)| *i == ni) {
                    if let Some(n) = notes.and_then(|ns| ns.get(ni)).cloned() {
                        set.push((ni, n));
                    }
                }
                self.drag_anchor = Some(ni);
                self.drag_origin = Some(set);
            }
            // 预览:锚点取新位置,其余按同一 beat/x 偏移平移(整组移动)。
            if let (Some(anchor), Some(set)) = (self.drag_anchor, &self.drag_origin) {
                if let Some((_, an0)) = set.iter().find(|(i, _)| *i == anchor) {
                    let dbeat = beat - an0.start_time.beats();
                    let dx = nx - an0.position_x;
                    let single = set.len() == 1;
                    self.drag_preview = Some(set.iter().map(|(i, n)| {
                        let nb = n.start_time.beats() + dbeat;
                        let ne = if single && n.kind == 2 { nb + 1.0 } else { n.end_time.beats() + dbeat };
                        (*i, nb, n.position_x + dx, ne)
                    }).collect());
                }
            }
            // 面板预览:强制刷新 notes 缓存,绘制时覆盖拖拽位置。
            self.cache_valid = false;
        }
        if self.overlay.drag_note.is_none() {
            if let (Some(set), Some(preview)) = (self.drag_origin.take(), self.drag_preview.take()) {
                if set.is_empty() {
                    // 异常:无可移动音符。
                    self.drag_anchor = None;
                    self.cache_valid = false;
                    self.ui_dirty = true;
                    return;
                }
                let anchor_idx = self.drag_anchor.take().unwrap_or(set[0].0);
                let (Some((_, an0)), Some((_, ab, ax, _))) = (
                    set.iter().find(|(i, _)| *i == anchor_idx),
                    preview.iter().find(|(i, _, _, _)| *i == anchor_idx),
                ) else {
                    // 异常:清残留,不提交。
                    self.cache_valid = false;
                    self.ui_dirty = true;
                    return;
                };
                // 释放时锚点 beat 吸附到 snap 网格(PMCORE-17,与放置一致),
                // 整组按同一偏移平移。
                let b = ui::snap_beat(*ab, self.snap as f64, 0.0);
                let dbeat = b - an0.start_time.beats();
                let dx = *ax - an0.position_x;
                let mut changes: Vec<(usize, core::model::RPENote)> = Vec::with_capacity(set.len());
                if set.len() == 1 {
                    // 单选保持 PMCORE-17 语义:hold end = start + 1.0。
                    let (i, n) = &set[0];
                    let mut nn = n.clone();
                    nn.start_time = core::bpm::Triple::from_beats(b);
                    nn.end_time = core::bpm::Triple::from_beats(b + if nn.kind == 2 { 1.0 } else { 0.0 });
                    nn.position_x = (*ax).clamp(-675.0, 675.0);
                    changes.push((*i, nn));
                } else {
                    // 多选:整体平移,保持各自时长(组移动不重置 hold 时长)。
                    for (i, n) in &set {
                        let mut nn = n.clone();
                        let nb = (n.start_time.beats() + dbeat).max(0.0);
                        nn.start_time = core::bpm::Triple::from_beats(nb);
                        nn.end_time = core::bpm::Triple::from_beats((n.end_time.beats() + dbeat).max(nb));
                        nn.position_x = (n.position_x + dx).clamp(-675.0, 675.0);
                        changes.push((*i, nn));
                    }
                }
                // 没实际移动(点击未拖动):不入 undo 栈。
                let moved = changes.iter().zip(&set).any(|((_, nn), (_, orig))| {
                    nn.start_time.beats() != orig.start_time.beats()
                        || nn.end_time.beats() != orig.end_time.beats()
                        || nn.position_x != orig.position_x
                });
                if moved {
                    let _ = self.doc.replace_notes_multi(self.selected_line, &changes);
                }
                self.rebuild_chart();
                self.ui_dirty = true;
            } else {
                // 拖拽结束但没移动(或异常):清残留。
                self.drag_origin = None;
                self.drag_anchor = None;
                self.drag_preview = None;
            }
        }
        // Hold 尾部拖拽松手:一次 replace_note 提交 end_time(单 undo op)。
        if let Some((ni, end)) = self.overlay.hold_updated.take() {
            let orig = self.doc.chart().judge_line_list.get(self.selected_line)
                .and_then(|l| l.notes.as_ref())
                .and_then(|n| n.get(ni))
                .cloned();
            if let Some(mut nn) = orig {
                nn.end_time = core::bpm::Triple::from_beats(end);
                let _ = self.doc.replace_note(self.selected_line, ni, nn);
                self.rebuild_chart();
                self.ui_dirty = true;
            }
        }
        // Loading screen: 后台加载期间只画加载屏,不渲染谱面。
        if self.loading_name.is_some() || self.loading_thread.is_some() {
            // 轮询后台线程(完成则应用)。
            let still_loading = self.poll_loading();
            if still_loading {
                let elapsed = self.loading_start.elapsed().as_secs_f32();
                // 假进度(加载中动画):缓慢逼近 0.95。
                let progress = (elapsed * 0.25).min(0.95);
                // 加载屏空档:预热全部内置特效 pipeline(编译卡顿在加载屏
                // 期间发生,进入谱面后特效首次出现不再卡帧)。
                self.renderer.warmup_effects();
                let name = self.loading_name.clone().unwrap_or_default();
                self.overlay.render_loading(self.renderer.queue(), &name, progress, self.gui_scale);
                let ui_bg = Some(self.overlay.bind_group());
                match self.renderer.surface_acquire() {
                    Ok(st) => {
                        let aspect = st.texture.width() as f32 / st.texture.height().max(1) as f32;
                        let view = st.texture.create_view(&wgpu::TextureViewDescriptor::default());
                        self.renderer.draw_to_view(&view, &core::chart::FrameState { time: 0., lines: vec![], fired: vec![] }, aspect, 1.0, ui_bg, None);
                        self.renderer.queue().present(st);
                    }
                    _ => {}
                }
                return;
            }
        }
        // Frame lock(PMCORE-6):窗口失焦且设置开启时跳过渲染(省 GPU/CPU),
        // 恢复焦点后 about_to_wait 的 request_redraw 立即重绘一帧。放在加载
        // 屏之后:后台加载轮询(poll_loading)不受 frame lock 影响。事件照常处理。
        if self.settings.frame_lock && !self.focused {
            return;
        }
        // Splash mode
        if self.splash_mode {
            let settings_view = if self.show_settings { Some(&self.settings) } else { None };
            let filtered = ui::filter_charts(&self.splash_charts, &self.splash_search, self.splash_sort);
            let data = ui::SplashData {
                charts: &self.splash_charts,
                filtered: &filtered,
                filter: &self.splash_search,
                hover: self.splash_hover,
                sel: self.splash_sel,
                sort: self.splash_sort,
                lib_path: &self.splash_lib_path,
                scroll: self.splash_scroll,
                recover_hint: self.splash_recover.as_deref(),
                error: self.splash_error.as_deref(),
                new_dlg: self.splash_new.as_ref(),
            };
            self.overlay.render_splash(self.renderer.queue(), &data, self.gui_scale, settings_view);
            let ui_bg = Some(self.overlay.bind_group());
            match self.renderer.surface_acquire() {
                Ok(st) => {
                    let aspect = st.texture.width() as f32 / st.texture.height().max(1) as f32;
                    let view = st.texture.create_view(&wgpu::TextureViewDescriptor::default());
                    self.renderer.draw_to_view(&view, &core::chart::FrameState { time: 0., lines: vec![], fired: vec![] }, aspect, 1.0, ui_bg, None);
                    self.renderer.queue().present(st);
                }
                _ => {}
            }
            return;
        }
        // Feed pending seek into scroll target for smooth animation
        if let Some(t) = self.pending_seek.take() {
            self.scroll_target = Some(t);
        }
        let audio_time = match &self.audio {
            Some(a) => a.time(),
            None => self.started.elapsed().as_secs_f64(),
        };
        let audio_time = if let Some(target) = self.scroll_target {
            if self.overlay.seek_dragging {
                self.scroll_target = None;
                target
            } else {
                let d = target - audio_time;
                if d.abs() < 0.008 { self.scroll_target = None; target }
                else {
                    let step = d.signum() * (d.abs() * 0.12 + 0.004).min(d.abs().max(0.004));
                    let t = (audio_time + step).clamp(0.0, self.chart.duration());
                    self.seek(t); t
                }
            }
        } else { audio_time };

        // 时间轴拖拽:开始拖时若在播放则自动暂停,松手恢复。否则"播放中
        // 拖动"会来回打架(拖到的位置被继续播放的音频顶回去),密集谱面
        // 尤甚;暂停后 predict 归零,拖拽渲染的就是精确位置。
        let dragging = self.show_overlay && self.overlay.seek_dragging;
        if dragging && !self.drag_was_playing && self.audio.as_ref().is_some_and(|a| !a.is_paused()) {
            self.drag_was_playing = true;
            if let Some(a) = &self.audio { a.set_paused(true); }
        }
        if !dragging && self.drag_was_playing {
            self.drag_was_playing = false;
            if let Some(a) = &self.audio { a.set_paused(false); }
        }

        // A-B 循环(PMCORE-22):播放中**自然前向**穿过 B → hard_seek 回 A。
        // 计数保护走 hard_seek 的 hits_before 重置(combo/hits 不二次累计),
        // 音频线程 Seek 命令自带 FireCursor.seek_reset(hitsound 不洪水)。
        // 手动定位(滚动动画 pending_seek/scroll_target、seek bar 拖拽、
        // 大步跳变)不触发——"循环中手动 seek 到 B 之后"可正常播放。
        // [combo 精度不变量] 必须在本帧 chart_time 计算之前执行:回跳后音频
        // 已 seek 到 A,若用回跳前的陈旧 audio_time 算 chart_time,state_at 会
        // 收到 B 附近的旧位置,(A, B] 窗口在后续重放时被二次触发 → combo 双计。
        let paused_now = self.audio.as_ref().is_some_and(|a| a.is_paused());
        let off = (self.chart.offset() + self.info.offset) as f64;
        let seek_anim = self.pending_seek.is_some() || self.scroll_target.is_some();
        let mut audio_time = audio_time;
        // 无音频时音频时钟由 Instant 驱动,seek 不移动它,循环回跳会每帧
        // 重复触发(播放头钉在 A)——因此循环仅在音频存在时生效。
        if self.loop_on && self.audio.is_some() && !seek_anim && !self.overlay.seek_dragging && !paused_now {
            if let (Some(a), Some(b)) = (self.loop_a, self.loop_b) {
                let d = audio_time - self.loop_prev_audio;
                if d >= 0.0 && d <= 0.5 {
                    let b_t = self.chart.beat_to_time(b) + off;
                    if audio_time >= b_t {
                        let a_t = self.chart.beat_to_time(a) + off;
                        self.hard_seek(a_t);
                        // hard_seek 的 a.seek() 是异步命令(audio/mod.rs:320-322),
                        // 本帧 a.time() 仍是回跳前的 B:chart_time 必须直接用跳转
                        // 目标 a_t,否则会收到陈旧位置并被钳制钉死在 B 一整圈循环
                        // (播放头/计数冻结,重放丢失)。audio 存在(循环条件已保证),
                        // 下一帧 a.time() 即追上 A。
                        audio_time = a_t;
                    }
                }
            }
        }
        self.loop_prev_audio = audio_time;

        // Frame-render prediction (notes hit the line when the frame is seen)
        // minus the audio device output latency (rodio's get_pos counts samples
        // handed to the device callback, which are heard ~latency ms later).
        // 暂停时两者都归零:预测/延迟补偿会让暂停中的 chart_time 偏离真实
        // 位置,窗口内的 note 会在暂停时被误触发(白加 combo/播放 hitFX)。
        let predict = if paused_now { 0.0 } else { self.frame_latency.min(0.05) };
        let latency = if paused_now { 0.0 } else { self.device_latency };
        let mut chart_time = (audio_time + predict - latency - off).max(0.0);
        // [combo 精度不变量] 暂停/恢复瞬间 predict/latency 归零会让 chart_time
        // 相对上一帧回跳(≤ |predict-latency| ≤ 0.05s):state_at 的 seek-back
        // 会在恢复后把回跳窗口 (回跳点, 原位置] 的 note 补触发 → combo 双计。
        // 真正的 seek 已通过 reposition + hits_before 重建同步了游标,且 seek/
        // hard_seek 把 chart_time_last 更新到目标;拖动/滚动动画期间位置移动
        // 合法(combo 逐帧重建),故仅在稳态(无动画)时把 chart_time 钳制为
        // 单调不减。chart.rs combo 不变量测试锁定了该口径。
        if !self.overlay.seek_dragging && self.scroll_target.is_none() {
            chart_time = chart_time.max(self.chart_time_last);
        }
        self.chart_time_last = chart_time;
        let duration = self.chart.duration();
        let chart_beat = self.chart.time_to_beat(chart_time);
        // 循环 toast 过期 → 全量重绘擦除(timeline pixmap 上的旧文本)。
        if self.loop_toast.as_ref().is_some_and(|(_, t)| t.elapsed().as_secs_f32() > 2.0) {
            self.loop_toast = None;
            self.ui_dirty = true;
        }
        // 循环高亮带的 chart 秒换算(seek bar 用;拍值本身留给时间轴)。
        let loop_a_time = self.loop_a.map(|a| self.chart.beat_to_time(a));
        let loop_b_time = self.loop_b.map(|b| self.chart.beat_to_time(b));
        let line_count = self.chart.line_count();
        let line_name = if self.selected_line < line_count { self.chart.line_name(self.selected_line).to_string() } else { "?".to_string() };
        // PHIMAKOR_PERF=1: per-frame stage timings (60-frame moving average),
        // to see where the CPU budget goes while playing.
        static PERF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let perf = *PERF.get_or_init(|| std::env::var("PHIMAKOR_PERF").is_ok());
        // 尖峰捕获阈值(ms):PHIMAKOR_PERF=1 + PHIMAKOR_SPIKE=30 时,
        // 整帧超过 30ms 立即打印分项(默认 25ms)。
        static SPIKE_MS: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
        let t_eval = std::time::Instant::now();
        // BPM 面板(tool 4):每帧重建表单(在 frame 借用之前,避开借用冲突)。
        if self.show_overlay && self.show_properties && self.overlay.selected_tool == 4 {
            self.bpm_refresh_form();
        }
        // 设置面板(tool 2):每帧重建表单。
        if self.show_overlay && self.show_properties && self.overlay.selected_tool == 2 {
            self.settings_refresh_form();
        }
        // Eff 面板(tool 3,PMCORE-59):每帧重建组件库表单(prev 保留焦点/缓冲)。
        if self.show_overlay && self.show_properties && self.overlay.selected_tool == 3 {
            self.eff_refresh_form();
        }
        // fx 触发点查询必须在 state_at 之前(chart 可变借用冲突)。
        let fx_triggers = self.chart.fx_in_window(chart_time - 0.5, chart_time);
        // 预计算每个触发点在 **t0 时刻** 的线位姿:hit-fx 不绑定当前帧线状态,
        // 线之后移动/旋转时已爆散的粒子留在触发瞬间的位置。
        // PMCORE-79:批量接口按 (line, t0) 聚合,和弦只算一次链 set_time。
        let fx_poses: Vec<([f32; 2], f32)> = self.chart.fx_poses(&fx_triggers);
        let frame = self.chart.state_at(chart_time);

        for fired in &frame.fired {
            if fired.hold_tail { if !fired.fake { self.combo += 1; self.hits += 1; } continue; }
            if fired.tick { continue; }
            if fired.fake { continue; }
            self.combo += 1; self.hits += 1;
        }

        let size = self.window.inner_size();
        let aspect = size.width as f32 / size.height.max(1) as f32;
        let dim = if Instant::now() < self.seek_dim_until { 0.7 } else { 1.0 };
        let score = (self.hits as f64 / self.note_count.max(1) as f64 * 1_000_000.).round() as u32;
        let visible_notes: usize = frame.lines.iter().map(|l| l.notes.len()).sum();

        let _s2 = trace_span!("prepare_gameinfo");
        // Extract event data for selected line (cached, rebuild on dirty)
        if !self.cache_valid {
            self.cached_events = Arc::new(extract_line_events(&self.doc, self.selected_line, self.selected_layer));
            self.cached_notes = Arc::new(if self.full_notes {
                let mut all = Vec::new();
                for li in 0..self.doc.chart().judge_line_list.len() {
                    all.extend(extract_line_notes(&self.doc, li));
                }
                all.sort_by(|a, b| a.start_beats.total_cmp(&b.start_beats));
                all
            } else { extract_line_notes(&self.doc, self.selected_line) });
            self.cache_valid = true;
        }
        // Note 拖拽预览:覆盖整组被拖 note 的显示位置(不改 doc,松手才提交)。
        if let Some(preview) = &self.drag_preview {
            let notes = Arc::make_mut(&mut self.cached_notes);
            for (ni, beat, nx, end) in preview {
                if let Some(e) = notes.iter_mut().find(|e| e.index == *ni) {
                    e.start_beats = (*beat).max(0.0);
                    e.end_beats = (*end).max(0.0);
                    e.x = (*nx).clamp(-675.0, 675.0);
                }
            }
        }
        // Hold 尾部拖拽预览:覆盖 end_beats(不改 doc,松手才提交)。
        if let Some((ni, end)) = self.overlay.hold_preview {
            let notes = Arc::make_mut(&mut self.cached_notes);
            if let Some(e) = notes.iter_mut().find(|e| e.index == ni) {
                e.end_beats = end;
            }
        }
        let line_events = &self.cached_events;
        let line_notes = &self.cached_notes;
        let max_layers = self.doc.chart().judge_line_list.get(self.selected_line).map_or(1, |l| l.event_layers.len().max(1));

        // Extract selected event data
        let (selected_event_idx, ev_kind, ev_start_beats, ev_end_beats, ev_start_val, ev_end_val, ev_easing) = {
            let sel = self.selected_event_idx.and_then(|i| line_events.get(i));
            let (k, sb, eb, sv, ev, ea) = if let Some(ev) = sel {
                (ev.kind.clone(), ev.start_beats, ev.end_beats, ev.start, ev.end, ev.easing)
            } else { (String::new(), 0.0, 0.0, 0.0, 0.0, 0) };
            (self.selected_event_idx, k, sb, eb, sv, ev, ea)
        };
        // FX 链(PMCORE-64):提前构建一次,post.active 与 effect_names 共用,
        // 避免 evaluate_effects 双份计算。
        let fx_chain = self.extra.as_ref().map(|extra| {
            let size = self.renderer.size();
            render::effects_chain::build_effect_chain(
                extra, chart_beat, chart_time, (size[0] as f32, size[1] as f32),
            )
        });
        let effect_names: Vec<String> = fx_chain.as_ref()
            .map(|chain| chain.iter().map(|e| {
                e.custom_name.clone().unwrap_or_else(|| {
                    render::shaders::EFFECTS.get(e.shader_idx)
                        .map(|d| d.name.to_string()).unwrap_or_default()
                })
            }).collect())
            .unwrap_or_default();
        let (chart_name, composer, charter, illustrator, level, difficulty) = self.info.display_fields();
        let info = ui::GameInfo {
            chart_time, audio_time, fps: self.fps,
            frame_latency_ms: self.frame_latency as f32 * 1000.0,
            playing: self.audio.as_ref().is_some_and(|a| !a.is_paused()),
            combo: self.combo,
            hits: self.hits, note_count: self.note_count, score,
            lines: frame.lines.len(), visible_notes,
            paused: self.audio.as_ref().is_some_and(|a| a.is_paused()),
            dim, show_overlay: self.show_overlay,
            show_properties: self.show_properties, show_events: self.show_events, show_notes: self.show_notes,
            selected_line: self.selected_line,
            line_name,
            line_count,
            selected_layer: self.selected_layer.min(max_layers.max(1) - 1),
            max_layers,
            events: line_events.clone(),
            notes: line_notes.clone(),
            selected_notes: Arc::new(self.overlay.selected_notes.clone()),
            selected_events: Arc::new(self.overlay.selected_events.clone()),
            gui_scale: self.gui_scale,
            snap: self.snap,
            vsync: self.renderer.vsync,
            vertical_split: self.vertical_split,
            selected_tool: self.overlay.selected_tool,
            chart_beat,
            events_progress: self.overlay.render_progress().0,
            notes_progress: self.overlay.render_progress().1,
            has_custom_tex: self.chart_dir.join("Texture2D").is_dir(),
            full_notes: self.full_notes,
            show_menu: self.overlay.menu.is_open(),
            chart_name: chart_name.to_string(),
            composer: composer.to_string(),
            level: level.to_string(),
            difficulty,
            offset: self.info.offset,
            duration,
            selected_event_idx,
            event_edit_target: self.event_edit_target,
            ev_kind, ev_start_beats, ev_end_beats, ev_start_val, ev_end_val, ev_easing,
            effect_names: effect_names,
            num_edit: self.num_edit.as_ref().map(|e| {
                // keyframe 行编辑渲染编码:100+sub(起点/终点),见 draw_kf_area。
                let NumTarget::Kf(_, sub) = e.target;
                (100 + sub, e.buf.clone())
            }),
            eff_kf_var: self.eff_kf_var,
            eff_kf_sel: self.eff_kf_sel,
            loop_on: self.loop_on,
            loop_a: self.loop_a,
            loop_b: self.loop_b,
            loop_a_time: loop_a_time,
            loop_b_time: loop_b_time,
            loop_toast: self.loop_toast.as_ref().map(|(msg, t)| (msg.clone(), (2.0 - t.elapsed().as_secs_f32()).max(0.0))),
            line_comment: self.doc.line_comment(self.selected_line).is_some(),
            comment_edit: self.comment_edit.as_ref().map(|e| {
                let title = match e.target {
                    CommentTarget::Note(_, _) => "音符注释",
                    CommentTarget::Line(_) => "判定线注释",
                };
                (title.to_string(), e.buf.clone())
            }),
            eff_kf_rows: {
                // Parse the expanded var's keyframes for display. Inlined
                // sort — `frame` borrows self, so no &self method calls.
                let mut rows = Vec::new();
                if let (Some(sel), Some(kv)) = (self.selected_effect, self.eff_kf_var) {
                    if let Some(extra) = &self.extra {
                        let mut sorted: Vec<(usize, f64)> = extra.effects.iter().enumerate()
                            .map(|(i, e)| (i, e.start.beats())).collect();
                        sorted.sort_by(|a, b| a.1.total_cmp(&b.1));
                        if let Some((orig, _)) = sorted.get(sel) {
                            if let Some(e) = extra.effects.get(*orig) {
                                let mut keys: Vec<&String> = e.vars.keys().collect();
                                keys.sort();
                                if let Some(key) = keys.get(kv) {
                                    if let Some(serde_json::Value::Array(kfs)) = e.vars.get(*key) {
                                        for kf in kfs {
                                            if let Some(obj) = kf.as_object() {
                                                rows.push(ui::KfRow {
                                                    start_beats: obj.get("startTime").and_then(core::bpm::triple_to_beats).unwrap_or(0.0),
                                                    end_beats: obj.get("endTime").and_then(core::bpm::triple_to_beats).unwrap_or(0.0),
                                                    v1: obj.get("start").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
                                                    v2: obj.get("end").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
                                                    easing: obj.get("easingType").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                rows
            },
            ..Default::default()
        };
        // 时间轴点击 seek 延后应用(PMCORE-22):click 处理段在 frame
        // (state_at 借用,存活到 draw_to_view)期间,先存局部变量,帧尾
        // frame 最后一次使用后再 seek——与 pending_event_create 同思路。
        let mut deferred_seek: Option<f64> = None;
        if self.show_overlay {
            // Chart 面板(tool 0):元数据键值网格(每帧重建,实时值)。
            if self.show_properties && self.overlay.selected_tool == 0 {
                let s = self.gui_scale;
                let pp = self.overlay.props_progress();
                let pan_w = ui::PANEL_W * s;
                let px = self.window.inner_size().width as f32 - pp * pan_w;
                let py = 56.0 * s;
                let mut grid_rows = vec![
                    ("name".into(), chart_name.to_string()),
                    ("composer".into(), composer.to_string()),
                    ("charter".into(), charter.to_string()),
                    ("illustrator".into(), illustrator.to_string()),
                    ("level".into(), level.to_string()),
                    ("difficulty".into(), format!("{difficulty:.1}")),
                    ("notes".into(), format!("{}", self.note_count)),
                    ("duration".into(), format!("{:.2}s", duration)),
                    ("fps".into(), format!("{:.0}", self.fps)),
                    ("combo".into(), format!("{}", self.combo)),
                    ("score".into(), format!("{:07}", score)),
                ];
                // PMCORE-21:谱面内容校验告警(存内存,面板可见;详情终端
                // PHIMAKOR_CHART_WARNINGS=1)。
                if !self.chart_warnings.is_empty() {
                    grid_rows.push(("warnings".into(), format!("⚠ {} 条", self.chart_warnings.len())));
                }
                let mut grid = ui::widgets::KeyValueGrid::new(px, py, pan_w, grid_rows);
                grid.row_h = 22.0 * s;
                grid.gap = 4.0 * s;
                grid.title = "Chart".to_string();
                // PMCORE-23:前 6 行(name/composer/charter/illustrator/level/
                // difficulty)可编辑,其余只读;每帧重建时保留编辑态(缓冲/
                // 光标/提交标记),避免打字时被重建丢掉。
                grid.edit_kind = vec![
                    Some(ui::widgets::GridFieldKind::Text),   // 0 name
                    Some(ui::widgets::GridFieldKind::Text),   // 1 composer
                    Some(ui::widgets::GridFieldKind::Text),   // 2 charter
                    Some(ui::widgets::GridFieldKind::Text),   // 3 illustrator
                    Some(ui::widgets::GridFieldKind::Text),   // 4 level
                    Some(ui::widgets::GridFieldKind::Number), // 5 difficulty
                    None, None, None, None, None,
                ];
                if !self.chart_warnings.is_empty() {
                    grid.edit_kind.push(None); // warnings 行只读
                }
                grid.num_min = 0.0;
                grid.num_max = 1000.0;
                if let Some(prev) = &self.overlay.chart_grid {
                    grid.editing = prev.editing;
                    grid.buf = prev.buf.clone();
                    grid.insert = prev.insert;
                    grid.num_buf = prev.num_buf.clone();
                    grid.committed = prev.committed;
                }
                self.overlay.chart_grid = Some(grid);
            }
            // Line 面板(tool 1):实时线数据滚动列表(每帧从 frame 构建)。
            if self.show_properties && self.overlay.selected_tool == 1 {
                let s = self.gui_scale;
                let pp = self.overlay.props_progress();
                let pan_w = ui::PANEL_W * s;
                let px = self.window.inner_size().width as f32 - pp * pan_w;
                // 面板上方配置信息高度:约 28+8*22*s,列表从下方开始。
                let py = (28.0 + 8.0 * 22.0) * s + 4.0 * s;
                let vh = self.window.inner_size().height as f32;
                let list_h = (vh - 48.0 * s - py).max(60.0);
                let visible = (list_h / (22.0 * s + 4.0 * s)) as usize;
                let labels: Vec<String> = frame.lines.iter().enumerate().map(|(i, l)| {
                    let name = self.doc.chart().judge_line_list.get(i)
                        .map(|jl| jl.name.as_str()).unwrap_or("");
                    format!("L{i} {name} x:{:.1} y:{:.1} r:{:.0}° a:{:.2}",
                        l.position[0], l.position[1],
                        l.rotation.to_degrees() % 360.0, l.alpha)
                }).collect();
                let mut list = ui::widgets::ScrollList::new(px, py, pan_w, frame.lines.len(), visible.max(1));
                list.row_h = 22.0 * s;
                list.gap = 4.0 * s;
                list.labels = labels;
                list.selected = Some(self.selected_line);
                // 保留滚动位置;仅当选中的线变化时,才对齐使其可见
                // (手动滚轮/拖拽滚动不应被每帧重建拉回去)。
                let prev_scroll = self.overlay.line_list.as_ref().map(|l| l.scroll).unwrap_or(0.0);
                let prev_selected = self.overlay.line_list.as_ref().and_then(|l| l.selected);
                list.scroll = prev_scroll.clamp(0.0, list.max_scroll_pub());
                if prev_selected != list.selected {
                    if let Some(sel) = list.selected {
                        let top = sel as f32;
                        if top < list.scroll { list.scroll = top; }
                        let bottom = top + 1.0;
                        if bottom > list.scroll + visible as f32 {
                            list.scroll = (bottom - visible as f32).max(0.0);
                        }
                    }
                }
                self.overlay.line_list = Some(list);
            }
            if let Some(click) = self.overlay.take_timeline_click() {
                if let Some(ev_idx) = click.hit {
                    // 单击选中;点击已框选集合内的事件保留批量(与音符面板语义一致)。
                    if !self.overlay.selected_events.contains(&ev_idx) {
                        self.overlay.selected_events.clear();
                    }
                    self.selected_event_idx = Some(ev_idx);
                    self.last_event_click = None;
                } else if let Some(kind) = click.kind {
                    // 空白列点击:同列 300ms 内双击 → 创建事件;单击清空选中。
                    let dbl = self.last_event_click.as_ref().is_some_and(|(t, k)| *k == kind && t.elapsed() < Duration::from_millis(300));
                    if dbl {
                        if let Some(k) = event_kind_of(&kind) {
                            // 延迟到帧顶应用:render_frame 内已持有 chart 借用,
                            // 不能直接 &mut self 调用(PMCORE-20)。
                            self.pending_event_create = Some((k, click.beat));
                        }
                        self.last_event_click = None;
                    } else {
                        self.last_event_click = Some((Instant::now(), kind));
                        self.selected_event_idx = None;
                        self.overlay.selected_events.clear();
                        // 事件面板空白单击 → seek 到该拍(PMCORE-22,吸附 snap)。
                        deferred_seek = Some(click.beat);
                    }
                }
                self.ui_dirty = true;
            }
            if let Some(ly) = self.overlay.take_layer_click(self.overlay.props_progress(), max_layers) {
                self.selected_layer = ly; self.ui_dirty = true;
            }
            // 光标动画中(自定义光标开启时)也强制全量重绘:暂停时 ui_dirty
            // 不再触发,否则光标冻结在帧里。
            let need_iced = self.ui_dirty || self.overlay.cursor_dirty;
            if need_iced {
                self.overlay.render_iced(self.renderer.queue(), &info);
                self.ui_dirty = false;
            } else {
                self.overlay.redraw_timeline(self.renderer.queue(), &info);
            }
        }
        let t_panel = std::time::Instant::now();
        // 应用 FX 链(已在上方构建,PMCORE-64)。
        self.renderer.post.active.clear();
        // 自定义 GLSL 特效的 time/u_time 注入(编辑器路径不经
        // set_effects_from_extra,必须显式传当前谱面时间)。
        self.renderer.post.chart_time = chart_time as f32;
        if let Some(chain) = &fx_chain {
            self.renderer.post.active.extend(chain.iter().cloned());
        }

        let ui_bg = if self.show_overlay { Some(self.overlay.bind_group()) } else { None };
        let ui_iced = if self.show_overlay { Some(self.overlay.iced_bind_group()) } else { None };
        let t_post = std::time::Instant::now();

        // Phigros-style HUD: hidden while editor panels cover the screen.
        // 帧时间叠层常驻 HUD(视口隐藏时仍显示)。
        self.renderer.set_hud(render::HudData {
            chart_name: chart_name.to_string(),
            difficulty: level.to_string(),
            score,
            combo: self.combo,
            paused: self.audio.as_ref().is_some_and(|a| a.is_paused()),
            visible: !self.show_overlay,
            frame_ms: (self.frame_latency * 1000.0) as f32,
            fps: self.fps as f32,
        });
        self.renderer.set_progress(audio_time as f32 / duration as f32);
        // 命中特效:纯时间函数——查询当前谱面时间窗口内的触发点,
        // 前进/倒退/跳转都按 chart_time 渲染对应帧。位置用触发瞬间 t0
        // 的线位姿(已预计算),不绑定当前帧线状态。
        {
            // 与 note 渲染同坐标系(画布 px):
            //   x_canvas = note.x(±1) × 675(note 偏移不乘 scale/ctrl_size——
            //   渲染 note_m 只含平移+旋转,明确 NOT its scale)
            //   cx = pos[0]×675 + cos(rot)×x_canvas - sin(rot)×y_canvas
            //   cy = pos[1]×450×ev_y + sin(rot)×x_canvas + cos(rot)×y_canvas
            //   y_canvas = tr.y×450×ev_y(above 符号 × y_offset,note 正式落点)
            // ev_y 必须用判定区比例(render::ASPECT=3:2,letterbox 后恒定),
            // 不能用窗口比例——否则非 3:2 窗口 fx 的 y 比例与 note 错位。
            let ev_y = 1.5 / render::ASPECT as f64;
            let fx: Vec<(f64, [f32; 2])> = fx_triggers.into_iter().zip(fx_poses).map(|(tr, (pos, rot))| {
                let rot = rot as f64;
                let x = tr.x as f64 * 675.0;
                let y = tr.y as f64 * 450.0 * ev_y;
                let cx = pos[0] as f64 * 675.0 + rot.cos() * x - rot.sin() * y;
                let cy = pos[1] as f64 * 450.0 * ev_y + rot.sin() * x + rot.cos() * y;
                (tr.t0, [cx as f32, cy as f32])
            }).collect();
            self.renderer.set_frame_fx(fx);
        }
        match self.renderer.surface_acquire() {
            Ok(st) => {
                let view = st.texture.create_view(&wgpu::TextureViewDescriptor::default());
                self.renderer.draw_to_view(&view, frame, aspect, dim, ui_bg, ui_iced);
                self.renderer.queue().present(st);
                self.frame_latency = self.frame_latency * 0.9 + t_eval.elapsed().as_secs_f64() * 0.1;
            }
            _ => {}
        }
        // frame 借用已结束,应用时间轴点击 seek(PMCORE-22)。
        if let Some(b) = deferred_seek.take() {
            self.seek_to_beat(b);
        }
        let t_draw = std::time::Instant::now();
        if perf {
            // 60-frame moving totals, printed every 60 frames.
            static PERF_ACC: std::sync::Mutex<([f64; 4], u32)> = std::sync::Mutex::new(([0.0; 4], 0));
            let ms = [
                t_eval.elapsed().as_secs_f64() * 1000.0,
                t_panel.elapsed().as_secs_f64() * 1000.0,
                t_post.elapsed().as_secs_f64() * 1000.0,
                t_draw.elapsed().as_secs_f64() * 1000.0,
            ];
            let mut acc = PERF_ACC.lock().unwrap();
            for i in 0..4 { acc.0[i] += ms[i]; }
            acc.1 += 1;
            // 尖峰捕获:整帧超过阈值(默认 25ms,PHIMAKOR_SPIKE 覆盖)时
            // 立即打印该帧分项 + 上下文——平均会被 60 帧平滑抹掉,尖峰
            // 只在这里可见。
            let total = ms[0] + ms[1] + ms[2] + ms[3];
            let spike_ms = SPIKE_MS.get_or_init(|| {
                std::env::var("PHIMAKOR_SPIKE").ok()
                    .and_then(|v| v.parse::<f64>().ok()).unwrap_or(25.0)
            });
            if total > *spike_ms {
                eprintln!(
                    "SPIKE {:.1}ms: eval {:.2} | panel {:.2} | post {:.2} | draw {:.2} | fps {:.0} | playing={} overlay={} ui_dirty={} tl={}",
                    total, ms[0], ms[1], ms[2], ms[3], self.fps,
                    self.audio.as_ref().is_some_and(|a| !a.is_paused()),
                    self.show_overlay, self.ui_dirty,
                    self.show_events || self.show_notes,
                );
            }
            // GPU 场景 pass 耗时(PHIMAKOR_GPU_TIMING=1 时有效,
            // 定位 CPU vs GPU 瓶颈)。
            if let Some(gpu_ms) = self.renderer.gpu_frame_ms() {
                eprintln!("gpu: scene pass {:.2}ms", gpu_ms);
            }
            if acc.1 >= 60 {
                let avg = |i: usize| acc.0[i] / acc.1 as f64;
                eprintln!("perf: eval {:.2}ms | panel {:.2}ms | post {:.2}ms | draw {:.2}ms | fps {:.0}",
                    avg(0), avg(1), avg(2), avg(3), self.fps);
                acc.0 = [0.0; 4];
                acc.1 = 0;
            }
        }

        let dt = self.fps_since.elapsed().as_secs_f64();
        self.fps_since = Instant::now();
        self.fps = self.fps * 0.95 + (1.0 / dt.max(1e-6)) * 0.05;
    }
}

/// A-B 循环点位校验(PMCORE-22):A/B 必须都已设置且 A < B(A==B 也拒绝)。
/// 复制粘贴 / 播放头放置共用的"目标拍位"解析(纯函数,PMCORE-XX):
/// 播放头所在拍优先(空白单击/时间轴点击/空格播放都会 seek 到那里);
/// 播放头从未 seek(停在 0)时退回最近交互点——选中音符首拍(音符块点击
/// 不 seek,播放头停留在旧位);兜底 0。返回前吸附 snap 网格。
fn resolve_target_beat(playhead_beat: f64, snap: f64, sel_note_start: Option<f64>) -> f64 {
    if playhead_beat > 1e-9 {
        return ui::snap_beat(playhead_beat, snap, 0.0);
    }
    match sel_note_start {
        Some(b) => ui::snap_beat(b, snap, 0.0),
        None => 0.0,
    }
}

/// 粘贴整组变换(纯函数,PMCORE-XX):每颗音符相对 `min_start` 的偏移
/// 保留,整组平移到 `anchor`(调用方已吸附 snap);hold 的 end_time 同步
/// 平移(end_time 为 0 的非 hold 保持 0);其余字段原样克隆(全字段快照)。
fn paste_notes_transform(clipboard: &[core::model::RPENote], anchor: f64) -> Vec<core::model::RPENote> {
    let min_start = clipboard
        .iter()
        .map(|n| n.start_time.beats())
        .fold(f64::INFINITY, f64::min);
    let offset = anchor - min_start;
    clipboard
        .iter()
        .map(|n| {
            let mut nn = n.clone();
            nn.start_time = core::bpm::Triple::from_beats(n.start_time.beats() + offset);
            if n.end_time.beats() > 0.0 {
                nn.end_time = core::bpm::Triple::from_beats(n.end_time.beats() + offset);
            }
            nn
        })
        .collect()
}

/// 返回 Err(原因)时调用方拒绝开启循环并提示。
fn loop_points_valid(a: Option<f64>, b: Option<f64>) -> Result<(f64, f64), &'static str> {
    match (a, b) {
        (Some(a), Some(b)) if a < b => Ok((a, b)),
        (None, _) => Err("先设 A 点 (I)"),
        (_, None) => Err("先设 B 点 (O)"),
        _ => Err("A 必须小于 B"),
    }
}

/// 事件列名 → EventKind(仅 5 列基础事件,扩展/ctrl 事件不在范围,PMCORE-20)。
fn event_kind_of(s: &str) -> Option<EventKind> {
    match s {
        "Alpha" => Some(EventKind::Alpha),
        "MoveX" => Some(EventKind::MoveX),
        "MoveY" => Some(EventKind::MoveY),
        "Rotate" => Some(EventKind::Rotate),
        "Speed" => Some(EventKind::Speed),
        _ => None,
    }
}

fn extract_line_events(doc: &ChartDocument, line: usize, layer_idx: usize) -> Vec<ui::EventEntry> {
    let _s = trace_span!("extract_line_events");
    let rpe = doc.chart();
    let Some(jl) = rpe.judge_line_list.get(line) else { return vec![] };
    let mut out = Vec::new();
    if let Some(Some(layer)) = jl.event_layers.get(layer_idx) {
        let kinds: [(EventKind, &Option<Vec<core::model::RPEEvent>>); 5] = [
            (EventKind::Alpha, &layer.alpha_events),
            (EventKind::MoveX, &layer.move_x_events),
            (EventKind::MoveY, &layer.move_y_events),
            (EventKind::Rotate, &layer.rotate_events),
            (EventKind::Speed, &layer.speed_events),
        ];
        for (kind, events) in kinds {
            let Some(events) = events else { continue };
            for (ei, ev) in events.iter().enumerate() {
                out.push(ui::EventEntry {
                    layer: layer_idx, kind: format!("{kind:?}"),
                    index: ei, start_beats: ev.start_time.beats(),
                    end_beats: ev.end_time.beats(),
                    start: ev.start, end: ev.end,
                    easing: ev.easing_type,
                });
            }
        }
    }
    // Sorted by end_beats: the timeline draws binary-search the visible window
    // (draw_5col_timeline partition_point). Index consistency is preserved —
    // clicks and drawing share the same sorted vec.
    out.sort_by(|a, b| a.end_beats.total_cmp(&b.end_beats));
    out
}

fn extract_line_notes(doc: &ChartDocument, line: usize) -> Vec<ui::NoteEntry> {
    let _s = trace_span!("extract_line_notes");
    let rpe = doc.chart();
    let Some(jl) = rpe.judge_line_list.get(line) else { return vec![] };
    let Some(notes) = &jl.notes else { return vec![] };
    let mut out: Vec<ui::NoteEntry> = notes
        .iter()
        .enumerate()
        .map(|(i, n)| ui::NoteEntry::from_rpe_note(n, i))
        .collect();
    // Sorted by end_beats for the timeline's binary-search visible window.
    out.sort_by(|a, b| a.end_beats.total_cmp(&b.end_beats));
    out
}

/// KeyCode + logical key → 组件库 WidgetKey(属性面板键盘路由共用)。
/// `tab: true` 接受 Tab 键,字符仅限数字/./-(数字输入框);
/// `tab: false` 接受任意字符与空格(文本输入框)。
fn code_to_widget_key(
    code: KeyCode,
    logical: &winit::keyboard::Key,
    tab: bool,
) -> Option<ui::widgets::WidgetKey> {
    match code {
        KeyCode::Backspace => Some(ui::widgets::WidgetKey::Backspace),
        KeyCode::Enter | KeyCode::NumpadEnter => Some(ui::widgets::WidgetKey::Enter),
        KeyCode::Escape => Some(ui::widgets::WidgetKey::Escape),
        KeyCode::ArrowLeft => Some(ui::widgets::WidgetKey::Left),
        KeyCode::ArrowRight => Some(ui::widgets::WidgetKey::Right),
        KeyCode::ArrowUp => Some(ui::widgets::WidgetKey::Up),
        KeyCode::ArrowDown => Some(ui::widgets::WidgetKey::Down),
        KeyCode::Home => Some(ui::widgets::WidgetKey::Home),
        KeyCode::End => Some(ui::widgets::WidgetKey::End),
        KeyCode::Tab if tab => Some(ui::widgets::WidgetKey::Tab),
        _ => match logical {
            winit::keyboard::Key::Character(s) => s
                .chars()
                .next()
                .filter(|c| !tab || c.is_ascii_digit() || *c == '.' || *c == '-')
                .map(ui::widgets::WidgetKey::Char),
            winit::keyboard::Key::Named(winit::keyboard::NamedKey::Space) if !tab => {
                Some(ui::widgets::WidgetKey::Char(' '))
            }
            _ => None,
        },
    }
}

/// 浮动文本/数字编辑(注释框与 Eff 数字框)的公共按键处理:Enter/NumpadEnter
/// → committed;Escape → cancelled;Backspace → 删尾;其余字符按 `allowed` 过滤
/// 入 buf。提交/取消需要调用方独占 state,所以用返回值回传,由调用方执行
/// commit 方法/置 None 并标记 ui_dirty。
///
/// 返回 `(committed, cancelled)`;二者互斥。
fn handle_text_edit(
    buf: &mut String,
    code: KeyCode,
    logical: &winit::keyboard::Key,
    allowed: impl Fn(char) -> bool,
) -> (bool, bool) {
    match code {
        KeyCode::Enter | KeyCode::NumpadEnter => (true, false),
        KeyCode::Escape => (false, true),
        KeyCode::Backspace => {
            buf.pop();
            (false, false)
        }
        _ => {
            let ch = match logical {
                winit::keyboard::Key::Character(s) => s.chars().next(),
                winit::keyboard::Key::Named(winit::keyboard::NamedKey::Space) => Some(' '),
                _ => None,
            };
            if let Some(c) = ch {
                if allowed(c) {
                    buf.push(c);
                }
            }
            (false, false)
        }
    }
}
impl App {
    /// Splash 模式键盘:图表列表导航/搜索/新建对话框/设置(Esc 关)/Ctrl+Q 退出。
    /// 仅 splash_mode 时由 window_event 路由调用;与编辑器键盘完全隔离。
    fn handle_splash_keyboard(
        &mut self,
        event_loop: &ActiveEventLoop,
        code: KeyCode,
        event: &winit::event::KeyEvent,
    ) {
        let state = self.state.as_mut().unwrap();
                            if state.show_settings {
                                if code == KeyCode::Escape { state.show_settings = false; }
                                return;
                            }
                            // PMCORE-8:IME 组合中按键交给输入法(候选/回退/确认),
                            // 应用不处理键盘输入——避免组合中 Backspace 弹字、
                            // Enter 误开谱面、字符重复进搜索。
                            if state.ime_active { return; }
                            // PMCORE-36:新建谱面命名对话框(Enter 创建 / Esc 取消 /
                            // 字符输入)。创建成功 → open_chart 进编辑器。
                            if state.splash_new.is_some() {
                                let mut open_path: Option<PathBuf> = None;
                                match code {
                                    KeyCode::Enter => {
                                        let name = state.splash_new.as_ref().map(|d| d.name.clone()).unwrap_or_default();
                                        match create_new_chart(&name) {
                                            Ok(dir) => {
                                                state.splash_new = None;
                                                state.splash_charts = scan_charts();
                                                state.splash_sel = None;
                                                open_path = Some(dir);
                                            }
                                            Err(e) => {
                                                if let Some(d) = &mut state.splash_new { d.err = Some(e); }
                                            }
                                        }
                                    }
                                    KeyCode::Escape => {
                                        state.splash_new = None;
                                        state.splash_hover = ui::SplashHover::None;
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(d) = &mut state.splash_new { d.name.pop(); }
                                    }
                                    _ => {
                                        if !state.ctrl {
                                            let ch = match &event.logical_key {
                                                winit::keyboard::Key::Character(s) => s.chars().next(),
                                                winit::keyboard::Key::Named(winit::keyboard::NamedKey::Space) => Some(' '),
                                                _ => None,
                                            };
                                            if let Some(c) = ch {
                                                if let Some(d) = &mut state.splash_new { d.name.push(c); }
                                            }
                                        }
                                    }
                                }
                                if let Some(path) = open_path {
                                    let _ = state;
                                    self.open_chart(event_loop, &path);
                                    return;
                                }
                                return;
                            }
                            let mut open_path: Option<PathBuf> = None;
                            if state.ctrl && code == KeyCode::KeyQ {
                                // PMCORE-24:退出前 flush(文档可能是上次编辑过的谱面)。
                                if let Err(e) = state.doc.flush() {
                                    eprintln!("flush on quit failed: {e:#}");
                                }
                                event_loop.exit();
                                return;
                            }
                            let filtered = ui::filter_charts(&state.splash_charts, &state.splash_search, state.splash_sort);
                            let n = filtered.len();
                            // Keep the selection (and thus the scroll) inside
                            // the visible list window after nav/filter edits.
                            let keep_visible = |st: &mut State, n: usize| {
                                let gs = st.overlay.gui_scale;
                                let row_step = 40.0 * gs;
                                let vh = st.window.inner_size().height as f32;
                                let view_h = (vh - 96.0 * gs - 96.0 * gs).max(1.0);
                                let max_scroll = (n as f32 * row_step - view_h).max(0.0);
                                if let Some(i) = st.splash_sel {
                                    let top = i as f32 * row_step;
                                    if top < st.splash_scroll { st.splash_scroll = top; }
                                    if top + row_step > st.splash_scroll + view_h {
                                        st.splash_scroll = (top + row_step - view_h).max(0.0);
                                    }
                                }
                                st.splash_scroll = st.splash_scroll.clamp(0.0, max_scroll);
                            };
                            match code {
                                KeyCode::Enter => {
                                    open_path = state.splash_sel.and_then(|i| filtered.get(i).copied())
                                        .and_then(|ci| state.splash_charts.get(ci))
                                        .map(|c| c.path.clone());
                                }
                                KeyCode::Escape => {
                                    if state.splash_search.is_empty() { state.splash_sel = None; }
                                    else { state.splash_search.clear(); state.splash_sel = None; state.splash_scroll = 0.0; }
                                }
                                KeyCode::Backspace => {
                                    if !state.splash_search.is_empty() { state.splash_search.pop(); state.splash_sel = None; state.splash_scroll = 0.0; }
                                }
                                KeyCode::ArrowUp => {
                                    state.splash_sel = Some(state.splash_sel.map_or(0, |i| i.saturating_sub(1)));
                                    keep_visible(state, n);
                                }
                                KeyCode::ArrowDown => {
                                    state.splash_sel = Some(n.saturating_sub(1).min(state.splash_sel.map_or(0, |i| i + 1)));
                                    keep_visible(state, n);
                                }
                                KeyCode::Delete => {
                                    if let Some(i) = state.splash_sel {
                                        if let Some(&ci) = filtered.get(i) {
                                            // PMCORE-26:多难度聚合条目删整曲根目录,单目录删自身。
                                            delete_chart_entry(&state.splash_charts[ci]);
                                            state.splash_charts = scan_charts();
                                            state.splash_sel = None;
                                            state.splash_scroll = 0.0;
                                        }
                                    }
                                }
                                _ => {
                                    if !state.ctrl {
                                        let ch = match &event.logical_key {
                                            winit::keyboard::Key::Character(s) => s.chars().next(),
                                            winit::keyboard::Key::Named(winit::keyboard::NamedKey::Space) => Some(' '),
                                            _ => None,
                                        };
                                        if let Some(c) = ch {
                                            state.splash_search.push(c);
                                            state.splash_sel = None;
                                            state.splash_scroll = 0.0;
                                        }
                                    }
                                }
                            }
                            if let Some(path) = open_path {
                                let _ = state;
                                self.open_chart(event_loop, &path);
                            }
                            return;
    }

    /// 编辑器键盘(拆分自 window_event,逐键行为一致):模态前缀
    /// (comment_edit / num_edit / 图表行编辑)优先消费,主 match 路由
    /// 编辑快捷键,末尾 BPM/Eff 表单键盘转发(有焦点行时)。
    fn handle_editor_keyboard(
        &mut self,
        event_loop: &ActiveEventLoop,
        code: KeyCode,
        ctrl: bool,
        alt: bool,
        event: &winit::event::KeyEvent,
    ) {
        let state = self.state.as_mut().unwrap();
        // 菜单键盘(P3):菜单打开时拦截导航键(Up/Down/Left/Right/Enter/Escape),
        // 在其它编辑器键盘路由之前;非导航键继续下行(快捷键不因菜单打开而失效,
        // 与旧行为一致)。菜单命令经 MenuAction → OverlayMessage 消息臂执行。
        if state.overlay.menu.is_open() {
            if event.state == ElementState::Pressed && !event.repeat {
                if let Some(k) = code_to_widget_key(code, &event.logical_key, true) {
                    if matches!(k, ui::widgets::WidgetKey::Up | ui::widgets::WidgetKey::Down
                        | ui::widgets::WidgetKey::Left | ui::widgets::WidgetKey::Right
                        | ui::widgets::WidgetKey::Enter | ui::widgets::WidgetKey::Escape)
                    {
                        let actions = state.overlay.menu_key(k);
                        for a in actions {
                            match a {
                                ui::menu::MenuAction::Close => state.overlay.menu.close(),
                                ui::menu::MenuAction::Command(c) => {
                                    if let Some(m) = ui::menu_command_message(c) {
                                        state.overlay.messages.push(m);
                                    }
                                    state.overlay.menu.close();
                                }
                                ui::menu::MenuAction::Submenu(_) => {}
                            }
                        }
                        state.ui_dirty = true;
                        return;
                    }
                }
            }
        }
                        // Comment input (PMCORE-77):右键菜单"注释"→ 浮动输入框。
                        // 任意字符键(含空格)进 buf;Enter 提交 / Escape 取消 /
                        // Backspace 删尾。IME 未接线(与 num_edit 同限),仅直接键入。
                        if state.comment_edit.is_some() && !state.ctrl {
                            if event.state == ElementState::Pressed {
                                let (committed, cancelled) = match &mut state.comment_edit {
                                    Some(edit) => handle_text_edit(&mut edit.buf, code, &event.logical_key, |c| !c.is_control()),
                                    None => (false, false),
                                };
                                if committed { state.commit_comment_edit(); }
                                if cancelled { state.comment_edit = None; }
                                state.ui_dirty = true;
                            }
                            return;
                        }
                        // Numeric input on the Eff panel (double-clicked field):
                        // consume digits / . / - / Enter / Escape / Backspace.
                        // Space commits the inline edit and falls through to
                        // the pause toggle below — it must never be swallowed
                        // while an edit is open (else "Space can't pause").
                        if state.num_edit.is_some() && !state.ctrl {
                            if code == KeyCode::Space && event.state == ElementState::Pressed {
                                state.commit_num_edit();
                            } else {
                                if event.state == ElementState::Pressed {
                                    let (committed, cancelled) = match &mut state.num_edit {
                                        Some(edit) => handle_text_edit(&mut edit.buf, code, &event.logical_key, |c| c.is_ascii_digit() || c == '.' || c == '-' || c == 'e'),
                                        None => (false, false),
                                    };
                                    if committed { state.commit_num_edit(); }
                                    if cancelled { state.num_edit = None; }
                                    state.ui_dirty = true;
                                }
                                return;
                            }
                        }
                        // PMCORE-23:Chart 面板(tool 0)元数据行编辑——点击行进入
                        // 输入后,按键转发给组件(on_key);Enter 提交 → 写 doc.info
                        // 并同步 self.info(HUD/标题/面板即时刷新)。IME 组合中不转发。
                        if state.show_properties
                            && state.overlay.selected_tool == 0
                            && state.overlay.chart_grid.as_ref().is_some_and(|g| g.editing.is_some())
                            && !state.ctrl
                            && !state.ime_active
                        {
                            let k = code_to_widget_key(code, &event.logical_key, false);
                            if let Some(k) = k {
                                if let Some(grid) = state.overlay.chart_grid.as_mut() {
                                    grid.on_key(k);
                                    let committed = grid.committed.take();
                                    let value = committed
                                        .and_then(|row| grid.rows.get(row).map(|r| r.1.clone()));
                                    if let (Some(row), Some(value)) = (committed, value) {
                                        let field = match row {
                                            0 => Some(InfoField::Name),
                                            1 => Some(InfoField::Composer),
                                            2 => Some(InfoField::Charter),
                                            3 => Some(InfoField::Illustrator),
                                            4 => Some(InfoField::Level),
                                            5 => Some(InfoField::Difficulty),
                                            _ => None,
                                        };
                                        if let Some(f) = field {
                                            match state.doc.set_info_field(f, value) {
                                                Ok(()) => state.info = state.doc.info().clone(),
                                                Err(e) => eprintln!("set info field: {e}"),
                                            }
                                        }
                                    }
                                }
                                state.ui_dirty = true;
                                return;
                            }
                        }
                        // pre-copy fields to avoid borrow conflicts
                        let has_event = state.selected_event_idx.is_some();
                        let edit_target = state.event_edit_target;
                        let snap = state.snap as f64;
                        match code {
                        KeyCode::Escape => {
                            state.selected_event_idx = None;
                            state.overlay.selected_note = None;
                            state.overlay.selected_notes.clear();
                            state.overlay.selected_events.clear();
                            state.last_event_click = None;
                            state.ui_dirty = true;
                        }
                        KeyCode::Insert => {
                            // 事件时间轴:Insert 在鼠标所在列创建事件(吸附 snap);
                            // 鼠标不在事件面板时回退到选中事件的 kind + 播放头 beat。
                            if state.show_events {
                                let at = if let Some((kind, beat)) = state.overlay.event_geo_at_mouse() {
                                    event_kind_of(&kind).map(|k| (k, beat))
                                } else {
                                    state.selected_event_kind().map(|k| (k, state.chart.time_to_beat(state.chart_time_last)))
                                };
                                if let Some((k, beat)) = at {
                                    state.add_event_at(k, beat);
                                }
                            }
                        }
                        KeyCode::Space => {
                            // 轨道真正播完(音频线程检测到队列排空,置
                            // ended)后,空格 = 从头重播并清统计;其余情况
                            // 一律普通暂停/继续——末尾附近想暂停不会再被
                            // 误判成"重播"而硬跳回开头。
                            let paused = state.audio.as_ref().is_some_and(|a| a.is_paused());
                            if state.audio.as_ref().is_some_and(|a| a.ended()) {
                                state.hard_seek(0.0);
                            }
                            if let Some(a) = &state.audio {
                                a.set_paused(!paused);
                            }
                        }
                        // Event delete (PMCORE-20):事件时间轴可见且选中事件时
                        // Delete/Backspace 删除事件——优先于 Eff/音符面板分支。
                        // BPM 面板打字中不吞键(键由 bpm_form on_key 处理)。
                        KeyCode::Delete | KeyCode::Backspace
                            if state.show_events
                                && (state.selected_event_idx.is_some() || !state.overlay.selected_events.is_empty())
                                && !(state.overlay.selected_tool == 4
                                    && state.overlay.bpm_form.as_ref().is_some_and(|f| f.focus_row.is_some()))
                                && !(state.overlay.selected_tool == 3
                                    && state.overlay.eff_form.as_ref().is_some_and(|f| f.focus_row.is_some())) =>
                        {
                            state.delete_selected_events();
                        }
                        KeyCode::Delete if state.show_properties && state.overlay.selected_tool == 3 => {
                            state.eff_remove_selected(); // Eff 面板 Delete 行为保持
                        }
                        // Note delete (PMCORE-17):选中音符 Delete/Backspace 删除;
                        // 未选中但鼠标在音符面板上方 → 最近命中兜底。BPM 面板
                        // 打字中不吞键(键由 bpm_form on_key 处理)。
                        KeyCode::Delete | KeyCode::Backspace
                            if state.overlay.selected_note.is_some()
                            && !(state.overlay.selected_tool == 4
                                && state.overlay.bpm_form.as_ref().is_some_and(|f| f.focus_row.is_some()))
                            && !(state.overlay.selected_tool == 3
                                && state.overlay.eff_form.as_ref().is_some_and(|f| f.focus_row.is_some())) =>
                        {
                            if let Some(ni) = state.overlay.selected_note.take() {
                                state.delete_note(ni);
                            }
                        }
                        KeyCode::Delete | KeyCode::Backspace
                            if !(state.overlay.selected_tool == 4
                                && state.overlay.bpm_form.as_ref().is_some_and(|f| f.focus_row.is_some()))
                            && !(state.overlay.selected_tool == 3
                                && state.overlay.eff_form.as_ref().is_some_and(|f| f.focus_row.is_some())) =>
                        {
                            if let Some(ni) = state.overlay.hit_note_at_mouse() {
                                state.delete_note(ni);
                            }
                        }
                        // Event editing (F2 = cycle target, Ctrl+arrows = edit).
                        // 守卫分支必须放在无守卫的 ArrowLeft/Right(seek)之前,
                        // 否则永远 unreachable(ctrl 编辑实际没生效)。
                        // 框选多选(≥2)时 Ctrl+方向键整组平移(单 undo op,PMCORE-20)。
                        KeyCode::ArrowLeft if ctrl && (has_event || !state.overlay.selected_events.is_empty()) => {
                            if state.overlay.selected_events.len() > 1 {
                                state.nudge_selected_events(-snap);
                            } else {
                                state.edit_selected_event(|ev| match edit_target {
                                    // 编辑后吸附 snap 网格(PMCORE-20)。
                                    0 => ev.start_time = core::bpm::Triple::from_beats(ui::snap_beat(ev.start_time.beats() - snap, snap, 0.0)),
                                    1 => ev.end_time = core::bpm::Triple::from_beats(ui::snap_beat(ev.end_time.beats() - snap, snap, 0.0)),
                                    _ => {}
                                });
                            }
                        }
                        KeyCode::ArrowRight if ctrl && (has_event || !state.overlay.selected_events.is_empty()) => {
                            if state.overlay.selected_events.len() > 1 {
                                state.nudge_selected_events(snap);
                            } else {
                                state.edit_selected_event(|ev| match edit_target {
                                    0 => ev.start_time = core::bpm::Triple::from_beats(ui::snap_beat(ev.start_time.beats() + snap, snap, 0.0)),
                                    1 => ev.end_time = core::bpm::Triple::from_beats(ui::snap_beat(ev.end_time.beats() + snap, snap, 0.0)),
                                    _ => {}
                                });
                            }
                        }
                        // Nudge 选中音符(PMCORE-18):Alt+方向键,步进 snap(beat)
                        // 与 5 单位(x),整组单次 replace_notes_multi(单 undo op)。
                        // 必须放在无守卫的 Arrow 分支之前。
                        KeyCode::ArrowLeft if alt => { state.nudge_selected(-snap, 0.0); }
                        KeyCode::ArrowRight if alt => { state.nudge_selected(snap, 0.0); }
                        KeyCode::ArrowUp if alt => { state.nudge_selected(0.0, 5.0); }
                        KeyCode::ArrowDown if alt => { state.nudge_selected(0.0, -5.0); }
                        KeyCode::ArrowLeft | KeyCode::ArrowRight => {
                            let d = if code == KeyCode::ArrowLeft { -5.0 } else { 5.0 };
                            let t = state.audio.as_ref().map(|a| a.time()).unwrap_or(0.0) + d;
                            state.seek(t);
                        }
                        KeyCode::Tab => {
                            const A: [f32; 4] = [3.0 / 2.0, 16.0 / 9.0, 4.0 / 3.0, 1.0];
                            state.aspect_idx = (state.aspect_idx + 1) % A.len();
                            state.renderer.set_playfield_aspect(A[state.aspect_idx]);
                        }
                        KeyCode::F1 => { state.show_overlay = !state.show_overlay; state.ui_dirty = true; }
                        KeyCode::F3 => {
                            state.show_properties = !state.show_properties;
                            // 面板关闭/切换:清拖拽与旧面板 hover 残留
                            // (hover 每帧由 flow 重算,这里防残留高亮)。
                            state.bpm_dragging = false;
                            state.settings_dragging = false;
                            state.line_dragging = false;
                            state.overlay.bpm_hover = None;
                            state.overlay.settings_hover = None;
                            state.overlay.eff_form_hover = None;
                            state.overlay.chart_grid_hover = None;
                            state.overlay.line_list_hover = None;
                            state.ui_dirty = true;
                        }
                        KeyCode::F4 => { state.show_events = !state.show_events; state.ui_dirty = true; }
                        KeyCode::F5 => { if state.ctrl { state.full_notes = !state.full_notes; state.cache_valid = false; } else { state.show_notes = !state.show_notes; } state.ui_dirty = true; }
                        KeyCode::F6 => { state.renderer.set_vsync(!state.renderer.vsync); state.ui_dirty = true; }
                        KeyCode::F7 => { state.debug_memory(true); }
                        KeyCode::BracketLeft => { state.gui_scale = (state.gui_scale - 0.1).max(0.5); state.ui_dirty = true; }
                        KeyCode::BracketRight => { state.gui_scale = (state.gui_scale + 0.1).min(2.0); state.ui_dirty = true; }
                        KeyCode::Digit1 => { state.snap = 1.0; state.ui_dirty = true; }
                        KeyCode::Digit2 => { state.snap = 0.5; state.ui_dirty = true; }
                        KeyCode::Digit3 => { state.snap = 0.25; state.ui_dirty = true; }
                        KeyCode::Digit4 => { state.snap = 0.125; state.ui_dirty = true; }
                        // A-B 循环(PMCORE-22):I=设/清 A,O=设/清 B,P=开关。
                        KeyCode::KeyI => state.set_loop_point(true),
                        KeyCode::KeyO => state.set_loop_point(false),
                        KeyCode::KeyP => state.toggle_loop(),
                        // Ctrl+Z = undo, Ctrl+Y = redo(事件扁平索引可能失效,清事件选择)
                        KeyCode::KeyZ if state.ctrl => { state.doc.undo(); state.overlay.selected_note = None; state.overlay.selected_notes.clear(); state.overlay.selected_events.clear(); state.last_event_click = None; state.rebuild_chart(); state.ui_dirty = true; }
                        KeyCode::KeyY if state.ctrl => { state.doc.redo(); state.overlay.selected_note = None; state.overlay.selected_notes.clear(); state.overlay.selected_events.clear(); state.last_event_click = None; state.rebuild_chart(); state.ui_dirty = true; }
                        // Ctrl+S = save now
                        KeyCode::KeyS if state.ctrl => {
                            if let Err(e) = state.doc.save() { eprintln!("save failed: {e:#}"); }
                            state.ui_dirty = true;
                        }
                        // Ctrl+Q = back to the splash screen (exit the app
                        // from there with Ctrl+Q again)
                        KeyCode::KeyQ if state.ctrl => {
                            let _ = state;
                            self.back_to_splash(event_loop);
                            return;
                        }
                        // Event editing (F2 = cycle target, Ctrl+arrows = edit)
                        KeyCode::F2 if has_event => { state.event_edit_target = (state.event_edit_target + 1) % 5; state.ui_dirty = true; }
                        KeyCode::ArrowUp if ctrl && has_event => {
                            state.edit_selected_event(|ev| match edit_target {
                                2 => { let v = ev.start + 0.01; ev.start = v.min(1.0).max(-1.0); },
                                3 => { let v = ev.end + 0.01; ev.end = v.min(1.0).max(-1.0); },
                                4 => ev.easing_type = (ev.easing_type + 1).min(5),
                                _ => {}
                            });
                        }
                        KeyCode::ArrowDown if ctrl && has_event => {
                            state.edit_selected_event(|ev| match edit_target {
                                2 => { let v = ev.start - 0.01; ev.start = v.min(1.0).max(-1.0); },
                                3 => { let v = ev.end - 0.01; ev.end = v.min(1.0).max(-1.0); },
                                4 => ev.easing_type = (ev.easing_type - 1).max(0),
                                _ => {}
                            });
                        }
                        // 复制/粘贴(PMCORE-XX):Ctrl+C 复制选中音符全字段快照到
                        // clipboard(跨线/跨谱保留);Ctrl+V 粘贴到当前线目标拍位
                        // (播放头 > 最近选中块 > 0),整组相对偏移保留、吸附 snap,
                        // 经 doc.add_notes_multi 单 undo op。
                        KeyCode::KeyC if state.ctrl => { state.copy_selected_notes(); }
                        KeyCode::KeyV if state.ctrl => { state.paste_clipboard(); }
                        // Enter 播放头放置(PMCORE-XX):在目标拍位放一个当前类型
                        // 音符(吸附 snap,单 undo op)。守卫:BPM/Eff 表单有焦点行
                        // 时不抢键(键由 on_key 处理,见下方转发块)。
                        KeyCode::Enter | KeyCode::NumpadEnter
                            if !(state.show_properties && state.overlay.selected_tool == 4
                                && state.overlay.bpm_form.as_ref().is_some_and(|f| f.focus_row.is_some()))
                            && !(state.show_properties && state.overlay.selected_tool == 3
                                && state.overlay.eff_form.as_ref().is_some_and(|f| f.focus_row.is_some()))
                            && !state.ctrl
                            && !state.ime_active =>
                        {
                            state.place_note_at_playhead();
                        }
                        // Note kind selection (mania 网格批次 1):Q/W/E/R 只选
                        // 放置类型,不再直接放置——放置走音符面板双击/拖拽
                        // (OverlayMessage::PlaceNote)。
                        KeyCode::KeyQ | KeyCode::KeyW | KeyCode::KeyE | KeyCode::KeyR => {
                            let kind: u8 = match code { KeyCode::KeyQ => 1, KeyCode::KeyW => 4, KeyCode::KeyE => 2, KeyCode::KeyR => 3, _ => 1 };
                            state.overlay.set_place_kind(kind);
                            state.ui_dirty = true;
                        }
                        // PMCORE-26:难度切换——N = 下一难度,B = 上一难度(单 chart
                        // 目录无效果)。BPM/Eff 表单有焦点行时不抢键(键由 on_key 处理)。
                        KeyCode::KeyN | KeyCode::KeyB
                            if !state.ctrl && !state.alt && !state.ime_active
                            && !(state.show_properties && state.overlay.selected_tool == 4
                                && state.overlay.bpm_form.as_ref().is_some_and(|f| f.focus_row.is_some()))
                            && !(state.show_properties && state.overlay.selected_tool == 3
                                && state.overlay.eff_form.as_ref().is_some_and(|f| f.focus_row.is_some())) =>
                        {
                            if state.difficulties.len() >= 2 {
                                let cur = state.chart_dir.clone();
                                let idx = state.difficulties.iter().position(|(_, d)| *d == cur);
                                let n = state.difficulties.len();
                                let target = match (idx, code) {
                                    (Some(i), KeyCode::KeyB) => state.difficulties.get((i + n - 1) % n).map(|(_, d)| d.clone()),
                                    (Some(i), KeyCode::KeyN) => state.difficulties.get((i + 1) % n).map(|(_, d)| d.clone()),
                                    _ => None,
                                };
                                if let Some(d) = target {
                                    // 切换前同步落盘当前难度(最简安全路径:直接切、不丢编辑)。
                                    if state.doc.is_dirty() {
                                        if let Err(e) = state.doc.save() {
                                            eprintln!("save before difficulty switch failed: {e:#}");
                                        }
                                    }
                                    if let Err(e) = state.reload_chart(&d) {
                                        eprintln!("switch difficulty: {e:#}");
                                    }
                                    state.overlay.menu.close();
                                    state.ui_dirty = true;
                                }
                            }
                        }
                         _ => {}
                         }
                        // BPM 面板(tool 4)键盘:数字/退格/Enter/Esc/Tab/方向键
                        // 转发到组件库 on_key(有焦点行时;Tab 在鼠标悬停面板时也
                        // 转发 —— 无焦点行时 RealtimeForm 从第 0 行开始循环)。
                        if state.show_properties && state.overlay.selected_tool == 4
                            && !state.ctrl
                            && (state.overlay.bpm_form.as_ref().is_some_and(|f| f.focus_row.is_some())
                                || (state.is_mouse_over_props(4) && matches!(code, KeyCode::Tab)))
                        {
                            let k = code_to_widget_key(code, &event.logical_key, true);
                            if let Some(k) = k {
                                if let Some(form) = state.overlay.bpm_form.as_mut() {
                                    form.on_key(k);
                                    state.bpm_focus = form.focus_row;
                                }
                                state.bpm_apply();
                                return;
                            }
                        }
                        // Eff 面板(tool 3,PMCORE-59)键盘:数字/退格/Enter/Esc/Tab
                        // 转发到组件库 on_key(有焦点行时;Tab 在鼠标悬停面板时也
                        // 转发 —— 无焦点行时 RealtimeForm 从第 0 行开始循环)。
                        if state.show_properties && state.overlay.selected_tool == 3
                            && !state.ctrl
                            && (state.overlay.eff_form.as_ref().is_some_and(|f| f.focus_row.is_some())
                                || (state.is_mouse_over_props(3) && matches!(code, KeyCode::Tab)))
                        {
                            let k = code_to_widget_key(code, &event.logical_key, true);
                            if let Some(k) = k {
                                if let Some(form) = state.overlay.eff_form.as_mut() {
                                    form.on_key(k);
                                }
                                state.eff_apply();
                                state.ui_dirty = true;
                                return;
                            }
                        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let _s = trace_span!("resumed");
        if self.state.is_some() { return; }
        if let Some(dir) = &self.dir.clone() {
            match self.create_state(event_loop, dir) {
                Ok(s) => self.state = Some(s),
                Err(e) => { eprintln!("{e:#}"); event_loop.exit(); }
            }
        } else {
            let charts = scan_charts();
            // Empty chart list: still show the splash (with a hint), so the
            // user can open a settings page / drop charts instead of the app
            // quitting with a console-only error.
            if let Some(s) = self.create_splash_state(event_loop, charts) {
                self.state = Some(s);
            } else {
                eprintln!("splash init failed, use CLI: phimakor <chart-dir>");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Splash-mode interactions (before the state guard). Gated on the
        // splash *mode flag*, not the CLI `dir` arg — `dir` stays `None`
        // after a chart was opened from the splash, so `dir.is_none()` would
        // leave the splash click layer active over the editor.
        let splash = self.state.as_ref().is_some_and(|s| s.splash_mode);
        if splash {
            // Drag & drop a chart folder, its info.json, or a chart .zip to
            // import + open.
            if let WindowEvent::DroppedFile(path) = &event {
                let path = path.clone();
                // 按内容(文件头魔数/格式检测)判断,不依赖后缀:
                // 1) zip 包: PK\x03\x04 魔数 → 解包导入;
                // 2) 目录: 含 info 文件 → 谱面目录;
                // 3) info.* 文件 → 其父目录;
                // 4) chart 文件(RPE/PEC/PGR/PSS,内容检测) → 父目录。
                let head = std::fs::File::open(&path).ok().and_then(|mut f| {
                    let mut b = [0u8; 4];
                    std::io::Read::read_exact(&mut f, &mut b).ok().map(|_| b)
                });
                let is_zip = head == Some(*b"PK\x03\x04");
                let open_path = if is_zip {
                    // Probe the archive for chart content, then extract.
                    match import_chart_zip(&path) {
                        Ok(dir) => {
                            if let Some(st) = &mut self.state { st.splash_charts = scan_charts(); }
                            dir
                        }
                        Err(e) => {
                            eprintln!("drop: not a valid chart zip: {e:#}");
                            return;
                        }
                    }
                } else if is_chart_dir(&path) {
                    // Folder: copy into the library (unless already inside),
                    // then open the library copy so the list picks it up.
                    let mut open_path = path.clone();
                    let name = dir_name(&path);
                    if !name.is_empty() {
                        let lib = charts_dir();
                        let dest = lib.join(&name);
                        let _ = std::fs::create_dir_all(&lib);
                        if dest != path && !dest.exists() {
                            if copy_dir_recursive(&path, &dest).is_ok() {
                                open_path = dest;
                            } else {
                                eprintln!("drop: failed to copy {path:?} into {dest:?}");
                            }
                        }
                        if let Some(st) = &mut self.state { st.splash_charts = scan_charts(); }
                    }
                    open_path
                } else if path.is_file() && (path.file_name().is_some_and(|n| n == "info.json" || n == "info.txt")) {
                    // info.json itself → its parent is the chart dir.
                    path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
                } else if path.is_file() {
                    // Chart file itself (any supported format, content-detected)
                    // → its parent is the chart dir.
                    match std::fs::read(&path) {
                        Ok(bytes) if core::chart_format::detect_format(&bytes) != "unknown" => {
                            path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
                        }
                        _ => {
                            eprintln!("drop: not a chart folder, zip, info or chart file: {path:?}");
                            return;
                        }
                    }
                } else {
                    eprintln!("drop: not a chart folder or zip: {path:?}");
                    return;
                };
                // Open the (imported) chart.
                self.open_chart(event_loop, &open_path);
                return;
            }
            if let WindowEvent::MouseInput { state: btn_state, button: winit::event::MouseButton::Left, .. } = &event {
                if *btn_state == ElementState::Released {
                    let st = self.state.as_mut().unwrap();
                    let (mx, my) = match st.overlay.mouse_pos { Some(p) => p, _ => return };
                    let gs = st.overlay.gui_scale;
                    let vw = st.window.inner_size().width as f32;
                    let vh = st.window.inner_size().height as f32;
                    let filtered_len = ui::filter_charts(&st.splash_charts, &st.splash_search, st.splash_sort).len();
                    let hover = ui::splash_hit_test(mx, my, vw, vh, gs, filtered_len, st.show_settings, st.splash_new.is_some(), st.splash_scroll);
                    match hover {
                        ui::SplashHover::Settings => { st.show_settings = true; }
                        ui::SplashHover::Back => { st.show_settings = false; }
                        ui::SplashHover::New => {
                            // PMCORE-36:打开新建对话框(模态)。IME 候选窗跟随输入框。
                            st.splash_new = Some(ui::NewChartDlg::default());
                            st.splash_hover = ui::SplashHover::None;
                            st.ime_active = false;
                            st.update_ime_area();
                        }
                        ui::SplashHover::NewInput => { st.update_ime_area(); }
                        ui::SplashHover::NewCreate => {
                            let name = st.splash_new.as_ref().map(|d| d.name.clone()).unwrap_or_default();
                            match create_new_chart(&name) {
                                Ok(dir) => {
                                    st.splash_new = None;
                                    st.splash_charts = scan_charts();
                                    st.splash_sel = None;
                                    let _ = st;
                                    self.open_chart(event_loop, &dir);
                                    return;
                                }
                                Err(e) => {
                                    if let Some(d) = &mut st.splash_new { d.err = Some(e); }
                                }
                            }
                        }
                        ui::SplashHover::NewCancel => {
                            st.splash_new = None;
                            st.splash_hover = ui::SplashHover::None;
                            st.update_ime_area();
                        }
                        ui::SplashHover::Vsync => { st.settings.vsync = !st.settings.vsync; st.renderer.set_vsync(st.settings.vsync); save_settings(&st.settings); }
                        ui::SplashHover::Backend => {
                            st.settings.backend = ui::backend_cycle(&st.settings.backend);
                            save_settings(&st.settings);
                        }
                        ui::SplashHover::Fullscreen => {
                            st.settings.fullscreen = !st.settings.fullscreen;
                            st.window.set_fullscreen(if st.settings.fullscreen { Some(winit::window::Fullscreen::Borderless(None)) } else { None });
                            // Borderless fullscreen can hide the OS cursor — force it back.
                            st.window.set_cursor_visible(true);
                            save_settings(&st.settings);
                        }
                        ui::SplashHover::ScaleMinus => { st.settings.gui_scale = (st.settings.gui_scale - 0.1).max(0.5); st.gui_scale = st.settings.gui_scale * st.dpi_scale; save_settings(&st.settings); }
                        ui::SplashHover::ScalePlus => { st.settings.gui_scale = (st.settings.gui_scale + 0.1).min(2.0); st.gui_scale = st.settings.gui_scale * st.dpi_scale; save_settings(&st.settings); }
                        ui::SplashHover::Library => { open_in_explorer(&charts_dir()); }
                        ui::SplashHover::Refresh => {
                            st.splash_charts = scan_charts();
                            st.splash_sel = None;
                            st.splash_scroll = 0.0;
                            // PMCORE-24:重新扫描后也重跑崩溃恢复检测。
                            st.splash_recover = find_recover_hint(&st.splash_charts);
                        }
                        ui::SplashHover::Sort => { st.splash_sort = (st.splash_sort + 1) % 2; }
                        ui::SplashHover::OpenFolder => { open_in_explorer(&charts_dir()); }
                        ui::SplashHover::Chart(fi) => {
                            let ci = ui::filter_charts(&st.splash_charts, &st.splash_search, st.splash_sort).get(fi).copied();
                            let Some(ci) = ci else { return };
                            let path = st.splash_charts[ci].path.clone();
                            let _ = st;
                            self.open_chart(event_loop, &path);
                            return;
                        }
                        ui::SplashHover::Delete(fi) => {
                            let ci = ui::filter_charts(&st.splash_charts, &st.splash_search, st.splash_sort).get(fi).copied();
                            let Some(ci) = ci else { return };
                            // PMCORE-26:多难度聚合条目删整曲根目录,单目录删自身。
                            delete_chart_entry(&st.splash_charts[ci]);
                            st.splash_charts = scan_charts();
                            st.splash_sel = None;
                            st.splash_scroll = 0.0;
                        }
                        _ => {}
                    }
                    return;
                }
            }
        }
        let Some(state) = self.state.as_mut() else { return };
        match event {
            // PMCORE-24:关窗前 flush saver 线程待写快照,未保存修改不丢。
            WindowEvent::CloseRequested => {
                if let Err(e) = state.doc.flush() {
                    eprintln!("flush on close failed: {e:#}");
                }
                event_loop.exit();
            }
            WindowEvent::Resized(s) => {
                state.renderer.resize(s.width, s.height);
                state.overlay.resize(state.renderer.device(), state.renderer.tex_bgl(), state.renderer.sampler(), s.width, s.height);
                // DPI 变化(拖到不同缩放显示器)时刷新预乘。
                state.dpi_scale = state.window.scale_factor() as f32;
                state.gui_scale = state.settings.gui_scale * state.dpi_scale;
                state.ui_dirty = true;
                // 窗口尺寸/DPI 变化后搜索框宽度变,IME 候选窗重新跟随(PMCORE-8)。
                state.update_ime_area();
                state.render_frame();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Splash mode: wheel scrolls the chart list.
                if state.splash_mode {
                    let dy = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y * 40.0 * state.overlay.gui_scale,
                        MouseScrollDelta::PixelDelta(p) => p.y as f32,
                    };
                    if dy != 0.0 {
                        let gs = state.overlay.gui_scale;
                        let n = ui::filter_charts(&state.splash_charts, &state.splash_search, state.splash_sort).len();
                        let row_step = 40.0 * gs;
                        let vh = state.window.inner_size().height as f32;
                        let view_h = (vh - 96.0 * gs - 96.0 * gs).max(1.0);
                        let max_scroll = (n as f32 * row_step - view_h).max(0.0);
                        state.splash_scroll = (state.splash_scroll + dy).clamp(0.0, max_scroll);
                    }
                    return;
                }
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 * 0.1, p.y as f32 * 0.1),
                };
                if dx == 0.0 && dy == 0.0 { return; }
                // Horizontal scroll = seek (smooth, touchpad-friendly)
                if dx != 0.0 {
                    let t = state.audio.as_ref().map(|a| a.time()).unwrap_or(0.0) + dx as f64 * 2.0;
                    state.scroll_target = Some(t.clamp(0.0, state.chart.duration()));
                }
                // Vertical scroll: Eff panel edit (tool 3), timeline
                // zoom/scroll over timeline panel, else seek.
                if dy != 0.0 {
                    if state.is_mouse_over_props(3) {
                        // Expanded keyframe list: wheel cycles the selected
                        // keyframe's easing; otherwise the focused Number 行
                        // 步进(组件库 on_wheel;Start/End 步进 snap,uniform 0.1)。
                        if state.eff_kf_var.is_some() {
                            state.eff_kf_wheel(dy);
                        } else {
                            if let Some(form) = state.overlay.eff_form.as_mut() {
                                form.on_wheel(dy);
                            }
                            state.eff_apply();
                        }
                    } else if state.is_mouse_over_props(4) {
                        // BPM 面板滚轮:焦点行 Number 步进(组件库 on_wheel)。
                        if let Some(form) = state.overlay.bpm_form.as_mut() {
                            let focus = form.focus_row;
                            form.on_wheel(dy);
                            state.bpm_focus = focus.or(form.focus_row);
                        }
                        state.bpm_apply();
                    } else if state.is_mouse_over_props(2) {
                        // 设置面板滚轮(拖拽中由 on_drag 处理;非拖拽用焦点行步进)。
                        if let Some(form) = state.overlay.settings_form.as_mut() {
                            form.on_wheel(dy);
                        }
                        state.settings_apply();
                    } else if state.is_mouse_over_props(1) {
                        // Line 面板滚轮:滚动线列表。
                        if let Some(list) = state.overlay.line_list.as_mut() {
                            list.on_wheel(dy);
                        }
                    } else if (state.show_events || state.show_notes) && state.overlay.is_over_timeline(state.overlay.props_progress()) {
                        if state.ctrl {
                            state.overlay.timeline_zoom_in(dy);
                        } else if state.overlay.mouse_pos.map_or(false, |(_, my)| my >= 28.0) {
                            state.overlay.timeline_scroll(dy);
                            // 滚轮滚动时间轴后吸附到拍数网格,窗口顶部对齐 snap 边界。
                            state.overlay.snap_timeline_scroll(state.snap);
                        }
                    } else {
                        let t = state.audio.as_ref().map(|a| a.time()).unwrap_or(0.0) + dy as f64 * -0.5;
                        state.scroll_target = Some(t.clamp(0.0, state.chart.duration()));
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let ctrl = state.ctrl;
                let alt = state.alt;
                match event.state {
                    ElementState::Pressed if !event.repeat => {
                        if code == KeyCode::ControlLeft || code == KeyCode::ControlRight { state.ctrl = true; }
                        if code == KeyCode::ShiftLeft || code == KeyCode::ShiftRight { state.shift = true; }
                        if code == KeyCode::AltLeft || code == KeyCode::AltRight { state.alt = true; }
                        let splash = state.splash_mode;
                        let _ = state; // 结束 self.state 借用;分发到独立按键处理器(内部重新借用)
                        if splash {
                            self.handle_splash_keyboard(event_loop, code, &event);
                        } else {
                            self.handle_editor_keyboard(event_loop, code, ctrl, alt, &event);
                        }
                    }
                    ElementState::Released => {
                        let state = self.state.as_mut().unwrap();
                        if code == KeyCode::ControlLeft || code == KeyCode::ControlRight {
                            state.ctrl = false;
                            state.overlay.finish_selection();
                            // 框选可能在 Ctrl 释放时定稿(鼠标仍按下):同步事件主选中。
                            if state.overlay.is_over_events(0.0) {
                                state.sync_event_sel();
                            }
                        }
                        if code == KeyCode::ShiftLeft || code == KeyCode::ShiftRight { state.shift = false; }
                        if code == KeyCode::AltLeft || code == KeyCode::AltRight { state.alt = false; }
                        match code {
                        // 释放时切线。!state.ctrl 守卫:Ctrl+C/Ctrl+Z 按下时
                        // Ctrl 仍按住,释放 C/Z 不得误触切线(PMCORE-XX)。
                        KeyCode::KeyZ if !state.ctrl => {
                            let n = state.chart.line_count();
                            if n > 0 { state.selected_line = (state.selected_line + n - 1) % n; state.cache_valid = false; state.ui_dirty = true; state.overlay.selected_note = None; state.overlay.selected_notes.clear(); state.selected_event_idx = None; state.overlay.selected_events.clear(); }
                        }
                        KeyCode::KeyC if !state.ctrl => {
                            let n = state.chart.line_count();
                            if n > 0 { state.selected_line = (state.selected_line + 1) % n; state.cache_valid = false; state.ui_dirty = true; state.overlay.selected_note = None; state.overlay.selected_notes.clear(); state.selected_event_idx = None; state.overlay.selected_events.clear(); }
                        }
                        _ => {}
                        }
                    },
                    _ => {}
                }
            }

            WindowEvent::Focused(f) => {
                state.focused = f;
                // 失焦:按着的释放事件可能被 OS 吞掉(Alt-Tab),流控停在
                // Pressed → 下一次按下无边沿(点击丢失)。重置边沿并取消拖拽。
                if !f {
                    state.overlay.on_focus_lost();
                    state.drag_origin = None;
                    state.drag_anchor = None;
                    state.drag_preview = None;
                }
                // 重新聚焦后 IME 候选窗重新定位到搜索框(winit 会重新允许 IME)。
                if f && state.splash_mode { state.update_ime_area(); }
            }
            WindowEvent::CursorLeft { .. } => {
                // 拖拽出窗:释放事件不会到达窗口,流控停在 Pressed → 下一次
                // 按下无边沿(点击丢失)、拖拽残留。与失焦同样处理(取消)。
                state.overlay.on_focus_lost();
                state.drag_origin = None;
                state.drag_anchor = None;
                state.drag_preview = None;
            }
            WindowEvent::Ime(ime) => {
                // PMCORE-8:splash 搜索框 IME 中文输入。组合期间(Preedit)
                // 只标记+跟随候选窗,Commit 一次性写入 splash_search。编辑器
                // 模式 set_ime_allowed(false) 收不到 IME 事件(数字输入后续再议)。
                match ime {
                    winit::event::Ime::Enabled => { if state.splash_mode { state.update_ime_area(); } }
                    winit::event::Ime::Preedit(..) => {
                        state.ime_active = true;
                        state.ui_dirty = true;
                        if state.splash_mode { state.update_ime_area(); }
                    }
                    winit::event::Ime::Commit(text) => {
                        state.ime_active = false;
                        if state.splash_mode && !state.show_settings {
                            // PMCORE-36:新建对话框打开时 IME 写入谱名,否则进搜索框。
                            if let Some(d) = &mut state.splash_new {
                                d.name.push_str(&text);
                            } else if !text.is_empty() {
                                state.splash_search.push_str(&text);
                                state.splash_sel = None;
                                state.splash_scroll = 0.0;
                            }
                        }
                        state.ui_dirty = true;
                    }
                    winit::event::Ime::Disabled => { state.ime_active = false; }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if state.splash_mode {
                    // Splash hover: chart rows / buttons / settings rows.
                    let gs = state.overlay.gui_scale;
                    let vw = state.window.inner_size().width as f32;
                    let vh = state.window.inner_size().height as f32;
                    let (mx, my) = (position.x as f32, position.y as f32);
                    let filtered_len = ui::filter_charts(&state.splash_charts, &state.splash_search, state.splash_sort).len();
                    state.splash_hover = ui::splash_hit_test(mx, my, vw, vh, gs, filtered_len, state.show_settings, state.splash_new.is_some(), state.splash_scroll);
                }
                state.overlay.handle_cursor(position.x, position.y);
                // 属性面板 hover(命中 = 绘制,单一真源 flow.hover;绘制高亮
                // 仍用组件 Area,经 overlay.panel_hover 从 flow id 映射回)。
                if state.show_properties && state.overlay.selected_tool == 0 {
                    state.overlay.chart_grid_hover = state.overlay.chart_grid.as_ref()
                        .and_then(|g| { let areas = g.areas(); state.overlay.panel_hover(&areas) });
                }
                // Line 面板:滚动条拖拽中转发 on_drag;hover 命中更新。
                if state.show_properties && state.overlay.selected_tool == 1 {
                    let (mx, my) = (position.x as f32, position.y as f32);
                    if state.line_dragging {
                        if let Some(list) = state.overlay.line_list.as_mut() {
                            list.on_drag((mx, my));
                        }
                    } else {
                        state.overlay.line_list_hover = state.overlay.line_list.as_ref()
                            .and_then(|l| l.hit_area((mx, my)));
                    }
                }
                // 设置面板:拖拽中转发 on_drag;hover 命中更新(flow 单一真源)。
                if state.show_properties && state.overlay.selected_tool == 2 {
                    let (mx, my) = (position.x as f32, position.y as f32);
                    if state.settings_dragging {
                        if let Some(form) = state.overlay.settings_form.as_mut() {
                            form.on_drag((mx, my));
                        }
                    } else {
                        state.overlay.settings_hover = state.overlay.settings_form.as_ref()
                            .and_then(|f| { let areas = f.areas(); state.overlay.panel_hover(&areas) });
                    }
                }
                // BPM 面板:拖拽中转发 on_drag;hover 命中更新(flow 单一真源)。
                if state.show_properties && state.overlay.selected_tool == 4 {
                    let (mx, my) = (position.x as f32, position.y as f32);
                    if state.bpm_dragging {
                        if let Some(form) = state.overlay.bpm_form.as_mut() {
                            form.on_drag((mx, my));
                            // 拖拽期间直接写回(值实时生效,但保留拖动行焦点)
                            let focus = form.focus_row;
                            state.bpm_focus = focus;
                        }
                    } else {
                        state.overlay.bpm_hover = state.overlay.bpm_form.as_ref()
                            .and_then(|f| { let areas = f.areas(); state.overlay.panel_hover(&areas) });
                    }
                }
                // Eff 面板(tool 3,PMCORE-59):hover 命中更新(flow 单一真源)。
                if state.show_properties && state.overlay.selected_tool == 3 {
                    state.overlay.eff_form_hover = state.overlay.eff_form.as_ref()
                        .and_then(|f| { let areas = f.areas(); state.overlay.panel_hover(&areas) });
                }
                if state.overlay.seek_dragging && state.show_overlay {
                    let s = state.overlay.gui_scale;
                    let pp = state.overlay.props_progress();
                    let qp_w = ui::QP_W * s;
                    let props_x = state.window.inner_size().width as f32 - pp * ui::PANEL_W * s;
                    let sb_x = qp_w + 2.0 * s;
                    let sb_w = (props_x - sb_x - 2.0 * s).max(20.0);
                    let ratio = ((position.x as f32 - sb_x) / sb_w).clamp(0.0, 1.0);
                    let t = ratio as f64 * state.chart.duration();
                    state.seek(t);
                }
            }
            WindowEvent::MouseInput { state: btn_state, button: winit::event::MouseButton::Left, .. } => {
                // Splash presses are handled by the splash block (releases);
                // skip overlay state here so nothing leaks into the editor.
                if state.splash_mode { return; }
                // 光标点击反馈(自定义光标收缩变色)。
                if btn_state == ElementState::Pressed {
                    state.overlay.cursor_click = 1.0;
                }
                // HUD pause button (window px hit-test against last frame).
                if btn_state == ElementState::Pressed {
                    if let Some(m) = state.overlay.mouse_pos {
                        if !state.show_overlay && state.renderer.hit_test_pause(m.0, m.1) {
                            if let Some(a) = &state.audio { a.set_paused(!a.is_paused()); }
                            return;
                        }
                    }
                }
                // Line panel (tool 1):按滚动条 → 拖拽滚动。
                if btn_state == ElementState::Pressed
                    && state.show_properties
                    && state.overlay.selected_tool == 1
                {
                    if let Some((mx, my)) = state.overlay.mouse_pos {
                        if let Some(list) = state.overlay.line_list.as_ref() {
                            if let Some(a) = list.hit_area((mx, my)) {
                                if a.kind == ui::widgets::AreaKind::ScrollBar {
                                    state.line_dragging = true;
                                }
                            }
                        }
                    }
                }
                // Settings panel (tool 2): press starts a drag on Slider rows.
                // on_click 只在释放时发一次(Toggle 不会切两次)。
                // 命中来源 = flow(单一真源,命中 = 绘制)。
                if btn_state == ElementState::Pressed
                    && state.show_properties
                    && state.overlay.selected_tool == 2
                {
                    if let Some((mx, my)) = state.overlay.mouse_pos {
                        let hit_kind = state.overlay.flow.hit_at((mx, my)).and_then(|id| {
                            ui::flow_area_kind(&state.overlay.flow.areas, id)
                        });
                        if matches!(hit_kind, Some(ui::flow::AreaKind::Widget(ui::widgets::AreaKind::SliderTrack))) {
                            state.settings_dragging = true;
                            // Slider 行点击即定位值(on_click 内处理,无副作用)。
                            if let Some(form) = state.overlay.settings_form.as_mut() {
                                form.on_click((mx, my));
                            }
                        }
                    }
                }
                // Eff panel (tool 3) click handling — releases only, so the
                // press (which the overlay may consume) and the release land
                // in the same spot.
                // BPM panel (tool 4): press starts a drag when hitting a
                // Number row (live value editing). on_click 只在释放时发一次,
                // 避免 Toggle 被按下/释放切两次。
                if btn_state == ElementState::Pressed
                    && state.show_properties
                    && state.overlay.selected_tool == 4
                {
                    if let Some((mx, my)) = state.overlay.mouse_pos {
                        // 命中来源 = flow(单一真源,命中 = 绘制)。
                        let hit_kind = state.overlay.flow.hit_at((mx, my)).and_then(|id| {
                            ui::flow_area_kind(&state.overlay.flow.areas, id)
                        });
                        if matches!(hit_kind, Some(ui::flow::AreaKind::Widget(ui::widgets::AreaKind::Field | ui::widgets::AreaKind::SliderTrack))) {
                            state.bpm_dragging = true;
                            // 初始化拖动锚点(last_x),使首次 on_drag 增量正确。
                            if let Some(form) = state.overlay.bpm_form.as_mut() {
                                form.on_click((mx, my));
                            }
                        }
                    }
                }
                // Eff panel (tool 3,PMCORE-59) click handling — RealtimeForm 组件驱动。
                // 命中来源 = flow(单一真源,命中 = 绘制):kf 区 → 行索引,表单 → id。
                if btn_state == ElementState::Released
                    && state.show_properties
                    && state.overlay.selected_tool == 3
                {
                    if let Some((mx, my)) = state.overlay.mouse_pos {
                        let hit = state.overlay.flow.hit_at((mx, my));
                        // 手写 keyframe 区(展开时注册为流控区域):点击选中行,双击编辑起点。
                        if let Some(ri) = hit.and_then(ui::eff_panel::kf_index_from_area) {
                            let (last_t, _last_sel, last_f) = state.last_eff_click;
                            let is_double = state.eff_kf_sel == Some(ri)
                                && last_f == 200 && last_t.elapsed() < std::time::Duration::from_millis(300);
                            state.eff_kf_sel = Some(ri);
                            if is_double {
                                state.start_kf_num_edit(ri, 0);
                            }
                            state.last_eff_click = (std::time::Instant::now(), state.selected_effect, 200);
                            state.ui_dirty = true;
                            return;
                        }
                        let n_effects = state.eff_sorted().len();
                        let hit_id = hit.map(|id| id.0);
                        match hit_id {
                            // Add 按钮(组件库行数+1):新增效果并选中。
                            Some(id) if state.overlay.eff_form.as_ref()
                                .is_some_and(|f| id as usize == f.rows.len() + 1) =>
                            {
                                state.eff_add();
                                state.num_edit = None;
                                state.ui_dirty = true;
                            }
                            // 效果列表行:点击选中。
                            Some(id) if id >= 1 && (id as usize) <= n_effects => {
                                state.selected_effect = Some((id - 1) as usize);
                                state.num_edit = None;
                                state.eff_kf_var = None;
                                state.eff_kf_sel = None;
                                state.overlay.eff_form = None; // 重建,清焦点/缓冲残留
                                state.ui_dirty = true;
                            }
                            // Combo 选项:选择即写 e.shader(组件 on_click 处理)。
                            Some(id) if id >= 1000 => {
                                if let Some(form) = state.overlay.eff_form.as_mut() {
                                    form.on_click((mx, my));
                                }
                                state.eff_apply();
                                state.ui_dirty = true;
                            }
                            // 字段行:keyframed var 行点击 = 展开/收起(不进入组件);
                            // 其余交给组件(on_click 设焦点/开 Combo/切 Toggle)。
                            Some(id) => {
                                let field_row = (id - 1) as usize;
                                if field_row >= n_effects + 4 && state.eff_var_is_keyframed(field_row - n_effects - 4) {
                                    let vi = field_row - n_effects - 4;
                                    if state.eff_kf_var == Some(vi) {
                                        state.eff_kf_var = None;
                                        state.eff_kf_sel = None;
                                    } else {
                                        state.eff_kf_var = Some(vi);
                                        state.eff_kf_sel = Some(0);
                                    }
                                    state.ui_dirty = true;
                                    return;
                                }
                                if let Some(form) = state.overlay.eff_form.as_mut() {
                                    form.on_click((mx, my));
                                }
                                state.eff_apply();
                                state.ui_dirty = true;
                            }
                            None => {}
                        }
                    }
                }
                // BPM panel (tool 4) click handling — component-library driven.
                if btn_state == ElementState::Released
                    && state.show_properties
                    && state.overlay.selected_tool == 4
                {
                    if let Some((mx, my)) = state.overlay.mouse_pos {
                        let mut applied = false;
                        if let Some(form) = state.overlay.bpm_form.as_mut() {
                            form.on_click((mx, my));
                            applied = true;
                        }
                        // Add 按钮行为:行数超出 → 新行(apply 里处理)。
                        if applied {
                            state.bpm_dragging = false;
                            state.bpm_focus = state.overlay.bpm_form.as_ref().and_then(|f| f.focus_row);
                            state.bpm_apply();
                        }
                    }
                }
                // Settings panel (tool 2) click handling.
                if btn_state == ElementState::Released
                    && state.show_properties
                    && state.overlay.selected_tool == 2
                {
                    if let Some((mx, my)) = state.overlay.mouse_pos {
                        if let Some(form) = state.overlay.settings_form.as_mut() {
                            form.on_click((mx, my));
                        }
                        state.settings_dragging = false;
                        state.settings_apply();
                    }
                }
                // Line panel (tool 1):点击线列表行 → 选中该线。
                if btn_state == ElementState::Released
                    && state.show_properties
                    && state.overlay.selected_tool == 1
                {
                    state.line_dragging = false;
                    if let Some((mx, my)) = state.overlay.mouse_pos {
                        if let Some(list) = state.overlay.line_list.as_mut() {
                            let before = list.selected;
                            list.on_click((mx, my));
                            if let Some(sel) = list.selected {
                                if sel != state.selected_line {
                                    state.selected_line = sel;
                                    state.cache_valid = false;
                                    state.ui_dirty = true;
                                    state.overlay.selected_note = None;
                                    state.overlay.selected_notes.clear();
                                    // 换线后事件扁平索引失效,清事件选择(PMCORE-20)。
                                    state.selected_event_idx = None;
                                    state.overlay.selected_events.clear();
                                    let _ = before;
                                }
                            }
                        }
                    }
                }
                // Chart 面板(tool 0):元数据行点击 → 进入/切换编辑(PMCORE-23)。
                // 组件库 on_click 处理行命中与缓冲初始化,释放时发一次。
                if btn_state == ElementState::Released
                    && state.show_properties
                    && state.overlay.selected_tool == 0
                {
                    if let Some((mx, my)) = state.overlay.mouse_pos {
                        if let Some(grid) = state.overlay.chart_grid.as_mut() {
                            grid.on_click((mx, my));
                            state.ui_dirty = true;
                        }
                    }
                }
                // 任何新按下都作废旧的拖拽快照(旧拖拽可能因释放落在底部
                // 按钮区而没被提交,PMCORE-18 防陈旧快照污染下一次拖拽)。
                if btn_state == ElementState::Pressed {
                    state.drag_origin = None;
                    state.drag_anchor = None;
                    state.drag_preview = None;
                }
                state.overlay.handle_click(btn_state == ElementState::Pressed, state.ctrl, state.shift);
                // 释放且鼠标在事件面板:框选定稿 → 同步事件主选中(PMCORE-20)。
                if btn_state == ElementState::Released && state.overlay.is_over_events(0.0) {
                    state.sync_event_sel();
                }
            }
            WindowEvent::MouseInput { state: btn_state, button: winit::event::MouseButton::Right, .. } => {
                if state.splash_mode { return; }
                if btn_state == ElementState::Pressed {
                    state.overlay.handle_right_click(state.overlay.props_progress());
                    // 事件面板右键:命中并选中事件(菜单"删除事件"作用于它)。
                    if state.overlay.ctx_on_events {
                        match state.overlay.hit_event_at_mouse() {
                            Some(ev_idx) => {
                                state.selected_event_idx = Some(ev_idx);
                                state.overlay.selected_events.clear();
                            }
                            None => {
                                state.selected_event_idx = None;
                                state.overlay.selected_events.clear();
                            }
                        }
                        state.ui_dirty = true;
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                state.render_frame();
                // Drain into an owned vec first: MenuQuit drops `state` to
                // rebuild the splash state, which can't happen while the
                // drain iterator borrows it.
                let messages: Vec<ui::OverlayMessage> = state.overlay.messages.drain(..).collect();
                for msg in messages {
                    match msg {
                        ui::OverlayMessage::ToggleEvents => { state.show_events = !state.show_events; state.ui_dirty = true; }
                        ui::OverlayMessage::ToggleNotes => {
                            state.show_notes = !state.show_notes;
                            state.ui_dirty = true;
                        }
                        ui::OverlayMessage::DeleteNote => {
                            // 音符面板右键菜单"删除音符"(PMCORE-17)。
                            if let Some(ni) = state.overlay.selected_note.take() {
                                state.delete_note(ni);
                            }
                        }
                        ui::OverlayMessage::DeleteEvent => {
                            // 事件时间轴右键菜单"删除事件"(PMCORE-20)。
                            state.delete_selected_events();
                        }
                        ui::OverlayMessage::MenuSave => {
                            if let Err(e) = state.doc.save() { eprintln!("save failed: {e:#}"); }
                            state.overlay.menu.close();
                            state.ui_dirty = true;
                        }
                        ui::OverlayMessage::MenuLoad => {
                            state.overlay.menu.close();
                            state.ui_dirty = true;
                            // 释放 self.state 借用后开对话框(与 MenuQuit 同模式)。
                            let _ = state;
                            self.start_chart_dialog();
                            break;
                        }
                        ui::OverlayMessage::MenuExport => {
                            state.overlay.menu.close();
                            state.ui_dirty = true;
                            let _ = state;
                            self.start_export_dialog();
                            break;
                        }
                        ui::OverlayMessage::MenuQuit => {
                            let _ = state;
                            self.back_to_splash(event_loop);
                            break;
                        }
                        ui::OverlayMessage::SeekToBeat(beat) => state.seek_to_beat(beat),
                        ui::OverlayMessage::PlaceNote { start, end, x, kind } => {
                            // mania 批次 1:双击/拖拽放置(经 doc.add_note 单 undo op)。
                            let note = core::model::RPENote {
                                kind, above: 1,
                                start_time: core::bpm::Triple::from_beats(start),
                                // 非 hold 类型 end_time == start;拖拽退化 tap 时
                                // overlay 已把 end 压成 start(kind 1)。
                                end_time: core::bpm::Triple::from_beats(if kind == 2 { end } else { start }),
                                position_x: x.clamp(-675.0, 675.0), y_offset: 0., alpha: 255, hitsound: None,
                                size: 1.0, speed: 1.0, is_fake: 0, visible_time: 999999.,
                                tint: None, tint_hit_effects: None, judge_area: None, comment: None,
                            };
                            if let Err(e) = state.doc.add_note(state.selected_line, note) {
                                eprintln!("add note: {e}");
                            } else {
                                state.rebuild_chart();
                                state.ui_dirty = true;
                            }
                        }
                        ui::OverlayMessage::SetLoopA => state.set_loop_point(true),
                        ui::OverlayMessage::SetLoopB => state.set_loop_point(false),
                        ui::OverlayMessage::EditComment => {
                            // PMCORE-77:右键菜单"注释"。事件面板或未命中音符 →
                            // 判定线注释;音符面板命中音符 → 该音符注释。
                            let target = if state.overlay.ctx_on_events {
                                CommentTarget::Line(state.selected_line)
                            } else if state.overlay.ctx_note_hit {
                                match state.overlay.selected_note {
                                    Some(ni) => CommentTarget::Note(state.selected_line, ni),
                                    None => CommentTarget::Line(state.selected_line),
                                }
                            } else {
                                CommentTarget::Line(state.selected_line)
                            };
                            state.start_comment_edit(target);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(feature = "profiling")]
        tracy_client::frame_mark();
        // PHIMAKOR_MEMLOG=1 → print the memory report every 5 s (watch for
        // leaks while playing / scrubbing). The env var is read once (a syscall
        // per frame was measurable); the report skips the wgpu registry walk
        // so playback is not stuttered every 5 s.
        static MEMLOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *MEMLOG.get_or_init(|| std::env::var("PHIMAKOR_MEMLOG").is_ok()) {
            static MEMLOG_LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
            let now = std::time::Instant::now();
            let mut last = MEMLOG_LAST.lock().unwrap();
            if last.map_or(true, |t| now.duration_since(t) >= std::time::Duration::from_secs(5)) {
                if let Some(s) = &self.state { s.debug_memory(false); }
                *last = Some(now);
            }
        }
        // 菜单文件对话框结果轮询(PMCORE-7)。
        self.poll_dialog(event_loop);
        if let Some(s) = &self.state {
            s.window.request_redraw();
        }
    }
    fn exiting(&mut self, _: &ActiveEventLoop) {
        if let Some(s) = &self.state { if let Some(a) = &s.audio { a.quit(); } }
    }
}

/// Config file lives in the system config directory, decoupled from the
/// chart library so the library dir itself can be customized:
/// `%APPDATA%\PhiMakor\config.json` on Windows,
/// `$XDG_CONFIG_HOME`/`~/.config/phimakor/config.json` elsewhere.
fn config_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("PhiMakor");
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("phimakor")
}

/// Settings file: `<config_dir>/config.json`. Falls back to the legacy
/// locations (executable-dir and `<Documents>/PhiMakor/config.json`) for
/// reading, and migrates them on the next save.
fn settings_path() -> PathBuf {
    config_dir().join("config.json")
}

/// Old settings location `<Documents>/PhiMakor/config.json`, coupled to the
/// default chart library. Migrated into the config dir on the next save.
fn legacy_settings_path() -> PathBuf {
    default_charts_dir()
        .parent()
        .map(|p| p.join("config.json"))
        .unwrap_or_else(|| PathBuf::from("config.json"))
}

/// Load editor settings from `config.json` in the config directory.
/// Missing or invalid files fall back to defaults; a config found in a
/// legacy location is migrated to the new one.
fn load_settings() -> ui::SettingsData {
    let path = settings_path();
    let legacy = legacy_settings_path();
    let bytes = std::fs::read(&path)
        .or_else(|_| std::fs::read("config.json")) // legacy executable-dir
        .or_else(|_| std::fs::read(&legacy))        // legacy documents dir
        .ok();
    if let Some(b) = &bytes {
        if let Ok(settings) = serde_json::from_slice::<ui::SettingsData>(b) {
            // Migrate: a config read from a legacy location gets copied to
            // the new path so the old one can be dropped.
            if !path.exists() {
                if let Ok(json) = serde_json::to_string_pretty(&settings) {
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(&path, json);
                }
            }
            return settings;
        }
    }
    ui::SettingsData::default()
}

/// Persist editor settings to `config.json` in the config directory.
/// Also removes legacy-location files once the new one is written.
fn save_settings(settings: &ui::SettingsData) {
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let path = settings_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&path, json).is_ok() {
            let _ = std::fs::remove_file("config.json");
            let _ = std::fs::remove_file(legacy_settings_path());
        }
    }
}

/// Process memory snapshot: (current working set, peak working set, commit
/// pagefile usage) in bytes, or `None` on non-Windows / failure.
fn process_mem() -> Option<(usize, usize, usize)> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut mci = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            PageFaultCount: 0,
            PeakWorkingSetSize: 0,
            WorkingSetSize: 0,
            QuotaPeakPagedPoolUsage: 0,
            QuotaPagedPoolUsage: 0,
            QuotaPeakNonPagedPoolUsage: 0,
            QuotaNonPagedPoolUsage: 0,
            PagefileUsage: 0,
            PeakPagefileUsage: 0,
        };
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut mci, mci.cb) };
        if ok != 0 {
            return Some((mci.WorkingSetSize as usize, mci.PeakWorkingSetSize as usize, mci.PagefileUsage as usize));
        }
    }
    let _ = ();
    None
}

/// Recursively copy `src` into `dst` (used for drag-and-drop import).
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Whether `p` looks like a chart directory: it must carry `info.json`,
/// RPE web-export `info.yml`, or a legacy `info.txt`.
fn is_chart_dir(p: &std::path::Path) -> bool {
    p.is_dir() && has_info_file(p)
}

/// True if the directory contains any supported info file.
fn has_info_file(p: &std::path::Path) -> bool {
    p.join("info.json").exists() || p.join("info.yml").exists() || p.join("info.txt").exists()
}

/// Probe a `.zip` chart package: it must contain an `info.json` / `info.yml` /
/// `info.txt` entry. Returns `Ok(chart_root)` where `chart_root` is the
/// archive-internal directory holding the info file (strip a single leading
/// wrapper dir).
fn probe_chart_zip(zip_path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let file = std::fs::File::open(zip_path).map_err(|e| anyhow::anyhow!("open zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| anyhow::anyhow!("zip parse: {e}"))?;
    // Collect candidate roots: paths of info entries, plus their parent dir.
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    for i in 0..archive.len() {
        let name = archive.by_index(i).map_err(|e| anyhow::anyhow!("zip entry: {e}"))?.name().to_string();
        let file_name = dir_name(std::path::Path::new(&name));
        if file_name == "info.json" || file_name == "info.yml" || file_name == "info.txt" {
            let p = std::path::Path::new(&name);
            let root = p.parent().filter(|d| !d.as_os_str().is_empty()).map(|d| d.to_path_buf()).unwrap_or_else(|| std::path::PathBuf::new());
            roots.push(root);
        }
    }
    if roots.is_empty() {
        anyhow::bail!("not a chart zip: no info.json/info.yml/info.txt inside");
    }
    // Prefer the shallowest root (root-level info beats a nested one).
    roots.sort_by_key(|r| r.components().count());
    Ok(roots.into_iter().next().unwrap())
}

/// Extract a chart zip into the chart library. `zip_path` must pass
/// [`probe_chart_zip`] first. Returns the extracted chart directory.
fn import_chart_zip(zip_path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let root = probe_chart_zip(zip_path)?;
    let stem = zip_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "imported".to_string());
    let lib = charts_dir();
    let dest = lib.join(&stem);
    let _ = std::fs::create_dir_all(&dest);

    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if entry.is_dir() { continue; }
        // Strip the archive-internal chart root prefix.
        let rel = std::path::Path::new(&name)
            .strip_prefix(&root)
            .map(|r| r.to_path_buf())
            .unwrap_or_else(|_| std::path::PathBuf::from(&name));
        if rel.as_os_str().is_empty() { continue; }
        let out = dest.join(&rel);
        if let Some(parent) = out.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut f = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut f)?;
    }
    if !is_chart_dir(&dest) {
        // Info file was nested deeper than the stripped root — rescan.
        anyhow::bail!("extracted chart has no info.json/info.yml/info.txt at root");
    }
    Ok(dest)
}

/// 导出当前谱面为 `.zip` 谱面包(PMCORE-25)。
/// 包内容为根级布局(与 probe_chart_zip/import_chart_zip 兼容,无 wrapper 目录):
/// - `info.json`:序列化当前 ChartInfo(load_info 兼容字段);
/// - `info.chart` 指向的 chart 文件(必打包,缺失报错);
/// - `info.music` / `info.illustration` 指向的文件(缺失跳过并警告)。
/// 先写 `<name>.zip.tmp` 再 rename,失败不产出半成品 zip。
/// 调用方应在导出前保存 chart(doc.save/flush),保证包内容与内存一致。
fn export_chart_zip(chart_dir: &Path, out_dir: &Path, info: &core::model::ChartInfo) -> anyhow::Result<PathBuf> {
    use std::io::Write;
    // zip 文件名取谱名(Windows 非法字符替换为 _),空名回退目录名。
    let name: String = info.name.trim().chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    let name = if name.is_empty() { dir_name(chart_dir) } else { name };
    let name = if name.is_empty() { "chart".to_string() } else { name };
    let out_path = out_dir.join(format!("{name}.zip"));
    let tmp_path = out_dir.join(format!("{name}.zip.tmp"));
    let file = std::fs::File::create(&tmp_path).map_err(|e| anyhow::anyhow!("create {}: {e}", tmp_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    // info.json:统一序列化当前 ChartInfo。
    zip.start_file("info.json", opts)?;
    zip.write_all(&serde_json::to_vec_pretty(info)?)?;
    // chart 必打包;music/illustration 缺失跳过并警告。
    let mut packed: Vec<&str> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for (label, field) in [("chart", &info.chart), ("music", &info.music), ("illustration", &info.illustration)] {
        let src = chart_dir.join(field);
        match std::fs::File::open(&src) {
            Ok(mut f) => {
                // 条目名与 info.json 引用一致,导入回读时路径不失效。
                zip.start_file(field.to_string(), opts)?;
                std::io::copy(&mut f, &mut zip)?;
                packed.push(label);
            }
            Err(e) if label == "chart" => {
                drop(zip);
                let _ = std::fs::remove_file(&tmp_path);
                anyhow::bail!("chart 文件缺失:{} ({e})", src.display());
            }
            Err(e) => {
                eprintln!("warning: 导出跳过缺失的 {label} 文件 {} ({e})", src.display());
                missing.push(label);
            }
        }
    }
    zip.finish()?;
    // 防半成品:临时文件写完后 rename;Windows 下 rename 不覆盖已存在文件,先删旧档。
    if out_path.exists() {
        let _ = std::fs::remove_file(&out_path);
    }
    std::fs::rename(&tmp_path, &out_path)?;
    eprintln!(
        "export ok: {} (packed: {}){}",
        out_path.display(),
        packed.join(", "),
        if missing.is_empty() { String::new() } else { format!("; missing: {}", missing.join(", ")) }
    );
    Ok(out_path)
}

/// Resolve the chart library directory. Precedence:
/// 1. `PHIMAKOR_CHARTS_DIR` env var (overrides everything, not persisted)
/// 2. `charts_dir` from `config.json` (customized via `--charts-dir`, persisted)
/// 3. default: `<Documents>/PhiMakor/charts` (created on first use)
fn charts_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PHIMAKOR_CHARTS_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Some(custom) = load_settings().charts_dir.filter(|d| !d.trim().is_empty()) {
        return PathBuf::from(custom);
    }
    default_charts_dir()
}

/// Default chart library: `<Documents>/PhiMakor/charts/`. Uses the *real*
/// system Documents folder — on Windows that is the known folder
/// (`SHGetKnownFolderPath`), so redirected/OneDrive locations are honored;
/// other platforms fall back to `$HOME/Documents`.
fn default_charts_dir() -> PathBuf {
    documents_dir().join("PhiMakor").join("charts")
}

/// The system Documents directory (real known-folder location on Windows).
fn documents_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(d) = known_documents_dir() {
        return d;
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("Documents")
}

/// Windows: resolve the Documents known folder (handles redirection such as
/// OneDrive and custom library locations), instead of assuming
/// `%USERPROFILE%\Documents`.
#[cfg(windows)]
fn known_documents_dir() -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Documents};

    let mut ptr: windows_sys::core::PWSTR = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_Documents, 0, std::ptr::null_mut(), &mut ptr) };
    if hr != 0 || ptr.is_null() {
        return None;
    }
    let len = unsafe { (0..).take_while(|&i| *ptr.add(i) != 0).count() };
    let path = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len)) };
    unsafe { CoTaskMemFree(ptr as *mut _) };
    if path.is_empty() { None } else { Some(PathBuf::from(path)) }
}

/// PMCORE-24:崩溃恢复检测。扫描谱面库,找第一张上次异常退出的谱面:
/// chart 主文件缺失、或 .bak 比主文件新(说明主文件最后一次写入没完成)。
/// 返回待恢复的 .bak 路径(splash 顶部提示条用),没有则 None。
fn find_recover_hint(charts: &[ui::ChartEntry]) -> Option<String> {
    for ch in charts {
        let Some(info) = read_chart_info(&ch.path) else { continue };
        let main = ch.path.join(&info.chart);
        let bak = ch.path.join(format!("{}.bak", info.chart));
        if !bak.is_file() {
            continue;
        }
        let main_t = main.metadata().and_then(|m| m.modified()).ok();
        let bak_t = bak.metadata().and_then(|m| m.modified()).ok();
        let abnormal = match (main_t, bak_t) {
            (_, None) => false,          // .bak 刚被删?不算异常。
            (Some(mt), Some(bt)) => mt < bt,
            (None, Some(_)) => true,     // 主文件缺失但 .bak 在 → 上次没写完。
        };
        if abnormal {
            return Some(bak.display().to_string());
        }
    }
    None
}

fn scan_charts() -> Vec<ui::ChartEntry> {
    let mut charts = Vec::new();
    let dir = charts_dir();
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    if let Ok(readdir) = std::fs::read_dir(&dir) {
        for e in readdir.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            if is_chart_dir(&p) {
                // 单 chart 目录:行为不变(PMCORE-26)。
                charts.push(read_chart_entry(&p));
            } else if let Some(agg) = read_multi_entry(&p) {
                // 同曲多难度(父目录含 ≥2 个同 info.name 的 chart 子目录):
                // 聚合为一条目,path = 首个难度子目录,难度徽章展示(PMCORE-26)。
                charts.push(agg);
            }
        }
    }
    charts.sort_by(|a, b| a.name.cmp(&b.name));
    charts
}

/// 难度显示名:level 首词(IN/HD/EZ/AT/SP 惯例)优先,否则 difficulty 数值,
/// 否则目录名。供 splash 徽章与编辑器内难度切换共用(PMCORE-26)。
fn diff_label(info: &core::model::ChartInfo, dir: &std::path::Path) -> String {
    let tag = info.level.split_whitespace().next().unwrap_or("").to_uppercase();
    if !tag.is_empty() {
        return tag;
    }
    if info.difficulty > 0.0 {
        return format!("{:.1}", info.difficulty);
    }
    dir_name(dir)
}

/// 扫描 parent 下的同曲难度子目录(PMCORE-26):≥2 个 chart 子目录且
/// info.name 非空且全部相同 → 返回按难度升序的 (difficulty, 显示名, 目录);
/// 否则 None(不聚合,保持单 chart 目录行为)。任一名字不一致即不聚合。
fn multi_difficulty_dirs(parent: &std::path::Path) -> Option<Vec<(f32, String, PathBuf)>> {
    let mut diffs: Vec<(f32, String, PathBuf)> = Vec::new();
    let mut name: Option<String> = None;
    for e in std::fs::read_dir(parent).ok()?.flatten() {
        let p = e.path();
        if !is_chart_dir(&p) {
            continue;
        }
        let Some(info) = read_chart_info(&p) else { continue };
        if info.name.trim().is_empty() {
            continue;
        }
        match &name {
            None => name = Some(info.name.clone()),
            Some(n) if *n != info.name => return None,
            _ => {}
        }
        diffs.push((info.difficulty, diff_label(&info, &p), p));
    }
    if diffs.len() < 2 {
        return None;
    }
    diffs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.1.cmp(&b.1)));
    Some(diffs)
}

/// 多难度聚合条目(PMCORE-26):元数据/缩略图取首个难度子目录(按难度升序),`path`
/// 指向该子目录(点击/预载直接打开),`difficulties` = 全部难度。不足 2 个难度返回 None。
fn read_multi_entry(parent: &std::path::Path) -> Option<ui::ChartEntry> {
    let diffs = multi_difficulty_dirs(parent)?;
    let first_dir = diffs[0].2.clone();
    let info = read_chart_info(&first_dir);
    let (name, composer, charter, level, difficulty, illustration) = info.as_ref().map(|i| (
        i.name.clone(), i.composer.clone(), i.charter.clone(), i.level.clone(), i.difficulty, i.illustration.clone(),
    )).unwrap_or_default();
    let modified = diffs.iter().map(|(_, _, d)| std::fs::metadata(d).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|x| x.as_secs()).unwrap_or(0)).max().unwrap_or(0);
    let thumb = load_thumb(&first_dir, &illustration);
    let difficulties = diffs.into_iter().map(|(_, label, d)| (label, d)).collect();
    Some(ui::ChartEntry {
        name, path: first_dir.to_path_buf(), composer, charter, level, difficulty,
        modified, thumb, difficulties,
    })
}

/// 打开目录时把歌曲根(无 info、含同曲难度子目录)解析到首个难度子目录
/// (PMCORE-26),使拖入/CLI/菜单打开歌曲根也能进入编辑器;其余路径原样返回。
fn resolve_open_dir(dir: &std::path::Path) -> PathBuf {
    if is_chart_dir(dir) {
        return dir.to_path_buf();
    }
    if let Some(agg) = read_multi_entry(dir) {
        return agg.path;
    }
    dir.to_path_buf()
}

/// 从已加载目录推导多难度上下文(PMCORE-26):dir 所在父目录含 ≥2 个同
/// info.name 的 chart 子目录(含 dir)→ (父目录, 难度列表);否则 (None, 空)。
/// 父目录自身是 chart 目录时视为单谱布局,不聚合。
fn resolve_difficulty_context(dir: &std::path::Path) -> (Option<PathBuf>, Vec<(String, PathBuf)>) {
    let Some(parent) = dir.parent() else { return (None, Vec::new()) };
    if is_chart_dir(parent) {
        return (None, Vec::new());
    }
    match multi_difficulty_dirs(parent) {
        Some(diffs) => {
            let list = diffs.into_iter().map(|(_, label, d)| (label, d)).collect();
            (Some(parent.to_path_buf()), list)
        }
        None => (None, Vec::new()),
    }
}

/// 删除谱面条目(PMCORE-26):多难度聚合条目删除整曲根目录(父目录),单 chart
/// 目录删除自身——避免删掉一个难度后其余难度从列表消失。
fn delete_chart_entry(entry: &ui::ChartEntry) {
    let target = if entry.difficulties.is_empty() {
        entry.path.clone()
    } else {
        entry.path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| entry.path.clone())
    };
    let _ = std::fs::remove_dir_all(&target);
}

/// PMCORE-36:生成新谱面模板目录并返回其路径。
///
/// 写入 `charts_dir()/<名>`:
/// - `info.json`:name = 用户输入谱名,chart = `chart.json`,难度字段用默认值;
/// - `chart.json`:单条空判定线,默认 BPM 120 / offset 0(rpe_version 160,
///   避开 ≥170 的现代 speed 语义;字段齐全,满足 detect_format/from_rpe_chart)。
///
/// 重名自动去重(`name`、`name-2`、`name-3`…),不静默覆盖现有谱面。
/// 失败返回可读错误(留在 splash 对话框显示)。
fn create_new_chart(name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("谱名不能为空".into());
    }
    let base = sanitize_folder_name(name);
    if base.is_empty() {
        return Err("谱名只包含非法字符,无法用作文件夹名".into());
    }
    let lib = charts_dir();
    std::fs::create_dir_all(&lib).map_err(|e| format!("无法创建谱面库目录 {}: {e}", lib.display()))?;
    let mut dir = lib.join(&base);
    let mut n = 2u32;
    while dir.exists() {
        dir = lib.join(format!("{base}-{n}"));
        n += 1;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建目录 {}: {e}", dir.display()))?;
    let info = core::model::ChartInfo {
        name: name.to_string(),
        chart: "chart.json".into(),
        difficulty: 0.0,
        level: String::new(),
        charter: String::new(),
        composer: String::new(),
        illustrator: String::new(),
        music: String::new(),
        illustration: String::new(),
        ..Default::default()
    };
    let info_json = serde_json::to_string_pretty(&info).map_err(|e| format!("info.json 序列化失败: {e}"))?;
    std::fs::write(dir.join("info.json"), info_json).map_err(|e| format!("写 info.json 失败: {e}"))?;
    let chart = core::model::RPEChart {
        meta: core::model::RPEMetadata { offset: 0, rpe_version: 160 },
        bpm_list: vec![core::model::RPEBpmItem { bpm: 120.0, start_time: core::bpm::Triple::default() }],
        judge_line_list: vec![core::model::RPEJudgeLine {
            name: "Main".into(),
            texture: "line.png".into(),
            parent: None,
            rotate_with_father: None,
            event_layers: vec![None],
            extended: None,
            notes: None,
            is_cover: 0,
            z_order: 0,
            attach_ui: None,
            pos_control: vec![],
            size_control: vec![],
            alpha_control: vec![],
            y_control: vec![],
            comment: None,
        }],
    };
    let chart_json = serde_json::to_string(&chart).map_err(|e| format!("chart.json 序列化失败: {e}"))?;
    std::fs::write(dir.join("chart.json"), chart_json).map_err(|e| format!("写 chart.json 失败: {e}"))?;
    Ok(dir)
}

/// Windows 文件/文件夹名清洗:非法字符(`<>:"/\|?*`、控制符)替换为 `_`,
/// 去尾部空格/句点,Windows 保留名(CON/NUL/COM1…等)追加 `_`。空结果返回 ""。
fn sanitize_folder_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    while s.ends_with(' ') || s.ends_with('.') {
        s.pop();
    }
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL",
        "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
        "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&s.to_uppercase().as_str()) {
        s.push('_');
    }
    s
}

/// Read a chart's `info.json` (falling back to RPE web-export `info.yml`,
/// then legacy `info.txt`), or `None`.
fn read_chart_info(dir: &std::path::Path) -> Option<core::model::ChartInfo> {
    if let Ok(src) = std::fs::read_to_string(dir.join("info.json")) {
        if let Ok(info) = serde_json::from_str::<core::model::ChartInfo>(&src) {
            return Some(info);
        }
    }
    if let Ok(src) = std::fs::read_to_string(dir.join("info.yml")) {
        if let Ok(yaml) = serde_yaml::from_str::<core::model::InfoYaml>(&src) {
            return Some(yaml.into_chart_info());
        }
    }
    if let Ok(src) = std::fs::read_to_string(dir.join("info.txt")) {
        return Some(core::model::parse_info_txt(&src));
    }
    None
}

/// 谱面显示名:info.name 非空用之,否则回退目录名(PMCORE-26)。
fn chart_display_name(info: &core::model::ChartInfo, dir: &std::path::Path) -> String {
    if info.name.is_empty() { dir_name(dir) } else { info.name.clone() }
}

/// Build a splash list entry: metadata + thumbnail (best effort).
fn read_chart_entry(dir: &std::path::Path) -> ui::ChartEntry {
    let folder = dir_name(dir);
    let info = read_chart_info(dir);
    let (name, composer, charter, level, difficulty, illustration) = info.as_ref().map(|i| (
        chart_display_name(i, dir),
        i.composer.clone(), i.charter.clone(), i.level.clone(), i.difficulty, i.illustration.clone(),
    )).unwrap_or_else(|| (folder, String::new(), String::new(), String::new(), 0.0, String::new()));
    let modified = std::fs::metadata(dir).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs()).unwrap_or(0);
    let thumb = load_thumb(dir, &illustration);
    ui::ChartEntry { name, path: dir.to_path_buf(), composer, charter, level, difficulty, modified, thumb, difficulties: Vec::new() }
}

/// Decode a chart's illustration (or bg fallback) as a small thumbnail.
fn load_thumb(dir: &std::path::Path, illustration: &str) -> Option<image::RgbaImage> {
    let mut path = dir.join(illustration);
    if !path.is_file() { path = dir.join("bg.png"); }
    if !path.is_file() { path = dir.join("background.png"); }
    if !path.is_file() { return None; }
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (w, h) = (img.width(), img.height());
    let max_dim = 200u32;
    if w.max(h) <= max_dim { return Some(img); }
    let scale = max_dim as f32 / w.max(h) as f32;
    Some(image::imageops::resize(&img, (w as f32 * scale).max(1.0) as u32, (h as f32 * scale).max(1.0) as u32, image::imageops::FilterType::Triangle))
}

/// Open a folder in the platform file manager (best effort).
fn open_in_explorer(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    { let _ = std::process::Command::new("explorer").arg(path).spawn(); }
    #[cfg(target_os = "macos")]
    { let _ = std::process::Command::new("open").arg(path).spawn(); }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    { let _ = std::process::Command::new("xdg-open").arg(path).spawn(); }
}

#[cfg(feature = "profiling")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(feature = "profiling")]
fn init_profiling() {
    tracy_client::Client::start();
}

fn init_tracing() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_env("RUST_LOG"))
        .with(tracing_subscriber::fmt::layer().with_timer(tracing_subscriber::fmt::time::uptime()))
        .init();
}

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "profiling")]
    let _heap_profiler = {
        init_profiling();
        // Heap profiler: tracks every allocation through the global
        // allocator and dumps `dhat-heap.json` (CWD) when main returns —
        // the definitive breakdown when Task Manager shows unexplained RSS.
        dhat::Profiler::new_heap()
    };
    init_tracing();
    // Args: `phimakor [--charts-dir <path>] [<chart dir>]`.
    // `--charts-dir` sets a custom chart library and persists it back to
    // `config.json` (env `PHIMAKOR_CHARTS_DIR` overrides without persisting).
    let mut dir: Option<PathBuf> = None;
    let mut custom_dir: Option<PathBuf> = None;
    {
        let mut args = std::env::args_os().skip(1);
        while let Some(a) = args.next() {
            if a.to_string_lossy() == "--charts-dir" {
                custom_dir = args.next().map(PathBuf::from);
            } else if !a.to_string_lossy().starts_with("--") && dir.is_none() {
                dir = Some(PathBuf::from(a));
            }
        }
    }
    if let Some(cd) = custom_dir {
        let mut settings = load_settings();
        settings.charts_dir = Some(cd.to_string_lossy().to_string());
        save_settings(&settings);
    }
    let el = EventLoop::new()?;
    el.set_control_flow(ControlFlow::Poll);
    el.run_app(&mut App { dir, state: None, dialog_rx: None })?;
    Ok(())
}

/// PMCORE-71 手动基准:`cargo test --bin phimakor preload_bench -- --ignored --nocapture`。
/// 输出 `load_chart_async` 完整耗时 = 现状点击后 loading 屏等待时间(预载把它移出
/// 关键路径;预载命中后点击只做 apply_loaded_chart 的 GPU 上传)。
#[cfg(test)]
mod preload_bench {
    use super::*;

    #[test]
    #[ignore]
    fn preload_bench() {
        let root = std::env::var("PHIMAKOR_CHARTS_DIR")
            .unwrap_or_else(|_| "D:/DOCU/PhiMakor/charts".to_string());
        let dirs: Vec<PathBuf> = std::fs::read_dir(&root).ok()
            .map(|it| it.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir()).collect())
            .unwrap_or_default();
        assert!(!dirs.is_empty(), "no chart dirs under {root}");
        println!("PMCORE-71 preload bench: {} charts under {root}\n", dirs.len());
        for dir in dirs.iter().take(6) {
            let t0 = Instant::now();
            match load_chart_async(dir.clone()) {
                Ok(loaded) => {
                    // 音频必须暂停落地(悬停不播音乐,PMCORE-71)。set_paused 是
                    // 命令,is_paused 仅在音频线程处理后才翻转(≤5ms poll),
                    // 故轮询确认(与 audio/mod.rs 测试同模式)。
                    let paused = loaded.audio.as_ref().is_none_or(|a| {
                        let start = Instant::now();
                        while !a.is_paused() && start.elapsed() < Duration::from_secs(2) {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        a.is_paused()
                    });
                    assert!(paused, "preloaded audio must be paused");
                    println!("{:24} load={:8.1}ms (audio: {})",
                        dir_name(dir),
                        t0.elapsed().as_secs_f64() * 1000.0,
                        if loaded.audio.is_some() { "paused" } else { "none" });
                }
                Err(e) => println!("{:24} FAILED: {e:#}", dir.display()),
            }
        }
    }
}

#[cfg(test)]
mod zip_tests {
    use super::*;

    fn make_zip(path: &std::path::Path, wrapped: bool) {
        use std::io::Write;
        let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        let base = if wrapped { "MyChart/" } else { "" };
        z.start_file(format!("{base}info.json"), opts).unwrap();
        z.write_all(br#"{"chart":"chart.json","name":"t"}"#).unwrap();
        z.start_file(format!("{base}chart.json"), opts).unwrap();
        z.write_all(br#"{"META":{"offset":0},"BPMList":[],"judgeLineList":[]}"#).unwrap();
        z.start_file(format!("{base}bg.png"), opts).unwrap();
        z.write_all(&[1, 2, 3]).unwrap();
        z.finish().unwrap();
    }

    #[test]
    fn zip_probe_and_import() {
        let dir = std::env::temp_dir().join("phimakor-zip-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Root-level info.
        let root_zip = dir.join("root.zip");
        make_zip(&root_zip, false);
        let probe = probe_chart_zip(&root_zip).unwrap();
        assert_eq!(probe.as_os_str(), "");

        // Wrapped info (common chart-pack pattern).
        let wrap_zip = dir.join("wrapped.zip");
        make_zip(&wrap_zip, true);
        let probe = probe_chart_zip(&wrap_zip).unwrap();
        assert_eq!(probe.to_string_lossy(), "MyChart");

        // Not a chart zip: no info file.
        let bad = dir.join("bad.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&bad).unwrap());
        z.start_file("readme.txt", zip::write::SimpleFileOptions::default()).unwrap();
        use std::io::Write;
        z.write_all(b"hi").unwrap();
        z.finish().unwrap();
        assert!(probe_chart_zip(&bad).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_roundtrip() {
        // 临时谱面目录:info.json + chart.json + music + illustration。
        let dir = std::env::temp_dir().join("phimakor-export-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let chart_dir = dir.join("MyChart");
        std::fs::create_dir_all(&chart_dir).unwrap();
        let info: core::model::ChartInfo = serde_json::from_str(
            r#"{"name":"往返测试","chart":"chart.json","music":"music.mp3","illustration":"illustration.png"}"#,
        )
        .unwrap();
        std::fs::write(chart_dir.join("info.json"), serde_json::to_vec_pretty(&info).unwrap()).unwrap();
        let chart_src = r#"{"META":{"offset":0},"BPMList":[],"judgeLineList":[]}"#;
        std::fs::write(chart_dir.join("chart.json"), chart_src).unwrap();
        std::fs::write(chart_dir.join("music.mp3"), b"fake-music").unwrap();
        std::fs::write(chart_dir.join("illustration.png"), b"fake-illustration").unwrap();

        // 导出 → probe → import 回读,info+chart 内容一致。
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let zip_path = export_chart_zip(&chart_dir, &out_dir, &info).unwrap();
        assert_eq!(zip_path.file_name().unwrap().to_string_lossy(), "往返测试.zip");
        // import 解包到临时谱面库,避免污染真实图表库。
        std::env::set_var("PHIMAKOR_CHARTS_DIR", dir.join("lib"));
        let root = probe_chart_zip(&zip_path).unwrap();
        assert_eq!(root.as_os_str(), ""); // 根级布局,无 wrapper 目录
        let imported = import_chart_zip(&zip_path).unwrap();
        std::env::remove_var("PHIMAKOR_CHARTS_DIR");
        assert_eq!(core::chart::load_info(&imported).unwrap(), info);
        assert_eq!(std::fs::read_to_string(imported.join("chart.json")).unwrap(), chart_src);
        assert!(imported.join("music.mp3").exists());
        assert!(imported.join("illustration.png").exists());

        // 曲绘缺失:跳过并警告,zip 仍可导入回读。
        std::fs::remove_file(chart_dir.join("illustration.png")).unwrap();
        let zip2 = export_chart_zip(&chart_dir, &out_dir, &info).unwrap();
        std::env::set_var("PHIMAKOR_CHARTS_DIR", dir.join("lib2"));
        let imported2 = import_chart_zip(&zip2).unwrap();
        std::env::remove_var("PHIMAKOR_CHARTS_DIR");
        assert_eq!(core::chart::load_info(&imported2).unwrap(), info);
        assert!(!imported2.join("illustration.png").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// PMCORE-21 加载错误提示:失败错误链必须含目录 + 原因(splash 横幅据此显示
/// "目录+原因+位置",不再只 eprintln 后静默回 splash)。
#[cfg(test)]
mod validate_load_error_tests {
    use super::*;

    #[test]
    fn load_error_contains_dir_and_reason() {
        let dir = std::env::temp_dir().join("phimakor-no-such-chart-xyz");
        let _ = std::fs::remove_dir_all(&dir); // 确保不存在,open 必失败
        // LoadedChart 无 Debug,不能用 unwrap_err,走 match。
        let err = match load_chart_async(dir.clone()) {
            Err(e) => e,
            Ok(_) => panic!("expected load error for {}", dir.display()),
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("load chart"), "应含目录上下文: {msg}");
        assert!(msg.contains("phimakor-no-such-chart-xyz"), "应含目录路径: {msg}");
        assert!(msg.contains("failed to read"), "应含失败原因: {msg}");
    }
}

/// PMCORE-26 多难度谱面:扫描聚合 / 保存目标切换 / 难度上下文推导。
#[cfg(test)]
mod multi_difficulty_tests {
    use super::*;

    /// 写一个最小可用谱面目录(info.json + chart.json,含一条空判定线)。
    fn make_chart_dir(dir: &Path, name: &str, difficulty: f32, level: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let info = serde_json::json!({
            "name": name, "chart": "chart.json",
            "difficulty": difficulty, "level": level,
        });
        std::fs::write(dir.join("info.json"), serde_json::to_vec(&info).unwrap()).unwrap();
        let chart = core::model::RPEChart {
            meta: core::model::RPEMetadata { offset: 0, rpe_version: 160 },
            bpm_list: vec![core::model::RPEBpmItem { bpm: 120.0, start_time: core::bpm::Triple::default() }],
            judge_line_list: vec![core::model::RPEJudgeLine {
                name: "Main".into(), texture: "line.png".into(), parent: None, rotate_with_father: None,
                event_layers: vec![None], extended: None, notes: Some(vec![]),
                is_cover: 0, z_order: 0, attach_ui: None,
                pos_control: vec![], size_control: vec![], alpha_control: vec![], y_control: vec![], comment: None,
            }],
        };
        std::fs::write(dir.join("chart.json"), serde_json::to_vec(&chart).unwrap()).unwrap();
    }

    fn test_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("phimakor-multidiff-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn tap(kind: u8, beats: f64, x: f32) -> core::model::RPENote {
        core::model::RPENote {
            kind, above: 1, start_time: core::bpm::Triple::from_beats(beats),
            end_time: core::bpm::Triple::from_beats(beats), position_x: x, y_offset: 0.,
            alpha: 255, hitsound: None, size: 1.0, speed: 1.0, is_fake: 0,
            visible_time: 999999., tint: None, tint_hit_effects: None, judge_area: None, comment: None,
        }
    }

    #[test]
    fn scan_charts_aggregates_multi_difficulty() {
        let root = test_root("scan");
        let song = root.join("MySong");
        make_chart_dir(&song.join("EZ"), "MySong", 4.0, "EZ");
        make_chart_dir(&song.join("HD"), "MySong", 8.0, "HD");
        make_chart_dir(&song.join("IN"), "MySong", 12.0, "IN");
        // 单 chart 目录(独立谱面,不聚合)。
        make_chart_dir(&root.join("Other"), "Other", 5.0, "HD");

        std::env::set_var("PHIMAKOR_CHARTS_DIR", &root);
        let charts = scan_charts();
        std::env::remove_var("PHIMAKOR_CHARTS_DIR");

        assert_eq!(charts.len(), 2, "聚合 + 单目录各一条");
        let agg = charts.iter().find(|c| c.name == "MySong").expect("聚合条目");
        assert_eq!(agg.difficulties.len(), 3);
        // 同曲按难度升序:EZ, HD, IN。
        let labels: Vec<&str> = agg.difficulties.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["EZ", "HD", "IN"]);
        // path = 首个难度子目录(点击/预载直接打开)。
        assert!(agg.path.ends_with("EZ"));
        assert_eq!(agg.difficulty, 4.0, "聚合条目的难度/元数据取首个难度");
        let single = charts.iter().find(|c| c.name == "Other").expect("单条目");
        assert!(single.difficulties.is_empty(), "单 chart 目录不聚合");
        assert!(single.path.ends_with("Other"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_skips_mismatched_names() {
        let root = test_root("mismatch");
        let song = root.join("Song");
        // 两个子目录名字不同 → 不聚合(保持未列出的原行为,不产生伪聚合)。
        make_chart_dir(&song.join("IN"), "Song", 12.0, "IN");
        make_chart_dir(&song.join("HD"), "OtherSong", 8.0, "HD");

        std::env::set_var("PHIMAKOR_CHARTS_DIR", &root);
        let charts = scan_charts();
        std::env::remove_var("PHIMAKOR_CHARTS_DIR");

        assert!(charts.is_empty(), "名字不一致不聚合,也不单独列出子目录");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_target_follows_difficulty_dir() {
        // 每个难度独立 info+chart:打开哪个难度,save 就写哪个目录的 chart 文件。
        let root = test_root("save");
        let ez = root.join("EZ");
        let hd = root.join("HD");
        make_chart_dir(&ez, "Song", 4.0, "EZ");
        make_chart_dir(&hd, "Song", 8.0, "HD");

        let mut doc = ChartDocument::open(&ez).unwrap();
        doc.add_note(0, tap(1, 1.0, 0.0)).unwrap();
        doc.save().unwrap();
        drop(doc);

        // 切换难度 = 打开另一难度目录(编辑器经 reload_chart 实现,保存目标随目录)。
        let mut doc2 = ChartDocument::open(&hd).unwrap();
        doc2.add_note(0, tap(4, 2.0, 100.0)).unwrap();
        doc2.save().unwrap();
        drop(doc2);

        let ez_chart: core::model::RPEChart =
            serde_json::from_str(&std::fs::read_to_string(ez.join("chart.json")).unwrap()).unwrap();
        let hd_chart: core::model::RPEChart =
            serde_json::from_str(&std::fs::read_to_string(hd.join("chart.json")).unwrap()).unwrap();
        let ez_notes = ez_chart.judge_line_list[0].notes.as_ref().unwrap();
        let hd_notes = hd_chart.judge_line_list[0].notes.as_ref().unwrap();
        assert_eq!(ez_notes.len(), 1);
        assert_eq!(ez_notes[0].kind, 1, "EZ 保存写 EZ 目录的 chart.json");
        assert_eq!(hd_notes.len(), 1);
        assert_eq!(hd_notes[0].kind, 4, "HD 保存写 HD 目录的 chart.json");
        assert_eq!(hd_notes[0].position_x, 100.0, "HD 音符内容独立");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_context_finds_siblings() {
        let root = test_root("ctx");
        let song = root.join("Song");
        make_chart_dir(&song.join("IN"), "Song", 12.0, "IN");
        make_chart_dir(&song.join("HD"), "Song", 8.0, "HD");
        // 单 chart 目录:上下文为空。
        make_chart_dir(&root.join("Solo"), "Solo", 3.0, "EZ");

        let (song_root, diffs) = resolve_difficulty_context(&song.join("IN"));
        assert_eq!(song_root.as_deref(), Some(song.as_path()), "根目录 = 难度目录的父目录");
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].0, "HD", "按难度升序");
        assert_eq!(diffs[1].0, "IN");

        let (r2, d2) = resolve_difficulty_context(&root.join("Solo"));
        assert!(r2.is_none());
        assert!(d2.is_empty(), "单 chart 目录无难度上下文");
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// PMCORE-22 A-B 循环:点位校验 + 时间轴点击 seek 的 snap→time 换算。
#[cfg(test)]
mod ab_loop_tests {
    use super::*;

    #[test]
    fn loop_points_valid_accepts_only_a_lt_b() {
        // 正常:A=0(允许)、B=谱面末尾(允许)。
        assert_eq!(loop_points_valid(Some(0.0), Some(8.0)), Ok((0.0, 8.0)));
        // A==B 拒绝。
        assert!(loop_points_valid(Some(4.0), Some(4.0)).is_err());
        // A>B 拒绝。
        assert!(loop_points_valid(Some(6.0), Some(4.0)).is_err());
        // 任一未设拒绝。
        assert!(loop_points_valid(None, Some(4.0)).is_err());
        assert!(loop_points_valid(Some(4.0), None).is_err());
        assert!(loop_points_valid(None, None).is_err());
    }

    #[test]
    fn timeline_click_snap_seek_conversion() {
        // 最小谱面:120 BPM(每拍 0.5s),offset 0。
        let src = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                { "Name": "l0", "Texture": "line.png", "father": -1,
                  "eventLayers": [ null ], "notes": [], "isCover": 0 }
            ]
        }"#;
        let rpe: core::model::RPEChart = serde_json::from_str(src).unwrap();
        let mut chart = core::chart::Chart::from_rpe_chart(&rpe, false).unwrap();
        // 点击 raw beat 4.4,snap=0.25 → 4.4/0.25=17.6 四舍五入到 18 → 4.5
        // → 120bpm(0.5s/拍)下 = 2.25s。
        let snapped = ui::snap_beat(4.4, 0.25, 0.0);
        assert_eq!(snapped, 4.5);
        assert!((chart.beat_to_time(snapped) - 2.25).abs() < 1e-9);
        // A=0 边界:beat_to_time(0) = 0。
        assert_eq!(chart.beat_to_time(0.0), 0.0);
        // B 超出最后音符(谱面末尾之外)也能安全换算(线性外推,不越界)。
        assert!((chart.beat_to_time(100.0) - 50.0).abs() < 1e-9);
        // seek 目标 = chart 秒 + 总 offset(音频域)。
        let off = (chart.offset() + 0.0) as f64;
        assert!((chart.beat_to_time(snapped) + off - 2.25).abs() < 1e-9);
    }
}

/// PMCORE-XX 复制粘贴:目标拍位解析 + 整组粘贴变换(纯函数)。
#[cfg(test)]
mod clipboard_tests {
    use super::*;

    fn note_at(beats: f64, kind: u8) -> core::model::RPENote {
        core::model::RPENote {
            kind,
            above: 1,
            start_time: core::bpm::Triple::from_beats(beats),
            end_time: core::bpm::Triple::from_beats(beats),
            position_x: 0.,
            y_offset: 0.,
            alpha: 255,
            hitsound: None,
            size: 1.,
            speed: 1.,
            is_fake: 0,
            visible_time: 999999.,
            tint: None,
            tint_hit_effects: None,
            judge_area: None,
            comment: None,
        }
    }

    #[test]
    fn resolve_target_beat_priority_playhead_then_selected_then_zero() {
        // 播放头优先(吸附 snap)。
        assert_eq!(resolve_target_beat(4.4, 0.25, Some(10.0)), 4.5);
        assert_eq!(resolve_target_beat(2.1, 0.5, None), 2.0);
        // 播放头从未 seek(停在 0)→ 退回选中音符首拍。
        assert_eq!(resolve_target_beat(0.0, 0.25, Some(3.7)), 3.75);
        // 播放头 0 且无选中 → 兜底 0。
        assert_eq!(resolve_target_beat(0.0, 0.25, None), 0.0);
    }

    #[test]
    fn paste_transform_keeps_offsets_and_snaps_anchor() {
        // 组内相对偏移(0/2/4 拍)在锚点 10.0 处原样保留;hold end 同步平移;
        // 其余字段(类型/x/注释等)全字段克隆。
        let mut tap = note_at(2.0, 1);
        tap.position_x = 123.0;
        tap.comment = Some("c".into());
        let mut hold = note_at(4.0, 2);
        hold.end_time = core::bpm::Triple::from_beats(6.0);
        let out = paste_notes_transform(&[tap.clone(), hold.clone()], 10.0);
        assert_eq!(out.len(), 2);
        // 组首(2.0)对齐锚点 → 偏移 +8。
        assert_eq!(out[0].start_time.beats(), 10.0);
        assert_eq!(out[1].start_time.beats(), 12.0);
        assert_eq!(out[1].end_time.beats(), 14.0); // hold end 同步偏移
        assert_eq!(out[0].kind, 1);
        assert_eq!(out[0].position_x, 123.0);
        assert_eq!(out[0].comment.as_deref(), Some("c"));
    }

    #[test]
    fn paste_transform_zero_end_stays_zero() {
        // end_time == 0 的非 hold 保持 0(不随 start 平移)。
        let mut n = note_at(1.0, 3);
        n.end_time = core::bpm::Triple::from_beats(0.0);
        let out = paste_notes_transform(&[n], 5.0);
        assert_eq!(out[0].start_time.beats(), 5.0);
        assert_eq!(out[0].end_time.beats(), 0.0);
    }
}








