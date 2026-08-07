//! UI 流控层(单一真源):命中几何、坐标映射、手写 rect/命中代码统一收敛。
//!
//! 结构:
//! - [`UIFlow`]:每帧状态机。事件层只经 [`UIFlow::on_mouse`] 写入(按下/释放/移动/
//!   修饰键),消费层每帧调 [`UIFlow::resolve`] 取命中/按下/释放/点击事件。
//!   [`UIFlow::hover`] 为绘制高亮唯一依据 —— 面板不再自己重算命中。
//! - [`PanelTransform`]:窗口像素 → 音符面板局部 → (beat, position_x) 的完整映射链
//!   (含 zoom/scroll/clamp/分栏),单一真源。mania 批次已建的共享映射函数
//!   (mod.rs 的 `notes_play_rect`/`panel_y_to_beat`/`beat_to_panel_y`/
//!   `panel_x_to_pos_x`/`pos_x_to_panel_x`/`pos_x_to_col_x`)在 Task B 迁移时
//!   改为委托本结构,公式保持一致。
//!
//! 约定(事件层与消费层共同遵守):
//! - [`ButtonState`] 由事件层写入(快照式:每次事件传全量 3 键状态),消费层只读。
//!   流控内部按状态变化检测边沿,重复快照不会重复触发。
//! - 命中 = 反向遍历 [`UIFlow::areas`](后注册者在上层,覆盖先注册者);
//!   `disabled` 的区域永不命中。
//! - `captured` 存在时(拖拽中)只命中 captured:hover 钉在其上、释放解析到其上。
//!   因此拖拽释放也会产生 `clicked`(按下+释放均命中同一 area),面板用自身拖拽
//!   状态区分"点击"与"拖拽结束"。
//! - 捕获权归第一个按下的键;其余键的按下/释放正常命中,不夺走拖拽。
//! - `clicked` 只对左键(下标 0)产生;中/右键经 `pressed`/`released` 与
//!   [`UIFlow::buttons`] 表达(如右键菜单直接读 `buttons[2]`)。
//! - 区域注册:`button` 分配单调递增 id(瞬态区域);`area` 显式 id upsert
//!   (内容派生 id,如音符索引 —— 布局重建后 hover/点击依然稳定)。

use super::timeline::{HEADER_H, NT_W, PANEL_W, TL_W};
use super::widgets;
use winit::keyboard::ModifiersState;

/// 区域 id。内容派生时(音符索引等)由面板显式传入,保证跨帧稳定。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct AreaId(pub u32);

/// 按键状态:事件层写入,消费层只读。
///
/// 事件层每次事件传全量 3 键快照(`buttons[0..3]` = 左/中/右),按下传
/// [`Pressed`](ButtonState::Pressed)、释放传 [`Released`](ButtonState::Released)、
/// 其余传 [`Up`](ButtonState::Up)。流控按快照差异检测边沿。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonState {
    Up,
    Pressed,
    Released,
}

/// 命中区域的语义类型。widget 组件复用 `super::widgets::AreaKind`
/// (经 [`AreaKind::Widget`] 包装);音符面板的块/空白/幽灵/hold 尾/列线单独成变体。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AreaKind {
    /// 通用 widget 组件区域(Button/Slider/Field/...),复用 widgets.rs 的枚举。
    Widget(widgets::AreaKind),
    /// 音符块(tap/hold/flick/drag)。
    NoteBlock,
    /// 音符面板空白(内容区兜底;注册在最底层,反向命中时最后才落到它)。
    NoteBlank,
    /// 放置/拖拽预览的幽灵音符。
    Ghost,
    /// hold 尾部调整柄。
    HoldTail,
    /// 分栏列线。
    ColumnLine,
}

impl From<widgets::AreaKind> for AreaKind {
    fn from(k: widgets::AreaKind) -> Self {
        AreaKind::Widget(k)
    }
}

/// 注册到流控的一个热区。`rect` 每帧由布局提供(单一真源),绘制/命中共用同一份。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HotArea {
    pub id: AreaId,
    pub rect: (f32, f32, f32, f32),
    pub kind: AreaKind,
    pub disabled: bool,
}

/// 一次 [`UIFlow::resolve`] 产出的帧事件。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlowEvents {
    /// 左键按下+释放均命中的区域(点击;仅左键)。
    pub clicked: Vec<AreaId>,
    /// 本帧按下命中的区域。
    pub pressed: Vec<AreaId>,
    /// 本帧释放命中的区域(拖拽中 = captured 区域)。
    pub released: Vec<AreaId>,
}

