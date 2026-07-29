mod audio;
mod core;
mod render;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    charts: Vec<(String, PathBuf)>,
    splash_hover: Option<usize>,
}

struct State {
    window: Arc<Window>,
    chart_dir: PathBuf,
    renderer: render::Renderer,
    overlay: ui::IcedOverlay,
    doc: ChartDocument,
    chart: core::chart::Chart,
    info: core::model::ChartInfo,
    audio: Option<audio::AudioHandle>,
    started: Instant,
    fps_since: Instant,
    aspect_idx: usize,
    show_overlay: bool,
    show_properties: bool,
    show_events: bool,
    show_notes: bool,
    full_notes: bool,
    overlay_last_render: Instant,
    combo: u32,
    hits: u32,
    note_count: usize,
    seek_dim_until: Instant,
    fps: f64,
    frame_latency: f64,
    selected_line: usize,
    selected_event_idx: Option<usize>,
    selected_layer: usize,
    scroll_target: Option<f64>,
    pending_seek: Option<f64>, // if set, use this instead of audio.time() next frame
    focused: bool,
    ctrl: bool,
    gui_scale: f32,
    ui_dirty: bool, // forces iced widget tree rebuild
    layout: LayoutDef,
}

impl State {
    fn placeholder() -> Self { panic!("placeholder State used before init") }
}

impl App {
    fn init(&self, event_loop: &ActiveEventLoop) -> anyhow::Result<State> {
        let dir = self.dir.as_ref().expect("chart dir set");
        let window = Arc::new(event_loop.create_window(
            WindowAttributes::default().with_title("phimakor").with_inner_size(LogicalSize::new(1200.0, 800.0)),
        )?);
        let mut renderer = pollster::block_on(render::Renderer::new(window.clone()))?;
        let res_dir = PathBuf::from("res");

        let doc = ChartDocument::open(dir)?;
        let info = doc.info().clone();
        let chart = core::chart::Chart::from_rpe_chart(doc.chart(), info.use_rpe_170_speed == Some(true))?;

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
        // Custom note textures from chart's Texture2D/
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
                // Also try HL variants (highlight sprites)
            }
        }
        if let Ok(bytes) = std::fs::read(dir.join(&info.illustration)) {
            if let Err(e) = renderer.set_background(&bytes, info.background_dim) { eprintln!("warning: bg: {e:#}"); }
        }

        let audio = audio::spawn_audio_thread(res_dir.as_path(), dir).ok();
        let note_count = chart.max_combo();
        let layout = LayoutDef::load(&res_dir.join("panels.json")).unwrap_or(LayoutDef { panels: vec![] });
        let mut overlay = ui::IcedOverlay::new(renderer.device(), renderer.tex_bgl(), renderer.sampler(), 1200, 800);
        overlay.set_panels(layout.panels.clone());
        Ok(State { window, chart_dir: dir.to_path_buf(), renderer, overlay, doc, chart, info, audio, started: Instant::now(), fps_since: Instant::now(), aspect_idx: 0, show_overlay: true, show_properties: false, show_events: false, show_notes: false, full_notes: false, overlay_last_render: Instant::now(), combo: 0, hits: 0, note_count, seek_dim_until: Instant::now(), fps: 0.0, frame_latency: 0.016, selected_line: 0, selected_event_idx: None, scroll_target: None, pending_seek: None, focused: true, ctrl: false, gui_scale: 1.0, selected_layer: 0, layout, ui_dirty: true })
    }

    /// Rebuild the render Chart from the current document state (after edits).
    fn rebuild_chart(&mut self) {
        if let Some(state) = &mut self.state {
            if let Ok(c) = core::chart::Chart::from_rpe_chart(state.doc.chart(), state.info.use_rpe_170_speed == Some(true)) {
                state.chart = c;
                state.note_count = state.chart.max_combo();
            }
        }
    }
}

impl State {
    fn seek(&mut self, t: f64) {
        let t = t.clamp(0.0, self.chart.duration());
        if let Some(a) = &self.audio { a.seek(t); }
        self.pending_seek = Some(t);
        let off = (self.chart.offset() + self.info.offset) as f64;
        let ct = (t - off).max(0.0);
        self.hits = self.chart.hits_before(ct) as u32;
        self.combo = self.hits;
        self.seek_dim_until = Instant::now() + Duration::from_millis(400);
    }

