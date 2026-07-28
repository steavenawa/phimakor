//! phimakor M0 preview: play an RPE chart with audio.

mod audio;
mod core;
mod debug;
mod render;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes, WindowId};

const COMBO_LABEL: &str = "combo";

struct App {
    dir: PathBuf,
    state: Option<State>,
    fps_tx: std::sync::mpsc::SyncSender<f64>,
}

struct State {
    window: Arc<Window>,
    renderer: render::Renderer,
    chart: core::chart::Chart,
    info: core::model::ChartInfo,
    audio: Option<audio::AudioHandle>,
    /// Fallback clock when the music file is missing.
    // ponytail: silent preview ignores pause/seek keys
    started: Instant,
    fps_frames: u32,
    fps_since: Instant,
    aspect_idx: usize,
    fps_tx: std::sync::mpsc::SyncSender<f64>,
    combo: u32,
    hits: u32,
    /// Cached at init (Chart::max_combo); avoids borrowing chart while the
    /// frame borrow is live.
    note_count: usize,
    seek_dim_until: Instant,
    debug: Option<debug::DebugWindow>,
    fps: f64,
}

impl App {
    fn init(&self, event_loop: &ActiveEventLoop) -> anyhow::Result<State> {
        let window = Arc::new(event_loop.create_window(
            WindowAttributes::default()
                .with_title("phimakor preview")
                .with_inner_size(LogicalSize::new(1200.0, 800.0)),
        )?);
        let mut renderer = pollster::block_on(render::Renderer::new(window.clone()))?;
        let (info, chart) = core::chart::Chart::load(&self.dir)?;

        for name in chart.textures() {
            match std::fs::read(self.dir.join(&name)) {
                Ok(bytes) => {
                    if let Err(e) = renderer.load_texture(&name, &bytes) {
                        eprintln!("warning: failed to upload texture {name}: {e:#}");
                    }
                }
                Err(e) => eprintln!("warning: cannot read texture {name}: {e}"),
            }
        }

        // Resource pack: `res/` under the process CWD.
        let res_dir = PathBuf::from("res");
        if let Ok(font) = std::fs::read(res_dir.join("Exo2.ttf")) {
            if !renderer.set_font(font) {
                eprintln!("warning: Exo2.ttf is not a usable font, system fallback");
            }
        }
        for kind in ["click", "drag", "flick", "hold", "click_mh", "drag_mh", "flick_mh", "hold_mh"] {
            let path = res_dir.join(format!("{kind}.png"));
            match std::fs::read(&path) {
                Ok(bytes) => {
                    if let Err(e) = renderer.load_texture(&format!("note:{kind}"), &bytes) {
                        eprintln!("warning: failed to upload note sprite {kind}: {e:#}");
                    }
                }
                Err(_) => eprintln!("warning: no note sprite {:?}, colored quad fallback", path),
            }
        }
        let path = res_dir.join("hit_fx.png");
        match std::fs::read(&path) {
            Ok(bytes) => {
                if let Err(e) = renderer.load_texture("note:hitfx", &bytes) {
                    eprintln!("warning: failed to upload hit fx sprite: {e:#}");
                }
            }
            Err(_) => eprintln!("warning: no hit fx sprite {:?}, colored quad fallback", path),
        }

        match std::fs::read(self.dir.join(&info.illustration)) {
            Ok(bytes) => {
                if let Err(e) = renderer.set_background(&bytes, info.background_dim) {
                    eprintln!("warning: failed to set background: {e:#}");
                }
            }
            Err(e) => eprintln!("warning: cannot read illustration {:?}: {e}", info.illustration),
        }

        // Hitsounds fire on a dedicated trigger thread so an occluded window
        // (winit stops RedrawRequested) can't delay them.
        let audio = match audio::spawn_audio_thread(res_dir.as_path(), &self.dir) {
            Ok(a) => Some(a),
            Err(e) => {
                eprintln!("WARNING: cannot play music {:?}: {e:#} — preview runs SILENT", info.music);
                None
            }
        };

        let note_count = chart.max_combo();
        let debug_window = debug::DebugWindow::new(event_loop);
        Ok(State { window, renderer, chart, info, audio, started: Instant::now(), fps_frames: 0, fps_since: Instant::now(), aspect_idx: 0, fps_tx: self.fps_tx.clone(), combo: 0, hits: 0, note_count, seek_dim_until: Instant::now(), debug: debug_window, fps: 0.0 })
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        match self.init(event_loop) {
            Ok(state) => self.state = Some(state),
            Err(e) => {
                eprintln!("failed to start: {e:#}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else { return };
        if state.debug.as_ref().is_some_and(|d| d.id() == id) {
            // Debug window: ignore everything (incl. CloseRequested) — the
            // main window owns the app lifecycle.
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => state.renderer.resize(size.width, size.height),
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                match code {
                    KeyCode::Escape => event_loop.exit(),
                    KeyCode::Space => {
                        if let Some(audio) = &state.audio {
                            audio.set_paused(!audio.is_paused());
                        }
                    }
                    KeyCode::ArrowLeft | KeyCode::ArrowRight => {
                        if let Some(audio) = &state.audio {
                            let delta = if code == KeyCode::ArrowLeft { -5.0 } else { 5.0 };
                            let new_audio = (audio.time() + delta).max(0.0);
                            audio.seek(new_audio);
                            // Recompute score state for the seek target
                            // (auto-play = all Perfect, so combo == hits).
                            let total_offset = (state.chart.offset() + state.info.offset) as f64;
                            let new_chart_time = (new_audio - total_offset).max(0.0);
                            let hits = state.chart.hits_before(new_chart_time) as u32;
                            state.hits = hits;
                            state.combo = hits;
                            state.seek_dim_until = Instant::now() + Duration::from_millis(400);
                        }
                    }
                    KeyCode::Tab => {
                        // Cycle playfield aspect (editor preview convention).
                        const ASPECTS: [f32; 4] = [3.0 / 2.0, 16.0 / 9.0, 4.0 / 3.0, 1.0];
                        state.aspect_idx = (state.aspect_idx + 1) % ASPECTS.len();
                        let a = ASPECTS[state.aspect_idx];
                        state.renderer.set_playfield_aspect(a);
                        eprintln!("playfield aspect: {a:.4}");
                    }
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => {
                let audio_time = match &state.audio {
                    Some(audio) => audio.time(),
                    None => state.started.elapsed().as_secs_f64(),
                };
                // prpr scene/game.rs: chart time lags audio by the total offset.
                let total_offset = (state.chart.offset() + state.info.offset) as f64;
                let chart_time = (audio_time - total_offset).max(0.0);
                let frame = state.chart.state_at(chart_time);
                // Hit effects + score for notes fired since last frame
                // (core reports crossings, incl. culled notes and hold re-hits).
                // Hitsounds are the trigger thread's job, not this loop's.
                for fired in &frame.fired {
                    let line = &frame.lines[fired.line];
                    let theta = line.rotation;
                    let x = fired.x as f32; // FiredNote.x is f64 in core
                    let cx = (line.position[0] + theta.cos() * x) * 675.0;
                    let cy = (line.position[1] + theta.sin() * x) * 450.0;
                    if fired.hold_tail {
                        // Hold tail: judge-only event, silent and effect-less.
                        if !fired.fake {
                            state.combo += 1;
                            state.hits += 1;
                        }
                        continue;
                    }
                    if fired.tick {
                        // Mid-hold tick: visual pulse only, hold stays muted.
                        state.renderer.spawn_hit_fx([cx, cy]);
                        continue;
                    }
                    if fired.fake {
                        continue;
                    }
                    state.combo += 1;
                    state.hits += 1;
                    state.renderer.spawn_hit_fx([cx, cy]);
                }
                let size = state.window.inner_size();
                let aspect = size.width as f32 / size.height.max(1) as f32;
                let dim = if Instant::now() < state.seek_dim_until { 0.7 } else { 1.0 };
                // Game UI (auto-play = all Perfect).
                const UI_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.85];
                let score =
                    (state.hits as f64 / state.note_count.max(1) as f64 * 1_000_000.).round() as u32;
                state.renderer.draw_text(&format!("{score:07}"), render::TextAnchor::TopLeft, UI_COLOR);
                if state.combo >= 3 {
                    state.renderer.draw_text(&state.combo.to_string(), render::TextAnchor::TopCenter, UI_COLOR);
                    state.renderer.draw_text(COMBO_LABEL, render::TextAnchor::ComboLabel, UI_COLOR);
                }
                state.renderer.draw_text(&state.info.name, render::TextAnchor::BottomLeftEdge, UI_COLOR);
                state.renderer.draw_text(&state.info.level, render::TextAnchor::BottomRightEdge, UI_COLOR);
                if let Err(e) = state.renderer.render(frame, aspect, dim) {
                    eprintln!("render error: {e:?}");
                }
                if let Some(dbg) = &mut state.debug {
                    let visible_notes: usize = frame.lines.iter().map(|l| l.notes.len()).sum();
                    dbg.update(&debug::DebugInfo {
                        chart_time,
                        audio_time,
                        fps: state.fps,
                        combo: state.combo,
                        hits: state.hits,
                        note_count: state.note_count,
                        lines: frame.lines.len(),
                        visible_notes,
                        fired: frame.fired.len(),
                        paused: state.audio.as_ref().is_some_and(|a| a.is_paused()),
                        dim,
                        window_size: (size.width, size.height),
                    });
                }
                state.fps_frames += 1;
                let elapsed = state.fps_since.elapsed();
                if elapsed.as_secs() >= 5 {
                    state.fps = state.fps_frames as f64 / elapsed.as_secs_f64();
                    // Never block the render loop on console IO: drop if the
                    // printer thread is behind.
                    let _ = state.fps_tx.try_send(state.fps);
                    state.fps_frames = 0;
                    state.fps_since = Instant::now();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = &self.state {
            // ponytail: vsync present mode is enough throttle
            state.window.request_redraw();
            if let Some(dbg) = &state.debug {
                dbg.request_redraw();
            }
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = &self.state {
            if let Some(audio) = &state.audio {
                audio.quit();
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Background printer: console writes can block on Windows, keep them off
    // the render loop (bounded channel, try_send drops when full).
    let (fps_tx, fps_rx) = std::sync::mpsc::sync_channel::<f64>(4);
    std::thread::spawn(move || {
        while let Ok(fps) = fps_rx.recv() {
            eprintln!("fps: {fps:.1}");
        }
    });
    let Some(dir) = std::env::args_os().nth(1) else {
        eprintln!("usage: phimakor <chart-dir>");
        std::process::exit(2);
    };
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App { dir: PathBuf::from(dir), state: None, fps_tx };
    event_loop.run_app(&mut app)?;
    Ok(())
}
