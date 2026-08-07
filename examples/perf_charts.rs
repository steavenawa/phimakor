//! 谱面渲染性能基准(PMCORE-27):加载谱面渲染 3600 帧(模拟 60s 播放),
//! 输出每帧 CPU 耗时统计(mean/p50/p95/max)+ GPU p95 + 每 5s 分段均值。
//! 万级谱渲染 p95 建议 <16.7ms(@60fps)——超阈值输出 ⚠ 标记。
//!
//! 用法:
//!   cargo run --release --example perf_charts                  # 默认:自动生成万级基准谱并渲染
//!   cargo run --release --example perf_charts <dir> [<dir>…]   # 渲染指定谱面目录(相对/绝对路径均可)
//!
//! 需 GPU(wgpu);CPU-only 基准见 benches/extract.rs 与 examples/bench_edit.rs。
//! 默认谱面程序化生成到 target/bench_chart_10k(10000 音符,见 bench_common/gen_chart.rs),
//! 无硬编码绝对路径。
//!
//! 基线(2026-08-07,i9-13900H / Windows 11 / release):
//!   target/bench_chart_10k(10000 notes,10250 max_combo):
//!     CPU mean=2.99ms p50=2.80ms p95=4.96ms max=20.03ms | gpu p95=0.00ms(虚拟显示适配器,GPU 计时不可用)
//!   渲染 p95 4.96ms < 16.7ms 阈值,通过。

use std::path::Path;
use std::time::Instant;

#[path = "../bench_common/gen_chart.rs"]
mod gen_chart;

fn bench(dir: &Path, frames: usize) -> anyhow::Result<()> {
    let mut s = phimakor::engine::ChartSession::new(1280, 720)?;
    s.load(dir)?;
    // 谱面规模
    let (lines, notes) = {
        let (_info, chart) = phimakor::core::chart::Chart::load(dir)?;
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
    // PMCORE-27:万级谱渲染阈值建议 <16.7ms(@60fps),超阈值标 ⚠。
    let ok = p95 < 16.7;
    println!("== {} | lines(peak)={lines} notes(peak)={notes}", dir.display());
    println!(
        "   total: mean={mean:.2}ms p50={p50:.2}ms p95={p95:.2}ms max={max:.2}ms | gpu p95={gpu_p95:.2}ms max={gpu_max:.2}ms"
    );
    println!("   [render p95 {} 16.7ms 阈值]", if ok { "PASS <" } else { "⚠ FAIL >=" });
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    // PMCORE-27:无参数时用程序化生成的万级基准谱(保持默认可用,不再依赖
    // 硬编码绝对路径 D:\DOCU\PhiMakor\charts);有参数则逐个渲染指定目录。
    let dirs: Vec<std::path::PathBuf> = if args.is_empty() {
        let dir = std::path::PathBuf::from(gen_chart::BENCH_CHART_DIR);
        gen_chart::ensure_bench_chart_dir(&dir)?;
        vec![dir]
    } else {
        args.iter().map(std::path::PathBuf::from).collect()
    };
    for dir in &dirs {
        bench(dir, 3600)?;
    }
    Ok(())
}
