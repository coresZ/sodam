//! 磁盘缓存维护。所有扫描/删除由后台调用；租约保护下载、待装载和正在播放的音频。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

pub const DEFAULT_AUDIO_LIMIT_MB: u64 = 10 * 1024;
const PART_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

type Leases = HashMap<PathBuf, std::sync::Weak<PathBuf>>;
fn leases() -> &'static Mutex<Leases> {
    static LEASES: OnceLock<Mutex<Leases>> = OnceLock::new();
    LEASES.get_or_init(Mutex::default)
}

// 释放租约仅减 Arc 引用计数，绝不在 UI 丢弃过期加载结果时等待维护线程的文件 IO。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CacheLease(Arc<PathBuf>);

impl CacheLease {
    pub(crate) fn acquire(path: &Path) -> Self {
        let mut active = leases().lock().unwrap_or_else(|err| err.into_inner());
        active.retain(|_, lease| lease.strong_count() > 0);
        if let Some(lease) = active.get(path).and_then(|lease| lease.upgrade()) {
            return Self(lease);
        }
        let lease = Arc::new(path.to_owned());
        active.insert(path.to_owned(), Arc::downgrade(&lease));
        Self(lease)
    }
}

/// 同一缓存目标的写者串行化，避免预取与手动播放互相覆盖文件/sidecar。
pub(crate) fn download_lock(path: &Path) -> Arc<Mutex<()>> {
    type Locks = HashMap<PathBuf, std::sync::Weak<Mutex<()>>>;
    static LOCKS: OnceLock<Mutex<Locks>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(|lock| lock.upgrade()) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_owned(), Arc::downgrade(&lock));
    lock
}

/// 解码失败时成对丢弃坏缓存；不能与另一个下载写者交错删除新文件/sidecar。
/// 音频线程不等待网络下载持有的锁，忙时留待下次失败重试。
pub(crate) fn invalidate_audio(path: &Path) {
    let lock = download_lock(path);
    let Ok(_writer) = lock.try_lock() else {
        return;
    };
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("quality"));
}

pub fn root_dir() -> PathBuf {
    // 便携模式下必须跟着程序目录走（`paths` 负责判定），否则便携版会把缓存
    // 写进用户目录，与「插 U 盘就能带走」的承诺冲突。
    if let Some(path) = std::env::var_os("SODAM_CACHE_DIR").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    crate::paths::cache_dir()
}

// .quality 与音频共享保护键，避免播放/下载时只删了 sidecar。
fn protection_key(path: &Path) -> PathBuf {
    if path.extension().is_some_and(|ext| ext == "quality") {
        path.with_extension("m4a")
    } else {
        path.to_owned()
    }
}

fn remove_idle(path: &Path) -> bool {
    let active = leases().lock().unwrap_or_else(|err| err.into_inner());
    if active
        .get(&protection_key(path))
        .is_some_and(|lease| lease.strong_count() > 0)
    {
        return false;
    }
    // 检查和 unlink 在同一临界区，避免租约在二者之间建立。
    std::fs::remove_file(path).is_ok()
}

fn files_in(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        // 不沿符号链接走到缓存目录外。
        if kind.is_dir() {
            files_in(&entry.path(), files);
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
}

fn is_fresh_part(path: &Path, modified: SystemTime) -> bool {
    path.extension().is_some_and(|ext| ext == "part")
        && modified.elapsed().unwrap_or_default() < PART_MAX_AGE
}

/// 递归清理缓存，保留正在使用的音频和近期临时文件。返回成功删除的文件数。
pub fn clear_in(root: &Path) -> usize {
    let mut files = Vec::new();
    files_in(root, &mut files);
    files
        .into_iter()
        .filter(|path| {
            let modified = std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::now());
            !is_fresh_part(path, modified) && remove_idle(path)
        })
        .count()
}

