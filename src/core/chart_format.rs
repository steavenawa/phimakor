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
    /// Probe `bytes` to determine whether this format can handle the input.
    fn detect(bytes: &[u8]) -> bool;
    /// Parse `bytes` into an [`RPEChart`] using the given `info` scaffolding.
    fn parse_chart(bytes: &[u8], info: &ChartInfo) -> Result<RPEChart>;
}

// ── Format registry ──

/// Probe the bytes against all registered parsers and return the format tag
/// (`"rpe"`, `"pec"`, `"pgr"`, `"pss"`, or `"unknown"`).
pub fn detect_format(bytes: &[u8]) -> &'static str {
    if RpeParser::detect(bytes) { "rpe" }
    else if PecParser::detect(bytes) { "pec" }
    else if PgrParser::detect(bytes) { "pgr" }
    else if PssParser::detect(bytes) { "pss" }
    else { "unknown" }
}

/// Dispatch to the appropriate parser by format tag and return the parsed chart.
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

/// RPE (RPE JSON) format parser. Detects `{"META": ...}` and deserialises
/// directly via serde.
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

/// PEC text-format parser. Detects a leading number (offset line) and parses
/// a multi-judge-line chart. Grammar (ported from prpr `parse/pec.rs`):
///
/// ```text
/// <offset-ms>
/// bp <bpm> <start-beats>                 (optional; default 120)
/// n1 <line> <time> <x> <above> <fake> [# <speed>] [& <size>]   tap
/// n2 <line> <time> <end-time> <x> <above> <fake> [...]          hold
/// n3 <line> <time> <x> <above> <fake> [...]                     flick
/// n4 <line> <time> <x> <above> <fake> [...]                     drag
/// cv <line> <time> <speed>                                       speed change
/// ca <line> <time> <alpha>                                      alpha set
/// cp <line> <time> <x> <y>                                      move set
/// cd <line> <time> <rotation>                                   rotate set
/// cm <line> <time> <end-time> <x> <y> <tween>                   move tween
/// cr <line> <time> <end-time> <rotation> <tween>                rotate tween
/// cf <line> <time> <end-time> <alpha>                           alpha tween (linear)
/// ```
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

/// One PEC judge line being accumulated during parsing.
#[derive(Default)]
struct PecLine {
    notes: Vec<RPENote>,
    speed: Vec<(f64, f32)>,
    alpha: Vec<PecEvent>,
    move_x: Vec<PecEvent>,
    move_y: Vec<PecEvent>,
    rotate: Vec<PecEvent>,
}

/// A PEC event: start/end in beats (`start == end` = single-value set) with a
/// tween id applied to the interpolation (0 = linear / single).
#[derive(Clone, Copy)]
struct PecEvent {
    start: f64,
    end: f64,
    value: f32,
    tween: u8,
}

impl PecEvent {
    fn single(time: f64, value: f32) -> Self {
        Self { start: time, end: time, value, tween: 0 }
    }
}

/// Resolve a PEC tween number through the RPE tween table (prpr semantics:
/// the stored id IS an RPE easingType, out-of-range falls back to linear).
fn pec_tween(t: u8) -> u8 {
    if (t as usize) < crate::core::easing::RPE_TWEEN_MAP.len() { t } else { 1 }
}

/// Sort events by end-then-start and clip overlaps (prpr `sanitize_events`):
/// an event whose start falls inside the previous one is pushed to its end.
fn sanitize_events(events: &mut [PecEvent]) {
    events.sort_by(|a, b| a.end.total_cmp(&b.end).then(a.start.total_cmp(&b.start)));
    let mut last_end = f64::NEG_INFINITY;
    for e in events.iter_mut() {
        if e.start < last_end {
            e.start = last_end;
        }
        last_end = last_end.max(e.end);
    }
}

