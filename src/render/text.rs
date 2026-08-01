//! Text overlay rendering (Phaser UI): fontdue-rasterized single-line
//! sprites, cached by `(text, anchor)` and drawn through the quad pipeline.
//!
//! All positions are canvas pixels (1350×900, center origin, y down).
//! [`Renderer::draw_text`] rasterizes + caches + queues; `render()` flushes
//! the queue via [`push_text`] at the end of the frame, on top of everything.

use std::collections::HashMap;

use super::{mat_mul, mat_rotate, mat_scale, mat_translate, DrawCmd, DrawUniform, Mat3, Renderer};

/// Anchor presets for [`Renderer::draw_text`]; heights are rasterization px.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextAnchor {
    /// Left edge -655, top -430, 36 px.
    TopLeft,
    /// Horizontally centered, top -440, 56 px.
    TopCenter,
    /// Left edge -655, bottom edge 430, 30 px.
    BottomLeft,
    /// Right edge 655, bottom edge 430, 30 px.
    BottomRight,
    /// Right edge 655, top 8 px, 36 px (score).
    TopRight,
    /// Hug the VIEWPORT bottom-left (window px space, not canvas), 30 px.
    BottomLeftEdge,
    /// Hug the VIEWPORT bottom-right (window px space, not canvas), 30 px.
    BottomRightEdge,
    /// Combo caption directly below the TopCenter number, centered, 26 px.
    ComboLabel,
    /// Free-positioned world text (attachUI labels): 28 px, centered at the
    /// quad position, rotated with the line.
    World,
}

#[derive(Clone, Copy)]
enum HAlign {
    Left(f32),
    Center,
    Right(f32),
}

impl TextAnchor {
    /// (rasterization px, horizontal alignment, line-box top y).
    /// NOTE: canvas +y renders as screen UP in the current letterbox, so the
    /// "Top" anchors use +y and the "Bottom" anchors -y. The visible canvas
    /// y-range is ±(675/aspect) — the playfield BOX edges (what "bar" aligns
    /// to), not the fixed canvas top ±450. 450 only equals the box edge at
    /// the 3:2 design aspect; wide aspects would push the HUD out of the box.
    fn layout(self, aspect: f32) -> (f32, HAlign, f32) {
        let top = 675.0 / aspect;
        let bottom = -675.0 / aspect;
        match self {
            Self::TopLeft => (36.0, HAlign::Left(-655.0), top - 8.0 - 36.0),
            Self::TopCenter => (56.0, HAlign::Center, top - 8.0 - 56.0),
            Self::BottomLeft => (30.0, HAlign::Left(-655.0), bottom + 6.0),
            Self::BottomRight => (30.0, HAlign::Right(655.0), bottom + 6.0),
            Self::TopRight => (36.0, HAlign::Right(655.0), top - 8.0 - 36.0),
            // Viewport edges: y resolved in push_text from the window size.
            Self::BottomLeftEdge => (30.0, HAlign::Left(-655.0), bottom),
            Self::BottomRightEdge => (30.0, HAlign::Right(655.0), bottom),
            // Directly below TopCenter on screen (+y = up): 6 px gap, 26 px.
            Self::ComboLabel => (26.0, HAlign::Center, top - 2.0 - 56.0 - 6.0 - 26.0),
            Self::World => (28.0, HAlign::Center, 0.0),
        }
    }

    fn is_viewport_edge(self) -> bool {
        matches!(self, Self::BottomLeftEdge | Self::BottomRightEdge)
    }
}

pub(crate) struct CachedText {
    bind_group: wgpu::BindGroup,
    /// Line size in canvas px (width × em-box height).
    size: [f32; 2],
}

pub(crate) struct PendingText {
    key: (String, u8),
    /// Quad center, canvas px (unused for viewport-edge anchors).
    pos: [f32; 2],
    /// Rotation in radians around the quad center (world text only).
    rotation: f32,
    /// Uniform scale applied on top of the rasterized size (attachUI lines).
    scale: f32,
    color: [f32; 4],
    /// Draw in window-px NDC space, hugging the viewport bottom.
    viewport_edge: bool,
    anchor: TextAnchor,
}

