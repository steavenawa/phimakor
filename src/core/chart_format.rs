use anyhow::{bail, Context, Result};
use crate::core::bpm::Triple;
use crate::core::model::*;

fn default_event() -> RPEEvent {
    RPEEvent {
        start_time: Triple::default(),
        end_time: Triple::default(),
        start: 0.0, end: 0.0,
        easing_type: 0, easing_left: 0.0, easing_right: 1.0,
        bezier: 0, bezier_points: [0.0; 4],
    }
}

/// Unified chart format interface.
/// Every parser converts its source into an `RPEChart` plus metadata,
/// so the rest of the editor works unchanged.

pub trait ChartParser {
    fn detect(bytes: &[u8]) -> bool;
    fn parse_chart(bytes: &[u8], info: &ChartInfo) -> Result<RPEChart>;
}

// ── Format registry ──

pub fn detect_format(bytes: &[u8]) -> &'static str {
    if RpeParser::detect(bytes) { "rpe" }
    else if PecParser::detect(bytes) { "pec" }
    else if PgrParser::detect(bytes) { "pgr" }
    else if PssParser::detect(bytes) { "pss" }
    else { "unknown" }
}

pub fn parse_chart(format: &str, bytes: &[u8], info: &ChartInfo) -> Result<RPEChart> {
    match format {
        "rpe" => RpeParser::parse_chart(bytes, info),
        "pec" => PecParser::parse_chart(bytes, info),
        "pgr" => PgrParser::parse_chart(bytes, info),
        "pss" => PssParser::parse_chart(bytes, info),
        _ => bail!("unsupported chart format: {format}"),
    }
}

// ── RPE parser (JSON) ──

pub struct RpeParser;

impl ChartParser for RpeParser {
    fn detect(bytes: &[u8]) -> bool {
        let s = String::from_utf8_lossy(bytes).trim().to_string();
        s.starts_with('{') && s.contains("\"META\"")
    }
    fn parse_chart(bytes: &[u8], _info: &ChartInfo) -> Result<RPEChart> {
        serde_json::from_slice(bytes).context("RPE JSON parse")
    }
}

// ── PEC parser (Phigros Editor Chart, text format) ──

pub struct PecParser;

impl ChartParser for PecParser {
    fn detect(bytes: &[u8]) -> bool {
        let Ok(s) = std::str::from_utf8(bytes) else { return false };
        let first = s.trim().lines().next().unwrap_or("");
        first.starts_with(|c: char| c.is_ascii_digit() || c == '-')
    }
    fn parse_chart(bytes: &[u8], _info: &ChartInfo) -> Result<RPEChart> {
        parse_pec_text(std::str::from_utf8(bytes)?)
    }
}

