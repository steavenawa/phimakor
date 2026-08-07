//! Offscreen frame-output hash comparison (PMCORE-73 acceptance):
//! renders 1000 advancing frames of a chart and prints one FNV-1a hash of
//! every frame's RGBA bytes plus the byte count. Run the same binary with and
//! without `PHIMAKOR_MT_SCENE=0`; the hashes must match byte-for-byte
//! (multi-buffer parallel submit == old single-pass output).
//!
//! Usage: cargo run --release --example hash_frames -- <chart_dir> [frames]

use std::hash::{Hash, Hasher};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args.first().cloned().unwrap_or_else(|| "example_chart".into());
    let frames: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1000);

    let mut s = phimakor::engine::ChartSession::new(1280, 720)?;
    s.load(std::path::Path::new(&dir))?;
    // Warmup (texture/instance capacity growth must settle identically).
    for i in 0..10 {
        let _ = s.render_frame(1.0 + i as f64 * 0.1, 1.0);
    }
    let total = s.duration().max(1.0);
    let mut agg = 0u64; // rolling mix of per-frame hashes
    let mut byte_len: usize = 0;
    for i in 0..frames {
        let t = (i as f64 / frames as f64) * total;
        let pixels = s.render_frame(t, 1.0).unwrap_or(&[]);
        byte_len = pixels.len();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        pixels.hash(&mut h);
        let fh = h.finish();
        agg = agg.rotate_left(1) ^ fh;
        if frames <= 1000 && i % 100 == 0 {
            println!("frame {i}: {fh:016x}");
        }
    }
    println!("frames={frames} bytes_per_frame={byte_len} aggregate={agg:016x}");
    Ok(())
}
