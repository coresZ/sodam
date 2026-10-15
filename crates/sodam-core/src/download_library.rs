//! 本地音乐库：下载清单的读写与校验。
//!
//! 下载管理器里的任务列表是**内存**的，进程退出即消失；这个模块负责持久化的那一段：
//! 「本地音乐」页面上的条目跨重启存在，靠的就是这里。
//!
//! 三个设计决定值得先说清：
//!
//! * **主键是相对路径，不是 `track_id`**。将来要支持扫描用户自己的本地文件夹
//!   （见 `docs/ROADMAP.md`），那些文件没有汽水 `track_id`；用 `track_id` 做主键
//!   会逼出一套额外的映射。用路径做主键，两种来源天然进同一张表。
//! * **元数据全部 `Option`**。本应用下载的条目字段齐全，扫描来的可能只知道
//!   `title/artist`（甚至只有文件名）。
//! * **清单放在下载目录里**，因为它记的是相对该目录的路径；换目录即换库。
//!
//! 清单只是"记账"，**真相在磁盘上**：启动时用 [`reconcile`] 拿 [`scan_downloaded`]
//! 的结果校验，文件没了的条目不冒充存在。

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::download::{scan_downloaded, DownloadSettings};
use crate::models::TrackItem;

/// 清单文件名（放在下载目录根部）。
pub const MANIFEST_FILE: &str = ".sodam-downloads.json";
/// 清单格式版本；将来改结构时靠它做迁移。
const MANIFEST_VERSION: u32 = 1;
/// 原子写的临时文件序号（同一下载目录可能并发写）。
const TMP_PREFIX: &str = ".sodam-downloads";

/// 条目来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrySource {
    /// 本应用下载完成。当前阶段只有这一种。
    Downloaded,
    /// 本地扫描到的文件（后续阶段启用）。
    Scanned,
}

/// 一条本地音乐记录。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// 主键：相对根目录的路径，**不含扩展名**（与 `render_relative_path` 同源）。
    pub relative: PathBuf,
    /// 实际落盘扩展名（`m4a` / `mp4` / `flac` …）。
    pub extension: String,
    pub source: EntrySource,
    /// 文件字节数（落盘时记下，页面直接显示，不必每次 stat）。
    pub bytes: u64,
    /// 入库时间（unix 秒）。
    pub added_at: i64,
    /// 实际音质描述（例：`无损 871k`）；扫描来的文件可能没有。
    #[serde(default)]
    pub quality: Option<String>,
    /// 下载完成时间（unix 秒）。
    #[serde(default)]
    pub downloaded_at: Option<i64>,
    /// 文件 mtime：后续阶段做增量扫描的判据。
    #[serde(default)]
    pub mtime: Option<i64>,
    // ---- 以下为曲目元数据；下载来的齐全，扫描来的可能缺 ----
    #[serde(default)]
    pub track_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub artist_id: Option<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub duration_seconds: Option<i64>,
    #[serde(default)]
    pub vip: Option<bool>,
}

impl LibraryEntry {
    /// 由一次成功下载构造。`path` 是落盘后的真实文件（用来取 mtime）。
    ///
    /// `quality` 用 `Option`：文件本来就在（按策略跳过）时质量未知，
    /// 宁可留空也不要写「已存在」这种占位串 —— 那会被当成真实音质显示。
    pub fn from_download(
        track: &TrackItem,
        relative: PathBuf,
        extension: &str,
        bytes: u64,
        quality: Option<&str>,
        path: &Path,
    ) -> Self {
        let now = now_seconds();
        Self {
            relative,
            extension: extension.to_string(),
            source: EntrySource::Downloaded,
            bytes,
            added_at: now,
            quality: quality.map(str::to_string),
            downloaded_at: Some(now),
            mtime: modified_seconds(path),
            track_id: Some(track.id.clone()),
            title: Some(track.title.clone()),
            artist: Some(track.artist.clone()),
            album: Some(track.album.clone()),
            artist_id: Some(track.artist_id.clone()),
            album_id: Some(track.album_id.clone()),
            cover: Some(track.cover.clone()),
            duration_seconds: Some(track.duration_seconds),
            vip: Some(track.vip),
        }
    }

