//! PMK deserialization — bytes → `PmkDoc`.
//! Spec: `../docs/pmk-spec.md` §2–§19.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;

use crate::core::bpm::Triple;
use crate::core::model::*;

use super::frame::*;
use super::records::*;
use super::{OpaqueEvnt, OpaqueXevt, PmkDoc, PmkUnknown};

struct MetaVals {
    offset: Option<i32>,
    rpe_compat: Option<u32>,
    preview: Option<f32>,
    dim: Option<f32>,
    unknown_lines: Vec<String>,
}

fn parse_meta(text: &str) -> Result<MetaVals> {
    let mut offset = None;
    let mut rpe_compat = None;
    let mut preview = None;
    let mut dim = None;
    let mut unknown_lines = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            unknown_lines.push(raw.to_string());
            continue;
        };
        let at = || format!("META line {}", i + 1);
        match k.trim() {
            "version" => {
                let ver: u32 = v.trim().parse().map_err(|e| anyhow::anyhow!("{}: `version`: {e}", at()))?;
                if ver != 1 {
                    bail!("{}: unsupported format version {ver} (this reader supports 1)", at());
                }
            }
            "offset" => {
                offset = Some(v.trim().parse().map_err(|e| anyhow::anyhow!("{}: `offset`: {e}", at()))?);
            }
            "rpe_compat" => {
                rpe_compat = Some(v.trim().parse().map_err(|e| anyhow::anyhow!("{}: `rpe_compat`: {e}", at()))?);
            }
            "preview" => {
                preview = Some(v.trim().parse().map_err(|e| anyhow::anyhow!("{}: `preview`: {e}", at()))?);
            }
            "dim" => {
                dim = Some(v.trim().parse().map_err(|e| anyhow::anyhow!("{}: `dim`: {e}", at()))?);
            }
            _ => unknown_lines.push(raw.to_string()),
        }
    }
    Ok(MetaVals { offset, rpe_compat, preview, dim, unknown_lines })
}

struct AccLine {
    name: String,
    texture: String,
    parent: Option<isize>,
    rotate_with_father: Option<bool>,
    is_cover: u8,
    z_order: i32,
    attach_ui: Option<String>,
    unknown_tlvs: Vec<TlvField>,
    layers: HashMap<usize, Vec<EventCore>>,
    ext_events: Option<Vec<XevtValue>>,
    pos_control: Vec<RPECtrlEvent>,
    size_control: Vec<RPECtrlEvent>,
    alpha_control: Vec<RPECtrlEvent>,
    y_control: Vec<RPECtrlEvent>,
    notes: Vec<RPENote>,
    note_tlvs: Vec<Vec<TlvField>>,
    ext_seen: bool,
}

