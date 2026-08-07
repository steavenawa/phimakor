// Derived from TeamFlos/phira prpr, GPL-3.0.
//! Keyframe animation, ported from `prpr/src/core/anim.rs` (minus the
//! macroquad `Vector` parts).

use super::easing::{StaticTween, TweenFunction, TweenId, Tweenable};
use std::rc::Rc;

/// A single keyframe defining a value at a point in time with an easing function.
#[derive(Clone)]
pub struct Keyframe<T> {
    /// Time position of this keyframe (in beats).
    pub time: f64,
    /// Animated value at this keyframe.
    pub value: T,
    /// Easing function used when interpolating from this keyframe to the next.
    pub tween: Rc<dyn TweenFunction>,
}

impl<T> Keyframe<T> {
    /// Create a new keyframe at the given time with a value and tween identifier.
    pub fn new(time: f64, value: T, tween: TweenId) -> Self {
        Self {
            time,
            value,
            tween: StaticTween::get_rc(tween),
        }
    }
}

/// A keyframe animation track over a generic tweenable type.
///
/// Anim Tween Function is using the `tween` value of the first keyframe of an interval `(kf1, kf2)`.
/// Supports chaining multiple animations together via `next`.
#[derive(Clone)]
pub struct Anim<T: Tweenable> {
    /// Current playback time.
    pub time: f64,
    /// Ordered array of keyframes defining the animation curve.
    pub keyframes: Box<[Keyframe<T>]>,
    /// Index of the last passed keyframe for fast lookup.
    pub cursor: usize,
    /// Optional chained animation that plays after (or stacked on) this one.
    pub next: Option<Box<Anim<T>>>,
}

impl<T: Tweenable> Default for Anim<T> {
    fn default() -> Self {
        Self {
            time: 0.0,
            keyframes: [].into(),
            cursor: 0,
            next: None,
        }
    }
}

impl<T: Tweenable> Anim<T> {
    /// Create a new animation from a non-empty list of keyframes.
    pub fn new(keyframes: Vec<Keyframe<T>>) -> Self {
        assert!(!keyframes.is_empty());
        Self {
            keyframes: keyframes.into_boxed_slice(),
            time: 0.0,
            cursor: 0,
            next: None,
        }
    }

    /// Create a single-keyframe animation that always returns a fixed value.
    pub fn fixed(value: T) -> Self {
        Self {
            keyframes: Box::new([Keyframe::new(0.0, value, 0)]),
            time: 0.0,
            cursor: 0,
            next: None,
        }
    }

    /// Returns true if this animation has no keyframes and no chained animation.
    pub fn is_default(&self) -> bool {
        self.keyframes.is_empty() && self.next.is_none()
    }

    /// Chain multiple animations into a sequence by linking each to the next.
    pub fn chain(elements: Vec<Anim<T>>) -> Self {
        if elements.is_empty() {
            return Self::default();
        }
        let mut elements: Vec<_> = elements.into_iter().map(Box::new).collect();
        elements.last_mut().unwrap().next = None;
        while elements.len() > 1 {
            let last = elements.pop().unwrap();
            elements.last_mut().unwrap().next = Some(last);
        }
        *elements.into_iter().next().unwrap()
    }

    /// Advance the cursor to the correct keyframe pair for the given time.
    pub fn set_time(&mut self, time: f64) {
        if self.keyframes.is_empty() || time == self.time {
            self.time = time;
            return;
        }
        while let Some(kf) = self.keyframes.get(self.cursor + 1) {
            if kf.time > time {
                break;
            }
            self.cursor += 1;
        }
        while self.cursor != 0 && self.keyframes[self.cursor].time > time {
            self.cursor -= 1;
        }
        self.time = time;
        if let Some(next) = &mut self.next {
            next.set_time(time);
        }
    }

    fn now_opt_inner(&self) -> Option<T> {
        if self.keyframes.is_empty() {
            return None;
        }
        Some(if self.cursor == self.keyframes.len() - 1 {
            self.keyframes[self.cursor].value.clone()
        } else {
            let kf1 = &self.keyframes[self.cursor];
            let kf2 = &self.keyframes[self.cursor + 1];
            let t = (self.time - kf1.time) / (kf2.time - kf1.time);
            T::tween(&kf1.value, &kf2.value, kf1.tween.y(t as f32))
        })
    }

    /// True when the value no longer depends on time: no keyframes (always
    /// `None`) or the cursor is already past the last keyframe in every
    /// chained animation (value = last keyframe, constant). Lets `state_at`
    /// skip re-interpolating settled tracks on steady forward playback
    /// (PMCORE-72).
    pub fn frozen(&self) -> bool {
        let mut a = self;
        loop {
            if !a.keyframes.is_empty() && a.cursor + 1 < a.keyframes.len() {
                return false;
            }
            match &a.next {
                Some(n) => a = n,
                None => return true,
            }
        }
    }

    /// Interpolate the current value, returning `None` if there are no keyframes.
    /// Chains into `next` animations, summing their values. An empty chained
    /// animation contributes nothing (instead of panicking).
    pub fn now_opt(&self) -> Option<T> {
        self.now_opt_inner().map(|now| {
            if let Some(next) = &self.next {
                match next.now_opt() {
                    Some(next_now) => T::add(&now, &next_now),
                    None => now,
                }
            } else {
                now
            }
        })
    }

    /// Apply a transformation function to every keyframe value in this animation and all chains.
    pub fn map_value(&mut self, mut f: impl FnMut(T) -> T) {
        self.keyframes.iter_mut().for_each(|it| it.value = f(it.value.clone()));
        if let Some(next) = &mut self.next {
            next.map_value(f);
        }
    }
}

impl<T: Tweenable + Default> Anim<T> {
    /// Interpolate the current value, returning the default if there are no keyframes.
    pub fn now(&self) -> T {
        self.now_opt().unwrap_or_default()
    }
}

/// Type alias for a float-based animation track.
pub type AnimFloat = Anim<f32>;
