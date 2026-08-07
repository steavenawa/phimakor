//! 浏览器渲染器:wgpu(WebGPU)按快照画线/音符。
//!
//! 桌面编辑器渲染管线(src/render/mod.rs)的播放端简化复刻:
//! - Instance 布局 / quad 管线 / WGSL 与主仓逐字段一致(顶点色 alpha 混合)
//! - letterbox_transform + 线/音符变换复刻(3:2 播放域)
//! - 纹理槽表:0 = 程序生成白 1×1,1..6 握手 PNG(固定顺序见 PROTOCOL.md),
//!   7+ 谱面自定义线纹理;缺失槽回退白
//! - hold 按 end_y 拆 head/tail/body 三段(简化:body 一个 quad)
//! - fx(0x02)第一版跳过:见 snap.rs 占位注释,粒子系统接入时补充

use std::mem::size_of;

use anyhow::Context as _;
use bytemuck::{Pod, Zeroable};

use crate::snap::{LineSnap, NoteSnap, Snapshot};

/// 播放域纵横比(3:2 = RPE 原生画布 1350×900),与主仓 render::ASPECT 同值。
const ASPECT: f32 = 3.0 / 2.0;
const CANVAS_W: f32 = 675.0; // 世界 x ±1 ↔ ±675 canvas px
const CANVAS_H: f32 = 450.0;
/// 内建线 quad 全长:phira draw_line(-len, 0, len, 0),len=6 世界单位。
const LINE_LEN: f32 = 2.0 * 6.0 * CANVAS_W;
const LINE_THICK: f32 = 0.01 * CANVAS_H;
/// prpr note sprite 宽:2 × NOTE_WIDTH_RATIO_BASE (0.13175016)。
const NOTE_SPRITE_W: f32 = 0.2635 * CANVAS_W;
const NOTE_W: f32 = 0.22 * CANVAS_W; // 纯色降级 quad 宽
const NOTE_H: f32 = 0.03 * CANVAS_H; // 纯色降级 quad 高
const HOLD_BODY_W: f32 = 0.1 * CANVAS_W; // 纯色降级 hold body 宽
/// hold 图集 cap 高(官方 respack:上 50px head,下 50px tail)。
const HOLD_CAP_PX: f32 = 50.0;

// 音符降级色(kind 与 RPE 编号一致),主仓同值。
const TAP_COLOR: [f32; 3] = [0.2, 0.6, 1.0];
const HOLD_COLOR: [f32; 3] = [0.3, 0.7, 1.0];
const FLICK_COLOR: [f32; 3] = [1.0, 0.3, 0.5];
const DRAG_COLOR: [f32; 3] = [1.0, 0.85, 0.2];

// ---------------------------------------------------------------------------
// 2D 仿射矩阵,列主序,3×vec4 填充(与 WGSL uniform 布局一致)。
// 列:(a, b, 0), (c, d, 0), (tx, ty, 1) — x' = a·x + c·y + tx。
// ---------------------------------------------------------------------------

type Mat3 = [[f32; 4]; 3];

fn mat_translate(x: f32, y: f32) -> Mat3 {
    [[1., 0., 0., 0.], [0., 1., 0., 0.], [x, y, 1., 0.]]
}

fn mat_scale(x: f32, y: f32) -> Mat3 {
    [[x, 0., 0., 0.], [0., y, 0., 0.], [0., 0., 1., 0.]]
}

fn mat_rotate(rad: f32) -> Mat3 {
    let (s, c) = rad.sin_cos();
    [[c, s, 0., 0.], [-s, c, 0., 0.], [0., 0., 1., 0.]]
}

/// `a * b`(先应用 b,再应用 a)。
fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut r = [[0.; 4]; 3];
    for j in 0..3 {
        for i in 0..3 {
            r[j][i] = a[0][i] * b[j][0] + a[1][i] * b[j][1] + a[2][i] * b[j][2];
        }
    }
    r
}

