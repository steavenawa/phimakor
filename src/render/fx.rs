//! Hit-effect bursts: 30-frame sprite-sheet animation (5×6 atlas) over 0.5 s.
//!
//! Bursts live in canvas-pixel space (1350×900, center origin); `render()`
//! calls [`Renderer::push_hit_fx`] after notes so effects draw on top.
//!
//! 0.5s 内:中心贴图帧(固定尺寸,原始 30 帧动画)+ 14 个碎片粒子
//! (白色纹理染色成金色,与中心同色;大小不一,大小变化动画,
//! 向外扩散,不旋转)。

use std::collections::HashMap;
use std::time::Instant;

use super::{
    mat_mul, mat_scale, mat_translate, DrawCmd, DrawUniform, Mat3, Renderer, TexEntry, NOTE_SPRITE_W,
};

// Sprite sheet (note:hitfx): 1280×1536, 5 cols × 6 rows = 30 frames.
const HIT_FX_COLS: u32 = 5;
const HIT_FX_ROWS: u32 = 6;
const HIT_FX_FRAMES: u32 = 30;
const HIT_FX_SECS: f32 = 0.5;
/// 碎片粒子数量。
const SHARD_COUNT: usize = 24;

/// One live burst: canvas-pixel center + spawn time.
pub(crate) struct HitFx {
    pos: [f32; 2],
    t0: Instant,
    /// 随机种子(碎片方向/大小,创建时确定,帧率无关)。
    seed: u64,
}

impl Renderer {
    /// Spawn a hit-effect burst at a canvas-pixel position (1350×900 space,
    /// center origin). Silently ignored when no `note:hitfx` texture is loaded.
    pub fn spawn_hit_fx(&mut self, pos_canvas: [f32; 2]) {
        if self.textures.contains_key("note:hitfx") {
            self.hit_fx.push(HitFx {
                pos: pos_canvas,
                t0: Instant::now(),
                seed: self.fx_seed,
            });
            self.fx_seed = self.fx_seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        }
    }

    /// Queue draws for all live bursts (called from `render`, after notes).
    /// `letterbox` is the canvas px → NDC playfield transform.
    /// `ev_x` = `ev_y` = aspect/1.5: event positions stretch with the playfield
    /// aspect (the fx burst pos is in canvas px, pre-ev). The sprite itself
    /// keeps the uniform scale (`side`), only its position stretches.
    /// `white` = 1×1 白色纹理(纯正方形粒子用)。
    /// Free function with field-split borrows: `cmds` already holds shared
    /// borrows of `self.textures`, so a `&mut self` method would not compile.
    pub(super) fn push_hit_fx<'a>(
        hit_fx: &mut Vec<HitFx>,
        textures: &'a HashMap<String, TexEntry>,
        white: &'a wgpu::BindGroup,
        cmds: &mut Vec<DrawCmd<'a>>,
        letterbox: &Mat3,
        ev_x: f32,
        ev_y: f32,
    ) {
        hit_fx.retain(|fx| fx.t0.elapsed().as_secs_f32() < HIT_FX_SECS);
        if hit_fx.is_empty() {
            return;
        }
        let Some(fx_tex) = textures.get("note:hitfx") else {
            return;
        };
        let base_side = 1.5 * NOTE_SPRITE_W; // canvas px,~1.5 note widths
        // 淡金色。
        const GOLD: [f32; 4] = [1.0, 0.92, 0.6, 1.0];
        for fx in hit_fx.iter() {
            let age = fx.t0.elapsed().as_secs_f32();
            // 中心贴图帧:原始逻辑(固定尺寸 + 固定金色)。
            let frame = ((age / HIT_FX_SECS) * HIT_FX_FRAMES as f32).min((HIT_FX_FRAMES - 1) as f32) as u32;
            let (col, row) = (frame % HIT_FX_COLS, frame / HIT_FX_COLS);
            cmds.push(DrawCmd {
                uniform: DrawUniform {
                    model: mat_mul(
                        letterbox,
                        &mat_mul(&mat_translate(fx.pos[0] * ev_x, fx.pos[1] * ev_y), &mat_scale(base_side, base_side)),
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
            // 碎片粒子:24 个白色纹理染色(金色)正方形。全随机:
            // 方向/大小/距离都伪随机(帧率无关),向外扩散,不旋转。
            // 偏移乘 ev_x/ev_y:与中心帧同坐标系,扩散保持圆形、不飞出画布。
            let t = (age / HIT_FX_SECS).clamp(0.0, 1.0);
            for i in 0..SHARD_COUNT {
                let s = fx.seed.wrapping_mul(0x9E3779B1).wrapping_add(i as u64 * 0x85EBCA6B);
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
                let px = fx.pos[0] * ev_x + ang.cos() * dist * ev_x;
                let py = fx.pos[1] * ev_y + ang.sin() * dist * ev_y;
                let alpha = (1.0 - t).max(0.0) * 0.9;
                cmds.push(DrawCmd {
                    uniform: DrawUniform {
                        model: mat_mul(
                            letterbox,
                            &mat_mul(&mat_translate(px, py), &mat_scale(sz, sz)),
                        ),
                        // 粒子与中心贴图一起染色(金色 GOLD,带 alpha 淡出);
                        // 白色纹理做底,颜色由 tint 决定。
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
