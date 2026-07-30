//! PSS — Phimakor Streamable Sheet v2.
//! Time-chunked NDJSON format for random-access seeking.
//!
//! Record sequence:
//!   meta     → metadata (always first)
//!   bpm      → BPM list
//!   lineDef  → judge line definitions (one per line)
//!   chunk    → time window [t, t+span) with active events + notes
//!   end      → chunk index for HTTP Range seeking (always last)
//!
//! Seeking to time T:
//!   1. Parse meta + bpm + lineDefs (always at start)
//!   2. Parse end record (always last line)
//!   3. Binary search chunks to find chunk containing T
//!   4. HTTP Range request that chunk's byte range
//!   5. Parse chunk → full frame state for T

use anyhow::{bail, Context, Result};
use crate::core::bpm::Triple;
use crate::core::model::*;
use std::collections::HashMap;

const DEFAULT_CHUNK_SPAN: f64 = 10.0;

/// Serialize into PSS bytes (tracking chunk offsets for seeking).
pub fn to_stream_bytes(chart: &RPEChart, info: &ChartInfo) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut chunk_index: Vec<ChunkEntry> = Vec::new();

    // meta
    let meta = serde_json::to_string(&StreamRecord::Meta(StreamMeta {
        format_version: 2, name: info.name.clone(), composer: info.composer.clone(),
        level: info.level.clone(), difficulty: info.difficulty,
        charter: info.charter.clone(), illustrator: info.illustrator.clone(),
        music: info.music.clone(), illustration: info.illustration.clone(),
        offset: info.offset, background_dim: info.background_dim,
        preview_start: info.preview_start,
    }))?;
    out.extend_from_slice(meta.as_bytes());
    out.push(b'\n');

    // bpm
    let bpm_items: Vec<StreamBpmItem> = chart.bpm_list.iter().map(|b| StreamBpmItem {
        bpm: b.bpm, start_time: b.start_time.beats(),
    }).collect();
    let bpm = serde_json::to_string(&StreamRecord::Bpm(StreamBpm { list: bpm_items }))?;
    out.extend_from_slice(bpm.as_bytes());
    out.push(b'\n');

    // line defs
    for (i, line) in chart.judge_line_list.iter().enumerate() {
        let ld = serde_json::to_string(&StreamRecord::LineDef(StreamLineDef {
            index: i as u32, name: line.name.clone(), texture: line.texture.clone(),
            parent: line.parent, z_order: line.z_order, is_cover: line.is_cover,
            attach_ui: line.attach_ui.clone(),
        }))?;
        out.extend_from_slice(ld.as_bytes());
        out.push(b'\n');
    }

    // Collect all events with their line/layer/kind paths
    struct FlatEvent {
        line: usize, layer: usize, kind: String,
        start_time: f64, end_time: f64,
        start: f32, end: f32, easing_type: i32,
        easing_left: f32, easing_right: f32, bezier: u8,
        bezier_points: [f32; 4],
    }
    struct FlatNote {
        line: usize, kind: u8, above: u8,
        time: f64, hold_end: f64,
        x: f32, y: f32,
        size: f32, speed: f32, alpha: u16, fake: u8,
    }

    let mut flat_events: Vec<FlatEvent> = Vec::new();
    let mut flat_notes: Vec<FlatNote> = Vec::new();
    let mut max_time: f64 = 0.0;

    for (li, line) in chart.judge_line_list.iter().enumerate() {
        for (ly, layer) in line.event_layers.iter().enumerate() {
            let Some(layer) = layer else { continue };
            for (kind_name, events) in [
                ("alpha", &layer.alpha_events),
                ("moveX", &layer.move_x_events),
                ("moveY", &layer.move_y_events),
                ("rotate", &layer.rotate_events),
                ("speed", &layer.speed_events),
            ] {
                let Some(events) = events else { continue };
                for ev in events {
                    let sb = ev.start_time.beats();
                    let eb = ev.end_time.beats();
                    max_time = max_time.max(eb);
                    flat_events.push(FlatEvent {
                        line: li, layer: ly, kind: kind_name.to_string(),
                        start_time: sb, end_time: eb,
                        start: ev.start, end: ev.end,
                        easing_type: ev.easing_type,
                        easing_left: ev.easing_left,
                        easing_right: ev.easing_right,
                        bezier: ev.bezier,
                        bezier_points: ev.bezier_points,
                    });
                }
            }
        }
        if let Some(notes) = &line.notes {
            for n in notes {
                let t = n.start_time.beats();
                let he = n.end_time.beats();
                max_time = max_time.max(he);
                flat_notes.push(FlatNote {
                    line: li, kind: n.kind, above: n.above, time: t, hold_end: he,
                    x: n.position_x, y: n.y_offset,
                    size: n.size, speed: n.speed, alpha: n.alpha, fake: n.is_fake,
                });
            }
        }
    }

    // Partition into time chunks
    let span = DEFAULT_CHUNK_SPAN;
    let num_chunks = ((max_time / span).ceil() as usize).max(1);
    let mut chunk_events: Vec<Vec<&FlatEvent>> = vec![Vec::new(); num_chunks];
    let mut chunk_notes: Vec<Vec<&FlatNote>> = vec![Vec::new(); num_chunks];

    // Each event goes into every chunk it overlaps
    for ev in &flat_events {
        let start_chunk = ((ev.start_time / span).floor() as usize).min(num_chunks - 1);
        let end_chunk = ((ev.end_time / span).floor() as usize).min(num_chunks - 1);
        for ci in start_chunk..=end_chunk {
            chunk_events[ci].push(ev);
        }
    }
    for note in &flat_notes {
        let ci = ((note.time / span).floor() as usize).min(num_chunks - 1);
        chunk_notes[ci].push(note);
    }

    // Write chunks
    for (ci, events) in chunk_events.iter().enumerate() {
        let t0 = ci as f64 * span;
        let offset_before = out.len();

        let ev_records: Vec<ChunkEvent> = events.iter().map(|e| ChunkEvent {
            line: e.line as u32, layer: e.layer as u32, kind: e.kind.clone(),
            start_time: e.start_time, end_time: e.end_time,
            start: e.start, end: e.end,
            easing_type: e.easing_type,
            easing_left: e.easing_left, easing_right: e.easing_right,
            bezier: e.bezier, bezier_points: e.bezier_points,
        }).collect();
        let note_records: Vec<ChunkNote> = chunk_notes[ci].iter().map(|n| ChunkNote {
            line: n.line as u32, kind: n.kind, above: n.above,
            time: n.time, hold_end: n.hold_end,
            x: n.x, y: n.y,
            size: n.size, speed: n.speed, alpha: n.alpha, fake: n.fake,
        }).collect();

        let chunk = serde_json::to_string(&StreamRecord::Chunk(ChunkRecord {
            t: t0, span,
            events: ev_records, notes: note_records,
        }))?;
        out.extend_from_slice(chunk.as_bytes());
        out.push(b'\n');

        chunk_index.push(ChunkEntry { t: t0, offset: offset_before as u64 });
    }

    // end record with chunk index
    let end = serde_json::to_string(&StreamRecord::End(StreamEnd { chunks: chunk_index }))?;
    out.extend_from_slice(end.as_bytes());
    out.push(b'\n');

    Ok(out)
}

