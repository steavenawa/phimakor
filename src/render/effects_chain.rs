//! FX 链构建:把"活跃特效(评估结果)"映射为渲染管线可消费的
//! [`ActiveEffect`] 链(PMCORE-64)。
//!
//! 职责:
//! - 按 shader 名查内置 [`EFFECTS`] 索引;自定义 shader 用原始 uniform
//! - 解析 uniform:内置特效用默认值打底 + 按名合并(剥离 `_r/_g/_b/_a/
//!   _x/_y` 后缀做大小写/下划线归一匹配),并注入 `screen_size`/`time`
//!   等内置 uniform(硬编码集中在此,不再散落 main.rs)
//!
//! main.rs 只调 [`build_effect_chain`] 一行。

use crate::core::extra::{EvalEffect, ExtraRoot};
use crate::render::post::ActiveEffect;
use crate::render::shaders::{EffectDef, EFFECTS};

/// 从 extra.json 的活跃特效构建渲染链(按优先级排序,已由
/// [`crate::core::extra::evaluate_effects`] 排序)。
pub fn build_effect_chain(
    extra: &ExtraRoot,
    chart_beat: f64,
    chart_time: f64,
    screen: (f32, f32),
) -> Vec<ActiveEffect> {
    let evals = crate::core::extra::evaluate_effects(extra, chart_beat);
    evals.iter().flat_map(|e| resolve_one(e, chart_time, screen)).collect()
}

/// 单个特效 → 1..N 个 ActiveEffect。
/// 复合特效(EffectDef::stages 非空,如 circleBlur = H+V 分离 max)展开成
/// 多个单 pass 条目;零强度特效在展开前整体跳过(避免 stage 白跑)。
fn resolve_one(e: &EvalEffect, chart_time: f64, screen: (f32, f32)) -> Vec<ActiveEffect> {
    let (sw, sh) = screen;
    let si = EFFECTS.iter().position(|d| d.name == e.shader_name).unwrap_or(usize::MAX);
    if si == usize::MAX {
        // 自定义 shader:用 extra.json 原始 uniform。
        return vec![ActiveEffect {
            shader_idx: usize::MAX,
            custom_name: Some(e.shader_name.clone()),
            priority: e.priority,
            uniform_values: e.uniforms.clone(),
            uniform_count: e.uniforms.len(),
        }];
    }
    let def = &EFFECTS[si];
    if effect_noop(e, def) {
        return vec![];
    }
    if def.stages.is_empty() {
        let (uv, uc) = resolve_builtin(e, si, chart_time, sw, sh);
        return vec![ActiveEffect {
            shader_idx: si,
            custom_name: None,
            priority: e.priority,
            uniform_values: uv,
            uniform_count: uc,
        }];
    }
    def.stages.iter().filter_map(|sname| {
        let ssi = EFFECTS.iter().position(|d| d.name == *sname)?;
        let (uv, uc) = resolve_builtin(e, ssi, chart_time, sw, sh);
        Some(ActiveEffect {
            shader_idx: ssi,
            custom_name: None,
            priority: e.priority,
            uniform_values: uv,
            uniform_count: uc,
        })
    }).collect()
}

/// 零强度特效判定(展开前,对原始特效):主参数 = 0 → 输出与输入相同。
fn effect_noop(e: &EvalEffect, def: &EffectDef) -> bool {
    let norm = |s: &str| s.to_lowercase().replace('_', "").replace('-', "");
    // 各内置特效的主强度参数(按名字归一匹配)。
    let main_param: Option<&str> = match def.name {
        "grayscale" => Some("factor"),
        "chromatic" => Some("power"),
        "glitch" => Some("power"),
        "fisheye" => Some("power"),
        "noise" => Some("power"),
        "radialBlur" => Some("power"),
        "pixel" => Some("size"),
        "circleBlur" | "circleBlurH" | "circleBlurV" => Some("size"),
        "vignette" => Some("color_a"),
        _ => None,
    };
    let Some(param) = main_param else { return false };
    let np = norm(param);
    e.uniforms_names.iter().zip(e.uniforms.iter())
        .find(|(n, _)| norm(n) == np)
        .is_some_and(|(_, v)| *v == 0.0)
}

