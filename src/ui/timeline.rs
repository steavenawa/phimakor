//! Timeline / note-track drawing.

use super::model::GameInfo;
use super::font::get_font;
use super::text::draw_text_on_pixmap;
use super::primitives::{fill_rect_clipped, hline};
use super::{beat_to_panel_y, env_flag, notes_play_rect, pos_x_to_col_x, pos_x_to_panel_x, KIND_COLORS, SKIP_CENTER, SKIP_GRID};
use phimakor::trace_span;


pub(crate) const TL_W: f32 = 260.0;
pub(crate) const NT_W: f32 = 520.0;
pub const QP_W: f32 = 44.0;   // left quick toolbar width
pub(crate) const COL_W: f32 = 36.0;
pub(crate) const COL_GAP: f32 = 4.0;
pub(crate) const HEADER_H: f32 = 20.0;
pub const PANEL_W: f32 = 280.0;

pub(crate) fn draw_5col_timeline(pixmap: &mut tiny_skia::PixmapMut, scroll: f32, zoom: f32, info: &GameInfo, px: f32, vh: f32, s: f32) {
    let _s = trace_span!("draw_5col");
        if !info.show_overlay || !info.show_events || info.events.is_empty() { return; }
    let tl_w = TL_W * s;
    let col_w = COL_W * s;
    let col_g = COL_GAP * s;
    let head_h = HEADER_H * s;

    let py = head_h + 4.0 * s;
    let ph = (vh - 56.0 * s - py) as f64;
    if ph <= 0.0 { return; }

    // Background
    let mut ebg = tiny_skia::Paint::default();
    ebg.set_color_rgba8(20, 25, 35, 200);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, tl_w, vh - 48.0 * s) {
        fill_rect_clipped(pixmap, r, &ebg);
    }

    let (scroll, zoom) = (scroll as f64, zoom as f64);
    let (min_b, max_b) = (scroll, scroll + zoom);

    let to_y = |b: f64| py as f64 + ph - (b - min_b) / zoom * ph;

    let n_cols: usize = if info.show_notes && !info.notes.is_empty() { 6 } else { 5 };
    let hdr_h = head_h - 4.0 * s;

    // Column header background (single rect)
    let hdr_x = px + 4.0 * s;
    let hdr_w = n_cols as f32 * col_w + (n_cols as f32 - 1.0) * col_g;
    let mut hbg = tiny_skia::Paint::default();
    hbg.set_color_rgba8(40, 50, 65, 200);
    if let Some(hr) = tiny_skia::Rect::from_xywh(hdr_x, 2.0 * s, hdr_w, hdr_h) {
        fill_rect_clipped(pixmap, hr, &hbg);
    }

    // Grid lines at snap divisions (single rect spanning all columns)
    let grid = (info.snap as f64).max(0.125);
    let g_start = (min_b / grid).ceil() as i32;
    let g_end = (max_b / grid).floor() as i32;
    let grid_x = px + 4.0 * s;
    let grid_w = n_cols as f32 * col_w + (n_cols as f32 - 1.0) * col_g;
    for gi in g_start..=g_end {
        let b = gi as f64 * grid;
        let y = to_y(b) as f32;
        let is_whole = (b.round() - b).abs() < 0.001;
        let is_half = !is_whole && ((b * 2.0).round() - b * 2.0).abs() < 0.001;
        let a = if is_whole { 100 } else if is_half { 70 } else { 40 };
        let h = if is_whole { 2.0 } else { 1.0 };
        for dy in 0..h as i32 {
            hline(pixmap, y + dy as f32, grid_x, grid_x + grid_w, [60, 60, 70, a]);
        }
    }

    // Current time line across all columns (beat-aligned)
    let ct_y = to_y(info.chart_beat).clamp(py as f64, py as f64 + ph) as f32;
    hline(pixmap, ct_y - 1.0, px + 2.0 * s, px + tl_w - 2.0 * s, [255, 200, 80, 230]);
    hline(pixmap, ct_y + 1.0, px + 2.0 * s, px + tl_w - 2.0 * s, [255, 200, 80, 230]);

    // Layer selector at bottom of timeline panel
    let layer_bar_y = (vh - 48.0 * s) as f32 - 26.0 * s;
    let mut lbg = tiny_skia::Paint::default();
    lbg.set_color_rgba8(30, 30, 35, 200);
    if let Some(r) = tiny_skia::Rect::from_xywh(px + 2.0 * s, layer_bar_y, tl_w - 4.0 * s, 22.0 * s) {
        fill_rect_clipped(pixmap, r, &lbg);
    }
    for li in 0..info.max_layers.min(10) {
        let bx = px + 6.0 * s + li as f32 * (20.0 * s + 4.0 * s);
        let mut lp = tiny_skia::Paint::default();
        if li == info.selected_layer { lp.set_color_rgba8(200, 200, 220, 220); }
        else { lp.set_color_rgba8(100, 100, 120, 160); }
        if let Some(r) = tiny_skia::Rect::from_xywh(bx, layer_bar_y + 3.0 * s, 20.0 * s, 16.0 * s) {
            fill_rect_clipped(pixmap, r, &lp);
        }
    }

    // Event blocks per column (cols 0-4), skip outside visible range.
    // info.events is sorted by end_beats (extract_line_events) → binary search
    // the visible window instead of iterating every event.
    let ev_min = min_b - zoom * 0.05;
    let ev_max = max_b + zoom * 0.05;
    let ev_start = info.events.partition_point(|e| e.end_beats < ev_min);
    for (ei, ev) in info.events[ev_start..].iter().enumerate() {
        if ev.start_beats > ev_max { break; }
        let ei = ev_start + ei;
        let ci = match ev.kind.as_str() {
            "Alpha" => Some(0), "MoveX" => Some(1), "MoveY" => Some(2),
            "Rotate" => Some(3), "Speed" => Some(4), _ => None,
        };
        let Some(ci) = ci else { continue };
        let c = KIND_COLORS[ci].1;
        let y0 = to_y(ev.start_beats).clamp(py as f64, py as f64 + ph) as f32;
        let y1 = to_y(ev.end_beats).clamp(py as f64, py as f64 + ph) as f32;
        let bh = (y1 - y0).abs().max(3.0);
        let selected = info.selected_event_idx == Some(ei) || info.selected_events.contains(&ei);
        if selected {
            let mut sp = tiny_skia::Paint::default();
            sp.set_color_rgba8(255, 255, 255, 220);
            let cx = px + 4.0 * s + ci as f32 * (col_w + col_g);
            if let Some(r) = tiny_skia::Rect::from_xywh(cx - 2.0, y0.min(y1) - 2.0, col_w + 4.0, bh + 4.0) {
                fill_rect_clipped(pixmap, r, &sp);
            }
        }
        let mut ep = tiny_skia::Paint::default();
        ep.set_color_rgba8(c[0], c[1], c[2], if selected { 255 } else { 200 });
        let cx = px + 4.0 * s + ci as f32 * (col_w + col_g);
        if let Some(r) = tiny_skia::Rect::from_xywh(cx, y0.min(y1), col_w, bh) {
            fill_rect_clipped(pixmap, r, &ep);
        }
    }

    // Note blocks per column (col 5, only when show_notes)
    if n_cols >= 6 {
        for note in info.notes.iter() {
            let c = match note.kind { 1 => [50, 150, 255], 2 => [100, 200, 255], 3 => [255, 80, 150], 4 => [255, 220, 60], _ => [180; 3] };
            let y0 = to_y(note.start_beats).clamp(py as f64, py as f64 + ph) as f32;
            let y1 = to_y(note.end_beats).clamp(py as f64, py as f64 + ph) as f32;
            let bh = (y1 - y0).abs().max(3.0);
            let mut np = tiny_skia::Paint::default();
            np.set_color_rgba8(c[0], c[1], c[2], 200);
            let cx = px + 4.0 * s + 5.0 * (col_w + col_g);
            if let Some(r) = tiny_skia::Rect::from_xywh(cx + 1.0, y0.min(y1), col_w - 2.0, bh) {
                fill_rect_clipped(pixmap, r, &np);
            }
        }
    }
}

