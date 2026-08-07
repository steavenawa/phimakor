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
#[allow(dead_code)] // line/kind/x: engine::ChartSession 与 python bindings 消费,editor 只用 tick/fake/hold_tail
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
#[allow(dead_code)] // time/fake: python bindings(PyNoteState)消费,editor 渲染路径不用
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
    /// [PMCORE-72] Reused buffers for parent-transform propagation in
    /// `state_at` (resolved rotations / own transforms / chain walks), so
    /// steady forward playback performs zero heap allocations per frame.
    /// `scratch_own_rot`/`scratch_own` hold the line's OWN transform from the
    /// track loop (retained across frames when the track is frozen) — the
    /// parent block reads them as the composition starting point instead of
    /// the output slots, which hold the RESOLVED value.
    scratch_rot: Vec<f32>,
    scratch_own_rot: Vec<f32>,
    scratch_own: Vec<[f32; 2]>,
    scratch_chain: Vec<usize>,
    scratch_visited: Vec<usize>,
    /// [PMCORE-72] Forces full track re-evaluation on the next `state_at`:
    /// set by `reposition` (a backward jump there produces no seek-back
    /// signal for `state_at`) and initially true so the first frame
    /// populates every output value before the frozen-track fast path may
    /// skip re-interpolation.
    eval_dirty: bool,
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

/// 一个命中特效触发点:谱面时间 `t0` + 线索引 + note 的 x/y(相对线中心,
/// 画布单位;y 已含 above 符号与 y_offset 归一化)。渲染端用线变换把
/// x/y 换算成画布位置。
#[derive(Clone, Copy, Debug)]
pub struct FxTrigger {
    pub t0: f64,
    pub line: usize,
    pub x: f32,
    /// note 的 y 偏移(above? + : -)×(y_offset×2/900×speed),与
    /// state_at 的 relative[1] 同语义(不含线高差 base 项)。
    pub y: f32,
}

/// 统一触发事件表条目(构建时生成):`t` = 谱面时钟秒。
/// `kind`:1/2/3/4 = note 头(tap/hold/flick/drag),5 = hold 半拍 tick,
/// 6 = hold 尾。fake note 不产生事件。
#[derive(Clone, Copy, Debug)]
pub struct TriggerEvent {
    pub t: f64,
    pub line: usize,
    pub x: f32,
    /// 同 [`FxTrigger::y`]:above 符号 × 归一化 y_offset。
    pub y: f32,
    pub kind: u8,
}

/// hold tick 与 hold 尾在事件表里的内部 kind 标记。
pub const TRIGGER_TICK: u8 = 5;
pub const TRIGGER_TAIL: u8 = 6;

/// 统一触发事件表构建(单一口径):非 fake note 头/尾/半拍 tick,按 t 排序。
/// [`from_rpe`](Chart::from_rpe) 用它填充 `Chart.triggers`;音频预加载路径
/// ([`Chart::fire_events_from_rpe`]) 也走它再过滤头事件,保证与
/// [`Chart::fire_events`] 逐项一致——三套触发判定(音频/fx/fired)共用同一
/// 份构建逻辑。beat→秒换算用调用方已建的 `r`(与 note 解析同一 `BpmList`),
/// 半拍 tick 反向解算(秒→拍→半拍→秒),tick 严格早于 hold 尾(恰落在
/// end_time 的半拍不产生 tick)。
fn build_triggers(rpe: &RPEChart, r: &mut BpmList) -> Vec<TriggerEvent> {
    let mut trig_bpm = BpmList::new(r.elements().iter().map(|&(b, _, v)| (b, v)).collect());
    let mut triggers: Vec<TriggerEvent> = Vec::new();
    for (line_idx, line) in rpe.judge_line_list.iter().enumerate() {
        for note in line.notes.iter().flatten() {
            if note.is_fake != 0 {
                continue;
            }
            let t = r.time(&note.start_time);
            let x = note.position_x / (RPE_WIDTH / 2.);
            // y:above 符号 × y_offset 归一化(与 parse_notes/state_at 同口径:
            // y_offset×2/RPE_HEIGHT×speed,below note 镜像取负)。fx 落点必须
            // 含这一项,否则带 y 偏移的 note 特效落在线上(用户实测:没有
            // 判断 note 是 up 还是 down)。
            let y = if note.above == 1 { 1.0 } else { -1.0 } * note.y_offset * 2.0 / RPE_HEIGHT * note.speed;
            triggers.push(TriggerEvent { t, line: line_idx, x, y, kind: note.kind });
            if note.kind == 2 {
                let end_time = r.time(&note.end_time);
                if end_time > t {
                    let b_lo = trig_bpm.beat(t);
                    let b_end = trig_bpm.beat(end_time);
                    let mut tb = (b_lo * 2.0).floor() / 2.0 + 0.5;
                    while tb < b_end {
                        let tt = trig_bpm.time_beats(tb);
                        if tt < end_time {
                            triggers.push(TriggerEvent { t: tt, line: line_idx, x, y, kind: TRIGGER_TICK });
                        }
                        tb += 0.5;
                    }
                    triggers.push(TriggerEvent { t: end_time, line: line_idx, x, y, kind: TRIGGER_TAIL });
                }
            }
        }
    }
    triggers.sort_by(|a, b| a.t.total_cmp(&b.t));
    triggers
}

impl Chart {
    /// Fast-forward the fired-note cursors to `time` without reporting the
    /// notes that were jumped over. The editor calls this after a **seek**
    /// (forward and backward alike): notes in a skipped window must neither
    /// fire again (double count on top of `hits_before`) nor spawn hit FX /
    /// hitsounds (a dense jump would flood the audio mixer). On a backward
    /// seek the cursor sync makes the replay-on-rewind start **exactly** at
    /// the seek target (the next `state_at` sees `time == last_state_time`,
    /// not a seek-back), so the `(target, target+δ]` predict-offset window is
    /// neither lost nor re-fired. Combo precision: locked by the invariant
    /// tests in `mod tests` (`combo_invariant_*`, PMCORE combo 精度).
    pub fn reposition(&mut self, time: f64) {
        if time < self.last_state_time {
            // 后向 seek:state_at 因 last_state_time 被吃而看不到 seek-back
            // (见下方注释),可见性游标必须在这里自己重置——否则过点即消的
            // note 后退时永远不显示(PMCORE 回归:combo 修复 B 引入)。
            for line in self.lines.iter_mut() {
                line.cursor = 0;
            }
        }
        self.last_state_time = time;
        // [PMCORE-72] `time` may jump backward here (e.g. reposition(0.0)
        // after playback) without a seek-back signal for `state_at`, which
        // would let the frozen-track fast path reuse stale values. Force a
        // full re-evaluation on the next `state_at`.
        self.eval_dirty = true;
    }

    /// Reads `info.json` in `dir` (falling back to an RPE-export `info.txt`
    /// when absent), then the chart file it names. Supports RPE, PEC, and PGR (官谱) formats.
    #[allow(dead_code)] // embedding API: python bindings / engine / bench bins 使用
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

    /// Compressed hitsound schedule derived from the unified trigger table:
    /// `(hit time in seconds on the chart clock, note kind)` for every
    /// non-fake note head, sorted by time. Head kinds pass through unchanged
    /// (1 tap, 2 hold, 3 flick, 4 drag — all fire a hitsound at their hit
    /// time); hold tick (5) / hold tail (6) 与 fake note 不产生音频事件。
    /// 过滤条件 kind∈1..=4 必须保留:tick/tail 混在统一表里,不过滤会把
    /// hold 中部/结尾误触发为 hitsound。
    pub fn fire_events(&self) -> Vec<(f64, u8)> {
        self.triggers
            .iter()
            .filter(|e| (1..=4).contains(&e.kind))
            .map(|e| (e.t, e.kind))
            .collect()
    }

    /// 音频触发线程在 Chart 构建前(后台加载线程)使用的入口:与
    /// [`Chart::fire_events`] 同一构建口径——共用 [`build_triggers`] 统一
    /// 触发事件表(唯一扫描点),再过滤头事件(kind∈1..=4)。音频调度路径
    /// 不再单独遍历 judge_line_list/note 列表。
    pub fn fire_events_from_rpe(rpe: &RPEChart) -> Vec<(f64, u8)> {
        let mut r = BpmList::new(rpe.bpm_list.iter().map(|it| (it.start_time.beats(), it.bpm)).collect());
        build_triggers(rpe, &mut r)
            .into_iter()
            .filter(|e| (1..=4).contains(&e.kind))
            .map(|e| (e.t, e.kind))
            .collect()
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
        // 统一触发事件表(单一口径,见 [`build_triggers`]):音频预加载路径
        // (fire_events_from_rpe) 也走同一 builder,保证三套触发判定一致。
        // 需在 rpe.judge_line_list 被消费(下方 into_iter)前借 rpe 构建。
        let triggers = build_triggers(&rpe, &mut r);

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
        let line_count = lines.len();
        Ok(Chart {
            offset,
            duration: max_time,
            lines,
            bpm_list: r,
            last_state_time: 0.,
            frame: FrameState { time: 0., lines: frame_lines, fired: Vec::new() },
            visible_scratch: Vec::new(),
            triggers,
            scratch_rot: vec![0.0; line_count],
            scratch_own_rot: vec![0.0; line_count],
            scratch_own: vec![[0.0; 2]; line_count],
            scratch_chain: Vec::with_capacity(line_count),
            scratch_visited: Vec::with_capacity(line_count),
            eval_dirty: true,
        })
    }

