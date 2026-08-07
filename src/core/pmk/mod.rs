//! PMK — PhiMakor Chart Format v1 (spec: `../docs/pmk-spec.md`).
//!
//! Binary container format, native save format of the editor: hand-rolled
//! fixed-size records + TLV tails, unknown chunks/fields preserved verbatim
//! (forward compatible). `RPEChart` stays the canonical interchange model;
//! this module is its lossless bidirectional mapping.

#![allow(dead_code)] // 部分 API 供编辑器原生保存路径/工具使用,主程序暂未全部接线

pub mod de;
pub mod frame;
pub mod records;
pub mod ser;

use std::collections::HashMap;

use crate::core::model::{ChartInfo, RPEChart};

pub use de::deserialize as from_bytes;
pub use frame::MAGIC;
pub use records::TlvField;
pub use ser::serialize as to_bytes;

/// Raw record of an unknown-kind core event, re-emitted verbatim.
#[derive(Clone, Debug)]
pub struct OpaqueEvnt {
    pub line: usize,
    pub layer: usize,
    pub bytes: Vec<u8>,
}

/// Raw record of an unknown/undecodable XEVT event, re-emitted verbatim.
#[derive(Clone, Debug)]
pub struct OpaqueXevt {
    pub line: usize,
    pub bytes: Vec<u8>,
}

/// Everything the reader must preserve across a load→save round trip that the
/// canonical model cannot express (spec §15).
#[derive(Default, Clone, Debug)]
pub struct PmkUnknown {
    /// Unknown chunks in original order (verbatim passthrough).
    pub chunks: Vec<frame::Chunk>,
    /// Unknown META `key=value` lines (raw text, original order).
    pub meta_lines: Vec<String>,
    /// Unknown LINE TLV segments, keyed by line index.
    pub line_tlvs: HashMap<usize, Vec<TlvField>>,
    /// Unknown NOTE TLV segments, keyed by (line, note ordinal in file order).
    pub note_tlvs: HashMap<(usize, usize), Vec<TlvField>>,
    /// Unknown-kind EVNT records.
    pub evnt_opaque: Vec<OpaqueEvnt>,
    /// Unknown/undecodable XEVT records.
    pub xevt_opaque: Vec<OpaqueXevt>,
}

/// A parsed PMK document: the canonical chart, its info, and preservation data.
#[derive(Clone, Debug)]
pub struct PmkDoc {
    pub chart: RPEChart,
    pub info: ChartInfo,
    pub unknown: PmkUnknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bpm::Triple;
    use crate::core::model::*;
    use crate::core::pmk::frame::{Writer, TAG_BPM, TAG_END, TAG_EVNT, TAG_LINE, TAG_META, TAG_NOTE};
    use crate::core::pmk::records::{read_note_record, write_note_record, Cursor, TlvField};

    fn ev(t1: f64, t2: f64, v1: f32, v2: f32, ease: i32) -> RPEEvent {
        RPEEvent {
            start_time: Triple::from_beats(t1),
            end_time: Triple::from_beats(t2),
            start: v1,
            end: v2,
            easing_type: ease,
            easing_left: 0.0,
            easing_right: 1.0,
            bezier: 0,
            bezier_points: [0.0; 4],
        }
    }

