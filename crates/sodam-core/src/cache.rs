//! 播放缓存上限：按 LRU 淘汰旧音频缓存，把占用收敛到上限的 85% 以内。
//!
//! 只管**播放缓存**（`cache/audio`，见 [`crate::audio::cache_dir`]）：不碰封面缓存，
//! 更不碰下载目录（`down/`）—— 缓存与下载刻意分离（见 [`crate::paths`]）。
//!
//! 时机：每次成功落一首缓存后调用（本身已在后台阻塞上下文），**不进渲染路径**。
//!
//! LRU 的依据是文件 mtime：缓存命中（播放 / 预取）时由 [`crate::session`] 刷新它，
//! 因此「最久没用过」= mtime 最旧。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// 高水位比例：占用达到上限的这个比例就开始删，留出余量避免频繁触发。
const HIGH_WATER_NUM: u64 = 85;
const HIGH_WATER_DEN: u64 = 100;

/// 播放缓存可淘汰文件的音质边车扩展名。
const SIDECAR_EXT: &str = "quality";
/// 音频缓存扩展名（与 [`crate::session`] 落盘时一致）。
const AUDIO_EXT: &str = "m4a";

/// 播放缓存设置（`config.json` 的 `cache` 块）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheSettings {
    /// 播放缓存上限（MB）；`0` = 不限（默认）。
    ///
    /// 纯本地项，**不参与 [`crate::config::Settings::fingerprint`]** —— 改它不该重建会话。
    pub limit_mb: u64,
}

impl CacheSettings {
    /// 上限字节数；`0`（不限）时返回 `None`。
    pub fn limit_bytes(&self) -> Option<u64> {
        (self.limit_mb > 0).then(|| self.limit_mb * 1024 * 1024)
    }

    /// 高水位线：占用超过它就开始淘汰，一直删到不超过它。
    pub fn threshold_bytes(&self) -> Option<u64> {
        self.limit_bytes()
            .map(|limit| limit * HIGH_WATER_NUM / HIGH_WATER_DEN)
    }
}

/// 一次淘汰的结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvictionReport {
    /// 删除的文件数（音频 + 边车）。
    pub removed_files: usize,
    /// 释放的字节数。
    pub freed_bytes: u64,
}

/// 按设置淘汰播放缓存；`protect` 是刚写入 / 正在播放的文件，永不删除。
///
/// 未设置上限时是 no-op。
pub fn enforce_cache_limit(settings: &CacheSettings, protect: Option<&Path>) -> EvictionReport {
    match settings.threshold_bytes() {
        Some(threshold) => evict_to_threshold(&crate::audio::cache_dir(), threshold, protect),
        None => EvictionReport::default(),
    }
}

