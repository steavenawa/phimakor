//! 轻量 UI 组件库。
//!
//! 设计原则:
//! - **绘制与命中同源**:组件通过 [`VList`]/[`HList`] 的 `areas()` 输出命中
//!   区域——布局计算只有一份,命中直接查区域,不再维护"绘制几何"与
//!   "命中几何"两套魔法数字(旧面板靠 `must match` 注释人工同步)。
//! - **纯数据**:组件只含布局数字,不含 GPU/渲染状态,可单元测试。
//! - **线性布局**:VList/HList 足够覆盖编辑器面板(列表/按钮排/编辑行),
//!   不做 flex 引擎。
//!
//! 使用模式:绘制时用 `areas()` 拿到区域(同时用于绘制背景/高亮),命中时
//! 用同一个布局函数 `hit()` 查指针位置。

use tiny_skia::Rect;

/// 命中区域:组件在布局阶段产出的可交互矩形。
/// `id` 由组件自定(如行索引、按钮索引),hit 返回即可。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area {
    pub id: u32,
    pub rect: Rect,
}

/// 样式 token:替代散落在各面板的魔法数字(48.0*s / 22.0*s 之类)。
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// 标准行高。
    pub row_h: f32,
    /// 组件间间距。
    pub gap: f32,
    /// 文本横向内边距。
    pub pad_x: f32,
    /// 标准字号(已含 gui_scale)。
    pub font_size: f32,
    /// 面板背景。
    pub bg: [u8; 4],
    /// 行背景。
    pub row: [u8; 4],
    /// 悬停/选中背景。
    pub hover: [u8; 4],
    /// 文本主色。
    pub text: [u8; 3],
    /// 文本暗色(次要信息)。
    pub text_dim: [u8; 3],
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            row_h: 22.0,
            gap: 6.0,
            pad_x: 8.0,
            font_size: 10.0,
            bg: [12, 12, 14, 200],
            row: [20, 20, 25, 120],
            hover: [60, 90, 140, 90],
            text: [230, 230, 235],
            text_dim: [130, 130, 140],
        }
    }
}

/// 垂直列表布局:固定行高,`n` 行,行 id = 行索引。
#[derive(Clone, Copy, Debug)]
pub struct VList {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub n: usize,
    pub row_h: f32,
    pub gap: f32,
}

impl VList {
    pub fn new(x: f32, y: f32, w: f32, n: usize) -> Self {
        Self { x, y, w, n, row_h: 22.0, gap: 0.0 }
    }

    pub fn with_row_h(mut self, row_h: f32) -> Self {
        self.row_h = row_h;
        self
    }

    pub fn with_gap(mut self, gap: f32) -> Self {
        self.gap = gap;
        self
    }

    /// 第 `i` 行的矩形。
    pub fn row_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + i as f32 * (self.row_h + self.gap), self.w, self.row_h)
            .expect("VList row rect")
    }

    /// 全部命中区域(行 id = 行索引)。
    pub fn areas(&self) -> Vec<Area> {
        (0..self.n).map(|i| Area { id: i as u32, rect: self.row_rect(i) }).collect()
    }

    /// 命中:返回行索引,或 `None`。
    pub fn hit(&self, p: (f32, f32)) -> Option<usize> {
        let (px, py) = p;
        let step = self.row_h + self.gap;
        if px < self.x || px > self.x + self.w || py < self.y {
            return None;
        }
        let i = ((py - self.y) / step) as usize;
        if i < self.n && py <= self.y + i as f32 * step + self.row_h {
            Some(i)
        } else {
            None
        }
    }
}

/// 水平列表布局:`n` 个等宽项(按钮排)。
#[derive(Clone, Copy, Debug)]
pub struct HList {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub gap: f32,
    pub n: usize,
}

impl HList {
    pub fn new(x: f32, y: f32, w: f32, h: f32, n: usize) -> Self {
        Self { x, y, w, h, gap: 8.0, n }
    }

    pub fn with_gap(mut self, gap: f32) -> Self {
        self.gap = gap;
        self
    }

