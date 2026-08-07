//! Eff 面板(tool 3):迁移到组件库 RealtimeForm(PMCORE-59)。
//!
//! 面板 = 单个 [`RealtimeForm`]:前 n 行 = 效果列表(点击选中,main.rs 拦截),
//! 之后 = 选中效果的字段行 Shader(Combo)/Start(Number)/End(Number)/
//! Global(Toggle)/每个 uniform 变量(Number;keyframed 数组为只读 Text)。
//! keyframe 展开区(KfRow)保留手写 tiny_skia 绘制 + 独立命中,不迁移。
//!
//! 与 BPM/设置面板同构:main.rs 每帧 `build_form(prev)` 重建,prev 保留
//! 焦点行 / Number 打字缓冲 / Combo 展开(跨帧持有);交互后 `eff_apply`
//! 写回 extra.json。

use super::font::get_font;
use super::flow;
use super::model::GameInfo;
use super::primitives::fill_rect_clipped;
use super::text::draw_text_on_pixmap;
use super::widgets::{RealtimeForm, RTControl, Widget};

/// 效果列表行数据。ui 模块在 main/measure 双副本编译,不能依赖 bin 私有的
/// `core` 模块——与 bpm_panel 的 `(beat, bpm)` 元组同策略,数据以纯类型进出。
#[derive(Clone)]
pub struct EffListRow {
    pub shader: String,
    pub start: f64,
    pub end: f64,
    pub global: bool,
}

/// 选中效果的一个 uniform 变量行。
pub enum VarRow {
    /// 普通数值(Number 行,滚轮/打字编辑)。
    Number { name: String, value: f64 },
    /// keyframed 数组(只读 Text 行,点击展开/收起 keyframe 区)。
    Keyframed { name: String, n: usize },
    /// 其它 JSON 值(只读)。
    Other { name: String },
}

/// 用户可选 shader 名:内置 [`phimakor::render::shaders::EFFECTS`](排除内部
/// 合成阶段,如 circleBlurH/V 等)+ 谱面 extra.json 引用的自定义 GLSL shader
/// (`custom_shaders`)。
pub fn shader_items(custom_shaders: &[String]) -> Vec<String> {
    let effects = phimakor::render::shaders::EFFECTS;
    let stage_names: std::collections::HashSet<&str> =
        effects.iter().flat_map(|d| d.stages.iter().copied()).collect();
    let mut items: Vec<String> = effects.iter()
        .filter(|d| !stage_names.contains(d.name))
        .map(|d| d.name.to_string())
        .collect();
    for name in custom_shaders {
        if !items.contains(name) {
            items.push(name.clone());
        }
    }
    items
}

