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
    /// PMCORE-24:自动保存开关(默认开)。编辑后经后台 saver 线程防抖落盘,
    /// 写主文件前旧版本保留为 <name>.bak。
    pub autosave: bool,
    /// PMCORE-24:自动保存防抖间隔(秒,默认 1.0)。
    pub autosave_interval: f32,
    /// PMCORE-6:frame lock —— 窗口失焦时停止渲染(省 GPU/CPU)。默认关,
    /// 保持现状(失焦仍渲染,可边看谱边切窗口)。
    pub frame_lock: bool,
    /// PMCORE-76:鼠标 hover 上下文信息浮层(时间轴 note/事件块)。默认开;
    /// 关掉后浮层完全不绘制(命中与渲染均跳过)。
    pub hover_tooltip: bool,
    /// 音频设备输出延迟补偿(毫秒,默认 15.0,与 PHIMAKOR_AUDIO_LATENCY_MS
    /// 环境变量默认一致)。驱动 render_frame 的 `device_latency = v/1000`。
    pub audio_latency_ms: f32,
}

/// 过激优化:hold 身体按视口裁剪(线段求交+勾股长度),长 hold 省大量
/// off-screen overdraw;视觉上等价但有回归风险。
pub use phimakor::render::AGGRESSIVE_HOLD_CLIP;

impl Default for SettingsData {
    fn default() -> Self {
        Self { vsync: true, gui_scale: 1.0, fullscreen: false, backend: None, charts_dir: None, perf_hint: false, fps_overlay: true, custom_cursor: false, aggressive: 0, half_res_fx: true, texture_compress: true, autosave: true, autosave_interval: 1.0, frame_lock: false, hover_tooltip: true, audio_latency_ms: 15.0 }
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

use super::flow;
use super::widgets::{RTControl, RealtimeForm, Widget};

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
        ("autosave".into(), RTControl::Toggle { on: settings.autosave, anim: if settings.autosave { 1.0 } else { 0.0 }, dir: if settings.autosave { 1.0 } else { -1.0 } }),
        ("autosave interval".into(), RTControl::Number { value: settings.autosave_interval as f64, step: 0.1, min: 0.1, max: 60.0, last_x: 0.0, buf: None }),
        ("audio latency".into(), RTControl::Number { value: settings.audio_latency_ms as f64, step: 1.0, min: 1.0, max: 100.0, last_x: 0.0, buf: None }),
        ("frame lock".into(), RTControl::Toggle { on: settings.frame_lock, anim: if settings.frame_lock { 1.0 } else { 0.0 }, dir: if settings.frame_lock { 1.0 } else { -1.0 } }),
        ("hover tooltip".into(), RTControl::Toggle { on: settings.hover_tooltip, anim: if settings.hover_tooltip { 1.0 } else { 0.0 }, dir: if settings.hover_tooltip { 1.0 } else { -1.0 } }),
    ]);
    form.row_h = 24.0 * s;
    form.gap = 4.0 * s;
    form
}

