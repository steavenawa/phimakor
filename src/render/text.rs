#![allow(dead_code)] // 库 API: Python 绑定/embedding/备用接口,主程序未全部使用

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
    /// Hug the VIEWPORT top-right (window px space, not canvas), 36 px.
    TopRightEdge,
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
            // TopRightEdge: 22px,贴顶小字(HUD 帧时间叠层)。
            Self::TopRightEdge => (22.0, HAlign::Right(655.0), top - 8.0 - 36.0),
            Self::BottomLeftEdge => (30.0, HAlign::Left(-655.0), bottom),
            Self::BottomRightEdge => (30.0, HAlign::Right(655.0), bottom),
            // Directly below TopCenter on screen (+y = up): 6 px gap, 26 px.
            Self::ComboLabel => (26.0, HAlign::Center, top - 2.0 - 56.0 - 6.0 - 26.0),
            Self::World => (28.0, HAlign::Center, 0.0),
        }
    }

    fn is_viewport_edge(self) -> bool {
        matches!(self, Self::BottomLeftEdge | Self::BottomRightEdge | Self::TopRightEdge)
    }
}

pub(crate) struct CachedText {
    bind_group: wgpu::BindGroup,
    /// Line size in canvas px (width × em-box height).
    size: [f32; 2],
}

impl Clone for CachedText {
    fn clone(&self) -> Self {
        Self { bind_group: self.bind_group.clone(), size: self.size }
    }
}

pub(crate) struct PendingText {
    entry: CachedText,
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

/// Pre-rasterized digit glyphs (0-9) for one px size. Dynamic numbers
/// (score / combo) are composed per frame by blitting these bitmaps — no
/// per-string GPU textures, so playback/seek can't leak memory.
struct DigitGlyphs {
    /// `(metrics, bitmap)` for '0'..='9', in order.
    glyphs: Vec<(fontdue::Metrics, Vec<u8>)>,
    line_h: usize,
    baseline: f32,
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
    /// LRU recency order for the cache keys (front = least recently used).
    access: Vec<(String, u8)>,
    /// Digit sprite sets by rasterization px (`(px*100) as u32`).
    digits: HashMap<u32, DigitGlyphs>,
    pub(crate) pending: Vec<PendingText>,
    /// Raw bytes of the loaded font files (rough heap footprint).
    font_bytes: usize,
}

impl TextState {
    pub(crate) fn new() -> Self {
        Self {
            font: None,
            cjk_font: None,
            cache: HashMap::new(),
            access: Vec::new(),
            digits: HashMap::new(),
            pending: Vec::new(),
            font_bytes: 0,
        }
    }

    /// Memory report: (static text cache entries, digit glyph bitmap bytes,
    /// font file bytes).
    pub(crate) fn mem_report(&self) -> (usize, usize, usize) {
        let digits: usize = self.digits.values()
            .map(|d| d.glyphs.iter().map(|(m, b)| b.len() + m.width as usize * m.height as usize).sum::<usize>())
            .sum();
        (self.cache.len(), digits, self.font_bytes)
    }
}

/// Lazy-load the main font into `slot` once (tracks raw file bytes).
/// Field-level helper: borrows only the font slot + byte counter, so it
/// composes with other field borrows of the same [`TextState`].
fn load_main_into(slot: &mut Option<Option<fontdue::Font>>, bytes_total: &mut usize) {
    if slot.is_none() {
        match load_font() {
            Some((font, bytes)) => {
                *bytes_total += bytes;
                *slot = Some(Some(font));
            }
            None => *slot = Some(None),
        }
    }
}

/// Lazy-load the CJK fallback font into `slot` once (tracks raw file bytes).
fn load_cjk_into(slot: &mut Option<Option<fontdue::Font>>, bytes_total: &mut usize) {
    if slot.is_none() {
        match load_cjk_font() {
            Some((font, bytes)) => {
                *bytes_total += bytes;
                *slot = Some(Some(font));
            }
            None => *slot = Some(None),
        }
    }
}

/// Exo2 ships in `res/` (the app's UI font); fall back to system fonts
/// (msyh.ttc covers latin+CJK) so offscreen renders work without res/.
/// `None` disables text rendering silently. Returns the raw file size too.
fn load_font() -> Option<(fontdue::Font, usize)> {
    for path in ["res/Exo2.ttf", r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\arial.ttf"] {
        if let Ok(bytes) = std::fs::read(path) {
            let n = bytes.len();
            if let Ok(font) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                return Some((font, n));
            }
        }
    }
    None
}

