//! phimakor library: RPE chart evaluation core, audio clock, wgpu renderer.
//!
//! Architecture overview: [`docs/render-arch.md`] and [`docs/perf.md`].
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
