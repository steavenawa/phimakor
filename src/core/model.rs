// Derived from TeamFlos/phira prpr, GPL-3.0.
//! Serde models for RPE chart JSON and `info.json`. Pure data structs; lowering
//! lives in [`crate::core::chart`]. Field names/types mirror
//! `prpr/src/parse/rpe.rs` and `prpr/src/info.rs` exactly.

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

/// A BPM change point in the chart timeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEBpmItem {
    /// Target BPM value.
    pub bpm: f64,
    /// Start time of this BPM segment (in beats).
    pub start_time: Triple,
}

/// A timed event with easing interpolation between two values.
///
/// The generic parameter `T` controls the value type (default `f32` for
/// scalar events; `RGBColor` for colour events, `String` for text events).
/// Easing is governed by `easing_type`, optional bezier control points,
/// and left/right clamping in [0, 1].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEEvent<T = f32> {
    /// Left-easing clamp in [0, 1]; the output value at `start_time` is
    /// biased toward `start` or `end` by this factor. Default 0.
    #[serde(default = "f32_zero")]
    pub easing_left: f32,
    /// Right-easing clamp in [0, 1]; complements `easing_left` at
    /// `end_time`. Default 1.
    #[serde(default = "f32_one")]
    pub easing_right: f32,
    /// Bitmask for bezier-curve usage (0 = linear easing only).
    #[serde(default)]
    pub bezier: u8,
    /// Four control points for a cubic bezier curve applied on top of the
    /// easing function.
    #[serde(default)]
    pub bezier_points: [f32; 4],
    /// Easing function type: 1 = linear, other values map to Phira's enum.
    /// Default 1.
    #[serde(default = "i32_one")]
    pub easing_type: i32,
    /// Value at `start_time`.
    pub start: T,
    /// Value at `end_time`.
    pub end: T,
    /// Event start time (in beats).
    pub start_time: Triple,
    /// Event end time (in beats).
    pub end_time: Triple,
}

/// Control event (e.g. `posControl`, `sizeControl`) for per-line controllers.
///
/// Parsed for tolerance only; not lowered in M0 (// ponytail: M3 ctrl events).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPECtrlEvent {
    /// Control easing mode.
    pub easing: u8,
    /// Control X coordinate.
    pub x: f64,
    /// Additional flattened key-value pairs (e.g. `{ "x": 0.5 }`).
    #[serde(flatten)]
    pub value: HashMap<String, f32>,
}

/// A layer grouping optional typed event sequences for a judgement line.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEEventLayer {
    /// Alpha (opacity) events over time.
    pub alpha_events: Option<Vec<RPEEvent>>,
    /// Horizontal position events.
    pub move_x_events: Option<Vec<RPEEvent>>,
    /// Vertical position events.
    pub move_y_events: Option<Vec<RPEEvent>>,
    /// Rotation events (in degrees).
    pub rotate_events: Option<Vec<RPEEvent>>,
    /// Speed-multiplier events.
    pub speed_events: Option<Vec<RPEEvent>>,
}

/// An RGB colour tuple (red, green, blue), each component 0-255.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RGBColor(pub u8, pub u8, pub u8);

impl From<RGBColor> for crate::core::Color {
    fn from(RGBColor(r, g, b): RGBColor) -> Self {
        Self::from_rgba(r, g, b, 255)
    }
}

/// Extended events that are not part of the basic event layer set.
///
/// Includes colour, text, scale, incline, paint, and GIF events.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEExtendedEvents {
    /// Events that change the note/judge-line colour over time.
    pub color_events: Option<Vec<RPEEvent<RGBColor>>>,
    /// Events that display changing text strings.
    pub text_events: Option<Vec<RPEEvent<String>>>,
    /// Events that scale the judge line horizontally.
    pub scale_x_events: Option<Vec<RPEEvent>>,
    /// Events that scale the judge line vertically.
    pub scale_y_events: Option<Vec<RPEEvent>>,
    /// Events that tilt/incline the judge line.
    pub incline_events: Option<Vec<RPEEvent>>,
    /// Paint / decal overlay events.
    pub paint_events: Option<Vec<RPEEvent>>,
    /// Animated GIF overlay events.
    pub gif_events: Option<Vec<RPEEvent>>,
}

