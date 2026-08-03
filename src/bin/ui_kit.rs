//! 独立 UI 组件测试程序。
//!
//! 把所有组件(VList / HList / Theme / Area)排列成一个"组件画廊"场景,
//! 然后:
//!
//! 1. 跑全组件排列测试(默认,headless):
//!    - 每个组件的 `areas()` 与 `hit()` 一致(每个区域的中心点命中到对应 id)
//!    - 组件外一点命中返回 `None`
//!    - 全部组件区域两两不重叠(排列正确)
//!    失败打印明细并返回非零退出码;同时渲染画廊 PNG 到 `target/ui_kit_gallery.png`。
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

use widgets::{Area, HList, Theme, VList};

const W: u32 = 900;
const H: u32 = 620;

/// 场景中的一个组件(带名字 + 摆放位置)。
struct SceneItem {
    name: &'static str,
    x: f32,
    y: f32,
    kind: Kind,
}

enum Kind {
    VList(VList),
    HList(HList),
}

impl Kind {
    fn areas(&self) -> Vec<Area> {
        match self {
            Kind::VList(v) => v.areas(),
            Kind::HList(h) => h.areas(),
        }
    }

    fn hit(&self, p: (f32, f32)) -> Option<usize> {
        match self {
            Kind::VList(v) => v.hit(p),
            Kind::HList(h) => h.hit(p),
        }
    }
}

/// 全组件排列场景:所有组件实例摆在同一画布上。
fn build_scene() -> Vec<SceneItem> {
    vec![
        SceneItem {
            name: "VList (gap 0, 3 rows)",
            x: 16.0, y: 52.0,
            kind: Kind::VList(VList::new(16.0, 52.0, 160.0, 3).with_row_h(22.0)),
        },
        SceneItem {
            name: "VList (gap 6, 4 rows)",
            x: 200.0, y: 52.0,
            kind: Kind::VList(VList::new(200.0, 52.0, 160.0, 4).with_row_h(20.0).with_gap(6.0)),
        },
        SceneItem {
            name: "VList (row_h 28)",
            x: 384.0, y: 52.0,
            kind: Kind::VList(VList::new(384.0, 52.0, 140.0, 3).with_row_h(28.0)),
        },
        SceneItem {
            name: "HList (gap 0, 3 items)",
            x: 16.0, y: 180.0,
            kind: Kind::HList(HList::new(16.0, 180.0, 240.0, 20.0, 3).with_gap(0.0)),
        },
        SceneItem {
            name: "HList (gap 8, 4 items)",
            x: 280.0, y: 180.0,
            kind: Kind::HList(HList::new(280.0, 180.0, 240.0, 24.0, 4).with_gap(8.0)),
        },
        SceneItem {
            name: "HList (5 items narrow)",
            x: 16.0, y: 240.0,
            kind: Kind::HList(HList::new(16.0, 240.0, 200.0, 18.0, 5).with_gap(2.0)),
        },
        // 组合排列:一个"面板" = 按钮排(HList)在上,内容行(VList)在下。
        SceneItem {
            name: "Panel: HList header + VList rows",
            x: 540.0, y: 52.0,
            kind: Kind::HList(HList::new(540.0, 52.0, 340.0, 22.0, 3).with_gap(6.0)),
        },
        SceneItem {
            name: "Panel: VList rows",
            x: 540.0, y: 92.0,
            kind: Kind::VList(VList::new(540.0, 92.0, 340.0, 6).with_row_h(24.0).with_gap(4.0)),
        },
        // 极窄场景:单行、单列(边界/退化情形)。
        SceneItem {
            name: "VList (1 row)",
            x: 16.0, y: 320.0,
            kind: Kind::VList(VList::new(16.0, 320.0, 120.0, 1).with_row_h(22.0)),
        },
        SceneItem {
            name: "HList (1 item)",
            x: 160.0, y: 320.0,
            kind: Kind::HList(HList::new(160.0, 320.0, 100.0, 20.0, 1).with_gap(0.0)),
        },
    ]
}

/// 全组件排列测试。返回失败信息列表;空 = 全部通过。
fn run_all_tests(scene: &[SceneItem]) -> Vec<String> {
    let mut fails = Vec::new();

    // 1. 每个组件:areas() 与 hit() 一致 —— 每个区域的中心点必须命中到该 id。
    for (si, item) in scene.iter().enumerate() {
        let areas = item.kind.areas();
        if areas.is_empty() {
            fails.push(format!("[{si}] {}: areas() 为空", item.name));
            continue;
        }
        for a in &areas {
            let cx = a.rect.x() + a.rect.width() * 0.5;
            let cy = a.rect.y() + a.rect.height() * 0.5;
            match item.kind.hit((cx, cy)) {
                Some(h) if h as u32 == a.id => {}
                Some(h) => fails.push(format!(
                    "[{si}] {}: area id={} 中心 ({cx:.1},{cy:.1}) 命中 id={h} 不一致",
                    item.name, a.id
                )),
                None => fails.push(format!(
                    "[{si}] {}: area id={} 中心 ({cx:.1},{cy:.1}) 未命中",
                    item.name, a.id
                )),
            }
        }
    }

    // 2. 组件外一点命中必须为 None。
    for (si, item) in scene.iter().enumerate() {
        let areas = item.kind.areas();
        let Some(first) = areas.first() else { continue };
        let r = first.rect;
        let outside = (r.x() - 8.0, r.y() - 8.0);
        if let Some(h) = item.kind.hit(outside) {
            fails.push(format!(
                "[{si}] {}: 组件外点 ({:.0},{:.0}) 误命中 id={h}",
                item.name, outside.0, outside.1
            ));
        }
    }

    // 3. 全部组件区域两两不重叠(全组件排列正确)。共享边缘不算重叠
    //    (tiny_skia::Rect::intersect 对零宽/零高交集也返回 Some,须用
    //    严格正面积判定)。
    let all: Vec<Area> = scene.iter().flat_map(|i| i.kind.areas()).collect();
    for i in 0..all.len() {
        for j in (i + 1)..all.len() {
            let a = &all[i];
            let b = &all[j];
            let overlap = a.rect.right() > b.rect.left()
                && b.rect.right() > a.rect.left()
                && a.rect.bottom() > b.rect.top()
                && b.rect.bottom() > a.rect.top();
            if overlap {
                fails.push(format!(
                    "排列重叠: 区域 #{i} id={} ({:.0},{:.0} {:.0}x{:.0}) ∩ #{j} id={} ({:.0},{:.0} {:.0}x{:.0})",
                    a.id, a.rect.x(), a.rect.y(), a.rect.width(), a.rect.height(),
                    b.id, b.rect.x(), b.rect.y(), b.rect.width(), b.rect.height(),
                ));
            }
        }
    }

    fails
}

