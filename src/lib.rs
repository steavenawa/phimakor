//! phimakor library: high performance chart editor/previewer that with any you need it. Including Python, Gui, FXs and More Fancy And Smoothly.
//!
//!
//! The `phimakor` binary (`main.rs`) is a thin winit shell over these
//! modules; embedders (iced editor, mobile readback) use
//! [`render::preview::PreviewEngine`] for surfaceless offscreen frames.
//!
//! # Feature flags
//!
//! - `profiling`: enables dhat heap profiler + Tracy GPU instrumentation.

/// Emit a tracing span AND (when `profiling` feature is active) a Tracy zone.
/// Use like `let _s = trace_span!("name");` at function entry.
#[macro_export]
macro_rules! trace_span {
    ($name:expr) => {{
        let _span = tracing::trace_span!($name).entered();
        #[cfg(feature = "profiling")]
        let _tracy = tracy_client::span!($name);
        _span
    }};
}

pub mod audio;
pub mod core;
pub mod engine;
pub mod render;

#[cfg(feature = "python")]
mod python_bindings;

/// Python bindings for the Phimakor chart engine.
///
/// ```text
/// import phimakor as pk
/// doc = pk.Editor.open("chart_dir")
/// print(doc.info().name)
/// ```
#[cfg(feature = "python")]
#[pyo3::pymodule]
fn phimakor(_py: pyo3::Python<'_>, m: &pyo3::Bound<'_, pyo3::types::PyModule>) -> pyo3::PyResult<()> {
    python_bindings::register(m)?;
    Ok(())
}
