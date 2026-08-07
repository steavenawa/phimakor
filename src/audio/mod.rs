//! Audio playback clock (rodio 0.22) + preloaded hitsounds.
//!
//! Wall-clock based position tracking: playback position is `pos + anchor.elapsed()`
//! while playing. Pause/seek re-anchor. Interior mutability (`Cell`) because the
//! contract exposes `&self` methods and everything lives on one thread.
//!
//! [`AudioClock`] manages the rodio device and player directly on the audio thread.
//! [`AudioHandle`] mirrors the same API to the main thread via atomics + command channel.
//! [`spawn_audio_thread`] owns the clock plus a precomputed hitsound schedule
//! (`(hit time, note kind)` pairs derived from the unified trigger table —
//! `Chart::fire_events` / its pre-Chart entry `Chart::fire_events_from_rpe`, see
//! `crate::core::chart`) on a dedicated trigger thread, so hitsound timing is
//! decoupled from the winit event loop (an occluded window stops
//! `RedrawRequested` but never the hitsounds).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rodio::buffer::SamplesBuffer;
use rodio::Source;

/// Maximum hitsounds mixed at once. Past this, new hits are dropped instead
/// of queued — dense chords / scrubs on extreme charts would otherwise flood
/// the mixer and stall the audio callback (crackle, frozen position, seeks
/// blocking for seconds).
const MAX_PENDING_HITS: usize = 32;
/// How long a hitsound can plausibly keep playing; entries older than this
/// are assumed finished and dropped from the pending count.
const HIT_TAIL: Duration = Duration::from_millis(200);
/// Single-hitsound volume relative to the music (soft limiter base). Kept
/// below 1 so hitsounds accent the beat instead of drowning out the track.
const HIT_BASE_VOLUME: f32 = 0.6;

pub struct AudioClock {
    // Kept alive: dropping the stream stops all playback.
    stream: rodio::MixerDeviceSink, // verify: 0.22 renamed OutputStream -> MixerDeviceSink
    player: rodio::Player,          // verify: 0.22 renamed Sink -> Player
    /// Playback position (seconds) at `anchor`.
    pos: Cell<f64>,
    anchor: Cell<Instant>,
    playing: Cell<bool>,
    // Preloaded hitsounds; None when the file was missing/undecodable.
    hit_click: Option<SamplesBuffer>,
    hit_drag: Option<SamplesBuffer>,
    hit_flick: Option<SamplesBuffer>,
    /// Path to the music file, stored for re-creating the source after exhaustion.
    music_path: std::path::PathBuf,
    /// Heap bytes of the preloaded hitsound buffers.
    hit_bytes: usize,
    /// Timestamps of recently added hitsounds, for the polyphony cap
    /// (see [`MAX_PENDING_HITS`]). Only touched by the trigger thread.
    pending_hits: RefCell<VecDeque<Instant>>,
}

/// Decode a whole ogg into an in-memory sample buffer. Returns the buffer
/// and its heap byte size (f32 samples).
fn load_samples(path: &Path) -> Option<(SamplesBuffer, usize)> {
    let decode = || -> anyhow::Result<(SamplesBuffer, usize)> {
        let decoder = rodio::Decoder::try_from(File::open(path)?)?;
        let channels = decoder.channels();
        let sample_rate = decoder.sample_rate();
        let samples: Vec<f32> = decoder.collect();
        let bytes = samples.len() * 4;
        // verify: 0.22 SamplesBuffer::new(ChannelCount, SampleRate, Vec<f32>)
        Ok((SamplesBuffer::new(channels, sample_rate, samples), bytes))
    };
    match decode() {
        Ok(buf) => Some(buf),
        Err(e) => {
            eprintln!("warning: cannot load hitsound {}: {e:#}", path.display());
            None
        }
    }
}

