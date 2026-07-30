//! Core chart model & evaluation — the cross-module contract.
//!
//! Derived from TeamFlos/phira `prpr` (GPL-3.0). Rendering consumes
//! [`FrameState`] only; it never touches serde models or file IO.
//!
//! Internal coordinate system (same as prpr): center origin, x ∈ [-1, 1],
//! y ∈ [-HEIGHT_RATIO, HEIGHT_RATIO], HEIGHT_RATIO = 0.83175.
//! RPE canvas is 1350×900, top-left origin.

pub mod anim;
pub mod bpm;
pub mod chart;
pub mod chart_format;
pub mod easing;
pub mod edit;
pub mod extra;
pub mod model;
pub mod stream;

pub use chart::{FrameState, LineState};

/// RGBA color, components in 0..=1. Stand-in for prpr's `macroquad::Color`
/// (core must not depend on render crates).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const WHITE: Self = Self { r: 1., g: 1., b: 1., a: 1. };

    pub fn from_rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self {
            r: r as f32 / 255.,
            g: g as f32 / 255.,
            b: b as f32 / 255.,
            a: a as f32 / 255.,
        }
    }
}

pub const HEIGHT_RATIO: f64 = 0.83175;
pub const EPS: f64 = 1e-5;
pub const RPE_WIDTH: f32 = 1350.;
pub const RPE_HEIGHT: f32 = 900.;
/// prpr: `10. / 45. / HEIGHT_RATIO`
pub const SPEED_RATIO: f64 = 10. / 45. / HEIGHT_RATIO;

/// One frame of evaluated chart state, produced by [`Chart::state_at`].
/// Renderer draws this verbatim: for each line, apply line transform
/// (translate → rotate → scale), draw line quad, then draw each note at
/// its relative offset under the same transform.
pub struct FrameStateView<'a> {
    pub time: f64,
    pub frame: &'a FrameState,
}
