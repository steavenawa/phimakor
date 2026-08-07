//! PhiMakor wasm player — browser-side WebGPU renderer.
//!
//! 架构:桌面 editor 是"状态源"(每帧推渲染快照 + 提供谱面纹理/音乐),
//! 本 crate 是"渲染端":wgpu(WebGPU)按快照画线/音符/hold,WebAudio 播音乐
//! (后续)。浏览器端不跑动画求值(动画在电脑端 state_at),只做几何与纹理。
//!
//! 第一版:core 子集 + WebGPU 初始化冒烟 + 快照渲染复刻(内嵌测试快照,
//! WS 接入下一步)。wasm 入口用 #[cfg(target_arch = "wasm32")] 门控,
//! host 下 `cargo test` 只测纯解析(core/snap),不碰浏览器 API。

// core 子集:剪掉 edit/stream/pmk(edit 含 Instant/thread/fs,wasm 运行
// panic;播放器只需要解析 + 求值)。
mod core;

// chart.rs 里 `crate::trace_span!("...")` 只作生命周期 guard,wasm 下
// no-op 即可(与 measure.rs `use phimakor::trace_span` 同语义)。
#[macro_export]
macro_rules! trace_span {
    ($($t:tt)*) => { () };
}

mod render;
pub mod snap;