/// 复刻主仓 letterbox_transform:窗口比例 vs 播放域比例决定压边轴。
/// 返回 (letterbox 矩阵, kx, ev_x, ev_y)。ev_y = 1.5/aspect 把 RPE 画布
/// 半高(450)映射到 box 边缘。
fn letterbox_transform(window_aspect: f32, playfield_aspect: f32) -> (Mat3, f32, f32, f32) {
    let aspect = playfield_aspect;
    let (kx, ky) = if window_aspect >= aspect {
        (aspect / window_aspect, 1.0)
    } else {
        (1.0, window_aspect / aspect)
    };
    let letterbox = mat_scale(kx / CANVAS_W, ky * aspect / CANVAS_W);
    let ev_x = 1.0;
    let ev_y = 1.5 / aspect;
    (letterbox, kx, ev_x, ev_y)
}

// ---------------------------------------------------------------------------
// GPU 实例记录(64B):仿射 + 色调 + uv 矩形。与主仓 Instance 同布局,
// 顶点着色器按 4 个 vec4 实例属性消费(m01 / m2p / color / uv_rect)。
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Instance {
    /// [a, b, c, d, tx, ty, 0, 0]:a..d 线性部分,tx/ty 平移。
    model: [f32; 8],
    /// RGBA 色调(直通,非预乘;与纹理相乘后 src-over 混合)。
    color: [f32; 4],
    /// [u0, v0, du, dv]:最终 uv = (u0, v0) + corner × (du, dv)。
    uv_rect: [f32; 4],
}

const INSTANCE_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: size_of::<Instance>() as u64,
    step_mode: wgpu::VertexStepMode::Instance,
    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4],
};

/// 单位 quad(±0.5)triangle strip + per-instance 仿射变换 + uv + 顶点色,
/// 与主仓 render/mod.rs 的 SHADER 逐字一致。
const SHADER: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsIn {
    @location(0) m01: vec4<f32>,  // a, b, c, d
    @location(1) m2p: vec4<f32>,  // tx, ty, pad, pad
    @location(2) color: vec4<f32>,
    @location(3) uv_rect: vec4<f32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
};

// Unit quad (±0.5) as a triangle strip; one instance per quad.
@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: VsIn) -> VsOut {
    let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let p = corner - 0.5;
    let world = vec2<f32>(
        inst.m01.x * p.x + inst.m01.z * p.y + inst.m2p.x,
        inst.m01.y * p.x + inst.m01.w * p.y + inst.m2p.y,
    );
    var out: VsOut;
    out.pos = vec4<f32>(world, 0.0, 1.0);
    out.uv = inst.uv_rect.xy + corner * inst.uv_rect.zw;
    out.color = inst.color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv) * in.color;
}
"#;

/// 纹理槽条目:槽 0 = 程序生成白 1×1,1..6 握手 PNG(固定顺序:1 click,
/// 2 drag, 3 flick, 4 hold, 5 hitfx, 6 line),7+ 谱面自定义线纹理。
/// `size` = 解码后像素尺寸(纹理线/音符按像素尺寸 × 缩放绘制)。
struct TexEntry {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    size: [f32; 2],
}

/// 浏览器渲染器:surface + quad 管线 + 纹理槽表。
pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    bgl: wgpu::BindGroupLayout,
    /// 纹理槽表,None = 槽缺失 → 回退槽 0(白)。
    slots: Vec<Option<TexEntry>>,
}

