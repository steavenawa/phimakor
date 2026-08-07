//! 轻量 UI 组件库。
//!
//! 设计原则:
//! - **绘制与命中同源**:组件通过 [`Widget::areas`] 输出命中区域,命中
//!   直接查区域,不再维护"绘制几何"与"命中几何"两套魔法数字(旧面板靠
//!   `must match` 注释人工同步)。
//! - **纯数据**:组件只含布局数字与状态,不含 GPU/渲染状态,可单元测试;
//!   绘制走 [`Canvas`] 抽象(矩形 + 文本),由宿主提供 tiny_skia/字体实现。
//! - **线性布局**:VList/HList 足够覆盖编辑器面板(列表/按钮排/编辑行),
//!   不做 flex 引擎。
//!
//! 组件库是增量开发的公共 API——大量构造器/方法当前只被部分面板使用,
//! 属于备用接口而非死代码,模块级抑制 dead_code 警告。

#![allow(dead_code)]
//!
//! 使用模式:绘制时用 `areas()` 拿到区域(同时用于绘制背景/高亮),命中时
//! 用同一个布局函数 `hit()` 查指针位置。

use tiny_skia::Rect;

/// 命中区域的语义类型(绘制层据此决定画法,hover 层据此区分交互)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AreaKind {
    /// 垂直列表行。
    ListRow,
    /// 按钮。
    Button,
    /// 开关行(label 区)。
    ToggleTrack,
    /// 开关滑块。
    ToggleKnob,
    /// 滑条轨道。
    SliderTrack,
    /// 滑条滑块。
    SliderKnob,
    /// 数值/文本字段行。
    Field,
    /// 可滚动列表的可见行。
    ScrollRow,
    /// 滚动条。
    ScrollBar,
    /// 面板标题行。
    PanelTitle,
    /// 下拉框主按钮。
    ComboBoxButton,
    /// 下拉框展开的选项行。
    ComboBoxItem,
    /// 步进器数值区。
    StepperField,
    /// 步进器减号按钮。
    StepperMinus,
    /// 步进器加号按钮。
    StepperPlus,
    /// 复选框行。
    Checkbox,
    /// 复选框勾选框。
    CheckboxBox,
    /// 标签页。
    TabBarTab,
    /// 进度条。
    ProgressBar,
    /// 键值网格行。
    GridRow,
    /// 文本输入框。
    TextInput,
    /// 拖动数值框。
    DragValue,
    /// 颜色通道行(R/G/B)。
    ColorChannel,
    /// 颜色预览块。
    ColorPreview,
    /// 多选列表行。
    ListBoxRow,
    /// 单选组选项行。
    RadioRow,
    /// 菜单外部捕获区(全窗;点击外部关闭菜单,全屏时兼遮罩)。
    MenuMask,
    /// 菜单项行。
    MenuItem,
}

/// 命中区域:组件在布局阶段产出的可交互矩形。
/// `id` 由组件自定(如行索引、按钮索引),hit 返回即可。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area {
    pub kind: AreaKind,
    pub id: u32,
    pub rect: Rect,
}

/// 绘制后端:矩形填充 + 文本。宿主实现(ui_kit 用 tiny_skia + 字体,
/// 主程序可换成现有 text 模块)。
pub trait Canvas {
    /// 填充矩形(组件矩形均已裁剪好,后端仍需防御越界)。
    fn fill(&mut self, r: Rect, rgba: [u8; 4]);
    /// 绘制文本。`y` 为基线。
    fn text(&mut self, s: &str, x: f32, y: f32, size: f32, rgb: [u8; 3]);
    /// 测量文本宽度(宿主用真实字体度量实现,供光标定位/居中)。
    fn text_width(&mut self, s: &str, size: f32) -> f32;
}

/// 通用按键(焦点组件的键盘交互)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetKey {
    Char(char),
    Backspace,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Enter,
    Tab,
    ShiftTab,
    Escape,
}

/// 组件统一接口:布局、命中、绘制共用同一份几何。
pub trait Widget {
    /// 全部命中区域(绘制层画这些矩形,同时用于背景/高亮)。
    fn areas(&self) -> Vec<Area>;
    /// 查指针位置,返回命中的那个区域(带 kind+id)。组件外返回 `None`。
    fn hit_area(&self, p: (f32, f32)) -> Option<Area>;
    /// 绘制自身。`hover` 为当前悬停的区域(由 hit_area 得出)。
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>);
    /// 顶层绘制:在全部组件画完之后调用,用于悬浮层(下拉列表、菜单等)。
    /// 默认不画。
    fn draw_overlay(&self, _cv: &mut dyn Canvas, _theme: &Theme, _hover: Option<&Area>) {}
    /// 每帧推进动画(dt 秒)。默认无动画。
    fn update(&mut self, _dt: f32) {}
    /// 点击(按下):切换状态。`p` 为指针位置。
    fn on_click(&mut self, _p: (f32, f32)) {}
    /// 带文本度量的点击(PMCORE-61):宿主注入 `pad_x` 与 `measure`(text_width
    /// 回调,字号须与 draw 一致),文本类组件据此把光标定位到最近字符边界。
    /// 默认退化为 `on_click`,未覆盖的组件行为不变。
    fn on_click_with_measure(&mut self, p: (f32, f32), _pad_x: f32, _measure: &dyn Fn(&str) -> f32) {
        self.on_click(p);
    }
    /// 拖拽(按下后移动):更新值。`p` 为指针位置。
    fn on_drag(&mut self, _p: (f32, f32)) {}
    /// 键盘输入一个字符(焦点组件接收)。默认忽略。
    #[allow(dead_code)] // trait 默认实现:被部分组件覆盖
    fn on_text(&mut self, _c: char) {}
    /// 键盘退格。默认忽略。
    #[allow(dead_code)]
    fn on_backspace(&mut self) {}
    /// 通用按键(方向键/Tab/Enter 等)。默认忽略。
    fn on_key(&mut self, _k: WidgetKey) {}
    /// 滚轮(垂直方向,delta 为步进数)。默认忽略。
    fn on_wheel(&mut self, _dy: f32) {}
    /// 焦点变化(点击/失焦)。默认忽略。
    fn set_focus(&mut self, _focused: bool) {}
}

// ── 缓动曲线(开关/滑块动画用,内联避免依赖 core::easing)──

/// cubic out:快速起步,缓停。
fn ease_out_cubic(x: f32) -> f32 {
    let x = x - 1.0;
    1.0 + x * x * x
}

/// 指数平滑推进动画值:向 `target` 逼近,帧率无关。
fn approach_anim(anim: &mut f32, target: f32, dt: f32, speed: f32) {
    let d = target - *anim;
    if d.abs() < 0.0005 {
        *anim = target;
    } else {
        *anim += d * (1.0 - (-dt * speed).exp());
    }
}

/// 点是否在 `r` 内(右开区间,与 HList::hit 一致)。
fn inside(p: (f32, f32), r: Rect) -> bool {
    p.0 >= r.left() && p.0 < r.right() && p.1 >= r.top() && p.1 < r.bottom()
}

/// 点击 x(相对文本左缘)→ 最近字符边界的光标位置(PMCORE-61)。
/// 边界 = 各前缀的 text_width(与 draw 光标同一度量来源);`avail` 为可见
/// 文本区宽度(输入框宽 − pad_x)。文本超宽时,点击落在最后一个可见边界
/// 右侧 → 直接到末尾(光标不再右移,也不会超过 len)。
fn caret_from_x(text: &str, x: f32, avail: f32, measure: &dyn Fn(&str) -> f32) -> usize {
    let n = text.chars().count();
    if n == 0 {
        return 0;
    }
    let avail = avail.max(0.0);
    // 前缀宽度表:widths[i] = 前 i 个字符的宽度(0..=n)。
    let mut widths = Vec::with_capacity(n + 1);
    widths.push(0.0);
    let mut prefix = String::with_capacity(text.len());
    for c in text.chars() {
        prefix.push(c);
        widths.push(measure(&prefix));
    }
    let x = x.max(0.0).min(avail);
    // 超宽:点击 ≥ 最后一个可见边界 → len。
    if widths[n] > avail {
        let last_visible = widths.iter().rposition(|&w| w <= avail).unwrap_or(0);
        if x >= widths[last_visible] {
            return n;
        }
    }
    // 最近字符边界(距某前缀 text_width 的差最小)。
    let mut best = 0;
    let mut best_d = (x - widths[0]).abs();
    for i in 1..=n {
        let d = (x - widths[i]).abs();
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// 矩形中心点(tiny_skia 的 Rect 没有 center())。
#[cfg(test)]
pub(crate) fn center(r: Rect) -> (f32, f32) {
    (r.x() + r.width() * 0.5, r.y() + r.height() * 0.5)
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
    /// 强调色(滑条填充/开关 on)。
    pub accent: [u8; 3],
    /// 滑块/开关把手。
    pub knob: [u8; 4],
    /// 禁用态。
    pub disabled: [u8; 4],
    /// 面板标题文本色。
    pub title: [u8; 3],
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
            accent: [100, 180, 255],
            knob: [205, 210, 225, 255],
            disabled: [40, 40, 46, 120],
            title: [255, 200, 80],
        }
    }
}

impl Theme {
    /// 整体缩放:行高/间距/内边距/字号 × `s`(颜色不变)。
    /// 搭配场景坐标同步 ×s 使用,让元件更大、更可读。
    pub fn scaled(&self, s: f32) -> Self {
        Self {
            row_h: self.row_h * s,
            gap: self.gap * s,
            pad_x: self.pad_x * s,
            font_size: self.font_size * s,
            ..*self
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
        (0..self.n)
            .map(|i| Area { kind: AreaKind::ListRow, id: i as u32, rect: self.row_rect(i) })
            .collect()
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

impl Widget for VList {
    fn areas(&self) -> Vec<Area> { self.areas() }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.hit(p).map(|i| Area { kind: AreaKind::ListRow, id: i as u32, rect: self.row_rect(i) })
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        for a in self.areas() {
            let on = hover == Some(&a);
            let c = if on { theme.hover } else { theme.row };
            cv.fill(a.rect, c);
            cv.text(&a.id.to_string(), a.rect.x() + theme.pad_x, a.rect.y() + theme.row_h * 0.72, theme.font_size, if on { theme.text } else { theme.text_dim });
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
        (0..self.n)
            .map(|i| Area { kind: AreaKind::Button, id: i as u32, rect: self.item_rect(i) })
            .collect()
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

impl Widget for HList {
    fn areas(&self) -> Vec<Area> { self.areas() }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.hit(p).map(|i| Area { kind: AreaKind::Button, id: i as u32, rect: self.item_rect(i) })
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        for a in self.areas() {
            let on = hover == Some(&a);
            let c = if on { theme.hover } else { theme.row };
            cv.fill(a.rect, c);
            cv.text(&a.id.to_string(), a.rect.x() + theme.pad_x, a.rect.y() + theme.row_h * 0.72, theme.font_size, if on { theme.text } else { theme.text_dim });
        }
    }
}

/// 按钮:文字居中,支持禁用。
#[derive(Clone, Debug)]
pub struct Button {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub enabled: bool,
}

impl Button {
    pub fn new(x: f32, y: f32, w: f32, h: f32, label: impl Into<String>) -> Self {
        Self { x, y, w, h, label: label.into(), enabled: true }
    }

    pub fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).expect("Button rect")
    }
}

impl Widget for Button {
    fn areas(&self) -> Vec<Area> {
        vec![Area { kind: AreaKind::Button, id: 0, rect: self.rect() }]
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        inside(p, self.rect()).then(|| Area { kind: AreaKind::Button, id: 0, rect: self.rect() })
    }
    fn on_click(&mut self, _p: (f32, f32)) {
        self.enabled = !self.enabled;
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let on = hover.is_some();
        let bg = if !self.enabled { theme.disabled } else if on { theme.hover } else { theme.row };
        cv.fill(self.rect(), bg);
        let color = if !self.enabled { theme.text_dim } else if on { theme.text } else { theme.text };
        let tw = self.label.len() as f32 * theme.font_size * 0.55;
        cv.text(&self.label, self.x + (self.w - tw) * 0.5, self.y + self.h * 0.72, theme.font_size, color);
    }
}

/// 开关行:label + 右侧滑块(开关),knob 位置带 ease 动画。
/// 开/关两个方向都是"快起步、慢结束"(ease_out_cubic 及其镜像)。
#[derive(Clone, Debug)]
pub struct Toggle {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub on: bool,
    /// knob 动画位置:0 = off,1 = on(on/off 切换时平滑过渡)。
    pub anim: f32,
    /// 动画方向:+1 = 正在开(0→1),-1 = 正在关(1→0)。
    pub dir: f32,
}

impl Toggle {
    pub fn new(x: f32, y: f32, w: f32, h: f32, label: impl Into<String>, on: bool) -> Self {
        Self { x, y, w, h, label: label.into(), on, anim: if on { 1.0 } else { 0.0 }, dir: if on { 1.0 } else { -1.0 } }
    }

    fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).expect("Toggle rect")
    }

    /// 右侧开关轨道矩形(30px 宽)。
    fn track_rect(&self) -> Rect {
        Rect::from_xywh(self.x + self.w - 34.0, self.y + (self.h - 14.0) * 0.5, 30.0, 14.0)
            .expect("Toggle track")
    }

    /// 当前方向对应的缓动曲线。两个方向都是"快起步、慢结束":
    /// 开(0→1)用 ease_out_cubic(anim),关(1→0)用其镜像
    /// 1 - ease_out_cubic(1-anim),保证关方向同样快起步慢结束。
    fn eased(&self) -> f32 {
        let a = self.anim.clamp(0.0, 1.0);
        if self.dir > 0.0 {
            ease_out_cubic(a)
        } else {
            1.0 - ease_out_cubic(1.0 - a)
        }
    }

    /// 开关滑块矩形(沿轨道移动,位置 = anim 经方向化 ease)。
    fn knob_rect(&self) -> Rect {
        let t = self.track_rect();
        let travel = t.width() - 10.0;
        let kx = t.left() + travel * self.eased();
        Rect::from_xywh(kx, t.y() + 2.0, 10.0, t.height() - 4.0).expect("Toggle knob")
    }
}

impl Widget for Toggle {
    fn areas(&self) -> Vec<Area> {
        vec![
            Area { kind: AreaKind::ToggleTrack, id: 0, rect: self.rect() },
            Area { kind: AreaKind::ToggleKnob, id: 1, rect: self.knob_rect() },
        ]
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        if inside(p, self.knob_rect()) {
            return Some(Area { kind: AreaKind::ToggleKnob, id: 1, rect: self.knob_rect() });
        }
        inside(p, self.rect()).then(|| Area { kind: AreaKind::ToggleTrack, id: 0, rect: self.rect() })
    }
    fn update(&mut self, dt: f32) {
        let target = if self.on { 1.0 } else { 0.0 };
        // 方向由目标减去当前位置决定(切换瞬间翻转,ease 曲线随之切换)。
        let d = target - self.anim;
        if d.abs() > 0.0005 {
            self.dir = d.signum();
        }
        approach_anim(&mut self.anim, target, dt, 24.0);
    }
    fn on_click(&mut self, _p: (f32, f32)) {
        self.on = !self.on;
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let on = hover.map_or(false, |a| a.kind == AreaKind::ToggleTrack);
        cv.fill(self.rect(), if on { theme.hover } else { theme.row });
        cv.text(&self.label, self.x + theme.pad_x, self.y + self.h * 0.72, theme.font_size, theme.text);
        let track = self.track_rect();
        let mut track_c = [theme.disabled[0], theme.disabled[1], theme.disabled[2], 255];
        if self.on {
            track_c = [theme.accent[0], theme.accent[1], theme.accent[2], 255];
        }
        cv.fill(track, track_c);
        cv.fill(self.knob_rect(), theme.knob);
    }
}

/// 滑条:轨道 + 滑块,value 0..1。
#[derive(Clone, Copy, Debug)]
pub struct Slider {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub value: f32,
}

impl Slider {
    pub fn new(x: f32, y: f32, w: f32, h: f32, value: f32) -> Self {
        Self { x, y, w, h, value: value.clamp(0.0, 1.0) }
    }

    fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).expect("Slider rect")
    }

    /// 填充部分(0..value)。
    fn fill_rect(&self) -> Rect {
        let r = self.rect();
        Rect::from_xywh(r.x(), r.y(), r.width() * self.value, r.height()).expect("Slider fill")
    }

    /// 滑块矩形(沿填充末端移动)。
    fn knob_rect(&self) -> Rect {
        let r = self.rect();
        let kx = r.x() + (r.width() - 8.0) * self.value;
        Rect::from_xywh(kx, r.y(), 8.0, r.height()).expect("Slider knob")
    }
}

impl Widget for Slider {
    fn areas(&self) -> Vec<Area> {
        vec![
            Area { kind: AreaKind::SliderTrack, id: 0, rect: self.rect() },
            Area { kind: AreaKind::SliderKnob, id: 1, rect: self.knob_rect() },
        ]
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        if inside(p, self.knob_rect()) {
            return Some(Area { kind: AreaKind::SliderKnob, id: 1, rect: self.knob_rect() });
        }
        inside(p, self.rect()).then(|| Area { kind: AreaKind::SliderTrack, id: 0, rect: self.rect() })
    }
    fn on_click(&mut self, p: (f32, f32)) {
        let r = self.rect();
        if p.0 >= r.left() && p.0 <= r.right() {
            self.value = ((p.0 - r.left()) / r.width()).clamp(0.0, 1.0);
        }
    }
    fn on_drag(&mut self, p: (f32, f32)) {
        self.on_click(p);
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let r = self.rect();
        cv.fill(r, theme.row);
        if self.value > 0.01 {
            cv.fill(self.fill_rect(), [theme.accent[0], theme.accent[1], theme.accent[2], 255]);
        }
        let on = hover.map_or(false, |a| a.kind == AreaKind::SliderKnob);
        cv.fill(self.knob_rect(), if on { theme.hover } else { theme.knob });
        let _ = on;
    }
}

/// 字段行:label 左 + value 右,可编辑高亮。
#[derive(Clone, Debug)]
pub struct Field {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub value: String,
    pub editing: bool,
}

impl Field {
    pub fn new(x: f32, y: f32, w: f32, h: f32, label: impl Into<String>, value: impl Into<String>) -> Self {
        Self { x, y, w, h, label: label.into(), value: value.into(), editing: false }
    }

    pub fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).expect("Field rect")
    }
}

impl Widget for Field {
    fn areas(&self) -> Vec<Area> {
        vec![Area { kind: AreaKind::Field, id: 0, rect: self.rect() }]
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        inside(p, self.rect()).then(|| Area { kind: AreaKind::Field, id: 0, rect: self.rect() })
    }
    fn on_click(&mut self, _p: (f32, f32)) {
        self.editing = !self.editing;
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let on = hover.is_some() || self.editing;
        cv.fill(self.rect(), if on { theme.hover } else { theme.row });
        cv.text(&self.label, self.x + theme.pad_x, self.y + self.h * 0.72, theme.font_size, theme.text);
        let tw = self.value.len() as f32 * theme.font_size * 0.55;
        cv.text(&self.value, self.x + self.w - tw - theme.pad_x, self.y + self.h * 0.72, theme.font_size, if self.editing { theme.accent } else { theme.text_dim });
    }
}

/// 面板行(复合组件用)。
#[derive(Clone, Debug)]
pub enum PanelRow {
    Button { label: String, enabled: bool },
    Toggle { label: String, on: bool, anim: f32, dir: f32 },
    Slider { value: f32 },
    Field { label: String, value: String, editing: bool },
}

