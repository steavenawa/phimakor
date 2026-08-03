//! Panel / effects / quick-toolbar drawing.

use super::model::{gameinfo_values, GameInfo};
use super::font::get_font;
use super::text::{draw_text_on_pixmap, text_width};
use super::primitives::fill_rect_clipped;
use super::panels;
use super::timeline::PANEL_W;
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

/// Eff panel interaction hit targets (geometry shared with
/// [`draw_effects_panel`] so draw/hit can't drift).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum EffHit {
    None,
    List(usize),
    Add,
    Del,
    Field(u8), // 0=shader, 1=start, 2=end, 3=global
}

/// Panel geometry constants (draw + hit-test share these).
fn eff_layout(vw: f32, vh: f32, s: f32, pp: f32) -> (f32, f32, f32, f32) {
    let pan_w = PANEL_W * s;
    let bar_h = 48.0 * s;
    let bg_h = (vh - bar_h).max(1.0);
    let props_x = vw - pp * pan_w;
    (props_x, pan_w, bg_h, bar_h)
}

const EFF_ROW_H: f32 = 22.0;

/// Hit-test the Eff panel (editor mode, tool 3). `n_rows` = effect list length.
/// The bottom edit block has 4 fixed fields (Shader/Start/End/Global) plus one
/// row per uniform variable (field ids 4+).
pub(crate) fn effects_hit_test(
    mx: f32, my: f32, vw: f32, vh: f32, s: f32, pp: f32, n_rows: usize, n_vars: usize,
) -> EffHit {
    let (px, pan_w, bg_h, _) = eff_layout(vw, vh, s, pp);
    if mx < px || mx > px + pan_w { return EffHit::None; }
    let row_h = EFF_ROW_H * s;
    let y0 = 52.0 * s;
    // List rows
    if my >= y0 && my < y0 + n_rows as f32 * row_h {
        let ri = ((my - y0) / row_h) as usize;
        if ri < n_rows { return EffHit::List(ri); }
    }
    // Add / Del buttons under the list
    let by = y0 + n_rows as f32 * row_h + 6.0 * s;
    if my >= by && my < by + row_h {
        if mx >= px + 8.0 * s && mx < px + 8.0 * s + 60.0 * s { return EffHit::Add; }
        if mx >= px + 72.0 * s && mx < px + 72.0 * s + 60.0 * s { return EffHit::Del; }
    }
    // Edit fields pinned to the bottom (only reachable when rows don't cover them)
    let n_edit = 4 + n_vars;
    let edit_h = n_edit as f32 * row_h + 8.0 * s;
    let ey = (bg_h - edit_h).max(y0);
    if my >= ey && my < ey + n_edit as f32 * row_h {
        let fi = ((my - ey) / row_h) as u8;
        if (fi as usize) < n_edit { return EffHit::Field(fi); }
    }
    EffHit::None
}

