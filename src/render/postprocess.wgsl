// ── Full-screen vertex shader (shared by all effects) ──
struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VOut {
    // Full-screen triangle covering [-1,1] in clip space
    let verts = array(
        vec2f(-1.0, -1.0),
        vec2f( 3.0, -1.0),
        vec2f(-1.0,  3.0),
    );
    let uv_verts = array(
        vec2f(0.0, 0.0),
        vec2f(2.0, 0.0),
        vec2f(0.0, 2.0),
    );
    var out: VOut;
    out.pos = vec4f(verts[vi], 0.0, 1.0);
    out.uv = uv_verts[vi];
    return out;
}

// ── Grayscale ──
@group(0) @binding(0) var screen_tex: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
@group(1) @binding(0) var<uniform> u_factor: f32;

@fragment
fn grayscale_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    let color = textureSample(screen_tex, screen_sampler, uv).rgb;
    let lum = vec3f(0.299, 0.587, 0.114);
    let gray = vec3f(dot(lum, color));
    return vec4f(mix(color, gray, u_factor), 1.0);
}

// ── Chromatic Aberration ──
@group(1) @binding(1) var<uniform> u_power: f32;
@group(1) @binding(2) var<uniform> u_sample_count: f32;
@group(1) @binding(3) var<uniform> u_screen_size: vec2f;

fn chromatic_slice(t: f32) -> vec3f {
    return max(vec3f(1.0 - t, 1.0 - abs(t - 1.0), t - 1.0), vec3f(0.0));
}

@fragment
fn chromatic_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    let offset = (uv - vec2f(0.5)) * vec2f(1.0, -1.0);
    let n = i32(u_sample_count);
    var sum: vec3f = vec3f(0.0);
    var c: vec3f = vec3f(0.0);
    for (var i: i32 = 0; i < 64; i = i + 1) {
        if (i >= n) { break; }
        let t = 2.0 * f32(i) / f32(max(n - 1, 1));
        let slice = chromatic_slice(t);
        sum = sum + slice;
        let slice_offset = (t - 1.0) * u_power * offset;
        c = c + slice * textureSample(screen_tex, screen_sampler, uv + slice_offset).rgb;
    }
    return vec4f(c / max(sum, vec3f(1e-6)), 1.0);
}

// ── Glitch ──
@group(1) @binding(4) var<uniform> u_time: f32;
@group(1) @binding(5) var<uniform> u_rate: f32;
@group(1) @binding(6) var<uniform> u_speed: f32;
@group(1) @binding(7) var<uniform> u_block_count: f32;
@group(1) @binding(8) var<uniform> u_color_rate: f32;

fn rand(seed: f32) -> f32 {
    return fract(543.2543 * sin(dot(vec2f(seed, seed), vec2f(3525.46, -54.3415))));
}

@fragment
fn glitch_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    let enable_shift = f32(rand(floor(u_time * u_speed)) < u_rate);
    var fixed_uv = uv;
    fixed_uv.x = fixed_uv.x + (rand(floor(uv.y * u_block_count) / u_block_count + u_time) - 0.5) * u_power * enable_shift;
    var pixel = textureSample(screen_tex, screen_sampler, fixed_uv);
    let r = mix(pixel.r, textureSample(screen_tex, screen_sampler, fixed_uv + vec2f(u_color_rate, 0.0)).r, enable_shift);
    let b = mix(pixel.b, textureSample(screen_tex, screen_sampler, fixed_uv + vec2f(-u_color_rate, 0.0)).b, enable_shift);
    return vec4f(r, pixel.g, b, 1.0);
}

// ── Vignette ──
@group(1) @binding(9) var<uniform> u_color: vec4f;
@group(1) @binding(10) var<uniform> u_extend: f32;
@group(1) @binding(11) var<uniform> u_radius: f32;

@fragment
fn vignette_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    let new_uv = uv * (1.0 - uv.yx);
    var vig = new_uv.x * new_uv.y * u_radius;
    vig = pow(vig, u_extend);
    return mix(u_color, textureSample(screen_tex, screen_sampler, uv), clamp(vig, 0.0, 1.0));
}

// ── Fisheye ──
@group(1) @binding(12) var<uniform> u_aspect: f32;

@fragment
fn fisheye_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    let p = vec2f(uv.x, uv.y * u_aspect);
    let m = vec2f(0.5, 0.5 / u_aspect);
    let d = p - m;
    let r = sqrt(dot(d, d));
    let pi = 3.14159265;
    let new_power = (2.0 * pi / (2.0 * sqrt(dot(m, m)))) * u_power;
    let bind = select(sqrt(dot(m, m)), select(m.x, m.y, u_aspect < 1.0), new_power > 0.0);
    var nuv: vec2f;
    if (new_power > 0.0) {
        nuv = m + normalize(d) * tan(r * new_power) * bind / tan(bind * new_power);
    } else {
        nuv = m + normalize(d) * atan(r * -new_power * 10.0) * bind / atan(-new_power * bind * 10.0);
    }
    return textureSample(screen_tex, screen_sampler, nuv);
}

