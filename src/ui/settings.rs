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
}

impl Default for SettingsData {
    fn default() -> Self {
        Self { vsync: true, gui_scale: 1.0, fullscreen: false, backend: None, charts_dir: None }
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