impl AudioClock {
    /// Start playback of `path`. If `res_dir` is given, click/drag/flick.ogg
    /// are preloaded from it as hitsounds (missing files only warn).
    pub fn start(path: &Path, res_dir: Option<&Path>) -> anyhow::Result<Self> {
        // verify: 0.22 renamed OutputStreamBuilder::open_default_stream -> DeviceSinkBuilder::open_default_sink
        let mut stream = rodio::DeviceSinkBuilder::open_default_sink()?;
        stream.log_on_drop(false); // suppress "Dropping DeviceSink" on exit
        let player = rodio::Player::connect_new(stream.mixer());
        // Decoder::try_from(File) wraps in BufReader and sets byte_len, so the
        // source is seekable (mp3/wav/ogg/flac via symphonia).
        let source = rodio::Decoder::try_from(File::open(path)?)?;
        player.append(source);

        let (mut click, mut drag, mut flick) = (None, None, None);
        let mut hit_bytes = 0usize;
        if let Some(res) = res_dir {
            for (slot, name) in [(&mut click, "click.ogg"), (&mut drag, "drag.ogg"), (&mut flick, "flick.ogg")] {
                if let Some((buf, bytes)) = load_samples(&res.join(name)) {
                    hit_bytes += bytes;
                    *slot = Some(buf);
                }
            }
        }

        let music_path = path.to_path_buf();
        Ok(Self {
            stream,
            player,
            pos: Cell::new(0.0),
            anchor: Cell::new(Instant::now()),
            playing: Cell::new(true),
            hit_click: click,
            hit_drag: drag,
            hit_flick: flick,
            music_path,
            hit_bytes,
            pending_hits: RefCell::new(VecDeque::new()),
        })
    }

    /// Heap footprint of the preloaded hitsound buffers (f32 samples).
    pub fn mem_bytes(&self) -> usize {
        self.hit_bytes
    }

    /// Seconds into the track, accounting for pauses and seeks.
    /// While playing, reports the rodio backend's sample position
    /// (`Player::get_pos`, refreshed every ~5ms from the audio thread) instead
    /// of wall-clock time — the wall clock drifts from the actual playback
    /// rate and starts ahead of the audible output (device buffer latency).
    /// When the queue drains (track finished), `get_pos` freezes; snapshot it
    /// into `pos` so seeks/restarts from the end work off the real position.
    pub fn time(&self) -> f64 {
        if self.playing.get() {
            let t = self.player.get_pos().as_secs_f64();
            if self.player.empty() {
                self.pos.set(t);
            }
            t
        } else {
            self.pos.get()
        }
    }

    /// Pause or resume playback. No-op if already in the requested state.
    /// Resuming after the track ended (queue drained) re-creates and
    /// re-appends the source so playback can restart from the start.
    pub fn set_paused(&self, paused: bool) {
        if paused == !self.playing.get() {
            return;
        }
        if paused {
            self.pos.set(self.time());
            self.playing.set(false);
            self.player.pause();
        } else {
            if self.player.empty() {
                // Track ended — re-append so play() has something to play.
                if let Ok(file) = std::fs::File::open(&self.music_path) {
                    if let Ok(source) = rodio::Decoder::try_from(file) {
                        self.player.append(source);
                        let _ = self.player.try_seek(Duration::ZERO);
                    }
                }
                self.pos.set(0.0);
            }
            self.anchor.set(Instant::now());
            self.playing.set(true);
            self.player.play();
        }
    }

