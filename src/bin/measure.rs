//! Headless perf measurement for the performance-experimental branch.
//!
//! Usage: cargo run --release --bin measure -- <chart_dir> [frames]
//!
//! Measures:
//!   1. Chart::load + from_rpe_chart
//!   2. state_at at advancing times (the per-frame CPU eval)
//!   3. Offscreen render_frame via PreviewEngine (CPU submit time)
//!   4. With --effects <chart_dir/extra.json>: also times the post pipe

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
    println!("{name:28} avg={avg:8.3}ms  p50={p50:8.3}ms  p95={p95:8.3}ms  max={max:8.3}ms  n={}", s.len());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args.first().cloned().unwrap_or_else(|| "example_chart".into());
    let frames: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(600);

    // 1. Load
    let t0 = Instant::now();
    let (info, mut chart) = Chart::load(std::path::Path::new(&dir)).unwrap();
    println!("load: {:.3}ms  ({} lines, {} notes, {:.1}s duration)",
        t0.elapsed().as_secs_f64() * 1000.0,
        chart.line_count(), chart.note_count(), chart.duration());

    let off = (chart.offset() + info.offset) as f64;

    // 2. state_at at advancing times
    let dur = chart.duration().max(1.0);
    let step = dur / frames as f64;
    let mut state_times = Vec::with_capacity(frames);
    let mut t = off;
    for _ in 0..frames {
        let s = Instant::now();
        chart.state_at(t);
        state_times.push(s.elapsed().as_secs_f64() * 1000.0);
        t += step;
        if t > dur + off { t = off; }
    }
    stats("state_at", &state_times);

    // 2b. Per-section breakdown using tracing spans: dump per-frame spans
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let _ = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_target(false).with_level(true)
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE))
            .with(tracing_subscriber::EnvFilter::new("phimakor=trace"))
            .try_init();
    }

    // 3. Offscreen render (CPU submit + readback-agnostic)
    let rt = pollster::block_on(phimakor::render::preview::PreviewEngine::new(1280, 720)).unwrap();
    let mut eng = rt;
    let mut render_times = Vec::with_capacity(frames);
    t = off;
    for _ in 0..frames {
        let frame = chart.state_at(t);
        let s = Instant::now();
        eng.render_frame(frame, 16.0 / 9.0, 1.0);
        render_times.push(s.elapsed().as_secs_f64() * 1000.0);
        t += step;
        if t > dur + off { t = off; }
    }
    stats("render_frame(cpu)", &render_times);

    // 4. Iced overlay + timeline (headless, the windowed-app hot path)
    {
        #[path = "../ui/mod.rs"]
        mod ui;
        use ui::{GameInfo, IcedOverlay, NoteEntry, EventEntry};
use std::sync::Arc;
        let r = eng.renderer();
        let (w, h) = (1280u32, 800u32);
        // Build realistic note/event entries from the chart (like main.rs does).
        let doc = phimakor::core::edit::ChartDocument::open(std::path::Path::new(&dir)).unwrap();
        let rpe = doc.chart().clone();
        let mut notes: Vec<NoteEntry> = Vec::new();
        let mut events: Vec<EventEntry> = Vec::new();
        for jl in rpe.judge_line_list.iter() {
            if let Some(ns) = &jl.notes {
                for (i, n) in ns.iter().enumerate() {
                    notes.push(NoteEntry {
                        index: i, kind: n.kind,
                        start_beats: n.start_time.beats(), end_beats: n.end_time.beats(),
                        x: n.position_x, speed: n.speed, scale: n.size,
                        texture: n.hitsound.clone().unwrap_or_default(),
                    });
                }
            }
            for (li, layer) in jl.event_layers.iter().flatten().enumerate() {
                for (kind, list) in [
                    ("Alpha", &layer.alpha_events), ("MoveX", &layer.move_x_events),
                    ("MoveY", &layer.move_y_events), ("Rotate", &layer.rotate_events),
                    ("Speed", &layer.speed_events),
                ] {
                    if let Some(evs) = list {
                        for (i, e) in evs.iter().enumerate() {
                            events.push(EventEntry {
                                layer: li, kind: kind.to_string(), index: i,
                                start_beats: e.start_time.beats(), end_beats: e.end_time.beats(),
                                start: e.start, end: e.end, easing: e.easing_type,
                            });
                        }
                    }
                }
            }
        }
        println!("real data: {} notes, {} events", notes.len(), events.len());
        if std::env::var("PHIMAKOR_NOTES_ONLY").is_ok() { events.clear(); }
        if std::env::var("PHIMAKOR_EVENTS_ONLY").is_ok() { notes.clear(); }
        // The real app only shows the SELECTED line's events in the timeline.
        if std::env::var("PHIMAKOR_ALL_LINES").is_err() {
            let sel = 0usize;
            let mut sel_events = Vec::new();
            let mut sel_notes = Vec::new();
            if let Some(jl) = rpe.judge_line_list.get(sel) {
                if let Some(ns) = &jl.notes {
                    for (i, n) in ns.iter().enumerate() {
                        sel_notes.push(NoteEntry {
                            index: i, kind: n.kind,
                            start_beats: n.start_time.beats(), end_beats: n.end_time.beats(),
                            x: n.position_x, speed: n.speed, scale: n.size,
                            texture: n.hitsound.clone().unwrap_or_default(),
                        });
                    }
                }
                for (li, layer) in jl.event_layers.iter().flatten().enumerate() {
                    for (kind, list) in [
                        ("Alpha", &layer.alpha_events), ("MoveX", &layer.move_x_events),
                        ("MoveY", &layer.move_y_events), ("Rotate", &layer.rotate_events),
                        ("Speed", &layer.speed_events),
                    ] {
                        if let Some(evs) = list {
                            for (i, e) in evs.iter().enumerate() {
                                sel_events.push(EventEntry {
                                    layer: li, kind: kind.to_string(), index: i,
                                    start_beats: e.start_time.beats(), end_beats: e.end_time.beats(),
                                    start: e.start, end: e.end, easing: e.easing_type,
                                });
                            }
                        }
                    }
                }
            }
            events = sel_events;
            notes = sel_notes;
        }
        println!("timeline data: {} notes, {} events (selected line)", notes.len(), events.len());
        events.sort_by(|a, b| a.end_beats.total_cmp(&b.end_beats));
        notes.sort_by(|a, b| a.end_beats.total_cmp(&b.end_beats));
        let mut overlay = IcedOverlay::new(r.device(), r.tex_bgl(), r.sampler(), w, h);
        let info = GameInfo {
            chart_time: 10.0, chart_beat: 20.0, audio_time: 10.0, fps: 60.0,
            combo: 100, hits: 100, note_count: 656, score: 500_000,
            lines: 90, visible_notes: 300, paused: false, dim: 1.0,
            chart_name: "Test".into(), composer: "T".into(), level: "IN 15".into(), difficulty: 15.0,
            offset: 0.0, duration: 224.0,
            show_overlay: true, show_properties: true, show_events: true,
            show_notes: true, events_progress: 1.0, notes_progress: 1.0,
            has_custom_tex: false, full_notes: false,
            selected_line: 0, line_name: "line0".into(), line_count: 90,
            selected_layer: 0, max_layers: 1, events: Arc::new(events), notes: Arc::new(notes),
            gui_scale: 1.0, snap: 0.25, vsync: true, vertical_split: 1,
            selected_tool: 0, show_menu: false, selected_event_idx: None,
            event_edit_target: 0, ev_kind: String::new(),
            ev_start_beats: 0.0, ev_end_beats: 0.0, ev_start_val: 0.0,
            ev_end_val: 0.0, ev_easing: 0, effect_names: vec![], effects: Arc::new(vec![]), selected_effect: None, eff_edit_field: 0, num_edit: None, eff_kf_var: None, eff_kf_sel: None, eff_kf_rows: vec![],
        };
        overlay.render_iced(r.queue(), &info); // warmup (iced layout + glyph cache)
        let mut overlay_times = Vec::with_capacity(frames);
        for _ in 0..frames {
            let s = Instant::now();
            overlay.render_iced(r.queue(), &info);
            overlay_times.push(s.elapsed().as_secs_f64() * 1000.0);
        }
        stats("render_iced(timeline+ui)", &overlay_times);

        // 4b. Per-frame path in the real app: timeline-only redraw.
        // Simulate playback: chart_beat advances each frame (scrolling).
        let mut tl_times = Vec::with_capacity(frames);
        for fi in 0..frames {
            let info2 = GameInfo {
                chart_beat: 20.0 + fi as f64 * 0.1,
                chart_time: (20.0 + fi as f64 * 0.1) * 0.5,
                chart_name: info.chart_name.clone(),
                composer: info.composer.clone(),
                level: info.level.clone(),
                line_name: info.line_name.clone(),
                ev_kind: info.ev_kind.clone(),
                events: info.events.clone(),
                notes: info.notes.clone(),
                effect_names: info.effect_names.clone(),
                effects: info.effects.clone(),
                num_edit: info.num_edit.clone(), eff_kf_var: info.eff_kf_var, eff_kf_sel: info.eff_kf_sel, eff_kf_rows: info.eff_kf_rows.clone(),
                ..info
            };
            let s = Instant::now();
            overlay.redraw_timeline(r.queue(), &info2);
            tl_times.push(s.elapsed().as_secs_f64() * 1000.0);
        }
        stats("redraw_timeline(playing)", &tl_times);

        // 4c. Paused path (static beat)
        let mut tl_paused = Vec::with_capacity(frames);
        for _ in 0..frames {
            let s = Instant::now();
            overlay.redraw_timeline(r.queue(), &info);
            tl_paused.push(s.elapsed().as_secs_f64() * 1000.0);
        }
        stats("redraw_timeline(paused)", &tl_paused);
    }
}





