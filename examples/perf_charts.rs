//! 谱面渲染性能对比基准:加载两个谱面各渲染 3600 帧(模拟 60s 播放),
//! 输出每帧 CPU 耗时统计(mean/p95/max)+ 每 5s 分段均值(定位密集段)。
//! 用法: cargo run --release --example perf_charts

use std::time::Instant;

fn bench(dir: &str, frames: usize) -> anyhow::Result<()> {
    let mut s = phimakor::engine::ChartSession::new(1280, 720)?;
    s.load(std::path::Path::new(dir))?;
    // 谱面规模
    let (lines, notes) = {
        let (_info, chart) = phimakor::core::chart::Chart::load(std::path::Path::new(dir))?;
        (chart.line_count(), chart.max_combo())
    };
    // 预热 10 帧。
    for i in 0..10 {
        let _ = s.render_frame(1.0 + i as f64 * 0.1, 1.0);
    }
    let mut times = Vec::with_capacity(frames);
    let mut gpu_times: Vec<f32> = Vec::with_capacity(frames);
    let total = 60.0;
    for i in 0..frames {
        let t = (i as f64 / frames as f64) * total;
        let start = Instant::now();
        let _ = s.render_frame(t, 1.0);
        times.push(start.elapsed().as_secs_f64() * 1000.0);
        gpu_times.push(s.gpu_frame_ms());
    }
    times.sort_by(f64::total_cmp);
    gpu_times.sort_by(f32::total_cmp);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    let p50 = times[times.len() / 2];
    let p95 = times[(times.len() as f64 * 0.95) as usize];
    let max = *times.last().unwrap();
    let gpu_max = gpu_times.last().copied().unwrap_or(0.0);
    let gpu_p95 = gpu_times[(gpu_times.len() as f64 * 0.95) as usize];
    println!("== {dir} | lines(peak)={lines} notes(peak)={notes}");
    println!(
        "   total: mean={mean:.2}ms p50={p50:.2}ms p95={p95:.2}ms max={max:.2}ms | gpu p95={gpu_p95:.2}ms max={gpu_max:.2}ms"
    );
    // 每 5s 分段均值(60 帧一段)
    let seg = frames / 12;
    let mut out = String::from("   5s 段:");
    for k in 0..12 {
        let seg_times = &times[k * seg..(k + 1) * seg];
        let m = seg_times.iter().sum::<f64>() / seg as f64;
        out.push_str(&format!(" {m:.1}"));
    }
    println!("{out}");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let base = std::path::Path::new(r"D:\DOCU\PhiMakor\charts");
    let mut names: Vec<String> = Vec::new();
    for e in std::fs::read_dir(base)? {
        let e = e?;
        if e.path().is_dir() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.contains("Infinity") || n.contains("SeeWTF") {
                names.push(n);
            }
        }
    }
    names.sort();
    for n in &names {
        bench(&base.join(n).to_string_lossy(), 3600)?;
    }
    if names.is_empty() {
        eprintln!("no charts found");
    }
    Ok(())
}