/// Font handle + line cache + per-frame pending queue.
pub(crate) struct TextState {
    /// `None` = load not attempted; `Some(None)` = failed (permanent no-op).
    font: Option<Option<fontdue::Font>>,
    /// CJK fallback font (msyh.ttc) for glyphs the main font lacks; `None`
    /// = not attempted / failed. Per-glyph fallback keeps latin in the
    /// main font while CJK chars render from the fallback.
    cjk_font: Option<Option<fontdue::Font>>,
    /// Field-split with `pending` for `push_text`; pub(crate) for mod.rs.
    pub(crate) cache: HashMap<(String, u8), CachedText>,
    pub(crate) pending: Vec<PendingText>,
}

impl TextState {
    pub(crate) fn new() -> Self {
        Self {
            font: None,
            cjk_font: None,
            cache: HashMap::new(),
            pending: Vec::new(),
        }
    }
}

/// Exo2 ships in `res/` (the app's UI font); fall back to system fonts
/// (msyh.ttc covers latin+CJK) so offscreen renders work without res/.
/// `None` disables text rendering silently.
fn load_font() -> Option<fontdue::Font> {
    for path in ["res/Exo2.ttf", r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\arial.ttf"] {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(font) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                return Some(font);
            }
        }
    }
    None
}

/// CJK fallback font for glyphs missing from the main font (e.g. Chinese /
/// Japanese / Korean song names). msyh.ttc covers latin+CJK; arial is a
/// last resort for narrow CJK support.
fn load_cjk_font() -> Option<fontdue::Font> {
    for path in [r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\simhei.ttf", r"C:\Windows\Fonts\msyhbd.ttc", r"C:\Windows\Fonts\arial.ttf"] {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(font) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                return Some(font);
            }
        }
    }
    None
}

fn parse_font(bytes: Vec<u8>) -> Option<fontdue::Font> {
    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()
}

/// Rasterize `text` as one line at `px` into a white×alpha RGBA texture.
/// Returns the bind group and the line size in px.
fn rasterize_line(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex_bgl: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    font: &fontdue::Font,
    text: &str,
    px: f32,
    cjk: Option<&fontdue::Font>,
) -> Option<(wgpu::BindGroup, [f32; 2])> {
    let lm = font.horizontal_line_metrics(px)?;
    // Line metrics from the main font; the CJK fallback's em box is usually
    // taller, so grow the line (and shift the baseline down) when CJK glyphs
    // are present.
    let (mut line_h, mut baseline) = ((lm.ascent - lm.descent).ceil().max(1.0) as usize, lm.ascent);
    if let Some(cjk) = cjk {
        if let Some(clm) = cjk.horizontal_line_metrics(px) {
            let clm_h = (clm.ascent - clm.descent).ceil().max(1.0) as usize;
            if clm_h > line_h {
                baseline += (clm_h - line_h) as f32;
                line_h = clm_h;
            }
        }
    }

    // Pick the font per glyph: fall back to the CJK font when the main font
    // has no glyph for the character (lookup_glyph_index == 0 = missing).
    let mut glyphs: Vec<(fontdue::Metrics, Vec<u8>, &fontdue::Font)> = Vec::with_capacity(text.len());
    for c in text.chars() {
        let f = if font.lookup_glyph_index(c) == 0 {
            match cjk {
                Some(cjk) if cjk.lookup_glyph_index(c) != 0 => cjk,
                _ => font, // still rasterize: fontdue returns an empty box for .notdef
            }
        } else {
            font
        };
        glyphs.push((f.rasterize(c, px).0, f.rasterize(c, px).1, f));
    }
    let line_w = glyphs
        .iter()
        .map(|(m, _, _)| m.advance_width)
        .sum::<f32>()
        .ceil()
        .max(1.0) as usize;

    // fontdue Metrics: xmin = bitmap left edge from pen origin; ymin = bitmap
    // BOTTOM edge above the baseline (y up). In image space (y down):
    // bitmap top = baseline - ymin - height.
    let mut gray = vec![0u8; line_w * line_h];
    let mut pen_x = 0.0f32;
    for (m, bitmap, _) in &glyphs {
        let gx0 = pen_x.round() as i32 + m.xmin;
        let gy0 = (baseline - m.ymin as f32 - m.height as f32).round() as i32;
        for gy in 0..m.height as i32 {
            let dy = gy0 + gy;
            if dy < 0 || dy >= line_h as i32 {
                continue;
            }
            for gx in 0..m.width as i32 {
                let dx = gx0 + gx;
                if dx < 0 || dx >= line_w as i32 {
                    continue;
                }
                let cov = bitmap[(gy * m.width as i32 + gx) as usize];
                let dst = &mut gray[dy as usize * line_w + dx as usize];
                if cov > *dst {
                    *dst = cov;
                }
            }
        }
        pen_x += m.advance_width;
    }

    let rgba: Vec<u8> = gray.iter().flat_map(|a| [255, 255, 255, *a]).collect();
    // create_texture uploads raw (no flip), but the quad pipeline expects
    // v=0 = image bottom (everywhere else uses upload_image's flip): flip rows.
    let row = line_w * 4;
    let flipped: Vec<u8> = rgba.chunks(row).rev().flat_map(|r| r.iter().copied()).collect();
    let texture = Renderer::create_texture(device, queue, &flipped, line_w as u32, line_h as u32);
    let bind_group = Renderer::texture_bind_group(device, tex_bgl, sampler, &texture);
    Some((bind_group, [line_w as f32, line_h as f32]))
}

