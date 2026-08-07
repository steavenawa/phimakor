//! 快照与纹理帧解析(网络序:大端,手写打包,协议见 PROTOCOL.md)。
//!
//! 快照帧 [0x01]:chart_time f64 / dim f32 / 线列表(每条含音符列表)。
//! 纹理清单帧 [0x00]:name + PNG 字节(槽位由发送顺序隐式,从 1 起);
//! [0xFF]:清单结束。任何格式错误/长度不足都返回 None(严格长度检查)。

/// 快照帧标签。
const SNAP_TAG: u8 = 0x01;
/// 纹理条目帧标签。
const TEX_TAG: u8 = 0x00;
/// 纹理清单结束标签。
const TEX_END_TAG: u8 = 0xFF;

/// 一帧渲染快照(全量,无增量)。
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// 谱面时间(秒,chart clock)。
    pub chart_time: f64,
    /// 背景压暗 0..1(渲染端乘到线/音符 rgb)。
    pub dim: f32,
    pub lines: Vec<LineSnap>,
}

/// 一条判定线(事件值,渲染端乘 CANVAS_W/H 转画布坐标)。
#[derive(Debug, Clone)]
pub struct LineSnap {
    /// 线位置。
    pub pos: [f32; 2],
    /// 线旋转(弧度)。
    pub rot: f32,
    pub scale: [f32; 2],
    pub alpha: f32,
    /// 绘制层级(小 z 在底,大 z 在上)。
    pub z: i32,
    /// 线纹理槽(默认 6 = line.png,7+ 谱面自定义)。
    pub tex: u8,
    pub notes: Vec<NoteSnap>,
}

/// 一个音符(线相对坐标)。
#[derive(Debug, Clone)]
pub struct NoteSnap {
    /// 1 tap / 2 hold / 3 flick / 4 drag。
    pub kind: u8,
    /// 线相对 x。
    pub x: f32,
    /// 线相对 y(已含 above 镜像符号,渲染端直接用)。
    pub y: f32,
    /// hold 尾(线相对 y);非 hold 为 NaN。
    pub end_y: f32,
    pub alpha: f32,
    pub scale: f32,
    /// 纹理槽(kind 映射:1→1, 2→4, 3→3, 4→2)。
    pub tex: u8,
}