impl Renderer {
    /// 新建渲染器:配置 surface + 建 quad 管线 + 程序生成白 1×1 纹理。
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        surface: wgpu::Surface<'static>,
        config: wgpu::SurfaceConfiguration,
    ) -> Self {
        surface.configure(&device, &config);
        let (pipeline, bgl, sampler) = create_pipeline(&device, config.format);
        let white = create_texture(&device, &queue, &[255, 255, 255, 255], 1, 1);
        let white_bg = texture_bind_group(&device, &bgl, &sampler, &white);
        let slots = vec![Some(TexEntry { _texture: white, bind_group: white_bg, size: [1.0, 1.0] })];
        Renderer { device, queue, surface, config, pipeline, sampler, bgl, slots }
    }

    /// 上传一条握手 PNG 到指定槽位(解码 → 垂直翻转 → 建纹理)。
    pub fn upload_png(&mut self, slot: u8, _name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        // 统一转 RGBA8(展开灰度/调色板/加 alpha,剥 16bit),texel 布局确定。
        let mut decoder = png::Decoder::new(bytes);
        decoder.set_transformations(
            png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
        );
        let mut reader = decoder.read_info().context("png decode")?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).context("png frame")?;
        let (w, h) = (info.width.max(1), info.height.max(1));
        buf.truncate(w as usize * h as usize * 4);
        // 垂直翻转:wgpu v=0 是第一行;翻转后 v=0 = 图片顶部(与主仓一致,
        // hold 图集 head 在上 → v=0,tail 在下 → v=1)。
        flip_vertical(&mut buf, w, h);
        let tex = create_texture(&self.device, &self.queue, &buf, w, h);
        let bg = texture_bind_group(&self.device, &self.bgl, &self.sampler, &tex);
        let entry = TexEntry { _texture: tex, bind_group: bg, size: [w as f32, h as f32] };
        let slot = slot as usize;
        if slot >= self.slots.len() {
            self.slots.resize_with(slot + 1, || None);
        }
        self.slots[slot] = Some(entry);
        Ok(())
    }

    /// 画一帧快照:清屏(黑)→ 按 z 升序画线 + 音符 → present。
    pub fn draw_snapshot(&mut self, snap: &Snapshot) -> anyhow::Result<()> {
        let aspect = self.config.width as f32 / self.config.height as f32;
        let (letterbox, kx, ev_x, ev_y) = letterbox_transform(aspect, ASPECT);
        let mut draw: Vec<(Instance, usize)> = Vec::new();
        // 按 z 升序画线(小 z 在底,大 z 在上),与主仓 sort_by_key(z_order) 一致。
        let mut lines: Vec<&LineSnap> = snap.lines.iter().collect();
        lines.sort_by_key(|l| l.z);
        for line in lines {
            build_line(&mut draw, line, &letterbox, kx, ev_x, ev_y, &self.slots);
        }
        // dim:全局背景压暗,CPU 侧乘到 rgb(与主仓 to_instances 同语义)。
        let dim = snap.dim.clamp(0.0, 1.0);
        for (inst, _) in draw.iter_mut() {
            inst.color[0] *= dim;
            inst.color[1] *= dim;
            inst.color[2] *= dim;
        }

        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            s => anyhow::bail!("surface acquire failed: {s:?}"),
        };
        let view = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("player-frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("player-scene"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            if !draw.is_empty() {
                let instances: Vec<Instance> = draw.iter().map(|(i, _)| *i).collect();
                let slots: Vec<usize> = draw.iter().map(|(_, s)| *s).collect();
                let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("player-instances"),
                    size: (instances.len() as u64 * size_of::<Instance>() as u64).max(1),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.queue.write_buffer(&buf, 0, bytemuck::cast_slice(&instances));
                pass.set_vertex_buffer(0, buf.slice(..));
                // 按槽位分组 draw(槽 → bind group 一一对应,与主仓 ptr::eq
                // 分组同语义)。
                let mut start = 0;
                for i in 1..=slots.len() {
                    if i == slots.len() || slots[i] != slots[start] {
                        pass.set_bind_group(0, self.bind(slots[start]), &[]);
                        pass.draw(0..4, start as u32..i as u32);
                        start = i;
                    }
                }
            }
        }
        self.queue.submit([encoder.finish()]);
        // wgpu 30:present 移到 Queue,提交后调用。
        self.queue.present(surface_texture);
        Ok(())
    }

    /// 槽位 → bind group;缺失槽回退槽 0(白)。
    fn bind(&self, slot: usize) -> &wgpu::BindGroup {
        &self
            .slots
            .get(slot)
            .and_then(|o| o.as_ref())
            .unwrap_or_else(|| self.slots[0].as_ref().expect("slot 0 = white always present"))
            .bind_group
    }
}

/// 组装一个实例:列主序 Mat3 → Instance.model([a,b,c,d,tx,ty,0,0])。
fn push(draw: &mut Vec<(Instance, usize)>, model: Mat3, color: [f32; 4], uv_rect: [f32; 4], slot: usize) {
    draw.push((
        Instance {
            model: [model[0][0], model[0][1], model[1][0], model[1][1], model[2][0], model[2][1], 0., 0.],
            color,
            uv_rect,
        },
        slot,
    ));
}