    fn render_frame(&mut self) {
        if !self.focused { return; }
        // Feed pending seek into scroll target for smooth animation
        if let Some(t) = self.pending_seek.take() {
            self.scroll_target = Some(t);
        }
        let audio_time = match &self.audio {
            Some(a) => a.time(),
            None => self.started.elapsed().as_secs_f64(),
        };
        let audio_time = if let Some(target) = self.scroll_target {
            let d = target - audio_time;
            if d.abs() < 0.008 { self.scroll_target = None; target }
            else {
                let step = d.signum() * (d.abs() * 0.12 + 0.004).min(d.abs().max(0.004));
                let t = (audio_time + step).clamp(0.0, self.chart.duration());
                self.seek(t); t
            }
        } else { audio_time };

        let predict = self.frame_latency.min(0.05);
        let off = (self.chart.offset() + self.info.offset) as f64;
        let chart_time = (audio_time + predict - off).max(0.0);
        let duration = self.chart.duration();
        let chart_beat = self.chart.time_to_beat(chart_time);
        let line_count = self.chart.line_count();
        let line_name = if self.selected_line < line_count { self.chart.line_name(self.selected_line).to_string() } else { "?".to_string() };
        let frame = self.chart.state_at(chart_time);

        for fired in &frame.fired {
            let line = &frame.lines[fired.line];
            let t = line.rotation;
            let x = fired.x as f32;
            let cx = (line.position[0] + t.cos() * x) * 675.0;
            let cy = (line.position[1] + t.sin() * x) * 450.0;
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

        // Extract event data for selected line
        let line_events = extract_line_events(&self.doc, self.selected_line, self.selected_layer);
        let line_notes = if self.full_notes {
            let mut all = Vec::new();
            for li in 0..self.doc.chart().judge_line_list.len() {
                all.extend(extract_line_notes(&self.doc, li));
            }
            all.sort_by(|a, b| a.start_beats.total_cmp(&b.start_beats));
            all
        } else { extract_line_notes(&self.doc, self.selected_line) };
        let max_layers = self.doc.chart().judge_line_list.get(self.selected_line).map_or(1, |l| l.event_layers.len().max(1));

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
            events: line_events,
            notes: line_notes,
            gui_scale: self.gui_scale,
            selected_tool: self.overlay.selected_tool,
            chart_beat,
            events_progress: self.overlay.render_progress().0,
            notes_progress: self.overlay.render_progress().1,
            has_custom_tex: self.chart_dir.join("Texture2D").is_dir(),
            full_notes: self.full_notes,
            chart_name: self.info.name.clone(),
            composer: self.info.composer.clone(),
            level: self.info.level.clone(),
            difficulty: self.info.difficulty,
            offset: self.info.offset,
            duration,
        };
        if self.show_overlay {
            if let Some(ev_idx) = self.overlay.take_timeline_click(&info, self.overlay.props_progress()) {
                self.selected_event_idx = Some(ev_idx); self.ui_dirty = true;
            }
            if let Some(ly) = self.overlay.take_layer_click(self.overlay.props_progress(), max_layers) {
                self.selected_layer = ly; self.ui_dirty = true;
            }
            self.overlay.render_iced(self.renderer.queue(), &info);
        }
        let ui_bg = if self.show_overlay { Some(self.overlay.bind_group()) } else { None };

        self.renderer.set_progress(audio_time as f32 / duration as f32);
        match self.renderer.surface_acquire() {
            Ok(st) => {
                let view = st.texture.create_view(&wgpu::TextureViewDescriptor::default());
                self.renderer.draw_to_view(&view, frame, aspect, dim, ui_bg);
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
    out.sort_by(|a, b| a.start_beats.total_cmp(&b.start_beats));
    out
}

fn extract_line_notes(doc: &ChartDocument, line: usize) -> Vec<ui::NoteEntry> {
    let rpe = doc.chart();
    let Some(jl) = rpe.judge_line_list.get(line) else { return vec![] };
    let Some(notes) = &jl.notes else { return vec![] };
    notes.iter().enumerate().map(|(i, n)| ui::NoteEntry {
        index: i,
        kind: n.kind,
        start_beats: n.start_time.beats(),
        end_beats: n.end_time.beats(),
        x: n.position_x,
        speed: n.speed,
        scale: n.size,
        texture: n.hitsound.clone().unwrap_or_default(),
    }).collect()
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() { return; }
        match self.init(event_loop) {
            Ok(s) => self.state = Some(s),
            Err(e) => { eprintln!("{e:#}"); event_loop.exit(); }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(s) => {
                state.renderer.resize(s.width, s.height);
                state.overlay.resize(state.renderer.device(), state.renderer.tex_bgl(), state.renderer.sampler(), s.width, s.height);
                state.render_frame();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let notch = match delta { MouseScrollDelta::LineDelta(_, y) => y, _ => 0.0 };
                if notch == 0.0 { return; }
                // Check if mouse is over timeline panel
                if (state.show_events || state.show_notes) && state.overlay.is_over_timeline(state.overlay.props_progress()) {
                    if state.ctrl {
                        state.overlay.timeline_zoom_in(notch);
                    } else if state.overlay.mouse_pos.map_or(false, |(_, my)| my >= 28.0) {
                        state.overlay.timeline_scroll(notch);
                    }
                } else {
                    let t = state.audio.as_ref().map(|a| a.time()).unwrap_or(0.0) + notch as f64 * -0.5;
                    state.scroll_target = Some(t.clamp(0.0, state.chart.duration()));
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                match event.state {
                    ElementState::Pressed if !event.repeat => {
                        if code == KeyCode::ControlLeft || code == KeyCode::ControlRight { state.ctrl = true; }
                        match code {
                        KeyCode::Escape => event_loop.exit(),
                        KeyCode::Space => { if let Some(a) = &state.audio { a.set_paused(!a.is_paused()); } }
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
                        KeyCode::F5 => { if state.ctrl { state.full_notes = !state.full_notes; } else { state.show_notes = !state.show_notes; } state.ui_dirty = true; }
                        KeyCode::BracketLeft => { state.gui_scale = (state.gui_scale - 0.1).max(0.5); state.ui_dirty = true; }
                        KeyCode::BracketRight => { state.gui_scale = (state.gui_scale + 0.1).min(2.0); state.ui_dirty = true; }
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
                            if n > 0 { state.selected_line = (state.selected_line + n - 1) % n; state.ui_dirty = true; }
                        }
                        KeyCode::KeyC => {
                            let n = state.chart.line_count();
                            if n > 0 { state.selected_line = (state.selected_line + 1) % n; state.ui_dirty = true; }
                        }
                        _ => {}
                        }
                    },
                    _ => {}
                }
            }
            WindowEvent::Focused(f) => { state.focused = f; }
            WindowEvent::CursorMoved { position, .. } => {
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
                state.overlay.handle_click(btn_state == ElementState::Pressed, state.ctrl);
            }
            WindowEvent::MouseInput { state: btn_state, button: winit::event::MouseButton::Right, .. } => {
                if btn_state == ElementState::Pressed {
                    state.overlay.handle_right_click(state.overlay.props_progress());
                }
            }
            WindowEvent::RedrawRequested => {
                state.render_frame();
                for msg in state.overlay.messages.drain(..) {
                    match msg {
                        ui::OverlayMessage::ToggleEvents => { state.show_events = !state.show_events; state.ui_dirty = true; }
                        ui::OverlayMessage::SelectLayer(ly) => {
                            if ly == 666 { state.show_notes = !state.show_notes; state.ui_dirty = true; }
                            else { state.selected_layer = ly; state.ui_dirty = true; }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) { if let Some(s) = &self.state { s.window.request_redraw(); } }
    fn exiting(&mut self, _: &ActiveEventLoop) {
        if let Some(s) = &self.state { if let Some(a) = &s.audio { a.quit(); } }
    }
}

fn scan_charts() -> Vec<(String, PathBuf)> {
    let mut charts = Vec::new();
    if let Ok(readdir) = std::fs::read_dir(".") {
        for e in readdir.flatten() {
            if e.path().is_dir() && (e.path().join("info.json").exists() || e.path().join("info.txt").exists()) {
                charts.push((e.file_name().to_string_lossy().to_string(), e.path()));
            }
        }
    }
    charts.sort_by(|a, b| a.0.cmp(&b.0));
    charts
}

fn main() -> anyhow::Result<()> {
    let dir = match std::env::args_os().nth(1) {
        Some(d) => Some(PathBuf::from(d)),
        None => None,
    };
    let el = EventLoop::new()?;
    el.set_control_flow(ControlFlow::Poll);
    let charts = scan_charts();
    el.run_app(&mut App { dir, state: None, charts, splash_hover: None })?;
    Ok(())
}
