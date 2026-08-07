//! Post-processing shaders (WGSL), one per effect + shared vertex.
//! Each effect is compiled as a separate pipeline with its own uniforms.

pub const VERT: &str = r"
struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
};
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VOut {
    let verts = array(
        vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0),
    );
    let uv_verts = array(
        vec2f(0.0, 0.0), vec2f(2.0, 0.0), vec2f(0.0, 2.0),
    );
    var out: VOut;
    out.pos = vec4f(verts[vi], 0.0, 1.0);
    // HACK: Y-flip UV because clip(-1,-1) maps to texture bottom, not top.
    // Without this, each effect pass flips the image vertically.
    out.uv = vec2f(uv_verts[vi].x, 1.0 - uv_verts[vi].y);
    return out;
}
";

/// GLSL 450(desktop)共享 vertex(自定义 GLSL 特效用,与 [`VERT`] 语义
/// 等价):全屏三角形 + uv。naga glsl-in 只接受 440+ 版本,用户 fragment
/// 由 `es100_to_glsl` 预转换后与本站点语法一致。
pub const GLSL_VERT: &str = r"
#version 450
layout(location = 0) out vec2 v_uv;
void main() {
    vec2 verts[3];
    verts[0] = vec2(-1.0, -1.0);
    verts[1] = vec2(3.0, -1.0);
    verts[2] = vec2(-1.0, 3.0);
    vec2 uv_verts[3];
    uv_verts[0] = vec2(0.0, 0.0);
    uv_verts[1] = vec2(2.0, 0.0);
    uv_verts[2] = vec2(0.0, 2.0);
    gl_Position = vec4(verts[gl_VertexIndex], 0.0, 1.0);
    v_uv = vec2(uv_verts[gl_VertexIndex].x, 1.0 - uv_verts[gl_VertexIndex].y);
}
";

pub const GRAYSCALE_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
@group(1) @binding(0) var<uniform> factor: f32;
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    let color = textureSample(screen_tex, screen_sampler, uv).rgb;
    let lum = vec3f(0.299, 0.587, 0.114);
    let gray = vec3f(dot(lum, color));
    return vec4f(mix(color, gray, factor), 1.0);
}
";

pub const CHROMATIC_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
struct Params { power: f32, sample_count: f32, screen_size: vec2f, }
@group(1) @binding(0) var<uniform> p: Params;
fn slice(t: f32) -> vec3f { return max(vec3f(1.0 - t, 1.0 - abs(t - 1.0), t - 1.0), vec3f(0.0)); }
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    let offset = (uv - vec2f(0.5)) * vec2f(1.0, -1.0);
    let n = i32(p.sample_count);
    var sum = vec3f(0.0); var c = vec3f(0.0);
    for (var i = 0i; i < 64; i = i + 1) { if (i >= n) { break; }
        let t = 2.0 * f32(i) / f32(max(n - 1, 1));
        let s = slice(t); sum = sum + s;
        c = c + s * textureSample(screen_tex, screen_sampler, uv + (t - 1.0) * p.power * offset).rgb;
    }
    return vec4f(c / max(sum, vec3f(1e-6)), 1.0);
}
";

pub const GLITCH_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
struct Params { power: f32, time: f32, rate: f32, speed: f32, block_count: f32, color_rate: f32, }
@group(1) @binding(0) var<uniform> p: Params;
fn rand(seed: f32) -> f32 { return fract(543.2543 * sin(dot(vec2f(seed, seed), vec2f(3525.46, -54.3415)))); }
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    let en = f32(rand(floor(p.time * p.speed)) < p.rate);
    var huv = uv; huv.x = huv.x + (rand(floor(uv.y * p.block_count) / p.block_count + p.time) - 0.5) * p.power * en;
    var pix = textureSample(screen_tex, screen_sampler, huv);
    let r = mix(pix.r, textureSample(screen_tex, screen_sampler, huv + vec2f(p.color_rate, 0.0)).r, en);
    let b = mix(pix.b, textureSample(screen_tex, screen_sampler, huv + vec2f(-p.color_rate, 0.0)).b, en);
    return vec4f(r, pix.g, b, 1.0);
}
";

pub const VIGNETTE_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
struct Params { color: vec4f, extend: f32, radius: f32, }
@group(1) @binding(0) var<uniform> p: Params;
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    let nuv = uv * (1.0 - uv.yx);
    var vig = nuv.x * nuv.y * p.radius;
    vig = pow(vig, p.extend);
    return mix(p.color, textureSample(screen_tex, screen_sampler, uv), clamp(vig, 0.0, 1.0));
}
";

