//! wgpu PipelineCache 持久化(PMCORE-68)。
//!
//! 磁盘缓存文件 `%APPDATA%\PhiMakor\pipeline_cache.bin`(路径约定镜像
//! main.rs `config_dir()`,见 [`cache_path`]),内容为
//! [`wgpu::PipelineCache::get_data`] 的原始输出。wgpu 数据自带校验头
//! (magic/version/backend/adapter key/validation key),换 GPU/驱动/升版
//! 自动失效:descriptor 的 `fallback: true` 让无效数据静默回退到空缓存,
//! 不崩溃。
//!
//! 禁用开关:env `PHIMAKOR_NO_PIPELINE_CACHE=1`(与 PHIMAKOR_GPU_TIMING
//! 等先例一致),设置后不读不写、不创建 PipelineCache,行为与无缓存完全
//! 一致。
//!
//! 写入采用 tmp+rename 原子写(与 edit.rs write_bytes_atomic 同模式);
//! 失败只告警不阻塞。读取全容错:文件不存在/短读/超大一律按无缓存处理。

use std::path::PathBuf;

/// 缓存文件字节数上限:超过按无缓存处理(防意外膨胀/恶意文件)。
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// 禁用开关是否生效(`PHIMAKOR_NO_PIPELINE_CACHE=1`)。
pub fn enabled() -> bool {
    std::env::var("PHIMAKOR_NO_PIPELINE_CACHE").map_or(true, |v| v != "1")
}

/// 缓存文件路径。镜像 main.rs `config_dir()` 的 3 行逻辑
/// (`%APPDATA%\PhiMakor\` → XDG/HOME → cwd);render 模块在 lib/bin 双
/// 上下文编译,不能引用 main.rs 私有函数,故在此保持同一约定。
pub fn cache_path() -> PathBuf {
    let base = if let Ok(appdata) = std::env::var("APPDATA") {
        PathBuf::from(appdata).join("PhiMakor")
    } else {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join("phimakor")
    };
    base.join("pipeline_cache.bin")
}

/// 读取缓存种子。任何异常(不存在/非文件/空/超大/IO 错误)都返回 `None`,
/// 调用方按"无缓存"处理——这是外部输入,读不进来不阻塞启动。
pub fn load_seed() -> Option<Vec<u8>> {
    if !enabled() {
        return None;
    }
    let path = cache_path();
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_CACHE_BYTES {
        return None;
    }
    let data = std::fs::read(&path).ok()?;
    if data.is_empty() {
        return None;
    }
    Some(data)
}

/// 把管线缓存写回磁盘(原子写 tmp+rename)。`get_data()` 返回 `None`
/// (后端不支持缓存,如非 Vulkan)时不动磁盘;写入失败只告警不阻塞。
pub fn save(cache: &wgpu::PipelineCache) {
    if !enabled() {
        return;
    }
    let Some(data) = cache.get_data() else { return };
    let path = cache_path();
    let tmp = path.with_file_name("pipeline_cache.bin.tmp");
    if let Err(e) = std::fs::write(&tmp, &data).and_then(|()| std::fs::rename(&tmp, &path)) {
        eprintln!("warning: pipeline cache 写盘失败(非阻塞,继续启动): {e}");
    }
}
