//! Hit-effect bursts: 30-frame sprite-sheet animation (5×6 atlas) over 0.5 s.
//!
//! Bursts live in canvas-pixel space (1350×900, center origin); `render()`
//! calls [`Renderer::push_hit_fx`] after notes so effects draw on top.
//!
//! 0.5s 内:中心贴图帧(固定尺寸,原始 30 帧动画)+ 24 个碎片粒子
//! (白色纹理染色成金色,与中心同色;大小不一,大小变化动画,
//! 向外扩散,不旋转)。
//!
//! **纯时间函数**:渲染只依赖"当前谱面时间 + 窗口内的触发点列表"
//! (由 `Chart::fx_in_window` 查询)。前进/倒退/任意跳转都按谱面时间
//! 渲染对应帧——age = now - t0,倒退时粒子回到较早状态。粒子随机种子
//! 从 t0 派生(确定性),同一触发点任意时刻查询粒子形状一致。

use std::collections::HashMap;

use super::{
    mat_mul, mat_scale, mat_translate, DrawCmd, DrawUniform, Mat3, Renderer, TexEntry, NOTE_SPRITE_W,
};

// Sprite sheet (note:hitfx): 1280×1536, 5 cols × 6 rows = 30 frames.
const HIT_FX_COLS: u32 = 5;
const HIT_FX_ROWS: u32 = 6;
const HIT_FX_FRAMES: u32 = 30;
const HIT_FX_SECS: f64 = 0.5;
    /// 每 fx 的碎片粒子数。
    const SHARD_COUNT: usize = 24;

/// 碎片粒子种子:由触发时间派生(确定性,可倒退渲染)。
fn fx_seed_of(t0: f64) -> u64 {
    let bits = t0.to_bits();
    bits.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407)
}

impl Renderer {
    /// Queue draws for all live bursts (called from `render`, after notes).
    /// `triggers` = 窗口内命中特效触发点 `(谱面时间 t0, 画布位置)`——
    /// 由 `Chart::fx_in_window` 查询生成,再经线变换换算成画布坐标。
    /// `now` = 当前谱面时间:age = now - t0,倒退/跳转自然对齐。
    /// `letterbox` is the canvas px → NDC playfield transform.
    /// `white` = 1×1 白色纹理(纯正方形粒子用)。
    pub(super) fn push_hit_fx<'a>(
        triggers: &[(f64, [f32; 2])],
        textures: &'a HashMap<String, TexEntry>,
        white: &'a wgpu::BindGroup,
        cmds: &mut Vec<DrawCmd<'a>>,
        letterbox: &Mat3,
        ev_x: f32,
        ev_y: f32,
        now: f64,
    ) {
        if triggers.is_empty() {
            return;
        }
        let Some(fx_tex) = textures.get("note:hitfx") else {
            return;
        };
        let base_side = 1.5 * NOTE_SPRITE_W; // canvas px,~1.5 note widths
        // 淡金色。
        const GOLD: [f32; 4] = [1.0, 0.92, 0.6, 1.0];
        for &(t0, pos) in triggers {
            // 只渲染生命周期内的触发点(倒退到出生前的不显示)。
            if now < t0 {
                continue;
            }
            let age = (now - t0) as f32;
            if age >= HIT_FX_SECS as f32 {
                continue;
            }
            let seed = fx_seed_of(t0);
            // 中心贴图帧:原始逻辑(固定尺寸 + 固定金色)。
            let frame = ((age / HIT_FX_SECS as f32) * HIT_FX_FRAMES as f32).min((HIT_FX_FRAMES - 1) as f32) as u32;
            let (col, row) = (frame % HIT_FX_COLS, frame / HIT_FX_COLS);
            cmds.push(DrawCmd {
                uniform: DrawUniform {
                    model: mat_mul(
                        letterbox,
                        &mat_mul(&mat_translate(pos[0] * ev_x, pos[1] * ev_y), &mat_scale(base_side, base_side)),
                    ),
                    color: GOLD,
                    uv_rect: [
                        col as f32 / HIT_FX_COLS as f32,
                        1.0 - (row + 1) as f32 / HIT_FX_ROWS as f32,
                        1.0 / HIT_FX_COLS as f32,
                        1.0 / HIT_FX_ROWS as f32,
                    ],
                },
                tex: &fx_tex.bind_group,
            });
            // 碎片粒子:随机(方向/大小/距离,种子由 t0 派生——确定性,
            // 倒退渲染时粒子形状一致),向外扩散,不旋转。
            let t = (age / HIT_FX_SECS as f32).clamp(0.0, 1.0);
            for i in 0..SHARD_COUNT {
                let s = seed.wrapping_mul(0x9E3779B1).wrapping_add(i as u64 * 0x85EBCA6B);
                let rnd = |salt: u64| -> f32 {
                    let x = s.wrapping_mul(salt).wrapping_add(0x27D4EB2F);
                    (x >> 33) as f32 / (1u64 << 31) as f32 - 1.0
                };
                // 方向:完全随机。
                let ang = rnd(1) * std::f32::consts::TAU;
                // 大小:0.035~0.11 随机 + 大小变化动画。
                let sz_f = 0.035 + rnd(2).abs() * 0.075;
                let grow = 0.5 + 0.9 * ((t * std::f32::consts::PI).sin()).abs();
                let sz = base_side * sz_f * grow;
                // 距离:随机区间(0.55~1.0)。
                let dist = base_side * ease_out_cubic(t) * 0.5 * (0.55 + rnd(3).abs() * 0.45);
                let px = pos[0] * ev_x + ang.cos() * dist * ev_x;
                let py = pos[1] * ev_y + ang.sin() * dist * ev_y;
                let alpha = (1.0 - t).max(0.0) * 0.9;
                cmds.push(DrawCmd {
                    uniform: DrawUniform {
                        model: mat_mul(
                            letterbox,
                            &mat_mul(&mat_translate(px, py), &mat_scale(sz, sz)),
                        ),
                        color: [GOLD[0], GOLD[1], GOLD[2], GOLD[3] * alpha],
                        uv_rect: [0.0, 0.0, 1.0, 1.0],
                    },
                    tex: white,
                });
            }
        }
    }
}

/// ease-out cubic(碎片扩散缓动,避免依赖 core::easing)。
fn ease_out_cubic(x: f32) -> f32 {
    let x = x - 1.0;
    1.0 + x * x * x
}