/// 设置表单流控区域(命中 = 绘制,单一真源):行 id = 索引+1,Add 按钮
/// id = rows+1。几何委托组件库 [`Widget::areas`](RealtimeForm::areas)
/// (widgets.rs 为 DevMenuBase 领地,只读,这里仅做 Area → HotArea 转换)。
pub fn form_areas(form: &RealtimeForm) -> Vec<flow::HotArea> {
    use flow::{AreaId, AreaKind, HotArea};
    form.areas()
        .into_iter()
        .filter(|a| a.id != 0) // 标题(id=0)不可交互,不注册
        .map(|a| HotArea {
            id: AreaId(a.id),
            rect: (a.rect.x(), a.rect.y(), a.rect.width(), a.rect.height()),
            kind: AreaKind::Widget(a.kind),
            disabled: false,
        })
        .collect()
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
            ("autosave", RTControl::Toggle { on, .. }) => {
                if *on != settings.autosave {
                    settings.autosave = *on;
                    changed = true;
                }
            }
            ("autosave interval", RTControl::Number { value, .. }) => {
                let v = (*value as f32).clamp(0.1, 60.0);
                if (v - settings.autosave_interval).abs() > 0.01 {
                    settings.autosave_interval = v;
                    changed = true;
                }
            }
            ("audio latency", RTControl::Number { value, .. }) => {
                let v = (*value as f32).clamp(1.0, 100.0);
                if (v - settings.audio_latency_ms).abs() > 0.01 {
                    settings.audio_latency_ms = v;
                    changed = true;
                }
            }
            ("frame lock", RTControl::Toggle { on, .. }) => {
                if *on != settings.frame_lock {
                    settings.frame_lock = *on;
                    changed = true;
                }
            }
            ("hover tooltip", RTControl::Toggle { on, .. }) => {
                if *on != settings.hover_tooltip {
                    settings.hover_tooltip = *on;
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PMCORE-6:frame_lock 默认 false;旧 config.json 缺该字段回退默认;
    /// 开启后序列化→反序列化保持(持久化契约)。
    #[test]
    fn frame_lock_defaults_false_and_roundtrips() {
        assert!(!SettingsData::default().frame_lock);
        let old: SettingsData = serde_json::from_str(r#"{"vsync":true}"#).unwrap();
        assert!(!old.frame_lock, "旧配置无 frame_lock 字段应回退默认 false");
        let mut on = SettingsData::default();
        on.frame_lock = true;
        let back: SettingsData = serde_json::from_str(&serde_json::to_string(&on).unwrap()).unwrap();
        assert!(back.frame_lock, "开启后写回 config.json 再读回应为 true");
    }

    /// PMCORE-6:build_settings_form 含 frame lock 行,apply_settings_form 写回。
    #[test]
    fn frame_lock_form_row_writes_back() {
        let mut s = SettingsData::default();
        let mut form = build_settings_form(0.0, 0.0, 300.0, 1.0, &s);
        let row = form.rows.iter_mut().find(|(l, _)| l == "frame lock").expect("表单应有 frame lock 行");
        if let RTControl::Toggle { on, .. } = &mut row.1 {
            *on = true;
        } else {
            panic!("frame lock 行应为 Toggle");
        }
        assert!(apply_settings_form(&form, &mut s));
        assert!(s.frame_lock);
    }

    /// PMCORE-76:hover tooltip 默认 true;旧 config.json 缺该字段回退默认;
    /// 关闭后序列化→反序列化保持(持久化契约)。
    #[test]
    fn hover_tooltip_defaults_true_and_roundtrips() {
        assert!(SettingsData::default().hover_tooltip);
        let old: SettingsData = serde_json::from_str(r#"{"vsync":true}"#).unwrap();
        assert!(old.hover_tooltip, "旧配置无 hover_tooltip 字段应回退默认 true");
        let mut off = SettingsData::default();
        off.hover_tooltip = false;
        let back: SettingsData = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(!back.hover_tooltip, "关闭后写回 config.json 再读回应为 false");
    }

    /// 流控区域(命中 = 绘制):设置表单每行注册为 Widget 区域(id = 行索引+1),
    /// 标题(0)与 Add 按钮语义与 BPM/Eff 同源(组件库 areas 转换)。
    #[test]
    fn form_areas_registers_all_rows() {
        let form = build_settings_form(0.0, 0.0, 300.0, 1.0, &SettingsData::default());
        let areas = form_areas(&form);
        assert!(!areas.iter().any(|a| a.id.0 == 0)); // 标题不注册
        assert_eq!(areas.len(), form.rows.len() + 1); // 行 + Add 按钮
        for i in 0..form.rows.len() {
            assert!(areas.iter().any(|a| a.id.0 == i as u32 + 1));
        }
        assert!(areas.iter().any(|a| a.id.0 == form.rows.len() as u32 + 1)); // Add
    }

    /// PMCORE-76:build_settings_form 含 hover tooltip 行,apply_settings_form 写回。
    #[test]
    fn hover_tooltip_form_row_writes_back() {
        let mut s = SettingsData::default();
        let mut form = build_settings_form(0.0, 0.0, 300.0, 1.0, &s);
        let row = form.rows.iter_mut().find(|(l, _)| l == "hover tooltip").expect("表单应有 hover tooltip 行");
        if let RTControl::Toggle { on, .. } = &mut row.1 {
            *on = false;
        } else {
            panic!("hover tooltip 行应为 Toggle");
        }
        assert!(apply_settings_form(&form, &mut s));
        assert!(!s.hover_tooltip);
    }

    /// audio latency 默认 15.0;旧 config.json 缺该字段回退默认;
    /// 表单含 "audio latency" Number 行,apply_settings_form 写回(clamp 1.0..100.0)。
    #[test]
    fn audio_latency_defaults_and_form_row_writes_back() {
        assert_eq!(SettingsData::default().audio_latency_ms, 15.0);
        let old: SettingsData = serde_json::from_str(r#"{"vsync":true}"#).unwrap();
        assert_eq!(old.audio_latency_ms, 15.0, "旧配置无 audio_latency_ms 字段应回退默认 15.0");
        let mut s = SettingsData::default();
        let mut form = build_settings_form(0.0, 0.0, 300.0, 1.0, &s);
        let row = form.rows.iter_mut().find(|(l, _)| l == "audio latency").expect("表单应有 audio latency 行");
        if let RTControl::Number { value, .. } = &mut row.1 {
            *value = 42.0;
        } else {
            panic!("audio latency 行应为 Number");
        }
        assert!(apply_settings_form(&form, &mut s));
        assert_eq!(s.audio_latency_ms, 42.0);
    }
}