/// 一条线的全部 quad:线本体 + 音符(按 kind 优先级,hold 先画)。
fn build_line(
    draw: &mut Vec<(Instance, usize)>,
    line: &LineSnap,
    letterbox: &Mat3,
    kx: f32,
    ev_x: f32,
    ev_y: f32,
    slots: &[Option<TexEntry>],
) {
    let ctrl_px = line.pos[0] * CANVAS_W * ev_x;
    let ctrl_py = line.pos[1] * CANVAS_H * ev_y;
    // T * R * S:平移到位置,绕自身旋转,缩放(与主仓 line_m 同构)。
    let line_m = mat_mul(
        letterbox,
        &mat_mul(
            &mat_translate(ctrl_px, ctrl_py),
            &mat_mul(&mat_rotate(line.rot), &mat_scale(line.scale[0], line.scale[1])),
        ),
    );
    let alpha = line.alpha;
    // 线 quad:槽 6(默认线纹理)/ 7+(自定义)存在 → 像素→clip 映射;
    // 缺失 → 白条降级。
    match slots.get(line.tex as usize).and_then(|o| o.as_ref()) {
        Some(t) => {
            // raw_scale:无 scale 事件时线 scale = 1.0(identity),隐式因子
            // 2/RPE_WIDTH 仍要生效 → 回退 2.0(主仓同逻辑)。
            let raw_sx = if (line.scale[0] - 1.0).abs() < 1e-6 { 2.0 } else { line.scale[0] * 675.0 };
            let raw_sy = if (line.scale[1] - 1.0).abs() < 1e-6 { 2.0 } else { line.scale[1] * 675.0 };
            let tw = t.size[0] * raw_sx / kx;
            let th = t.size[1] * raw_sy / kx;
            let tex_m = mat_mul(
                letterbox,
                &mat_mul(
                    &mat_translate(ctrl_px, ctrl_py),
                    &mat_mul(&mat_rotate(line.rot), &mat_scale(tw, th)),
                ),
            );
            push(draw, tex_m, [1., 1., 1., alpha], [0., 0., 1., 1.], line.tex as usize);
        }
        None => {
            push(draw, mat_mul(&line_m, &mat_scale(LINE_LEN, LINE_THICK)), [1., 1., 1., alpha], [0., 0., 1., 1.], 0);
        }
    }
    // 音符:跟随线的平移+旋转,但不随线的缩放/alpha(主仓锁定决策)。
    let note_m = mat_mul(letterbox, &mat_mul(&mat_translate(ctrl_px, ctrl_py), &mat_rotate(line.rot)));
    // 绘制顺序:kind 优先级 hold → drag → tap → flick(与主仓一致)。
    for kind in [2u8, 4, 1, 3] {
        for note in line.notes.iter().filter(|n| n.kind == kind) {
            build_note(draw, note, &note_m, ev_x, ev_y, slots);
        }
    }
}