pub const FISHEYE_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
struct Params { power: f32, aspect: f32, }
@group(1) @binding(0) var<uniform> p: Params;
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    let uv2 = vec2f(uv.x, uv.y * p.aspect);
    let m = vec2f(0.5, 0.5 / p.aspect);
    let d = uv2 - m; let r = sqrt(dot(d, d));
    let pi = 3.14159265;
    let np = (2.0 * pi / (2.0 * sqrt(dot(m, m)))) * p.power;
    let bind = select(sqrt(dot(m, m)), select(m.x, m.y, p.aspect < 1.0), np > 0.0);
    var scale: f32;
    if (np > 0.0) { scale = tan(r * np) * bind / tan(bind * np); }
    else { scale = atan(r * -np * 10.0) * bind / atan(-np * bind * 10.0); }
    let nuv = m + normalize(d) * scale;
    return textureSample(screen_tex, screen_sampler, nuv);
}
";

pub const SHOCKWAVE_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
struct Params { progress: f32, center: vec2f, width: f32, distortion: f32, expand: f32, screen_size: vec2f, }
@group(1) @binding(0) var<uniform> p: Params;
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    let asp = p.screen_size.y / p.screen_size.x;
    let ctr = vec2f(p.center.x, (p.center.y - 0.5) * asp + 0.5);
    var tex = uv; tex.y = (tex.y - 0.5) * asp + 0.5;
    let dist = distance(tex, ctr);
    if (p.progress - p.width <= dist && dist <= p.progress + p.width) {
        let diff = dist - p.progress;
        let sd = 1.0 - pow(abs(diff * p.expand), p.distortion);
        let dt = diff * sd; let dir = normalize(tex - ctr);
        tex = tex + dir * dt / (p.progress * dist * 40.0);
        var col = textureSample(screen_tex, screen_sampler, vec2f(tex.x, (tex.y - 0.5) / asp + 0.5));
        return col + col * sd / (p.progress * dist * 40.0);
    }
    return textureSample(screen_tex, screen_sampler, vec2f(tex.x, (tex.y - 0.5) / asp + 0.5));
}
";

/// 一维水平 max 模糊:取 `[-size, size]` 范围内最亮像素(rgb 亮度最大)。
/// 与 `CIRCLE_BLUR_V_FRAG` 组成分离式方形 max 模糊(circleBlur 复合特效)。
/// 方形区域 max 精确可分离:水平 pass + 垂直 pass = 方形邻域 max,
/// 采样数 2×size vs 原暴力 size²×π/4(16 倍提速)。
pub const CIRCLE_BLUR_H_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
struct Params { size: f32, screen_size: vec2f, }
@group(1) @binding(0) var<uniform> p: Params;
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    var c = textureSample(screen_tex, screen_sampler, uv);
    let ps = 1.0 / p.screen_size;
    for (var x = -p.size; x < p.size; x = x + 1.0) {
        let nc = textureSample(screen_tex, screen_sampler, uv + ps * vec2f(x, 0.0));
        if (dot(nc.rgb, nc.rgb) > dot(c.rgb, c.rgb)) { c = nc; }
    }
    return c;
}
";

/// 一维垂直 max 模糊(`CIRCLE_BLUR_H_FRAG` 的垂直版,组成 circleBlur)。
pub const CIRCLE_BLUR_V_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
struct Params { size: f32, screen_size: vec2f, }
@group(1) @binding(0) var<uniform> p: Params;
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    var c = textureSample(screen_tex, screen_sampler, uv);
    let ps = 1.0 / p.screen_size;
    for (var y = -p.size; y < p.size; y = y + 1.0) {
        let nc = textureSample(screen_tex, screen_sampler, uv + ps * vec2f(0.0, y));
        if (dot(nc.rgb, nc.rgb) > dot(c.rgb, c.rgb)) { c = nc; }
    }
    return c;
}
";

pub const RADIAL_BLUR_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
struct Params { power: f32, sample_count: f32, center: vec2f, }
@group(1) @binding(0) var<uniform> p: Params;
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    let dir = uv - p.center; var c = vec3f(0.0);
    let f = 1.0 / p.sample_count;
    for (var i = 0.0; i < 64.0; i = i + 1.0) { if (i >= p.sample_count) { break; }
        c = c + textureSample(screen_tex, screen_sampler, uv - p.power * dir * i).rgb * f;
    }
    return vec4f(c, 1.0);
}
";

