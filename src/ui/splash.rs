//! Splash screen: chart library picker.

use super::font::get_font;
use super::text::{draw_text_c, draw_text_on_pixmap, fit_text, text_width};
use super::primitives::fill_rect_clipped;
use super::settings::{backend_label, SettingsData};
use phimakor::trace_span;


/// One chart in the library, with metadata for the splash list / preview.
#[derive(Clone)]
pub struct ChartEntry {
    pub name: String,
    pub path: std::path::PathBuf,
    pub composer: String,
    pub charter: String,
    pub level: String,
    pub difficulty: f32,
    /// Last-modified time (unix seconds), for "Newest" sorting.
    pub modified: u64,
    /// Downscaled illustration thumbnail (fits a 200px box).
    pub thumb: Option<image::RgbaImage>,
}

/// What the splash cursor is currently over. Indexes are positions in the
/// *filtered* chart list (same convention as [`SplashData::filtered`]).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SplashHover {
    None,
    Chart(usize),
    Delete(usize),
    Search,
    Sort,
    Refresh,
    OpenFolder,
    Settings,
    // Settings page rows
    Vsync,
    Fullscreen,
    Backend,
    ScaleRow,
    ScaleMinus,
    ScalePlus,
    Library,
    Back,
}

/// Everything the splash screen needs to draw a frame.
pub struct SplashData<'a> {
    pub charts: &'a [ChartEntry],
    pub filtered: &'a [usize],
    pub filter: &'a str,
    pub hover: SplashHover,
    pub sel: Option<usize>,
    pub sort: u8,
    pub lib_path: &'a str,
    /// List scroll offset in px (clamped to the content height).
    pub scroll: f32,
}

