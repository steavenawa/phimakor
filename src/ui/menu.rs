//! 菜单基系统(P3):全屏菜单 / 局部菜单(右键)/ 菜单条下拉,三种形态共用
//! 同一套 [`MenuState`] + flow.areas。
//!
//! 增量 API 约定同 widgets.rs:open_fullscreen 等构造器当前只被测试/未来
//! 迁移(NewChartDlg → fullscreen)使用,属备用接口而非死代码,模块级抑制
//! dead_code 警告。//!
//! 设计:
//! - **几何单源**:打开时的面板矩形与行几何全部由 [`MenuHost::layout`] 派生,
//!   [`MenuHost::areas`](注册到 flow 的命中区)与 [`MenuHost::snapshot`]
//!   (worker 线程绘制用)共用同一份布局 —— 命中 = 绘制,不再有两套魔法数字。
//! - **命中经 flow**:菜单区作为 [`flow::HotArea`] 注册(注册在 notes 区之上),
//!   外部点击由最底层全窗 mask 区承接 → 关闭;键盘导航(Up/Down/Enter/Esc/
//!   左右进/出子菜单)由 [`MenuHost::on_key`] 处理,宿主在其它键盘路由前拦截。
//! - **纯数据**:不依赖 tiny_skia/字体/GPU,可单元测试;绘制由宿主
//!   (timeline_draw.rs 的 `draw_menu_snap`)消费 [`MenuDrawInfo`] 快照。
//!
//! 区域 id 分配(与 notes 面板的 id 空间错开):
//! - [`AREA_MENU_MASK`] = 全窗捕获区(外部点击关闭;全屏时兼遮罩);
//! - [`AREA_MENU_ITEM_BASE`] + i = 当前层第 i 项;
//! - [`AREA_MENU_SUB_BASE`] + i = 展开子菜单第 i 项。
#![allow(dead_code)] // 备用接口约定同 widgets.rs(open_fullscreen 等仅测试/未来迁移使用)

use super::flow;
use super::flow::{AreaId, FlowEvents, HotArea};
use super::widgets::{self, WidgetKey};

/// 全窗 mask/capture 区 id(最底层,点击外部 → Close)。
pub const AREA_MENU_MASK: u32 = 2_000_000;
/// 当前层菜单项 id 基址(顶层或已展开的子菜单父级)。
pub const AREA_MENU_ITEM_BASE: u32 = 2_000_001;
/// 展开子菜单项 id 基址。
pub const AREA_MENU_SUB_BASE: u32 = 2_001_000;

// ── 菜单条(bar)几何:复用旧 draw_menu 的 MENU_* 常量(PMCORE-7)──

/// 菜单条下拉面板宽。
pub const MENU_PANEL_W: f32 = 240.0;
/// 菜单条条目高(含行距)。
pub const MENU_ITEM_H: f32 = 32.0;
/// 菜单条标题下第一项 y 偏移。
pub const MENU_ITEM_TOP: f32 = 50.0;

/// 菜单项动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// 触发宿主命令(命令名由调用方约定,如 "save"/"delete_note")。
    Command(&'static str),
    /// 展开当前层第 `i` 项的子菜单(该项须带 `sub`)。
    Submenu(usize),
    /// 关闭整个菜单。
    Close,
}

/// 菜单项。
#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: String,
    pub action: MenuAction,
    pub disabled: bool,
    /// 子菜单项(Some = 该项是子菜单头,展开/激活进入子菜单)。
    pub sub: Option<Vec<MenuItem>>,
}

impl MenuItem {
    pub fn new(label: impl Into<String>, action: MenuAction) -> Self {
        Self { label: label.into(), action, disabled: false, sub: None }
    }

    pub fn disabled(mut self) -> Self {
        self.disabled = true;
        self
    }

    pub fn with_sub(mut self, sub: Vec<MenuItem>) -> Self {
        self.sub = Some(sub);
        self
    }
}

/// 菜单形态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuForm {
    Closed,
    /// 菜单条下拉(居中面板,复用 MENU_* 几何)。
    Bar,
    /// 右键局部菜单(锚点弹出,点击外部关闭)。
    Context,
    /// 模态全屏菜单(遮罩 + 面板,flow 独占捕获,ESC 关闭)。
    Fullscreen,
}