/// 构建 Eff 面板表单。
/// `effects` = 按 start beat 排序的效果列表行;`selected` = 选中效果行索引;
/// `vars` = 选中效果的排序 uniform 变量行;`kf_var` = 展开的 keyframed var
/// (其行显示 ▶);`snap` = Start/End 滚轮步进;`prev` = 上一帧表单(保留
/// 焦点行 / Number 打字缓冲 / Combo 展开,跨帧持有)。
pub fn build_form(
    x: f32,
    y: f32,
    w: f32,
    s: f32,
    effects: &[EffListRow],
    selected: Option<usize>,
    vars: &[VarRow],
    kf_var: Option<usize>,
    snap: f64,
    custom_shaders: &[String],
    prev: Option<&RealtimeForm>,
) -> RealtimeForm {
    let mut rows: Vec<(String, RTControl)> = Vec::new();
    // 效果列表行(点击选中由 main.rs 拦截处理)。
    for e in effects {
        let tag = if e.global { " [G]" } else { "" };
        let range = format!("{:.1}~{:.1}b", e.start, e.end);
        rows.push((
            format!("{}{}", e.shader, tag),
            RTControl::Text { value: range, insert: 0, caret: 0.0 },
        ));
    }
    // 选中效果的字段行:Shader(Combo)/Start/End(Number,步进 snap)/
    // Global(Toggle)/每个 uniform 变量(Number 或 keyframed 只读 Text)。
    if let Some(sel) = selected {
        if let Some(e) = effects.get(sel) {
        let mut items = shader_items(custom_shaders);
        let shader_sel = match items.iter().position(|n| *n == e.shader) {
            Some(p) => p,
            None => {
                // 当前 shader 不在列表(自定义文件缺失等):补进去,
                // 保证 Combo 显示与选中值一致。
                items.push(e.shader.clone());
                items.len() - 1
            }
        };
        rows.push(("Shader".into(), RTControl::Combo { items, selected: shader_sel, open: false }));
        let step = snap.max(0.01);
        rows.push(("Start".into(), RTControl::Number { value: e.start, step, min: 0.0, max: 1e9, last_x: 0.0, buf: None }));
        rows.push(("End".into(), RTControl::Number { value: e.end, step, min: 0.0, max: 1e9, last_x: 0.0, buf: None }));
        rows.push(("Global".into(), RTControl::Toggle {
            on: e.global,
            anim: if e.global { 1.0 } else { 0.0 },
            dir: if e.global { 1.0 } else { -1.0 },
        }));
        for (vi, v) in vars.iter().enumerate() {
            match v {
                VarRow::Number { name, value } => {
                    rows.push((name.clone(), RTControl::Number {
                        value: *value,
                        step: 0.1, min: -1e9, max: 1e9, last_x: 0.0, buf: None,
                    }));
                }
                VarRow::Keyframed { name, n } => {
                    let disp = if *n == 0 { "…".to_string() } else { format!("{n} kf") };
                    let disp = if kf_var == Some(vi) { format!("▶ {disp}") } else { disp };
                    rows.push((name.clone(), RTControl::Text { value: disp, insert: 0, caret: 0.0 }));
                }
                VarRow::Other { name } => {
                    rows.push((name.clone(), RTControl::Text { value: "…".into(), insert: 0, caret: 0.0 }));
                }
            }
        }
        }
    }
    let mut form = RealtimeForm::new(x, y, w, format!("Effects ({})", effects.len()), rows);
    form.row_h = 24.0 * s;
    form.gap = 4.0 * s;
    // prev 保留模式:焦点行 / Number 打字缓冲 / Combo 展开跨帧持有
    // (RealtimeForm 每帧重建会丢这些状态)。
    if let Some(prev) = prev {
        form.focus_row = prev.focus_row;
        for (a, b) in form.rows.iter_mut().zip(prev.rows.iter()) {
            match (&mut a.1, &b.1) {
                (RTControl::Combo { open, .. }, RTControl::Combo { open: po, .. }) => *open = *po,
                (RTControl::Number { buf, last_x, .. }, RTControl::Number { buf: pb, last_x: plx, .. }) => {
                    *buf = pb.clone();
                    *last_x = *plx;
                }
                _ => {}
            }
        }
    }
    // 无交互焦点时,默认焦点 = 选中效果列表行:组件 draw 对 focus_row 行
    // 高亮(等价旧手写面板的选中行高亮)。选中行是 Text 行,焦点下打字
    // 只会临时改其 value(下一帧重建恢复),滚轮/Enter 均无副作用。
    if selected.is_some() && prev.map_or(true, |p| p.focus_row.is_none()) {
        form.focus_row = selected;
    }
    form
}

// ── keyframe 手写区(不迁移,PMCORE-59)──

/// keyframe 手写区在流控中的区域 id 基址:`KF_AREA_BASE + 行索引`。
/// 与表单行 id(1..=rows+1,1000+…)不重叠。
pub const KF_AREA_BASE: u32 = 1_000_000;

/// keyframe 行区域 id → 行索引(命中 kf 区返回 Some;表单/其它区域 None)。
pub fn kf_index_from_area(id: flow::AreaId) -> Option<usize> {
    (id.0 >= KF_AREA_BASE).then(|| (id.0 - KF_AREA_BASE) as usize)
}

