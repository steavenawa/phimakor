//! 独立 UI 组件测试程序。
//!
//! 把所有组件(VList / HList / Button / Toggle / Slider / Field /
//! Panel / ScrollList / Theme / Area)排列成一个"组件画廊"场景,然后:
//!
//! 1. 跑全组件排列测试(默认,headless):
//!    - 每个组件的 `areas()` 与 `hit_area()` 一致(每个区域的中心点命中
//!      到同 kind+id 的区域)
//!    - 组件外一点命中返回 `None`
//!    - 全部组件区域两两不重叠(排列正确)
//!    失败打印明细并返回非零退出码;同时渲染画廊 PNG 到
//!    `target/ui_kit_gallery.png`(带文字绘制)。
//!
//! 2. 窗口交互模式(`--view`):显示画廊,鼠标移动高亮命中项,左键点击打印命中。
//!
//! 用法:
//!   cargo run --bin ui_kit
//!   cargo run --bin ui_kit -- --view

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;

use tiny_skia::{Paint, Rect, Transform};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

#[path = "../ui/widgets.rs"]
mod widgets;

use widgets::{Area, AreaKind, Button, Canvas, Checkbox, ColorPicker, ComboBox, DragValue, Field, Form, FormField, HList, KeyValueGrid, ListBox, Panel, PanelRow, ProgressBar, RTControl, RealtimeForm, ScrollList, Slider, Stepper, TabBar, TextInput, Theme, Toggle, VList, Widget, WidgetKey};

const W: u32 = 1600;
const H: u32 = 1000;

// ── Canvas:tiny_skia + fontdue 绘制后端 ──

struct SkiaCanvas<'a> {
    pm: &'a mut tiny_skia::Pixmap,
    font: Option<&'static fontdue::Font>,
}

impl Canvas for SkiaCanvas<'_> {
    fn fill(&mut self, r: Rect, rgba: [u8; 4]) {
        let w = self.pm.width() as i32;
        let h = self.pm.height() as i32;
        let l = (r.left().floor() as i32).max(0);
        let t = (r.top().floor() as i32).max(0);
        let rr = (r.right().ceil() as i32).min(w);
        let bb = (r.bottom().ceil() as i32).min(h);
        if rr <= l || bb <= t {
            return;
        }
        if let Some(cr) = Rect::from_ltrb(l as f32, t as f32, rr as f32, bb as f32) {
            let mut p = Paint::default();
            p.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
            self.pm.fill_rect(cr, &p, Transform::default(), None);
        }
    }

    fn text(&mut self, s: &str, x: f32, y: f32, size: f32, rgb: [u8; 3]) {
        let Some(font) = self.font else { return };
        let w = self.pm.width() as i32;
        let h = self.pm.height() as i32;
        let mut pen = x;
        for ch in s.chars() {
            let (m, bitmap) = font.rasterize(ch, size);
            let gx0 = pen.round() as i32 + m.xmin;
            let gy0 = (y - m.ymin as f32 - m.height as f32).round() as i32;
            for row in 0..m.height {
                let py = gy0 + row as i32;
                if py < 0 || py >= h {
                    continue;
                }
                for col in 0..m.width {
                    let a = bitmap[row * m.width + col];
                    if a < 160 {
                        continue;
                    }
                    let px = gx0 + col as i32;
                    if px < 0 || px >= w {
                        continue;
                    }
                    let idx = (py * w + px) as usize * 4;
                    let data = self.pm.data_mut();
                    data[idx] = rgb[0];
                    data[idx + 1] = rgb[1];
                    data[idx + 2] = rgb[2];
                    data[idx + 3] = 255;
                }
            }
            pen += m.advance_width;
        }
    }

    fn text_width(&mut self, s: &str, size: f32) -> f32 {
        let Some(font) = self.font else { return s.len() as f32 * size * 0.55 };
        s.chars().map(|ch| font.metrics(ch, size).advance_width).sum()
    }
}

