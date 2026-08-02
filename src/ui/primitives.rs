//! Low-level pixel drawing primitives.


/// 1px horizontal line via direct pixel writes (tiny_skia fill_rect is
/// ~300µs/call in debug — grid lines are the hot path).
pub(crate) fn hline(pm: &mut tiny_skia::PixmapMut, y: f32, x0: f32, x1: f32, rgba: [u8; 4]) {
    let h = pm.height() as i32;
    let w = pm.width() as i32;
    let y = y.round() as i32;
    if y < 0 || y >= h { return; }
    let x0 = (x0.round() as i32).max(0);
    let x1 = (x1.round() as i32).min(w - 1);
    if x0 > x1 { return; }
    let row = (y as usize) * (w as usize);
    let data = pm.data_mut();
    for x in x0..=x1 {
        let i = (row + x as usize) * 4;
        data[i] = rgba[0]; data[i + 1] = rgba[1]; data[i + 2] = rgba[2]; data[i + 3] = rgba[3];
    }
}

/// fill_rect clipped to the pixmap bounds. Panels slide in from off-screen
/// (x > w during animations), and tiny_skia asserts on out-of-bounds rects.
/// Edges are rounded OUTWARD to whole pixels: tiny_skia 0.11.4's hairline AA
/// asserts on 1px-wide rects with fractional origins (inner width → 0).
/// Solid colors are written directly via `slice::fill` — tiny_skia's
/// per-pixel pipeline costs ~5ms for a large rect in debug builds.
pub fn fill_rect_clipped(pm: &mut tiny_skia::PixmapMut, rect: tiny_skia::Rect, paint: &tiny_skia::Paint) {
    let w = pm.width() as i32;
    let h = pm.height() as i32;
    let l = (rect.left().floor() as i32).max(0);
    let t = (rect.top().floor() as i32).max(0);
    let r = (rect.right().ceil() as i32).min(w);
    let b = (rect.bottom().ceil() as i32).min(h);
    if r <= l || b <= t { return; }
    // Fast path: solid color → direct u32 fill.
    if let tiny_skia::Shader::SolidColor(c) = &paint.shader {
        let rgba = c.premultiply().to_color_u8();
        let color = u32::from_le_bytes([rgba.red(), rgba.green(), rgba.blue(), rgba.alpha()]);
        let data = pm.data_mut();
        for y in t..b {
            let row = (y as usize) * (w as usize) * 4;
            let start = row + (l as usize) * 4;
            let end = row + (r as usize) * 4;
            let bytes = &mut data[start..end];
            let words = bytemuck::cast_slice_mut::<u8, u32>(bytes);
            words.fill(color);
        }
        return;
    }
    if let Some(cr) = tiny_skia::Rect::from_ltrb(l as f32, t as f32, r as f32, b as f32) {
        let mut p = paint.clone();
        p.anti_alias = false;
        pm.fill_rect(cr, &p, tiny_skia::Transform::default(), None);
    }
}