/// 展开中的子菜单(挂在父菜单项上)。
#[derive(Clone, Debug)]
pub struct SubMenuState {
    /// 父菜单中的项索引。
    pub parent: usize,
    /// 子菜单面板几何。
    pub rect: (f32, f32, f32, f32),
    pub items: Vec<MenuItem>,
    /// 键盘/悬停高亮。
    pub sel: Option<usize>,
}

/// 打开中的菜单栈(几何单源:面板 rect 在此,行矩形由 layout 派生)。
#[derive(Clone, Debug)]
pub struct MenuState {
    pub form: MenuForm,
    /// 面板矩形(已乘 gui_scale)。
    pub rect: (f32, f32, f32, f32),
    /// 当前层菜单项。
    pub items: Vec<MenuItem>,
    /// 键盘/悬停高亮(当前层)。
    pub sel: Option<usize>,
    /// 展开的子菜单(None = 未展开)。
    pub sub: Option<SubMenuState>,
    /// 全屏标题(仅 Fullscreen 绘制)。
    pub title: String,
    /// 是否绘制全屏遮罩。
    pub mask: bool,
}

impl Default for MenuState {
    fn default() -> Self {
        Self {
            form: MenuForm::Closed,
            rect: (0.0, 0.0, 0.0, 0.0),
            items: Vec::new(),
            sel: None,
            sub: None,
            title: String::new(),
            mask: false,
        }
    }
}

/// 一行菜单项的绘制快照(worker 线程用;几何由 MenuHost 派生,单一真源)。
#[derive(Clone, Debug, PartialEq)]
pub struct MenuDrawRow {
    pub rect: (f32, f32, f32, f32),
    pub label: String,
    pub disabled: bool,
    pub sel: bool,
    pub has_sub: bool,
}

/// 菜单绘制快照(跨线程安全;与 areas 共用同一 layout)。
#[derive(Clone, Debug, PartialEq)]
pub struct MenuDrawInfo {
    pub form: MenuForm,
    /// 全屏遮罩(画全窗半透明底)。
    pub mask: bool,
    /// 全屏标题。
    pub title: String,
    /// 面板矩形。
    pub panel: (f32, f32, f32, f32),
    /// 当前层行矩形。
    pub rows: Vec<MenuDrawRow>,
    /// 展开子菜单:(面板, 行矩形)。
    pub sub: Option<((f32, f32, f32, f32), Vec<MenuDrawRow>)>,
}

/// 菜单宿主:统一打开/关闭/命中/键盘/快照。持有打开中的菜单栈。
#[derive(Clone, Debug, Default)]
pub struct MenuHost {
    pub state: MenuState,
    /// 视口(像素)与 gui_scale;打开与 layout 用,由宿主每帧同步。
    vw: f32,
    vh: f32,
    s: f32,
    /// 键盘导航进行中:为 true 时鼠标悬停不覆盖高亮(方向键按下即置位)。
    keys: bool,
}

impl MenuHost {
    pub fn new() -> Self {
        Self { state: MenuState::default(), vw: 0.0, vh: 0.0, s: 1.0, keys: false }
    }

    pub fn is_open(&self) -> bool {
        self.state.form != MenuForm::Closed
    }

    /// 同步视口与缩放(打开/布局前由宿主调用;窗口 resize 后必须重设)。
    pub fn set_viewport(&mut self, vw: f32, vh: f32, s: f32) {
        self.vw = vw;
        self.vh = vh;
        self.s = s;
    }

    /// 菜单条下拉:面板居中(复用 MENU_* 几何)。`x <= 0` = 水平居中
    /// (编辑器主菜单);`x > 0` = 以 x 为锚(未来顶栏 item 下拉用)。
    pub fn open_bar(&mut self, x: f32, items: &[MenuItem]) {
        let s = self.s;
        let pw = MENU_PANEL_W * s;
        let ph = (MENU_ITEM_TOP + items.len() as f32 * MENU_ITEM_H + 12.0) * s;
        let px = if x > 0.0 { x.min(self.vw - pw).max(0.0) } else { (self.vw - pw) * 0.5 };
        let py = (self.vh - ph) * 0.5;
        self.open(MenuForm::Bar, (px, py, pw, ph), items.to_vec(), false, String::new());
    }

