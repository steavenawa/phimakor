//! 基准谱生成器(PMCORE-27 共享)。
//!
//! 被 `benches/extract.rs`、`examples/bench_edit.rs`、`examples/perf_charts.rs`
//! 通过 `#[path]` 模块包含,保证三处基准使用同一份程序化生成的万级音符谱。
//! 只依赖 std + `phimakor` 库 crate(无 criterion / GPU),CPU-only 与 GPU
//! 基准均可引用。
//!
//! 生成的谱:8 条判定线 × 1250 音符 = 10000 音符,含 tap/hold/flick/fake 与
//! 每线 alpha/moveX/rotate/speed 四层事件(`BENCH_EVENTS_PER_LAYER` 个)。
//! 音符按线轮转,start_time = i × 0.5 拍,谱长 5000 拍。每条线音符已按
//! start_time 升序(`from_rpe`/`add_note` 的前置条件)。

use phimakor::core::bpm::Triple;
use phimakor::core::model::{
    RPEBpmItem, RPEChart, RPEEvent, RPEEventLayer, RPEJudgeLine, RPEMetadata, RPENote,
};

/// 判定线数(万级谱的"多线"维度)。
pub const BENCH_LINES: usize = 8;
/// 基准谱音符总数(≥10000)。
pub const BENCH_NOTES: usize = 10_000;
/// 每线每事件层的事件数。
pub const BENCH_EVENTS_PER_LAYER: usize = 64;
/// 生成图目录名(相对路径,位于 gitignore 的 `target/` 下,缺省自动生成)。
pub const BENCH_CHART_DIR: &str = "target/bench_chart_10k";

/// 程序化生成 `n_notes` 个音符的 RPE 谱(默认 [`BENCH_NOTES`]):
/// - [`BENCH_LINES`] 条判定线,音符轮转分配(每线 n/BENCH_LINES 个)
/// - 每 4 个音符一个 hold(kind=2,end=start+0.5)、每 8 个一个 flick(kind=3),其余 tap(kind=1)
/// - 偶数线每 5 个音符一个 fake(is_fake=1)
/// - 每线 4 层事件层(alpha/moveX/rotate/speed)× [`BENCH_EVENTS_PER_LAYER`] 事件
pub fn gen_large_chart(n_notes: usize) -> RPEChart {
    let mut lines = Vec::with_capacity(BENCH_LINES);
    for li in 0..BENCH_LINES {
        let mut notes = Vec::with_capacity(n_notes / BENCH_LINES + 8);
        for i in (li..n_notes).step_by(BENCH_LINES) {
            let kind = match i % 8 {
                3 => 2, // hold
                7 => 3, // flick
                _ => 1, // tap
            };
            let start = i as f64 * 0.5;
            let mut note = RPENote {
                kind,
                above: 1,
                start_time: Triple::from_beats(start),
                end_time: if kind == 2 {
                    Triple::from_beats(start + 0.5)
                } else {
                    Triple::default()
                },
                position_x: (i % 9) as f32 / 8.0 * 2.0 - 1.0,
                y_offset: 0.0,
                alpha: 255,
                ..Default::default()
            };
            if li % 2 == 0 && i % 5 == 0 {
                note.is_fake = 1;
            }
            notes.push(note);
        }
        lines.push(RPEJudgeLine {
            name: format!("Line{li}"),
            texture: "hold.png".into(),
            parent: None,
            rotate_with_father: None,
            event_layers: gen_event_layers(),
            extended: None,
            notes: Some(notes),
            is_cover: 0,
            z_order: li as i32,
            attach_ui: None,
            pos_control: Vec::new(),
            size_control: Vec::new(),
            alpha_control: Vec::new(),
            y_control: Vec::new(),
            comment: None,
        });
    }
    RPEChart {
        meta: RPEMetadata { offset: 0, rpe_version: 160 },
        bpm_list: vec![RPEBpmItem { bpm: 120.0, start_time: Triple::default() }],
        judge_line_list: lines,
    }
}

/// 每线 4 层事件层,每层 [`BENCH_EVENTS_PER_LAYER`] 个线性事件。
/// alpha 层取值 0..255 口径(与 `from_rpe` 的 `1./255.` 因子一致)。
fn gen_event_layers() -> Vec<Option<RPEEventLayer>> {
    let ev = |base: f32| {
        (0..BENCH_EVENTS_PER_LAYER)
            .map(|k| RPEEvent {
                easing_left: 0.0,
                easing_right: 1.0,
                bezier: 0,
                bezier_points: [0.0; 4],
                easing_type: 1,
                start: base + (k % 5) as f32 * 0.1,
                end: base + ((k + 3) % 5) as f32 * 0.1,
                start_time: Triple::from_beats(k as f64 * 8.0),
                end_time: Triple::from_beats(k as f64 * 8.0 + 4.0),
            })
            .collect()
    };
    vec![
        Some(RPEEventLayer {
            alpha_events: Some(ev(128.0)),
            move_x_events: None,
            move_y_events: None,
            rotate_events: None,
            speed_events: None,
        }),
        Some(RPEEventLayer {
            alpha_events: None,
            move_x_events: Some(ev(0.0)),
            move_y_events: None,
            rotate_events: None,
            speed_events: None,
        }),
        Some(RPEEventLayer {
            alpha_events: None,
            move_x_events: None,
            move_y_events: None,
            rotate_events: Some(ev(0.0)),
            speed_events: None,
        }),
        Some(RPEEventLayer {
            alpha_events: None,
            move_x_events: None,
            move_y_events: None,
            rotate_events: None,
            speed_events: Some(ev(0.0)),
        }),
    ]
}

/// 基准谱的判定线数与音符总数。
pub fn chart_stats(chart: &RPEChart) -> (usize, usize) {
    let notes = chart
        .judge_line_list
        .iter()
        .map(|l| l.notes.as_ref().map_or(0, Vec::len))
        .sum();
    (chart.judge_line_list.len(), notes)
}

/// 确保基准谱目录存在:缺省则程序化生成并写入 info.json + chart.json
/// (PMCORE-27:benches/extract.rs 缺目录时不再静默跳过,改为自动生成)。
/// 幂等:目录已存在直接读回。目录位于 gitignore 的 `target/` 下。
pub fn ensure_bench_chart_dir(dir: &std::path::Path) -> std::io::Result<RPEChart> {
    if !dir.join("chart.json").is_file() {
        let chart = gen_large_chart(BENCH_NOTES);
        write_chart_dir(dir, &chart)?;
        Ok(chart)
    } else {
        let bytes = std::fs::read(dir.join("chart.json"))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// 写 info.json + chart.json 到 `dir`。
pub fn write_chart_dir(dir: &std::path::Path, chart: &RPEChart) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let (lines, notes) = chart_stats(chart);
    let info = format!(
        r#"{{"name":"PMCORE-27 bench {lines}L/{notes}N","chart":"chart.json","level":"IN Lv.15","difficulty":15.0,"charter":"bench","composer":"bench"}}"#
    );
    std::fs::write(dir.join("info.json"), info)?;
    let json = serde_json::to_vec_pretty(chart)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(dir.join("chart.json"), json)?;
    Ok(())
}