fn load_font() -> Option<&'static fontdue::Font> {
    static FONT: std::sync::OnceLock<Option<fontdue::Font>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        for path in [
            "res/Exo2.ttf",
            "res.dis/Exo2.ttf",
            r"C:\Windows\Fonts\arial.ttf",
            r"C:\Windows\Fonts\segoeui.ttf",
        ] {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(f) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                    return Some(f);
                }
            }
        }
        eprintln!("warning: no UI font found, text disabled");
        None
    })
    .as_ref()
}

// ── 场景 ──

/// 全组件排列场景:所有组件实例摆在同一画布上。
/// `s` = 整体缩放(坐标、尺寸、字号全部 ×s)。
/// 布局:列 A(x=16)/列 B(x=280)/列 C(x=700),各行错开不重叠。
/// 返回 (名称, 组件)。新组件在 build_scene 里加一行即自动进排列测试。
fn build_scene(s: f32) -> Vec<(&'static str, Box<dyn Widget>)> {
    let x = |v: f32| v * s;
    let y = |v: f32| v * s;
    let w = |v: f32| v * s;
    let h = |v: f32| v * s;
    vec![
        // ── 列 A:x=16 ──
        ("VList (gap 0, 4 rows)", Box::new(VList::new(x(16.0), y(52.0), w(240.0), 4).with_row_h(h(22.0)))),
        ("VList (gap 6, 5 rows)", Box::new(VList::new(x(16.0), y(170.0), w(240.0), 5).with_row_h(h(20.0)).with_gap(h(6.0)))),
        ("VList (row_h 28)", Box::new(VList::new(x(16.0), y(320.0), w(240.0), 4).with_row_h(h(28.0)))),
        ("ScrollList 15/5", {
            let mut l = ScrollList::new(x(16.0), y(460.0), w(240.0), 15, 5);
            l.scroll = 4.0;
            l.row_h = h(22.0);
            l.gap = h(4.0);
            Box::new(l)
        }),
        // ── 列 B:x=280 ──
        ("HList (gap 0, 4 items)", Box::new(HList::new(x(280.0), y(52.0), w(360.0), h(24.0), 4).with_gap(0.0))),
        ("HList (gap 8, 5 items)", Box::new(HList::new(x(280.0), y(96.0), w(360.0), h(24.0), 5).with_gap(h(8.0)))),
        ("HList (6 items narrow)", Box::new(HList::new(x(280.0), y(140.0), w(280.0), h(20.0), 6).with_gap(h(2.0)))),
        ("Button enabled", Box::new(Button::new(x(280.0), y(190.0), w(150.0), h(26.0), "Add Effect"))),
        ("Button disabled", {
            let mut b = Button::new(x(450.0), y(190.0), w(150.0), h(26.0), "Remove");
            b.enabled = false;
            Box::new(b)
        }),
        ("Toggle on", Box::new(Toggle::new(x(280.0), y(250.0), w(180.0), h(24.0), "Global", true))),
        ("Toggle off", Box::new(Toggle::new(x(480.0), y(250.0), w(180.0), h(24.0), "Vsync", false))),
        ("Slider 0.35", Box::new(Slider::new(x(280.0), y(310.0), w(320.0), h(20.0), 0.35))),
        ("Slider 0.8", Box::new(Slider::new(x(280.0), y(350.0), w(360.0), h(20.0), 0.8))),
        ("Field editable", {
            let mut f = Field::new(x(280.0), y(400.0), w(360.0), h(24.0), "start", "12.000");
            f.editing = true;
            Box::new(f)
        }),
        ("Field idle", Box::new(Field::new(x(280.0), y(450.0), w(360.0), h(24.0), "end", "14.000"))),
        ("ScrollList 8/3", {
            let mut l = ScrollList::new(x(280.0), y(540.0), w(220.0), 8, 3);
            l.scroll = 1.0;
            l.row_h = h(22.0);
            l.gap = h(4.0);
            Box::new(l)
        }),
        // ── 动画 Slider(draw 时由 KitApp 更新 value,验证持续重绘)──
        ("Slider animated", Box::new(Slider::new(x(280.0), y(620.0), w(400.0), h(26.0), 0.5))),
        // ── 列 C:x=700,宽 860 ──
        ("Panel composite", Box::new(Panel::new(x(700.0), y(52.0), w(860.0), "Effect Settings", vec![
            PanelRow::Button { label: "Apply".into(), enabled: true },
            PanelRow::Toggle { label: "Global".into(), on: true, anim: 1.0, dir: 1.0 },
            PanelRow::Slider { value: 0.45 },
            PanelRow::Field { label: "start".into(), value: "10.500".into(), editing: false },
            PanelRow::Field { label: "end".into(), value: "14.250".into(), editing: true },
        ]))),
        ("Panel rows x6", Box::new(Panel::new(x(700.0), y(240.0), w(860.0), "Timeline", vec![
            PanelRow::Toggle { label: "Events".into(), on: true, anim: 1.0, dir: 1.0 },
            PanelRow::Toggle { label: "Notes".into(), on: false, anim: 0.0, dir: -1.0 },
            PanelRow::Slider { value: 0.1 },
            PanelRow::Slider { value: 0.9 },
            PanelRow::Button { label: "Snap 0.25".into(), enabled: true },
            PanelRow::Field { label: "zoom".into(), value: "8.0".into(), editing: false },
        ]))),
        ("Panel wide x8", Box::new(Panel::new(x(700.0), y(430.0), w(860.0), "Mixing Board", vec![
            PanelRow::Slider { value: 0.2 },
            PanelRow::Slider { value: 0.55 },
            PanelRow::Slider { value: 0.85 },
            PanelRow::Toggle { label: "Mute".into(), on: false, anim: 0.0, dir: -1.0 },
            PanelRow::Toggle { label: "Solo".into(), on: true, anim: 1.0, dir: 1.0 },
            PanelRow::Button { label: "Reset Mix".into(), enabled: true },
            PanelRow::Field { label: "volume".into(), value: "-6.0 dB".into(), editing: false },
            PanelRow::Field { label: "pan".into(), value: "0.5".into(), editing: false },
        ]))),
        // ── 列 D:x=1000,实用元件 ──
        ("ComboBox", Box::new(ComboBox::new(x(1600.0), y(52.0), w(260.0), h(24.0), vec![
            "grayscale".into(), "vignette".into(), "chromatic".into(), "bloom".into(),
        ]))),
        ("Stepper", Box::new(Stepper::new(x(1600.0), y(110.0), w(300.0), h(26.0), "BPM", 120.0))),
        ("Checkbox", Box::new(Checkbox::new(x(1600.0), y(170.0), w(200.0), h(24.0), "Fullscreen", true))),
        ("TabBar", Box::new(TabBar::new(x(1600.0), y(230.0), w(360.0), h(26.0), vec![
            "Timeline".into(), "Events".into(), "Notes".into(),
        ]))),
        ("ProgressBar", Box::new(ProgressBar::new(x(1600.0), y(290.0), w(400.0), h(20.0), "loading 62%", 0.62))),
        ("KeyValueGrid", Box::new(KeyValueGrid::new(x(1600.0), y(350.0), w(400.0), vec![
            ("chart".into(), "Test Chart".into()),
            ("composer".into(), "Example".into()),
            ("level".into(), "IN 15".into()),
            ("duration".into(), "224.0 s".into()),
        ]))),
        // 展开态 ComboBox:主按钮 y=320 不与任何组件重叠,展开的 items
        // 向下盖住 KeyValueGrid 行 —— 验证下拉列表悬浮在 overlay 层。
        ("ComboBox open (overlaps grid)", {
            let mut c = ComboBox::new(x(1600.0), y(320.0), w(260.0), h(24.0), vec![
                "linear".into(), "cubic".into(), "expo".into(),
            ]);
            c.open = true;
            Box::new(c)
        }),
        // ── 数据输入元件(列 D 下部)──
        ("TextInput", Box::new(TextInput::new(x(1600.0), y(470.0), w(380.0), h(28.0), "search chart…"))),
        ("DragValue", {
            let mut d = DragValue::new(x(1600.0), y(530.0), w(380.0), h(26.0), "speed", 1.5);
            d.speed = 0.05;
            d.min = 0.0;
            d.max = 20.0;
            Box::new(d)
        }),
        ("ColorPicker", Box::new(ColorPicker::new(x(1600.0), y(590.0), w(380.0), [0.35, 0.7, 1.0]))),
        ("ListBox", Box::new(ListBox::new(x(1600.0), y(700.0), w(380.0), vec![
            "Alpha".into(), "MoveX".into(), "MoveY".into(), "Rotate".into(), "Speed".into(),
        ]))),
        // ── 表单(列 C 底部,y=680 起两列并排)──
        ("Form", Box::new(Form::new(x(700.0), y(680.0), w(380.0), "Chart Settings", vec![
            FormField::Text { label: "title".into(), value: "Test Chart".into(), insert: 10, caret: 0.0 },
            FormField::Number { label: "bpm".into(), value: 120.0, step: 1.0, min: 40.0, max: 300.0, buf: None },
            FormField::Combo { label: "level".into(), items: vec!["EZ".into(), "HD".into(), "IN".into(), "AT".into()], selected: 2, open: false },
            FormField::Toggle { label: "auto-play".into(), on: false, anim: 0.0, dir: -1.0 },
            FormField::Checkbox { label: "lock".into(), checked: false },
        ]))),
        ("RealtimeForm", Box::new(RealtimeForm::new(x(1100.0), y(680.0), w(380.0), "FX Params (live)", vec![
            ("gain".into(), RTControl::Number { value: 0.0, step: 0.1, min: -12.0, max: 12.0, last_x: 0.0, buf: None }),
            ("wet".into(), RTControl::Slider { value: 0.5 }),
            ("mute".into(), RTControl::Toggle { on: false, anim: 0.0, dir: -1.0 }),
            ("name".into(), RTControl::Text { value: "fx1".into(), insert: 3, caret: 0.0 }),
            ("mode".into(), RTControl::Combo { items: vec!["off".into(), "low".into(), "high".into()], selected: 0, open: false }),
        ]))),
    ]
}

