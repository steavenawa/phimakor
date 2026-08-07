//! PhiMakor Embedding Engine — surfaceless chart renderer.
//!
//! Other programs (Python, C#, game engines, web backends) can embed this
//! engine to render Phigros charts offscreen. The API is synchronous and
//! purely CPU-blocking: no async, no window, no event loop.
//!
//! # Usage
//!
//! ```no_run
//! # use phimakor::engine::ChartSession;
//! let mut session = ChartSession::new(1280, 720)?;
//! session.load(std::path::Path::new("path/to/chart/dir"))?;
//!
//! // Seek to 30s, get RGBA pixels
//! let pixels = session.render_frame(30.0, 0.5).ok_or_else(|| anyhow::anyhow!("no frame rendered"))?;
//! // pixels is &[u8] length = 1280*720*4, row-major top-down RGBA
//! # Ok::<_, anyhow::Error>(())
//! ```

use crate::core;
use crate::render;
use anyhow::Result;

/// Wraps the full chart lifecycle: load → evaluate → render → readback.
///
/// Internal state machine:
///   1. `new(w, h)` creates a surfaceless GPU context
///   2. `load(dir)` parses chart (RPE/PEC/PGR/PSS), sets up textures/audio
///   3. `render_frame(t, dim)` evaluates chart at time `t`, renders to RGBA
///   4. `seek(t)` / `step(dt)` advance the internal clock
///   5. `pixels()` returns the last rendered frame
pub struct ChartSession {
    engine: render::preview::PreviewEngine,
    chart: Option<core::chart::Chart>,
    info: Option<core::model::ChartInfo>,
    extra: Option<core::extra::ExtraRoot>,
    width: u32,
    height: u32,
    chart_dir: Option<std::path::PathBuf>,
    /// Fired notes from the last `render_frame` call.
    pub last_fired: Vec<core::chart::FiredNote>,
}

