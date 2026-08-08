//! wgpu 30 renderer for M0. Architecture:
//!
//! - **Quad pipeline**: instanced `TriangleStrip` quads with a shared WGSL shader
//!   (`SHADER`). Each instance encodes a 2D affine transform, RGBA tint, and UV rect.
//! - **Coordinate model**: centre-origin world space (x ∈ ±1, y ∈ ±1/ASPECT),
//!   letterboxed for non-16:9 windows. Canvas-to-world: 675 px → 1.0 world unit.
//! - **Draw pipeline**: `draw_to_view` builds a `DrawCmd` list
//!   → converts to `Instance` records → vertex‑buffer upload → scene render pass
//!   → post‑processing (see `post`) → UI overlay pass (iced + timeline).
//! - **Note rendering**: sprite‑based (textured) with coloured‑quad fallback.
//!   Holds use atlas textures (head + stretchable body + tail); _mh variants for
//!   multi‑press hints. Z‑order sorted by judge line, then kind priority.
//! - **Post‑processing**: ping‑pong render targets with per‑effect pipelines.
//!   Effects loaded from `extra.json` (grayscale, chromatic, glitch, vignette, etc.).
//! - **Hit effects**: sprite‑sheet burst particles (`fx.rs`).
//! - **Consumes** [`crate::core::FrameState`]; never touches serde models or IO.

use std::{borrow::Cow, collections::HashMap, mem::size_of, sync::Arc};

use anyhow::Context as _;
use winit::window::Window;

use crate::core::FrameState;

mod fx;
pub mod effects_chain;
mod pipeline_cache;
pub mod post;
pub mod preview;
pub mod shaders;
mod text;

pub use text::TextAnchor;

/// Playfield aspect ratio (w/h). 3:2 = native RPE canvas (1350×900).
/// pub:fx 位置计算(main.rs/engine.rs)必须用同一常量,不能用窗口比例。
pub const ASPECT: f32 = 3.0 / 2.0;

// Contract render-semantics constants, in RPE CANVAS pixels (1350×900).
const CANVAS_W: f32 = 675.0; // world x ±1 ↔ ±675 canvas px

/// 过激优化位标志(设置里逐个开关):hold 身体按视口裁剪(线段求交+
/// 勾股长度),长 hold 省大量 off-screen overdraw;视觉上等价但有回归风险。
pub const AGGRESSIVE_HOLD_CLIP: u32 = 1 << 0;
const CANVAS_H: f32 = 450.0; // world y ±1 ↔ ±450 canvas px
/// Built-in line quad FULL length at the default `line_length = 6.0`:
/// phira draws `draw_line(-len, 0, len, 0)` (half-length `len` in world
/// units, 675 canvas px per world unit) → full length 2×len×675 = 8100 px.
/// [`Renderer::set_line_length`] scales this per chart (info.yml `lineLength`).
const LINE_LEN: f32 = 2.0 * 6.0 * CANVAS_W; // built-in line quad length
const LINE_THICK: f32 = 0.01 * CANVAS_H; // built-in line quad thickness
const NOTE_W: f32 = 0.22 * CANVAS_W; // note quad width factor (× note.scale)
const NOTE_H: f32 = 0.03 * CANVAS_H; // note quad height factor (× note.scale)
const HOLD_BODY_W: f32 = 0.1 * CANVAS_W; // hold body width factor (× note.scale)
/// prpr note sprite width: 2 × NOTE_WIDTH_RATIO_BASE (0.13175016).
const NOTE_SPRITE_W: f32 = 0.2635 * CANVAS_W; // 2×0.13175016 world ≈ 177.8 canvas px
/// Hold atlas cap height in px (official respack: 50px head at top, 50px tail at bottom).
const HOLD_CAP_PX: f32 = 50.0;

// Contract note colors (kind: u8 follows RPE numbering).
const TAP_COLOR: [f32; 3] = [0.2, 0.6, 1.0]; // kind 1
const HOLD_COLOR: [f32; 3] = [0.3, 0.7, 1.0]; // kind 2
const FLICK_COLOR: [f32; 3] = [1.0, 0.3, 0.5]; // kind 3
const DRAG_COLOR: [f32; 3] = [1.0, 0.85, 0.2]; // kind 4

const INITIAL_DRAW_CAPACITY: usize = 4096;

// ---------------------------------------------------------------------------
// 2D affine matrix, column-major, padded to WGSL uniform layout (3 × vec4).
// Columns: (a, b, 0), (c, d, 0), (tx, ty, 1) — i.e. x' = a·x + c·y + tx.
// ---------------------------------------------------------------------------

type Mat3 = [[f32; 4]; 3];

fn mat_translate(x: f32, y: f32) -> Mat3 {
    [[1., 0., 0., 0.], [0., 1., 0., 0.], [x, y, 1., 0.]]
}

fn mat_scale(x: f32, y: f32) -> Mat3 {    [[x, 0., 0., 0.], [0., y, 0., 0.], [0., 0., 1., 0.]]
}

fn mat_rotate(rad: f32) -> Mat3 {
    let (s, c) = rad.sin_cos();
    [[c, s, 0., 0.], [-s, c, 0., 0.], [0., 0., 1., 0.]]
}

/// `a * b` (apply `b` first, then `a`).
fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut r = [[0.; 4]; 3];
    for j in 0..3 {
        for i in 0..3 {
            r[j][i] = a[0][i] * b[j][0] + a[1][i] * b[j][1] + a[2][i] * b[j][2];
        }
    }
    r
}

/// Liang-Barsky line-vs-AABB clipping. Returns the visible sub-segment as
/// `(t0, t1)` in `[0, 1]` along `p0 → p1`, or `None` when fully outside.
fn clip_segment(
    p0: (f32, f32),
    p1: (f32, f32),
    xmin: f32,
    xmax: f32,
    ymin: f32,
    ymax: f32,
) -> Option<(f32, f32)> {
    let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    // For each boundary: p·t ≤ q keeps the segment inside.
    let edges: [(f32, f32); 4] = [
        (-dx, p0.0 - xmin),
        (dx, xmax - p0.0),
        (-dy, p0.1 - ymin),
        (dy, ymax - p0.1),
    ];
    for (p, q) in edges {
        if p.abs() < 1e-9 {
            if q < 0.0 {
                return None; // parallel and outside
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 {
                    return None;
                }
                if r > t0 {
                    t0 = r;
                }
            } else {
                if r < t0 {
                    return None;
                }
                if r < t1 {
                    t1 = r;
                }
            }
        }
    }
    Some((t0, t1))
}

/// CPU-side per-quad assembly record; converted to [`Instance`] at upload.
/// `uv_rect`: xy = offset, zw = scale — `uv = xy + quad_uv * zw`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct DrawUniform {
    /// Column-major 2D affine transform (Mat3 padded to 3×vec4).
    model: Mat3,
    /// RGBA tint, multiplied with the sampled texture in the fragment shader.
    /// Straight (non-premultiplied) alpha: the scene pass blends with
    /// `ALPHA_BLENDING` (src-over), so the texture's alpha edge stays correct.
    color: [f32; 4],
    /// UV rectangle: [u0, v0, du, dv]; final UV = xy + quad_uv * zw.
    uv_rect: [f32; 4],
}

struct DrawCmd<'a> {
    uniform: DrawUniform,
    tex: &'a wgpu::BindGroup,
}

/// GPU instance record (64B): 2D affine + pad, tint, uv rect. One per quad,
/// Instance data consumed by the vertex shader (`SHADER → fn vs`).
///
/// The vertex shader treats this as four `vec4<f32>` instance attributes
/// (`VsIn` struct) and applies the affine parts as:
///
/// ```wgsl
/// // @location(0): m01 = (a, b, c, d)   — linear part of Mat3
/// // @location(1): m2p = (tx, ty, 0, 0)  — translation + padding
/// // @location(2): color                  — rgba tint
/// // @location(3): uv_rect = (u0, v0, du, dv) — UV rectangle
/// let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
/// let p = corner - 0.5; // [-0.5, +0.5]
/// world.x = a * p.x + c * p.y + tx;
/// world.y = b * p.x + d * p.y + ty;
/// uv = uv_rect.xy + corner * uv_rect.zw;
/// ```
///
/// `model` encodes a 2D affine transform `x' = M × x` in column-major order:
/// ```text
/// [a  c  tx]   [0   2   4]
/// [b  d  ty] = [1   3   5]
/// [0  0  1 ]   [6=0 7=0 /]
/// ```
///
/// Three draw types share this same layout, distinguished at construction:
///
/// | Type | `model` encodes | Consumer |
/// |------|----------------|----------|
/// | **Scene** (lines, notes, bg) | `letterbox × translate × rotate × scale(...)` | `line_m`, `note_m`, `bg_m` |
/// | **Texture** (custom judge line) | pixel→clip mapping | `tex_m` (see `Some(name)` case) |
/// | **UI overlay** (iced, timeline) | fullscreen NDC stretch | `mat_scale(2, 2)` + UV flip |
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    /// Affine matrix: [a, b, c, d, tx, ty, 0, 0].
    /// a,b,c,d = linear; tx,ty = translation; last 2 always 0.
    model: [f32; 8],
    /// RGBA tint (straight, non-premultiplied — see `DrawUniform::color`).
    color: [f32; 4],
    /// UV rect: [u0, v0, du, dv]. Final UV = (u0, v0) + corner * (du, dv).
    /// V⵨ is standard GPU bottom-up; overlay textures pass (du=1, dv=-1) to flip.
    uv_rect: [f32; 4],
}

/// Pack the affine part of a column-major Mat3 into Instance.model format:
/// `[m00, m01, m10, m11, m20, m21, 0, 0]`.
/// Letterbox + event-position transform for the playfield box.
/// `window_aspect` vs `playfield_aspect` decides the letterbox axes;
/// `ev_y = 1.5/aspect` maps the RPE canvas half-height (450) onto the box edge.
/// Returns (letterbox matrix, kx, ky, ev_x, ev_y).
pub(crate) fn letterbox_transform(window_aspect: f32, playfield_aspect: f32) -> (Mat3, f32, f32, f32, f32) {
    let aspect = playfield_aspect;
    let (kx, ky) = if window_aspect >= aspect {
        (aspect / window_aspect, 1.0)
    } else {
        (1.0, window_aspect / aspect)
    };
    let letterbox = mat_scale(kx / CANVAS_W, ky * aspect / CANVAS_W);
    let ev_x = 1.0;
    let ev_y = 1.5 / aspect;
    (letterbox, kx, ky, ev_x, ev_y)
}
fn instance_model(m: &Mat3) -> [f32; 8] {
    [m[0][0], m[0][1], m[1][0], m[1][1], m[2][0], m[2][1], 0., 0.]
}

const INSTANCE_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: size_of::<Instance>() as u64,
    step_mode: wgpu::VertexStepMode::Instance,
    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4],
};

struct TexEntry {
    bind_group: wgpu::BindGroup,
    /// Pixel size of the decoded image. prpr draws textured lines at natural
    /// pixel size × object scale (CoreEval's scale includes the ×2/1350 factor).
    size: [f32; 2],
}

/// Immutable per-frame scene-build context shared with worker threads
/// (PMCORE-73). All wgpu handles are `Send + Sync` (Arc-backed), and
/// `LineState` is plain data, so a shared `&SceneCtx` moves across
/// `std::thread::scope` workers safely — the main thread only submits.
struct SceneCtx<'a> {
    textures: &'a HashMap<String, TexEntry>,
    white: &'a wgpu::BindGroup,
    letterbox: Mat3,
    kx: f32,
    ev_x: f32,
    ev_y: f32,
    line_length: f32,
    aggressive: u32,
}

