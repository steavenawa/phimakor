//! Audio playback clock (rodio 0.22) + preloaded hitsounds.
//!
//! Wall-clock based position tracking: playback position is `pos + anchor.elapsed()`
//! while playing. Pause/seek re-anchor. Interior mutability (`Cell`) because the
//! contract exposes `&self` methods and everything lives on one thread.
//!
//! [`AudioClock`] manages the rodio device and player directly on the audio thread.
//! [`AudioHandle`] mirrors the same API to the main thread via atomics + command channel.
//! [`spawn_audio_thread`] owns the clock plus a second `Chart` on a dedicated
//! trigger thread, so hitsound timing is decoupled from the winit event loop
//! (an occluded window stops `RedrawRequested` but never the hitsounds).

use std::cell::Cell;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::core::chart::Chart;

use rodio::buffer::SamplesBuffer;
use rodio::Source;

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
}

/// Decode a whole ogg into an in-memory sample buffer.
fn load_samples(path: &Path) -> Option<SamplesBuffer> {
    let decode = || -> anyhow::Result<SamplesBuffer> {
        let decoder = rodio::Decoder::try_from(File::open(path)?)?;
        let channels = decoder.channels();
        let sample_rate = decoder.sample_rate();
        let samples: Vec<f32> = decoder.collect();
        // verify: 0.22 SamplesBuffer::new(ChannelCount, SampleRate, Vec<f32>)
        Ok(SamplesBuffer::new(channels, sample_rate, samples))
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
        if let Some(res) = res_dir {
            click = load_samples(&res.join("click.ogg"));
            drag = load_samples(&res.join("drag.ogg"));
            flick = load_samples(&res.join("flick.ogg"));
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
        })
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

    /// Whether playback is currently paused.
    pub fn is_paused(&self) -> bool {
        !self.playing.get()
    }

    /// Seek to `t` seconds from the start, clamped at 0. If the previous
    /// source has exhausted (track ended — rodio's `try_seek` returns `Ok`
    /// without seeking on an empty queue), re-create and re-append it so
    /// seek + playback can restart. If the player was paused before the
    /// seek, pause it again after re-appending.
    pub fn seek(&self, t: f64) {
        let t = t.max(0.0);
        let was_playing = self.playing.get();
        // rodio: on an empty queue try_seek() is a silent no-op returning Ok,
        // so detect exhaustion explicitly and rebuild the source.
        let empty = self.player.empty();
        let seeked = self.player.try_seek(Duration::from_secs_f64(t)).is_ok() && !empty;
        if !seeked {
            if let Ok(file) = std::fs::File::open(&self.music_path) {
                if let Ok(source) = rodio::Decoder::try_from(file) {
                    self.player.append(source);
                    let _ = self.player.try_seek(Duration::from_secs_f64(t));
                    if !was_playing {
                        self.player.pause();
                    }
                }
            }
        }
        self.pos.set(t);
        self.anchor.set(Instant::now());
    }

    /// Fire a hitsound: kind 1|2 -> click (hold uses click), 3 -> flick,
    /// 4 -> drag. Silent while paused; cheap enough to call per note per frame.
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
        if let Some(buf) = buf {
            self.stream.mixer().add(buf.clone());
        }
    }
}

/// Commands from the main thread to the audio trigger thread.
pub enum AudioCmd {
    Pause(bool),
    Seek(f64),
    Quit,
}

/// Main-thread handle to the audio trigger thread. Same call shape as
/// `AudioClock` (`time`/`is_paused`/`set_paused`/`seek`) so call sites are
/// unchanged; reads go through atomics, mutations through the command channel.
pub struct AudioHandle {
    time: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    cmd: mpsc::Sender<AudioCmd>,
    // ponytail: never joined — process exit reaps the thread; `exiting` sends Quit
    #[allow(dead_code)]
    join: JoinHandle<()>,
}

impl AudioHandle {
    /// Current playback position in seconds (read from shared atomic).
    pub fn time(&self) -> f64 {
        f64::from_bits(self.time.load(Ordering::Relaxed))
    }

    /// Whether the audio thread is currently paused (read from shared atomic).
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Send a pause/resume command to the audio thread.
    pub fn set_paused(&self, paused: bool) {
        let _ = self.cmd.send(AudioCmd::Pause(paused));
    }

    /// Send a seek command to the audio thread.
    pub fn seek(&self, t: f64) {
        let _ = self.cmd.send(AudioCmd::Seek(t));
    }

    /// Signal the audio thread to shut down.
    pub fn quit(&self) {
        let _ = self.cmd.send(AudioCmd::Quit);
    }
}

/// Start the audio trigger thread: it builds its own `AudioClock` (music +
/// hitsounds) and its own `Chart` (Chart is !Send), then ticks ~2ms firing
/// `clock.hit` for note crossings. Blocks until the thread reports the clock
/// is up, so a missing/undecodable music file still fails here (caller falls
/// back to the silent `Instant` clock).
pub fn spawn_audio_thread(res_dir: &Path, chart_dir: &Path) -> anyhow::Result<AudioHandle> {
    let res_dir = res_dir.to_path_buf();
    let chart_dir = chart_dir.to_path_buf();
    let time = Arc::new(AtomicU64::new(0f64.to_bits()));
    let paused = Arc::new(AtomicBool::new(false));
    let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();
    let (ready_tx, ready_rx) = mpsc::channel::<anyhow::Result<()>>();
    let (time2, paused2) = (time.clone(), paused.clone());
    let join = std::thread::Builder::new()
        .name("hitsound-trigger".into())
        .spawn(move || {
            let built = (|| -> anyhow::Result<(AudioClock, Chart, f64)> {
                let (info, chart2) = Chart::load(&chart_dir)?;
                // prpr scene/game.rs: chart time lags audio by the total offset.
                let total_offset = (chart2.offset() + info.offset) as f64;
                let clock = AudioClock::start(&chart_dir.join(&info.music), Some(&res_dir))?;
                Ok((clock, chart2, total_offset))
            })();
            let (clock, mut chart2, total_offset) = match built {
                Ok(v) => {
                    let _ = ready_tx.send(Ok(()));
                    v
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            loop {
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        AudioCmd::Pause(p) => {
                            clock.set_paused(p);
                            paused2.store(p, Ordering::Relaxed);
                        }
                        AudioCmd::Seek(t) => clock.seek(t),
                        AudioCmd::Quit => return,
                    }
                }
                let t = clock.time();
                time2.store(t.to_bits(), Ordering::Relaxed);
                let chart_time = (t - total_offset).max(0.0);
                // Lightweight fired-only scan (no animation/visibility work).
                for fired in chart2.advance_fired(chart_time) {
                    if !fired.fake && !fired.tick && !fired.hold_tail {
                        clock.hit(fired.kind);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })?;
    // Thread panicked before reporting -> sender dropped -> recv errors.
    ready_rx.recv()??;
    Ok(AudioHandle { time, paused, cmd: cmd_tx, join })
}
