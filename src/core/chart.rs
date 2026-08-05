#![allow(dead_code)] // 库 API: Python 绑定/embedding/备用接口,主程序未全部使用

// Derived from TeamFlos/phira prpr, GPL-3.0.
//! RPE chart loading and per-frame state evaluation.
//! Lowering ported from `prpr/src/parse/rpe.rs` (`parse_rpe`); note/line
//! render math ported from `prpr/src/core/note.rs` and `core/line.rs`.

use anyhow::{Context, Result};
use std::{collections::HashMap, rc::Rc};

use super::{
    anim::{Anim, AnimFloat, Keyframe},
    bpm::{BpmList, Triple},
    easing::{
        speed_linear_tween, speed_segment_tween, BezierTween, ClampedTween, RPE_TWEEN_MAP, SpeedEasingMode, StaticTween, TweenFunction, Tweenable,
    },
    model::{parse_info_txt, ChartInfo, InfoYaml, RPEChart, RPEEvent, RPEEventLayer, RPEJudgeLine, RPENote},
    Color, EPS, RPE_HEIGHT, RPE_WIDTH, SPEED_RATIO,
};

// ---------------------------------------------------------------------------
// Frame state (renderer contract)
// ---------------------------------------------------------------------------

/// One frame of evaluated chart state, produced by [`Chart::state_at`].
pub struct FrameState {
    /// Chart clock time in seconds for this frame.
    pub time: f64,
    /// Per-line evaluated state in `judgeLineList` order.
    pub lines: Vec<LineState>,
    /// Notes whose hit time was crossed between the previous and this
    /// `state_at` call (for hitsounds/FX). Independent of culling: notes no
    /// longer present in `lines` are still reported.
    pub fired: Vec<FiredNote>,
}

/// A note whose hit time was crossed since the last [`Chart::state_at`] call.
pub struct FiredNote {
    /// Index into `judgeLineList`.
    pub line: usize,
    /// 1 tap, 2 hold, 3 flick, 4 drag.
    pub kind: u8,
    /// Note x in internal units (`position_x / 675`).
    pub x: f32,
    /// `true` if this note is a fake (no score contribution).
    pub fake: bool,
    /// `true` for hold sustain-beat repeats; `false` for the initial hit
    /// (all kinds, including a hold's first hit).
    pub tick: bool,
    /// `true` for a hold's release event (kind 2, `tick == false`), fired
    /// when `end_time` is crossed. Distinct from the initial hit so the
    /// consumer can count the hold tail toward combo.
    pub hold_tail: bool,
}

/// Per-line evaluated state for one frame.
/// See also [`Chart::state_at`].
pub struct LineState {
    /// Line opacity in [0, 1].
    pub alpha: f32,
    /// Radians, ready for `Rotation2`.
    pub rotation: f32,
    /// Internal coords (center origin, x ±1, y ±0.83175).
    pub position: [f32; 2],
    /// Scale multiplier `[x, y]` in internal units.
    pub scale: [f32; 2],
    /// RGBA color in `[0, 1]` range.
    pub color: [f32; 4],
    /// `None` = built-in `line.png`; `Some` = file in chart dir.
    pub texture: Option<String>,
    /// Draw ordering (higher = drawn on top).
    pub z_order: i32,
    /// Visible notes for this frame, in draw order.
    pub notes: Vec<NoteState>,
    /// `[E]` CtrlObject: pos / size / alpha / y control values for this frame.
    pub ctrl_pos_x: f32,
    pub ctrl_pos_y: f32,
    pub ctrl_size_x: f32,
    pub ctrl_size_y: f32,
    pub ctrl_alpha: f32,
    pub ctrl_y: f32,
    /// `father` index into `judgeLineList`.
    pub parent: Option<usize>,
    /// `rotateWithFather`: child rotation inherits the parent's rotation.
    pub rot_with_parent: bool,
    /// PE alpha extension: `floor(-alpha) == 1` hides line + notes.
    pub pe_hide: bool,
    /// PE alpha extension: `floor(-alpha) == 2` hides only below notes.
    pub draw_below: bool,
    /// PE alpha extension: notes appear this many seconds before their hit
    /// time (`w = floor(-alpha)` in 100..1000 → `(w-100)/10`).
    pub appear_before: f64,
    /// Sine of the incline angle (perspective X distortion).
    pub incline_sin: f32,
    /// attachUI lines are not rendered as judge lines (phira removes them
    /// from the render order); they only position game UI elements.
    pub attach_ui: Option<String>,
}

/// Evaluated per-note state for one frame.
/// See also [`LineState`].
pub struct NoteState {
    /// 1 tap, 2 hold, 3 flick, 4 drag.
    pub kind: u8,
    /// Hit time in seconds (chart clock).
    pub time: f64,
    /// `[x, y]` relative to the line anchor, internal units.
    /// `y = (note_height - line_height_at_now) * speed`, negated for
    /// `above == false` notes (prpr mirrors below notes with a `(1, -1)`
    /// scale in `core/line.rs` render).
    pub relative: [f32; 2],
    /// Hold body end in the same relative space as `relative[1]`.
    pub hold_end_y: Option<f64>,
    /// Opacity in [0, 1].
    pub alpha: f32,
    /// Note size multiplier.
    pub scale: f32,
    /// `true` = above the line (normal), `false` = below (mirrored).
    pub above: bool,
    /// `true` if this is a fake note (no score, no hitsound).
    pub fake: bool,
    /// Multiple-hint: another note (any line, fakes included) shares the
    /// exact same hit time. Port of prpr `process_lines` (parse.rs).
    pub multiple_hint: bool,
}

// ---------------------------------------------------------------------------
// Internal per-line data
// ---------------------------------------------------------------------------

struct NoteData {
    kind: u8,
    time: f64,
    end_time: f64,
    height: f64,
    end_height: f64,
    /// `position_x / 675`.
    x: f32,
    /// `y_offset * 2/900 * speed`.
    y_offset: f32,
    speed: f64,
    scale: f32,
    alpha: AnimFloat,
    above: bool,
    fake: bool,
    multiple_hint: bool,
}

impl NoteData {
    /// Port of `Note::plain`: not fake, not hold, no y translation animation
    /// (ours is always fixed).
    fn plain(&self) -> bool {
        !self.fake && self.kind != 2
    }
}

struct LineData {
    name: String,
    alpha: AnimFloat,
    /// Raw (unscaled) alpha track, for the PE alpha extension `w` decode.
    pe_alpha: AnimFloat,
    /// Degrees (converted to radians in `state_at`).
    rotation: AnimFloat,
    move_x: AnimFloat,
    move_y: AnimFloat,
    scale_x: AnimFloat,
    scale_y: AnimFloat,
    color: Anim<Color>,
    /// ∫ speed dt — the "height" of the line over time.
    height: AnimFloat,
    notes: Vec<NoteData>,
    /// Indices into `notes` sorted by `max(time, end_time)` — the moment a
    /// note is both invisible (过点即消) and unable to fire. Lets `state_at`
    /// binary-search its per-frame start instead of scanning every note.
    /// `notes` itself stays in draw order (see `parse_notes`), so scans over
    /// `order` re-sort the small visible/fired subsets by index afterwards.
    order: Vec<usize>,
    /// Cursor into `order`: first note that can still be visible. Advanced
    /// monotonically; reset on backward seek. Unused for `show_below` lines
    /// (their notes never vanish by time).
    cursor: usize,
    texture: Option<String>,
    z_order: i32,
    parent: Option<usize>,
    /// `rotateWithFather`: child rotation inherits the parent's rotation.
    rot_with_parent: bool,
    /// prpr `show_below` (`isCover != 1`): keep drawing notes past their time.
    show_below: bool,
    /// attachUI binding (e.g. "pause", "score"); such lines are not rendered.
    attach_ui: Option<String>,
    /// [E] CtrlObject animatable values (parsed from pos/size/alpha/y control).
    ctrl_pos_x: AnimFloat,
    ctrl_pos_y: AnimFloat,
    ctrl_size_x: AnimFloat,
    ctrl_size_y: AnimFloat,
    ctrl_alpha: AnimFloat,
    ctrl_y: AnimFloat,
    incline: AnimFloat,
}

// ---------------------------------------------------------------------------
// Event lowering (ported from prpr/src/parse/rpe.rs)
// ---------------------------------------------------------------------------

type BezierMap = HashMap<(u16, i16, i16), Rc<dyn TweenFunction>>;

impl<T> RPEEvent<T> {
    /// `bezierPoints` quantized to 0.01 for the cache key.
    fn bezier_key(&self) -> (u16, i16, i16) {
        let p = &self.bezier_points;
        let int = |p: f32| (p * 100.).round() as i16;
        ((int(p[0]) * 100 + int(p[1])) as u16, int(p[2]), int(p[3]))
    }

    /// Resolve the tween function for this RPE event.
    ///
    /// Caches bezier curves in `bezier_map`; clamps easing left/right values.
    pub fn tween(&self, bezier_map: &BezierMap) -> Rc<dyn TweenFunction> {
        // easingType < 1 counts as 1; >= 30 falls back to linear.
        let tween = RPE_TWEEN_MAP.get(self.easing_type.max(1) as usize).copied().unwrap_or(RPE_TWEEN_MAP[0]);
        let left = self.easing_left.clamp(0., 1.);
        let right = self.easing_right.clamp(0., 1.);
        if self.bezier != 0 {
            let key = self.bezier_key();
            match bezier_map.get(&key) {
                Some(f) => Rc::clone(f),
                // The map can miss events (e.g. speed events in older parser
                // paths); build the curve on the fly instead of panicking.
                None => Rc::new(BezierTween::new(
                    (self.bezier_points[0], self.bezier_points[1]),
                    (self.bezier_points[2], self.bezier_points[3]),
                )),
            }
        } else if tween <= 2 || (left.abs() < EPS as f32 && (right - 1.0).abs() < EPS as f32) || left >= right {
            StaticTween::get_rc(tween)
        } else {
            Rc::new(ClampedTween::new(tween, left..right))
        }
    }
}

fn parse_events<T: Tweenable, V: Clone + Into<T>>(
    r: &mut BpmList,
    rpe: &[RPEEvent<V>],
    default: Option<T>,
    bezier_map: &BezierMap,
) -> Result<Anim<T>> {
    if rpe.is_empty() {
        return Ok(Anim::default());
    }
    let mut kfs = Vec::new();
    if let Some(default) = default {
        if rpe[0].start_time.beats() != 0.0 {
            kfs.push(Keyframe::new(0.0, default, 0));
        }
    }
    for e in rpe {
        kfs.push(Keyframe {
            time: r.time(&e.start_time),
            value: e.start.clone().into(),
            tween: e.tween(bezier_map),
        });
        kfs.push(Keyframe::new(r.time(&e.end_time), e.end.clone().into(), 0));
    }
    Ok(Anim::new(kfs))
}

