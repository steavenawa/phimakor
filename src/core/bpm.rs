// Derived from TeamFlos/phira prpr, GPL-3.0.
//! Beat/time conversion, ported from `prpr/src/core.rs` (`Triple`, `BpmList`).

#[derive(serde::Deserialize, Clone, Copy, Debug)]
/// `(i, n, d)`: `i + n / d`. `d == 0` yields inf/NaN (unchecked, same as prpr).
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
}