/// A single note on a judge line.
///
/// JSON key `type` is deserialized into `kind`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPENote {
    /// Note type (JSON `"type"`): 1 = Tap, 2 = Drag, 3 = Hold, 4 = Flick.
    #[serde(rename = "type")]
    pub kind: u8,
    /// 1 = above the judgement line, 0 = below it.
    pub above: u8,
    /// Hit time for tap/drag/flick, or hold start time (in beats).
    pub start_time: Triple,
    /// Hold release time (in beats); 0 for non-hold notes.
    pub end_time: Triple,
    /// X position on the judge line, typically 0.0 (left) to 1.0 (right).
    pub position_x: f32,
    /// Vertical offset from the judge line.
    pub y_offset: f32,
    /// Opacity (0-255; some charts use values up to 256).
    pub alpha: u16,
    /// Optional hitsound file path; tolerated but ignored in M0.
    pub hitsound: Option<String>,
    /// Note scale multiplier. Default 1.0.
    #[serde(default = "f32_one")]
    pub size: f32,
    /// Note speed multiplier. Default 1.0.
    #[serde(default = "f32_one")]
    pub speed: f32,
    /// Fake/ghost flag: 0 = real note, 1 = fake.
    #[serde(default)]
    pub is_fake: u8,
    /// How far ahead (in milliseconds) the note becomes visible. Default 999999.
    #[serde(default = "visible_time_default")]
    pub visible_time: f64,
    /// Optional RGB tint for the note body; tolerated but ignored in M0.
    #[serde(default)]
    pub tint: Option<[u8; 3]>,
    /// Optional RGB tint for hit effects; tolerated but ignored in M0.
    #[serde(default)]
    pub tint_hit_effects: Option<[u8; 3]>,
    /// Optional custom judge area radius; tolerated but ignored in M0.
    #[serde(default)]
    pub judge_area: Option<f32>,
}

/// A single judge line in the RPE chart.
///
/// Contains notes, event layers, extended events, and per-line control
/// parameters.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEJudgeLine {
    /// Judge-line name (JSON `"Name"`).
    #[serde(rename = "Name")]
    pub name: String,
    /// Texture file path (JSON `"Texture"`).
    #[serde(rename = "Texture")]
    pub texture: String,
    /// Parent line index (JSON `"father"`); `None` for top-level lines.
    #[serde(rename = "father")]
    pub parent: Option<isize>,
    /// Whether the line rotates together with its parent.
    pub rotate_with_father: Option<bool>,
    /// Event layers for animation.
    pub event_layers: Vec<Option<RPEEventLayer>>,
    /// Extended events (colour, text, scale, etc.).
    pub extended: Option<RPEExtendedEvents>,
    /// Notes belonging to this line.
    pub notes: Option<Vec<RPENote>>,
    /// Whether this line is a cover line (non-zero = cover).
    pub is_cover: u8,
    /// Z-ordering of this line (higher = drawn on top).
    #[serde(default)]
    pub z_order: i32,
    /// Optional attach-UI identifier (JSON `"attachUI"`); tolerated but
    /// ignored in M0 (// ponytail: M3 attachUI).
    #[serde(rename = "attachUI")]
    pub attach_ui: Option<String>,
    /// Position-control events for dynamic line placement.
    #[serde(default)]
    pub pos_control: Vec<RPECtrlEvent>,
    /// Size-control events for dynamic line scaling.
    #[serde(default)]
    pub size_control: Vec<RPECtrlEvent>,
    /// Alpha-control events for dynamic line opacity.
    #[serde(default)]
    pub alpha_control: Vec<RPECtrlEvent>,
    /// Y-offset control events.
    #[serde(default)]
    pub y_control: Vec<RPECtrlEvent>,
}