fn parse_speed_events(r: &mut BpmList, rpe: &[RPEEventLayer], bezier_map: &BezierMap, max_time: f64, mode: SpeedEasingMode) -> Result<AnimFloat> {
    let layers: Vec<_> = rpe.iter().filter_map(|it| it.speed_events.as_ref()).collect();
    if layers.is_empty() {
        return Ok(AnimFloat::default());
    }
    let mut anis = Vec::new();
    for layer in layers {
        if layer.is_empty() {
            continue;
        }
        let mut events = layer.iter().collect::<Vec<_>>();
        events.sort_by(|a, b| a.start_time.beats().total_cmp(&b.start_time.beats()));

        let mut kfs = vec![Keyframe::new(0.0, 0.0, 2)];
        let mut height = 0f64;
        // Segments with duration <= EPS are dropped here.
        let mut push_kf = |start_time: f64, end_time: f64, tween: Rc<dyn TweenFunction>, factor: f32| {
            if end_time - start_time <= EPS {
                return;
            }
            if let Some(last) = kfs.last_mut() {
                if (last.time - start_time).abs() < EPS {
                    last.value = height as f32;
                    last.tween = tween;
                } else {
                    kfs.push(Keyframe {
                        time: start_time,
                        value: height as f32,
                        tween,
                    });
                }
            }
            height += factor as f64 * (end_time - start_time);
        };

        let mut cursor = 0.0;
        let mut last_speed = 0.0;
        for event in events {
            let start_time = r.time(&event.start_time).max(cursor);
            let end_time = r.time(&event.end_time).max(start_time);
            let start_speed = event.start * SPEED_RATIO as f32;
            let end_speed = event.end * SPEED_RATIO as f32;

            push_kf(cursor, start_time, StaticTween::get_rc(2), last_speed);
            if end_time > start_time + EPS {
                if event.easing_type == 0 {
                    push_kf(start_time, end_time, StaticTween::get_rc(2), start_speed);
                } else if event.easing_type <= 1 {
                    if start_speed * end_speed < 0. {
                        // zero crossing: split into two segments at speed = 0
                        let x = start_speed / (start_speed - end_speed);
                        let mid = f64::tween(&start_time, &end_time, x);
                        for (start_time, end_time, start, end) in [(start_time, mid, start_speed, 0.), (mid, end_time, 0., end_speed)] {
                            let factor = start.midpoint(end);
                            let tween = speed_linear_tween(start, end);
                            push_kf(start_time, end_time, tween, factor);
                        }
                    } else {
                        let factor = start_speed.midpoint(end_speed);
                        let tween = speed_linear_tween(start_speed, end_speed);
                        push_kf(start_time, end_time, tween, factor);
                    }
                } else {
                    let (tween, factor) = speed_segment_tween(mode, start_speed, end_speed, event.tween(bezier_map));
                    push_kf(start_time, end_time, tween, factor);
                }
            }
            cursor = end_time;
            last_speed = end_speed;
        }

        push_kf(cursor, max_time, StaticTween::get_rc(2), last_speed);
        if let Some(last) = kfs.last() {
            if (last.time - max_time).abs() > EPS {
                kfs.push(Keyframe::new(max_time, height as f32, 0));
            }
        }
        anis.push(AnimFloat::new(kfs));
    }
    if anis.is_empty() {
        return Ok(AnimFloat::default());
    }
    Ok(AnimFloat::chain(anis))
}

fn parse_speed_events_legacy(r: &mut BpmList, rpe: &[RPEEventLayer], max_time: f64) -> Result<AnimFloat> {
    let rpe: Vec<_> = rpe.iter().filter_map(|it| it.speed_events.as_ref()).collect();
    if rpe.is_empty() {
        return Ok(AnimFloat::default());
    };
    let anis: Vec<_> = rpe
        .into_iter()
        .filter_map(|it| {
            if it.is_empty() {
                return None;
            }
            let mut kfs = Vec::new();
            for e in it {
                kfs.push(Keyframe::new(r.time(&e.start_time), e.start, 2));
                kfs.push(Keyframe::new(r.time(&e.end_time), e.end, 0));
            }
            Some(AnimFloat::new(kfs))
        })
        .collect();
    if anis.is_empty() {
        return Ok(AnimFloat::default());
    }
    let mut pts: Vec<f64> = anis.iter().flat_map(|it| it.keyframes.iter().map(|it| it.time)).collect();
    pts.push(max_time);
    pts.sort_by(f64::total_cmp);
    pts.dedup();
    let mut sani = AnimFloat::chain(anis);
    sani.map_value(|v| v * SPEED_RATIO as f32);
    let init_len = pts.len();
    for i in 0..(init_len - 1) {
        let now_time = pts[i];
        let end_time = pts[i + 1];
        sani.set_time(now_time);
        let speed = sani.now();
        sani.set_time(end_time - 1e-4);
        let end_speed = sani.now();
        if speed.signum() * end_speed.signum() < 0. {
            // zero crossing inside the segment: split there
            pts.push(f64::tween(&now_time, &end_time, speed / (speed - end_speed)));
        }
    }
    pts.sort_by(f64::total_cmp);
    pts.dedup();
    let mut kfs = Vec::new();
    let mut height = 0f64;
    for i in 0..(pts.len() - 1) {
        let now_time = pts[i];
        let end_time = pts[i + 1];
        sani.set_time(now_time);
        let speed = sani.now();
        // this can affect a lot! do not use end_time...
        // using end_time causes Hold tween (x |-> 0) to be recognized as Linear tween (x |-> x)
        sani.set_time(end_time - 1e-4);
        let end_speed = sani.now();
        kfs.push(if (speed - end_speed).abs() < EPS as f32 {
            Keyframe::new(now_time, height as f32, 2)
        } else if speed.abs() > end_speed.abs() {
            Keyframe {
                time: now_time,
                value: height as f32,
                tween: Rc::new(ClampedTween::new(7 /*quadOut*/, 0.0..(1. - end_speed / speed))),
            }
        } else {
            Keyframe {
                time: now_time,
                value: height as f32,
                tween: Rc::new(ClampedTween::new(6 /*quadIn*/, (speed / end_speed)..1.)),
            }
        });
        height += (speed + end_speed) as f64 * (end_time - now_time) / 2.;
    }
    if kfs.is_empty() {
        return Ok(Anim::default());
    }
    kfs.push(Keyframe::new(max_time, height as f32, 0));
    Ok(AnimFloat::new(kfs))
}

/// Note lowering. prpr interleaves hitsound loading here; M0 has no
/// hitsounds, so this is pure (// ponytail: M3 hitsounds).
fn parse_notes(r: &mut BpmList, rpe: Vec<RPENote>, height: &mut AnimFloat) -> Result<Vec<NoteData>> {
    let mut notes = Vec::new();
    for note in rpe {
        let time = r.time(&note.start_time);
        height.set_time(time);
        let note_height = height.now();
        // phira: y_offset = raw y_offset × 2/RPE_HEIGHT × note.speed
        // (parse_rpe — the speed factor is part of the offset).
        let y_offset = note.y_offset * 2. / RPE_HEIGHT * note.speed;
        let (kind, end_time, end_height) = match note.kind {
            1 => (1, 0., 0.),
            2 => {
                let end_time = r.time(&note.end_time);
                height.set_time(end_time);
                (2, end_time, height.now() as f64)
            }
            3 => (3, 0., 0.),
            4 => (4, 0., 0.),
            other => anyhow::bail!("unknown-note-type: type {other}"),
        };
        // [visible_time] RPE documents visibleTime in ms, but real charts use
        // sub-second values (e.g. Sanctuary 0.59) that phira treats as seconds
        // (`note.visible_time >= time` with time in seconds). Match phira:
        // note is invisible until `time - visible_time`, then appears at full
        // alpha (TweenId 0 = step; matches phira's keyframe layout).
        let vt = note.visible_time;
        let alpha = if vt >= time {
            if note.alpha >= 255 {
                AnimFloat::default()
            } else {
                AnimFloat::fixed(note.alpha as f32 / 255.)
            }
        } else {
            let alpha = note.alpha.min(255) as f32 / 255.;
            AnimFloat::new(vec![Keyframe::new(0.0, 0.0, 0), Keyframe::new(time - vt, alpha, 0)])
        };
        notes.push(NoteData {
            kind,
            time,
            end_time,
            height: note_height as f64,
            end_height,
            x: note.position_x / (RPE_WIDTH / 2.),
            y_offset,
            speed: note.speed as f64,
            scale: note.size,
            alpha,
            above: note.above == 1,
            fake: note.is_fake != 0,
            multiple_hint: false,
        });
    }
    // Draw order, port of `JudgeLineCache::new`'s sort:
    // (plain, !above, speed, (height + y_offset) * speed).
    notes.sort_by(|a, b| {
        (a.plain(), !a.above)
            .cmp(&(b.plain(), !b.above))
            .then_with(|| a.speed.total_cmp(&b.speed))
            .then_with(|| {
                ((a.height + a.y_offset as f64) * a.speed).total_cmp(&((b.height + b.y_offset as f64) * b.speed))
            })
    });
    Ok(notes)
}

fn parse_ctrl_events(r: &mut BpmList, events: &[super::model::RPECtrlEvent], key: &str) -> AnimFloat {
    use super::easing::{RPE_TWEEN_MAP, StaticTween, TweenFunction};
    use std::rc::Rc;
    let vals: Vec<f32> = events.iter().map(|it| it.value[key]).collect();
    // phira: 2 default events (easing=1, value≈1.0) → identity (no control).
    if events.is_empty() || (events.len() == 2 && events[0].easing == 1 && (vals[0] - 1.0).abs() < 1e-4) {
        return AnimFloat::default();
    }
    // Shift tween: kf[i] gets the tween of event[i+1].
    let tweens: Vec<Rc<dyn TweenFunction>> = events
        .iter()
        .skip(1)
        .map(|it| {
            let idx = (it.easing as usize).max(1).min(RPE_TWEEN_MAP.len() - 1);
            StaticTween::get_rc(RPE_TWEEN_MAP[idx])
        })
        .chain(std::iter::once(StaticTween::get_rc(0)))
        .collect();
    // CtrlObject `x` is in BEATS (phira convention); `state_at` queries in
    // seconds, so convert here — mixing units made ctrl events evaluate on
    // the wrong timeline entirely.
    AnimFloat::new(
        events.iter().zip(vals).zip(tweens).map(|((it, val), tween)| Keyframe {
            time: r.time(&Triple::from_beats(it.x)),
            value: val,
            tween,
        }).collect(),
    )
}