/// UI 流控状态机。见模块文档。
pub struct UIFlow {
    /// 当前鼠标位置(最近一次 on_mouse 写入)。
    pub mouse: (f32, f32),
    /// 3 键当前状态(左/中/右),快照式,事件层写入、消费层只读。
    pub buttons: [ButtonState; 3],
    /// 修饰键状态。
    pub mods: ModifiersState,
    /// 本帧命中区域(绘制高亮依据;captured 存在时 = captured)。
    pub hover: Option<AreaId>,
    /// 拖拽捕获:存在时只命中它(悬停钉住、释放解析到它)。
    pub captured: Option<AreaId>,
    /// 每帧注册的热区(布局重建时由面板重建)。
    pub areas: Vec<HotArea>,
    /// 自上次 resolve 以来输入或布局是否有变化(面板据此决定重绘)。
    pub dirty: bool,

    // ── 内部边沿/捕获状态 ──
    prev_buttons: [ButtonState; 3],
    /// 待消费的按下边沿(记录按下时的位置,命中用按下点而非 resolve 时点)。
    press: [Option<(f32, f32)>; 3],
    /// 待消费的释放边沿。
    release: [Option<(f32, f32)>; 3],
    /// 每键按下时命中的区域(点击判定:与释放命中相同)。
    press_hit: [Option<AreaId>; 3],
    /// 持有捕获权的按键下标;`usize::MAX` = 无捕获。
    capture_button: usize,
    next_id: u32,
}

impl Default for UIFlow {
    fn default() -> Self {
        Self::new()
    }
}

impl UIFlow {
    pub fn new() -> Self {
        UIFlow {
            mouse: (0.0, 0.0),
            buttons: [ButtonState::Up; 3],
            mods: ModifiersState::default(),
            hover: None,
            captured: None,
            areas: Vec::new(),
            dirty: false,
            prev_buttons: [ButtonState::Up; 3],
            press: [None; 3],
            release: [None; 3],
            press_hit: [None; 3],
            capture_button: usize::MAX,
            next_id: 0,
        }
    }

    /// 事件层唯一入口:MouseInput / CursorMoved / ModifiersChanged 统一写入。
    /// `buttons` 为当前 3 键状态快照(左/中/右);流控按状态变化检测按下/释放边沿,
    /// 并记录边沿发生时的鼠标位置,resolve 用该位置命中(而非 resolve 时点)。
    pub fn on_mouse(&mut self, x: f32, y: f32, buttons: &[ButtonState; 3], mods: ModifiersState) {
        self.mouse = (x, y);
        self.mods = mods;
        for i in 0..3 {
            let c = buttons[i];
            if c != self.prev_buttons[i] {
                self.prev_buttons[i] = c;
                self.buttons[i] = c;
                match c {
                    ButtonState::Pressed => self.press[i] = Some((x, y)),
                    ButtonState::Released => self.release[i] = Some((x, y)),
                    ButtonState::Up => {}
                }
            }
        }
        self.dirty = true;
    }

    /// 每帧调用:按 [`Self::areas`] 顺序命中(反向 = 上层覆盖),更新 hover;
    /// 消费按下/释放边沿,返回本帧 pressed/released/clicked。
    pub fn resolve(&mut self) -> FlowEvents {
        let mut ev = FlowEvents::default();
        // hover:captured 优先(拖拽中钉在 captured),否则反向命中当前鼠标点。
        self.hover = match self.captured {
            Some(c) if self.alive(c) => Some(c),
            _ => self.hit_test(self.mouse),
        };
        // 按下:命中处成为 captured(拖拽接管);未命中则作废该键按下记录。
        // 已被其他键持有的捕获不被夺走(右键菜单不影响左键拖拽)。
        for i in 0..3 {
            if let Some(p) = self.press[i].take() {
                match self.hit_test(p) {
                    Some(id) => {
                        self.captured = Some(id);
                        self.capture_button = i;
                        self.press_hit[i] = Some(id);
                        ev.pressed.push(id);
                    }
                    None => {
                        self.press_hit[i] = None;
                        if self.capture_button == i {
                            self.captured = None;
                        }
                    }
                }
            }
        }
        // 释放:capturing 键解析到 captured(拖拽中必然命中),其余键正常命中。
        // 左键按下+释放同 area → clicked。
        for i in 0..3 {
            if let Some(r) = self.release[i].take() {
                let target = if i == self.capture_button {
                    self.captured.filter(|&c| self.alive(c))
                } else {
                    self.hit_test(r)
                };
                if let Some(id) = target {
                    ev.released.push(id);
                    if i == 0 && self.press_hit[i] == Some(id) {
                        ev.clicked.push(id);
                    }
                }
                self.press_hit[i] = None;
                if i == self.capture_button {
                    self.captured = None;
                    self.capture_button = usize::MAX;
                }
            }
        }
        self.dirty = false;
        ev
    }