    fn rich_chart() -> (RPEChart, ChartInfo) {
        let note1 = RPENote {
            kind: 1,
            above: 1,
            start_time: Triple::from_beats(1.0),
            end_time: Triple::from_beats(1.0),
            position_x: 337.5,
            y_offset: 12.0,
            alpha: 255,
            hitsound: Some("tap.mp3".into()),
            size: 1.0,
            speed: 1.2,
            is_fake: 0,
            visible_time: 999999.0,
            tint: Some([255, 0, 128]),
            tint_hit_effects: None,
            judge_area: Some(0.9),
            comment: None,
        };
        let note2 = RPENote {
            kind: 2,
            above: 0,
            start_time: Triple::from_beats(4.0),
            end_time: Triple::from_beats(6.0),
            position_x: 1000.0,
            y_offset: 0.0,
            alpha: 128,
            hitsound: None,
            size: 0.8,
            speed: 1.0,
            is_fake: 1,
            visible_time: 2.5,
            tint: None,
            tint_hit_effects: Some([10, 20, 30]),
            judge_area: None,
            comment: None,
        };
        let note3 = RPENote {
            kind: 9,
            ..note2.clone()
        };
        let layer0 = RPEEventLayer {
            alpha_events: Some(vec![ev(0.0, 4.0, 255.0, 0.0, 1)]),
            move_x_events: Some(vec![ev(0.0, 2.0, 0.0, 675.0, 2)]),
            move_y_events: None,
            rotate_events: Some(vec![ev(0.0, 8.0, 0.0, 45.0, 3)]),
            speed_events: Some(vec![ev(0.0, 0.0, 1.0, 1.0, 0)]),
        };
        let layer1 = RPEEventLayer {
            alpha_events: Some(vec![ev(2.0, 6.0, 255.0, 100.0, 4)]),
            move_x_events: None,
            move_y_events: None,
            rotate_events: None,
            speed_events: None,
        };
        let ext = RPEExtendedEvents {
            color_events: Some(vec![RPEEvent {
                start_time: Triple::from_beats(0.0),
                end_time: Triple::from_beats(4.0),
                start: RGBColor(255, 0, 0),
                end: RGBColor(0, 0, 255),
                easing_type: 1,
                easing_left: 0.0,
                easing_right: 1.0,
                bezier: 0,
                bezier_points: [0.0; 4],
            }]),
            text_events: Some(vec![RPEEvent {
                start_time: Triple::from_beats(1.0),
                end_time: Triple::from_beats(2.0),
                start: "Hello".into(),
                end: "World".into(),
                easing_type: 2,
                easing_left: 0.0,
                easing_right: 1.0,
                bezier: 0,
                bezier_points: [0.0; 4],
            }]),
            scale_x_events: Some(vec![ev(0.0, 1.0, 1.0, 2.0, 1)]),
            scale_y_events: None,
            incline_events: None,
            paint_events: None,
            gif_events: None,
        };
        let chart = RPEChart {
            meta: RPEMetadata { offset: -150, rpe_version: 170 },
            bpm_list: vec![
                RPEBpmItem { bpm: 120.0, start_time: Triple::from_beats(0.0) },
                RPEBpmItem { bpm: 180.0, start_time: Triple::from_beats(8.0) },
            ],
            judge_line_list: vec![
                RPEJudgeLine {
                    name: "Main".into(),
                    texture: "line.png".into(),
                    parent: Some(1),
                    rotate_with_father: Some(true),
                    event_layers: vec![Some(layer0), Some(layer1)],
                    extended: Some(ext),
                    notes: Some(vec![note1, note2, note3]),
                    is_cover: 1,
                    z_order: 5,
                    attach_ui: Some("pause".into()),
                    pos_control: vec![RPECtrlEvent {
                        easing: 1,
                        x: 0.5,
                        value: HashMap::from([("pos".to_string(), 1.0), ("extra".to_string(), 0.25)]),
                    }],
                    size_control: vec![],
                    alpha_control: vec![RPECtrlEvent { easing: 2, x: 1.0, value: HashMap::from([("alpha".to_string(), 0.5)]) }],
                    y_control: vec![],
                    comment: None,
                },
                RPEJudgeLine {
                    name: "Sub".into(),
                    texture: "line.png".into(),
                    parent: None,
                    rotate_with_father: None,
                    event_layers: vec![None],
                    extended: None,
                    notes: None,
                    is_cover: 0,
                    z_order: 0,
                    attach_ui: None,
                    pos_control: vec![],
                    size_control: vec![],
                    alpha_control: vec![],
                    y_control: vec![],
                    comment: None,
                },
            ],
        };
        let mut info = ChartInfo::default();
        info.name = "Test Song".into();
        info.composer = "Tester".into();
        info.preview_start = 12.5;
        info.background_dim = 0.7;
        (chart, info)
    }

