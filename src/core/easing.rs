// Derived from TeamFlos/phira prpr, GPL-3.0.
//! Tween/easing functions, ported from `prpr/src/core/tween.rs`, plus the
//! RPE-specific speed-integration tweens from `prpr/src/parse/rpe.rs` and the
//! `RPE_TWEEN_MAP` from `prpr/src/parse.rs`.

use std::{any::Any, ops::Range, rc::Rc};

use crate::core::{Color, EPS};

/// Numeric identifier for a tween function, indexing into [`TWEEN_FUNCTIONS`]
/// or [`INT_TWEEN_FUNCTIONS`].
pub type TweenId = u8;

const PI: f32 = std::f32::consts::PI;

macro_rules! f1 {
    ($fn:ident) => {
        $fn
    };
}

macro_rules! f2 {
    ($fn:ident) => {
        |x| (1. - $fn(1. - x))
    };
}

macro_rules! f3 {
    ($fn:ident) => {
        |x| {
            let x = x * 2.;
            if x < 1. {
                $fn(x) / 2.
            } else {
                1. - $fn(2. - x) / 2.
            }
        }
    };
}

#[inline]
fn sine(x: f32) -> f32 {
    1. - ((x * PI) / 2.).cos()
}

#[inline]
fn quad(x: f32) -> f32 {
    x * x
}

#[inline]
fn cubic(x: f32) -> f32 {
    x * x * x
}

#[inline]
fn quart(x: f32) -> f32 {
    x * x * x * x
}

#[inline]
fn quint(x: f32) -> f32 {
    x * x * x * x * x
}

#[inline]
fn expo(x: f32) -> f32 {
    (2.0_f32).powf(10. * (x - 1.))
}

#[inline]
fn circ(x: f32) -> f32 {
    1. - (1. - x * x).sqrt()
}

#[inline]
fn back(x: f32) -> f32 {
    const C1: f32 = 1.70158;
    const C3: f32 = C1 + 1.;
    (C3 * x - C1) * x * x
}

#[inline]
fn elastic(x: f32) -> f32 {
    const C4: f32 = (2. * PI) / 3.;
    -((2.0_f32).powf(10. * x - 10.) * ((x * 10. - 10.75) * C4).sin())
}

#[inline]
fn bounce(x: f32) -> f32 {
    const N1: f32 = 7.5625;
    const D1: f32 = 2.75;

    let x = 1. - x;
    1. - (if x < 1. / D1 {
        N1 * x.powi(2)
    } else if x < 2. / D1 {
        N1 * (x - 1.5 / D1).powi(2) + 0.75
    } else if x < 2.5 / D1 {
        N1 * (x - 2.25 / D1).powi(2) + 0.9375
    } else {
        N1 * (x - 2.625 / D1).powi(2) + 0.984375
    })
}

/// Lookup table of 33 easing functions indexed by [`TweenId`].
///
/// Indices: 0 = zero, 1 = one, 2 = linear, then groups of three (In, Out,
/// InOut) for Sine, Quad, Cubic, Quart, Quint, Expo, Circ, Back, Elastic,
/// Bounce.
#[rustfmt::skip]
pub static TWEEN_FUNCTIONS: [fn(f32) -> f32; 33] = [
	|_| 0.,			|_| 1.,			|x| x,
	/* In */		/* Out */		/* InOut */
	f1!(sine),		f2!(sine),		f3!(sine),
	f1!(quad),		f2!(quad),		f3!(quad),
	f1!(cubic),		f2!(cubic),		f3!(cubic),
	f1!(quart),		f2!(quart),		f3!(quart),
	f1!(quint),		f2!(quint),		f3!(quint),
	f1!(expo),		f2!(expo),		f3!(expo),
	f1!(circ),		f2!(circ),		f3!(circ),
	f1!(back),		f2!(back),		f3!(back),
	f1!(elastic),	f2!(elastic),	f3!(elastic),
	f1!(bounce),	f2!(bounce),	f3!(bounce),
];

