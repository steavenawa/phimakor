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
//! session.load("path/to/chart/dir")?;
//!
//! // Seek to 30s, get RGBA pixels
//! let pixels = session.render_frame(30.0, 0.5)?;
//! // pixels is &[u8] length = 1280*720*4, row-major top-down RGBA
//! # Ok::<_, anyhow::Error>(())
//! ```

use crate::core;
use crate::render;
use crate::audio;
use anyhow::{Context, Result};

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
    audio: Option<audio::AudioHandle>,
    width: u32,
    height: u32,
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
            audio: None,
            width,
            height,
        })
    }

    /// Load a chart from a directory (info.json + chart.{json|pec|pgr|pss}).
    /// Loads textures, background image, and (optionally) starts audio playback.
    ///
    /// Supports RPE, PEC, PGR (官谱), and PSS (PhiMakor Streamable Sheet).
    pub fn load(&mut self, dir: &std::path::Path) -> Result<()> {
        let res_dir = std::path::PathBuf::from("res");
        let (info, chart) = core::chart::Chart::load(dir)?;

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

        self.chart = Some(chart);
        self.info = Some(info);
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
        let duration = chart.duration();
        let frame = chart.state_at(time);
        self.engine.set_progress((time / duration.max(0.01)) as f32);
        let aspect = self.width as f32 / self.height.max(1) as f32;
        Some(self.engine.render_frame(frame, aspect, dim))
    }

    /// Returns the last rendered frame's RGBA pixels (row-major, top-down).
    pub fn pixels(&self) -> &[u8] {
        self.engine.pixels() // PreviewEngine stores pixels in a Vec<u8>
    }

    /// Chart duration in seconds.
    pub fn duration(&self) -> f64 {
        self.chart.as_ref().map(|c| c.duration()).unwrap_or(0.0)
    }

    /// Total note count (for score calculation).
    pub fn note_count(&self) -> usize {
        self.chart.as_ref().map(|c| c.max_combo()).unwrap_or(0)
    }

    /// Access the underlying preview renderer for advanced use.
    pub fn engine(&mut self) -> &mut render::preview::PreviewEngine {
        &mut self.engine
    }
}