/// Parse full PSS from bytes (all chunks).
pub fn from_stream_bytes(bytes: &[u8]) -> Result<(RPEChart, ChartInfo)> {
    let source = std::str::from_utf8(bytes)?;
    let mut info = ChartInfo::default();
    let mut bpm_list: Vec<RPEBpmItem> = Vec::new();
    let mut judge_lines: HashMap<usize, RPEJudgeLine> = HashMap::new();
    let mut meta_offset = 0i32;

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let Ok(record) = serde_json::from_str::<StreamRecord>(line) else { continue; };
        match record {
            StreamRecord::Meta(m) => {
                info.name = m.name; info.composer = m.composer;
                info.level = m.level; info.difficulty = m.difficulty;
                info.charter = m.charter; info.illustrator = m.illustrator;
                info.music = m.music; info.illustration = m.illustration;
                info.offset = m.offset; info.background_dim = m.background_dim;
                info.preview_start = m.preview_start;
            }
            StreamRecord::Bpm(b) => {
                for item in b.list {
                    bpm_list.push(RPEBpmItem {
                        bpm: item.bpm, start_time: Triple::from_beats(item.start_time),
                    });
                }
            }
            StreamRecord::LineDef(ld) => {
                judge_lines.insert(ld.index as usize, RPEJudgeLine {
                    name: ld.name, texture: ld.texture,
                    parent: ld.parent, rotate_with_father: None,
                    event_layers: vec![None], extended: None,
                    notes: None, is_cover: ld.is_cover, z_order: ld.z_order,
                    attach_ui: ld.attach_ui,
                    pos_control: vec![], size_control: vec![],
                    alpha_control: vec![], y_control: vec![],
                });
            }
            StreamRecord::Chunk(c) => {
                // Merge chunk events and notes into judge lines
                for ev in c.events {
                    let kind = match ev.kind.as_str() {
                        "alpha" => Some(0), "moveX" => Some(1), "moveY" => Some(2),
                        "rotate" => Some(3), "speed" => Some(4), _ => None,
                    };
                    let Some(kind) = kind else { continue };
                    let (li, ly) = (ev.line as usize, ev.layer as usize);
                    let line = judge_lines.entry(li).or_insert_with(|| RPEJudgeLine {
                        name: format!("Line {li}"), texture: "line.png".into(),
                        parent: None, rotate_with_father: None,
                        event_layers: Vec::new(), extended: None,
                        notes: None, is_cover: 0, z_order: 0, attach_ui: None,
                        pos_control: vec![], size_control: vec![],
                        alpha_control: vec![], y_control: vec![],
                    });
                    // Ensure enough layers
                    while line.event_layers.len() <= ly {
                        line.event_layers.push(None);
                    }
                    let layer = line.event_layers[ly].get_or_insert_with(|| RPEEventLayer {
                        alpha_events: None, move_x_events: None,
                        move_y_events: None, rotate_events: None,
                        speed_events: None,
                    });
                    let rpe_ev = RPEEvent {
                        start_time: Triple::from_beats(ev.start_time),
                        end_time: Triple::from_beats(ev.end_time),
                        start: ev.start, end: ev.end,
                        easing_type: ev.easing_type,
                        easing_left: ev.easing_left, easing_right: ev.easing_right,
                        bezier: ev.bezier, bezier_points: ev.bezier_points,
                    };
                    let list = match kind {
                        0 => &mut layer.alpha_events,
                        1 => &mut layer.move_x_events,
                        2 => &mut layer.move_y_events,
                        3 => &mut layer.rotate_events,
                        4 => &mut layer.speed_events,
                        _ => unreachable!(),
                    };
                    list.get_or_insert_with(Vec::new).push(rpe_ev);
                }
                for n in c.notes {
                    let li = n.line as usize;
                    let note = RPENote {
                        kind: n.kind, above: n.above,
                        start_time: Triple::from_beats(n.time),
                        end_time: Triple::from_beats(n.hold_end),
                        position_x: n.x, y_offset: n.y, alpha: n.alpha,
                        hitsound: None, size: n.size, speed: n.speed,
                        is_fake: n.fake, visible_time: 999999.,
                        tint: None, tint_hit_effects: None, judge_area: None,
                    };
                    let line = judge_lines.entry(li).or_insert_with(|| RPEJudgeLine {
                        name: format!("Line {li}"), texture: "line.png".into(),
                        parent: None, rotate_with_father: None,
                        event_layers: Vec::new(), extended: None,
                        notes: None, is_cover: 0, z_order: 0, attach_ui: None,
                        pos_control: vec![], size_control: vec![],
                        alpha_control: vec![], y_control: vec![],
                    });
                    line.notes.get_or_insert_with(Vec::new).push(note);
                }
            }
            StreamRecord::End(_) => {} // skip, used only for seeking
        }
    }

    if bpm_list.is_empty() {
        bpm_list.push(RPEBpmItem { bpm: 120.0, start_time: Triple::from_beats(0.0) });
    }
    info.format = Some(ChartFormat::Pss);

    // Sort events within each kind (chunks may deliver out of order)
    for line in judge_lines.values_mut() {
        for layer in line.event_layers.iter_mut().flatten() {
            for list in [&mut layer.alpha_events, &mut layer.move_x_events, &mut layer.move_y_events,
                         &mut layer.rotate_events, &mut layer.speed_events] {
                if let Some(list) = list {
                    list.sort_by(|a, b| a.start_time.beats().total_cmp(&b.start_time.beats()));
                }
            }
        }
    }

    let num_lines = judge_lines.keys().max().map(|m| m + 1).unwrap_or(0);
    let mut judge_line_list: Vec<RPEJudgeLine> = Vec::with_capacity(num_lines);
    for i in 0..num_lines {
        judge_line_list.push(judge_lines.remove(&i).unwrap_or(RPEJudgeLine {
            name: format!("Line {i}"), texture: "line.png".into(),
            parent: None, rotate_with_father: None,
            event_layers: vec![None], extended: None,
            notes: None, is_cover: 0, z_order: 0, attach_ui: None,
            pos_control: vec![], size_control: vec![],
            alpha_control: vec![], y_control: vec![],
        }));
    }

    Ok((RPEChart {
        meta: RPEMetadata { offset: (info.offset * 1000.0) as i32, rpe_version: 160 },
        bpm_list,
        judge_line_list,
    }, info))
}