/// Metadata header for an RPE chart.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEMetadata {
    /// Chart offset in **milliseconds**.
    pub offset: i32,
    /// RPE version number (JSON `"RPEVersion"`); accepts number or
    /// stringified number. Default 160.
    #[serde(rename = "RPEVersion", default = "rpe_version_default", deserialize_with = "deserialize_rpe_version")]
    pub rpe_version: i32,
}

/// Top-level RPE chart container wrapping metadata, BPM list, and judge lines.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RPEChart {
    /// Chart metadata (JSON `"META"`).
    #[serde(rename = "META")]
    pub meta: RPEMetadata,
    /// BPM list (JSON `"BPMList"`).
    #[serde(rename = "BPMList")]
    pub bpm_list: Vec<RPEBpmItem>,
    /// All judge lines in the chart.
    pub judge_line_list: Vec<RPEJudgeLine>,
}

// ---------------------------------------------------------------------------
// info.json
// ---------------------------------------------------------------------------

/// Recognised chart-file formats.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChartFormat {
    /// Standard RPE JSON format.
    Rpe,
    /// Phigros Editor Chart (`.pec`).
    Pec,
    /// Phigros Game Record (`.pgr`).
    Pgr,
    /// Phigros Backup Chart (`.pbc`).
    Pbc,
    /// Phimakor Streamable Sheet — NDJSON format.
    Pss,
}

/// Chart-level metadata mirroring `prpr/src/info.rs`.
///
/// `serde(default)` on the whole struct so an empty `{}` is valid.
/// `created`/`updated`/`chart_updated` are kept as raw strings
/// (no chrono dependency; unused in M0).
///
/// Fields are documented with whether they originate from the RPE export
/// or are Phimakor extensions.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct ChartInfo {
    /// **Phimakor.** Internal database ID.
    pub id: Option<i32>,
    /// **Phimakor.** Uploader user ID.
    pub uploader: Option<i32>,

    /// RPE export. Song/chart display name.
    pub name: String,
    /// RPE export. Chart difficulty rating (e.g. 12.4).
    pub difficulty: f32,
    /// RPE export. Difficulty level string (e.g. "IN Lv.15").
    pub level: String,
    /// RPE export. Charter name.
    pub charter: String,
    /// RPE export. Composer name.
    pub composer: String,
    /// RPE export. Illustrator name.
    pub illustrator: String,

    /// RPE export. Chart file path (e.g. `"chart.json"`).
    pub chart: String,
    /// **Phimakor.** Chart file format override.
    pub format: Option<ChartFormat>,
    /// RPE export. Music file path.
    pub music: String,
    /// RPE export. Illustration/background image path.
    pub illustration: String,
    /// RPE export. Unlock-video file path.
    pub unlock_video: Option<String>,

    /// RPE export. Preview start time (in seconds).
    pub preview_start: f32,
    /// RPE export. Preview end time (in seconds).
    pub preview_end: Option<f32>,
    /// RPE export. Display aspect ratio (e.g. 16/9).
    pub aspect_ratio: f32,
    /// RPE export. Background dimming level (0.0–1.0).
    pub background_dim: f32,
    /// RPE export. Judge-line length multiplier.
    pub line_length: f32,
    /// RPE export. Global offset in **seconds**; stacks with
    /// `META.offset` (they never override each other).
    pub offset: f32,
    /// RPE export. In-game tip text.
    pub tip: Option<String>,
    /// RPE export. Search/display tags.
    pub tags: Vec<String>,

    /// RPE export. Intro/description text.
    pub intro: String,

    /// **Phimakor.** Whether hold notes show partial cover.
    pub hold_partial_cover: bool,
    /// **Phimakor.** Whether notes use uniform scaling.
    pub note_uniform_scale: bool,
    /// **Phimakor.** Whether to force aspect ratio display.
    pub force_aspect_ratio: bool,
    /// **Phimakor.** Whether to use RPE 1.7.0 speed semantics.
    pub use_rpe_170_speed: Option<bool>,
    /// **Phimakor.** Whether to apply the attach-UI fix.
    pub use_attach_ui_fix: Option<bool>,

    /// **Phimakor.** ISO 8601 creation timestamp.
    pub created: Option<String>,
    /// **Phimakor.** ISO 8601 last-update timestamp.
    pub updated: Option<String>,
    /// **Phimakor.** ISO 8601 chart-last-updated timestamp.
    pub chart_updated: Option<String>,
}