impl PanelRow {
    fn kind(&self) -> AreaKind {
        match self {
            PanelRow::Button { .. } => AreaKind::Button,
            PanelRow::Toggle { .. } => AreaKind::ToggleTrack,
            PanelRow::Slider { .. } => AreaKind::SliderTrack,
            PanelRow::Field { .. } => AreaKind::Field,
        }
    }
}

/// 面板:标题 + 行序列(每行一个简单组件)。行 id = 行索引 + 1(0 = 标题)。
#[derive(Clone, Debug)]
pub struct Panel {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub title: String,
    pub rows: Vec<PanelRow>,
    pub row_h: f32,
    pub gap: f32,
}

impl Panel {
    pub fn new(x: f32, y: f32, w: f32, title: impl Into<String>, rows: Vec<PanelRow>) -> Self {
        Self { x, y, w, title: title.into(), rows, row_h: 22.0, gap: 6.0 }
    }

    fn row_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + self.row_h + i as f32 * (self.row_h + self.gap), self.w, self.row_h)
            .expect("Panel row rect")
    }
}

impl Widget for Panel {
    fn areas(&self) -> Vec<Area> {
        let mut out = vec![Area {
            kind: AreaKind::PanelTitle,
            id: 0,
            rect: Rect::from_xywh(self.x, self.y, self.w, self.row_h).expect("Panel title rect"),
        }];
        for (i, row) in self.rows.iter().enumerate() {
            out.push(Area { kind: row.kind(), id: (i + 1) as u32, rect: self.row_rect(i) });
        }
        out
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.areas().into_iter().find(|a| inside(p, a.rect))
    }
    fn update(&mut self, dt: f32) {
        for row in &mut self.rows {
            if let PanelRow::Toggle { on, anim, dir, .. } = row {
                let target = if *on { 1.0 } else { 0.0 };
                let d = target - *anim;
                if d.abs() > 0.0005 {
                    *dir = d.signum();
                }
                approach_anim(anim, target, dt, 24.0);
            }
        }
    }
    fn on_click(&mut self, p: (f32, f32)) {
        // 行 id = 索引 + 1(0 = 标题)。
        let Some(a) = self.hit_area(p) else { return };
        if a.id == 0 {
            return;
        }
        let i = (a.id - 1) as usize;
        // 先取行矩形(不可变借用),再可变借用行本身。
        let r = self.row_rect(i);
        let Some(row) = self.rows.get_mut(i) else { return };
        match row {
            PanelRow::Button { enabled, .. } => *enabled = !*enabled,
            PanelRow::Toggle { on, .. } => *on = !*on,
            PanelRow::Slider { value } => {
                if p.0 >= r.left() && p.0 <= r.right() {
                    *value = ((p.0 - r.left()) / r.width()).clamp(0.0, 1.0);
                }
            }
            PanelRow::Field { editing, .. } => *editing = !*editing,
        }
    }
    fn on_drag(&mut self, p: (f32, f32)) {
        let Some(a) = self.hit_area(p) else { return };
        if a.id == 0 || a.kind != AreaKind::SliderTrack {
            return;
        }
        let i = (a.id - 1) as usize;
        let r = self.row_rect(i);
        let Some(PanelRow::Slider { value }) = self.rows.get_mut(i) else { return };
        *value = ((p.0 - r.left()) / r.width()).clamp(0.0, 1.0);
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let title_rect = Rect::from_xywh(self.x, self.y, self.w, self.row_h).expect("Panel title rect");
        let title_on = hover.map_or(false, |a| a.kind == AreaKind::PanelTitle);
        cv.fill(title_rect, if title_on { theme.hover } else { theme.bg });
        cv.text(&self.title, self.x + theme.pad_x, self.y + self.row_h * 0.72, theme.font_size, theme.title);

        for (i, row) in self.rows.iter().enumerate() {
            let r = self.row_rect(i);
            let on = hover.map_or(false, |a| a.id == (i + 1) as u32);
            cv.fill(r, if on { theme.hover } else { theme.row });
            match row {
                PanelRow::Button { label, enabled } => {
                    let tw = label.len() as f32 * theme.font_size * 0.55;
                    cv.text(label, r.x() + (r.width() - tw) * 0.5, r.y() + r.height() * 0.72, theme.font_size,
                        if *enabled { theme.text } else { theme.text_dim });
                }
                PanelRow::Toggle { label, on: t_on, anim, dir } => {
                    cv.text(label, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
                    let track = Rect::from_xywh(r.x() + r.width() - 34.0, r.y() + (r.height() - 14.0) * 0.5, 30.0, 14.0).expect("Panel toggle track");
                    let mut track_c = [theme.disabled[0], theme.disabled[1], theme.disabled[2], 255];
                    if *t_on {
                        track_c = [theme.accent[0], theme.accent[1], theme.accent[2], 255];
                    }
                    cv.fill(track, track_c);
                    let a = anim.clamp(0.0, 1.0);
                    let e = if *dir > 0.0 { ease_out_cubic(a) } else { 1.0 - ease_out_cubic(1.0 - a) };
                    let kx = track.left() + (track.width() - 10.0) * e;
                    cv.fill(Rect::from_xywh(kx, track.y() + 2.0, 10.0, track.height() - 4.0).expect("Panel toggle knob"), theme.knob);
                }
                PanelRow::Slider { value } => {
                    let v = value.clamp(0.0, 1.0);
                    let fx = r.width() * v;
                    if fx > 1.0 {
                        cv.fill(Rect::from_xywh(r.x(), r.y(), fx, r.height()).expect("Panel slider fill"), [theme.accent[0], theme.accent[1], theme.accent[2], 255]);
                    }
                    let kx = r.x() + (r.width() - 8.0) * v;
                    cv.fill(Rect::from_xywh(kx, r.y(), 8.0, r.height()).expect("Panel slider knob"), theme.knob);
                }
                PanelRow::Field { label, value, editing } => {
                    cv.text(label, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
                    let tw = value.len() as f32 * theme.font_size * 0.55;
                    cv.text(value, r.x() + r.width() - tw - theme.pad_x, r.y() + r.height() * 0.72, theme.font_size,
                        if *editing { theme.accent } else { theme.text_dim });
                }
            }
        }
    }
}

/// 可滚动列表:total 行中显示 visible 行,带右侧滚动条。
/// 行 id = 全局行号(从 scroll 起点计);滚动条 id = u32::MAX。
#[derive(Clone, Debug)]
pub struct ScrollList {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub total: usize,
    pub visible: usize,
    /// 当前滚动偏移(行数,0..=total-visible)。
    pub scroll: f32,
    pub row_h: f32,
    pub gap: f32,
    pub bar_w: f32,
    /// 每行显示文本(按全局行号取);空 = 显示行号。
    pub labels: Vec<String>,
    /// 选中行(全局行号,点击切换)。
    pub selected: Option<usize>,
}

impl ScrollList {
    pub fn new(x: f32, y: f32, w: f32, total: usize, visible: usize) -> Self {
        Self {
            x, y, w, total, visible: visible.max(1), scroll: 0.0,
            row_h: 22.0, gap: 4.0, bar_w: 8.0,
            labels: Vec::new(), selected: None,
        }
    }

    fn rows_h(&self) -> f32 {
        self.visible as f32 * self.row_h + (self.visible - 1) as f32 * self.gap
    }

    fn max_scroll(&self) -> f32 {
        (self.total.saturating_sub(self.visible)) as f32
    }

    /// 最大滚动偏移(公开访问,供宿主对齐滚动位置)。
    #[allow(dead_code)]
    pub fn max_scroll_pub(&self) -> f32 {
        self.max_scroll()
    }

    fn row_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + i as f32 * (self.row_h + self.gap), self.w - self.bar_w, self.row_h)
            .expect("ScrollList row rect")
    }

    fn bar_rect(&self) -> Rect {
        let rows_h = self.rows_h();
        let bh = rows_h * (self.visible as f32 / self.total.max(1) as f32).clamp(0.1, 1.0);
        let max = self.max_scroll();
        let by = if max > 0.0 { self.y + (rows_h - bh) * (self.scroll / max) } else { self.y };
        Rect::from_xywh(self.x + self.w - self.bar_w, by, self.bar_w, bh).expect("ScrollList bar rect")
    }
}

impl Widget for ScrollList {
    fn areas(&self) -> Vec<Area> {
        let mut out = Vec::with_capacity(self.visible + 1);
        let start = self.scroll.round() as usize;
        for i in 0..self.visible {
            out.push(Area {
                kind: AreaKind::ScrollRow,
                id: (start + i) as u32,
                rect: self.row_rect(i),
            });
        }
        out.push(Area { kind: AreaKind::ScrollBar, id: u32::MAX, rect: self.bar_rect() });
        out
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        let bar = Area { kind: AreaKind::ScrollBar, id: u32::MAX, rect: self.bar_rect() };
        if inside(p, bar.rect) {
            return Some(bar);
        }
        self.areas().into_iter().find(|a| a.kind == AreaKind::ScrollRow && inside(p, a.rect))
    }
    fn on_click(&mut self, p: (f32, f32)) {
        if let Some(a) = self.hit_area(p) {
            if a.kind == AreaKind::ScrollRow {
                self.selected = Some(a.id as usize);
            }
        }
    }
    /// 滚轮滚动列表(整行步进)。
    fn on_wheel(&mut self, dy: f32) {
        let max = self.max_scroll();
        if max <= 0.0 {
            return;
        }
        self.scroll = (self.scroll - dy).clamp(0.0, max);
    }
    /// 拖动滚动条:把滚动条中心对齐到指针 y。
    fn on_drag(&mut self, p: (f32, f32)) {
        let rows_h = self.rows_h();
        let max = self.max_scroll();
        if max <= 0.0 || rows_h <= 0.0 {
            return;
        }
        let bar = self.bar_rect();
        // 拖动时按指针相对轨道的位置,但保持滚动条中心跟随指针。
        let bh = bar.height();
        let travel = (rows_h - bh).max(1.0);
        let frac = ((p.1 - self.y) - bh * 0.5) / travel;
        self.scroll = (frac * max).clamp(0.0, max);
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let start = self.scroll.round() as usize;
        for i in 0..self.visible {
            let gi = start + i;
            let r = self.row_rect(i);
            let on = hover.map_or(false, |a| a.kind == AreaKind::ScrollRow && a.id == gi as u32);
            let sel = self.selected == Some(gi);
            cv.fill(r, if sel { theme.hover } else if on { theme.hover } else { theme.row });
            // 选中行左侧强调条
            if sel {
                cv.fill(Rect::from_xywh(r.x(), r.y(), 3.0, r.height()).expect("ScrollList sel bar"), [theme.accent[0], theme.accent[1], theme.accent[2], 255]);
            }
            let label = self.labels.get(gi).cloned().unwrap_or_else(|| gi.to_string());
            cv.text(&label, r.x() + theme.pad_x + if sel { 3.0 } else { 0.0 }, r.y() + theme.row_h * 0.72, theme.font_size,
                if sel { theme.text } else if on { theme.text } else { theme.text_dim });
        }
        let bar = self.bar_rect();
        let on = hover.map_or(false, |a| a.kind == AreaKind::ScrollBar);
        cv.fill(bar, if on { theme.hover } else { theme.knob });
    }
}

/// 下拉选择框:主按钮显示当前项,点击展开选项列表。
/// 主按钮 id=0;展开的选项 id = 1 + 索引。
#[derive(Clone, Debug)]
pub struct ComboBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub items: Vec<String>,
    pub selected: usize,
    /// 是否展开下拉列表(点击切换)。
    pub open: bool,
}

impl ComboBox {
    pub fn new(x: f32, y: f32, w: f32, h: f32, items: Vec<String>) -> Self {
        Self { x, y, w, h, items, selected: 0, open: false }
    }

    fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).expect("ComboBox rect")
    }

    fn item_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + self.h + i as f32 * self.h, self.w, self.h)
            .expect("ComboBox item rect")
    }
}

impl Widget for ComboBox {
    fn areas(&self) -> Vec<Area> {
        let mut out = vec![Area { kind: AreaKind::ComboBoxButton, id: 0, rect: self.rect() }];
        if self.open {
            for (i, _) in self.items.iter().enumerate() {
                out.push(Area { kind: AreaKind::ComboBoxItem, id: (i + 1) as u32, rect: self.item_rect(i) });
            }
        }
        out
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.areas().into_iter().find(|a| inside(p, a.rect))
    }
    fn on_click(&mut self, p: (f32, f32)) {
        // 点主按钮:切换展开;点选项:选中并收起。
        if inside(p, self.rect()) {
            self.open = !self.open;
            return;
        }
        if self.open {
            for (i, _) in self.items.iter().enumerate() {
                if inside(p, self.item_rect(i)) {
                    self.selected = i;
                    self.open = false;
                    return;
                }
            }
        }
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let on = hover.map_or(false, |a| a.kind == AreaKind::ComboBoxButton);
        cv.fill(self.rect(), if on { theme.hover } else { theme.row });
        let label = self.items.get(self.selected).cloned().unwrap_or_default();
        cv.text(&label, self.x + theme.pad_x, self.y + self.h * 0.72, theme.font_size, theme.text);
        // 右侧下拉箭头(用 "v" 字形)
        cv.text("v", self.x + self.w - theme.pad_x - 6.0, self.y + self.h * 0.72, theme.font_size, theme.text_dim);
    }
    /// 展开的选项列表画在 overlay 层,悬浮于其他组件之上。
    fn draw_overlay(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        if !self.open {
            return;
        }
        // 下拉层背景:比行色更实,盖住下方组件。
        cv.fill(self.rect(), theme.hover);
        for (i, item) in self.items.iter().enumerate() {
            let r = self.item_rect(i);
            let ion = hover.map_or(false, |a| a.kind == AreaKind::ComboBoxItem && a.id == (i + 1) as u32);
            cv.fill(r, if ion { theme.hover } else { theme.row });
            cv.text(item, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size,
                if i == self.selected { theme.accent } else { theme.text });
        }
    }
}

/// 数值步进器:label + value + [-][+] 按钮。
/// 数值区 id=0,减 id=1,加 id=2。
#[derive(Clone, Debug)]
pub struct Stepper {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub value: f64,
    pub step: f64,
    pub min: f64,
    pub max: f64,
}

impl Stepper {
    pub fn new(x: f32, y: f32, w: f32, h: f32, label: impl Into<String>, value: f64) -> Self {
        Self { x, y, w, h, label: label.into(), value, step: 1.0, min: 0.0, max: 100.0 }
    }

    fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).expect("Stepper rect")
    }

    fn btn_w(&self) -> f32 {
        (self.h * 1.2).min(28.0)
    }

    fn minus_rect(&self) -> Rect {
        let bw = self.btn_w();
        Rect::from_xywh(self.x + self.w - bw * 2.0 - 4.0, self.y, bw, self.h).expect("Stepper minus")
    }

    fn plus_rect(&self) -> Rect {
        let bw = self.btn_w();
        Rect::from_xywh(self.x + self.w - bw, self.y, bw, self.h).expect("Stepper plus")
    }
}

impl Widget for Stepper {
    fn areas(&self) -> Vec<Area> {
        vec![
            Area { kind: AreaKind::StepperField, id: 0, rect: self.rect() },
            Area { kind: AreaKind::StepperMinus, id: 1, rect: self.minus_rect() },
            Area { kind: AreaKind::StepperPlus, id: 2, rect: self.plus_rect() },
        ]
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        // 子区域(减/加按钮)优先于整行。
        if inside(p, self.minus_rect()) {
            return Some(Area { kind: AreaKind::StepperMinus, id: 1, rect: self.minus_rect() });
        }
        if inside(p, self.plus_rect()) {
            return Some(Area { kind: AreaKind::StepperPlus, id: 2, rect: self.plus_rect() });
        }
        inside(p, self.rect()).then(|| Area { kind: AreaKind::StepperField, id: 0, rect: self.rect() })
    }
    fn on_click(&mut self, p: (f32, f32)) {
        if inside(p, self.minus_rect()) {
            self.value = (self.value - self.step).clamp(self.min, self.max);
        } else if inside(p, self.plus_rect()) {
            self.value = (self.value + self.step).clamp(self.min, self.max);
        }
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let on = hover.map_or(false, |a| a.kind == AreaKind::StepperField);
        cv.fill(self.rect(), if on { theme.hover } else { theme.row });
        cv.text(&self.label, self.x + theme.pad_x, self.y + self.h * 0.72, theme.font_size, theme.text);
        let vtext = format!("{:.2}", self.value);
        let tw = vtext.len() as f32 * theme.font_size * 0.55;
        cv.text(&vtext, self.minus_rect().x() - tw - theme.pad_x, self.y + self.h * 0.72, theme.font_size, theme.text_dim);
        for (rect, kind, sym) in [(self.minus_rect(), AreaKind::StepperMinus, "-"), (self.plus_rect(), AreaKind::StepperPlus, "+")] {
            let on = hover.map_or(false, |a| a.kind == kind);
            cv.fill(rect, if on { theme.hover } else { theme.row });
            cv.text(sym, rect.x() + (rect.width() - theme.font_size * 0.55) * 0.5, rect.y() + rect.height() * 0.72,
                theme.font_size, theme.text);
        }
    }
}

/// 复选框:勾选框 + label。
#[derive(Clone, Debug)]
pub struct Checkbox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub checked: bool,
}

impl Checkbox {
    pub fn new(x: f32, y: f32, w: f32, h: f32, label: impl Into<String>, checked: bool) -> Self {
        Self { x, y, w, h, label: label.into(), checked }
    }

    fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).expect("Checkbox rect")
    }

    /// 左侧勾选框(18px)。
    fn box_rect(&self) -> Rect {
        let s = self.h.min(18.0);
        Rect::from_xywh(self.x + 8.0, self.y + (self.h - s) * 0.5, s, s).expect("Checkbox box")
    }
}

impl Widget for Checkbox {
    fn areas(&self) -> Vec<Area> {
        vec![
            Area { kind: AreaKind::Checkbox, id: 0, rect: self.rect() },
            Area { kind: AreaKind::CheckboxBox, id: 1, rect: self.box_rect() },
        ]
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        // 勾选框子区域优先。
        if inside(p, self.box_rect()) {
            return Some(Area { kind: AreaKind::CheckboxBox, id: 1, rect: self.box_rect() });
        }
        inside(p, self.rect()).then(|| Area { kind: AreaKind::Checkbox, id: 0, rect: self.rect() })
    }
    fn on_click(&mut self, _p: (f32, f32)) {
        self.checked = !self.checked;
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let on = hover.map_or(false, |a| a.kind == AreaKind::Checkbox);
        cv.fill(self.rect(), if on { theme.hover } else { theme.row });
        let b = self.box_rect();
        cv.fill(b, if self.checked { [theme.accent[0], theme.accent[1], theme.accent[2], 255] } else { theme.disabled });
        if self.checked {
            // 简单对勾
            cv.text("x", b.x() + 4.0, b.y() + b.height() * 0.78, theme.font_size, [theme.knob[0], theme.knob[1], theme.knob[2]]);
        }
        cv.text(&self.label, self.x + theme.pad_x * 2.0 + b.width(), self.y + self.h * 0.72, theme.font_size, theme.text);
    }
}

/// 标签页:等宽 tab 排,选中高亮。
#[derive(Clone, Debug)]
pub struct TabBar {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub tabs: Vec<String>,
    pub selected: usize,
}

impl TabBar {
    pub fn new(x: f32, y: f32, w: f32, h: f32, tabs: Vec<String>) -> Self {
        Self { x, y, w, h, tabs, selected: 0 }
    }

    fn tab_w(&self) -> f32 {
        (self.w / self.tabs.len().max(1) as f32).max(1.0)
    }

    fn tab_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x + i as f32 * self.tab_w(), self.y, self.tab_w(), self.h)
            .expect("TabBar tab rect")
    }
}