/// 两矩形是否重叠(严格正面积,共享边缘不算)。
fn overlaps(a: &Rect, b: &Rect) -> bool {
    a.right() > b.left() && b.right() > a.left()
        && a.bottom() > b.top() && b.bottom() > a.top()
}

/// `a` 是否完全包含 `b`(允许共享边缘)。
fn contains(a: &Rect, b: &Rect) -> bool {
    a.left() <= b.left() && a.right() >= b.right()
        && a.top() <= b.top() && a.bottom() >= b.bottom()
}

/// 全组件排列测试。返回失败信息列表;空 = 全部通过。
fn run_all_tests(scene: &[(&str, Box<dyn Widget>)]) -> Vec<String> {
    let mut fails = Vec::new();

    // 1. 每个组件:areas() 与 hit_area() 一致 —— 每个区域的中心点必须被命中,
    //    且命中区域与该区域构成"父子包含"关系(Toggle/Slider 的 knob 在
    //    track 内部,中心点落在子区域上命中子区域是正确交互语义)。
    for (si, (name, w)) in scene.iter().enumerate() {
        let areas = w.areas();
        if areas.is_empty() {
            fails.push(format!("[{si}] {name}: areas() 为空"));
            continue;
        }
        for a in &areas {
            let cx = a.rect.x() + a.rect.width() * 0.5;
            let cy = a.rect.y() + a.rect.height() * 0.5;
            match w.hit_area((cx, cy)) {
                Some(h) if (h.kind, h.id) == (a.kind, a.id) => {}
                Some(h) if contains(&h.rect, &a.rect) || contains(&a.rect, &h.rect) => {}
                Some(h) => fails.push(format!(
                    "[{si}] {name}: area {:?} id={} 中心 ({:.1},{:.1}) 命中到 {:?} id={} 不一致(非父子)",
                    a.kind, a.id, a.rect.x() + a.rect.width() * 0.5, a.rect.y() + a.rect.height() * 0.5, h.kind, h.id
                )),
                None => fails.push(format!(
                    "[{si}] {name}: area {:?} id={} 中心未命中",
                    a.kind, a.id
                )),
            }
        }
    }

    // 2. 组件外一点命中必须为 None。
    for (si, (name, w)) in scene.iter().enumerate() {
        let areas = w.areas();
        let Some(first) = areas.first() else { continue };
        let r = first.rect;
        let outside = (r.x() - 8.0, r.y() - 8.0);
        if let Some(h) = w.hit_area(outside) {
            fails.push(format!(
                "[{si}] {name}: 组件外点 ({:.0},{:.0}) 误命中 {:?} id={}",
                outside.0, outside.1, h.kind, h.id
            ));
        }
    }

    // 3. 全部组件区域两两不重叠。共享边缘不算重叠(tiny_skia::Rect::
    //    intersect 对零宽/零高交集也返回 Some,须用严格正面积判定)。
    //    同一组件内允许"完全包含"(Toggle 的 knob ⊂ track、Slider 的
    //    knob ⊂ track 是合法子区域),禁止部分交叉;跨组件禁止一切重叠。
    //    例外:overlay 层区域(下拉列表等)画在悬浮层,允许盖住下方组件。
    let is_overlay = |k: AreaKind| matches!(k, AreaKind::ComboBoxItem);
    let with_idx: Vec<(String, usize, usize, Area)> = scene.iter()
        .enumerate()
        .flat_map(|(si, (name, w))| {
            w.areas().into_iter().enumerate().map(move |(li, a)| (name.to_string(), si, li, a))
        })
        .collect();
    for i in 0..with_idx.len() {
        for j in (i + 1)..with_idx.len() {
            let (name_i, si, li, a) = &with_idx[i];
            let (name_j, sj, _, b) = &with_idx[j];
            if is_overlay(a.kind) || is_overlay(b.kind) {
                continue;
            }
            let a_rect = &a.rect;
            let b_rect = &b.rect;
            if !overlaps(a_rect, b_rect) {
                continue;
            }
            let same_component = si == sj;
            let partial = !contains(a_rect, b_rect) && !contains(b_rect, a_rect);
            if !same_component || partial {
                fails.push(format!(
                    "排列重叠: [{name_i}]#{li} {:?} id={} ({:.0},{:.0} {:.0}x{:.0}) ∩ [{name_j}]#{li} {:?} id={} ({:.0},{:.0} {:.0}x{:.0})",
                    a.kind, a.id, a_rect.x(), a_rect.y(), a_rect.width(), a_rect.height(),
                    b.kind, b.id, b_rect.x(), b_rect.y(), b_rect.width(), b_rect.height(),
                ));
            }
        }
    }

    // 4. 组件种类覆盖:场景里出现的 AreaKind 集合必须覆盖全部交互语义
    //    (防止新组件忘了加进画廊)。
    let used: std::collections::BTreeSet<AreaKind> = with_idx.iter().map(|(_, _, _, a)| a.kind).collect();
    let all_kinds: [AreaKind; 25] = [
        AreaKind::ListRow, AreaKind::Button, AreaKind::ToggleTrack, AreaKind::ToggleKnob,
        AreaKind::SliderTrack, AreaKind::SliderKnob, AreaKind::Field, AreaKind::ScrollRow,
        AreaKind::ScrollBar, AreaKind::PanelTitle, AreaKind::ComboBoxButton,
        AreaKind::ComboBoxItem, AreaKind::StepperField, AreaKind::StepperMinus,
        AreaKind::StepperPlus, AreaKind::Checkbox, AreaKind::CheckboxBox,
        AreaKind::TabBarTab, AreaKind::ProgressBar, AreaKind::GridRow,
        AreaKind::TextInput, AreaKind::DragValue, AreaKind::ColorChannel,
        AreaKind::ColorPreview, AreaKind::ListBoxRow,
    ];
    for k in all_kinds {
        if !used.contains(&k) {
            fails.push(format!("场景缺少组件种类: {k:?}"));
        }
    }

    fails
}