/// 内置 shader:默认值打底,按名合并,注入 screen_size/time。
fn resolve_builtin(e: &EvalEffect, si: usize, chart_time: f64, sw: f32, sh: f32) -> (Vec<f32>, usize) {
    let def = &EFFECTS[si];
    let mut uv: Vec<f32> = def.defaults.iter().map(|(_, v)| *v).collect();
    let norm = |s: &str| s.to_lowercase().replace('_', "").replace('-', "");
    for (i, (dname, _)) in def.defaults.iter().enumerate() {
        // 剥离通道后缀(_r/_g/_b/_a/_x/_y)做归一匹配。
        let base = dname.trim_end_matches("_r").trim_end_matches("_g")
            .trim_end_matches("_b").trim_end_matches("_a")
            .trim_end_matches("_x").trim_end_matches("_y");
        let nbase = norm(base);
        if let Some(pos) = e.uniforms_names.iter().position(|n| norm(n) == nbase) {
            uv[i] = e.uniforms[pos];
        }
        // 内置 uniform 注入(硬编码集中在此)。
        if dname.contains("screen_size") {
            uv[i] = if dname.ends_with('x') { sw } else { sh };
        }
        if *dname == "time" {
            uv[i] = chart_time as f32;
        }
    }
    let l = uv.len();
    (uv, l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bpm::Triple;
    use crate::core::extra::ExtraEffect;

    fn effect(shader: &str, vars: Vec<(&str, f64)>) -> ExtraEffect {
        ExtraEffect {
            start: Triple::from_beats(0.0),
            end: Triple::from_beats(100.0),
            shader: shader.to_string(),
            global: true,
            priority: 0,
            vars: vars.into_iter()
                .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
                .collect(),
        }
    }

    #[test]
    fn builtin_uniforms_merged_and_injected() {
        // grayscale 的默认值 + 自定义强度 + time 注入。
        let extra = ExtraRoot { bpm: vec![], effects: vec![
            effect("grayscale", vec![("factor", 0.7), ("time", 3.5)]),
        ]};
        let chain = build_effect_chain(&extra, 5.0, 10.0, (1920.0, 1080.0));
        assert_eq!(chain.len(), 1);
        let fx = &chain[0];
        assert_ne!(fx.shader_idx, usize::MAX);
        // factor 合并生效(字母序 sorted_keys:factor 在 time 前)。
        assert!((fx.uniform_values[0] - 0.7).abs() < 1e-6, "factor merged: {:?}", fx.uniform_values);
        // time 注入(内置覆盖 extra 的 time)。
        let def = &EFFECTS[fx.shader_idx];
        if let Some(pos) = def.defaults.iter().position(|(n, _)| *n == "time") {
            assert!((fx.uniform_values[pos] - 10.0).abs() < 1e-6, "time injected: {:?}", fx.uniform_values);
        }
    }

    #[test]
    fn custom_shader_uses_raw_uniforms() {
        // uniforms 按 sorted_keys(字母序)排列:bar < foo。
        let extra = ExtraRoot { bpm: vec![], effects: vec![
            effect("custom.frag", vec![("foo", 1.5), ("bar", 2.5)]),
        ]};
        let chain = build_effect_chain(&extra, 5.0, 0.0, (0.0, 0.0));
        assert_eq!(chain.len(), 1);
        let fx = &chain[0];
        assert_eq!(fx.shader_idx, usize::MAX);
        assert_eq!(fx.custom_name.as_deref(), Some("custom.frag"));
        assert_eq!(fx.uniform_values, vec![2.5, 1.5]);
        assert_eq!(fx.uniform_count, 2);
    }

    #[test]
    fn inactive_effects_excluded() {
        let mut fx = effect("grayscale", vec![("intensity", 0.5)]);
        fx.start = Triple::from_beats(10.0);
        fx.end = Triple::from_beats(20.0);
        let extra = ExtraRoot { bpm: vec![], effects: vec![fx] };
        // beat 5 不在 10..20 内 → 空链
        assert!(build_effect_chain(&extra, 5.0, 0.0, (0.0, 0.0)).is_empty());
        // beat 15 在范围内
        assert_eq!(build_effect_chain(&extra, 15.0, 0.0, (0.0, 0.0)).len(), 1);
    }

    #[test]
    fn priority_order_preserved() {
        let mut a = effect("grayscale", vec![]);
        a.priority = 10;
        let mut b = effect("vignette", vec![]);
        b.priority = 2;
        let extra = ExtraRoot { bpm: vec![], effects: vec![a, b] };
        let chain = build_effect_chain(&extra, 5.0, 0.0, (0.0, 0.0));
        assert_eq!(chain.len(), 2);
        assert!(chain[0].priority <= chain[1].priority, "sorted by priority");
    }

    #[test]
    fn composite_circle_blur_expands_to_two_passes() {
        // circleBlur → circleBlurH + circleBlurV 两个单 pass。
        let extra = ExtraRoot { bpm: vec![], effects: vec![
            effect("circleBlur", vec![("size", 12.0)]),
        ]};
        let chain = build_effect_chain(&extra, 5.0, 0.0, (1920.0, 1080.0));
        assert_eq!(chain.len(), 2);
        let names: Vec<&str> = chain.iter().map(|fx| {
            let si = fx.shader_idx;
            if si == usize::MAX { "custom" } else { crate::render::shaders::EFFECTS[si].name }
        }).collect();
        assert_eq!(names, vec!["circleBlurH", "circleBlurV"]);
        // size 均匀传递到两个 stage。
        for fx in &chain {
            assert!((fx.uniform_values[0] - 12.0).abs() < 1e-6, "size propagated: {:?}", fx.uniform_values);
        }
    }

    #[test]
    fn composite_zero_size_skipped_before_expansion() {
        // size = 0 → 整个 circleBlur 不展开(零强度)。
        let extra = ExtraRoot { bpm: vec![], effects: vec![
            effect("circleBlur", vec![("size", 0.0)]),
        ]};
        let chain = build_effect_chain(&extra, 5.0, 0.0, (0.0, 0.0));
        assert!(chain.is_empty());
    }
}