/// 浏览器端接入(仅 wasm target 编译):canvas surface + rAF 循环 +
/// WS 帧接口(handle_frame/handle_texture/start_stream)。
/// host 下 `cargo test` 编译不到这里。
#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    /// 播放器全局单例(wasm 单线程,thread_local 安全)。
    /// start() 初始化后,JS 侧 handle_frame/handle_texture 喂数据。
    thread_local! {
        static PLAYER: RefCell<Option<Rc<RefCell<crate::render::Renderer>>>> = const { RefCell::new(None) };
        /// 最新快照(WS 帧解析结果;rAF 每帧消费)。
        static LATEST: RefCell<Option<crate::snap::Snapshot>> = const { RefCell::new(None) };
        /// 待上传纹理(槽位, 名字, PNG 字节;rAF 帧内上传)。
        static TEX_QUEUE: RefCell<Vec<(u8, String, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
        /// 流模式:true 后不再画内嵌测试快照,只画 WS 帧。
        static STREAM_MODE: Cell<bool> = const { Cell::new(false) };
    }

    /// wasm 入口:启动 WebGPU + core 冒烟 + rAF 渲染循环。
    #[wasm_bindgen(start)]
    pub fn start() {
        wasm_bindgen_futures::spawn_local(async move {
            match run().await {
                Ok(()) => log("player ok"),
                Err(e) => log(&format!("player init failed: {e:#}")),
            }
        });
    }

    /// JS → wasm:一帧快照字节(PROTOCOL.md 0x01),解析后入队。
    #[wasm_bindgen]
    pub fn handle_frame(data: &[u8]) {
        match crate::snap::parse_snapshot(data) {
            Some(s) => LATEST.with(|l| *l.borrow_mut() = Some(s)),
            None => log(&format!("snapshot parse failed ({} bytes)", data.len())),
        }
    }

    /// JS → wasm:一帧纹理字节(PROTOCOL.md 0x00 / 0xFF),解析后入队。
    #[wasm_bindgen]
    pub fn handle_texture(data: &[u8]) {
        match crate::snap::parse_texture_frame(data) {
            Some((slot, name, png)) => TEX_QUEUE.with(|q| q.borrow_mut().push((slot, name, png))),
            None => log("texture frame parse failed"),
        }
    }

    /// JS → wasm:进入流模式(不再画内嵌测试快照)。
    #[wasm_bindgen]
    pub fn start_stream() {
        STREAM_MODE.with(|m| m.set(true));
        log("stream mode on");
    }

    fn log(msg: &str) {
        web_sys::console::log_1(&JsValue::from_str(msg));
    }

    /// 取渲染目标 canvas:#screen 优先,否则页面上第一个 <canvas>。
    fn get_canvas() -> anyhow::Result<web_sys::HtmlCanvasElement> {
        let doc = web_sys::window()
            .and_then(|w| w.document())
            .ok_or_else(|| anyhow::anyhow!("no document"))?;
        doc.get_element_by_id("screen")
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .or_else(|| {
                doc.query_selector("canvas")
                    .ok()
                    .flatten()
                    .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            })
            .ok_or_else(|| anyhow::anyhow!("canvas not found (#screen or <canvas>)"))
    }

    /// canvas 物理像素尺寸 = CSS 尺寸 × devicePixelRatio。
    fn canvas_size(canvas: &web_sys::HtmlCanvasElement) -> anyhow::Result<(u32, u32)> {
        let dpr = web_sys::window().map(|w| w.device_pixel_ratio()).unwrap_or(1.0);
        let w = ((canvas.client_width() as f64) * dpr).ceil().max(1.0) as u32;
        let h = ((canvas.client_height() as f64) * dpr).ceil().max(1.0) as u32;
        Ok((w, h))
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
        let rpe: crate::core::model::RPEChart = serde_json::from_str(src)?;
        let mut chart = crate::core::chart::Chart::from_rpe_chart(&rpe, false)?;
        let frame = chart.state_at(0.5);
        let notes = frame.lines.iter().map(|l| l.notes.len()).sum::<usize>();
        log(&format!("core wasm ok: {} lines, {} visible notes, {:.2}s", frame.lines.len(), notes, frame.time));

        // ── canvas + surface + adapter/device ──
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let canvas = get_canvas()?;
        let (w, h) = canvas_size(&canvas)?;
        canvas.set_width(w);
        canvas.set_height(h);
        let surface = instance.create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
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
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("phimakor-player"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
                ..Default::default()
            })
            .await?;
        log("wgpu device ok");

        // ── 表面配置:优先非 sRGB 格式(作者色为显示就绪 sRGB,直通)──
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: w,
            height: h,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
        };
        let renderer = crate::render::Renderer::new(device, queue, surface, config);

        // ── 内嵌测试快照:2 线 3 音符(含 hold),WS 接入前先画它 ──
        let snap = test_snapshot();
        log(&format!(
            "test snapshot: {} lines, {} notes",
            snap.lines.len(),
            snap.lines.iter().map(|l| l.notes.len()).sum::<usize>()
        ));

        // ── rAF 循环:消费 WS 帧(或测试快照),每秒打一次 fps ──
        // 自引用闭包:闭包持有 cb2(Rc),帧内用它重新调度下一帧;
        // cb(外层句柄)持有 Function,Function 持有 JS 闭包 → 循环自持,
        // 随页面存活(wasm-bindgen 0.2.126 统一为 ScopedClosure)。
        let window = web_sys::window().ok_or_else(|| anyhow::anyhow!("no window"))?;
        let renderer = Rc::new(RefCell::new(renderer));
        PLAYER.with(|p| *p.borrow_mut() = Some(renderer.clone()));
        let cb: Rc<RefCell<Option<js_sys::Function>>> = Default::default();
        let cb2 = cb.clone();
        let mut frames: u32 = 0;
        let mut t0 = js_sys::Date::now();
        let closure = wasm_bindgen::closure::ScopedClosure::<'static, dyn FnMut()>::wrap_assert_unwind_safe(
            Box::new(move || {
                // 纹理队列先上传(握手帧在快照前到达)。
                TEX_QUEUE.with(|q| {
                    let mut q = q.borrow_mut();
                    for (slot, name, png) in q.drain(..) {
                        if let Err(e) = renderer.borrow_mut().upload_png(slot, &name, &png) {
                            log(&format!("tex {name}: {e}"));
                        }
                    }
                });
                // 画最新快照:WS 帧优先,流模式未开时回退内嵌测试快照。
                let stream = STREAM_MODE.with(|m| m.get());
                let snap = if !stream {
                    Some(snap.clone())
                } else {
                    LATEST.with(|l| l.borrow_mut().take())
                };
                if let Some(s) = snap {
                    if let Err(e) = renderer.borrow_mut().draw_snapshot(&s) {
                        log(&format!("draw: {e:#}"));
                    }
                }
                frames += 1;
                let now = js_sys::Date::now();
                if now - t0 >= 1000.0 {
                    log(&format!("fps: {:.1}", frames as f64 * 1000.0 / (now - t0)));
                    frames = 0;
                    t0 = now;
                }
                if let Some(cb) = cb2.borrow().as_ref() {
                    if let Some(w) = web_sys::window() {
                        let _ = w.request_animation_frame(cb);
                    }
                }
            }),
        );
        let f_fn: js_sys::Function = closure.into_js_value().into();
        *cb.borrow_mut() = Some(f_fn.clone());
        window
            .request_animation_frame(&f_fn)
            .map_err(|e| anyhow::anyhow!("requestAnimationFrame: {e:?}"))?;
        Ok(())
    }

    /// 手工构造的内嵌测试快照:2 条线 3 个音符(1 tap + 1 hold + 1 drag)。
    fn test_snapshot() -> crate::snap::Snapshot {
        use crate::snap::{LineSnap, NoteSnap, Snapshot};
        Snapshot {
            chart_time: 1.5,
            dim: 0.5,
            lines: vec![
                LineSnap {
                    pos: [0.0, 0.0],
                    rot: 0.0,
                    scale: [1.0, 1.0],
                    alpha: 1.0,
                    z: 0,
                    tex: 6,
                    notes: vec![
                        NoteSnap { kind: 1, x: -0.25, y: -0.2, end_y: f32::NAN, alpha: 1.0, scale: 1.0, tex: 1 },
                        NoteSnap { kind: 2, x: 0.05, y: -0.35, end_y: 0.15, alpha: 0.9, scale: 1.0, tex: 4 },
                    ],
                },
                LineSnap {
                    pos: [0.0, 0.25],
                    rot: 0.0,
                    scale: [1.0, 1.0],
                    alpha: 1.0,
                    z: 1,
                    tex: 6,
                    notes: vec![NoteSnap { kind: 4, x: 0.3, y: -0.1, end_y: f32::NAN, alpha: 1.0, scale: 1.0, tex: 2 }],
                },
            ],
        }
    }
}