/// 把目录占用淘汰到 `threshold` 字节以下（LRU：最久没用的先删）。
///
/// 淘汰单位是**整首歌**：`<id>-<tag>.m4a` 连同它的 `.quality` 边车一起删 ——
/// 只删边车会让 [`crate::session`] 的大小校验失配、白白重下一遍。
/// 传输中的 `.part` 与孤儿边车不单独列为淘汰单位（它们要么还在用、要么无所谓）。
/// 单个文件删不掉（Windows 上被占用）时跳过它继续下一首。
fn evict_to_threshold(dir: &Path, threshold: u64, protect: Option<&Path>) -> EvictionReport {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return EvictionReport::default();
    };

    let mut total = 0u64;
    let mut units: Vec<Unit> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        total += meta.len();
        // 只把 `.m4a` 当作可淘汰单位；`.part` / `.quality` 不单列。
        if path.extension().map(|ext| ext == AUDIO_EXT) != Some(true) {
            continue;
        }
        units.push(Unit {
            audio: path,
            bytes: meta.len(),
            used_at: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    if total <= threshold {
        return EvictionReport::default();
    }

    // 最久没用的先删。
    units.sort_by_key(|unit| unit.used_at);

    let mut report = EvictionReport::default();
    for unit in units {
        if total <= threshold {
            break;
        }
        if Some(unit.audio.as_path()) == protect {
            continue;
        }
        let sidecar = unit.audio.with_extension(SIDECAR_EXT);
        let sidecar_bytes = std::fs::metadata(&sidecar)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if std::fs::remove_file(&unit.audio).is_err() {
            // 删除失败（Windows 上文件被占用）：跳过，继续看下一首。
            continue;
        }
        report.removed_files += 1;
        report.freed_bytes += unit.bytes;
        total = total.saturating_sub(unit.bytes);
        if std::fs::remove_file(&sidecar).is_ok() {
            report.removed_files += 1;
            report.freed_bytes += sidecar_bytes;
            total = total.saturating_sub(sidecar_bytes);
        }
    }
    report
}

/// 一个可淘汰单位：一首歌的音频文件。
struct Unit {
    audio: PathBuf,
    bytes: u64,
    used_at: SystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    /// 造一个「下载于 `age` 秒前」的缓存文件（音频 + 边车），返回音频路径。
    fn write_unit(dir: &Path, name: &str, bytes: usize, age: Duration) -> PathBuf {
        let audio = dir.join(format!("{name}.m4a"));
        let sidecar = dir.join(format!("{name}.quality"));
        std::fs::write(&audio, vec![0u8; bytes]).expect("write audio");
        let mut file = std::fs::File::create(&sidecar).expect("create sidecar");
        write!(file, "lossless\t{bytes}").expect("write sidecar");
        let when = SystemTime::now() - age;
        set_mtime(&audio, when);
        set_mtime(&sidecar, when);
        audio
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for touch");
        file.set_modified(when).expect("set mtime");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sodam-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn limit_zero_means_unbounded() {
        let settings = CacheSettings { limit_mb: 0 };
        assert_eq!(settings.limit_bytes(), None);
        assert_eq!(settings.threshold_bytes(), None);
    }

    #[test]
    fn threshold_is_85_percent_of_the_limit() {
        let settings = CacheSettings { limit_mb: 100 };
        assert_eq!(settings.limit_bytes(), Some(100 * 1024 * 1024));
        assert_eq!(settings.threshold_bytes(), Some(85 * 1024 * 1024));
    }

    /// 低于阈值不动；超过阈值时按 LRU 删到阈值以下，且音频与边车成对删除。
    #[test]
    fn evicts_oldest_until_under_threshold() {
        let dir = temp_dir("evict");
        // 三首各 100 字节：oldest / middle / newest
        let oldest = write_unit(&dir, "old", 100, Duration::from_secs(300));
        write_unit(&dir, "mid", 100, Duration::from_secs(200));
        write_unit(&dir, "new", 100, Duration::from_secs(100));

        // 阈值 300：占用 336（3×112）> 300，删掉最老的一首（112）后降到 224 ≤ 300。
        let report = evict_to_threshold(&dir, 300, None);
        assert_eq!(report.removed_files, 2, "音频 + 边车成对删除");
        assert!(report.freed_bytes > 0);
        assert!(!oldest.exists(), "最久没用的先被删");
        assert!(dir.join("mid.m4a").exists());
        assert!(dir.join("new.m4a").exists());
        assert!(!dir.join("old.quality").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `protect` 指定的文件（正在播放 / 刚写入）永不被删，即使它最旧。
    #[test]
    fn protected_file_is_never_removed() {
        let dir = temp_dir("protect");
        let oldest = write_unit(&dir, "playing", 400, Duration::from_secs(300));
        write_unit(&dir, "other", 100, Duration::from_secs(100));

        let report = evict_to_threshold(&dir, 150, Some(&oldest));
        assert!(oldest.exists(), "受保护的文件不能删");
        assert!(!dir.join("other.m4a").exists(), "没有保护的文件照常淘汰");
        assert_eq!(report.removed_files, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 已经低于阈值时是 no-op。
    #[test]
    fn does_nothing_when_under_threshold() {
        let dir = temp_dir("under");
        write_unit(&dir, "a", 50, Duration::from_secs(10));
        let report = evict_to_threshold(&dir, 10_000, None);
        assert_eq!(report, EvictionReport::default());
        assert!(dir.join("a.m4a").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
