//! Criterion 基准:万级谱 open / from_rpe_chart / parse / add_note / undo / save。
//! (PMCORE-27 大谱面性能基线,CPU-only,无 GPU 依赖)
//!
//! 谱面:程序化生成的 10000 音符 RPE 谱(8 线,含 hold/flick/fake 与事件层),
//! 见 `bench_common/gen_chart.rs`。**缺 `target/bench_chart_10k` 时自动生成**
//! (PMCORE-27:原实现在缺 example_chart 目录时静默跳过,已改为自动生成)。
//! 目录位于 gitignore 的 `target/` 下,不污染仓库。
//!
//! 运行:`cargo bench --bench extract`
//!
//! 基线(2026-08-07,i9-13900H / Windows 11 / release bench profile opt3 lto=fat,
//! criterion 100 samples,单位 ms/iter):
//!   from_rpe_chart_10k : 47.9 [45.2, 51.0]
//!   parse_rpe_json_10k : 16.5 [15.9, 17.3]
//!   open_10k           : 42.2 [41.0, 43.4](含 spawn saver 线程)
//!   add_note_x10000    : 22.0 [21.0, 23.2](整批 10000 次插入;单次 ~2.2µs)
//!   undo_x10000        : 20.2 [19.5, 21.1](整批 10000 次撤销;单次 ~2.0µs)
//!   serialize_pretty_10k: 32.3 [31.5, 33.2]
//!   save_10k_full      : 29.6 [28.2, 31.0](序列化+tmp/rename 写盘)
//! 阈值(建议值):打开 <500ms;单次编辑 p95 <1ms(见 examples/bench_edit.rs)。
//! 注:本机 CPU 热节流下数值可差 2-3×(bench_edit 短脉冲 7ms vs 本表 22ms),防回归
//! 对比请用同一 harness 的 criterion 存档(100 samples 稳态)。

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use phimakor::core::bpm::Triple;
use phimakor::core::chart::Chart;
use phimakor::core::edit::ChartDocument;
use phimakor::core::model::{RPEChart, RPENote};

#[path = "../bench_common/gen_chart.rs"]
mod gen_chart;

fn bench_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(gen_chart::BENCH_CHART_DIR)
}

/// 10k 谱的 add_note 测试音符:插在现有 5000 拍谱之后,保持升序(append 最优)。
fn edit_note(i: usize) -> RPENote {
    RPENote {
        kind: 1,
        above: 1,
        start_time: Triple::from_beats(5000.0 + i as f64 * 0.5),
        ..Default::default()
    }
}

fn bench_from_rpe_chart(c: &mut Criterion) {
    let chart = gen_chart::ensure_bench_chart_dir(&bench_dir()).expect("generate bench chart");
    let mut g = c.benchmark_group("from_rpe_chart");
    g.bench_function("from_rpe_chart_10k", |b| {
        b.iter(|| std::hint::black_box(Chart::from_rpe_chart(&chart, false).unwrap()));
    });
    g.finish();
}

fn bench_parse_rpe_json(c: &mut Criterion) {
    let dir = bench_dir();
    gen_chart::ensure_bench_chart_dir(&dir).expect("generate bench chart");
    let json = std::fs::read_to_string(dir.join("chart.json")).expect("read chart.json");
    let mut g = c.benchmark_group("parse");
    g.bench_function("parse_rpe_json_10k", |b| {
        b.iter(|| std::hint::black_box(serde_json::from_str::<RPEChart>(&json).unwrap()));
    });
    g.finish();
}

/// 打开基线:`ChartDocument::open`(读 info+chart、parse、spawn saver 线程)。
fn bench_open_large(c: &mut Criterion) {
    let dir = bench_dir();
    gen_chart::ensure_bench_chart_dir(&dir).expect("generate bench chart");
    let mut g = c.benchmark_group("open");
    g.bench_function("open_10k", |b| {
        b.iter(|| std::hint::black_box(ChartDocument::open(&dir).unwrap()));
    });
    g.finish();
}

/// 编辑基线:add_note×10000 与 undo×10000 整批耗时(criterion 报告的是整批
/// 10000 次操作/iter;单次 p95 见 examples/bench_edit.rs)。
fn bench_edit_large(c: &mut Criterion) {
    let dir = bench_dir();
    gen_chart::ensure_bench_chart_dir(&dir).expect("generate bench chart");
    let mut g = c.benchmark_group("edit");
    g.bench_function("add_note_x10000", |b| {
        b.iter_batched(
            || ChartDocument::open(&dir).unwrap(),
            |mut doc| {
                for i in 0..gen_chart::BENCH_NOTES {
                    doc.add_note(0, edit_note(i)).unwrap();
                }
            },
            BatchSize::LargeInput,
        );
    });
    g.bench_function("undo_x10000", |b| {
        b.iter_batched(
            || {
                let mut doc = ChartDocument::open(&dir).unwrap();
                for i in 0..gen_chart::BENCH_NOTES {
                    doc.add_note(0, edit_note(i)).unwrap();
                }
                doc
            },
            |mut doc| {
                for _ in 0..gen_chart::BENCH_NOTES {
                    assert!(doc.undo(), "undo 栈在 10000 次撤销完成前被清空");
                }
            },
            BatchSize::LargeInput,
        );
    });
    g.finish();
}

/// save 基线:纯 JSON 序列化 + 全量 save()(序列化 + tmp/rename 写盘)。
fn bench_save_large(c: &mut Criterion) {
    let dir = bench_dir();
    let chart = gen_chart::ensure_bench_chart_dir(&dir).expect("generate bench chart");
    let mut g = c.benchmark_group("save");
    g.bench_function("serialize_pretty_10k", |b| {
        b.iter(|| std::hint::black_box(serde_json::to_vec_pretty(&chart).unwrap()));
    });
    let mut doc = ChartDocument::open(&dir).unwrap();
    g.bench_function("save_10k_full", |b| {
        b.iter(|| doc.save().unwrap());
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_from_rpe_chart,
    bench_parse_rpe_json,
    bench_open_large,
    bench_edit_large,
    bench_save_large
);
criterion_main!(benches);