    /// 右键局部菜单:在锚点弹出,越界钳到视口内。
    pub fn open_context(&mut self, at: (f32, f32), items: &[MenuItem]) {
        let s = self.s;
        let pw = 160.0 * s;
        let ph = (8.0 + items.len() as f32 * 30.0) * s;
        let px = at.0.min(self.vw - pw).max(0.0);
        let py = at.1.min(self.vh - ph).max(0.0);
        self.open(MenuForm::Context, (px, py, pw, ph), items.to_vec(), false, String::new());
    }

    /// 模态全屏菜单:遮罩 + 居中面板,flow 独占捕获,ESC 关闭。
    pub fn open_fullscreen(&mut self, title: &str, body: Vec<MenuItem>) {
        let s = self.s;
        let pw = 320.0 * s;
        let ph = (64.0 + body.len() as f32 * 34.0 + 12.0) * s;
        let px = (self.vw - pw) * 0.5;
        let py = (self.vh - ph) * 0.5;
        self.open(MenuForm::Fullscreen, (px, py, pw, ph), body, true, title.to_string());
    }

    fn open(&mut self, form: MenuForm, rect: (f32, f32, f32, f32), items: Vec<MenuItem>, mask: bool, title: String) {
        self.state = MenuState { form, rect, items, sel: None, sub: None, title, mask };
        self.keys = false;
    }

    pub fn close(&mut self) {
        self.state = MenuState::default();
    }

    // ── 几何(单一真源:areas / snapshot / dispatch 全部由此派生)──

    /// 当前形态的行几何(未乘 s):(行高, 首行上边距)。
    fn row_metrics(&self) -> (f32, f32) {
        match self.state.form {
            MenuForm::Bar => (MENU_ITEM_H, MENU_ITEM_TOP),
            MenuForm::Context => (30.0, 4.0),
            MenuForm::Fullscreen => (34.0, 64.0),
            MenuForm::Closed => (0.0, 0.0),
        }
    }

    /// 面板内第 `i` 行矩形(与旧 draw_menu/ctx 菜单行几何一致)。
    fn item_rect(&self, i: usize) -> (f32, f32, f32, f32) {
        let (row_h, top_pad) = self.row_metrics();
        let s = self.s;
        let (x, y, w, _) = self.state.rect;
        (x + 8.0 * s, y + (top_pad + i as f32 * row_h) * s, w - 16.0 * s, (row_h - 4.0) * s)
    }

    /// 子菜单面板矩形:父面板右缘弹出,贴右缘时左弹出;与父项行对齐。
    fn submenu_rect(&self, parent: usize, n: usize) -> (f32, f32, f32, f32) {
        let (row_h, _) = self.row_metrics();
        let s = self.s;
        let (x, y, w, _) = self.state.rect;
        let sw = w;
        let sh = (8.0 + n as f32 * row_h) * s;
        let sx = if x + w + sw <= self.vw { x + w } else { (x - sw).max(0.0) };
        let sy = (y + parent as f32 * row_h * s).min((self.vh - sh).max(0.0)).max(0.0);
        (sx, sy, sw, sh)
    }

    /// 当前层全部行矩形(顶层,或展开子菜单时只有子菜单层参与命中/绘制)。
    fn layout(&self) -> (Vec<(f32, f32, f32, f32)>, Option<(Vec<(f32, f32, f32, f32)>, (f32, f32, f32, f32))>) {
        let rows = (0..self.state.items.len()).map(|i| self.item_rect(i)).collect();
        let sub = self.state.sub.as_ref().map(|s| {
            let sr = (0..s.items.len()).map(|i| self.sub_item_rect(i)).collect();
            (sr, s.rect)
        });
        (rows, sub)
    }

    /// 子菜单面板内第 `i` 行矩形。
    fn sub_item_rect(&self, i: usize) -> (f32, f32, f32, f32) {
        let (row_h, _) = self.row_metrics();
        let s = self.s;
        let (x, y, w, _) = self.state.sub.as_ref().map(|s| s.rect).unwrap_or((0.0, 0.0, 0.0, 0.0));
        (x + 8.0 * s, y + (4.0 + i as f32 * row_h) * s, w - 16.0 * s, (row_h - 4.0) * s)
    }

