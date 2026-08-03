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
use crate::render::shaders::EFFECTS;

/// 从 extra.json 的活跃特效构建渲染链(按优先级排序,已由
/// [`crate::core::extra::evaluate_effects`] 排序)。
pub fn build_effect_chain(
    extra: &ExtraRoot,
    chart_beat: f64,
    chart_time: f64,
    screen: (f32, f32),
) -> Vec<ActiveEffect> {
    let evals = crate::core::extra::evaluate_effects(extra, chart_beat);
    evals.iter().map(|e| resolve_one(e, chart_time, screen)).collect()
}

/// 单个特效 → ActiveEffect(uniform 解析 + 内置 uniform 注入)。
fn resolve_one(e: &EvalEffect, chart_time: f64, screen: (f32, f32)) -> ActiveEffect {
    let (sw, sh) = screen;
    let si = EFFECTS.iter().position(|d| d.name == e.shader_name).unwrap_or(usize::MAX);
    let (uniform_values, uniform_count) = if si == usize::MAX {
        // 自定义 shader:用 extra.json 原始 uniform。
        (e.uniforms.clone(), e.uniforms.len())
    } else {
        resolve_builtin(e, si, chart_time, sw, sh)
    };
    ActiveEffect {
        shader_idx: si,
        custom_name: if si == usize::MAX { Some(e.shader_name.clone()) } else { None },
        priority: e.priority,
        uniform_values,
        uniform_count,
    }
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
}