fn parse_judge_line(
    r: &mut BpmList,
    rpe: RPEJudgeLine,
    max_time: f64,
    speed_mode: SpeedEasingMode,
    use_rpe_170_speed: bool,
    bezier_map: &BezierMap,
) -> Result<LineData> {
    // null event layers are skipped
    let event_layers: Vec<_> = rpe.event_layers.into_iter().flatten().collect();
    let pos_control = rpe.pos_control.clone();
    let size_control = rpe.size_control.clone();
    let alpha_control = rpe.alpha_control.clone();
    let y_control = rpe.y_control.clone();
    fn events_with_factor(
        r: &mut BpmList,
        event_layers: &[RPEEventLayer],
        get: impl Fn(&RPEEventLayer) -> &Option<Vec<RPEEvent>>,
        factor: f32,
        bezier_map: &BezierMap,
    ) -> Result<AnimFloat> {
        let anis: Vec<_> = event_layers
            .iter()
            .filter_map(|it| get(it).as_ref().map(|es| parse_events(r, es, None, bezier_map)))
            .collect::<Result<_>>()?;
        let mut res = AnimFloat::chain(anis);
        if res.is_default() {
            return Ok(AnimFloat::default());
        }
        res.map_value(|v| v * factor);
        Ok(res)
    }
    let mut height = if use_rpe_170_speed {
        parse_speed_events(r, &event_layers, bezier_map, max_time, speed_mode)?
    } else {
        parse_speed_events_legacy(r, &event_layers, max_time)?
    };
    let notes = parse_notes(r, rpe.notes.unwrap_or_default(), &mut height)?;
    let mut order: Vec<usize> = (0..notes.len()).collect();
    order.sort_by(|&a, &b| {
        let (na, nb) = (&notes[a], &notes[b]);
        na.time.max(na.end_time).total_cmp(&nb.time.max(nb.end_time))
    });

    // Scaled alpha (0..1 for rendering) AND the raw alpha track (unscaled
    // 0..255-style values) — the PE alpha extension's `w = floor(-alpha)`
    // is defined on the RAW value (phira line.rs:365), not the 1/255-scaled
    // one we render with.
    let alpha = events_with_factor(r, &event_layers, |it| &it.alpha_events, 1. / 255., bezier_map)?;
    let pe_alpha = events_with_factor(r, &event_layers, |it| &it.alpha_events, 1., bezier_map)?;
    let rotation = events_with_factor(r, &event_layers, |it| &it.rotate_events, -1., bezier_map)?;
    let move_x = events_with_factor(r, &event_layers, |it| &it.move_x_events, 2. / RPE_WIDTH, bezier_map)?;
    let move_y = events_with_factor(r, &event_layers, |it| &it.move_y_events, 2. / RPE_HEIGHT, bezier_map)?;

    let (scale_x, scale_y) = {
        fn parse(r: &mut BpmList, opt: &Option<Vec<RPEEvent>>, factor: f32, bezier_map: &BezierMap) -> Result<AnimFloat> {
            let mut res = opt
                .as_ref()
                .map(|it| parse_events(r, it, None, bezier_map))
                .transpose()?
                .unwrap_or_default();
            res.map_value(|v| v * factor);
            Ok(res)
        }
        let factor = if rpe.texture == "line.png" { 1. } else { 2. / RPE_WIDTH };
        let factor_x = factor
            * if rpe.texture == "line.png"
                && rpe
                    .extended
                    .as_ref()
                    .and_then(|it| it.text_events.as_ref())
                    .is_none_or(|it| it.is_empty())
                && rpe.attach_ui.is_none()
            {
                0.5
            } else {
                1.
            };
        let (scale_x, scale_y) = match rpe.extended.as_ref() {
            Some(e) => (
                parse(r, &e.scale_x_events, factor_x, bezier_map)?,
                parse(r, &e.scale_y_events, factor, bezier_map)?,
            ),
            None => (AnimFloat::default(), AnimFloat::default()),
        };
        (scale_x, scale_y)
    };

    let color: Anim<Color> = if let Some(events) = rpe.extended.as_ref().and_then(|e| e.color_events.as_ref()) {
        parse_events(r, events, Some(Color::WHITE), bezier_map)?
    } else {
        Anim::default()
    };
    let incline = match rpe.extended.as_ref().and_then(|e| e.incline_events.as_ref()) {
        Some(events) => parse_events(r, events, Some(0.), bezier_map)?,
        None => AnimFloat::default(),
    };

    // ponytail: M3 — incline/text/paint/gif events, attachUI, ctrl events
    // (posControl etc.) and rotateWithFather are parsed but not lowered.
    let parent = {
        let parent = rpe.parent.unwrap_or(-1);
        match parent {
            -1 => None,
            p if p < 0 => anyhow::bail!("invalid father index {p} (must be -1 or >= 0)"),
            p => Some(p as usize),
        }
    };

    Ok(LineData {
        name: rpe.name,
        alpha,
        pe_alpha,
        rotation,
        move_x,
        move_y,
        scale_x,
        scale_y,
        color,
        height,
        notes,
        order,
        cursor: 0,
        texture: if rpe.texture == "line.png" { None } else { Some(rpe.texture.clone()) },
        z_order: rpe.z_order,
        parent,
        rot_with_parent: rpe.rotate_with_father.unwrap_or(false),
        show_below: rpe.is_cover != 1,
        attach_ui: rpe.attach_ui.clone(),
        // [E] CtrlObject: parse pos/size/alpha/y control events.
        // posControl uses key "pos" (single f32, factor 2/RPE_WIDTH for x, 2/RPE_HEIGHT for y).
        // sizeControl uses key "size" (single f32, factor 1.0).
        // alphaControl uses key "alpha" (0-1 range, factor 1.0).
        // yControl uses key "y" (factor 1.0).
        // phira: CtrlObject uses raw values (no factor), x is in beats.
        ctrl_pos_x: parse_ctrl_events(r, &pos_control, "pos"),
        ctrl_pos_y: parse_ctrl_events(r, &pos_control, "pos"),
        ctrl_size_x: parse_ctrl_events(r, &size_control, "size"),
        ctrl_size_y: parse_ctrl_events(r, &size_control, "size"),
        ctrl_alpha: parse_ctrl_events(r, &alpha_control, "alpha"),
        ctrl_y: parse_ctrl_events(r, &y_control, "y"),
        incline,
    })
}

fn add_bezier<T>(map: &mut BezierMap, event: &RPEEvent<T>) {
    if event.bezier != 0 {
        let p = &event.bezier_points;
        map.entry(event.bezier_key())
            .or_insert_with(|| Rc::new(BezierTween::new((p[0], p[1]), (p[2], p[3]))));
    }
}

fn add_bezier_events<T>(map: &mut BezierMap, events: &Option<Vec<RPEEvent<T>>>) {
    if let Some(events) = events {
        for event in events {
            add_bezier(map, event);
        }
    }
}