    // ── 命中注册(经 flow;外部点击 → 最底层 mask → Close)──

    pub fn areas(&mut self) -> Vec<HotArea> {
        if !self.is_open() {
            return Vec::new();
        }
        let kind = flow::AreaKind::Widget(widgets::AreaKind::MenuMask);
        let item_kind = flow::AreaKind::Widget(widgets::AreaKind::MenuItem);
        let mut out = vec![HotArea {
            id: AreaId(AREA_MENU_MASK),
            rect: (0.0, 0.0, self.vw, self.vh),
            kind,
            disabled: false,
        }];
        for (i, item) in self.state.items.iter().enumerate() {
            out.push(HotArea {
                id: AreaId(AREA_MENU_ITEM_BASE + i as u32),
                rect: self.item_rect(i),
                kind: item_kind,
                disabled: item.disabled,
            });
        }
        if let Some(sub) = &self.state.sub {
            for (i, item) in sub.items.iter().enumerate() {
                out.push(HotArea {
                    id: AreaId(AREA_MENU_SUB_BASE + i as u32),
                    rect: self.sub_item_rect(i),
                    kind: item_kind,
                    disabled: item.disabled,
                });
            }
        }
        out
    }

    // ── 鼠标派发(按下即触发,与旧右键菜单一致)──

    pub fn dispatch(&mut self, ev: &FlowEvents, out: &mut Vec<MenuAction>) {
        if !self.is_open() {
            return;
        }
        for &id in &ev.pressed {
            match id.0 {
                AREA_MENU_MASK => out.push(MenuAction::Close),
                n if n >= AREA_MENU_SUB_BASE => {
                    let i = (n - AREA_MENU_SUB_BASE) as usize;
                    let item = self.state.sub.as_ref().and_then(|s| s.items.get(i)).cloned();
                    if let Some(item) = item {
                        self.activate(&item, i, out);
                    }
                }
                n if n >= AREA_MENU_ITEM_BASE => {
                    let i = (n - AREA_MENU_ITEM_BASE) as usize;
                    let item = self.state.items.get(i).cloned();
                    if let Some(item) = item {
                        self.activate(&item, i, out);
                    }
                }
                // 任何非菜单区域(音符块/面板行/空白)按下 → 关闭。只靠全窗
                // mask 不够:面板内 NoteBlank/NoteBlock 在 mask 之上,点它们
                // 命中非菜单 id,旧逻辑不关(菜单要等别的鼠标触发
                // 才销毁)。
                _ => out.push(MenuAction::Close),
            }
        }
    }

    /// 激活一项:子菜单头 → 展开/收起;命令 → 输出;Close → 关闭。
    fn activate(&mut self, item: &MenuItem, i: usize, out: &mut Vec<MenuAction>) {
        if item.disabled {
            return;
        }
        if item.sub.is_some() {
            if self.state.sub.as_ref().is_some_and(|s| s.parent == i) {
                self.state.sub = None;
            } else {
                let sub_items = item.sub.clone().unwrap_or_default();
                let rect = self.submenu_rect(i, sub_items.len());
                self.state.sub = Some(SubMenuState { parent: i, rect, items: sub_items, sel: None });
            }
            return;
        }
        match item.action {
            MenuAction::Command(c) => out.push(MenuAction::Command(c)),
            MenuAction::Close => out.push(MenuAction::Close),
            MenuAction::Submenu(_) => {} // 无 sub 的 Submenu 动作:无操作
        }
    }

    // ── 键盘导航(Up/Down/Enter/Esc + 左右进出子菜单;宿主在路由前拦截)──

