//! BPM 面板:自研组件库(widgets.rs)的第一个正式接入试点。
//!
//! 用 [`RealtimeForm`] 承载 BPM 列表:每行 label = 起始拍(beat),Number
//! 控件 = BPM 值。绘制/命中/交互全部走组件库的 areas/hit/draw/on_*,
//! 不再维护"绘制几何"与"命中几何"两套魔法数字。
//!
//! 本模块只依赖组件库与 ui 绘制原语,**不依赖 core 类型**(数据以
//! `(beat, bpm)` 元组进出),因此 measure/ui_kit 的 `#[path]` 副本也能编译。

use super::widgets::{Canvas, RealtimeForm, RTControl, Theme, Widget};
use super::primitives::fill_rect_clipped;
use super::text::draw_text_on_pixmap;

/// 从 `(beat, bpm)` 行数据构建 BPM 面板组件。
/// `x/y/w`: 面板矩形(逻辑坐标,已含 gui_scale);`focus_row` 保留交互焦点。
pub fn build_form(
    x: f32,
    y: f32,
    w: f32,
    rows: &[(f64, f64)],
    focus_row: Option<usize>,
    s: f32,
) -> RealtimeForm {
    let rows: Vec<(String, RTControl)> = rows.iter().map(|(beat, bpm)| {
        (
            format!("@{beat:.3}"),
            RTControl::Number {
                value: *bpm,
                step: 1.0,
                min: 1.0,
                max: 1000.0,
                last_x: 0.0,
                buf: None,
            },
        )
    }).collect();
    let mut form = RealtimeForm::new(x, y, w, "BPM", rows);
    form.row_h = 24.0 * s;
    form.gap = 4.0 * s;
    form.focus_row = focus_row;
    form
}

/// 从表单提取当前 `(beat, bpm)` 行数据(回写用)。
pub fn rows_of(form: &RealtimeForm) -> Vec<(f64, f64)> {
    form.rows.iter().map(|(label, ctrl)| {
        let beat = label.trim_start_matches('@').parse::<f64>().unwrap_or(0.0);
        let bpm = match ctrl {
            RTControl::Number { value, .. } => *value,
            _ => 0.0,
        };
        (beat, bpm)
    }).collect()
}

/// tiny_skia Canvas 适配(主程序绘制后端)。
pub(crate) struct SkiaCanvas<'a> {
    pub pm: &'a mut tiny_skia::PixmapMut<'a>,
}

impl<'a> Canvas for SkiaCanvas<'a> {
    fn fill(&mut self, r: tiny_skia::Rect, rgba: [u8; 4]) {
        let mut p = tiny_skia::Paint::default();
        p.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
        fill_rect_clipped(self.pm, r, &p);
    }

    fn text(&mut self, s: &str, x: f32, y: f32, size: f32, rgb: [u8; 3]) {
        if let Some(font) = super::font::get_font() {
            draw_text_on_pixmap(self.pm, s, x, y, size, font);
        }
    }

    fn text_width(&mut self, s: &str, size: f32) -> f32 {
        let fonts = super::font::get_fonts();
        s.chars().map(|ch| {
            let f = fonts.iter().find(|f| f.has_glyph(ch)).or_else(|| fonts.first());
            f.map(|f| f.metrics(ch, size).advance_width).unwrap_or(0.0)
        }).sum()
    }
}

/// 绘制 BPM 面板(挂在 overlay 的 tool 4 分支)。
pub(crate) fn draw_bpm_panel<'a>(
    pixmap: &'a mut tiny_skia::PixmapMut<'a>,
    form: &RealtimeForm,
    hover: Option<&super::widgets::Area>,
    s: f32,
) {
    let theme = Theme::default().scaled(s);
    let mut cv = SkiaCanvas { pm: pixmap };
    form.draw(&mut cv, &theme, hover);
    form.draw_overlay(&mut cv, &theme, hover);
}