/// Parse a full PMK buffer (magic checked) into a document.
pub fn deserialize(bytes: &[u8]) -> Result<PmkDoc> {
    if bytes.len() < MAGIC.len() || bytes[..MAGIC.len()] != MAGIC {
        bail!("not a PMK file (bad magic)");
    }

    let mut meta: Option<MetaVals> = None;
    let mut info_json: Option<String> = None;
    let mut bpm: Vec<RPEBpmItem> = Vec::new();
    let mut lines: Vec<AccLine> = Vec::new();
    let mut unknown_chunks: Vec<Chunk> = Vec::new();
    let mut evnt_opaque: Vec<OpaqueEvnt> = Vec::new();
    let mut xevt_opaque: Vec<OpaqueXevt> = Vec::new();

    let mut it = ChunkIter::new(&bytes[MAGIC.len()..]);
    let mut seen = std::collections::HashSet::new();
    let mut first_offset = 0usize;
    while let Some(chunk) = it.next_chunk()? {
        let chunk_len = chunk.payload.len();
        match &chunk.tag {
            t if *t == TAG_META => {
                if meta.is_some() {
                    bail!("duplicate META chunk");
                }
                if first_offset != 0 {
                    bail!("META must be the first chunk");
                }
                let text = std::str::from_utf8(&chunk.payload).context("META not UTF-8")?;
                meta = Some(parse_meta(text)?);
            }
            t if *t == TAG_INFO => {
                ensure_single(&mut seen, &chunk.tag, "INFO")?;
                info_json = Some(String::from_utf8(chunk.payload.clone()).context("INFO not UTF-8")?);
            }
            t if *t == TAG_BPM => {
                ensure_single(&mut seen, &chunk.tag, "BPM")?;
                if chunk.payload.len() % 16 != 0 {
                    bail!("BPM payload length {} is not a multiple of 16", chunk.payload.len());
                }
                let mut cur = Cursor::new(&chunk.payload, "BPM");
                while cur.pos() < cur.len() {
                    let beat = cur.f64()?;
                    let bpm_v = cur.f64()?;
                    bpm.push(RPEBpmItem { bpm: bpm_v, start_time: Triple::from_beats(beat) });
                }
            }
            t if *t == TAG_LINE => {
                ensure_single(&mut seen, &chunk.tag, "LINE")?;
                lines = parse_lines(&chunk.payload)?;
            }
            t if *t == TAG_NOTE => parse_notes_chunk(&chunk, &mut lines)?,
            t if *t == TAG_EVNT => parse_evnt_chunk(&chunk, &mut lines, &mut evnt_opaque)?,
            t if *t == TAG_XEVT => parse_xevt_chunk(&chunk, &mut lines, &mut xevt_opaque)?,
            t if *t == TAG_CTRL => parse_ctrl_chunk(&chunk, &mut lines)?,
            t if *t == TAG_END => break,
            _ => unknown_chunks.push(chunk),
        }
        first_offset += 8 + chunk_len;
    }

    let meta = meta.context("missing META chunk")?;
    if lines.is_empty() {
        bail!("missing LINE chunk");
    }

    let mut bpm_list = bpm;
    bpm_list.sort_by(|a, b| a.start_time.beats().total_cmp(&b.start_time.beats()));
    if bpm_list.is_empty() {
        bpm_list.push(RPEBpmItem { bpm: 120.0, start_time: Triple::default() });
    }

    let mut pmk_line_tlvs: HashMap<usize, Vec<TlvField>> = HashMap::new();
    let mut pmk_note_tlvs: HashMap<(usize, usize), Vec<TlvField>> = HashMap::new();

    let judge_line_list = lines
        .into_iter()
        .enumerate()
        .map(|(li, mut acc)| {
            if !acc.unknown_tlvs.is_empty() {
                pmk_line_tlvs.insert(li, std::mem::take(&mut acc.unknown_tlvs));
            }
            for (ni, tlvs) in acc.note_tlvs.iter().enumerate() {
                if !tlvs.is_empty() {
                    pmk_note_tlvs.insert((li, ni), tlvs.clone());
                }
            }

            let max_layer = acc.layers.keys().max().copied().unwrap_or(0);
            let mut event_layers: Vec<Option<RPEEventLayer>> = Vec::new();
            for li in 0..=max_layer {
                let Some(events) = acc.layers.remove(&li) else {
                    event_layers.push(None);
                    continue;
                };
                let mut sorted = events;
                sorted.sort_by(|a, b| a.t1.total_cmp(&b.t1).then(a.kind.cmp(&b.kind)));
                let (alpha, move_x, move_y, rotate, speed) = split_event_layers(sorted);
                let opt = |v: Vec<RPEEvent>| if v.is_empty() { None } else { Some(v) };
                event_layers.push(Some(RPEEventLayer {
                    alpha_events: opt(alpha),
                    move_x_events: opt(move_x),
                    move_y_events: opt(move_y),
                    rotate_events: opt(rotate),
                    speed_events: opt(speed),
                }));
            }
            if event_layers.is_empty() {
                event_layers.push(None);
            }

            let extended = if let Some(mut events) = acc.ext_events.take() {
                events.sort_by(|a, b| xevt_key(a).total_cmp(&xevt_key(b)));
                let mut ext = RPEExtendedEvents {
                    color_events: None,
                    text_events: None,
                    scale_x_events: None,
                    scale_y_events: None,
                    incline_events: None,
                    paint_events: None,
                    gif_events: None,
                };
                for ev in events {
                    match ev {
                        XevtValue::Color(e) => ext.color_events.get_or_insert_with(Vec::new).push(e),
                        XevtValue::Text(e) => ext.text_events.get_or_insert_with(Vec::new).push(e),
                        XevtValue::Scalar(kind, e) => match kind {
                            3 => ext.scale_x_events.get_or_insert_with(Vec::new).push(e),
                            4 => ext.scale_y_events.get_or_insert_with(Vec::new).push(e),
                            5 => ext.incline_events.get_or_insert_with(Vec::new).push(e),
                            6 => ext.paint_events.get_or_insert_with(Vec::new).push(e),
                            _ => ext.gif_events.get_or_insert_with(Vec::new).push(e),
                        },
                    }
                }
                Some(ext)
            } else if acc.ext_seen {
                Some(RPEExtendedEvents {
                    color_events: None,
                    text_events: None,
                    scale_x_events: None,
                    scale_y_events: None,
                    incline_events: None,
                    paint_events: None,
                    gif_events: None,
                })
            } else {
                None
            };

            let notes = if acc.notes.is_empty() { None } else { Some(acc.notes) };
            RPEJudgeLine {
                name: acc.name,
                texture: acc.texture,
                parent: acc.parent,
                rotate_with_father: acc.rotate_with_father,
                event_layers,
                extended,
                notes,
                is_cover: acc.is_cover,
                z_order: acc.z_order,
                attach_ui: acc.attach_ui,
                pos_control: acc.pos_control,
                size_control: acc.size_control,
                alpha_control: acc.alpha_control,
                y_control: acc.y_control,
                comment: None,
            }
        })
        .collect();

    let info = match info_json {
        Some(json) => serde_json::from_str(&json).context("INFO chunk is not a valid ChartInfo JSON")?,
        None => {
            let mut info = ChartInfo::default();
            if let Some(p) = meta.preview {
                info.preview_start = p;
            }
            if let Some(d) = meta.dim {
                info.background_dim = d;
            }
            info
        }
    };

    Ok(PmkDoc {
        chart: RPEChart {
            meta: RPEMetadata {
                offset: meta.offset.unwrap_or(0),
                rpe_version: meta.rpe_compat.unwrap_or(160) as i32,
            },
            bpm_list,
            judge_line_list,
        },
        info,
        unknown: PmkUnknown {
            chunks: unknown_chunks,
            meta_lines: meta.unknown_lines,
            line_tlvs: pmk_line_tlvs,
            note_tlvs: pmk_note_tlvs,
            evnt_opaque,
            xevt_opaque,
        },
    })
}