    /// Seek to `t` seconds from the start, clamped at 0. If the previous
    /// source has exhausted (track ended — rodio's `try_seek` returns `Ok`
    /// without seeking on an empty queue), re-create and re-append it so
    /// seek + playback can restart. If the player was paused before the
    /// seek, pause it again after re-appending.
    ///
    /// Returns `false` (leaving the clock untouched) when the music file
    /// cannot be reopened — otherwise the position would desync from what
    /// is actually audible.
    pub fn seek(&self, t: f64) -> bool {
        // [panic 兜底] 非有限秒(Inf/NaN,来自坏谱面时间链)会炸
        // Duration::from_secs_f64(用户实测 hitsound-trigger 线程崩溃);
        // 直接拒绝,保持时钟不动。
        if !t.is_finite() {
            eprintln!("warning: audio seek rejected (non-finite {t})");
            return false;
        }
        let t = t.max(0.0);
        let was_playing = self.playing.get();
        // rodio: on an empty queue try_seek() is a silent no-op returning Ok,
        // so detect exhaustion explicitly and rebuild the source.
        let empty = self.player.empty();
        let seeked = self.player.try_seek(Duration::from_secs_f64(t)).is_ok() && !empty;
        if !seeked {
            // Only an EXHAUSTED queue gets a rebuilt source. A non-empty
            // queue whose try_seek failed must be left untouched — appending
            // a fresh source here would play a duplicate from 0 after the
            // current one ends (audio/display permanently desynced).
            if !empty {
                eprintln!("warning: audio seek to {t:.2}s failed on an active queue {}", self.music_path.display());
                return false;
            }
            let reopen = std::fs::File::open(&self.music_path)
                .ok()
                .and_then(|file| rodio::Decoder::try_from(file).ok());
            match reopen {
                Some(source) => {
                    self.player.append(source);
                    if self.player.try_seek(Duration::from_secs_f64(t)).is_err() {
                        eprintln!("warning: audio seek to {t:.2}s failed after reopening {}", self.music_path.display());
                        return false;
                    }
                    if !was_playing {
                        self.player.pause();
                    }
                }
                None => {
                    eprintln!("warning: audio seek to {t:.2}s failed: cannot reopen {}", self.music_path.display());
                    return false;
                }
            }
        }
        self.pos.set(t);
        self.anchor.set(Instant::now());
        true
    }

    /// Fire a hitsound: kind 1|2 -> click (hold uses click), 3 -> flick,
    /// 4 -> drag. Silent while paused; cheap enough to call per note per frame.
    ///
    /// Non-blocking: the sound is handed to the mixer over an unbounded
    /// channel — this call never waits on the audio callback, so dense
    /// charts can't stall playback. Soft limiter: each hit is amplified to
    /// `HIT_BASE_VOLUME / sqrt(concurrent+1)`, so a single hit is audible
    /// but a chord of overlapping hits stays bounded and never drowns out
    /// the music. Polyphony-capped: beyond [`MAX_PENDING_HITS`] hits within
    /// [`HIT_TAIL`], hits are dropped (the mixer would otherwise spend its
    /// whole callback budget summing them, stalling everything downstream).
    pub fn hit(&self, kind: u8) {
        if !self.playing.get() {
            return;
        }
        let buf = match kind {
            1 | 2 => &self.hit_click,
            3 => &self.hit_flick,
            4 => &self.hit_drag,
            _ => return,
        };
        let Some(buf) = buf else { return };
        let now = Instant::now();
        let mut pending = self.pending_hits.borrow_mut();
        while let Some(&t) = pending.front() {
            if now.duration_since(t) < HIT_TAIL {
                break;
            }
            pending.pop_front();
        }
        if pending.len() >= MAX_PENDING_HITS {
            return;
        }
        // Soft limiter: concurrent count → per-hit volume (1/sqrt).
        let concurrent = pending.len() as f32 + 1.0;
        let vol = HIT_BASE_VOLUME / concurrent.sqrt();
        pending.push_back(now);
        drop(pending);
        self.stream.mixer().add(buf.clone().amplify(vol));
    }
}

/// Commands from the main thread to the audio trigger thread.
pub enum AudioCmd {
    Pause(bool),
    Seek(f64),
    /// 替换 hitsound 调度(编辑后 rebuild_chart 调用;线程换表并重装游标)。
    SetEvents(Vec<(f64, u8)>),
    Quit,
}

/// Main-thread handle to the audio trigger thread. Same call shape as
/// `AudioClock` (`time`/`is_paused`/`set_paused`/`seek`) so call sites are
/// unchanged; reads go through atomics, mutations through the command channel.
pub struct AudioHandle {
    time: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    cmd: mpsc::Sender<AudioCmd>,
    /// Heap bytes of the preloaded hitsounds (set once by the audio thread).
    mem: Arc<AtomicU64>,
    /// Set when the music queue drained (track finished); cleared by any
    /// seek or resume. Lets the main thread tell "paused at the end" from
    /// "paused mid-song" without heuristics about chart duration.
    ended: Arc<AtomicBool>,
    // ponytail: never joined — process exit reaps the thread; `exiting` sends Quit
    #[allow(dead_code)]
    join: JoinHandle<()>,
}

