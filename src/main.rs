mod audio;
mod core;
mod render;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use phimakor::trace_span;
use core::bpm::Triple;
use core::edit::{ChartDocument, EventKind};
use ui::panels::LayoutDef;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes, WindowId};

struct App {
    dir: Option<PathBuf>,
    state: Option<State>,
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
    seek_dim_until: Instant,
    focused: bool,
    ctrl: bool,

    // ── UI state ──
    show_overlay: bool,
    show_properties: bool,
    show_events: bool,
    show_notes: bool,
    full_notes: bool,
    gui_scale: f32,
    snap: f32, // snap interval in beats: 0.25, 0.5, 1.0
    vertical_split: u32, // number of vertical columns in notes panel (1 = no split)
    ui_dirty: bool,
    overlay_last_render: Instant,
    show_menu: bool,

    // ── Selection ──
    selected_line: usize,
    selected_layer: usize,
    selected_event_idx: Option<usize>,
    event_edit_target: u8, // 0=start_beats, 1=end_beats, 2=start_val, 3=end_val, 4=easing
    cache_valid: bool,
    cached_events: Arc<Vec<ui::EventEntry>>,
    cached_notes: Arc<Vec<ui::NoteEntry>>,

    // ── Layout / panels ──
    layout: LayoutDef,
    splash_mode: bool,
    splash_charts: Vec<ui::ChartEntry>,
    splash_search: String,
    splash_sel: Option<usize>,
    splash_sort: u8,
    splash_scroll: f32,
    splash_lib_path: String,
    splash_hover: ui::SplashHover,
    show_settings: bool,
    settings: ui::SettingsData,

    // ── Post-processing effects ──
    extra: Option<core::extra::ExtraRoot>,
    /// Selected row in the Eff panel list + edit field under the wheel
    /// (0 = shader, 1 = start, 2 = end, 3 = global).
    selected_effect: Option<usize>,
    eff_edit_field: u8,
}

