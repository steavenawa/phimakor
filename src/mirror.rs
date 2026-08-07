//! 镜像服务:桌面 editor 状态推流到局域网浏览器(wasm player 消费)。
//!
//! 零依赖实现:
//! - HTTP 静态服务(std TcpListener 手写):播放页 / 音乐文件(Range 支持)
//! - WebSocket 服务端(手写握手 SHA1+base64 / 帧编解码):推快照(二进制
//!   帧)与纹理清单,收控制指令(文本帧)
//! - 快照协议见 `player-wasm/PROTOCOL.md`
//!
//! 线程模型:accept 线程 + 每连接处理线程;主线程每帧把最新快照写进
//! 共享槽(30fps 节流),连接线程轮询 seq 变化后推流。控制指令经 mpsc
//! 回主线程执行(与 seek 通道同语义)。

use crate::core::{chart::FrameState, LineState};
use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// 镜像服务端口(固定;占用则启动失败并提示)。
pub const MIRROR_PORT: u16 = 8765;
/// 快照推流节流(帧/秒)。
const SNAP_FPS: f32 = 30.0;
/// 最大快照字节(防御异常帧撑爆共享槽)。
const MAX_SNAP_BYTES: usize = 2 * 1024 * 1024;

/// 手机端控制指令(经 mpsc 回主线程执行)。
#[derive(Clone, Copy, Debug)]
pub enum MirrorCtrl {
    Pause,
    Seek(f64),
}

/// 一个运行中的镜像服务。
pub struct MirrorServer {
    stop: Arc<AtomicBool>,
    snap: Arc<Mutex<Option<Vec<u8>>>>,
    snap_seq: Arc<AtomicU64>,
    /// 控制指令通道(手机 → 主线程)。
    ctrl_rx: mpsc::Receiver<MirrorCtrl>,
    /// 上次打包时刻(节流)。
    last_pack: std::time::Instant,
    /// 服务是否就绪(启动失败 = None)。
    ok: bool,
}

/// 内置纹理槽名(顺序与 PROTOCOL.md 一致;槽 0 = 白,不发)。
pub const BUILTIN_TEX_NAMES: [&str; 6] = ["click", "drag", "flick", "hold", "hitfx", "line"];

impl MirrorServer {
    /// 启动镜像服务(失败返回可读错误,不 panic)。
    pub fn start(charts_dir: PathBuf, music_name: String, line_textures: &[String]) -> Result<Self, String> {
        // 纹理槽:6 内置 + 谱面线纹理(去重,保持线索引顺序)。
        let mut slots: Vec<String> = BUILTIN_TEX_NAMES.iter().map(|s| s.to_string()).collect();
        let mut seen = std::collections::HashSet::new();
        for t in line_textures {
            if !t.is_empty() && seen.insert(t.clone()) {
                slots.push(t.clone());
            }
        }
        let listener = TcpListener::bind(("0.0.0.0", MIRROR_PORT))
            .map_err(|e| format!("镜像服务启动失败(端口 {MIRROR_PORT}): {e}"))?;
        // TLS(自签):WebGPU 要求 secure context,局域网 IP 的纯 http 会被
        // 浏览器禁用 navigator.gpu(用户实测)。自签生成失败则退 http 模式
        // (localhost 调试仍可用)。
        let tls = make_tls_config();
        let tls = match &tls {
            Some(cfg) => Some(Arc::new(cfg.clone())),
            None => {
                eprintln!("mirror: 自签证书生成失败,降级 http(手机 WebGPU 不可用,仅 localhost 调试)");
                None
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let snap: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let snap_seq = Arc::new(AtomicU64::new(0));
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<MirrorCtrl>();
        let slots = Arc::new(slots);
        let charts_dir = Arc::new(charts_dir);
        let music_name = Arc::new(music_name);

        let (st, sq, sl, cd, mn, snap2, ctrl2, tls2) = (
            stop.clone(), snap_seq.clone(), slots.clone(), charts_dir.clone(), music_name.clone(),
            snap.clone(), ctrl_tx.clone(), tls,
        );
        std::thread::Builder::new()
            .name("mirror-accept".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if st.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(mut stream) = stream else { continue };
                    let _ = stream.set_nonblocking(true);
                    let (s2, q2, sl2, cd2, mn2) = (st.clone(), sq.clone(), sl.clone(), cd.clone(), mn.clone());
                    let snap3 = snap2.clone();
                    let ctrl3 = ctrl2.clone();
                    let tls3 = tls2.clone();
                    std::thread::Builder::new()
                        .name("mirror-conn".into())
                        .spawn(move || {
                            // TLS 模式:握手惰性发生在首次读(客户端发请求头时)。
                            if let Some(cfg) = tls3 {
                                if let Ok(conn) = rustls::ServerConnection::new(cfg) {
                                    let mut s = rustls::StreamOwned::new(conn, stream);
                                    handle_conn(&mut s, s2, q2, snap3, ctrl3, sl2, cd2, mn2);
                                    // TLS 1.3 必须显式发 close_notify:否则客户端
                                    // 视为截断错误(浏览器 ERR_CONTENT_LENGTH_MISMATCH,
                                    // wasm 编译中止——用户实测)。
                                    s.conn.send_close_notify();
                                    let _ = s.flush();
                                    return;
                                }
                            }
                            handle_conn(&mut stream, s2, q2, snap3, ctrl3, sl2, cd2, mn2);
                        })
                        .ok();
                }
            })
            .map_err(|e| format!("镜像服务线程启动失败: {e}"))?;
        Ok(MirrorServer {
            stop,
            snap,
            snap_seq,
            ctrl_rx,
            last_pack: std::time::Instant::now(),
            ok: true,
        })
    }