/// Build the `DrawCmd` list for a z-sorted chunk of judge lines (worker task).
/// Pure function of the shared context — no `&mut` state. Chunks are
/// contiguous in z-order, so concatenating their output reproduces the old
/// single-threaded loop exactly.
fn build_line_cmds<'a>(lines: &[&'a crate::core::LineState], ctx: &SceneCtx<'a>) -> Vec<DrawCmd<'a>> {
    let (letterbox, kx, ev_x, ev_y) = (ctx.letterbox, ctx.kx, ctx.ev_x, ctx.ev_y);
    let mut cmds: Vec<DrawCmd<'a>> = Vec::with_capacity(lines.iter().map(|l| 1 + 3 * l.notes.len()).sum());
    for line in lines {
        if line.pe_hide || line.attach_ui.is_some() { continue; }
        // [E] CtrlObject: ctrl_alpha scales the line's final alpha.
        let line_alpha = line.alpha * line.ctrl_alpha;
        // T * R * S: translate to position, rotate around self, scale
        // [E] CtrlObject: ctrl_pos is a multiplier (phira applies to incline);
        // ctrl_size scales the line.
        let ctrl_px = line.position[0] * CANVAS_W * ev_x;
        let ctrl_py = line.position[1] * CANVAS_H * ev_y;
        let line_m = mat_mul(
            &letterbox,
            &mat_mul(
                &mat_translate(ctrl_px, ctrl_py),
                &mat_mul(
                    &mat_rotate(line.rotation),
                    &mat_scale(line.scale[0] * line.ctrl_size_x, line.scale[1] * line.ctrl_size_y),
                ),
            ),
        );

        // Line quad.
        match &line.texture {
            None => {
                let c = line.color;
                cmds.push(DrawCmd {
                    uniform: DrawUniform {
                            model: mat_mul(&line_m, &mat_scale(LINE_LEN * ctx.line_length / 6.0, LINE_THICK)),
                        color: [c[0], c[1], c[2], c[3] * line_alpha],
                        uv_rect: [0., 0., 1., 1.],
                    },
                    tex: ctx.white,
                });
            }
            Some(name) => {
                let c = line.color;
                if let Some(t) = ctx.textures.get(name) {
                    // [Texture] raw_scale = line.scale / (2/1350). For lines without
                    // scale events, line.scale = 1.0 (identity) but the implicit factor
                    // 2/RPE_WIDTH must still apply → fall back to 2.0.
                    let raw_sx = if (line.scale[0] - 1.0).abs() < 1e-6 { 2.0 } else { line.scale[0] * 675.0 };
                    let raw_sy = if (line.scale[1] - 1.0).abs() < 1e-6 { 2.0 } else { line.scale[1] * 675.0 };
                    let tw = t.size[0] * raw_sx / kx;
                    let th = t.size[1] * raw_sy / kx;
                    let tex_m = mat_mul(
                        &letterbox,
                        &mat_mul(
                            &mat_translate(ctrl_px, ctrl_py),
                            &mat_mul(&mat_rotate(line.rotation), &mat_scale(tw, th)),
                        ),
                    );
                    cmds.push(DrawCmd {
                        uniform: DrawUniform {
                            model: tex_m,
                            color: [c[0], c[1], c[2], c[3] * line_alpha],
                            uv_rect: [0., 0., 1., 1.],
                        },
                        tex: &t.bind_group,
                    });
                } else {
                    // Fallback: draw default white bar if texture not found
                    cmds.push(DrawCmd {
                        uniform: DrawUniform {
                        model: mat_mul(&line_m, &mat_scale(LINE_LEN * ctx.line_length / 6.0, LINE_THICK)),
                            color: [c[0], c[1], c[2], c[3] * line_alpha],
                            uv_rect: [0., 0., 1., 1.],
                        },
                        tex: ctx.white,
                    });
                }
            }
        }

        // Notes follow the line's translation AND rotation, but NOT its
        // scale or alpha (locked product decision).
        let note_m = mat_mul(
            &letterbox,
            &mat_mul(
                &mat_translate(ctrl_px, ctrl_py),
                &mat_rotate(line.rotation),
            ),
        );
        // Notes: below-notes first, above-notes after (contract).
        // Within each group draw by kind priority (prpr NoteKind::order):
        // hold at the bottom, then drag, tap, flick on top.
        for &above in &[false, true] {
            for kind in [2u8, 4, 1, 3] {
                for note in line.notes.iter().filter(|n| n.above == above && n.kind == kind) {
                let rgb = match note.kind {
                    1 => TAP_COLOR,
                    2 => HOLD_COLOR,
                    3 => FLICK_COLOR,
                    4 => DRAG_COLOR,
                    _ => continue,
                };
                // Multi-press hint: prefer the _mh sprite when flagged and
                // loaded, else the normal sprite, else colored quads.
                let (base, mh) = match note.kind {
                    1 => ("note:click", "note:click_mh"),
                    2 => ("note:hold", "note:hold_mh"),
                    3 => ("note:flick", "note:flick_mh"),
                    4 => ("note:drag", "note:drag_mh"),
                    _ => continue,
                };
                // prpr: _mh sprites render wider by mh_width/normal_width
                // (note.rs scale factor), e.g. 1089/989 ≈ 1.10 for click.
                let (sprite, mh_factor, is_mh) = if note.multiple_hint {
                    match (ctx.textures.get(mh), ctx.textures.get(base)) {
                        (Some(m), Some(b)) => (Some(m), m.size[0] / b.size[0], true),
                        (m, b) => (m.or(b), 1.0, false),
                    }
                } else {
                    (ctx.textures.get(base), 1.0, false)
                };
                let alpha = note.alpha;
                // fake notes render at full opacity (character art etc.)
                if alpha <= 0.0 {
                    continue;
                }
                // [F] Incline: perspective X distortion based on note Y position
                let incline_factor = 1.0 - line.incline_sin * note.relative[1] * 0.5;
                // [E] CtrlObject from LineState (evaluated in state_at)
                let ctrl_y = line.ctrl_y;
                let x = note.relative[0] * CANVAS_W * ev_x;
                // [E] ctrl_y scales the note's relative Y position
                let y = note.relative[1] * CANVAS_H * ev_y * ctrl_y;
                let note_base = mat_mul(&note_m, &mat_translate(x, y));

                // [C] Canvas-space culling: the quad's bounding circle vs
                // the playfield box AABB [±CANVAS_W]×[±CANVAS_H]. The
                // letterbox maps that rectangle to the whole window, so
                // anything fully outside it is off-screen — skipping it
                // saves instance upload + rasterization. Conservative
                // for any line rotation (circle encloses the rotated
                // quad). Local (lx, ly) is pre-rotation canvas space.
                let (cxn, cyn) = (line.rotation.cos(), line.rotation.sin());
                let to_canvas = |lx: f32, ly: f32| -> (f32, f32) {
                    (ctrl_px + cxn * lx - cyn * ly, ctrl_py + cyn * lx + cxn * ly)
                };
                let (cx0, cy0) = to_canvas(x, y);
                let outside = |qx: f32, qy: f32, r: f32| {
                    qx + r < -CANVAS_W || qx - r > CANVAS_W || qy + r < -CANVAS_H || qy - r > CANVAS_H
                };

                match (note.kind, sprite) {
                    (1 | 3 | 4, Some(t)) => {
                        let w = NOTE_SPRITE_W * note.scale * mh_factor * incline_factor;
                        let h = w * t.size[1] / t.size[0];
                        if !outside(cx0, cy0, (w * w + h * h).sqrt() * 0.5) {
                            cmds.push(DrawCmd {
                                uniform: DrawUniform {
                                    model: mat_mul(&note_base, &mat_scale(w, h)),
                                    color: [1.0, 1.0, 1.0, alpha],
                                    uv_rect: [0., 0., 1., 1.],
                                },
                                tex: &t.bind_group,
                            });
                        }
                    }
                    // Textured hold: atlas = TAIL (image top) + stretchable
                    // body + HEAD (image bottom). _mh atlas has a taller
                    // head (holdAtlasMH = [50 tail, 95 head]). Textures are
                    // flipped at upload: head = v0, tail = v1. body→head→tail.
                    (2, Some(t)) => {
                        let w = NOTE_SPRITE_W * note.scale * mh_factor * incline_factor;
                        let (tail_px, head_px) = if is_mh { (50.0, 95.0) } else { (HOLD_CAP_PX, HOLD_CAP_PX) };
                        let head_uv = head_px / t.size[1]; // v0-fraction for head
                        let tail_uv = tail_px / t.size[1];
                        let head_h = w * head_px / t.size[0]; // quad heights
                        let tail_h = w * tail_px / t.size[0];
                        let tint = [1.0, 1.0, 1.0, alpha];
                        // [B] chart.rs negates relative[1] for below notes; mirror UV
                        // so the hold body gradient (head→tail) stays correct.
                        let v_at = |v0: f32, len: f32| -> [f32; 4] {
                            if note.above { [0., v0, 1., len] } else { [0., v0 + len, 1., -len] }
                        };
                        // [B] Hold body/head/tail each get below_rot at their position
                        let hd = |cy: f32| mat_mul(&note_m, &mat_mul(
                            &mat_translate(x, cy), &mat_scale(w, w),
                        ));
                        if let Some(end_y) = note.hold_end_y {
                            let y1 = end_y as f32 * CANVAS_H * ev_y * ctrl_y;
                            let (head_y, tail_y) = (y, y1);
                            // [FIX] 头尾偏移:body 从 head 边缘延伸到 tail 边缘,
                            // 否则 body 与头尾中心重叠(中段和头尾重叠 bug)。
                            let (h0, h1) = if tail_y >= head_y {
                                (head_y + head_h * 0.5, tail_y - tail_h * 0.5)
                            } else {
                                (head_y - head_h * 0.5, tail_y + tail_h * 0.5)
                            };
                            let bh = (h1 - h0).abs();
                            if bh > 1e-5 {
                                if ctx.aggressive & AGGRESSIVE_HOLD_CLIP != 0 {
                                    // [C] 过激优化(设置里开):hold body 沿
                                    // 线方向裁剪。线段(head边缘→tail边缘,
                                    // 含线旋转)与画布矩形求交(Liang-Barsky),
                                    // 可见长度用勾股定理 sqrt(dx²+dy²);
                                    // 矩形按半宽膨胀,线在视口外时 body 宽边
                                    // 仍可见。UV 按沿 body 方向的投影比例映射,
                                    // head 端渐变在速度事件下仍锚定 head。
                                    let (ax, ay) = to_canvas(x, h0);
                                    let (bx, by) = to_canvas(x, h1);
                                    let (dx, dy) = (bx - ax, by - ay);
                                    let _len = (dx * dx + dy * dy).sqrt();
                                    let hw = w * 0.5;
                                    if let Some((t0, t1)) = clip_segment(
                                        (ax, ay), (bx, by),
                                        -CANVAS_W - hw, CANVAS_W + hw,
                                        -CANVAS_H - hw, CANVAS_H + hw,
                                    ) {
                                        if t1 - t0 > 1e-6 {
                                            let (x0, y0) = (ax + dx * t0, ay + dy * t0);
                                            let (x1, y1) = (ax + dx * t1, ay + dy * t1);
                                            let vis_len = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
                                            // Visible segment midpoint, rotated
                                            // back into line-local space (the
                                            // quad's local Y axis then aligns
                                            // with the body direction again).
                                            let rel_x = (x0 + x1) * 0.5 - ctrl_px;
                                            let rel_y = (y0 + y1) * 0.5 - ctrl_py;
                                            let lcx = cxn * rel_x + cyn * rel_y;
                                            let lcy = -cyn * rel_x + cxn * rel_y;
                                            let body_len = 1.0 - head_uv - tail_uv;
                                            cmds.push(DrawCmd {
                                                uniform: DrawUniform {
                                                    model: mat_mul(&note_m, &mat_mul(
                                                        &mat_translate(lcx, lcy),
                                                        &mat_scale(w, vis_len),
                                                    )),
                                                    color: tint,
                                                    uv_rect: v_at(head_uv + body_len * t0, body_len * (t1 - t0)),
                                                },
                                                tex: &t.bind_group,
                                            });
                                        }
                                    }
                                } else {
                                    cmds.push(DrawCmd {
                                        uniform: DrawUniform {
                                            model: mat_mul(&hd((h0 + h1) * 0.5), &mat_scale(1.0, bh / w)),
                                            color: tint,
                                            uv_rect: v_at(head_uv, 1. - head_uv - tail_uv),
                                        },
                                        tex: &t.bind_group,
                                    });
                                }
                            }
                            for (cy, v0, len, quad_h) in [
                                (head_y, 0.0, head_uv, head_h),
                                (tail_y, 1.0 - tail_uv, tail_uv, tail_h),
                            ] {
                                // [C] head/tail: bounding-circle cull.
                                if !outside(to_canvas(x, cy).0, to_canvas(x, cy).1, (w * w + quad_h * quad_h).sqrt() * 0.5) {
                                    cmds.push(DrawCmd {
                                        uniform: DrawUniform {
                                            model: mat_mul(&hd(cy), &mat_scale(1.0, quad_h / w)),
                                            color: tint,
                                            uv_rect: v_at(v0, len),
                                        },
                                        tex: &t.bind_group,
                                    });
                                }
                            }
                        } else {
                            if !outside(cx0, cy0, (w * w + head_h * head_h).sqrt() * 0.5) {
                                cmds.push(DrawCmd {
                                    uniform: DrawUniform {
                                        model: mat_mul(&hd(y), &mat_scale(1.0, head_h / w)),
                                        color: tint,
                                        uv_rect: v_at(0., head_uv),
                                    },
                                    tex: &t.bind_group,
                                });
                            }
                        }
                    }
                    // Colored-quad fallback (no sprite loaded).
                    _ => {
                        // [B] below rotation applies to all fallback quads too
                        let nb = || mat_mul(&note_m, &mat_translate(x, y));
                        if note.kind == 2 {
                            if let Some(end_y) = note.hold_end_y {
                            let y1 = end_y as f32 * CANVAS_H * ev_y * ctrl_y;
                                let h = (y1 - y).abs();
                                let wb = HOLD_BODY_W * note.scale * incline_factor;
                                if h > 1e-5 && !outside(to_canvas(x, (y + y1) * 0.5).0, to_canvas(x, (y + y1) * 0.5).1, (wb * wb + h * h).sqrt() * 0.5) {
                                    cmds.push(DrawCmd {
                                        uniform: DrawUniform {
                                            model: mat_mul(&nb(), &mat_scale(wb, h)),
                                            color: [rgb[0], rgb[1], rgb[2], alpha],
                                            uv_rect: [0., 0., 1., 1.],
                                        },
                                        tex: ctx.white,
                                    });
                                }
                            }
                        }
                        let wq = NOTE_W * note.scale * incline_factor;
                        let hq = NOTE_H * note.scale;
                        if !outside(cx0, cy0, (wq * wq + hq * hq).sqrt() * 0.5) {
                            cmds.push(DrawCmd {
                                uniform: DrawUniform {
                                    model: mat_mul(&nb(), &mat_scale(wq, hq)),
                                    color: [rgb[0], rgb[1], rgb[2], alpha],
                                    uv_rect: [0., 0., 1., 1.],
                                },
                                tex: ctx.white,
                            });
                        }
                    }
                }
            }
        }
        }
    }
    cmds
}

