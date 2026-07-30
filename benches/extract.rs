use criterion::{black_box, criterion_group, criterion_main, Criterion};
use phimakor::core::chart::Chart;
use phimakor::core::edit::ChartDocument;
use phimakor::core::model::RPEChart;

fn bench_from_rpe_chart(c: &mut Criterion) {
    // Load a small test chart
    let dir = std::path::PathBuf::from("example_chart");
    if !dir.join("info.json").exists() { return; }
    let doc = ChartDocument::open(&dir).unwrap();
    let chart = doc.chart().clone();
    c.bench_function("from_rpe_chart", |b| {
        b.iter(|| {
            black_box(Chart::from_rpe_chart(&chart, false).unwrap());
        })
    });
}

fn bench_parse_rpe_json(c: &mut Criterion) {
    let dir = std::path::PathBuf::from("example_chart");
    let json = std::fs::read_to_string(dir.join("chart.json")).unwrap();
    c.bench_function("parse_rpe_json", |b| {
        b.iter(|| {
            black_box(serde_json::from_str::<RPEChart>(&json).unwrap());
        })
    });
}

criterion_group!(benches, bench_from_rpe_chart, bench_parse_rpe_json);
criterion_main!(benches);