    /// 声明式注册(瞬态/自动 id):追加一个启用热区,返回单调递增的 id。
    /// 需要跨帧稳定的区域(音符等)请用 [`Self::area`] 传内容派生 id。
    pub fn button(&mut self, rect: (f32, f32, f32, f32), kind: AreaKind) -> AreaId {
        let id = AreaId(self.next_id);
        self.next_id += 1;
        self.areas.push(HotArea { id, rect, kind, disabled: false });
        self.dirty = true;
        id
    }

    /// 显式 id 注册(upsert):同 id 已存在则原位更新 rect/kind/disabled,id 不变。
    /// 音符面板每帧重建 areas 时用音符索引作 id,布局变化后 hover/点击依然稳定。
    pub fn area(&mut self, id: AreaId, rect: (f32, f32, f32, f32), kind: AreaKind, disabled: bool) -> AreaId {
        if let Some(a) = self.areas.iter_mut().find(|a| a.id == id) {
            if a.rect != rect || a.kind != kind || a.disabled != disabled {
                a.rect = rect;
                a.kind = kind;
                a.disabled = disabled;
                self.dirty = true;
            }
        } else {
            self.areas.push(HotArea { id, rect, kind, disabled });
            self.dirty = true;
        }
        id
    }

    /// 丢释放自愈:窗口失焦 / 光标离开窗口 / 拖拽出窗后,OS 不再补发
    /// Released,`prev_buttons` 停在 Pressed → 下一次按下检测不到边沿
    /// (点击静默丢失)、hover 被陈旧的 captured 钉死。重置全部边沿与
    /// 捕获,使下一次按下重新产生 press 边沿。
    pub fn force_release_all(&mut self) {
        self.prev_buttons = [ButtonState::Up; 3];
        self.buttons = [ButtonState::Up; 3];
        self.press = [None; 3];
        self.release = [None; 3];
        self.press_hit = [None; 3];
        self.captured = None;
        self.capture_button = usize::MAX;
        self.dirty = true;
    }

    /// 纯命中测试(事件层/面板共用):返回 `p` 命中的区域 id;不受 captured
    /// 影响(面板点击/滚轮用当前点或释放点命中,拖拽中也能取真实位置)。
    pub fn hit_at(&self, p: (f32, f32)) -> Option<AreaId> {
        self.hit_test(p)
    }

    /// 反向遍历 areas,返回第一个包含 `p` 且未禁用的区域。
    fn hit_test(&self, p: (f32, f32)) -> Option<AreaId> {
        self.areas
            .iter()
            .rev()
            .find(|a| !a.disabled && inside(a.rect, p.0, p.1))
            .map(|a| a.id)
    }

    /// captured 区域是否仍注册且未禁用(被删除/禁用则回落普通命中)。
    fn alive(&self, id: AreaId) -> bool {
        self.areas.iter().any(|a| a.id == id && !a.disabled)
    }
}

/// 半开区间命中:右/下缘不命中(与 tiny_skia Rect 一致)。
fn inside(r: (f32, f32, f32, f32), mx: f32, my: f32) -> bool {
    let (x, y, w, h) = r;
    mx >= x && mx < x + w && my >= y && my < y + h
}

/// 音符面板坐标映射链(单一真源):窗口像素 → 面板局部 → (beat, position_x)。
///
/// 含 zoom/scroll/clamp/分栏。公式与 mod.rs 的共享映射函数一致
/// (Task B 迁移时由那些函数委托本结构,删除重复实现)。
#[derive(Clone, Copy, Debug)]
pub struct PanelTransform {
    /// 窗口顶拍(tl_scroll)。
    pub scroll: f32,
    /// 窗口拍高(tl_zoom)。
    pub zoom: f32,
    /// 分栏数(vertical_split;<=1 = 单栏线性)。
    pub v_split: u32,
    /// 界面缩放(gui_scale)。
    pub s: f32,
    /// 视口宽。
    pub w: f32,
    /// 视口高。
    pub h: f32,
    /// 音符面板滑入进度(决定 panel_x)。
    pub notes_progress: f32,
    /// 事件面板滑入进度(决定 panel_x:两个时间轴横排,notes 在 events 左侧)。
    pub events_progress: f32,
    /// properties 面板进度(决定 panel_x)。
    pub props_progress: f32,
}