/// Convert `DrawCmd`s to GPU instances, applying the global rgb `dim` on the
/// CPU (mirrors the old inline `instances.extend(cmds.iter().map(...))`).
/// 字段级 HUD 文本缓存:值没变返回上次的 `&str`(零分配),变了才重建。
fn hud_fmt<T: PartialEq>(cache: &mut (T, String), v: T, f: impl FnOnce(&T) -> String) -> &str {
    if cache.0 != v {
        cache.1 = f(&v);
        cache.0 = v;
    }
    &cache.1
}

fn to_instances(cmds: &[DrawCmd<'_>], dim: f32) -> Vec<Instance> {
    cmds.iter()
        .map(|cmd| {
            let u = &cmd.uniform;
            Instance {
                model: instance_model(&u.model),
                color: [u.color[0] * dim, u.color[1] * dim, u.color[2] * dim, u.color[3]],
                uv_rect: u.uv_rect,
            }
        })
        .collect()
}

/// PMCORE-73: one worker's full scene slice — build the chunk's cmds +
/// instances, upload them to a chunk-local vertex buffer, and record the
/// scene render pass into `scene_view` as its own command buffer. Only chunk
/// 0 clears the target (and draws the opaque background quad); later chunks
/// `Load` on top. Submitting the buffers in order therefore reproduces the
/// old single-pass output byte-for-byte, and `queue.submit` sees multiple
/// command buffers in one call.
#[allow(clippy::too_many_arguments)]
fn encode_scene_chunk<'a>(
    chunk: &[&'a crate::core::LineState],
    ctx: &SceneCtx<'a>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::RenderPipeline,
    opaque: &wgpu::RenderPipeline,
    scene_view: &wgpu::TextureView,
    first: bool,
    bg: Option<(Mat3, [f32; 4], &'a wgpu::BindGroup)>,
    bg_instance: Option<Instance>,
    timers: Option<&GpuTimers>,
    dim: f32,
) -> wgpu::CommandBuffer {
    // Black 30 % overlay sits under every judge line; it is the first scene
    // cmd (chunk 0 only — the background quad owns instance 0).
    let mut cmds: Vec<DrawCmd<'a>> = Vec::with_capacity(
        chunk.iter().map(|l| 1 + 3 * l.notes.len()).sum::<usize>() + usize::from(bg.is_some()),
    );
    if let Some((bg_m, _, _)) = &bg {
        cmds.push(DrawCmd {
            uniform: DrawUniform { model: *bg_m, color: [0., 0., 0., 0.3], uv_rect: [0., 0., 1., 1.] },
            tex: ctx.white,
        });
    }
    cmds.extend(build_line_cmds(chunk, ctx));
    // 单遍构建:先推背景实例再 extend,避免 to_instances 后的 insert(0) 全量移位。
    let mut instances: Vec<Instance> = Vec::with_capacity(cmds.len() + usize::from(bg_instance.is_some()));
    if let Some(bgi) = bg_instance {
        instances.push(bgi);
    }
    instances.extend(to_instances(&cmds, dim));
    // Chunk-local buffer: created per frame on the worker (wgpu drops it once
    // the submitted command buffer finishes executing). Small (≤ a few 100 KB)
    // and one-shot, so no pooling bookkeeping is worth it. Size ≥ 1 even for
    // fully-culled chunks (wgpu rejects zero-size buffers).
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("scene-chunk-instances"),
        size: ((instances.len() as u64) * size_of::<Instance>() as u64).max(1),
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    if !instances.is_empty() {
        queue.write_buffer(&buf, 0, bytemuck::cast_slice(&instances));
    }
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("scene-chunk") });
    // GPU 计时 t0:scene pass 起点(chunk 0 的 cb 最先提交,先于其余 chunk)。
    if let Some(t) = timers {
        encoder.write_timestamp(&t.query_set, 0);
    }
    let base = usize::from(bg.is_some());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: scene_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // Chunk 0 always clears (even without a background image);
                    // later chunks `Load` the previous chunks' output.
                    load: if first {
                        wgpu::LoadOp::Clear(wgpu::Color::BLACK)
                    } else {
                        wgpu::LoadOp::Load
                    },
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(pipeline);
        pass.set_vertex_buffer(0, buf.slice(..));
        // [C] Background sits at instance 0 (fully opaque, no-blend pass);
        // cmds follow at offset 1 with the alpha pipeline. The draw loop
        // keeps cmd ordering (translucent layering stays correct).
        if base == 1 {
            pass.set_pipeline(opaque);
            pass.set_bind_group(0, bg.expect("base==1 implies bg").2, &[]);
            pass.draw(0..4, 0..1);
        }
        pass.set_pipeline(pipeline);
        let mut start = 0usize;
        for i in 1..=cmds.len() {
            if i == cmds.len() || !std::ptr::eq(cmds[i].tex, cmds[start].tex) {
                pass.set_bind_group(0, cmds[start].tex, &[]);
                pass.draw(0..4, (start + base) as u32..(i + base) as u32);
                start = i;
            }
        }
    }
    encoder.finish()
}

const SHADER: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsIn {
    @location(0) m01: vec4<f32>,  // a, b, c, d
    @location(1) m2p: vec4<f32>,  // tx, ty, pad, pad
    @location(2) color: vec4<f32>,
    @location(3) uv_rect: vec4<f32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
};

// Unit quad (±0.5) as a triangle strip; one instance per quad.
@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: VsIn) -> VsOut {
    let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let p = corner - 0.5;
    let world = vec2<f32>(
        inst.m01.x * p.x + inst.m01.z * p.y + inst.m2p.x,
        inst.m01.y * p.x + inst.m01.w * p.y + inst.m2p.y,
    );
    var out: VsOut;
    out.pos = vec4<f32>(world, 0.0, 1.0);
    out.uv = inst.uv_rect.xy + corner * inst.uv_rect.zw;
    out.color = inst.color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv) * in.color;
}
"#;

/// Shared pipeline setup for the window and offscreen paths: quad shader,
/// texture bind-group layout + sampler, alpha-blended TriangleStrip instanced
/// quads. `format` is the color target (surface format or offscreen Rgba8Unorm).
///
/// The vertex shader expects per-instance attributes matching [`Instance`] layout:
/// `@location(0)`: `m01` (linear part `[a, b, c, d]`),
/// `@location(1)`: `m2p` (translation `[tx, ty, 0, 0]`),
/// `@location(2)`: `color` (RGBA),
/// `@location(3)`: `uv_rect` (`[u0, v0, du, dv]`).
/// See the `VsIn` struct inside `SHADER` for the WGSL definition.
/// Returns (alpha-blended pipeline, no-blend pipeline, texture BGL, sampler).
/// The no-blend variant is for fully opaque quads (background image): it
/// skips the blending unit entirely.
pub(crate) fn create_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    cache: Option<&wgpu::PipelineCache>,
) -> (wgpu::RenderPipeline, wgpu::RenderPipeline, wgpu::BindGroupLayout, wgpu::Sampler) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("phimakor-quad"),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SHADER)),
    });

    let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&tex_bgl)],
        immediate_size: 0,
    });
    let make = |blend: Option<wgpu::BlendState>| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("phimakor-quad"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(INSTANCE_LAYOUT)],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache,
        })
    };
    let pipeline = make(Some(wgpu::BlendState::ALPHA_BLENDING));
    let opaque_pipeline = make(None);

    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: None,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    (pipeline, opaque_pipeline, tex_bgl, sampler)
}

/// wgpu-based renderer driving the main window (or offscreen preview).
/// Holds the device, queue, pipelines, double-buffered instance storage,
/// texture cache, background image, post-processing pipe, and UI overlay state.
/// Screen-space pause button rect (window px, top-left origin), recorded
/// while drawing the HUD so the main loop can hit-test clicks.
#[derive(Clone, Copy, Default)]
pub struct PauseHitRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Per-frame HUD (Phigros-style game UI): song name, difficulty, score,
/// combo and pause button. Drawn inside the render pipeline so exports
/// (readback / embedded engine) include them. When an `attachUI` line
/// matches an element ("name"/"level"/"score"/"combo"/"pause"), the element
/// follows that line's transform (position/rotation/alpha); otherwise the
/// element sits at its default Phigros layout corner.
#[derive(Clone, Default)]
pub struct HudData {
    pub chart_name: String,
    pub difficulty: String,
    pub score: u32,
    pub combo: u32,
    /// Paused state: swaps the pause icon (II) for the play icon (▶).
    pub paused: bool,
    /// Whether the HUD is visible at all (false = editor panels hide it).
    pub visible: bool,
    /// 帧时间叠层(ms):编辑器视口隐藏时 HUD 仍显示,帧时间不能跟着
    /// 编辑器 timeline 层消失——画在 HUD 右上角。
    pub frame_ms: f32,
    pub fps: f32,
}

pub struct Renderer {
    /// Root instance (kept for `generate_report` memory diagnostics).
    instance: wgpu::Instance,
    /// Window surface; `None` for surfaceless (offscreen preview) renderers.
    surface: Option<wgpu::Surface<'static>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Present configuration; `None` when surfaceless.
    config: Option<wgpu::SurfaceConfiguration>,
    /// Viewport size in px (drives the text overlay layout).
    size: [u32; 2],

    pipeline: wgpu::RenderPipeline,
    /// No-blend variant for fully opaque quads (background image).
    opaque_pipeline: wgpu::RenderPipeline,
    tex_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,

    /// Double-buffered instance storage; frames alternate so an in-flight
    /// frame's buffer is never overwritten by `queue.write_buffer`.
    instance_bufs: [wgpu::Buffer; 2],
    instance_capacity: usize, // in instances
    frame_idx: usize,         // ping-pong 0/1

    white: wgpu::BindGroup, // 1×1 white, for all solid quads
    /// 64×64 white disc (anti-aliased), for the pause button backdrop.
    disc: wgpu::BindGroup,
    /// 64×64 white play triangle (▶), shown while paused.
    play_tri: wgpu::BindGroup,
    /// 纹理压缩开关(设置里可关):大纹理超 [`MAX_TEXTURE_DIM`] 时
    /// Lanczos3 降采样 + BC3 块压缩(显存/带宽 4:1,有损)。
    pub texture_compress: bool,
    /// 设备是否支持 BC 压缩纹理(不支持时压缩自动回退 RGBA8)。
    bc_supported: bool,
    textures: HashMap<String, TexEntry>,
    background: Option<(wgpu::BindGroup, [f32; 2])>,
    background_dim: f32,
    /// Playfield canvas aspect (w/h), hotkey-switchable.
    playfield_aspect: f32,
    /// Judge-line length multiplier (info.yml `lineLength`, default 6.0).
    line_length: f32,
    /// Playback progress 0..1 for the top progress bar.
    progress: f32,
    /// Phigros-style HUD (song/difficulty/score/combo/pause) drawn each frame.
    hud: HudData,
    /// HUD 数字文本字段级缓存:(上次值, 文本)。值没变则复用 String,
    /// 免去每帧 format! 分配(score/combo 只在命中时变化)。
    hud_score_txt: (u32, String),
    hud_combo_txt: (u32, String),
    /// Screen rect of the pause button (window px), for hit-testing.
    pause_rect: PauseHitRect,
    pub vsync: bool,
    /// 过激优化位标志(设置里开):见 `ui::settings::AGGRESSIVE_*`。
    pub aggressive: u32,
    /// Present modes the surface/adapter supports (for vsync-off Mailbox
    /// fallback); empty for surfaceless renderers.
    present_modes: Vec<wgpu::PresentMode>,
    /// Post-processing pipeline (effects from extra.json).
    pub post: post::PostPipe,
    /// Intermediate scene texture (for post-processing).
    scene_tex: Option<wgpu::Texture>,
    scene_view: Option<wgpu::TextureView>,

    /// 当前帧命中特效触发点 `(谱面时间 t0, 画布位置)`——由 host 每帧
    /// 从 `Chart::fx_in_window` 查询生成(纯时间函数,见 `fx.rs`)。
    frame_fx: Vec<(f64, [f32; 2])>,
    /// Text overlay state (see `text.rs`).
    text: text::TextState,
    /// Persistent single-instance buffer for UI overlay draws.
    ui_inst_buf: wgpu::Buffer,
    /// 可选 GPU 计时(env PHIMAKOR_GPU_TIMING=1 启用,默认 None 零开销)。
    gpu_timers: Option<GpuTimers>,
    /// 管线缓存(PMCORE-68):所有 create_render_pipeline 共享的种子/收集器。
    /// `None` = 禁用开关或设备不支持;懒加载特效管线经 post 方法带引用。
    pipeline_cache: Option<wgpu::PipelineCache>,
}

/// GPU 渲染耗时计时:每帧两个 timestamp(scene pass 起止),resolve 后读回。
/// 测量的是 GPU 实际执行时间(不含 CPU 提交/上传)。
struct GpuTimers {
    query_set: wgpu::QuerySet,
    resolve_buf: wgpu::Buffer,
    readback_buf: wgpu::Buffer,
    /// 上一帧 GPU 耗时(秒,由 poll_timing 更新)。
    last_ms: f32,
}