/// Filter (case-insensitive name match) and sort (0 = name, 1 = newest)
/// the chart list, returning indices into `charts`.
pub fn filter_charts(charts: &[ChartEntry], query: &str, sort: u8) -> Vec<usize> {
    let q = query.to_lowercase();
    let mut idx: Vec<usize> = charts.iter().enumerate()
        .filter(|(_, c)| q.is_empty() || c.name.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect();
    match sort {
        1 => idx.sort_by(|&a, &b| {
            charts[b].modified.cmp(&charts[a].modified)
                .then_with(|| charts[a].name.cmp(&charts[b].name))
        }),
        _ => idx.sort_by(|&a, &b| charts[a].name.cmp(&charts[b].name)),
    }
    idx
}

// ── Splash layout constants (draw + hit-test share these) ──

const SPLASH_M: f32 = 40.0;          // left/right margin
const SPLASH_BAR_H: f32 = 48.0;      // bottom bar height
const SPLASH_CTRL_Y: f32 = 56.0;     // search box top
const SPLASH_CTRL_H: f32 = 32.0;     // search box height
const SPLASH_LIST_TOP: f32 = 96.0;   // first chart row top
const SPLASH_ROW_H: f32 = 36.0;      // row height
const SPLASH_ROW_GAP: f32 = 4.0;     // row spacing
const SPLASH_DETAIL_W: f32 = 280.0;  // preview panel width
const SPLASH_DETAIL_GAP: f32 = 24.0; // list ↔ preview gap
const SPLASH_BTN_H: f32 = 28.0;      // bottom buttons
const SPLASH_HDR_BTN_Y: f32 = 10.0;  // header buttons top
const SPLASH_HDR_BTN_H: f32 = 28.0;  // header buttons height

pub fn splash_detail_x(vw: f32, s: f32) -> f32 { vw - SPLASH_M * s - SPLASH_DETAIL_W * s }
pub fn splash_list_right(vw: f32, s: f32) -> f32 { splash_detail_x(vw, s) - SPLASH_DETAIL_GAP * s }

/// Resolve the splash cursor to a [`SplashHover`]. `filtered_len` is the
/// number of currently visible chart rows (used to clamp row indices),
/// `scroll` the list scroll offset in px.
pub fn splash_hit_test(mx: f32, my: f32, vw: f32, vh: f32, s: f32, filtered_len: usize, settings: bool, scroll: f32) -> SplashHover {
    if settings {
        let row_h = 34.0 * s;
        let lx = 60.0 * s;
        let rx = vw - 60.0 * s;
        if mx < lx || mx > rx { return SplashHover::None; }
        let hit = |my: f32, y: f32| my >= y && my <= y + row_h;
        let mut y = 90.0 * s;
        if hit(my, y) { return SplashHover::Vsync; }
        y += row_h + 6.0 * s;
        if hit(my, y) { return SplashHover::Fullscreen; }
        y += row_h + 6.0 * s;
        if hit(my, y) { return SplashHover::Backend; }
        y += row_h + 6.0 * s;
        if hit(my, y) { return SplashHover::ScaleRow; }
        y += row_h + 6.0 * s;
        if hit(my, y) { return SplashHover::Library; }
        y += row_h + 6.0 * s;
        let bw = 60.0 * s;
        if mx >= lx && mx <= lx + bw && hit(my, y) { return SplashHover::ScaleMinus; }
        if mx >= lx + bw + 8.0 * s && mx <= lx + bw * 2.0 + 8.0 * s && hit(my, y) { return SplashHover::ScalePlus; }
        let by = vh - SPLASH_BAR_H * s - row_h - 10.0 * s;
        if hit(my, by) && mx <= lx + 100.0 * s { return SplashHover::Back; }
        return SplashHover::None;
    }
    // Header buttons (top-right: sort / refresh / open folder)
    let ty = SPLASH_HDR_BTN_Y * s;
    if my >= ty && my <= ty + SPLASH_HDR_BTN_H * s {
        let right = vw - SPLASH_M * s;
        let x_dir = right - 104.0 * s;
        let x_ref = x_dir - 8.0 * s - 88.0 * s;
        let x_sort = x_ref - 8.0 * s - 100.0 * s;
        if mx >= x_sort && mx <= x_sort + 100.0 * s { return SplashHover::Sort; }
        if mx >= x_ref && mx <= x_ref + 88.0 * s { return SplashHover::Refresh; }
        if mx >= x_dir && mx <= x_dir + 104.0 * s { return SplashHover::OpenFolder; }
    }
    // Search box
    let lr = splash_list_right(vw, s);
    if mx >= SPLASH_M * s && mx <= lr && my >= SPLASH_CTRL_Y * s && my <= (SPLASH_CTRL_Y + SPLASH_CTRL_H) * s {
        return SplashHover::Search;
    }
    // Chart rows (delete button at the right edge). The list scrolls: row
    // index maps through the scroll offset, rows above/below the viewport
    // are unreachable.
    if mx >= SPLASH_M * s && mx <= lr && my >= SPLASH_LIST_TOP * s && my < vh - 96.0 * s {
        let ri = ((my - (SPLASH_LIST_TOP * s - scroll)) / ((SPLASH_ROW_H + SPLASH_ROW_GAP) * s)) as usize;
        if ri < filtered_len {
            if mx >= lr - 26.0 * s && mx <= lr - 8.0 * s { return SplashHover::Delete(ri); }
            return SplashHover::Chart(ri);
        }
    }
    // Settings button (bottom-left)
    let by = vh - SPLASH_BAR_H * s - SPLASH_BTN_H * s - 10.0 * s;
    if mx >= SPLASH_M * s && mx <= 160.0 * s && my >= by && my <= by + SPLASH_BTN_H * s { return SplashHover::Settings; }
    SplashHover::None
}

/// Render the splash screen (chart picker or settings page) into the overlay
/// texture. `settings == Some(_)` shows the settings page (with a Back button).
pub fn draw_splash(pm: &mut tiny_skia::PixmapMut, data: &SplashData, vw: f32, vh: f32, s: f32, settings: Option<&SettingsData>) {
    let _s = trace_span!("draw_splash");
    let font = get_font();
    // Background
    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(13, 13, 18, 255);
    if let Some(r) = tiny_skia::Rect::from_xywh(0.0, 0.0, vw, vh) {
        fill_rect_clipped(pm, r, &bg);
    }
    // Header buttons
    if settings.is_none() {
        draw_splash_header_buttons(pm, data.hover, data.sort, vw, s);
    }
    if let Some(font) = font {
        draw_text_on_pixmap(pm, "PhiMakor", SPLASH_M * s, 32.0 * s, 20.0 * s, font);
    }
    if let Some(cfg) = settings {
        if let Some(font) = font {
            draw_text_on_pixmap(pm, "Settings", SPLASH_M * s, 50.0 * s, 14.0 * s, font);
        }
        draw_splash_settings(pm, cfg, data.lib_path, data.hover, vw, vh, s);
        return;
    }
    let detail_x = splash_detail_x(vw, s);
    let list_right = splash_list_right(vw, s);
    // Search box
    let sx = SPLASH_M * s;
    let sw = (list_right - sx).max(20.0);
    let mut sbp = tiny_skia::Paint::default();
    let searching = !data.filter.is_empty();
    sbp.set_color_rgba8(if searching { 38 } else { 26 }, if searching { 44 } else { 29 }, if searching { 58 } else { 34 }, 230);
    if let Some(r) = tiny_skia::Rect::from_xywh(sx, SPLASH_CTRL_Y * s, sw, SPLASH_CTRL_H * s) {
        fill_rect_clipped(pm, r, &sbp);
    }
    if let Some(font) = font {
        if searching {
            let txt = data.filter;
            draw_text_on_pixmap(pm, txt, sx + 10.0 * s, SPLASH_CTRL_Y * s + SPLASH_CTRL_H * 0.5 * s + 4.0 * s, 13.0 * s, font);
            let tw = text_width(txt, 13.0 * s);
            draw_text_on_pixmap(pm, "|", sx + 10.0 * s + tw + 2.0 * s, SPLASH_CTRL_Y * s + SPLASH_CTRL_H * 0.5 * s + 4.0 * s, 13.0 * s, font);
        } else {
            draw_text_c(pm, "Search charts…", sx + 10.0 * s, SPLASH_CTRL_Y * s + SPLASH_CTRL_H * 0.5 * s + 4.0 * s, 13.0 * s, font, [130, 130, 140]);
        }
    }
    // Bottom bar: settings button
    let by = vh - SPLASH_BAR_H * s - SPLASH_BTN_H * s - 10.0 * s;
    let is_set = data.hover == SplashHover::Settings;
    let mut bp = tiny_skia::Paint::default();
    bp.set_color_rgba8(if is_set { 60 } else { 40 }, if is_set { 65 } else { 45 }, if is_set { 75 } else { 52 }, 220);
    if let Some(r) = tiny_skia::Rect::from_xywh(SPLASH_M * s, by, 120.0 * s, SPLASH_BTN_H * s) {
        fill_rect_clipped(pm, r, &bp);
    }
    if let Some(font) = font {
        draw_text_on_pixmap(pm, "Settings", 48.0 * s, by + SPLASH_BTN_H * 0.5 * s + 4.0 * s, 13.0 * s, font);
        // Library path + chart count
        draw_text_c(pm, data.lib_path, SPLASH_M * s, vh - 18.0 * s, 11.0 * s, font, [120, 120, 130]);
        let count = format!("{} chart(s)", data.charts.len());
        let cw = text_width(&count, 11.0 * s);
        draw_text_c(pm, &count, vw - SPLASH_M * s - cw, vh - 18.0 * s, 11.0 * s, font, [150, 150, 160]);
    }
    // Empty library: big hint instead of the list.
    if data.charts.is_empty() {
        if let Some(font) = font {
            let msg = "No charts found in the library";
            let msg2 = "Drop a chart folder or .zip file anywhere in this window";
            let msg3 = data.lib_path;
            let cy = vh * 0.42;
            draw_text_on_pixmap(pm, msg, (vw - text_width(msg, 16.0 * s)) * 0.5, cy, 16.0 * s, font);
            draw_text_c(pm, msg2, (vw - text_width(msg2, 12.0 * s)) * 0.5, cy + 24.0 * s, 12.0 * s, font, [140, 140, 152]);
            draw_text_c(pm, msg3, (vw - text_width(msg3, 11.0 * s)) * 0.5, cy + 44.0 * s, 11.0 * s, font, [110, 110, 122]);
        }
        return;
    }
    // Chart rows (scrollable). Rows above the viewport are skipped, rows
    // below are cut off by `list_bottom`; the scrollbar appears when the
    // content overflows.
    let list_bottom = vh - 96.0 * s;
    let list_top = SPLASH_LIST_TOP * s;
    let row_step = (SPLASH_ROW_H + SPLASH_ROW_GAP) * s;
    let view_h = (list_bottom - list_top).max(1.0);
    let content_h = data.filtered.len() as f32 * row_step;
    let max_scroll = (content_h - view_h).max(0.0);
    let scroll = data.scroll.clamp(0.0, max_scroll);
    let mut y = list_top - scroll;
    for (ri, &ci) in data.filtered.iter().enumerate() {
        if y >= list_bottom { break; }
        if y + SPLASH_ROW_H * s > list_top {
            draw_splash_row(pm, &data.charts[ci], data.hover, data.sel, ri, list_right, y, s);
        }
        y += row_step;
    }
    // Scrollbar
    if max_scroll > 0.0 {
        let sw = 3.0 * s;
        let sx = list_right - 2.0 * s;
        let mut track = tiny_skia::Paint::default();
        track.set_color_rgba8(60, 60, 70, 160);
        if let Some(r) = tiny_skia::Rect::from_xywh(sx, list_top, sw, view_h) {
            fill_rect_clipped(pm, r, &track);
        }
        let thumb_h = (view_h * view_h / content_h).max(18.0 * s);
        let thumb_y = list_top + (view_h - thumb_h) * (scroll / max_scroll);
        let mut thumb = tiny_skia::Paint::default();
        thumb.set_color_rgba8(180, 180, 195, 200);
        if let Some(r) = tiny_skia::Rect::from_xywh(sx, thumb_y, sw, thumb_h) {
            fill_rect_clipped(pm, r, &thumb);
        }
    }
    // No search results
    if data.filtered.is_empty() {
        if let Some(font) = font {
            let msg = format!("No results for \"{}\"", data.filter);
            let cy = SPLASH_LIST_TOP * s + 40.0 * s;
            draw_text_c(pm, &msg, (vw - text_width(&msg, 13.0 * s)) * 0.5, cy, 13.0 * s, font, [150, 150, 160]);
            let msg2 = "Press Esc to clear the search";
            draw_text_c(pm, msg2, (vw - text_width(msg2, 11.0 * s)) * 0.5, cy + 20.0 * s, 11.0 * s, font, [120, 120, 132]);
        }
    }
    // Detail preview: hovered chart wins over keyboard selection.
    let preview = match data.hover {
        SplashHover::Chart(i) | SplashHover::Delete(i) => data.filtered.get(i).copied(),
        _ => data.sel,
    };
    draw_splash_detail(pm, data, preview, detail_x, vh, s);
}

/// Top-right utility buttons: sort toggle / refresh / open folder.
fn draw_splash_header_buttons(pm: &mut tiny_skia::PixmapMut, hover: SplashHover, sort: u8, vw: f32, s: f32) {
    let ty = SPLASH_HDR_BTN_Y * s;
    let bh = SPLASH_HDR_BTN_H * s;
    let right = vw - SPLASH_M * s;
    let mut x = right;
    x -= 104.0 * s;
    let dir = (x, 104.0 * s);
    x -= 8.0 * s;
    x -= 88.0 * s;
    let refresh = (x, 88.0 * s);
    x -= 8.0 * s;
    x -= 100.0 * s;
    let sort_b = (x, 100.0 * s);
    let sort_label = if sort == 1 { "Sort: Newest" } else { "Sort: Name" };
    draw_splash_btn(pm, sort_b.0, ty, sort_b.1, bh, hover == SplashHover::Sort, sort_label, 11.0, s);
    draw_splash_btn(pm, refresh.0, ty, refresh.1, bh, hover == SplashHover::Refresh, "Refresh", 11.0, s);
    draw_splash_btn(pm, dir.0, ty, dir.1, bh, hover == SplashHover::OpenFolder, "Open Folder", 11.0, s);
}

/// One gray button with centered text.
fn draw_splash_btn(pm: &mut tiny_skia::PixmapMut, x: f32, y: f32, w: f32, h: f32, hovered: bool, label: &str, size: f32, s: f32) {
    let mut bp = tiny_skia::Paint::default();
    bp.set_color_rgba8(if hovered { 55 } else { 34 }, if hovered { 60 } else { 38 }, if hovered { 72 } else { 46 }, 230);
    if let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) {
        fill_rect_clipped(pm, r, &bp);
    }
    if let Some(font) = get_font() {
        let tw = text_width(label, size * s);
        draw_text_c(pm, label, x + (w - tw) * 0.5, y + h * 0.5 + 4.0 * s, size * s, font, [225, 225, 230]);
    }
}

