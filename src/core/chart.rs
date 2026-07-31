// Derived from TeamFlos/phira prpr, GPL-3.0.
//! RPE chart loading and per-frame state evaluation.
//! Lowering ported from `prpr/src/parse/rpe.rs` (`parse_rpe`); note/line
//! render math ported from `prpr/src/core/note.rs` and `core/line.rs`.

use anyhow::{Context, Result};
use std::{collections::HashMap, rc::Rc};

use super::{
    anim::{Anim, AnimFloat, Keyframe},
    bpm::BpmList,
    easing::{
        speed_linear_tween, speed_segment_tween, BezierTween, ClampedTween, RPE_TWEEN_MAP, SpeedEasingMode, StaticTween, TweenFunction, Tweenable,
    },
    model::{parse_info_txt, ChartFormat, ChartInfo, RPEChart, RPEEvent, RPEEventLayer, RPEJudgeLine, RPENote},
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
    /// PE extension: alpha < 0 hides line + notes.
    pub pe_hide: bool,
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
    /// Cursor into `order` for fired detection: notes before it have
    /// `max(time, end_time) <= last_state_time` and can never fire again.
    fired_cursor: usize,
    texture: Option<String>,
    z_order: i32,
    parent: Option<usize>,
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
            Rc::clone(&bezier_map[&self.bezier_key()])
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
        // [visible_time] RPE stores visibleTime in milliseconds; convert to seconds.
        let vt_sec = note.visible_time / 1000.0;
        let alpha = if vt_sec >= time {
            if note.alpha >= 255 {
                AnimFloat::default()
            } else {
                AnimFloat::fixed(note.alpha as f32 / 255.)
            }
        } else {
            let alpha = note.alpha.min(255) as f32 / 255.;
            AnimFloat::new(vec![Keyframe::new(time - vt_sec, 0.0, 0), Keyframe::new(time, alpha, 0)])
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

fn parse_ctrl_events(events: &[super::model::RPECtrlEvent], key: &str) -> AnimFloat {
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
    AnimFloat::new(
        events.iter().zip(vals).zip(tweens).map(|((it, val), tween)| Keyframe {
            time: it.x,
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

    let alpha = events_with_factor(r, &event_layers, |it| &it.alpha_events, 1. / 255., bezier_map)?;
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
        if parent == -1 {
            None
        } else {
            // other negative values wrap to a huge index and panic later on
            // access, same as prpr
            Some(parent as usize)
        }
    };

    Ok(LineData {
        name: rpe.name,
        alpha,
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
        fired_cursor: 0,
        texture: if rpe.texture == "line.png" { None } else { Some(rpe.texture.clone()) },
        z_order: rpe.z_order,
        parent,
        show_below: rpe.is_cover != 1,
        attach_ui: rpe.attach_ui.clone(),
        // [E] CtrlObject: parse pos/size/alpha/y control events.
        // posControl uses key "pos" (single f32, factor 2/RPE_WIDTH for x, 2/RPE_HEIGHT for y).
        // sizeControl uses key "size" (single f32, factor 1.0).
        // alphaControl uses key "alpha" (0-1 range, factor 1.0).
        // yControl uses key "y" (factor 1.0).
        // phira: CtrlObject uses raw values (no factor), x is in beats.
        ctrl_pos_x: parse_ctrl_events(&pos_control, "pos"),
        ctrl_pos_y: parse_ctrl_events(&pos_control, "pos"),
        ctrl_size_x: parse_ctrl_events(&size_control, "size"),
        ctrl_size_y: parse_ctrl_events(&size_control, "size"),
        ctrl_alpha: parse_ctrl_events(&alpha_control, "alpha"),
        ctrl_y: parse_ctrl_events(&y_control, "y"),
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
    /// Scratch reused across frames: visible notes / fired events collected
    /// in time-sorted `order` scan order, index-tagged so they can be
    /// restored to draw order before publishing.
    visible_scratch: Vec<(usize, NoteState)>,
    fired_scratch: Vec<(usize, FiredNote)>,
}

/// Reads `info.json` in `dir` (falling back to an RPE-export `info.txt`
/// when absent), rejecting non-RPE formats. Shared by [`Chart::load`] and
/// the editor document API ([`crate::core::edit`]).
pub(crate) fn load_info(dir: &std::path::Path) -> Result<ChartInfo> {
    let info: ChartInfo = match std::fs::read_to_string(dir.join("info.json")) {
        Ok(src) => serde_json::from_str(&src).context("failed to parse info.json")?,
        Err(json_err) => {
            let src = std::fs::read_to_string(dir.join("info.txt"))
                .with_context(|| format!("failed to read info.json ({json_err}) or info.txt"))?;
            parse_info_txt(&src)
        }
    };
    Ok(info)
}

impl Chart {
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
                    vec(&e.scale_x_events)
                        .chain(vec(&e.scale_y_events))
                        .map(|it| r.time(&it.end_time))
                        .reduce(f64::max)
                        .unwrap_or(0.)
                        .max(vec(&e.text_events).map(|it| r.time(&it.end_time)).reduce(f64::max).unwrap_or(0.))
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
                anyhow::bail!("found infinite recursive parent relations: line {line}");
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
                ctrl_pos_x: 0.0, ctrl_pos_y: 0.0, ctrl_size_x: 1.0, ctrl_size_y: 1.0,
                ctrl_alpha: 1.0, ctrl_y: 1.0, pe_hide: false, incline_sin: 0.0,
                attach_ui: None,
            })
            .collect();
        Ok(Chart {
            offset,
            duration: max_time,
            lines,
            bpm_list: r,
            last_state_time: 0.,
            frame: FrameState { time: 0., lines: frame_lines, fired: Vec::new() },
            visible_scratch: Vec::new(),
            fired_scratch: Vec::new(),
        })
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
        let Chart {
            lines,
            bpm_list,
            last_state_time,
            frame,
            visible_scratch,
            fired_scratch,
            ..
        } = self;
        frame.time = time;
        frame.fired.clear();
        let last = *last_state_time;
        if time < last {
            // backward seek — cursors restart from the top
            for line in lines.iter_mut() {
                line.cursor = 0;
                line.fired_cursor = 0;
            }
        } else {
            for (line_idx, line) in lines.iter_mut().enumerate() {
                // notes with max(time, end_time) <= last can never fire again
                let start = line.fired_cursor;
                line.fired_cursor = start
                    + line.order[start..].partition_point(|&ni| {
                        let n = &line.notes[ni];
                        n.time.max(n.end_time) <= last
                    });
                for &ni in &line.order[line.fired_cursor..] {
                    let note = &line.notes[ni];
                    // hit events fire even for notes culled out of the frame
                    if last < note.time && note.time <= time {
                        fired_scratch.push((
                            ni,
                            FiredNote {
                                line: line_idx,
                                kind: note.kind,
                                x: note.x,
                                fake: note.fake,
                                tick: false,
                                hold_tail: false,
                            },
                        ));
                    }
                    // hold sustain: one event per half beat crossed while
                    // the hold is active (strictly after its hit beat)
                    if note.kind == 2 && note.time < time && last < note.end_time {
                        let b_lo = bpm_list.beat(last.max(note.time));
                        // exclusive cap: a beat landing exactly on end_time is
                        // the hold's release (t < end_time strict), not a tick
                        let cap = if time >= note.end_time { note.end_time - EPS } else { time };
                        let b_hi = bpm_list.beat(cap);
                        let crossed = ((b_hi * 2.).floor() - (b_lo * 2.).floor()).max(0.) as usize;
                        for _ in 0..crossed {
                            fired_scratch.push((
                                ni,
                                FiredNote {
                                    line: line_idx,
                                    kind: 2,
                                    x: note.x,
                                    fake: note.fake,
                                    tick: true,
                                    hold_tail: false,
                                },
                            ));
                        }
                    }
                    // hold release: fired once when end_time is crossed
                    if note.kind == 2 && last < note.end_time && note.end_time <= time {
                        fired_scratch.push((
                            ni,
                            FiredNote {
                                line: line_idx,
                                kind: 2,
                                x: note.x,
                                fake: note.fake,
                                tick: false,
                                hold_tail: true,
                            },
                        ));
                    }
                }
                // restore draw order (stable: per-note head/ticks/tail stays)
                fired_scratch.sort_by_key(|(ni, _)| *ni);
                frame.fired.extend(fired_scratch.drain(..).map(|(_, f)| f));
            }
        }
        // else: seek backward — report nothing this call
        *last_state_time = time;
        for (line, out) in lines.iter_mut().zip(frame.lines.iter_mut()) {
            line.alpha.set_time(time);
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
            out.pe_hide = raw_alpha < 0.0; // PE extension: -1 hides everything
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
        // [parent] Second pass: propagate parent transforms (rotation + position + alpha)
        // Based on phira's fetch_rot / fetch_pos (line.rs:211-228).
        for i in 0..frame.lines.len() {
            let parent_idx = frame.lines[i].parent;
            if let Some(pidx) = parent_idx {
                if pidx < frame.lines.len() {
                    let (parent_rot, parent_pos, parent_alpha) = {
                        let p = &frame.lines[pidx];
                        (p.rotation, p.position, p.alpha)
                    };
                    let out = &mut frame.lines[i];
                    out.rotation += parent_rot;
                    let cos = parent_rot.cos();
                    let sin = parent_rot.sin();
                    let lx = out.position[0];
                    let ly = out.position[1];
                    out.position[0] = parent_pos[0] + cos * lx - sin * ly;
                    out.position[1] = parent_pos[1] + sin * lx + cos * ly;
                    out.alpha *= parent_alpha; // child alpha × parent alpha
                }
            }
        }
        for (line, out) in lines.iter_mut().zip(frame.lines.iter_mut()) {
            // PE extension: line+notes hidden (alpha < 0 triggers this)
            if out.pe_hide { continue; }
            let line_height = line.height.now() as f64;
            let show_below = line.show_below;
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
            for &ni in &line.order[line.cursor..] {
                let note = &mut line.notes[ni];
                note.alpha.set_time(time);
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
                    (yoff + bottom, Some(yoff + (note.end_height - line_height) * spd))
                } else {
                    // Editor preview: notes vanish instantly at hit time
                    // (no prpr 0.16s fade-out).
                    // Fake notes always vanish past their hit time to prevent trails.
                    if (!show_below || note.fake) && time >= note.time {
                        continue;
                    }
                    if !show_below && note.time > time && base <= -0.001 {
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

    fn beat_to_time(chart: &mut Chart, beat: f64) -> f64 {
        let mut lo = 0.0;
        let mut hi = chart.duration();
        for _ in 0..60 {
            let mid = (lo + hi) / 2.0;
            if chart.time_to_beat(mid) < beat {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo + hi) / 2.0
    }

    #[test]
    fn rainshower_l75_flicker() {
        let dir = r"D:\phimakor\example_chart\RainShower";
        let (_info, mut chart) = Chart::load(std::path::Path::new(dir)).unwrap();
        for beat in [68.5, 69.05, 69.15, 69.20, 69.30, 69.40, 70.5] {
            let t = beat_to_time(&mut chart, beat);
            let state = chart.state_at(t);
            let line = &state.lines[75];
            println!(
                "beat {beat:5.2} (t={t:7.3}s): line75 alpha={:.4} pe_hide={}",
                line.alpha, line.pe_hide
            );
        }
    }

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
}