/// Convert PEC events to RPE events, remapping values into RPE's coordinate
/// conventions (see [`parse_judge_line`] factors):
/// move_x ×2/1350, move_y ×2/900, rotation ×-1, alpha raw (×1/255), and the
/// PEC raw domain (x 0..2048, y 0..1400, rotation degrees, alpha 0..255).
fn pec_to_rpe_events(events: Vec<PecEvent>, kind: &str) -> Vec<RPEEvent> {
    let mut out = Vec::with_capacity(events.len());
    for e in events {
        let (start, end) = (e.start, e.end.max(e.start));
        let rpe = |v: f32| -> RPEEvent {
            RPEEvent {
                start_time: Triple::from_beats(start),
                end_time: Triple::from_beats(end),
                start: v, end: v,
                easing_type: pec_tween(e.tween) as i32,
                easing_left: 0.0, easing_right: 1.0,
                bezier: 0, bezier_points: [0.0; 4],
            }
        };
        let v = e.value;
        let remapped = match kind {
            // RPE move values are in canvas px; PEC raw x is 0..2048 with
            // 1024 = center. (raw/1024 - 1) * 675 → px so the ×2/1350 factor
            // in parse_judge_line lands on prpr's (raw/2048*2 - 1).
            "x" => (v / 1024.0 - 1.0) * 675.0,
            "y" => (v / 1400.0 - 1.0) * 450.0,
            // prpr PEC negates rotation; RPE's ×-1 factor restores the raw sign.
            "r" => v,
            // Alpha and speed stay raw (RPE scales alpha by 1/255 itself).
            _ => v,
        };
        out.push(rpe(remapped));
    }
    out
}

