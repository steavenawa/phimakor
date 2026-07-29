use serde::Deserialize;
use std::collections::HashMap;

/// One field/cell in a panel.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum CellDef {
    /// Label + value pair.
    Field {
        label: String,
        /// Template like `{line_name}` or `{pos_x:.3}`.
        value: String,
        #[serde(default = "one")]
        span: u32,
    },
    /// Horizontal separator.
    Separator,
    /// Static label spanning full width.
    Label {
        label: String,
        #[serde(default)]
        span: u32,
    },
    /// Section split (thicker divider).
    Split,
}

fn one() -> u32 { 1 }

/// One panel definition.
#[derive(Clone, Debug, Deserialize)]
pub struct PanelDef {
    pub name: String,
    #[serde(default = "default_width")]
    pub width: f32,
    #[serde(default)]
    pub cells: Vec<CellDef>,
}

fn default_width() -> f32 { 280.0 }

/// Top-level layout definition.
#[derive(Clone, Debug, Deserialize)]
pub struct LayoutDef {
    pub panels: Vec<PanelDef>,
}

impl LayoutDef {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let src = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
        serde_json::from_str(&src).map_err(|e| format!("parse: {e}"))
    }
}

impl PanelDef {
    /// Render panel fields by substituting `{key}` with values from `vals`.
    /// Returns (label, value, span, kind) where kind: 0=field, 1=separator, 2=split.
    pub fn resolve<'a>(&'a self, vals: &HashMap<&str, String>) -> Vec<(&'a str, String, u32, u8)> {
        let mut out = Vec::new();
        for cell in &self.cells {
            match cell {
                CellDef::Field { label, value, span } => {
                    let resolved = resolve_template(value, vals);
                    out.push((label.as_str(), resolved, *span, 0));
                }
                CellDef::Separator => {
                    out.push(("", String::new(), 0, 1));
                }
                CellDef::Label { label, span } => {
                    out.push((label.as_str(), String::new(), *span, 0));
                }
                CellDef::Split => {
                    out.push(("", String::new(), 0, 2));
                }
            }
        }
        out
    }
}

fn resolve_template(tmpl: &str, vals: &HashMap<&str, String>) -> String {
    let mut s = tmpl.to_string();
    for (k, v) in vals {
        let pat = format!("{{{}}}", k);
        s = s.replace(&pat, v);
    }
    s
}
