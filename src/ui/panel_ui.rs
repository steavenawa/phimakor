//! Panel definition / quick-toolbar drawing.
//! (Eff 面板已迁移到组件库 RealtimeForm + eff_panel.rs,PMCORE-59。)

use super::model::{gameinfo_values, GameInfo};
use super::font::get_font;
use super::text::draw_text_on_pixmap;
use super::primitives::fill_rect_clipped;
use super::panels;
use phimakor::trace_span;



/// Draw a panel definition into the right-side area.
pub fn draw_panel_def(pixmap: &mut tiny_skia::PixmapMut, def: &panels::PanelDef, info: &GameInfo, px: f32, _vw: f32, vh: f32, s: f32) {
    let _s = trace_span!("draw_panel");
    if !info.show_overlay { return; }
    if vh < 10.0 { return; }
    let font = get_font();
    let vals = gameinfo_values(info);
    let rows = def.resolve(&vals);
    let bar_h = 48.0 * s;
    let pan_w = def.width * s;
    let bg_h = (vh - bar_h).max(1.0);

    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(12, 12, 14, 200);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, pan_w, bg_h) {
        fill_rect_clipped(pixmap, r, &bg);
    }

    let mut hp = tiny_skia::Paint::default();
    hp.set_color_rgba8(60, 60, 70, 180);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, pan_w, 24.0 * s) {
        fill_rect_clipped(pixmap, r, &hp);
    }

    if let Some(font) = &font {
        draw_text_on_pixmap(pixmap, &def.name, px + 6.0 * s, 16.0 * s, 12.0 * s, font);
    }

    let cell_h = 22.0 * s;
    let mut y = 28.0 * s;

    for (label, val, span, kind) in &rows {
        if *kind == 1 {
            // Regular separator line
            y += cell_h * 0.3;
            let mut sp = tiny_skia::Paint::default();
            sp.set_color_rgba8(50, 50, 60, 100);
            if let Some(r) = tiny_skia::Rect::from_xywh(px + 4.0 * s, y, pan_w - 8.0 * s, 1.0) {
                fill_rect_clipped(pixmap, r, &sp);
            }
            y += cell_h * 0.3;
            continue;
        }
        if *kind == 2 {
            // Section split: thicker divider + extra spacing
            y += cell_h * 0.4;
            let mut sp = tiny_skia::Paint::default();
            sp.set_color_rgba8(60, 60, 75, 160);
            if let Some(r) = tiny_skia::Rect::from_xywh(px + 2.0 * s, y, pan_w - 4.0 * s, 2.0 * s) {
                fill_rect_clipped(pixmap, r, &sp);
            }
            y += cell_h * 0.4;
            continue;
        }
        let row_h = cell_h * (*span as f32).max(1.0);
        let mut lp = tiny_skia::Paint::default();
        lp.set_color_rgba8(20, 20, 25, 120);
        if let Some(r) = tiny_skia::Rect::from_xywh(px, y, pan_w, row_h) {
            fill_rect_clipped(pixmap, r, &lp);
        }
        if let Some(font) = &font {
            draw_text_on_pixmap(pixmap, label, px + 8.0 * s, y + cell_h * 0.5, 10.0 * s, font);
            let vx = px + pan_w * 0.5 + 4.0 * s;
            draw_text_on_pixmap(pixmap, val, vx, y + cell_h * 0.5, 10.0 * s, font);
        }
        y += row_h;
    }
    // Selected event details (overlay at bottom of panel)
    if let Some(_ei) = info.selected_event_idx {
        let event_h = 28.0 * s;
        let ey = bg_h - event_h * 7.0 - 8.0 * s;
        if ey > y {
            let mut eb = tiny_skia::Paint::default();
            eb.set_color_rgba8(40, 42, 50, 230);
            if let Some(r) = tiny_skia::Rect::from_xywh(px, ey, pan_w, event_h * 7.0 + 8.0 * s) {
                fill_rect_clipped(pixmap, r, &eb);
            }
            let rows: [(&str, &str, u8); 6] = [
                ("Event", &info.ev_kind, 255),
                ("Start", &format!("{:.3}b", info.ev_start_beats), 0),
                ("End", &format!("{:.3}b", info.ev_end_beats), 1),
                ("From", &format!("{:.4}", info.ev_start_val), 2),
                ("To", &format!("{:.4}", info.ev_end_val), 3),
                ("Easing", &format!("{}", info.ev_easing), 4),
            ];
            if let Some(font) = &font {
                let mut ry = ey + 4.0 * s;
                for (label, val, tgt) in &rows {
                    let highlight = info.event_edit_target == *tgt;
                    if highlight {
                        let mut hp = tiny_skia::Paint::default();
                        hp.set_color_rgba8(60, 80, 120, 60);
                        if let Some(r) = tiny_skia::Rect::from_xywh(px + 4.0 * s, ry, pan_w - 8.0 * s, event_h) {
                            fill_rect_clipped(pixmap, r, &hp);
                        }
                    }
                    draw_text_on_pixmap(pixmap, label, px + 8.0 * s, ry + cell_h * 0.5, 10.0 * s, font);
                    draw_text_on_pixmap(pixmap, val, px + pan_w * 0.5 + 4.0 * s, ry + cell_h * 0.5, 10.0 * s, font);
                    ry += event_h;
                }
            }
        }
    }
}

/// Quick toolbar (left side, tool buttons).
pub(crate) fn draw_quick_panel(pixmap: &mut tiny_skia::PixmapMut, _tool_hover: Option<usize>, selected_tool: usize, hp: [f32; 5], info: &GameInfo, qp_w: f32, vw: f32, vh: f32, s: f32) {
    if !info.show_overlay { return; }
    if vh < 10.0 || vw < 10.0 { return; }
    let font = get_font();
    let bar_h = 48.0 * s;
    let max_hp = hp.iter().cloned().reduce(f32::max).unwrap_or(0.0);
    let bg_extra = max_hp * 100.0 * s;
    let bg_w = qp_w + bg_extra;
    let bg_h = (vh - bar_h).max(1.0);

    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(18, 18, 22, 230);
    if let Some(r) = tiny_skia::Rect::from_xywh(0.0, 0.0, bg_w, bg_h) {
        fill_rect_clipped(pixmap, r, &bg);
    }

    let tools: [(&str, [u8; 3]); 5] = [
        ("Chart",  [100, 200, 255]),
        ("Line",   [100, 220, 100]),
        ("Settings", [180, 180, 190]),
        ("Eff",    [255, 150, 100]),
        ("BPM",    [220, 180, 255]),
    ];

    let btn_base = 34.0 * s;
    let gap = 6.0 * s;

    for (i, (name, color)) in tools.iter().enumerate() {
        let p = hp[i];
        let y = gap + i as f32 * (btn_base + gap);
        let extra = p * 80.0 * s;
        let bw = btn_base + extra;
        let bx = 4.0 * s; // left-align within panel

        let mut bp = tiny_skia::Paint::default();
        let a = if i == selected_tool { 230 } else { (80.0 + p * 100.0) as u8 };
        bp.set_color_rgba8(color[0], color[1], color[2], a);
        if let Some(rect) = tiny_skia::Rect::from_xywh(bx, y, bw, btn_base) {
            fill_rect_clipped(pixmap, rect, &bp);
        }

        // Label text on button when hovered (skip if no font)
        if p > 0.3 {
            if let Some(font) = font {
                let lx = bx + 6.0 * s;
                let ly = y + btn_base * 0.5 + 4.0 * s;
                draw_text_on_pixmap(pixmap, name, lx, ly, 11.0 * s, font);
            }
        }
    }
}

// ── Display data ──







