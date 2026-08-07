//! 播放器用的 core 子集(主仓源码 #[path] 引入,剪掉 edit/stream/pmk)。
//!
//! edit 的生产代码用 `std::time::Instant` + 线程 + fs(wasm 运行 panic,
//! 播放器不需要编辑);stream/pmk 是二进制格式,播放器不消费。
//! 保留:anim/bpm/chart/chart_format/easing/model(纯计算 + serde 解析)。
//!
//! 注意:与主仓 src/core/mod.rs 的公共常量必须保持同值(图表计算口径)。

#[path = "../../../src/core/anim.rs"]
pub mod anim;
#[path = "../../../src/core/bpm.rs"]
pub mod bpm;
#[path = "../../../src/core/chart.rs"]
pub mod chart;
#[path = "../../../src/core/chart_format.rs"]
pub mod chart_format;
#[path = "../../../src/core/easing.rs"]
pub mod easing;
#[path = "../../../src/core/model.rs"]
pub mod model;
// pmk/stream 纯计算(二进制/NDJSON 解析),保留可解析全部格式。
#[path = "../../../src/core/pmk/mod.rs"]
pub mod pmk;
#[path = "../../../src/core/stream.rs"]
pub mod stream;

#[allow(unused_imports)] // 与主仓 core/mod.rs 同构的再导出
pub use chart::{FrameState, LineState};

/// RGBA color, components in 0..=1(与主仓 core::Color 同值)。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const WHITE: Self = Self { r: 1., g: 1., b: 1., a: 1. };

    pub fn from_rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self {
            r: r as f32 / 255.,
            g: g as f32 / 255.,
            b: b as f32 / 255.,
            a: a as f32 / 255.,
        }
    }
}

pub const HEIGHT_RATIO: f64 = 0.83175;
pub const EPS: f64 = 1e-5;
pub const RPE_WIDTH: f32 = 1350.;
pub const RPE_HEIGHT: f32 = 900.;
/// prpr: `10. / 45. / HEIGHT_RATIO`
pub const SPEED_RATIO: f64 = 10. / 45. / HEIGHT_RATIO;