    /// Fired-note detection: returns trigger events (heads / hold ticks /
    /// hold tails) crossed since the previous call, backed by the unified
    /// [`TriggerEvent`] table (no per-frame scanning, no cursor state).
    /// Backward seeks (`time < previous call`) report nothing this call;
    /// the next call replays events after the seek target (same semantics
    /// as the old cursor-based implementation).
    ///
    /// 口径与 `hits_before` 互补(窗口 `(last, time]`,`<=` 边界),组合起来
    /// 即 combo 不变量 `累计 fired == hits_before(chart_time)`——由 `mod tests`
    /// 的 `combo_invariant_*` 系列逐帧锁定(暂停 predict 归零回跳、seek、
    /// A-B 回跳均不双计/不漏计)。
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
        // [PMCORE-72] Full track re-evaluation whenever time may have moved
        // out of band: backward seek (track cursors must walk back) or
        // `reposition`. In steady forward playback frozen tracks skip it.
        let force_tracks = seek_back || self.eval_dirty;
        self.eval_dirty = false;
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
            scratch_rot,
            scratch_own_rot,
            scratch_own,
            scratch_chain,
            scratch_visited,
            ..
        } = self;
        // Own-transform caches are indexed by line; grow them once if the
        // line count ever exceeded the build-time sizing (no-op normally).
        scratch_own_rot.resize(lines.len(), 0.0);
        scratch_own.resize(lines.len(), [0.0; 2]);
        for (li, (line, out)) in lines.iter_mut().zip(frame.lines.iter_mut()).enumerate() {
            // [PMCORE-72] Frozen-track fast path: a track whose value no
            // longer depends on time (no keyframes, or the cursor already
            // past the last one, across the whole chain) keeps the previous
            // frame's output — `out` persists across calls, so skipping the
            // set_time/now_opt pair is value-exact. `force_tracks` (seek /
            // reposition) recomputes every track.
            //
            // rotation/position write the OWN transform into the scratch
            // caches (retained when frozen); the parent block below composes
            // from those and writes the RESOLVED value to `out`.
            let full = force_tracks;
            if full || !line.alpha.frozen() {
                line.alpha.set_time(time);
                let raw_alpha = line.alpha.now_opt().unwrap_or(1.0);
                out.alpha = raw_alpha.max(0.);
            }
            if full || !line.pe_alpha.frozen() {
                line.pe_alpha.set_time(time);
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
            }
            if full || !line.rotation.frozen() {
                line.rotation.set_time(time);
                scratch_own_rot[li] = line.rotation.now().to_radians();
            }
            if full || !(line.move_x.frozen() && line.move_y.frozen()) {
                line.move_x.set_time(time);
                line.move_y.set_time(time);
                scratch_own[li] = [line.move_x.now(), line.move_y.now()];
            }
            if full || !(line.scale_x.frozen() && line.scale_y.frozen()) {
                line.scale_x.set_time(time);
                line.scale_y.set_time(time);
                out.scale = [line.scale_x.now_opt().unwrap_or(1.0), line.scale_y.now_opt().unwrap_or(1.0)];
            }
            if full || !line.color.frozen() {
                line.color.set_time(time);
                out.color = line.color.now_opt().map(|c| [c.r, c.g, c.b, c.a]).unwrap_or([1.; 4]);
            }
            // height feeds the note loop below via `line.height.now()`; when
            // frozen that read already returns the constant value.
            if full || !line.height.frozen() {
                line.height.set_time(time);
            }
            // [E] CtrlObject evaluation (tracks are usually empty → frozen).
            // now_opt below stays unconditional: on frozen/empty tracks it is
            // a constant read, not an interpolation.
            if full || !line.ctrl_pos_x.frozen() { line.ctrl_pos_x.set_time(time); }
            if full || !line.ctrl_pos_y.frozen() { line.ctrl_pos_y.set_time(time); }
            if full || !line.ctrl_size_x.frozen() { line.ctrl_size_x.set_time(time); }
            if full || !line.ctrl_size_y.frozen() { line.ctrl_size_y.set_time(time); }
            if full || !line.ctrl_alpha.frozen() { line.ctrl_alpha.set_time(time); }
            if full || !line.ctrl_y.frozen() { line.ctrl_y.set_time(time); }
            if full || !line.incline.frozen() { line.incline.set_time(time); }
            // phira: CtrlObject values are multipliers with default 1.0.
            out.ctrl_pos_x = line.ctrl_pos_x.now_opt().unwrap_or(1.0);
            out.ctrl_pos_y = line.ctrl_pos_y.now_opt().unwrap_or(1.0);
            out.ctrl_size_x = line.ctrl_size_x.now_opt().unwrap_or(1.0);
            out.ctrl_size_y = line.ctrl_size_y.now_opt().unwrap_or(1.0);
            out.ctrl_alpha = line.ctrl_alpha.now_opt().unwrap_or(1.0);
            out.ctrl_y = line.ctrl_y.now_opt().unwrap_or(1.0);
            let incline_deg = line.incline.now_opt().unwrap_or(0.0);
            out.incline_sin = incline_deg.to_radians().sin();
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
            // [PMCORE-72] All temporaries are Chart-owned scratch buffers
            // (resized once, reused every frame — zero steady-state allocs);
            // own transforms come from the track loop's scratch caches.
            let n = frame.lines.len();
            scratch_rot.resize(n, 0.0);
            scratch_own_rot.resize(n, 0.0);
            scratch_own.resize(n, [0.0; 2]);
            for i in 0..n {
                let mut rot = scratch_own_rot[i];
                let mut node = i;
                let mut cur = frame.lines[i].parent;
                scratch_chain.clear();
                while let Some(pidx) = cur {
                    if pidx >= n || scratch_chain.contains(&pidx) || pidx == i {
                        break;
                    }
                    scratch_chain.push(pidx);
                    if !frame.lines[node].rot_with_parent {
                        break;
                    }
                    // Parent's OWN rotation (its output slot holds the
                    // previous frame's RESOLVED value — the copy-out to
                    // `out` happens after this loop).
                    rot += scratch_own_rot[pidx];
                    node = pidx;
                    cur = frame.lines[pidx].parent;
                }
                scratch_rot[i] = rot;
            }
            for i in 0..n {
                frame.lines[i].rotation = scratch_rot[i];
            }
            // Then positions (phira fetch_pos, recursive): walk the parent
            // chain from root to leaf, composing R(parent_rot) × own_offset.
            // `scratch_own` already holds each line's OWN position (written
            // by the track loop above, retained when the tracks are frozen).
            for i in 0..n {
                let mut acc = scratch_own[i];
                let mut cur = frame.lines[i].parent;
                scratch_chain.clear();
                scratch_visited.clear();
                while let Some(pidx) = cur {
                    if pidx >= n || scratch_visited.contains(&pidx) || pidx == i {
                        break;
                    }
                    scratch_visited.push(pidx);
                    scratch_chain.push(pidx);
                    cur = frame.lines[pidx].parent;
                }
                // Root first (last pushed) → direct parent (first pushed).
                for &pidx in scratch_chain.iter().rev() {
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
            .map(|e| FxTrigger { t0: e.t, line: e.line, x: e.x, y: e.y })
            .collect()
    }

    /// 求线在 `time` 时刻的位姿 `(position, rotation 弧度, [scale_x, scale_y])`,
    /// 含 parent 继承(与 state_at 的 fetch_rot/fetch_pos 等价)。hit-fx 用:
    /// 特效位置在 **触发瞬间 t0** 的线变换下计算,不绑定当前帧线状态——
    /// 线之后移动/旋转时,已爆散的粒子留在原地。
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
            line.scale_x.set_time(time);
            line.scale_y.set_time(time);
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
        // Position: root-first composition (same as state_at)。
        let mut acc = [self.lines[line_idx].move_x.now(), self.lines[line_idx].move_y.now()];
        // chain = [self, parent, ..., root];rev 后 = [root, ..., parent, self]。
        // 从 root 到 parent 组合,**跳过 self**(否则用自己的 own 组合自己,
        // 父线平移/旋转全部丢失——父线有 move 事件时 fx 落在未组合坐标)。
        // rev 枚举自带链内下标:第 k 个 rev 元素 = chain[len-1-k],免去每步
        // 线性查找(深链 O(depth²) → O(depth),零分配)。
        for (k, &pidx) in chain.iter().rev().enumerate() {
            if pidx == line_idx {
                continue;
            }
            let ci = chain.len() - 1 - k;
            let pr = rot_resolved[ci];
            let (cos, sin) = (pr.cos(), pr.sin());
            let (lx, ly) = (acc[0], acc[1]);
            acc[0] = self.lines[pidx].move_x.now() + cos * lx - sin * ly;
            acc[1] = self.lines[pidx].move_y.now() + sin * lx + cos * ly;
        }
(acc, rot_resolved[0])
    }

    /// 批量求多个 fx 触发点在各自 **t0 时刻** 的线位姿(PMCORE-79 聚合)。
    ///
    /// 按 `(line, t0)` 去重:同一键(和弦/多 note 同拍)只做一次链 set_time
    /// 与位姿组合,结果按输入顺序映射回每个触发点(输出与 `triggers` 等长)。
    /// 密集段落一帧上百触发点时消掉重复的链遍历与四个临时 Vec 分配;每个键
    /// 的求值就是一次 [`line_pose_at`],位姿语义逐像素一致。幂等性同
    /// `line_pose_at`:只对事件轨道 set_time,不扰动 notes 可见性游标或
    /// fired 状态。
    pub fn fx_poses(&mut self, triggers: &[FxTrigger]) -> Vec<([f32; 2], f32)> {
        // ponytail: 线性扫键去重(键数 ≤ 窗口内触发点数),无需 f64 位哈希;
        // 单帧触发点上万时才值得换 HashMap,真到那天再换。
        let mut keys: Vec<(usize, f64)> = Vec::new();
        let mut poses: Vec<([f32; 2], f32)> = Vec::with_capacity(triggers.len());
        for tr in triggers {
            match keys.iter().position(|&k| k == (tr.line, tr.t0)) {
                Some(i) => poses.push(poses[i]),
                None => {
                    keys.push((tr.line, tr.t0));
                    poses.push(self.line_pose_at(tr.line, tr.t0));
                }
            }
        }
        poses
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
    #[allow(dead_code)] // python bindings / measure bin 使用
    pub fn note_count(&self) -> usize {
        self.lines.iter().map(|line| line.notes.iter().filter(|note| !note.fake).count()).sum()
    }

    /// Combo 口径:非 fake 音符 note.time <= `time` 各计 1(hold 头),
    /// 外加非 fake hold 中 end_time <= `time` 各再计 1(hold 尾)。
    /// O(n) full scan — intended for seek handling only.
    ///
    /// Boundary: `<=` is the exact complement of the fired condition
    /// (`last < note.time <= time`) — after a seek to `time`, notes at exactly
    /// `time` can never fire again (last = time), so they must be counted
    /// here or they would be lost from combo/score restoration.
    ///
    /// 已由不变量测试锁定:`combo == hits_before(chart_time)` 在正常播放
    /// (含 predict 注入)、暂停/恢复、前后向 seek、A-B 循环回跳、边界精确
    /// 停留场景下逐帧成立(`mod tests` 的 `combo_invariant_*` 系列)。
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

    /// Convert beats to chart seconds (inverse of [`time_to_beat`]; used by
    /// the A-B loop seek conversion, PMCORE-22).
    pub fn beat_to_time(&mut self, beats: f64) -> f64 {
        self.bpm_list.time_beats(beats)
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
// Chart content validation (PMCORE-21)
// ---------------------------------------------------------------------------

/// 校验检出的一类问题(按类别区分;测试/UI 按 `kind` 机器匹配)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueKind {
    /// 音符/事件起始拍为负。
    NegativeBeat,
    /// hold(end_time) < start_time。
    HoldEndBeforeStart,
    /// 事件 start_time > end_time(零长事件合法,不报)。
    EventStartAfterEnd,
    /// |position_x| > 675。
    PositionXOutOfRange,
    /// alpha > 255。
    AlphaOutOfRange,
    /// BPM ≤ 0。
    BpmNonPositive,
    /// 同线同拍同 x 重复音符(叠键仅告警;fake 不参与)。
    DuplicateNote,
}

/// 谱面内容校验问题(加载后收集)。内容级问题默认放行+告警,不阻断加载;
/// 结构级问题(解析失败)由解析层 `Err` 硬失败,不进入此列表。
#[derive(Clone, Debug, PartialEq)]
pub struct ChartIssue {
    pub kind: IssueKind,
    /// 判定线索引(`judge_line_list` 下标);None = 全局问题(BPM 等)。
    pub line: Option<usize>,
    /// 音符/事件在所属列表内的下标;None = 线级问题。
    pub index: Option<usize>,
    /// 拍号定位:问题项的起始拍(反序问题时为 end 拍)。
    pub beat: Option<f64>,
    /// 人类可读描述(含定位,可直接展示)。
    pub message: String,
}

/// 事件时间合法性(负拍 / start>end),所有事件类型共用。零长事件
/// (start == end,如 PEC speed 步进,chart_format.rs:397-413)合法,不报。
fn check_event_times<T>(
    issues: &mut Vec<ChartIssue>,
    li: usize,
    path: &str,
    ei: usize,
    ev: &RPEEvent<T>,
) {
    let s = ev.start_time.beats();
    let e = ev.end_time.beats();
    if s < 0.0 {
        issues.push(ChartIssue {
            kind: IssueKind::NegativeBeat,
            line: Some(li),
            index: Some(ei),
            beat: Some(s),
            message: format!("line {li} {path}[{ei}] start beat {s} < 0"),
        });
    }
    if s > e {
        issues.push(ChartIssue {
            kind: IssueKind::EventStartAfterEnd,
            line: Some(li),
            index: Some(ei),
            beat: Some(e),
            message: format!("line {li} {path}[{ei}] start {s} > end {e}"),
        });
    }
}

impl RPEChart {
    /// 内容级校验(纯函数,不改任何状态):返回全部问题列表(含定位字段:
    /// 线索引/音符或事件索引/拍号)。
    ///
    /// 复杂度 O(n log n)(重复检测按 (拍, x) 排序后线性扫描),万级音符
    /// <100ms。结构级问题(解析失败)由 parse 层 `Err` 承担,不在返回值里;
    /// 内容级问题默认放行+告警,不阻断加载。
    pub fn validate(&self) -> Vec<ChartIssue> {
        let mut issues = Vec::new();

        // ── BPM(全局,无线索引)──
        for (bi, bpm) in self.bpm_list.iter().enumerate() {
            if bpm.bpm <= 0.0 {
                issues.push(ChartIssue {
                    kind: IssueKind::BpmNonPositive,
                    line: None,
                    index: Some(bi),
                    beat: Some(bpm.start_time.beats()),
                    message: format!(
                        "BPMList[{bi}] bpm={} ≤ 0 (start beat {})",
                        bpm.bpm,
                        bpm.start_time.beats()
                    ),
                });
            }
        }

        const MAX_X: f32 = RPE_WIDTH / 2.0; // ±675
        for (li, line) in self.judge_line_list.iter().enumerate() {
            // ── 音符 ──
            if let Some(notes) = &line.notes {
                for (ni, n) in notes.iter().enumerate() {
                    let s = n.start_time.beats();
                    let e = n.end_time.beats();
                    if s < 0.0 {
                        issues.push(ChartIssue {
                            kind: IssueKind::NegativeBeat,
                            line: Some(li),
                            index: Some(ni),
                            beat: Some(s),
                            message: format!("line {li} notes[{ni}] start beat {s} < 0"),
                        });
                    }
                    // hold(kind=2)的 end_time 语义上必须 ≥ start_time。
                    if n.kind == 2 && e < s {
                        issues.push(ChartIssue {
                            kind: IssueKind::HoldEndBeforeStart,
                            line: Some(li),
                            index: Some(ni),
                            beat: Some(e),
                            message: format!("line {li} notes[{ni}] hold end {e} < start {s}"),
                        });
                    }
                    if n.position_x.abs() > MAX_X {
                        issues.push(ChartIssue {
                            kind: IssueKind::PositionXOutOfRange,
                            line: Some(li),
                            index: Some(ni),
                            beat: Some(s),
                            message: format!(
                                "line {li} notes[{ni}] position_x {} out of ±{MAX_X}",
                                n.position_x
                            ),
                        });
                    }
                    if n.alpha > 255 {
                        issues.push(ChartIssue {
                            kind: IssueKind::AlphaOutOfRange,
                            line: Some(li),
                            index: Some(ni),
                            beat: Some(s),
                            message: format!("line {li} notes[{ni}] alpha {} > 255", n.alpha),
                        });
                    }
                }
                // 重复检测:同线同拍同 x(排除 fake——fake 常与实体音符叠放)。
                // 排序后线性扫描,O(n log n),万级音符 <100ms。
                let mut order: Vec<usize> = (0..notes.len()).collect();
                order.sort_by(|&a, &b| {
                    notes[a]
                        .start_time
                        .beats()
                        .total_cmp(&notes[b].start_time.beats())
                        .then_with(|| notes[a].position_x.total_cmp(&notes[b].position_x))
                });
                let same_pos = |a: usize, b: usize| {
                    notes[a].is_fake == 0
                        && notes[b].is_fake == 0
                        && notes[a].start_time.beats() == notes[b].start_time.beats()
                        && notes[a].position_x == notes[b].position_x
                };
                let mut i = 0;
                while i < order.len() {
                    let mut j = i + 1;
                    while j < order.len() && same_pos(order[i], order[j]) {
                        j += 1;
                    }
                    // 每组第 2..n 个报重复(每组只报一次,不重复计数)。
                    for &k in &order[i + 1..j] {
                        issues.push(ChartIssue {
                            kind: IssueKind::DuplicateNote,
                            line: Some(li),
                            index: Some(k),
                            beat: Some(notes[k].start_time.beats()),
                            message: format!(
                                "line {li} notes[{k}] duplicate of notes[{}] (beat {}, x {})",
                                order[i],
                                notes[k].start_time.beats(),
                                notes[k].position_x
                            ),
                        });
                    }
                    i = j;
                }
            }

            // ── 事件:5 个基础层 + extended 7 类。只查拍号合法性
            // (负拍 / start>end);零长事件(如 PEC speed 步进)合法,不报。──
            macro_rules! check_ev {
                ($path:expr, $evs:expr) => {
                    if let Some(evs) = $evs {
                        for (ei, ev) in evs.iter().enumerate() {
                            check_event_times(&mut issues, li, $path, ei, ev);
                        }
                    }
                };
            }
            for (layer_i, layer) in line.event_layers.iter().flatten().enumerate() {
                let lp = format!("layers[{layer_i}].");
                check_ev!(&format!("{lp}alphaEvents"), layer.alpha_events.as_ref());
                check_ev!(&format!("{lp}moveXEvents"), layer.move_x_events.as_ref());
                check_ev!(&format!("{lp}moveYEvents"), layer.move_y_events.as_ref());
                check_ev!(&format!("{lp}rotateEvents"), layer.rotate_events.as_ref());
                check_ev!(&format!("{lp}speedEvents"), layer.speed_events.as_ref());
                // alpha 事件值域 0..255(值本身,与音符 alpha 同口径)。
                if let Some(evs) = layer.alpha_events.as_ref() {
                    for (ei, ev) in evs.iter().enumerate() {
                        if ev.start > 255.0 || ev.end > 255.0 {
                            issues.push(ChartIssue {
                                kind: IssueKind::AlphaOutOfRange,
                                line: Some(li),
                                index: Some(ei),
                                beat: Some(ev.start_time.beats()),
                                message: format!(
                                    "line {li} layers[{layer_i}].alphaEvents[{ei}] value {}/{} > 255",
                                    ev.start, ev.end
                                ),
                            });
                        }
                    }
                }
            }
            if let Some(ext) = &line.extended {
                check_ev!("extended.colorEvents", ext.color_events.as_ref());
                check_ev!("extended.textEvents", ext.text_events.as_ref());
                check_ev!("extended.scaleXEvents", ext.scale_x_events.as_ref());
                check_ev!("extended.scaleYEvents", ext.scale_y_events.as_ref());
                check_ev!("extended.inclineEvents", ext.incline_events.as_ref());
                check_ev!("extended.paintEvents", ext.paint_events.as_ref());
                check_ev!("extended.gifEvents", ext.gif_events.as_ref());
            }
        }
        issues
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
    fn frozen_tracks_recompute_on_seek_and_reposition() {
        // [PMCORE-72] A settled track (cursor past its last keyframe) keeps
        // its output during steady forward playback; a backward seek AND a
        // backward `reposition` (which produces no seek-back signal for
        // `state_at`) must both force full re-evaluation back to the seek
        // target — otherwise the frozen fast path would reuse stale values.
        let chart = r#"{
            "META": { "offset": 0 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "a", "Texture": "line.png", "father": -1,
                    "eventLayers": [
                        { "rotateEvents": [ { "start": 0.0, "end": 90.0, "startTime": [0, 0, 1], "endTime": [2, 0, 1], "easingType": 0 } ] }
                    ],
                    "notes": [], "isCover": 1
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(chart, false).unwrap();
        // Event 0→90°(解析时乘 −1,存储 0→−90°)over 0..2 beats = 0..1s at
        // 120 BPM(0.5s/拍)。midpoint t=0.5s → −45°。
        let f = chart.state_at(0.5);
        assert!((f.lines[0].rotation.to_degrees() + 45.0).abs() < 1e-3, "t=0.5s: {}", f.lines[0].rotation.to_degrees());
        // past the end (t>1s): track frozen at −90°
        let f = chart.state_at(2.0);
        assert!((f.lines[0].rotation.to_degrees() + 90.0).abs() < 1e-3, "t=2s: {}", f.lines[0].rotation.to_degrees());
        // backward seek (time < previous call): must recompute → −45° again
        let f = chart.state_at(0.5);
        assert!(
            (f.lines[0].rotation.to_degrees() + 45.0).abs() < 1e-3,
            "backward seek must recompute, got {}",
            f.lines[0].rotation.to_degrees()
        );
        // settle again, then backward reposition(0.0): time == last_state_time
        // so no seek-back signal — eval_dirty must force recompute → 0°
        chart.state_at(2.0);
        chart.reposition(0.0);
        let f = chart.state_at(0.0);
        assert!(
            f.lines[0].rotation.to_degrees().abs() < 1e-3,
            "backward reposition must recompute, got {}",
            f.lines[0].rotation.to_degrees()
        );
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
    fn reposition_backward_restores_visibility() {
        // 回归:combo 精度修复让后向 seek 也走 reposition,而 reposition 把
        // last_state_time 设为目标、state_at 看不到 seek-back → 可见性游标
        // 永不重置 → 过点即消的 note 后退时永久不显示(用户实测:退回去
        // 显示不出来但 hit-fx 正常)。修复:reposition 在后向时自重置游标。
        // 120bpm: taps at beats 2, 4, 6 → 1.0s, 2.0s, 3.0s。
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
                        { "type": 1, "above": 1, "startTime": [6, 0, 1], "endTime": [6, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ]
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(src, false).unwrap();
        // 播放过全部 note:过点即消,frame 里不可见。
        chart.state_at(4.0);
        assert!(chart.frame.lines[0].notes.is_empty(), "passed notes are culled");
        // 后向 seek(main.rs hard_seek 同路径:reposition + 下一帧 state_at)。
        // 回归判据:seek 前被消掉的 note 必须重新可见(修复前游标不重置,
        // 退回去永远 0 条——用户实测"退回去显示不出来但 hit-fx 正常")。
        chart.reposition(0.5);
        {
            let frame = chart.state_at(0.5);
            // 接近阶段:三条 note 都未命中,应全部重新可见。
            assert_eq!(frame.lines[0].notes.len(), 3, "notes visible again after backward seek");
        }
        {
            let frame = chart.state_at(1.5);
            assert_eq!(frame.lines[0].notes.len(), 2, "note @1.0s consumed as playback resumes");
        }
        {
            let frame = chart.state_at(4.0);
            assert_eq!(frame.lines[0].notes.len(), 0, "culling resumes past the seek target");
        }
        // combo 口径不受影响:reposition 后 hits_before 与可见性一致。
        assert_eq!(chart.hits_before(0.5), 0);
        chart.reposition(4.0);
        assert_eq!(chart.hits_before(4.0), 3);
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
        let chart = Chart::from_rpe_chart(&rpe, false).unwrap();
        // 统一触发事件表派生:仅非 fake 头、kind∈1..=4、按 t 升序。
        // fake @1.0s dropped;hold 只保留头 @1.5s(tick/tail 无音频事件)。
        let events = chart.fire_events();
        let expect: Vec<(f64, u8)> = vec![(0.5, 1), (1.5, 2), (2.0, 3), (2.25, 4)];
        assert_eq!(events, expect);
        // Sanity: same times as the full chart build reports.
        let mut chart = chart;
        let mut hits = Vec::new();
        for t in [0.5, 1.5, 2.0, 2.25] {
            hits.extend(chart.advance_fired(t).iter().filter(|f| !f.fake && !f.tick && !f.hold_tail).map(|f| f.kind));
        }
        assert_eq!(hits, vec![1, 2, 3, 4]);
    }

    #[test]
    fn fire_events_unified_table_matches_old_scan() {
        // 统一触发表派生(fire_events)与旧 fire_events_from_rpe 独立扫描
        // 逐项相等(时间+kind)。谱面含 hold(产生 tick/tail)、fake、变速、
        // 跨线 —— 任一过滤条件(tick/tail/fake)写错都会在此暴露。
        // 120bpm beats 0..4(0.5s/beat),240bpm after(0.25s/beat):
        // tap @1 (0.5s), hold @3→6 (1.5s→2.5s), flick @4 (2.0s),
        // fake tap @2 (1.0s), drag @8 (3.0s), 第二线 tap @9 (3.25s)。
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
                        { "type": 2, "above": 1, "startTime": [3, 0, 1], "endTime": [6, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 3, "above": 1, "startTime": [4, 0, 1], "endTime": [4, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 4, "above": 1, "startTime": [8, 0, 1], "endTime": [8, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 1, "above": 1, "startTime": [2, 0, 1], "endTime": [2, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 1, "visibleTime": 999999.0 }
                    ]
                },
                {
                    "Name": "line1", "Texture": "line.png", "father": -1,
                    "eventLayers": [], "isCover": 0,
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [9, 0, 1], "endTime": [9, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ]
                }
            ]
        }"#;
        // 旧 fire_events_from_rpe 的独立扫描逻辑(测试内参考实现,保持原样)。
        fn old_scan(rpe: &RPEChart) -> Vec<(f64, u8)> {
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
        let rpe: RPEChart = serde_json::from_str(src).unwrap();
        let chart = Chart::from_rpe_chart(&rpe, false).unwrap();
        assert_eq!(chart.fire_events(), old_scan(&rpe));
        // 逐项断言:头事件全部命中,时间/kind/顺序正确(hold tick 与 flick
        // 同刻 2.0s —— kind 过滤必须保留 flick 头、丢掉 tick)。
        assert_eq!(chart.fire_events(), vec![(0.5, 1), (1.5, 2), (2.0, 3), (3.0, 4), (3.25, 1)]);
    }

    #[test]
    fn fire_events_excludes_hold_mid_and_fake() {
        // 120bpm(0.5s/拍):tap @1s,hold 2s→4s,fake tap @1.5s。
        // hold tick 2.25/2.5/.../3.75 与尾 4.0 均不得作为音频事件触发。
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
        // 只有 tap 头 @1.0 与 hold 头 @2.0;fake @1.5s 与全部 tick/tail 排除。
        assert_eq!(chart.fire_events(), vec![(1.0, 1), (2.0, 2)]);
        // 负例:hold 中部任意半拍 tick / 尾不得触发 hitsound。
        for t in [2.25, 2.5, 3.0, 3.75, 4.0] {
            assert!(
                chart.fire_events().iter().all(|(et, _)| *et != t),
                "hold mid/tail time {t} must not fire a hitsound"
            );
        }
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
    fn fx_poses_dedups_by_line_and_t0() {
        // 同 fixture:线 0 拍在 (0,0),4 拍移到 (100,100),旋转 0°→90°
        // (120bpm → 0..2s)。加两条同拍 note(和弦)与一条更晚的 note。
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
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [1, 0, 1], "endTime": [1, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 1, "above": 1, "startTime": [1, 0, 1], "endTime": [1, 0, 1], "positionX": 337.5, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 1, "above": 1, "startTime": [3, 0, 1], "endTime": [3, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ]
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(&src, false).unwrap();
        // 120bpm:beat 1 → 0.5s,beat 3 → 1.5s。和弦两个触发点同 (line, t0)。
        let trigs = [
            FxTrigger { t0: 0.5, line: 0, x: 0.0, y: 0.0 },
            FxTrigger { t0: 0.5, line: 0, x: 0.5, y: 0.0 },
            FxTrigger { t0: 1.5, line: 0, x: 0.0, y: 0.0 },
        ];
        let poses = chart.fx_poses(&trigs);
        // 输出与输入等长、顺序映射。
        assert_eq!(poses.len(), trigs.len());
        // 同 (line, t0) 和弦:位姿逐位一致。
        assert_eq!(poses[0], poses[1], "和弦触发点应共享同一位姿");
        // 同线不同 t0:线在动,位姿必须不同(聚合键错误合并会改变 fx 落点)。
        assert!((poses[2].0[0] - poses[0].0[0]).abs() > 0.01
            || (poses[2].1 - poses[0].1).abs() > 0.01,
            "不同 t0 位姿应不同: {:?} vs {:?}", poses[0], poses[2]);
        // 与 state_at 同刻位姿一致(±1e-3),即批量接口语义与单点逐像素等价。
        let (fpos, frot) = {
            let frame = chart.state_at(0.5);
            (frame.lines[0].position, frame.lines[0].rotation)
        };
        assert!((poses[0].0[0] - fpos[0]).abs() < 1e-3);
        assert!((poses[0].0[1] - fpos[1]).abs() < 1e-3);
        assert!((poses[0].1 - frot).abs() < 1e-3);
    }

    #[test]
    fn trigger_y_carries_above_sign_and_y_offset() {
        // 回归:fx 落点必须含 note 的 y 偏移(above 符号 × y_offset×2/900×
        // speed)——用户实测 hit-fx 没有判断 note 是 up 还是 down,带
        // y_offset 的 note 特效落在线上。触发表的 y 与 state_at 的
        // relative[1] 同口径(不含线高差 base 项)。
        // 120bpm:above note y_offset=100 → +100×2/900;below note
        // y_offset=50 → -50×2/900。
        let src = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "line0", "Texture": "line.png", "father": -1,
                    "eventLayers": [ null, null ],
                    "isCover": 0,
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [2, 0, 1], "endTime": [2, 0, 1], "positionX": 0.0, "yOffset": 100.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 },
                        { "type": 1, "above": 0, "startTime": [3, 0, 1], "endTime": [3, 0, 1], "positionX": 0.0, "yOffset": 50.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ]
                }
            ]
        }"#;
        let chart = Chart::from_rpe(&src, false).unwrap();
        let trigs = chart.fx_in_window(-0.5, 10.0);
        assert_eq!(trigs.len(), 2, "two heads in window");
        // 按 t 升序:t0=1.0s(above),1.5s(below)。
        assert!((trigs[0].y - (100.0 * 2.0 / 900.0)).abs() < 1e-6,
            "above y {} != +0.2222", trigs[0].y);
        assert!((trigs[1].y - (-50.0 * 2.0 / 900.0)).abs() < 1e-6,
            "below y {} != -0.1111", trigs[1].y);
    }

    #[test]
    fn fx_poses_stable_across_frames() {
        // 回归:fx 位置必须冻结在触发时刻 t0 的线位姿——播放推进中每帧
        // 重算(窗口滑动)结果必须逐位一致。若 set_time 游标往返不幂等,
        // 同一 t0 的位姿会随播放漂移 = 用户实测的"fx 后续帧重新跟踪线"。
        // 线 0..2s 线性平移 (0→0.5) + 旋转 (0°→90°),note 触发于 1.0s。
        let src = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "line0", "Texture": "line.png", "father": -1,
                    "eventLayers": [
                        {
                            "moveXEvents": [ { "start": 0.0, "end": 0.5, "startTime": [0, 0, 1], "endTime": [2, 0, 1], "easingType": 0 } ],
                            "rotateEvents": [ { "start": 0.0, "end": 90.0, "startTime": [0, 0, 1], "endTime": [2, 0, 1], "easingType": 0 } ]
                        },
                        null
                    ],
                    "isCover": 0,
                    "notes": [
                        { "type": 1, "above": 1, "startTime": [2, 0, 1], "endTime": [2, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                    ]
                }
            ]
        }"#;
        let mut chart = Chart::from_rpe(&src, false).unwrap();
        let t0 = 1.0;
        // 参照:t0 时刻 state_at 的线位姿(fx 必须冻结在它上面)。
        let (ref_pos, ref_rot) = {
            let f = chart.state_at(t0);
            (f.lines[0].position, f.lines[0].rotation)
        };
        let mut last: Option<([f32; 2], f32)> = None;
        for t in [1.0, 1.1, 1.2, 1.3, 1.4, 1.5] {
            // 推进游标(模拟播放经过 t0 之后继续前进)。
            chart.state_at(t);
            let trigs = chart.fx_in_window(t - 0.5, t);
            let idx = trigs.iter().position(|tr| (tr.t0 - t0).abs() < 1e-9)
                .expect("window contains the t0 head");
            let poses = chart.fx_poses(&trigs);
            let p = poses[idx];
            // 与 t0 时刻参照一致(不随播放漂移 = 不"重新跟踪"线)。
            assert!((p.0[0] - ref_pos[0]).abs() < 1e-3, "t={t} px {} vs {}", p.0[0], ref_pos[0]);
            assert!((p.0[1] - ref_pos[1]).abs() < 1e-3, "t={t} py {} vs {}", p.0[1], ref_pos[1]);
            assert!((p.1 - ref_rot).abs() < 1e-3, "t={t} rot {} vs {}", p.1, ref_rot);
            // 跨帧逐位一致。
            if let Some(prev) = last {
                assert_eq!(p, prev, "t={t}: pose drifted from previous frame");
            }
            last = Some(p);
        }
    }

    #[test]
    fn line_pose_at_with_parent_chain_matches_state_at() {
        // 双线父链:line0 旋转 0°→90° 且平移 (0,0)→(0.5,-0.5)(父线有 move
        // 事件——曾漏组合父平移,子线位姿停在未组合坐标),line1 挂到 line0
        // (father=0) + rotateWithFather。子线位姿应随父线旋转/平移继承。
        let src = r#"{
            "META": { "offset": 0, "RPEVersion": 160 },
            "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
            "judgeLineList": [
                {
                    "Name": "parent", "Texture": "line.png", "father": -1, "rotateWithFather": false,
                    "eventLayers": [
                        {
                            "moveXEvents": [ { "start": 0.0, "end": 0.5, "startTime": [0, 0, 1], "endTime": [2, 0, 1], "easingType": 0 } ],
                            "moveYEvents": [ { "start": 0.0, "end": -0.5, "startTime": [0, 0, 1], "endTime": [2, 0, 1], "easingType": 0 } ],
                            "rotateEvents": [ { "start": 0.0, "end": 90.0, "startTime": [0, 0, 1], "endTime": [2, 0, 1], "easingType": 0 } ]
                        },
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
        // 0.5s:父线旋转到 22.5° 且平移 (0.125,-0.125)。
        // 子线解析旋转 = 自身(0) + 父(22.5°),位置 = 父位 + R(父旋转)×子偏移(0)。
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

    // ── PMCORE-21 校验 ──

    fn mk_note(kind: u8, start: f64, end: f64, x: f32, alpha: u16, fake: u8) -> RPENote {
        RPENote {
            kind,
            above: 1,
            start_time: Triple::from_beats(start),
            end_time: Triple::from_beats(end),
            position_x: x,
            y_offset: 0.0,
            alpha,
            hitsound: None,
            size: 1.0,
            speed: 1.0,
            is_fake: fake,
            visible_time: 999999.0,
            tint: None,
            tint_hit_effects: None,
            judge_area: None,
            comment: None,
        }
    }

    fn mk_line(notes: Vec<RPENote>, layers: Vec<Option<RPEEventLayer>>) -> RPEJudgeLine {
        RPEJudgeLine {
            name: "L".into(),
            texture: "line.png".into(),
            parent: None,
            rotate_with_father: None,
            event_layers: layers,
            extended: None,
            notes: Some(notes),
            is_cover: 0,
            z_order: 0,
            attach_ui: None,
            pos_control: vec![],
            size_control: vec![],
            alpha_control: vec![],
            y_control: vec![],
            comment: None,
        }
    }

    fn mk_chart(lines: Vec<RPEJudgeLine>, bpms: Vec<(f64, f64)>) -> RPEChart {
        RPEChart {
            meta: crate::core::model::RPEMetadata { offset: 0, rpe_version: 160 },
            bpm_list: bpms
                .into_iter()
                .map(|(b, v)| crate::core::model::RPEBpmItem {
                    bpm: v,
                    start_time: Triple::from_beats(b),
                })
                .collect(),
            judge_line_list: lines,
        }
    }

    #[test]
    fn validate_detects_bad_chart_with_locations() {
        // 一条线内构造全部内容级问题:负拍音符、hold end<start、|x|>675、
        // alpha>255、重复音符(同拍同 x);全局:BPM≤0。事件:负拍事件、
        // start>end 事件。
        let layers = vec![Some(RPEEventLayer {
            alpha_events: Some(vec![RPEEvent {
                start_time: Triple::from_beats(-1.0),
                end_time: Triple::from_beats(2.0),
                start: 255.0,
                end: 0.0,
                easing_type: 1,
                easing_left: 0.0,
                easing_right: 1.0,
                bezier: 0,
                bezier_points: [0.0; 4],
            }]),
            move_x_events: Some(vec![RPEEvent {
                start_time: Triple::from_beats(4.0),
                end_time: Triple::from_beats(3.0), // start > end
                start: 0.0,
                end: 1.0,
                easing_type: 1,
                easing_left: 0.0,
                easing_right: 1.0,
                bezier: 0,
                bezier_points: [0.0; 4],
            }]),
            move_y_events: None,
            rotate_events: None,
            speed_events: None,
        })];
        let chart = mk_chart(
            vec![mk_line(
                vec![
                    mk_note(1, -0.5, -0.5, 0.0, 255, 0),   // 负拍
                    mk_note(2, 2.0, 1.0, 0.0, 255, 0),     // hold end < start
                    mk_note(1, 3.0, 3.0, 700.0, 255, 0),   // |x| > 675
                    mk_note(1, 4.0, 4.0, 0.0, 300, 0),     // alpha > 255
                    mk_note(1, 5.0, 5.0, 0.0, 255, 0),     // 重复对成员 1
                    mk_note(1, 5.0, 5.0, 0.0, 255, 0),     // 重复对成员 2
                ],
                layers,
            )],
            vec![(0.0, 0.0)], // BPM ≤ 0
        );
        let issues = chart.validate();
        let kinds: Vec<IssueKind> = issues.iter().map(|i| i.kind).collect();
        assert!(kinds.contains(&IssueKind::NegativeBeat), "{kinds:?}");
        assert!(kinds.contains(&IssueKind::HoldEndBeforeStart), "{kinds:?}");
        assert!(kinds.contains(&IssueKind::PositionXOutOfRange), "{kinds:?}");
        assert!(kinds.contains(&IssueKind::AlphaOutOfRange), "{kinds:?}");
        assert!(kinds.contains(&IssueKind::BpmNonPositive), "{kinds:?}");
        assert!(kinds.contains(&IssueKind::EventStartAfterEnd), "{kinds:?}");
        assert!(kinds.contains(&IssueKind::DuplicateNote), "{kinds:?}");
        // 定位字段齐全:每条问题有 line/index/beat,且指到正确位置。
        let neg = issues.iter().find(|i| i.kind == IssueKind::NegativeBeat).unwrap();
        assert_eq!((neg.line, neg.index), (Some(0), Some(0)));
        assert_eq!(neg.beat, Some(-0.5));
        let hold = issues.iter().find(|i| i.kind == IssueKind::HoldEndBeforeStart).unwrap();
        assert_eq!((hold.line, hold.index), (Some(0), Some(1)));
        assert_eq!(hold.beat, Some(1.0)); // end 拍
        let x = issues.iter().find(|i| i.kind == IssueKind::PositionXOutOfRange).unwrap();
        assert_eq!((x.line, x.index), (Some(0), Some(2)));
        let alpha = issues.iter().find(|i| i.kind == IssueKind::AlphaOutOfRange).unwrap();
        assert_eq!((alpha.line, alpha.index), (Some(0), Some(3)));
        let bpm = issues.iter().find(|i| i.kind == IssueKind::BpmNonPositive).unwrap();
        assert_eq!((bpm.line, bpm.index), (None, Some(0)));
        let ev_neg = issues
            .iter()
            .filter(|i| i.kind == IssueKind::NegativeBeat)
            .find(|i| i.message.contains("alphaEvents"))
            .unwrap();
        assert_eq!((ev_neg.line, ev_neg.index), (Some(0), Some(0)));
        assert!(ev_neg.message.contains("alphaEvents"));
        let ev_inv = issues.iter().find(|i| i.kind == IssueKind::EventStartAfterEnd).unwrap();
        assert_eq!((ev_inv.line, ev_inv.index), (Some(0), Some(0)));
        assert!(ev_inv.message.contains("moveXEvents"));
        let dups: Vec<&ChartIssue> = issues.iter().filter(|i| i.kind == IssueKind::DuplicateNote).collect();
        assert_eq!(dups.len(), 1, "一组重复只报 1 条(第 2 个成员)");
        assert_eq!((dups[0].line, dups[0].index), (Some(0), Some(5)));
        assert_eq!(dups[0].beat, Some(5.0));
    }

    #[test]
    fn validate_no_false_positives() {
        // 合法谱:叠键(同拍不同 x)、fake 与实体同拍同 x、零长 speed 事件、
        // 合法 alpha 事件、合法 hold。全部不得误报。
        let layers = vec![Some(RPEEventLayer {
            alpha_events: Some(vec![RPEEvent {
                start_time: Triple::from_beats(0.0),
                end_time: Triple::from_beats(4.0),
                start: 255.0,
                end: 0.0,
                easing_type: 1,
                easing_left: 0.0,
                easing_right: 1.0,
                bezier: 0,
                bezier_points: [0.0; 4],
            }]),
            move_x_events: None,
            move_y_events: None,
            rotate_events: None,
            // PEC 合法产物:零长 speed 步进事件(start == end)。
            speed_events: Some(vec![
                RPEEvent {
                    start_time: Triple::from_beats(0.0),
                    end_time: Triple::from_beats(0.0),
                    start: 1.0,
                    end: 1.0,
                    easing_type: 1,
                    easing_left: 0.0,
                    easing_right: 1.0,
                    bezier: 0,
                    bezier_points: [0.0; 4],
                },
                RPEEvent {
                    start_time: Triple::from_beats(2.0),
                    end_time: Triple::from_beats(2.0),
                    start: 1.5,
                    end: 1.5,
                    easing_type: 1,
                    easing_left: 0.0,
                    easing_right: 1.0,
                    bezier: 0,
                    bezier_points: [0.0; 4],
                },
            ]),
        })];
        let chart = mk_chart(
            vec![mk_line(
                vec![
                    mk_note(1, 1.0, 1.0, 0.0, 255, 0),   // 实体
                    mk_note(3, 1.0, 1.0, 0.0, 255, 0),   // 同拍同 x 不同类(flick)= 叠键,仅告警
                    mk_note(1, 1.0, 1.0, 300.0, 255, 0), // 同拍不同 x,合法
                    mk_note(2, 2.0, 4.0, -100.0, 255, 0), // 合法 hold
                    mk_note(3, 3.0, 3.0, 0.0, 255, 1),   // fake 与实体同拍同 x,不报重复
                    mk_note(3, 3.0, 3.0, 0.0, 255, 0),
                ],
                layers,
            )],
            vec![(0.0, 120.0)],
        );
        let issues = chart.validate();
        // 只有叠键告警(DuplicateNote),且恰好是 1.0 拍那组(flick 相对 tap)。
        for i in &issues {
            assert_eq!(i.kind, IssueKind::DuplicateNote, "误报: {i:?}");
        }
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!((issues[0].line, issues[0].index, issues[0].beat), (Some(0), Some(1), Some(1.0)));
    }

    #[test]
    fn validate_10k_notes_under_100ms() {
        // 万级音符(单线 10k,拍号各不相同):排序后线性扫描,必须 <100ms。
        let notes: Vec<RPENote> = (0..10_000)
            .map(|i| mk_note(1, i as f64, i as f64, (i as f32 % 1000.0) - 500.0, 255, 0))
            .collect();
        let chart = mk_chart(vec![mk_line(notes, vec![])], vec![(0.0, 120.0)]);
        let start = std::time::Instant::now();
        let issues = chart.validate();
        let el = start.elapsed();
        assert!(issues.is_empty());
        assert!(
            el < std::time::Duration::from_millis(100),
            "validate 10k notes took {:?}",
            el
        );
    }

    // ── combo 计数精度不变量(PMCORE)──
    //
    // 口径:主层 combo 的增量来源只有 state_at 返回的 fired 头/尾(hold 半拍
    // tick 与 fake 不计),seek 时 combo = hits_before(ct) 重建。因此任意时刻
    // `combo == chart.hits_before(chart_time)` 成立 ⟺ 累计 fired 集合恰等于
    // hits_before 的"已过 note"集合(头/尾各一)。下面按 main.rs render_frame
    // 的时钟推导(预测注入、暂停 predict/latency 归零、seek/hard_seek、A-B
    // 回跳)逐帧仿真并断言该不变量,锁定:暂停归零回跳、前后向 seek、A-B
    // hard_seek、边界精确停留都不产生双计/漏计。

    /// 多 BPM 谱面:120 → 175 → 240 → 120(段边界拍 2/5/8),段长含非整秒
    /// (beat 5→8 段 = 3 拍 @240 = 0.75s,起点 2.02857s)。音符落在段边界
    /// (2.0/5.0/8.0)、半拍(1.5/2.5/5.5)、1/3 拍(4/3, 10/3, 20/3, 25/3)、
    /// 3/5 拍(1.6);含一根 hold(beat 5→6,跨 175→240 边界)与一个 fake。
    fn combo_chart() -> Chart {
        let bpms = vec![(0.0, 120.0), (2.0, 175.0), (5.0, 240.0), (8.0, 120.0)];
        let notes = vec![
            mk_note(1, 1.0, 1.0, 0.0, 255, 0),
            mk_note(1, 4.0 / 3.0, 4.0 / 3.0, 0.0, 255, 0),
            mk_note(1, 1.5, 1.5, 0.0, 255, 0),
            mk_note(1, 1.6, 1.6, 0.0, 255, 0),
            mk_note(1, 2.0, 2.0, 0.0, 255, 0),
            mk_note(1, 2.5, 2.5, 0.0, 255, 0),
            mk_note(1, 10.0 / 3.0, 10.0 / 3.0, 0.0, 255, 0),
            mk_note(2, 5.0, 6.0, 0.0, 255, 0), // hold:头@beat5 尾@beat6
            mk_note(1, 5.5, 5.5, 0.0, 255, 0),
            mk_note(1, 20.0 / 3.0, 20.0 / 3.0, 0.0, 255, 0),
            mk_note(1, 8.0, 8.0, 0.0, 255, 0),
            mk_note(1, 25.0 / 3.0, 25.0 / 3.0, 0.0, 255, 0),
            mk_note(1, 2.0, 2.0, 300.0, 255, 1), // fake:与真实 note 同拍,排除
        ];
        Chart::from_rpe_chart(&mk_chart(vec![mk_line(notes, vec![])], bpms), false).unwrap()
    }

    /// 主层播放时钟 + combo 的最小仿真(main.rs render_frame / seek 语义):
    /// - chart_time = audio + predict - latency(META offset 0);暂停时 predict/latency 归零
    /// - 暂停归零造成的回跳被钳制为单调不减(chart_time_last 高水位,
    ///   main.rs render_frame 修复)——真正的 seek 走 reposition + hits_before 重建
    /// - seek:reposition(ct) + combo = hits_before(ct)(前向后向一致,
    ///   main.rs seek/hard_seek 修复)
    /// 每帧断言 combo == hits_before(交付的 chart_time)。
    struct PlaySim {
        audio: f64,
        paused: bool,
        predict: f64,
        latency: f64,
        delivered: f64,
        combo: usize,
    }

    impl PlaySim {
        fn new() -> Self {
            PlaySim { audio: 0.0, paused: false, predict: 0.05, latency: 0.0, delivered: 0.0, combo: 0 }
        }
        fn pause(&mut self) { self.paused = true; }
        fn resume(&mut self) { self.paused = false; }
        /// 一帧:audio 前进 dt 后求 chart_time(含 predict/latency 与暂停归零),
        /// state_at + fired 累计,断言不变量。
        fn frame(&mut self, chart: &mut Chart, dt: f64) {
            self.audio += dt;
            let predict = if self.paused { 0.0 } else { self.predict };
            let latency = if self.paused { 0.0 } else { self.latency };
            let mut ct = (self.audio + predict - latency).max(0.0);
            ct = ct.max(self.delivered); // 暂停归零回跳钳制(高水位)
            self.delivered = ct;
            {
                let frame = chart.state_at(ct);
                for f in &frame.fired {
                    if !f.fake && !f.tick {
                        self.combo += 1;
                    }
                }
            }
            assert_eq!(self.combo, chart.hits_before(ct), "combo invariant @ chart_time {ct:.9}");
        }
        /// seek(等价 main.rs seek/hard_seek):reposition 同步游标 + hits_before 重建。
        fn seek(&mut self, chart: &mut Chart, audio_t: f64) {
            self.audio = audio_t;
            self.delivered = audio_t; // chart_time_last 同步到目标
            chart.reposition(audio_t);
            self.combo = chart.hits_before(audio_t);
        }
        /// 逐帧播放到 audio 精确等于 `audio_t`(最后一段用小步长)。
        fn play_until(&mut self, chart: &mut Chart, audio_t: f64) {
            while self.audio + 1e-12 < audio_t {
                let dt = (audio_t - self.audio).min(0.016);
                self.frame(chart, dt);
            }
        }
    }

    /// 直接以 chart 时间为输入跑一帧并断言不变量(测试 advance_fired 窗口语义本身)。
    fn assert_frame_invariant(chart: &mut Chart, combo: &mut usize, t: f64) {
        {
            let frame = chart.state_at(t);
            for f in &frame.fired {
                if !f.fake && !f.tick {
                    *combo += 1;
                }
            }
        }
        assert_eq!(*combo, chart.hits_before(t), "combo invariant @ chart_time {t:.9}");
    }

    #[test]
    fn combo_invariant_bpm_conversion_locked() {
        // 段边界拍→秒精确(beat_to_time 与 note 解析同走 BpmList::time_beats)。
        let mut chart = combo_chart();
        assert!((chart.beat_to_time(2.0) - 1.0).abs() < 1e-9, "120→175 边界");
        // 175→240 边界 = 2.02857142857…s(非整秒);段长 beat 5→8 @240 = 0.75s。
        let b5 = chart.beat_to_time(5.0);
        let b8 = chart.beat_to_time(8.0);
        assert!((b5 - 2.0285714285714286).abs() < 1e-9, "175→240 边界(非整秒)");
        assert!((b8 - 2.7785714285714286).abs() < 1e-9, "240→120 边界");
        assert!((b8 - b5 - 0.75).abs() < 1e-9, "非整秒段长");
        assert_eq!(chart.max_combo(), 13, "11 tap + hold head + hold tail;fake 排除");
    }

    #[test]
    fn combo_invariant_full_playback_with_predict() {
        // 正常播放:0.05s 预测延迟注入,逐帧不变量,直到谱面末尾。
        let mut chart = combo_chart();
        let mut sim = PlaySim::new();
        for _ in 0..200 {
            sim.frame(&mut chart, 0.016);
        }
        assert_eq!(sim.combo, chart.hits_before(99.0));
        assert_eq!(sim.combo, 13);
    }

    #[test]
    fn combo_invariant_pause_resume_predict_zeroing() {
        // 暂停 predict/latency 归零 → 原始 chart_time 回跳 0.05s;高水位钳制
        // 后 delivered 不回退,恢复时 (回跳点, 原位置] 窗口不重放 → 不双计。
        // 顺带确认:恢复瞬间 chart_time 前跳 0.05(暂停窗口内未过的 note 正常
        // 触发一次,前跳=正常触发,非双计——由逐帧不变量覆盖)。
        let mut chart = combo_chart();
        let mut sim = PlaySim::new();
        // 播放到 audio 1.15(chart 1.20):note@1.1714 已计。
        sim.play_until(&mut chart, 1.15);
        assert_eq!(sim.combo, 6); // 0.5, 0.667, 0.75, 0.8, 1.0, 1.1714
        let delivered_before = sim.delivered;
        sim.pause();
        for _ in 0..5 {
            sim.frame(&mut chart, 0.0); // 暂停帧(音频冻结)
        }
        assert_eq!(sim.delivered, delivered_before, "回跳被钳制");
        assert_eq!(sim.combo, 6);
        sim.resume();
        // 恢复:chart_time 从 1.20 继续,(1.20, ...] 无已计 note → 不双计。
        sim.play_until(&mut chart, 2.25);
        assert_eq!(sim.combo, 10); // 含 hold 尾@2.2786(audio 2.2286 已过)
        let delivered_before = sim.delivered;
        sim.pause();
        for _ in 0..3 {
            sim.frame(&mut chart, 0.0);
        }
        assert_eq!(sim.delivered, delivered_before);
        sim.resume();
        sim.play_until(&mut chart, 3.2);
        assert_eq!(sim.combo, 13);
    }

    #[test]
    fn combo_invariant_forward_seek() {
        // 前向 seek 跨多个 note:reposition 跳过窗口,combo 从 hits_before 重建。
        let mut chart = combo_chart();
        let mut sim = PlaySim::new();
        sim.play_until(&mut chart, 1.5); // chart 1.55
        assert_eq!(sim.combo, chart.hits_before(sim.delivered));
        sim.seek(&mut chart, 2.6); // 跳过 1.4571 / hold头@2.0286 / 2.1536 / 尾@2.2786 / 2.4452
        assert_eq!(sim.combo, 11); // hits_before(2.6)
        sim.play_until(&mut chart, 3.2);
        assert_eq!(sim.combo, 13);
    }

    #[test]
    fn combo_invariant_backward_seek_replay() {
        // 后向 seek:combo 从 hits_before 重建后重放继续。reposition 让重放
        // 精确从目标开始——note@0.8 恰在 (0.78, 0.78+0.05] 预测窗口内,
        // 若不 reposition,seek-back 重放从 0.83 起会漏掉它(combo 到不了 13)。
        let mut chart = combo_chart();
        let mut sim = PlaySim::new();
        sim.play_until(&mut chart, 2.0); // chart 2.05,combo = hits_before(2.05) = 8
        assert_eq!(sim.combo, 8);
        sim.seek(&mut chart, 0.78);
        assert_eq!(sim.combo, 3); // hits_before(0.78) = 0.5/0.667/0.75
        sim.play_until(&mut chart, 3.2);
        assert_eq!(sim.combo, 13, "重放不漏计 note@0.8");
    }

    #[test]
    fn combo_invariant_ab_loop_jumpback() {
        // A-B 循环回跳(main.rs 2144-2163 的 hard_seek 路径,纯 chart 模拟):
        // 播放穿 B → hard_seek 回 A(reposition + hits_before 重建)。修复后
        // 本帧 chart_time 用跳转后的音频位置计算——若仍用回跳前的陈旧位置,
        // state_at 会收到 B 附近的时间,(A, B] 窗口在重放时二次触发 → 双计。
        let mut chart = combo_chart();
        let mut sim = PlaySim::new();
        let a_t = chart.beat_to_time(2.0); // 1.0(120→175 段边界)
        let b_t = chart.beat_to_time(5.0); // 2.0285714...(175→240 段边界)
        for loop_i in 0..2 {
            sim.play_until(&mut chart, b_t);
            sim.seek(&mut chart, a_t); // hard_seek(A) 等价调用
            sim.frame(&mut chart, 0.016); // 跳转帧:state_at(1.05) 只补触发 (1.0, 1.05]
            assert_eq!(sim.combo, chart.hits_before(sim.delivered), "loop {loop_i} jump frame");
        }
        sim.play_until(&mut chart, 3.2);
        assert_eq!(sim.combo, 13);
    }

    #[test]
    fn combo_invariant_note_boundary_exact() {
        // 播放头恰好停在 note 时间点再小幅前进:边界 note 由 hits_before 的
        // `<=` 计入、不补触发(`<=` 与 fired 条件 `last < t <= time` 精确互补)。
        let mut chart = combo_chart();
        let mut combo = 0usize;
        let t25 = chart.beat_to_time(2.5); // note@beat2.5(半拍,175 段)
        let t_tail = chart.beat_to_time(6.0); // hold 尾(240 段)
        assert_frame_invariant(&mut chart, &mut combo, 1.0); // 恰停在 note@1.0(段边界)
        assert_eq!(combo, 5);
        assert_frame_invariant(&mut chart, &mut combo, 1.001); // 小幅前进:窗口空
        assert_eq!(combo, 5);
        assert_frame_invariant(&mut chart, &mut combo, t25); // 恰穿越 note@beat2.5
        assert_eq!(combo, 6);
        // 后向 seek 到恰在 note@0.75 前:重放精确从 0.75 起,note@0.75 由
        // hits_before 计入且不重触发(seek 到恰在 note 上)。
        chart.reposition(0.75);
        combo = chart.hits_before(0.75); // 0.5, 0.667, 0.75 = 3
        assert_frame_invariant(&mut chart, &mut combo, 0.75);
        assert_eq!(combo, 3);
        assert_frame_invariant(&mut chart, &mut combo, 0.76);
        assert_eq!(combo, 3);
        // hold 尾精确边界(beat 6.0):恰穿越计 1,再小幅前进不重复。
        assert_frame_invariant(&mut chart, &mut combo, t_tail);
        assert_frame_invariant(&mut chart, &mut combo, t_tail + 0.0001);
        assert_eq!(combo, chart.hits_before(t_tail + 0.0001));
    }

    #[test]
    fn pause_predict_zeroing_raw_drop_double_counts_without_clamp() {
        // 缺陷机制锁定:若暂停归零回跳不被钳制(原始 chart_time 从 1.186 回退
        // 到 1.152),state_at 的 seek-back 重放会让恢复后 (1.152, 1.202] 窗口
        // 的 note@1.1714 二次触发 → combo 超过 hits_before。main.rs render_frame
        // 的单调钳制就是为此;此测试固定"回跳必然双计"的事实,防止未来静默
        // 改变 advance_fired 窗口语义时丢失修复理由。
        let mut chart = combo_chart();
        let mut combo = 0usize;
        for i in 0..72 {
            assert_frame_invariant(&mut chart, &mut combo, i as f64 * 0.016 + 0.05);
        }
        assert_eq!(combo, 6); // 已计 note@1.1714
        // 暂停帧:原始 chart_time 回跳到 1.152(< 1.186)→ seek-back 不报,但
        // combo 不回退 → combo(6) 已超过 hits_before(1.152)(note 弹回)。
        {
            let frame = chart.state_at(1.152);
            assert!(frame.fired.is_empty());
        }
        assert_eq!(chart.hits_before(1.152), 5);
        assert_eq!(combo, 6);
        // 恢复:chart_time 回到 1.202 → (1.152, 1.202] 重放 note@1.1714 → 双计。
        {
            let frame = chart.state_at(1.202);
            assert_eq!(frame.fired.len(), 1);
            combo += 1;
        }
        assert_eq!(combo, 7);
        assert_ne!(combo, chart.hits_before(1.202), "不钳制时 combo 超过 hits_before");
    }
}

