// Derived from TeamFlos/phira prpr, GPL-3.0.
//! Beat/time conversion, ported from `prpr/src/core.rs` (`Triple`, `BpmList`).

#[derive(serde::Deserialize, serde::Serialize, Clone, Copy, Debug)]
/// Beat position in rational form `(i, n, d)` → `i + n/d` beats.
///
/// Rational representation avoids floating-point drift across chart edits:
/// moving a note by one snap increment won't accumulate rounding errors.
/// `d == 0` yields inf/NaN (unchecked, same as prpr — only well-formed
/// RPE files reach this code).
pub struct Triple(i32, u32, u32);
impl Default for Triple {
    fn default() -> Self {
        Self(0, 0, 1)
    }
}

impl Triple {
    pub fn beats(&self) -> f64 {
        self.0 as f64 + self.1 as f64 / self.2 as f64
    }

    /// Approximates a beat position as `i + n/d` (fixed denominator 1e6,
    /// reduced by gcd). // ponytail: ~1e-6-beat precision is plenty for
    /// editor split points; exact rationals need the chart's original text.
    pub fn from_beats(beats: f64) -> Self {
        let i = beats.floor() as i32;
        let frac = beats - f64::from(i);
        const D: u32 = 1_000_000;
        let n = (frac * f64::from(D)).round() as u32;
        if n >= D {
            return Self(i + 1, 0, 1);
        }
        let g = gcd(n, D);
        Self(i, n / g, D / g)
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.max(1)
}

#[derive(Default)] // the default is a dummy
pub struct BpmList {
    /// (beats, time, bpm)
    /// time in seconds
    elements: Vec<(f64, f64, f64)>,
    /// cursor for searching, value is the index of `elements`
    cursor: usize,
}

impl BpmList {
    /// Create a new BpmList from a list of (beats, bpm) pairs
    ///
    /// Basically just calculate the time for each pair(key frame)
    ///
    /// PANICS: an empty list panics on first use via out-of-bounds indexing
    /// (`elements[0]`), exactly like prpr (`core.rs`). Charts must have
    /// `BPMList` with at least one entry.
    pub fn new(ranges: Vec<(f64, f64)>) -> Self {
        let mut elements = Vec::new();
        let mut time = 0.0;
        let mut last_beats = 0.0;
        let mut last_bpm: Option<f64> = None;
        for (now_beats, bpm) in ranges {
            // bpm ≤ 0 或非有限 → 120 兜底(与空表 fallback 同语义):坏谱面
            // 时间表退化但不产生 Inf/NaN(60/0=Inf 会污染 duration →
            // seek clamp 出 Inf → 音频线程 from_secs_f64 panic)
            // hitsound-trigger 崩溃。validate 的 BpmNonPositive 已告警。
            let bpm = if bpm.is_finite() && bpm > 0.0 { bpm } else { 120.0 };
            if let Some(bpm) = last_bpm {
                time += (now_beats - last_beats) * (60. / bpm);
            }
            last_beats = now_beats;
            last_bpm = Some(bpm);
            elements.push((now_beats, time, bpm));
        }
        BpmList { elements, cursor: 0 }
    }

    /// Get the time in seconds for a given beats
    pub fn time_beats(&mut self, beats: f64) -> f64 {
        // Empty BPMList used to OOB-panic via `elements[0]`; fall back to a
        // 120 BPM assumption so malformed charts degrade instead of crashing.
        if self.elements.is_empty() {
            return beats * 0.5;
        }
        debug_assert!(!self.elements.is_empty(), "empty BPMList (prpr panics here via elements[0] OOB)");
        while let Some(kf) = self.elements.get(self.cursor + 1) {
            if kf.0 > beats {
                break;
            }
            self.cursor += 1;
        }
        while self.cursor != 0 && self.elements[self.cursor].0 > beats {
            self.cursor -= 1;
        }
        let (start_beats, time, bpm) = &self.elements[self.cursor];
        time + (beats - start_beats) * (60. / bpm)
    }