/// [Eff] panel: effect list + add/delete + per-field editing (wheel).
/// Layout must stay in sync with [`effects_hit_test`].
pub(crate) fn draw_effects_panel(pixmap: &mut tiny_skia::PixmapMut, info: &GameInfo, px: f32, _vw: f32, vh: f32, s: f32) {
    if !info.show_overlay { return; }
    if vh < 10.0 { return; }
    let font = get_font();
    let pan_w = PANEL_W * s;
    let bar_h = 48.0 * s;
    let bg_h = (vh - bar_h).max(1.0);
    let cell_h = EFF_ROW_H * s;

    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(12, 12, 14, 200);
    if let Some(r) = tiny_skia::Rect::from_xywh(px, 0.0, pan_w, bg_h) {
        fill_rect_clipped(pixmap, r, &bg);
    }

    let mut y = 28.0 * s;
    if let Some(font) = font {
        draw_text_on_pixmap(pixmap, &format!("Effects ({})", info.effects.len()), px + 8.0 * s, y, 12.0 * s, font);
    }
    y += 20.0 * s;

    // List rows (sorted by start beat)
    let list_top = y;
    if info.effects.is_empty() {
        if let Some(font) = font {
            draw_text_on_pixmap(pixmap, "(no effects — Add)", px + 12.0 * s, y, 10.0 * s, font);
        }
        y += cell_h;
    } else {
        for (ri, e) in info.effects.iter().enumerate() {
            let hovered = info.selected_effect == Some(ri);
            let mut lp = tiny_skia::Paint::default();
            if hovered {
                lp.set_color_rgba8(50, 80, 130, 200);
            } else if e.active {
                lp.set_color_rgba8(30, 45, 70, 180);
            } else {
                lp.set_color_rgba8(20, 20, 25, 120);
            }
            if let Some(r) = tiny_skia::Rect::from_xywh(px + 4.0 * s, y, pan_w - 8.0 * s, cell_h) {
                fill_rect_clipped(pixmap, r, &lp);
            }
            if let Some(font) = font {
                let tag = if e.global { " [G]" } else { "" };
                let label = format!("{}{}", e.shader, tag);
                draw_text_on_pixmap(pixmap, &label, px + 10.0 * s, y + cell_h * 0.5, 10.0 * s, font);
                let range = format!("{:.1}~{:.1}b", e.start_beats, e.end_beats);
                let tw = text_width(&range, 10.0 * s);
                draw_text_on_pixmap(pixmap, &range, px + pan_w - tw - 10.0 * s, y + cell_h * 0.5, 10.0 * s, font);
            }
            y += cell_h;
        }
    }

    // Add / Del buttons
    let by = y + 6.0 * s;
    for (idx, label) in [(0usize, "+ Add"), (1usize, "Del")] {
        let bx = px + 8.0 * s + idx as f32 * (68.0 * s);
        let mut bp = tiny_skia::Paint::default();
        bp.set_color_rgba8(40, 55, 75, 200);
        if let Some(r) = tiny_skia::Rect::from_xywh(bx, by, 60.0 * s, cell_h) {
            fill_rect_clipped(pixmap, r, &bp);
        }
        if let Some(font) = font {
            draw_text_on_pixmap(pixmap, label, bx + 6.0 * s, by + cell_h * 0.5, 10.0 * s, font);
        }
    }

    // Edit fields pinned to the bottom: 4 fixed rows + one row per uniform
    // variable (wheel-adjustable when the var is a plain number).
    if let Some(sel) = info.selected_effect {
        if let Some(e) = info.effects.get(sel) {
            let n_vars = e.vars.len();
            let n_edit = 4 + n_vars;
            let edit_h = n_edit as f32 * cell_h + 8.0 * s;
            let ey = (bg_h - edit_h).max(list_top);
            let mut eb = tiny_skia::Paint::default();
            eb.set_color_rgba8(30, 32, 40, 230);
            if let Some(r) = tiny_skia::Rect::from_xywh(px, ey, pan_w, edit_h) {
                fill_rect_clipped(pixmap, r, &eb);
            }
            let mut rows: Vec<(String, String, u8)> = vec![
                ("Shader".to_string(), e.shader.clone(), 0),
                ("Start".to_string(), format!("{:.3}b", e.start_beats), 1),
                ("End".to_string(), format!("{:.3}b", e.end_beats), 2),
                ("Global".to_string(), if e.global { "ON" } else { "OFF" }.to_string(), 3),
            ];
            for (vi, (name, val)) in e.vars.iter().enumerate() {
                rows.push((format!("{name}"), val.clone(), (4 + vi) as u8));
            }
            if let Some(font) = font {
                let mut ry = ey + 4.0 * s;
                for (label, val, tgt) in &rows {
                    if info.eff_edit_field == *tgt {
                        let mut hp = tiny_skia::Paint::default();
                        hp.set_color_rgba8(60, 90, 140, 90);
                        if let Some(r) = tiny_skia::Rect::from_xywh(px + 4.0 * s, ry, pan_w - 8.0 * s, cell_h) {
                            fill_rect_clipped(pixmap, r, &hp);
                        }
                    }
                    draw_text_on_pixmap(pixmap, label, px + 8.0 * s, ry + cell_h * 0.5, 10.0 * s, font);
                    draw_text_on_pixmap(pixmap, val, px + pan_w * 0.5 + 4.0 * s, ry + cell_h * 0.5, 10.0 * s, font);
                    ry += cell_h;
                }
            }
        }
    }
}

pub(crate) fn draw_quick_panel(pixmap: &mut tiny_skia::PixmapMut, _tool_hover: Option<usize>, selected_tool: usize, hp: [f32; 4], info: &GameInfo, qp_w: f32, vw: f32, vh: f32, s: f32) {
    let _s = trace_span!("draw_quick_panel");
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

    let tools: [(&str, [u8; 3]); 4] = [
        ("Chart",  [100, 200, 255]),
        ("Line",   [100, 220, 100]),
        ("Settings", [180, 180, 190]),
        ("Eff",    [255, 150, 100]),
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