    fn item_w(&self) -> f32 {
        let gaps = self.gap * (self.n.saturating_sub(1)) as f32;
        ((self.w - gaps) / self.n.max(1) as f32).max(1.0)
    }

    /// 第 `i` 项的矩形。
    pub fn item_rect(&self, i: usize) -> Rect {
        let iw = self.item_w();
        Rect::from_xywh(self.x + i as f32 * (iw + self.gap), self.y, iw, self.h).expect("HList item rect")
    }

    /// 全部命中区域(项 id = 项索引)。
    pub fn areas(&self) -> Vec<Area> {
        (0..self.n).map(|i| Area { id: i as u32, rect: self.item_rect(i) }).collect()
    }

    /// 命中:返回项索引,或 `None`(右开区间,px = 右缘不命中)。
    pub fn hit(&self, p: (f32, f32)) -> Option<usize> {
        let (px, py) = p;
        if py < self.y || py > self.y + self.h || px < self.x || px > self.x + self.w {
            return None;
        }
        (0..self.n).find(|&i| {
            let r = self.item_rect(i);
            px >= r.left() && px < r.right()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlist_hit_returns_row_index() {
        let l = VList::new(0.0, 10.0, 100.0, 3).with_row_h(22.0);
        // 第 0 行
        assert_eq!(l.hit((50.0, 12.0)), Some(0));
        // 第 1 行
        assert_eq!(l.hit((50.0, 33.0)), Some(1));
        // 第 2 行底部边缘
        assert_eq!(l.hit((50.0, 10.0 + 2.0 * 22.0 + 21.0)), Some(2));
        // 越界
        assert_eq!(l.hit((50.0, 5.0)), None);
        assert_eq!(l.hit((50.0, 10.0 + 3.0 * 22.0)), None);
        assert_eq!(l.hit((120.0, 20.0)), None);
    }

    #[test]
    fn vlist_hit_respects_gap() {
        let l = VList::new(0.0, 0.0, 100.0, 3).with_row_h(20.0).with_gap(4.0);
        // 第 0 行:0-20
        assert_eq!(l.hit((10.0, 10.0)), Some(0));
        // 间隙 20-24 → 无命中(落到第 1 行区域外)
        assert_eq!(l.hit((10.0, 22.0)), None);
        // 第 1 行:24-44
        assert_eq!(l.hit((10.0, 30.0)), Some(1));
    }

    #[test]
    fn vlist_areas_match_row_rects() {
        let l = VList::new(5.0, 5.0, 80.0, 2).with_row_h(22.0);
        let areas = l.areas();
        assert_eq!(areas.len(), 2);
        for (i, a) in areas.iter().enumerate() {
            assert_eq!(a.id, i as u32);
            assert_eq!(a.rect, l.row_rect(i));
        }
        assert_eq!(l.row_rect(1).top(), 5.0 + 22.0);
    }

    #[test]
    fn hlist_hit_returns_item_index() {
        // 3 个等宽按钮:总宽 100,gap 0 → 每项 33.33
        let l = HList::new(0.0, 0.0, 100.0, 20.0, 3).with_gap(0.0);
        assert_eq!(l.hit((10.0, 10.0)), Some(0));
        assert_eq!(l.hit((40.0, 10.0)), Some(1));
        assert_eq!(l.hit((80.0, 10.0)), Some(2));
        assert_eq!(l.hit((100.0, 10.0)), None);
        assert_eq!(l.hit((10.0, 25.0)), None);
    }

    #[test]
    fn hlist_with_gap_no_hit_in_gap() {
        // 2 个按钮,总宽 100,gap 10 → 每项 45
        let l = HList::new(0.0, 0.0, 100.0, 20.0, 2).with_gap(10.0);
        assert_eq!(l.hit((10.0, 10.0)), Some(0));
        // 间隙 45-55
        assert_eq!(l.hit((50.0, 10.0)), None);
        assert_eq!(l.hit((60.0, 10.0)), Some(1));
    }

    #[test]
    fn theme_default_is_sane() {
        let t = Theme::default();
        assert!(t.row_h > 0.0 && t.font_size > 0.0);
        assert_eq!(t.bg.len(), 4);
        assert_eq!(t.text.len(), 3);
    }
}