    fn assert_json_eq(a: &RPEChart, b: &RPEChart) {
        let ja = serde_json::to_value(a).unwrap();
        let jb = serde_json::to_value(b).unwrap();
        assert_eq!(ja, jb);
    }

    #[test]
    fn rich_roundtrip_is_lossless() {
        let (chart, info) = rich_chart();
        let doc = PmkDoc { chart: chart.clone(), info: info.clone(), unknown: PmkUnknown::default() };
        let bytes = to_bytes(&doc).unwrap();
        let back = from_bytes(&bytes).unwrap();
        assert_json_eq(&chart, &back.chart);
        assert_eq!(back.info.name, "Test Song");
        assert_eq!(back.info.preview_start, 12.5);
        assert_eq!(back.info.background_dim, 0.7);
        assert!(back.unknown.chunks.is_empty());
    }

    #[test]
    fn serialize_is_idempotent() {
        let (chart, info) = rich_chart();
        let doc = PmkDoc { chart: chart.clone(), info: info.clone(), unknown: PmkUnknown::default() };
        let bytes1 = to_bytes(&doc).unwrap();
        let doc2 = from_bytes(&bytes1).unwrap();
        let bytes2 = to_bytes(&doc2).unwrap();
        assert_eq!(bytes1, bytes2, "PMK→PMK must be byte-stable");
    }

    fn one_line_payload() -> Vec<u8> {
        let mut line = Vec::new();
        line.extend_from_slice(&1u32.to_le_bytes());
        line.extend_from_slice(&(-1i32).to_le_bytes());
        line.extend_from_slice(&0i32.to_le_bytes());
        line.extend_from_slice(&[0u8; 4]);
        line.extend_from_slice(&0u32.to_le_bytes());
        line.extend_from_slice(&0u32.to_le_bytes());
        line.push(0);
        line
    }

    #[test]
    fn unknown_chunk_is_preserved() {
        let mut w = Writer::new();
        w.chunk(TAG_META, b"version=1\noffset=0\n");
        w.chunk(TAG_LINE, &one_line_payload());
        w.chunk(*b"TEST", b"\xde\xad\xbe\xef");
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&w.buf);