impl State {
    fn placeholder() -> Self { panic!("placeholder State used before init") }
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
        Some(State {
            window, chart_dir: PathBuf::new(), renderer, overlay, doc, chart, info,
            audio: None, started: Instant::now(), fps_since: Instant::now(),
            aspect_idx: 0, show_overlay: true,
            show_properties: false, show_events: false, show_notes: false, full_notes: false,
            overlay_last_render: Instant::now(), combo: 0, hits: 0, note_count: 0,
            seek_dim_until: Instant::now(), fps: 0.0, frame_latency: 0.016,
            device_latency: std::env::var("PHIMAKOR_AUDIO_LATENCY_MS")
                .ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(15.0) / 1000.0,
            selected_line: 0, selected_event_idx: None, scroll_target: None,
            pending_seek: None, chart_time_last: 0.0, focused: true, ctrl: false,
            gui_scale: settings.gui_scale, snap: 0.25, selected_layer: 0, event_edit_target: 0,
            vertical_split: 14, layout: LayoutDef { panels: vec![] },
            ui_dirty: true, show_menu: false, splash_mode: true, splash_charts: charts,
            splash_search: String::new(), splash_sel: None, splash_sort: 0, splash_scroll: 0.0,
            splash_lib_path: charts_dir().display().to_string(),
            splash_hover: ui::SplashHover::None, show_settings: false, settings,
            cache_valid: false, cached_events: Arc::new(Vec::new()), cached_notes: Arc::new(Vec::new()),
            extra: None, selected_effect: None, eff_edit_field: 0,
        })
    }

    fn create_state(&mut self, event_loop: &ActiveEventLoop, dir: &PathBuf) -> anyhow::Result<State> {
        let window = Arc::new(event_loop.create_window(
            WindowAttributes::default().with_title("phimakor").with_inner_size(LogicalSize::new(1200.0, 800.0)),
        )?);
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

        let doc = ChartDocument::open(dir)?;
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

        let audio = audio::spawn_audio_thread(res_dir.as_path(), dir).ok();
        let note_count = chart.max_combo();
        let layout = LayoutDef::load(&res_dir.join("panels.json"))
            .or_else(|_| LayoutDef::load(&PathBuf::from("res.dis/panels.json")))
            .unwrap_or(LayoutDef { panels: vec![] });
        let mut overlay = ui::IcedOverlay::new(renderer.device(), renderer.tex_bgl(), renderer.sampler(), 1200, 800);
        overlay.set_panels(layout.panels.clone());
        Ok(State {
            window, chart_dir: dir.to_path_buf(), renderer, overlay, doc, chart, info, audio,
            started: Instant::now(), fps_since: Instant::now(), aspect_idx: 0,
            show_overlay: true, show_properties: false, show_events: false, show_notes: false,
            full_notes: false, overlay_last_render: Instant::now(), combo: 0, hits: 0, note_count,
            seek_dim_until: Instant::now(), fps: 0.0, frame_latency: 0.016,
            device_latency: std::env::var("PHIMAKOR_AUDIO_LATENCY_MS")
                .ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(15.0) / 1000.0,
            selected_line: 0, selected_event_idx: None, event_edit_target: 0,
            scroll_target: None, pending_seek: None, chart_time_last: 0.0,
            focused: true, ctrl: false, gui_scale: settings.gui_scale, snap: 0.25, selected_layer: 0,
            vertical_split: 14, layout, ui_dirty: true, show_menu: false, splash_mode: false,
            splash_charts: vec![], splash_search: String::new(), splash_sel: None, splash_sort: 0, splash_scroll: 0.0,
            splash_lib_path: String::new(), splash_hover: ui::SplashHover::None,
            show_settings: false, settings,
            cache_valid: false, cached_events: Arc::new(Vec::new()), cached_notes: Arc::new(Vec::new()), extra,
            selected_effect: None, eff_edit_field: 0,
        })
    }

    fn rebuild_chart(&mut self) {
        if let Some(state) = &mut self.state {
            state.rebuild_chart();
        }
    }

    /// Leave the editor and return to the splash screen: stop the audio
    /// thread, rescan the library, and swap in a fresh splash state.
    fn back_to_splash(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(s) = &self.state {
            if let Some(a) = &s.audio { a.quit(); }
        }
        let charts = scan_charts();
        self.state = None;
        if let Some(s) = self.create_splash_state(event_loop, charts) {
            self.state = Some(s);
        }
    }

    /// Drop the current state and open a chart directory (shared by the
    /// drag-and-drop, splash-click, and reload paths).
    fn open_chart(&mut self, event_loop: &ActiveEventLoop, path: &std::path::Path) {
        self.state = None;
        match self.create_state(event_loop, &path.to_path_buf()) {
            Ok(st) => self.state = Some(st),
            Err(e) => { eprintln!("failed to load {path:?}: {e:#}"); }
        }
    }
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

    /// Add a built-in effect spanning ±2 beats around the playhead.
    fn eff_add(&mut self) {
        let beat = self.chart.time_to_beat(self.chart_time_last);
        let (name, defaults) = crate::render::shaders::EFFECTS.first()
            .map(|d| (d.name.to_string(), d.defaults.to_vec()))
            .unwrap_or_else(|| ("grayscale".to_string(), Vec::new()));
        let vars: std::collections::HashMap<String, serde_json::Value> = defaults
            .into_iter()
            .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
            .collect();
        let extra = self.extra.get_or_insert_with(|| core::extra::ExtraRoot { bpm: vec![], effects: vec![] });
        extra.effects.push(core::extra::ExtraEffect {
            start: core::bpm::Triple::from_beats(beat - 2.0),
            end: core::bpm::Triple::from_beats(beat + 2.0),
            shader: name,
            global: true,
            priority: 0,
            vars,
        });
        self.selected_effect = None;
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
        self.eff_save();
    }

    /// Wheel editing on the selected effect's active field.
    fn eff_wheel(&mut self, delta: f32) {
        if delta == 0.0 { return; }
        let Some(sel) = self.selected_effect else { return };
        let idx = self.eff_sorted();
        let Some((orig, _)) = idx.get(sel).copied() else { return };
        let Some(extra) = &mut self.extra else { return };
        let e = &mut extra.effects[orig];
        match self.eff_edit_field {
            0 => {
                // Cycle through the built-in shaders; a custom shader jumps
                // to the first built-in.
                let names: Vec<&str> = crate::render::shaders::EFFECTS.iter().map(|d| d.name).collect();
                let next = match names.iter().position(|n| **n == e.shader) {
                    Some(p) => (p as isize + delta.signum() as isize).rem_euclid(names.len() as isize) as usize,
                    None => 0,
                };
                if let Some(n) = names.get(next) {
                    e.shader = n.to_string();
                }
            }
            1 | 2 => {
                let step = self.snap.max(0.01) as f64 * delta as f64;
                if self.eff_edit_field == 1 {
                    let start = (e.start.beats() + step).max(0.0);
                    e.start = core::bpm::Triple::from_beats(start);
                    if e.end.beats() <= start {
                        e.end = core::bpm::Triple::from_beats(start + 0.01);
                    }
                } else {
                    let end = (e.end.beats() + step).max(e.start.beats() + 0.01);
                    e.end = core::bpm::Triple::from_beats(end);
                }
            }
            3 => e.global = !e.global,
            _ => {}
        }
        self.eff_save();
    }

    fn rebuild_chart(&mut self) {
        let cur_time = self.chart_time_last;
        if let Ok(c) = core::chart::Chart::from_rpe_chart(self.doc.chart(), self.info.use_rpe_170_speed == Some(true)) {
            self.chart = c;
            self.note_count = self.chart.max_combo();
        }
        // Advance past current time so state_at doesn't re-fire all notes
        self.chart.state_at(cur_time);
        self.renderer.clear_hit_fx();
        self.cache_valid = false;
        self.selected_event_idx = None;
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
        self.hits = self.chart.hits_before(ct) as u32;
        self.combo = self.hits;
        self.seek_dim_until = Instant::now() + Duration::from_millis(400);
    }

    fn edit_selected_event(&mut self, f: impl Fn(&mut core::model::RPEEvent<f32>)) {
        let _s = trace_span!("edit_selected_event");
        let Some(ev_idx) = self.selected_event_idx else { return };
        let line_events = extract_line_events(&self.doc, self.selected_line, self.selected_layer);
        let Some(entry) = line_events.get(ev_idx) else { return };
        let kind = match entry.kind.as_str() {
            "Alpha" => EventKind::Alpha, "MoveX" => EventKind::MoveX, "MoveY" => EventKind::MoveY,
            "Rotate" => EventKind::Rotate, "Speed" => EventKind::Speed, _ => return,
        };
        if let Ok(mut ev) = self.doc.remove_event(self.selected_line, self.selected_layer, kind, entry.index) {
            f(&mut ev);
            if self.doc.add_event(self.selected_line, self.selected_layer, kind, ev).is_ok() {
                self.rebuild_chart();
                self.ui_dirty = true;
            }
        }
    }

    fn render_frame(&mut self) {
        let _span = trace_span!("render_frame");
        // Frame lock: the original code skipped rendering while the window
        // lost focus (saves GPU/CPU). That is not good for editing — the
        // view freezes and you can't watch the chart while alt-tabbed or
        // docked. The lock is disabled for now; WIP: add a proper frame
        // lock (and a setting to toggle it) here.
        // if !self.focused { return; }
        
        // Process note drag (before splash/chart frame to avoid chart borrow conflict)
        if let Some((ni, beat, nx)) = self.overlay.drag_updated.take() {
            if let Ok(old) = self.doc.remove_note(self.selected_line, ni) {
                let mut nn = old;
                nn.start_time = core::bpm::Triple::from_beats(beat.max(0.0));
                nn.end_time = core::bpm::Triple::from_beats((beat + if nn.kind == 2 { 1.0 } else { 0.0 }).max(0.0));
                nn.position_x = nx.clamp(-675.0, 675.0);
                let _ = self.doc.add_note(self.selected_line, nn);
                self.rebuild_chart();
                self.ui_dirty = true;
            }
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

        // Frame-render prediction (notes hit the line when the frame is seen)
        // minus the audio device output latency (rodio's get_pos counts samples
        // handed to the device callback, which are heard ~latency ms later).
        let predict = self.frame_latency.min(0.05);
        let off = (self.chart.offset() + self.info.offset) as f64;
        let chart_time = (audio_time + predict - self.device_latency - off).max(0.0);
        self.chart_time_last = chart_time;
        let duration = self.chart.duration();
        let chart_beat = self.chart.time_to_beat(chart_time);
        let line_count = self.chart.line_count();
        let line_name = if self.selected_line < line_count { self.chart.line_name(self.selected_line).to_string() } else { "?".to_string() };
        let frame = self.chart.state_at(chart_time);

        let pf_aspect = self.renderer.playfield_aspect();
        for fired in &frame.fired {
            let line = &frame.lines[fired.line];
            let t = line.rotation;
            let x = fired.x as f32;
            let cx = (line.position[0] + t.cos() * x) * 675.0;
            // Rotation term ×pf_aspect: ev_x/ev_y = 1/(1.5/P) = P/1.5, so the
            // canvas-X rotation px (675) meets the canvas-Y ev scale at P.
            let cy = (line.position[1] + t.sin() * x * pf_aspect) * 450.0;
            if fired.hold_tail { if !fired.fake { self.combo += 1; self.hits += 1; } continue; }
            if fired.tick { self.renderer.spawn_hit_fx([cx, cy]); continue; }
            if fired.fake { continue; }
            self.combo += 1; self.hits += 1;
            self.renderer.spawn_hit_fx([cx, cy]);
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
        let effect_names: Vec<String> = self.extra.as_ref().map_or(vec![], |extra| {
            core::extra::evaluate_effects(extra, chart_beat).iter().map(|e| e.shader_name.clone()).collect()
        });
        // Eff panel list: ALL effects sorted by start beat (index maps back to
        // ExtraRoot::effects for edits).
        let effects: Arc<Vec<ui::EffectRow>> = {
            let mut rows: Vec<ui::EffectRow> = self.extra.as_ref().map_or(Vec::new(), |extra| {
                extra.effects.iter().enumerate().map(|(i, e)| {
                    let (sb, eb) = (e.start.beats(), e.end.beats());
                    ui::EffectRow {
                        index: i,
                        shader: e.shader.clone(),
                        start_beats: sb,
                        end_beats: eb,
                        global: e.global,
                        active: chart_beat >= sb && chart_beat <= eb,
                    }
                }).collect()
            });
            rows.sort_by(|a, b| a.start_beats.total_cmp(&b.start_beats));
            Arc::new(rows)
        };
        let info = ui::GameInfo {
            chart_time, audio_time, fps: self.fps, combo: self.combo,
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
            show_menu: self.show_menu,
            chart_name: self.info.name.clone(),
            composer: self.info.composer.clone(),
            level: self.info.level.clone(),
            difficulty: self.info.difficulty,
            offset: self.info.offset,
            duration,
            selected_event_idx,
            event_edit_target: self.event_edit_target,
            ev_kind, ev_start_beats, ev_end_beats, ev_start_val, ev_end_val, ev_easing,
            effect_names: effect_names,
            effects,
            selected_effect: self.selected_effect,
            eff_edit_field: self.eff_edit_field,
        };
        if self.show_overlay {
            if let Some(ev_idx) = self.overlay.take_timeline_click(&info, self.overlay.props_progress()) {
                self.selected_event_idx = Some(ev_idx); self.ui_dirty = true;
            }
            if let Some(ly) = self.overlay.take_layer_click(self.overlay.props_progress(), max_layers) {
                self.selected_layer = ly; self.ui_dirty = true;
            }
            if self.ui_dirty {
                self.overlay.render_iced(self.renderer.queue(), &info);
                self.ui_dirty = false;
            } else {
                self.overlay.redraw_timeline(self.renderer.queue(), &info);
            }
        }
        // Evaluate post-processing effects at current beat
        self.renderer.post.active.clear();
        if let Some(extra) = &self.extra {
            let evals = core::extra::evaluate_effects(extra, chart_beat);
            let size = self.renderer.size();
            let (sw, sh) = (size[0] as f32, size[1] as f32);
            for e in &evals {
                let si = crate::render::shaders::EFFECTS.iter().position(|d| d.name == e.shader_name).unwrap_or(usize::MAX);
                let (uv, count) = if si == usize::MAX {
                    // Custom shader: use raw uniforms from extra.json vars
                    (e.uniforms.clone(), e.uniforms.len())
                } else {
                    let def = &crate::render::shaders::EFFECTS[si];
                    let mut uv: Vec<f32> = def.defaults.iter().map(|(_, v)| *v).collect();
                    let norm = |s: &str| s.to_lowercase().replace("_", "").replace("-", "");
                    for (i, (dname, _)) in def.defaults.iter().enumerate() {
                        let base = dname.trim_end_matches("_r").trim_end_matches("_g")
                            .trim_end_matches("_b").trim_end_matches("_a")
                            .trim_end_matches("_x").trim_end_matches("_y");
                        let nbase = norm(base);
                        if let Some(pos) = e.uniforms_names.iter().position(|n| norm(n) == nbase) {
                            uv[i] = e.uniforms[pos];
                        }
                        if dname.contains("screen_size") {
                            uv[i] = if dname.ends_with('x') { sw } else { sh };
                        }
                        if *dname == "time" {
                            uv[i] = chart_time as f32;
                        }
                    }
                    let l = uv.len(); (uv, l)
                };
                self.renderer.post.active.push(render::post::ActiveEffect {
                    shader_idx: si,
                    custom_name: if si == usize::MAX { Some(e.shader_name.clone()) } else { None },
                    priority: e.priority,
                    uniform_values: uv,
                    uniform_count: count,
                });
            }
        }

        let ui_bg = if self.show_overlay { Some(self.overlay.bind_group()) } else { None };
        let ui_iced = if self.show_overlay { Some(self.overlay.iced_bind_group()) } else { None };

        // Phigros-style HUD: hidden while editor panels cover the screen.
        self.renderer.set_hud(render::HudData {
            chart_name: self.info.name.clone(),
            difficulty: self.info.level.clone(),
            score,
            combo: self.combo,
            paused: self.audio.as_ref().is_some_and(|a| a.is_paused()),
            visible: !self.show_overlay,
        });
        self.renderer.set_progress(audio_time as f32 / duration as f32);
        match self.renderer.surface_acquire() {
            Ok(st) => {
                let view = st.texture.create_view(&wgpu::TextureViewDescriptor::default());
                self.renderer.draw_to_view(&view, frame, aspect, dim, ui_bg, ui_iced);
                self.renderer.queue().present(st);
                self.frame_latency = self.frame_latency * 0.9 + Instant::now().elapsed().as_secs_f64() * 0.1;
            }
            _ => {}
        }

        let dt = self.fps_since.elapsed().as_secs_f64();
        self.fps_since = Instant::now();
        self.fps = self.fps * 0.95 + (1.0 / dt.max(1e-6)) * 0.05;
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
    let mut out: Vec<ui::NoteEntry> = notes.iter().enumerate().map(|(i, n)| ui::NoteEntry {
        index: i,
        kind: n.kind,
        start_beats: n.start_time.beats(),
        end_beats: n.end_time.beats(),
        x: n.position_x,
        speed: n.speed,
        scale: n.size,
        texture: n.hitsound.clone().unwrap_or_default(),
    }).collect();
    // Sorted by end_beats for the timeline's binary-search visible window.
    out.sort_by(|a, b| a.end_beats.total_cmp(&b.end_beats));
    out
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
                let lower = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                let open_path = if lower == "zip" {
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
                    if let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) {
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
                    let hover = ui::splash_hit_test(mx, my, vw, vh, gs, filtered_len, st.show_settings, st.splash_scroll);
                    match hover {
                        ui::SplashHover::Settings => { st.show_settings = true; }
                        ui::SplashHover::Back => { st.show_settings = false; }
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
                        ui::SplashHover::ScaleMinus => { st.settings.gui_scale = (st.settings.gui_scale - 0.1).max(0.5); st.gui_scale = st.settings.gui_scale; save_settings(&st.settings); }
                        ui::SplashHover::ScalePlus => { st.settings.gui_scale = (st.settings.gui_scale + 0.1).min(2.0); st.gui_scale = st.settings.gui_scale; save_settings(&st.settings); }
                        ui::SplashHover::Library => { open_in_explorer(&charts_dir()); }
                        ui::SplashHover::Refresh => { st.splash_charts = scan_charts(); st.splash_sel = None; st.splash_scroll = 0.0; }
                        ui::SplashHover::Sort => { st.splash_sort = (st.splash_sort + 1) % 2; }
                        ui::SplashHover::OpenFolder => { open_in_explorer(&charts_dir()); }
                        ui::SplashHover::Chart(fi) => {
                            let ci = ui::filter_charts(&st.splash_charts, &st.splash_search, st.splash_sort).get(fi).copied();
                            let Some(ci) = ci else { return };
                            let path = st.splash_charts[ci].path.clone();
                            drop(st);
                            self.open_chart(event_loop, &path);
                            return;
                        }
                        ui::SplashHover::Delete(fi) => {
                            let ci = ui::filter_charts(&st.splash_charts, &st.splash_search, st.splash_sort).get(fi).copied();
                            let Some(ci) = ci else { return };
                            let path = st.splash_charts[ci].path.clone();
                            let _ = std::fs::remove_dir_all(&path);
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
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(s) => {
                state.renderer.resize(s.width, s.height);
                state.overlay.resize(state.renderer.device(), state.renderer.tex_bgl(), state.renderer.sampler(), s.width, s.height);
                state.ui_dirty = true;
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
                    if !state.splash_mode
                        && state.show_properties
                        && state.overlay.selected_tool == 3
                        && state.overlay.mouse_pos.is_some_and(|(mx, _)| {
                            let s = state.overlay.gui_scale;
                            let pp = state.overlay.props_progress();
                            let pan_w = ui::PANEL_W * s;
                            let props_x = state.window.inner_size().width as f32 - pp * pan_w;
                            mx >= props_x && mx <= props_x + pan_w
                        })
                    {
                        state.eff_wheel(dy);
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
                match event.state {
                    ElementState::Pressed if !event.repeat => {
                        if code == KeyCode::ControlLeft || code == KeyCode::ControlRight { state.ctrl = true; }
                        // Splash mode: search box + keyboard chart navigation.
                        if state.splash_mode {
                            if state.show_settings {
                                if code == KeyCode::Escape { state.show_settings = false; }
                                return;
                            }
                            let mut open_path: Option<PathBuf> = None;
                            if state.ctrl && code == KeyCode::KeyQ {
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
                                            let p = state.splash_charts[ci].path.clone();
                                            let _ = std::fs::remove_dir_all(&p);
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
                                drop(state);
                                self.open_chart(event_loop, &path);
                            }
                            return;
                        }
                        // pre-copy fields to avoid borrow conflicts
                        let has_event = state.selected_event_idx.is_some();
                        let edit_target = state.event_edit_target;
                        let snap = state.snap as f64;
                        match code {
                        KeyCode::Escape => {
                            if state.show_menu { state.show_menu = false; }
                            state.selected_event_idx = None;
                            state.ui_dirty = true;
                        }
                        KeyCode::Space => {
                            // Resuming after the track ended: the audio thread
                            // re-appends the source and replays from the start,
                            // but combo/hits/score only reset in `seek` — so a
                            // restart from the end kept the previous run's
                            // stats. Detect the end-of-chart resume and seek 0.
                            let at_end = state.chart_time_last >= state.chart.duration() - 0.1;
                            let paused = state.audio.as_ref().is_some_and(|a| a.is_paused());
                            if paused && at_end {
                                state.seek(0.0);
                            }
                            if let Some(a) = &state.audio {
                                a.set_paused(!paused);
                            }
                        }
                        KeyCode::Delete if state.show_properties && state.overlay.selected_tool == 3 => {
                            state.eff_remove_selected();
                        }
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
                        KeyCode::F3 => { state.show_properties = !state.show_properties; state.ui_dirty = true; }
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
                        // Ctrl+Z = undo, Ctrl+Y = redo
                        KeyCode::KeyZ if state.ctrl => { state.doc.undo(); state.rebuild_chart(); state.ui_dirty = true; }
                        KeyCode::KeyY if state.ctrl => { state.doc.redo(); state.rebuild_chart(); state.ui_dirty = true; }
                        // Ctrl+S = save now
                        KeyCode::KeyS if state.ctrl => {
                            if let Err(e) = state.doc.save() { eprintln!("save failed: {e:#}"); }
                            state.ui_dirty = true;
                        }
                        // Ctrl+Q = back to the splash screen (exit the app
                        // from there with Ctrl+Q again)
                        KeyCode::KeyQ if state.ctrl => {
                            drop(state);
                            self.back_to_splash(event_loop);
                            return;
                        }
                        // Event editing (F2 = cycle target, Ctrl+arrows = edit)
                        KeyCode::F2 if has_event => { state.event_edit_target = (state.event_edit_target + 1) % 5; state.ui_dirty = true; }
                        KeyCode::ArrowLeft if ctrl && has_event => {
                            state.edit_selected_event(|ev| match edit_target {
                                0 => ev.start_time = core::bpm::Triple::from_beats((ev.start_time.beats() - snap).max(0.0)),
                                1 => ev.end_time = core::bpm::Triple::from_beats((ev.end_time.beats() - snap).max(0.0)),
                                _ => {}
                            });
                        }
                        KeyCode::ArrowRight if ctrl && has_event => {
                            state.edit_selected_event(|ev| match edit_target {
                                0 => ev.start_time = core::bpm::Triple::from_beats(ev.start_time.beats() + snap),
                                1 => ev.end_time = core::bpm::Triple::from_beats(ev.end_time.beats() + snap),
                                _ => {}
                            });
                        }
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
                        // Note placement (RPE convention)
                        KeyCode::KeyQ | KeyCode::KeyW | KeyCode::KeyE | KeyCode::KeyR => {
                            let kind: u8 = match code { KeyCode::KeyQ => 1, KeyCode::KeyW => 4, KeyCode::KeyE => 2, KeyCode::KeyR => 3, _ => 1 };
                            use core::model::RPENote;
                            let raw = state.overlay.mouse_beat;
                            let snap = state.snap as f64;
                            let beats = (raw / snap).round() * snap; // snap to grid
                            let note = RPENote {
                                kind, above: 1, start_time: core::bpm::Triple::from_beats(beats),
                                end_time: core::bpm::Triple::from_beats(beats + if kind == 2 { 1.0 } else { 0.0 }),
                                position_x: 0., y_offset: 0., alpha: 255, hitsound: None,
                                size: 1.0, speed: 1.0, is_fake: 0, visible_time: 999999.,
                                tint: None, tint_hit_effects: None, judge_area: None,
                            };
                            if let Err(e) = state.doc.add_note(state.selected_line, note) {
                                eprintln!("add note: {e}");
                            } else {
                                state.rebuild_chart();
                                state.ui_dirty = true;
                            }
                        }
                        _ => {}
                        }
                    },
                    ElementState::Released => {
                        if code == KeyCode::ControlLeft || code == KeyCode::ControlRight {
                            state.ctrl = false;
                            state.overlay.finish_selection();
                        }
                        match code {
                        KeyCode::KeyZ => {
                            let n = state.chart.line_count();
                            if n > 0 { state.selected_line = (state.selected_line + n - 1) % n; state.cache_valid = false; state.ui_dirty = true; }
                        }
                        KeyCode::KeyC => {
                            let n = state.chart.line_count();
                            if n > 0 { state.selected_line = (state.selected_line + 1) % n; state.cache_valid = false; state.ui_dirty = true; }
                        }
                        _ => {}
                        }
                    },
                    _ => {}
                }
            }
            WindowEvent::Focused(f) => { state.focused = f; }
            WindowEvent::CursorMoved { position, .. } => {
                if state.splash_mode {
                    // Splash hover: chart rows / buttons / settings rows.
                    let gs = state.overlay.gui_scale;
                    let vw = state.window.inner_size().width as f32;
                    let vh = state.window.inner_size().height as f32;
                    let (mx, my) = (position.x as f32, position.y as f32);
                    let filtered_len = ui::filter_charts(&state.splash_charts, &state.splash_search, state.splash_sort).len();
                    state.splash_hover = ui::splash_hit_test(mx, my, vw, vh, gs, filtered_len, state.show_settings, state.splash_scroll);
                }
                state.overlay.handle_cursor(position.x, position.y);
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
                // HUD pause button (window px hit-test against last frame).
                if btn_state == ElementState::Pressed {
                    if let Some(m) = state.overlay.mouse_pos {
                        if !state.show_overlay && state.renderer.hit_test_pause(m.0, m.1) {
                            if let Some(a) = &state.audio { a.set_paused(!a.is_paused()); }
                            return;
                        }
                    }
                }
                // Eff panel (tool 3) click handling — releases only, so the
                // press (which the overlay may consume) and the release land
                // in the same spot.
                if btn_state == ElementState::Released
                    && state.show_properties
                    && state.overlay.selected_tool == 3
                {
                    if let Some((mx, my)) = state.overlay.mouse_pos {
                        let gs = state.overlay.gui_scale;
                        let vw = state.window.inner_size().width as f32;
                        let vh = state.window.inner_size().height as f32;
                        let pp = state.overlay.props_progress();
                        let n_rows = state.extra.as_ref().map_or(0, |e| e.effects.len());
                        match ui::effects_hit_test(mx, my, vw, vh, gs, pp, n_rows) {
                            ui::EffHit::List(ri) => { state.selected_effect = Some(ri); state.ui_dirty = true; }
                            ui::EffHit::Add => state.eff_add(),
                            ui::EffHit::Del => state.eff_remove_selected(),
                            ui::EffHit::Field(f) => { state.eff_edit_field = f; state.ui_dirty = true; }
                            ui::EffHit::None => {}
                        }
                    }
                }
                state.overlay.handle_click(btn_state == ElementState::Pressed, state.ctrl);
            }
            WindowEvent::MouseInput { state: btn_state, button: winit::event::MouseButton::Right, .. } => {
                if state.splash_mode { return; }
                if btn_state == ElementState::Pressed {
                    state.overlay.handle_right_click(state.overlay.props_progress());
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
                        ui::OverlayMessage::SelectLayer(ly) => {
                            state.selected_layer = ly;
                            state.cache_valid = false;
                            state.ui_dirty = true;
                        }
                        ui::OverlayMessage::ToggleMenu => { state.show_menu = !state.show_menu; state.ui_dirty = true; }
                        ui::OverlayMessage::MenuSave => {
                            if let Err(e) = state.doc.save() { eprintln!("save failed: {e:#}"); }
                            state.show_menu = false;
                            state.ui_dirty = true;
                        }
                        ui::OverlayMessage::MenuQuit => {
                            drop(state);
                            self.back_to_splash(event_loop);
                            break;
                        }
                        ui::OverlayMessage::ToggleVsync => { state.renderer.set_vsync(!state.renderer.vsync); state.ui_dirty = true; }
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
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
        if let Some(s) = &self.state { s.window.request_redraw(); }
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
        let file_name = std::path::Path::new(&name).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
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

fn scan_charts() -> Vec<ui::ChartEntry> {
    let mut charts = Vec::new();
    let dir = charts_dir();
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    if let Ok(readdir) = std::fs::read_dir(&dir) {
        for e in readdir.flatten() {
            let p = e.path();
            if is_chart_dir(&p) {
                charts.push(read_chart_entry(&p));
            }
        }
    }
    charts.sort_by(|a, b| a.name.cmp(&b.name));
    charts
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

/// Build a splash list entry: metadata + thumbnail (best effort).
fn read_chart_entry(dir: &std::path::Path) -> ui::ChartEntry {
    let folder = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let info = read_chart_info(dir);
    let (name, composer, charter, level, difficulty, illustration) = info.as_ref().map(|i| (
        if i.name.is_empty() { folder.clone() } else { i.name.clone() },
        i.composer.clone(), i.charter.clone(), i.level.clone(), i.difficulty, i.illustration.clone(),
    )).unwrap_or_else(|| (folder, String::new(), String::new(), String::new(), 0.0, String::new()));
    let modified = std::fs::metadata(dir).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs()).unwrap_or(0);
    let thumb = load_thumb(dir, &illustration);
    ui::ChartEntry { name, path: dir.to_path_buf(), composer, charter, level, difficulty, modified, thumb }
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
    el.run_app(&mut App { dir, state: None })?;
    Ok(())
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
}