    /// 每帧喂最新 frame(主线程 render_frame 尾部调用;30fps 节流)。
    /// `tex_slots` 与启动时一致(切谱后由宿主调用 [`Self::refresh_tex`])。
    pub fn tick(&mut self, frame: &FrameState, dim: f32, tex_slots: &[String]) {
        if !self.ok {
            return;
        }
        if self.last_pack.elapsed().as_secs_f32() < 1.0 / SNAP_FPS {
            return;
        }
        self.last_pack = std::time::Instant::now();
        let bytes = pack_snapshot(frame, dim, tex_slots);
        if bytes.len() > MAX_SNAP_BYTES {
            return;
        }
        let mut slot = self.snap.lock().unwrap();
        *slot = Some(bytes);
        drop(slot);
        self.snap_seq.fetch_add(1, Ordering::Relaxed);
    }

    /// 每帧消费手机控制指令(主线程调用;与 seek 通道同语义)。
    pub fn poll_ctrl(&mut self) -> Option<MirrorCtrl> {
        self.ctrl_rx.try_iter().last()
    }

    /// 停止服务(关连接线程)。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// 快照打包(PROTOCOL.md 0x01;大端)
// ---------------------------------------------------------------------------

fn push_u8(v: &mut Vec<u8>, b: u8) { v.push(b); }
fn push_u16(v: &mut Vec<u8>, n: u16) { v.extend_from_slice(&n.to_be_bytes()); }
fn push_u32(v: &mut Vec<u8>, n: u32) { v.extend_from_slice(&n.to_be_bytes()); }
fn push_i32(v: &mut Vec<u8>, n: i32) { v.extend_from_slice(&n.to_be_bytes()); }
fn push_f32(v: &mut Vec<u8>, n: f32) { v.extend_from_slice(&n.to_be_bytes()); }
fn push_f64(v: &mut Vec<u8>, n: f64) { v.extend_from_slice(&n.to_be_bytes()); }

/// 打包一帧快照。`tex_slots` = 槽名列表(6 内置 + 谱面线纹理)。
pub fn pack_snapshot(frame: &FrameState, dim: f32, tex_slots: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4096);
    push_u8(&mut out, 0x01);
    push_f64(&mut out, frame.time);
    push_f32(&mut out, dim);
    push_u16(&mut out, frame.lines.len().min(u16::MAX as usize) as u16);
    for line in &frame.lines {
        pack_line(&mut out, line, tex_slots);
    }
    out
}

