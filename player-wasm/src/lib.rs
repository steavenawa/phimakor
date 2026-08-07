//! PhiMakor wasm player — browser-side WebGPU renderer.
//!
//! 架构:桌面 editor 是"状态源"(每帧推渲染快照 + 提供谱面纹理/音乐),
//! 本 crate 是"渲染端":wgpu(WebGPU)按快照画线/音符/fx,WebAudio 播音乐。
//! 浏览器端不跑动画求值(动画在电脑端 state_at),只做几何与纹理。
//!
//! 第一版(冒烟):core 子集编译 + WebGPU 设备初始化 + 最小谱面 state_at
//! 验证。渲染复刻与网络快照协议随后接入。

// core 子集:剪掉 edit/stream/pmk(edit 含 Instant/thread/fs,wasm 运行
// panic;播放器只需要解析 + 求值)。
mod core;

// chart.rs 里 `crate::trace_span!("...")` 只作生命周期 guard,wasm 下
// no-op 即可(与 measure.rs `use phimakor::trace_span` 同语义)。
#[macro_export]
macro_rules! trace_span {
    ($($t:tt)*) => { () };
}

use wasm_bindgen::prelude::*;

/// wasm 入口:启动 WebGPU + core 冒烟。
#[wasm_bindgen(start)]
pub fn start() {
    wasm_bindgen_futures::spawn_local(async move {
        match run().await {
            Ok(()) => log("player ok"),
            Err(e) => log(&format!("player init failed: {e:#}")),
        }
    });
}

fn log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}

async fn run() -> anyhow::Result<()> {
    // ── core 冒烟:内嵌最小谱面,from_rpe + state_at(浏览器端能算帧)──
    let src = r#"{
        "META": { "offset": 0, "RPEVersion": 160 },
        "BPMList": [ { "bpm": 120.0, "startTime": [0, 0, 1] } ],
        "judgeLineList": [
            {
                "Name": "line0", "Texture": "line.png", "father": -1,
                "eventLayers": [], "notes": [
                    { "type": 1, "above": 1, "startTime": [1, 0, 1], "endTime": [1, 0, 1], "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "size": 1.0, "speed": 1.0, "isFake": 0, "visibleTime": 999999.0 }
                ], "isCover": 1
            }
        ]
    }"#;
    let rpe: core::model::RPEChart = serde_json::from_str(src)?;
    let mut chart = core::chart::Chart::from_rpe_chart(&rpe, false)?;
    let frame = chart.state_at(0.5);
    let notes = frame.lines.iter().map(|l| l.notes.len()).sum::<usize>();
    log(&format!("core wasm ok: {} lines, {} visible notes, {:.2}s", frame.lines.len(), notes, frame.time));

    // ── WebGPU 冒烟:adapter + device(WebGPU 后端)──
    // wgpu 30:InstanceDescriptor 无 Default,wasm 无 display handle。
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: true,
        })
        .await
        .map_err(|e| anyhow::anyhow!("WebGPU adapter not found (Chrome/Edge 113+): {e}"))?;
    let info = adapter.get_info();
    log(&format!(
        "webgpu ok: {:?} / {:?} (backend {:?})",
        info.name, info.device_type, info.backend
    ));
    let (device, _queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("phimakor-player"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            ..Default::default()
        })
        .await?;
    log("wgpu device ok");

    // 后续:canvas surface + 渲染管线复刻 + 快照 WebSocket。
    let _ = device;
    Ok(())
}