fn parse_pec_text(source: &str) -> Result<RPEChart> {
    let lines: Vec<&str> = source.lines().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with("//")).collect();
    if lines.is_empty() { bail!("empty PEC file") }

    let offset_ms: f64 = lines[0].parse().unwrap_or(0.0);
    let offset_sec = offset_ms / 1000.0 - 0.15;

    let mut bpm_list: Vec<RPEBpmItem> = Vec::new();
    let mut notes: Vec<RPENote> = Vec::new();
    let mut alpha_events: Vec<RPEEvent> = Vec::new();
    let mut move_x_events: Vec<RPEEvent> = Vec::new();
    let mut move_y_events: Vec<RPEEvent> = Vec::new();
    let mut rotate_events: Vec<RPEEvent> = Vec::new();
    let mut speed_events: Vec<RPEEvent> = Vec::new();

    let mut note_speed = 1.0;
    let mut note_size = 1.0;

    for line in &lines[1..] {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() { continue; }
        match parts[0] {
            "bp" if parts.len() >= 3 => {
                let bpm: f64 = parts[1].parse().unwrap_or(120.0);
                let start_beats: f64 = parts[2].parse().unwrap_or(0.0);
                bpm_list.push(RPEBpmItem { bpm, start_time: Triple::from_beats(start_beats) });
            }
            "#" if parts.len() >= 2 => { note_speed = parts[1].parse().unwrap_or(1.0); }
            "&" if parts.len() >= 2 => { note_size = parts[1].parse().unwrap_or(1.0); }
            "n1" | "n2" | "n3" | "n4" => {
                let kind: u8 = match parts[0] { "n2" => 2, "n3" => 3, "n4" => 4, _ => 1 };
                if parts.len() < 4 { continue; }
                let start_beats: f64 = parts[1].parse().unwrap_or(0.0);
                let x: f32 = parts[2].parse().unwrap_or(0.0);
                let y: f32 = parts[3].parse().unwrap_or(0.0);
                let end_beats = if kind == 2 && parts.len() >= 5 {
                    let dur: f64 = parts[4].parse().unwrap_or(0.0);
                    start_beats + dur
                } else if kind == 4 && parts.len() >= 5 {
                    let dur: f64 = parts[4].parse().unwrap_or(0.0);
                    start_beats + dur
                } else { start_beats };
                notes.push(RPENote {
                    kind, above: if y >= 0.0 { 1 } else { 0 },
                    start_time: Triple::from_beats(start_beats),
                    end_time: Triple::from_beats(end_beats),
                    position_x: x, y_offset: y, alpha: 255, hitsound: None,
                    size: note_size, speed: note_speed, is_fake: 0, visible_time: 999999.,
                    tint: None, tint_hit_effects: None, judge_area: None,
                });
            }
            "cv" if parts.len() >= 4 => {
                let sb: f64 = parts[1].parse().unwrap_or(0.0);
                let dur: f64 = parts[2].parse().unwrap_or(0.0);
                let start: f32 = parts[3].parse().unwrap_or(0.0);
                let end = if parts.len() >= 5 { parts[4].parse().unwrap_or(start) } else { start };
                speed_events.push(RPEEvent {
                    start_time: Triple::from_beats(sb), end_time: Triple::from_beats(sb + dur),
                    start, end, easing_type: 0,
                    easing_left: 0.0, easing_right: 0.0, bezier: 0, bezier_points: [0.0; 4],
                });
            }
            "cp" if parts.len() >= 5 => {
                let sb: f64 = parts[1].parse().unwrap_or(0.0);
                let dur: f64 = parts[2].parse().unwrap_or(0.0);
                let x1: f32 = parts[3].parse().unwrap_or(0.0);
                let y1: f32 = parts[4].parse().unwrap_or(0.0);
                let x2 = if parts.len() >= 6 { parts[5].parse().unwrap_or(x1) } else { x1 };
                let y2 = if parts.len() >= 7 { parts[6].parse().unwrap_or(y1) } else { y1 };
                move_x_events.push(RPEEvent {
                    start_time: Triple::from_beats(sb), end_time: Triple::from_beats(sb + dur),
                    start: x1, end: x2, easing_type: 0,
                    easing_left: 0.0, easing_right: 0.0, bezier: 0, bezier_points: [0.0; 4],
                });
                move_y_events.push(RPEEvent {
                    start_time: Triple::from_beats(sb), end_time: Triple::from_beats(sb + dur),
                    start: y1, end: y2, easing_type: 0,
                    easing_left: 0.0, easing_right: 0.0, bezier: 0, bezier_points: [0.0; 4],
                });
            }
            "cd" if parts.len() >= 4 => {
                let sb: f64 = parts[1].parse().unwrap_or(0.0);
                let dur: f64 = parts[2].parse().unwrap_or(0.0);
                let start: f32 = parts[3].parse().unwrap_or(0.0);
                let end = if parts.len() >= 5 { parts[4].parse().unwrap_or(start) } else { start };
                rotate_events.push(RPEEvent {
                    start_time: Triple::from_beats(sb), end_time: Triple::from_beats(sb + dur),
                    start, end, easing_type: 0,
                    easing_left: 0.0, easing_right: 0.0, bezier: 0, bezier_points: [0.0; 4],
                });
            }
            "ca" if parts.len() >= 4 => {
                let sb: f64 = parts[1].parse().unwrap_or(0.0);
                let dur: f64 = parts[2].parse().unwrap_or(0.0);
                let start: f32 = parts[3].parse().unwrap_or(0.0);
                let end = if parts.len() >= 5 { parts[4].parse().unwrap_or(start) } else { start };
                alpha_events.push(RPEEvent {
                    start_time: Triple::from_beats(sb), end_time: Triple::from_beats(sb + dur),
                    start, end, easing_type: 0,
                    easing_left: 0.0, easing_right: 0.0, bezier: 0, bezier_points: [0.0; 4],
                });
            }
            "cm" if parts.len() >= 7 => {
                let sb: f64 = parts[1].parse().unwrap_or(0.0);
                let dur: f64 = parts[2].parse().unwrap_or(0.0);
                let x1: f32 = parts[3].parse().unwrap_or(0.0);
                let y1: f32 = parts[4].parse().unwrap_or(0.0);
                let x2: f32 = parts[5].parse().unwrap_or(0.0);
                let y2: f32 = parts[6].parse().unwrap_or(0.0);
                let left = if parts.len() >= 8 { parts[7].parse().unwrap_or(0.0) } else { 0.0 };
                let right = if parts.len() >= 9 { parts[8].parse().unwrap_or(0.0) } else { 0.0 };
                move_x_events.push(RPEEvent {
                    start_time: Triple::from_beats(sb), end_time: Triple::from_beats(sb + dur),
                    start: x1, end: x2, easing_type: 0,
                    easing_left: left, easing_right: right, bezier: 0, bezier_points: [0.0; 4],
                });
                move_y_events.push(RPEEvent {
                    start_time: Triple::from_beats(sb), end_time: Triple::from_beats(sb + dur),
                    start: y1, end: y2, easing_type: 0,
                    easing_left: left, easing_right: right, bezier: 0, bezier_points: [0.0; 4],
                });
            }
            "cr" if parts.len() >= 5 => {
                let sb: f64 = parts[1].parse().unwrap_or(0.0);
                let dur: f64 = parts[2].parse().unwrap_or(0.0);
                let start: f32 = parts[3].parse().unwrap_or(0.0);
                let end: f32 = parts[4].parse().unwrap_or(0.0);
                let left = if parts.len() >= 6 { parts[5].parse().unwrap_or(0.0) } else { 0.0 };
                let right = if parts.len() >= 7 { parts[6].parse().unwrap_or(0.0) } else { 0.0 };
                rotate_events.push(RPEEvent {
                    start_time: Triple::from_beats(sb), end_time: Triple::from_beats(sb + dur),
                    start, end, easing_type: 0,
                    easing_left: left, easing_right: right, bezier: 0, bezier_points: [0.0; 4],
                });
            }
            "cf" if parts.len() >= 5 => {
                let sb: f64 = parts[1].parse().unwrap_or(0.0);
                let dur: f64 = parts[2].parse().unwrap_or(0.0);
                let start: f32 = parts[3].parse().unwrap_or(0.0);
                let end: f32 = parts[4].parse().unwrap_or(0.0);
                let left = if parts.len() >= 6 { parts[5].parse().unwrap_or(0.0) } else { 0.0 };
                let right = if parts.len() >= 7 { parts[6].parse().unwrap_or(0.0) } else { 0.0 };
                alpha_events.push(RPEEvent {
                    start_time: Triple::from_beats(sb), end_time: Triple::from_beats(sb + dur),
                    start, end, easing_type: 0,
                    easing_left: left, easing_right: right, bezier: 0, bezier_points: [0.0; 4],
                });
            }
            _ => {}
        }
    }

    if bpm_list.is_empty() {
        bpm_list.push(RPEBpmItem { bpm: 120.0, start_time: Triple::from_beats(0.0) });
    }

    let event_layer = Some(RPEEventLayer {
        alpha_events: Some(alpha_events),
        move_x_events: Some(move_x_events),
        move_y_events: Some(move_y_events),
        rotate_events: Some(rotate_events),
        speed_events: Some(speed_events),
    });

    let judge_line = RPEJudgeLine {
        name: "PEC Line".into(), texture: "line.png".into(),
        parent: None, rotate_with_father: None,
        event_layers: vec![event_layer], extended: None,
        notes: if notes.is_empty() { None } else { Some(notes) },
        is_cover: 0, z_order: 0, attach_ui: None,
        pos_control: vec![], size_control: vec![], alpha_control: vec![], y_control: vec![],
    };

    Ok(RPEChart {
        meta: RPEMetadata { offset: (offset_sec * 1000.0) as i32, rpe_version: 160 },
        bpm_list,
        judge_line_list: vec![judge_line],
    })
}