/// Eff 表单流控区域(命中 = 绘制,单一真源):列表行 / Shader Combo /
/// Start / End / Global / var 行(id = 行索引+1)、Combo 弹出项(1000+…)、
/// Add 按钮(rows+1),几何委托组件库 [`Widget::areas`](RealtimeForm::areas)
/// (widgets.rs 为 DevMenuBase 领地,只读,这里仅做 Area → HotArea 转换)。
/// 展开的 keyframe 手写区(kf_rows > 0 时)每行注册一个区域,点击选中 /
/// 双击编辑命中统一走 flow(声明:该区保留手写绘制,是 P3 菜单系统的
/// 无关区)。
pub fn form_areas(form: &RealtimeForm, s: f32, kf_rows: usize) -> Vec<flow::HotArea> {
    use flow::{AreaId, AreaKind, HotArea};
    let mut out: Vec<HotArea> = form
        .areas()
        .into_iter()
        .filter(|a| a.id != 0) // 标题(id=0)不可交互,不注册
        .map(|a| HotArea {
            id: AreaId(a.id),
            rect: (a.rect.x(), a.rect.y(), a.rect.width(), a.rect.height()),
            kind: AreaKind::Widget(a.kind),
            disabled: false,
        })
        .collect();
    // keyframe 手写区(展开时):每行一个区域。几何与 draw_kf_area 同源
    // (kf_area_y + row_h)。
    if kf_rows > 0 {
        let y0 = kf_area_y(form, s);
        for ki in 0..kf_rows {
            out.push(HotArea {
                id: AreaId(KF_AREA_BASE + ki as u32),
                rect: (form.x, y0 + ki as f32 * form.row_h, form.w, form.row_h),
                kind: AreaKind::Widget(super::widgets::AreaKind::ListRow),
                disabled: false,
            });
        }
    }
    out
}

/// 表单底部(Add 按钮下缘),keyframe 手写区从这开始。
fn form_bottom(form: &RealtimeForm) -> f32 {
    form.y + form.row_h + form.rows.len() as f32 * (form.row_h + form.gap) + form.row_h
}

/// keyframe 手写区的起始 y(表单 Add 按钮下方)。
pub fn kf_area_y(form: &RealtimeForm, s: f32) -> f32 {
    form_bottom(form) + 6.0 * s
}