impl Widget for TabBar {
    fn areas(&self) -> Vec<Area> {
        (0..self.tabs.len())
            .map(|i| Area { kind: AreaKind::TabBarTab, id: i as u32, rect: self.tab_rect(i) })
            .collect()
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.areas().into_iter().find(|a| inside(p, a.rect))
    }
    fn on_click(&mut self, p: (f32, f32)) {
        for (i, _) in self.tabs.iter().enumerate() {
            if inside(p, self.tab_rect(i)) {
                self.selected = i;
                return;
            }
        }
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        for (i, tab) in self.tabs.iter().enumerate() {
            let r = self.tab_rect(i);
            let sel = i == self.selected;
            let on = hover.map_or(false, |a| a.kind == AreaKind::TabBarTab && a.id == i as u32);
            cv.fill(r, if sel { [theme.accent[0], theme.accent[1], theme.accent[2], 255] } else if on { theme.hover } else { theme.row });
            let tw = tab.len() as f32 * theme.font_size * 0.55;
            cv.text(tab, r.x() + (r.width() - tw) * 0.5, r.y() + r.height() * 0.72, theme.font_size,
                if sel { [theme.knob[0], theme.knob[1], theme.knob[2]] } else { theme.text });
        }
    }
}

/// 进度条:轨道 + 填充 + 文本,可点击设置进度(如 seek bar)。
#[derive(Clone, Debug)]
pub struct ProgressBar {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// 0..1
    pub progress: f32,
    pub label: String,
}

impl ProgressBar {
    pub fn new(x: f32, y: f32, w: f32, h: f32, label: impl Into<String>, progress: f32) -> Self {
        Self { x, y, w, h, progress: progress.clamp(0.0, 1.0), label: label.into() }
    }

    fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).expect("ProgressBar rect")
    }

    fn fill_rect(&self) -> Rect {
        let r = self.rect();
        Rect::from_xywh(r.x(), r.y(), r.width() * self.progress, r.height()).expect("ProgressBar fill")
    }
}

impl Widget for ProgressBar {
    fn areas(&self) -> Vec<Area> {
        vec![Area { kind: AreaKind::ProgressBar, id: 0, rect: self.rect() }]
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        inside(p, self.rect()).then(|| Area { kind: AreaKind::ProgressBar, id: 0, rect: self.rect() })
    }
    fn on_click(&mut self, p: (f32, f32)) {
        let r = self.rect();
        self.progress = ((p.0 - r.left()) / r.width()).clamp(0.0, 1.0);
    }
    fn on_drag(&mut self, p: (f32, f32)) {
        self.on_click(p);
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let r = self.rect();
        cv.fill(r, theme.row);
        if self.progress > 0.01 {
            cv.fill(self.fill_rect(), [theme.accent[0], theme.accent[1], theme.accent[2], 255]);
        }
        let on = hover.is_some();
        cv.text(&self.label, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size,
            if on { theme.text } else { [theme.knob[0], theme.knob[1], theme.knob[2]] });
    }
}

/// 键值网格行的编辑类型(PMCORE-23):None = 只读。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridFieldKind {
    /// 自由字符串(Char/Backspace/方向键 + Enter 提交)。
    Text,
    /// f64 数值(打字 → 缓冲,Enter 解析 + clamp 提交)。
    Number,
}

/// 键值网格:key 左、value 右的多行属性(可选中行)。
/// PMCORE-23:行可标记为可编辑(`edit_kind`),点击进入输入、Enter 提交;
/// 提交后置 `committed = Some(行索引)`,由宿主读取 `rows[行].1` 并清空。
#[derive(Clone, Debug)]
pub struct KeyValueGrid {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub rows: Vec<(String, String)>,
    pub row_h: f32,
    pub gap: f32,
    /// 选中行索引(点击切换)。
    pub selected: Option<usize>,
    /// 标题(显示在首行上方);空 = 无标题。
    pub title: String,
    /// PMCORE-23:每行编辑类型;与 rows 等长(不足视为只读)。
    pub edit_kind: Vec<Option<GridFieldKind>>,
    /// 正在编辑的行(进入输入态后值来源为 buf/num_buf)。
    pub editing: Option<usize>,
    /// Text 编辑缓冲(与行值同源,Enter 提交后写回 rows)。
    pub buf: String,
    /// Text 光标插入位置(字符索引)。
    pub insert: usize,
    /// Number 编辑缓冲(Form 同语义:Enter 解析+clamp,Esc 取消)。
    pub num_buf: Option<String>,
    /// Number 提交钳制范围。
    pub num_min: f64,
    pub num_max: f64,
    /// 光标闪烁相位(update 推进)。
    pub caret: f32,
    /// Enter 提交后置 Some(行索引);宿主消费后置 None。
    pub committed: Option<usize>,
}

impl KeyValueGrid {
    pub fn new(x: f32, y: f32, w: f32, rows: Vec<(String, String)>) -> Self {
        Self {
            x, y, w, rows, row_h: 22.0, gap: 4.0, selected: None, title: String::new(),
            edit_kind: Vec::new(), editing: None, buf: String::new(), insert: 0,
            num_buf: None, num_min: 0.0, num_max: 9999.0, caret: 0.0, committed: None,
        }
    }

    fn editable(&self, i: usize) -> bool {
        self.edit_kind.get(i).copied().flatten().is_some()
    }

    /// 进入第 `i` 行编辑:以当前行值初始化缓冲(Number 行缓冲预填当前值,
    /// Backspace 可清空重输)。
    fn start_edit(&mut self, i: usize) {
        let value = self.rows.get(i).map(|r| r.1.clone()).unwrap_or_default();
        self.buf = value;
        self.insert = self.buf.chars().count();
        self.num_buf = if self.edit_kind.get(i) == Some(&Some(GridFieldKind::Number)) {
            Some(self.buf.clone())
        } else {
            None
        };
        self.caret = 0.0;
        self.editing = Some(i);
    }

    fn title_h(&self) -> f32 {
        if self.title.is_empty() { 0.0 } else { self.row_h }
    }

    fn row_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + self.title_h() + i as f32 * (self.row_h + self.gap), self.w, self.row_h)
            .expect("KeyValueGrid row rect")
    }

    /// 编辑态的值区绘制:缓冲 + 闪烁光标(Text 光标停在 insert 后)。
    fn draw_edit_value(&self, cv: &mut dyn Canvas, theme: &Theme, i: usize, r: Rect) {
        let is_num = self.edit_kind.get(i) == Some(&Some(GridFieldKind::Number));
        let (shown, caret_idx): (&str, usize) = if is_num {
            // Number 光标固定缓冲尾(与 Form 一致)。
            (self.num_buf.as_deref().unwrap_or(""), usize::MAX)
        } else {
            (&self.buf, self.insert.min(self.buf.chars().count()))
        };
        let tw = cv.text_width(shown, theme.font_size);
        let vx = (r.x() + r.width() - tw - theme.pad_x).max(r.x() + theme.pad_x);
        cv.text(shown, vx, r.y() + r.height() * 0.72, theme.font_size, theme.accent);
        if (self.caret * 2.0) as i32 % 2 == 0 {
            let before: String = shown.chars().take(caret_idx).collect();
            let btw = cv.text_width(&before, theme.font_size);
            cv.fill(
                Rect::from_xywh(vx + btw + 1.0, r.y() + 3.0, 2.0, r.height() - 6.0).expect("KeyValueGrid caret"),
                [theme.accent[0], theme.accent[1], theme.accent[2], 255],
            );
        }
    }
}

impl Widget for KeyValueGrid {
    fn areas(&self) -> Vec<Area> {
        (0..self.rows.len())
            .map(|i| Area { kind: AreaKind::GridRow, id: i as u32, rect: self.row_rect(i) })
            .collect()
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.areas().into_iter().find(|a| inside(p, a.rect))
    }
    fn on_click(&mut self, p: (f32, f32)) {
        self.committed = None;
        if let Some(a) = self.hit_area(p) {
            let i = a.id as usize;
            self.selected = Some(i);
            if self.editing == Some(i) {
                return; // 已在该行编辑,点击不重置缓冲
            }
            if self.editable(i) {
                self.start_edit(i);
            } else {
                self.editing = None; // 点击只读行/空白:退出编辑
            }
        } else {
            self.editing = None;
        }
    }
    fn on_key(&mut self, k: WidgetKey) {
        let Some(i) = self.editing else { return };
        let is_num = self.edit_kind.get(i) == Some(&Some(GridFieldKind::Number));
        if is_num {
            match k {
                WidgetKey::Char(c) if c.is_ascii_digit() || c == '.' || c == '-' => {
                    let b = self.num_buf.get_or_insert_with(|| {
                        let cur = self.rows.get(i).map(|r| r.1.clone()).unwrap_or_default();
                        format!("{:.1}", cur.parse::<f64>().unwrap_or(0.0))
                    });
                    b.push(c);
                }
                WidgetKey::Backspace => {
                    if let Some(b) = &mut self.num_buf {
                        b.pop();
                    }
                }
                WidgetKey::Enter => {
                    let fallback = self.rows.get(i).and_then(|r| r.1.parse::<f64>().ok()).unwrap_or(0.0);
                    let value = self
                        .num_buf
                        .take()
                        .and_then(|b| b.parse::<f64>().ok())
                        .map(|v| v.clamp(self.num_min, self.num_max))
                        .unwrap_or(fallback);
                    let s = format!("{value:.1}");
                    if let Some(r) = self.rows.get_mut(i) {
                        r.1 = s;
                    }
                    self.committed = Some(i);
                    self.editing = None;
                }
                WidgetKey::Escape => {
                    self.num_buf = None;
                    self.editing = None;
                }
                _ => {}
            }
            return;
        }
        // Text 行
        match k {
            WidgetKey::Char(c) => {
                let ins = self.insert.min(self.buf.chars().count());
                self.buf.insert(ins, c);
                self.insert = ins + 1;
            }
            WidgetKey::Backspace => {
                let ins = self.insert.min(self.buf.chars().count());
                if ins > 0 {
                    self.buf.remove(ins - 1);
                    self.insert = ins - 1;
                }
            }
            WidgetKey::Enter => {
                let s = self.buf.clone();
                if let Some(r) = self.rows.get_mut(i) {
                    r.1 = s;
                }
                self.committed = Some(i);
                self.editing = None;
            }
            WidgetKey::Escape => {
                self.editing = None;
            }
            WidgetKey::Left => self.insert = self.insert.saturating_sub(1),
            WidgetKey::Right => self.insert = (self.insert + 1).min(self.buf.chars().count()),
            WidgetKey::Home => self.insert = 0,
            WidgetKey::End => self.insert = self.buf.chars().count(),
            _ => {}
        }
    }
    fn update(&mut self, dt: f32) {
        if self.editing.is_some() {
            self.caret += dt;
        }
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        if !self.title.is_empty() {
            let tr = Rect::from_xywh(self.x, self.y, self.w, self.row_h).expect("KeyValueGrid title rect");
            cv.fill(tr, theme.bg);
            cv.text(&self.title, self.x + theme.pad_x, self.y + self.row_h * 0.72, theme.font_size, theme.title);
        }
        for (i, (k, v)) in self.rows.iter().enumerate() {
            let r = self.row_rect(i);
            let on = hover.map_or(false, |a| a.kind == AreaKind::GridRow && a.id == i as u32);
            let sel = self.selected == Some(i);
            let editing = self.editing == Some(i);
            cv.fill(r, if sel || on || editing { theme.hover } else { theme.row });
            cv.text(k, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
            if editing {
                self.draw_edit_value(cv, theme, i, r);
            } else {
                let tw = v.len() as f32 * theme.font_size * 0.55;
                cv.text(v, r.x() + r.width() - tw - theme.pad_x, r.y() + r.height() * 0.72, theme.font_size,
                    if sel { theme.accent } else if self.editable(i) { theme.text } else { theme.text_dim });
            }
        }
    }
}

/// 文本输入框:聚焦后可接收键盘字符。
/// 支持光标移动(←/→/Home/End)、插入位置编辑。
#[derive(Clone, Debug)]
pub struct TextInput {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub text: String,
    pub placeholder: String,
    pub focused: bool,
    /// 光标闪烁相位(update 推进)。
    pub caret: f32,
    /// 光标插入位置(字符索引)。
    pub insert: usize,
}

impl TextInput {
    pub fn new(x: f32, y: f32, w: f32, h: f32, placeholder: impl Into<String>) -> Self {
        Self { x, y, w, h, text: String::new(), placeholder: placeholder.into(), focused: false, caret: 0.0, insert: 0 }
    }

    fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).expect("TextInput rect")
    }
}

impl Widget for TextInput {
    fn areas(&self) -> Vec<Area> {
        vec![Area { kind: AreaKind::TextInput, id: 0, rect: self.rect() }]
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        inside(p, self.rect()).then(|| Area { kind: AreaKind::TextInput, id: 0, rect: self.rect() })
    }
    fn on_click(&mut self, _p: (f32, f32)) {
        // 无度量访问,保持现状;宿主需要光标定位时调用 on_click_with_measure。
    }
    fn on_click_with_measure(&mut self, p: (f32, f32), pad_x: f32, measure: &dyn Fn(&str) -> f32) {
        // 点击定位光标:按 text_width 逐前缀测量最近字符边界(与 draw 光标同源)。
        // 空文本/placeholder 态 → insert=0;超宽文本点击最右 → len。
        self.focused = true;
        self.caret = 0.0;
        self.insert = caret_from_x(&self.text, p.0 - (self.x + pad_x), self.w - pad_x, measure);
    }
    fn on_text(&mut self, c: char) {
        if self.focused {
            let i = self.insert.min(self.text.chars().count());
            self.text.insert(i, c);
            self.insert = i + 1;
        }
    }
    fn on_backspace(&mut self) {
        if !self.focused || self.insert == 0 {
            return;
        }
        let i = self.insert.min(self.text.chars().count());
        if i > 0 {
            self.text.remove(i - 1);
            self.insert = i - 1;
        }
    }
    fn on_key(&mut self, k: WidgetKey) {
        if !self.focused {
            return;
        }
        match k {
            WidgetKey::Left => self.insert = self.insert.saturating_sub(1),
            WidgetKey::Right => self.insert = (self.insert + 1).min(self.text.chars().count()),
            WidgetKey::Home => self.insert = 0,
            WidgetKey::End => self.insert = self.text.chars().count(),
            _ => {}
        }
    }
    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        self.caret = 0.0;
    }
    fn update(&mut self, dt: f32) {
        if self.focused {
            self.caret += dt;
        }
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let on = hover.map_or(false, |a| a.kind == AreaKind::TextInput);
        cv.fill(self.rect(), if on { theme.hover } else { theme.row });
        // 聚焦时描边提示
        if self.focused {
            cv.fill(Rect::from_xywh(self.x, self.y, self.w, 2.0).expect("focus top"), [theme.accent[0], theme.accent[1], theme.accent[2], 255]);
            cv.fill(Rect::from_xywh(self.x, self.y + self.h - 2.0, self.w, 2.0).expect("focus bottom"), [theme.accent[0], theme.accent[1], theme.accent[2], 255]);
        }
        if self.text.is_empty() && !self.focused {
            cv.text(&self.placeholder, self.x + theme.pad_x, self.y + self.h * 0.72, theme.font_size, theme.text_dim);
            return;
        }
        cv.text(&self.text, self.x + theme.pad_x, self.y + self.h * 0.72, theme.font_size, theme.text);
        // 光标:闪烁竖线,精确停在 insert 字符后(text_width 真实度量)。
        if self.focused && (self.caret * 2.0) as i32 % 2 == 0 {
            let before: String = self.text.chars().take(self.insert.min(self.text.chars().count())).collect();
            let tw = cv.text_width(&before, theme.font_size);
            cv.fill(Rect::from_xywh(self.x + theme.pad_x + tw + 1.0, self.y + 3.0, 2.0, self.h - 6.0).expect("caret"), [theme.text[0], theme.text[1], theme.text[2], 255]);
        }
    }
}

/// 拖动数值框:按住左右拖动改值(Blender 式),也可点击定位。
#[derive(Clone, Debug)]
pub struct DragValue {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub value: f64,
    /// 每像素增量(拖动灵敏度)。
    pub speed: f64,
    pub min: f64,
    pub max: f64,
    /// 上次指针 x(计算拖动增量)。
    last_x: f32,
    /// 是否正在拖。
    pub dragging: bool,
}

impl DragValue {
    pub fn new(x: f32, y: f32, w: f32, h: f32, label: impl Into<String>, value: f64) -> Self {
        Self { x, y, w, h, label: label.into(), value, speed: 0.1, min: 0.0, max: 100.0, last_x: 0.0, dragging: false }
    }

    fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).expect("DragValue rect")
    }
}

impl Widget for DragValue {
    fn areas(&self) -> Vec<Area> {
        vec![Area { kind: AreaKind::DragValue, id: 0, rect: self.rect() }]
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        inside(p, self.rect()).then(|| Area { kind: AreaKind::DragValue, id: 0, rect: self.rect() })
    }
    fn on_click(&mut self, p: (f32, f32)) {
        self.last_x = p.0;
        self.dragging = true;
    }
    fn on_drag(&mut self, p: (f32, f32)) {
        let dx = p.0 - self.last_x;
        self.last_x = p.0;
        self.value = (self.value + dx as f64 * self.speed).clamp(self.min, self.max);
        self.dragging = true;
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let on = hover.map_or(false, |a| a.kind == AreaKind::DragValue);
        cv.fill(self.rect(), if self.dragging || on { theme.hover } else { theme.row });
        cv.text(&self.label, self.x + theme.pad_x, self.y + self.h * 0.72, theme.font_size, theme.text);
        let vtext = format!("{:.2}", self.value);
        let tw = vtext.len() as f32 * theme.font_size * 0.55;
        cv.text(&vtext, self.x + self.w - tw - theme.pad_x, self.y + self.h * 0.72, theme.font_size,
            if self.dragging { theme.accent } else { theme.text_dim });
        // 拖动提示:行内小滑块标记(显示当前比例)
        if self.dragging {
            let frac = ((self.value - self.min) / (self.max - self.min).max(1e-9)) as f32;
            let kx = self.x + theme.pad_x * 2.0 + frac * (self.w - theme.pad_x * 4.0);
            cv.fill(Rect::from_xywh(kx, self.y + 2.0, 2.0, self.h - 4.0).expect("drag marker"), theme.knob);
        }
    }
}

/// 颜色通道行(三个 Slider 式行)+ 预览块。
/// 行 id = 0..2(R/G/B),预览 id = 3。
#[derive(Clone, Debug)]
pub struct ColorPicker {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub row_h: f32,
    pub gap: f32,
    /// [r, g, b] ∈ [0, 1]
    pub rgb: [f32; 3],
}

impl ColorPicker {
    pub fn new(x: f32, y: f32, w: f32, rgb: [f32; 3]) -> Self {
        Self { x, y, w, row_h: 22.0, gap: 4.0, rgb }
    }

    fn row_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + i as f32 * (self.row_h + self.gap), self.w, self.row_h)
            .expect("ColorPicker row rect")
    }

    fn preview_rect(&self) -> Rect {
        let last = self.row_rect(2);
        Rect::from_xywh(self.x + self.w - 34.0, last.y() + self.row_h + self.gap, 30.0, 18.0)
            .expect("ColorPicker preview")
    }

    fn channel_rect(&self, i: usize) -> Rect {
        let r = self.row_rect(i);
        Rect::from_xywh(r.x() + 40.0, r.y(), r.width() - 40.0, r.height())
            .expect("ColorPicker channel")
    }
}