    /// 还原成 [`TrackItem`]，供「重新下载」与播放使用。
    ///
    /// 缺必需字段（扫描来的文件常见）时返回 `None` —— 调用方据此禁用重下，
    /// 而不是拿空 id 去请求接口。
    pub fn to_track(&self) -> Option<TrackItem> {
        Some(TrackItem {
            id: self.track_id.clone()?,
            title: self.title.clone().unwrap_or_default(),
            artist: self.artist.clone().unwrap_or_default(),
            album: self.album.clone().unwrap_or_default(),
            artist_id: self.artist_id.clone().unwrap_or_default(),
            album_id: self.album_id.clone().unwrap_or_default(),
            cover: self.cover.clone().unwrap_or_default(),
            duration_seconds: self.duration_seconds.unwrap_or(0),
            vip: self.vip.unwrap_or(false),
        })
    }

    /// 页面展示用标题；缺元数据时退化成文件名。
    pub fn display_title(&self) -> String {
        match self.title.as_deref().map(str::trim) {
            Some(title) if !title.is_empty() => title.to_string(),
            _ => self
                .relative
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| self.relative.to_string_lossy().to_string()),
        }
    }

    /// 页面展示用歌手；缺时给占位（英文界面用 `未知歌手` 的词典项）。
    pub fn display_artist(&self) -> String {
        self.artist
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("未知歌手")
            .to_string()
    }
}

/// 清单文件（带版本号）。
#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    entries: Vec<LibraryEntry>,
}

/// 清单在磁盘上的位置。
pub fn manifest_path(settings: &DownloadSettings) -> PathBuf {
    settings.resolve_dir().join(MANIFEST_FILE)
}

/// 读取清单。
///
/// 文件缺失、内容损坏、版本不认识 —— **一律返回空集合**而不是报错：
/// 清单只是记账，读不出来不该让「本地音乐」页或整个启动流程挂掉。
pub fn load(settings: &DownloadSettings) -> Vec<LibraryEntry> {
    let Ok(text) = std::fs::read_to_string(manifest_path(settings)) else {
        return Vec::new();
    };
    let Ok(manifest) = serde_json::from_str::<Manifest>(&text) else {
        return Vec::new();
    };
    if manifest.version != MANIFEST_VERSION {
        return Vec::new();
    }
    manifest.entries
}