    pub fn on_key(&mut self, k: WidgetKey) -> Vec<MenuAction> {
        let mut out = Vec::new();
        if !self.is_open() {
            return out;
        }
        self.keys = true;
        match k {
            WidgetKey::Escape => {
                if self.state.sub.is_some() {
                    self.state.sub = None;
                } else {
                    out.push(MenuAction::Close);
                }
            }
            WidgetKey::Left => {
                if self.state.sub.is_some() {
                    self.state.sub = None;
                }
            }
            WidgetKey::Right => {
                if self.state.sub.is_none() {
                    if let Some(i) = self.state.sel {
                        if let Some(item) = self.state.items.get(i) {
                            if item.sub.is_some() {
                                let sub_items = item.sub.clone().unwrap_or_default();
                                let rect = self.submenu_rect(i, sub_items.len());
                                self.state.sub = Some(SubMenuState { parent: i, rect, items: sub_items, sel: Some(0) });
                            }
                        }
                    }
                }
            }
            WidgetKey::Up => self.move_sel(-1),
            WidgetKey::Down => self.move_sel(1),
            WidgetKey::Enter => {
                let target = if let Some(sub) = &self.state.sub {
                    sub.sel.map(|i| (i, sub.items[i].clone()))
                } else {
                    self.state.sel.map(|i| (i, self.state.items[i].clone()))
                };
                if let Some((i, item)) = target {
                    self.activate(&item, i, &mut out);
                }
            }
            _ => {}
        }
        out
    }

    /// 在当前激活层移动高亮(跳过禁用项;无高亮时从首/尾开始)。
    fn move_sel(&mut self, d: i32) {
        let n = match &self.state.sub {
            Some(s) => s.items.len(),
            None => self.state.items.len(),
        };
        if n == 0 {
            return;
        }
        let cur = match &self.state.sub {
            Some(s) => s.sel,
            None => self.state.sel,
        };
        let start = match cur {
            Some(c) => c as i32,
            None => if d > 0 { -1 } else { n as i32 },
        };
        let mut i = start + d;
        for _ in 0..=n {
            if i < 0 {
                i = n as i32 - 1;
            }
            if i >= n as i32 {
                i = 0;
            }
            let enabled = match &self.state.sub {
                Some(s) => s.items.get(i as usize).map_or(false, |it| !it.disabled),
                None => self.state.items.get(i as usize).map_or(false, |it| !it.disabled),
            };
            if enabled {
                match &mut self.state.sub {
                    Some(s) => s.sel = Some(i as usize),
                    None => self.state.sel = Some(i as usize),
                }
                return;
            }
            i += d;
        }
        // 全部禁用:保持原高亮。
    }

    /// 由 flow.hover 更新悬停高亮(鼠标明确悬停到菜单项上才接管键盘高亮)。
    pub fn apply_hover(&mut self, hover: Option<AreaId>) {
        if self.keys {
            return; // 键盘导航中:鼠标悬停不覆盖高亮。
        }
        let Some(n) = hover.map(|h| h.0) else { return };
        if n >= AREA_MENU_SUB_BASE {
            let i = (n - AREA_MENU_SUB_BASE) as usize;
            if let Some(s) = &mut self.state.sub {
                if i < s.items.len() {
                    self.keys = false;
                    s.sel = Some(i);
                }
            }
        } else if n >= AREA_MENU_ITEM_BASE {
            let i = (n - AREA_MENU_ITEM_BASE) as usize;
            if i < self.state.items.len() {
                self.keys = false;
                self.state.sel = Some(i);
            }
        }
    }

    // ── 绘制快照(worker 线程用;几何 = layout,与 areas 同源)──

