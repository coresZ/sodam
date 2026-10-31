//! 路径解析：安装版写系统目录，便携版把配置 / 缓存 / 下载全部写在程序目录下。
//!
//! 便携模式的判定只有一条规则：**可执行文件同目录存在 `portable.txt`**。
//! 便携版 zip 里自带这个标记，用户解压即用；安装版没有它，行为与以前完全一致。
//!
//! 环境变量 `SODAM_PORTABLE=1` 可强制开启（调试、或程序目录不可写标记文件时用）。
//!
//! 便携版布局（以 exe 所在目录为根）：
//!
//! ```text
//! SodaM/
//!   sodam.exe
//!   portable.txt      ← 标记文件，有它才启用便携模式
//!   data/config.json
//!   cache/audio/      播放缓存
//!   cache/covers/     封面缓存
//!   down/             默认下载目录
//! ```

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 便携标记文件名（与可执行文件同目录）。
pub const PORTABLE_MARKER: &str = "portable.txt";

/// 便携模式下的目录名。
const DATA_DIR: &str = "data";
const CACHE_DIR: &str = "cache";
const DOWNLOAD_DIR: &str = "down";

// ---------------------------------------------------------------------------
// 便携判定
// ---------------------------------------------------------------------------

/// 判定逻辑（可测）：给定 exe 目录，看有没有标记文件。
fn marker_present(exe_dir: Option<&Path>) -> bool {
    exe_dir
        .map(|dir| dir.join(PORTABLE_MARKER).is_file())
        .unwrap_or(false)
}

/// `SODAM_PORTABLE` 是否为「开」。
///
/// 空串 / `0` / `false` 视为关；其余非空值视为开（`1` / `true` / `yes` 都行）。
fn env_forced() -> bool {
    match std::env::var("SODAM_PORTABLE") {
        Ok(value) => {
            let value = value.trim().to_ascii_lowercase();
            !value.is_empty() && value != "0" && value != "false"
        }
        Err(_) => false,
    }
}

/// 可执行文件所在目录（缓存；进程内不变）。
fn exe_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
    })
    .as_deref()
}

/// 是否便携模式（结果在进程内缓存）。
pub fn is_portable() -> bool {
    static PORTABLE: OnceLock<bool> = OnceLock::new();
    *PORTABLE.get_or_init(|| env_forced() || marker_present(exe_dir()))
}

/// 便携根目录（非便携模式返回 `None`）。
pub fn portable_root() -> Option<&'static Path> {
    if is_portable() {
        exe_dir()
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 便携版布局（给定根目录；单独拆出来是为了可测）
// ---------------------------------------------------------------------------

fn layout_config_dir(root: &Path) -> PathBuf {
    root.join(DATA_DIR)
}

fn layout_cache_dir(root: &Path) -> PathBuf {
    root.join(CACHE_DIR)
}

fn layout_download_dir(root: &Path) -> PathBuf {
    root.join(DOWNLOAD_DIR)
}

// ---------------------------------------------------------------------------
// 各目录的对外入口
// ---------------------------------------------------------------------------

/// 配置目录。
pub fn config_dir() -> PathBuf {
    match portable_root() {
        Some(root) => layout_config_dir(root),
        None => dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("sodam"),
    }
}

/// 缓存根目录（其下再分 `audio/` 与 `covers/`）。
pub fn cache_dir() -> PathBuf {
    match portable_root() {
        Some(root) => layout_cache_dir(root),
        None => dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("sodam"),
    }
}

/// 默认下载目录：便携版是 `<exe 同级>/down`，安装版是「音乐 / SodaM」。
pub fn default_download_dir() -> PathBuf {
    if let Some(root) = portable_root() {
        return layout_download_dir(root);
    }
    dirs::audio_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("SodaM")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_detection_needs_a_file_next_to_the_exe() {
        let dir = std::env::temp_dir().join(format!("sodam-portable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        // 没有标记文件 → 不是便携版
        assert!(!marker_present(Some(&dir)));
        // exe 目录拿不到时也不该误判成便携版
        assert!(!marker_present(None));

        std::fs::write(dir.join(PORTABLE_MARKER), b"marker\n").expect("write marker");
        assert!(marker_present(Some(&dir)));

        // 目录不能冒充标记文件（只认文件）
        let other = dir.join("subdir");
        std::fs::create_dir_all(other.join(PORTABLE_MARKER)).expect("mkdir fake marker");
        assert!(!marker_present(Some(&other)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn portable_layout_keeps_everything_under_the_root() {
        let root = PathBuf::from("/opt/SodaM");
        assert_eq!(layout_config_dir(&root), root.join("data"));
        assert_eq!(layout_cache_dir(&root), root.join("cache"));
        assert_eq!(layout_download_dir(&root), root.join("down"));
        // 下载目录必须是用户要求的 `down`
        assert!(layout_download_dir(&root).ends_with("down"));
    }

    #[test]
    fn env_flag_parsing_is_forgiving() {
        // 这个测试不碰进程环境（并发下会互相干扰），只验证取值规则本身
        let truthy = ["1", "true", "yes", "on"];
        let falsy = ["", "0", "false", " "];
        for value in truthy {
            let normalized = value.trim().to_ascii_lowercase();
            let on = !normalized.is_empty() && normalized != "0" && normalized != "false";
            assert!(on, "{value:?} 应视为开启");
        }
        for value in falsy {
            let normalized = value.trim().to_ascii_lowercase();
            let on = !normalized.is_empty() && normalized != "0" && normalized != "false";
            assert!(!on, "{value:?} 应视为关闭");
        }
    }
}