impl Renderer {
    /// Queue a text line for this frame. First call with a given
    /// `(text, anchor)` rasterizes and caches; repeats are cache hits.
    /// Silent no-op if no usable font was found.
    pub fn draw_text(&mut self, text: &str, anchor: TextAnchor, color: [f32; 4]) {
        draw_text_queued(
            &mut self.text,
            &self.device,
            &self.queue,
            &self.tex_bgl,
            &self.sampler,
            self.playfield_aspect,
            text,
            anchor,
            [0.0, 0.0],
            0.0,
            1.0,
            color,
        );
    }

    /// Queue free-positioned text (attachUI labels): centered at `pos`
    /// (canvas px), rotated by `rotation` (radians) around its center.
    pub fn draw_text_world(&mut self, text: &str, pos: [f32; 2], rotation: f32, color: [f32; 4]) {
        draw_text_world(&mut self.text, &self.device, &self.queue, &self.tex_bgl, &self.sampler, self.playfield_aspect, text, pos, rotation, color);
    }

    /// Use a custom font (e.g. res-pack Exo2) for all future rasterizations;
    /// clears the line cache. Returns false if the bytes are not a usable font.
    pub fn set_font(&mut self, bytes: Vec<u8>) -> bool {
        match parse_font(bytes) {
            Some(font) => {
                self.text.font = Some(Some(font));
                self.text.cache.clear();
                true
            }
            None => false,
        }
    }
}

/// Field-split version of [`Renderer::draw_text`]: queues an anchored text
/// line. Callable while `cmds` holds borrows of other renderer fields.
///
/// `offset` (canvas px), `rotation` and `scale` come from the attachUI line
/// binding: the element always sits at its default anchor and the line
/// transform is applied on top (RPE attachUI semantics).
pub(crate) fn draw_text_queued(
    text_state: &mut TextState,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex_bgl: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    aspect: f32,
    text: &str,
    anchor: TextAnchor,
    offset: [f32; 2],
    rotation: f32,
    scale: f32,
    color: [f32; 4],
) {
    let key = (text.to_string(), anchor as u8);
    if !text_state.cache.contains_key(&key) {
        if text_state.font.is_none() {
            text_state.font = Some(load_font());
        }
        let Some(font) = text_state.font.as_ref().and_then(|f| f.as_ref()) else {
            return;
        };
        if text_state.cjk_font.is_none() {
            text_state.cjk_font = Some(load_cjk_font());
        }
        let cjk = text_state.cjk_font.as_ref().and_then(|f| f.as_ref());
        let (px, _, _) = anchor.layout(aspect);
        let Some((bind_group, size)) = rasterize_line(device, queue, tex_bgl, sampler, font, text, px, cjk) else {
            return;
        };
        text_state.cache.insert(key.clone(), CachedText { bind_group, size });
    }
    let entry = &text_state.cache[&key];
    let (_, halign, top) = anchor.layout(aspect);
    let cx = match halign {
        HAlign::Left(l) => l + entry.size[0] * 0.5,
        HAlign::Center => 0.0,
        HAlign::Right(r) => r - entry.size[0] * 0.5,
    };
    let pos = [cx + offset[0], top + entry.size[1] * 0.5 + offset[1]];
    text_state.pending.push(PendingText { key, pos, rotation, scale, color, viewport_edge: anchor.is_viewport_edge(), anchor });
}