/// 大端游标读取器:越界即返回 None,由调用方整体失败。
struct R<'a> {
    b: &'a [u8],
    o: usize,
}

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.o + n > self.b.len() {
            return None;
        }
        let s = &self.b[self.o..self.o + n];
        self.o += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_be_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i32(&mut self) -> Option<i32> {
        self.take(4).map(|s| i32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn f32(&mut self) -> Option<f32> {
        self.take(4).map(|s| f32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn f64(&mut self) -> Option<f64> {
        self.take(8).map(|s| f64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
    }
}

/// 每条线的最小字节数(2×pos + rot + 2×scale + alpha + z + tex + note_count)。
const LINE_MIN: usize = 31;
/// 每个 note 的最小字节数(kind + 5×f32 + tex)。
const NOTE_MIN: usize = 22;

/// 解析快照帧。严格:标签不匹配、长度不足、尾随字节都返回 None。
/// 计数用 `buf.len()/最小字段长` 封顶,防恶意计数撑爆内存。
pub fn parse_snapshot(buf: &[u8]) -> Option<Snapshot> {
    let mut r = R { b: buf, o: 0 };
    if r.u8()? != SNAP_TAG {
        return None;
    }
    let chart_time = r.f64()?;
    let dim = r.f32()?;
    let line_count = (r.u16()? as usize).min(buf.len() / LINE_MIN + 1);
    let mut lines = Vec::with_capacity(line_count);
    for _ in 0..line_count {
        let pos = [r.f32()?, r.f32()?];
        let rot = r.f32()?;
        let scale = [r.f32()?, r.f32()?];
        let alpha = r.f32()?;
        let z = r.i32()?;
        let tex = r.u8()?;
        let note_count = (r.u16()? as usize).min(buf.len() / NOTE_MIN + 1);
        let mut notes = Vec::with_capacity(note_count);
        for _ in 0..note_count {
            notes.push(NoteSnap {
                kind: r.u8()?,
                x: r.f32()?,
                y: r.f32()?,
                end_y: r.f32()?,
                alpha: r.f32()?,
                scale: r.f32()?,
                tex: r.u8()?,
            });
        }
        lines.push(LineSnap { pos, rot, scale, alpha, z, tex, notes });
    }
    if r.o != buf.len() {
        return None; // 尾随字节
    }
    Some(Snapshot { chart_time, dim, lines })
}

/// 解析一条纹理清单帧。返回:
/// - `Some((0x00, name, png))`:纹理条目,槽位由发送顺序隐式(从 1 起累加)
/// - `Some((0xFF, "", []))`:清单结束
/// - `None`:格式错误/长度不足
pub fn parse_texture_frame(buf: &[u8]) -> Option<(u8, String, Vec<u8>)> {
    let mut r = R { b: buf, o: 0 };
    match r.u8()? {
        TEX_TAG => {
            let name_len = r.u16()? as usize;
            let name = std::str::from_utf8(r.take(name_len)?).ok()?.to_string();
            let len = r.u32()? as usize;
            let png = r.take(len)?.to_vec();
            if r.o != buf.len() {
                return None;
            }
            Some((TEX_TAG, name, png))
        }
        TEX_END_TAG => {
            if r.o != buf.len() {
                return None;
            }
            Some((TEX_END_TAG, String::new(), Vec::new()))
        }
        _ => None,
    }
}

// fx 帧 [0x02] 第一版跳过(渲染端不消费,占位注释):t0 f64 + count u16 +
// 每点 [x f32, y f32, rot f32, age f32]。解析待粒子系统接入时补充。

#[cfg(test)]
mod tests {
    use super::*;

    // ── 测试侧打包(与协议一致的手写大端),用于 roundtrip 验证 ──

    fn w_u16(v: u16, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn w_u32(v: u32, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn w_i32(v: i32, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn w_f32(v: f32, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn w_f64(v: f64, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    fn pack_snapshot(s: &Snapshot) -> Vec<u8> {
        let mut b = vec![SNAP_TAG];
        w_f64(s.chart_time, &mut b);
        w_f32(s.dim, &mut b);
        w_u16(s.lines.len() as u16, &mut b);
        for l in &s.lines {
            w_f32(l.pos[0], &mut b);
            w_f32(l.pos[1], &mut b);
            w_f32(l.rot, &mut b);
            w_f32(l.scale[0], &mut b);
            w_f32(l.scale[1], &mut b);
            w_f32(l.alpha, &mut b);
            w_i32(l.z, &mut b);
            b.push(l.tex);
            w_u16(l.notes.len() as u16, &mut b);
            for n in &l.notes {
                b.push(n.kind);
                w_f32(n.x, &mut b);
                w_f32(n.y, &mut b);
                w_f32(n.end_y, &mut b);
                w_f32(n.alpha, &mut b);
                w_f32(n.scale, &mut b);
                b.push(n.tex);
            }
        }
        b
    }

    /// f32 逐位比较(NaN 不能直接用 ==)。
    fn f32_eq(a: f32, b: f32) -> bool {
        a.to_bits() == b.to_bits()
    }

    fn assert_snap_eq(a: &Snapshot, b: &Snapshot) {
        assert_eq!(a.chart_time.to_bits(), b.chart_time.to_bits());
        assert!(f32_eq(a.dim, b.dim));
        assert_eq!(a.lines.len(), b.lines.len());
        for (la, lb) in a.lines.iter().zip(&b.lines) {
            assert!(f32_eq(la.pos[0], lb.pos[0]) && f32_eq(la.pos[1], lb.pos[1]));
            assert!(f32_eq(la.rot, lb.rot));
            assert!(f32_eq(la.scale[0], lb.scale[0]) && f32_eq(la.scale[1], lb.scale[1]));
            assert!(f32_eq(la.alpha, lb.alpha));
            assert_eq!(la.z, lb.z);
            assert_eq!(la.tex, lb.tex);
            assert_eq!(la.notes.len(), lb.notes.len());
            for (na, nb) in la.notes.iter().zip(&lb.notes) {
                assert_eq!(na.kind, nb.kind);
                assert!(f32_eq(na.x, nb.x) && f32_eq(na.y, nb.y) && f32_eq(na.end_y, nb.end_y));
                assert!(f32_eq(na.alpha, nb.alpha) && f32_eq(na.scale, nb.scale));
                assert_eq!(na.tex, nb.tex);
            }
        }
    }

    /// 覆盖 4 种 kind + hold end_y,线与 note 槽位按协议映射。
    fn sample() -> Snapshot {
        Snapshot {
            chart_time: 12.5,
            dim: 0.4,
            lines: vec![
                LineSnap {
                    pos: [-0.25, 0.1],
                    rot: 0.3,
                    scale: [1.2, 0.8],
                    alpha: 0.9,
                    z: 0,
                    tex: 6,
                    notes: vec![
                        NoteSnap { kind: 1, x: -0.3, y: -0.2, end_y: f32::NAN, alpha: 1.0, scale: 1.0, tex: 1 },
                        NoteSnap { kind: 2, x: 0.1, y: -0.5, end_y: 0.2, alpha: 0.8, scale: 1.0, tex: 4 },
                    ],
                },
                LineSnap {
                    pos: [0.0, 0.25],
                    rot: -0.1,
                    scale: [1.0, 1.0],
                    alpha: 1.0,
                    z: 1,
                    tex: 6,
                    notes: vec![
                        NoteSnap { kind: 3, x: 0.2, y: 0.1, end_y: f32::NAN, alpha: 1.0, scale: 1.1, tex: 3 },
                        NoteSnap { kind: 4, x: -0.1, y: 0.05, end_y: f32::NAN, alpha: 1.0, scale: 0.9, tex: 2 },
                    ],
                },
            ],
        }
    }

    #[test]
    fn snapshot_roundtrip() {
        let s = sample();
        let parsed = parse_snapshot(&pack_snapshot(&s)).expect("parse");
        assert_snap_eq(&s, &parsed);
    }

    #[test]
    fn snapshot_rejects_bad_tag() {
        let mut b = pack_snapshot(&sample());
        b[0] = 0x02; // fx 帧标签:快照解析应拒绝
        assert!(parse_snapshot(&b).is_none());
    }

    #[test]
    fn snapshot_rejects_truncated() {
        let b = pack_snapshot(&sample());
        for cut in [1usize, 9, 14, b.len() - 1] {
            assert!(parse_snapshot(&b[..cut]).is_none(), "cut={cut}");
        }
    }

    #[test]
    fn snapshot_rejects_trailing_bytes() {
        let mut b = pack_snapshot(&sample());
        b.push(0x00);
        assert!(parse_snapshot(&b).is_none());
    }

    #[test]
    fn snapshot_empty_lines_ok() {
        let s = Snapshot { chart_time: 0.0, dim: 1.0, lines: vec![] };
        let parsed = parse_snapshot(&pack_snapshot(&s)).expect("parse");
        assert!(parsed.lines.is_empty());
        assert_eq!(parsed.chart_time, 0.0);
    }

    #[test]
    fn texture_frame_roundtrip() {
        let png: Vec<u8> = (0..=255u8).collect();
        let name = "hold.png";
        let mut b = vec![TEX_TAG];
        w_u16(name.len() as u16, &mut b);
        b.extend_from_slice(name.as_bytes());
        w_u32(png.len() as u32, &mut b);
        b.extend_from_slice(&png);
        let (tag, n, data) = parse_texture_frame(&b).expect("parse");
        assert_eq!(tag, TEX_TAG);
        assert_eq!(n, name);
        assert_eq!(data, png);
    }

    #[test]
    fn texture_end_marker() {
        let (tag, n, data) = parse_texture_frame(&[TEX_END_TAG]).expect("parse");
        assert_eq!(tag, TEX_END_TAG);
        assert!(n.is_empty() && data.is_empty());
        // 结束帧不允许尾随字节
        assert!(parse_texture_frame(&[TEX_END_TAG, 0]).is_none());
        // 未知标签拒绝
        assert!(parse_texture_frame(&[0x42]).is_none());
    }

    #[test]
    fn texture_frame_rejects_truncated() {
        let name = "line.png";
        let mut b = vec![TEX_TAG];
        w_u16(name.len() as u16, &mut b);
        b.extend_from_slice(name.as_bytes());
        w_u32(8, &mut b);
        b.extend_from_slice(&[1, 2, 3]);
        assert!(parse_texture_frame(&b).is_none()); // PNG 字节不足
        assert!(parse_texture_frame(&b[..3]).is_none()); // name 截断
    }

    /// 协议槽位映射:线默认 6,note kind → 1/4/3/2。
    #[test]
    fn protocol_tex_mapping() {
        let s = sample();
        for l in &s.lines {
            assert_eq!(l.tex, 6);
        }
        for l in &s.lines {
            for n in &l.notes {
                let expect = match n.kind {
                    1 => 1,
                    2 => 4,
                    3 => 3,
                    4 => 2,
                    _ => unreachable!(),
                };
                assert_eq!(n.tex, expect);
            }
        }
    }
}