/// 按最后使用时间淘汰音频（命中缓存时更新 mtime）。租约文件可暂时超额。
/// sidecar 与音频成组淘汰；失败的删除不计入释放空间。调用方必须在后台执行。
pub fn trim_audio_in(dir: &Path, limit_bytes: u64) -> u64 {
    let mut files = Vec::new();
    files_in(dir, &mut files);
    let mut groups: HashMap<PathBuf, (SystemTime, Vec<(PathBuf, u64)>)> = HashMap::new();
    let mut total = 0u64;
    for path in files {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let bytes = meta.len();
        total = total.saturating_add(bytes);
        let modified = meta.modified().unwrap_or(SystemTime::now());
        if is_fresh_part(&path, modified) {
            continue;
        }
        let key = protection_key(&path);
        let group = groups.entry(key).or_insert((modified, Vec::new()));
        group.0 = group.0.max(modified);
        group.1.push((path, bytes));
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by_key(|(_, (modified, _))| *modified);
    for (_, (_, files)) in groups {
        if total <= limit_bytes {
            break;
        }
        for (path, bytes) in files {
            if remove_idle(&path) {
                total = total.saturating_sub(bytes);
            }
        }
    }
    total
}

pub fn trim_audio(limit_mb: u64) -> u64 {
    trim_audio_in(
        &root_dir().join("audio"),
        limit_mb.saturating_mul(1024 * 1024),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &Path, name: &str, age_seconds: u64) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, [0; 10]).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(age_seconds))
            .unwrap();
        path
    }

    #[test]
    fn clear_recurses_and_preserves_leases_and_fresh_parts() {
        let dir = tempfile::tempdir().unwrap();
        let audio = file(dir.path(), "audio/playing.m4a", 0);
        file(dir.path(), "audio/playing.quality", 0);
        let lease = CacheLease::acquire(&audio);
        file(dir.path(), "audio/download.part", 0);
        file(dir.path(), "audio/abandoned.part", 90_000);
        file(dir.path(), "covers/thumb.img", 0);
        file(dir.path(), "covers/original/full.img", 0);
        assert_eq!(clear_in(dir.path()), 3);
        assert!(audio.exists());
        assert!(audio.with_extension("quality").exists());
        assert!(dir.path().join("audio/download.part").exists());
        drop(lease);
        assert_eq!(clear_in(dir.path()), 2);
    }

    /// #60 的回归断言：清空缓存不能顺手删掉排障证据。
    #[test]
    fn clearing_cache_keeps_diagnostic_logs() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let log_dir = temp.path().join("logs");
        file(&cache, "audio/song.m4a", 0);
        file(&cache, "covers/cover.img", 0);
        let outlet = crate::log_outlet::LogOutlet::new(log_dir.clone());
        outlet.write_line("diagnostic entry");

        assert_eq!(clear_in(&cache), 2);
        assert!(log_dir.join(crate::log_outlet::LOG_FILE_NAME).exists());
    }

    #[test]
    fn trim_evicts_oldest_pairs_but_not_active_audio() {
        let dir = tempfile::tempdir().unwrap();
        let active = file(dir.path(), "active.m4a", 100);
        file(dir.path(), "active.quality", 100);
        let lease = CacheLease::acquire(&active);
        let old = file(dir.path(), "old.m4a", 50);
        file(dir.path(), "old.quality", 50);
        let recent = file(dir.path(), "recent.m4a", 0);
        file(dir.path(), "recent.quality", 0);
        assert_eq!(trim_audio_in(dir.path(), 40), 40);
        assert!(!old.exists());
        assert!(!old.with_extension("quality").exists());
        assert!(active.exists() && recent.exists());
        assert_eq!(trim_audio_in(dir.path(), 0), 20);
        drop(lease);
        assert_eq!(trim_audio_in(dir.path(), 0), 0);
    }

    #[test]
    fn cloned_and_independent_leases_keep_files_alive() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path(), "a.m4a", 0);
        let first = CacheLease::acquire(&path);
        let clone = first.clone();
        let second = CacheLease::acquire(&path);
        drop(first);
        drop(second);
        assert_eq!(clear_in(dir.path()), 0);
        drop(clone);
        assert_eq!(clear_in(dir.path()), 1);
    }

    #[test]
    fn invalidation_does_not_overlap_a_cache_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path(), "a.m4a", 0);
        let quality = file(dir.path(), "a.quality", 0);
        let lock = download_lock(&path);
        let writer = lock.lock().unwrap();
        invalidate_audio(&path);
        assert!(path.exists() && quality.exists());
        drop(writer);
        invalidate_audio(&path);
        assert!(!path.exists() && !quality.exists());
    }

    #[cfg(unix)]
    #[test]
    fn clear_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = file(outside.path(), "keep.img", 0);
        std::os::unix::fs::symlink(outside.path(), dir.path().join("outside")).unwrap();
        assert_eq!(clear_in(dir.path()), 0);
        assert!(path.exists());
    }
}