impl PanelTransform {
    pub fn new(
        scroll: f32,
        zoom: f32,
        v_split: u32,
        s: f32,
        w: f32,
        h: f32,
        notes_progress: f32,
        events_progress: f32,
        props_progress: f32,
    ) -> Self {
        PanelTransform { scroll, zoom, v_split, s, w, h, notes_progress, events_progress, props_progress }
    }

    /// 由 (play_x, play_y, play_w, play_h) 反解面板几何(逆 [`Self::notes_play_rect`]),
    /// 供旧共享映射函数委托本结构时重建面板参数,零重复常量。
    /// `progress` 取 0:反向构建的变换再次调用 [`Self::notes_play_rect`] 时
    /// 原样还原传入的 play(往返精确)。
    pub fn from_play(play: (f32, f32, f32, f32), scroll: f32, zoom: f32, v_split: u32) -> Self {
        let s = play.1 / (HEADER_H + 4.0);
        PanelTransform {
            scroll,
            zoom,
            v_split,
            s,
            w: play.0 - 12.0 * s,
            h: play.3 + 56.0 * s + play.1,
            notes_progress: 0.0,
            events_progress: 0.0,
            props_progress: 0.0,
        }
    }

    /// 音符面板左缘 x(命中/绘制/ghost 共用;等价 mod.rs 的 panel_x 链)。
    /// 两个时间轴横排:notes 在 events 左侧,events 在 properties 左侧——
    /// 双面板同开时命中区必须同样左移,否则 notes 区域压在 events 上
    /// (用户实测:event 面板响应 notes 位置)。
    pub fn panel_x(&self) -> f32 {
        let props_x = self.w - self.props_progress * PANEL_W * self.s;
        props_x - self.events_progress * TL_W * self.s - self.notes_progress * NT_W * self.s
    }

    /// 音符面板内容区几何 (play_x, play_y, play_w, play_h)。
    /// play_x = 面板左缘 + 横向 pad;play_y = 头部底;play_h = 可视高。
    /// 与 mod.rs 的 `notes_play_rect` 同源。
    pub fn notes_play_rect(&self) -> (f32, f32, f32, f32) {
        let nt_w = NT_W * self.s;
        let pad_x = 12.0 * self.s;
        let py = HEADER_H * self.s + 4.0 * self.s;
        let ph = (self.h - 56.0 * self.s - py).max(0.0);
        (self.panel_x() + pad_x, py, (nt_w - pad_x * 2.0).max(0.0), ph)
    }

    /// 像素 y → beat(越界钳到可见范围;与 [`Self::beat_to_y`] 互逆)。
    pub fn y_to_beat(&self, my: f32) -> f64 {
        let (_, py, _, ph) = self.notes_play_rect();
        if ph <= 0.0 {
            return self.scroll as f64;
        }
        let ratio = 1.0 - ((my - py) / ph).clamp(0.0, 1.0) as f64;
        self.scroll as f64 + ratio * self.zoom as f64
    }

    /// beat → 像素 y(不 clamp,调用方按需;与 [`Self::y_to_beat`] 互逆)。
    pub fn beat_to_y(&self, beat: f64) -> f32 {
        let (_, py, _, ph) = self.notes_play_rect();
        (py as f64 + ph as f64 - (beat - self.scroll as f64) / self.zoom as f64 * ph as f64) as f32
    }

    /// 像素 x → 音符 position_x(±675 线性;与 [`Self::pos_x_to_x`] 互逆)。
    pub fn x_to_pos_x(&self, mx: f32) -> f32 {
        let (px, _, pw, _) = self.notes_play_rect();
        ((mx - px) / pw * 2.0 - 1.0) * 675.0
    }

    /// 音符 position_x → 像素 x(与 [`Self::x_to_pos_x`] 互逆)。
    pub fn pos_x_to_x(&self, nx: f32) -> f32 {
        let (px, _, pw, _) = self.notes_play_rect();
        px + (nx / 675.0 + 1.0) * pw * 0.5
    }