/// 渲染画廊到 pixmap。`hover`: (场景项索引, 区域 id) 或 None。
fn draw_gallery(pm: &mut tiny_skia::Pixmap, theme: &Theme, scene: &[(&str, Box<dyn Widget>)], hover: Option<(usize, u32)>) {
    pm.fill(tiny_skia::Color::BLACK);
    let (pw, ph) = (pm.width() as f32, pm.height() as f32);

    let mut cv = SkiaCanvas { pm, font: load_font() };

    let bg = theme.bg;
    if let Some(r) = Rect::from_xywh(8.0, 8.0, pw - 16.0, ph - 16.0) {
        cv.fill(r, bg);
    }

    for (si, (name, w)) in scene.iter().enumerate() {
        // 组件名标签
        cv.text(name, w.areas().first().map(|a| a.rect.x()).unwrap_or(16.0), {
            let top = w.areas().first().map(|a| a.rect.y()).unwrap_or(0.0);
            top - 8.0
        }, theme.font_size * 0.9, theme.text_dim);

        let hover_area = hover.and_then(|(si2, id)| {
            if si2 != si { return None; }
            w.areas().into_iter().find(|a| a.id == id)
        });
        w.draw(&mut cv, theme, hover_area.as_ref());
    }
    // 第二遍:overlay 层(下拉列表等悬浮层),确保在所有组件之上。
    for (si, (_, w)) in scene.iter().enumerate() {
        let hover_area = hover.and_then(|(si2, id)| {
            if si2 != si { return None; }
            w.areas().into_iter().find(|a| a.id == id)
        });
        w.draw_overlay(&mut cv, theme, hover_area.as_ref());
    }
}