impl AudioHandle {
    /// Current playback position in seconds (read from shared atomic).
    pub fn time(&self) -> f64 {
        f64::from_bits(self.time.load(Ordering::Relaxed))
    }

    /// Heap footprint of the preloaded hitsound buffers (f32 samples).
    pub fn mem_bytes(&self) -> usize {
        self.mem.load(Ordering::Relaxed) as usize
    }

    /// Whether the audio thread is currently paused (read from shared atomic).
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Whether the music queue has fully drained (track finished). Cleared
    /// again by any seek or resume.
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }

    /// Send a pause/resume command to the audio thread.
    pub fn set_paused(&self, paused: bool) {
        let _ = self.cmd.send(AudioCmd::Pause(paused));
    }

    /// Send a seek command to the audio thread.
    pub fn seek(&self, t: f64) {
        let _ = self.cmd.send(AudioCmd::Seek(t));
    }

    /// Replace the hitsound schedule (editor edits rebuilt the chart): the
    /// thread swaps the table and re-arms the cursor at the current clock
    /// position, so newly added notes sound immediately and deleted ones
    /// stop sounding.
    pub fn set_events(&self, events: Vec<(f64, u8)>) {
        let _ = self.cmd.send(AudioCmd::SetEvents(events));
    }

    /// Signal the audio thread to shut down.
    pub fn quit(&self) {
        let _ = self.cmd.send(AudioCmd::Quit);
    }
}

/// Precomputed hitsound schedule with a monotonic cursor, standing in for
/// the second full `Chart` the trigger thread used to build (parse, easing
/// graph, line states — none of which hitsounds need).
struct FireCursor {
    /// (hit time on the chart clock, note kind), sorted by hit time.
    events: Vec<(f64, u8)>,
    /// Index of the first event with `time >` the last processed time
    /// (or `> seek target` after a backward seek).
    cursor: usize,
}

impl FireCursor {
    /// Fire events whose hit time lies in `(last, t]` and advance the cursor.
    /// One binary search per tick — no per-line scan, no beat math.
    fn advance(&mut self, t: f64) -> &[(f64, u8)] {
        let start = self.cursor;
        let end = start + self.events[start..].partition_point(|&(et, _)| et <= t);
        self.cursor = end;
        &self.events[start..end]
    }

    /// Backward seek to `t`: everything at `<= t` is passed for good (same
    /// strict-bound semantics as the old chart scan — a note exactly on the
    /// seek target doesn't re-fire), so events after `t` fire again when
    /// time crosses them. Forward seeks deliberately keep the cursor: notes
    /// skipped by the jump fire immediately on the next tick.
    fn seek_reset(&mut self, t: f64) {
        self.cursor = self.events.partition_point(|&(et, _)| et <= t);
    }
}