impl ChartSession {
    /// Create an offscreen renderer at the given resolution.
    /// Blocks briefly (~100ms) to enumerate GPU adapters.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let engine = pollster::block_on(render::preview::PreviewEngine::new(width, height))?;
        Ok(Self {
            engine,
            chart: None,
            info: None,
            extra: None,
            width,
            height,
            chart_dir: None,
            last_fired: Vec::new(),
        })
    }

    /// Load a chart from a directory (info.json + chart.{json|pec|pgr|pss}).
    /// Loads textures, background image, and (optionally) starts audio playback.
    ///
    /// Supports RPE, PEC, PGR (官谱), and PSS (PhiMakor Streamable Sheet).
    pub fn load(&mut self, dir: &std::path::Path) -> Result<()> {
        let res_dir = std::path::PathBuf::from("res");
        let (info, chart) = core::chart::Chart::load(dir)?;
        self.engine.set_line_length(info.line_length);

        // Load textures referenced by the chart
        for name in chart.textures() {
            if let Ok(bytes) = std::fs::read(dir.join(&name)) {
                self.engine.load_texture(&name, &bytes).ok();
            }
        }
        // Load note/UI sprites from res/
        for kind in ["click", "drag", "flick", "hold", "click_mh", "drag_mh", "flick_mh", "hold_mh", "hit_fx"] {
            let path = res_dir.join(format!("{kind}.png"));
            let key = if kind == "hit_fx" { "note:hitfx".to_string() } else { format!("note:{kind}") };
            if let Ok(bytes) = std::fs::read(&path) {
                self.engine.load_texture(&key, &bytes).ok();
            }
        }
        // Custom textures: chart's Texture2D/ overrides defaults
        let tex_dir = dir.join("Texture2D");
        if tex_dir.is_dir() {
            for (file, suffix) in [("Tap","click"),("Drag","drag"),("Flick","flick"),("Hold","hold")] {
                for ext in [".png",".jpg"] {
                    if let Ok(bytes) = std::fs::read(tex_dir.join(format!("{file}{ext}"))) {
                        self.engine.load_texture(&format!("note:{suffix}"), &bytes).ok();
                        break;
                    }
                }
            }
        }
        // Background
        if let Ok(bytes) = std::fs::read(dir.join(&info.illustration)) {
            self.engine.set_background(&bytes, info.background_dim).ok();
        }

        // Post-processing effects from extra.json
        self.extra = std::fs::read(dir.join("extra.json")).ok()
            .and_then(|b| core::extra::parse_extra(&b).ok());

        self.chart = Some(chart);
        self.info = Some(info);
        self.chart_dir = Some(dir.to_path_buf());
        // 自定义 GLSL 特效:engine 的渲染器需要谱目录才能加载 shader 文件。
        self.engine.set_chart_dir(dir.to_path_buf());
        Ok(())
    }

    /// Evaluate the chart at `time` seconds and render one frame into an
    /// internal RGBA buffer. Returns a slice of the rendered pixels
    /// (row-major, top-down, 4 bytes per pixel).
    ///
    /// `dim` controls how much the background shows through (0 = black, 1 = full).
    /// Returns `None` if no chart is loaded.
    pub fn render_frame(&mut self, time: f64, dim: f32) -> Option<&[u8]> {
        let chart = self.chart.as_mut()?;
        let info = self.info.as_ref()?;
        let duration = chart.duration();
        let off = chart.offset() as f64 + info.offset as f64;
        let chart_time = (time - off).max(0.0);
        let chart_beat = chart.time_to_beat(chart_time);
        // Hit FX: pure time function — query the trigger window BEFORE
        // state_at (which mutably borrows chart; the frame borrows it too).
        let triggers = chart.fx_in_window(chart_time - 0.5, chart_time);
        // 预计算触发瞬间 t0 的线位姿:hit-fx 不绑定当前帧线状态。
        // PMCORE-79:批量接口按 (line, t0) 聚合,和弦只算一次链 set_time。
        let poses: Vec<([f32; 2], f32)> = chart.fx_poses(&triggers);
        let frame = chart.state_at(chart_time);
        self.last_fired.clear();
        self.last_fired.extend(frame.fired.iter().map(|f| core::chart::FiredNote {
            line: f.line, kind: f.kind, x: f.x,
            fake: f.fake, tick: f.tick, hold_tail: f.hold_tail,
        }));
        // Convert trigger points to canvas positions via t0 line transforms.
        // 与 main.rs 同公式:note 偏移不乘 scale(渲染 note_m 只含平移+旋转)。
        {
            let ev_y = 1.5 / (self.width as f32 / self.height.max(1) as f32) as f64;
            let fx: Vec<(f64, [f32; 2])> = triggers.into_iter().zip(poses).map(|(tr, (pos, rot))| {
                let rot = rot as f64;
                let x = tr.x as f64 * 675.0;
                let cx = pos[0] as f64 * 675.0 + rot.cos() * x;
                let cy = pos[1] as f64 * 450.0 * ev_y + rot.sin() * x;
                (tr.t0, [cx as f32, cy as f32])
            }).collect();
            self.engine.set_frame_fx(fx);
        }
        // Evaluate post-processing effects
        if let Some(extra) = &self.extra {
            self.engine.set_effects_from_extra(extra, chart_beat, chart_time as f32);
        }
        self.engine.set_progress((time / duration.max(0.01)) as f32);
        let aspect = self.width as f32 / self.height.max(1) as f32;
        Some(self.engine.render_frame(frame, aspect, dim))
    }

    /// Render one frame, copying pixels into `out` (reused buffer) to avoid
    /// the per-frame allocation of [`render_frame`](Self::render_frame).
    /// Returns the frame bytes (len = w*h*4, row-major top-down RGBA).
    pub fn render_frame_into<'o>(&mut self, time: f64, dim: f32, out: &'o mut Vec<u8>) -> Option<&'o [u8]> {
        if self.chart.is_none() { return None; }
        self.render_frame(time, dim)?;
        Some(self.engine.pixels_reused(out))
    }

    /// Returns the last rendered frame's RGBA pixels (row-major, top-down).
    pub fn pixels(&self) -> &[u8] {
        self.engine.pixels() // PreviewEngine stores pixels in a Vec<u8>
    }

    /// Chart duration in seconds.
    pub fn duration(&self) -> f64 {
        self.chart.as_ref().map(|c| c.duration()).unwrap_or(0.0)
    }

    /// 上一帧场景 pass 的 GPU 耗时(ms)。需 PHIMAKOR_GPU_TIMING=1,
    /// 否则返回 0。阻塞到 GPU 完成——仅诊断用。
    pub fn gpu_frame_ms(&mut self) -> f32 {
        self.engine.gpu_frame_ms()
    }

    /// Total note count (for score calculation).
    pub fn note_count(&self) -> usize {
        self.chart.as_ref().map(|c| c.max_combo()).unwrap_or(0)
    }

    /// Set the Phigros-style HUD (song name / difficulty / score / combo /
    /// pause). Drawn inside the render pipeline, so exported frames include
    /// it; `visible=false` hides it. Pause-button clicks are not interactive
    /// in the embedded engine (hit-testing is window-side).
    pub fn set_hud(&mut self, hud: render::HudData) {
        self.engine.set_hud(hud);
    }

    /// Access the underlying preview renderer for advanced use.
    pub fn engine(&mut self) -> &mut render::preview::PreviewEngine {
        &mut self.engine
    }

    /// Path to the music file (for ffmpeg audio muxing).
    pub fn music_path(&self) -> Option<std::path::PathBuf> {
        let dir = self.chart_dir.as_ref()?;
        let info = self.info.as_ref()?;
        Some(dir.join(&info.music))
    }
}
