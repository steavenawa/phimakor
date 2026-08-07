//! 万级谱编辑性能基线(PMCORE-27):打开 / add_note×10000 / undo×10000 / save
//! 的实测数值,输出总耗时与单次操作 p95,并与建议阈值对比(超阈值标 ⚠)。
//! CPU-only,无 GPU 依赖;基准谱自动生成到 `target/bench_chart_10k`。
//!
//! 运行:`cargo run --release --example bench_edit`
//!
//! 基线(2026-08-07,i9-13900H@win11 / Windows x86_64 / release opt3 lto=fat):
//!   open(ChartDocument::open + Chart::from_rpe_chart): 25.6 ms mean / 28.2 ms p95
//!      (二次运行实测范围 25.6–42.9 ms,首次为冷页缓存;阈值 <500ms)
//!   add_note×10000 : 6.6 ms total / 0.5 µs p95(<1ms)
//!   undo×10000     : 4.7 ms total / 0.4 µs p95(<1ms)
//!   save 序列化    : 11.1 ms mean / 11.9 ms p95(pretty json 6.3 MB)
//!   save() 全量    : 10.5 ms mean / 12.5 ms p95(tmp/rename 写盘)
//! 全部低于建议阈值(10-50× 余量)。本 harness 为短脉冲,受 CPU 热节流影响
//! 波动 ±30%;稳态回归对比请用 benches/extract.rs(criterion,100 samples)。

use std::time::Instant;

use phimakor::core::bpm::Triple;
use phimakor::core::chart::Chart;
use phimakor::core::edit::ChartDocument;
use phimakor::core::model::RPENote;

#[path = "../bench_common/gen_chart.rs"]
mod gen_chart;

/// 编辑操作数(与谱面音符数一致,spec: add_note×10000 + undo×10000)。
const N: usize = 10_000;

fn p95(sorted: &[f64]) -> f64 {
    sorted[(sorted.len() as f64 * 0.95) as usize]
}

fn machine_info() -> String {
    let cpu = std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".into());
    format!(
        "{} {} | CPU: {} | release(opt3, lto=fat)",
        std::env::consts::OS,
        std::env::consts::ARCH,
        cpu
    )
}

fn mark(ok: bool) -> &'static str {
    if ok { "PASS" } else { "⚠ FAIL" }
}

fn main() {
    let dir = std::path::PathBuf::from(gen_chart::BENCH_CHART_DIR);
    let chart = gen_chart::ensure_bench_chart_dir(&dir).expect("generate bench chart");
    let (lines, notes) = gen_chart::chart_stats(&chart);
    println!("== 机器: {}", machine_info());
    println!("== 基准谱: {lines} 线 / {notes} 音符(BENCH_NOTES={})", gen_chart::BENCH_NOTES);
    println!("== 建议阈值: 打开 <500ms | 单次编辑 p95 <1ms | 渲染 p95 <16.7ms(perf_charts)");

    // ---- 1. 打开基线:ChartDocument::open + Chart::from_rpe_chart ----
    let mut open_times = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let doc = ChartDocument::open(&dir).expect("open");
        let c = Chart::from_rpe_chart(doc.chart(), false).expect("from_rpe_chart");
        drop((doc, c)); // Drop 会 join saver 线程
        open_times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    open_times.sort_by(f64::total_cmp);
    let open_mean = open_times.iter().sum::<f64>() / open_times.len() as f64;
    let open_p95 = p95(&open_times);
    println!(
        "[open] {}  open+from_rpe_chart: mean={open_mean:.1}ms p95={open_p95:.1}ms (阈值<500ms)",
        mark(open_p95 < 500.0)
    );

    // ---- 2/3. 编辑基线:add_note×10000 与 undo×10000(同一 doc,undo 恢复原状) ----
    let mut doc = ChartDocument::open(&dir).expect("open");
    let mut add_ops = Vec::with_capacity(N);
    let t0 = Instant::now();
    for i in 0..N {
        let t = Instant::now();
        let note = RPENote {
            kind: 1,
            above: 1,
            start_time: Triple::from_beats(5000.0 + i as f64 * 0.5),
            ..Default::default()
        };
        doc.add_note(0, note).expect("add_note");
        add_ops.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let add_total = t0.elapsed().as_secs_f64() * 1000.0;
    add_ops.sort_by(f64::total_cmp);
    let add_p95 = p95(&add_ops);
    println!(
        "[edit] {}  add_note×{N}: total={add_total:.1}ms 单次 p95={:.1}µs (阈值<1ms)",
        mark(add_p95 < 1.0),
        add_p95 * 1000.0
    );

    let mut undo_ops = Vec::with_capacity(N);
    let t0 = Instant::now();
    for _ in 0..N {
        let t = Instant::now();
        assert!(doc.undo(), "undo 栈提前清空");
        undo_ops.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let undo_total = t0.elapsed().as_secs_f64() * 1000.0;
    undo_ops.sort_by(f64::total_cmp);
    let undo_p95 = p95(&undo_ops);
    println!(
        "[edit] {}  undo×{N}: total={undo_total:.1}ms 单次 p95={:.1}µs (阈值<1ms)",
        mark(undo_p95 < 1.0),
        undo_p95 * 1000.0
    );

    // ---- 4. save 基线:纯序列化 + 全量 save() ----
    let mut ser_times = Vec::new();
    let mut size = 0;
    for _ in 0..5 {
        let t = Instant::now();
        let bytes = serde_json::to_vec_pretty(doc.chart()).expect("serialize");
        size = bytes.len();
        ser_times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    ser_times.sort_by(f64::total_cmp);
    let ser_mean = ser_times.iter().sum::<f64>() / ser_times.len() as f64;
    println!(
        "[save]  serialize_pretty: mean={ser_mean:.1}ms p95={:.1}ms (json {:.1} MB)",
        p95(&ser_times),
        size as f64 / 1e6
    );
    let mut save_times = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        doc.save().expect("save");
        save_times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    save_times.sort_by(f64::total_cmp);
    println!(
        "[save]  save() 全量(tmp+rename): mean={:.1}ms p95={:.1}ms",
        save_times.iter().sum::<f64>() / save_times.len() as f64,
        p95(&save_times)
    );
}