/// Start the audio trigger thread: it builds its own `AudioClock` (music +
/// hitsounds) and fires hitsounds from the precomputed schedule `events`
/// (`(hit time, kind)` pairs on the chart clock, see `Chart::fire_events`),
/// ticked by the clock's reported position
/// minus `total_offset`. Blocks until the thread reports the clock is up, so
/// a missing/undecodable music file still fails here (caller falls back to
/// the silent `Instant` clock).
pub fn spawn_audio_thread(
    res_dir: &Path,
    music_path: &Path,
    total_offset: f64,
    events: Vec<(f64, u8)>,
) -> anyhow::Result<AudioHandle> {
    let res_dir = res_dir.to_path_buf();
    let music_path = music_path.to_path_buf();
    let time = Arc::new(AtomicU64::new(0f64.to_bits()));
    let paused = Arc::new(AtomicBool::new(false));
    let mem = Arc::new(AtomicU64::new(0));
    let ended = Arc::new(AtomicBool::new(false));
    let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();
    let (ready_tx, ready_rx) = mpsc::channel::<anyhow::Result<()>>();
    let (time2, paused2, mem2, ended2) = (time.clone(), paused.clone(), mem.clone(), ended.clone());
    let join = std::thread::Builder::new()
        .name("hitsound-trigger".into())
        .spawn(move || {
            let clock = match AudioClock::start(&music_path, Some(&res_dir)) {
                Ok(clock) => {
                    let _ = ready_tx.send(Ok(()));
                    clock
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            mem2.store(clock.mem_bytes() as u64, Ordering::Relaxed);
            let mut fired = FireCursor { events, cursor: 0 };
            fired.seek_reset(0.0);
            loop {
                // Sleep until a command arrives or the poll period elapses;
                // commands wake the thread immediately. 5ms while playing
                // keeps the time atomic fresh for the renderer; 50ms while
                // paused — position then only changes via Seek commands,
                // which publish the atomic themselves.
                //
                // NOTE: the message returned by recv_timeout is consumed
                // here — it must be processed, never discarded (a dropped
                // command makes pause/seek appear dead).
                let dur = if clock.playing.get() { Duration::from_millis(5) } else { Duration::from_millis(50) };
                let first = match cmd_rx.recv_timeout(dur) {
                    Ok(cmd) => Some(cmd),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                // Coalesce commands: seeks are absolute positions, so when a
                // burst arrives (a timeline drag fires one per mouse move)
                // only the last one matters. rodio's try_seek blocks up to
                // ~5ms waiting for the audio callback to acknowledge, so
                // processing every seek would stall this thread — frozen
                // time atomic, and Pause commands stuck behind the backlog
                // (the "can't pause after scrubbing" bug).
                let mut last_seek: Option<f64> = None;
                for cmd in first.into_iter().chain(cmd_rx.try_iter()) {
                    match cmd {
                        AudioCmd::Pause(p) => {
                            clock.set_paused(p);
                            paused2.store(p, Ordering::Relaxed);
                            if !p {
                                ended2.store(false, Ordering::Relaxed);
                            }
                        }
                        AudioCmd::Quit => return,
                        AudioCmd::Seek(t) => last_seek = Some(t),
                        AudioCmd::SetEvents(events) => {
                            // 换表:游标重装到当前钟位(<= 当前位置的事件已过,不再补响;
                            // 之后的事件按新表正常触发)。同批 Seek 随后会再次 seek_reset,
                            // 以 Seek 后的位置为准。
                            fired.events = events;
                            let t = (clock.time() - total_offset).max(0.0);
                            fired.seek_reset(t);
                        }
                    }
                }
                if let Some(t) = last_seek {
                    if clock.seek(t) {
                        // Publish the new position immediately (not on the
                        // next poll), so scrubbing while paused has no
                        // poll-period lag.
                        time2.store(t.to_bits(), Ordering::Relaxed);
                        // Every seek repositions the fire cursor: notes
                        // jumped over simply don't sound (a forward jump
                        // over a dense section would otherwise flood the
                        // mixer with the whole skipped window at once),
                        // while a backward seek re-fires everything after
                        // the target when time crosses it again (replay).
                        fired.seek_reset((t - total_offset).max(0.0));
                        ended2.store(false, Ordering::Relaxed);
                    }
                }
                let t = clock.time();
                time2.store(t.to_bits(), Ordering::Relaxed);
                // Track ended: the queue drained but `playing` stays true
                // (rodio has no end callback), so the main thread can't tell
                // "paused at the end" from "still playing". Report it as
                // paused + ended — Space then sees `ended && paused` and
                // seeks 0, which resets combo/hits/score before the replay.
                if clock.playing.get() && clock.player.empty() {
                    clock.playing.set(false);
                    paused2.store(true, Ordering::Relaxed);
                    ended2.store(true, Ordering::Relaxed);
                }
                let chart_time = (t - total_offset).max(0.0);
                for &(_, kind) in fired.advance(chart_time) {
                    clock.hit(kind);
                }
            }
        })?;
    // Thread panicked before reporting -> sender dropped -> recv errors.
    ready_rx.recv()??;
    Ok(AudioHandle { time, paused, cmd: cmd_tx, mem, ended, join })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Write a small mono 16-bit PCM WAV (44100 Hz, seconds long).
    fn write_tone_wav(path: &std::path::Path, seconds: f64, hz: f64) {
        let rate = 44100u32;
        let n = (seconds * rate as f64) as u32;
        let mut data = Vec::with_capacity(44 + n as usize * 2);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + n * 2).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&rate.to_le_bytes());
        data.extend_from_slice(&(rate * 2).to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&(n * 2).to_le_bytes());
        for i in 0..n {
            let v = ((i as f64 / rate as f64 * hz * std::f64::consts::TAU).sin() * 12000.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&data).unwrap();
    }

    fn poll<T: Copy + PartialEq>(mut f: impl FnMut() -> T, want: T, timeout: Duration) -> T {
        let start = Instant::now();
        loop {
            let v = f();
            if v == want || start.elapsed() > timeout {
                return v;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Real-device end-to-end smoke test of the whole trigger thread:
    /// pause / resume / seek / seek-spam / track-end / replay. Runs inside
    /// an owned thread with a watchdog so a rodio stall fails the test
    /// instead of hanging it. Skipped when no audio device exists.
    #[test]
    fn trigger_thread_pause_seek_ended() {
        let wav = std::env::temp_dir().join("phimakor_test_tone.wav");
        write_tone_wav(&wav, 5.0, 440.0);
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        std::thread::spawn(move || {
            let run = || -> Result<(), String> {
                let handle = spawn_audio_thread(
                    Path::new("res-does-not-exist"),
                    &wav,
                    0.0,
                    vec![(0.25, 1), (0.5, 4), (0.75, 3), (1.0, 2)],
                )
                .map_err(|e| format!("spawn failed: {e:#}"))?;
                // Playing: time advances.
                let t0 = poll(|| handle.time(), 0.0, Duration::from_millis(0));
                std::thread::sleep(Duration::from_millis(300));
                let t1 = handle.time();
                if t1 < t0 + 0.2 {
                    return Err(format!("clock did not advance: {t0} -> {t1}"));
                }
                // Pause takes effect (atomic flips; time freezes).
                handle.set_paused(true);
                if !poll(|| handle.is_paused(), true, Duration::from_secs(2)) {
                    return Err("pause never took effect".into());
                }
                std::thread::sleep(Duration::from_millis(150));
                let t2 = handle.time();
                if (t2 - t1).abs() > 0.05 {
                    return Err(format!("time moved while paused: {t1} -> {t2}"));
                }
                // Seek while paused lands.
                handle.seek(1.0);
                if !poll(|| handle.time() > 0.95, true, Duration::from_secs(2)) {
                    return Err(format!("seek-while-paused did not land: {}", handle.time()));
                }
                // Resume advances from ~1.0.
                handle.set_paused(false);
                if poll(|| handle.is_paused(), false, Duration::from_secs(2)) {
                    return Err("resume never took effect".into());
                }
                std::thread::sleep(Duration::from_millis(250));
                if !(handle.time() > 1.05) {
                    return Err(format!("did not advance after resume: {}", handle.time()));
                }
                // Rapid seek spam must not hang or wedge the thread.
                for i in 0..40 {
                    handle.seek((i % 20) as f64 * 0.05);
                }
                std::thread::sleep(Duration::from_millis(200));
                // Track end: ended + auto-pause, time frozen near 5.0s.
                handle.seek(0.0);
                handle.set_paused(false);
                if !poll(|| handle.ended(), true, Duration::from_secs(8)) {
                    return Err("track-end was never detected".into());
                }
                let end = handle.time();
                if !(end >= 4.9 && end <= 5.4) {
                    return Err(format!("end position off: {end}"));
                }
                // Replay: seek from the end restarts and clears ended.
                handle.seek(0.5);
                if !poll(|| !handle.ended(), true, Duration::from_secs(2)) {
                    return Err("seek after end did not clear ended".into());
                }
                if !poll(|| handle.time() >= 0.45, true, Duration::from_secs(2)) {
                    return Err(format!("post-end seek did not land: {}", handle.time()));
                }
                // Pause still toggles after all of the above.
                handle.set_paused(true);
                if !poll(|| handle.is_paused(), true, Duration::from_secs(2)) {
                    return Err("pause broken after seek spam".into());
                }
                handle.quit();
                Ok(())
            };
            let r = match run() {
                Ok(()) => Ok(()),
                Err(e) => Err(e),
            };
            let _ = tx.send(r);
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("{e}"),
            Err(_) => panic!("trigger thread stalled (30s watchdog)"),
        }
    }
}