/// 窗口模式:交互查看组件画廊,持续重绘(动画 slider 驱动)。
struct KitApp {
    window: Arc<Window>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    theme: Theme,
    scene: Vec<(&'static str, Box<dyn Widget>)>,
    hover: Option<(usize, u32)>,
    /// 动画 slider 的场景索引。
    anim_slider: usize,
    start: std::time::Instant,
    last_frame: std::time::Instant,
    scale: f32,
    /// 正在拖拽的组件索引(拖拽中的 slider / 滚动条)。
    drag: Option<usize>,
    /// 键盘焦点组件(TextInput 等)。
    focus: Option<usize>,
    /// Shift 是否按下(ModifiersChanged 跟踪,用于 Shift+Tab)。
    shift: bool,
    /// 最近一次指针位置(物理像素,与场景坐标一致)。
    mouse_pos: Option<(f32, f32)>,
}

impl KitApp {
    fn draw(&mut self) {
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        // 每帧推进所有组件动画(toggle ease 等)。
        for (_, w) in self.scene.iter_mut() {
            w.update(dt);
        }
        // 动画 slider:值随时间正弦摆动(持续重绘的演示 + 验证)。
        let t = self.start.elapsed().as_secs_f64();
        let v = (0.5 + 0.5 * (t * 1.8).sin()) as f32;
        // 重建动画 slider(纯数据组件,每帧重新构造很便宜)。
        let s = self.scale;
        let sl = Slider::new(280.0 * s, 620.0 * s, 400.0 * s, 26.0 * s, v);
        self.scene[self.anim_slider] = ("Slider animated", Box::new(sl));

        let size = self.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        let Some(mut pm) = tiny_skia::Pixmap::new(w.get(), h.get()) else { return };
        draw_gallery(&mut pm, &self.theme, &self.scene, self.hover);

        if self.surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = self.surface.buffer_mut() else { return };
        let data = pm.data();
        for (dst, src) in buffer.iter_mut().zip(data.chunks_exact(4)) {
            // softbuffer: 0x00RRGGBB (opaque)
            *dst = ((src[0] as u32) << 16) | ((src[1] as u32) << 8) | (src[2] as u32);
        }
        let _ = buffer.present();
        self.last_frame = std::time::Instant::now();
    }
}

impl ApplicationHandler for KitApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let _ = event_loop;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // 持续重绘:每帧都请求(动画 slider 在动)。
        self.window.request_redraw();
        let _ = event_loop;
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::CursorMoved { position, .. } => {
                let (mx, my) = (position.x as f32, position.y as f32);
                self.mouse_pos = Some((mx, my));
                // 拖拽中:值跟随指针。
                if let Some(si) = self.drag {
                    self.scene[si].1.on_drag((mx, my));
                    return;
                }
                let mut found = None;
                for (si, (_, w)) in self.scene.iter().enumerate() {
                    if let Some(a) = w.hit_area((mx, my)) {
                        found = Some((si, a.id));
                        break;
                    }
                }
                if found != self.hover {
                    self.hover = found;
                }
            }
            WindowEvent::MouseInput { state, button: winit::event::MouseButton::Left, .. } => {
                let Some((mx, my)) = self.mouse_pos else { return };
                if state == winit::event::ElementState::Pressed {
                    // 点击:先切换状态(toggle/button/field);可拖组件进入拖拽。
                    let mut hit: Option<(usize, AreaKind, u32)> = None;
                    for (si, (_, w)) in self.scene.iter().enumerate() {
                        if let Some(a) = w.hit_area((mx, my)) {
                            hit = Some((si, a.kind, a.id));
                            break;
                        }
                    }
                    let Some((si, kind, id)) = hit else { return };
                    let name = self.scene[si].0;
                    // 焦点:文本输入类组件获得焦点,其他组件失焦。
                    let needs_focus = matches!(kind, AreaKind::TextInput);
                    if let Some(fi) = self.focus {
                        if fi != si {
                            self.scene[fi].1.set_focus(false);
                        }
                    }
                    self.focus = if needs_focus { Some(si) } else { None };
                    self.scene[si].1.set_focus(needs_focus);
                    self.scene[si].1.on_click((mx, my));
                    if matches!(kind, AreaKind::SliderTrack | AreaKind::SliderKnob | AreaKind::ScrollBar | AreaKind::DragValue | AreaKind::ColorChannel | AreaKind::ProgressBar) {
                        self.drag = Some(si);
                    }
                    println!("click: {name} → {kind:?} id={id}");
                } else {
                    self.drag = None;
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.shift = m.state().shift_key();
            }            WindowEvent::KeyboardInput { event, .. } => {
                // 统一转发为 WidgetKey:焦点组件处理;无焦点时给 hover 组件(Form/RealtimeForm 的行导航)。
                use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
                let shift = self.shift;
                let k = match &event.logical_key {
                    Key::Character(s) => {
                        let mut chars = s.chars();
                        let c = chars.next();
                        if chars.next().is_none() {
                            c.map(WidgetKey::Char)
                        } else {
                            None
                        }
                    }
                    Key::Named(NamedKey::Backspace) => Some(WidgetKey::Backspace),
                    Key::Named(NamedKey::ArrowLeft) => Some(WidgetKey::Left),
                    Key::Named(NamedKey::ArrowRight) => Some(WidgetKey::Right),
                    Key::Named(NamedKey::Home) => Some(WidgetKey::Home),
                    Key::Named(NamedKey::End) => Some(WidgetKey::End),
                    Key::Named(NamedKey::Enter) => Some(WidgetKey::Enter),
                    Key::Named(NamedKey::Tab) => {
                        if shift {
                            Some(WidgetKey::ShiftTab)
                        } else {
                            Some(WidgetKey::Tab)
                        }
                    }
                    Key::Named(NamedKey::Escape) => Some(WidgetKey::Escape),
                    _ => match event.physical_key {
                        PhysicalKey::Code(KeyCode::NumpadEnter) => Some(WidgetKey::Enter),
                        _ => None,
                    },
                };
                if let Some(k) = k {
                    if let Some(si) = self.focus {
                        self.scene[si].1.on_key(k);
                        // 组件自己管理失焦(Tab 到末尾后 Esc 由宿主处理)
                        if k == WidgetKey::Escape {
                            self.scene[si].1.set_focus(false);
                            self.focus = None;
                        }
                    } else if let Some((si, _)) = self.hover {
                        // 无焦点:Tab/Enter 给 hover 的行导航组件(Form)
                        if matches!(k, WidgetKey::Tab | WidgetKey::ShiftTab | WidgetKey::Enter) {
                            self.scene[si].1.on_key(k);
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                    winit::event::MouseScrollDelta::PixelDelta(p) => (p.y / 30.0) as f32,
                };
                if dy != 0.0 {
                    if let Some(si) = self.focus {
                        self.scene[si].1.on_wheel(dy);
                    } else if let Some((si, _)) = self.hover {
                        self.scene[si].1.on_wheel(dy);
                    }
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let view = args.iter().any(|a| a == "--view");
    // 整体缩放:元件变大/变小(窗口、场景坐标、字号、PNG 全部跟随)。
    let scale = args.iter().find_map(|a| {
        a.strip_prefix("--scale=").and_then(|v| v.parse::<f32>().ok())
    }).unwrap_or(1.5);
    let scale = scale.max(0.5).min(3.0);

    let theme = Theme::default().scaled(scale);
    let scene = build_scene(scale);
    let anim_slider = scene.iter().position(|(name, _)| *name == "Slider animated")
        .expect("scene must contain the animated slider");

    if view {
        let event_loop = EventLoop::new().expect("event loop");
        // Poll + about_to_wait 持续请求重绘 → 动画 slider 每帧摆动。
        event_loop.set_control_flow(ControlFlow::Poll);
        let window = Arc::new(
            event_loop
                .create_window(WindowAttributes::default()
                    .with_title("phimakor ui-kit")
                    .with_inner_size(LogicalSize::new(W as f64 * scale as f64, H as f64 * scale as f64)))
                .expect("window"),
        );
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface = softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        let mut app = KitApp {
            window, surface, theme, scene, hover: None,
            anim_slider, start: std::time::Instant::now(),
            last_frame: std::time::Instant::now(),
            scale, drag: None, focus: None, shift: false, mouse_pos: None,
        };
        app.draw();
        event_loop.run_app(&mut app).expect("run");
        return;
    }

    // Headless: 全组件排列测试 + 渲染 PNG。
    let fails = run_all_tests(&scene);
    println!("全组件排列测试 (scale={scale}):");
    println!("  场景组件数: {}", scene.len());
    let area_count: usize = scene.iter().map(|(_, w)| w.areas().len()).sum();
    println!("  总区域数  : {area_count}");
    if fails.is_empty() {
        println!("  PASS — 全部通过");
    } else {
        for f in &fails {
            println!("  FAIL: {f}");
        }
    }

    let out = PathBuf::from("target/ui_kit_gallery.png");
    let (pw, ph) = ((W as f32 * scale) as u32, (H as f32 * scale) as u32);
    if let Some(mut pm) = tiny_skia::Pixmap::new(pw, ph) {
        draw_gallery(&mut pm, &theme, &scene, None);
        if let Ok(png) = pm.encode_png() {
            if std::fs::create_dir_all("target").is_ok() {
                if std::fs::write(&out, png).is_ok() {
                    println!("画廊已渲染: {} ({}x{})", out.display(), pw, ph);
                }
            }
        }
    }

    if !fails.is_empty() {
        std::process::exit(1);
    }
}