fn xevt_key(v: &XevtValue) -> f64 {
    match v {
        XevtValue::Color(e) => e.start_time.beats(),
        XevtValue::Text(e) => e.start_time.beats(),
        XevtValue::Scalar(_, e) => e.start_time.beats(),
    }
}

fn ensure_single(seen: &mut std::collections::HashSet<[u8; 4]>, tag: &[u8; 4], name: &str) -> Result<()> {
    if !seen.insert(*tag) {
        bail!("duplicate {name} chunk");
    }
    Ok(())
}

fn line_ref<'a>(lines: &'a mut Vec<AccLine>, line_idx: u32) -> Result<&'a mut AccLine> {
    let n = lines.len();
    lines
        .get_mut(line_idx as usize)
        .ok_or_else(|| anyhow::anyhow!("line_idx {line_idx} out of range ({n} lines)"))
}

fn parse_lines(payload: &[u8]) -> Result<Vec<AccLine>> {
    let mut cur = Cursor::new(payload, "LINE");
    let count = cur.u32()? as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        cur.set_ctx(format!("LINE record {i}"));
        let pl = read_line_record(&mut cur)?;
        out.push(AccLine {
            name: pl.name,
            texture: pl.texture,
            parent: pl.parent,
            rotate_with_father: pl.rotate_with_father,
            is_cover: pl.is_cover,
            z_order: pl.z_order,
            attach_ui: pl.attach_ui,
            unknown_tlvs: pl.unknown_tlvs,
            layers: HashMap::new(),
            ext_events: None,
            pos_control: Vec::new(),
            size_control: Vec::new(),
            alpha_control: Vec::new(),
            y_control: Vec::new(),
            notes: Vec::new(),
            note_tlvs: Vec::new(),
            ext_seen: false,
        });
    }
    Ok(out)
}