        let doc = from_bytes(&bytes).unwrap();
        assert_eq!(doc.unknown.chunks.len(), 1);
        assert_eq!(doc.unknown.chunks[0].tag, *b"TEST");
        assert_eq!(doc.unknown.chunks[0].payload, vec![0xde, 0xad, 0xbe, 0xef]);
        // Re-serialization re-emits the unknown chunk (before END).
        let out = to_bytes(&doc).unwrap();
        assert!(out.windows(12).any(|w| w == b"TEST\x04\x00\x00\x00\xde\xad\xbe\xef"));
    }

    #[test]
    fn unknown_note_tlv_is_preserved() {
        let mut w = Writer::new();
        w.chunk(TAG_META, b"version=1\n");
        w.chunk(TAG_LINE, &one_line_payload());
        let mut n = RPENote::default();
        n.kind = 1;
        n.start_time = Triple::from_beats(1.0);
        n.end_time = Triple::from_beats(1.0);
        let mut note = Vec::new();
        write_note_record(&mut note, &n, &[TlvField { id: 9, payload: vec![1, 2, 3, 4] }]);
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&note);
        w.chunk(TAG_NOTE, &payload);
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&w.buf);

        let doc2 = from_bytes(&bytes).unwrap();
        let segs = doc2.unknown.note_tlvs.get(&(0, 0)).expect("unknown TLV preserved");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].id, 9);
        assert_eq!(doc2.chart.judge_line_list[0].notes.as_ref().unwrap()[0].kind, 1);
        // Re-emission carries the unknown segment back into the bytes.
        let out = to_bytes(&doc2).unwrap();
        assert!(out.windows(9).any(|w| w == [9, 4, 0, 0, 0, 1, 2, 3, 4]));
    }

    #[test]
    fn bad_magic_rejected() {
        assert!(from_bytes(b"PMK2").is_err());
        assert!(from_bytes(b"").is_err());
        assert!(from_bytes(b"{\"META\":{}}").is_err());
    }

    #[test]
    fn missing_meta_rejected() {
        let err = from_bytes(&MAGIC).unwrap_err().to_string();
        assert!(err.contains("missing META"), "{err}");
        // A NOTE chunk before META/LINE is rejected too (line out of range).
        let mut w = Writer::new();
        w.chunk(TAG_NOTE, &[0u8; 12]);
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&w.buf);
        assert!(from_bytes(&bytes).is_err());
    }

    #[test]
    fn meta_not_first_rejected() {
        let mut w = Writer::new();
        w.chunk(TAG_BPM, &[0u8; 16]);
        w.chunk(TAG_META, b"version=1\n");
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&w.buf);
        let err = from_bytes(&bytes).unwrap_err().to_string();
        assert!(err.contains("first"), "{err}");
    }

    #[test]
    fn bpm_len_must_be_multiple_of_16() {
        let mut w = Writer::new();
        w.chunk(TAG_META, b"version=1\n");
        w.chunk(TAG_BPM, &[0u8; 8]);
        w.chunk(TAG_LINE, &one_line_payload());
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&w.buf);
        let err = from_bytes(&bytes).unwrap_err().to_string();
        assert!(err.contains("multiple of 16"), "{err}");
    }

    #[test]
    fn line_idx_out_of_range_rejected() {
        let mut w = Writer::new();
        w.chunk(TAG_META, b"version=1\n");
        w.chunk(TAG_LINE, &one_line_payload());
        let mut note = Vec::new();
        note.extend_from_slice(&5u32.to_le_bytes());
        note.extend_from_slice(&0u32.to_le_bytes());
        w.chunk(TAG_NOTE, &note);
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&w.buf);
        let err = from_bytes(&bytes).unwrap_err().to_string();
        assert!(err.contains("out of range"), "{err}");
    }

    #[test]
    fn note_count_overflow_rejected() {
        let mut w = Writer::new();
        w.chunk(TAG_META, b"version=1\n");
        w.chunk(TAG_LINE, &one_line_payload());
        // claims 10 notes but payload holds none
        let mut note = Vec::new();
        note.extend_from_slice(&0u32.to_le_bytes());
        note.extend_from_slice(&10u32.to_le_bytes());
        w.chunk(TAG_NOTE, &note);
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&w.buf);
        assert!(from_bytes(&bytes).is_err());
    }

    #[test]
    fn end_chunk_stops_parsing() {
        let (chart, info) = rich_chart();
        let doc = PmkDoc { chart, info, unknown: PmkUnknown::default() };
        let mut bytes = to_bytes(&doc).unwrap();
        // Append a chunk after END: must be ignored, not an error.
        let mut w = Writer::new();
        w.chunk(*b"TAIL", b"junk");
        bytes.extend_from_slice(&w.buf);
        let doc2 = from_bytes(&bytes).unwrap();
        assert!(doc2.unknown.chunks.is_empty());
    }

    #[test]
    fn visible_time_roundtrip_exact() {
        let mut note = crate::core::model::RPENote::default();
        note.kind = 1;
        note.start_time = Triple::from_beats(1.5);
        note.visible_time = 999999.0;
        let mut buf = Vec::new();
        write_note_record(&mut buf, &note, &[]);
        assert_eq!(buf.len(), 44, "no-TLV note is exactly 44 bytes");
        let mut cur = Cursor::new(&buf, "test");
        let parsed = read_note_record(&mut cur).unwrap();
        assert_eq!(parsed.note.visible_time, 999999.0);
        assert_eq!(parsed.note.start_time.beats(), 1.5);
    }

    #[test]
    fn end_index_referenced_chunks() {
        let (chart, info) = rich_chart();
        let doc = PmkDoc { chart, info, unknown: PmkUnknown::default() };
        let bytes = to_bytes(&doc).unwrap();
        let mut it = crate::core::pmk::frame::ChunkIter::new(&bytes[MAGIC.len()..]);
        let mut last = None;
        while let Some(c) = it.next_chunk().unwrap() {
            last = Some(c.tag);
        }
        assert_eq!(last, Some(TAG_END));
    }
}