/// 绘制音符时间轴面板(mania 网格批次 1 起为主编辑区)。`ghost` 为放置幽灵
/// (start, end, x, kind):悬停/拖拽中半透明块显示将生成的音符。命中/绘制
/// 共用 super::notes_play_rect 映射(铁律)。
pub(crate) fn draw_notes_timeline(pixmap: &mut tiny_skia::PixmapMut, scroll: f32, zoom: f32, info: &GameInfo, px: f32, vh: f32, s: f32, ghost: Option<(f64, f64, f32, u8)>) {
    let _s = trace_span!("draw_notes");
        // 空线也要画面板(网格/ghost 用):不再因 notes.is_empty() 提前返回。
        if !info.show_overlay || !info.show_notes { return; }
    let nt_w = NT_W * s;
    let head_h = HEADER_H * s;
    let pad_x = 12.0 * s;
    let play_w = nt_w - pad_x * 2.0;
    let py = head_h + 4.0 * s;
    let ph = (vh - 56.0 * s - py) as f64;
    if ph <= 0.0 { return; }

    let v_split = info.vertical_split.max(1) as f32;

    let (scroll, zoom) = (scroll as f64, zoom as f64);
    let (min_b, max_b) = (scroll, scroll + zoom);
    // 命中/绘制共用同一映射(mania 批次 1):几何常量收敛到 notes_play_rect。
    let play = notes_play_rect(px, vh, s);
    let to_y = |b: f64| beat_to_panel_y(b, play, scroll as f32, zoom as f32) as f64;
    // X position: note.position_x / 675 → -1..1 → panel center ±play_w/2
    let to_x = |nx: f32| pos_x_to_panel_x(nx, play);
    // Column-aware x: map position_x to column center when split > 1
    let to_col_x = |nx: f32| pos_x_to_col_x(nx, play, info.vertical_split.max(1));

    // Background
    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(20, 25, 35, 215);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, nt_w, vh - 48.0 * s) {
        fill_rect_clipped(pixmap, r, &bg);
    }

    // Header (full preview mode = different color)
    let mut hp = tiny_skia::Paint::default();
    if info.full_notes { hp.set_color_rgba8(200, 140, 255, 200); }
    else { hp.set_color_rgba8(140, 200, 255, 180); }
    if let Some(r) = tiny_skia::Rect::from_xywh(px + pad_x, 2.0 * s, play_w, head_h - 4.0 * s) {
        fill_rect_clipped(pixmap, r, &hp);
    }
    // 判定线注释标记(PMCORE-77):当前线带注释时在头部左缘画橙色小方块。
    if info.line_comment {
        let mut cp = tiny_skia::Paint::default();
        cp.set_color_rgba8(255, 170, 60, 255);
        let d = 6.0 * s;
        let cy = 2.0 * s + (head_h - 4.0 * s) * 0.5 - d * 0.5;
        if let Some(r) = tiny_skia::Rect::from_xywh(px + pad_x + 6.0 * s, cy, d, d) {
            fill_rect_clipped(pixmap, r, &cp);
        }
    }
    // Header split indicator (top-right)
    if let Some(font) = get_font() {
        let label = format!("横:{:.0}b 纵:{:.0}", info.snap * 4.0, v_split);
        draw_text_on_pixmap(pixmap, &label, px + nt_w - pad_x - 120.0 * s, 2.0 * s + head_h * 0.4, 10.0 * s, font);
    }

    // Vertical lane lines (mania grid 视觉,PMCORE-XX):列(道)边界浅色
    // 竖线常显,与水平 snap 网格线同 SKIP_GRID 开关(v_split=1 时单道
    // 无边线,中轴由下方 Center line 绘制)。
    // Horizontal grid lines at snap divisions:整拍/半拍/细分三级(与
    // draw_5col_timeline 同式),整拍线 2px 保持。
    let grid = (info.snap as f64).max(0.125);
    let g_start = (min_b / grid).ceil() as i32;
    let g_end = (max_b / grid).floor() as i32;
    if !*SKIP_GRID.get_or_init(|| env_flag("PHIMAKOR_SKIP_GRID")) {
        if v_split > 1.0 {
            let col_w = play_w / v_split;
            for ci in 1..v_split as usize {
                let x = px + pad_x + ci as f32 * col_w;
                let mut vp = tiny_skia::Paint::default();
                vp.set_color_rgba8(110, 130, 180, 55);
                if let Some(r) = tiny_skia::Rect::from_xywh(x, py, 1.0, ph as f32) {
                    fill_rect_clipped(pixmap, r, &vp);
                }
            }
        }
        for gi in g_start..=g_end {
            let b = gi as f64 * grid;
            let y = to_y(b) as f32;
            let is_whole = (b.round() - b).abs() < 0.001;
            let is_half = !is_whole && ((b * 2.0).round() - b * 2.0).abs() < 0.001;
            let a = if is_whole { 100 } else if is_half { 70 } else { 40 };
            let h = if is_whole { 2.0 } else { 1.0 };
            for dy in 0..h as i32 {
                hline(pixmap, y + dy as f32, px + pad_x, px + pad_x + play_w, [60, 60, 70, a]);
            }
        }
    }

    // Center line
    if !*SKIP_CENTER.get_or_init(|| env_flag("PHIMAKOR_SKIP_CENTER")) {
    let cx = to_x(0.0);
    // 1px vertical line: write pixels directly
    {
        let h = pixmap.height() as i32;
        let w = pixmap.width() as i32;
        let x = (cx.round() as i32).clamp(0, w - 1);
        let y0 = (py.round() as i32).max(0);
        let y1 = ((ph as f32 + py).round() as i32).min(h - 1);
        let data = pixmap.data_mut();
        for yy in y0..=y1 {
            let i = (yy as usize * w as usize + x as usize) * 4;
            data[i] = 100; data[i+1] = 100; data[i+2] = 120; data[i+3] = 120;
        }
    }
    }

    // Current time (beat-aligned)
    let ct_y = to_y(info.chart_beat).clamp(py as f64, py as f64 + ph) as f32;
    hline(pixmap, ct_y - 1.0, px + pad_x, px + pad_x + play_w, [255, 200, 80, 230]);
    hline(pixmap, ct_y + 1.0, px + pad_x, px + pad_x + play_w, [255, 200, 80, 230]);

    // Notes, skip outside visible range (info.notes sorted by end_beats)
    let nt_min = min_b - zoom * 0.05;
    let nt_max = max_b + zoom * 0.05;
    if std::env::var("PHIMAKOR_SKIP_NOTES").is_err() {
    let nt_start = info.notes.partition_point(|n| n.end_beats < nt_min);
    for note in &info.notes[nt_start..] {
        if note.start_beats > nt_max { break; }
        let c = match note.kind { 1 => [50, 150, 255], 2 => [100, 200, 255], 3 => [255, 80, 150], 4 => [255, 220, 60], _ => [180; 3] };
        let xn = to_col_x(note.x);
        let y0 = to_y(note.start_beats).clamp(py as f64, py as f64 + ph) as f32;
        let y1 = to_y(note.end_beats).clamp(py as f64, py as f64 + ph) as f32;
        let bh = (y1 - y0).abs().max(3.0);
        let sc = note.scale.max(0.1);
        let nw = (64.0 * s * sc).max(4.0);
        let nh = (8.0 * s * sc).max(2.0);
        let has_tex = info.has_custom_tex;
        let mut np = tiny_skia::Paint::default();
        if has_tex { np.set_color_rgba8(c[0]/2+128, c[1]/2+128, c[2]/2+128, 220); }
        else { np.set_color_rgba8(c[0], c[1], c[2], 220); }

        if note.kind == 2 && bh > nh * 2.0 {
            if let Some(r) = tiny_skia::Rect::from_xywh(xn - 2.0 * s, y0.min(y1), 4.0 * s, bh) {
                fill_rect_clipped(pixmap, r, &np);
            }
        }
        if let Some(r) = tiny_skia::Rect::from_xywh(xn - nw * 0.5, y0 - nh * 0.5, nw, nh) {
            fill_rect_clipped(pixmap, r, &np);
        }
        // 注释标记(PMCORE-77):带注释的音符在块右侧画橙色小方块(避开
        // hold 竖条)。
        if note.comment {
            let mut cp = tiny_skia::Paint::default();
            cp.set_color_rgba8(255, 170, 60, 255);
            let d = (4.0 * s).max(2.0);
            if let Some(r) = tiny_skia::Rect::from_xywh(xn + nw * 0.5 + 3.0 * s, y0 - d * 0.5, d, d) {
                fill_rect_clipped(pixmap, r, &cp);
            }
        }
        // 选中高亮(PMCORE-18):黄色描边。命中几何 = 绘制几何(同一
        // to_y / to_col_x / nw / nh,见 ui/mod.rs note_screen_rect)。
        if info.selected_notes.contains(&note.index) {
            let hw = nw * 0.5 + 2.0 * s;
            let hh = nh * 0.5 + 2.0 * s;
            let mut hp = tiny_skia::Paint::default();
            hp.set_color_rgba8(255, 220, 90, 255);
            for (rx, ry, rw, rh) in [
                (xn - hw, y0 - hh, nw + 4.0 * s, 2.0 * s),
                (xn - hw, y0 + hh - 2.0 * s, nw + 4.0 * s, 2.0 * s),
                (xn - hw, y0 - hh, 2.0 * s, nh + 4.0 * s),
                (xn + hw - 2.0 * s, y0 - hh, 2.0 * s, nh + 4.0 * s),
            ] {
                if let Some(r) = tiny_skia::Rect::from_xywh(rx, ry, rw, rh) {
                    fill_rect_clipped(pixmap, r, &hp);
                }
            }
        }
    }
    }
    // 放置幽灵预览(mania 批次 1):悬停/拖拽中半透明块显示将生成的音符。
    // 与放置共用同一映射(pos_x 不做列吸附,ghost 显示即实际落点)。
    if let Some((gb0, gb1, gx, gk)) = ghost {
        if gb1 >= min_b && gb0 <= max_b {
            let c = match gk { 1 => [50, 150, 255], 2 => [100, 200, 255], 3 => [255, 80, 150], 4 => [255, 220, 60], _ => [180; 3] };
            let mut gp = tiny_skia::Paint::default();
            gp.set_color_rgba8(c[0], c[1], c[2], 110);
            let xn = pos_x_to_panel_x(gx, play);
            let gy0 = beat_to_panel_y(gb0.clamp(min_b, max_b), play, scroll as f32, zoom as f32) as f64;
            let gy1 = beat_to_panel_y(gb1.clamp(min_b, max_b), play, scroll as f32, zoom as f32) as f64;
            let bh = (gy1 - gy0).abs().max(3.0);
            if gk == 2 && bh > 8.0 * s as f64 {
                if let Some(r) = tiny_skia::Rect::from_xywh(xn - 2.0 * s, gy0.min(gy1) as f32, 4.0 * s, bh as f32) {
                    fill_rect_clipped(pixmap, r, &gp);
                }
            }
            if let Some(r) = tiny_skia::Rect::from_xywh(xn - 32.0 * s, gy0.min(gy1) as f32 - 4.0 * s, 64.0 * s, 8.0 * s) {
                fill_rect_clipped(pixmap, r, &gp);
            }
        }
    }
}