// ── PGR (Phigros official chart JSON) parser ──

pub struct PgrParser;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PgrEvent {
    start_time: f64,
    end_time: f64,
    start: f32,
    end: f32,
    start2: Option<f32>,
    end2: Option<f32>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PgrSpeedEvent {
    start_time: f64,
    end_time: f64,
    value: f32,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgrNote {
    kind: u8,
    time: f64,
    position_x: f32,
    hold_time: Option<f64>,
    speed: Option<f32>,
    floor_position: Option<f32>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PgrJudgeLine {
    bpm: Option<f64>,
    alpha_events: Option<Vec<PgrEvent>>,
    rotate_events: Option<Vec<PgrEvent>>,
    move_events: Option<Vec<PgrEvent>>,
    speed_events: Option<Vec<PgrSpeedEvent>>,
    notes_above: Option<Vec<PgrNote>>,
    notes_below: Option<Vec<PgrNote>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PgrChart {
    format_version: Option<i32>,
    offset: f64,
    judge_line_list: Vec<PgrJudgeLine>,
}

impl ChartParser for PgrParser {
    fn detect(bytes: &[u8]) -> bool {
        let s = String::from_utf8_lossy(bytes).trim().to_string();
        s.starts_with('{') && (s.contains("\"format_version\"") || s.contains("\"judge_line_list\"")) && !s.contains("\"META\"")
    }
    fn parse_chart(bytes: &[u8], _info: &ChartInfo) -> Result<RPEChart> {
        let pgr: PgrChart = serde_json::from_slice(bytes).context("PGR JSON parse")?;
        let offset_ms = (pgr.offset * 1000.0) as i32;
        let mut bpm_list: Vec<RPEBpmItem> = Vec::new();
        let mut judge_lines: Vec<RPEJudgeLine> = Vec::new();

        for (li, pgr_line) in pgr.judge_line_list.into_iter().enumerate() {
            if let Some(bpm) = pgr_line.bpm {
                bpm_list.push(RPEBpmItem { bpm, start_time: Triple::from_beats(0.0) });
            }

            let mut alpha_events = Vec::new();
            let mut move_x_events = Vec::new();
            let mut move_y_events = Vec::new();
            let mut rotate_events = Vec::new();
            let mut speed_events = Vec::new();

            if let Some(evs) = pgr_line.alpha_events {
                for ev in evs {
                    alpha_events.push(ev_to_rpe(ev));
                }
            }
            if let Some(evs) = pgr_line.rotate_events {
                for ev in evs {
                    rotate_events.push(ev_to_rpe(ev));
                }
            }
            if let Some(evs) = pgr_line.move_events {
                for ev in evs {
                    // Move events in PGR are 2D: start=(start,start2), end=(end,end2)
                    let mut x = default_event();
                    x.start_time = Triple::from_beats(ev.start_time);
                    x.end_time = Triple::from_beats(ev.end_time);
                    x.start = ev.start; x.end = ev.end;
                    let mut y = default_event();
                    y.start_time = Triple::from_beats(ev.start_time);
                    y.end_time = Triple::from_beats(ev.end_time);
                    y.start = ev.start2.unwrap_or(ev.start);
                    y.end = ev.end2.unwrap_or(ev.end);
                    move_x_events.push(x);
                    move_y_events.push(y);
                }
            }
            if let Some(evs) = pgr_line.speed_events {
                for ev in evs {
                    let mut rpe_ev = default_event();
                    rpe_ev.start_time = Triple::from_beats(ev.start_time);
                    rpe_ev.end_time = Triple::from_beats(ev.end_time);
                    rpe_ev.start = ev.value; rpe_ev.end = ev.value;
                    speed_events.push(rpe_ev);
                }
            }

            let mut notes: Vec<RPENote> = Vec::new();
            for (above, list) in [(1, pgr_line.notes_above), (0, pgr_line.notes_below)] {
                if let Some(ns) = list {
                    for n in ns {
                        let kind = n.kind;
                        let start_beats = n.time;
                        let end_beats = start_beats + n.hold_time.unwrap_or(0.0);
                        notes.push(RPENote {
                            kind, above,
                            start_time: Triple::from_beats(start_beats),
                            end_time: Triple::from_beats(end_beats),
                            position_x: n.position_x, y_offset: 0.0, alpha: 255,
                            hitsound: None, size: 1.0, speed: n.speed.unwrap_or(1.0),
                            is_fake: 0, visible_time: 999999.,
                            tint: None, tint_hit_effects: None, judge_area: None,
                        });
                    }
                }
            }

            judge_lines.push(RPEJudgeLine {
                name: format!("Line {li}"), texture: "line.png".into(),
                parent: None, rotate_with_father: None,
                event_layers: vec![Some(RPEEventLayer {
                    alpha_events: if alpha_events.is_empty() { None } else { Some(alpha_events) },
                    move_x_events: if move_x_events.is_empty() { None } else { Some(move_x_events) },
                    move_y_events: if move_y_events.is_empty() { None } else { Some(move_y_events) },
                    rotate_events: if rotate_events.is_empty() { None } else { Some(rotate_events) },
                    speed_events: if speed_events.is_empty() { None } else { Some(speed_events) },
                })],
                extended: None,
                notes: if notes.is_empty() { None } else { Some(notes) },
                is_cover: 0, z_order: 0, attach_ui: None,
                pos_control: vec![], size_control: vec![], alpha_control: vec![], y_control: vec![],
            });
        }

        if bpm_list.is_empty() {
            bpm_list.push(RPEBpmItem { bpm: 120.0, start_time: Triple::from_beats(0.0) });
        }

        Ok(RPEChart {
            meta: RPEMetadata { offset: offset_ms, rpe_version: 160 },
            bpm_list,
            judge_line_list: judge_lines,
        })
    }
}

fn ev_to_rpe(ev: PgrEvent) -> RPEEvent {
    RPEEvent {
        start_time: Triple::from_beats(ev.start_time),
        end_time: Triple::from_beats(ev.end_time),
        start: ev.start, end: ev.end,
        easing_type: 0, easing_left: 0.0, easing_right: 0.0,
        bezier: 0, bezier_points: [0.0; 4],
    }
}

// ── PSS parser (Phimakor Streamable Sheet, NDJSON) ──

pub struct PssParser;

impl ChartParser for PssParser {
    fn detect(bytes: &[u8]) -> bool {
        let Ok(s) = std::str::from_utf8(bytes) else { return false };
        s.lines().next().map_or(false, |l| l.contains(r#""type":"meta""#))
    }
    fn parse_chart(bytes: &[u8], _info: &ChartInfo) -> Result<RPEChart> {
        let (chart, _info) = crate::core::stream::from_stream_bytes(bytes)?;
        Ok(chart)
    }
}