// ── Serde structs ──

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum StreamRecord {
    Meta(StreamMeta),
    Bpm(StreamBpm),
    #[serde(rename = "lineDef")]
    LineDef(StreamLineDef),
    Chunk(ChunkRecord),
    End(StreamEnd),
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StreamMeta {
    format_version: u32,
    #[serde(default)] name: String,
    #[serde(default)] composer: String,
    #[serde(default)] level: String,
    #[serde(default)] difficulty: f32,
    #[serde(default)] charter: String,
    #[serde(default)] illustrator: String,
    #[serde(default)] music: String,
    #[serde(default)] illustration: String,
    #[serde(default)] offset: f32,
    #[serde(default)] background_dim: f32,
    #[serde(default)] preview_start: f32,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StreamBpmItem { bpm: f64, start_time: f64 }

#[derive(serde::Serialize, serde::Deserialize)]
struct StreamBpm { list: Vec<StreamBpmItem> }

#[derive(serde::Serialize, serde::Deserialize)]
struct StreamLineDef {
    index: u32,
    name: String,
    #[serde(default)] texture: String,
    #[serde(skip_serializing_if = "Option::is_none")] parent: Option<isize>,
    #[serde(default)] z_order: i32,
    #[serde(default)] is_cover: u8,
    #[serde(skip_serializing_if = "Option::is_none")] attach_ui: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ChunkRecord {
    t: f64,
    span: f64,
    #[serde(default)] events: Vec<ChunkEvent>,
    #[serde(default)] notes: Vec<ChunkNote>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ChunkEvent {
    line: u32, layer: u32, kind: String,
    start_time: f64, end_time: f64,
    start: f32, end: f32,
    #[serde(default)] easing_type: i32,
    #[serde(default)] easing_left: f32,
    #[serde(default = "f32_one")] easing_right: f32,
    #[serde(default)] bezier: u8,
    #[serde(default)] bezier_points: [f32; 4],
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ChunkNote {
    line: u32, kind: u8, above: u8,
    time: f64, hold_end: f64,
    x: f32, y: f32,
    #[serde(default)] size: f32,
    #[serde(default)] speed: f32,
    #[serde(default)] alpha: u16,
    #[serde(default)] fake: u8,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StreamEnd {
    chunks: Vec<ChunkEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ChunkEntry {
    t: f64,
    offset: u64,
}

fn f32_one() -> f32 { 1.0 }