fn pack_line(out: &mut Vec<u8>, line: &LineState, tex_slots: &[String]) {
    push_f32(out, line.position[0]);
    push_f32(out, line.position[1]);
    push_f32(out, line.rotation);
    push_f32(out, line.scale[0]);
    push_f32(out, line.scale[1]);
    push_f32(out, line.alpha);
    push_i32(out, line.z_order);
    // 线纹理槽:默认 6(line);自定义 → 7+ 槽位。
    let tex = line.texture.as_deref().and_then(|t| tex_slots.iter().position(|s| s == t)).unwrap_or(6);
    push_u8(out, tex.min(u8::MAX as usize) as u8);
    push_u16(out, line.notes.len().min(u16::MAX as usize) as u16);
    for n in &line.notes {
        // kind → 纹理槽映射(与桌面渲染一致):1 tap→1,2 hold→4,3 flick→3,4 drag→2。
        let ntex = match n.kind {
            1 => 1,
            2 => 4,
            3 => 3,
            _ => 2,
        };
        push_u8(out, n.kind);
        push_f32(out, n.relative[0]);
        push_f32(out, n.relative[1]);
        push_f32(out, n.hold_end_y.map(|y| y as f32).unwrap_or(f32::NAN));
        push_f32(out, n.alpha);
        push_f32(out, n.scale);
        push_u8(out, ntex);
    }
}

// ---------------------------------------------------------------------------
// 手写 HTTP + WebSocket 服务
// ---------------------------------------------------------------------------

