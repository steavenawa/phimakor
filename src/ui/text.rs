//! Text rasterization with per-character font fallback.

use super::font::font_for;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use phimakor::trace_span;

/// Approximate rendered text width (per-char fallback metrics, matching
/// [`draw_text_c`] so measured width always matches the drawn width).
/// 与绘制同走 font_for:CJK 字符用 CJK 字体度量(预热后零开销),
/// 否则中文宽度按缺字形度量,截断/居中会错位。
pub(crate) fn text_width(text: &str, size: f32) -> f32 {
    text.chars().map(|ch| {
        font_for(ch)
            .map(|f| f.metrics(ch, size).advance_width)
            .unwrap_or(0.0)
    }).sum()
}

/// Truncate `text` with a trailing "..." to fit `max_w`.
pub(crate) fn fit_text(text: &str, max_w: f32, size: f32) -> String {
    if text_width(text, size) <= max_w { return text.to_string(); }
    let mut out = String::new();
    for ch in text.chars() {
        if text_width(&format!("{out}{ch}"), size) > max_w { break; }
        out.push(ch);
    }
    out.push_str("...");
    out
}

/// Settings page: toggles for vsync / fullscreen and a gui-scale slider.
/// Hover targets: Vsync / Fullscreen / ScaleRow / ScaleMinus / ScalePlus /

// Keyed by (char, size, font) — the same char can come from a different
// fallback font, and its metrics/bitmap differ per font.
static GLYPH_CACHE: LazyLock<Mutex<HashMap<(char, u32, usize), (fontdue::Metrics, Vec<u8>)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn draw_text_on_pixmap(pixmap: &mut tiny_skia::PixmapMut, text: &str, x: f32, y: f32, size: f32, font: &fontdue::Font) {
    draw_text_c(pixmap, text, x, y, size, font, [255, 255, 255]);
}

/// Draw text with per-character global font fallback: each char is rasterized
/// from the first loaded font that contains its glyph (CJK → system fonts).
/// `font` is only a hint for metrics consistency — glyphs fall through.
pub(crate) fn draw_text_c(pixmap: &mut tiny_skia::PixmapMut, text: &str, x: f32, y: f32, size: f32, font: &fontdue::Font, rgb: [u8; 3]) {
    let _s = trace_span!("draw_text");
    let w = pixmap.width() as i32;
    let h = pixmap.height() as i32;
    let size_key = (size * 100.0) as u32;
    let mut pen_x = x;
    for ch in text.chars() {
        let f = font_for(ch).unwrap_or(font);
        let font_id = f as *const fontdue::Font as usize;
        let (m, bitmap) = {
            let mut cache = GLYPH_CACHE.lock().unwrap();
            if let Some(entry) = cache.get(&(ch, size_key, font_id)) {
                (entry.0.clone(), entry.1.clone())
            } else {
                let result = f.rasterize(ch, size);
                cache.insert((ch, size_key, font_id), (result.0.clone(), result.1.clone()));
                result
            }
        };
        let gx0 = pen_x.round() as i32 + m.xmin;
        let gy0 = (y - m.ymin as f32 - m.height as f32).round() as i32;
        for row in 0..m.height {
            let py = gy0 + row as i32;
            if py < 0 || py >= h { continue; }
            for col in 0..m.width {
                let a = bitmap[row * m.width + col];
                if a < 160 { continue; }
                let px = gx0 + col as i32;
                if px < 0 || px >= w { continue; }
                let idx = (py * w + px) as usize * 4;
                let data = pixmap.data_mut();
                data[idx] = rgb[0]; data[idx+1] = rgb[1]; data[idx+2] = rgb[2]; data[idx+3] = 255;
            }
        }
        pen_x += m.advance_width;
    }
}