impl GpuTimers {
    fn new(device: &wgpu::Device) -> Self {
        let query_set = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("gpu-timers"),
            ty: wgpu::QueryType::Timestamp,
            count: 2,
        });
        let resolve_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu-timers-resolve"),
            size: 16,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu-timers-readback"),
            size: 16,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self { query_set, resolve_buf, readback_buf, last_ms: 0.0 }
    }

    /// 读回上一帧 GPU 耗时(阻塞到可用;返回 ms)。
    /// timestamp 单位为 GPU 时钟周期,需 × get_timestamp_period 换算。
    fn poll(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> f32 {
        let slice = self.readback_buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        if let Ok(data) = slice.get_mapped_range() {
            let t0 = u64::from_le_bytes(data[0..8].try_into().unwrap_or_default());
            let t1 = u64::from_le_bytes(data[8..16].try_into().unwrap_or_default());
            let period = queue.get_timestamp_period() as f64; // 周期 → 纳秒
            self.last_ms = (t1.saturating_sub(t0)) as f32 * period as f32 * 1e-6; // ns → ms
        }
        self.readback_buf.unmap();
        self.last_ms
    }
}

/// 后台预压缩的纹理数据(load_chart_async 线程产出,主线程只做 GPU 上传)。
/// 纯 CPU 步骤(尺寸限制 + Lanczos 降采样 + BC3 块压缩)全在后台,
/// 避免切谱时主线程被大图压缩卡住。
#[derive(Clone)]
pub struct PreparedTex {
    /// BC3 块(compressed)或 RGBA8 像素。
    pub data: Vec<u8>,
    /// 处理后的尺寸(压缩时 4 对齐)。
    pub w: u32,
    pub h: u32,
    /// data 是否为 BC3 块(否则 RGBA8)。
    pub compressed: bool,
    /// 渲染语义尺寸 = 原始像素(降采样不改视觉)。
    pub orig_w: u32,
    pub orig_h: u32,
}

impl Renderer {
    /// z 层遮罩渲染:只画 z_order ∈ range 的线(长条)+ 音符(方块),
    /// 全部白色不透明——特效混合用的形状遮罩(targetRange 语义)。
    /// 简化几何:线 = 线 quad,音符 = 方块(不拆 hold 三段,形状够用)。
    fn render_mask(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        window_aspect: f32,
        frame: &crate::core::FrameState,
        range: (f32, f32),
        mask_view: &wgpu::TextureView,
    ) {
        let aspect = self.playfield_aspect;
        let (letterbox, _kx, _ky, ev_x, ev_y) = letterbox_transform(window_aspect, aspect);
        let mut inst: Vec<Instance> = Vec::new();
        let white = [1.0, 1.0, 1.0, 1.0];
        for line in frame.lines.iter().filter(|l| {
            let z = l.z_order as f32;
            z >= range.0 && z <= range.1
        }) {
            if line.pe_hide || line.attach_ui.is_some() {
                continue;
            }
            let ctrl_px = line.position[0] * CANVAS_W * ev_x;
            let ctrl_py = line.position[1] * CANVAS_H * ev_y;
            let line_m = mat_mul(
                &letterbox,
                &mat_mul(
                    &mat_translate(ctrl_px, ctrl_py),
                    &mat_mul(
                        &mat_rotate(line.rotation),
                        &mat_scale(line.scale[0] * line.ctrl_size_x, line.scale[1] * line.ctrl_size_y),
                    ),
                ),
            );
            inst.push(Instance {
                model: instance_model(&mat_mul(&line_m, &mat_scale(LINE_LEN * self.line_length / 6.0, LINE_THICK))),
                color: white,
                uv_rect: [0., 0., 1., 1.],
            });
            let note_m = mat_mul(&letterbox, &mat_mul(&mat_translate(ctrl_px, ctrl_py), &mat_rotate(line.rotation)));
            for n in &line.notes {
                if n.alpha <= 0.0 {
                    continue;
                }
                let x = n.relative[0] * CANVAS_W * ev_x;
                let y = n.relative[1] * CANVAS_H * ev_y;
                let w = NOTE_SPRITE_W * n.scale;
                let h = NOTE_H * n.scale.max(1.0);
                inst.push(Instance {
                    model: instance_model(&mat_mul(&note_m, &mat_mul(&mat_translate(x, y), &mat_scale(w, h)))),
                    color: white,
                    uv_rect: [0., 0., 1., 1.],
                });
            }
        }
        if inst.is_empty() {
            return; // 遮罩保持上一帧内容(本帧无目标层)→ 混合无效果
        }
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mask-instances"),
            size: (inst.len() * size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buf, 0, bytemuck::cast_slice(&inst));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene-mask"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: mask_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, buf.slice(..));
        pass.set_bind_group(0, &self.white, &[]);
        pass.draw(0..4, 0..inst.len() as u32);
    }

    /// Borrow the wgpu device.
    pub fn device(&self) -> &wgpu::Device { &self.device }
    /// Borrow the wgpu queue.
    pub fn queue(&self) -> &wgpu::Queue { &self.queue }
    /// Borrow the texture bind-group layout (shared by all textures).
    pub fn tex_bgl(&self) -> &wgpu::BindGroupLayout { &self.tex_bgl }
    /// Borrow the shared linear sampler.
    pub fn sampler(&self) -> &wgpu::Sampler { &self.sampler }

    /// 场景 pass 的 GPU 耗时(ms)。仅 PHIMAKOR_GPU_TIMING=1 时有效;
    /// 内部 device.poll(Wait) 阻塞——只用于诊断(perf 模式)。
    pub fn gpu_frame_ms(&mut self) -> Option<f32> {
        self.gpu_timers.as_mut().map(|g| g.poll(&self.device, &self.queue))
    }