macro_rules! i1 {
    ($fn:ident) => {
        $fn
    };
}

macro_rules! i2 {
    ($fn:ident) => {
        // I(x) = x + \int_1^{1-x} f(u)du = x + I(1-x) - I(1)
        |x| x + $fn(1. - x) - $fn(1.)
    };
}

macro_rules! i3 {
    ($fn:ident) => {
        |x| {
            let x2 = x * 2.;
            if x2 < 1. {
                $fn(x2) / 4.
            } else {
                x - 0.5 + $fn(2. - x2) / 4.
            }
        }
    };
}

#[inline]
fn int_sine(x: f32) -> f32 {
    // f(x) = 1 - cos(x * PI / 2)
    // I(x) = x - sin(x * PI / 2) * (2 / PI)
    x - (x * PI / 2.).sin() * (2. / PI)
}

#[inline]
fn int_quad(x: f32) -> f32 {
    x.powi(3) / 3.
}

#[inline]
fn int_cubic(x: f32) -> f32 {
    x.powi(4) / 4.
}

#[inline]
fn int_quart(x: f32) -> f32 {
    x.powi(5) / 5.
}

#[inline]
fn int_quint(x: f32) -> f32 {
    x.powi(6) / 6.
}

#[inline]
fn int_expo(x: f32) -> f32 {
    // f(x) = 2^(10x - 10)
    // I(x) = (2^(10x - 10) - 2^(-10)) / (10 * ln(2))
    let ln2 = std::f32::consts::LN_2;
    ((2.0_f32).powf(10. * x - 10.) - (2.0_f32).powf(-10.)) / (10. * ln2)
}

#[inline]
fn int_circ(x: f32) -> f32 {
    // f(x) = 1 - sqrt(1 - x^2)
    // I(x) = x - 0.5 * (x * sqrt(1 - x^2) + arcsin(x))
    x - 0.5 * (x * (1. - x * x).sqrt() + x.asin())
}

#[inline]
fn int_back(x: f32) -> f32 {
    const C1: f32 = 1.70158;
    const C3: f32 = C1 + 1.;
    // f(x) = C3 * x^3 - C1 * x^2
    // I(x) = (C3/4 * x - C1/3) * x^3
    (C3 * x / 4. - C1 / 3.) * x * x * x
}

#[inline]
fn int_elastic(x: f32) -> f32 {
    #[inline]
    fn elastic_f_antideriv(x: f32) -> f32 {
        const C4: f32 = (2. * PI) / 3.;
        let a = std::f32::consts::LN_2;
        let b = C4;
        let u = 10. * x - 10.;
        let v = (x * 10. - 10.75) * b;

        -((2.0_f32).powf(u) / (10. * (a * a + b * b))) * (a * v.sin() - b * v.cos())
    }
    elastic_f_antideriv(x) - elastic_f_antideriv(0.)
}

#[inline]
fn int_bounce(x: f32) -> f32 {
    #[inline]
    fn bounce_h(u: f32) -> f32 {
        const N1: f32 = 7.5625;
        const D1: f32 = 2.75;

        let h1 = |u: f32| N1 / 3. * u.powi(3);
        let end1 = 1. / D1;
        let val1 = h1(end1);

        let h2 = |u: f32| N1 / 3. * (u - 1.5 / D1).powi(3) + 0.75 * u;
        let end2 = 2. / D1;
        let c2 = val1 - h2(end1);
        let val2 = h2(end2) + c2;

        let h3 = |u: f32| N1 / 3. * (u - 2.25 / D1).powi(3) + 0.9375 * u;
        let end3 = 2.5 / D1;
        let c3 = val2 - h3(end2);
        let val3 = h3(end3) + c3;

        let h4 = |u: f32| N1 / 3. * (u - 2.625 / D1).powi(3) + 0.984375 * u;
        let c4 = val3 - h4(end3);

        if u < end1 {
            h1(u)
        } else if u < end2 {
            h2(u) + c2
        } else if u < end3 {
            h3(u) + c3
        } else {
            h4(u) + c4
        }
    }

    x - bounce_h(1.) + bounce_h(1. - x)
}