/// 手写绘制展开的 keyframe 行(行为与迁移前一致:E{id} start~end v1→v2,
/// 选中高亮,双击编辑时显示打字 buf)。
pub fn draw_kf_area(pm: &mut tiny_skia::PixmapMut, info: &GameInfo, form: &RealtimeForm, s: f32) {
    let n_kf = info.eff_kf_rows.len();
    if n_kf == 0 { return; }
    let Some(font) = get_font() else { return };
    let x0 = form.x;
    let y0 = kf_area_y(form, s);
    let w = form.w;
    let row_h = form.row_h;
    let mut bg = tiny_skia::Paint::default();
    bg.set_color_rgba8(30, 32, 40, 230);
    if let Some(r) = tiny_skia::Rect::from_xywh(x0, y0 - 4.0 * s, w, n_kf as f32 * row_h + 8.0 * s) {
        fill_rect_clipped(pm, r, &bg);
    }
    let mut ry = y0;
    for (ki, k) in info.eff_kf_rows.iter().enumerate() {
        if info.eff_kf_sel == Some(ki) {
            let mut hp = tiny_skia::Paint::default();
            hp.set_color_rgba8(60, 90, 140, 90);
            if let Some(r) = tiny_skia::Rect::from_xywh(x0 + 4.0 * s, ry, w - 8.0 * s, row_h) {
                fill_rect_clipped(pm, r, &hp);
            }
        }
        // 双击编辑的 keyframe 行显示打字缓冲(字段编码 100+ki 起点 /
        // 101+ki 终点,与 main.rs NumTarget::Kf 编码一致)。
        let is_kf_edit = info.num_edit.as_ref().is_some_and(|(f, _)| *f >= 100);
        let shown = if is_kf_edit {
            let (f, buf) = info.num_edit.as_ref().unwrap();
            if *f as usize == 100 + ki {
                format!("{buf}|")
            } else {
                format!("{:.3}", if *f as usize == 101 + ki { k.end_beats } else { k.start_beats })
            }
        } else {
            format!("E{} {:.2}~{:.2} {:.2}→{:.2}", k.easing, k.start_beats, k.end_beats, k.v1, k.v2)
        };
        draw_text_on_pixmap(pm, &format!("kf{ki}"), x0 + 8.0 * s, ry + row_h * 0.5, 10.0 * s, font);
        draw_text_on_pixmap(pm, &shown, x0 + w * 0.5 + 4.0 * s, ry + row_h * 0.5, 10.0 * s, font);
        ry += row_h;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_items_filters_stages_and_appends_customs() {
        let items = shader_items(&[]);
        // 内置用户可选:circleBlur 在,内部阶段 circleBlurH/V 不在。
        assert!(items.contains(&"grayscale".to_string()));
        assert!(items.contains(&"circleBlur".to_string()));
        assert!(!items.contains(&"circleBlurH".to_string()));
        assert!(!items.contains(&"circleBlurV".to_string()));
        // 自定义 GLSL shader(extra.json 引用)追加、去重。
        let items = shader_items(&["myglow.frag".to_string(), "myglow.frag".to_string()]);
        assert_eq!(items.iter().filter(|n| *n == "myglow.frag").count(), 1);
        assert!(items.contains(&"myglow.frag".to_string()));
    }

    #[test]
    fn build_form_row_layout_title_and_selected_focus() {
        let effects = vec![
            EffListRow { shader: "grayscale".into(), start: 0.0, end: 4.0, global: true },
            EffListRow { shader: "noise".into(), start: 4.0, end: 8.0, global: false },
        ];
        let vars = vec![
            VarRow::Number { name: "factor".into(), value: 1.0 },
            VarRow::Keyframed { name: "seed".into(), n: 3 },
        ];
        let form = build_form(0.0, 0.0, 300.0, 1.0, &effects, Some(0), &vars, None, 0.25, &[], None);
        assert_eq!(form.title, "Effects (2)");
        assert_eq!(form.rows.len(), 2 + 4 + 2);
        // 行序列:列表行 → Shader(Combo)/Start/End(Number)/Global(Toggle)/vars。
        assert_eq!(form.rows[0].0, "grayscale [G]");
        assert!(matches!(form.rows[0].1, RTControl::Text { .. }));
        assert_eq!(form.rows[2].0, "Shader");
        assert!(matches!(form.rows[2].1, RTControl::Combo { .. }));
        assert_eq!(form.rows[3].0, "Start");
        assert!(matches!(form.rows[3].1, RTControl::Number { .. }));
        assert_eq!(form.rows[5].0, "Global");
        assert!(matches!(form.rows[5].1, RTControl::Toggle { .. }));
        assert_eq!(form.rows[6].0, "factor");
        assert!(matches!(form.rows[6].1, RTControl::Number { .. }));
        assert_eq!(form.rows[7].0, "seed");
        assert!(matches!(form.rows[7].1, RTControl::Text { .. }));
        // 无交互焦点时默认焦点 = 选中行(组件高亮 = 旧面板的选中态)。
        assert_eq!(form.focus_row, Some(0));
        // 未选中效果:只渲染列表行。
        let form = build_form(0.0, 0.0, 300.0, 1.0, &effects, None, &vars, None, 0.25, &[], None);
        assert_eq!(form.rows.len(), 2);
        assert_eq!(form.focus_row, None);
    }

    #[test]
    fn build_form_prev_preserves_focus_buf_and_combo_open() {
        let effects = vec![EffListRow { shader: "grayscale".into(), start: 0.0, end: 4.0, global: true }];
        let vars = vec![VarRow::Number { name: "factor".into(), value: 1.0 }];
        // 单个效果时行布局:0=列表行,1=Shader,2=Start,3=End,4=Global,5=factor。
        let mut prev = build_form(0.0, 0.0, 300.0, 1.0, &effects, Some(0), &vars, None, 0.25, &[], None);
        // 模拟:焦点在 Start 行(索引 2),打字缓冲已开,Shader Combo 展开。
        prev.focus_row = Some(2);
        if let RTControl::Number { buf, .. } = &mut prev.rows[2].1 {
            *buf = Some("12.5".to_string());
        }
        if let RTControl::Combo { open, .. } = &mut prev.rows[1].1 {
            *open = true;
        }
        let form = build_form(0.0, 0.0, 300.0, 1.0, &effects, Some(0), &vars, None, 0.25, &[], Some(&prev));
        assert_eq!(form.focus_row, Some(2));
        assert!(matches!(&form.rows[1].1, RTControl::Combo { open: true, .. }));
        assert!(matches!(&form.rows[2].1, RTControl::Number { buf: Some(b), .. } if b == "12.5"));
    }

    /// 流控区域(命中 = 绘制):行 id = 索引+1、Add = rows+1、Combo 展开项
    /// 1000+…、keyframe 区 KF_AREA_BASE+ki;标题(0)不注册。
    #[test]
    fn form_areas_registers_rows_add_combo_and_kf() {
        let effects = vec![
            EffListRow { shader: "grayscale".into(), start: 0.0, end: 4.0, global: true },
            EffListRow { shader: "noise".into(), start: 4.0, end: 8.0, global: false },
        ];
        let vars = vec![VarRow::Number { name: "factor".into(), value: 1.0 }];
        let mut form = build_form(0.0, 0.0, 300.0, 1.0, &effects, Some(0), &vars, None, 0.25, &[], None);
        // 展开 Shader Combo(行 2),模拟 on_click 后的 open 态。
        if let RTControl::Combo { open, .. } = &mut form.rows[2].1 {
            *open = true;
        }
        let areas = form_areas(&form, 1.0, 3);
        // 标题(id=0)不注册;行 + Add + Combo 展开项 + 3 kf。
        let n_combo = if let RTControl::Combo { items, .. } = &form.rows[2].1 { items.len() } else { 0 };
        assert!(!areas.iter().any(|a| a.id.0 == 0));
        assert_eq!(areas.len(), form.rows.len() + 1 + n_combo + 3);
        assert!(areas.iter().any(|a| a.id == flow::AreaId(1))); // 列表行 0
        assert!(areas.iter().any(|a| a.id.0 == form.rows.len() as u32 + 1)); // Add
        assert!(areas.iter().any(|a| a.id.0 >= 1000 && a.id.0 < 2000)); // Combo 项
        // keyframe 区:KF_AREA_BASE + 行索引,几何 = kf_area_y 起、row_h 高。
        let y0 = kf_area_y(&form, 1.0);
        for ki in 0..3 {
            let a = areas.iter().find(|a| a.id == flow::AreaId(KF_AREA_BASE + ki as u32)).unwrap();
            assert_eq!(a.rect, (0.0, y0 + ki as f32 * form.row_h, 300.0, form.row_h));
        }
        assert_eq!(kf_index_from_area(flow::AreaId(KF_AREA_BASE + 1)), Some(1));
        assert_eq!(kf_index_from_area(flow::AreaId(5)), None);
        // 未展开 kf:不注册 kf 区。
        let areas = form_areas(&form, 1.0, 0);
        assert!(!areas.iter().any(|a| a.id.0 >= KF_AREA_BASE));
    }

    #[test]
    fn kf_area_geometry_below_form() {
        let effects = vec![EffListRow { shader: "grayscale".into(), start: 0.0, end: 4.0, global: true }];
        let form = build_form(100.0, 56.0, 300.0, 1.0, &effects, Some(0), &[], None, 0.25, &[], None);
        let bottom = form.y + form.row_h + form.rows.len() as f32 * (form.row_h + form.gap) + form.row_h;
        assert_eq!(kf_area_y(&form, 1.0), bottom + 6.0);
        // kf 行几何 = kf_area_y 起、row_h 高(流控区域与绘制同源)。
        let y0 = kf_area_y(&form, 1.0);
        let areas = form_areas(&form, 1.0, 3);
        let kf: Vec<_> = areas.iter().filter(|a| a.id.0 >= KF_AREA_BASE).collect();
        assert_eq!(kf.len(), 3);
        assert_eq!(kf[0].rect, (100.0, y0, 300.0, form.row_h));
        assert_eq!(kf[2].rect.1, y0 + 2.0 * form.row_h);
        assert_eq!(kf_index_from_area(kf[0].id), Some(0));
        // 未展开:无 kf 区。
        assert!(form_areas(&form, 1.0, 0).iter().all(|a| a.id.0 < KF_AREA_BASE));
    }
}