pub const PIXELATE_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
struct Params { size: f32, screen_size: vec2f, }
@group(1) @binding(0) var<uniform> p: Params;
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    let factor = p.screen_size / p.size;
    let x = floor(uv.x * factor.x + 0.5) / factor.x;
    let y = floor(uv.y * factor.y + 0.5) / factor.y;
    return textureSample(screen_tex, screen_sampler, vec2f(x, y));
}
";

pub const NOISE_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
struct Params { seed: f32, power: f32, }
@group(1) @binding(0) var<uniform> p: Params;
fn rand2(pos: vec2f) -> vec2f { return fract(sin(vec2f(dot(pos, vec2f(12.9898,78.233)), dot(pos, vec2f(-148.998,-65.233)))) * 43758.5453); }
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    let nuv = uv + (rand2(uv + vec2f(p.seed, 0.0)) - vec2f(0.5, 0.5)) * p.power;
    return textureSample(screen_tex, screen_sampler, nuv);
}
";

pub const RAINBOW_FRAG: &str = r"
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
@group(1) @binding(0) var<uniform> time: f32;
fn rgb2hsv(c: vec3f) -> vec3f {
    let K = vec4f(0.0, -1.0/3.0, 2.0/3.0, -1.0);
    let p = mix(vec4f(c.bg, K.wz), vec4f(c.gb, K.xy), step(c.b, c.g));
    let q = mix(vec4f(p.xyw, c.r), vec4f(c.r, p.yzx), step(p.x, c.r));
    let d = q.x - min(q.w, q.y); let e = 1e-10;
    return vec3f(abs(q.z + (q.w - q.y) / (6.0 * d + e)), d / (q.x + e), q.x);
}
fn hsv2rgb(c: vec3f) -> vec3f {
    let p = abs(fract(c.xxx + vec3f(0.0, 2.0/3.0, 1.0/3.0)) * 6.0 - 3.0);
    return c.z * mix(vec3f(1.0), clamp(p - 1.0, vec3f(0.0), vec3f(1.0)), c.y);
}
@fragment
fn fs_main(@location(0) uv: vec2f) -> @location(0) vec4f {
    let color = textureSample(screen_tex, screen_sampler, uv);
    let hsv = rgb2hsv(color.rgb);
    return vec4f(hsv2rgb(vec3f(fract(hsv.x + time * 0.5), hsv.y, hsv.z)), color.a);
}
";

/// All registered effects with their shader source and default uniform values.
pub struct EffectDef {
    pub name: &'static str,
    pub frag: &'static str,
    /// 复合特效的多阶段 shader 名(空 = 单 pass)。非空时 `frag` 是占位,
    /// 实际按 stages 逐个编译执行(如 circleBlur = [circleBlurH, circleBlurV]
    /// 分离式 max 模糊)。uniform 由各阶段自己的 defaults 解析。
    pub stages: &'static [&'static str],
    /// Default uniform values: (label, WGSL 类型, default_f32)。类型用于
    /// 按 WGSL uniform 地址空间(std140 兼容)规则自动计算字节偏移,
    /// 替代旧的手工 `_pad` 占位成员 + `i*4` 顺序写入。
    pub defaults: &'static [(&'static str, &'static str, f32)],
}

/// WGSL uniform 地址空间(std140 兼容)布局:按每个默认值声明的 WGSL 类型
/// 计算字节偏移。每个默认值占 4B(f32 分量);成员对齐 f32=4、vec2f=8、
/// vec4f=16,复合成员(拆成 `_x/_y` 或 `_r/_g/_b/_a` 分量的 vec2/vec4)的
/// 对齐只在其起始分量生效一次,后续分量依次 +4。
///
/// 写入 `out[i]` = 第 i 个默认值的字节偏移,返回写入条数(≤ defaults.len())。
pub fn wgsl_uniform_offsets(defaults: &[(&str, &str, f32)], out: &mut [u32]) -> usize {
    let mut off = 0u32;
    let mut i = 0usize;
    while i < defaults.len() {
        let (align, comps) = match defaults[i].1 {
            "vec2f" => (8u32, 2usize),
            "vec4f" => (16u32, 4usize),
            _ => (4u32, 1usize),
        };
        let comps = comps.min(defaults.len() - i);
        off = (off + align - 1) & !(align - 1);
        for k in 0..comps {
            if i + k < out.len() {
                out[i + k] = off + (k as u32) * 4;
            }
        }
        off += (comps as u32) * 4;
        i += comps;
    }
    defaults.len().min(out.len())
}

