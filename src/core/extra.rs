//! `extra.json` parser: post-processing effects, video overlays, BPM overrides.
//! Ported from `prpr/src/parse/extra.rs`.
//!
//! Each effect defines a shader name, active time range, and keyframed uniform
//! values. The evaluator resolves which effects are active at a given beat and
//! computes their current uniform values.

use crate::core::bpm::Triple;
use serde::Deserialize;
use std::collections::HashMap;

// ── Serde models matching extra.json ──

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraRoot {
    #[serde(default)]
    pub bpm: Vec<ExtraBpmItem>,
    #[serde(default)]
    pub effects: Vec<ExtraEffect>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraBpmItem {
    pub time: Triple,
    pub bpm: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraEffect {
    pub start: Triple,
    pub end: Triple,
    pub shader: String,
    #[serde(default)]
    pub global: bool,
    #[serde(default)]
    pub priority: u32,
    #[serde(default)]
    pub vars: HashMap<String, serde_json::Value>,
}

// ── Evaluated effect instance ──

/// A resolved, active effect ready for the GPU pipeline.
pub struct EvalEffect {
    pub shader_name: String,
    pub start_beat: f64,
    pub end_beat: f64,
    pub global: bool,
    pub priority: u32,
    /// Flat uniform values computed at the current beat.
    pub uniforms: Vec<f32>,
    /// Variable names corresponding to each uniform value.
    pub uniforms_names: Vec<String>,
}

/// Load and parse extra.json from a byte slice.
pub fn parse_extra(bytes: &[u8]) -> Result<ExtraRoot, String> {
    serde_json::from_slice(bytes).map_err(|e| format!("extra.json parse: {e}"))
}

/// Evaluate active effects at a given beat position.
/// Returns effects sorted by priority (lowest first).
pub fn evaluate_effects(root: &ExtraRoot, beat: f64) -> Vec<EvalEffect> {
    let _s = crate::trace_span!("evaluate_effects");
    let mut active: Vec<EvalEffect> = Vec::new();
    for ef in &root.effects {
        let sb = ef.start.beats();
        let eb = ef.end.beats();
        if beat < sb || beat > eb { continue; }
        let (uniforms, uniforms_names) = resolve_uniforms(&ef.vars, beat);
        active.push(EvalEffect {
            shader_name: ef.shader.clone(),
            start_beat: sb,
            end_beat: eb,
            global: ef.global,
            priority: ef.priority,
            uniforms,
            uniforms_names,
        });
    }
    active.sort_by_key(|e| e.priority);
    active
}

/// Resolve all uniform variables at a given beat.
/// Each variable can be a plain float, or an array of keyframes:
/// ```json
/// { "varName": { "startTime": [...], "endTime": [...], ... } }
/// ```
/// or:
/// ```json
/// { "varName": [ { "startTime": [...], ... }, ... ] }
/// ```
fn resolve_uniforms(vars: &HashMap<String, serde_json::Value>, beat: f64) -> (Vec<f32>, Vec<String>) {
    let mut out = Vec::new();
    let mut names = Vec::new();
    for key in sorted_keys(vars) {
        let val = &vars[&key];
        let resolved = resolve_value(val, beat);
        if resolved.len() == 1 {
            out.push(resolved[0]);
            names.push(key.clone());
        } else {
            // Multi-value var: append _0, _1, ... suffix
            for (i, v) in resolved.iter().enumerate() {
                out.push(*v);
                names.push(format!("{}_{}", key, i));
            }
        }
    }
    (out, names)
}

fn resolve_value(val: &serde_json::Value, beat: f64) -> Vec<f32> {
    match val {
        serde_json::Value::Number(n) => vec![n.as_f64().unwrap_or(0.0) as f32],
        serde_json::Value::Array(arr) => {
            // Array of keyframe objects or flat values
            if arr.iter().any(|v| v.is_object()) {
                let mut last_end = 0.0f64;
                let mut last_val = 0.0f32;
                for item in arr {
                    if let Some(obj) = item.as_object() {
                        let sb = obj.get("startTime").and_then(|t| parse_triple_value(t)).unwrap_or(0.0);
                        let eb = obj.get("endTime").and_then(|t| parse_triple_value(t)).unwrap_or(0.0);
                        let end_val = obj.get("end").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                        if beat >= sb && beat <= eb {
                            let start = obj.get("start").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                            let easing = obj.get("easingType").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                            let t = if eb > sb { ((beat - sb) / (eb - sb)) as f32 } else { 1.0 };
                            let t = apply_easing(t, easing);
                            return vec![start + (end_val - start) * t];
                        }
                        if eb <= beat && eb >= last_end { last_end = eb; last_val = end_val; }
                    }
                }
                vec![last_val] // hold last keyframe's end value
            } else {
                // Flat values (e.g. [0.5, 0.5] for a vec2)
                arr.iter().filter_map(|v| v.as_f64()).map(|v| v as f32).collect()
            }
        }
        serde_json::Value::Object(obj) => {
            // Single keyframe object
            let sb = obj.get("startTime").and_then(|t| parse_triple_value(t)).unwrap_or(0.0);
            let eb = obj.get("endTime").and_then(|t| parse_triple_value(t)).unwrap_or(0.0);
            let start = obj.get("start").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let end = obj.get("end").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let easing = obj.get("easingType").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let t = if eb > sb { ((beat - sb) / (eb - sb)) as f32 } else { 1.0 };
            let t = apply_easing(t, easing);
            vec![start + (end - start) * t]
        }
        _ => vec![0.0],
    }
}

fn parse_triple_value(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Array(arr) if arr.len() >= 2 => {
            let i = arr[0].as_i64()? as i32;
            let n = arr[1].as_i64()? as u32;
            let d = arr.get(2).and_then(|v| v.as_i64()).unwrap_or(1) as u32;
            Some((i as f64) + (n as f64) / (d as f64).max(1.0))
        }
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn sorted_keys(map: &HashMap<String, serde_json::Value>) -> Vec<String> {
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    keys
}

/// prpr easing types (subset). Ported from prpr easing constants.
fn apply_easing(t: f32, easing: i32) -> f32 {
    if t <= 0.0 { return 0.0; }
    if t >= 1.0 { return 1.0; }
    match easing {
        0 => t,                                    // linear
        1 => t * t,                                // ease_in_quad
        2 => 1.0 - (1.0 - t) * (1.0 - t),          // ease_out_quad
        3 => if t < 0.5 { 2.0 * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(2) / 2.0 }, // ease_in_out_quad
        4 => t * t * t,                            // ease_in_cubic
        5 => 1.0 - (1.0 - t).powi(3),               // ease_out_cubic
        6..=19 => ease_smoothstep(t, easing - 6),
        _ => t,
    }
}

/// prpr smoothstep variants (6→0, 7→1, ... 19→13).
fn ease_smoothstep(t: f32, variant: i32) -> f32 {
    let s = (variant as f32).max(0.0).min(13.0);
    if s == 0.0 { return t * t * (3.0 - 2.0 * t); }  // smoothstep
    // Higher-order smoothsteps: t^(s+2) * poly
    let n = s + 2.0;
    let p = t.powf(n);
    p / (p + (1.0 - t).powf(n))
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_grayscale_effect() {
        let json = r#"{
            "effects": [{
                "start": [0,0,1],
                "end": [10,0,1],
                "shader": "grayscale",
                "global": true,
                "vars": {
                    "factor": [{ "startTime": [0,0,1], "endTime": [10,0,1], "easingType": 0, "start": 0, "end": 1 }]
                }
            }]
        }"#;
        let root = parse_extra(json.as_bytes()).unwrap();
        let active = evaluate_effects(&root, 5.0);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].shader_name, "grayscale");
        assert!((active[0].uniforms[0] - 0.5).abs() < 0.01);
        assert_eq!(active[0].uniforms_names[0], "factor");
    }
}