/// One chart row: name, composer · charter, level badge, hover delete button.
fn draw_splash_row(pm: &mut tiny_skia::PixmapMut, ch: &ChartEntry, hover: SplashHover, sel: Option<usize>, ri: usize, list_right: f32, y: f32, s: f32) {
    let row_w = list_right - SPLASH_M * s;
    let is_hover = hover == SplashHover::Chart(ri) || hover == SplashHover::Delete(ri);
    let is_sel = sel == Some(ri);
    let mut rp = tiny_skia::Paint::default();
    let (r, g, b) = if is_sel { (58, 66, 88) } else if is_hover { (42, 46, 58) } else { (24, 27, 33) };
    rp.set_color_rgba8(r, g, b, 240);
    if let Some(rect) = tiny_skia::Rect::from_xywh(SPLASH_M * s, y, row_w, SPLASH_ROW_H * s) {
        fill_rect_clipped(pm, rect, &rp);
    }
    let Some(font) = get_font() else { return };
    let text_w = row_w - 170.0 * s;
    // Name
    let name = fit_text(&ch.name, text_w, 14.0 * s);
    draw_text_on_pixmap(pm, &name, SPLASH_M * s + 8.0 * s, y + 12.0 * s, 14.0 * s, font);
    // Composer · charter
    let meta = if ch.composer.is_empty() && ch.charter.is_empty() {
        String::new()
    } else if ch.composer.is_empty() {
        ch.charter.clone()
    } else if ch.charter.is_empty() {
        ch.composer.clone()
    } else {
        format!("{} · {}", ch.composer, ch.charter)
    };
    if !meta.is_empty() {
        let meta = fit_text(&meta, text_w, 11.0 * s);
        draw_text_c(pm, &meta, SPLASH_M * s + 8.0 * s, y + 27.0 * s, 11.0 * s, font, [140, 140, 152]);
    }
    // Level badge
    if !ch.level.is_empty() || ch.difficulty > 0.0 {
        let label = if ch.level.is_empty() { format!("{:.1}", ch.difficulty) } else { ch.level.clone() };
        let badge_right = if is_hover { list_right - 30.0 * s } else { list_right - 8.0 * s };
        draw_badge(pm, &label, badge_right, y + 9.0 * s, 11.0 * s, level_color(&ch.level));
    }
    // Delete button (hover only)
    if is_hover {
        let dbx = list_right - 26.0 * s;
        let dby = y + 9.0 * s;
        let mut dp = tiny_skia::Paint::default();
        dp.set_color_rgba8(70, 30, 35, 240);
        if let Some(r) = tiny_skia::Rect::from_xywh(dbx, dby, 18.0 * s, 18.0 * s) {
            fill_rect_clipped(pm, r, &dp);
        }
        draw_text_c(pm, "x", dbx + 6.0 * s, dby + 14.0 * s, 14.0 * s, font, [255, 150, 150]);
    }
}

