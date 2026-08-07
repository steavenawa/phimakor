//! Font loading & global fallback chain (extracted from ui/mod.rs).

static UI_FONTS: std::sync::OnceLock<Vec<fontdue::Font>> = std::sync::OnceLock::new();
static UI_FONT_BYTES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Eagerly loaded fonts: primary + latin fallbacks (small).
const FONT_LATIN: [&str; 6] = [
    "res/Exo2.ttf",
    "res.dis/Exo2.ttf",
    "C:\\Windows\\Fonts\\arial.ttf",
    "C:\\Windows\\Fonts\\segoeui.ttf",
    "/System/Library/Fonts/Helvetica.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

/// Lazily loaded CJK candidates, best first. Each slot parses its font only
/// when an earlier font failed to cover a requested glyph.
const FONT_CJK: [&str; 12] = [
    "C:\\Windows\\Fonts\\msyh.ttc",
    "C:\\Windows\\Fonts\\msyhbd.ttc",
    "C:\\Windows\\Fonts\\simhei.ttf",
    "C:\\Windows\\Fonts\\simsun.ttc",
    "C:\\Windows\\Fonts\\Deng.ttf",
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
];

static CJK_SLOTS: [std::sync::OnceLock<Option<fontdue::Font>>; FONT_CJK.len()] = [const { std::sync::OnceLock::new() }; FONT_CJK.len()];
static CJK_BYTES: [std::sync::atomic::AtomicUsize; FONT_CJK.len()] = [const { std::sync::atomic::AtomicUsize::new(0) }; FONT_CJK.len()];

fn load_font_from(path: &str) -> Option<fontdue::Font> {
    std::fs::read(path).ok().and_then(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok())
}

/// All loaded eager fonts, primary first.
pub(crate) fn get_fonts() -> &'static Vec<fontdue::Font> {
    UI_FONTS.get_or_init(|| {
        let mut bytes = 0usize;
        let fonts: Vec<fontdue::Font> = FONT_LATIN.iter().filter_map(|p| {
            let font = load_font_from(p);
            if font.is_some() {
                if let Ok(md) = std::fs::metadata(p) { bytes += md.len() as usize; }
            }
            font
        }).collect();
        let _ = UI_FONT_BYTES.set(bytes);
        fonts
    })
}

/// Raw file bytes of the loaded UI font chain (rough heap footprint).
pub fn font_mem_bytes() -> usize {
    use std::sync::atomic::Ordering;
    let eager = *UI_FONT_BYTES.get().unwrap_or(&0);
    let cjk: usize = CJK_BYTES.iter().map(|b| b.load(Ordering::Relaxed)).sum();
    eager + cjk
}

/// The primary font (or None if nothing loaded).
pub(crate) fn get_font() -> Option<&'static fontdue::Font> {
    get_fonts().first()
}

/// First font in the chain that contains `ch`; falls back to the primary
/// font when nothing has the glyph (renders the font's default glyph).
/// CJK slots load on demand, stopping at the first font covering `ch`.
pub(crate) fn font_for(ch: char) -> Option<&'static fontdue::Font> {
    let fonts = get_fonts();
    if let Some(f) = fonts.iter().find(|f| f.has_glyph(ch)) {
        return Some(f);
    }
    for (i, path) in FONT_CJK.iter().enumerate() {
        let slot = &CJK_SLOTS[i];
        let font = slot.get_or_init(|| {
            let font = load_font_from(path);
            if font.is_some() {
                if let Ok(md) = std::fs::metadata(path) {
                    CJK_BYTES[i].store(md.len() as usize, std::sync::atomic::Ordering::Relaxed);
                }
            }
            font
        });
        if let Some(f) = font {
            if f.has_glyph(ch) {
                return Some(f);
            }
        }
    }
    fonts.first()
}

/// 预热 CJK 字体槽(启动时调用一次)。首次中文绘制会同步读盘+解析
/// msyh.ttc(~20MB,约 200ms),若发生在主线程绘制路径(加载屏谱名/
/// splash 列表)会把窗口"堵死"(用户实测:字体把加载路径堵住)。
/// 启动时预热后,所有路径的 font_for 零阻塞(OnceLock 已填充)。
/// 只触发第一个覆盖中文的槽位('中' 走 CJK 链,msyh 优先命中)。
pub fn warmup_cjk() {
    let _ = font_for('中');
}



