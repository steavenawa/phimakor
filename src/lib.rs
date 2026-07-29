//! phimakor library: RPE chart evaluation core, audio clock, wgpu renderer.
//!
//! The `phimakor` binary (`main.rs`) is a thin winit shell over these
//! modules; embedders (iced editor, mobile readback) use
//! [`render::preview::PreviewEngine`] for surfaceless offscreen frames.

pub mod audio;
pub mod core;
pub mod render;