// ── Shockwave ──
@group(1) @binding(13) var<uniform> u_center: vec2f;
@group(1) @binding(14) var<uniform> u_width: f32;
@group(1) @binding(15) var<uniform> u_distortion: f32;
@group(1) @binding(16) var<uniform> u_expand: f32;

@fragment
fn shockwave_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    let aspect = u_screen_size.y / u_screen_size.x;
    let center = vec2f(u_center.x, (u_center.y - 0.5) * aspect + 0.5);
    var tex = uv;
    tex.y = (tex.y - 0.5) * aspect + 0.5;
    let dist = distance(tex, center);
    if (u_progress - u_width <= dist && dist <= u_progress + u_width) {
        let diff = dist - u_progress;
        let scale_diff = 1.0 - pow(abs(diff * u_expand), u_distortion);
        let dt = diff * scale_diff;
        let dir = normalize(tex - center);
        tex = tex + ((dir * dt) / (u_progress * dist * 40.0));
        var col = textureSample(screen_tex, screen_sampler, vec2f(tex.x, (tex.y - 0.5) / aspect + 0.5));
        col = col + (col * scale_diff) / (u_progress * dist * 40.0);
        return col;
    } else {
        return textureSample(screen_tex, screen_sampler, vec2f(tex.x, (tex.y - 0.5) / aspect + 0.5));
    }
}

// ── Circle Blur ──
@group(1) @binding(17) var<uniform> u_blur_size: f32;

@fragment
fn circle_blur_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    var c = textureSample(screen_tex, screen_sampler, uv);
    let pixel_size = 1.0 / u_screen_size;
    let size = u_blur_size;
    for (var x: f32 = -size; x < size; x = x + 1.0) {
        for (var y: f32 = -size; y < size; y = y + 1.0) {
            if (x * x + y * y > size * size) { continue; }
            let new_c = textureSample(screen_tex, screen_sampler, uv + pixel_size * vec2f(x, y));
            if (dot(new_c.rgb, new_c.rgb) > dot(c.rgb, c.rgb)) {
                c = new_c;
            }
        }
    }
    return c;
}

// ── Radial Blur ──
@fragment
fn radial_blur_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    let direction = uv - u_center;
    var c: vec3f = vec3f(0.0);
    let f = 1.0 / u_sample_count;
    for (var i: f32 = 0.0; i < 64.0; i = i + 1.0) {
        if (i >= u_sample_count) { break; }
        c = c + textureSample(screen_tex, screen_sampler, uv - u_power * direction * i).rgb * f;
    }
    return vec4f(c, 1.0);
}

// ── Pixelate ──
@fragment
fn pixel_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    let factor = u_screen_size / u_blur_size;
    let x = floor(uv.x * factor.x + 0.5) / factor.x;
    let y = floor(uv.y * factor.y + 0.5) / factor.y;
    return textureSample(screen_tex, screen_sampler, vec2f(x, y));
}

// ── Noise ──
@group(1) @binding(18) var<uniform> u_noise_seed: f32;
@group(1) @binding(19) var<uniform> u_noise_power: f32;

fn rand2(pos: vec2f) -> vec2f {
    return fract(sin(vec2f(dot(pos, vec2f(12.9898, 78.233)), dot(pos, vec2f(-148.998, -65.233)))) * 43758.5453);
}

@fragment
fn noise_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    let new_uv = uv + (rand2(uv + vec2f(u_noise_seed, 0.0)) - vec2f(0.5, 0.5)) * u_noise_power;
    return textureSample(screen_tex, screen_sampler, new_uv);
}

// ── FXAA (simplified pass-through placeholder) ──
@fragment
fn fxaa_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    return textureSample(screen_tex, screen_sampler, uv);
}

// ── Rainbow ──
@group(1) @binding(20) var<uniform> u_rainbow_time: f32;

fn rgb2hsv(c: vec3f) -> vec3f {
    let K = vec4f(0.0, -1.0/3.0, 2.0/3.0, -1.0);
    let p = mix(vec4f(c.bg, K.wz), vec4f(c.gb, K.xy), step(c.b, c.g));
    let q = mix(vec4f(p.xyw, c.r), vec4f(c.r, p.yzx), step(p.x, c.r));
    let d = q.x - min(q.w, q.y);
    let e = 1.0e-10;
    return vec3f(abs(q.z + (q.w - q.y) / (6.0 * d + e)), d / (q.x + e), q.x);
}

fn hsv2rgb(c: vec3f) -> vec3f {
    let p = abs(fract(c.xxx + vec3f(0.0, 2.0/3.0, 1.0/3.0)) * 6.0 - 3.0);
    return c.z * mix(vec3f(1.0), clamp(p - 1.0, vec3f(0.0), vec3f(1.0)), c.y);
}

@fragment
fn rainbow_fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    let color = textureSample(screen_tex, screen_sampler, uv);
    let hsv = rgb2hsv(color.rgb);
    return vec4f(hsv2rgb(vec3f(fract(hsv.x + u_rainbow_time * 0.5), hsv.y, hsv.z)), color.a);
}
