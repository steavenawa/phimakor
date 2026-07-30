// Derived from TeamFlos/phira prpr, GPL-3.0.
//! Serde models for RPE chart JSON and `info.json`. Pure data; lowering lives
//! in [`crate::core::chart`]. Field names/types mirror `prpr/src/parse/rpe.rs`
//! and `prpr/src/info.rs` exactly.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

use crate::core::bpm::Triple;

fn f32_zero() -> f32 {
    0.
}

fn f32_one() -> f32 {
    1.
}

fn i32_one() -> i32 {
    1
}

fn visible_time_default() -> f64 { 999999.0 }

fn rpe_version_default() -> i32 {
    160
}

/// `RPEVersion` accepts either a JSON number or a stringified number
/// (e.g. `"170"`); anything else falls back to 160.
fn deserialize_rpe_version<'de, D>(deserializer: D) -> std::result::Result<i32, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    let parsed = match value {
        Some(serde_json::Value::Number(v)) => v.as_i64().map(|it| it as i32),
        Some(serde_json::Value::String(s)) => s.parse::<i32>().ok(),
        _ => None,
    };
    Ok(parsed.unwrap_or_else(rpe_version_default))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEBpmItem {
    pub bpm: f64,
    pub start_time: Triple,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEEvent<T = f32> {
    #[serde(default = "f32_zero")]
    pub easing_left: f32,
    #[serde(default = "f32_one")]
    pub easing_right: f32,
    #[serde(default)]
    pub bezier: u8,
    #[serde(default)]
    pub bezier_points: [f32; 4],
    #[serde(default = "i32_one")]
    pub easing_type: i32,
    pub start: T,
    pub end: T,
    pub start_time: Triple,
    pub end_time: Triple,
}

/// Control event (`posControl` etc.). Parsed for tolerance only; not lowered
/// in M0 (// ponytail: M3 ctrl events).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPECtrlEvent {
    pub easing: u8,
    pub x: f64,
    #[serde(flatten)]
    pub value: HashMap<String, f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEEventLayer {
    pub alpha_events: Option<Vec<RPEEvent>>,
    pub move_x_events: Option<Vec<RPEEvent>>,
    pub move_y_events: Option<Vec<RPEEvent>>,
    pub rotate_events: Option<Vec<RPEEvent>>,
    pub speed_events: Option<Vec<RPEEvent>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RGBColor(pub u8, pub u8, pub u8);

impl From<RGBColor> for crate::core::Color {
    fn from(RGBColor(r, g, b): RGBColor) -> Self {
        Self::from_rgba(r, g, b, 255)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEExtendedEvents {
    pub color_events: Option<Vec<RPEEvent<RGBColor>>>,
    pub text_events: Option<Vec<RPEEvent<String>>>,
    pub scale_x_events: Option<Vec<RPEEvent>>,
    pub scale_y_events: Option<Vec<RPEEvent>>,
    pub incline_events: Option<Vec<RPEEvent>>,
    pub paint_events: Option<Vec<RPEEvent>>,
    pub gif_events: Option<Vec<RPEEvent>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPENote {
    #[serde(rename = "type")]
    pub kind: u8,
    pub above: u8,
    pub start_time: Triple,
    pub end_time: Triple,
    pub position_x: f32,
    pub y_offset: f32,
    pub alpha: u16, // some charts have 256...
    pub hitsound: Option<String>, // tolerated, ignored in M0 (no hitsounds)
    #[serde(default = "f32_one")]
    pub size: f32,
    #[serde(default = "f32_one")]
    pub speed: f32,
    #[serde(default)]
    pub is_fake: u8,
    #[serde(default = "visible_time_default")]
    pub visible_time: f64,
    #[serde(default)]
    pub tint: Option<[u8; 3]>, // tolerated, ignored in M0 (note tint FX)
    #[serde(default)]
    pub tint_hit_effects: Option<[u8; 3]>,
    #[serde(default)]
    pub judge_area: Option<f32>, // no judging in M0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEJudgeLine {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Texture")]
    pub texture: String,
    #[serde(rename = "father")]
    pub parent: Option<isize>,
    pub rotate_with_father: Option<bool>,
    pub event_layers: Vec<Option<RPEEventLayer>>,
    pub extended: Option<RPEExtendedEvents>,
    pub notes: Option<Vec<RPENote>>,
    pub is_cover: u8,
    #[serde(default)]
    pub z_order: i32,
    // prpr fails the whole chart on an unknown attachUI string; M0 tolerates
    // and ignores it (// ponytail: M3 attachUI).
    #[serde(rename = "attachUI")]
    pub attach_ui: Option<String>,

    #[serde(default)]
    pub pos_control: Vec<RPECtrlEvent>,
    #[serde(default)]
    pub size_control: Vec<RPECtrlEvent>,
    #[serde(default)]
    pub alpha_control: Vec<RPECtrlEvent>,
    #[serde(default)]
    pub y_control: Vec<RPECtrlEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEMetadata {
    /// Chart offset in **milliseconds**.
    pub offset: i32,
    #[serde(rename = "RPEVersion", default = "rpe_version_default", deserialize_with = "deserialize_rpe_version")]
    pub rpe_version: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEChart {
    #[serde(rename = "META")]
    pub meta: RPEMetadata,
    #[serde(rename = "BPMList")]
    pub bpm_list: Vec<RPEBpmItem>,
    pub judge_line_list: Vec<RPEJudgeLine>,
}

// ---------------------------------------------------------------------------
// info.json
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChartFormat {
    Rpe,
    Pec,
    Pgr,
    Pbc,
    Pss, // Phimakor Streamable Sheet (NDJSON)
}

/// Mirrors `prpr/src/info.rs`. `serde(default)` on the whole struct: an empty
/// `{}` is valid. `created`/`updated`/`chart_updated` are kept as raw strings
/// (no chrono dependency; unused in M0).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct ChartInfo {
    pub id: Option<i32>,
    pub uploader: Option<i32>,

    pub name: String,
    pub difficulty: f32,
    pub level: String,
    pub charter: String,
    pub composer: String,
    pub illustrator: String,

    pub chart: String,
    pub format: Option<ChartFormat>,
    pub music: String,
    pub illustration: String,
    pub unlock_video: Option<String>,

    pub preview_start: f32,
    pub preview_end: Option<f32>,
    pub aspect_ratio: f32,
    pub background_dim: f32,
    pub line_length: f32,
    /// Global offset in **seconds**; stacks with `META.offset` (they never
    /// override each other).
    pub offset: f32,
    pub tip: Option<String>,
    pub tags: Vec<String>,

    pub intro: String,

    pub hold_partial_cover: bool,
    pub note_uniform_scale: bool,
    pub force_aspect_ratio: bool,
    pub use_rpe_170_speed: Option<bool>,
    pub use_attach_ui_fix: Option<bool>,

    pub created: Option<String>,
    pub updated: Option<String>,
    pub chart_updated: Option<String>,
}

/// Parses an RPE-export `info.txt` (`Key: Value` lines; lines starting with
/// `#` are comments) into a [`ChartInfo`]. Unmapped keys (`Path`, unknowns)
/// are ignored; unmapped fields keep [`ChartInfo::default`] values.
pub fn parse_info_txt(source: &str) -> ChartInfo {
    let mut info = ChartInfo::default();
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        // value may itself contain ':' (e.g. paths) — split only at the first
        let value = value.trim();
        match key.trim() {
            "Name" => info.name = value.to_string(),
            "Song" => info.music = value.to_string(),
            "Picture" => info.illustration = value.to_string(),
            "Chart" => info.chart = value.to_string(),
            "Level" => info.level = value.to_string(),
            "Composer" => info.composer = value.to_string(),
            "Charter" => info.charter = value.to_string(),
            _ => {}
        }
    }
    info
}

impl Default for ChartInfo {
    fn default() -> Self {
        Self {
            id: None,
            uploader: None,

            name: "UK".to_string(),
            difficulty: 10.,
            level: "UK Lv.10".to_string(),
            charter: "UK".to_string(),
            composer: "UK".to_string(),
            illustrator: "UK".to_string(),

            chart: "chart.json".to_string(),
            format: None,
            music: "song.mp3".to_string(),
            illustration: "background.png".to_string(),
            unlock_video: None,

            preview_start: 0.,
            preview_end: None,
            aspect_ratio: 16. / 9.,
            background_dim: 0.6,
            line_length: 6.,
            offset: 0.,
            tip: None,
            tags: Vec::new(),

            intro: String::new(),

            hold_partial_cover: false,
            note_uniform_scale: false,
            force_aspect_ratio: false,
            use_rpe_170_speed: None,
            use_attach_ui_fix: None,

            created: None,
            updated: None,
            chart_updated: None,
        }
    }
}