impl Widget for ColorPicker {
    fn areas(&self) -> Vec<Area> {
        let mut out = Vec::new();
        for i in 0..3 {
            out.push(Area { kind: AreaKind::ColorChannel, id: i as u32, rect: self.row_rect(i) });
        }
        out.push(Area { kind: AreaKind::ColorPreview, id: 3, rect: self.preview_rect() });
        out
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.areas().into_iter().find(|a| inside(p, a.rect))
    }
    fn on_click(&mut self, p: (f32, f32)) {
        for i in 0..3 {
            if inside(p, self.channel_rect(i)) {
                let r = self.channel_rect(i);
                self.rgb[i] = ((p.0 - r.left()) / r.width()).clamp(0.0, 1.0);
            }
        }
    }
    fn on_drag(&mut self, p: (f32, f32)) {
        self.on_click(p);
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let names = ["R", "G", "B"];
        for i in 0..3 {
            let r = self.row_rect(i);
            let on = hover.map_or(false, |a| a.kind == AreaKind::ColorChannel && a.id == i as u32);
            cv.fill(r, if on { theme.hover } else { theme.row });
            cv.text(names[i], r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
            let ch = self.channel_rect(i);
            cv.fill(ch, theme.row);
            let v = self.rgb[i];
            let fw = ch.width() * v;
            if fw > 1.0 {
                // 通道色(单通道亮色)
                let mut c = [0u8; 4];
                if i == 0 { c = [255, 60, 60, 255]; }
                if i == 1 { c = [60, 255, 60, 255]; }
                if i == 2 { c = [60, 120, 255, 255]; }
                cv.fill(Rect::from_xywh(ch.x(), ch.y(), fw, ch.height()).expect("ch fill"), c);
            }
            let kx = ch.x() + (ch.width() - 8.0) * v;
            cv.fill(Rect::from_xywh(kx, ch.y(), 8.0, ch.height()).expect("ch knob"), theme.knob);
        }
        // 预览块:当前混合色
        let p = self.preview_rect();
        cv.fill(p, [(self.rgb[0] * 255.0) as u8, (self.rgb[1] * 255.0) as u8, (self.rgb[2] * 255.0) as u8, 255]);
        let on = hover.map_or(false, |a| a.kind == AreaKind::ColorPreview);
        if on {
            cv.fill(p, theme.hover);
        }
        let tw = "preview".len() as f32 * theme.font_size * 0.55;
        cv.text("preview", p.x() - tw - theme.pad_x, p.y() + p.height() * 0.72, theme.font_size, theme.text_dim);
    }
}

/// 多选列表:点击行切换选中,行首显示勾选标记。
#[derive(Clone, Debug)]
pub struct ListBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub items: Vec<String>,
    pub selected: Vec<usize>,
    pub row_h: f32,
    pub gap: f32,
}

impl ListBox {
    pub fn new(x: f32, y: f32, w: f32, items: Vec<String>) -> Self {
        Self { x, y, w, items, selected: Vec::new(), row_h: 22.0, gap: 4.0 }
    }

    fn row_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + i as f32 * (self.row_h + self.gap), self.w, self.row_h)
            .expect("ListBox row rect")
    }
}

impl Widget for ListBox {
    fn areas(&self) -> Vec<Area> {
        (0..self.items.len())
            .map(|i| Area { kind: AreaKind::ListBoxRow, id: i as u32, rect: self.row_rect(i) })
            .collect()
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.areas().into_iter().find(|a| inside(p, a.rect))
    }
    fn on_click(&mut self, p: (f32, f32)) {
        let Some(a) = self.hit_area(p) else { return };
        let i = a.id as usize;
        if let Some(pos) = self.selected.iter().position(|&s| s == i) {
            self.selected.remove(pos);
        } else {
            self.selected.push(i);
        }
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        for (i, item) in self.items.iter().enumerate() {
            let r = self.row_rect(i);
            let sel = self.selected.contains(&i);
            let on = hover.map_or(false, |a| a.kind == AreaKind::ListBoxRow && a.id == i as u32);
            cv.fill(r, if sel { theme.hover } else if on { theme.hover } else { theme.row });
            // 行首勾选标记
            let box_rect = Rect::from_xywh(r.x() + theme.pad_x, r.y() + 3.0, r.height() - 6.0, r.height() - 6.0).expect("ListBox box");
            cv.fill(box_rect, if sel { [theme.accent[0], theme.accent[1], theme.accent[2], 255] } else { theme.disabled });
            if sel {
                cv.text("x", box_rect.x() + 3.0, box_rect.y() + box_rect.height() * 0.78, theme.font_size, [theme.knob[0], theme.knob[1], theme.knob[2]]);
            }
            cv.text(item, r.x() + theme.pad_x * 2.0 + box_rect.width(), r.y() + r.height() * 0.72, theme.font_size,
                if sel { theme.text } else { theme.text_dim });
        }
    }
}

/// 互斥单选组:点击选项选中,同组至多一个选中(设置表单用)。
/// 行 id = 行索引,`AreaKind::RadioRow`。
#[derive(Clone, Debug)]
pub struct RadioGroup {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub rows: Vec<(String, bool)>,
    pub gap: f32,
}

impl RadioGroup {
    pub fn new(x: f32, y: f32, w: f32, h: f32, rows: Vec<String>, selected: usize) -> Self {
        let sel = selected.min(rows.len().saturating_sub(1));
        Self {
            x, y, w, h, gap: 4.0,
            rows: rows.into_iter().enumerate().map(|(i, label)| (label, i == sel)).collect(),
        }
    }

    /// 当前选中行索引(不变式:恒有且仅有一行选中)。
    pub fn selected(&self) -> usize {
        self.rows.iter().position(|(_, s)| *s).unwrap_or(0)
    }

    fn row_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + i as f32 * (self.h + self.gap), self.w, self.h)
            .expect("RadioGroup row rect")
    }

    /// 点击选择;返回选中是否发生变化(供调用方判断变更)。
    pub fn click(&mut self, p: (f32, f32)) -> bool {
        let Some(a) = self.hit_area(p) else { return false };
        let i = a.id as usize;
        if self.selected() == i {
            return false;
        }
        for (_, s) in self.rows.iter_mut() {
            *s = false;
        }
        self.rows[i].1 = true;
        true
    }
}

impl Widget for RadioGroup {
    fn areas(&self) -> Vec<Area> {
        (0..self.rows.len())
            .map(|i| Area { kind: AreaKind::RadioRow, id: i as u32, rect: self.row_rect(i) })
            .collect()
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.areas().into_iter().find(|a| inside(p, a.rect))
    }
    fn on_click(&mut self, p: (f32, f32)) {
        self.click(p);
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        for (i, (label, sel)) in self.rows.iter().enumerate() {
            let r = self.row_rect(i);
            let on = hover.map_or(false, |a| a.kind == AreaKind::RadioRow && a.id == i as u32);
            cv.fill(r, if *sel || on { theme.hover } else { theme.row });
            // 圆圈:外框 + 选中时实心内点(与勾选框的 "x" 区分)。
            let box_rect = Rect::from_xywh(r.x() + theme.pad_x, r.y() + 3.0, r.height() - 6.0, r.height() - 6.0)
                .expect("RadioGroup box");
            cv.fill(box_rect, theme.disabled);
            if *sel {
                let dot = Rect::from_xywh(box_rect.x() + 3.0, box_rect.y() + 3.0, box_rect.width() - 6.0, box_rect.height() - 6.0)
                    .expect("RadioGroup dot");
                cv.fill(dot, [theme.accent[0], theme.accent[1], theme.accent[2], 255]);
            }
            cv.text(label, r.x() + theme.pad_x * 2.0 + box_rect.width(), r.y() + r.height() * 0.72, theme.font_size,
                if *sel { theme.text } else { theme.text_dim });
        }
    }
}

/// 多选组:每行独立勾选,点击切换(可全选/全不选)。
/// 行 id = 行索引;复用 `AreaKind::Checkbox` 语义(行命中即切换)。
#[derive(Clone, Debug)]
pub struct CheckboxGroup {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub rows: Vec<(String, bool)>,
    pub gap: f32,
}

impl CheckboxGroup {
    pub fn new(x: f32, y: f32, w: f32, h: f32, rows: Vec<String>, checked: Vec<usize>) -> Self {
        Self {
            x, y, w, h, gap: 4.0,
            rows: rows.into_iter().enumerate()
                .map(|(i, label)| (label, checked.contains(&i)))
                .collect(),
        }
    }

    /// 勾选的行索引(升序)。
    pub fn checked(&self) -> Vec<usize> {
        self.rows.iter().enumerate().filter(|(_, (_, c))| *c).map(|(i, _)| i).collect()
    }

    /// 切换第 `idx` 行的勾选状态(越界忽略)。
    pub fn toggle(&mut self, idx: usize) {
        if let Some((_, c)) = self.rows.get_mut(idx) {
            *c = !*c;
        }
    }

    fn row_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + i as f32 * (self.h + self.gap), self.w, self.h)
            .expect("CheckboxGroup row rect")
    }
}

impl Widget for CheckboxGroup {
    fn areas(&self) -> Vec<Area> {
        (0..self.rows.len())
            .map(|i| Area { kind: AreaKind::Checkbox, id: i as u32, rect: self.row_rect(i) })
            .collect()
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.areas().into_iter().find(|a| inside(p, a.rect))
    }
    fn on_click(&mut self, p: (f32, f32)) {
        if let Some(a) = self.hit_area(p) {
            self.toggle(a.id as usize);
        }
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        for (i, (label, sel)) in self.rows.iter().enumerate() {
            let r = self.row_rect(i);
            let on = hover.map_or(false, |a| a.kind == AreaKind::Checkbox && a.id == i as u32);
            cv.fill(r, if *sel || on { theme.hover } else { theme.row });
            let box_rect = Rect::from_xywh(r.x() + theme.pad_x, r.y() + 3.0, r.height() - 6.0, r.height() - 6.0)
                .expect("CheckboxGroup box");
            cv.fill(box_rect, if *sel { [theme.accent[0], theme.accent[1], theme.accent[2], 255] } else { theme.disabled });
            if *sel {
                cv.text("x", box_rect.x() + 3.0, box_rect.y() + box_rect.height() * 0.78, theme.font_size,
                    [theme.knob[0], theme.knob[1], theme.knob[2]]);
            }
            cv.text(label, r.x() + theme.pad_x * 2.0 + box_rect.width(), r.y() + r.height() * 0.72, theme.font_size,
                if *sel { theme.text } else { theme.text_dim });
        }
    }
}

/// 可搜索下拉:ComboBox + 顶部搜索输入行。输入按大小写不敏感子串过滤选项,
/// 方向键在高亮项间移动,Enter 选中(原始索引),Esc/失焦关闭。
/// 收起时主按钮为 `ComboBoxButton`;展开时搜索行为 `TextInput`(宿主焦点
/// 系统据此给它键盘),选项行为 `ComboBoxItem`,id = 1 + 原始索引。
#[derive(Clone, Debug)]
pub struct SearchableCombo {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub items: Vec<String>,
    /// 已选中的原始索引(未选为 None)。
    pub selected: Option<usize>,
    pub open: bool,
    pub focused: bool,
    /// 搜索词。
    pub filter: String,
    /// 过滤后列表中高亮项下标(0..可见数)。
    pub highlighted: usize,
}

impl SearchableCombo {
    pub fn new(x: f32, y: f32, w: f32, h: f32, items: Vec<String>) -> Self {
        Self {
            x, y, w, h, items, selected: None,
            open: false, focused: false, filter: String::new(), highlighted: 0,
        }
    }

    /// 当前选中项原始索引(未选 None)。
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).expect("SearchableCombo rect")
    }

    fn option_rect(&self, vi: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + self.h + vi as f32 * self.h, self.w, self.h)
            .expect("SearchableCombo option rect")
    }

    /// 过滤后可见项的原始索引(保持原顺序)。空过滤 = 全部。
    fn visible(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        self.items.iter().enumerate()
            .filter(|(_, it)| f.is_empty() || it.to_lowercase().contains(&f))
            .map(|(i, _)| i)
            .collect()
    }

    fn open_up(&mut self) {
        self.open = true;
        self.focused = true;
        self.filter.clear();
        self.highlighted = 0;
    }

    fn close_down(&mut self) {
        self.open = false;
        self.focused = false;
        self.filter.clear();
    }
}

impl Widget for SearchableCombo {
    fn areas(&self) -> Vec<Area> {
        let mut out = vec![Area {
            kind: if self.open { AreaKind::TextInput } else { AreaKind::ComboBoxButton },
            id: 0,
            rect: self.rect(),
        }];
        if self.open {
            for (vi, i) in self.visible().iter().enumerate() {
                out.push(Area { kind: AreaKind::ComboBoxItem, id: (i + 1) as u32, rect: self.option_rect(vi) });
            }
        }
        out
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.areas().into_iter().find(|a| inside(p, a.rect))
    }
    fn on_click(&mut self, p: (f32, f32)) {
        if self.open {
            // 点选项:选中原始索引并收起;点搜索行:保持展开(不误关)。
            if let Some(a) = self.hit_area(p) {
                if a.kind == AreaKind::ComboBoxItem {
                    self.selected = Some(a.id as usize - 1);
                    self.close_down();
                }
            }
        } else if inside(p, self.rect()) {
            self.open_up();
        }
    }
    fn on_text(&mut self, c: char) {
        self.on_key(WidgetKey::Char(c));
    }
    fn on_key(&mut self, k: WidgetKey) {
        if !self.open && !self.focused {
            // 关闭且无焦点:仅打字可唤醒(输入即搜索),其余按键忽略。
            if matches!(k, WidgetKey::Char(_)) {
                self.open_up();
            } else {
                return;
            }
        }
        match k {
            WidgetKey::Char(c) => {
                self.filter.push(c);
                self.highlighted = 0;
            }
            WidgetKey::Backspace => {
                self.filter.pop();
                self.highlighted = 0;
            }
            WidgetKey::Down => {
                let n = self.visible().len();
                if n > 0 {
                    self.highlighted = (self.highlighted + 1).min(n - 1);
                }
            }
            WidgetKey::Up => {
                self.highlighted = self.highlighted.saturating_sub(1);
            }
            WidgetKey::Enter => {
                let v = self.visible();
                if let Some(&i) = v.get(self.highlighted.min(v.len().saturating_sub(1))) {
                    self.selected = Some(i);
                    self.close_down();
                }
            }
            WidgetKey::Escape => self.close_down(),
            _ => {}
        }
    }
    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        if !focused && self.open {
            self.close_down();
        }
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let on = hover.map_or(false, |a| a.id == 0);
        cv.fill(self.rect(), if on || self.open { theme.hover } else { theme.row });
        if self.open || !self.filter.is_empty() {
            cv.text(&self.filter, self.x + theme.pad_x, self.y + self.h * 0.72, theme.font_size, theme.text);
        } else {
            let label = self.selected
                .and_then(|i| self.items.get(i).cloned())
                .unwrap_or_else(|| "select…".into());
            cv.text(&label, self.x + theme.pad_x, self.y + self.h * 0.72, theme.font_size,
                if self.selected.is_some() { theme.text } else { theme.text_dim });
        }
        cv.text("v", self.x + self.w - theme.pad_x - 6.0, self.y + self.h * 0.72, theme.font_size, theme.text_dim);
    }
    /// 展开的过滤选项列表画在 overlay 层,悬浮于其他组件之上。
    fn draw_overlay(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        if !self.open {
            return;
        }
        for (vi, i) in self.visible().iter().enumerate() {
            let r = self.option_rect(vi);
            let ion = hover.map_or(false, |a| a.kind == AreaKind::ComboBoxItem && a.id == (i + 1) as u32);
            cv.fill(r, if vi == self.highlighted || ion { theme.hover } else { theme.row });
            cv.text(&self.items[*i], r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size,
                if self.selected == Some(*i) { theme.accent } else { theme.text });
        }
    }
}

/// 表单字段(Form 的行)。
#[derive(Clone, Debug)]
pub enum FormField {
    Text { label: String, value: String, insert: usize, caret: f32 },
    Number { label: String, value: f64, min: f64, max: f64, buf: Option<String> },
    Combo { label: String, items: Vec<String>, selected: usize, open: bool },
    Toggle { label: String, on: bool, anim: f32, dir: f32 },
    Checkbox { label: String, checked: bool },
}

impl FormField {
    fn kind(&self) -> AreaKind {
        match self {
            FormField::Text { .. } => AreaKind::TextInput,
            FormField::Number { .. } => AreaKind::Field,
            FormField::Combo { .. } => AreaKind::ComboBoxButton,
            FormField::Toggle { .. } => AreaKind::ToggleTrack,
            FormField::Checkbox { .. } => AreaKind::Checkbox,
        }
    }
}

/// 多功能表单:多行 label + 控件,支持 Tab 切换焦点行、Enter 提交数值。
/// 标题 id=0,行 id = 索引 + 1。
#[derive(Clone, Debug)]
pub struct Form {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub title: String,
    pub fields: Vec<FormField>,
    pub row_h: f32,
    pub gap: f32,
    /// 当前焦点行索引(Tab 导航)。
    pub focus_row: Option<usize>,
}

impl Form {
    pub fn new(x: f32, y: f32, w: f32, title: impl Into<String>, fields: Vec<FormField>) -> Self {
        Self { x, y, w, title: title.into(), fields, row_h: 24.0, gap: 4.0, focus_row: None }
    }

    fn row_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + self.row_h + i as f32 * (self.row_h + self.gap), self.w, self.row_h)
            .expect("Form row rect")
    }

    fn value_rect(&self, i: usize) -> Rect {
        let r = self.row_rect(i);
        Rect::from_xywh(r.x() + 90.0, r.y(), r.width() - 90.0, r.height())
            .expect("Form value rect")
    }

    fn next_focus(&mut self, dir: i32) {
        let n = self.fields.len();
        if n == 0 {
            return;
        }
        let cur = self.focus_row.map(|i| i as i32).unwrap_or(-1);
        let next = (cur + dir).rem_euclid(n as i32) as usize;
        self.focus_row = Some(next);
    }

    fn combo_item_rect(&self, i: usize, field_idx: usize) -> Rect {
        let r = self.value_rect(field_idx);
        Rect::from_xywh(r.x(), r.y() + r.height() + (i as f32 + 0.0) * r.height(), r.width(), r.height())
            .expect("Form combo item")
    }

    /// 统一点击处理:`measure` 存在时 Text 行把光标定位到最近字符边界(PMCORE-61),
    /// 否则与旧 on_click 行为一致(Text 行只设焦点)。
    fn handle_click(&mut self, p: (f32, f32), pad_x: Option<f32>, measure: Option<&dyn Fn(&str) -> f32>) {
        let Some(a) = self.hit_area(p) else { return };
        if a.id == 0 {
            return;
        }
        let i = (a.id - 1) as usize;
        // 展开的 Combo 选项优先
        if a.kind == AreaKind::ComboBoxItem && a.id >= 1000 {
            let field = (a.id - 1000) / 100;
            let opt = (a.id - 1000) % 100;
            if let Some(FormField::Combo { selected, open, .. }) = self.fields.get_mut(field as usize) {
                *selected = opt as usize;
                *open = false;
            }
            return;
        }
        // 值列几何(点击定位用):先算几何(不可变借用),再可变借用行。
        let vr = self.value_rect(i);
        if let Some(f) = self.fields.get_mut(i) {
            match f {
                FormField::Toggle { on, .. } => *on = !*on,
                FormField::Checkbox { checked, .. } => *checked = !*checked,
                FormField::Combo { open, .. } => *open = !*open,
                FormField::Number { .. } => self.focus_row = Some(i),
                FormField::Text { value, insert, .. } => {
                    self.focus_row = Some(i);
                    if let (Some(px), Some(m)) = (pad_x, measure) {
                        // 点击 label 列只设焦点;值列内才定位光标。
                        if p.0 >= vr.left() {
                            *insert = caret_from_x(value, p.0 - (vr.left() + px), vr.width() - px, m);
                        }
                    }
                }
            }
        }
    }
}