fn parse_pec_text(source: &str) -> Result<RPEChart> {
    let mut lines: Vec<PecLine> = Vec::new();
    let mut bpm_list: Vec<RPEBpmItem> = Vec::new();
    let mut offset_ms = 0.0f64;
    let mut offset_seen = false;
    // Global defaults set by standalone `# speed` / `& size` lines (legacy
    // PEC flavor); per-note overrides win.
    let mut default_speed = 1.0f32;
    let mut default_size = 1.0f32;

    fn get_line<'a>(lines: &'a mut Vec<PecLine>, id: usize) -> &'a mut PecLine {
        if lines.len() <= id {
            lines.resize_with(id + 1, PecLine::default);
        }
        &mut lines[id]
    }

    for (line_no, raw) in source.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") { continue; }
        let ctx = || format!("line {}", line_no + 1);
        let mut it = line.split_whitespace();
        if !offset_seen {
            // First non-comment line: chart offset in ms. prpr subtracts 0.15s
            // (the audio latency calibration baked into the PEC format).
            offset_ms = it.next().ok_or_else(|| anyhow::anyhow!("{}: missing offset", ctx()))?.parse::<f64>()
                .map_err(|e| anyhow::anyhow!("{}: bad offset: {e}", ctx()))?;
            offset_seen = true;
            if it.next().is_some() {
                return Err(anyhow::anyhow!("{}: unexpected tokens after offset", ctx()));
            }
            continue;
        }
        let cmd = it.next().ok_or_else(|| anyhow::anyhow!("{}: empty command", ctx()))?;
        // token helper with contextual errors
        macro_rules! tok {
            () => {{
                it.next().ok_or_else(|| anyhow::anyhow!("{}: `{cmd}`: unexpected end of line", ctx()))?
            }};
        }
        macro_rules! f32t {
            () => {{
                tok!().parse::<f32>().map_err(|e| anyhow::anyhow!("{}: `{cmd}`: expected number, got {e}", ctx()))?
            }};
        }
        macro_rules! usizet {
            () => {{
                tok!().parse::<usize>().map_err(|e| anyhow::anyhow!("{}: `{cmd}`: expected integer, got {e}", ctx()))?
            }};
        }
        macro_rules! f64t {
            () => {{
                tok!().parse::<f64>().map_err(|e| anyhow::anyhow!("{}: `{cmd}`: expected number, got {e}", ctx()))?
            }};
        }
        match cmd {
            "bp" => {
                let a = f64t!();
                let b = f64t!();
                // PEC dialects disagree on the argument order: prpr writes
                // `bp <bpm> <beats>`, RPE's exporter writes `bp <beats> <bpm>`.
                // A beat is never a plausible BPM, so use that to disambiguate.
                let plausible_bpm = |v: f64| v > 0.0 && v <= 999.0;
                let (bpm, start_beats) = match (plausible_bpm(a), plausible_bpm(b)) {
                    (true, false) => (a, b),
                    (false, true) => (b, a),
                    _ => (a, b),
                };
                if bpm <= 0.0 {
                    return Err(anyhow::anyhow!("{}: `bp`: BPM must be positive, got {bpm}", ctx()));
                }
                bpm_list.push(RPEBpmItem { bpm, start_time: Triple::from_beats(start_beats) });
            }
            "n1" | "n2" | "n3" | "n4" => {
                let li = usizet!();
                let time = f64t!();
                let kind: u8 = match cmd.as_bytes()[1] {
                    b'1' => 1, b'2' => 2, b'3' => 3, _ => 4,
                };
                let end_time = if kind == 2 { f64t!() } else { time };
                let x_raw = f32t!();
                let above: u8 = if usizet!() == 1 { 1 } else { 0 };
                let fake: u8 = if usizet!() == 1 { 1 } else { 0 };
                // Per-note overrides: `# speed` then `& size`; fall back to
                // the global defaults (set by standalone `#`/`&` lines).
                let mut speed = default_speed;
                let mut size = default_size;
                while let Some(t) = it.next() {
                    match t {
                        "#" => speed = f32t!(),
                        "&" => size = f32t!(),
                        other => return Err(anyhow::anyhow!("{}: `{cmd}`: unexpected token `{other}`", ctx())),
                    }
                }
                let line = get_line(&mut lines, li);
                line.notes.push(RPENote {
                    kind, above,
                    start_time: Triple::from_beats(time),
                    end_time: Triple::from_beats(end_time),
                    position_x: x_raw / 1024.0 * 675.0,
                    y_offset: 0.0,
                    alpha: 255, hitsound: None,
                    size, speed, is_fake: fake, visible_time: 999999.,
                    tint: None, tint_hit_effects: None, judge_area: None,
                });
            }
            "cv" | "ca" | "cp" | "cd" | "cm" | "cr" | "cf" => {
                let li = usizet!();
                let time = f64t!();
                let line = get_line(&mut lines, li);
                match cmd {
                    "cv" => {
                        // PEC speed unit: prpr divides by 5.85; RPE applies its
                        // own SPEED_RATIO on top — compensate so the height
                        // integral matches prpr's PEC evaluation.
                        let v = f32t!();
                        line.speed.push((time, v / 5.85 / crate::core::SPEED_RATIO as f32));
                    }
                    "ca" => line.alpha.push(PecEvent::single(time, f32t!())),
                    "cp" => {
                        let x = f32t!();
                        let y = f32t!();
                        line.move_x.push(PecEvent::single(time, x));
                        line.move_y.push(PecEvent::single(time, y));
                    }
                    "cd" => line.rotate.push(PecEvent::single(time, f32t!())),
                    "cm" => {
                        let end = f64t!();
                        let x = f32t!();
                        let y = f32t!();
                        let tween = pec_tween(tok!().parse::<u8>().map_err(|e| anyhow::anyhow!("{}: `cm`: bad tween: {e}", ctx()))?);
                        line.move_x.push(PecEvent { start: time, end, value: x, tween });
                        line.move_y.push(PecEvent { start: time, end, value: y, tween });
                    }
                    "cr" => {
                        let end = f64t!();
                        let rot = f32t!();
                        let tween = pec_tween(tok!().parse::<u8>().map_err(|e| anyhow::anyhow!("{}: `cr`: bad tween: {e}", ctx()))?);
                        line.rotate.push(PecEvent { start: time, end, value: rot, tween });
                    }
                    "cf" => {
                        let end = f64t!();
                        let alpha = f32t!();
                        // prpr forces linear (tween 2 → RPE easingType 1).
                        line.alpha.push(PecEvent { start: time, end, value: alpha, tween: 1 });
                    }
                    _ => unreachable!(),
                }
            }
            "#" | "&" => {
                let v = f32t!();
                // Standalone `#`/`&`: prpr applies them to the last inserted
                // note; legacy PEC files also use them as global defaults
                // before any note exists — accept both.
                if let Some(last) = lines.last_mut().and_then(|l| l.notes.last_mut()) {
                    if cmd == "#" { last.speed = v; } else { last.size = v; }
                } else {
                    if cmd == "#" { default_speed = v; } else { default_size = v; }
                }
            }
            other => {
                return Err(anyhow::anyhow!("{}: unknown PEC command `{other}`", ctx()));
            }
        }
        if let Some(next) = it.next() {
            return Err(anyhow::anyhow!("{}: `{cmd}`: unexpected trailing token `{next}`", ctx()));
        }
    }

    if !offset_seen {
        bail!("empty PEC file");
    }
    if bpm_list.is_empty() {
        bpm_list.push(RPEBpmItem { bpm: 120.0, start_time: Triple::from_beats(0.0) });
    }
    if lines.is_empty() {
        lines.push(PecLine::default());
    }

    let mut judge_line_list = Vec::with_capacity(lines.len());
    for (i, mut pec) in lines.into_iter().enumerate() {
        sanitize_events(&mut pec.alpha);
        sanitize_events(&mut pec.move_x);
        sanitize_events(&mut pec.move_y);
        sanitize_events(&mut pec.rotate);
        // PEC speed is a step function (time → speed). Each step becomes a
        // zero-length RPE speed event: the evaluator drops the (empty) segment
        // but advances its cursor/last_speed, so the interval after the last
        // step is filled with that speed up to max_time.
        let mut speeds = pec.speed;
        speeds.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut speed_events: Vec<RPEEvent> = Vec::with_capacity(speeds.len());
        for &(t, v) in &speeds {
            speed_events.push(RPEEvent {
                start_time: Triple::from_beats(t),
                end_time: Triple::from_beats(t),
                start: v, end: v,
                easing_type: 1,
                easing_left: 0.0, easing_right: 1.0,
                bezier: 0, bezier_points: [0.0; 4],
            });
        }
        let event_layer = RPEEventLayer {
            alpha_events: Some(pec_to_rpe_events(pec.alpha, "a")),
            move_x_events: Some(pec_to_rpe_events(pec.move_x, "x")),
            move_y_events: Some(pec_to_rpe_events(pec.move_y, "y")),
            rotate_events: Some(pec_to_rpe_events(pec.rotate, "r")),
            speed_events: Some(speed_events),
        };
        judge_line_list.push(RPEJudgeLine {
            name: format!("PEC Line {i}"),
            texture: "line.png".into(),
            parent: None, rotate_with_father: None,
            event_layers: vec![Some(event_layer)],
            extended: None,
            notes: if pec.notes.is_empty() { None } else { Some(pec.notes) },
            is_cover: 0, z_order: 0, attach_ui: None,
            pos_control: vec![], size_control: vec![], alpha_control: vec![], y_control: vec![],
        });
    }

    Ok(RPEChart {
        meta: RPEMetadata { offset: ((offset_ms / 1000.0 - 0.15) * 1000.0) as i32, rpe_version: 160 },
        bpm_list,
        judge_line_list,
    })
}

