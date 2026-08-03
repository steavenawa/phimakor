//! GPU 渲染耗时测量:离屏渲染 N 帧,用 wgpu timestamp query 测每帧
//! GPU 实际执行时间(场景 pass + 后处理)。
//!
//! 用法:
//!   $env:PHIMAKOR_GPU_TIMING = "1"
//!   cargo run --release --bin gpu_measure -- <chart_dir> [frames] [width] [height]
//!
//! 输出:GPU 帧耗时统计(avg/p50/p95/max)。

use std::time::Instant;

use phimakor::core::chart::Chart;

fn stats(name: &str, samples: &[f64]) {
    if samples.is_empty() { return; }
    let mut s = samples.to_vec();
    s.sort_by(f64::total_cmp);
    let avg = s.iter().sum::<f64>() / s.len() as f64;
    let p50 = s[s.len() / 2];
    let p95 = s[((s.len() as f64 * 0.95) as usize).min(s.len() - 1)];
    let max = *s.last().unwrap();
    println!("{name:22} avg={avg:8.3}ms  p50={p50:8.3}ms  p95={p95:8.3}ms  max={max:8.3}ms  n={}", s.len());
}

fn main() {
    if std::env::var("PHIMAKOR_GPU_TIMING").is_err() {
        eprintln!("warning: PHIMAKOR_GPU_TIMING not set — GPU timestamps disabled, results will be 0");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args.first().cloned().unwrap_or_else(|| "example_chart".into());
    let frames: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(600);
    let width: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1280);
    let height: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(720);

    let (_info, mut chart) = Chart::load(std::path::Path::new(&dir)).unwrap();
    let off = (chart.offset() + _info.offset) as f64;
    let dur = chart.duration().max(1.0);

    let mut eng = pollster::block_on(phimakor::render::preview::PreviewEngine::new(width, height)).unwrap();

    // Warmup:让管线/缓存就绪。
    for _ in 0..30 {
        eng.render_frame(&chart.state_at(off), width as f32 / height as f32, 1.0);
        eng.gpu_frame_ms();
    }

    let step = dur / frames as f64;
    let mut t = off;
    let mut samples = Vec::with_capacity(frames);
    for _ in 0..frames {
        let frame = chart.state_at(t);
        eng.render_frame(frame, width as f32 / height as f32, 1.0);
        let ms = eng.gpu_frame_ms() as f64;
        samples.push(ms);
        t += step;
        if t > dur + off { t = off; }
    }
    println!("resolution: {width}x{height}");
    stats("gpu_frame", &samples);
    let _ = Instant::now();
}
