//! PMK serialization — `PmkDoc` → bytes.
//! Spec: `../docs/pmk-spec.md` §4–§13, §16.

use anyhow::Result;
use crate::core::model::*;

use super::frame::*;
use super::records::*;
use super::{OpaqueEvnt, OpaqueXevt, PmkDoc};

/// Serialize a chart document into PMK bytes (spec §2–§13).
pub fn serialize(doc: &PmkDoc) -> Result<Vec<u8>> {
    let chart = &doc.chart;
    let mut w = Writer::new();

    // META (spec §5): known keys in canonical order, unknown lines preserved.
    let meta = format_meta(chart, &doc.info, &doc.unknown.meta_lines);
    w.chunk(TAG_META, meta.as_bytes());

    // INFO (spec §6).
    w.chunk(TAG_INFO, serde_json::to_string(&doc.info)?.as_bytes());

    // BPM (spec §7).
    let mut bpm = Vec::new();
    for b in &chart.bpm_list {
        bpm.extend_from_slice(&b.start_time.beats().to_le_bytes());
        bpm.extend_from_slice(&b.bpm.to_le_bytes());
    }
    w.chunk(TAG_BPM, &bpm);

    // LINE (spec §8).
    let mut line_payload = Vec::new();
    line_payload.extend_from_slice(&(chart.judge_line_list.len() as u32).to_le_bytes());
    for (i, line) in chart.judge_line_list.iter().enumerate() {
        let extra = doc.unknown.line_tlvs.get(&i).map(Vec::as_slice).unwrap_or(&[]);
        write_line_record(&mut line_payload, line, extra);
    }
    w.chunk(TAG_LINE, &line_payload);

    // NOTE (spec §9): one chunk per line, notes in model order.
    for (li, line) in chart.judge_line_list.iter().enumerate() {
        let Some(notes) = &line.notes else { continue };
        if notes.is_empty() {
            continue;
        }
        let mut p = Vec::new();
        p.extend_from_slice(&(li as u32).to_le_bytes());
        p.extend_from_slice(&(notes.len() as u32).to_le_bytes());
        for (ni, n) in notes.iter().enumerate() {
            let extra = doc.unknown.note_tlvs.get(&(li, ni)).map(Vec::as_slice).unwrap_or(&[]);
            write_note_record(&mut p, n, extra);
        }
        w.chunk(TAG_NOTE, &p);
    }

    // EVNT (spec §10): one chunk per (line, layer); opaque unknown records
    // are re-emitted after the decoded events of their own layer.
    for (li, line) in chart.judge_line_list.iter().enumerate() {
        for (ly, layer) in line.event_layers.iter().enumerate() {
            let mut events: Vec<EventCore> = Vec::new();
            if let Some(layer) = layer {
                for (kind, list) in rpe_layer_kinds(layer) {
                    for ev in list {
                        events.push(event_core_from_rpe_scalar(ev, kind));
                    }
                }
            }
            let opaque: Vec<&OpaqueEvnt> = doc
                .unknown
                .evnt_opaque
                .iter()
                .filter(|o| o.line == li && o.layer == ly)
                .collect();
            if events.is_empty() && opaque.is_empty() && layer.is_none() {
                continue;
            }
            events.sort_by(|a, b| a.t1.total_cmp(&b.t1).then(a.kind.cmp(&b.kind)));
            let mut p = Vec::new();
            p.extend_from_slice(&(li as u32).to_le_bytes());
            p.extend_from_slice(&(ly as u32).to_le_bytes());
            p.extend_from_slice(&((events.len() + opaque.len()) as u32).to_le_bytes());
            for e in &events {
                e.write(&mut p);
            }
            for o in &opaque {
                p.extend_from_slice(&o.bytes);
            }
            w.chunk(TAG_EVNT, &p);
        }
    }

    // XEVT (spec §11): one chunk per line.
    for (li, line) in chart.judge_line_list.iter().enumerate() {
        let mut recs: Vec<XevtEnc> = Vec::new();
        if let Some(ext) = &line.extended {
            for ev in ext.color_events.iter().flatten() {
                recs.push(xevt_color(ev));
            }
            for ev in ext.text_events.iter().flatten() {
                recs.push(xevt_text(ev));
            }
            for (kind, list) in [
                (3u8, &ext.scale_x_events),
                (4, &ext.scale_y_events),
                (5, &ext.incline_events),
                (6, &ext.paint_events),
                (7, &ext.gif_events),
            ] {
                for ev in list.iter().flatten() {
                    recs.push(XevtEnc { core: event_core_from_rpe_scalar(ev, kind), tlv: None });
                }
            }
        }
        let opaque: Vec<&OpaqueXevt> = doc
            .unknown
            .xevt_opaque
            .iter()
            .filter(|o| o.line == li)
            .collect();
        if recs.is_empty() && opaque.is_empty() && line.extended.is_none() {
            continue;
        }
        recs.sort_by(|a, b| a.core.t1.total_cmp(&b.core.t1).then(a.core.kind.cmp(&b.core.kind)));
        let mut p = Vec::new();
        p.extend_from_slice(&(li as u32).to_le_bytes());
        p.extend_from_slice(&((recs.len() + opaque.len()) as u32).to_le_bytes());
        for r in &recs {
            r.core.write(&mut p);
            if let Some(tlv) = &r.tlv {
                p.extend_from_slice(tlv);
            }
        }
        for o in &opaque {
            p.extend_from_slice(&o.bytes);
        }
        w.chunk(TAG_XEVT, &p);
    }

    // CTRL (spec §12): one chunk per (line, control kind).
    for (li, line) in chart.judge_line_list.iter().enumerate() {
        for (kind, list) in [
            (1u32, &line.pos_control),
            (2, &line.size_control),
            (3, &line.alpha_control),
            (4, &line.y_control),
        ] {
            if list.is_empty() {
                continue;
            }
            let mut p = Vec::new();
            p.extend_from_slice(&(li as u32).to_le_bytes());
            p.extend_from_slice(&kind.to_le_bytes());
            p.extend_from_slice(&(list.len() as u32).to_le_bytes());
            for c in list {
                write_ctrl_record(&mut p, c);
            }
            w.chunk(TAG_CTRL, &p);
        }
    }

    // Unknown chunks: verbatim passthrough, original relative order (spec §15.1).
    for chunk in &doc.unknown.chunks {
        w.chunk(chunk.tag, &chunk.payload);
    }

    // END index (spec §13).
    let indexed: Vec<u32> = INDEXED_TAGS.map(u32::from_le_bytes).to_vec();
    let entries: Vec<(u32, u64)> = w
        .index
        .iter()
        .filter(|(t, _)| indexed.contains(t))
        .copied()
        .collect();
    let mut end = Vec::new();
    end.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (tag, offset) in &entries {
        end.extend_from_slice(&tag.to_le_bytes());
        end.extend_from_slice(&offset.to_le_bytes());
    }
    w.chunk(TAG_END, &end);

    let mut out = Vec::with_capacity(MAGIC.len() + w.buf.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&w.buf);
    Ok(out)
}

fn format_meta(chart: &RPEChart, info: &ChartInfo, unknown_lines: &[String]) -> String {
    let mut s = format!(
        "version=1\noffset={}\nrpe_compat={}\n",
        chart.meta.offset, chart.meta.rpe_version
    );
    if info.preview_start != 0.0 {
        s.push_str(&format!("preview={}\n", info.preview_start));
    }
    if info.background_dim != 0.6 {
        s.push_str(&format!("dim={}\n", info.background_dim));
    }
    for l in unknown_lines {
        s.push_str(l);
        s.push('\n');
    }
    s
}