/// 原子写入：先写临时文件再 rename，避免中途崩溃留下半截 JSON。
///
/// 与下载落盘同思路（`download.rs` 的 `.part` + rename）。
pub fn save(settings: &DownloadSettings, entries: &[LibraryEntry]) -> std::io::Result<()> {
    let dir = settings.resolve_dir();
    std::fs::create_dir_all(&dir)?;
    let target = manifest_path(settings);
    let tmp = dir.join(format!(
        "{TMP_PREFIX}.{}-{}.tmp",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        entries: entries.to_vec(),
    };
    let text = serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".to_string());
    let write = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        // 显式 flush：rename 之前必须确保内容真的落到了文件里。
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// 清单读改写全程串行化：并发完成的任务不会互相覆盖。
///
/// 每次 upsert 都重新读一遍清单（而不是进程内缓存）：一次下载要好几秒，
/// 而清单的读写只有毫秒级，用简单换正确。
static MANIFEST_LOCK: Mutex<()> = Mutex::new(());
static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 追加或更新一条（按 `relative` 去重）。返回写入后的条目数。
pub fn upsert(settings: &DownloadSettings, entry: LibraryEntry) -> std::io::Result<usize> {
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let mut entries = load(settings);
    match entries
        .iter_mut()
        .find(|existing| existing.relative == entry.relative)
    {
        Some(existing) => {
            // 覆盖时保留原入库时间：`upsert` 的语义是"这条现在是这样"，不是"新加一条"。
            let added_at = existing.added_at;
            *existing = entry;
            existing.added_at = added_at;
        }
        None => entries.push(entry),
    }
    let count = entries.len();
    save(settings, &entries)?;
    Ok(count)
}

/// 按相对路径移除条目；返回是否真的删掉了。
pub fn remove(settings: &DownloadSettings, relative: &Path) -> std::io::Result<bool> {
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let mut entries = load(settings);
    let before = entries.len();
    entries.retain(|entry| entry.relative != relative);
    if entries.len() == before {
        return Ok(false);
    }
    save(settings, &entries)?;
    Ok(true)
}

/// 校验结果。
pub struct ReconcileOutcome {
    /// 文件确实还在的条目。
    pub present: Vec<LibraryEntry>,
    /// 清单里有、磁盘上已经没有的（幽灵）；页面把它们归到「待重下」。
    pub missing: Vec<LibraryEntry>,
}

/// 用扫盘结果把幽灵条目分出来。
///
/// 复用 [`scan_downloaded`]（与下载侧的"已存在即跳过"是同一套判定），
/// 所以"清单说在、下载说不在"这种自相矛盾不会出现。
pub fn reconcile(settings: &DownloadSettings, entries: Vec<LibraryEntry>) -> ReconcileOutcome {
    reconcile_with(&scan_downloaded(settings), entries)
}

/// 同 [`reconcile`]，但复用调用方已经扫好的集合。
///
/// 「已下载」标记和本地音乐库都依赖同一份扫盘结果；让调用方扫一次传进来，
/// 比各自扫一遍目录省一半 I/O（大目录 / 网络盘上差别明显）。
pub fn reconcile_with(on_disk: &HashSet<PathBuf>, entries: Vec<LibraryEntry>) -> ReconcileOutcome {
    let mut present = Vec::with_capacity(entries.len());
    let mut missing = Vec::new();
    for entry in entries {
        if on_disk.contains(&entry.relative) {
            present.push(entry);
        } else {
            missing.push(entry);
        }
    }
    ReconcileOutcome { present, missing }
}

/// 当前 unix 秒。
fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or(0)
}

/// 文件 mtime（unix 秒）；取不到返回 `None`。
fn modified_seconds(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let delta = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(delta.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用独立目录，避免相互踩踏。
    fn temp_settings(tag: &str) -> (DownloadSettings, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "sodam-library-{tag}-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let settings = DownloadSettings {
            dir: dir.to_string_lossy().to_string(),
            ..Default::default()
        };
        (settings, dir)
    }

    fn sample(title: &str, relative: &str) -> LibraryEntry {
        LibraryEntry {
            relative: PathBuf::from(relative),
            extension: "m4a".into(),
            source: EntrySource::Downloaded,
            bytes: 1024,
            added_at: 100,
            quality: Some("无损 871k".into()),
            downloaded_at: Some(100),
            mtime: Some(100),
            track_id: Some(format!("id-{title}")),
            title: Some(title.into()),
            artist: Some("歌手".into()),
            album: Some("专辑".into()),
            artist_id: Some("a1".into()),
            album_id: Some("al1".into()),
            cover: Some("http://cover".into()),
            duration_seconds: Some(240),
            vip: Some(false),
        }
    }

    #[test]
    fn missing_manifest_reads_as_empty() {
        let (settings, dir) = temp_settings("missing");
        assert!(load(&settings).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_manifest_reads_as_empty_instead_of_panicking() {
        let (settings, dir) = temp_settings("corrupt");
        std::fs::write(manifest_path(&settings), b"{ this is not json").expect("write");
        assert!(load(&settings).is_empty(), "损坏的清单应退化成空，而不是崩");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_version_reads_as_empty() {
        let (settings, dir) = temp_settings("version");
        std::fs::write(
            manifest_path(&settings),
            br#"{"version": 999, "entries": []}"#,
        )
        .expect("write");
        assert!(load(&settings).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_round_trips() {
        let (settings, dir) = temp_settings("roundtrip");
        let entries = vec![sample("A", "歌手 - A"), sample("B", "专辑/B")];
        save(&settings, &entries).expect("save");

        let loaded = load(&settings);
        assert_eq!(loaded, entries);
        // 版本号必须写进文件，否则以后没法迁移
        let raw = std::fs::read_to_string(manifest_path(&settings)).expect("read");
        assert!(raw.contains("\"version\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_dedupes_by_relative_path_and_keeps_added_at() {
        let (settings, dir) = temp_settings("upsert");
        upsert(&settings, sample("A", "同名")).expect("first");
        assert_eq!(load(&settings).len(), 1);

        // 同一个相对路径再写一次：只更新，不新增
        let mut again = sample("A2", "同名");
        again.added_at = 999;
        again.bytes = 2048;
        assert_eq!(upsert(&settings, again).expect("second"), 1);

        let loaded = load(&settings);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title.as_deref(), Some("A2"));
        assert_eq!(loaded[0].bytes, 2048);
        assert_eq!(loaded[0].added_at, 100, "入库时间不该被覆盖");

        // 不同路径则是新增
        assert_eq!(upsert(&settings, sample("B", "另一个")).expect("third"), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_reports_whether_it_actually_removed() {
        let (settings, dir) = temp_settings("remove");
        upsert(&settings, sample("A", "甲")).expect("a");
        upsert(&settings, sample("B", "乙")).expect("b");

        assert!(remove(&settings, Path::new("甲")).expect("remove hit"));
        assert_eq!(load(&settings).len(), 1);
        assert!(!remove(&settings, Path::new("不存在")).expect("remove miss"));
        assert_eq!(load(&settings).len(), 1, "未命中不该改动文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_splits_present_from_missing() {
        let (settings, dir) = temp_settings("reconcile");
        // 「在」的条目对应磁盘上真有文件；「没了」的没有
        std::fs::write(dir.join("在.m4a"), b"x").expect("write");

        let entries = vec![sample("在", "在"), sample("没了", "没了")];
        let outcome = reconcile(&settings, entries);

        assert_eq!(outcome.present.len(), 1);
        assert_eq!(outcome.present[0].relative, PathBuf::from("在"));
        assert_eq!(outcome.missing.len(), 1);
        assert_eq!(outcome.missing[0].relative, PathBuf::from("没了"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_finds_nested_and_ignores_sidecars() {
        let (settings, dir) = temp_settings("nested");
        std::fs::create_dir_all(dir.join("某专辑")).expect("mkdir");
        std::fs::create_dir_all(dir.join("image")).expect("mkdir");
        std::fs::write(dir.join("某专辑").join("A - B.flac"), b"x").expect("write");
        // 旁路文件不该被当成音频
        std::fs::write(dir.join("image").join("A - B.png"), b"x").expect("write");

        let outcome = reconcile(&settings, vec![sample("A - B", "某专辑/A - B")]);
        assert_eq!(outcome.present.len(), 1, "分层目录里的音频应被认出来");
        assert!(outcome.missing.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_library_reconciles_without_touching_disk() {
        let (settings, dir) = temp_settings("empty");
        let outcome = reconcile(&settings, Vec::new());
        assert!(outcome.present.is_empty());
        assert!(outcome.missing.is_empty());
        assert!(!manifest_path(&settings).exists(), "只是校验，不该建清单");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn to_track_round_trips_and_refuses_incomplete_entries() {
        let entry = sample("晴天", "周杰伦 - 晴天");
        let track = entry.to_track().expect("完整条目应能还原");
        assert_eq!(track.id, "id-晴天");
        assert_eq!(track.title, "晴天");
        assert_eq!(track.duration_seconds, 240);

        // 扫描来的文件没有 track_id：必须拒绝还原，而不是拿空 id 去请求接口
        let mut scanned = sample("本地歌", "本地歌");
        scanned.track_id = None;
        scanned.source = EntrySource::Scanned;
        assert!(scanned.to_track().is_none());
    }

    #[test]
    fn display_helpers_fall_back_to_file_name() {
        let mut entry = sample("晴天", "周杰伦 - 晴天");
        entry.title = None;
        entry.artist = None;
        assert_eq!(entry.display_title(), "周杰伦 - 晴天");
        assert_eq!(entry.display_artist(), "未知歌手");

        let mut blank = sample("晴天", "周杰伦 - 晴天");
        blank.title = Some("   ".into());
        assert_eq!(blank.display_title(), "周杰伦 - 晴天", "空白标题也要退化");
    }
}