/// Lookup table of 33 integrated easing functions indexed by [`TweenId`].
///
/// Same index layout as [`TWEEN_FUNCTIONS`]; each entry is the definite
/// integral from 0 to x of the corresponding easing curve.
#[rustfmt::skip]
pub static INT_TWEEN_FUNCTIONS: [fn(f32) -> f32; 33] = [
    |_| 0.,				|x| x,			|x| x * x / 2.,
    /* In */			/* Out */			/* InOut */
    i1!(int_sine),		i2!(int_sine),		i3!(int_sine),
    i1!(int_quad),		i2!(int_quad),		i3!(int_quad),
    i1!(int_cubic),		i2!(int_cubic),		i3!(int_cubic),
    i1!(int_quart),		i2!(int_quart),		i3!(int_quart),
    i1!(int_quint),		i2!(int_quint),		i3!(int_quint),
    i1!(int_expo),		i2!(int_expo),		i3!(int_expo),
    i1!(int_circ),		i2!(int_circ),		i3!(int_circ),
    i1!(int_back),		i2!(int_back),		i3!(int_back),
    i1!(int_elastic),	i2!(int_elastic),	i3!(int_elastic),
    i1!(int_bounce),	i2!(int_bounce),	i3!(int_bounce),
];

/// Trait for easing functions: `y(x)` maps an input `x` in `[0, 1]` to an
/// eased output (not necessarily in `[0, 1]`).
pub trait TweenFunction {
    /// Evaluate the easing curve at `x ∈ [0, 1]`.
    fn y(&self, x: f32) -> f32;
    fn as_any(&self) -> &dyn Any;

    /// Approximate the derivative at `x` via central differences.
    fn derivative(&self, x: f32) -> f32 {
        let eps = 1e-6;
        let l = (x - eps).max(1e-7);
        let r = (x + eps).min(1. - 1e-7);
        if r <= l {
            return 0.;
        }
        (self.y(r) - self.y(l)) / (r - l)
    }
}