fn parse_notes_chunk(chunk: &Chunk, lines: &mut Vec<AccLine>) -> Result<()> {
    let mut cur = Cursor::new(&chunk.payload, "NOTE");
    let line_idx = cur.u32()?;
    let count = cur.u32()? as usize;
    let line = line_ref(lines, line_idx)?;
    for i in 0..count {
        cur.set_ctx(format!("NOTE line {line_idx} note {i}"));
        let parsed = read_note_record(&mut cur)?;
        line.note_tlvs.push(parsed.unknown_tlvs);
        line.notes.push(parsed.note);
    }
    Ok(())
}

fn parse_evnt_chunk(chunk: &Chunk, lines: &mut Vec<AccLine>, opaque: &mut Vec<OpaqueEvnt>) -> Result<()> {
    let mut cur = Cursor::new(&chunk.payload, "EVNT");
    let line_idx = cur.u32()?;
    let layer = cur.u32()? as usize;
    let count = cur.u32()? as usize;
    if line_idx as usize >= lines.len() {
        bail!("EVNT line_idx {line_idx} out of range ({} lines)", lines.len());
    }
    for i in 0..count {
        cur.set_ctx(format!("EVNT line {line_idx} layer {layer} record {i}"));
        let start = cur.pos();
        let core = EventCore::read(&mut cur)?;
        if core.kind <= 4 {
            line_ref(lines, line_idx)?.layers.entry(layer).or_insert_with(Vec::new).push(core);
        } else {
            if core.flags & 1 != 0 {
                let mut junk = Vec::new();
                read_tlv(&mut cur, &mut |_, _| Ok(false), &mut junk)?;
            }
            opaque.push(OpaqueEvnt { line: line_idx as usize, layer, bytes: chunk.payload[start..cur.pos()].to_vec() });
        }
    }
    Ok(())
}

fn parse_xevt_chunk(chunk: &Chunk, lines: &mut Vec<AccLine>, opaque: &mut Vec<OpaqueXevt>) -> Result<()> {
    let mut cur = Cursor::new(&chunk.payload, "XEVT");
    let line_idx = cur.u32()?;
    let count = cur.u32()? as usize;
    if line_idx as usize >= lines.len() {
        bail!("XEVT line_idx {line_idx} out of range ({} lines)", lines.len());
    }
    {
        let line = line_ref(lines, line_idx)?;
        line.ext_seen = true;
    }
    for i in 0..count {
        cur.set_ctx(format!("XEVT line {line_idx} record {i}"));
        let start = cur.pos();
        let out = read_xevt_record(&mut cur)?;
        if let Some(value) = out.value {
            line_ref(lines, line_idx)?.ext_events.get_or_insert_with(Vec::new).push(value);
        }
        if out.opaque {
            opaque.push(OpaqueXevt { line: line_idx as usize, bytes: chunk.payload[start..cur.pos()].to_vec() });
        }
    }
    Ok(())
}

fn parse_ctrl_chunk(chunk: &Chunk, lines: &mut Vec<AccLine>) -> Result<()> {
    let mut cur = Cursor::new(&chunk.payload, "CTRL");
    let line_idx = cur.u32()?;
    let control_kind = cur.u32()?;
    let count = cur.u32()? as usize;
    let line = line_ref(lines, line_idx)?;
    for i in 0..count {
        cur.set_ctx(format!("CTRL line {line_idx} kind {control_kind} record {i}"));
        let rec = read_ctrl_record(&mut cur)?;
        let list = match control_kind {
            1 => &mut line.pos_control,
            2 => &mut line.size_control,
            3 => &mut line.alpha_control,
            _ => &mut line.y_control,
        };
        list.push(rec);
    }
    Ok(())
}
