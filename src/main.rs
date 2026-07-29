mod audio;
mod core;
mod render;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes, WindowId};

struct App {
    dir: PathBuf,
    state: Option<State>,
    fps_tx: std::sync::mpsc::SyncSender<f64>,
}

struct State {
    window: Arc<Window>,
    renderer: render::Renderer,
    overlay: ui::IcedOverlay,
    chart: core::chart::Chart,
    info: core::model::ChartInfo,
    audio: Option<audio::AudioHandle>,
    started: Instant,
    fps_frames: u32,
    fps_since: Instant,
    aspect_idx: usize,
    show_overlay: bool,
    overlay_last_render: Instant,
    fps_tx: std::sync::mpsc::SyncSender<f64>,
    combo: u32,
    hits: u32,
    note_count: usize,
    seek_dim_until: Instant,
    fps: f64,
}

impl App {
    fn init(&self, event_loop: &ActiveEventLoop) -> anyhow::Result<State> {
        let window = Arc::new(event_loop.create_window(
            WindowAttributes::default()
                .with_title("phimakor")
                .with_inner_size(LogicalSize::new(1200.0, 800.0)),
        )?);
        let mut renderer = pollster::block_on(render::Renderer::new(window.clone()))?;
        let (info, chart) = core::chart::Chart::load(&self.dir)?;

        for name in chart.textures() {
            if let Ok(bytes) = std::fs::read(self.dir.join(&name)) {
                if let Err(e) = renderer.load_texture(&name, &bytes) {
                    eprintln!("warning: texture {name}: {e:#}");
                }
            }
        }

        let res_dir = PathBuf::from("res");
        for kind in ["click", "drag", "flick", "hold", "click_mh", "drag_mh", "flick_mh", "hold_mh", "hit_fx"] {
            let path = res_dir.join(format!("{kind}.png"));
            let key = if kind == "hit_fx" { "note:hitfx".to_string() } else { format!("note:{kind}") };
            if let Ok(bytes) = std::fs::read(&path) {
                if let Err(e) = renderer.load_texture(&key, &bytes) {
                    eprintln!("warning: note sprite {kind}: {e:#}");
                }
            }
        }
        if let Ok(bytes) = std::fs::read(self.dir.join(&info.illustration)) {
            if let Err(e) = renderer.set_background(&bytes, info.background_dim) {
                eprintln!("warning: bg: {e:#}");
            }
        }

        let audio = audio::spawn_audio_thread(res_dir.as_path(), &self.dir).ok();
        let note_count = chart.max_combo();
        let overlay = ui::IcedOverlay::new(renderer.device(), renderer.tex_bgl(), renderer.sampler(), 1200, 800);

        Ok(State { window, renderer, overlay, chart, info, audio, started: Instant::now(), fps_frames: 0, fps_since: Instant::now(), aspect_idx: 0, show_overlay: false, overlay_last_render: Instant::now(), fps_tx: self.fps_tx.clone(), combo: 0, hits: 0, note_count, seek_dim_until: Instant::now(), fps: 0.0 })
    }
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
                state.overlay.resize(
                    state.renderer.device(),
                    state.renderer.tex_bgl(),
                    state.renderer.sampler(),
                    s.width,
                    s.height,
                );
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                match code {
                    KeyCode::Escape => event_loop.exit(),
                    KeyCode::Space => { if let Some(a) = &state.audio { a.set_paused(!a.is_paused()); } }
                    KeyCode::ArrowLeft | KeyCode::ArrowRight => {
                        if let Some(a) = &state.audio {
                            let d = if code == KeyCode::ArrowLeft { -5.0 } else { 5.0 };
                            let t = (a.time() + d).max(0.0); a.seek(t);
                            let off = (state.chart.offset() + state.info.offset) as f64;
                            let ct = (t - off).max(0.0);
                            let h = state.chart.hits_before(ct) as u32;
                            state.hits = h; state.combo = h;
                            state.seek_dim_until = Instant::now() + Duration::from_millis(400);
                        }
                    }
                    KeyCode::Tab => {
                        const ASPECTS: [f32; 4] = [3.0 / 2.0, 16.0 / 9.0, 4.0 / 3.0, 1.0];
                        state.aspect_idx = (state.aspect_idx + 1) % ASPECTS.len();
                        state.renderer.set_playfield_aspect(ASPECTS[state.aspect_idx]);
                    }
                    KeyCode::F3 => {
                        state.show_overlay = !state.show_overlay;
                    }
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => {
                let audio_time = match &state.audio {
                    Some(a) => a.time(),
                    None => state.started.elapsed().as_secs_f64(),
                };
                let off = (state.chart.offset() + state.info.offset) as f64;
                let chart_time = (audio_time - off).max(0.0);
                let duration = state.chart.duration();
                let frame = state.chart.state_at(chart_time);

                for fired in &frame.fired {
                    let line = &frame.lines[fired.line];
                    let t = line.rotation;
                    let x = fired.x as f32;
                    let cx = (line.position[0] + t.cos() * x) * 675.0;
                    let cy = (line.position[1] + t.sin() * x) * 450.0;
                    if fired.hold_tail { if !fired.fake { state.combo += 1; state.hits += 1; } continue; }
                    if fired.tick { state.renderer.spawn_hit_fx([cx, cy]); continue; }
                    if fired.fake { continue; }
                    state.combo += 1; state.hits += 1;
                    state.renderer.spawn_hit_fx([cx, cy]);
                }

                let size = state.window.inner_size();
                let aspect = size.width as f32 / size.height.max(1) as f32;
                let dim = if Instant::now() < state.seek_dim_until { 0.7 } else { 1.0 };
                let score = (state.hits as f64 / state.note_count.max(1) as f64 * 1_000_000.).round() as u32;
                let visible_notes: usize = frame.lines.iter().map(|l| l.notes.len()).sum();

                // Build Iced UI overlay
                let info = ui::GameInfo {
                    chart_time, audio_time, fps: state.fps, combo: state.combo,
                    hits: state.hits, note_count: state.note_count, score,
                    lines: frame.lines.len(), visible_notes,
                    paused: state.audio.as_ref().is_some_and(|a| a.is_paused()),
                    dim,
                };
                // Rebuild the overlay at ~10 Hz (debug text needs no more);
                // between updates the previous texture is reused as-is.
                let ui_bg = if state.show_overlay {
                    if state.overlay_last_render.elapsed().as_millis() >= 100 {
                        state.overlay.render(state.renderer.queue(), &info);
                        state.overlay_last_render = Instant::now();
                    }
                    Some(state.overlay.bind_group())
                } else {
                    None
                };

                state.renderer.set_progress(audio_time as f32 / duration as f32);
                match state.renderer.surface_acquire() {
                    Ok(st) => {
                        let view = st.texture.create_view(&wgpu::TextureViewDescriptor::default());
                        state.renderer.draw_to_view(&view, frame, aspect, dim, ui_bg);
                        state.renderer.queue().present(st);
                    }
                    _ => {}
                }

                state.fps_frames += 1;
                let elapsed = state.fps_since.elapsed();
                if elapsed.as_secs() >= 5 {
                    state.fps = state.fps_frames as f64 / elapsed.as_secs_f64();
                    let _ = state.fps_tx.try_send(state.fps);
                    state.fps_frames = 0;
                    state.fps_since = Instant::now();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        if let Some(s) = &self.state { s.window.request_redraw(); }
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        if let Some(s) = &self.state { if let Some(a) = &s.audio { a.quit(); } }
    }
}

fn main() -> anyhow::Result<()> {
    let (fps_tx, fps_rx) = std::sync::mpsc::sync_channel(4);
    std::thread::spawn(move || { while let Ok(f) = fps_rx.recv() { eprintln!("fps: {f:.1}"); } });
    let Some(dir) = std::env::args_os().nth(1) else { eprintln!("usage: phimakor <chart-dir>"); std::process::exit(2); };
    let el = EventLoop::new()?;
    el.set_control_flow(ControlFlow::Poll);
    el.run_app(&mut App { dir: PathBuf::from(dir), state: None, fps_tx })?;
    Ok(())
}
