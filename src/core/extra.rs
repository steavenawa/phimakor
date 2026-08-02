//! `extra.json` parser: post-processing effects, video overlays, BPM overrides.
//! Ported from `prpr/src/parse/extra.rs`.
//!
//! Each effect defines a shader name, active time range, and keyframed uniform
//! values. The evaluator resolves which effects are active at a given beat and
//! computes their current uniform values.

use crate::core::bpm::Triple;
use crate::core::easing::{RPE_TWEEN_MAP, TWEEN_FUNCTIONS};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Serde models matching extra.json ──

/// Root data from extra.json, containing BPM overrides and effect definitions.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraRoot {
    /// BPM override timeline entries.
    #[serde(default)]
    pub bpm: Vec<ExtraBpmItem>,
    /// Post-processing effect definitions.
    #[serde(default)]
    pub effects: Vec<ExtraEffect>,
}

/// A BPM override entry, specifying a new BPM value at a given time signature.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraBpmItem {
    /// Time signature (measure, beat, division) at which the override takes effect.
    pub time: Triple,
    /// Target BPM value.
    pub bpm: f64,
}

/// A post-processing effect definition loaded from extra.json.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraEffect {
    /// Start time of the effect (measure, beat, division).
    pub start: Triple,
    /// End time of the effect (measure, beat, division).
    pub end: Triple,
    /// Name of the shader to apply.
    pub shader: String,
    /// Whether the effect applies globally to the entire frame.
    #[serde(default)]
    pub global: bool,
    /// Render priority (lower values are processed first).
    #[serde(default)]
    pub priority: u32,
    /// Keyframed uniform variable values.
    #[serde(default)]
    pub vars: HashMap<String, serde_json::Value>,
}

impl ExtraRoot {
    /// Write the root back to `path` as pretty JSON (used by the editor's
    /// Eff panel add/edit flows).
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("extra.json serialize: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("extra.json write: {e}"))
    }
}

// ── Evaluated effect instance ──

/// A resolved, active effect ready for the GPU pipeline.
pub struct EvalEffect {
    /// Name of the shader program to use.
    pub shader_name: String,
    /// Start beat (computed from time signature).
    pub start_beat: f64,
    /// End beat (computed from time signature).
    pub end_beat: f64,
    /// Whether the effect applies globally.
    pub global: bool,
    /// Render priority for ordering.
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

/// RPE `easingType` → engine tween curve.
///
/// Single source of truth: the main chart evaluator uses the same table, so
/// extra.json effects and chart events agree on easing semantics. (This file
/// used to carry its own table where 1 = quad-in — RPE 1 is linear — giving
/// two different interpolation results for the same chart.)
fn apply_easing(t: f32, easing: i32) -> f32 {
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    let idx = (easing as usize).max(1).min(RPE_TWEEN_MAP.len() - 1);
    TWEEN_FUNCTIONS[RPE_TWEEN_MAP[idx] as usize](t)
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

    #[test]
    fn extra_roundtrip_preserves_effects() {
        // The editor's Eff panel saves edits via serde — the roundtrip must
        // preserve effect timing/shader/vars.
        let root = ExtraRoot {
            bpm: vec![ExtraBpmItem { time: Triple::from_beats(2.0), bpm: 140.0 }],
            effects: vec![ExtraEffect {
                start: Triple::from_beats(4.0),
                end: Triple::from_beats(8.0),
                shader: "grayscale".to_string(),
                global: true,
                priority: 0,
                vars: HashMap::from([("factor".to_string(), serde_json::json!(0.5))]),
            }],
        };
        let json = serde_json::to_string(&root).unwrap();
        let back: ExtraRoot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.bpm.len(), 1);
        assert!((back.bpm[0].bpm - 140.0).abs() < 1e-9);
        assert_eq!(back.effects.len(), 1);
        let e = &back.effects[0];
        assert_eq!(e.shader, "grayscale");
        assert!((e.start.beats() - 4.0).abs() < 1e-9);
        assert!((e.end.beats() - 8.0).abs() < 1e-9);
        assert!(e.global);
        assert_eq!(e.vars["factor"], 0.5);
    }

    #[test]
    fn easing_matches_engine_table() {
        // RPE easingType 1 is linear (same curve as 0); this file used to map
        // 1 → quad-in, giving results that disagreed with the chart evaluator.
        for t in [0.1, 0.3, 0.5, 0.7, 0.9] {
            assert!((apply_easing(t, 1) - t).abs() < 1e-6, "easing 1 at {t}");
            assert!((apply_easing(t, 0) - apply_easing(t, 1)).abs() < 1e-6);
        }
        // Index 2 is sine-out in RPE_TWEEN_MAP, and 8 is cubic-out.
        let sine_out = TWEEN_FUNCTIONS[RPE_TWEEN_MAP[2] as usize];
        let cubic_out = TWEEN_FUNCTIONS[RPE_TWEEN_MAP[8] as usize];
        assert!((apply_easing(0.5, 2) - sine_out(0.5)).abs() < 1e-6);
        assert!((apply_easing(0.5, 8) - cubic_out(0.5)).abs() < 1e-6);
    }
}