impl Widget for Form {
    fn areas(&self) -> Vec<Area> {
        let mut out = vec![Area {
            kind: AreaKind::PanelTitle,
            id: 0,
            rect: Rect::from_xywh(self.x, self.y, self.w, self.row_h).expect("Form title rect"),
        }];
        for (i, f) in self.fields.iter().enumerate() {
            out.push(Area { kind: f.kind(), id: (i + 1) as u32, rect: self.row_rect(i) });
            // 展开的 Combo 选项(overlay 区域)
            if let FormField::Combo { open: true, items, .. } = f {
                for (j, _) in items.iter().enumerate() {
                    out.push(Area {
                        kind: AreaKind::ComboBoxItem,
                        id: 1000 + (i as u32) * 100 + j as u32,
                        rect: self.combo_item_rect(j, i),
                    });
                }
            }
        }
        out
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.areas().into_iter().find(|a| inside(p, a.rect))
    }
    fn on_click(&mut self, p: (f32, f32)) {
        self.handle_click(p, None, None);
    }
    fn on_click_with_measure(&mut self, p: (f32, f32), pad_x: f32, measure: &dyn Fn(&str) -> f32) {
        self.handle_click(p, Some(pad_x), Some(measure));
    }
    fn on_key(&mut self, k: WidgetKey) {
        // 焦点行的 Number 编辑(打字 → buf,Enter 提交,Esc 取消)。
        if let Some(i) = self.focus_row {
            if let Some(FormField::Number { value, min, max, buf, .. }) = self.fields.get_mut(i) {
                match k {
                    WidgetKey::Char(c) if c.is_ascii_digit() || c == '.' || c == '-' => {
                        let b = buf.get_or_insert_with(|| format!("{value:.3}"));
                        b.push(c);
                    }
                    WidgetKey::Backspace => {
                        if let Some(b) = buf {
                            b.pop();
                        }
                    }
                    WidgetKey::Enter => {
                        if let Some(b) = buf.take() {
                            if let Ok(v) = b.parse::<f64>() {
                                *value = v.clamp(*min, *max);
                            }
                        }
                        self.next_focus(1);
                        return;
                    }
                    WidgetKey::Escape => {
                        *buf = None;
                    }
                    _ => {}
                }
                if matches!(k, WidgetKey::Char(_) | WidgetKey::Backspace | WidgetKey::Escape) {
                    return;
                }
            }
        }
        match k {
            WidgetKey::Tab => self.next_focus(1),
            WidgetKey::ShiftTab => self.next_focus(-1),
            WidgetKey::Enter => {
                self.next_focus(1);
            }
            WidgetKey::Char(c) => {
                if let Some(i) = self.focus_row {
                    if let Some(FormField::Text { value, insert, .. }) = self.fields.get_mut(i) {
                        value.insert(*insert, c);
                        *insert += 1;
                    }
                }
            }
            WidgetKey::Backspace => {
                if let Some(i) = self.focus_row {
                    if let Some(FormField::Text { value, insert, .. }) = self.fields.get_mut(i) {
                        if *insert > 0 {
                            value.remove(*insert - 1);
                            *insert -= 1;
                        }
                    }
                }
            }
            WidgetKey::Left | WidgetKey::Right | WidgetKey::Home | WidgetKey::End => {
                if let Some(i) = self.focus_row {
                    if let Some(FormField::Text { insert, value, .. }) = self.fields.get_mut(i) {
                        let n = value.chars().count();
                        match k {
                            WidgetKey::Left => *insert = insert.saturating_sub(1),
                            WidgetKey::Right => *insert = (*insert + 1).min(n),
                            WidgetKey::Home => *insert = 0,
                            WidgetKey::End => *insert = n,
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    fn update(&mut self, dt: f32) {
        for f in &mut self.fields {
            match f {
                FormField::Toggle { on, anim, dir, .. } => {
                    let target = if *on { 1.0 } else { 0.0 };
                    let d = target - *anim;
                    if d.abs() > 0.0005 {
                        *dir = d.signum();
                    }
                    approach_anim(anim, target, dt, 24.0);
                }
                FormField::Text { caret, .. } => *caret += dt,
                _ => {}
            }
        }
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let title_rect = Rect::from_xywh(self.x, self.y, self.w, self.row_h).expect("Form title rect");
        cv.fill(title_rect, theme.bg);
        cv.text(&self.title, self.x + theme.pad_x, self.y + self.row_h * 0.72, theme.font_size, theme.title);

        for (i, f) in self.fields.iter().enumerate() {
            let r = self.row_rect(i);
            let on = hover.map_or(false, |a| a.id == (i + 1) as u32);
            let focused = self.focus_row == Some(i);
            cv.fill(r, if on || focused { theme.hover } else { theme.row });
            match f {
                FormField::Text { label, value, insert, caret } => {
                    cv.text(label, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
                    let vx = r.x() + 90.0 + theme.pad_x;
                    cv.text(value, vx, r.y() + r.height() * 0.72, theme.font_size,
                        if focused { theme.accent } else { theme.text });
                    // 光标:精确停在 insert 字符后(text_width 真实度量)
                    if focused && (caret * 2.0) as i32 % 2 == 0 {
                        let before: String = value.chars().take(*insert).collect();
                        let tw = cv.text_width(&before, theme.font_size);
                        cv.fill(Rect::from_xywh(vx + tw + 1.0, r.y() + 3.0, 2.0, r.height() - 6.0).expect("Form caret"), [theme.text[0], theme.text[1], theme.text[2], 255]);
                    }
                }
                FormField::Number { label, value, buf, .. } => {
                    cv.text(label, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
                    // 编辑中显示 buf + 光标,否则显示格式化值。
                    let vx = r.x() + 90.0 + theme.pad_x;
                    if let Some(b) = buf {
                        cv.text(b, vx, r.y() + r.height() * 0.72, theme.font_size, theme.accent);
                        let tw = cv.text_width(b, theme.font_size);
                        cv.fill(Rect::from_xywh(vx + tw + 1.0, r.y() + 3.0, 2.0, r.height() - 6.0).expect("Form num caret"), [theme.accent[0], theme.accent[1], theme.accent[2], 255]);
                    } else {
                        let vtext = format!("{value:.2}");
                        cv.text(&vtext, vx, r.y() + r.height() * 0.72, theme.font_size,
                            if focused { theme.accent } else { theme.text });
                    }
                }
                FormField::Combo { label, items, selected, .. } => {
                    cv.text(label, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
                    let label = items.get(*selected).cloned().unwrap_or_default();
                    cv.text(&label, r.x() + 90.0 + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
                }
                FormField::Toggle { label, on: t_on, anim, dir } => {
                    cv.text(label, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
                    let track = Rect::from_xywh(r.x() + r.width() - 34.0, r.y() + (r.height() - 14.0) * 0.5, 30.0, 14.0).expect("Form toggle track");
                    let mut track_c = [theme.disabled[0], theme.disabled[1], theme.disabled[2], 255];
                    if *t_on {
                        track_c = [theme.accent[0], theme.accent[1], theme.accent[2], 255];
                    }
                    cv.fill(track, track_c);
                    let a = anim.clamp(0.0, 1.0);
                    let e = if *dir > 0.0 { ease_out_cubic(a) } else { 1.0 - ease_out_cubic(1.0 - a) };
                    let kx = track.left() + (track.width() - 10.0) * e;
                    cv.fill(Rect::from_xywh(kx, track.y() + 2.0, 10.0, track.height() - 4.0).expect("Form toggle knob"), theme.knob);
                }
                FormField::Checkbox { label, checked } => {
                    cv.text(label, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
                    let b = Rect::from_xywh(r.x() + r.width() - 34.0, r.y() + 3.0, r.height() - 6.0, r.height() - 6.0).expect("Form checkbox box");
                    cv.fill(b, if *checked { [theme.accent[0], theme.accent[1], theme.accent[2], 255] } else { theme.disabled });
                    if *checked {
                        cv.text("x", b.x() + 3.0, b.y() + b.height() * 0.78, theme.font_size, [theme.knob[0], theme.knob[1], theme.knob[2]]);
                    }
                }
            }
        }
    }
    fn draw_overlay(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        for (i, f) in self.fields.iter().enumerate() {
            if let FormField::Combo { open: true, items, .. } = f {
                for (j, item) in items.iter().enumerate() {
                    let r = self.combo_item_rect(j, i);
                    let on = hover.map_or(false, |a| a.kind == AreaKind::ComboBoxItem && a.id == 1000 + (i as u32) * 100 + j as u32);
                    cv.fill(r, if on { theme.hover } else { theme.row });
                    cv.text(item, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
                }
            }
        }
    }
}

/// 实时表单字段控件(RealtimeForm 的行)。
/// 值改动立即生效(无提交概念),支持滚轮步进/拖拽/键盘输入。
#[derive(Clone, Debug)]
pub enum RTControl {
    Number { value: f64, step: f64, min: f64, max: f64, last_x: f32, buf: Option<String> },
    Slider { value: f32 },
    Toggle { on: bool, anim: f32, dir: f32 },
    Text { value: String, insert: usize, caret: f32 },
    Combo { items: Vec<String>, selected: usize, open: bool },
}

impl RTControl {
    fn kind(&self) -> AreaKind {
        match self {
            RTControl::Number { .. } => AreaKind::Field,
            RTControl::Slider { .. } => AreaKind::SliderTrack,
            RTControl::Toggle { .. } => AreaKind::ToggleTrack,
            RTControl::Text { .. } => AreaKind::TextInput,
            RTControl::Combo { .. } => AreaKind::ComboBoxButton,
        }
    }
}

/// 实时表单:标题 + 行(每行 label + 实时控件),底部 Add 按钮增行。
/// 值修改即时生效(滚轮步进 / 拖拽 / 键盘 / 点击),行可增删。
/// 标题 id=0;行 id = 索引 + 1;Add 按钮 id = rows+1。
#[derive(Clone, Debug)]
pub struct RealtimeForm {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub title: String,
    pub rows: Vec<(String, RTControl)>,
    pub row_h: f32,
    pub gap: f32,
    /// 当前键盘焦点行(Text/Number 编辑)。
    pub focus_row: Option<usize>,
}

impl RealtimeForm {
    pub fn new(x: f32, y: f32, w: f32, title: impl Into<String>, rows: Vec<(String, RTControl)>) -> Self {
        Self { x, y, w, title: title.into(), rows, row_h: 24.0, gap: 4.0, focus_row: None }
    }

    pub fn add_row(&mut self, label: impl Into<String>, control: RTControl) {
        self.rows.push((label.into(), control));
    }

    #[allow(dead_code)]
    pub fn remove_row(&mut self, i: usize) {
        if i < self.rows.len() {
            self.rows.remove(i);
            if let Some(f) = self.focus_row {
                if f == i {
                    self.focus_row = None;
                } else if f > i {
                    self.focus_row = Some(f - 1);
                }
            }
        }
    }

    fn title_h(&self) -> f32 {
        self.row_h
    }

    fn row_rect(&self, i: usize) -> Rect {
        Rect::from_xywh(self.x, self.y + self.title_h() + i as f32 * (self.row_h + self.gap), self.w, self.row_h)
            .expect("RealtimeForm row rect")
    }

    fn add_rect(&self) -> Rect {
        let y = self.y + self.title_h() + self.rows.len() as f32 * (self.row_h + self.gap);
        Rect::from_xywh(self.x, y, self.w, self.row_h).expect("RealtimeForm add rect")
    }

    fn value_rect(&self, i: usize) -> Rect {
        let r = self.row_rect(i);
        Rect::from_xywh(r.x() + 90.0, r.y(), r.width() - 90.0, r.height())
            .expect("RealtimeForm value rect")
    }

    fn combo_item_rect(&self, i: usize, j: usize) -> Rect {
        let r = self.value_rect(i);
        Rect::from_xywh(r.x(), r.y() + r.height() + j as f32 * r.height(), r.width(), r.height())
            .expect("RealtimeForm combo item")
    }

    /// 统一点击处理:`measure` 存在时 Text 行把光标定位到最近字符边界(PMCORE-61),
    /// 否则与旧 on_click 行为一致(Text 行只设焦点)。
    fn handle_click(&mut self, p: (f32, f32), pad_x: Option<f32>, measure: Option<&dyn Fn(&str) -> f32>) {
        let Some(a) = self.hit_area(p) else { return };
        if a.id == 0 {
            return;
        }
        // Add 按钮
        if a.id as usize == self.rows.len() + 1 {
            self.add_row(format!("var{}", self.rows.len() + 1), RTControl::Number { value: 0.0, step: 0.1, min: -100.0, max: 100.0, last_x: 0.0, buf: None });
            return;
        }
        // Combo 选项
        if a.kind == AreaKind::ComboBoxItem && a.id >= 1000 {
            let field = (a.id - 1000) / 100;
            let opt = (a.id - 1000) % 100;
            if let Some((_, RTControl::Combo { selected, open, .. })) = self.rows.get_mut(field as usize) {
                *selected = opt as usize;
                *open = false;
            }
            return;
        }
        let i = (a.id - 1) as usize;
        // 先算几何(不可变借用),再可变借用行。
        let r = self.value_rect(i);
        if let Some((_, c)) = self.rows.get_mut(i) {
            match c {
                RTControl::Toggle { on, .. } => *on = !*on,
                RTControl::Combo { open, .. } => *open = !*open,
                RTControl::Number { last_x, .. } => {
                    self.focus_row = Some(i);
                    *last_x = p.0;
                }
                RTControl::Text { value, insert, .. } => {
                    self.focus_row = Some(i);
                    if let (Some(px), Some(m)) = (pad_x, measure) {
                        // 点击 label 列只设焦点;值列内才定位光标。
                        if p.0 >= r.left() {
                            *insert = caret_from_x(value, p.0 - (r.left() + px), r.width() - px, m);
                        }
                    }
                }
                RTControl::Slider { value } => {
                    *value = ((p.0 - r.left()) / r.width()).clamp(0.0, 1.0);
                    self.focus_row = None;
                }
            }
        }
    }
}

impl Widget for RealtimeForm {
    fn areas(&self) -> Vec<Area> {
        let mut out = vec![Area {
            kind: AreaKind::PanelTitle,
            id: 0,
            rect: Rect::from_xywh(self.x, self.y, self.w, self.title_h()).expect("RealtimeForm title rect"),
        }];
        for (i, (_, c)) in self.rows.iter().enumerate() {
            out.push(Area { kind: c.kind(), id: (i + 1) as u32, rect: self.row_rect(i) });
            if let RTControl::Combo { open: true, items, .. } = c {
                for (j, _) in items.iter().enumerate() {
                    out.push(Area {
                        kind: AreaKind::ComboBoxItem,
                        id: 1000 + (i as u32) * 100 + j as u32,
                        rect: self.combo_item_rect(i, j),
                    });
                }
            }
        }
        out.push(Area {
            kind: AreaKind::Button,
            id: (self.rows.len() + 1) as u32,
            rect: self.add_rect(),
        });
        out
    }
    fn hit_area(&self, p: (f32, f32)) -> Option<Area> {
        self.areas().into_iter().find(|a| inside(p, a.rect))
    }
    fn on_click(&mut self, p: (f32, f32)) {
        self.handle_click(p, None, None);
    }
    fn on_click_with_measure(&mut self, p: (f32, f32), pad_x: f32, measure: &dyn Fn(&str) -> f32) {
        self.handle_click(p, Some(pad_x), Some(measure));
    }
    fn on_drag(&mut self, p: (f32, f32)) {
        let Some(a) = self.hit_area(p) else { return };
        if a.id == 0 || a.id as usize == self.rows.len() + 1 {
            return;
        }
        let i = (a.id - 1) as usize;
        let r = self.value_rect(i);
        if let Some((_, c)) = self.rows.get_mut(i) {
            match c {
                RTControl::Number { value, step, min, max, last_x, buf } => {
                    // 拖拽时取消键盘编辑缓冲(值直接跟随指针)。
                    *buf = None;
                    let dx = p.0 - *last_x;
                    *last_x = p.0;
                    *value = (*value + dx as f64 * *step).clamp(*min, *max);
                }
                RTControl::Slider { value } => {
                    *value = ((p.0 - r.left()) / r.width()).clamp(0.0, 1.0);
                }
                _ => {}
            }
        }
    }
    /// 滚轮步进:对焦点行(Number)按 step 调整;无焦点行时忽略。
    fn on_wheel(&mut self, dy: f32) {
        let Some(i) = self.focus_row else { return };
        if let Some((_, RTControl::Number { value, step, min, max, .. })) = self.rows.get_mut(i) {
            *value = (*value + dy as f64 * *step).clamp(*min, *max);
        }
    }
    fn on_key(&mut self, k: WidgetKey) {
        // 焦点行的 Number 编辑(打字 → buf,Enter 提交,Esc 取消)。
        if let Some(i) = self.focus_row {
            if let Some((_, RTControl::Number { value, step, min, max, buf, .. })) = self.rows.get_mut(i) {
                match k {
                    WidgetKey::Char(c) if c.is_ascii_digit() || c == '.' || c == '-' => {
                        let b = buf.get_or_insert_with(|| format!("{value:.3}"));
                        b.push(c);
                    }
                    WidgetKey::Backspace => {
                        if let Some(b) = buf {
                            b.pop();
                        }
                    }
                    WidgetKey::Enter => {
                        if let Some(b) = buf.take() {
                            if let Ok(v) = b.parse::<f64>() {
                                *value = v.clamp(*min, *max);
                            }
                        }
                        self.focus_row = None;
                        return;
                    }
                    WidgetKey::Escape => {
                        *buf = None;
                    }
                    _ => {}
                }
                if matches!(k, WidgetKey::Char(_) | WidgetKey::Backspace | WidgetKey::Escape) {
                    let _ = step;
                    return;
                }
            }
        }
        match k {
            WidgetKey::Tab => {
                let n = self.rows.len();
                if n > 0 {
                    let cur = self.focus_row.map(|i| i as i32).unwrap_or(-1);
                    self.focus_row = Some(((cur + 1).rem_euclid(n as i32)) as usize);
                }
            }
            WidgetKey::ShiftTab => {
                let n = self.rows.len();
                if n > 0 {
                    let cur = self.focus_row.map(|i| i as i32).unwrap_or(n as i32);
                    self.focus_row = Some(((cur - 1).rem_euclid(n as i32)) as usize);
                }
            }
            WidgetKey::Char(c) => {
                if let Some(i) = self.focus_row {
                    if let Some((_, RTControl::Text { value, insert, .. })) = self.rows.get_mut(i) {
                        value.insert(*insert, c);
                        *insert += 1;
                    }
                }
            }
            WidgetKey::Backspace => {
                if let Some(i) = self.focus_row {
                    if let Some((_, RTControl::Text { value, insert, .. })) = self.rows.get_mut(i) {
                        if *insert > 0 {
                            value.remove(*insert - 1);
                            *insert -= 1;
                        }
                    }
                }
            }
            WidgetKey::Left | WidgetKey::Right | WidgetKey::Home | WidgetKey::End => {
                if let Some(i) = self.focus_row {
                    if let Some((_, RTControl::Text { value, insert, .. })) = self.rows.get_mut(i) {
                        let n = value.chars().count();
                        match k {
                            WidgetKey::Left => *insert = insert.saturating_sub(1),
                            WidgetKey::Right => *insert = (*insert + 1).min(n),
                            WidgetKey::Home => *insert = 0,
                            WidgetKey::End => *insert = n,
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    fn update(&mut self, dt: f32) {
        for (_, c) in &mut self.rows {
            match c {
                RTControl::Toggle { on, anim, dir, .. } => {
                    let target = if *on { 1.0 } else { 0.0 };
                    let d = target - *anim;
                    if d.abs() > 0.0005 {
                        *dir = d.signum();
                    }
                    approach_anim(anim, target, dt, 24.0);
                }
                RTControl::Text { caret, .. } => *caret += dt,
                _ => {}
            }
        }
    }
    fn draw(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        let title_rect = Rect::from_xywh(self.x, self.y, self.w, self.title_h()).expect("RealtimeForm title rect");
        cv.fill(title_rect, theme.bg);
        cv.text(&self.title, self.x + theme.pad_x, self.y + self.title_h() * 0.72, theme.font_size, theme.title);

        for (i, (label, c)) in self.rows.iter().enumerate() {
            let r = self.row_rect(i);
            let on = hover.map_or(false, |a| a.id == (i + 1) as u32);
            let focused = self.focus_row == Some(i);
            cv.fill(r, if on || focused { theme.hover } else { theme.row });
            cv.text(label, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
            let vx = r.x() + 90.0 + theme.pad_x;
            match c {
                RTControl::Number { value, buf, .. } => {
                    // 编辑中显示 buf + 光标,否则格式化值。
                    if let Some(b) = buf {
                        cv.text(b, vx, r.y() + r.height() * 0.72, theme.font_size, theme.accent);
                        let tw = cv.text_width(b, theme.font_size);
                        cv.fill(Rect::from_xywh(vx + tw + 1.0, r.y() + 3.0, 2.0, r.height() - 6.0).expect("RT num caret"), [theme.accent[0], theme.accent[1], theme.accent[2], 255]);
                    } else {
                        let vtext = format!("{value:.2}");
                        cv.text(&vtext, vx, r.y() + r.height() * 0.72, theme.font_size,
                            if focused { theme.accent } else { theme.text });
                    }
                    cv.text("d", r.x() + r.width() - theme.pad_x - 8.0, r.y() + r.height() * 0.72, theme.font_size, theme.text_dim);
                }
                RTControl::Slider { value } => {
                    let vr = self.value_rect(i);
                    let v = value.clamp(0.0, 1.0);
                    let fw = vr.width() * v;
                    if fw > 1.0 {
                        cv.fill(Rect::from_xywh(vr.x(), vr.y(), fw, vr.height()).expect("RT slider fill"), [theme.accent[0], theme.accent[1], theme.accent[2], 255]);
                    }
                    let kx = vr.x() + (vr.width() - 8.0) * v;
                    cv.fill(Rect::from_xywh(kx, vr.y(), 8.0, vr.height()).expect("RT slider knob"), theme.knob);
                }
                RTControl::Toggle { on: t_on, anim, dir, .. } => {
                    let track = Rect::from_xywh(r.x() + r.width() - 34.0, r.y() + (r.height() - 14.0) * 0.5, 30.0, 14.0).expect("RT toggle track");
                    let mut track_c = [theme.disabled[0], theme.disabled[1], theme.disabled[2], 255];
                    if *t_on {
                        track_c = [theme.accent[0], theme.accent[1], theme.accent[2], 255];
                    }
                    cv.fill(track, track_c);
                    let a = anim.clamp(0.0, 1.0);
                    let e = if *dir > 0.0 { ease_out_cubic(a) } else { 1.0 - ease_out_cubic(1.0 - a) };
                    let kx = track.left() + (track.width() - 10.0) * e;
                    cv.fill(Rect::from_xywh(kx, track.y() + 2.0, 10.0, track.height() - 4.0).expect("RT toggle knob"), theme.knob);
                }
                RTControl::Text { value, insert, caret } => {
                    cv.text(value, vx, r.y() + r.height() * 0.72, theme.font_size,
                        if focused { theme.accent } else { theme.text });
                    if focused && (caret * 2.0) as i32 % 2 == 0 {
                        let before: String = value.chars().take(*insert).collect();
                        let tw = cv.text_width(&before, theme.font_size);
                        cv.fill(Rect::from_xywh(vx + tw + 1.0, r.y() + 3.0, 2.0, r.height() - 6.0).expect("RT caret"), [theme.text[0], theme.text[1], theme.text[2], 255]);
                    }
                }
                RTControl::Combo { items, selected, open, .. } => {
                    let label = items.get(*selected).cloned().unwrap_or_default();
                    cv.text(&label, vx, r.y() + r.height() * 0.72, theme.font_size, theme.text);
                    let _ = open;
                }
            }
        }
        // Add 行
        let ar = self.add_rect();
        let on = hover.map_or(false, |a| a.id as usize == self.rows.len() + 1);
        cv.fill(ar, if on { theme.hover } else { theme.row });
        cv.text("+ Add", ar.x() + theme.pad_x, ar.y() + ar.height() * 0.72, theme.font_size, theme.accent);
    }
    fn draw_overlay(&self, cv: &mut dyn Canvas, theme: &Theme, hover: Option<&Area>) {
        for (i, (_, c)) in self.rows.iter().enumerate() {
            if let RTControl::Combo { open: true, items, .. } = c {
                for (j, item) in items.iter().enumerate() {
                    let r = self.combo_item_rect(i, j);
                    let on = hover.map_or(false, |a| a.kind == AreaKind::ComboBoxItem && a.id == 1000 + (i as u32) * 100 + j as u32);
                    cv.fill(r, if on { theme.hover } else { theme.row });
                    cv.text(item, r.x() + theme.pad_x, r.y() + r.height() * 0.72, theme.font_size, theme.text);
                }
            }
        }
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
            assert_eq!(a.kind, AreaKind::ListRow);
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

    #[test]
    fn button_hit_inside_outside_and_disabled() {
        let b = Button::new(10.0, 20.0, 100.0, 22.0, "Add").enabled;
        let _ = b;
        let b = Button::new(10.0, 20.0, 100.0, 22.0, "Add");
        assert_eq!(b.areas().len(), 1);
        assert_eq!(b.hit_area((50.0, 30.0)).unwrap().kind, AreaKind::Button);
        assert_eq!(b.hit_area((9.9, 30.0)), None);
        assert_eq!(b.hit_area((110.0, 30.0)), None); // 右开区间
        assert_eq!(b.hit_area((50.0, 42.0)), None);
    }

    #[test]
    fn toggle_knob_tracks_on_state() {
        let off = Toggle::new(0.0, 0.0, 120.0, 22.0, "Global", false);
        let on = Toggle::new(0.0, 0.0, 120.0, 22.0, "Global", true);
        let (k_off, k_on) = (off.knob_rect(), on.knob_rect());
        // on 时滑块在右侧
        assert!(k_on.left() > k_off.left());
        // 轨道区命中 id=0,滑块区命中 id=1
        assert_eq!(off.hit_area((50.0, 11.0)).unwrap().id, 0);
        assert_eq!(off.hit_area(center(k_off)).unwrap().kind, AreaKind::ToggleKnob);
        // label 区(轨道)也能命中
        assert_eq!(off.hit_area((8.0, 11.0)).unwrap().kind, AreaKind::ToggleTrack);
    }

    #[test]
    fn toggle_anim_eases_between_states() {
        let mut t = Toggle::new(0.0, 0.0, 120.0, 22.0, "Global", false);
        assert!((t.anim - 0.0).abs() < 1e-6);
        // 点击切换 on:动画位置开始向 1 推进(更新 0.1s 后仍 < 1,渐进)。
        t.on_click((10.0, 11.0));
        assert!(t.on);
        let start = t.anim;
        t.update(0.05);
        let mid = t.anim;
        t.update(0.05);
        let later = t.anim;
        // anim 单调向右推进(knob 位置经 ease_out_back 允许过冲回弹)
        assert!(mid > start, "anim 应向 on 方向推进: {start} -> {mid}");
        assert!(later >= mid);
        // 多步后到达 on 终点
        for _ in 0..60 {
            t.update(0.1);
        }
        assert!((t.anim - 1.0).abs() < 0.01);
        let end = t.knob_rect().left();
        let on_ref = Toggle::new(0.0, 0.0, 120.0, 22.0, "Global", true);
        assert!((end - on_ref.knob_rect().left()).abs() < 0.01);
    }

    #[test]
    fn slider_value_drives_fill_and_knob() {
        let s = Slider::new(0.0, 0.0, 100.0, 18.0, 0.5);
        // 填充一半
        assert!((s.fill_rect().width() - 50.0).abs() < 0.01);
        // 滑块在中间
        assert!((s.knob_rect().left() - (100.0 - 8.0) * 0.5).abs() < 0.01);
        // value 钳位
        let c = Slider::new(0.0, 0.0, 100.0, 18.0, 1.7);
        assert!((c.knob_rect().right() - 100.0).abs() < 0.01);
        // 轨道命中 id=0,滑块命中 id=1
        assert_eq!(s.hit_area((20.0, 9.0)).unwrap().id, 0);
        assert_eq!(s.hit_area(center(s.knob_rect())).unwrap().kind, AreaKind::SliderKnob);
    }

    #[test]
    fn field_whole_row_hit() {
        let f = Field::new(0.0, 0.0, 200.0, 22.0, "start", "12.000");
        assert_eq!(f.areas().len(), 1);
        assert_eq!(f.hit_area((100.0, 11.0)).unwrap().kind, AreaKind::Field);
        assert_eq!(f.hit_area((199.0, 21.9)), Some(f.hit_area((1.0, 1.0)).unwrap()));
        assert_eq!(f.hit_area((200.0, 11.0)), None);
    }

    #[test]
    fn panel_title_and_rows_hit() {
        let p = Panel::new(0.0, 0.0, 240.0, "Settings", vec![
            PanelRow::Button { label: "Save".into(), enabled: true },
            PanelRow::Toggle { label: "Vsync".into(), on: true, anim: 1.0, dir: 1.0 },
            PanelRow::Slider { value: 0.3 },
            PanelRow::Field { label: "Name".into(), value: "test".into(), editing: false },
        ]);
        let areas = p.areas();
        assert_eq!(areas.len(), 5);
        assert_eq!(areas[0].kind, AreaKind::PanelTitle);
        assert_eq!(areas[1].kind, AreaKind::Button);
        assert_eq!(areas[2].kind, AreaKind::ToggleTrack);
        assert_eq!(areas[3].kind, AreaKind::SliderTrack);
        assert_eq!(areas[4].kind, AreaKind::Field);
        // 标题命中 id=0
        assert_eq!(p.hit_area((10.0, 11.0)).unwrap().id, 0);
        // 第 1 行(id=1)
        let r1 = p.row_rect(0);
        assert_eq!(p.hit_area(center(r1)).unwrap().id, 1);
        // 行内组件坐标:标题在面板上方,行依次向下
        assert!(p.row_rect(1).top() > p.row_rect(0).bottom());
    }

    #[test]
    fn scrolllist_rows_follow_scroll_and_bar() {
        let mut l = ScrollList::new(0.0, 0.0, 160.0, 10, 4);
        l.scroll = 3.0;
        let areas = l.areas();
        // 4 个可见行 + 滚动条
        assert_eq!(areas.len(), 5);
        // 行 id = 全局行号:3,4,5,6
        let rows: Vec<u32> = areas.iter().filter(|a| a.kind == AreaKind::ScrollRow).map(|a| a.id).collect();
        assert_eq!(rows, vec![3, 4, 5, 6]);
        // 点击第 2 个可见行 → 全局行号 4
        let r1 = l.row_rect(1);
        assert_eq!(l.hit_area(center(r1)).unwrap().id, 4);
        // 滚动条命中
        let bar = l.bar_rect();
        assert_eq!(l.hit_area(center(bar)).unwrap().kind, AreaKind::ScrollBar);
        // 滚动条在右侧
        assert!((bar.right() - 160.0).abs() < 0.01);
        // 滚动条外、行外不命中
        assert_eq!(l.hit_area((80.0, -5.0)), None);
    }

    #[test]
    fn scrolllist_scroll_clamps_visible() {
        let mut l = ScrollList::new(0.0, 0.0, 160.0, 10, 4);
        l.scroll = 999.0; // 越界,行 id 会超 total — 显示层负责钳位
        l.scroll = l.scroll.clamp(0.0, l.max_scroll());
        assert_eq!(l.scroll, 6.0);
    }

    #[test]
    fn combo_expands_and_selects() {
        let mut c = ComboBox::new(0.0, 0.0, 120.0, 22.0, vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(c.areas().len(), 1); // 收起:只有主按钮
        // 点主按钮展开
        c.on_click((60.0, 11.0));
        assert!(c.open);
        assert_eq!(c.areas().len(), 4); // 主按钮 + 3 项
        // 点第 3 项(id=3)→ 选中并收起
        let item = c.item_rect(2);
        c.on_click(center(item));
        assert!(!c.open);
        assert_eq!(c.selected, 2);
        // 再次展开,点主按钮收回
        c.on_click((60.0, 11.0));
        assert!(c.open);
        c.on_click((60.0, 11.0));
        assert!(!c.open);
        // 展开时选项命中
        c.open = true;
        assert_eq!(c.hit_area(center(c.item_rect(1))).unwrap().kind, AreaKind::ComboBoxItem);
        assert_eq!(c.hit_area(center(c.item_rect(1))).unwrap().id, 2);
    }

    #[test]
    fn stepper_minus_plus_clamped() {
        let mut s = Stepper::new(0.0, 0.0, 200.0, 22.0, "volume", 1.0);
        s.step = 0.5;
        s.min = 0.0;
        s.max = 2.0;
        // 加
        s.on_click(center(s.plus_rect()));
        assert!((s.value - 1.5).abs() < 1e-9);
        // 减
        s.on_click(center(s.minus_rect()));
        assert!((s.value - 1.0).abs() < 1e-9);
        // 钳位到 max
        s.on_click(center(s.plus_rect()));
        s.on_click(center(s.plus_rect()));
        s.on_click(center(s.plus_rect()));
        assert!((s.value - 2.0).abs() < 1e-9);
        // 钳位到 min
        for _ in 0..8 {
            s.on_click(center(s.minus_rect()));
        }
        assert!((s.value - 0.0).abs() < 1e-9);
        // 命中:减按钮区域
        assert_eq!(s.hit_area(center(s.minus_rect())).unwrap().kind, AreaKind::StepperMinus);
        assert_eq!(s.hit_area(center(s.plus_rect())).unwrap().kind, AreaKind::StepperPlus);
        assert_eq!(s.hit_area((100.0, 11.0)).unwrap().kind, AreaKind::StepperField);
    }

    #[test]
    fn checkbox_toggles_and_hits() {
        let mut c = Checkbox::new(0.0, 0.0, 160.0, 22.0, "mute", false);
        assert_eq!(c.areas().len(), 2);
        assert_eq!(c.hit_area(center(c.box_rect())).unwrap().kind, AreaKind::CheckboxBox);
        assert_eq!(c.hit_area((150.0, 11.0)).unwrap().kind, AreaKind::Checkbox);
        c.on_click((10.0, 11.0));
        assert!(c.checked);
        c.on_click((10.0, 11.0));
        assert!(!c.checked);
        // 勾选框在 label 左侧
        assert!(c.box_rect().right() < c.x + c.w * 0.5);
    }

    #[test]
    fn tabbar_selects_and_areas() {
        let mut t = TabBar::new(0.0, 0.0, 240.0, 24.0, vec!["A".into(), "B".into(), "C".into()]);
        assert_eq!(t.areas().len(), 3);
        assert_eq!(t.selected, 0);
        t.on_click(center(t.tab_rect(2)));
        assert_eq!(t.selected, 2);
        // tab 等宽
        assert!((t.tab_rect(0).width() - 80.0).abs() < 0.01);
        assert!((t.tab_rect(1).x() - 80.0).abs() < 0.01);
        assert_eq!(t.hit_area((10.0, 12.0)).unwrap().id, 0);
    }

    #[test]
    fn progressbar_click_sets_progress() {
        let mut p = ProgressBar::new(0.0, 0.0, 200.0, 18.0, "loading", 0.0);
        p.on_click((100.0, 9.0));
        assert!((p.progress - 0.5).abs() < 0.01);
        p.on_drag((150.0, 9.0));
        assert!((p.progress - 0.75).abs() < 0.01);
        p.on_click((250.0, 9.0)); // 越界:clamp 到 1
        assert!((p.progress - 1.0).abs() < 0.01);
        assert_eq!(p.hit_area((10.0, 9.0)).unwrap().kind, AreaKind::ProgressBar);
    }

    #[test]
    fn keyvaluegrid_selects_rows() {
        let mut g = KeyValueGrid::new(0.0, 0.0, 240.0, vec![
            ("name".into(), "test".into()),
            ("bpm".into(), "120".into()),
            ("level".into(), "IN 15".into()),
        ]);
        assert_eq!(g.areas().len(), 3);
        assert!(g.selected.is_none());
        let r1 = g.row_rect(1);
        g.on_click(center(r1));
        assert_eq!(g.selected, Some(1));
        g.on_click(center(g.row_rect(2)));
        assert_eq!(g.selected, Some(2));
        assert_eq!(g.hit_area(center(r1)).unwrap().id, 1);
    }

    #[test]
    fn keyvaluegrid_editable_rows_commit_on_enter() {
        let mut g = KeyValueGrid::new(0.0, 0.0, 240.0, vec![
            ("name".into(), "old".into()),
            ("difficulty".into(), "10.0".into()),
            ("notes".into(), "42".into()), // 只读
        ]);
        g.edit_kind = vec![
            Some(GridFieldKind::Text),
            Some(GridFieldKind::Number),
            None,
        ];
        g.num_min = 0.0;
        g.num_max = 100.0;

        // 点击可编辑行 → 进入编辑;缓冲初始化为行值,光标在末尾。
        g.on_click(center(g.row_rect(0)));
        assert_eq!(g.editing, Some(0));
        assert_eq!(g.buf, "old");

        // 打字(末尾追加)+ Left 移动 + Backspace 删光标前字符。
        g.on_key(WidgetKey::Char('N'));
        g.on_key(WidgetKey::Char('e'));
        assert_eq!(g.buf, "oldNe");
        g.on_key(WidgetKey::Left);
        g.on_key(WidgetKey::Backspace); // 删 'N'
        assert_eq!(g.buf, "olde");
        g.on_key(WidgetKey::Home);
        g.on_key(WidgetKey::Char('N'));
        assert_eq!(g.buf, "Nolde");

        // Enter 提交:行值更新 + committed 通知;编辑态退出。
        g.on_key(WidgetKey::Enter);
        assert_eq!(g.committed, Some(0));
        assert_eq!(g.editing, None);
        assert_eq!(g.rows[0].1, "Nolde");
        g.committed = None;

        // 点击只读行 → 不进入编辑。
        g.on_click(center(g.row_rect(2)));
        assert_eq!(g.editing, None);

        // Number 行:缓冲以当前值格式化开头(与 Form 同语义),Backspace 清空
        // 后重新输入,Enter 解析 + clamp,无效输入回退原值。
        g.on_click(center(g.row_rect(1)));
        assert_eq!(g.editing, Some(1));
        for _ in 0..4 {
            g.on_key(WidgetKey::Backspace); // 清掉 "10.0"
        }
        assert_eq!(g.num_buf, Some(String::new()));
        for c in ['9', '9', '9', '.', '5'] {
            g.on_key(WidgetKey::Char(c));
        }
        g.on_key(WidgetKey::Enter);
        assert_eq!(g.committed, Some(1));
        assert_eq!(g.rows[1].1, "100.0", "999.5 被 clamp 到 100.0");
        g.committed = None;

        // 无效数字 Enter → 保持原行值。
        g.on_click(center(g.row_rect(1)));
        g.on_key(WidgetKey::Char('a')); // Number 行忽略非数字
        g.on_key(WidgetKey::Char('b'));
        g.on_key(WidgetKey::Enter);
        assert_eq!(g.rows[1].1, "100.0");
        g.committed = None;

        // Escape 取消编辑,不提交。
        g.on_click(center(g.row_rect(0)));
        g.on_key(WidgetKey::Char('x'));
        g.on_key(WidgetKey::Escape);
        assert_eq!(g.editing, None);
        assert_eq!(g.committed, None);
        assert_eq!(g.rows[0].1, "Nolde", "Escape 不写回");
    }

    #[test]
    fn textinput_focus_chars_backspace() {
        let mut t = TextInput::new(0.0, 0.0, 200.0, 24.0, "search…");
        // 未聚焦:字符被忽略
        t.on_text('a');
        assert!(t.text.is_empty());
        // 聚焦后输入
        t.set_focus(true);
        assert!(t.focused);
        for c in ['p', 'h', 'i', 'm', 'a'] {
            t.on_text(c);
        }
        assert_eq!(t.text, "phima");
        t.on_backspace();
        assert_eq!(t.text, "phim");
        // 失焦后忽略
        t.set_focus(false);
        t.on_text('x');
        assert_eq!(t.text, "phim");
        // 光标闪烁推进(不 panic)
        t.set_focus(true);
        t.update(0.5);
        t.update(0.5);
        // 命中
        assert_eq!(t.hit_area((10.0, 12.0)).unwrap().kind, AreaKind::TextInput);
        assert_eq!(t.hit_area((250.0, 12.0)), None);
    }

    #[test]
    fn dragvalue_drag_changes_value_clamped() {
        let mut d = DragValue::new(0.0, 0.0, 200.0, 24.0, "x", 5.0);
        d.speed = 0.1;
        d.min = 0.0;
        d.max = 10.0;
        d.on_click((100.0, 12.0));
        assert!(d.dragging);
        // 向右拖 20px → +2
        d.on_drag((120.0, 12.0));
        assert!((d.value - 7.0).abs() < 1e-9);
        // 向左拖 100px → -10 → clamp 到 min
        d.on_drag((20.0, 12.0));
        assert!((d.value - 0.0).abs() < 1e-9);
        // 命中
        assert_eq!(d.hit_area((50.0, 12.0)).unwrap().kind, AreaKind::DragValue);
    }

    #[test]
    fn colorpicker_channels_and_preview() {
        let mut c = ColorPicker::new(0.0, 0.0, 240.0, [0.0, 0.5, 1.0]);
        assert_eq!(c.areas().len(), 4);
        // 通道行 id 0..2,预览 id 3
        assert_eq!(c.hit_area(center(c.row_rect(0))).unwrap().kind, AreaKind::ColorChannel);
        assert_eq!(c.hit_area(center(c.row_rect(2))).unwrap().id, 2);
        assert_eq!(c.hit_area(center(c.preview_rect())).unwrap().kind, AreaKind::ColorPreview);
        // 点击 R 通道中间 → r ≈ 0.5
        let ch = c.channel_rect(0);
        c.on_click((ch.x() + ch.width() * 0.5, ch.y() + 1.0));
        assert!((c.rgb[0] - 0.5).abs() < 0.01);
        // G/B 不变
        assert!((c.rgb[1] - 0.5).abs() < 1e-6);
        assert!((c.rgb[2] - 1.0).abs() < 1e-6);
        // 拖拽同样生效
        c.on_drag((ch.x() + ch.width() * 0.25, ch.y() + 1.0));
        assert!((c.rgb[0] - 0.25).abs() < 0.01);
    }

    #[test]
    fn listbox_multi_select_toggle() {
        let mut l = ListBox::new(0.0, 0.0, 200.0, vec!["a".into(), "b".into(), "c".into()]);
        assert!(l.selected.is_empty());
        l.on_click(center(l.row_rect(0)));
        assert_eq!(l.selected, vec![0]);
        l.on_click(center(l.row_rect(2)));
        assert_eq!(l.selected, vec![0, 2]);
        // 再点 0 → 取消
        l.on_click(center(l.row_rect(0)));
        assert_eq!(l.selected, vec![2]);
        // 命中
        assert_eq!(l.hit_area(center(l.row_rect(1))).unwrap().kind, AreaKind::ListBoxRow);
        assert_eq!(l.hit_area(center(l.row_rect(1))).unwrap().id, 1);
    }

    #[test]
    fn form_tab_navigation_and_text_edit() {
        let mut f = Form::new(0.0, 0.0, 300.0, "Params", vec![
            FormField::Text { label: "name".into(), value: String::new(), insert: 0, caret: 0.0 },
            FormField::Number { label: "bpm".into(), value: 120.0, min: 0.0, max: 300.0, buf: None },
            FormField::Toggle { label: "global".into(), on: false, anim: 0.0, dir: -1.0 },
        ]);
        // 点击 Text 行聚焦
        f.on_click(center(f.row_rect(0)));
        assert_eq!(f.focus_row, Some(0));
        // 输入字符
        f.on_key(WidgetKey::Char('x'));
        f.on_key(WidgetKey::Char('y'));
        if let FormField::Text { value, insert, .. } = &f.fields[0] {
            assert_eq!(value, "xy");
            assert_eq!(*insert, 2);
        }
        // 左移 + 退格:光标前移到 'x' 后,删除 'x' → 剩 "y",光标回到 0
        f.on_key(WidgetKey::Left);
        f.on_key(WidgetKey::Backspace);
        if let FormField::Text { value, insert, .. } = &f.fields[0] {
            assert_eq!(value, "y");
            assert_eq!(*insert, 0);
        }
        // Tab 切换焦点
        f.on_key(WidgetKey::Tab);
        assert_eq!(f.focus_row, Some(1));
        f.on_key(WidgetKey::Tab);
        assert_eq!(f.focus_row, Some(2));
        f.on_key(WidgetKey::ShiftTab);
        assert_eq!(f.focus_row, Some(1));
        // 点击 Toggle 切换
        f.on_click(center(f.row_rect(2)));
        if let FormField::Toggle { on, .. } = &f.fields[2] {
            assert!(*on);
        }
        // 命中与区域
        assert_eq!(f.hit_area(center(f.row_rect(0))).unwrap().kind, AreaKind::TextInput);
        assert_eq!(f.hit_area(center(f.row_rect(1))).unwrap().kind, AreaKind::Field);
    }

    #[test]
    fn form_number_keyboard_input() {
        let mut f = Form::new(0.0, 0.0, 300.0, "Params", vec![
            FormField::Number { label: "bpm".into(), value: 120.0, min: 0.0, max: 300.0, buf: None },
        ]);
        f.on_click(center(f.row_rect(0)));
        assert_eq!(f.focus_row, Some(0));
        f.on_key(WidgetKey::Char('5'));
        if let FormField::Number { value, buf, .. } = &f.fields[0] {
            assert_eq!(buf.as_deref(), Some("120.0005"));
            assert!((*value - 120.0).abs() < 1e-9);
        }
        f.on_key(WidgetKey::Enter);
        if let FormField::Number { value, buf, .. } = &f.fields[0] {
            assert!((*value - 120.0005).abs() < 1e-9);
            assert!(buf.is_none());
        }
        // Enter 后焦点前进(环回)
        assert_eq!(f.focus_row, Some(0));
        // 输入超 max → clamp:先打字创建 buf,清空后输入 999
        f.on_click(center(f.row_rect(0)));
        f.on_key(WidgetKey::Char('9')); // 创建 buf(从当前值 120.001 开始)
        for _ in 0..10 { f.on_key(WidgetKey::Backspace); }
        f.on_key(WidgetKey::Char('9'));
        f.on_key(WidgetKey::Char('9'));
        f.on_key(WidgetKey::Char('9'));
        f.on_key(WidgetKey::Enter);
        if let FormField::Number { value, .. } = &f.fields[0] {
            assert!((*value - 300.0).abs() < 1e-9, "clamped to max, got {value}");
        }
    }

    #[test]
    fn realtime_form_live_edit_and_add() {
        let mut f = RealtimeForm::new(0.0, 0.0, 320.0, "Eff", vec![
            ("gain".into(), RTControl::Number { value: 0.0, step: 0.1, min: -10.0, max: 10.0, last_x: 0.0, buf: None }),
            ("wet".into(), RTControl::Slider { value: 0.5 }),
            ("mute".into(), RTControl::Toggle { on: false, anim: 0.0, dir: -1.0 }),
            ("name".into(), RTControl::Text { value: "fx".into(), insert: 2, caret: 0.0 }),
            ("mode".into(), RTControl::Combo { items: vec!["a".into(), "b".into()], selected: 0, open: false }),
        ]);
        // 标题 + 5 行 + Add
        assert_eq!(f.areas().len(), 7);
        // Number 行拖拽实时改值:按下起点 → 拖 +20px → +2
        let r0 = f.value_rect(0);
        let (y0, x_start) = (r0.y() + 1.0, r0.x() + 10.0);
        f.on_click((x_start, y0));
        assert_eq!(f.focus_row, Some(0));
        f.on_drag((x_start + 20.0, y0));
        if let (_, RTControl::Number { value, .. }) = &f.rows[0] {
            assert!((*value - 2.0).abs() < 1e-9, "drag +20px × step 0.1 = +2, got {value}");
        }
        // 滚轮步进:+3 × step 0.1 = +0.3
        f.on_wheel(3.0);
        if let (_, RTControl::Number { value, .. }) = &f.rows[0] {
            assert!((*value - 2.3).abs() < 1e-9, "wheel +3 × step 0.1 = +0.3, got {value}");
        }
        // Slider 拖拽
        let r1 = f.value_rect(1);
        f.on_drag((r1.x() + r1.width() * 0.25, r1.y() + 1.0));
        if let (_, RTControl::Slider { value }) = &f.rows[1] {
            assert!((*value - 0.25).abs() < 0.01);
        }
        // Toggle 点击
        f.on_click(center(f.row_rect(2)));
        if let (_, RTControl::Toggle { on, .. }) = &f.rows[2] {
            assert!(*on);
        }
        // Text 键盘
        f.on_click(center(f.row_rect(3)));
        f.on_key(WidgetKey::Char('!'));
        if let (_, RTControl::Text { value, insert, .. }) = &f.rows[3] {
            assert_eq!(value, "fx!");
            assert_eq!(*insert, 3);
        }
        // Combo 展开/选择
        f.on_click(center(f.row_rect(4)));
        f.on_click(center(f.combo_item_rect(4, 1)));
        if let (_, RTControl::Combo { selected, open, .. }) = &f.rows[4] {
            assert_eq!(*selected, 1);
            assert!(!*open);
        }
        // Add 按钮增行
        f.on_click(center(f.add_rect()));
        assert_eq!(f.rows.len(), 6);
        if let (label, RTControl::Number { .. }) = &f.rows[5] {
            assert!(label.starts_with("var"));
        }
        // 删除行(焦点行之后的行删除 → 焦点前移)
        f.focus_row = Some(4);
        f.remove_row(3);
        assert_eq!(f.rows.len(), 5);
        assert_eq!(f.focus_row, Some(3));
        // 删除焦点行本身 → 焦点清除
        f.focus_row = Some(3);
        f.remove_row(3);
        assert_eq!(f.rows.len(), 4);
        assert_eq!(f.focus_row, None);
    }

    #[test]
    fn realtime_form_number_keyboard_input() {
        let mut f = RealtimeForm::new(0.0, 0.0, 320.0, "Eff", vec![
            ("gain".into(), RTControl::Number { value: 1.0, step: 0.1, min: 0.0, max: 10.0, last_x: 0.0, buf: None }),
        ]);
        // 聚焦 Number 行
        f.on_click(center(f.value_rect(0)));
        assert_eq!(f.focus_row, Some(0));
        // 打字:buf 初始化为当前值,追加数字
        f.on_key(WidgetKey::Char('2'));
        f.on_key(WidgetKey::Char('5'));
        if let (_, RTControl::Number { value, buf, .. }) = &f.rows[0] {
            assert_eq!(buf.as_deref(), Some("1.00025"));
            assert!((*value - 1.0).abs() < 1e-9); // 值未提交
        }
        // 退格
        f.on_key(WidgetKey::Backspace);
        if let (_, RTControl::Number { buf, .. }) = &f.rows[0] {
            assert_eq!(buf.as_deref(), Some("1.0002"));
        }
        // Enter 提交
        f.on_key(WidgetKey::Enter);
        if let (_, RTControl::Number { value, buf, .. }) = &f.rows[0] {
            assert!((*value - 1.0002).abs() < 1e-9);
            assert!(buf.is_none());
        }
        // 焦点清除
        assert_eq!(f.focus_row, None);
        // 直接输入数值(新 buf 从当前值开始)+ Esc 取消
        f.on_click(center(f.value_rect(0)));
        f.on_key(WidgetKey::Char('9'));
        if let (_, RTControl::Number { buf, .. }) = &f.rows[0] {
            assert_eq!(buf.as_deref(), Some("1.0009"));
        }
        f.on_key(WidgetKey::Escape);
        if let (_, RTControl::Number { value, buf, .. }) = &f.rows[0] {
            assert!((*value - 1.0002).abs() < 1e-9);
            assert!(buf.is_none());
        }
    }

    #[test]
    fn widget_areas_hit_consistent() {
        // 通用一致性:每个组件的每个区域,中心点必须命中到同 kind+id 的区域。
        let widgets: Vec<Box<dyn Widget>> = vec![
            Box::new(VList::new(0.0, 0.0, 100.0, 3).with_row_h(22.0)),
            Box::new(HList::new(0.0, 0.0, 120.0, 20.0, 3).with_gap(4.0)),
            Box::new(Button::new(0.0, 0.0, 80.0, 22.0, "B")),
            Box::new(Toggle::new(0.0, 0.0, 120.0, 22.0, "T", true)),
            Box::new(Slider::new(0.0, 0.0, 100.0, 18.0, 0.4)),
            Box::new(Field::new(0.0, 0.0, 160.0, 22.0, "f", "v")),
            Box::new(ScrollList::new(0.0, 0.0, 140.0, 8, 3)),
            Box::new(Panel::new(0.0, 0.0, 200.0, "P", vec![
                PanelRow::Button { label: "x".into(), enabled: true },
                PanelRow::Toggle { label: "y".into(), on: false, anim: 0.0, dir: -1.0 },
            ])),
            Box::new(ComboBox::new(0.0, 0.0, 120.0, 22.0, vec!["a".into(), "b".into()])),
            Box::new(Stepper::new(0.0, 0.0, 200.0, 22.0, "s", 1.0)),
            Box::new(Checkbox::new(0.0, 0.0, 160.0, 22.0, "c", true)),
            Box::new(TabBar::new(0.0, 0.0, 240.0, 24.0, vec!["A".into(), "B".into()])),
            Box::new(ProgressBar::new(0.0, 0.0, 200.0, 18.0, "p", 0.5)),
            Box::new(KeyValueGrid::new(0.0, 0.0, 240.0, vec![
                ("k".into(), "v".into()),
                ("k2".into(), "v2".into()),
            ])),
            Box::new(TextInput::new(0.0, 0.0, 200.0, 24.0, "ph")),
            Box::new(DragValue::new(0.0, 0.0, 200.0, 24.0, "d", 1.0)),
            Box::new(ColorPicker::new(0.0, 0.0, 240.0, [0.2, 0.5, 0.8])),
            Box::new(ListBox::new(0.0, 0.0, 200.0, vec!["a".into(), "b".into(), "c".into()])),
            Box::new(Form::new(0.0, 0.0, 300.0, "F", vec![
                FormField::Text { label: "t".into(), value: String::new(), insert: 0, caret: 0.0 },
                FormField::Toggle { label: "g".into(), on: false, anim: 0.0, dir: -1.0 },
            ])),
            Box::new(RealtimeForm::new(0.0, 0.0, 320.0, "RT", vec![
                ("n".into(), RTControl::Number { value: 0.0, step: 0.1, min: -1.0, max: 1.0, last_x: 0.0, buf: None }),
                ("s".into(), RTControl::Slider { value: 0.5 }),
            ])),
        ];
        for w in &widgets {
            let areas = w.areas();
            assert!(!areas.is_empty(), "empty areas");
            for a in &areas {
                let hit = w.hit_area(center(a.rect)).unwrap_or_else(|| panic!("no hit at center {:?}", a));
                assert_eq!((hit.kind, hit.id), (a.kind, a.id), "center of {:?}", a);
            }
        }
    }

    // ── Form/RealtimeForm Number 键盘编辑(PMCORE-60)──

    #[test]
    fn form_number_typing_then_enter_commits_and_advances() {
        let mut f = Form::new(0.0, 0.0, 300.0, "F", vec![
            FormField::Number { label: "n".into(), value: 1.0, min: 0.0, max: 10.0, buf: None },
            FormField::Text { label: "t".into(), value: String::new(), insert: 0, caret: 0.0 },
        ]);
        f.focus_row = Some(0);
        // 打字 → buf 出现(初始为当前值的 "1.000",再 push '1')
        f.on_key(WidgetKey::Char('1'));
        let FormField::Number { buf, .. } = &f.fields[0] else { panic!("field 0 must be Number") };
        assert_eq!(buf.as_deref(), Some("1.0001"));
        // Enter → 提交并前进焦点
        f.on_key(WidgetKey::Enter);
        let FormField::Number { value, buf, .. } = &f.fields[0] else { panic!("field 0 must be Number") };
        assert!((*value - 1.0001).abs() < 1e-9, "value {value}");
        assert!(buf.is_none());
        assert_eq!(f.focus_row, Some(1));
    }

    #[test]
    fn form_number_boundary_cases() {
        // 非法字符 'abc' 被忽略,Enter 后值不变
        let mut f = Form::new(0.0, 0.0, 300.0, "F", vec![
            FormField::Number { label: "n".into(), value: 3.0, min: 0.0, max: 10.0, buf: None },
        ]);
        f.focus_row = Some(0);
        for c in ['a', 'b', 'c'] {
            f.on_key(WidgetKey::Char(c));
        }
        f.on_key(WidgetKey::Enter);
        let FormField::Number { value, buf, .. } = &f.fields[0] else { panic!("field 0 must be Number") };
        assert!((*value - 3.0).abs() < 1e-12, "value {value}");
        assert!(buf.is_none());

        // 超 max 被 clamp:value=5.0=max,输入 '9' → buf "5.0009" 解析后 5.0009 > 5.0 → 钳回 5.0
        let mut f = Form::new(0.0, 0.0, 300.0, "F", vec![
            FormField::Number { label: "n".into(), value: 5.0, min: 0.0, max: 5.0, buf: None },
        ]);
        f.focus_row = Some(0);
        f.on_key(WidgetKey::Char('9'));
        f.on_key(WidgetKey::Enter);
        let FormField::Number { value, .. } = &f.fields[0] else { panic!("field 0 must be Number") };
        assert_eq!(*value, 5.0, "若未 clamp 会是 5.0009");

        // 低于 min 被 clamp:value=1.0 < min=2.0,输入 '9' → 1.0009 → 钳到 2.0
        let mut f = Form::new(0.0, 0.0, 300.0, "F", vec![
            FormField::Number { label: "n".into(), value: 1.0, min: 2.0, max: 10.0, buf: None },
        ]);
        f.focus_row = Some(0);
        f.on_key(WidgetKey::Char('9'));
        f.on_key(WidgetKey::Enter);
        let FormField::Number { value, .. } = &f.fields[0] else { panic!("field 0 must be Number") };
        assert_eq!(*value, 2.0);

        // buf 空(未输入)Enter → 值不变,仅焦点前进
        let mut f = Form::new(0.0, 0.0, 300.0, "F", vec![
            FormField::Number { label: "n".into(), value: 7.0, min: 0.0, max: 10.0, buf: None },
            FormField::Text { label: "t".into(), value: String::new(), insert: 0, caret: 0.0 },
        ]);
        f.focus_row = Some(0);
        f.on_key(WidgetKey::Enter);
        let FormField::Number { value, buf, .. } = &f.fields[0] else { panic!("field 0 must be Number") };
        assert!((*value - 7.0).abs() < 1e-12, "value {value}");
        assert!(buf.is_none());
        assert_eq!(f.focus_row, Some(1));

        // Escape 清 buf,值不变、焦点不动
        let mut f = Form::new(0.0, 0.0, 300.0, "F", vec![
            FormField::Number { label: "n".into(), value: 4.0, min: 0.0, max: 10.0, buf: None },
        ]);
        f.focus_row = Some(0);
        f.on_key(WidgetKey::Char('1'));
        f.on_key(WidgetKey::Escape);
        let FormField::Number { value, buf, .. } = &f.fields[0] else { panic!("field 0 must be Number") };
        assert!((*value - 4.0).abs() < 1e-12, "value {value}");
        assert!(buf.is_none());
        assert_eq!(f.focus_row, Some(0));
    }

    #[test]
    fn form_number_backspace_and_enter_wraps() {
        let mut f = Form::new(0.0, 0.0, 300.0, "F", vec![
            FormField::Number { label: "n".into(), value: 1.0, min: 0.0, max: 10.0, buf: None },
            FormField::Text { label: "t".into(), value: String::new(), insert: 0, caret: 0.0 },
        ]);
        f.focus_row = Some(0);
        // 输入 '1' 再 Backspace → 回到初始 buf "1.000",Enter 提交后值不变
        f.on_key(WidgetKey::Char('1'));
        f.on_key(WidgetKey::Backspace);
        let FormField::Number { buf, .. } = &f.fields[0] else { panic!("field 0 must be Number") };
        assert_eq!(buf.as_deref(), Some("1.000"));
        f.on_key(WidgetKey::Enter);
        let FormField::Number { value, .. } = &f.fields[0] else { panic!("field 0 must be Number") };
        assert!((*value - 1.0).abs() < 1e-12, "value {value}");
        assert_eq!(f.focus_row, Some(1));
        // 连续第二次 Enter:焦点 1 → 0(rem_euclid 循环回绕)
        f.on_key(WidgetKey::Enter);
        assert_eq!(f.focus_row, Some(0));
    }

    #[test]
    fn realtime_form_number_keyboard_edit() {
        let mut f = RealtimeForm::new(0.0, 0.0, 320.0, "RT", vec![
            ("n".into(), RTControl::Number { value: 1.0, step: 0.1, min: 0.0, max: 10.0, last_x: 0.0, buf: None }),
        ]);
        f.focus_row = Some(0);
        // 打字 → buf;Enter → 提交且失焦(与 Form 的焦点前进不同)
        f.on_key(WidgetKey::Char('1'));
        let (_, RTControl::Number { buf, .. }) = &f.rows[0] else { panic!("row 0 must be Number") };
        assert_eq!(buf.as_deref(), Some("1.0001"));
        f.on_key(WidgetKey::Enter);
        let (_, RTControl::Number { value, buf, .. }) = &f.rows[0] else { panic!("row 0 must be Number") };
        assert!((*value - 1.0001).abs() < 1e-9, "value {value}");
        assert!(buf.is_none());
        assert_eq!(f.focus_row, None);
    }

    #[test]
    fn realtime_form_number_boundaries_and_backspace() {
        let mut f = RealtimeForm::new(0.0, 0.0, 320.0, "RT", vec![
            ("n".into(), RTControl::Number { value: 1.0, step: 0.1, min: 2.0, max: 5.0, last_x: 0.0, buf: None }),
        ]);
        f.focus_row = Some(0);
        // 非法字符忽略;Escape 清 buf,值不变
        f.on_key(WidgetKey::Char('a'));
        f.on_key(WidgetKey::Escape);
        let (_, RTControl::Number { buf, value, .. }) = &f.rows[0] else { panic!("row 0 must be Number") };
        assert!(buf.is_none());
        assert!((*value - 1.0).abs() < 1e-12, "value {value}");
        // 输入 '9' → "1.0009" < min=2.0 → Enter 后 clamp 到 2.0;Backspace 删尾字符
        f.on_key(WidgetKey::Char('9'));
        f.on_key(WidgetKey::Backspace);
        let (_, RTControl::Number { buf, .. }) = &f.rows[0] else { panic!("row 0 must be Number") };
        assert_eq!(buf.as_deref(), Some("1.000"));
        f.on_key(WidgetKey::Char('9'));
        f.on_key(WidgetKey::Enter);
        let (_, RTControl::Number { value, buf, .. }) = &f.rows[0] else { panic!("row 0 must be Number") };
        assert_eq!(*value, 2.0, "clamp 生效");
        assert!(buf.is_none());
        assert_eq!(f.focus_row, None); // Enter 失焦
    }

    // ── TextInput/Form/RealtimeForm 点击定位光标(PMCORE-61)──

    /// 假度量表:半角 10px、全角/CJK 20px(全角字符按字符边界定位,不按字节)。
    fn fake_measure(s: &str) -> f32 {
        s.chars().map(|c| if c as u32 > 0x7F { 20.0 } else { 10.0 }).sum()
    }

    #[test]
    fn textinput_click_positions_caret_by_text_width() {
        let mut t = TextInput::new(0.0, 0.0, 200.0, 24.0, "");
        t.text = "ab界c".into(); // 宽度 10,10,20,10 → 边界 0,10,20,40,50
        let measure = |s: &str| fake_measure(s);
        // 左缘 → 0
        t.on_click_with_measure((8.0, 12.0), 8.0, &measure);
        assert!(t.focused);
        assert_eq!(t.insert, 0);
        // 各字符间隙:点中边界 → 落在右侧字符前(与 draw 光标位置一致)
        t.on_click_with_measure((8.0 + 10.0, 12.0), 8.0, &measure);
        assert_eq!(t.insert, 1);
        t.on_click_with_measure((8.0 + 20.0, 12.0), 8.0, &measure);
        assert_eq!(t.insert, 2); // 全角"界"之后
        t.on_click_with_measure((8.0 + 40.0, 12.0), 8.0, &measure);
        assert_eq!(t.insert, 3);
        // 右缘 → chars 数
        t.on_click_with_measure((8.0 + 50.0, 12.0), 8.0, &measure);
        assert_eq!(t.insert, 4);
        // 中间点:27 → 距 20(7)/40(13) → 2
        t.on_click_with_measure((8.0 + 27.0, 12.0), 8.0, &measure);
        assert_eq!(t.insert, 2);
        // pad 左侧点击 → 钳到 0
        t.on_click_with_measure((4.0, 12.0), 8.0, &measure);
        assert_eq!(t.insert, 0);
    }

    #[test]
    fn textinput_click_overflow_snaps_to_end() {
        // 文本 10 字符 × 10px = 100,可用宽 60-8 = 52 → 超宽
        let mut t = TextInput::new(0.0, 0.0, 60.0, 24.0, "");
        t.text = "abcdefghij".into();
        let measure = |s: &str| s.chars().count() as f32 * 10.0;
        // 点击最右(框右缘)→ insert = len,不再右移
        t.on_click_with_measure((59.5, 12.0), 8.0, &measure);
        assert_eq!(t.insert, 10);
        // 可见区中间(36 → 距 40 比 30 近)→ 正常最近边界
        t.on_click_with_measure((8.0 + 36.0, 12.0), 8.0, &measure);
        assert_eq!(t.insert, 4);
        // 空文本点击 → insert=0(placeholder 态 focus + 0)
        let mut e = TextInput::new(0.0, 0.0, 60.0, 24.0, "ph");
        e.on_click_with_measure((30.0, 12.0), 8.0, &measure);
        assert!(e.focused);
        assert_eq!(e.insert, 0);
    }

    /// 假画布:记录窄竖线填充(光标)的 x,与点击测距用同一度量表。
    struct FakeCanvas {
        caret_x: Option<f32>,
    }
    impl Canvas for FakeCanvas {
        fn fill(&mut self, r: Rect, _rgba: [u8; 4]) {
            if r.width() <= 2.0 && r.height() > 4.0 {
                self.caret_x = Some(r.x());
            }
        }
        fn text(&mut self, _s: &str, _x: f32, _y: f32, _size: f32, _rgb: [u8; 3]) {}
        fn text_width(&mut self, s: &str, _size: f32) -> f32 {
            fake_measure(s)
        }
    }

    #[test]
    fn textinput_click_and_draw_caret_same_measure() {
        // 点击定位与 draw 光标使用同一 text_width 来源:点击边界 x → insert i,
        // 该 insert 的光标也画在同一边界 x(差一个光标自身 1px 偏移)。
        let mut t = TextInput::new(0.0, 0.0, 200.0, 24.0, "");
        t.text = "a界bc".into(); // 宽度 10,20,10,10 → 边界 0,10,30,40,50
        let measure = |s: &str| fake_measure(s);
        t.on_click_with_measure((8.0 + 30.0, 12.0), 8.0, &measure);
        assert_eq!(t.insert, 2); // "界"之后
        // draw 光标 x = x + pad_x + text_width(前 2 字符)+ 1 = 0+8+30+1
        let mut cv = FakeCanvas { caret_x: None };
        t.draw(&mut cv, &Theme::default(), None);
        assert_eq!(cv.caret_x, Some(39.0));
        // 点中光标线本身 → 仍落回同一边界
        t.on_click_with_measure((39.0, 12.0), 8.0, &measure);
        assert_eq!(t.insert, 2);
    }

    #[test]
    fn form_text_row_click_positions_caret_in_value_column() {
        let measure = |s: &str| fake_measure(s);
        let mut f = Form::new(0.0, 0.0, 300.0, "F", vec![
            FormField::Text { label: "name".into(), value: "hello".into(), insert: 0, caret: 0.0 },
        ]);
        // 值列文本起点 = row.x + 90 + pad_x = 98;点 98+20(h 与 e 之间)→ insert=2
        f.on_click_with_measure((98.0 + 20.0, 30.0), 8.0, &measure);
        assert_eq!(f.focus_row, Some(0));
        let FormField::Text { insert, .. } = &f.fields[0] else { panic!("row 0 must be Text") };
        assert_eq!(*insert, 2);
        // 点击 label 列(< 90)→ 只设焦点,不定位
        f.on_click_with_measure((10.0, 30.0), 8.0, &measure);
        assert_eq!(f.focus_row, Some(0));
        let FormField::Text { insert, .. } = &f.fields[0] else { panic!("row 0 must be Text") };
        assert_eq!(*insert, 2);
        // on_click(无度量)→ 保持旧行为:只设焦点,不定位
        f.on_click(center(f.row_rect(0)));
        let FormField::Text { insert, .. } = &f.fields[0] else { panic!("row 0 must be Text") };
        assert_eq!(*insert, 2);
    }

    #[test]
    fn realtime_form_text_row_click_positions_caret_in_value_column() {
        let measure = |s: &str| fake_measure(s);
        let mut f = RealtimeForm::new(0.0, 0.0, 320.0, "RT", vec![
            ("name".into(), RTControl::Text { value: "hi界".into(), insert: 0, caret: 0.0 }),
        ]);
        // 值列文本起点 = 98;"hi界" 宽度 10,10,20 → 边界 0,10,20,40;点 98+20(界 之前)→ 2
        f.on_click_with_measure((98.0 + 20.0, 30.0), 8.0, &measure);
        assert_eq!(f.focus_row, Some(0));
        let (_, RTControl::Text { insert, .. }) = &f.rows[0] else { panic!("row 0 must be Text") };
        assert_eq!(*insert, 2);
        // label 列 → 只设焦点
        f.on_click_with_measure((5.0, 30.0), 8.0, &measure);
        let (_, RTControl::Text { insert, .. }) = &f.rows[0] else { panic!("row 0 must be Text") };
        assert_eq!(*insert, 2);
        // 值列右缘 → chars 数
        f.on_click_with_measure((98.0 + 40.0, 30.0), 8.0, &measure);
        let (_, RTControl::Text { insert, .. }) = &f.rows[0] else { panic!("row 0 must be Text") };
        assert_eq!(*insert, 3);
    }

    // ── RadioGroup / CheckboxGroup / SearchableCombo ──

    #[test]
    fn radiogroup_exclusive_and_default() {
        let mut rg = RadioGroup::new(0.0, 0.0, 200.0, 24.0, vec!["a".into(), "b".into(), "c".into()], 1);
        assert_eq!(rg.selected(), 1);
        assert_eq!(rg.areas().len(), 3);
        // 点第 3 行 → 选中 2,其余失选
        rg.on_click(center(rg.row_rect(2)));
        assert_eq!(rg.selected(), 2);
        assert!(!rg.rows[1].1);
        assert!(!rg.rows[0].1);
        // 点击空白(行外)→ 不变
        rg.on_click((150.0, -5.0));
        assert_eq!(rg.selected(), 2);
        // 命中:行 kind = RadioRow,id = 行索引
        assert_eq!(rg.hit_area(center(rg.row_rect(0))).unwrap().kind, AreaKind::RadioRow);
        assert_eq!(rg.hit_area(center(rg.row_rect(0))).unwrap().id, 0);
        // 默认 selected 越界 → 钳到末行
        let rg2 = RadioGroup::new(0.0, 0.0, 200.0, 24.0, vec!["a".into()], 99);
        assert_eq!(rg2.selected(), 0);
    }

    #[test]
    fn radiogroup_click_reports_change() {
        let mut rg = RadioGroup::new(0.0, 0.0, 200.0, 24.0, vec!["a".into(), "b".into()], 0);
        assert!(!rg.click(center(rg.row_rect(0)))); // 重复点当前项 → 不变
        assert!(rg.click(center(rg.row_rect(1)))); // 切到 1 → 变更
        assert_eq!(rg.selected(), 1);
        assert!(!rg.click((0.0, 100.0))); // 空白 → 不变
    }

    #[test]
    fn checkboxgroup_multi_toggle_and_none() {
        let mut cg = CheckboxGroup::new(0.0, 0.0, 200.0, 24.0, vec!["a".into(), "b".into(), "c".into()], vec![0, 2]);
        assert_eq!(cg.checked(), vec![0, 2]);
        cg.on_click(center(cg.row_rect(1)));
        assert_eq!(cg.checked(), vec![0, 1, 2]);
        cg.toggle(0);
        assert_eq!(cg.checked(), vec![1, 2]);
        // 全不选
        cg.toggle(1);
        cg.toggle(2);
        assert!(cg.checked().is_empty());
        // 越界 toggle 不 panic
        cg.toggle(99);
        assert!(cg.checked().is_empty());
        // 空白点击不变
        cg.on_click((0.0, 100.0));
        assert!(cg.checked().is_empty());
        // 命中:复用 Checkbox 语义,id = 行索引
        assert_eq!(cg.hit_area(center(cg.row_rect(0))).unwrap().kind, AreaKind::Checkbox);
        assert_eq!(cg.hit_area(center(cg.row_rect(0))).unwrap().id, 0);
    }

    #[test]
    fn searchablecombo_filter_case_insensitive_substring() {
        let mut sc = SearchableCombo::new(0.0, 0.0, 200.0, 24.0, vec![
            "Grayscale".into(), "Vignette".into(), "Chromatic".into(), "Bloom".into(),
        ]);
        // 收起:单区域(主按钮),kind = ComboBoxButton
        assert_eq!(sc.areas().len(), 1);
        assert_eq!(sc.areas()[0].kind, AreaKind::ComboBoxButton);
        // 点主按钮展开;展开后搜索行 kind = TextInput(宿主据此给键盘焦点)
        sc.on_click((100.0, 12.0));
        assert!(sc.open);
        assert_eq!(sc.areas()[0].kind, AreaKind::TextInput);
        // 大小写不敏感子串过滤('G' 同时命中 Grayscale 与 Vignette)
        sc.on_key(WidgetKey::Char('G'));
        assert_eq!(sc.visible(), vec![0, 1]);
        sc.on_key(WidgetKey::Backspace);
        sc.on_key(WidgetKey::Char('r'));
        assert_eq!(sc.visible(), vec![0, 2]); // Grayscale, Chromatic
        // 无匹配 → 空态:选项区为空,Enter 无操作
        sc.filter = "zzz".into();
        assert!(sc.visible().is_empty());
        sc.on_key(WidgetKey::Enter);
        assert!(sc.open);
        assert_eq!(sc.selected(), None);
    }

    #[test]
    fn searchablecombo_enter_selects_original_index() {
        let mut sc = SearchableCombo::new(0.0, 0.0, 200.0, 24.0, vec![
            "Alpha".into(), "Beta".into(), "AlphaMax".into(),
        ]);
        // 过滤出 [Alpha(0), AlphaMax(2)],高亮 0 → Enter 选中原始索引 0
        sc.open_up();
        sc.on_key(WidgetKey::Char('a'));
        sc.on_key(WidgetKey::Char('l')); // "al" → [0, 2]
        assert_eq!(sc.visible(), vec![0, 2]);
        sc.on_key(WidgetKey::Enter);
        assert_eq!(sc.selected(), Some(0));
        assert!(!sc.open);
        // 重新打开,Down 导航到第 2 项(原始 2)→ Enter 选中
        sc.open_up();
        sc.on_key(WidgetKey::Char('a'));
        sc.on_key(WidgetKey::Char('l'));
        sc.on_key(WidgetKey::Down);
        sc.on_key(WidgetKey::Enter);
        assert_eq!(sc.selected(), Some(2));
    }

    #[test]
    fn searchablecombo_arrows_clamp_and_esc() {
        let mut sc = SearchableCombo::new(0.0, 0.0, 200.0, 24.0, vec!["a".into(), "b".into(), "c".into()]);
        sc.open_up();
        sc.on_key(WidgetKey::Down);
        assert_eq!(sc.highlighted, 1);
        sc.on_key(WidgetKey::Down);
        sc.on_key(WidgetKey::Down); // 越界 → 钳到末项
        assert_eq!(sc.highlighted, 2);
        sc.on_key(WidgetKey::Up);
        assert_eq!(sc.highlighted, 1);
        // 过滤变化后高亮回到 0
        sc.on_key(WidgetKey::Char('b'));
        assert_eq!(sc.visible(), vec![1]);
        assert_eq!(sc.highlighted, 0);
        // Esc 关闭并清空过滤
        sc.on_key(WidgetKey::Escape);
        assert!(!sc.open);
        assert!(sc.filter.is_empty());
        // 点选项选中原始索引(过滤 [c] → 唯一项 id=3 → 原始 2)
        sc.open_up();
        sc.filter = "c".into();
        sc.on_click(center(sc.option_rect(0)));
        assert_eq!(sc.selected(), Some(2));
        assert!(!sc.open);
    }

    #[test]
    fn searchablecombo_blur_and_typing_from_closed() {
        let mut sc = SearchableCombo::new(0.0, 0.0, 200.0, 24.0, vec!["a".into(), "b".into()]);
        // 失焦关闭
        sc.open_up();
        sc.set_focus(false);
        assert!(!sc.open);
        // 关闭态直接打字 → 自动打开并输入
        sc.on_key(WidgetKey::Char('b'));
        assert!(sc.open);
        assert_eq!(sc.visible(), vec![1]);
        // 未打开也未聚焦 → 非打字键忽略
        let mut idle = SearchableCombo::new(0.0, 0.0, 200.0, 24.0, vec!["a".into()]);
        idle.on_key(WidgetKey::Escape);
        assert!(!idle.open);
        assert!(idle.filter.is_empty());
    }
}