/// A tween that dispatches to [`TWEEN_FUNCTIONS`] by index.
pub struct StaticTween(pub TweenId);
impl TweenFunction for StaticTween {
    fn y(&self, x: f32) -> f32 {
        TWEEN_FUNCTIONS[self.0 as usize](x)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl StaticTween {
    /// Wrap a [`TweenId`] in an `Rc<dyn TweenFunction>`.
    pub fn get_rc(tween: TweenId) -> Rc<dyn TweenFunction> {
        // ponytail: prpr caches these in a thread_local; one small alloc per
        // keyframe at load time is fine, add the cache back if profiling says so.
        Rc::new(StaticTween(tween))
    }
}

/// A tween that dispatches to [`INT_TWEEN_FUNCTIONS`] by index.
pub struct IntStaticTween(pub TweenId);
impl TweenFunction for IntStaticTween {
    fn y(&self, x: f32) -> f32 {
        INT_TWEEN_FUNCTIONS[self.0 as usize](x)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl IntStaticTween {
    /// Wrap a [`TweenId`] in an `Rc<dyn TweenFunction>` backed by the integral
    /// table.
    pub fn get_rc(tween: TweenId) -> Rc<dyn TweenFunction> {
        Rc::new(IntStaticTween(tween))
    }
}

/// An integrated tween clamped to an arbitrary `x`/`y` range.
pub struct IntClampedTween {
    tween_id: TweenId,
    x_range: Range<f32>,
    y_range: Range<f32>,
    base: f32,
}
impl TweenFunction for IntClampedTween {
    fn y(&self, x: f32) -> f32 {
        let denom = self.y_range.end - self.y_range.start;
        if !denom.is_finite() || denom.abs() < 1e-8 {
            return x * x / 2.;
        }

        let x = f32::tween(&self.x_range.start, &self.x_range.end, x);
        let int = INT_TWEEN_FUNCTIONS[self.tween_id as usize](x) - self.base - self.y_range.start * (x - self.x_range.start);
        let scale = (self.x_range.end - self.x_range.start) * denom;
        int / scale
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl IntClampedTween {
    /// Create an integrated clamped tween from a standard [`TweenId`] and an
    /// `x` range; the `y` range is computed automatically (normalized for
    /// non-monotone tweens).
    pub fn new(tween_id: TweenId, x_range: Range<f32>) -> Self {
        let tween = TWEEN_FUNCTIONS[tween_id as usize];
        let (a, b) = (tween(x_range.start), tween(x_range.end));
        let y_range = a.min(b)..a.max(b);
        let base = INT_TWEEN_FUNCTIONS[tween_id as usize](x_range.start);
        Self {
            tween_id,
            x_range,
            y_range,
            base,
        }
    }
}

/// A standard tween clamped to given `x` and `y` ranges.
///
/// NOTE: the y range is the interval between the tween's values at the range
/// endpoints. Non-monotone tweens (Back/Elastic/Bounce) overshoot beyond it
/// mid-interval; the range is normalized (min..max) so it stays well-formed.
pub struct ClampedTween(pub TweenId, pub Range<f32>, pub Range<f32>);
impl TweenFunction for ClampedTween {
    fn y(&self, x: f32) -> f32 {
        let y = TWEEN_FUNCTIONS[self.0 as usize](f32::tween(&self.1.start, &self.1.end, x));
        let span = self.2.end - self.2.start;
        if span.abs() < 1e-6 {
            return 0.5; // degenerate range: no meaningful clamp
        }
        (y - self.2.start) / span
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl ClampedTween {
    /// Create a clamped tween from a [`TweenId`] and `x` range; the `y` range
    /// is computed automatically (normalized for non-monotone tweens).
    pub fn new(tween: TweenId, range: Range<f32>) -> Self {
        let f = TWEEN_FUNCTIONS[tween as usize];
        let (a, b) = (f(range.start), f(range.end));
        let y_range = a.min(b)..a.max(b);
        Self(tween, range, y_range)
    }
}

/// The numerical integral of an arbitrary [`TweenFunction`] via Gauss–Legendre
/// quadrature (3-point).
pub struct GeneralIntTween(Rc<dyn TweenFunction>);

impl GeneralIntTween {
    /// Wrap a tween function for numerical integration.
    pub fn new(tween: Rc<dyn TweenFunction>) -> Self {
        Self(tween)
    }
}

impl TweenFunction for GeneralIntTween {
    fn y(&self, x: f32) -> f32 {
        let sqrt_06: f32 = 0.7745967;
        let nodes: [f32; 3] = [-sqrt_06, 0.0, sqrt_06];
        let weights: [f32; 3] = [5.0 / 9.0, 8.0 / 9.0, 5.0 / 9.0];

        let radius = x / 2.0;

        let sum: f32 = nodes
            .iter()
            .zip(weights.iter())
            .map(|(&vi, &wi)| {
                let sample_x = radius * (vi + 1.0);
                wi * self.0.y(sample_x)
            })
            .sum();

        radius * sum
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// https://github.com/gre/bezier-easing

const SAMPLE_TABLE_SIZE: usize = 21;
const SAMPLE_STEP: f32 = 1. / (SAMPLE_TABLE_SIZE - 1) as f32;
const NEWTON_MIN_STEP: f32 = 1e-3;
const NEWTON_ITERATIONS: usize = 4;
const SUBDIVISION_PRECISION: f32 = 1e-7;
const SUBDIVISION_MAX_ITERATION: usize = 10;
const SLOPE_EPS: f32 = 1e-7;

/// A cubic Bézier easing curve parameterized by two control points.
pub struct BezierTween {
    sample_table: [f32; SAMPLE_TABLE_SIZE],
    pub p1: (f32, f32),
    pub p2: (f32, f32),
}

impl TweenFunction for BezierTween {
    fn y(&self, x: f32) -> f32 {
        Self::sample(self.p1.1, self.p2.1, self.t_for_x(x))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl BezierTween {
    #[inline]
    fn coefficients(x1: f32, x2: f32) -> (f32, f32, f32) {
        ((x1 - x2) * 3. + 1., x2 * 3. - x1 * 6., x1 * 3.)
    }

    #[inline]
    fn sample(x1: f32, x2: f32, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(x1, x2);
        ((a * t + b) * t + c) * t
    }
    #[inline]
    fn slope(x1: f32, x2: f32, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(x1, x2);
        (a * 3. * t + b * 2.) * t + c
    }

    fn newton_raphson_iterate(x: f32, mut t: f32, x1: f32, x2: f32) -> f32 {
        for _ in 0..NEWTON_ITERATIONS {
            let slope = Self::slope(x1, x2, t);
            if slope <= SLOPE_EPS {
                return t;
            }
            let diff = Self::sample(x1, x2, t) - x;
            t -= diff / slope;
        }
        t
    }

    fn binary_subdivide(x: f32, mut l: f32, mut r: f32, x1: f32, x2: f32) -> f32 {
        let mut t = (l + r) / 2.;
        for _ in 0..SUBDIVISION_MAX_ITERATION {
            let diff = Self::sample(x1, x2, t) - x;
            if diff.abs() <= SUBDIVISION_PRECISION {
                break;
            }
            if diff > 0. {
                r = t;
            } else {
                l = t;
            }
            t = (l + r) / 2.;
        }
        t
    }

    /// Solve for the parameter `t` such that the Bézier curve's x-coordinate
    /// equals the given `x`.
    pub fn t_for_x(&self, x: f32) -> f32 {
        if x == 0. || x == 1. {
            return x;
        }
        let id = (x / SAMPLE_STEP) as usize;
        let id = id.min(SAMPLE_TABLE_SIZE - 1);
        let dist = (x - self.sample_table[id]) / (self.sample_table[id + 1] - self.sample_table[id]);
        let init_t = SAMPLE_STEP * (id as f32 + dist);
        match Self::slope(self.p1.0, self.p2.0, init_t) {
            y if y <= SLOPE_EPS => init_t,
            y if y >= NEWTON_MIN_STEP => Self::newton_raphson_iterate(x, init_t, self.p1.0, self.p2.0),
            _ => Self::binary_subdivide(x, SAMPLE_STEP * id as f32, SAMPLE_STEP * (id + 1) as f32, self.p1.0, self.p2.0),
        }
    }

    /// Create a new Bézier tween with the given control points `(x1, y1)` and
    /// `(x2, y2)`.
    pub fn new(p1: (f32, f32), p2: (f32, f32)) -> Self {
        Self {
            sample_table: std::array::from_fn(|i| Self::sample(p1.0, p2.0, i as f32 * SAMPLE_STEP)),
            p1,
            p2,
        }
    }
}

/// The "major" easing category (sine, quad, cubic, …).
#[repr(u8)]
pub enum TweenMajor {
    Plain,
    Sine,
    Quad,
    Cubic,
    Quart,
    Quint,
    Expo,
    Circ,
    Back,
    Elastic,
    Bounce,
}

/// The "minor" easing variant: in, out, or in-out.
#[repr(u8)]
pub enum TweenMinor {
    In,
    Out,
    InOut,
}

/// Combine a [`TweenMajor`] and [`TweenMinor`] into a [`TweenId`].
pub const fn easing_from(major: TweenMajor, minor: TweenMinor) -> TweenId {
    major as u8 * 3 + minor as u8
}

/// RPE `easingType` → tween id. Indices 0..=29; out-of-range falls back to
/// `RPE_TWEEN_MAP[0]` (linear) at the call site. From `prpr/src/parse.rs`.
#[rustfmt::skip]
pub const RPE_TWEEN_MAP: [TweenId; 30] = {
    use TweenMajor::*;
    use TweenMinor::*;
    [
        2, 2, // linear
        easing_from(Sine, Out), easing_from(Sine, In),
        easing_from(Quad, Out), easing_from(Quad, In),
        easing_from(Sine, InOut), easing_from(Quad, InOut),
        easing_from(Cubic, Out), easing_from(Cubic, In),
        easing_from(Quart, Out), easing_from(Quart, In),
        easing_from(Cubic, InOut), easing_from(Quart, InOut),
        easing_from(Quint, Out), easing_from(Quint, In),
        easing_from(Expo, Out), easing_from(Expo, In),
        easing_from(Circ, Out), easing_from(Circ, In),
        easing_from(Back, Out), easing_from(Back, In),
        easing_from(Circ, InOut), easing_from(Back, InOut),
        easing_from(Elastic, Out), easing_from(Elastic, In),
        easing_from(Bounce, Out), easing_from(Bounce, In),
        easing_from(Bounce, InOut), easing_from(Elastic, InOut),
    ]
};

/// Trait for values that can be linearly interpolated (tweened).
pub trait Tweenable: Clone {
    /// Linearly interpolate between `x` and `y` at parameter `t` ∈ [0, 1].
    fn tween(x: &Self, y: &Self, t: f32) -> Self;
    /// Combine two values for chained animations (see [`Anim::chain`]);
    /// usually component-wise addition.
    fn add(_x: &Self, _y: &Self) -> Self {
        panic!("Tweenable::add not implemented for {}", std::any::type_name::<Self>())
    }
}

impl Tweenable for f32 {
    fn tween(x: &Self, y: &Self, t: f32) -> Self {
        x + (y - x) * t
    }

    fn add(x: &Self, y: &Self) -> Self {
        x + y
    }
}

impl Tweenable for f64 {
    fn tween(x: &Self, y: &Self, t: f32) -> Self {
        x + (y - x) * t as f64
    }

    fn add(x: &Self, y: &Self) -> Self {
        x + y
    }
}

impl Tweenable for Color {
    fn tween(x: &Self, y: &Self, t: f32) -> Self {
        Self {
            r: f32::tween(&x.r, &y.r, t),
            g: f32::tween(&x.g, &y.g, t),
            b: f32::tween(&x.b, &y.b, t),
            a: f32::tween(&x.a, &y.a, t),
        }
    }

    /// Component-wise addition for chained color animations.
    fn add(x: &Self, y: &Self) -> Self {
        Self { r: x.r + y.r, g: x.g + y.g, b: x.b + y.b, a: x.a + y.a }
    }
}

// ---------------------------------------------------------------------------
// Speed integration (from prpr/src/parse/rpe.rs)
// ---------------------------------------------------------------------------

/// Determines how speed-integral tweens are evaluated.
#[derive(Copy, Clone)]
pub enum SpeedEasingMode {
    /// Uses the derivative of the tween to compute speed.
    Legacy,
    /// Uses the integral of the tween to compute speed.
    Modern,
}

/// A tween that integrates a speed function to produce a position curve.
///
/// Wraps a [`TweenFunction`] and applies a linear transform `y(x)·k + b·x`,
/// then normalizes by the total area so `y(0) = 0` and `y(1) = 1`.
pub struct SpeedIntegralTween {
    tween: Rc<dyn TweenFunction>,
    k: f32,
    b: f32,
    total: f32,
}

impl SpeedIntegralTween {
    /// Try to create a speed-integral tween. Returns `None` if the total area
    /// is zero or non-finite.
    pub fn try_create(tween: Rc<dyn TweenFunction>, k: f32, b: f32) -> Option<(Rc<dyn TweenFunction>, f32)> {
        let mut result = Self { tween, k, b, total: 0. };
        let total = result.partial(1.);
        if !total.is_finite() || total.abs() < EPS as f32 {
            return None;
        }
        result.total = total;
        Some((Rc::new(result), total))
    }

    fn partial(&self, x: f32) -> f32 {
        self.tween.y(x) * self.k + self.b * x
    }
}

impl TweenFunction for SpeedIntegralTween {
    fn y(&self, x: f32) -> f32 {
        if x <= 0. {
            return 0.;
        }
        if x >= 1. {
            return 1.;
        }
        let y = self.partial(x) / self.total;
        if y.is_finite() {
            y
        } else {
            x
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Build a linear speed tween between `start_speed` and `end_speed`.
pub fn speed_linear_tween(start_speed: f32, end_speed: f32) -> Rc<dyn TweenFunction> {
    if (start_speed - end_speed).abs() < EPS as f32 {
        StaticTween::get_rc(2)
    } else if start_speed.abs() > end_speed.abs() {
        Rc::new(ClampedTween::new(7 /*quadOut*/, 0.0..(1. - end_speed / start_speed)))
    } else {
        Rc::new(ClampedTween::new(6 /*quadIn*/, (start_speed / end_speed)..1.))
    }
}

/// Build a speed tween for a segment using the given easing curve and mode.
///
/// Returns the tween and the total area (used for timing). Falls back to
/// [`speed_linear_tween`] when the computation fails.
pub fn speed_segment_tween(mode: SpeedEasingMode, start_speed: f32, end_speed: f32, tween: Rc<dyn TweenFunction>) -> (Rc<dyn TweenFunction>, f32) {
    let (tween, total) = match mode {
        SpeedEasingMode::Legacy => {
            let df0 = tween.derivative(0.);
            let df1 = tween.derivative(1.);
            let denom = df1 - df0;
            if !denom.is_finite() || denom.abs() < 1e-8 {
                return (speed_linear_tween(start_speed, end_speed), (start_speed + end_speed) / 2.);
            }
            let k = (end_speed - start_speed) / denom;
            let b = start_speed - k * df0;
            SpeedIntegralTween::try_create(tween, k, b)
        }
        SpeedEasingMode::Modern => {
            let int_tween: Rc<dyn TweenFunction> = if let Some(s) = tween.as_any().downcast_ref::<StaticTween>() {
                IntStaticTween::get_rc(s.0)
            } else if let Some(s) = tween.as_any().downcast_ref::<ClampedTween>() {
                Rc::new(IntClampedTween::new(s.0, s.1.clone()))
            } else {
                Rc::new(GeneralIntTween::new(tween))
            };
            SpeedIntegralTween::try_create(int_tween, end_speed - start_speed, start_speed)
        }
    }
    .unwrap_or_else(|| (speed_linear_tween(start_speed, end_speed), (start_speed + end_speed) / 2.));
    (tween, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::anim::{Anim, Keyframe};
    use crate::core::Color;

    #[test]
    fn clamped_tween_non_monotone_range_normalized() {
        // Back tween overshoots: f(0) and f(1) both > 1 or < 1. The y range
        // must stay well-formed (start <= end) so y() never divides by a
        // negative span.
        let id = RPE_TWEEN_MAP[20]; // backOut
        let c = ClampedTween::new(id, 0.2..0.8);
        assert!(c.2.start <= c.2.end);
        for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let y = c.y(t);
            assert!(y.is_finite(), "y({t}) not finite");
        }
        // Degenerate range: no division by zero.
        let d = ClampedTween::new(id, 0.5..0.5);
        assert!((d.y(0.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn color_chain_does_not_panic() {
        // Tweenable::add was unimplemented!() for Color; chained color
        // animations panicked. Component-wise add keeps chains working.
        let a = Anim::new(vec![Keyframe::new(0.0, Color::WHITE, 0)]);
        let b = Anim::new(vec![Keyframe::new(0.0, Color::WHITE, 0)]);
        let mut chained = Anim::chain(vec![a, b]);
        chained.set_time(0.5);
        let v = chained.now_opt().expect("chained color anim must resolve");
        assert!((v.r - 2.0).abs() < 1e-6);
    }

    #[test]
    fn empty_chain_contributes_nothing() {
        let a = Anim::new(vec![Keyframe::new(0.0, 1.0f32, 0)]);
        let empty = Anim::default();
        let mut chained = Anim::chain(vec![a, empty]);
        chained.set_time(0.5);
        assert!((chained.now_opt().unwrap() - 1.0).abs() < 1e-6);
    }
}