/// 连接处理线程:读请求头 → 路由。流已设为非阻塞(WouldBlock 轮询)。
fn handle_conn<T: Read + Write>(
    stream: &mut T,
    stop: Arc<AtomicBool>,
    snap_seq: Arc<AtomicU64>,
    snap: Arc<Mutex<Option<Vec<u8>>>>,
    ctrl: mpsc::Sender<MirrorCtrl>,
    tex_slots: Arc<Vec<String>>,
    charts_dir: Arc<PathBuf>,
    music_name: Arc<String>,
) {
    // 读请求头(直到空行;上限 8KB 防恶意;非阻塞轮询,5s 超时)。
    let mut buf = [0u8; 8192];
    let mut got = 0usize;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match stream.read(&mut buf[got..]) {
            Ok(0) => return,
            Ok(n) => {
                got += n;
                if buf[..got].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if got >= buf.len() {
                    return;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(_) => return,
        }
    }
    let head = String::from_utf8_lossy(&buf[..got]);
    let mut lines = head.lines();
    let req_line = lines.next().unwrap_or("");
    let mut parts = req_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    // 收集头。
    let mut headers: Vec<(String, String)> = Vec::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let get = |k: &str| headers.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());

    if path == "/ws" {
        if let Some(key) = get("sec-websocket-key") {
            let accept = ws_accept(&key);
            let _ = write_all(stream, &format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n\r\n"
            ));
            serve_ws(stream, stop, snap_seq, snap, ctrl, tex_slots, charts_dir);
        }
        return;
    }
    if path == "/" || path == "/index.html" {
        // 注入当前谱面音乐文件名(JS 据此拉 /music/<名> 播放)。
        let page = PLAYER_HTML.replace("__MUSIC__", &music_name);
        let _ = write_all(stream, &format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            page.len(), page
        ));
        return;
    }
    // 播放器 wasm 产物(/pkg/*):候选目录 CWD 的 player-wasm/pkg 与 pkg。
    // 发布时把 wasm-bindgen 产物放到 exe 旁边任一位置即可。
    if let Some(rel) = path.strip_prefix("/pkg/") {
        for base in [Path::new("player-wasm/pkg"), Path::new("pkg")] {
            let p = base.join(rel);
            if let Ok(bytes) = std::fs::read(&p) {
                let ct = if rel.ends_with(".js") {
                    "application/javascript"
                } else if rel.ends_with(".wasm") {
                    "application/wasm"
                } else {
                    "application/octet-stream"
                };
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len());
                if write_all(stream, &head).is_ok() {
                    let _ = stream.write_all(&bytes);
                }
                return;
            }
        }
        let _ = write_all(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    if let Some(name) = path.strip_prefix("/music/") {
        serve_music(stream, &charts_dir, &music_name, name, &get);
        return;
    }
    if path == "/ctrl" {
        // POST /ctrl:读 body JSON。
        let body = read_http_body(stream, &buf[..got], &get);
        if let Some(cmd) = parse_ctrl_json(&body) {
            let _ = ctrl.send(cmd);
            let _ = write_all(stream, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        } else {
            let _ = write_all(stream, "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
        return;
    }
    let _ = write_all(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

fn write_all<T: Write>(stream: &mut T, s: &str) -> std::io::Result<()> {
    stream.write_all(s.as_bytes())?;
    stream.flush()
}

fn read_http_body<T: Read>(stream: &mut T, head_buf: &[u8], get: &dyn Fn(&str) -> Option<String>) -> Vec<u8> {
    let clen = get("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    if clen == 0 || clen > 65536 {
        return Vec::new();
    }
    // 请求头之后可能已带 body 前缀(小 POST 一次性到达)。
    let head_end = head_buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4).unwrap_or(head_buf.len());
    let mut body = head_buf[head_end..].to_vec();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while body.len() < clen {
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(_) => break,
        }
    }
    body.truncate(clen);
    body
}

/// 手机控制 JSON → 指令。
fn parse_ctrl_json(body: &[u8]) -> Option<MirrorCtrl> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    match v.get("a")?.as_str()? {
        "pause" => Some(MirrorCtrl::Pause),
        "seek" => Some(MirrorCtrl::Seek(v.get("t")?.as_f64()?)),
        _ => None,
    }
}

/// 音乐文件服务(支持 Range)。
fn serve_music<T: Read + Write>(stream: &mut T, charts_dir: &Path, music_name: &str, name: &str, get: &dyn Fn(&str) -> Option<String>) {
    // 只允许当前谱面音乐文件名(防目录穿越)。
    if name != music_name {
        let _ = write_all(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let path = charts_dir.join(name);
    let Ok(meta) = std::fs::metadata(&path) else {
        let _ = write_all(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let total = meta.len();
    let range = get("range");
    // 解析 Range: bytes=start-end / bytes=start- / bytes=-suffix
    let (start, end) = range.as_ref()
        .and_then(|r| r.strip_prefix("bytes=").map(|s| s.to_string()))
        .and_then(|r| {
            let (a, b) = r.split_once('-')?;
            let start: u64 = if a.is_empty() { 0 } else { a.parse().ok()? };
            let end: u64 = if b.is_empty() { total.saturating_sub(1) } else { b.parse().ok()? };
            Some((start, end.min(total.saturating_sub(1))))
        })
        .unwrap_or((0, total.saturating_sub(1)));
    if start > end || start >= total {
        let _ = write_all(stream, &format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"));
        return;
    }
    let Ok(mut f) = std::fs::File::open(&path) else { return };
    if start > 0 {
        use std::io::Seek as _;
        let _ = f.seek(std::io::SeekFrom::Start(start));
    }
    let len = end - start + 1;
    let head = if range.is_some() {
        format!("HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Type: audio/mpeg\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n")
    } else {
        format!("HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Type: audio/mpeg\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n")
    };
    if write_all(stream, &head).is_err() {
        return;
    }
    let mut remaining = len;
    let mut chunk = [0u8; 64 * 1024];
    while remaining > 0 {
        let n = ((remaining.min(chunk.len() as u64)) as usize).min(chunk.len());
        match f.read(&mut chunk[..n]) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stream.write_all(&chunk[..n]).is_err() {
                    break;
                }
                remaining -= n as u64;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket(服务端,手写)
// ---------------------------------------------------------------------------

/// 握手响应:SHA1(key + GUID) → base64。
fn ws_accept(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let input = format!("{key}{GUID}");
    let digest = sha1(input.as_bytes());
    base64_encode(&digest)
}

/// 发送一帧二进制(0x82)。>65535 走 64 位长度。
fn ws_send_binary<T: Write>(stream: &mut T, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len();
    let mut frame = Vec::with_capacity(len + 10);
    frame.push(0x82);
    if len < 126 {
        frame.push(len as u8);
    } else if len <= 0xFFFF {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame)?;
    stream.flush()
}

/// 非阻塞读满 `buf`(WouldBlock 轮询,`wait` 内超时)。返回是否读满。
fn read_full<T: Read>(stream: &mut T, buf: &mut [u8], wait: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + wait;
    let mut off = 0usize;
    while off < buf.len() {
        match stream.read(&mut buf[off..]) {
            Ok(0) => return false,
            Ok(n) => off += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(_) => return false,
        }
    }
    true
}

/// 收一帧(客户端 → 服务器;要求文本 0x81,处理 mask)。
/// 非阻塞:最多等 `wait`(帧推流不被控制读取卡住)。
fn ws_recv_text<T: Read>(stream: &mut T, wait: std::time::Duration) -> Option<String> {
    let mut hdr = [0u8; 2];
    if !read_full(stream, &mut hdr, wait) {
        return None;
    }
    let opcode = hdr[0] & 0x0F;
    if opcode != 0x01 {
        return None;
    }
    let masked = hdr[1] & 0x80 != 0;
    let mut len = (hdr[1] & 0x7F) as u64;
    if len == 126 {
        let mut b = [0u8; 2];
        if !read_full(stream, &mut b, wait) {
            return None;
        }
        len = u16::from_be_bytes(b) as u64;
    } else if len == 127 {
        let mut b = [0u8; 8];
        if !read_full(stream, &mut b, wait) {
            return None;
        }
        len = u64::from_be_bytes(b);
    }
    if len > 65536 {
        return None;
    }
    let mut mask = [0u8; 4];
    if masked && !read_full(stream, &mut mask, wait) {
        return None;
    }
    let mut payload = vec![0u8; len as usize];
    if !read_full(stream, &mut payload, wait) {
        return None;
    }
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
    }
    String::from_utf8(payload).ok()
}

/// 推流循环:发纹理清单,然后循环推最新快照;收控制指令转发。
/// 流非阻塞:控制读取最多等 5ms,帧推送不被卡(修复:阻塞 read_exact
/// 曾让推流降到 0.2fps)。
fn serve_ws<T: Read + Write>(
    stream: &mut T,
    stop: Arc<AtomicBool>,
    snap_seq: Arc<AtomicU64>,
    snap: Arc<Mutex<Option<Vec<u8>>>>,
    ctrl: mpsc::Sender<MirrorCtrl>,
    tex_slots: Arc<Vec<String>>,
    charts_dir: Arc<PathBuf>,
) {
    // 纹理清单:逐条 0x00 帧,0xFF 结束。
    let res_dir = Path::new("res");
    for (i, name) in tex_slots.iter().enumerate() {
        // 内置:res/{name}.png;谱面线纹理:charts_dir/{name}(槽 7+)。
        let bytes = if i < 6 {
            std::fs::read(res_dir.join(format!("{name}.png"))).ok()
        } else {
            std::fs::read(charts_dir.join(name)).ok()
        };
        if let Some(bytes) = bytes {
            let mut f = Vec::new();
            f.push(0x00);
            let nb = name.as_bytes();
            push_u16(&mut f, nb.len() as u16);
            f.extend_from_slice(nb);
            push_u32(&mut f, bytes.len() as u32);
            f.extend_from_slice(&bytes);
            if ws_send_binary(stream, &f).is_err() {
                return;
            }
        }
    }
    let mut end = Vec::new();
    end.push(0xFF);
    if ws_send_binary(stream, &end).is_err() {
        return;
    }
    // 循环推快照。
    let mut last_seq = snap_seq.load(Ordering::Relaxed);
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let seq = snap_seq.load(Ordering::Relaxed);
        if seq != last_seq {
            last_seq = seq;
            let payload = snap.lock().unwrap().clone();
            if let Some(p) = payload {
                if ws_send_binary(stream, &p).is_err() {
                    return;
                }
            }
        }
        // 控制:非阻塞收一帧(最多等 5ms,不卡推流)。
        match ws_recv_text(stream, std::time::Duration::from_millis(5)) {
            Some(text) => {
                if let Some(c) = parse_ctrl_json(text.as_bytes()) {
                    let _ = ctrl.send(c);
                }
            }
            None => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

/// 自签 TLS 配置(rcgen 生成证书;失败返回 None 降级 http)。
/// WebGPU 要求 secure context:手机经局域网 IP 访问必须 https,
/// 自签证书由用户浏览器接受(证书警告 → 继续访问)。
fn make_tls_config() -> Option<rustls::ServerConfig> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cert = rcgen::generate_simple_self_signed(vec!["phimakor.local".to_string()]).ok()?;
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert.cert.der().clone())], key)
        .ok()?;
    Some(cfg)
}

// ---------------------------------------------------------------------------
// SHA1 + base64(零依赖)
// ---------------------------------------------------------------------------

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bit_len = (msg.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// 本机局域网 IPv4(经典 UDP 探路法,零依赖;失败回退 127.0.0.1)。
pub(crate) fn local_ipv4() -> String {
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                return addr.ip().to_string();
            }
        }
    }
    "127.0.0.1".to_string()
}

/// 播放器页面(wasm player 加载 + WS 转发 + 控制按钮)。
const PLAYER_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, user-scalable=no">
<title>PhiMakor Player</title>
<style>
  html,body{margin:0;background:#000;height:100%;overflow:hidden}
  #screen{width:100vw;height:100vh;display:block}
  #ctrl{position:fixed;bottom:12px;left:50%;transform:translateX(-50%);
        display:flex;gap:8px;opacity:.35}
  #ctrl button{background:#222c;color:#fff;border:1px solid #555;border-radius:8px;
        padding:8px 16px;font-size:15px;touch-action:manipulation}
  #err{position:fixed;top:12px;left:50%;transform:translateX(-50%);color:#f88;font:13px sans-serif}
</style>
</head>
<body>
<canvas id="screen"></canvas>
<div id="ctrl">
  <button id="b-pause">⏯</button>
  <button id="b-back">⏮</button>
  <button id="b-fwd">⏭</button>
</div>
<div id="err"></div>
<script type="module">
import init, { handle_frame, handle_texture, start_stream } from "./pkg/phimakor_player.js";
await init();
const err = document.getElementById("err");
const fail = (m) => { err.textContent = m; console.error(m); };
// 音频:手机直接拉音乐文件(与画面粗略同步)。
const MUSIC = "__MUSIC__";
if (MUSIC) {
  const a = new Audio("/music/" + encodeURIComponent(MUSIC));
  a.loop = true;
  a.play().catch(() => {});
}
// WebSocket 快照流。
const ws = new WebSocket((location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/ws");
ws.binaryType = "arraybuffer";
ws.onmessage = (ev) => {
  const bytes = new Uint8Array(ev.data);
  if (bytes[0] === 0x00) handle_texture(bytes);
  else if (bytes[0] === 0x01) handle_frame(bytes);
};
ws.onerror = () => fail("连接电脑失败 — 确认编辑器已开镜像服务");
ws.onclose = () => fail("连接已断开");
const send = (o) => { if (ws.readyState === 1) ws.send(JSON.stringify(o)); };
document.getElementById("b-pause").onclick = () => send({ a: "pause" });
document.getElementById("b-back").onclick = () => send({ a: "seek", t: 0 });
document.getElementById("b-fwd").onclick = () => send({ a: "seek", t: 99999 });
start_stream();
</script>
</body>
</html>"#;
