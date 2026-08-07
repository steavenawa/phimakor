//! PMK record encodings — TLV framing, NoteRecord, EventRecord, LINE, CTRL.
//! Spec: `../docs/pmk-spec.md` §4.3–§4.6, §8, §9, §10, §11, §12.

use anyhow::{bail, Result};
use std::collections::HashMap;

use crate::core::bpm::Triple;
use crate::core::model::{RGBColor, RPECtrlEvent, RPEEvent, RPEEventLayer, RPEJudgeLine, RPENote};

pub const NOTE_CORE_SIZE: usize = 44;
pub const EVENT_CORE_SIZE: usize = 52;

/// Byte cursor over a chunk payload; errors carry the context set by the caller.
pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
    ctx: String,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8], ctx: impl Into<String>) -> Self {
        Self { data, pos: 0, ctx: ctx.into() }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_ctx(&mut self, ctx: impl Into<String>) {
        self.ctx = ctx.into();
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    fn need(&self, n: usize) -> Result<()> {
        if self.pos + n > self.data.len() {
            bail!("{}: truncated at 0x{:x} (need {n} bytes)", self.ctx, self.pos);
        }
        Ok(())
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.need(n)?;
        self.pos += n;
        Ok(())
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    pub fn utf8(&mut self, n: usize) -> Result<String> {
        let b = self.bytes(n)?;
        String::from_utf8(b.to_vec())
            .map_err(|e| anyhow::anyhow!("{}: invalid UTF-8 at 0x{:x}: {e}", self.ctx, self.pos))
    }
}

// ── TLV (spec §15.3) ──

#[derive(Clone, Debug, PartialEq)]
pub struct TlvField {
    pub id: u8,
    pub payload: Vec<u8>,
}

pub fn write_tlv(buf: &mut Vec<u8>, id: u8, payload: &[u8]) {
    buf.push(id);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
}

/// Read a TLV chain until the id-0 terminator. `known` returns `true` for
/// recognized ids (and must fully consume the payload); unrecognized ids are
/// collected into `unknown` so they can be re-emitted on save.
pub fn read_tlv(
    cur: &mut Cursor,
    known: &mut impl FnMut(u8, &[u8]) -> Result<bool>,
    unknown: &mut Vec<TlvField>,
) -> Result<()> {
    loop {
        let id = cur.u8()?;
        if id == 0 {
            return Ok(());
        }
        let len = cur.u32()? as usize;
        let payload = cur.bytes(len)?.to_vec();
        if known(id, &payload)? {
            continue;
        }
        unknown.push(TlvField { id, payload });
    }
}

// ── LINE record (spec §8) ──

pub struct ParsedLine {
    pub name: String,
    pub texture: String,
    pub parent: Option<isize>,
    pub rotate_with_father: Option<bool>,
    pub is_cover: u8,
    pub z_order: i32,
    pub attach_ui: Option<String>,
    pub unknown_tlvs: Vec<TlvField>,
}

pub fn write_line_record(buf: &mut Vec<u8>, l: &RPEJudgeLine, extra_tlvs: &[TlvField]) {
    buf.extend_from_slice(&(match l.parent { None => -1, Some(p) => p as i32 }).to_le_bytes());
    buf.extend_from_slice(&l.z_order.to_le_bytes());
    buf.push(l.is_cover);
    buf.push(match l.rotate_with_father { None => 0, Some(false) => 1, Some(true) => 2 });
    buf.push(0);
    buf.push(0);
    buf.extend_from_slice(&(l.name.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(l.texture.len() as u32).to_le_bytes());
    buf.extend_from_slice(l.name.as_bytes());
    buf.extend_from_slice(l.texture.as_bytes());
    if let Some(ui) = &l.attach_ui {
        write_tlv(buf, 1, ui.as_bytes());
    }
    for f in extra_tlvs {
        write_tlv(buf, f.id, &f.payload);
    }
    buf.push(0);
}

pub fn read_line_record(cur: &mut Cursor) -> Result<ParsedLine> {
    let parent = cur.i32()?;
    let z_order = cur.i32()?;
    let is_cover = cur.u8()?;
    let rotate = cur.u8()?;
    cur.skip(2)?;
    let name_len = cur.u32()? as usize;
    let tex_len = cur.u32()? as usize;
    let name = cur.utf8(name_len)?;
    let texture = cur.utf8(tex_len)?;
    let mut attach_ui = None;
    let mut unknown_tlvs = Vec::new();
    let ctx = cur.ctx.clone();
    let mut known = |id: u8, payload: &[u8]| -> Result<bool> {
        if id == 1 {
            attach_ui = Some(String::from_utf8(payload.to_vec())
                .map_err(|e| anyhow::anyhow!("{ctx}: attach_ui: {e}"))?);
            Ok(true)
        } else {
            Ok(false)
        }
    };
    read_tlv(cur, &mut known, &mut unknown_tlvs)?;
    Ok(ParsedLine {
        name,
        texture,
        parent: if parent < 0 { None } else { Some(parent as isize) },
        rotate_with_father: match rotate {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        },
        is_cover,
        z_order,
        attach_ui,
        unknown_tlvs,
    })
}

// ── NOTE record (spec §9) ──

pub struct ParsedNote {
    pub note: RPENote,
    pub unknown_tlvs: Vec<TlvField>,
}

pub fn write_note_record(buf: &mut Vec<u8>, n: &RPENote, extra_tlvs: &[TlvField]) {
    let has_tlv = n.hitsound.is_some()
        || n.tint.is_some()
        || n.tint_hit_effects.is_some()
        || n.judge_area.is_some()
        || !extra_tlvs.is_empty();
    buf.push(n.kind);
    buf.push(n.above);
    buf.push(n.is_fake);
    buf.push(if has_tlv { 1 } else { 0 });
    buf.extend_from_slice(&n.start_time.beats().to_le_bytes());
    buf.extend_from_slice(&n.end_time.beats().to_le_bytes());
    buf.extend_from_slice(&n.position_x.to_le_bytes());
    buf.extend_from_slice(&n.y_offset.to_le_bytes());
    buf.extend_from_slice(&n.size.to_le_bytes());
    buf.extend_from_slice(&n.speed.to_le_bytes());
    buf.extend_from_slice(&n.alpha.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&(n.visible_time as f32).to_le_bytes());
    if has_tlv {
        if let Some(h) = &n.hitsound {
            write_tlv(buf, 1, h.as_bytes());
        }
        if let Some(t) = n.tint {
            write_tlv(buf, 2, &t);
        }
        if let Some(t) = n.tint_hit_effects {
            write_tlv(buf, 3, &t);
        }
        if let Some(j) = n.judge_area {
            write_tlv(buf, 4, &j.to_le_bytes());
        }
        for f in extra_tlvs {
            write_tlv(buf, f.id, &f.payload);
        }
        buf.push(0);
    }
}

pub fn read_note_record(cur: &mut Cursor) -> Result<ParsedNote> {
    let kind = cur.u8()?;
    let above = cur.u8()?;
    let is_fake = cur.u8()?;
    let flags = cur.u8()?;
    let time = cur.f64()?;
    let hold_end = cur.f64()?;
    let position_x = cur.f32()?;
    let y_offset = cur.f32()?;
    let size = cur.f32()?;
    let speed = cur.f32()?;
    let alpha = cur.u16()?;
    cur.skip(2)?;
    let visible_time = cur.f32()? as f64;

    let mut hitsound = None;
    let mut tint = None;
    let mut tint_hit_effects = None;
    let mut judge_area = None;
    let mut unknown_tlvs = Vec::new();
    let ctx = cur.ctx.clone();
    if flags & 1 != 0 {
        let mut known = |id: u8, payload: &[u8]| -> Result<bool> {
            match id {
                1 => {
                    hitsound = Some(String::from_utf8(payload.to_vec())
                        .map_err(|e| anyhow::anyhow!("{ctx}: hitsound: {e}"))?);
                    Ok(true)
                }
                2 => {
                    tint = Some(payload.try_into().map_err(|_| {
                        anyhow::anyhow!("{ctx}: tint must be 3 bytes")
                    })?);
                    Ok(true)
                }
                3 => {
                    tint_hit_effects = Some(payload.try_into().map_err(|_| {
                        anyhow::anyhow!("{ctx}: tint_hit_effects must be 3 bytes")
                    })?);
                    Ok(true)
                }
                4 => {
                    let b: [u8; 4] = payload.try_into().map_err(|_| {
                        anyhow::anyhow!("{ctx}: judge_area must be 4 bytes")
                    })?;
                    judge_area = Some(f32::from_le_bytes(b));
                    Ok(true)
                }
                _ => Ok(false),
            }
        };
        read_tlv(cur, &mut known, &mut unknown_tlvs)?;
    }

    Ok(ParsedNote {
        note: RPENote {
            kind,
            above,
            start_time: Triple::from_beats(time),
            end_time: Triple::from_beats(hold_end),
            position_x,
            y_offset,
            alpha,
            hitsound,
            size,
            speed,
            is_fake,
            visible_time,
            tint,
            tint_hit_effects,
            judge_area,
            comment: None,
        },
        unknown_tlvs,
    })
}

// ── EventRecord core (spec §10, reused by XEVT §11) ──

#[derive(Clone, Debug)]
pub struct EventCore {
    pub kind: u8,
    pub tween: u8,
    pub bezier: u8,
    pub flags: u8,
    pub t1: f64,
    pub t2: f64,
    pub v1: f32,
    pub v2: f32,
    pub easing_l: f32,
    pub easing_r: f32,
    pub bezier_pts: [f32; 4],
}

impl EventCore {
    pub fn write(&self, buf: &mut Vec<u8>) {
        buf.push(self.kind);
        buf.push(self.tween);
        buf.push(self.bezier);
        buf.push(self.flags);
        buf.extend_from_slice(&self.t1.to_le_bytes());
        buf.extend_from_slice(&self.t2.to_le_bytes());
        buf.extend_from_slice(&self.v1.to_le_bytes());
        buf.extend_from_slice(&self.v2.to_le_bytes());
        buf.extend_from_slice(&self.easing_l.to_le_bytes());
        buf.extend_from_slice(&self.easing_r.to_le_bytes());
        for p in self.bezier_pts {
            buf.extend_from_slice(&p.to_le_bytes());
        }
    }

    pub fn read(cur: &mut Cursor) -> Result<Self> {
        Ok(Self {
            kind: cur.u8()?,
            tween: cur.u8()?,
            bezier: cur.u8()?,
            flags: cur.u8()?,
            t1: cur.f64()?,
            t2: cur.f64()?,
            v1: cur.f32()?,
            v2: cur.f32()?,
            easing_l: cur.f32()?,
            easing_r: cur.f32()?,
            bezier_pts: [cur.f32()?, cur.f32()?, cur.f32()?, cur.f32()?],
        })
    }
}

pub fn event_core_to_rpe(core: &EventCore) -> RPEEvent {
    RPEEvent {
        start_time: Triple::from_beats(core.t1),
        end_time: Triple::from_beats(core.t2),
        start: core.v1,
        end: core.v2,
        easing_type: core.tween as i32,
        easing_left: core.easing_l,
        easing_right: core.easing_r,
        bezier: core.bezier,
        bezier_points: core.bezier_pts,
    }
}

fn event_core_from_rpe(ev: &RPEEvent, kind: u8, v1: f32, v2: f32) -> EventCore {
    EventCore {
        kind,
        tween: ev.easing_type as u8,
        bezier: ev.bezier,
        flags: 0,
        t1: ev.start_time.beats(),
        t2: ev.end_time.beats(),
        v1,
        v2,
        easing_l: ev.easing_left,
        easing_r: ev.easing_right,
        bezier_pts: ev.bezier_points,
    }
}

pub fn event_core_from_rpe_scalar(ev: &RPEEvent, kind: u8) -> EventCore {
    event_core_from_rpe(ev, kind, ev.start, ev.end)
}

/// Split a merged per-layer event stream back into the five RPE lists.
pub fn split_event_layers(
    events: Vec<EventCore>,
) -> (Vec<RPEEvent>, Vec<RPEEvent>, Vec<RPEEvent>, Vec<RPEEvent>, Vec<RPEEvent>) {
    let mut alpha = Vec::new();
    let mut move_x = Vec::new();
    let mut move_y = Vec::new();
    let mut rotate = Vec::new();
    let mut speed = Vec::new();
    for core in events {
        let list = match core.kind {
            0 => &mut alpha,
            1 => &mut move_x,
            2 => &mut move_y,
            3 => &mut rotate,
            _ => &mut speed,
        };
        list.push(event_core_to_rpe(&core));
    }
    (alpha, move_x, move_y, rotate, speed)
}

/// Layer 0..=4 kind tags for serialization.
pub fn rpe_layer_kinds(layer: &RPEEventLayer) -> [(u8, &Vec<RPEEvent>); 5] {
    [
        (0, layer.alpha_events.as_ref().unwrap_or(&EMPTY_EVENTS)),
        (1, layer.move_x_events.as_ref().unwrap_or(&EMPTY_EVENTS)),
        (2, layer.move_y_events.as_ref().unwrap_or(&EMPTY_EVENTS)),
        (3, layer.rotate_events.as_ref().unwrap_or(&EMPTY_EVENTS)),
        (4, layer.speed_events.as_ref().unwrap_or(&EMPTY_EVENTS)),
    ]
}

static EMPTY_EVENTS: Vec<RPEEvent> = Vec::new();

// ── XEVT value TLV (spec §11) ──

/// An encoded XEVT record: 52-byte core plus an optional pre-encoded TLV chain
/// (including the id-0 terminator, present when `flags.bit0` is set).
pub struct XevtEnc {
    pub core: EventCore,
    pub tlv: Option<Vec<u8>>,
}

pub fn xevt_color(ev: &RPEEvent<RGBColor>) -> XevtEnc {
    let core = EventCore {
        kind: 1,
        tween: ev.easing_type as u8,
        bezier: ev.bezier,
        flags: 1,
        t1: ev.start_time.beats(),
        t2: ev.end_time.beats(),
        v1: 0.0,
        v2: 0.0,
        easing_l: ev.easing_left,
        easing_r: ev.easing_right,
        bezier_pts: ev.bezier_points,
    };
    let mut tlv = Vec::new();
    write_tlv(&mut tlv, 1, &[ev.start.0, ev.start.1, ev.start.2, ev.end.0, ev.end.1, ev.end.2]);
    tlv.push(0);
    XevtEnc { core, tlv: Some(tlv) }
}

pub fn xevt_text(ev: &RPEEvent<String>) -> XevtEnc {
    let core = EventCore {
        kind: 2,
        tween: ev.easing_type as u8,
        bezier: ev.bezier,
        flags: 1,
        t1: ev.start_time.beats(),
        t2: ev.end_time.beats(),
        v1: 0.0,
        v2: 0.0,
        easing_l: ev.easing_left,
        easing_r: ev.easing_right,
        bezier_pts: ev.bezier_points,
    };
    let mut pl = Vec::new();
    pl.extend_from_slice(&(ev.start.len() as u32).to_le_bytes());
    pl.extend_from_slice(ev.start.as_bytes());
    pl.extend_from_slice(&(ev.end.len() as u32).to_le_bytes());
    pl.extend_from_slice(ev.end.as_bytes());
    let mut tlv = Vec::new();
    write_tlv(&mut tlv, 2, &pl);
    tlv.push(0);
    XevtEnc { core, tlv: Some(tlv) }
}

pub enum XevtValue {
    Color(RPEEvent<RGBColor>),
    Text(RPEEvent<String>),
    Scalar(u8, RPEEvent),
}

pub struct XevtOut {
    pub value: Option<XevtValue>,
    /// The record could not be fully decoded (unknown kind / unknown value
    /// TLV); the caller keeps its raw bytes for re-emission.
    pub opaque: bool,
}

pub fn read_xevt_record(cur: &mut Cursor) -> Result<XevtOut> {
    let core = EventCore::read(cur)?;
    match core.kind {
        1 | 2 => {
            let mut color = None;
            let mut text = None;
            let mut junk = Vec::new();
            let ctx = cur.ctx.clone();
            if core.flags & 1 != 0 {
                let mut known = |id: u8, payload: &[u8]| -> Result<bool> {
                    match id {
                        1 => {
                            let b: [u8; 6] = payload.try_into().map_err(|_| {
                                anyhow::anyhow!("{ctx}: color TLV must be 6 bytes")
                            })?;
                            color = Some((b[0], b[1], b[2], b[3], b[4], b[5]));
                            Ok(true)
                        }
                        2 => {
                            let mut c = Cursor::new(payload, "text TLV");
                            let l1 = c.u32()? as usize;
                            let s1 = c.utf8(l1)?;
                            let l2 = c.u32()? as usize;
                            let s2 = c.utf8(l2)?;
                            text = Some((s1, s2));
                            Ok(true)
                        }
                        _ => Ok(false),
                    }
                };
                read_tlv(cur, &mut known, &mut junk)?;
            }
            if !junk.is_empty() {
                return Ok(XevtOut { value: None, opaque: true });
            }
            let value = if core.kind == 1 {
                let (r1, g1, b1, r2, g2, b2) = color.ok_or_else(|| {
                    anyhow::anyhow!("{ctx}: color event missing color TLV")
                })?;
                XevtValue::Color(RPEEvent {
                    easing_type: core.tween as i32,
                    easing_left: core.easing_l,
                    easing_right: core.easing_r,
                    bezier: core.bezier,
                    bezier_points: core.bezier_pts,
                    start: RGBColor(r1, g1, b1),
                    end: RGBColor(r2, g2, b2),
                    start_time: Triple::from_beats(core.t1),
                    end_time: Triple::from_beats(core.t2),
                })
            } else {
                let (s1, s2) = text.ok_or_else(|| {
                    anyhow::anyhow!("{ctx}: text event missing text TLV")
                })?;
                XevtValue::Text(RPEEvent {
                    easing_type: core.tween as i32,
                    easing_left: core.easing_l,
                    easing_right: core.easing_r,
                    bezier: core.bezier,
                    bezier_points: core.bezier_pts,
                    start: s1,
                    end: s2,
                    start_time: Triple::from_beats(core.t1),
                    end_time: Triple::from_beats(core.t2),
                })
            };
            Ok(XevtOut { value: Some(value), opaque: false })
        }
        3..=7 => Ok(XevtOut { value: Some(XevtValue::Scalar(core.kind, event_core_to_rpe(&core))), opaque: false }),
        _ => {
            if core.flags & 1 != 0 {
                let mut junk = Vec::new();
                read_tlv(cur, &mut |_, _| Ok(false), &mut junk)?;
            }
            Ok(XevtOut { value: None, opaque: true })
        }
    }
}

// ── CTRL record (spec §12) ──

pub fn write_ctrl_record(buf: &mut Vec<u8>, c: &RPECtrlEvent) {
    buf.push(c.easing);
    buf.push(0);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&c.x.to_le_bytes());
    let mut kv: Vec<(&String, &f32)> = c.value.iter().collect();
    kv.sort_by(|a, b| a.0.cmp(b.0));
    buf.extend_from_slice(&(kv.len() as u16).to_le_bytes());
    for (k, v) in kv {
        buf.extend_from_slice(&(k.len() as u16).to_le_bytes());
        buf.extend_from_slice(k.as_bytes());
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

pub fn read_ctrl_record(cur: &mut Cursor) -> Result<RPECtrlEvent> {
    let easing = cur.u8()?;
    cur.skip(3)?;
    let x = cur.f64()?;
    let n = cur.u16()? as usize;
    let mut value = HashMap::new();
    for _ in 0..n {
        let kl = cur.u16()? as usize;
        let k = cur.utf8(kl)?;
        let v = cur.f32()?;
        value.insert(k, v);
    }
    Ok(RPECtrlEvent { easing, x, value })
}