/// YAML info file (`info.yml`) adapter — the RPE web-export metadata format.
/// Field names follow the actual `info.yml` convention (a mix of snake_case
/// and camelCase), so each is renamed explicitly.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct InfoYaml {
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
    #[serde(rename = "unlockVideo")]
    pub unlock_video: Option<String>,
    #[serde(rename = "previewStart")]
    pub preview_start: f32,
    #[serde(rename = "previewEnd")]
    pub preview_end: Option<f32>,
    #[serde(rename = "aspectRatio")]
    pub aspect_ratio: f32,
    #[serde(rename = "backgroundDim")]
    pub background_dim: f32,
    #[serde(rename = "lineLength")]
    pub line_length: f32,
    pub offset: f32,
    pub tip: Option<String>,
    pub tags: Vec<String>,
    pub intro: String,
    #[serde(rename = "holdPartialCover")]
    pub hold_partial_cover: bool,
    #[serde(rename = "noteUniformScale")]
    pub note_uniform_scale: bool,
    #[serde(rename = "forceAspectRatio")]
    pub force_aspect_ratio: bool,
    #[serde(rename = "useRpe170Speed")]
    pub use_rpe_170_speed: Option<bool>,
    #[serde(rename = "useAttachUIFix")]
    pub use_attach_ui_fix: Option<bool>,
}

impl InfoYaml {
    /// Convert into the canonical [`ChartInfo`], filling defaults for the
    /// fields the YAML export omits.
    pub fn into_chart_info(self) -> ChartInfo {
        let mut info = ChartInfo::default();
        info.id = self.id;
        info.uploader = self.uploader;
        info.name = self.name;
        info.difficulty = self.difficulty;
        info.level = self.level;
        info.charter = self.charter;
        info.composer = self.composer;
        info.illustrator = self.illustrator;
        if !self.chart.is_empty() { info.chart = self.chart; }
        info.format = self.format;
        if !self.music.is_empty() { info.music = self.music; }
        if !self.illustration.is_empty() { info.illustration = self.illustration; }
        info.unlock_video = self.unlock_video;
        if self.preview_start != 0.0 { info.preview_start = self.preview_start; }
        info.preview_end = self.preview_end;
        if self.aspect_ratio != 0.0 { info.aspect_ratio = self.aspect_ratio; }
        info.background_dim = self.background_dim;
        if self.line_length != 0.0 { info.line_length = self.line_length; }
        info.offset = self.offset;
        info.tip = self.tip;
        if !self.tags.is_empty() { info.tags = self.tags; }
        if !self.intro.is_empty() { info.intro = self.intro; }
        info.hold_partial_cover = self.hold_partial_cover;
        info.note_uniform_scale = self.note_uniform_scale;
        info.force_aspect_ratio = self.force_aspect_ratio;
        info.use_rpe_170_speed = self.use_rpe_170_speed;
        info.use_attach_ui_fix = self.use_attach_ui_fix;
        info
    }
}

/// Parse an RPE-exported `info.txt` (`Key: Value` lines; `#` comments) into a
/// [`ChartInfo`]. Unmapped keys (e.g. `Path`, unknowns) are silently ignored;
/// unmapped fields keep [`ChartInfo::default`] values.
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