/// CJK fallback font for glyphs missing from the main font (e.g. Chinese /
/// Japanese / Korean song names). msyh.ttc covers latin+CJK; arial is a
/// last resort for narrow CJK support. Returns the raw file size too.
fn load_cjk_font() -> Option<(fontdue::Font, usize)> {
    for path in [r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\simhei.ttf", r"C:\Windows\Fonts\msyhbd.ttc", r"C:\Windows\Fonts\arial.ttf"] {
        if let Ok(bytes) = std::fs::read(path) {
            let n = bytes.len();
            if let Ok(font) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                return Some((font, n));
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
    let glyphs: Vec<(fontdue::Metrics, Vec<u8>)> = text.chars().map(|c| {
        let f = if font.lookup_glyph_index(c) == 0 {
            match cjk {
                Some(cjk) if cjk.lookup_glyph_index(c) != 0 => cjk,
                _ => font, // still rasterize: fontdue returns an empty box for .notdef
            }
        } else {
            font
        };
        f.rasterize(c, px)
    }).collect();
    let line_w = glyphs
        .iter()
        .map(|(m, _)| m.advance_width)
        .sum::<f32>()
        .ceil()
        .max(1.0) as usize;
    let gray = blit_glyphs(glyphs.iter().map(|(m, b)| (m, b.as_slice())), line_h, baseline);
    Some(upload_gray_line(device, queue, tex_bgl, sampler, gray, line_w, line_h))
}

/// Blit glyph bitmaps into a line-height gray buffer (max coverage, so
/// overlapping glyphs merge). fontdue Metrics: xmin = bitmap left edge from
/// pen origin; ymin = bitmap BOTTOM edge above the baseline (y up). In
/// image space (y down): bitmap top = baseline - ymin - height.
fn blit_glyphs<'a>(
    glyphs: impl IntoIterator<Item = (&'a fontdue::Metrics, &'a [u8])>,
    line_h: usize,
    baseline: f32,
) -> Vec<u8> {
    let glyphs: Vec<(&'a fontdue::Metrics, &'a [u8])> = glyphs.into_iter().collect();
    let line_w = glyphs
        .iter()
        .map(|(m, _)| m.advance_width)
        .sum::<f32>()
        .ceil()
        .max(1.0) as usize;
    let mut gray = vec![0u8; line_w * line_h];
    let mut pen_x = 0.0f32;
    for (m, bitmap) in &glyphs {
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
    gray
}

/// Turn a gray line buffer into a white×alpha texture + bind group (rows
/// flipped: the quad pipeline expects v=0 = image bottom).
fn upload_gray_line(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex_bgl: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    gray: Vec<u8>,
    line_w: usize,
    line_h: usize,
) -> (wgpu::BindGroup, [f32; 2]) {
    let rgba: Vec<u8> = gray.iter().flat_map(|a| [255, 255, 255, *a]).collect();
    let row = line_w * 4;
    let flipped: Vec<u8> = rgba.chunks(row).rev().flat_map(|r| r.iter().copied()).collect();
    let texture = Renderer::create_texture(device, queue, &flipped, line_w as u32, line_h as u32);
    let bind_group = Renderer::texture_bind_group(device, tex_bgl, sampler, &texture);
    (bind_group, [line_w as f32, line_h as f32])
}

/// All-ASCII-digits string (score / combo numbers).
fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// Compose a digit string from pre-rasterized 0-9 glyphs into one texture.
/// Loads fonts / builds the glyph set lazily from `state` (no external font
/// borrows), and never caches per-string textures — cheap per-frame blits,
/// so dynamic numbers (score/combo) can't leak GPU memory.
fn compose_digits(
    state: &mut TextState,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex_bgl: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    px: f32,
    text: &str,
) -> Option<(wgpu::BindGroup, [f32; 2])> {
    if state.font.is_none() {
        load_main_into(&mut state.font, &mut state.font_bytes);
    }
    let font = state.font.as_ref().and_then(|f| f.as_ref())?;
    // Digits are always in the main font — never load the heavy CJK
    // fallback just for score/combo numbers.
    let key = (px * 100.0) as u32;
    if !state.digits.contains_key(&key) {
        let lm = font.horizontal_line_metrics(px)?;
        let line_h = (lm.ascent - lm.descent).ceil().max(1.0) as usize;
        let baseline = lm.ascent;
        let mut glyphs = Vec::with_capacity(10);
        for c in '0'..='9' {
            glyphs.push(font.rasterize(c, px));
        }
        state.digits.insert(key, DigitGlyphs { glyphs, line_h, baseline });
    }
    let ds = &state.digits[&key];
    let line_w = text.chars()
        .map(|c| ds.glyphs[(c as u8 - b'0') as usize].0.advance_width)
        .sum::<f32>()
        .ceil()
        .max(1.0) as usize;
    let gray = blit_glyphs(text.chars().map(|c| {
        let i = (c as u8 - b'0') as usize;
        (&ds.glyphs[i].0, ds.glyphs[i].1.as_slice())
    }), ds.line_h, ds.baseline);
    Some(upload_gray_line(device, queue, tex_bgl, sampler, gray, line_w, ds.line_h))
}

/// True if `text` has any char the main font lacks (CJK/symbols) — the
/// signal to load the heavy CJK fallback.
fn needs_cjk(font: &fontdue::Font, text: &str) -> bool {
    text.chars().any(|c| font.lookup_glyph_index(c) == 0)
}

impl Renderer {
    /// Queue a text line for this frame. First call with a given
    /// `(text, anchor)` rasterizes and caches; repeats are cache hits.
    /// Digit-only strings are composed per frame from pre-rasterized glyphs
    /// (no caching). Silent no-op if no usable font was found.
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

/// Rasterize `text` for this frame (or fetch the cached line), touching the
/// LRU order. Digit-only strings are composed per frame, never cached.
/// Shared by the anchored and world-space draw paths.
fn rasterized_entry(
    text_state: &mut TextState,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex_bgl: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    px: f32,
    text: &str,
    anchor: TextAnchor,
) -> Option<CachedText> {
    if text_state.font.is_none() {
        load_main_into(&mut text_state.font, &mut text_state.font_bytes);
    }
    let font = text_state.font.as_ref().and_then(|f| f.as_ref())?;
    // CJK fallback loads only when the text actually needs it (Chinese song
    // names) — ASCII-only HUDs never touch the ~19 MB msyh.
    let cjk = if needs_cjk(font, text) {
        if text_state.cjk_font.is_none() {
            load_cjk_into(&mut text_state.cjk_font, &mut text_state.font_bytes);
        }
        text_state.cjk_font.as_ref().and_then(|f| f.as_ref())
    } else {
        None
    };
    if is_digits(text) {
        // Dynamic numbers: compose from pre-rasterized digits, no cache.
        return compose_digits(text_state, device, queue, tex_bgl, sampler, px, text)
            .map(|(bind_group, size)| CachedText { bind_group, size });
    }
    let key = (text.to_string(), anchor as u8);
    if let Some(entry) = text_state.cache.get(&key) {
        // Touch LRU: move to the back (most recently used).
        lru_touch(&mut text_state.access, &key);
        return Some(entry.clone());
    }
    let (bind_group, size) = rasterize_line(device, queue, tex_bgl, sampler, font, text, px, cjk)?;
    let entry = CachedText { bind_group, size };
    // LRU eviction: drop the least recently used entry when over the cap,
    // instead of wiping the whole cache (which destroyed every GPU texture).
    if let Some(oldest) = lru_evict_oldest(&mut text_state.access, TEXT_CACHE_CAP) {
        text_state.cache.remove(&oldest);
    }
    text_state.cache.insert(key.clone(), entry.clone());
    text_state.access.push(key);
    Some(entry)
}

/// LRU cap for the static text line cache.
const TEXT_CACHE_CAP: usize = 1024;

/// LRU 命中维护(纯逻辑,单测锁定):把 key 移到 `access` 末尾(最近使用)。
/// `access` 与 `cache` 保持同长同序(新增/逐出两边同步,命中只动 access)。
fn lru_touch(access: &mut Vec<(String, u8)>, key: &(String, u8)) {
    if let Some(pos) = access.iter().position(|k| k == key) {
        access.remove(pos);
        access.push(key.clone());
    }
}

/// LRU 新增逐出(纯逻辑,单测锁定):缓存已满(≥ cap)时返回应逐出的最旧
/// key —— 只逐一条,不整清空。调用方逐出后把新 key push 到 `access` 末尾。
fn lru_evict_oldest(access: &mut Vec<(String, u8)>, cap: usize) -> Option<(String, u8)> {
    if access.len() >= cap {
        Some(access.remove(0))
    } else {
        None
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
    let (px, _, _) = anchor.layout(aspect);
    let Some(entry) = rasterized_entry(text_state, device, queue, tex_bgl, sampler, px, text, anchor) else {
        return;
    };
    let (_, halign, top) = anchor.layout(aspect);
    let cx = match halign {
        HAlign::Left(l) => l + entry.size[0] * 0.5,
        HAlign::Center => 0.0,
        HAlign::Right(r) => r - entry.size[0] * 0.5,
    };
    let pos = [cx + offset[0], top + entry.size[1] * 0.5 + offset[1]];
    text_state.pending.push(PendingText { entry, pos, rotation, scale, color, viewport_edge: anchor.is_viewport_edge(), anchor });
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
    let (px, _, _) = anchor.layout(aspect);
    let Some(entry) = rasterized_entry(text_state, device, queue, tex_bgl, sampler, px, text, anchor) else {
        return;
    };
    text_state.pending.push(PendingText { entry, pos, rotation, scale: 1.0, color, viewport_edge: false, anchor });
}

/// Flush the pending text queue into `cmds` (called at the end of
/// `draw_to_view`, after hit effects). The queue was cleared at the START of
/// the frame (before any text was queued), so every entry here is current.
/// `window` = surface size in px, for viewport-edge anchors.
pub(crate) fn push_text<'a>(
    pending: &'a mut Vec<PendingText>,
    cmds: &mut Vec<DrawCmd<'a>>,
    letterbox: &Mat3,
    window: [f32; 2],
) {
    const MARGIN: f32 = 16.0; // px from viewport edges
    for pt in pending.iter() {
        let sx = pt.entry.size[0] * pt.scale;
        let sy = pt.entry.size[1] * pt.scale;
        let model = if pt.viewport_edge {
            // 边距定位用 px(修前语义),quad 尺寸用 NDC(px→NDC 换算,
            // 否则像素数被当全屏宽倍数)。
            let sx_ndc = sx * 2.0 / window[0];
            let sy_ndc = sy * 2.0 / window[1];
            let cx = match pt.anchor {
                TextAnchor::BottomLeftEdge => -1.0 + (MARGIN + sx * 0.5) * 2.0 / window[0],
                _ => 1.0 - (MARGIN + sx * 0.5) * 2.0 / window[0],
            };
            // TopRightEdge 贴窗口顶部(4px 小边距),其余边缘锚定贴底部。
            let cy = match pt.anchor {
                TextAnchor::TopRightEdge => 1.0 - (4.0 + sy * 0.5) * 2.0 / window[1],
                _ => -1.0 + (MARGIN + sy * 0.5) * 2.0 / window[1],
            };
            mat_mul(&mat_translate(cx, cy), &mat_scale(sx_ndc, sy_ndc))
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
            tex: &pt.entry.bind_group,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_cache_lru_evicts_oldest_only() {
        // 回归(PMCORE-45):超上限只逐最旧一条,不整清空(旧实现曾摧毁
        // 全部缓存)。小 cap 模拟 TEXT_CACHE_CAP 的逐出策略。
        let mut access: Vec<(String, u8)> = (0..4).map(|i| (format!("t{i}"), 0u8)).collect();
        // 满 4 条再插:逐出 t0,其余保留
        let evicted = lru_evict_oldest(&mut access, 4);
        assert_eq!(evicted, Some(("t0".to_string(), 0)));
        assert_eq!(access.len(), 3);
        access.push(("new".to_string(), 0));
        assert_eq!(access, vec![
            ("t1".to_string(), 0), ("t2".to_string(), 0), ("t3".to_string(), 0), ("new".to_string(), 0),
        ]);
        // 命中 new → 移到末尾,逐出轮不到它
        lru_touch(&mut access, &("new".to_string(), 0));
        assert_eq!(access.last(), Some(&("new".to_string(), 0)));
        let evicted = lru_evict_oldest(&mut access, 4);
        assert_eq!(evicted, Some(("t1".to_string(), 0)));
        access.push(("new2".to_string(), 0));
        assert_eq!(access.len(), 4);
        assert_eq!(access.first(), Some(&("t2".to_string(), 0)));
    }

    #[test]
    fn text_cache_cap_holds_1024() {
        // 真实 cap:插入 > TEXT_CACHE_CAP 条后长度稳定在 1024,仅最旧被逐。
        // 与 mem_report 的 cache.len() 同源(cache/access 同步逐出)。
        let mut access: Vec<(String, u8)> = Vec::new();
        for i in 0..=(TEXT_CACHE_CAP + 7) {
            if let Some(_evicted) = lru_evict_oldest(&mut access, TEXT_CACHE_CAP) {}
            access.push((format!("k{i}"), 0));
        }
        assert_eq!(access.len(), TEXT_CACHE_CAP);
        assert_eq!(access[0], ("k8".to_string(), 0)); // 前 8 条被逐,非整清空
        assert!(access.contains(&("k1024".to_string(), 0)));
        assert_eq!(access.last(), Some(&("k1031".to_string(), 0)));
    }
}
