//! Editor settings persisted to config.json.

/// Draw splash screen (chart picker) when no chart is loaded.
/// Editor settings surfaced on the splash screen. Persisted to `config.json`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SettingsData {
    pub vsync: bool,
    pub gui_scale: f32,
    pub fullscreen: bool,
    /// GPU backend: `None` = auto (all), or "dx12" / "vulkan" / "gl".
    /// On shared-memory GPUs (iGPU) DX12 is the heaviest by far.
    pub backend: Option<String>,
    /// Custom chart library directory. `None` = the system Documents
    /// folder (`<Documents>/PhiMakor/charts`). Persisted back to
    /// `config.json` so the choice survives restarts.
    pub charts_dir: Option<String>,
    /// 右上角极小性能提示(播放中帧延迟过大时显示)。默认关,设置里开。
    pub perf_hint: bool,
    /// 右上角常驻帧时间叠层(显示 frame ms / fps)。默认开。
    pub fps_overlay: bool,
    /// 自定义 GPU 光标(隐藏系统光标,worker 画动态光标)。默认关。
    pub custom_cursor: bool,
    /// 过激优化位标志(默认 0 = 全关):见 [`AGGRESSIVE_*`] 常量。
    /// 每一项都是有视觉风险、但收益明显的激进优化,设置里逐个开关。
    pub aggressive: u32,
    /// 后处理半分辨率特效降采样(默认开,省 ~75% 像素带宽)。关闭后所有
    /// 特效全分辨率跑,用于排查特效质量问题。
    pub half_res_fx: bool,
    /// 纹理压缩(BC3,默认开):大纹理超 2048² 时 Lanczos3 降采样(抗锯齿)
    /// + BC3 块压缩,显存/带宽 4:1。有损(视觉上通常不可察觉);
    /// 关闭后 RGBA8 原样上传。
    pub texture_compress: bool,
}

/// 过激优化:hold 身体按视口裁剪(线段求交+勾股长度),长 hold 省大量
/// off-screen overdraw;视觉上等价但有回归风险。
pub use phimakor::render::AGGRESSIVE_HOLD_CLIP;

impl Default for SettingsData {
    fn default() -> Self {
        Self { vsync: true, gui_scale: 1.0, fullscreen: false, backend: None, charts_dir: None, perf_hint: false, fps_overlay: true, custom_cursor: false, aggressive: 0, half_res_fx: true, texture_compress: true }
    }
}

/// Display name for a backend setting value.
pub fn backend_label(backend: &Option<String>) -> String {
    match backend.as_deref() {
        Some("dx12") => "DX12".to_string(),
        Some("vulkan") => "Vulkan".to_string(),
        Some("gl") => "GL".to_string(),
        _ => "Auto".to_string(),
    }
}

/// Cycle the backend setting: Auto → DX12 → Vulkan → GL → Auto.
pub fn backend_cycle(backend: &Option<String>) -> Option<String> {
    match backend.as_deref() {
        Some("dx12") => Some("vulkan".to_string()),
        Some("vulkan") => Some("gl".to_string()),
        Some("gl") => None,
        _ => Some("dx12".to_string()),
    }
}

// ── 设置面板(widgets 组件库,挂在 tool 2)──

use super::widgets::{RTControl, RealtimeForm};

/// GUI scale 的滑条映射范围。
const SCALE_MIN: f32 = 0.5;
const SCALE_MAX: f32 = 2.0;

