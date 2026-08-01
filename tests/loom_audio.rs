//! Loom concurrency models for the audio clock + command channel.
//!
//! Run with: `RUSTFLAGS="--cfg loom" cargo test --release --test loom_audio`
//!
//! NOTE: loom's generator-based scheduler is unreliable on Windows (known
//! `STATUS_STACK_OVERFLOW` in `loom::rt::thread`), so these tests are skipped
//! there. Run them on Linux/WSL instead:
//! `RUSTFLAGS="--cfg loom" cargo +nightly test --release --test loom_audio`

#![cfg(not(windows))]

use loom::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use loom::sync::mpsc;
use loom::sync::Arc;

/// Mirrors `AudioHandle::time()`: f64 is transported as its u64 bit pattern
/// through an atomic, then reinterpreted. The invariant we model-check: no
/// interleaving ever produces a NaN or otherwise corrupted bit pattern (i.e.
/// the bit pattern always equals some value the writer stored).
#[test]
fn f64_atomic_bit_pattern_never_corrupts() {
    loom::model(|| {
        let time = Arc::new(AtomicU64::new(0f64.to_bits()));
        let writer = time.clone();
        let reader = time.clone();

        // Writer: simulates the audio trigger thread storing clock.time().
        let wt = loom::thread::spawn(move || {
            for i in 0..8u64 {
                let t = (i as f64) * 0.25;
                writer.store(t.to_bits(), Ordering::Relaxed);
            }
        });

        // Reader: simulates the main thread's AudioHandle::time().
        let rt = loom::thread::spawn(move || {
            for _ in 0..8 {
                let bits = reader.load(Ordering::Relaxed);
                let t = f64::from_bits(bits);
                // Any interleaving must yield a value one of the stores made,
                // or the initial 0.0 — never garbage from a torn load.
                assert!(t.is_finite() && t >= 0.0 && t <= 2.0, "corrupt f64: {t}");
            }
        });

        wt.join().unwrap();
        rt.join().unwrap();
    });
}

/// Mirrors the pause flag handoff between the audio thread and the handle:
/// `AudioCmd::Pause(bool)` goes over the channel, and the thread stores the
/// value into the shared `paused` atomic. The main thread must never observe
/// a pause command being "lost" — after a pause is sent and the thread has
/// acknowledged by storing, the flag is visible. (We model exactly one
/// consumer + one producer, so ordering is total; the interesting part is
/// that the store after recv is always observable.)
#[test]
fn pause_flag_delivery_is_total() {
    loom::model(|| {
        let paused = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let flag = paused.clone();

        let worker = loom::thread::spawn(move || {
            // audio thread loop: drain commands, mirror into atomic
            while let Ok(cmd) = rx.recv() {
                flag.store(cmd, Ordering::Relaxed);
            }
        });

        // main thread: send pause, then read flag
        tx.send(true).unwrap();
        let seen = paused.load(Ordering::Relaxed);
        drop(tx); // allow worker to exit
        worker.join().unwrap();
        // The worker stored true before exiting; a relaxed read after join
        // is guaranteed to observe it (join = happens-before).
        assert_eq!(seen || paused.load(Ordering::Relaxed), true);
    });
}

/// Models the `AudioHandle` time clock: the audio thread continuously updates
/// `time` while the main thread may issue seeks (channel) and read `time`.
/// Invariant: reads always see a monotonic-ish valid clock value; the seek
/// command never races with the store such that a torn value appears.
#[test]
fn seek_and_clock_reads_never_torn() {
    loom::model(|| {
        let time = Arc::new(AtomicU64::new(0f64.to_bits()));
        let (tx, rx) = mpsc::channel::<f64>();

        let clock = time.clone();
        let seek_rx = rx;
        let audio = loom::thread::spawn(move || {
            let mut t = 0.0f64;
            // process seeks, advance clock, store — interleaved
            loop {
                match seek_rx.try_recv() {
                    Ok(seek) => t = seek,
                    Err(_) => {
                        t += 0.5;
                        if t > 4.0 { break; }
                    }
                }
                clock.store(t.to_bits(), Ordering::Relaxed);
            }
        });

        let reader = time.clone();
        let main = loom::thread::spawn(move || {
            for _ in 0..10 {
                let bits = reader.load(Ordering::Relaxed);
                let t = f64::from_bits(bits);
                assert!(t.is_finite() && (0.0..=4.5).contains(&t), "torn read: {t}");
            }
        });

        let _ = tx.send(1.0);
        let _ = tx.send(3.0);
        drop(tx);
        audio.join().unwrap();
        main.join().unwrap();
    });
}