/// Right detail panel: thumbnail + metadata of the hovered/selected chart.
fn draw_splash_detail(pm: &mut tiny_skia::PixmapMut, data: &SplashData, preview: Option<usize>, x: f32, vh: f32, s: f32) {
    let w = SPLASH_DETAIL_W * s;
    let y = SPLASH_CTRL_Y * s;
    let h = (vh - 60.0 * s - y).max(80.0);
    let mut bp = tiny_skia::Paint::default();
    bp.set_color_rgba8(22, 24, 30, 240);
    if let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) {
        fill_rect_clipped(pm, r, &bp);
    }
    let Some(font) = get_font() else { return };
    let Some(ci) = preview else {
        draw_text_c(pm, "Select a chart to preview", x + 16.0 * s, y + 24.0 * s, 13.0 * s, font, [140, 140, 152]);
        return;
    };
    let ch = &data.charts[ci];
    // Illustration thumbnail
    let ty = y + 12.0 * s;
    let th = 170.0 * s;
    let tw = w - 24.0 * s;
    let mut tbg = tiny_skia::Paint::default();
    tbg.set_color_rgba8(12, 12, 17, 255);
    if let Some(r) = tiny_skia::Rect::from_xywh(x + 12.0 * s, ty, tw, th) {
        fill_rect_clipped(pm, r, &tbg);
    }
    if let Some(img) = &ch.thumb {
        draw_thumb(pm, img, x + 12.0 * s, ty, tw, th);
    }
    // Name
    let ny = ty + th + 14.0 * s;
    let name = fit_text(&ch.name, w - 32.0 * s, 16.0 * s);
    draw_text_on_pixmap(pm, &name, x + 16.0 * s, ny, 16.0 * s, font);
    // Metadata rows
    let mut my = ny + 24.0 * s;
    for (label, value) in [("Composer", &ch.composer), ("Charter", &ch.charter)] {
        if value.is_empty() { continue; }
        let val = fit_text(value, w - 16.0 * s - 100.0 * s, 12.0 * s);
        draw_text_c(pm, label, x + 16.0 * s, my, 11.0 * s, font, [130, 130, 142]);
        draw_text_c(pm, &val, x + 100.0 * s, my, 12.0 * s, font, [215, 215, 220]);
        my += 18.0 * s;
    }
    // Level / difficulty badge
    if !ch.level.is_empty() || ch.difficulty > 0.0 {
        let label = if ch.level.is_empty() { format!("Lv. {:.1}", ch.difficulty) } else { ch.level.clone() };
        draw_text_c(pm, "Level", x + 16.0 * s, my, 11.0 * s, font, [130, 130, 142]);
        draw_badge(pm, &label, x + w - 16.0 * s, my - 13.0 * s, 12.0 * s, level_color(&ch.level));
        my += 22.0 * s;
    }
    // Hint
    draw_text_c(pm, "Click to open  ·  Enter", x + 16.0 * s, y + h - 14.0 * s, 11.0 * s, font, [120, 120, 132]);
}

