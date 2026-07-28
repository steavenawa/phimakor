//! Hit-effect bursts: 30-frame sprite-sheet animation (5×6 atlas) over 0.5 s.
//!
//! Bursts live in canvas-pixel space (1350×900, center origin); `render()`
//! calls [`Renderer::push_hit_fx`] after notes so effects draw on top.

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

/// One live burst: canvas-pixel center + spawn time.
pub(crate) struct HitFx {
    pos: [f32; 2],
    t0: Instant,
}

impl Renderer {
    /// Spawn a hit-effect burst at a canvas-pixel position (1350×900 space,
    /// center origin). Silently ignored when no `note:hitfx` texture is loaded.
    pub fn spawn_hit_fx(&mut self, pos_canvas: [f32; 2]) {
        if self.textures.contains_key("note:hitfx") {
            self.hit_fx.push(HitFx {
                pos: pos_canvas,
                t0: Instant::now(),
            });
        }
    }

    /// Queue draws for all live bursts (called from `render`, after notes).
    /// `letterbox` is the canvas px → NDC playfield transform.
    /// Free function with field-split borrows: `cmds` already holds shared
    /// borrows of `self.textures`, so a `&mut self` method would not compile.
    pub(crate) fn push_hit_fx<'a>(
        hit_fx: &mut Vec<HitFx>,
        textures: &'a HashMap<String, TexEntry>,
        cmds: &mut Vec<DrawCmd<'a>>,
        letterbox: &Mat3,
    ) {
        hit_fx.retain(|fx| fx.t0.elapsed().as_secs_f32() < HIT_FX_SECS);
        if hit_fx.is_empty() {
            return;
        }
        let Some(fx_tex) = textures.get("note:hitfx") else {
            return;
        };
        let side = 1.5 * NOTE_SPRITE_W; // canvas px, ~1.5 note widths
        // Phigros gold burst tint.
        const GOLD: [f32; 4] = [1.0, 0.85, 0.35, 1.0];
        for fx in hit_fx.iter() {
            let age = fx.t0.elapsed().as_secs_f32();
            let frame = ((age / HIT_FX_SECS) * HIT_FX_FRAMES as f32).min((HIT_FX_FRAMES - 1) as f32) as u32;
            let (col, row) = (frame % HIT_FX_COLS, frame / HIT_FX_COLS);
            cmds.push(DrawCmd {
                uniform: DrawUniform {
                    model: mat_mul(
                        letterbox,
                        &mat_mul(&mat_translate(fx.pos[0], fx.pos[1]), &mat_scale(side, side)),
                    ),
                    color: GOLD,
                    // Upload is flipped (v0 = image BOTTOM), so atlas rows index
                    // from the bottom: frame row 0 (image top) sits at v = 1-1/6.
                    uv_rect: [
                        col as f32 / HIT_FX_COLS as f32,
                        1.0 - (row + 1) as f32 / HIT_FX_ROWS as f32,
                        1.0 / HIT_FX_COLS as f32,
                        1.0 / HIT_FX_ROWS as f32,
                    ],
                },
                tex: &fx_tex.bind_group,
            });
        }
    }
}