    /// Get the time in seconds for a given `i + n / d`
    pub fn time(&mut self, triple: &Triple) -> f64 {
        self.time_beats(triple.beats())
    }

    /// Get the beat coordinate for a given time in seconds
    pub fn beat(&mut self, time: f64) -> f64 {
        // Empty BPMList: 120 BPM fallback (see [`BpmList::time_beats`]).
        if self.elements.is_empty() {
            return time * 2.0;
        }
        debug_assert!(!self.elements.is_empty(), "empty BPMList (prpr panics here via elements[0] OOB)");
        while let Some(kf) = self.elements.get(self.cursor + 1) {
            if kf.1 > time {
                break;
            }
            self.cursor += 1;
        }
        while self.cursor != 0 && self.elements[self.cursor].1 > time {
            self.cursor -= 1;
        }
        let (beats, start_time, bpm) = &self.elements[self.cursor];
        beats + (time - start_time) / (60. / bpm)
    }

    /// 只读访问内部 `(beats, time, bpm)` 表(用于重建局部 BpmList)。
    pub fn elements(&self) -> &[(f64, f64, f64)] {
        &self.elements
    }
}

/// Parse an RPE time triple `[i, n, d]` (or a plain number) to beats.
///
/// Single source of truth for extra.json keyframe timing and chart event
/// timing (main.rs 与 core::extra 共用;`d == 0` 按 1 处理防除零)。
pub fn triple_to_beats(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Array(a) if a.len() >= 2 => {
            let i = a[0].as_i64()? as f64;
            let n = a[1].as_i64()? as f64;
            let d = a.get(2).and_then(|v| v.as_i64()).unwrap_or(1) as f64;
            Some(i + n / d.max(1.0))
        }
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triple_to_beats_array_and_number_forms() {
        use serde_json::json;
        // [i, n, d] → i + n/d
        assert_eq!(triple_to_beats(&json!([2, 1, 2])), Some(2.5));
        // 缺省分母 d=1
        assert_eq!(triple_to_beats(&json!([3, 0])), Some(3.0));
        // d=0 防除零(按 1)
        assert_eq!(triple_to_beats(&json!([1, 2, 0])), Some(3.0));
        // 纯数字
        assert_eq!(triple_to_beats(&json!(4.25)), Some(4.25));
        // 短数组/非数字 → None
        assert_eq!(triple_to_beats(&json!([1])), None);
        assert_eq!(triple_to_beats(&json!([1, "x"])), None);
        assert_eq!(triple_to_beats(&json!("1")), None);
    }

    #[test]
    fn empty_bpm_list_does_not_panic() {
        let mut b = BpmList::new(vec![]);
        // Used to OOB-panic via elements[0]; falls back to 120 BPM.
        assert_eq!(b.time_beats(4.0), 2.0);
        assert_eq!(b.beat(2.0), 4.0);
    }

    #[test]
    fn non_positive_bpm_degrades_to_120() {
        // 回归:坏谱面 bpm=0/负/NaN 曾把时间表污染成 Inf/NaN
        // (60/0),duration 跟着 Inf,seek 传导到音频线程
        // Duration::from_secs_f64 直接 panic。
        let mut b = BpmList::new(vec![(0.0, 0.0), (4.0, -120.0), (8.0, f64::NAN)]);
        for beats in [0.0, 2.0, 4.0, 6.0, 8.0, 10.0] {
            let t = b.time_beats(beats);
            assert!(t.is_finite(), "time_beats({beats}) = {t} 非有限");
            assert!(t >= 0.0);
        }
        // 0..4 拍按 120 兜底:4 拍 = 2.0s。
        assert!((b.time_beats(4.0) - 2.0).abs() < 1e-9);
        // 反向换算也有限。
        assert!(b.beat(10.0).is_finite());
    }
}