    /// Acquire the next surface texture. Returns `Err` variants that the caller
    /// should handle (Timeout/Occluded → skip frame, Validation → propagate).
    pub fn surface_acquire(&mut self) -> Result<wgpu::SurfaceTexture, wgpu::CurrentSurfaceTexture> {
        let _s = crate::trace_span!("surface_acquire");
        let surface = self.surface.as_ref().expect("surface_acquire on a surfaceless renderer");
        match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(st) | wgpu::CurrentSurfaceTexture::Suboptimal(st) => Ok(st),
            other => Err(other),
        }
    }

    /// Create a new windowed renderer: surface, adapter, device, pipelines,
    /// double-buffered instance storage, white texture, and post-processing pipe.
    /// `backends` selects the wgpu backend (from the `backend` setting;
    /// [`wgpu::Backends::all`] for auto). On shared-memory GPUs (iGPU) the
    /// DX12 backend's GPU heaps live in system RAM — Vulkan/GL can be far
    /// cheaper there.
    pub async fn new(window: Arc<Window>, backends: wgpu::Backends) -> anyhow::Result<Self> {
        let size = window.inner_size();
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        desc.backends = backends;
        let instance = wgpu::Instance::new(desc);
        let surface = instance
            .create_surface(window)
            .context("failed to create surface")?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .context("no suitable GPU adapter")?;

        let caps = surface.get_capabilities(&adapter);
        // Prefer a non-sRGB format so authored sRGB values pass through
        // unchanged (prpr colors are display-ready sRGB).
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            // 2 buffers max: 4 frames of swapchain at 4K ≈ 100+ MB of GPU
            // memory accounted against the process — 2 is plenty for vsync.
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
        };
        let (width, height) = (config.width, config.height);
        Self::init(instance, Some((surface, config)), adapter, format, width, height, caps.present_modes).await
    }

    /// Surfaceless constructor for offscreen rendering (preview / embedding).
    /// No compatible surface on the adapter request; the target format is
    /// fixed to non-sRGB Rgba8Unorm, matching the window path's pass-through
    /// color choice.
    pub(crate) async fn new_surfaceless(width: u32, height: u32) -> anyhow::Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .context("no suitable GPU adapter")?;
        Self::init(instance, None, adapter, wgpu::TextureFormat::Rgba8Unorm, width.max(1), height.max(1), vec![]).await
    }

    /// Shared construction for the window and surfaceless paths: device/queue,
    /// pipeline (via [`create_pipeline`]), instance double-buffer, white
    /// texture. A passed-in surface is configured with its config here.
    async fn init(
        instance: wgpu::Instance,
        surface: Option<(wgpu::Surface<'static>, wgpu::SurfaceConfiguration)>,
        adapter: wgpu::Adapter,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        present_modes: Vec<wgpu::PresentMode>,
    ) -> anyhow::Result<Self> {
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: None,
                required_features: {
                    let mut f = wgpu::Features::empty();
                    if std::env::var("PHIMAKOR_GPU_TIMING").is_ok() {
                        f |= wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
                    }
                    // BC 纹理压缩(texpresso 编码 BC3):适配器支持才请求,
                    // 否则纹理压缩开关自动失效(RGBA8 回退)。
                    if adapter.features().contains(wgpu::Features::TEXTURE_COMPRESSION_BC) {
                        f |= wgpu::Features::TEXTURE_COMPRESSION_BC;
                    }
                    // 管线缓存(PMCORE-68):支持才请求;禁用开关时与现状完全一致。
                    if pipeline_cache::enabled()
                        && adapter.features().contains(wgpu::Features::PIPELINE_CACHE)
                    {
                        f |= wgpu::Features::PIPELINE_CACHE;
                    }
                    f
                },
                required_limits: wgpu::Limits::default(),
                experimental_features: wgpu::ExperimentalFeatures::default(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::Off,
            })
            .await
            .context("failed to create device")?;

        let (surface, config) = match surface {
            Some((surface, config)) => {
                surface.configure(&device, &config);
                (Some(surface), Some(config))
            }
            None => (None, None),
        };

        // PMCORE-68:管线缓存持久化。种子只来自本进程上次 get_data() 的
        // 写盘输出;wgpu 内部校验 magic/version/backend/adapter/validation
        // key,不匹配由 fallback:true 静默回退为空缓存,不崩溃。
        let pipeline_cache = if pipeline_cache::enabled()
            && device.features().contains(wgpu::Features::PIPELINE_CACHE)
        {
            let seed = pipeline_cache::load_seed();
            // SAFETY: data 仅来自上一次 PipelineCache::get_data() 的产物
            // (本进程或本应用先前运行的写盘输出),满足
            // create_pipeline_cache 的 safety 条件;外部篡改/换 GPU/换驱动
            // 由 wgpu 头校验 + fallback:true 兜底,无效数据不会被执行。
            let cache = unsafe {
                device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor {
                    label: Some("phimakor-pipeline-cache"),
                    data: seed.as_deref(),
                    fallback: true,
                })
            };
            Some(cache)
        } else {
            None
        };

        let (pipeline, opaque_pipeline, tex_bgl, sampler) =
            create_pipeline(&device, format, pipeline_cache.as_ref());

        let instance_bufs = [
            Self::make_instance_buf(&device, INITIAL_DRAW_CAPACITY),
            Self::make_instance_buf(&device, INITIAL_DRAW_CAPACITY),
        ];

        let white_tex = Self::create_texture(&device, &queue, &[255; 4], 1, 1);
        let white = Self::texture_bind_group(&device, &tex_bgl, &sampler, &white_tex);
        // 64×64 anti-aliased white disc (pause button backdrop / play dot).
        let disc = {
            const N: usize = 64;
            let mut px = [0u8; N * N * 4];
            let c = (N as f32 - 1.0) * 0.5;
            for y in 0..N {
                for x in 0..N {
                    let d = (((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt() - c) / 2.0;
                    let a = (0.5 - d).clamp(0.0, 1.0);
                    let i = (y * N + x) * 4;
                    px[i] = 255; px[i + 1] = 255; px[i + 2] = 255;
                    px[i + 3] = (a * 255.0) as u8;
                }
            }
            let tex = Self::create_texture(&device, &queue, &px, N as u32, N as u32);
            Self::texture_bind_group(&device, &tex_bgl, &sampler, &tex)
        };
        // 64×64 anti-aliased white play triangle (▶), shown while paused.
        let play_tri = {
            const N: usize = 64;
            let mut px = [0u8; N * N * 4];
            // Triangle: top-left (8,10), bottom-left (8,54), right tip (58,32).
            // Edge SDF: inside = x >= 8 && y between the two slanted edges.
            let (x0, yt, yb, xt) = (8.0f32, 10.0f32, 54.0f32, 58.0f32);
            for y in 0..N {
                for x in 0..N {
                    let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                    if fx < x0 || fy < yt || fy > yb { continue; }
                    let top_slope = (32.0 - yt) / (xt - x0);   // dy/dx of top edge
                    let bot_slope = (32.0 - yb) / (xt - x0);   // dy/dx of bottom edge
                    let top_y = yt + (fx - x0) * top_slope;
                    let bot_y = yb + (fx - x0) * bot_slope;
                    if fy < top_y || fy > bot_y { continue; }
                    // AA: distance to the nearest edge (in px).
                    let d = ((fx - x0).min(fy - top_y).min(bot_y - fy)).min(1.0).max(0.0);
                    let a = d;
                    let i = (y * N + x) * 4;
                    px[i] = 255; px[i + 1] = 255; px[i + 2] = 255;
                    px[i + 3] = (a * 255.0) as u8;
                }
            }
            let tex = Self::create_texture(&device, &queue, &px, N as u32, N as u32);
            Self::texture_bind_group(&device, &tex_bgl, &sampler, &tex)
        };

        let post = post::PostPipe::new(&device, width, height, format, pipeline_cache.as_ref());
        let bc_supported = device.features().contains(wgpu::Features::TEXTURE_COMPRESSION_BC);
        let scene_tex = Some(post::PostPipe::make_target2(&device, width, height, "scene", format));
        let scene_view = Some(scene_tex.as_ref().unwrap().create_view(&wgpu::TextureViewDescriptor::default()));
        let ui_inst_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-inst"),
            size: std::mem::size_of::<Instance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let gpu_timers = if std::env::var("PHIMAKOR_GPU_TIMING").is_ok() {
            Some(GpuTimers::new(&device))
        } else {
            None
        };
        // 启动期管线(quad×2 + blit)已建完:立即写回缓存,首次启动落盘。
        if let Some(c) = &pipeline_cache {
            pipeline_cache::save(c);
        }
        Ok(Self {
            instance,
            surface,
            device,
            queue,
            config,
            size: [width, height],
            pipeline,
            opaque_pipeline,
            tex_bgl,
            sampler,
            instance_bufs,
            instance_capacity: INITIAL_DRAW_CAPACITY,
            frame_idx: 0,
            white,
            disc,
            play_tri,
            texture_compress: true,
            bc_supported,
            textures: HashMap::new(),
            pipeline_cache,
            gpu_timers,
            background: None,
            background_dim: 1.0,
            playfield_aspect: ASPECT,
            line_length: 6.0,
            progress: 0.0,
            hud: HudData::default(),
            hud_score_txt: (0, String::new()),
            hud_combo_txt: (0, String::new()),
            pause_rect: PauseHitRect::default(),
            post,
            scene_tex,
            scene_view,
            frame_fx: Vec::new(),
            text: text::TextState::new(),
            vsync: true,
            aggressive: 0,
            present_modes,
            ui_inst_buf,
        })
    }

    fn make_instance_buf(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (capacity * size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// 纹理尺寸上限(px):超出时 Lanczos3 降采样(抗锯齿)。
    pub const MAX_TEXTURE_DIM: u32 = 2048;

    /// 压缩对齐的纯函数:尺寸限制到 [`MAX_TEXTURE_DIM`],BC3 块压缩时
    /// 对齐到 4 的倍数。`compress=false` 只限制不缩放非 4 倍尺寸。
    pub fn fit_texture_dim(w: u32, h: u32, compress: bool) -> (u32, u32) {
        let scale = Self::MAX_TEXTURE_DIM as f32 / w.max(h).max(1) as f32;
        let (mut nw, mut nh) = if scale < 1.0 {
            (((w as f32 * scale).max(1.0)).round() as u32, ((h as f32 * scale).max(1.0)).round() as u32)
        } else {
            (w, h)
        };
        if compress {
            nw &= !3;
            nh &= !3;
            // [1~3px 纹理] 对齐后可能变 0×0:create_texture(0) 会 wgpu
            // Validation Error panic(PMCORE-71 1×1 PNG 案例)。钳回 1。
            nw = nw.max(1);
            nh = nh.max(1);
        }
        (nw, nh)
    }

    /// 当前纹理压缩开关(设置 + 设备能力)。后台线程据此预压缩。
    pub fn compress_enabled(&self) -> bool {
        self.texture_compress && self.bc_supported
    }

    /// 纯 CPU 纹理准备(fit + Lanczos 降采样 + BC3 压缩),可在后台线程
    /// 调用。`compress=false` 时只做尺寸限制。子 4px 纹理压缩对齐后
    /// 尺寸 < 块宽,wgpu 会 Validation Error——自动回退 RGBA8。
    pub fn prepare_texture(rgba: &[u8], w: u32, h: u32, compress: bool) -> PreparedTex {
        let (nw, nh) = Self::fit_texture_dim(w, h, compress);
        // 尺寸限制:超上限降采样(抗锯齿)。
        let data: Vec<u8> = if nw != w || nh != h {
            let img = image::RgbaImage::from_raw(w, h, rgba.to_vec()).unwrap_or_default();
            image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3).into_raw()
        } else {
            rgba.to_vec()
        };
        if compress && nw >= 4 && nh >= 4 {
            // BC3(DXT5):RGBA 块压缩,4:1。API:Format::compress(texpresso 2.x)。
            let mut blocks = vec![0u8; texpresso::Format::Bc3.compressed_size(nw as usize, nh as usize)];
            texpresso::Format::Bc3.compress(&data, nw as usize, nh as usize, texpresso::Params::default(), &mut blocks);
            PreparedTex { data: blocks, w: nw, h: nh, compressed: true, orig_w: w, orig_h: h }
        } else {
            PreparedTex { data, w: nw, h: nh, compressed: false, orig_w: w, orig_h: h }
        }
    }

    /// 上传 RGBA8 到 GPU 纹理(统一入口):
    /// - 超 [`MAX_TEXTURE_DIM`] → Lanczos3 降采样(抗锯齿,无条件)
    /// - `texture_compress` 开 → BC3 块压缩(Bc3RgbaUnorm,显存/带宽 4:1)
    /// - 关 → RGBA8 原样
    /// 返回 (bind group, 实际尺寸)。
    fn upload_texture_rgba(&mut self, rgba: &[u8], w: u32, h: u32) -> (wgpu::BindGroup, [f32; 2]) {
        let compress = self.texture_compress && self.bc_supported;
        let prepared = Self::prepare_texture(rgba, w, h, compress);
        self.upload_prepared(&prepared)
    }

    /// 主线程同步写纹理(后台预压缩结果):仅 GPU 创建/上传,不做 CPU 重活。
    /// 返回 (bind group, 渲染语义尺寸 = 原始像素)。
    fn upload_prepared(&mut self, tex: &PreparedTex) -> (wgpu::BindGroup, [f32; 2]) {
        let PreparedTex { data, w, h, compressed, .. } = tex;
        let texture = if *compressed {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: *w, height: *h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bc3RgbaUnorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let block_w = w.div_ceil(4);
            let block_h = h.div_ceil(4);
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(block_w * 16),
                    rows_per_image: Some(block_h),
                },
                wgpu::Extent3d { width: *w, height: *h, depth_or_array_layers: 1 },
            );
            tex
        } else {
            Self::create_texture(&self.device, &self.queue, data, *w, *h)
        };
        let bind_group = Self::texture_bind_group(&self.device, &self.tex_bgl, &self.sampler, &texture);
        // 渲染语义尺寸 = 原始像素尺寸(纹理线宽 = 像素 × 系数,降采样是
        // 性能优化,不应改变视觉——否则大线纹理降采样后判定线变窄)。
        (bind_group, [tex.orig_w as f32, tex.orig_h as f32])
    }

    fn create_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> wgpu::Texture {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        texture
    }

    fn texture_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        texture: &wgpu::Texture,
    ) -> wgpu::BindGroup {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }

    fn upload_image(&mut self, bytes: &[u8]) -> anyhow::Result<(wgpu::BindGroup, [f32; 2])> {
        let mut img = image::load_from_memory(bytes)
            .context("failed to decode image")?
            .to_rgba8();
        // wgpu v=0 is the first uploaded row; flip so v=0 is the image TOP,
        // keeping shader UV math (incl. hold atlas) in image space.
        image::imageops::flip_vertical_in_place(&mut img);
        let (w, h) = (img.width().max(1), img.height().max(1));
        // 尺寸限制 + 可选 BC3 压缩(与 load_texture_rgba 同路径)。
        let (bind_group, size) = self.upload_texture_rgba(img.as_raw(), w, h);
        Ok((bind_group, size))
    }

    fn reconfigure(&self) {
        if let (Some(surface), Some(config)) = (&self.surface, &self.config) {
            surface.configure(&self.device, config);
        }
    }

    /// 设置当前帧的命中特效触发点列表 `(谱面时间 t0, 画布位置)`。
    /// 由 host 每帧从 `Chart::fx_in_window` 查询生成——fx 是纯时间函数,
    /// 前进/倒退/跳转都按当前谱面时间渲染对应帧。
    pub fn set_frame_fx(&mut self, fx: Vec<(f64, [f32; 2])>) {
        self.frame_fx = fx;
    }

    /// Viewport size in pixels.
    pub fn size(&self) -> [u32; 2] { self.size }

    /// 预热全部内置特效 pipeline(启动/切谱加载屏调用)。
    /// 消除"特效首次出现时惰性编译卡帧"。
    pub fn warmup_effects(&mut self) {
        self.post.start_warmup();
        let _ = self.post.tick_warmup(&self.device, 4096, self.pipeline_cache.as_ref());
        self.save_pipeline_cache();
    }

    /// 预热当前谱目录的全部自定义 WGSL shader(切谱加载完成后调用)。
    /// 自定义特效首次激活时的同步编译(磁盘 I/O + pipeline 编译 tens of
    /// ms)是运行时尖峰源——编译挪到切谱瞬间一次性完成。
    pub fn warmup_custom_effects(&mut self) {
        self.post.warmup_custom(&self.device, self.pipeline_cache.as_ref());
        self.save_pipeline_cache();
    }

    /// 写回管线缓存(get_data → 原子写 tmp+rename)。失败只告警不阻塞;
    /// 由 init/预热/析构调用,永不在稳态帧循环里触发磁盘写。
    fn save_pipeline_cache(&self) {
        if let Some(cache) = &self.pipeline_cache {
            pipeline_cache::save(cache);
        }
    }

    /// Enable or disable V-sync (reconfigures the surface present mode).
    ///
    /// With V-sync off, prefer `Mailbox` when the surface supports it — it
    /// latches the newest completed frame at the next vblank (triple
    /// buffering): no CPU stall, no tearing, low latency. Fall back to the
    /// driver default `AutoNoVsync` where Mailbox is unavailable.
    pub fn set_vsync(&mut self, enabled: bool) {
        self.vsync = enabled;
        if let Some(config) = &mut self.config {
            config.present_mode = if enabled {
                wgpu::PresentMode::AutoVsync
            } else if self.present_modes.contains(&wgpu::PresentMode::Mailbox) {
                wgpu::PresentMode::Mailbox
            } else {
                wgpu::PresentMode::AutoNoVsync
            };
            self.reconfigure();
        }
    }

    /// Resize the viewport, surface, and post-processing targets.
    /// Ignores zero dimensions (minimised window).
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return; // minimized; keep last valid config
        }
        self.size = [width, height];
        self.post.resize(&self.device, width, height);
        // Recreate scene texture at new size
        let fmt = self.config.as_ref().map(|c| c.format).unwrap_or(wgpu::TextureFormat::Rgba8Unorm);
        self.scene_tex = Some(post::PostPipe::make_target2(&self.device, width, height, "scene", fmt));
        self.scene_view = Some(self.scene_tex.as_ref().unwrap().create_view(&wgpu::TextureViewDescriptor::default()));
        if let Some(config) = &mut self.config {
            config.width = width;
            config.height = height;
            self.reconfigure();
        }
    }

    /// 背景高斯模糊 σ(PMCORE-66:所有背景模糊路径共用,单点定义)。
    pub const BACKGROUND_BLUR_SIGMA: f32 = 8.0;

    /// 解码 + fastblur 高斯模糊 → RGBA(alpha=255),输出尺寸与源图一致。
    /// 背景不翻转(原 set_background 无 flip,与 upload_image 不同)。
    /// 主加载路径(load_chart_async)与 set_background 共用
    /// (fastblur SIMD 替代 image::imageops::blur,σ=8,PMCORE-66)。
    pub fn blur_background_rgba(img_bytes: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
        let img = image::load_from_memory(img_bytes)
            .context("background decode")?
            .to_rgb8();
        let (w, h) = (img.width().max(1), img.height().max(1));
        let mut raw: Vec<[u8; 3]> = img
            .as_raw()
            .chunks_exact(3)
            .map(|c| [c[0], c[1], c[2]])
            .collect();
        fastblur::gaussian_blur(&mut raw, w as usize, h as usize, Self::BACKGROUND_BLUR_SIGMA);
        // RGB → RGBA(alpha=255)
        let rgba: Vec<u8> = raw.into_iter().flat_map(|[r, g, b]| [r, g, b, 255]).collect();
        Ok((rgba, w, h))
    }

    pub fn set_background(&mut self, img_bytes: &[u8], dim: f32) -> anyhow::Result<()> {
        // Static Gaussian blur at load time(fastblur SIMD,PMCORE-66)。
        let (rgba, w, h) = Self::blur_background_rgba(img_bytes)?;
        self.set_background_rgba(&rgba, w, h, dim);
        Ok(())
    }

    /// Switch the playfield canvas aspect (w/h) at runtime.
    pub fn set_playfield_aspect(&mut self, aspect: f32) {
        self.playfield_aspect = aspect;
    }

    /// Current playfield canvas aspect (w/h).
    pub fn playfield_aspect(&self) -> f32 {
        self.playfield_aspect
    }

    /// Update the top progress bar (0..1).
    pub fn set_progress(&mut self, progress: f32) {
        self.progress = progress.clamp(0.0, 1.0);
    }

    /// Set the judge-line length multiplier (info.yml `lineLength`). The
    /// built-in line's full length = 2 × line_length × 675 canvas px, matching
    /// phira's `draw_line(-len, 0, len, 0)` under the world scale.
    pub fn set_line_length(&mut self, length: f32) {
        if length > 0.0 {
            self.line_length = length;
        }
    }

    /// Update the HUD contents for this frame (song name / difficulty /
    /// score / combo / pause state). `visible=false` hides the HUD (editor
    /// panels on screen). Pause button clicks are hit-tested against the    /// rect recorded by the previous frame's draw ([`Self::pause_rect`]).
    pub fn set_hud(&mut self, hud: HudData) {
        self.hud = hud;
    }

    /// Memory report for the HUD text system (see [`TextState::mem_report`]):
    /// (static cache entries, digit glyph bytes, font file bytes).
    pub fn text_mem(&self) -> (usize, usize, usize) {
        self.text.mem_report()
    }

    /// wgpu-side resource counts from [`wgpu::Instance::generate_report`]
    /// (wgpu-core 30 dropped byte-level reports; live resource COUNTS are
    /// the leak detector — textures/views/buffers must stay flat per frame).
    pub fn gpu_mem(&self) -> Option<String> {
        let r = self.instance.generate_report()?;
        let h = r.hub_report();
        let n = |rr: &wgpu_core::registry::RegistryReport| rr.num_allocated;
        Some(format!(
            "textures {} | views {} | buffers {} | bindgroups {} | samplers {} | shaders {} | pipelines {}",
            n(&h.textures), n(&h.texture_views), n(&h.buffers),
            n(&h.bind_groups), n(&h.samplers), n(&h.shader_modules), n(&h.render_pipelines),
        ))
    }

    /// Hit-test a window-px click against the last frame's pause button.
    pub fn hit_test_pause(&self, x: f32, y: f32) -> bool {
        let r = self.pause_rect;
        r.w > 0.0 && x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h
    }

    pub fn load_texture(&mut self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let (bind_group, size) = self.upload_image(bytes).with_context(|| format!("texture {name:?}"))?;
        self.textures.insert(name.to_string(), TexEntry { bind_group, size });
        Ok(())
    }

    /// 清掉全部 chart 纹理(切谱面时调用,避免旧谱纹理累积泄漏)。
    /// 保留 `note:` 前缀的内置 hitsound 纹理(与 chart 无关)。
    pub fn clear_chart_textures(&mut self) {
        self.textures.retain(|k, _| k.starts_with("note:"));
    }

    /// 直载后台预压缩的纹理(零 CPU 重活,只 GPU 上传)。
    pub fn load_texture_prepared(&mut self, name: &str, tex: &PreparedTex) {
        let (bind_group, size) = self.upload_prepared(tex);
        self.textures.insert(name.to_string(), TexEntry { bind_group, size });
    }

    /// 直载后台预压缩的背景(已模糊 + 翻转)。
    pub fn set_background_prepared(&mut self, tex: &PreparedTex, dim: f32) {
        let (bind_group, size) = self.upload_prepared(tex);
        self.background = Some((bind_group, size));
        self.background_dim = dim;
    }

    /// 直载预解码背景(已模糊 + 翻转)。经尺寸限制 + 可选 BC3 压缩。
    pub fn set_background_rgba(&mut self, rgba: &[u8], w: u32, h: u32, dim: f32) {
        let (bind_group, _) = self.upload_texture_rgba(rgba, w, h);
        self.background = Some((bind_group, [w as f32, h as f32]));
        self.background_dim = dim;
    }

    /// 清掉背景(切到无插图的谱面时调用,否则旧背景残留)。
    pub fn clear_background(&mut self) {
        self.background = None;
        self.background_dim = 1.0;
    }

    /// Build this frame's draw list and execute the render pass into `view`
    /// (window surface view or offscreen target). Submits on the internal
    /// queue; the caller presents (window path) or copies out (preview path).
    /// `dim`: global brightness multiplied into every quad's rgb (alpha kept).
    /// If `ui_overlay` is `Some`, a fullscreen textured quad is drawn last.
    /// If `ui_iced` is also `Some`, it is drawn BEFORE `ui_overlay` (so the
    /// iced background sits under the per-frame overlay — no CPU copy needed).
    pub(crate) fn draw_to_view(
        &mut self,
        view: &wgpu::TextureView,
        frame: &FrameState,
        window_aspect: f32,
        dim: f32,
        ui_overlay: Option<&wgpu::BindGroup>,
        ui_iced: Option<&wgpu::BindGroup>,
    ) {
        let _s = crate::trace_span!("draw_to_view");
        // Text queue is per-frame: drop last frame's entries before any new
        // text is queued (push_text borrows them for the frame's cmds, so
        // they can't be cleared after).
        self.text.pending.clear();
        // ── Coordinate model ──────────────────────────────────────────────
        // The RPE canvas is 1350×900 px (x ±675, y ±450). The playfield fills
        // the window at ANY playfield aspect (Tab cycles 3:2 → 16:9 → 4:3 →
        // 1:1): the RPE canvas stretches to fill the playfield box, so y=1
        // (450 px) is always at the visible top. Canvas px → world:
        //
        //   world_x = canvas_x / 675
        //   world_y = canvas_y / 450
        //
        // kx/ky then letterbox the playfield into the window, preserving the
        // playfield's aspect ratio (no shear, uniform pixel scale per axis).
        // ──────────────────────────────────────────────────────────────────
        let aspect = self.playfield_aspect;
        let (letterbox, kx, ky, ev_x, ev_y) = letterbox_transform(window_aspect, aspect);
        // Coordinate mapping from the playfield's x:y ratio: the playfield box
        // is 1350 × (1350/aspect) px, and the RPE canvas (1350×900) is scaled
        // UNIFORMLY by fit = min(1, 1.5/aspect) to fit inside it. The canvas
        // never clips, never distorts, and 3:2 fills the box exactly.
        // kx/ky letterbox the playfield into the window.
        let _fit = (1.5 / aspect).min(1.0);
        // Uniform letterbox: canvas px → world /675 (x) and ×aspect/675 (y) —
        // the y×aspect exactly compensates the window aspect so SPRITES keep
        // a uniform screen scale at every playfield aspect. Positions get the
        // per-axis factors separately (ev_x/ev_y fill the box at every aspect,
        // and the y stretch overflows the box at wide aspects): see below.
        // Event-position factors: depend ONLY on the playfield aspect P (the
        // Tab-switchable design ratio), NEVER on the window — the window only
        // letterboxes (kx/ky) the playfield box uniformly, so note/event
        // positions keep their shape at any window size.
        //   ev_x = 1.0:   the letterbox x (kx/675) already fills the box width
        //   ev_y = 1.5/P: canvas half-height 450 lands on the box edge ±ky
        //                 (450 × 1.5/P × ky·P/675 = ky)
        // 3:2 → (1.0, 1.0), 16:9 → (1.0, 0.844), 4:3 → (1.0, 1.125),
        // 1:1 → (1.0, 1.5). Sprites keep the uniform letterbox scale.
        // Rough pre-estimate for the cmds vec capacity.
        let needed = 2 + frame
            .lines
            .iter()
            .map(|l| 1 + 3 * l.notes.len())
            .sum::<usize>();

        // Background: cover-fill with Gaussian-blurred illustration + 30 % black
        // overlay so judge lines / notes pop against any image.
        // [C] The background quad is fully opaque (no alpha channel, color.a=1)
        // so it is drawn OUTSIDE cmds with the no-blend pipeline — a full-screen
        // quad that skips the blending unit. The 30 % black overlay stays in
        // cmds (it is genuinely translucent).
        let bg_quad: Option<(Mat3, [f32; 4], &wgpu::BindGroup)> = self.background.as_ref().map(|(bg, size)| {
            let r = size[0] / size[1];
            let uv = if r > aspect {
                let w = aspect / r;
                [(1.0 - w) * 0.5, 1.0, w, -1.0]
            } else {
                let h = r / aspect;
                [0.0, (1.0 + h) * 0.5, 1.0, -h]
            };
            // Cover the full RPE canvas (1350×900): it stretches to the
            // playfield box at any aspect.
            let bg_m = mat_mul(&letterbox, &mat_scale(1350.0, 1350.0 / aspect));
            (bg_m, uv, bg)
        });
        // Background instance: fully opaque quad at instance 0. It keeps the
        // stored `background_dim` (the frame `dim` applies to cmds only).
        let bg_instance: Option<Instance> = bg_quad.as_ref().map(|(bg_m, uv, _)| Instance {
            model: instance_model(bg_m),
            color: [self.background_dim, self.background_dim, self.background_dim, 1.0],
            uv_rect: *uv,
        });

        // Track original index before z-sort for selection highlight
        let mut lines: Vec<(usize, &crate::core::LineState)> = frame.lines.iter().enumerate().collect();
        lines.sort_by_key(|(_, l)| l.z_order);
        let line_refs: Vec<&crate::core::LineState> = lines.into_iter().map(|(_, l)| l).collect();

        // ── PMCORE-73: parallel scene build + encode ─────────────────────
        // Per-line DrawCmd generation / matrix math / instance recording runs
        // on worker threads; each worker additionally records its own scene
        // render pass into `scene_view` (chunk-local vertex buffer). Chunk 0
        // clears the target and draws the opaque background quad; later
        // chunks `Load` on top — submitting the buffers in order reproduces
        // the old single-pass output byte-for-byte. The main thread keeps the
        // render_frame contract: it only builds the HUD/progress/fx/text
        // extras, then submits ALL command buffers in one `queue.submit`.
        let has_effects = { let p = &self.post; !p.active.is_empty() };
        let scene_view: &wgpu::TextureView = if has_effects {
            self.scene_view.as_ref().unwrap()
        } else {
            &view
        };
        let ctx = SceneCtx {
            textures: &self.textures,
            white: &self.white,
            letterbox,
            kx,
            ev_x,
            ev_y,
            line_length: self.line_length,
            aggressive: self.aggressive,
        };
        let n_workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, line_refs.len().max(1));
        // Spawning threads costs tens of µs each — only parallelize when the
        // estimated cmd count makes it pay off (single-core machines / tiny
        // charts / 0..1 lines fall back to the single-threaded path below).
        // `PHIMAKOR_MT_SCENE=0` forces the single-thread path (hash/ab test
        // hook — both paths must produce byte-identical frames).
        let mt_scene = std::env::var_os("PHIMAKOR_MT_SCENE").is_none_or(|v| v != "0");
        let parallel = mt_scene && n_workers > 1 && needed >= 256;
        let mut submit: Vec<wgpu::CommandBuffer> = Vec::new();
        if parallel {
            let _s1 = crate::trace_span!("draw_cmds_build");
            // Hoist shared refs into Copy locals so the closures capture only
            // those — never `self` / `ctx` themselves. rayon::scope borrows
            // them safely while running jobs on the PERSISTENT global pool
            // (per-frame `std::thread::scope` spawning measured ~4 ms/frame
            // for 20 threads on Windows — a net regression, hence rayon).
            let ctx_ref = &ctx;
            let device = &self.device;
            let queue = &self.queue;
            let pipeline = &self.pipeline;
            let opaque = &self.opaque_pipeline;
            let timers0 = self.gpu_timers.as_ref();
            let per = line_refs.len().div_ceil(n_workers);
            let n_chunks = line_refs.chunks(per).count();
            // One slot per chunk; command buffers must end up in z-order, so
            // each job writes its own slot (rayon completes jobs out of order).
            let slots: Vec<std::sync::Mutex<Option<wgpu::CommandBuffer>>> =
                (0..n_chunks).map(|_| std::sync::Mutex::new(None)).collect();
            rayon::scope(|s| {
                for (i, chunk) in line_refs.chunks(per).enumerate() {
                    let first = i == 0;
                    let bg = if first { bg_quad } else { None };
                    let bg_inst = if first { bg_instance } else { None };
                    let timers = if first { timers0 } else { None };
                    let slot = &slots[i];
                    s.spawn(move |_| {
                        *slot.lock().expect("scene chunk slot") = Some(encode_scene_chunk(
                            chunk,
                            ctx_ref,
                            device,
                            queue,
                            pipeline,
                            opaque,
                            scene_view,
                            first,
                            bg,
                            bg_inst,
                            timers,
                            dim,
                        ));
                    });
                }
            });
            submit.extend(slots.into_iter().map(|m| {
                m.into_inner().expect("slot mutex").expect("scene chunk result")
            }));
            drop(_s1);
        }

        // HUD/progress/fx/text extras are always main-thread (they touch
        // `&mut self.text` / `pause_rect`), drawn ON TOP of the line scene.
        let mut extra: Vec<DrawCmd> = Vec::new();
        // ── Phigros-style HUD ───────────────────────────────────────────
        // Score (top-left), combo number (top-center) with "COMBO" caption
        // below it, song name (bottom-left), difficulty (bottom-right), the
        // pause button (top-right) and the top progress bar.
        //
        // attachUI is an RPE format feature: the judge line carries an
        // `attachUI` binding ("score"/"combonumber"/"combo"/"pause"/"name"/
        // "level"/"bar") that links it to one game HUD element. The element
        // sits at its default anchor and the bound line's transform is
        // applied on top — position/rotation/scale/alpha/color — as a
        // relative transform, never an absolute position. Drawn through the
        // scene pipeline so exports (readback / embedded engine) include it.
        if self.hud.visible {
            let hud = &self.hud;
            // attachUI lookup: first matching line per binding.
            let ui_line = |name: &str| frame.lines.iter().find(|l| l.attach_ui.as_deref() == Some(name));
            // RPE attachUI transform of the bound line:
            //   Some(transform) — line exists & visible (alpha > 0): draw at
            //                     the default anchor + line transform offset.
            //   None — no bound line: draw at the default anchor, opaque.
            //   Hidden — line exists but alpha <= 0 (or pe_hide): DON'T draw
            //            (the element is faded out by the line's alpha).
            enum HudBind { Hidden, Default, Bound([f32; 2], f32, f32, f32, [f32; 4]) }
            let bind = |name: &str| -> HudBind {
                let Some(l) = ui_line(name) else { return HudBind::Default };
                if l.pe_hide || l.alpha <= 0.0 { return HudBind::Hidden; }
                // Offset in canvas px (line position is a delta, not absolute).
                let off = [l.position[0] * CANVAS_W * ev_x, l.position[1] * CANVAS_H * ev_y];
                let a = l.alpha * l.ctrl_alpha;
                let c = l.color;
                HudBind::Bound(off, l.rotation, l.scale[0], a, [c[0], c[1], c[2], c[3]])
            };
            // Default: no binding → offset 0, no rotation, scale 1, white.
            let dflt = ([0.0, 0.0], 0.0, 1.0, 1.0, [1.0, 1.0, 1.0, 1.0]);
            let mut el = |name: &str, text: &str, anchor: TextAnchor| {
                let (off, rot, sc, a, c) = match bind(name) {
                    HudBind::Hidden => return,
                    HudBind::Default => dflt,
                    HudBind::Bound(off, rot, sc, a, c) => (off, rot, sc, a, c),
                };
                let col = [c[0], c[1], c[2], c[3] * a];
                text::draw_text_queued(
                    &mut self.text, &self.device, &self.queue, &self.tex_bgl, &self.sampler,
                    aspect, text, anchor, off, rot, sc, col,
                );
            };

            // Score (top-left): fixed 7-digit zero-padded, so it starts at
            // "0000000" and counts up without shifting. Text cached per value
            // (score only changes on hits — zero per-frame alloc otherwise).
            el("score", hud_fmt(&mut self.hud_score_txt, hud.score, |v| format!("{:07}", v)), TextAnchor::TopLeft);
            // Combo: number top-center + "COMBO" caption below, only >= 3
            // (RPE/Phigros behavior: combo hidden until the 3rd).
            if hud.combo >= 3 {
                el("combonumber", hud_fmt(&mut self.hud_combo_txt, hud.combo, |v| format!("{}", v)), TextAnchor::TopCenter);
                el("combo", "COMBO", TextAnchor::ComboLabel);
            }
            // Song name (bottom-left)
            if !hud.chart_name.is_empty() {
                el("name", hud.chart_name.as_str(), TextAnchor::BottomLeft);
            }
            // Difficulty (bottom-right)
            if !hud.difficulty.is_empty() {
                el("level", hud.difficulty.as_str(), TextAnchor::BottomRight);
            }
            // 帧时间叠层(窗口右上角,编辑器视口隐藏时也显示;
            // viewport 边缘锚定,与 timeline 层的 fps 同位置语义)。
            if hud.frame_ms > 0.0 {
                let txt = format!("frame {:.1}ms / {:.0}fps", hud.frame_ms, hud.fps);
                let col = if hud.frame_ms > 50.0 { [1.0, 0.35, 0.35, 1.0] }
                    else if hud.frame_ms > 25.0 { [1.0, 0.82, 0.35, 1.0] }
                    else { [0.63, 0.9, 0.78, 1.0] };
                text::draw_text_queued(
                    &mut self.text, &self.device, &self.queue, &self.tex_bgl, &self.sampler,
                    aspect, &txt, TextAnchor::TopRightEdge, [0.0, 0.0], 0.0, 1.0, col,
                );
            }

            // Pause button (top-right of the playfield box): circular
            // translucent backdrop with two bars (pause icon); while paused
            // the bars swap to a play triangle (▶). No glyph font
            // dependency — all geometry.
            let btn_size = 44.0; // canvas px backdrop diameter
            let pause_w = 9.0; // canvas px bar width
            let pause_h = 22.0; // bar height
            let box_top = 675.0 / aspect;
            let default_bc = [CANVAS_W - btn_size * 0.5 - 12.0, box_top - btn_size * 0.5 - 10.0];
            let (off, rot, _sc, a, c) = match bind("pause") {
                HudBind::Hidden => ([0.0, 0.0], 0.0, 1.0, 0.0, [1.0, 1.0, 1.0, 1.0]),
                HudBind::Default => dflt,
                HudBind::Bound(off, rot, sc, a, c) => (off, rot, sc, a, c),
            };
            let bc = [default_bc[0] + off[0], default_bc[1] + off[1]];
            if a > 0.0 {
                let col = [c[0], c[1], c[2], c[3] * a];
                // Circular backdrop (anti-aliased disc texture).
                let bg_m = mat_mul(
                    &letterbox,
                    &mat_mul(
                        &mat_translate(bc[0], bc[1]),
                        &mat_mul(&mat_rotate(rot), &mat_scale(btn_size, btn_size)),
                    ),
                );
                extra.push(DrawCmd {
                    uniform: DrawUniform { model: bg_m, color: [0.05, 0.05, 0.08, 0.55 * a], uv_rect: [0., 0., 1., 1.] },
                    tex: &self.disc,
                });
                if hud.paused {
                    // Play icon: triangle (▶), slightly smaller than the bars.
                    let tri_h = pause_h * 0.95;
                    let tri_m = mat_mul(
                        &letterbox,
                        &mat_mul(
                            &mat_translate(bc[0], bc[1]),
                            &mat_mul(
                                &mat_rotate(rot),
                                &mat_scale(tri_h * 1.25, tri_h),
                            ),
                        ),
                    );
                    extra.push(DrawCmd {
                        uniform: DrawUniform { model: tri_m, color: col, uv_rect: [0., 0., 1., 1.] },
                        tex: &self.play_tri,
                    });
                } else {
                    // Pause icon: two bars, tight spacing (±1.1×bar width).
                    for s in [-1.0f32, 1.0] {
                        let bar_m = mat_mul(
                            &letterbox,
                            &mat_mul(
                                &mat_translate(bc[0], bc[1]),
                                &mat_mul(
                                    &mat_rotate(rot),
                                    &mat_translate(s * pause_w * 1.1, 0.0),
                                ),
                            ),
                        );
                        let bar_m = mat_mul(&bar_m, &mat_scale(pause_w, pause_h));
                        extra.push(DrawCmd {
                            uniform: DrawUniform { model: bar_m, color: col, uv_rect: [0., 0., 1., 1.] },
                            tex: &self.white,
                        });
                    }
                }
            }
            // Record the pause hit rect in window px. World text scales with
            // the letterbox: map the button center through it. Zeroed when
            // the button is hidden (faded out by its attachUI line) so it
            // can't be clicked.
            let win = [self.size[0] as f32, self.size[1] as f32];
            if a <= 0.0 {
                self.pause_rect = PauseHitRect::default();
            } else {
                let (nx, ny) = (
                    bc[0] * kx / CANVAS_W,
                    bc[1] * ky * aspect / CANVAS_W,
                );
                self.pause_rect = PauseHitRect {
                    x: (nx + 1.0) * 0.5 * win[0] - btn_size * kx / CANVAS_W * win[0] * 0.25,
                    y: (1.0 - ny) * 0.5 * win[1] - btn_size * ky * aspect / CANVAS_W * win[1] * 0.25,
                    w: btn_size * kx / CANVAS_W * win[0] * 0.5,
                    h: btn_size * ky * aspect / CANVAS_W * win[1] * 0.5,
                };
            }
        }

        // Hit effects: sprite-sheet bursts in canvas-pixel space, after notes.
        // Progress bar: bound to the attachUI "bar" line when present (RPE
        // attachUI: the bar follows the line transform); falls back to the
        // visible canvas top edge otherwise.
        if self.progress > 0.0 {
            let bar_h = 5.0;
            // Bar anchored at the box top-left in the raw letterbox space:
            // y = 675/aspect maps to the box top at every aspect.
            let top = 675.0 / aspect;
            let bar_w = 1350.0 * self.progress;
            if let Some(bl) = frame.lines.iter().find(|l| l.attach_ui.as_deref() == Some("bar")) {
                if !bl.pe_hide && bl.alpha > 0.0 {
                    let bl_alpha = bl.alpha * bl.ctrl_alpha;
                    // Bar rect anchored at the playfield's top-left in the RAW
                    // letterbox space: (-675, 675/aspect) maps to the box
                    // corner (-kx, +ky) at every aspect. The attachUI line's
                    // transform (ev-mapped position + rotation + scale) then
                    // moves/rotates/scales it.
                    let bar_local = mat_mul(
                        &mat_translate(-CANVAS_W + bar_w * 0.5, 675.0 / aspect - bar_h * 0.5),
                        &mat_scale(bar_w, bar_h),
                    );
                    let bar_m = mat_mul(
                        &letterbox,
                        &mat_mul(
                            &mat_mul(
                                &mat_translate(bl.position[0] * CANVAS_W * ev_x, bl.position[1] * CANVAS_H * ev_y),
                                &mat_rotate(bl.rotation),
                            ),
                            &mat_mul(
                                &mat_scale(bl.scale[0] * bl.ctrl_size_x, bl.scale[1] * bl.ctrl_size_y),
                                &bar_local,
                            ),
                        ),
                    );
                    extra.push(DrawCmd {
                        uniform: DrawUniform {
                            model: bar_m,
                            color: [1.0, 1.0, 1.0, 0.9 * bl_alpha],
                            uv_rect: [0., 0., 1., 1.],
                        },
                        tex: &self.white,
                    });
                }
            } else {
                extra.push(DrawCmd {
                    uniform: DrawUniform {
                        model: mat_mul(
                            &letterbox,
                            &mat_mul(
                                &mat_translate(-CANVAS_W + bar_w * 0.5, top - bar_h * 0.5),
                                &mat_scale(bar_w, bar_h),
                            ),
                        ),
                        color: [1.0, 1.0, 1.0, 0.9],
                        uv_rect: [0., 0., 1., 1.],
                    },
                    tex: &self.white,
                });
            }
        }

        // Field-split call: `cmds` already borrows `self.textures`/`self.white`.
        // fx 纯时间函数:host 查询的 frame_fx + 当前谱面时间(frame.time)
        // 渲染——倒退/跳转时 age = now - t0 自然对齐。
        Self::push_hit_fx(&self.frame_fx, &self.textures, &self.white, &mut extra, &letterbox, ev_x, ev_y, frame.time);

        // Text overlay (Phaser UI), on top of everything; queue is per-frame.
        text::push_text(
            &mut self.text.pending,
            &mut extra,
            &letterbox,
            [self.size[0] as f32, self.size[1] as f32],
        );

        // ── Scene encode ────────────────────────────────────────────────
        // Parallel path: the worker chunks already recorded their scene
        // passes; the main encoder here carries only the extras scene pass
        // (`Load` — chunk 0 cleared, workers drew the lines) + post + blit +
        // ui. Single-threaded fallback: everything in one buffer + one
        // encoder, identical to the pre-PMCORE-73 flow.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        if !parallel {
            // GPU 计时(可选):t0 = 场景 pass 开始前(并行路径由 chunk 0 写)。
            if let Some(t) = &self.gpu_timers {
                encoder.write_timestamp(&t.query_set, 0);
            }
            // Convert cmds to instances (applying the global rgb dim on the
            // CPU), growing both instance buffers together when capacity
            // falls short. The opaque background quad (if any) occupies
            // instance 0.
            let mut cmds: Vec<DrawCmd> = Vec::with_capacity(needed);
            if let Some((bg_m, _, _)) = &bg_quad {
                // 30 % black overlay
                cmds.push(DrawCmd {
                    uniform: DrawUniform { model: *bg_m, color: [0., 0., 0., 0.3], uv_rect: [0., 0., 1., 1.] },
                    tex: &self.white,
                });
            }
            cmds.extend(build_line_cmds(&line_refs, &ctx));
            cmds.extend(extra);
            let total_instances = cmds.len() + usize::from(bg_quad.is_some());
            if total_instances > self.instance_capacity {
                // Smooth growth: 1.5× current, minimum +512. Avoids the double-
                // capacity burst that causes visible frame spikes on D3D12.
                self.instance_capacity = (total_instances as f64 * 1.5) as usize + 512;
                self.instance_bufs = [
                    Self::make_instance_buf(&self.device, self.instance_capacity),
                    Self::make_instance_buf(&self.device, self.instance_capacity),
                ];
            }
            let _s1 = crate::trace_span!("draw_cmds_build");
            let mut instances: Vec<Instance> = Vec::with_capacity(total_instances);
            if let Some(bgi) = bg_instance {
                instances.push(bgi);
            }
            instances.extend(to_instances(&cmds, dim));
            drop(_s1);
            if !instances.is_empty() {
                self.queue.write_buffer(
                    &self.instance_bufs[self.frame_idx],
                    0,
                    bytemuck::cast_slice(&instances),
                );
            }
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("scene"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: scene_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_vertex_buffer(0, self.instance_bufs[self.frame_idx].slice(..));
                // [C] Background sits at instance 0 (fully opaque, no-blend pass);
                // cmds follow at offset 1 with the alpha pipeline. The draw loop
                // keeps cmd ordering (translucent layering stays correct).
                // NOTE: the offset must track whether a background exists — with
                // no background the cmds start at instance 0.
                let base = usize::from(bg_quad.is_some());
                if base == 1 {
                    pass.set_pipeline(&self.opaque_pipeline);
                    pass.set_bind_group(0, bg_quad.as_ref().unwrap().2, &[]);
                    pass.draw(0..4, 0..1);
                }
                pass.set_pipeline(&self.pipeline);
                let mut start = 0usize;
                for i in 1..=cmds.len() {
                    if i == cmds.len() || !std::ptr::eq(cmds[i].tex, cmds[start].tex) {
                        pass.set_bind_group(0, cmds[start].tex, &[]);
                        pass.draw(0..4, (start + base) as u32..(i + base) as u32);
                        start = i;
                    }
                }
            }
        } else {
            // Parallel path: the persistent instance buffer holds just the
            // extras; the scene pass `Load`s on top of the workers' output.
            let main_instances = to_instances(&extra, dim);
            if main_instances.len() > self.instance_capacity {
                // Smooth growth: 1.5× current, minimum +512. Avoids the double-
                // capacity burst that causes visible frame spikes on D3D12.
                self.instance_capacity = (main_instances.len() as f64 * 1.5) as usize + 512;
                self.instance_bufs = [
                    Self::make_instance_buf(&self.device, self.instance_capacity),
                    Self::make_instance_buf(&self.device, self.instance_capacity),
                ];
            }
            if !main_instances.is_empty() {
                self.queue.write_buffer(
                    &self.instance_bufs[self.frame_idx],
                    0,
                    bytemuck::cast_slice(&main_instances),
                );
            }
            if !extra.is_empty() {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("scene-extras"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: scene_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_vertex_buffer(0, self.instance_bufs[self.frame_idx].slice(..));
                let mut start = 0usize;
                for i in 1..=extra.len() {
                    if i == extra.len() || !std::ptr::eq(extra[i].tex, extra[start].tex) {
                        pass.set_bind_group(0, extra[start].tex, &[]);
                        pass.draw(0..4, start as u32..i as u32);
                        start = i;
                    }
                }
            }
        }
        // Step 2: Run post-processing effects (if any)
        if has_effects {
            // Step 1.5:z 层遮罩(targetRange)——对每个 distinct range 渲染
            // 白色场景(只画 z 在 range 内的线+音符)到遮罩纹理,特效
            // 执行时按遮罩混合,否则非 global 特效被无差别全屏应用。
            let mut ranges: Vec<(f32, f32)> = Vec::new();
            for ae in &self.post.active {
                if let Some(r) = ae.target_range {
                    if !ranges.contains(&r) {
                        ranges.push(r);
                    }
                }
            }
            for r in ranges {
                let mask_view = self.post.mask_view(&self.device, r).clone();
                self.render_mask(&mut encoder, window_aspect, frame, r, &mask_view);
            }
            self.post.apply(
                &mut encoder,
                &self.device,
                &self.queue,
                scene_view,
                self.pipeline_cache.as_ref(),
            );
        }
        // Step 3: Blit final to surface (only when using intermediate texture)
        if has_effects {
            let blit_pipe = self.post.blit_pipeline.as_ref().unwrap().clone();
            // Bind group cached per ping-pong target (post.rs, keyed by
            // SrcTag::Full(last_output)); built once, reused every frame.
            let blit_bg = self.post.surface_blit_bg(&self.device);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&blit_pipe);
            pass.set_bind_group(0, &blit_bg, &[]);
            pass.draw(0..3, 0..1);
        }
        // Step 4: UI overlay (iced + timeline) on the surface, never post-processed
        let ui_instance = Instance {
            model: [2.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0],
            color: [1.0, 1.0, 1.0, 1.0],
            uv_rect: [0., 1., 1., -1.],
        };
        self.queue.write_buffer(&self.ui_inst_buf, 0, bytemuck::cast_slice(&[ui_instance]));
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui-overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.ui_inst_buf.slice(..));
            if let Some(bg) = ui_iced {
                pass.set_bind_group(0, bg, &[]);
                pass.draw(0..4, 0..1);
            }
            if let Some(bg) = ui_overlay {
                pass.set_bind_group(0, bg, &[]);
                pass.draw(0..4, 0..1);
            }
        }
        // GPU 计时:所有渲染结束后写 t1 + resolve 到读回缓冲。
        if let Some(t) = &self.gpu_timers {
            encoder.write_timestamp(&t.query_set, 1);
            encoder.resolve_query_set(&t.query_set, 0..2, &t.resolve_buf, 0);
            encoder.copy_buffer_to_buffer(&t.resolve_buf, 0, &t.readback_buf, 0, 16);
        }
        let _s2 = crate::trace_span!("draw_upload_submit");
        submit.push(encoder.finish());
        // PMCORE-73: multiple command buffers (scene chunks + main) in ONE
        // submit — wgpu executes them in order, so draw order is unchanged.
        self.queue.submit(submit);
        self.frame_idx ^= 1;
        drop(_s2);
    }

    /// 读回上一帧 GPU 渲染耗时(ms)。仅 PHIMAKOR_GPU_TIMING=1 时有效;
    /// 默认返回 0(未启用)。阻塞到 GPU 完成。
    pub fn gpu_last_frame_ms(&mut self) -> f32 {
        match &mut self.gpu_timers {
            Some(t) => t.poll(&self.device, &self.queue),
            None => 0.0,
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // 兑底写回:运行期 apply 懒加载创建的特效管线也进缓存。设备字段在
        // drop 体之后才销毁,get_data 是同步 CPU 查询,此处安全。
        if let Some(cache) = &self.pipeline_cache {
            pipeline_cache::save(cache);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: &Mat3, x: f32, y: f32) -> (f32, f32) {
        (
            m[0][0] * x + m[1][0] * y + m[2][0],
            m[0][1] * x + m[1][1] * y + m[2][1],
        )
    }

    #[test]
    fn line_transform_composes_translate_rotate_scale() {
        // T(0.3,-0.2) · R(90°) · S(1,1) applied to (0, 0.5): rotate first
        // (→ (-0.5, 0)), then translate (→ (-0.2, -0.2)).
        let m = mat_mul(
            &mat_translate(0.3, -0.2),
            &mat_mul(&mat_rotate(std::f32::consts::FRAC_PI_2), &mat_scale(1., 1.)),
        );
        let (x, y) = apply(&m, 0., 0.5);
        assert!((x + 0.2).abs() < 1e-6 && (y + 0.2).abs() < 1e-6, "({x}, {y})");
    }

    #[test]
    fn instance_model_packs_column_major_affine() {
        // T(3,-2) · R(90°): a=cos=0, b=sin=1, c=-sin=-1, d=cos=0, tx=3, ty=-2.
        // (1,0) rotates to (0,1), then translates to (3,-1).
        let m = mat_mul(&mat_translate(3., -2.), &mat_rotate(std::f32::consts::FRAC_PI_2));
        let p = instance_model(&m);
        let apply = |x: f32, y: f32| (p[0] * x + p[2] * y + p[4], p[1] * x + p[3] * y + p[5]);
        let (x, y) = apply(1., 0.);
        assert!((x - 3.).abs() < 1e-6 && (y + 1.).abs() < 1e-6, "({x}, {y})");
        assert!(p[6] == 0. && p[7] == 0., "pad must stay zero");
        assert_eq!(size_of::<Instance>(), 64);
    }

    #[test]
    fn clip_segment_basic() {
        let (xmin, xmax, ymin, ymax) = (-675.0, 675.0, -450.0, 450.0);
        // Fully inside: unchanged.
        let (t0, t1) = clip_segment((-100., -100.), (100., 100.), xmin, xmax, ymin, ymax).unwrap();
        assert!((t0 - 0.0).abs() < 1e-6 && (t1 - 1.0).abs() < 1e-6);
        // Fully outside (above): none.
        assert!(clip_segment((0., 500.), (0., 900.), xmin, xmax, ymin, ymax).is_none());
        // Vertical body crossing the top edge: head inside (0,-100),
        // tail far above (0, 1200). Visible part t0=0 (head), t1 where y=450.
        let (t0, t1) = clip_segment((0., -100.), (0., 1200.), xmin, xmax, ymin, ymax).unwrap();
        assert!((t0 - 0.0).abs() < 1e-6);
        assert!((t1 - (450.0 - (-100.0)) / 1300.0).abs() < 1e-4, "t1={t1}");
        // Diagonal (rotated line): clip both ends, visible length via
        // Pythagoras must equal the clipped sub-segment's hypotenuse.
        let (t0, t1) = clip_segment((-900., -200.), (900., 600.), xmin, xmax, ymin, ymax).unwrap();
        assert!(t0 > 0.0 && t1 < 1.0);
        let (dx, dy) = (1800.0, 800.0);
        let vis = ((dx * (t1 - t0)).powi(2) + (dy * (t1 - t0)).powi(2)).sqrt();
        // Cross-check against manual endpooints:
        let (x0, y0) = (-900. + dx * t0, -200. + dy * t0);
        let (x1, y1) = (-900. + dx * t1, -200. + dy * t1);
        assert!(((x0 - x1).powi(2) + (y0 - y1).powi(2)).sqrt() - vis < 1e-3);
        assert!(x0 >= xmin && x1 <= xmax && y0 >= ymin && y1 <= ymax);
    }

    #[test]
    fn letterbox_maps_internal_rect_to_playfield() {
        // 4:3 window (1.333 < 3:2): canvas x ±675px fills width, y letterboxed.
        let ky = (4. / 3.) / super::ASPECT;
        let l = mat_scale(1. / 675., ky / 450.);
        let (x, y) = apply(&l, 675., 450.);
        assert!((x - 1.).abs() < 1e-4 && (y - ky).abs() < 1e-4, "({x}, {y})");
    }

    #[test]
    fn letterbox_transform_ev_factors() {
        // 3:2 playfield at a matching window: no letterbox, ev_y = 1.
        let (_, kx, ky, ev_x, ev_y) = letterbox_transform(1.5, 1.5);
        assert!((kx - 1.).abs() < 1e-6 && (ky - 1.).abs() < 1e-6);
        assert!((ev_x - 1.).abs() < 1e-6 && (ev_y - 1.).abs() < 1e-6);
        // 16:9 window on a 3:2 playfield: x letterboxed, y fills.
        let (_, kx, ky, _, _) = letterbox_transform(16. / 9., 1.5);
        assert!((kx - 0.84375).abs() < 1e-4 && (ky - 1.).abs() < 1e-6);
        // 4:3 window: y letterboxed, x fills; ev_y = 1.5 / 1.5 = 1.
        let (_, kx, ky, _, ev_y) = letterbox_transform(4. / 3., 1.5);
        assert!((kx - 1.).abs() < 1e-6 && (ky - 0.8889).abs() < 1e-4);
        assert!((ev_y - 1.).abs() < 1e-6);
    }

    #[test]
    fn fit_texture_dim_caps_and_aligns() {
        // 未超限:原尺寸;压缩时对齐 4。
        assert_eq!(Renderer::fit_texture_dim(1024, 768, true), (1024, 768));
        // 超限:等比缩到上限内。
        let (w, h) = Renderer::fit_texture_dim(4096, 2048, true);
        assert!(w <= Renderer::MAX_TEXTURE_DIM && h <= Renderer::MAX_TEXTURE_DIM);
        assert_eq!(w % 4, 0);
        assert_eq!(h % 4, 0);
        // 不压缩:只限制,不对齐。
        let (w, h) = Renderer::fit_texture_dim(4096, 2048, false);
        assert!(w <= Renderer::MAX_TEXTURE_DIM && h <= Renderer::MAX_TEXTURE_DIM);
        // 非 4 倍数尺寸压缩时对齐。
        let (w, h) = Renderer::fit_texture_dim(1000, 999, true);
        assert_eq!(w % 4, 0);
        assert_eq!(h % 4, 0);
        assert_eq!(w, 1000 & !3);
        // 回归:1~3px 纹理压缩对齐后不得为 0(0×0 纹理 wgpu panic)。
        assert_eq!(Renderer::fit_texture_dim(1, 1, true), (1, 1));
        assert_eq!(Renderer::fit_texture_dim(2, 3, true), (1, 1));
        assert_eq!(Renderer::fit_texture_dim(5, 1, true), (4, 1));
    }

    #[test]
    fn bc3_compress_roundtrip() {
        // 64×64 渐变图(含透明通道):BC3 压缩 → 解压 → 尺寸/数据合法。
        let w = 64usize;
        let h = 64usize;
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 4) as u8, (y * 4) as u8, 128, (x + y) as u8]);
            }
        }
        let size = texpresso::Format::Bc3.compressed_size(w, h);
        assert_eq!(size, (w / 4) * (h / 4) * 16);
        let mut blocks = vec![0u8; size];
        texpresso::Format::Bc3.compress(&rgba, w, h, texpresso::Params::default(), &mut blocks);
        let mut decoded = vec![0u8; w * h * 4];
        texpresso::Format::Bc3.decompress(&blocks, w, h, &mut decoded);
        // 解压后像素应大致接近原值(有损,容差大)。
        let mut err = 0.0f64;
        for i in 0..rgba.len() {
            let d = (rgba[i] as f64 - decoded[i] as f64).abs();
            err += d;
        }
        let avg = err / rgba.len() as f64;
        assert!(avg < 20.0, "BC3 roundtrip avg error {avg:.1} too high");
        // 透明度保留(α 通道解压有值)。
        assert!(decoded[3] > 0 || decoded.iter().step_by(4).any(|&a| a > 0));
    }
}