    /// 分栏列中心 x(vertical_split > 1 时按列显示;命中 = 绘制)。
    pub fn pos_x_to_col_x(&self, nx: f32) -> f32 {
        let (px, _, pw, _) = self.notes_play_rect();
        if self.v_split <= 1 {
            return self.pos_x_to_x(nx);
        }
        let v = self.v_split as f32;
        let col = ((nx / 675.0 + 1.0) * v * 0.5).clamp(0.0, v - 1.0).round() as usize;
        let col = col.min(v as usize - 1);
        px + (col as f32 + 0.5) * pw / v
    }

    /// 组合映射:鼠标点 → (beat, position_x)(y 已 clamp、x 不 clamp)。
    pub fn mouse_beat(&self, mx: f32, my: f32) -> (f64, f32) {
        (self.y_to_beat(my), self.x_to_pos_x(mx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up() -> [ButtonState; 3] {
        [ButtonState::Up, ButtonState::Up, ButtonState::Up]
    }
    fn press_left() -> [ButtonState; 3] {
        [ButtonState::Pressed, ButtonState::Up, ButtonState::Up]
    }
    fn release_left() -> [ButtonState; 3] {
        [ButtonState::Released, ButtonState::Up, ButtonState::Up]
    }
    fn mods() -> ModifiersState {
        ModifiersState::default()
    }

    /// 命中覆盖顺序:后注册者在上层;空白(先注册)兜底;面板外不命中。
    #[test]
    fn hit_prefers_later_registered_area() {
        let mut f = UIFlow::new();
        let blank = f.button((0.0, 0.0, 100.0, 100.0), AreaKind::NoteBlank);
        let note = f.button((20.0, 20.0, 40.0, 40.0), AreaKind::NoteBlock);
        f.on_mouse(30.0, 30.0, &up(), mods());
        f.resolve();
        assert_eq!(f.hover, Some(note), "上层(后注册)覆盖");
        f.on_mouse(5.0, 5.0, &up(), mods());
        f.resolve();
        assert_eq!(f.hover, Some(blank), "空白兜底");
        f.on_mouse(150.0, 150.0, &up(), mods());
        f.resolve();
        assert_eq!(f.hover, None, "面板外不命中");
    }

    /// captured 优先:拖拽中 hover 钉在 captured,释放解析到 captured(拖动释放也算点击)。
    #[test]
    fn captured_keeps_hover_and_release_during_drag() {
        let mut f = UIFlow::new();
        let note = f.button((10.0, 10.0, 40.0, 40.0), AreaKind::NoteBlock);
        let other = f.button((100.0, 100.0, 40.0, 40.0), AreaKind::NoteBlock);
        f.on_mouse(20.0, 20.0, &press_left(), mods());
        let ev = f.resolve();
        assert_eq!(ev.pressed, vec![note]);
        assert_eq!(f.captured, Some(note));
        // 拖到 other 上方(左键仍按住,快照保持 Pressed):hover 不变。
        f.on_mouse(120.0, 120.0, &press_left(), mods());
        let ev = f.resolve();
        assert_eq!(f.hover, Some(note), "拖拽中 hover 钉在 captured");
        assert!(ev.pressed.is_empty(), "快照重复不产生新边沿");
        // 在 other 上方释放:仍解析到 captured note → released + clicked。
        f.on_mouse(120.0, 120.0, &release_left(), mods());
        let ev = f.resolve();
        assert_eq!(ev.released, vec![note]);
        assert_eq!(ev.clicked, vec![note]);
        assert_eq!(f.captured, None);
        let _ = other;
    }

    /// 点击判定:左键按下+释放同 area → clicked;按下未命中则释放不产生点击;
    /// 非左键不产生 clicked。
    #[test]
    fn click_requires_press_and_release_on_same_area() {
        let mut f = UIFlow::new();
        let a = f.button((0.0, 0.0, 50.0, 50.0), AreaKind::NoteBlock);
        // 按下 + 释放同一 area → clicked。
        f.on_mouse(10.0, 10.0, &press_left(), mods());
        f.resolve();
        f.on_mouse(10.0, 10.0, &release_left(), mods());
        let ev = f.resolve();
        assert!(ev.clicked.contains(&a) && ev.released.contains(&a));
        // 空白按下(未命中)→ 释放落到 area 上也不产生 click。
        f.on_mouse(200.0, 200.0, &press_left(), mods());
        f.resolve();
        f.on_mouse(10.0, 10.0, &release_left(), mods());
        let ev = f.resolve();
        assert!(ev.clicked.is_empty());
        // 右键按下+释放命中同一 area:只有 pressed/released,无 clicked。
        // pressed 在按下那次 resolve 产出,released 在释放那次产出。
        f.on_mouse(10.0, 10.0, &[ButtonState::Up, ButtonState::Up, ButtonState::Pressed], mods());
        let evp = f.resolve();
        f.on_mouse(10.0, 10.0, &[ButtonState::Up, ButtonState::Up, ButtonState::Released], mods());
        let ev = f.resolve();
        assert!(evp.pressed.contains(&a) && ev.released.contains(&a));
        assert!(ev.clicked.is_empty());
    }

    /// disabled 区域不命中:悬停/按下均跳过。
    #[test]
    fn disabled_area_is_not_hit() {
        let mut f = UIFlow::new();
        f.area(AreaId(9), (10.0, 10.0, 30.0, 30.0), AreaKind::NoteBlock, true);
        f.on_mouse(20.0, 20.0, &up(), mods());
        f.resolve();
        assert_eq!(f.hover, None);
        f.on_mouse(20.0, 20.0, &press_left(), mods());
        let ev = f.resolve();
        assert!(ev.pressed.is_empty());
        assert_eq!(f.captured, None);
        // 解除 disabled 后可命中。
        f.area(AreaId(9), (10.0, 10.0, 30.0, 30.0), AreaKind::NoteBlock, false);
        f.on_mouse(20.0, 20.0, &up(), mods());
        f.resolve();
        assert_eq!(f.hover, Some(AreaId(9)));
    }

    /// 多键:第一个按下的键持有捕获权,其余键不夺走(右键菜单不影响左键拖拽)。
    #[test]
    fn press_of_other_button_does_not_steal_capture() {
        let mut f = UIFlow::new();
        let note = f.button((0.0, 0.0, 50.0, 50.0), AreaKind::NoteBlock);
        f.on_mouse(10.0, 10.0, &press_left(), mods());
        f.resolve();
        assert_eq!(f.captured, Some(note));
        // 右键在空白按下:不夺走左键拖拽。
        f.on_mouse(200.0, 200.0, &[ButtonState::Up, ButtonState::Up, ButtonState::Pressed], mods());
        f.resolve();
        assert_eq!(f.captured, Some(note));
        // 右键释放(空白):released 空;左键捕获仍在。
        f.on_mouse(200.0, 200.0, &[ButtonState::Up, ButtonState::Up, ButtonState::Released], mods());
        f.resolve();
        assert_eq!(f.captured, Some(note));
        // 左键在远处释放:解析到 captured note。
        f.on_mouse(90.0, 90.0, &release_left(), mods());
        let ev = f.resolve();
        assert_eq!(ev.released, vec![note]);
        assert_eq!(f.captured, None);
    }

    /// area 显式 id upsert:同 id 原位更新,id 稳定、数量不增长。
    #[test]
    fn area_upsert_keeps_id_and_updates_rect() {
        let mut f = UIFlow::new();
        let id = f.area(AreaId(3), (0.0, 0.0, 10.0, 10.0), AreaKind::NoteBlock, false);
        assert_eq!(id, AreaId(3));
        f.area(AreaId(3), (5.0, 5.0, 20.0, 20.0), AreaKind::NoteBlock, false);
        assert_eq!(f.areas.len(), 1);
        assert_eq!(f.areas[0].rect, (5.0, 5.0, 20.0, 20.0));
    }

    /// 丢释放自愈:按下后释放被 OS 吞掉(拖拽出窗/失焦),prev_buttons 停在
    /// Pressed → 下一次按下检测不到边沿(点击静默丢失,表现"要双击");
    /// force_release_all 后下一次按下必须重新产生 press 边沿。
    #[test]
    fn force_release_all_recovers_lost_release() {
        let mut f = UIFlow::new();
        let a = f.button((0.0, 0.0, 50.0, 50.0), AreaKind::NoteBlock);
        f.on_mouse(10.0, 10.0, &press_left(), mods());
        f.resolve();
        assert_eq!(f.captured, Some(a));
        // 释放事件丢失:流控仍认为左键按下。
        assert_eq!(f.buttons[0], ButtonState::Pressed);
        // 下一次按下(物理上已抬起):无自愈时检测不到边沿 → 点击丢失。
        f.on_mouse(10.0, 10.0, &press_left(), mods());
        let ev = f.resolve();
        assert!(ev.pressed.is_empty(), "未重置时重复按下不产生边沿(点击丢失的根源)");
        // self-heal:重置后再次按下 → 边沿恢复。
        f.force_release_all();
        assert_eq!(f.captured, None);
        f.on_mouse(10.0, 10.0, &press_left(), mods());
        let ev = f.resolve();
        assert_eq!(ev.pressed, vec![a]);
        f.on_mouse(10.0, 10.0, &release_left(), mods());
        let ev = f.resolve();
        assert!(ev.clicked.contains(&a), "自愈后点击完整可用");
    }

    /// 释放丢失后 hover 被陈旧 captured 钉死(拖拽中移动也不跟鼠标);
    /// force_release_all 解除钉死,恢复正常命中。
    #[test]
    fn force_release_all_unpins_stale_captured_hover() {
        let mut f = UIFlow::new();
        let a = f.button((0.0, 0.0, 50.0, 50.0), AreaKind::NoteBlock);
        let b = f.button((100.0, 100.0, 50.0, 50.0), AreaKind::NoteBlock);
        f.on_mouse(10.0, 10.0, &press_left(), mods());
        f.resolve();
        // 释放丢失:移动到 b 上方,hover 仍钉在 a。
        f.on_mouse(120.0, 120.0, &press_left(), mods());
        f.resolve();
        assert_eq!(f.hover, Some(a), "释放丢失 → hover 被陈旧 captured 钉死");
        f.force_release_all();
        f.on_mouse(120.0, 120.0, &up(), mods());
        f.resolve();
        assert_eq!(f.hover, Some(b), "重置后 hover 恢复正常命中");
    }

    /// 快速点击(按下+释放在一次 resolve 前先后到达,事件间隔 < 一帧):
    /// 两个边沿都必须保留,一次 resolve 同时产出 pressed/released/clicked。
    #[test]
    fn fast_click_press_release_before_resolve_produces_click() {
        let mut f = UIFlow::new();
        let a = f.button((0.0, 0.0, 50.0, 50.0), AreaKind::NoteBlock);
        f.on_mouse(10.0, 10.0, &press_left(), mods());
        f.on_mouse(10.0, 10.0, &release_left(), mods());
        let ev = f.resolve();
        assert_eq!(ev.pressed, vec![a]);
        assert_eq!(ev.released, vec![a]);
        assert_eq!(ev.clicked, vec![a], "快速点击必须产生 clicked");
        assert_eq!(f.captured, None, "释放后捕获归还");
    }

    /// dirty:输入/布局变化置位,resolve 后清除。
    #[test]
    fn dirty_marks_input_and_layout_changes() {
        let mut f = UIFlow::new();
        assert!(!f.dirty);
        f.on_mouse(1.0, 1.0, &up(), mods());
        assert!(f.dirty);
        f.resolve();
        assert!(!f.dirty);
        f.button((0.0, 0.0, 10.0, 10.0), AreaKind::NoteBlank);
        assert!(f.dirty);
        f.resolve();
        assert!(!f.dirty);
    }

    fn sample_transform() -> PanelTransform {
        // w = 800,s = 1,两个面板进度都为 1 → panel_x = 0,内容区几何确定。
        PanelTransform::new(4.0, 8.0, 1, 1.0, 800.0, 800.0, 1.0, 0.0, 1.0)
    }

    /// 映射往返与越界钳制(与 mod.rs 既有共享映射函数行为一致)。
    #[test]
    fn panel_transform_mappings_roundtrip_and_clamp() {
        let t = sample_transform();
        let play = t.notes_play_rect();
        // x:左缘 → -675,右缘 → +675,中点 → 0。
        assert!((t.x_to_pos_x(play.0) - (-675.0)).abs() < 1e-3);
        assert!((t.x_to_pos_x(play.0 + play.2) - 675.0).abs() < 1e-3);
        assert!(t.x_to_pos_x(play.0 + play.2 * 0.5).abs() < 1e-3);
        for nx in [-675.0, -200.0, 0.0, 333.0, 675.0] {
            let mx = t.pos_x_to_x(nx);
            let back = t.x_to_pos_x(mx);
            assert!((back - nx).abs() < 1e-3, "pos_x {nx} → x {mx} → {back}");
        }
        // y:窗口顶 = scroll+zoom,底 = scroll;越界钳到可见范围。
        assert!((t.y_to_beat(play.1) - 12.0).abs() < 1e-4);
        assert!((t.y_to_beat(play.1 + play.3) - 4.0).abs() < 1e-4);
        assert_eq!(t.y_to_beat(-500.0), 12.0);
        assert_eq!(t.y_to_beat(1e9), 4.0);
        for b in [4.0, 5.5, 8.0, 11.9, 12.0] {
            let y = t.beat_to_y(b);
            let back = t.y_to_beat(y);
            assert!((back - b).abs() < 1e-4, "beat {b} → y {y} → {back}");
        }
        // mouse_beat 组合:内容区中点 = scroll + zoom/2,pos_x = 0。
        let (b, x) = t.mouse_beat(play.0 + play.2 * 0.5, play.1 + play.3 * 0.5);
        assert!((b - 8.0).abs() < 1e-4);
        assert!(x.abs() < 1e-3);
    }

    /// 分栏列中心:多栏按列居中,单栏退化为线性。
    #[test]
    fn panel_transform_col_x_splits_columns() {
        let t = PanelTransform::new(0.0, 10.0, 4, 1.0, 800.0, 800.0, 1.0, 0.0, 1.0);
        let play = t.notes_play_rect();
        let col0 = play.0 + play.2 / 8.0; // 4 栏第 0 列中心(px + 0.5*pw/4)
        for nx in [-675.0, -650.0, -600.0, -550.0] {
            assert!((t.pos_x_to_col_x(nx) - col0).abs() < 1e-2, "nx {nx} → col 0");
        }
        let col1 = play.0 + 3.0 * play.2 / 8.0; // 第 1 列中心(px + 1.5*pw/4)
        for nx in [-450.0, -400.0, -300.0, -250.0] {
            assert!((t.pos_x_to_col_x(nx) - col1).abs() < 1e-2, "nx {nx} → col 1");
        }
        let col2 = play.0 + 5.0 * play.2 / 8.0; // 第 2 列中心(px + 2.5*pw/4)
        for nx in [-100.0, 0.0, 100.0] {
            assert!((t.pos_x_to_col_x(nx) - col2).abs() < 1e-2, "nx {nx} → col 2");
        }
        let t1 = PanelTransform::new(0.0, 10.0, 1, 1.0, 800.0, 800.0, 1.0, 0.0, 1.0);
        assert!((t1.pos_x_to_col_x(0.0) - t1.pos_x_to_x(0.0)).abs() < 1e-6);
    }

    /// 双面板同开布局:notes 面板左移必须含 events 面板宽度(命中=绘制)。
    /// 回归:旧公式只减 notes 宽,双面板同开时 notes 命中区右移一个
    /// TL_W,压在 events 面板上(用户实测:event 面板响应 notes 位置)。
    #[test]
    fn panel_x_accounts_for_events_panel_when_both_open() {
        let s = 1.0;
        let w = 1280.0;
        // props 面板开、events 开、notes 开:三面板横排链式左移。
        let t = PanelTransform::new(0.0, 8.0, 1, s, w, 800.0, 1.0, 1.0, 1.0);
        let props_x = w - 1.0 * PANEL_W * s;
        let expect = props_x - TL_W * s - NT_W * s;
        assert!((t.panel_x() - expect).abs() < 1e-3,
            "both panels open: panel_x {} != {}", t.panel_x(), expect);
        // 仅 notes 开(events 关):与旧行为一致(不左移 events 宽)。
        let t2 = PanelTransform::new(0.0, 8.0, 1, s, w, 800.0, 1.0, 0.0, 1.0);
        assert!((t2.panel_x() - (props_x - NT_W * s)).abs() < 1e-3);
        // 全部关闭:notes 面板左缘 = 视口宽(面板未滑入,不占位)。
        let t3 = PanelTransform::new(0.0, 8.0, 1, s, w, 800.0, 0.0, 0.0, 0.0);
        assert!((t3.panel_x() - w).abs() < 1e-3);
    }

    /// from_play 逆构造:notes_play_rect 往返精确还原(旧 free fn 委托用)。
    #[test]
    fn from_play_roundtrips_notes_play_rect() {
        let t = sample_transform();
        let play = t.notes_play_rect();
        let inv = PanelTransform::from_play(play, t.scroll, t.zoom, t.v_split);
        let play2 = inv.notes_play_rect();
        assert!((play2.0 - play.0).abs() < 1e-3);
        assert!((play2.1 - play.1).abs() < 1e-3);
        assert!((play2.2 - play.2).abs() < 1e-3);
        assert!((play2.3 - play.3).abs() < 1e-3);
        // 映射一致(委托 free fn 时行为不变)。
        for b in [4.0, 8.0, 11.5] {
            assert!((inv.y_to_beat(t.beat_to_y(b)) - b).abs() < 1e-4);
        }
        for nx in [-675.0, 0.0, 333.0] {
            assert!((inv.x_to_pos_x(t.pos_x_to_x(nx)) - nx).abs() < 1e-3);
        }
    }
}