pub const EFFECTS: &[EffectDef] = &[
    EffectDef { stages: &[], name: "grayscale",  frag: GRAYSCALE_FRAG,   defaults: &[("factor", "f32", 1.0)] },
    // chromatic: power(f32) + sample_count(f32) + screen_size(vec2f=2xf32)
    EffectDef { stages: &[], name: "chromatic",  frag: CHROMATIC_FRAG,   defaults: &[("power", "f32", 0.01), ("sample_count", "f32", 3.0), ("screen_size_x", "vec2f", 0.0), ("screen_size_y", "vec2f", 0.0)] },
    // glitch: power, time, rate, speed, block_count, color_rate = 6xf32
    EffectDef { stages: &[], name: "glitch",     frag: GLITCH_FRAG,      defaults: &[("power", "f32", 0.03), ("time", "f32", 0.0), ("rate", "f32", 0.6), ("speed", "f32", 5.0), ("block_count", "f32", 30.5), ("color_rate", "f32", 0.01)] },
    // vignette: color(vec4f) + extend(f32) + radius(f32)
    EffectDef { stages: &[], name: "vignette",   frag: VIGNETTE_FRAG,    defaults: &[("color_r", "vec4f", 0.0), ("color_g", "vec4f", 0.0), ("color_b", "vec4f", 0.0), ("color_a", "vec4f", 1.0), ("extend", "f32", 0.25), ("radius", "f32", 15.0)] },
    // fisheye: power, aspect = 2xf32
    EffectDef { stages: &[], name: "fisheye",    frag: FISHEYE_FRAG,     defaults: &[("power", "f32", -0.1), ("aspect", "f32", 1.0)] },
    // shockwave: progress(f32) + center(vec2f) + width + distortion + expand
    //            (3xf32) + screen_size(vec2f) — offset 由类型自动计算
    EffectDef { stages: &[], name: "shockwave",  frag: SHOCKWAVE_FRAG,   defaults: &[("progress", "f32", 0.2), ("center_x", "vec2f", 0.5), ("center_y", "vec2f", 0.5), ("width", "f32", 0.1), ("distortion", "f32", 0.8), ("expand", "f32", 10.0), ("screen_size_x", "vec2f", 0.0), ("screen_size_y", "vec2f", 0.0)] },
    // circleBlur: 复合特效 = 分离式方形 max 模糊(水平+垂直两 pass,
    // 采样 2×size vs 原暴力 size²×π/4,16 倍提速)。frag 占位用 H。
    EffectDef { stages: &["circleBlurH", "circleBlurV"], name: "circleBlur", frag: CIRCLE_BLUR_H_FRAG, defaults: &[("size", "f32", 10.0), ("screen_size_x", "vec2f", 0.0), ("screen_size_y", "vec2f", 0.0)] },
    // circleBlurH: 水平 1D max(方形 max 的水平 pass)。
    EffectDef { stages: &[], name: "circleBlurH", frag: CIRCLE_BLUR_H_FRAG, defaults: &[("size", "f32", 10.0), ("screen_size_x", "vec2f", 0.0), ("screen_size_y", "vec2f", 0.0)] },
    // circleBlurV: 垂直 1D max(方形 max 的垂直 pass)。
    EffectDef { stages: &[], name: "circleBlurV", frag: CIRCLE_BLUR_V_FRAG, defaults: &[("size", "f32", 10.0), ("screen_size_x", "vec2f", 0.0), ("screen_size_y", "vec2f", 0.0)] },
    // radialBlur: power, sample_count, center(vec2f=2)
    EffectDef { stages: &[], name: "radialBlur", frag: RADIAL_BLUR_FRAG, defaults: &[("power", "f32", 0.01), ("sample_count", "f32", 6.0), ("center_x", "vec2f", 0.5), ("center_y", "vec2f", 0.5)] },
    // pixel: size(f32) + screen_size(vec2f)
    EffectDef { stages: &[], name: "pixel",      frag: PIXELATE_FRAG,    defaults: &[("size", "f32", 10.0), ("screen_size_x", "vec2f", 0.0), ("screen_size_y", "vec2f", 0.0)] },
    // noise: seed, power = 2xf32
    EffectDef { stages: &[], name: "noise",      frag: NOISE_FRAG,       defaults: &[("seed", "f32", 81.0), ("power", "f32", 0.03)] },
    // rainbow: time = 1xf32
    EffectDef { stages: &[], name: "rainbow",    frag: RAINBOW_FRAG,     defaults: &[("time", "f32", 0.0)] },
];