/// Difficulty tag color: AT/IN/HD/EZ/SP get distinctive hues, else gray.
fn level_color(level: &str) -> [u8; 3] {
    let tag = level.split_whitespace().next().unwrap_or("").to_uppercase();
    match tag.as_str() {
        "AT" => [190, 120, 255],
        "IN" => [255, 90, 130],
        "HD" => [255, 160, 60],
        "EZ" => [90, 210, 120],
        "SP" => [90, 200, 230],
        _ => [150, 150, 165],
    }
}

/// Colored pill badge, right-aligned at `right`.
fn draw_badge(pm: &mut tiny_skia::PixmapMut, text: &str, right: f32, y: f32, size: f32, color: [u8; 3]) {
    let Some(font) = get_font() else { return };
    let w = text_width(text, size) + 2.4 * size;
    let h = size * 1.7;
    let mut bp = tiny_skia::Paint::default();
    bp.set_color_rgba8(color[0], color[1], color[2], 210);
    if let Some(r) = tiny_skia::Rect::from_xywh(right - w, y, w, h) {
        fill_rect_clipped(pm, r, &bp);
    }
    draw_text_c(pm, text, right - w + 1.2 * size, y + h * 0.72, size, font, [255, 255, 255]);
}

/// Scale an image to *cover* the `maxw`×`maxh` box (fill, center-crop the
/// overflow — no letterboxing) and blit it onto the pixmap, premultiplying
/// alpha. Only the box region is written.
fn draw_thumb(pm: &mut tiny_skia::PixmapMut, img: &image::RgbaImage, x: f32, y: f32, maxw: f32, maxh: f32) {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    if iw <= 1.0 || ih <= 1.0 { return; }
    let boxw = maxw.round() as i32;
    let boxh = maxh.round() as i32;
    if boxw <= 0 || boxh <= 0 { return; }
    // Cover: scale so the smaller dimension fills the box, crop the rest.
    let scale = (maxw / iw).max(maxh / ih);
    let (tw, th) = ((iw * scale).round() as u32, (ih * scale).round() as u32);
    let r = image::imageops::resize(img, tw.max(1), th.max(1), image::imageops::FilterType::Triangle);
    let ox = ((tw as f32 - maxw) * 0.5).round() as i32;
    let oy = ((th as f32 - maxh) * 0.5).round() as i32;
    let pm_w = pm.width() as i32;
    let pm_h = pm.height() as i32;
    let dx0 = x.round() as i32;
    let dy0 = y.round() as i32;
    let data = pm.data_mut();
    let src = r.as_raw();
    for row in 0..boxh {
        let sy = oy + row;
        if sy < 0 || sy >= r.height() as i32 { continue; }
        let py = dy0 + row;
        if py < 0 || py >= pm_h { continue; }
        for col in 0..boxw {
            let sx = ox + col;
            if sx < 0 || sx >= r.width() as i32 { continue; }
            let px = dx0 + col;
            if px < 0 || px >= pm_w { continue; }
            let si = ((sy as u32) * r.width() + sx as u32) as usize * 4;
            let a = src[si + 3] as u32;
            if a == 0 { continue; }
            let di = ((py * pm_w + px) as usize) * 4;
            if a == 255 {
                data[di..di + 4].copy_from_slice(&src[si..si + 4]);
            } else {
                data[di] = (src[si] as u32 * a / 255) as u8;
                data[di + 1] = (src[si + 1] as u32 * a / 255) as u8;
                data[di + 2] = (src[si + 2] as u32 * a / 255) as u8;
                data[di + 3] = a as u8;
            }
        }
    }
}