/// 渲染画廊到 pixmap。`hover`: (场景项索引, 区域 id) 或 None。
fn draw_gallery(pm: &mut tiny_skia::Pixmap, theme: &Theme, scene: &[SceneItem], hover: Option<(usize, u32)>) {
    pm.fill(tiny_skia::Color::BLACK);

    let bg = theme.bg;
    let mut bgp = Paint::default();
    bgp.set_color_rgba8(bg[0], bg[1], bg[2], bg[3]);
    if let Some(r) = Rect::from_xywh(8.0, 8.0, pm.width() as f32 - 16.0, pm.height() as f32 - 16.0) {
        pm.fill_rect(r, &bgp, Transform::default(), None);
    }

    let row = theme.row;
    let hover_c = theme.hover;
    let mut rp = Paint::default();
    rp.set_color_rgba8(row[0], row[1], row[2], row[3]);
    let mut hp = Paint::default();
    hp.set_color_rgba8(hover_c[0], hover_c[1], hover_c[2], hover_c[3]);

    for (si, item) in scene.iter().enumerate() {
        for a in item.kind.areas() {
            let on = hover == Some((si, a.id));
            let p = if on { &hp } else { &rp };
            if let Some(r) = Rect::from_xywh(a.rect.x(), a.rect.y(), a.rect.width(), a.rect.height()) {
                pm.fill_rect(r, p, Transform::default(), None);
            }
            // 区域编号(小数字,直接用像素块画 —— 避免字体依赖)
            let n = format!("{}{}", if on { "*" } else { "" }, a.id);
            let fw = (n.len() * 4) as f32;
            let mut tp = Paint::default();
            tp.set_color_rgba8(theme.text[0], theme.text[1], theme.text[2], 255);
            if let Some(r) = Rect::from_xywh(a.rect.x() + theme.pad_x, a.rect.y() + 6.0, fw, 10.0) {
                pm.fill_rect(r, &tp, Transform::default(), None);
            }
        }
    }
}

/// 窗口模式:交互查看组件画廊。
struct KitApp {
    window: Arc<Window>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    theme: Theme,
    scene: Vec<SceneItem>,
    hover: Option<(usize, u32)>,
}

impl KitApp {
    fn draw(&mut self) {
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
    }
}

impl ApplicationHandler for KitApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let _ = event_loop;
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::CursorMoved { position, .. } => {
                let (mx, my) = (position.x as f32, position.y as f32);
                let mut found = None;
                for (si, item) in self.scene.iter().enumerate() {
                    if let Some(id) = item.kind.hit((mx, my)) {
                        found = Some((si, id as u32));
                        break;
                    }
                }
                if found != self.hover {
                    self.hover = found;
                    self.window.request_redraw();
                }
            }
            WindowEvent::MouseInput { state: winit::event::ElementState::Pressed, button: winit::event::MouseButton::Left, .. } => {
                if let Some((si, id)) = self.hover {
                    println!("click: {} → id={}", self.scene[si].name, id);
                } else {
                    println!("click: (no component)");
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

    let theme = Theme::default();
    let scene = build_scene();

    if view {
        let event_loop = EventLoop::new().expect("event loop");
        event_loop.set_control_flow(ControlFlow::Wait);
        let window = Arc::new(
            event_loop
                .create_window(WindowAttributes::default()
                    .with_title("phimakor ui-kit")
                    .with_inner_size(LogicalSize::new(W as f64, H as f64)))
                .expect("window"),
        );
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface = softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        let mut app = KitApp { window, surface, theme, scene, hover: None };
        app.draw();
        event_loop.run_app(&mut app).expect("run");
        return;
    }

    // Headless: 全组件排列测试 + 渲染 PNG。
    let fails = run_all_tests(&scene);
    println!("全组件排列测试:");
    println!("  场景组件数: {}", scene.len());
    let area_count: usize = scene.iter().map(|i| i.kind.areas().len()).sum();
    println!("  总区域数  : {area_count}");
    if fails.is_empty() {
        println!("  PASS — 全部通过");
    } else {
        for f in &fails {
            println!("  FAIL: {f}");
        }
    }

    let out = PathBuf::from("target/ui_kit_gallery.png");
    if let Some(mut pm) = tiny_skia::Pixmap::new(W, H) {
        draw_gallery(&mut pm, &theme, &scene, None);
        if let Ok(png) = pm.encode_png() {
            if std::fs::create_dir_all("target").is_ok() {
                if std::fs::write(&out, png).is_ok() {
                    println!("画廊已渲染: {}", out.display());
                }
            }
        }
    }

    if !fails.is_empty() {
        std::process::exit(1);
    }
}
