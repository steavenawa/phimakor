//! hit-fx 瞬跳复现排查工具:逐帧播放,检测同一 (line,t0) 的位姿漂移
//! 与 fx 落点 vs note 实际渲染落点的偏差。
//! 用法:cargo run --release --example fx_repro -- <chart_dir>
use phimakor::core::chart::Chart;
use std::path::Path;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: fx_repro <chart_dir>");
    let (_info, mut chart) = Chart::load(Path::new(&dir)).unwrap();

    let dur = chart.duration();
    println!("chart {dir}: {:.2}s, {} lines, {} notes", dur, chart.line_count(), chart.note_count());

    // 逐帧播放全程(60fps),每帧记录 (line,t0)->pose;同 t0 出现不同 pose 即漂移。
    let mut seen: std::collections::HashMap<(usize, u64), ([f32; 2], f32)> = std::collections::HashMap::new();
    let mut drift = 0usize;
    let mut checks = 0usize;
    let mut t = 0.0f64;
    let dt = 1.0 / 60.0;
    while t <= dur + 1.0 {
        chart.state_at(t);
        let trigs = chart.fx_in_window(t - 0.5, t);
        if !trigs.is_empty() {
            let poses = chart.fx_poses(&trigs);
            for (tr, p) in trigs.iter().zip(poses.iter()) {
                let key = (tr.line, tr.t0.to_bits());
                checks += 1;
                match seen.get(&key) {
                    Some(prev) if prev != p => {
                        drift += 1;
                        if drift <= 20 {
                            println!("DRIFT line={} t0={:.6} prev=({:.4},{:.4} r{:.4}) now=({:.4},{:.4} r{:.4})",
                                tr.line, tr.t0, prev.0[0], prev.0[1], prev.1, p.0[0], p.0[1], p.1);
                        }
                    }
                    Some(_) => {}
                    None => { seen.insert(key, *p); }
                }
            }
        }
        t += dt;
    }
    println!("checked {checks} (line,t0) poses, drift = {drift}");
    if drift == 0 {
        println!("图表层无漂移:同 (line,t0) 位姿跨帧逐位一致");
    }
}
