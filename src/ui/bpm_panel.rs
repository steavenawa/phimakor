//! BPM 面板:自研组件库(widgets.rs)的第一个正式接入试点。
//!
//! 用 [`RealtimeForm`] 承载 BPM 列表:每行 label = `b{beat} @ {sec}s`(起始拍 +
//! 换算秒),Number 控件 = BPM 值。绘制/命中/交互全部走组件库的
//! areas/hit/draw/on_*,不再维护"绘制几何"与"命中几何"两套魔法数字。
//!
//! 本模块只依赖组件库与 ui 绘制原语,**不依赖 core 类型**(数据以
//! `(beat, bpm)` 元组进出),因此 measure/ui_kit 的 `#[path]` 副本也能编译。

use super::flow;
use super::widgets::{Canvas, RealtimeForm, RTControl, Theme, Widget};
use super::primitives::fill_rect_clipped;
use super::text::draw_text_on_pixmap;

/// 从 `(beat, bpm)` 行数据构建 BPM 面板组件。
/// `x/y/w`: 面板矩形(逻辑坐标,已含 gui_scale);`secs[i]` 为第 i 行对应秒数
/// (越界按 0.0);`highlight` 为播放头所在行(驱动半透明高亮);`focus_row` 保留交互焦点。
pub fn build_form(
    x: f32,
    y: f32,
    w: f32,
    rows: &[(f64, f64)],
    secs: &[f64],
    highlight: Option<usize>,
    focus_row: Option<usize>,
    s: f32,
) -> RealtimeForm {
    let rows: Vec<(String, RTControl)> = rows.iter().enumerate().map(|(i, (beat, bpm))| {
        let sec = secs.get(i).copied().unwrap_or(0.0);
        (
            format!("b{beat:.2} @ {sec:.2}s"),
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
    let mut form = RealtimeForm::new(x, y, w, format!("BPM × {}", rows.len()), rows);
    form.row_h = 24.0 * s;
    form.gap = 4.0 * s;
    form.highlight_row = highlight;
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

/// BPM 表单流控区域(命中 = 绘制,单一真源):行 id = 索引+1,Add 按钮
/// id = rows+1。几何委托组件库 [`Widget::areas`](RealtimeForm::areas)
/// (widgets.rs 为 DevMenuBase 领地,只读,这里仅做 Area → HotArea 转换)。
pub fn form_areas(form: &RealtimeForm) -> Vec<flow::HotArea> {
    use flow::{AreaId, AreaKind, HotArea};
    form.areas()
        .into_iter()
        .filter(|a| a.id != 0) // 标题(id=0)不可交互,不注册
        .map(|a| HotArea {
            id: AreaId(a.id),
            rect: (a.rect.x(), a.rect.y(), a.rect.width(), a.rect.height()),
            kind: AreaKind::Widget(a.kind),
            disabled: false,
        })
        .collect()
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

    fn text(&mut self, s: &str, x: f32, y: f32, size: f32, _rgb: [u8; 3]) {
        if let Some(font) = super::font::get_font() {
            draw_text_on_pixmap(self.pm, s, x, y, size, font);
        }
    }

    fn text_width(&mut self, s: &str, size: f32) -> f32 {
        // 与绘制同走 font_for:CJK 字符用 CJK 字体度量,避免中文宽度
        // 按缺字形度量导致截断/居中错位(预热后零开销)。
        s.chars().map(|ch| {
            super::font::font_for(ch)
                .map(|f| f.metrics(ch, size).advance_width)
                .unwrap_or(0.0)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 流控区域(命中 = 绘制):BPM 行 id = 索引+1、Add = rows+1;标题(0)不注册。
    #[test]
    fn form_areas_registers_rows_and_add() {
        let rows = vec![(0.0, 120.0), (4.0, 90.0), (8.0, 150.0)];
        let secs = vec![0.0, 2.5, 5.0];
        let form = build_form(0.0, 0.0, 300.0, &rows, &secs, Some(1), None, 1.0);
        let areas = form_areas(&form);
        assert_eq!(form.title, "BPM × 3");
        assert_eq!(form.rows[0].0, "b0.00 @ 0.00s");
        assert_eq!(form.rows[1].0, "b4.00 @ 2.50s");
        assert_eq!(form.highlight_row, Some(1));
        assert!(!areas.iter().any(|a| a.id.0 == 0));
        assert_eq!(areas.len(), 3 + 1); // 3 行 + Add
        for i in 0..3 {
            let a = areas.iter().find(|a| a.id.0 == i as u32 + 1).unwrap();
            assert_eq!(a.rect.0, 0.0);
            assert_eq!(a.rect.2, 300.0);
            // 行几何与组件库 row_rect 一致(命中 = 绘制)。
            let expect_y = 0.0 + 24.0 + i as f32 * (24.0 + 4.0);
            assert!((a.rect.1 - expect_y).abs() < 1e-4);
        }
        assert!(areas.iter().any(|a| a.id.0 == 4)); // Add
    }
}