fn get_bezier_map(rpe: &RPEChart) -> BezierMap {
    let mut map = HashMap::new();
    for line in &rpe.judge_line_list {
        for event_layer in line.event_layers.iter().flatten() {
            add_bezier_events(&mut map, &event_layer.alpha_events);
            add_bezier_events(&mut map, &event_layer.move_x_events);
            add_bezier_events(&mut map, &event_layer.move_y_events);
            add_bezier_events(&mut map, &event_layer.rotate_events);
            add_bezier_events(&mut map, &event_layer.speed_events);
        }
        if let Some(ext_layer) = &line.extended {
            add_bezier_events(&mut map, &ext_layer.paint_events);
            add_bezier_events(&mut map, &ext_layer.scale_x_events);
            add_bezier_events(&mut map, &ext_layer.scale_y_events);
            add_bezier_events(&mut map, &ext_layer.gif_events);
            add_bezier_events(&mut map, &ext_layer.incline_events);
            add_bezier_events(&mut map, &ext_layer.text_events);
            add_bezier_events(&mut map, &ext_layer.color_events);
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Chart
// ---------------------------------------------------------------------------

/// A fully loaded Phigros chart, ready for per-frame evaluation.
///
/// Parses RPE-format chart data and produces per-frame [`FrameState`]
/// via [`state_at`](Chart::state_at).
pub struct Chart {
    offset: f32,
    duration: f64,
    lines: Vec<LineData>,
    bpm_list: BpmList,
    /// Time of the previous `state_at` call, for fired-note detection.
    last_state_time: f64,
    frame: FrameState,
    /// 统一触发事件表(构建时生成一次,排序):每个非 fake note 的头/尾/
    /// 半拍 tick 一个事件。fx 查询、fired 报告都查这张表,消除此前
    /// fx_in_window(每帧窗口扫描)与 advance_fired(游标状态机)两套
    /// 独立扫描 + 两套判定标准。kind: 1/2/3/4 = note 头,5 = hold tick,
    /// 6 = hold 尾。
    triggers: Vec<TriggerEvent>,
    /// Scratch reused across frames: visible notes collected in time-sorted
    /// `order` scan order, index-tagged so they can be restored to draw
    /// order before publishing.
    visible_scratch: Vec<(usize, NoteState)>,
}

/// Reads `info.json` in `dir` (falling back to an RPE web-export `info.yml`,
/// then a legacy `info.txt`), rejecting non-RPE formats. Shared by
/// [`Chart::load`] and the editor document API ([`crate::core::edit`]).
pub(crate) fn load_info(dir: &std::path::Path) -> Result<ChartInfo> {
    let info: ChartInfo = match std::fs::read_to_string(dir.join("info.json")) {
        Ok(src) => serde_json::from_str(&src).context("failed to parse info.json")?,
        Err(json_err) => {
            // info.yml: RPE web export (serde_yaml), converted to ChartInfo.
            match std::fs::read_to_string(dir.join("info.yml")) {
                Ok(src) => {
                    let yaml: InfoYaml = serde_yaml::from_str(&src)
                        .context("failed to parse info.yml")?;
                    yaml.into_chart_info()
                }
                Err(yml_err) => {
                    let src = std::fs::read_to_string(dir.join("info.txt"))
                        .with_context(|| format!("failed to read info.json ({json_err}), info.yml ({yml_err}) or info.txt"))?;
                    parse_info_txt(&src)
                }
            }
        }
    };
    Ok(info)
}

/// 一个命中特效触发点:谱面时间 `t0` + 线索引 + note 的 x(相对线中心,
/// 画布单位)。渲染端用线变换把 x 换算成画布位置。
#[derive(Clone, Copy, Debug)]
pub struct FxTrigger {
    pub t0: f64,
    pub line: usize,
    pub x: f32,
}

/// 统一触发事件表条目(构建时生成):`t` = 谱面时钟秒。
/// `kind`:1/2/3/4 = note 头(tap/hold/flick/drag),5 = hold 半拍 tick,
/// 6 = hold 尾。fake note 不产生事件。
#[derive(Clone, Copy, Debug)]
pub struct TriggerEvent {
    pub t: f64,
    pub line: usize,
    pub x: f32,
    pub kind: u8,
}

/// hold tick 与 hold 尾在事件表里的内部 kind 标记。
pub const TRIGGER_TICK: u8 = 5;
pub const TRIGGER_TAIL: u8 = 6;

impl Chart {
    /// Fast-forward the fired-note cursors to `time` without reporting the
    /// notes that were jumped over. The editor calls this after a **forward**
    /// seek: notes in the skipped window must neither fire again (double
    /// count on top of `hits_before`) nor spawn hit FX / hitsounds (a dense
    /// jump would flood the audio mixer). `time` must be >= the previous
    /// call; backward seeks keep the replay-on-rewind semantics, which
    /// `state_at` already handles via its seek-back detection.
    pub fn reposition(&mut self, time: f64) {
        self.last_state_time = time;
    }

    /// Reads `info.json` in `dir` (falling back to an RPE-export `info.txt`
    /// when absent), then the chart file it names. Supports RPE, PEC, and PGR (官谱) formats.
    pub fn load(dir: &std::path::Path) -> Result<(ChartInfo, Chart)> {
        let info = load_info(dir)?;
        let chart_path = dir.join(&info.chart);
        let bytes = std::fs::read(&chart_path).with_context(|| format!("failed to read chart file {:?}", info.chart))?;
        let fmt = super::chart_format::detect_format(&bytes);
        let rpe = super::chart_format::parse_chart(fmt, &bytes, &info).context("chart format parse")?;
        let chart = Self::from_rpe_chart(&rpe, info.use_rpe_170_speed == Some(true))?;
        Ok((info, chart))
    }

    /// Build from an already-parsed `RPEChart` (editor path).
    pub fn from_rpe_chart(rpe: &RPEChart, use_rpe_170_speed: bool) -> Result<Chart> {
        let json = serde_json::to_string(rpe).context("rpe-re-serialize")?;
        Self::from_rpe(&json, use_rpe_170_speed)
    }

    /// Compressed hitsound schedule: `(hit time in seconds on the chart
    /// clock, note kind)` for every non-fake note, sorted by time.
    ///
    /// The audio trigger thread needs nothing else — no line/event state, no
    /// easing curves, no visibility logic — so this replaces the second full
    /// `Chart` it used to build (which re-read and re-parsed the chart files
    /// from disk and kept a full animation-state graph alive on that thread).
    /// Only beat→second conversion happens here (same `BpmList` math as
    /// [`parse_notes`]); kinds pass through unchanged (1 tap, 2 hold,
    /// 3 flick, 4 drag — all fire a hitsound at their hit time).
    pub fn fire_events_from_rpe(rpe: &RPEChart) -> Vec<(f64, u8)> {
        let mut bpm = BpmList::new(rpe.bpm_list.iter().map(|it| (it.start_time.beats(), it.bpm)).collect());
        let mut events = Vec::new();
        for line in &rpe.judge_line_list {
            if let Some(notes) = &line.notes {
                for note in notes {
                    if note.is_fake != 0 {
                        continue;
                    }
                    events.push((bpm.time(&note.start_time), note.kind));
                }
            }
        }
        events.sort_by(|a, b| a.0.total_cmp(&b.0));
        events
    }

    fn from_rpe(source: &str, use_rpe_170_speed: bool) -> Result<Chart> {
        let rpe: RPEChart = serde_json::from_str(source).context("json-parse-failed")?;
        let speed_mode = if rpe.meta.rpe_version >= 170 {
            SpeedEasingMode::Modern
        } else {
            SpeedEasingMode::Legacy
        };
        let offset = rpe.meta.offset as f32 / 1000.0;
        let bezier_map = get_bezier_map(&rpe);
        let mut r = BpmList::new(rpe.bpm_list.iter().map(|it| (it.start_time.beats(), it.bpm)).collect());
        fn vec<T>(v: &Option<Vec<T>>) -> impl Iterator<Item = &T> {
            v.iter().flat_map(|it| it.iter())
        }
        // max over all note endTimes and move/rotate/alpha/scale/text event
        // endTimes, + 1 second. speedEvents are stretched to fill max_time.
        let mut max_time = 0f64;
        for line in &rpe.judge_line_list {
            let notes_max = line
                .notes
                .as_ref()
                .map(|notes| notes.iter().map(|note| r.time(&note.end_time)).reduce(f64::max).unwrap_or(0.))
                .unwrap_or(0.);
            let events_max = line
                .event_layers
                .iter()
                .filter_map(|it| it.as_ref())
                .map(|layer| {
                    vec(&layer.alpha_events)
                        .chain(vec(&layer.move_x_events))
                        .chain(vec(&layer.move_y_events))
                        .chain(vec(&layer.rotate_events))
                        .map(|it| r.time(&it.end_time))
                        .reduce(f64::max)
                        .unwrap_or(0.)
                })
                .reduce(f64::max)
                .unwrap_or(0.);
            let ext_max = line
                .extended
                .as_ref()
                .map(|e| {
                    let mut m: f64 = 0.0;
                    // Same-type event chains (f32), incl. incline/paint/gif
                    // which used to be missed (shortened chart duration).
                    for it in vec(&e.scale_x_events)
                        .chain(vec(&e.scale_y_events))
                        .chain(vec(&e.incline_events))
                        .chain(vec(&e.paint_events))
                        .chain(vec(&e.gif_events))
                    {
                        m = m.max(r.time(&it.end_time));
                    }
                    for it in vec(&e.text_events) {
                        m = m.max(r.time(&it.end_time));
                    }
                    for it in vec(&e.color_events) {
                        m = m.max(r.time(&it.end_time));
                    }
                    m
                })
                .unwrap_or(0.);
            max_time = max_time.max(notes_max).max(events_max).max(ext_max);
        }
        let max_time = max_time + 1.;

        let mut lines = Vec::new();
        for (id, line) in rpe.judge_line_list.into_iter().enumerate() {
            let name = line.name.clone();
            lines.push(
                parse_judge_line(&mut r, line, max_time, speed_mode, use_rpe_170_speed, &bezier_map)
                    .with_context(|| format!("judge-line-location-name: jlid {id}, name {name}"))?,
            );
        }
        fn has_cycle(lines: &[LineData], index: usize, visited: &mut Vec<usize>) -> Option<usize> {
            if let Some(parent_index) = lines[index].parent {
                // Father indices out of range used to OOB-panic here; report
                // them as an error instead (parse_judge_line already rejects
                // negative values, this guards hand-built/edited data).
                if parent_index >= lines.len() {
                    return Some(parent_index);
                }
                if visited.contains(&parent_index) {
                    return Some(parent_index);
                }
                visited.push(parent_index);
                return has_cycle(lines, parent_index, visited);
            }
            None
        }
        for (i, _line) in lines.iter().enumerate() {
            let mut vec = Vec::new();
            vec.push(i);
            if let Some(line) = has_cycle(&lines, i, &mut vec) {
                anyhow::bail!("invalid parent relation (cycle or out-of-range father): line {line}");
            }
        }
        // Multiple-hint, port of prpr `process_lines`: every note whose hit
        // time occurs 2+ times globally (any line, fakes included) gets the
        // hint. O(n log n): sort all times, scan adjacent equal groups.
        {
            let mut all: Vec<(f64, usize, usize)> = Vec::new();
            for (line_idx, line) in lines.iter().enumerate() {
                for (note_idx, note) in line.notes.iter().enumerate() {
                    all.push((note.time, line_idx, note_idx));
                }
            }
            all.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut i = 0;
            while i < all.len() {
                let mut j = i + 1;
                // times are produced by the same algorithm, so exact f64
                // equality is sound (prpr relies on the same)
                while j < all.len() && all[j].0 == all[i].0 {
                    j += 1;
                }
                if j != i + 1 {
                    for &(_, line_idx, note_idx) in &all[i..j] {
                        lines[line_idx].notes[note_idx].multiple_hint = true;
                    }
                }
                i = j;
            }
        }
        // Persistent per-line output slots: `state_at` clears and refills
        // each `notes` vec in place instead of allocating fresh ones.
        let frame_lines = lines
            .iter()
            .map(|line| LineState {
                alpha: 0.,
                rotation: 0.,
                position: [0.; 2],
                scale: [1.; 2],
                color: [1.; 4],
                texture: line.texture.clone(),
                z_order: line.z_order,
                notes: Vec::new(),
                parent: line.parent,
                rot_with_parent: line.rot_with_parent,
                ctrl_pos_x: 0.0, ctrl_pos_y: 0.0, ctrl_size_x: 1.0, ctrl_size_y: 1.0,
                ctrl_alpha: 1.0, ctrl_y: 1.0, pe_hide: false, draw_below: true, appear_before: 0.0, incline_sin: 0.0,
                attach_ui: None,
            })
            .collect();
        // 统一触发事件表:头/尾/半拍 tick(非 fake),按时间排序。
        // 半拍 tick 反向解算(秒→拍→半拍→秒),与旧 fx_in_window 同语义;
        // tick 严格早于 hold 尾(恰落在 end_time 的半拍不产生 tick)。
        let mut trig_bpm = BpmList::new(r.elements().iter().map(|&(b, _, v)| (b, v)).collect());
        let mut triggers: Vec<TriggerEvent> = Vec::new();
        for (line_idx, line) in lines.iter().enumerate() {
            for n in &line.notes {
                if n.fake {
                    continue;
                }
                triggers.push(TriggerEvent { t: n.time, line: line_idx, x: n.x, kind: n.kind });
                if n.kind == 2 && n.end_time > n.time {
                    let b_lo = trig_bpm.beat(n.time);
                    let b_end = trig_bpm.beat(n.end_time);
                    let mut tb = (b_lo * 2.0).floor() / 2.0 + 0.5;
                    while tb < b_end {
                        let tt = trig_bpm.time_beats(tb);
                        if tt < n.end_time {
                            triggers.push(TriggerEvent { t: tt, line: line_idx, x: n.x, kind: TRIGGER_TICK });
                        }
                        tb += 0.5;
                    }
                    triggers.push(TriggerEvent { t: n.end_time, line: line_idx, x: n.x, kind: TRIGGER_TAIL });
                }
            }
        }
        triggers.sort_by(|a, b| a.t.total_cmp(&b.t));
        Ok(Chart {
            offset,
            duration: max_time,
            lines,
            bpm_list: r,
            last_state_time: 0.,
            frame: FrameState { time: 0., lines: frame_lines, fired: Vec::new() },
            visible_scratch: Vec::new(),
            triggers,
        })
    }

    /// Fired-note detection: returns trigger events (heads / hold ticks /
    /// hold tails) crossed since the previous call, backed by the unified
    /// [`TriggerEvent`] table (no per-frame scanning, no cursor state).
    /// Backward seeks (`time < previous call`) report nothing this call;
    /// the next call replays events after the seek target (same semantics
    /// as the old cursor-based implementation).
    pub fn advance_fired(&mut self, time: f64) -> &[FiredNote] {
        let last = self.last_state_time;
        self.last_state_time = time;
        // 帧时钟:渲染端 hit-fx 的 `now`(= frame.time)依赖它,倒退时
        // age = now - t0 才对齐。务必每帧更新,否则粒子全被 `now < t0` 跳过。
        self.frame.time = time;
        self.frame.fired.clear();
        if time < last {
            // seek backward — report nothing this call
            return &self.frame.fired;
        }
        let start = self.triggers.partition_point(|e| e.t <= last);
        let end = self.triggers.partition_point(|e| e.t <= time);
        self.frame.fired.extend(self.triggers[start..end].iter().map(|e| {
            let (tick, hold_tail, kind) = match e.kind {
                TRIGGER_TICK => (true, false, 2),
                TRIGGER_TAIL => (false, true, 2),
                k => (false, false, k),
            };
            FiredNote {
                line: e.line,
                kind,
                x: e.x,
                fake: false,
                tick,
                hold_tail,
            }
        }));
        &self.frame.fired
    }

    /// Evaluates all animations at `time` (seconds on the chart clock) and
    /// returns the frame. The returned reference is invalidated by the next
    /// call.
    ///
    /// Per line, notes are scanned only from a persistent cursor into the
    /// load-time `order` index (sorted by `max(time, end_time)`): notes
    /// behind it are past both their hit and hold end, so under 过点即消
    /// semantics they can neither be visible nor fire again. Backward seeks
    /// (`time < previous call`) reset the cursors.
    pub fn state_at(&mut self, time: f64) -> &FrameState {
        let _s = crate::trace_span!("state_at");
        let seek_back = time < self.last_state_time;
        self.advance_fired(time);
        if seek_back {
            // backward seek — the visibility cursor restarts from the top
            // too (advance_fired only owns the fired cursor)
            for line in self.lines.iter_mut() {
                line.cursor = 0;
            }
        }
        let Chart {
            lines,
            frame,
            visible_scratch,
            bpm_list,
            ..
        } = self;
        for (line, out) in lines.iter_mut().zip(frame.lines.iter_mut()) {
            line.alpha.set_time(time);
            line.pe_alpha.set_time(time);
            line.rotation.set_time(time);
            line.move_x.set_time(time);
            line.move_y.set_time(time);
            line.scale_x.set_time(time);
            line.scale_y.set_time(time);
            line.color.set_time(time);
            line.height.set_time(time);
            // [E] CtrlObject evaluation
            line.ctrl_pos_x.set_time(time);
            line.ctrl_pos_y.set_time(time);
            line.ctrl_size_x.set_time(time);
            line.ctrl_size_y.set_time(time);
            line.ctrl_alpha.set_time(time);
            line.ctrl_y.set_time(time);
            line.incline.set_time(time);
            // phira: CtrlObject values are multipliers with default 1.0.
            out.ctrl_pos_x = line.ctrl_pos_x.now_opt().unwrap_or(1.0);
            out.ctrl_pos_y = line.ctrl_pos_y.now_opt().unwrap_or(1.0);
            out.ctrl_size_x = line.ctrl_size_x.now_opt().unwrap_or(1.0);
            out.ctrl_size_y = line.ctrl_size_y.now_opt().unwrap_or(1.0);
            out.ctrl_alpha = line.ctrl_alpha.now_opt().unwrap_or(1.0);
            out.ctrl_y = line.ctrl_y.now_opt().unwrap_or(1.0);
            let incline_deg = line.incline.now_opt().unwrap_or(0.0);
            out.incline_sin = incline_deg.to_radians().sin();
            let raw_alpha = line.alpha.now_opt().unwrap_or(1.0);
            out.alpha = raw_alpha.max(0.);
            // PE alpha extension (phira `Chart::render` line.rs:365-384).
            // The `w` decode uses the RAW (unscaled) alpha value — the
            // 1/255-scaled rendering alpha would turn -255 into -1 and
            // misdecode every extended value as `w == 1` (hide everything).
            let pe_w = line.pe_alpha.now_opt().unwrap_or(1.0);
            let w = (-pe_w).floor() as i64;
            out.pe_hide = w == 1;
            out.draw_below = !(w == 2);
            // appear_before = (w-100)/100 BEATS. NOTE: phira implements
            // (w-100)/10 (10× longer, 15.5 beats for -255); the /100 form
            // matches charting convention (~1.55 beats for -255, i.e. 0.458s
            // at 203 BPM) and actual chart behavior — the /10 value makes
            // notes activate far too early in play.
            out.appear_before = if (100..1000).contains(&w) {
                (w as f64 - 100.0) / 100.0
            } else {
                0.0
            };
            out.rotation = line.rotation.now().to_radians();
            out.position = [line.move_x.now(), line.move_y.now()];
            out.scale = [line.scale_x.now_opt().unwrap_or(1.0), line.scale_y.now_opt().unwrap_or(1.0)];
            out.color = line.color.now_opt().map(|c| [c.r, c.g, c.b, c.a]).unwrap_or([1.; 4]);
            if out.texture != line.texture {
                out.texture.clone_from(&line.texture);
            }
            out.z_order = line.z_order;
            out.parent = line.parent;
            if out.attach_ui != line.attach_ui {
                out.attach_ui.clone_from(&line.attach_ui);
            }
            out.notes.clear();
        }
        // [parent] Propagate parent transforms, matching phira's fetch_rot /
        // fetch_pos (line.rs:211-228). Semantics:
        //   - position: ALWAYS inherited, recursively:
        //       child_pos = parent.fetch_pos() + R(parent.fetch_rot()) × child_own
        //   - rotation: inherited ONLY when rotateWithFather is set:
        //       rot = own_rot + (rot_with_parent ? parent.fetch_rot() : 0)
        //   - alpha: NOT inherited (phira render uses each line's own alpha).
        // Parents are processed before children regardless of list order, and
        // multi-level chains resolve recursively.
        {
            // Resolve rotations first (phira fetch_rot, recursive).
            // phira: fetch_rot(i) = rot_i + (i.rot_with_parent ? fetch_rot(parent) : 0),
            // 递归展开 = 逐级检查"当前节点"自己的 rot_with_parent,为 true 则累加
            // 父线旋转并上移。注意:检查的是当前节点(首层为 i),不是父线!
            let mut rot_resolved = vec![0.0f32; frame.lines.len()];
            for i in 0..frame.lines.len() {
                let mut rot = frame.lines[i].rotation;
                let mut node = i;
                let mut cur = frame.lines[i].parent;
                let mut visited: Vec<usize> = Vec::new();
                while let Some(pidx) = cur {
                    if pidx >= frame.lines.len() || visited.contains(&pidx) || pidx == i {
                        break;
                    }
                    visited.push(pidx);
                    if !frame.lines[node].rot_with_parent {
                        break;
                    }
                    rot += frame.lines[pidx].rotation;
                    node = pidx;
                    cur = frame.lines[pidx].parent;
                }
                rot_resolved[i] = rot;
            }
            for i in 0..frame.lines.len() {
                frame.lines[i].rotation = rot_resolved[i];
            }
            // Then positions (phira fetch_pos, recursive): walk the parent
            // chain from root to leaf, composing R(parent_rot) × own_offset.
            let own_pos: Vec<[f32; 2]> = frame.lines.iter().map(|l| l.position).collect();
            for i in 0..frame.lines.len() {
                let mut acc = own_pos[i];
                let mut cur = frame.lines[i].parent;
                let mut stack: Vec<usize> = Vec::new();
                let mut visited: Vec<usize> = Vec::new();
                while let Some(pidx) = cur {
                    if pidx >= frame.lines.len() || visited.contains(&pidx) || pidx == i {
                        break;
                    }
                    visited.push(pidx);
                    stack.push(pidx);
                    cur = frame.lines[pidx].parent;
                }
                // Root first (last pushed) → direct parent (first pushed).
                for &pidx in stack.iter().rev() {
                    let p = &frame.lines[pidx];
                    let pr = p.rotation; // phira: parent's fetch_rot
                    let cos = pr.cos();
                    let sin = pr.sin();
                    let (lx, ly) = (acc[0], acc[1]);
                    acc[0] = p.position[0] + cos * lx - sin * ly;
                    acc[1] = p.position[1] + sin * lx + cos * ly;
                }
                frame.lines[i].position = acc;
            }
        }
        for (line, out) in lines.iter_mut().zip(frame.lines.iter_mut()) {
            // PE alpha extension: w=1 hides line + notes entirely.
            if out.pe_hide { continue; }
            let line_height = line.height.now() as f64;
            // PE alpha extension: w=2 forces draw_below=false (hides only
            // below notes); otherwise the line's own show_below applies.
            let show_below = line.show_below && out.draw_below;
            if !show_below {
                // skip notes dead under 过点即消: max(time, end_time) is past
                // (0.001 slack mirrors the below-screen cull epsilon)
                let start = line.cursor;
                line.cursor = start
                    + line.order[start..].partition_point(|&ni| {
                        let n = &line.notes[ni];
                        n.time.max(n.end_time) < time - 0.001
                    });
            }
            // PE alpha extension: notes appear `appear_before` beats before
            // their hit beat (phira note.rs:198-204, unit = beats).
            let appear_beat = if out.appear_before > 0.0 {
                Some(bpm_list.beat(time) + out.appear_before)
            } else {
                None
            };
            for &ni in &line.order[line.cursor..] {
                let note = &mut line.notes[ni];
                note.alpha.set_time(time);
                if let Some(ab) = appear_beat {
                    if bpm_list.beat(note.time) > ab {
                        continue;
                    }
                }
                let note_alpha = note.alpha.now_opt().unwrap_or(1.0).max(0.);
                let spd = note.speed;
                let base = (note.height - line_height) * spd;
                let yoff = note.y_offset as f64;
                let (y, hold_end_y) = if note.kind == 2 {
                    if time >= note.end_time {
                        continue;
                    }
                    // after the hit time the hold head sticks to the line
                    let bottom = if note.time <= time { 0. } else { base };
                    if !show_below && note.time > time && base <= -0.001 {
                        continue;
                    }
                    let head_y = yoff + bottom;
                    let tail_y = yoff + (note.end_height - line_height) * spd;
                    // Viewport cull on the head/tail INTERVAL, not the closest
                    // endpoint: a long hold whose head is far above still has
                    // its body dipping into view — culling on head_y alone
                    // (or on min(head,tail)) would wrongly drop the whole
                    // hold for part of its visible lifetime.
                    let near = head_y.min(tail_y);
                    let far = head_y.max(tail_y);
                    if far < -2.0 || near > 2.0 {
                        continue;
                    }
                    (head_y, Some(tail_y))
                } else {
                    // Editor preview: notes vanish instantly at hit time
                    // (no prpr 0.16s fade-out). Applies to every note — prpr
                    // skips judged non-hold notes outright (note.rs render),
                    // regardless of show_below. The old `!show_below ||`
                    // guard kept notes alive past their hit time on cover
                    // lines (is_cover=0 → show_below=true).
                    // Fake notes always vanish past their hit time to prevent trails.
                    if time >= note.time {
                        continue;
                    }
                    if !show_below && note.time > time && base <= -0.001 {
                        continue;
                    }
                    // Viewport cull (phira line.rs height_above/height_below):
                    // skip notes still far above or below the visible canvas.
                    let above = base + yoff;
                    if above > 2.0 || above < -2.0 {
                        continue;
                    }
                    (yoff + base, None)
                };
                // prpr renders below notes under a (1, -1) mirror: y negates
                let sign = if note.above { 1. } else { -1. };
                visible_scratch.push((
                    ni,
                    NoteState {
                        kind: note.kind,
                        time: note.time,
                        relative: [note.x, (sign * y) as f32],
                        hold_end_y: hold_end_y.map(|it| sign * it),
                        alpha: note_alpha,
                        scale: note.scale,
                        above: note.above,
                        fake: note.fake,
                        multiple_hint: note.multiple_hint,
                    },
                ));
            }
            // restore draw order (indices are unique: unstable sort is fine)
            visible_scratch.sort_unstable_by_key(|(ni, _)| *ni);
            out.notes.extend(visible_scratch.drain(..).map(|(_, n)| n));
        }
        frame
    }

    /// 查询 `[t_lo, t_hi]` 窗口内的所有命中特效触发点——**纯时间函数**:
    /// 前进/后退/任意跳转都按当前谱面时间查询,天然可倒退渲染。
    ///
    /// 触发点来源:
    /// - note 头:`note.time`
    /// - hold 尾(释放):`note.end_time`
    /// - hold 半拍 tick:反向解算——`beat(time)` 秒→拍,对齐半拍步进,
    ///   `time_beats` 拍→秒(周期化回调,BPM 相关)
    ///
    /// 不裁剪:窗口内全部触发点都返回(密集段落靠粒子渲染自身承担成本)。
    pub fn fx_in_window(&self, t_lo: f64, t_hi: f64) -> Vec<FxTrigger> {
        // 统一事件表闭区间查询 [t_lo, t_hi](头/尾/tick 全含,非 fake),
        // 不裁剪上限。
        let start = self.triggers.partition_point(|e| e.t < t_lo);
        let end = self.triggers.partition_point(|e| e.t <= t_hi);
        self.triggers[start..end].iter()
            .map(|e| FxTrigger { t0: e.t, line: e.line, x: e.x })
            .collect()
    }

    /// 统一触发事件表区间查询 `(t_lo, t_hi]`(开左闭右,与
    /// advance_fired 的游标语义一致)。
    pub fn triggers_in(&self, t_lo: f64, t_hi: f64) -> &[TriggerEvent] {
        let start = self.triggers.partition_point(|e| e.t <= t_lo);
        let end = self.triggers.partition_point(|e| e.t <= t_hi);
        &self.triggers[start..end]
    }

    /// 求线在 `time` 时刻的位姿 (position, rotation 弧度),含 parent 继承
    /// (与 state_at 的 fetch_rot/fetch_pos 等价)。hit-fx 用:特效位置在
    /// **触发瞬间 t0** 的线变换下计算,不绑定当前帧线状态——线之后移动/
    /// 旋转时,已爆散的粒子留在原地。
    ///
    /// 幂等:只对事件轨道 set_time(无游标副作用),不会扰动 notes 可见性
    /// 游标或 fired 状态,可安全插在任何 state_at 调用之间。
    pub fn line_pose_at(&mut self, line_idx: usize, time: f64) -> ([f32; 2], f32) {
        // 只 set_time 目标线 + 其父链(位姿组合只依赖这些线),其余线不动。
        // 密集谱面每帧多次调用时这是数量级差异(全量 set_time → 定向)。
        let mut chain: Vec<usize> = vec![line_idx];
        let mut cur = self.lines[line_idx].parent;
        let mut visited: Vec<usize> = Vec::new();
        while let Some(pidx) = cur {
            if pidx >= self.lines.len() || visited.contains(&pidx) || pidx == line_idx {
                break;
            }
            visited.push(pidx);
            chain.push(pidx);
            cur = self.lines[pidx].parent;
        }
        for &i in &chain {
            let line = &mut self.lines[i];
            line.move_x.set_time(time);
            line.move_y.set_time(time);
            line.rotation.set_time(time);
        }
        // 每个链成员的解析旋转:自身 + 父链累加(逐级检查当前节点自己的
        // rot_with_parent,与 state_at/phira fetch_rot 一致)。祖先都在 chain 内。
        let mut rot_resolved = vec![0.0f32; chain.len()];
        for (ci, &i) in chain.iter().enumerate() {
            let mut rot = self.lines[i].rotation.now().to_radians();
            let mut node = i;
            let mut cur = self.lines[i].parent;
            let mut seen: Vec<usize> = Vec::new();
            while let Some(pidx) = cur {
                if pidx >= self.lines.len() || seen.contains(&pidx) || pidx == i {
                    break;
                }
                seen.push(pidx);
                if !self.lines[node].rot_with_parent {
                    break;
                }
                rot += self.lines[pidx].rotation.now().to_radians();
                node = pidx;
                cur = self.lines[pidx].parent;
            }
            rot_resolved[ci] = rot;
        }
        // Position: root-first composition (same as state_at).
        let mut acc = [self.lines[line_idx].move_x.now(), self.lines[line_idx].move_y.now()];
        // chain = [self, parent, ..., root];rev 后跳过 self → root first。
        for &pidx in chain.iter().rev().skip(1) {
            let ci = chain.iter().position(|&c| c == pidx).unwrap();
            let pr = rot_resolved[ci];
            let (cos, sin) = (pr.cos(), pr.sin());
            let (lx, ly) = (acc[0], acc[1]);
            acc[0] = self.lines[pidx].move_x.now() + cos * lx - sin * ly;
            acc[1] = self.lines[pidx].move_y.now() + sin * lx + cos * ly;
        }
        (acc, rot_resolved[0])
    }

    /// Total chart duration in seconds.
    pub fn duration(&self) -> f64 {
        self.duration
    }

    /// `META.offset` in **seconds** (`ms / 1000`).
    ///
    /// prpr sign convention (`prpr/src/scene/game.rs:801,1051`): during play
    /// `res.time` (the chart clock) = `(audio_time - total_offset).max(0.)`,
    /// where `total_offset = chart.offset + info.offset + user config.offset`.
    /// Equivalently `audio_time = chart_time + total_offset`: a **positive**
    /// offset means chart beats *lag* the audio (the audio leads). The app
    /// must therefore feed `state_at(audio_time - total_offset)`, clamped to
    /// `>= 0`.
    pub fn offset(&self) -> f32 {
        self.offset
    }

    /// Total number of non-fake notes (combo/score denominator).
    pub fn note_count(&self) -> usize {
        self.lines.iter().map(|line| line.notes.iter().filter(|note| !note.fake).count()).sum()
    }

    /// Combo 口径：非 fake 音符 note.time <= `time` 各计 1（hold 头），
    /// 外加非 fake hold 中 end_time <= `time` 各再计 1（hold 尾）。
    /// O(n) full scan — intended for seek handling only.
    ///
    /// Boundary: `<=` is the exact complement of the fired condition
    /// (`last < note.time <= time`) — after a seek to `time`, notes at exactly
    /// `time` can never fire again (last = time), so they must be counted
    /// here or they would be lost from combo/score restoration.
    pub fn hits_before(&self, time: f64) -> usize {
        self.lines
            .iter()
            .map(|line| {
                line.notes
                    .iter()
                    .filter(|note| !note.fake)
                    .map(|note| (note.time <= time) as usize + (note.kind == 2 && note.end_time <= time) as usize)
                    .sum::<usize>()
            })
            .sum()
    }

    /// Maximum combo: every non-fake note contributes its head, and every
    /// non-fake hold contributes its tail on top.
    pub fn max_combo(&self) -> usize {
        self.lines
            .iter()
            .map(|line| line.notes.iter().filter(|note| !note.fake).map(|note| 1 + (note.kind == 2) as usize).sum::<usize>())
            .sum()
    }

    /// Number of judge lines.
    pub fn line_count(&self) -> usize { self.lines.len() }

    /// Convert seconds to beats at the current playback position.
    pub fn time_to_beat(&mut self, time: f64) -> f64 {
        self.bpm_list.beat(time)
    }

    /// Name of line `i`.
    pub fn line_name(&self, i: usize) -> &str { &self.lines[i].name }

    /// Distinct non-`line.png` texture filenames used by judge lines.
    pub fn textures(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.lines
            .iter()
            .filter_map(|it| it.texture.clone())
            .filter(|it| seen.insert(it.clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bpm::Triple;

    const MINIMAL: &str = r#"{
        "META": { "offset": 100, "RPEVersion": 160 },
        "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
        "judgeLineList": [
            {
                "Name": "line0",
                "Texture": "line.png",
                "father": -1,
                "eventLayers": [
                    {
                        "alphaEvents": [
                            { "start": 255.0, "end": 0.0, "startTime": [0, 0, 1], "endTime": [4, 0, 1], "easingType": 1 }
                        ]
                    },
                    null
                ],
                "notes": [
                    {
                        "type": 1, "above": 1,
                        "startTime": [1, 0, 1], "endTime": [1, 0, 1],
                        "positionX": 675.0, "yOffset": 0.0,
                        "alpha": 255, "hitsound": null,
                        "size": 1.0, "speed": 1.0,
                        "isFake": 0, "visibleTime": 999999.0
                    }
                ],
                "isCover": 1
            }
        ]
    }"#;

    #[test]
    fn triple_and_bpm_list() {
        // Triple deserializes from [i, n, d] and means i + n/d
        let t: Triple = serde_json::from_str("[1, 1, 2]").unwrap();
        assert_eq!(t.beats(), 1.5);

        // 120bpm for beats 0..4 (0.5s each), then 240bpm (0.25s each)
        let mut b = BpmList::new(vec![(0.0, 120.0), (4.0, 240.0)]);
        assert!((b.time_beats(4.0) - 2.0).abs() < 1e-9);
        assert!((b.time_beats(6.0) - 2.5).abs() < 1e-9);
        assert!((b.beat(2.5) - 6.0).abs() < 1e-9);
        // beats before the first entry are extrapolated by the first bpm
        assert!((b.time_beats(-2.0) - -1.0).abs() < 1e-9);
    }

    #[test]
    fn rpe_version_accepts_string() {
        let chart: RPEChart = serde_json::from_str(r#"{"META":{"offset":0,"RPEVersion":"170"},"BPMList":[],"judgeLineList":[]}"#).unwrap();
        assert_eq!(chart.meta.rpe_version, 170);
        let chart: RPEChart = serde_json::from_str(r#"{"META":{"offset":0},"BPMList":[],"judgeLineList":[]}"#).unwrap();
        assert_eq!(chart.meta.rpe_version, 160);
    }

    #[test]
    fn load_info_from_yml() {
        let dir = std::env::temp_dir().join("phimakor-info-yml-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("info.yml"), r#"
name: Test Song
composer: Composer A
charter: Charter B
level: "IN 14"
difficulty: 11.5
chart: chart.json
music: audio.mp3
illustration: bg.png
lineLength: 4.5
previewStart: 12
"#).unwrap();
        let info = load_info(&dir).unwrap();
        assert_eq!(info.name, "Test Song");
        assert_eq!(info.composer, "Composer A");
        assert_eq!(info.charter, "Charter B");
        assert_eq!(info.level, "IN 14");
        assert!((info.difficulty - 11.5).abs() < 1e-6);
        assert_eq!(info.chart, "chart.json");
        assert!((info.line_length - 4.5).abs() < 1e-6);
        assert!((info.preview_start - 12.0).abs() < 1e-6);
        // info.json wins over info.yml when both exist.
        std::fs::write(dir.join("info.json"), r#"{"chart":"chart.json","name":"Json Name"}"#).unwrap();
        let info = load_info(&dir).unwrap();
        assert_eq!(info.name, "Json Name");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn negative_father_rejected() {
        // father < -1 used to wrap to a huge usize and panic in has_cycle.
        let chart = r#"{
            "META": { "offset": 0 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                { "Name": "a", "Texture": "line.png", "father": -2, "eventLayers": [], "notes": [], "isCover": 1 }
            ]
        }"#;
        assert!(Chart::from_rpe(chart, false).is_err());
    }

    #[test]
    fn out_of_range_father_rejected() {
        // father index beyond judgeLineList must error, not OOB-panic.
        let chart = r#"{
            "META": { "offset": 0 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                { "Name": "a", "Texture": "line.png", "father": 5, "eventLayers": [], "notes": [], "isCover": 1 }
            ]
        }"#;
        assert!(Chart::from_rpe(chart, false).is_err());
    }

    #[test]
    fn speed_bezier_events_load() {
        // A bezier speed event used to panic (bezier_map[&key] miss — speed
        // events were not collected by get_bezier_map).
        let chart = r#"{
            "META": { "offset": 0, "RPEVersion": 170 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "a", "Texture": "line.png", "father": -1,
                    "eventLayers": [
                        {
                            "speedEvents": [
                                { "start": 1.0, "end": 2.0, "startTime": [0, 0, 1], "endTime": [4, 0, 1], "easingType": 0, "bezier": 1, "bezierPoints": [0.0, 0.0, 1.0, 1.0] }
                            ]
                        }
                    ],
                    "notes": [], "isCover": 1
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(chart, false).expect("speed bezier chart must load");
        let frame = chart.state_at(0.0);
        assert_eq!(frame.lines.len(), 1);
    }

    #[test]
    fn ctrl_events_use_seconds_timeline() {
        // CtrlObject `x` is in beats; state_at queries in seconds. A pos
        // control keyframe at x=2 beats (1.0s @ 120 BPM) must resolve at t=1s.
        let chart = r#"{
            "META": { "offset": 0 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "a", "Texture": "line.png", "father": -1,
                    "eventLayers": [], "notes": [], "isCover": 1,
                    "posControl": [ { "easing": 1, "x": 2, "pos": 0.5 } ]
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(chart, false).unwrap();
        let frame = chart.state_at(1.0);
        assert!((frame.lines[0].ctrl_pos_x - 0.5).abs() < 1e-3, "ctrl_pos_x at t=1s: {}", frame.lines[0].ctrl_pos_x);
    }

    #[test]
    fn max_time_covers_extended_events() {
        // incline events used to be missed by max_time → duration too short.
        // incline endTime 4 beats @120bpm = 2.0s, +1s = 3.0s.
        let chart = r#"{
            "META": { "offset": 0 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "a", "Texture": "line.png", "father": -1,
                    "eventLayers": [],
                    "extended": {
                        "inclineEvents": [
                            { "start": 0.0, "end": 10.0, "startTime": [0, 0, 1], "endTime": [4, 0, 1], "easingType": 1 }
                        ]
                    },
                    "notes": [], "isCover": 1
                }
            ]
        }"#;
        let chart = Chart::from_rpe(chart, false).unwrap();
        assert!((chart.duration() - 3.0).abs() < 1e-9, "duration: {}", chart.duration());
    }

    #[test]
    fn minimal_chart_state() {
        let mut chart = Chart::from_rpe(MINIMAL, false).unwrap();
        assert!((chart.offset() - 0.1).abs() < 1e-6); // 100ms -> 0.1s
        // max(note end 0.5s, alpha event end 4 beats @120bpm = 2.0s) + 1
        assert!((chart.duration() - 3.0).abs() < 1e-9);
        assert!(chart.textures().is_empty());

        let frame = chart.state_at(0.0);
        assert_eq!(frame.lines.len(), 1);
        let line = &frame.lines[0];
        assert!((line.alpha - 1.0).abs() < 1e-4); // 255/255 at t=0
        assert_eq!(line.position, [0.0, 0.0]);
        assert_eq!(line.rotation, 0.0);
        assert_eq!(line.scale, [1.0, 1.0]);
        assert_eq!(line.color, [1.0; 4]);
        assert_eq!(line.texture, None);
        assert_eq!(line.z_order, 0);
        assert_eq!(line.parent, None);

        assert_eq!(line.notes.len(), 1);
        let note = &line.notes[0];
        assert_eq!(note.kind, 1);
        assert!((note.time - 0.5).abs() < 1e-9); // 1 beat @120bpm
        assert!((note.relative[0] - 1.0).abs() < 1e-6); // 675/675
        assert!(note.relative[1].abs() < 1e-6); // no speed events -> height 0
        assert_eq!(note.hold_end_y, None);
        assert!((note.alpha - 1.0).abs() < 1e-6);
        assert!((note.scale - 1.0).abs() < 1e-6);
        assert!(note.above);
        assert!(!note.fake);

        // halfway through the linear 255->0 alpha event (4 beats = 2s)
        let frame = chart.state_at(1.0);
        assert!((frame.lines[0].alpha - 0.5).abs() < 1e-4);
        // the tap note is past hit time (0.5) -> culled instantly
        assert!(frame.lines[0].notes.is_empty());

        // just before hit time the note is still there, unfaded
        let frame = chart.state_at(0.49);
        assert_eq!(frame.lines[0].notes.len(), 1);
        assert!((frame.lines[0].notes[0].alpha - 1.0).abs() < 1e-4);
    }

    #[test]
    fn multiple_hint_across_lines() {
        // line0: tap @ beat 1; line1: fake tap @ beat 1 + tap @ beat 2.
        // The two beat-1 notes (fake included) get the hint; the beat-2 one doesn't.
        const MULTI: &str = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "a", "Texture": "line.png", "father": -1,
                    "eventLayers": [],
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [1, 0, 1], "endTime": [1, 0, 1], "positionX": 100.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ],
                    "isCover": 1
                },
                {
                    "Name": "b", "Texture": "line.png", "father": -1,
                    "eventLayers": [],
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [1, 0, 1], "endTime": [1, 0, 1], "positionX": 200.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 1, "visibleTime": 999999.0 },
                        { "type": 1, "above": 1, "startTime": [2, 0, 1], "endTime": [2, 0, 1], "positionX": 300.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ],
                    "isCover": 1
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(MULTI, false).unwrap();
        let frame = chart.state_at(0.0);
        assert_eq!(frame.lines.len(), 2);
        assert_eq!(frame.lines[0].notes.len(), 1);
        assert!(frame.lines[0].notes[0].multiple_hint);
        assert_eq!(frame.lines[1].notes.len(), 2);
        let mut by_time: Vec<_> = frame.lines[1].notes.iter().collect();
        by_time.sort_by(|a, b| a.time.total_cmp(&b.time));
        assert!(by_time[0].multiple_hint); // t = 0.5, shared with line0 (fake)
        assert!(by_time[0].fake);
        assert!(!by_time[1].multiple_hint); // t = 1.0, unique
    }

    #[test]
    fn fired_notes_tap() {
        // single tap @ beat 1 = 0.5s @120bpm, x = 675/675 = 1.0
        const SRC: &str = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "a", "Texture": "line.png", "father": -1,
                    "eventLayers": [],
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [1, 0, 1], "endTime": [1, 0, 1], "positionX": 675.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ],
                    "isCover": 1
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(SRC, false).unwrap();

        assert!(chart.state_at(0.4).fired.is_empty());

        // 0.4 -> 0.6 crosses note.time = 0.5
        let frame = chart.state_at(0.6);
        assert_eq!(frame.fired.len(), 1);
        let f = &frame.fired[0];
        assert_eq!(f.line, 0);
        assert_eq!(f.kind, 1);
        assert!((f.x - 1.0).abs() < 1e-6);
        assert!(!f.fake);
        assert!(!f.tick); // initial hit, not a sustain tick

        // 0.6 -> 0.7: nothing new
        assert!(chart.state_at(0.7).fired.is_empty());

        // seek backward: nothing reported
        assert!(chart.state_at(0.3).fired.is_empty());

        // jump past the note: reported even though culled from `lines`
        let frame = chart.state_at(1.0);
        assert!(frame.lines[0].notes.is_empty()); // culled (past fadeout)
        assert_eq!(frame.fired.len(), 1);
        assert_eq!(frame.fired[0].kind, 1);
    }

    #[test]
    fn hold_body_culling_keeps_interval_in_view() {
        // A long hold whose HEAD is far above the viewport but whose BODY
        // (tail end) is inside it must stay rendered. Culling used to check
        // only min(head_y, tail_y): the far-above head dropped the whole hold.
        //
        // Speed profile (beats @120bpm, 0.5s/beat):
        //   beat 0-2: 0 -> -160 (integrates height to ~-21, head far above)
        //   beat 2-4: 0        (height frozen)
        //   beat 4-6: +45      (body rises back into view)
        // hold beat 4->6 with yOffset 10 → head_y ≈ -11 (culled before),
        // tail_y ≈ +0.6 (inside the ±2 viewport).
        const SRC: &str = r#"{
            "META": { "offset": 0, "RPEVersion": 170 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "a", "Texture": "line.png", "father": -1,
                    "eventLayers": [
                        {
                            "speedEvents": [
                                { "start": 0.0, "end": -160.0, "startTime": [0, 0, 1], "endTime": [2, 0, 1], "easingType": 1 },
                                { "start": 0.0, "end": 0.0, "startTime": [2, 0, 1], "endTime": [4, 0, 1], "easingType": 1 },
                                { "start": 45.0, "end": 45.0, "startTime": [4, 0, 1], "endTime": [6, 0, 1], "easingType": 1 }
                            ]
                        }
                    ],
                    "notes": [
                        { "type": 2, "above": 1, "startTime": [4, 0, 1], "endTime": [6, 0, 1], "positionX": 675.0, "yOffset": 10.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ],
                    "isCover": 1
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(SRC, false).unwrap();
        // 1.9s = beat 3.8: hold not hit yet (hit at beat 4 = 2.0s). Head is
        // far above the viewport; the body dips in → must stay visible.
        let frame = chart.state_at(1.9);
        assert_eq!(frame.lines[0].notes.len(), 1, "hold body in viewport must not be culled");
        assert_eq!(frame.lines[0].notes[0].kind, 2);
        // Just before the head even approaches: both ends far above → culled.
        let frame = chart.state_at(0.8);
        assert!(frame.lines[0].notes.is_empty(), "fully offscreen hold must be culled");
    }

    #[test]
    fn fired_notes_hold_beats() {
        // hold beat 1 -> beat 4 @120bpm (0.5s per beat)
        const SRC: &str = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "a", "Texture": "line.png", "father": -1,
                    "eventLayers": [],
                    "notes": [
                        { "type": 2, "above": 1, "startTime": [1, 0, 1], "endTime": [4, 0, 1], "positionX": 675.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ],
                    "isCover": 1
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(SRC, false).unwrap();

        // land just after beat 1: exactly the hit event, no sustain beats yet
        let frame = chart.state_at(0.6);
        assert_eq!(frame.fired.len(), 1);
        assert_eq!(frame.fired[0].kind, 2);
        assert!(!frame.fired[0].tick); // hold's first hit is not a tick
        assert!(!frame.fired[0].hold_tail);

        // jump to just after beat 3: four half-beat sustain events (beats
        // 1.5, 2.0, 2.5, 3.0), hit not re-reported
        let frame = chart.state_at(1.6);
        assert_eq!(frame.fired.len(), 4);
        assert!(frame.fired.iter().all(|f| f.kind == 2));
        assert!(frame.fired.iter().all(|f| f.tick));
        assert!(frame.fired.iter().all(|f| !f.hold_tail));
        assert!(frame.fired.iter().all(|f| (f.x - 1.0).abs() < 1e-6));

        // jump past the end (2.0s): one last half-beat tick (1.75s) plus the
        // release event — release is not a tick
        let frame = chart.state_at(3.0);
        assert_eq!(frame.fired.len(), 2);
        assert!(frame.fired[0].tick && !frame.fired[0].hold_tail);
        let tail = &frame.fired[1];
        assert_eq!(tail.kind, 2);
        assert!(tail.hold_tail);
        assert!(!tail.tick);
        assert!((tail.x - 1.0).abs() < 1e-6);

        // past the end: no more events
        assert!(chart.state_at(4.0).fired.is_empty());

        assert_eq!(chart.note_count(), 1);
    }

    #[test]
    fn fired_notes_hold_half_beat_ticks() {
        // hold beat 1 -> beat 5 @120bpm: 1 second from head to beat 3 crosses
        // 4 half beats
        const SRC: &str = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "a", "Texture": "line.png", "father": -1,
                    "eventLayers": [],
                    "notes": [
                        { "type": 2, "above": 1, "startTime": [1, 0, 1], "endTime": [5, 0, 1], "positionX": 675.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ],
                    "isCover": 1
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(SRC, false).unwrap();

        // hit exactly at the head (0.5s)
        let frame = chart.state_at(0.5);
        assert_eq!(frame.fired.len(), 1);
        assert!(!frame.fired[0].tick);

        // 0.5s -> 1.5s: 2 beats = 4 half-beat ticks
        let frame = chart.state_at(1.5);
        assert_eq!(frame.fired.len(), 4);
        assert!(frame.fired.iter().all(|f| f.tick && !f.hold_tail));

        // release exactly at end_time (2.5s): half beats at 1.75..2.25 (3
        // ticks), the beat-5 half beat is the release, not a tick
        let frame = chart.state_at(2.5);
        assert_eq!(frame.fired.len(), 4);
        assert!(frame.fired[..3].iter().all(|f| f.tick));
        assert!(frame.fired[3].hold_tail && !frame.fired[3].tick);
    }

    #[test]
    fn hits_before_and_max_combo_count_hold_tails() {
        // tap @ beat 1, hold beat 1 -> beat 3, fake hold beat 1 -> beat 3
        const SRC: &str = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "a", "Texture": "line.png", "father": -1,
                    "eventLayers": [],
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [1, 0, 1], "endTime": [1, 0, 1], "positionX": 100.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 2, "above": 1, "startTime": [1, 0, 1], "endTime": [3, 0, 1], "positionX": 200.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 2, "above": 1, "startTime": [1, 0, 1], "endTime": [3, 0, 1], "positionX": 300.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 1, "visibleTime": 999999.0 }
                    ],
                    "isCover": 1
                }
            ]
        }"#;
        let chart = Chart::from_rpe(SRC, false).unwrap();
        assert_eq!(chart.note_count(), 2); // fakes excluded
        assert_eq!(chart.max_combo(), 3); // tap + hold head + hold tail

        assert_eq!(chart.hits_before(0.4), 0);
        assert_eq!(chart.hits_before(0.5), 2); // tap head + hold head
        assert_eq!(chart.hits_before(1.4), 2); // hold tail (1.5s) not yet
        assert_eq!(chart.hits_before(1.5), 3); // hold tail landed
        assert_eq!(chart.hits_before(99.0), 3);
    }

    #[test]
    fn note_count_excludes_fakes() {
        // multiple_hint chart: 3 notes total, 1 fake → 2
        const MULTI: &str = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "a", "Texture": "line.png", "father": -1,
                    "eventLayers": [],
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [1, 0, 1], "endTime": [1, 0, 1], "positionX": 100.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ],
                    "isCover": 1
                },
                {
                    "Name": "b", "Texture": "line.png", "father": -1,
                    "eventLayers": [],
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [1, 0, 1], "endTime": [1, 0, 1], "positionX": 200.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 1, "visibleTime": 999999.0 },
                        { "type": 1, "above": 1, "startTime": [2, 0, 1], "endTime": [2, 0, 1], "positionX": 300.0, "yOffset": 0.0, "alpha": 255, "hitsound": null, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ],
                    "isCover": 1
                }
            ]
        }"#;
        let chart = Chart::from_rpe(MULTI, false).unwrap();
        assert_eq!(chart.note_count(), 2);
        assert_eq!(chart.hits_before(0.4), 0);
        assert_eq!(chart.hits_before(0.5), 1); // fake at the same time excluded
        assert_eq!(chart.hits_before(0.7), 1);
        assert_eq!(chart.hits_before(2.0), 2);
    }

    #[test]
    fn info_txt_parsing() {
        let info = parse_info_txt(
            "# RPE export\n\
             Name: Test Song\n\
             Song: song.ogg\n\
             Picture: bg.png\n\
             Chart: chart.json\n\
             Level: IN Lv.14\n\
             Composer: Some Composer\n\
             Charter: Some Charter\n\
             Path: ignored/path\n\
             UnknownKey: ignored\n\
             \n\
             # trailing comment\n",
        );
        assert_eq!(info.name, "Test Song");
        assert_eq!(info.music, "song.ogg");
        assert_eq!(info.illustration, "bg.png");
        assert_eq!(info.chart, "chart.json");
        assert_eq!(info.level, "IN Lv.14");
        assert_eq!(info.composer, "Some Composer");
        assert_eq!(info.charter, "Some Charter");
        // unmapped fields keep defaults
        assert!((info.difficulty - 10.0).abs() < 1e-6);
        assert_eq!(info.offset, 0.0);
        assert!((info.background_dim - 0.6).abs() < 1e-6);
        assert_eq!(info.format, None);
    }

    #[test]
    fn bad_note_type_is_err() {
        let src = MINIMAL.replace(r#""type": 1"#, r#""type": 7"#);
        assert!(Chart::from_rpe(&src, false).is_err());
    }

    #[test]
    fn reposition_skips_forward_jump_window_but_backward_replays() {
        // 120bpm: taps at beats 2, 4, 6, 10 → 1.0s, 2.0s, 3.0s, 5.0s.
        let src = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "line0", "Texture": "line.png", "father": -1,
                    "eventLayers": [], "isCover": 0,
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [2, 0, 1], "endTime": [2, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 1, "above": 1, "startTime": [4, 0, 1], "endTime": [4, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 1, "above": 1, "startTime": [6, 0, 1], "endTime": [6, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 1, "above": 1, "startTime": [10, 0, 1], "endTime": [10, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ]
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(src, false).unwrap();
        // Play to the first note: fires tap@1.0s.
        let fired = chart.advance_fired(1.0);
        assert_eq!(fired.iter().filter(|f| !f.tick && !f.hold_tail).count(), 1);
        // Forward jump to 3.0s: notes at 2.0/3.0 are jumped over — nothing
        // fires, neither now nor on subsequent calls past them.
        chart.reposition(3.0);
        assert!(chart.advance_fired(3.0).is_empty());
        assert!(chart.advance_fired(4.0).is_empty());
        // Normal firing resumes after the jump window.
        assert_eq!(chart.advance_fired(5.0).iter().filter(|f| !f.tick && !f.hold_tail).count(), 1);
        // Backward seek still replays the notes it passes over again.
        chart.advance_fired(1.5); // seek-back: cursors reset, reports nothing
        let fired = chart.advance_fired(2.0);
        assert_eq!(fired.iter().filter(|f| !f.tick && !f.hold_tail).count(), 1);
    }

    #[test]
    fn fire_events_matches_chart_scan() {
        // 120bpm beats 0..4 (0.5s/beat), 240bpm after (0.25s/beat).
        // Notes: tap @1 (0.5s), hold @3 (1.5s), flick @4 (2.0s), drag @5
        // (2.25s), fake tap @2 (1.0s — must be absent).
        let src = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [
                { "bpm": 120.0, "startTime": [0, 0, 1] },
                { "bpm": 240.0, "startTime": [4, 0, 1] }
            ],
            "judgeLineList": [
                {
                    "Name": "line0", "Texture": "line.png", "father": -1,
                    "eventLayers": [], "isCover": 0,
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [1, 0, 1], "endTime": [1, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 2, "above": 1, "startTime": [3, 0, 1], "endTime": [5, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 3, "above": 1, "startTime": [4, 0, 1], "endTime": [4, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 4, "above": 1, "startTime": [5, 0, 1], "endTime": [5, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 1, "above": 1, "startTime": [2, 0, 1], "endTime": [2, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 1, "visibleTime": 999999.0 }
                    ]
                }
            ]
        }"#;
        let rpe: RPEChart = serde_json::from_str(src).unwrap();
        let events = Chart::fire_events_from_rpe(&rpe);
        // fake @1.0s dropped.
        let expect: Vec<(f64, u8)> = vec![(0.5, 1), (1.5, 2), (2.0, 3), (2.25, 4)];
        assert_eq!(events, expect);
        // Sanity: same times as the full chart build reports.
        let mut chart = Chart::from_rpe_chart(&rpe, false).unwrap();
        let mut hits = Vec::new();
        for t in [0.5, 1.5, 2.0, 2.25] {
            hits.extend(chart.advance_fired(t).iter().filter(|f| !f.fake && !f.tick && !f.hold_tail).map(|f| f.kind));
        }
        assert_eq!(hits, vec![1, 2, 3, 4]);
    }

    #[test]
    fn fx_in_window_heads_ticks_tails_and_cap() {
        // 120bpm(0.5s/拍,半拍 tick = 0.25s):tap @1s,hold 2s→4s。
        // fake @1.5s。hold tick:2.25/2.5/.../3.75(头 2.0 + 尾 4.0)。
        let src = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "line0", "Texture": "line.png", "father": -1,
                    "eventLayers": [], "isCover": 0,
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [2, 0, 1], "endTime": [2, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 2, "above": 1, "startTime": [4, 0, 1], "endTime": [8, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 1, "above": 1, "startTime": [3, 0, 1], "endTime": [3, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 1, "visibleTime": 999999.0 }
                    ]
                }
            ]
        }"#;
        let chart = Chart::from_rpe(&src, false).unwrap();
        // 窗口 [0.8, 1.2]:只有 tap 头 @1.0。
        let fx = chart.fx_in_window(0.8, 1.2);
        assert_eq!(fx.iter().map(|f| f.t0).collect::<Vec<_>>(), vec![1.0]);
        // 窗口 [2.0, 4.0]:头 2.0 + tick 2.25..3.75(0.25s 间隔) + 尾 4.0。
        let expect_full: Vec<f64> = (0..9).map(|i| 2.0 + i as f64 * 0.25).collect();
        let fx = chart.fx_in_window(2.0, 4.0);
        assert_eq!(fx.iter().map(|f| f.t0).collect::<Vec<_>>(), expect_full);
        // 窗口 [2.9, 3.1]:只含 tick @3.0。
        let fx = chart.fx_in_window(2.9, 3.1);
        assert_eq!(fx.iter().map(|f| f.t0).collect::<Vec<_>>(), vec![3.0]);
        // 全窗口:全部触发点都在(不裁剪),fake note 排除。
        let fx = chart.fx_in_window(0.0, 4.0);
        let mut expect_all = vec![1.0];
        expect_all.extend(expect_full);
        assert_eq!(fx.iter().map(|f| f.t0).collect::<Vec<_>>(), expect_all);
        assert!(fx.iter().all(|f| f.t0 != 1.5));
    }

    #[test]
    fn line_pose_at_matches_state_at_and_tracks_time() {
        // 线:0 拍在 (0,0),4 拍移到 (100,100)(120bpm → 0..2s);旋转同区间 0°→90°。
        let src = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "line0", "Texture": "line.png", "father": -1,
                    "eventLayers": [
                        {
                            "moveXEvents": [ { "start": 0.0, "end": 100.0, "startTime": [0, 0, 1], "endTime": [4, 0, 1], "easingType": 0 } ],
                            "moveYEvents": [ { "start": 0.0, "end": 100.0, "startTime": [0, 0, 1], "endTime": [4, 0, 1], "easingType": 0 } ],
                            "rotateEvents": [ { "start": 0.0, "end": 90.0, "startTime": [0, 0, 1], "endTime": [4, 0, 1], "easingType": 0 } ]
                        },
                        null
                    ],
                    "isCover": 0,
                    "notes": []
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(&src, false).unwrap();
        // line_pose_at 与 state_at 在同一时刻的位姿一致(±1e-3)。
        for t in [0.0, 1.0, 2.0, 4.0] {
            let (fpos, frot) = {
                let frame = chart.state_at(t);
                (frame.lines[0].position, frame.lines[0].rotation)
            };
            let (pos, rot) = chart.line_pose_at(0, t);
            assert!((pos[0] - fpos[0]).abs() < 1e-3, "t={t} px");
            assert!((pos[1] - fpos[1]).abs() < 1e-3, "t={t} py");
            assert!((rot - frot).abs() < 1e-3, "t={t} rot");
        }
        // 时间不同位姿不同:移动线在 1s 与 3s 位置不应相同(moveX 值域 -1..1)。
        let (p1, _) = chart.line_pose_at(0, 1.0);
        let (p3, _) = chart.line_pose_at(0, 3.0);
        assert!((p1[0] - p3[0]).abs() > 0.01, "线位姿应随时间移动: {p1:?} vs {p3:?}");
        // 幂等:line_pose_at 不扰动 state_at 的结果。
        let frame_a = chart.state_at(2.0);
        let l_a = frame_a.lines[0].position;
        let _ = chart.line_pose_at(0, 0.0);
        let frame_b = chart.state_at(2.0);
        let l_b = frame_b.lines[0].position;
        assert_eq!(l_a, l_b);
    }

    #[test]
    fn line_pose_at_with_parent_chain_matches_state_at() {
        // 双线父链:line0 旋转 0°→90°,line1 挂到 line0(father=0)+ rotateWithFather,
        // 自身不动。子线位姿应随父线旋转继承 + 位置继承。
        let src = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "parent", "Texture": "line.png", "father": -1, "rotateWithFather": false,
                    "eventLayers": [
                        { "rotateEvents": [ { "start": 0.0, "end": 90.0, "startTime": [0, 0, 1], "endTime": [2, 0, 1], "easingType": 0 } ] },
                        null
                    ],
                    "isCover": 0, "notes": []
                },
                {
                    "Name": "child", "Texture": "line.png", "father": 0, "rotateWithFather": true,
                    "eventLayers": [ null, null ],
                    "isCover": 0, "notes": []
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(&src, false).unwrap();
        // 0.5s:父线旋转到 22.5°。子线解析旋转 = 自身(0) + 父(22.5°),位置 = 父位(0) + R(父旋转)×子偏移(0)。
        for t in [0.0, 0.5, 1.0, 2.0] {
            let (fpos, frot) = {
                let frame = chart.state_at(t);
                (frame.lines[1].position, frame.lines[1].rotation)
            };
            let (pos, rot) = chart.line_pose_at(1, t);
            assert!((pos[0] - fpos[0]).abs() < 1e-3, "t={t} px");
            assert!((pos[1] - fpos[1]).abs() < 1e-3, "t={t} py");
            assert!((rot - frot).abs() < 1e-3, "t={t} rot: {rot} vs {frot}");
        }
        // 2s 时父线旋转 90°:子线旋转应继承父线位姿的旋转(rot_with_parent)。
        let (_, rot) = chart.line_pose_at(1, 2.0);
        let (_, pro) = chart.line_pose_at(0, 2.0);
        assert!((rot - pro).abs() < 1e-3, "子线应继承父旋转: {}/{} rad", rot, pro);
        assert!(pro.to_degrees().abs() > 10.0, "父线在 2s 应有明显旋转: {}°", pro.to_degrees());
    }

    /// state_at 性能基准(非断言,打印每帧平均耗时):
    /// 大谱面 60 线 × 12 轨道 × ~200 keyframe × 每线 100 note,播放 1000 帧。
    /// 观察 release 下 state_at 是否值得做增量评估(PMCORE-72)。
    #[test]
    #[ignore = "manual benchmark"]
    fn state_at_bench_big_chart() {
        let mut src = String::from(r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 180.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": ["#);
        const LINES: usize = 60;
        for li in 0..LINES {
            if li > 0 {
                src.push(',');
            }
            let line = format!(
                r#"{{ "Name": "line{li}", "Texture": "line.png", "father": -1,
                    "eventLayers": [
                        {{ "moveXEvents": [ {{ "start": -0.5, "end": 0.5, "startTime": [0, 0, 1], "endTime": [2, 0, 1], "easingType": 0 }}, {{ "start": 0.5, "end": -0.5, "startTime": [2, 0, 1], "endTime": [4, 0, 1], "easingType": 0 }} ],
                           "moveYEvents": [ {{ "start": -0.5, "end": 0.5, "startTime": [0, 0, 1], "endTime": [2, 0, 1], "easingType": 0 }} ],
                           "rotateEvents": [ {{ "start": 0.0, "end": 90.0, "startTime": [0, 0, 1], "endTime": [2, 0, 1], "easingType": 0 }}, {{ "start": 90.0, "end": -90.0, "startTime": [2, 0, 1], "endTime": [4, 0, 1], "easingType": 0 }} ],
                           "alphaEvents": [ {{ "start": 255.0, "end": 255.0, "startTime": [0, 0, 1], "endTime": [6, 0, 1], "easingType": 0 }} ],
                           "speedEvents": [ {{ "start": 1.0, "end": 1.0, "startTime": [0, 0, 1], "endTime": [6, 0, 1], "easingType": 0 }} ]
                        }},
                        null
                    ],
                    "isCover": 0,
                    "notes": [
                        {{ "type": 1, "above": 1, "startTime": [0, 0, 1], "endTime": [0, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }},
                        {{ "type": 2, "above": 1, "startTime": [1, 0, 1], "endTime": [4, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }}
                    ]
                }}"#
            );
            src.push_str(&line);
        }
        src.push_str("]}");
        let start_build = std::time::Instant::now();
        let mut chart = Chart::from_rpe(&src, false).unwrap();
        eprintln!("build: {:?}", start_build.elapsed());
        const FRAMES: usize = 1000;
        let start = std::time::Instant::now();
        let mut acc = 0usize;
        for f in 0..FRAMES {
            let t = f as f64 * 0.016;
            let frame = chart.state_at(t);
            acc += frame.lines.len();
        }
        let el = start.elapsed();
        eprintln!(
            "state_at {} frames ({} lines): {:.2}ms total, {:.2}us/frame, acc={}",
            FRAMES, LINES, el.as_millis(), el.as_micros() as f64 / FRAMES as f64, acc
        );
        // 倒退 seek 场景(游标回退循环最坏情况):每 10 帧回跳一次。
        chart.reposition(0.0);
        let start = std::time::Instant::now();
        for f in 0..FRAMES {
            let t = if f % 10 == 0 { 0.0 } else { (f % 10) as f64 * 0.016 };
            let _ = chart.state_at(t);
        }
        let el = start.elapsed();
        eprintln!(
            "state_at seek-back {} frames: {:.2}ms total, {:.2}us/frame",
            FRAMES, el.as_millis(), el.as_micros() as f64 / FRAMES as f64
        );
    }
}