/// 从 SettingsData 构建设置表单。
pub fn build_settings_form(x: f32, y: f32, w: f32, s: f32, settings: &SettingsData) -> RealtimeForm {
    let gui_scale = ((settings.gui_scale - SCALE_MIN) / (SCALE_MAX - SCALE_MIN)).clamp(0.0, 1.0);
    let backend_idx = match settings.backend.as_deref() {
        Some("dx12") => 1,
        Some("vulkan") => 2,
        Some("gl") => 3,
        _ => 0,
    };
    let mut form = RealtimeForm::new(x, y, w, "Settings", vec![
        ("gui scale".into(), RTControl::Slider { value: gui_scale }),
        ("vsync".into(), RTControl::Toggle { on: settings.vsync, anim: if settings.vsync { 1.0 } else { 0.0 }, dir: if settings.vsync { 1.0 } else { -1.0 } }),
        ("fullscreen".into(), RTControl::Toggle { on: settings.fullscreen, anim: if settings.fullscreen { 1.0 } else { 0.0 }, dir: if settings.fullscreen { 1.0 } else { -1.0 } }),
        ("backend".into(), RTControl::Combo { items: vec!["Auto".into(), "DX12".into(), "Vulkan".into(), "GL".into()], selected: backend_idx, open: false }),
        ("perf hint".into(), RTControl::Toggle { on: settings.perf_hint, anim: if settings.perf_hint { 1.0 } else { 0.0 }, dir: if settings.perf_hint { 1.0 } else { -1.0 } }),
        ("fps overlay".into(), RTControl::Toggle { on: settings.fps_overlay, anim: if settings.fps_overlay { 1.0 } else { 0.0 }, dir: if settings.fps_overlay { 1.0 } else { -1.0 } }),
        ("custom cursor".into(), RTControl::Toggle { on: settings.custom_cursor, anim: if settings.custom_cursor { 1.0 } else { 0.0 }, dir: if settings.custom_cursor { 1.0 } else { -1.0 } }),
        ("aggressive cull".into(), RTControl::Toggle { on: settings.aggressive & AGGRESSIVE_HOLD_CLIP != 0, anim: if settings.aggressive & AGGRESSIVE_HOLD_CLIP != 0 { 1.0 } else { 0.0 }, dir: if settings.aggressive & AGGRESSIVE_HOLD_CLIP != 0 { 1.0 } else { -1.0 } }),
        ("half-res fx".into(), RTControl::Toggle { on: settings.half_res_fx, anim: if settings.half_res_fx { 1.0 } else { 0.0 }, dir: if settings.half_res_fx { 1.0 } else { -1.0 } }),
        ("tex compress".into(), RTControl::Toggle { on: settings.texture_compress, anim: if settings.texture_compress { 1.0 } else { 0.0 }, dir: if settings.texture_compress { 1.0 } else { -1.0 } }),
    ]);
    form.row_h = 24.0 * s;
    form.gap = 4.0 * s;
    form
}

/// 把设置表单的当前值应用到 SettingsData(返回是否有变化)。
/// `backend` 变化需要重启渲染器才生效,单独标记。
pub fn apply_settings_form(form: &RealtimeForm, settings: &mut SettingsData) -> bool {
    let mut changed = false;
    for (label, ctrl) in &form.rows {
        match (label.as_str(), ctrl) {
            ("gui scale", RTControl::Slider { value }) => {
                let v = SCALE_MIN + value.clamp(0.0, 1.0) * (SCALE_MAX - SCALE_MIN);
                if (v - settings.gui_scale).abs() > 0.005 {
                    settings.gui_scale = v;
                    changed = true;
                }
            }
            ("vsync", RTControl::Toggle { on, .. }) => {
                if *on != settings.vsync {
                    settings.vsync = *on;
                    changed = true;
                }
            }
            ("fullscreen", RTControl::Toggle { on, .. }) => {
                if *on != settings.fullscreen {
                    settings.fullscreen = *on;
                    changed = true;
                }
            }
            ("backend", RTControl::Combo { selected, .. }) => {
                let v = match selected {
                    1 => Some("dx12".to_string()),
                    2 => Some("vulkan".to_string()),
                    3 => Some("gl".to_string()),
                    _ => None,
                };
                if v != settings.backend {
                    settings.backend = v;
                    changed = true;
                }
            }
            ("perf hint", RTControl::Toggle { on, .. }) => {
                if *on != settings.perf_hint {
                    settings.perf_hint = *on;
                    changed = true;
                }
            }
            ("fps overlay", RTControl::Toggle { on, .. }) => {
                if *on != settings.fps_overlay {
                    settings.fps_overlay = *on;
                    changed = true;
                }
            }
            ("custom cursor", RTControl::Toggle { on, .. }) => {
                if *on != settings.custom_cursor {
                    settings.custom_cursor = *on;
                    changed = true;
                }
            }
            ("aggressive cull", RTControl::Toggle { on, .. }) => {
                let bit = if *on { AGGRESSIVE_HOLD_CLIP } else { 0 };
                if settings.aggressive & AGGRESSIVE_HOLD_CLIP != bit {
                    settings.aggressive = (settings.aggressive & !AGGRESSIVE_HOLD_CLIP) | bit;
                    changed = true;
                }
            }
            ("half-res fx", RTControl::Toggle { on, .. }) => {
                if *on != settings.half_res_fx {
                    settings.half_res_fx = *on;
                    changed = true;
                }
            }
            ("tex compress", RTControl::Toggle { on, .. }) => {
                if *on != settings.texture_compress {
                    settings.texture_compress = *on;
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}