/// Free-positioned text (attachUI labels): centered at `pos` (canvas px),
/// rotated by `rotation` (radians) around its center. Field-borrow friendly
/// (takes `&mut TextState` + the renderer's immutable handles separately) so
/// it can be called while `cmds` holds other renderer borrows.
pub(crate) fn draw_text_world(
    text_state: &mut TextState,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex_bgl: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    aspect: f32,
    text: &str,
    pos: [f32; 2],
    rotation: f32,
    color: [f32; 4],
) {
    let anchor = TextAnchor::World;
    let key = (text.to_string(), anchor as u8);
    if !text_state.cache.contains_key(&key) {
        if text_state.font.is_none() {
            text_state.font = Some(load_font());
        }
        let Some(font) = text_state.font.as_ref().and_then(|f| f.as_ref()) else {
            return;
        };
        if text_state.cjk_font.is_none() {
            text_state.cjk_font = Some(load_cjk_font());
        }
        let cjk = text_state.cjk_font.as_ref().and_then(|f| f.as_ref());
        let (px, _, _) = anchor.layout(aspect);
        let Some((bind_group, size)) = rasterize_line(device, queue, tex_bgl, sampler, font, text, px, cjk) else {
            return;
        };
        text_state.cache.insert(key.clone(), CachedText { bind_group, size });
    }
    text_state.pending.push(PendingText { key, pos, rotation, scale: 1.0, color, viewport_edge: false, anchor });
}

/// Flush the pending text queue into `cmds` (called from `render`, after hit
/// effects). Field-split borrows: `cmds` may hold shared borrows of `cache`.
/// `window` = surface size in px, for viewport-edge anchors.
pub(crate) fn push_text<'a>(
    pending: &mut Vec<PendingText>,
    cache: &'a HashMap<(String, u8), CachedText>,
    cmds: &mut Vec<DrawCmd<'a>>,
    letterbox: &Mat3,
    window: [f32; 2],
) {
    const MARGIN: f32 = 16.0; // px from viewport edges
    for pt in pending.iter() {
        if let Some(entry) = cache.get(&pt.key) {
            let sx = entry.size[0] * pt.scale;
            let sy = entry.size[1] * pt.scale;
            let model = if pt.viewport_edge {
                let sx = sx * 2.0 / window[0];
                let sy = sy * 2.0 / window[1];
                let cx = match pt.anchor {
                    TextAnchor::BottomLeftEdge => -1.0 + (MARGIN + sx * 0.5) * 2.0 / window[0],
                    _ => 1.0 - (MARGIN + sx * 0.5) * 2.0 / window[0],
                };
                let cy = -1.0 + (MARGIN + sy * 0.5) * 2.0 / window[1];
                mat_mul(&mat_translate(cx, cy), &mat_scale(sx, sy))
            } else if pt.anchor == TextAnchor::World {
                mat_mul(
                    letterbox,
                    &mat_mul(
                        &mat_translate(pt.pos[0], pt.pos[1]),
                        &mat_mul(
                            &mat_rotate(pt.rotation),
                            &mat_scale(sx, sy),
                        ),
                    ),
                )
            } else {
                mat_mul(
                    letterbox,
                    &mat_mul(
                        &mat_translate(pt.pos[0], pt.pos[1]),
                        &mat_mul(
                            &mat_rotate(pt.rotation),
                            &mat_scale(sx, sy),
                        ),
                    ),
                )
            };
            cmds.push(DrawCmd {
                uniform: DrawUniform {
                    model,
                    color: pt.color,
                    uv_rect: [0., 0., 1., 1.],
                },
                tex: &entry.bind_group,
            });
        }
    }
    pending.clear();
}