/// Settings page: toggles for vsync / fullscreen and a gui-scale slider.
/// Hover targets: Vsync / Fullscreen / ScaleRow / ScaleMinus / ScalePlus /
/// Back (hit-testing in [`splash_hit_test`]).
fn draw_splash_settings(pm: &mut tiny_skia::PixmapMut, cfg: &SettingsData, lib_path: &str, hover: SplashHover, vw: f32, vh: f32, s: f32) {
    let font = get_font();
    let bar_h = SPLASH_BAR_H * s;
    let mut y = 90.0 * s;
    let row_h = 34.0 * s;
    let lx = 60.0 * s;
    let rx = vw - 60.0 * s;
    let row_bg = |hover: bool| -> tiny_skia::Paint {
        let mut p = tiny_skia::Paint::default();
        p.set_color_rgba8(if hover { 45 } else { 30 }, if hover { 50 } else { 33 }, if hover { 60 } else { 38 }, 220);
        p
    };

    // Library path: truncate long paths for the row, full path in the tooltip line below.
    let lib_short = if lib_path.chars().count() > 46 {
        let head: String = lib_path.chars().take(20).collect();
        let tail: String = lib_path.chars().skip(lib_path.chars().count() - 18).collect();
        format!("{head}…{tail}")
    } else {
        lib_path.to_string()
    };
    let rows: [(&str, String, SplashHover); 5] = [
        ("Vsync", format!("{}", if cfg.vsync { "ON" } else { "OFF" }), SplashHover::Vsync),
        ("Fullscreen", format!("{}", if cfg.fullscreen { "ON" } else { "OFF" }), SplashHover::Fullscreen),
        ("GPU Backend", backend_label(&cfg.backend), SplashHover::Backend),
        ("GUI Scale", format!("{:.1}", cfg.gui_scale), SplashHover::ScaleRow),
        ("Chart Library", lib_short, SplashHover::Library),
    ];
    for (label, value, target) in rows {
        let hovered = hover == target;
        if let Some(r) = tiny_skia::Rect::from_xywh(lx, y, rx - lx, row_h) {
            fill_rect_clipped(pm, r, &row_bg(hovered));
        }
        if let Some(font) = font {
            draw_text_on_pixmap(pm, label, lx + 12.0 * s, y + row_h * 0.5, 14.0 * s, font);
            let tw = text_width(&value, 14.0 * s);
            draw_text_on_pixmap(pm, &value, rx - tw - 12.0 * s, y + row_h * 0.5, 14.0 * s, font);
        }
        y += row_h + 6.0 * s;
    }
    // Scale +/- buttons row
    let scale_row = y;
    let bw = 60.0 * s;
    for (idx, (target, label)) in [(0usize, (SplashHover::ScaleMinus, "-")), (1usize, (SplashHover::ScalePlus, "+"))] {
        let hovered = hover == target;
        let bx = lx + idx as f32 * (bw + 8.0 * s);
        if let Some(r) = tiny_skia::Rect::from_xywh(bx, scale_row, bw, row_h) {
            fill_rect_clipped(pm, r, &row_bg(hovered));
        }
        if let Some(font) = font {
            draw_text_on_pixmap(pm, label, bx + bw * 0.5 - 4.0 * s, scale_row + row_h * 0.5, 16.0 * s, font);
        }
    }
    // Back button
    let by = vh - bar_h - row_h - 10.0 * s;
    let hovered = hover == SplashHover::Back;
    if let Some(r) = tiny_skia::Rect::from_xywh(lx, by, 100.0 * s, row_h) {
        fill_rect_clipped(pm, r, &row_bg(hovered));
    }
    if let Some(font) = font {
        draw_text_on_pixmap(pm, "← Back", lx + 12.0 * s, by + row_h * 0.5, 14.0 * s, font);
    }
}