// ── PGR (Phigros official chart JSON) parser ──

/// PGR (Phigros official JSON) format parser. Detects `"format_version"` or
/// `"judge_line_list"` (without `"META"`) and deserialises via serde with
/// camelCase mapping.
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

/// PSS (Phimakor Streamable Sheet v2, NDJSON) format parser. Detects
/// `"type":"meta"` on the first line and delegates to
/// [`from_stream_bytes`](crate::core::stream::from_stream_bytes).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::chart::Chart;

    const PEC_SAMPLE: &str = r#"1000
bp 120 0
n1 0 1 512 1 0
n2 0 1 3 1024 1 0 # 1.5 & 0.8
n3 0 2 768 1 1
n4 0 4 1280 0 0
cv 0 0 1.5
ca 0 0 255
cp 0 0 512 700
cd 0 0 30
cm 0 1 3 600 400 2
cr 0 2 4 45 2
cf 0 3 5 128
"#;

    #[test]
    fn pec_full_parse() {
        let chart = parse_pec_text(PEC_SAMPLE).unwrap();
        assert_eq!(chart.judge_line_list.len(), 1);
        let line = &chart.judge_line_list[0];
        assert_eq!(line.notes.as_ref().unwrap().len(), 4);
        let notes = line.notes.as_ref().unwrap();
        // n1: tap @ beat 1, x=512 → 512/1024*675 = 337.5, above, not fake
        assert_eq!(notes[0].kind, 1);
        assert!((notes[0].position_x - 337.5).abs() < 1e-3);
        assert_eq!(notes[0].above, 1);
        assert_eq!(notes[0].is_fake, 0);
        assert!((notes[0].start_time.beats() - 1.0).abs() < 1e-9);
        // n2: hold beat 1→3, per-note speed 1.5 / size 0.8
        assert_eq!(notes[1].kind, 2);
        assert!((notes[1].end_time.beats() - 3.0).abs() < 1e-9);
        assert!((notes[1].speed - 1.5).abs() < 1e-6);
        assert!((notes[1].size - 0.8).abs() < 1e-6);
        // n3: flick, fake
        assert_eq!(notes[2].kind, 3);
        assert_eq!(notes[2].is_fake, 1);
        // n4: drag, below
        assert_eq!(notes[3].kind, 4);
        assert_eq!(notes[3].above, 0);

        let layer = line.event_layers[0].as_ref().unwrap();
        // cv @ beat 0: 1.5 / 5.85 / SPEED_RATIO step segment
        let spd = layer.speed_events.as_ref().unwrap();
        assert!(!spd.is_empty());
        assert!((spd[0].end - 1.5 / 5.85 / crate::core::SPEED_RATIO as f32).abs() < 1e-4);
        // ca: alpha 255 (raw; RPE scales by 1/255)
        let alpha = layer.alpha_events.as_ref().unwrap();
        assert!((alpha[0].start - 255.0).abs() < 1e-6);
        // cp: move x 512 → (512/1024-1)*675 = -337.5 px; move y 700 → (700/1400-1)*450 = -225
        let mx = layer.move_x_events.as_ref().unwrap();
        let my = layer.move_y_events.as_ref().unwrap();
        assert!((mx[0].start + 337.5).abs() < 1e-3);
        assert!((my[0].start + 225.0).abs() < 1e-3);
        // cd: rotate 30 stays raw (RPE ×-1)
        let rot = layer.rotate_events.as_ref().unwrap();
        assert!((rot[0].start - 30.0).abs() < 1e-6);
        // cm: tweened move @ beat 1→3 with tween 2 (sine-out)
        assert!((mx[1].start_time.beats() - 1.0).abs() < 1e-9);
        assert!((mx[1].end_time.beats() - 3.0).abs() < 1e-9);
        assert_eq!(mx[1].easing_type, 2);
        // cf: alpha tween forced linear (RPE easingType 1)
        let alpha2 = layer.alpha_events.as_ref().unwrap();
        assert_eq!(alpha2[1].easing_type, 1);

        // Offset: 1000ms → (1000/1000 - 0.15) = 0.85s → 850
        assert_eq!(chart.meta.offset, 850);
        // BPM 120 @ beat 0
        assert_eq!(chart.bpm_list.len(), 1);
        assert!((chart.bpm_list[0].bpm - 120.0).abs() < 1e-9);
    }

    #[test]
    fn pec_multiple_lines_and_loading() {
        const SRC: &str = r#"0
n1 1 0 512 1 0
n1 0 4 768 1 0
"#;
        let chart = parse_pec_text(SRC).unwrap();
        assert_eq!(chart.judge_line_list.len(), 2);
        // The chart must load and evaluate through the main pipeline.
        let mut c = Chart::from_rpe_chart(&chart, false).unwrap();
        let frame = c.state_at(0.5);
        assert_eq!(frame.lines.len(), 2);
    }

    #[test]
    fn pec_errors_are_reported() {
        // Unknown command
        assert!(parse_pec_text("0\nzz 1 2 3\n").is_err());
        // Trailing junk after a note
        assert!(parse_pec_text("0\nn1 0 1 512 1 0 garbage\n").is_err());
        // Bad number
        assert!(parse_pec_text("0\nn1 0 xx 512 1 0\n").is_err());
        // Empty file
        assert!(parse_pec_text("").is_err());
        // detect: leading number only
        assert!(PecParser::detect(b"1000\nn1 0 1 512 1 0\n"));
        assert!(!PecParser::detect(b"{\"META\":{}}"));
    }

    #[test]
    fn pec_global_speed_before_notes() {
        // Legacy PEC flavor: `#` / `&` on their own lines BEFORE any note set
        // global defaults (prpr would reject this; the old Phimakor parser
        // accepted it and real-world charts rely on it).
        const SRC: &str = r#"0
# 0.8
& 1.25
n1 0 1 512 1 0
"#;
        let chart = parse_pec_text(SRC).unwrap();
        let notes = chart.judge_line_list[0].notes.as_ref().unwrap();
        assert_eq!(notes.len(), 1);
        assert!((notes[0].speed - 0.8).abs() < 1e-6);
        assert!((notes[0].size - 1.25).abs() < 1e-6);
    }

    #[test]
    fn pec_rpe_export_dialect() {
        // RPE's PEC exporter writes `bp <beats> <bpm>` (three repeated lines)
        // and x in the 0..2048 domain (1024 = center). A 0 BPM used to slip
        // through and poison the BPM list with inf → NaN beats.
        const SRC: &str = r#"-30
bp 0.000 320.000
bp 0.000 320.000
bp 0.000 320.000
n1 0 52.000 -269.000 1 0
n2 0 84.000 84.098 683.000 1 0
"#;
        let chart = parse_pec_text(SRC).unwrap();
        // bp lines disambiguated to bpm=320 @ beat 0
        assert_eq!(chart.bpm_list.len(), 3);
        assert!(chart.bpm_list.iter().all(|b| (b.bpm - 320.0).abs() < 1e-9));
        assert!(chart.bpm_list.iter().all(|b| b.start_time.beats().abs() < 1e-9));
        let notes = chart.judge_line_list[0].notes.as_ref().unwrap();
        assert_eq!(notes.len(), 2);
        // x: 0..2048 domain → /1024 * 675
        assert!((notes[0].position_x - (-269.0 / 1024.0 * 675.0)).abs() < 1e-3);
        assert!((notes[1].position_x - (683.0 / 1024.0 * 675.0)).abs() < 1e-3);
        // Loading through the full pipeline must not produce NaN beats.
        let mut c = Chart::from_rpe_chart(&chart, false).unwrap();
        let frame = c.state_at(1.0);
        assert!(frame.time.is_finite());
        assert!(frame.lines[0].notes.iter().all(|n| n.time.is_finite()));
    }
}


