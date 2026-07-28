//! Separate CPU-rendered debug window (softbuffer + fontdue), independent of
//! the wgpu render path. Black background, white text, 16px font, 20px rows.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use softbuffer::{Context, Surface};
use winit::dpi::LogicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes, WindowId};

const FONT_PX: f32 = 16.0;
const ROW_H: usize = 20;
const PAD_X: f32 = 8.0;
const UPDATE_INTERVAL: Duration = Duration::from_millis(100); // ~10 fps

// Contract originally had DebugInfo<'a>; all fields are scalars so no lifetime.
pub struct DebugInfo {
    pub chart_time: f64,
    pub audio_time: f64,
    pub fps: f64,
    pub combo: u32,
    pub hits: u32,
    pub note_count: usize,
    pub lines: usize,
    pub visible_notes: usize,
    pub fired: usize,
    pub paused: bool,
    pub dim: f32,
    pub window_size: (u32, u32),
}

pub struct DebugWindow {
    window: Arc<Window>,
    // verify: softbuffer 0.4 Surface<D, W> owns Arc clones of the handles
    surface: Surface<Arc<Window>, Arc<Window>>,
    font: Option<fontdue::Font>,
    last_update: Instant,
}

fn load_font() -> Option<fontdue::Font> {
    for path in [r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\arial.ttf"] {
        if let Ok(bytes) = std::fs::read(path) {
            match fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                Ok(font) => return Some(font),
                Err(e) => eprintln!("warning: fontdue cannot parse {path}: {e}"),
            }
        }
    }
    eprintln!("warning: no usable font for debug window, text disabled");
    None
}

impl DebugWindow {
    /// Any failure (window creation, surface, …) yields None silently-ish.
    pub fn new(event_loop: &ActiveEventLoop) -> Option<Self> {
        let window = Arc::new(
            event_loop
                .create_window(
                    WindowAttributes::default()
                        .with_title("phimakor debug")
                        .with_inner_size(LogicalSize::new(500.0, 400.0)),
                )
                .map_err(|e| eprintln!("warning: debug window creation failed: {e}"))
                .ok()?,
        );
        // verify: softbuffer 0.4 Context::new(window.clone()) / Surface::new(&context, window.clone());
        // Arc<Window> satisfies HasWindowHandle + HasDisplayHandle via raw-window-handle blanket impls.
        let context = Context::new(window.clone())
            .map_err(|e| eprintln!("warning: softbuffer context failed: {e}"))
            .ok()?;
        let surface = Surface::new(&context, window.clone())
            .map_err(|e| eprintln!("warning: softbuffer surface failed: {e}"))
            .ok()?;
        Some(Self {
            window,
            surface,
            font: load_font(),
            last_update: Instant::now() - UPDATE_INTERVAL,
        })
    }

    /// Rasterize and present; rate-limited internally to ~10 fps.
    pub fn update(&mut self, info: &DebugInfo) {
        if self.last_update.elapsed() < UPDATE_INTERVAL {
            return;
        }
        self.last_update = Instant::now();

        let size = self.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if self.surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = self.surface.buffer_mut() else { return };

        // Black background (0x00RRGGBB).
        buffer.fill(0);

        if let Some(font) = &self.font {
            let (width, height) = (size.width as usize, size.height as usize);
            let lines = [
                format!("time   {:8.3} (audio {:8.3})", info.chart_time, info.audio_time),
                format!("fps    {:.1}", info.fps),
                format!("combo  {}", info.combo),
                format!("hits   {}/{}", info.hits, info.note_count),
                format!("lines  {}", info.lines),
                format!("notes  {}", info.visible_notes),
                format!("fired  {}", info.fired),
                format!("paused {}", info.paused),
                format!("dim    {:.2}", info.dim),
                format!("window {}x{}", info.window_size.0, info.window_size.1),
            ];
            for (row, line) in lines.iter().enumerate() {
                let baseline = (row * ROW_H + ROW_H - 4) as i32;
                let mut pen_x = PAD_X;
                for ch in line.chars() {
                    let (metrics, bitmap) = font.rasterize(ch, FONT_PX);
                    let glyph_x = pen_x.round() as i32 + metrics.xmin;
                    // verify: fontdue bitmap bottom sits at baseline - ymin,
                    // so top = baseline - ymin - height (screen y grows down).
                    let glyph_y = baseline - metrics.ymin - metrics.height as i32;
                    for gy in 0..metrics.height {
                        let py = glyph_y + gy as i32;
                        if py < 0 || py as usize >= height {
                            continue;
                        }
                        for gx in 0..metrics.width {
                            let v = bitmap[gy * metrics.width + gx] as u32;
                            if v == 0 {
                                continue;
                            }
                            let px = glyph_x + gx as i32;
                            if px < 0 || px as usize >= width {
                                continue;
                            }
                            // Coverage over black background = grayscale white.
                            buffer[py as usize * width + px as usize] = (v << 16) | (v << 8) | v;
                        }
                    }
                    pen_x += metrics.advance_width;
                }
            }
        }

        let _ = buffer.present();
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }
}