/// 一个音符的 quad(s):纹理 sprite / hold 三段 / 纯色降级。
fn build_note(
    draw: &mut Vec<(Instance, usize)>,
    note: &NoteSnap,
    note_m: &Mat3,
    ev_x: f32,
    ev_y: f32,
    slots: &[Option<TexEntry>],
) {
    if note.alpha <= 0.0 {
        return;
    }
    let rgb = match note.kind {
        1 => TAP_COLOR,
        2 => HOLD_COLOR,
        3 => FLICK_COLOR,
        4 => DRAG_COLOR,
        _ => return, // 未知 kind 不画
    };
    let x = note.x * CANVAS_W * ev_x;
    let y = note.y * CANVAS_H * ev_y;
    let note_base = mat_mul(note_m, &mat_translate(x, y));
    // kind → 纹理槽(协议映射:1 tap→1, 2 hold→4, 3 flick→3, 4 drag→2)。
    let slot = match note.kind {
        1 => 1usize,
        2 => 4,
        3 => 3,
        4 => 2,
        _ => return,
    };
    let sprite = slots.get(slot).and_then(|o| o.as_ref());
    match (note.kind, sprite) {
        (1 | 3 | 4, Some(t)) => {
            let w = NOTE_SPRITE_W * note.scale;
            let h = w * t.size[1] / t.size[0];
            push(draw, mat_mul(&note_base, &mat_scale(w, h)), [1., 1., 1., note.alpha], [0., 0., 1., 1.], slot);
        }
        // 纹理 hold:图集 = 上 head + 可拉伸 body + 下 tail(上传时已翻转,
        // head 在 v=0)。三段:body 一个 quad 从 head 边缘延伸到 tail 边缘。
        (2, Some(t)) => {
            let w = NOTE_SPRITE_W * note.scale;
            let head_uv = HOLD_CAP_PX / t.size[1];
            let tail_uv = HOLD_CAP_PX / t.size[1];
            let head_h = w * HOLD_CAP_PX / t.size[0];
            let tail_h = w * HOLD_CAP_PX / t.size[0];
            let tint = [1., 1., 1., note.alpha];
            let hd = |cy: f32| mat_mul(note_m, &mat_mul(&mat_translate(x, cy), &mat_scale(w, w)));
            if note.end_y.is_finite() {
                let y1 = note.end_y * CANVAS_H * ev_y;
                let (head_y, tail_y) = (y, y1);
                // 头尾偏移:body 从 head 边缘延伸到 tail 边缘,避免中段与
                // 头尾中心重叠(主仓 [FIX] 同语义)。
                let (h0, h1) = if tail_y >= head_y {
                    (head_y + head_h * 0.5, tail_y - tail_h * 0.5)
                } else {
                    (head_y - head_h * 0.5, tail_y + tail_h * 0.5)
                };
                let bh = (h1 - h0).abs();
                if bh > 1e-5 {
                    push(
                        draw,
                        mat_mul(&hd((h0 + h1) * 0.5), &mat_scale(1.0, bh / w)),
                        tint,
                        [0., head_uv, 1., 1. - head_uv - tail_uv],
                        slot,
                    );
                }
                push(draw, mat_mul(&hd(head_y), &mat_scale(1.0, head_h / w)), tint, [0., 0., 1., head_uv], slot);
                push(draw, mat_mul(&hd(tail_y), &mat_scale(1.0, tail_h / w)), tint, [0., 1. - tail_uv, 1., tail_uv], slot);
            } else {
                // 无 end_y(未判定 hold 或已收起):只画 head。
                push(draw, mat_mul(&hd(y), &mat_scale(1.0, head_h / w)), tint, [0., 0., 1., head_uv], slot);
            }
        }
        // 无纹理槽:纯色 quad 降级(主仓同款回退)。
        _ => {
            let nb = mat_mul(note_m, &mat_translate(x, y));
            if note.kind == 2 {
                if note.end_y.is_finite() {
                    let y1 = note.end_y * CANVAS_H * ev_y;
                    let h = (y1 - y).abs();
                    let wb = HOLD_BODY_W * note.scale;
                    if h > 1e-5 {
                        push(draw, mat_mul(&nb, &mat_scale(wb, h)), [rgb[0], rgb[1], rgb[2], note.alpha], [0., 0., 1., 1.], 0);
                    }
                }
            }
            push(draw, mat_mul(&nb, &mat_scale(NOTE_W * note.scale, NOTE_H * note.scale)), [rgb[0], rgb[1], rgb[2], note.alpha], [0., 0., 1., 1.], 0);
        }
    }
}

/// 管线:quad 着色器 + 纹理 bind-group + alpha 混合(照搬主仓
/// create_pipeline,只留混合版;背景压暗走 clear + CPU dim,不需要不透明管线)。
fn create_pipeline(device: &wgpu::Device, format: wgpu::TextureFormat) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout, wgpu::Sampler) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("phimakor-player-quad"),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(SHADER)),
    });
    let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&tex_bgl)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("phimakor-player-quad"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[Some(INSTANCE_LAYOUT)],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: None,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    (pipeline, tex_bgl, sampler)
}

/// RGBA8 纹理 + 上传(照搬主仓 create_texture;格式 Rgba8Unorm 非 sRGB,
/// 作者色为显示就绪 sRGB,直通)。
fn create_texture(device: &wgpu::Device, queue: &wgpu::Queue, rgba: &[u8], width: u32, height: u32) -> wgpu::Texture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * width),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    texture
}

fn texture_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    texture: &wgpu::Texture,
) -> wgpu::BindGroup {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    })
}

/// 逐行垂直翻转(上传前调用,让 v=0 = 图片顶部)。
fn flip_vertical(buf: &mut [u8], w: u32, h: u32) {
    let row = w as usize * 4;
    for y in 0..(h as usize / 2) {
        let top = y * row;
        let bot = (h as usize - 1 - y) * row;
        for x in 0..row {
            buf.swap(top + x, bot + x);
        }
    }
}