    pub fn snapshot(&self) -> MenuDrawInfo {
        if !self.is_open() {
            return MenuDrawInfo {
                form: MenuForm::Closed,
                mask: false,
                title: String::new(),
                panel: (0.0, 0.0, 0.0, 0.0),
                rows: Vec::new(),
                sub: None,
            };
        }
        let (top_rows, sub) = self.layout();
        let rows = top_rows
            .iter()
            .enumerate()
            .map(|(i, &r)| MenuDrawRow {
                rect: r,
                label: self.state.items[i].label.clone(),
                disabled: self.state.items[i].disabled,
                sel: self.state.sel == Some(i),
                has_sub: self.state.items[i].sub.is_some(),
            })
            .collect();
        let sub_snap = sub.map(|(sr, panel)| {
            let sub_state = self.state.sub.as_ref().expect("sub layout implies sub state");
            (
                panel,
                sr.iter()
                    .enumerate()
                    .map(|(i, &r)| MenuDrawRow {
                        rect: r,
                        label: sub_state.items[i].label.clone(),
                        disabled: sub_state.items[i].disabled,
                        sel: sub_state.sel == Some(i),
                        has_sub: sub_state.items[i].sub.is_some(),
                    })
                    .collect(),
            )
        });
        MenuDrawInfo {
            form: self.state.form,
            mask: self.state.mask,
            title: self.state.title.clone(),
            panel: self.state.rect,
            rows,
            sub: sub_snap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::flow::{ButtonState, UIFlow};
    use winit::keyboard::ModifiersState;

    fn flow_with(host: &mut MenuHost) -> UIFlow {
        let mut f = UIFlow::new();
        f.areas = host.areas();
        f
    }

    fn press(f: &mut UIFlow, x: f32, y: f32) -> FlowEvents {
        f.on_mouse(x, y, &[ButtonState::Pressed, ButtonState::Up, ButtonState::Up], ModifiersState::default());
        f.resolve()
    }

    fn sample_items() -> Vec<MenuItem> {
        vec![
            MenuItem::new("Open", MenuAction::Command("open")),
            MenuItem::new("Sub", MenuAction::Submenu(1)).with_sub(vec![
                MenuItem::new("Sub A", MenuAction::Command("sub_a")),
                MenuItem::new("Sub B (off)", MenuAction::Command("sub_b")).disabled(),
            ]),
            MenuItem::new("Quit", MenuAction::Close),
            MenuItem::new("Disabled", MenuAction::Command("no")).disabled(),
        ]
    }

    #[test]
    fn open_close_roundtrip() {
        let mut h = MenuHost::new();
        h.set_viewport(800.0, 600.0, 1.0);
        assert!(!h.is_open());
        h.open_bar(0.0, &sample_items());
        assert!(h.is_open());
        assert_eq!(h.state.form, MenuForm::Bar);
        // 面板几何:240 宽,4 项 = 50 + 4*32 + 12 高,水平居中。
        assert_eq!(h.state.rect.0, (800.0 - 240.0) * 0.5);
        assert_eq!(h.state.rect.2, 240.0);
        assert!((h.state.rect.3 - 190.0).abs() < 1e-3);
        h.close();
        assert!(!h.is_open());
        assert!(h.areas().is_empty());
    }

    #[test]
    fn context_anchor_clamped_to_viewport() {
        let mut h = MenuHost::new();
        h.set_viewport(800.0, 600.0, 1.0);
        // 锚点在右下角:面板必须完全钳回视口内。
        h.open_context((790.0, 590.0), &sample_items());
        assert_eq!(h.state.form, MenuForm::Context);
        assert!(h.state.rect.0 + h.state.rect.2 <= 800.0 + 1e-3);
        assert!(h.state.rect.1 + h.state.rect.3 <= 600.0 + 1e-3);
    }

    #[test]
    fn fullscreen_mask_and_exclusive_capture() {
        let mut h = MenuHost::new();
        h.set_viewport(800.0, 600.0, 1.0);
        h.open_fullscreen("Test", sample_items());
        assert!(h.state.mask);
        let snap = h.snapshot();
        assert!(snap.mask);
        assert_eq!(snap.form, MenuForm::Fullscreen);
        // 独占捕获:areas 含全窗 mask,且 mask 在最底层(首个)。
        let areas = h.areas();
        assert_eq!(areas[0].id.0, AREA_MENU_MASK);
        assert_eq!(areas[0].rect, (0.0, 0.0, 800.0, 600.0));
        // 点 mask 任意处 → Close。
        let mut f = flow_with(&mut h);
        let mut out = Vec::new();
        h.dispatch(&press(&mut f, 5.0, 5.0), &mut out);
        assert_eq!(out, vec![MenuAction::Close]);
    }

    #[test]
    fn click_item_emits_command_disabled_ignored() {
        let mut h = MenuHost::new();
        h.set_viewport(800.0, 600.0, 1.0);
        h.open_bar(0.0, &sample_items());
        let mut f = flow_with(&mut h);
        // 第一项 "Open" → Command("open")。
        let mut out = Vec::new();
        let ev = press(&mut f, h.state.rect.0 + 40.0, h.state.rect.1 + MENU_ITEM_TOP + 16.0);
        assert!(ev.pressed.iter().any(|id| id.0 == AREA_MENU_ITEM_BASE));
        h.dispatch(&ev, &mut out);
        assert_eq!(out, vec![MenuAction::Command("open")]);
        // 禁用项 "Disabled"(第 4 项)不产生动作。
        let mut out = Vec::new();
        let y = h.state.rect.1 + MENU_ITEM_TOP + 3.0 * MENU_ITEM_H + 16.0;
        let ev = press(&mut f, h.state.rect.0 + 40.0, y);
        h.dispatch(&ev, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn click_outside_closes() {
        let mut h = MenuHost::new();
        h.set_viewport(800.0, 600.0, 1.0);
        h.open_bar(0.0, &sample_items());
        let mut f = flow_with(&mut h);
        // 面板外(左上角)→ mask → Close。
        let mut out = Vec::new();
        h.dispatch(&press(&mut f, 3.0, 3.0), &mut out);
        assert_eq!(out, vec![MenuAction::Close]);
    }

    #[test]
    fn submenu_open_via_click_and_keyboard() {
        let mut h = MenuHost::new();
        h.set_viewport(800.0, 600.0, 1.0);
        h.open_context((100.0, 100.0), &sample_items());
        // 点击第 2 项(子菜单头)→ 展开;子菜单区注册在顶层之上。
        // 每次按下用全新 flow(按下边沿一次消费,press→release 循环)。
        let click_item1 = |h: &mut MenuHost| {
            let mut f = flow_with(&mut *h);
            press(&mut f, h.state.rect.0 + 40.0, h.state.rect.1 + 4.0 + 30.0 + 14.0)
        };
        let ev = click_item1(&mut h);
        h.dispatch(&ev, &mut Vec::new());
        assert!(h.state.sub.is_some());
        let sub = h.state.sub.as_ref().unwrap();
        assert_eq!(sub.parent, 1);
        // 子菜单项 id 命中。
        let areas = h.areas();
        assert!(areas.iter().any(|a| a.id.0 == AREA_MENU_SUB_BASE));
        assert!(areas.iter().any(|a| a.id.0 == AREA_MENU_SUB_BASE + 1));
        // 再点子菜单头 → 收起。
        let ev = click_item1(&mut h);
        h.dispatch(&ev, &mut Vec::new());
        assert!(h.state.sub.is_none());
        // 键盘:Enter 展开,子项 Enter 触发命令,Esc 收起,Esc 关闭。
        let ev = click_item1(&mut h);
        h.dispatch(&ev, &mut Vec::new());
        assert_eq!(h.on_key(WidgetKey::Enter), Vec::<MenuAction>::new());
        assert!(h.state.sub.is_some());
        h.state.sub.as_mut().unwrap().sel = Some(0);
        assert_eq!(h.on_key(WidgetKey::Enter), vec![MenuAction::Command("sub_a")]);
        h.on_key(WidgetKey::Escape);
        assert!(h.state.sub.is_none());
        let out = h.on_key(WidgetKey::Escape);
        assert_eq!(out, vec![MenuAction::Close]);
    }

    #[test]
    fn keyboard_navigation_up_down_enter_esc() {
        let mut h = MenuHost::new();
        h.set_viewport(800.0, 600.0, 1.0);
        h.open_bar(0.0, &sample_items());
        // 初始无高亮:Down 从第一项开始。
        assert_eq!(h.on_key(WidgetKey::Down), Vec::<MenuAction>::new());
        assert_eq!(h.state.sel, Some(0));
        // Down 跳到 1(Open 已在 0),Down 到 1,再 Down 跳过禁用项(3)→ 2。
        h.on_key(WidgetKey::Down);
        assert_eq!(h.state.sel, Some(1));
        h.on_key(WidgetKey::Down);
        assert_eq!(h.state.sel, Some(2));
        // Up 回到 1。
        h.on_key(WidgetKey::Up);
        assert_eq!(h.state.sel, Some(1));
        // 首项 Enter → Command。
        h.state.sel = Some(0);
        assert_eq!(h.on_key(WidgetKey::Enter), vec![MenuAction::Command("open")]);
        // Esc → Close。
        assert_eq!(h.on_key(WidgetKey::Escape), vec![MenuAction::Close]);
    }

    #[test]
    fn keyboard_skips_disabled_from_bottom() {
        let mut h = MenuHost::new();
        h.set_viewport(800.0, 600.0, 1.0);
        h.open_bar(0.0, &sample_items());
        // 无高亮时 Up 从尾部起:3(禁用)跳过 → 2(Quit)。
        h.on_key(WidgetKey::Up);
        assert_eq!(h.state.sel, Some(2));
    }

    #[test]
    fn geometry_hit_consistency() {
        // 几何 = 命中:每个注册区中心点命中到同 id 区域;行矩形与 snapshot 一致。
        let mut h = MenuHost::new();
        h.set_viewport(800.0, 600.0, 1.0);
        h.open_bar(0.0, &sample_items());
        let mut f0 = flow_with(&mut h);
        h.dispatch(&press(&mut f0, h.state.rect.0 + 40.0, h.state.rect.1 + MENU_ITEM_TOP + 30.0 + 16.0), &mut Vec::new());
        assert!(h.state.sub.is_some());
        let snap = h.snapshot();
        // 每行中心按下 → 命中到对应菜单项区(几何 = 命中)。每次按下用全新
        // flow(按下边沿一次消费);禁用行 flow 永不命中,跳过。
        for row in &snap.rows {
            if row.disabled {
                continue;
            }
            let (cx, cy) = (row.rect.0 + row.rect.2 * 0.5, row.rect.1 + row.rect.3 * 0.5);
            let mut ff = flow_with(&mut h);
            let ev = press(&mut ff, cx, cy);
            let id = ev.pressed.iter().find(|id| id.0 >= AREA_MENU_ITEM_BASE).expect("row center must hit a menu item");
            assert!(id.0 < AREA_MENU_SUB_BASE, "row id {id:?}");
        }
        if let Some((_, sub_rows)) = &snap.sub {
            for row in sub_rows {
                if row.disabled {
                    continue;
                }
                let (cx, cy) = (row.rect.0 + row.rect.2 * 0.5, row.rect.1 + row.rect.3 * 0.5);
                let mut ff = flow_with(&mut h);
                let ev = press(&mut ff, cx, cy);
                assert!(ev.pressed.iter().any(|id| id.0 >= AREA_MENU_SUB_BASE), "sub row center must hit a sub item");
            }
        }
        // 面板外一点命中 mask(最底层捕获)。
        let mut ff = flow_with(&mut h);
        let ev = press(&mut ff, 1.0, 1.0);
        assert!(ev.pressed.iter().any(|id| id.0 == AREA_MENU_MASK), "outside must hit mask");
    }

    #[test]
    fn hover_updates_sel_only_when_mouse_over_item() {
        let mut h = MenuHost::new();
        h.set_viewport(800.0, 600.0, 1.0);
        h.open_bar(0.0, &sample_items());
        h.apply_hover(Some(AreaId(AREA_MENU_ITEM_BASE + 1)));
        assert_eq!(h.state.sel, Some(1));
        // 键盘导航后,悬停不覆盖(keys=true)。
        h.on_key(WidgetKey::Up);
        h.state.sel = Some(0);
        h.apply_hover(Some(AreaId(AREA_MENU_ITEM_BASE + 2)));
        assert_eq!(h.state.sel, Some(0));
        // 悬停在菜单外(mask)不清高亮。
        h.apply_hover(Some(AreaId(AREA_MENU_MASK)));
        assert_eq!(h.state.sel, Some(0));
    }

    #[test]
    fn right_arrow_opens_submenu_left_arrow_closes() {
        let mut h = MenuHost::new();
        h.set_viewport(800.0, 600.0, 1.0);
        h.open_bar(0.0, &sample_items());
        h.state.sel = Some(1);
        h.on_key(WidgetKey::Right);
        assert!(h.state.sub.is_some());
        assert_eq!(h.state.sub.as_ref().unwrap().sel, Some(0));
        h.on_key(WidgetKey::Left);
        assert!(h.state.sub.is_none());
    }
}
