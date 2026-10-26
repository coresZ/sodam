//! 下载：任务队列、并发调度、落盘与元数据旁路。
//!
//! 设计对齐 `design-plan.md`：
//!
//! * **领域层无 GPUI**：队列与状态机都在这里，UI 只读快照、发命令；
//! * **不阻塞渲染**：网络 / 解密 / 写盘全部在专用线程，调度线程只跑状态机；
//! * **下载音质与播放音质分离**：任务创建时快照档位，中途改设置不影响已在队列的任务；
//! * **临时文件 + 校验 + rename**：中断永远留不下「看起来有效」的半截文件；
//! * **去重**：同一首已在队列（或已下载）时不重复排，批量下载不会排进几百个重复任务。
//!
//! 暂停 / 取消的语义：正在传输的任务会被立刻中止并丢弃半截临时文件，
//! 「继续」从头重下。CDN 是否支持 Range 续传无法保证，与其留下不可靠的断点，
//! 不如让状态机保持简单可预测。

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::download_library::LibraryEntry;
use crate::log_outlet::{log_event, LogLevel, LogTopic};
use crate::models::{LyricLine, TrackItem};
use crate::session::Session;

/// 默认命名模板（对齐设计文档的预设）。
pub const DEFAULT_NAMING: &str = "{artist} - {title}";
/// 命名模板里允许的占位符（设置页展示用）。
pub const NAMING_PRESETS: &[&str] = &[
    "{artist} - {title}",
    "{title}",
    "{album}/{artist} - {title}",
];
/// 文件名兜底（模板整段被清空时用）。
const FALLBACK_STEM: &str = "未知曲目";
/// 单个路径片段的最大字符数：过长的名字在部分文件系统上会直接写失败。
const MAX_SEGMENT_CHARS: usize = 80;
/// 下载文件认识的音频扩展名（探测已存在文件 / 决定容器后缀）。
const AUDIO_EXTENSIONS: &[&str] = &["m4a", "mp4", "flac", "mp3", "wav", "aac"];

/// 目标文件已存在、且无法从文件本身探测出音质时的占位标签。
///
/// 它**不是音质**：写清单、界面展示都要把它当成「没有值」（见 `finish_task` 与
/// `views::downloads`），只是先占着 `quality` 字段的位置。
pub const EXISTING_PLACEHOLDER: &str = "已存在";
/// 封面旁路子目录（`<下载根>/image/`）。
///
/// 旁路固定落在**下载根**的子目录里，不跟随命名模板的 `/` 分层 ——
/// 音乐库软件（foobar2000 / MusicBee 等）默认就往这两个目录找图与词。
const SIDECAR_IMAGE_DIR: &str = "image";
/// 歌词旁路子目录（`<下载根>/lrc/`）。
const SIDECAR_LRC_DIR: &str = "lrc";
/// 下载并发上限（设置页只给 1~3，这里再兜一层）。
const MAX_CONCURRENT: u8 = 3;
/// 单次自动重试之间的退避基数。
const RETRY_BACKOFF: Duration = Duration::from_millis(900);

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

/// 下载相关设置（`config.json` 的 `download` 块）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadSettings {
    /// 下载优先音质：空 / `auto` = 与播放音质一致，其余同 libresoda 档位 key。
    pub quality: String,
    /// 下载根目录；空 = 平台默认「音乐 / SodaM」。
    pub dir: String,
    /// 文件命名模板，支持 `{artist}` / `{title}` / `{album}` 占位符与 `/` 分层。
    pub naming: String,
    /// 是否写封面图（旁路图片，落 `<下载根>/image/`）。
    pub write_cover: bool,
    /// 是否写歌词（旁路 `.lrc`，落 `<下载根>/lrc/`）。
    pub write_lyrics: bool,
    /// 同时下载数（1~3）。
    pub concurrent: u8,
    /// 失败自动重试次数（不含首次）。
    pub retry_count: u8,
    /// 目标文件已存在时是否覆盖（false = 跳过）。
    pub overwrite: bool,
}

impl Default for DownloadSettings {
    fn default() -> Self {
        Self {
            // 默认无损封顶：与设计文档一致，避免下载总去抢 Hi-Res。
            quality: "lossless".to_string(),
            dir: String::new(),
            naming: DEFAULT_NAMING.to_string(),
            write_cover: true,
            write_lyrics: true,
            concurrent: 2,
            retry_count: 2,
            overwrite: false,
        }
    }
}

impl DownloadSettings {
    /// 生效的命名模板（空串回退默认值）。
    pub fn naming(&self) -> &str {
        let naming = self.naming.trim();
        if naming.is_empty() {
            DEFAULT_NAMING
        } else {
            naming
        }
    }

    /// 生效的下载音质 key（空 / `auto` 时回落到播放音质）。
    pub fn quality_for(&self, base: &Settings) -> String {
        let value = self.quality.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("auto") {
            let playback = base.quality.trim();
            if playback.is_empty() {
                "best".to_string()
            } else {
                playback.to_string()
            }
        } else {
            value.to_ascii_lowercase()
        }
    }

    /// 同时下载数（收敛到 1~3）。
    pub fn concurrent(&self) -> u8 {
        self.concurrent.clamp(1, MAX_CONCURRENT)
    }

    /// 自动重试次数（收敛到 0~5，避免把签名服务打爆）。
    pub fn retry_count(&self) -> u8 {
        self.retry_count.min(5)
    }

    /// 下载目录：自定义优先，否则平台默认。
    pub fn resolve_dir(&self) -> PathBuf {
        let custom = self.dir.trim();
        if !custom.is_empty() {
            return PathBuf::from(custom);
        }
        default_download_dir()
    }
}

/// 平台默认下载目录：安装版是「音乐 / SodaM」，便携版是 `<exe 同级>/down`。
///
/// 与播放缓存目录分离：清播放缓存不会删掉下载文件。
pub fn default_download_dir() -> PathBuf {
    crate::paths::default_download_dir()
}

// ---------------------------------------------------------------------------
// 命名模板
// ---------------------------------------------------------------------------

/// 占位符展开为空值时的哨兵字符：用来精确清理「歌手为空」这类模板空洞
/// （`{artist} - {title}` → ` - 晴天`），而不会误伤歌名里本来就有的连字符。
const EMPTY_SENTINEL: char = '\u{1}';

/// 用曲目信息填充命名模板（未识别的占位符原样保留，空字段落哨兵）。
fn fill_placeholders(template: &str, track: &TrackItem) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        let Some(close) = tail.find('}') else {
            out.push_str(tail);
            return out;
        };
        let key = &tail[1..close];
        let value = match key {
            "artist" => track.artist.as_str(),
            "title" => track.title.as_str(),
            "album" => track.album.as_str(),
            "artist_id" => track.artist_id.as_str(),
            "album_id" => track.album_id.as_str(),
            "track_id" | "id" => track.id.as_str(),
            _ => {
                // 不认识的占位符原样保留（不吞掉用户自己写的内容）
                out.push_str(&tail[..=close]);
                rest = &tail[close + 1..];
                continue;
            }
        };
        if value.trim().is_empty() {
            out.push(EMPTY_SENTINEL);
        } else {
            out.push_str(value);
        }
        rest = &tail[close + 1..];
    }
    out.push_str(rest);
    out
}

/// 是否属于「占位符之间的连接符」：空字段清理时连同这些一起吃掉。
fn is_separator(character: char) -> bool {
    character.is_whitespace() || matches!(character, '-' | '–' | '—' | '_' | '.' | ',' | '·')
}

/// 清掉空占位符（哨兵）及其两侧的连接符，返回清理后的字符串。
fn strip_empty_placeholders(filled: &str) -> String {
    let chars: Vec<char> = filled.chars().collect();
    let mut out = String::with_capacity(filled.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != EMPTY_SENTINEL {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        // 吃掉哨兵左侧已经写出去、且全是连接符的那一段
        while out.chars().next_back().map(is_separator).unwrap_or(false) {
            out.pop();
        }
        // 再吃掉右侧紧跟的连接符
        index += 1;
        while index < chars.len() && is_separator(chars[index]) {
            index += 1;
        }
    }
    out
}

/// 清理单个路径片段：去掉非法字符、首尾空白与结尾的点。
///
/// 结尾的点 / 空格在 Windows 上会被静默截掉，导致「写出去的文件名和预期不一致」。
fn sanitize_segment(segment: &str) -> String {
    let mut cleaned = String::with_capacity(segment.len());
    for character in segment.chars() {
        let illegal = matches!(
            character,
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
        ) || character.is_control();
        cleaned.push(if illegal { '_' } else { character });
    }
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(['.', ' ']).trim();
    if trimmed.chars().count() <= MAX_SEGMENT_CHARS {
        return trimmed.to_string();
    }
    trimmed.chars().take(MAX_SEGMENT_CHARS).collect()
}

/// 模板 → 相对路径（不含扩展名）。空片段被丢弃，末段为空时用兜底名。
pub fn render_relative_path(naming: &str, track: &TrackItem) -> PathBuf {
    let template = if naming.trim().is_empty() {
        DEFAULT_NAMING
    } else {
        naming.trim()
    };
    let mut path = PathBuf::new();
    for raw in template.split('/') {
        let cleaned = sanitize_segment(&strip_empty_placeholders(&fill_placeholders(raw, track)));
        if cleaned.is_empty() {
            continue;
        }
        path.push(cleaned);
    }
    let has_file = path
        .file_name()
        .map(|name| !name.is_empty())
        .unwrap_or(false);
    if !has_file {
        path.push(FALLBACK_STEM);
    }
    path
}

/// 已存在的成品文件（按认识的音频扩展名探测；空文件不算）。
fn existing_file(base: &Path) -> Option<PathBuf> {
    for extension in AUDIO_EXTENSIONS {
        let candidate = base.with_extension(extension);
        if let Ok(metadata) = std::fs::metadata(&candidate) {
            if metadata.is_file() && metadata.len() > 0 {
                return Some(candidate);
            }
        }
    }
    None
}

/// 扫到的一个本地音频文件。
#[derive(Clone, Debug, PartialEq)]
pub struct ScannedFile {
    /// 相对根目录、**不含扩展名**（与 `render_relative_path`、清单主键同源）。
    pub relative: PathBuf,
    /// 实际扩展名（`m4a` / `mp4` / `flac` …）。
    pub extension: String,
    pub bytes: u64,
    /// 文件 mtime（unix 秒）；取不到为 `None`。
    pub modified: Option<i64>,
    /// 容器头探测出的时长（秒）；取不到为 `None`。
    ///
    /// 只在**清单里没有该条目、或清单条目缺时长**时探测 —— 有权威值时不必读盘。
    pub duration_seconds: Option<f64>,
}

/// 扫描下载目录里的音频文件，带大小与 mtime。
///
/// 判定规则与 [`existing_file`] 完全一致：扩展名属于 [`AUDIO_EXTENSIONS`] 且文件非空。
/// 调用方用 [`render_relative_path`] 算出目标相对路径后做比对，得到的正是
/// 「点下载会不会说"已存在"」这一语义 —— 界面标记与跳过逻辑因此不可能自相矛盾。
///
/// * **递归**：命名模板允许 `/` 分层，音频可能在任何深度；
/// * **不跟进符号链接 / 目录联接**：避免自引用把扫描绕死，另有深度上限兜底；
/// * `image/` / `lrc/` 里的图片与歌词不是音频扩展名，天然被排除；
/// * 目录不存在时返回空列表（一次都没下载过是常态，不是错误）；
/// * 结果按 mtime **倒序**（新的在前），mtime 取不到时按相对路径 —— 顺序稳定，
///   列表可以直接渲染。
///
/// 纯文件系统操作，**必须在后台线程调用**（见 `AGENTS.md` 第 2 条）。
///
/// 时长探测按需进行：`known_durations` 里已有的（来自清单）不重复读盘。
pub fn scan_local_files(
    settings: &DownloadSettings,
    known_durations: &std::collections::HashMap<PathBuf, f64>,
) -> Vec<ScannedFile> {
    let root = settings.resolve_dir();
    let mut found = Vec::new();
    walk_audio(&root, &root, 0, &mut found, known_durations);
    found.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.relative.cmp(&right.relative))
    });
    found
}

/// 同上，但不探测时长（「已下载」标记这类只看存在性的调用方用）。
pub fn scan_local_files_fast(settings: &DownloadSettings) -> Vec<ScannedFile> {
    scan_local_files(settings, &std::collections::HashMap::new())
}

/// 「存在与否」的轻量视图：与 [`scan_local_files`] 同一次判定，只取相对路径。
///
/// 「已下载」标记与清单校验只需要这个；要展示大小 / 时间就用完整版
/// （一次目录遍历同时喂给两者，别扫两遍）。
pub fn scan_downloaded(settings: &DownloadSettings) -> HashSet<PathBuf> {
    scan_local_files_fast(settings)
        .into_iter()
        .map(|file| file.relative)
        .collect()
}

/// 扫描深度上限：模板分层是人写的，不会有这么深；纯粹防御异常目录树。
const SCAN_MAX_DEPTH: usize = 16;

/// 旁路封面候选扩展名（写侧用魔数嗅探，扩展名不固定）。
const SIDECAR_IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp"];

/// 扫到的旁路文件：按**文件名 stem** 索引（旁路不跟随模板分层，见模块内说明）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScannedSidecars {
    /// 文件名 stem → 封面绝对路径。
    pub covers: std::collections::HashMap<String, PathBuf>,
    /// 文件名 stem → 歌词绝对路径。
    pub lyrics: std::collections::HashMap<String, PathBuf>,
}

/// 扫描 `<下载根>/image/` 与 `<下载根>/lrc/`。
///
/// 与音频扫描分开：旁路目录是**扁平**的（不跟随命名模板分层），
/// 两个 `read_dir` 就够，成本极低。
///
/// 键用**文件名 stem**（不含扩展名）：音频的 `relative` 可能带模板子目录，
/// 但旁路永远落在根目录 —— 这是既有设计的必然结果（见 `MODIFICATIONS.md`）。
///
/// 纯文件系统操作，**必须在后台线程调用**。
pub fn scan_sidecars(settings: &DownloadSettings) -> ScannedSidecars {
    let root = settings.resolve_dir();
    let mut sidecars = ScannedSidecars::default();

    if let Ok(entries) = std::fs::read_dir(root.join(SIDECAR_IMAGE_DIR)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
                continue;
            };
            if !SIDECAR_IMAGE_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
            {
                continue;
            }
            // 同名多扩展名时先到先得（正常只会有一个）。
            sidecars.covers.entry(stem.to_string()).or_insert(path);
        }
    }

    if let Ok(entries) = std::fs::read_dir(root.join(SIDECAR_LRC_DIR)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
                continue;
            };
            if !extension.eq_ignore_ascii_case("lrc") {
                continue;
            }
            sidecars.lyrics.entry(stem.to_string()).or_insert(path);
        }
    }

    sidecars
}

/// 读一份旁路 `.lrc` 并解析成歌词行（复用网络歌词同一套解析器）。
pub fn read_sidecar_lyrics(path: &Path) -> Option<Vec<crate::models::LyricLine>> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines = crate::models::parse_lrc(&text);
    (!lines.is_empty()).then_some(lines)
}

fn walk_audio(
    root: &Path,
    dir: &Path,
    depth: usize,
    found: &mut Vec<ScannedFile>,
    known_durations: &std::collections::HashMap<PathBuf, f64>,
) {
    if depth > SCAN_MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // 符号链接与目录联接一律不跟进：自引用会绕成死循环。
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk_audio(root, &path, depth + 1, found, known_durations);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        if !AUDIO_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
        {
            continue;
        }
        // 一次 stat 同时拿到大小与 mtime。
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        // 空文件不算成品：与 `existing_file` 同规则，被截断的半截文件不冒充已下载。
        if meta.len() == 0 {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.with_extension("");
        // 时长：清单里已有就不读盘；否则探测（MP4 走快速 mvhd 路径）。
        let duration_seconds = match known_durations.get(&relative).copied() {
            Some(seconds) if seconds > 0.0 => Some(seconds),
            _ => crate::audio::probe_duration(&path),
        };
        found.push(ScannedFile {
            // 去掉扩展名，与 `render_relative_path` 的返回值对齐。
            relative,
            extension: extension.to_string(),
            bytes: meta.len(),
            modified: meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|delta| delta.as_secs() as i64),
            duration_seconds,
        });
    }
}

/// 源格式 → 落盘扩展名（不认识时按 m4a 处理，与播放缓存一致）。
fn format_extension(info: &libresoda::soda::types::DownloadInfo) -> &'static str {
    let format = info.format.trim().to_ascii_lowercase();
    // `flac` / `audio/mp4` / `audio/mpeg` 这类都要认：取最后一段再比。
    let tail = format.rsplit('/').next().unwrap_or("").trim();
    let candidate = if tail.is_empty() {
        format.as_str()
    } else {
        tail
    };
    for extension in AUDIO_EXTENSIONS {
        if candidate == *extension {
            return extension;
        }
    }
    if candidate.contains("flac") {
        return "flac";
    }
    if candidate.contains("mpeg") || candidate.contains("mp3") {
        return "mp3";
    }
    if candidate.contains("mp4") || candidate.contains("aac") {
        return "m4a";
    }
    "m4a"
}

// ---------------------------------------------------------------------------
// 失败分类
// ---------------------------------------------------------------------------

/// 单曲下载失败：带「能不能重试」的判定，UI 直接照着展示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadFailure {
    pub message: String,
    pub retryable: bool,
    /// 用户主动中止（暂停 / 取消）导致的停止，不算失败。
    pub stopped: bool,
}

impl DownloadFailure {
    pub fn new(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            message: message.into(),
            retryable,
            stopped: false,
        }
    }

    fn stopped() -> Self {
        Self {
            message: String::new(),
            retryable: false,
            stopped: true,
        }
    }
}

/// IO 错误 → 失败描述：磁盘满 / 无权限给短提示，其余保留系统原文。
fn io_failure(err: std::io::Error) -> DownloadFailure {
    let text = err.to_string();
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("space") {
        return DownloadFailure::new("磁盘空间不足，请清理后重试", false);
    }
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        return DownloadFailure::new(format!("没有写入权限：{text}"), false);
    }
    DownloadFailure::new(format!("写入文件失败：{text}"), true)
}

// ---------------------------------------------------------------------------
// 单曲下载
// ---------------------------------------------------------------------------

/// 一次成功下载的产物。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackDownloadOutcome {
    pub path: PathBuf,
    pub bytes: u64,
    /// 实际拉到的音质（例：`无损 871k`）；跳过已存在文件时为 `已存在`。
    pub quality: String,
    /// 目标文件本来就在（按「已存在」策略跳过，没有走网络）。
    pub skipped: bool,
    pub cover: Option<PathBuf>,
    pub lyrics: Option<PathBuf>,
}

/// 下载单曲：取流 → 落盘 → 解密 → 校验 → 改名 →（可选）封面 / 歌词旁路。
///
/// `stop` 置位时立刻中止并丢弃临时文件；`progress` 收到的是**已写入字节数**。
pub fn download_track(
    base: &Settings,
    download: &DownloadSettings,
    track: &TrackItem,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<TrackDownloadOutcome, DownloadFailure> {
    let dir = download.resolve_dir();
    std::fs::create_dir_all(&dir).map_err(io_failure)?;

    let base_path = dir.join(render_relative_path(download.naming(), track));
    if !download.overwrite {
        if let Some(existing) = existing_file(&base_path) {
            let bytes = std::fs::metadata(&existing)
                .map(|meta| meta.len())
                .unwrap_or(0);
            return Ok(TrackDownloadOutcome {
                path: existing.clone(),
                bytes,
                // 没有走网络、拿不到服务端下发的音质：从文件本身反推一个估值，
                // 探测不出来才退回「已存在」（界面显示为「未知音质」）。
                quality: crate::audio::probe_quality_label(&existing)
                    .unwrap_or_else(|| EXISTING_PLACEHOLDER.to_string()),
                skipped: true,
                cover: None,
                lyrics: None,
            });
        }
    }

    // 下载音质与播放音质分离：任务创建时快照的档位在这里生效。
    let mut settings = base.clone();
    settings.quality = download.quality_for(base);
    let session = Session::new(settings);

    let song = crate::session::song_for(track);
    let info = session
        .soda()
        .get_download_info(&song)
        .map_err(|err| DownloadFailure::new(format!("取流失败：{err}"), true))?;

    if info.url.trim().is_empty() {
        return Err(DownloadFailure::new(
            "取流失败：服务端没有返回可下载地址",
            true,
        ));
    }
    // 试听片段：账号权益 / 签名不足。下载 30 秒残片对用户没有意义，直接失败并说清原因。
    if info.is_preview {
        let note = info.note.trim();
        let message = if note.is_empty() {
            "当前账号仅可试听，无法下载完整曲目".to_string()
        } else {
            format!("当前账号仅可试听，无法下载完整曲目（{note}）")
        };
        return Err(DownloadFailure::new(message, false));
    }

    let extension = format_extension(&info);
    let path = base_path.with_extension(extension);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_failure)?;
    }
    // 临时名带进程号 + 任务序号：并发下载同一首时互不踩踏（与播放缓存同思路）。
    let part = path.with_extension(format!(
        "{extension}.{}-{}.part",
        std::process::id(),
        next_part_seq()
    ));

    let written = stream_to_file(&info, &part, stop, progress)?;
    if is_truncated(info.size, written) {
        let _ = std::fs::remove_file(&part);
        return Err(DownloadFailure::new(
            format!(
                "下载不完整（至少需要 {} 字节，实际 {written} 字节）",
                info.size
            ),
            true,
        ));
    }

    // 加密流（带 play_auth）必须解密后再落盘，否则播放器读不了。
    if !info.play_auth.trim().is_empty() {
        decrypt_in_place(&part, &info.play_auth)?;
    }

    std::fs::rename(&part, &path).map_err(|err| {
        let _ = std::fs::remove_file(&part);
        io_failure(err)
    })?;

    let bytes = std::fs::metadata(&path)
        .map(|meta| meta.len())
        .unwrap_or(written);
    let mut outcome = TrackDownloadOutcome {
        path: path.clone(),
        bytes,
        quality: crate::session::describe_quality(&info),
        skipped: false,
        cover: None,
        lyrics: None,
    };
    // 旁路元数据是「加分项」：失败不影响下载本身，只留调试日志。
    if download.write_cover {
        outcome.cover = write_cover_sidecar(&session, track, &dir, &path);
        if outcome.cover.is_none() {
            log_sidecar("封面", &track.title);
        }
    }
    if download.write_lyrics {
        outcome.lyrics = write_lyrics_sidecar(&session, track, &dir, &path);
        if outcome.lyrics.is_none() {
            log_sidecar("歌词", &track.title);
        }
    }
    Ok(outcome)
}

/// 旁路写入失败只在开启调试日志时提示：缺封面 / 缺歌词属于常态，不该刷屏。
fn log_sidecar(kind: &str, title: &str) {
    log_event(
        LogLevel::Debug,
        LogTopic::Media,
        "sidecar_skipped",
        &[("kind", kind), ("title", title)],
    );
}

/// 进程内临时文件序号。
fn next_part_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// 是否被截断：只看「实际字节数是否少于服务端声明」。
///
/// 与播放缓存同规则：个别曲目的 `size` 元数据滞后于真实资产（实际更多），
/// 那不是截断；真正的风险只有实际更少（连接提前断开的静默 EOF）。
fn is_truncated(declared: i64, written: u64) -> bool {
    declared > 0 && written < declared as u64
}

/// 拉流写临时文件，边写边回报进度。
fn stream_to_file(
    info: &libresoda::soda::types::DownloadInfo,
    part: &Path,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, DownloadFailure> {
    let response = download_agent()
        .get(info.url.trim())
        .set("User-Agent", libresoda::soda::types::USER_AGENT)
        .call()
        .map_err(|err| DownloadFailure::new(format!("请求音频流失败：{err}"), true))?;
    let total = response
        .header("Content-Length")
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .or_else(|| (info.size > 0).then_some(info.size as u64));

    let mut reader = response.into_reader();
    let mut file = std::fs::File::create(part).map_err(io_failure)?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(part);
            return Err(DownloadFailure::stopped());
        }
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(err) => {
                drop(file);
                let _ = std::fs::remove_file(part);
                return Err(DownloadFailure::new(format!("读取音频流失败：{err}"), true));
            }
        };
        if read == 0 {
            break;
        }
        if let Err(err) = file.write_all(&buffer[..read]) {
            drop(file);
            let _ = std::fs::remove_file(part);
            return Err(io_failure(err));
        }
        written += read as u64;
        progress(written, total);
    }
    if let Err(err) = file.flush() {
        drop(file);
        let _ = std::fs::remove_file(part);
        return Err(io_failure(err));
    }
    Ok(written)
}

/// 下载专用 HTTP agent：读超时保证「取消」最迟一次读就能生效，
/// 同时不给整条下载设总时限（大文件在慢网络下需要更久）。
fn download_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(20))
            .timeout_write(Duration::from_secs(20))
            .build()
    })
}

/// 就地解密（`play_auth` 密钥还原 + MP4/CENC 样本解密由 libresoda 负责）。
fn decrypt_in_place(part: &Path, play_auth: &str) -> Result<(), DownloadFailure> {
    let data = std::fs::read(part).map_err(io_failure)?;
    let plain = libresoda::soda::crypto::decrypt_audio(&data, play_auth).map_err(|err| {
        let _ = std::fs::remove_file(part);
        DownloadFailure::new(format!("解密失败：{err}"), true)
    })?;
    std::fs::write(part, plain).map_err(io_failure)
}

// ---------------------------------------------------------------------------
// 旁路元数据（封面 / 歌词）
// ---------------------------------------------------------------------------

/// 图片字节 → 扩展名（封面 CDN 可能给 PNG / WebP，写成 `.jpg` 会误导播放器）。
fn image_extension(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else {
        "jpg"
    }
}

/// 旁路文件目标路径：`<下载根>/<子目录>/<音频文件名>.<扩展名>`。
///
/// 只替换最后一段扩展名（`set_extension`），所以歌名里本来就有的点号
/// （`A - B 1.5`）不会被截断。音频可以按模板 `/` 分层，旁路不跟。
fn sidecar_path(root: &Path, audio: &Path, subdir: &str, extension: &str) -> Option<PathBuf> {
    let name = audio.file_name()?;
    let mut target = root.join(subdir).join(name);
    target.set_extension(extension);
    Some(target)
}

/// 写封面旁路（`<下载根>/image/`）。失败返回 `None`。
fn write_cover_sidecar(
    session: &Session,
    track: &TrackItem,
    root: &Path,
    audio: &Path,
) -> Option<PathBuf> {
    if track.cover.trim().is_empty() {
        return None;
    }
    let cached = session.original_cover_path(&track.cover)?;
    let bytes = std::fs::read(&cached).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let target = sidecar_path(root, audio, SIDECAR_IMAGE_DIR, image_extension(&bytes))?;
    // 新布局比音频多一层目录，不能再指望音频的父目录已经建好。
    std::fs::create_dir_all(target.parent()?).ok()?;
    std::fs::write(&target, bytes).ok()?;
    Some(target)
}

/// 写歌词旁路（`<下载根>/lrc/`）。失败返回 `None`。
fn write_lyrics_sidecar(
    session: &Session,
    track: &TrackItem,
    root: &Path,
    audio: &Path,
) -> Option<PathBuf> {
    let lines = session.lyrics(track).ok()?;
    if lines.is_empty() {
        return None;
    }
    let target = sidecar_path(root, audio, SIDECAR_LRC_DIR, "lrc")?;
    std::fs::create_dir_all(target.parent()?).ok()?;
    std::fs::write(&target, lyrics_to_lrc(&lines)).ok()?;
    Some(target)
}

/// 逐字歌词 → 标准 LRC（行级时间轴，任何播放器都能读）。
pub fn lyrics_to_lrc(lines: &[LyricLine]) -> String {
    let mut out = String::with_capacity(lines.len() * 48);
    for line in lines {
        let text = line.text.trim();
        if text.is_empty() {
            continue;
        }
        let total = line.start_seconds.max(0.0);
        let minutes = (total / 60.0).floor();
        let seconds = total - minutes * 60.0;
        out.push_str(&format!("[{minutes:02}:{seconds:05.2}]"));
        out.push_str(text);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// 任务与状态机
// ---------------------------------------------------------------------------

/// 单个下载任务的状态。
#[derive(Clone, Debug, PartialEq)]
pub enum DownloadState {
    Queued,
    Running {
        downloaded: u64,
        total: Option<u64>,
        /// 字节/秒；还没量出来时为 0。
        speed: f64,
    },
    Paused,
    Completed {
        path: PathBuf,
        bytes: u64,
        quality: String,
        /// 目标文件本来就在（按策略跳过）。
        skipped: bool,
    },
    Failed {
        error: String,
        retryable: bool,
    },
    Cancelled,
}

impl DownloadState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running { .. } | Self::Paused)
    }

    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    /// 是否属于「失败」分区（用户取消也算需要重试的一类）。
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. } | Self::Cancelled)
    }

    /// 进度比例（0.0~1.0）；总长未知时返回 `None`。
    pub fn progress(&self) -> Option<f32> {
        match self {
            Self::Running {
                downloaded, total, ..
            } => {
                let total = (*total)?;
                if total == 0 {
                    return None;
                }
                Some((*downloaded as f32 / total as f32).clamp(0.0, 1.0))
            }
            Self::Completed { .. } => Some(1.0),
            _ => None,
        }
    }
}

/// 下载任务（UI 只读快照）。
#[derive(Clone, Debug)]
pub struct DownloadTask {
    pub id: u64,
    pub track: TrackItem,
    /// 创建时快照的下载音质 key。
    pub quality: String,
    /// 来源（歌单 / 专辑名）；单曲下载为空。
    pub source: String,
    /// 完成后自动打开所在文件夹。
    pub open_when_done: bool,
    pub state: DownloadState,
    pub created_at: Instant,
    pub finished_at: Option<Instant>,
    /// 已尝试次数（含自动重试）。
    pub attempts: u32,
}

/// 入队结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EnqueueReport {
    /// 新增任务数。
    pub added: usize,
    /// 因为已在队列而跳过的数量。
    pub skipped: usize,
}

/// 任务数量统计（侧栏徽章 / 面板标签用）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DownloadCounts {
    pub queued: usize,
    pub running: usize,
    pub paused: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
}

impl DownloadCounts {
    pub fn active(&self) -> usize {
        self.queued + self.running + self.paused
    }

    /// 还在「等着被调度 / 正在跑」的数量（不含暂停）：决定底栏按钮是「全部暂停」还是「全部继续」。
    pub fn pending(&self) -> usize {
        self.queued + self.running
    }

    pub fn total(&self) -> usize {
        self.active() + self.completed + self.failed + self.cancelled
    }
}

/// 下载生命周期事件：UI 消费后给提示 / 打开文件夹。
#[derive(Clone, Debug, PartialEq)]
pub enum DownloadEvent {
    Completed {
        title: String,
        path: PathBuf,
        open_when_done: bool,
    },
    Failed {
        title: String,
        error: String,
    },
}

/// 任务控制句柄（调度线程与任务线程共享）。
#[derive(Default)]
struct TaskFlags {
    /// 要求立刻中止当前传输（暂停 / 取消 / 删除都会置位）。
    stop: AtomicBool,
}

struct State {
    tasks: Vec<DownloadTask>,
    next_id: u64,
    /// 「全部暂停」：新任务不再启动（已在跑的走各自的 stop）。
    paused_all: bool,
}

impl State {
    /// 正在传输的任务数。
    ///
    /// **不用计数器**：暂停 / 取消 / 删除都会在传输途中改写任务状态，
    /// 计数器一旦漏减就再也回不去（额度永久被占，队列卡死）。
    /// 任务数量级在几百以内，直接数一遍最稳。
    fn running(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| matches!(task.state, DownloadState::Running { .. }))
            .count()
    }
}

struct Shared {
    state: Mutex<State>,
    /// 基础设置（Cookie / 签名 / 播放音质）——下载音质在任务里覆盖。
    settings: Mutex<Settings>,
    download: Mutex<DownloadSettings>,
    flags: Mutex<BTreeMap<u64, Arc<TaskFlags>>>,
    events: Mutex<Vec<DownloadEvent>>,
    revision: AtomicU64,
    fingerprint: AtomicU64,
}

impl Shared {
    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    fn flag(&self, id: u64) -> Option<Arc<TaskFlags>> {
        self.flags.lock().ok()?.get(&id).cloned()
    }
}

/// 下载管理器：入队 / 暂停 / 重试 / 查询快照。
///
/// 内部有一个调度线程（只跑状态机，不做 IO）与每任务一个工作线程。
/// 管理器可 `Clone`（内部全是 `Arc`），可以安全地放进 GPUI 的 `Root`。
#[derive(Clone)]
pub struct DownloadManager {
    shared: Arc<Shared>,
}

impl DownloadManager {
    /// 创建管理器并启动调度线程。
    pub fn new(settings: Settings) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                tasks: Vec::new(),
                next_id: 1,
                paused_all: false,
            }),
            fingerprint: AtomicU64::new(settings.fingerprint()),
            settings: Mutex::new(settings),
            download: Mutex::new(DownloadSettings::default()),
            flags: Mutex::new(BTreeMap::new()),
            events: Mutex::new(Vec::new()),
            revision: AtomicU64::new(0),
        });
        spawn_scheduler(Arc::downgrade(&shared));
        Self { shared }
    }

    /// 更新基础设置（登录、换签名服务、改播放音质时调用）。
    pub fn set_settings(&self, settings: Settings) {
        self.shared
            .fingerprint
            .store(settings.fingerprint(), Ordering::Relaxed);
        if let Ok(mut slot) = self.shared.settings.lock() {
            *slot = settings;
        }
    }

    /// 当前基础设置的变更指纹（UI 用它判断要不要推新设置，避免每帧深拷贝）。
    pub fn settings_fingerprint(&self) -> u64 {
        self.shared.fingerprint.load(Ordering::Relaxed)
    }

    /// 更新下载设置（设置页保存时调用）。
    pub fn set_download_settings(&self, settings: DownloadSettings) {
        if let Ok(mut slot) = self.shared.download.lock() {
            *slot = settings;
        }
        self.shared.bump();
    }

    pub fn download_settings(&self) -> DownloadSettings {
        self.shared
            .download
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }

    /// 入队一批曲目（歌单 / 专辑批量下载也走这里）。
    ///
    /// 去重规则：同一首已有「排队中 / 已暂停 / 已完成」任务时跳过；
    /// 上一轮失败或取消过的允许重新排（用户再点一次就是重试的意思）。
    pub fn enqueue(
        &self,
        tracks: &[TrackItem],
        source: &str,
        open_when_done: bool,
    ) -> EnqueueReport {
        let download = self.download_settings();
        let base = self
            .shared
            .settings
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        let quality = download.quality_for(&base);
        let mut report = EnqueueReport::default();
        {
            let Ok(mut state) = self.shared.state.lock() else {
                return report;
            };
            for track in tracks {
                if track.id.trim().is_empty() {
                    report.skipped += 1;
                    continue;
                }
                let duplicate = state.tasks.iter().any(|task| {
                    task.track.id == track.id
                        && !matches!(
                            task.state,
                            DownloadState::Failed { .. } | DownloadState::Cancelled
                        )
                });
                if duplicate {
                    report.skipped += 1;
                    continue;
                }
                let id = state.next_id;
                state.next_id += 1;
                state.tasks.push(DownloadTask {
                    id,
                    track: track.clone(),
                    quality: quality.clone(),
                    source: source.trim().to_string(),
                    open_when_done,
                    state: DownloadState::Queued,
                    created_at: Instant::now(),
                    finished_at: None,
                    attempts: 0,
                });
                if let Ok(mut flags) = self.shared.flags.lock() {
                    flags.insert(id, Arc::new(TaskFlags::default()));
                }
                report.added += 1;
            }
        }
        if report.added > 0 {
            self.shared.bump();
        }
        report
    }

    /// 任务快照（UI 渲染用；数量级在几十到几百，克隆可接受）。
    pub fn tasks(&self) -> Vec<DownloadTask> {
        self.shared
            .state
            .lock()
            .map(|state| state.tasks.clone())
            .unwrap_or_default()
    }

    pub fn counts(&self) -> DownloadCounts {
        let mut counts = DownloadCounts::default();
        let Ok(state) = self.shared.state.lock() else {
            return counts;
        };
        for task in &state.tasks {
            match task.state {
                DownloadState::Queued => counts.queued += 1,
                DownloadState::Running { .. } => counts.running += 1,
                DownloadState::Paused => counts.paused += 1,
                DownloadState::Completed { .. } => counts.completed += 1,
                DownloadState::Failed { .. } => counts.failed += 1,
                DownloadState::Cancelled => counts.cancelled += 1,
            }
        }
        counts
    }

    /// 状态版本号：UI 心跳比对它决定要不要重绘。
    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Relaxed)
    }

    /// 取走待处理事件（提示 / 打开文件夹）。
    pub fn drain_events(&self) -> Vec<DownloadEvent> {
        self.shared
            .events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }

    /// 暂停单个任务（正在传输的立刻中止并丢弃半成品）。
    pub fn pause(&self, id: u64) {
        if self.set_state(id, |task| match task.state {
            DownloadState::Queued | DownloadState::Running { .. } => Some(DownloadState::Paused),
            _ => None,
        }) {
            self.stop_transfer(id);
        }
    }

    /// 继续 / 重试（重新排队，从头下载）。
    ///
    /// 单条「继续」会解除「全部暂停」：用户明确要求这一首跑起来，
    /// 若全局标志还在，任务排进队列后会被调度器一直跳过（点了没反应）。
    pub fn resume(&self, id: u64) {
        let resumed = self.set_state(id, |task| match task.state {
            DownloadState::Paused | DownloadState::Failed { .. } | DownloadState::Cancelled => {
                task.attempts = 0;
                Some(DownloadState::Queued)
            }
            _ => None,
        });
        if resumed {
            if let Ok(mut state) = self.shared.state.lock() {
                state.paused_all = false;
            }
            if let Some(flags) = self.shared.flag(id) {
                flags.stop.store(false, Ordering::Relaxed);
            }
        }
    }

    /// 取消任务（中止传输并丢弃半成品）。
    pub fn cancel(&self, id: u64) {
        if self.set_state(id, |task| match task.state {
            DownloadState::Completed { .. } | DownloadState::Cancelled => None,
            _ => Some(DownloadState::Cancelled),
        }) {
            self.stop_transfer(id);
        }
    }

    /// 从列表里删除任务（运行中的先中止）。
    pub fn remove(&self, id: u64) {
        self.stop_transfer(id);
        {
            let Ok(mut state) = self.shared.state.lock() else {
                return;
            };
            let Some(index) = state.tasks.iter().position(|task| task.id == id) else {
                return;
            };
            state.tasks.remove(index);
        }
        if let Ok(mut flags) = self.shared.flags.lock() {
            flags.remove(&id);
        }
        self.shared.bump();
    }

    /// 全部暂停。
    pub fn pause_all(&self) {
        let ids: Vec<u64> = {
            let Ok(mut state) = self.shared.state.lock() else {
                return;
            };
            state.paused_all = true;
            state
                .tasks
                .iter_mut()
                .filter_map(|task| match task.state {
                    DownloadState::Queued | DownloadState::Running { .. } => {
                        task.state = DownloadState::Paused;
                        Some(task.id)
                    }
                    _ => None,
                })
                .collect()
        };
        for id in ids {
            self.stop_transfer(id);
        }
        self.shared.bump();
    }

    /// 全部继续（失败 / 取消过的也一并重新排队）。
    pub fn resume_all(&self) {
        {
            let Ok(mut state) = self.shared.state.lock() else {
                return;
            };
            state.paused_all = false;
            for task in state.tasks.iter_mut() {
                if matches!(
                    task.state,
                    DownloadState::Paused | DownloadState::Failed { .. } | DownloadState::Cancelled
                ) {
                    task.state = DownloadState::Queued;
                    task.attempts = 0;
                }
            }
        }
        if let Ok(flags) = self.shared.flags.lock() {
            for flag in flags.values() {
                flag.stop.store(false, Ordering::Relaxed);
            }
        }
        self.shared.bump();
    }

    /// 清空已完成 / 已取消 / 失败记录。
    pub fn clear_finished(&self) {
        let removed: Vec<u64> = {
            let Ok(mut state) = self.shared.state.lock() else {
                return;
            };
            let removed: Vec<u64> = state
                .tasks
                .iter()
                .filter(|task| !task.state.is_active())
                .map(|task| task.id)
                .collect();
            state.tasks.retain(|task| task.state.is_active());
            removed
        };
        if removed.is_empty() {
            return;
        }
        if let Ok(mut flags) = self.shared.flags.lock() {
            for id in removed {
                flags.remove(&id);
            }
        }
        self.shared.bump();
    }

    /// 修改任务状态；返回是否真的改动了。
    fn set_state(
        &self,
        id: u64,
        change: impl FnOnce(&mut DownloadTask) -> Option<DownloadState>,
    ) -> bool {
        let changed = {
            let Ok(mut state) = self.shared.state.lock() else {
                return false;
            };
            let Some(task) = state.tasks.iter_mut().find(|task| task.id == id) else {
                return false;
            };
            match change(task) {
                Some(next) => {
                    task.state = next;
                    true
                }
                None => false,
            }
        };
        if changed {
            self.shared.bump();
        }
        changed
    }

    /// 置位中止标志（任务线程会在下一次读取时退出）。
    fn stop_transfer(&self, id: u64) {
        if let Some(flags) = self.shared.flag(id) {
            flags.stop.store(true, Ordering::Relaxed);
        }
    }
}

/// 调度线程：按并发上限把 `Queued` 任务派给工作线程。
///
/// 只持有 `Weak`：所有管理器句柄都释放后线程自行退出，不会把进程钉住。
fn spawn_scheduler(shared: Weak<Shared>) {
    let spawned = std::thread::Builder::new()
        .name("sodam-download".to_string())
        .spawn(move || loop {
            let Some(shared) = shared.upgrade() else {
                return;
            };
            let download = shared
                .download
                .lock()
                .map(|slot| slot.clone())
                .unwrap_or_default();
            let base = shared
                .settings
                .lock()
                .map(|slot| slot.clone())
                .unwrap_or_default();
            let concurrent = download.concurrent() as usize;
            let mut started: Vec<u64> = Vec::new();
            {
                let Ok(mut state) = shared.state.lock() else {
                    return;
                };
                if !state.paused_all {
                    // 先把可调度数量算出来，避免在遍历 `tasks` 时又可变借用整个 `state`。
                    let slots = concurrent.saturating_sub(state.running());
                    for task in state.tasks.iter_mut() {
                        if started.len() >= slots {
                            break;
                        }
                        if matches!(task.state, DownloadState::Queued) {
                            task.state = DownloadState::Running {
                                downloaded: 0,
                                total: None,
                                speed: 0.0,
                            };
                            task.attempts += 1;
                            started.push(task.id);
                        }
                    }
                }
            }
            for id in started {
                let worker = shared.clone();
                let base = base.clone();
                let download = download.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("sodam-download-{id}"))
                    .spawn(move || run_task(worker, id, base, download));
            }
            drop(shared);
            std::thread::sleep(Duration::from_millis(120));
        });
    if spawned.is_err() {
        // 线程创建失败（极端资源紧张）：不 panic，下载功能静默不可用。
        log_event(
            LogLevel::Error,
            LogTopic::App,
            "download_worker_spawn_failed",
            &[],
        );
    }
}

/// 单个任务的执行体：重试 → 进度回报 → 收尾状态。
fn run_task(shared: Arc<Shared>, id: u64, base: Settings, download: DownloadSettings) {
    let Some(flags) = shared.flag(id) else {
        return;
    };
    let Some(task) = shared
        .state
        .lock()
        .ok()
        .and_then(|state| state.tasks.iter().find(|task| task.id == id).cloned())
    else {
        return;
    };

    let mut reporter = ProgressReporter::new(shared.clone(), id);
    let mut last_failure: Option<DownloadFailure> = None;
    let attempts_allowed = download.retry_count() as u32 + 1;
    let mut result: Option<TrackDownloadOutcome> = None;
    for attempt in 0..attempts_allowed {
        if flags.stop.load(Ordering::Relaxed) {
            break;
        }
        let outcome = {
            let mut callback = reporter.callback();
            download_track(&base, &download, &task.track, &flags.stop, &mut callback)
        };
        match outcome {
            Ok(outcome) => {
                result = Some(outcome);
                break;
            }
            Err(failure) => {
                if failure.stopped {
                    last_failure = None;
                    break;
                }
                let retryable = failure.retryable;
                last_failure = Some(failure);
                if !retryable || attempt + 1 >= attempts_allowed {
                    break;
                }
                // 退避期间也要能被「暂停 / 取消」立刻打断。
                let deadline = Instant::now() + RETRY_BACKOFF * (attempt + 1);
                while Instant::now() < deadline {
                    if flags.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(80));
                }
            }
        }
    }

    finish_task(
        &shared,
        id,
        result,
        last_failure,
        flags.stop.load(Ordering::Relaxed),
        &download,
    );
}

/// 进度回报节流：每 150ms 或每 512KB 才写一次共享状态（否则每块都加锁会拖慢下载）。
struct ProgressReporter {
    shared: Arc<Shared>,
    id: u64,
    last_flush: Instant,
    last_flush_bytes: u64,
    speed: f64,
}

impl ProgressReporter {
    fn new(shared: Arc<Shared>, id: u64) -> Self {
        Self {
            shared,
            id,
            last_flush: Instant::now(),
            last_flush_bytes: 0,
            speed: 0.0,
        }
    }

    fn callback(&mut self) -> impl FnMut(u64, Option<u64>) + '_ {
        move |downloaded, total| {
            let elapsed = self.last_flush.elapsed();
            let delta = downloaded.saturating_sub(self.last_flush_bytes);
            if elapsed < Duration::from_millis(150) && delta < 512 * 1024 {
                return;
            }
            // 平滑一下瞬时速度，避免数字乱跳。
            let instant = delta as f64 / elapsed.as_secs_f64().max(0.001);
            self.speed = if self.speed <= 0.0 {
                instant
            } else {
                self.speed * 0.6 + instant * 0.4
            };
            self.last_flush = Instant::now();
            self.last_flush_bytes = downloaded;
            if let Ok(mut state) = self.shared.state.lock() {
                if let Some(task) = state.tasks.iter_mut().find(|task| task.id == self.id) {
                    if matches!(task.state, DownloadState::Running { .. }) {
                        task.state = DownloadState::Running {
                            downloaded,
                            total,
                            speed: self.speed,
                        };
                    }
                }
            }
            self.shared.bump();
        }
    }
}

/// 收尾：写最终状态、发事件、归还并发额度。
fn finish_task(
    shared: &Arc<Shared>,
    id: u64,
    result: Option<TrackDownloadOutcome>,
    failure: Option<DownloadFailure>,
    stopped: bool,
    download: &DownloadSettings,
) {
    let mut completed = None;
    let mut failed = None;
    // 清单写入要落盘，**必须放在状态锁外面做**（占着任务锁做 I/O 会拖住调度器与 UI）。
    let mut entry: Option<LibraryEntry> = None;
    {
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        let Some(index) = state.tasks.iter().position(|task| task.id == id) else {
            // 任务在传输期间被删掉：什么都不用做（没有计数器要归还）。
            return;
        };
        let task = &mut state.tasks[index];
        match result {
            Some(outcome) => {
                task.state = DownloadState::Completed {
                    path: outcome.path.clone(),
                    bytes: outcome.bytes,
                    quality: outcome.quality.clone(),
                    skipped: outcome.skipped,
                };
                task.finished_at = Some(Instant::now());
                // 「本地音乐」页跨重启的依据。
                // 文件本就在（按策略跳过）也记一条 —— 老版本留下的文件借此补进清单。
                // 跳过时 `quality` 是从磁盘反推的估值（探测不出来则是「已存在」占位符，
                // 不能当音质写进去）。
                let extension = outcome
                    .path
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
                    .to_string();
                let quality =
                    (outcome.quality != EXISTING_PLACEHOLDER).then_some(outcome.quality.as_str());
                entry = Some(LibraryEntry::from_download(
                    &task.track,
                    render_relative_path(download.naming(), &task.track),
                    &extension,
                    outcome.bytes,
                    quality,
                    &outcome.path,
                ));
                if !outcome.skipped {
                    completed = Some(DownloadEvent::Completed {
                        title: task.track.title.clone(),
                        path: outcome.path,
                        open_when_done: task.open_when_done,
                    });
                }
            }
            None => {
                // 停止期间用户可能已经改成「暂停 / 取消」：尊重用户意图，不覆盖成失败。
                let paused = matches!(task.state, DownloadState::Paused);
                let cancelled = matches!(task.state, DownloadState::Cancelled);
                if paused || cancelled {
                    task.finished_at = None;
                } else if stopped {
                    task.state = DownloadState::Paused;
                    task.finished_at = None;
                } else {
                    let failure = failure
                        .unwrap_or_else(|| DownloadFailure::new("下载中断（原因未知）", true));
                    task.state = DownloadState::Failed {
                        error: failure.message.clone(),
                        retryable: failure.retryable,
                    };
                    task.finished_at = Some(Instant::now());
                    failed = Some(DownloadEvent::Failed {
                        title: task.track.title.clone(),
                        error: failure.message,
                    });
                }
            }
        }
    }
    // 清单写入失败不影响下载本身（与封面 / 歌词旁路的处理一致）。
    if let Some(entry) = entry {
        if let Err(err) = crate::download_library::upsert(download, entry) {
            let detail = err.to_string();
            log_event(
                LogLevel::Warn,
                LogTopic::App,
                "download_library_upsert_failed",
                &[("error", detail.as_str())],
            );
        }
    }
    if completed.is_some() || failed.is_some() {
        if let Ok(mut events) = shared.events.lock() {
            if let Some(event) = completed {
                events.push(event);
            }
            if let Some(event) = failed {
                events.push(event);
            }
        }
    }
    shared.bump();
}

// ---------------------------------------------------------------------------
// 展示辅助
// ---------------------------------------------------------------------------

/// 字节/秒 → 人类可读速度。
pub fn human_speed(bytes_per_second: f64) -> String {
    let value = bytes_per_second.max(0.0);
    if value >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", value / (1024.0 * 1024.0))
    } else if value >= 1024.0 {
        format!("{:.0} KB/s", value / 1024.0)
    } else if value > 0.0 {
        format!("{value:.0} B/s")
    } else {
        String::new()
    }
}

/// 剩余时间（秒）→ `mm:ss`；速度为 0 或总量未知时返回空串。
pub fn human_eta(remaining_bytes: u64, speed: f64) -> String {
    if speed <= 0.0 || remaining_bytes == 0 {
        return String::new();
    }
    let seconds = (remaining_bytes as f64 / speed).round().max(0.0) as u64;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(artist: &str, title: &str) -> TrackItem {
        TrackItem {
            id: "123456".to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            album: "专辑".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn naming_template_renders_and_sanitizes() {
        let item = track("周杰伦", "晴天");
        assert_eq!(
            render_relative_path("{artist} - {title}", &item),
            PathBuf::from("周杰伦 - 晴天")
        );
        assert_eq!(
            render_relative_path("{title}", &item),
            PathBuf::from("晴天")
        );
        assert_eq!(
            render_relative_path("{album}/{artist} - {title}", &item),
            PathBuf::from("专辑").join("周杰伦 - 晴天")
        );

        // 非法字符与结尾点：Windows 上直接写不出文件
        let dirty = track("AC/DC", "Back: in Black?");
        assert_eq!(
            render_relative_path("{artist} - {title}", &dirty),
            PathBuf::from("AC_DC - Back_ in Black_")
        );
    }

    #[test]
    fn naming_template_handles_empty_fields_and_unknown_placeholders() {
        let mut item = track("", "");
        item.album = String::new();
        // 空片段被丢弃，末段为空时兜底
        assert_eq!(
            render_relative_path("{album}/{artist} - {title}", &item),
            PathBuf::from(FALLBACK_STEM)
        );
        // 不认识的占位符原样保留（不吞掉用户自己写的内容）
        item.artist = "A".into();
        assert_eq!(
            render_relative_path("{artist} {unknown}", &item),
            PathBuf::from("A {unknown}")
        );
    }

    #[test]
    fn naming_template_truncates_long_segments() {
        let long = "长".repeat(200);
        let item = track("A", &long);
        let rendered = render_relative_path("{title}", &item);
        assert_eq!(
            rendered.to_string_lossy().chars().count(),
            MAX_SEGMENT_CHARS
        );
    }

    #[test]
    fn quality_falls_back_to_playback_preference() {
        let base = Settings {
            quality: "highest".into(),
            ..Default::default()
        };
        let settings = DownloadSettings {
            quality: String::new(),
            ..Default::default()
        };
        assert_eq!(settings.quality_for(&base), "highest");
        let settings = DownloadSettings {
            quality: "auto".into(),
            ..Default::default()
        };
        assert_eq!(settings.quality_for(&base), "highest");
        let settings = DownloadSettings {
            quality: "Lossless".into(),
            ..Default::default()
        };
        assert_eq!(settings.quality_for(&base), "lossless");
        // 播放音质也没设置时退回 best
        let settings = DownloadSettings {
            quality: String::new(),
            ..Default::default()
        };
        assert_eq!(settings.quality_for(&Settings::default()), "best");
    }

    #[test]
    fn settings_clamp_ranges() {
        let settings = DownloadSettings {
            concurrent: 9,
            retry_count: 42,
            ..Default::default()
        };
        assert_eq!(settings.concurrent(), MAX_CONCURRENT);
        assert_eq!(settings.retry_count(), 5);
        let settings = DownloadSettings {
            concurrent: 0,
            retry_count: 0,
            ..Default::default()
        };
        assert_eq!(settings.concurrent(), 1);
        assert_eq!(settings.retry_count(), 0);
    }

    #[test]
    fn download_dir_prefers_custom_then_platform_default() {
        let settings = DownloadSettings {
            dir: "  D:/music/sodam  ".into(),
            ..Default::default()
        };
        assert_eq!(settings.resolve_dir(), PathBuf::from("D:/music/sodam"));
        // 没配目录时用平台默认：便携版落在 `<exe 同级>/down`，安装版落在「音乐 / SodaM」
        let settings = DownloadSettings::default();
        let dir = settings.resolve_dir();
        if crate::paths::is_portable() {
            assert!(dir.ends_with("down"), "{dir:?}");
        } else {
            assert!(dir.ends_with("SodaM"), "{dir:?}");
        }
    }

    #[test]
    fn lrc_export_uses_line_timeline() {
        let lines = vec![
            LyricLine {
                start_seconds: 1.2,
                end_seconds: 3.0,
                text: "第一句".into(),
                words: Vec::new(),
            },
            LyricLine {
                start_seconds: 65.44,
                end_seconds: 70.0,
                text: "第二句".into(),
                words: Vec::new(),
            },
            LyricLine {
                start_seconds: 80.0,
                end_seconds: 81.0,
                text: "   ".into(),
                words: Vec::new(),
            },
        ];
        assert_eq!(
            lyrics_to_lrc(&lines),
            "[00:01.20]第一句\n[01:05.44]第二句\n"
        );
    }

    #[test]
    fn sidecar_paths_land_in_root_subdirs() {
        let root = PathBuf::from("D:/music/SodaM");
        let audio = root.join("Artist - Title.flac");

        let cover = sidecar_path(&root, &audio, SIDECAR_IMAGE_DIR, "png").expect("cover");
        assert_eq!(cover, root.join("image").join("Artist - Title.png"));

        let lyrics = sidecar_path(&root, &audio, SIDECAR_LRC_DIR, "lrc").expect("lyrics");
        assert_eq!(lyrics, root.join("lrc").join("Artist - Title.lrc"));

        // 音频按命名模板分了子目录时，旁路仍然固定在下载根，不跟随分层
        let nested = root.join("某专辑").join("Artist - Title.flac");
        let nested_lyrics = sidecar_path(&root, &nested, SIDECAR_LRC_DIR, "lrc").expect("lyrics");
        assert_eq!(nested_lyrics, root.join("lrc").join("Artist - Title.lrc"));
        assert!(!nested_lyrics.to_string_lossy().contains("某专辑"));

        // 歌名里本来就有点号时，只替换真正的音频扩展名
        let dotted = root.join("A - B 1.5.m4a");
        let dotted_cover = sidecar_path(&root, &dotted, SIDECAR_IMAGE_DIR, "jpg").expect("cover");
        assert_eq!(dotted_cover, root.join("image").join("A - B 1.5.jpg"));
    }

    #[test]
    fn scan_downloaded_agrees_with_the_skip_semantics() {
        let root = std::env::temp_dir().join(format!("sodam-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("某专辑")).expect("mkdir");
        std::fs::create_dir_all(root.join(SIDECAR_IMAGE_DIR)).expect("mkdir");
        std::fs::create_dir_all(root.join(SIDECAR_LRC_DIR)).expect("mkdir");

        // 认得的音频：根目录一个、模板分层目录一个
        std::fs::write(root.join("Artist - Title.m4a"), b"x").expect("write");
        std::fs::write(root.join("某专辑").join("A - B.flac"), b"x").expect("write");
        // 空文件不算（与 existing_file 同规则）
        std::fs::write(root.join("Empty.m4a"), []).expect("write");
        // 旁路与非音频不算
        std::fs::write(
            root.join(SIDECAR_IMAGE_DIR).join("Artist - Title.png"),
            b"x",
        )
        .expect("write");
        std::fs::write(root.join(SIDECAR_LRC_DIR).join("Artist - Title.lrc"), b"x").expect("write");
        std::fs::write(root.join("notes.txt"), b"x").expect("write");

        let settings = DownloadSettings {
            dir: root.to_string_lossy().to_string(),
            ..Default::default()
        };
        let found = scan_downloaded(&settings);

        assert!(
            found.contains(&PathBuf::from("Artist - Title")),
            "{found:?}"
        );
        assert!(
            found.contains(&PathBuf::from("某专辑").join("A - B")),
            "{found:?}"
        );
        assert!(
            !found.contains(&PathBuf::from("Empty")),
            "空文件不该算已下载"
        );
        assert!(
            !found.contains(&PathBuf::from("notes")),
            "非音频不该算已下载"
        );
        assert_eq!(found.len(), 2, "{found:?}");

        // 关键不变式：render_relative_path 的输出能直接拿来查这个集合
        let track = track("Artist", "Title");
        let expected = render_relative_path(settings.naming(), &track);
        assert!(found.contains(&expected), "{expected:?} 应命中");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_local_files_reports_extension_size_and_mtime() {
        let root = std::env::temp_dir().join(format!("sodam-scan-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("某专辑")).expect("mkdir");
        std::fs::write(root.join("A.m4a"), b"1234").expect("write");
        std::fs::write(root.join("某专辑").join("B.flac"), b"12").expect("write");

        let settings = DownloadSettings {
            dir: root.to_string_lossy().to_string(),
            ..Default::default()
        };
        let files = scan_local_files_fast(&settings);
        assert_eq!(files.len(), 2, "{files:?}");

        let a = files
            .iter()
            .find(|file| file.relative.as_path() == Path::new("A"))
            .expect("A 应在结果里");
        assert_eq!(a.extension, "m4a");
        assert_eq!(a.bytes, 4);
        assert!(a.modified.is_some(), "本地音乐列表需要 mtime");

        let b = files
            .iter()
            .find(|file| file.relative == PathBuf::from("某专辑").join("B"))
            .expect("分层目录里的 B 应在结果里");
        assert_eq!(b.extension, "flac");
        assert_eq!(b.bytes, 2);

        // 轻量版必须与完整版看到同一批文件（否则标记与列表会自相矛盾）
        let light = scan_downloaded(&settings);
        assert_eq!(light.len(), files.len());
        assert!(light.contains(&PathBuf::from("A")));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn image_extension_sniffs_magic() {
        assert_eq!(image_extension(&[0x89, b'P', b'N', b'G', 0x0D]), "png");
        assert_eq!(image_extension(&[0xFF, 0xD8, 0xFF]), "jpg");
        assert_eq!(
            image_extension(b"RIFF____WEBPVP8 "),
            "webp",
            "WebP 需要同时看 RIFF 与 WEBP 标记"
        );
        assert_eq!(image_extension(b"unknown"), "jpg");
    }

    #[test]
    fn truncation_check_only_rejects_short_writes() {
        assert!(!is_truncated(0, 10));
        assert!(!is_truncated(100, 100));
        assert!(!is_truncated(100, 120));
        assert!(is_truncated(100, 99));
    }

    #[test]
    fn format_extension_keeps_source_container() {
        let info = |format: &str| libresoda::soda::types::DownloadInfo {
            format: format.to_string(),
            ..Default::default()
        };
        assert_eq!(format_extension(&info("m4a")), "m4a");
        assert_eq!(format_extension(&info("FLAC")), "flac");
        assert_eq!(format_extension(&info("audio/mp4")), "mp4");
        assert_eq!(format_extension(&info("mp3")), "mp3");
        assert_eq!(format_extension(&info("")), "m4a");
        assert_eq!(format_extension(&info("weird")), "m4a");
    }

    #[test]
    fn finishing_a_download_writes_the_library_manifest() {
        // 「重启不再丢」的命门：下载完成必须落一条清单，且能还原成可重下的记录。
        let dir = std::env::temp_dir().join(format!("sodam-dl-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        let download = DownloadSettings {
            dir: dir.display().to_string(),
            ..Default::default()
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                tasks: Vec::new(),
                next_id: 3,
                paused_all: false,
            }),
            settings: Mutex::new(Settings::default()),
            download: Mutex::new(download.clone()),
            flags: Mutex::new(BTreeMap::new()),
            events: Mutex::new(Vec::new()),
            revision: AtomicU64::new(0),
            fingerprint: AtomicU64::new(0),
        });

        let first = track("歌手", "晴天");
        let relative = render_relative_path(download.naming(), &first);
        let path = dir.join(relative.clone()).with_extension("m4a");
        std::fs::write(&path, b"audio").expect("write");
        shared.state.lock().expect("lock").tasks.push(DownloadTask {
            id: 1,
            track: first,
            quality: "lossless".into(),
            source: String::new(),
            open_when_done: false,
            state: DownloadState::Running {
                downloaded: 5,
                total: Some(5),
                speed: 0.0,
            },
            created_at: Instant::now(),
            finished_at: None,
            attempts: 1,
        });

        finish_task(
            &shared,
            1,
            Some(TrackDownloadOutcome {
                path: path.clone(),
                bytes: 5,
                quality: "无损 871k".into(),
                skipped: false,
                cover: None,
                lyrics: None,
            }),
            None,
            false,
            &download,
        );

        let entries = crate::download_library::load(&download);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].relative, relative);
        assert_eq!(entries[0].bytes, 5);
        assert_eq!(entries[0].quality.as_deref(), Some("无损 871k"));
        assert_eq!(entries[0].title.as_deref(), Some("晴天"));
        assert!(
            entries[0].to_track().is_some(),
            "下载来的条目应能还原成 TrackItem"
        );

        // 模拟重启：重新读清单 + 扫盘校验，这条应当仍在「本地音乐」里
        let reloaded = crate::download_library::load(&download);
        let outcome = crate::download_library::reconcile(&download, reloaded);
        assert_eq!(outcome.present.len(), 1);
        assert!(outcome.missing.is_empty());

        // 「已存在（跳过）」也要落账（老版本留下的文件借此补进清单），
        // 但质量是未知的 —— 不能把「已存在」当成音质写进去。
        let second = track("歌手", "另一首");
        let second_relative = render_relative_path(download.naming(), &second);
        let second_path = dir.join(second_relative.clone()).with_extension("m4a");
        std::fs::write(&second_path, b"audio").expect("write");
        shared.state.lock().expect("lock").tasks.push(DownloadTask {
            id: 2,
            track: second,
            quality: "lossless".into(),
            source: String::new(),
            open_when_done: false,
            state: DownloadState::Queued,
            created_at: Instant::now(),
            finished_at: None,
            attempts: 0,
        });
        finish_task(
            &shared,
            2,
            Some(TrackDownloadOutcome {
                path: second_path,
                bytes: 5,
                quality: "已存在".into(),
                skipped: true,
                cover: None,
                lyrics: None,
            }),
            None,
            false,
            &download,
        );

        let entries = crate::download_library::load(&download);
        assert_eq!(entries.len(), 2, "{entries:?}");
        let skipped = entries
            .iter()
            .find(|entry| entry.relative == second_relative)
            .expect("跳过的那条也应在清单里");
        assert_eq!(
            skipped.quality, None,
            "跳过的条目质量未知，不能写「已存在」"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn human_speed_and_eta_format() {
        assert_eq!(human_speed(1536.0), "2 KB/s");
        assert_eq!(human_speed(3.0 * 1024.0 * 1024.0), "3.0 MB/s");
        assert_eq!(human_speed(0.0), "");
        assert_eq!(human_eta(1024 * 1024, 512.0 * 1024.0), "00:02");
        assert_eq!(human_eta(1024, 0.0), "");
    }

    #[test]
    fn state_helpers_classify_categories() {
        assert!(DownloadState::Queued.is_active());
        assert!(DownloadState::Paused.is_active());
        assert!(DownloadState::Running {
            downloaded: 1,
            total: None,
            speed: 0.0
        }
        .is_active());
        assert!(DownloadState::Completed {
            path: PathBuf::from("a"),
            bytes: 1,
            quality: "无损".into(),
            skipped: false
        }
        .is_finished());
        assert!(DownloadState::Failed {
            error: "x".into(),
            retryable: true
        }
        .is_failed());
        assert!(DownloadState::Cancelled.is_failed());
    }

    #[test]
    fn progress_fraction_needs_total() {
        assert_eq!(
            DownloadState::Running {
                downloaded: 50,
                total: Some(100),
                speed: 1.0
            }
            .progress(),
            Some(0.5)
        );
        assert_eq!(
            DownloadState::Running {
                downloaded: 50,
                total: None,
                speed: 1.0
            }
            .progress(),
            None
        );
    }

    #[test]
    fn existing_file_probe_ignores_empty_and_partials() {
        let dir = std::env::temp_dir().join(format!("sodam-dl-exists-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let base = dir.join("歌 - 名");

        std::fs::write(base.with_extension("m4a"), []).expect("write empty");
        assert!(existing_file(&base).is_none(), "空文件不算已下载");
        std::fs::write(base.with_extension("m4a.part"), b"partial").expect("write part");
        assert!(existing_file(&base).is_none(), "半成品不算已下载");
        std::fs::write(base.with_extension("flac"), b"done").expect("write flac");
        assert_eq!(existing_file(&base), Some(base.with_extension("flac")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 队列去重：重复入队跳过；失败 / 取消过的允许重新排。
    #[test]
    fn enqueue_dedupes_and_allows_retry_after_failure() {
        let manager = DownloadManager::new(Settings::default());
        let item = track("A", "B");

        let report = manager.enqueue(std::slice::from_ref(&item), "", false);
        assert_eq!(
            report,
            EnqueueReport {
                added: 1,
                skipped: 0
            }
        );
        let report = manager.enqueue(std::slice::from_ref(&item), "", false);
        assert_eq!(
            report,
            EnqueueReport {
                added: 0,
                skipped: 1
            }
        );

        // 空 id 直接跳过
        let mut blank = item.clone();
        blank.id = "  ".into();
        let report = manager.enqueue(std::slice::from_ref(&blank), "", false);
        assert_eq!(
            report,
            EnqueueReport {
                added: 0,
                skipped: 1
            }
        );

        // 失败之后允许重新入队（用户再点一次 = 重试）
        let id = manager.tasks()[0].id;
        manager.cancel(id);
        let report = manager.enqueue(std::slice::from_ref(&item), "", false);
        assert_eq!(
            report,
            EnqueueReport {
                added: 1,
                skipped: 0
            }
        );
    }

    /// 暂停 / 继续 / 取消 / 清空的状态流转（不依赖网络）。
    #[test]
    fn task_lifecycle_transitions() {
        let manager = DownloadManager::new(Settings::default());
        let item = track("A", "B");
        manager.enqueue(std::slice::from_ref(&item), "歌单", true);
        let id = manager.tasks()[0].id;

        // 任务会因为「无 Cookie / 无签名」很快失败，但状态机本身可独立验证：
        manager.pause(id);
        assert_eq!(manager.tasks()[0].state, DownloadState::Paused);
        assert_eq!(manager.counts().paused, 1);

        manager.resume(id);
        assert_eq!(manager.tasks()[0].state, DownloadState::Queued);
        assert_eq!(manager.tasks()[0].attempts, 0);

        manager.cancel(id);
        assert_eq!(manager.tasks()[0].state, DownloadState::Cancelled);
        assert!(manager.counts().failed + manager.counts().cancelled >= 1);

        manager.clear_finished();
        assert!(manager.tasks().is_empty());
        assert_eq!(manager.counts().total(), 0);
    }

    /// 下载设置里的音质快照会写进任务。
    #[test]
    fn enqueue_snapshots_quality() {
        let manager = DownloadManager::new(Settings::default());
        manager.set_download_settings(DownloadSettings {
            quality: "highest".into(),
            ..Default::default()
        });
        manager.enqueue(&[track("A", "B")], "", false);
        assert_eq!(manager.tasks()[0].quality, "highest");
        // 之后再改设置不影响已在队列的任务
        manager.set_download_settings(DownloadSettings {
            quality: "low".into(),
            ..Default::default()
        });
        assert_eq!(manager.tasks()[0].quality, "highest");
    }

    /// 并发额度必须是「数出来的」，不是「记账记出来的」。
    ///
    /// 回归用例：暂停一个正在传输的任务后，若额度靠计数器维护，
    /// 这一格就再也回不来 —— 队列会在剩余任务上永久卡死（实测踩过）。
    #[test]
    fn pausing_a_running_task_frees_its_slot() {
        let manager = DownloadManager::new(Settings::default());
        manager.set_download_settings(DownloadSettings {
            concurrent: 1,
            retry_count: 0,
            // 输出到临时目录，避免污染真实音乐目录
            dir: std::env::temp_dir()
                .join(format!("sodam-dl-slot-{}", std::process::id()))
                .display()
                .to_string(),
            ..Default::default()
        });
        let tracks: Vec<TrackItem> = (0..3)
            .map(|index| TrackItem {
                id: format!("slot-{index}"),
                title: format!("曲目 {index}"),
                artist: "测试".into(),
                ..Default::default()
            })
            .collect();
        manager.enqueue(&tracks, "", false);

        // 等到有一个任务真的进入 Running（调度线程 120ms 一拍）
        let running = wait_for(Duration::from_secs(5), || {
            manager
                .tasks()
                .iter()
                .find(|task| matches!(task.state, DownloadState::Running { .. }))
                .map(|task| task.id)
        });
        let Some(id) = running else {
            // 极端情况下（无网络 / 立刻失败）也不该把测试判成失败
            return;
        };
        manager.pause(id);

        // 暂停后额度必须立刻可用：下一个任务要能在短时间内被调度起来。
        let next = wait_for(Duration::from_secs(5), || {
            manager
                .tasks()
                .iter()
                .find(|task| task.id != id && matches!(task.state, DownloadState::Running { .. }))
                .map(|task| task.id)
        });
        assert!(
            next.is_some(),
            "暂停正在传输的任务后，并发额度没有释放，队列卡死：{:?}",
            manager.tasks()
        );
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("sodam-dl-slot-{}", std::process::id())),
        );
    }

    /// 全部暂停后，单条「继续」必须能真的跑起来。
    ///
    /// 回归用例：`paused_all` 是全局闸门，单条继续若不清掉它，
    /// 任务会被调度器一直跳过（用户点了「继续」但没有任何反应）。
    #[test]
    fn single_resume_clears_the_global_pause() {
        let manager = DownloadManager::new(Settings::default());
        manager.enqueue(&[track("A", "B")], "", false);
        let id = manager.tasks()[0].id;

        manager.pause_all();
        assert_eq!(manager.tasks()[0].state, DownloadState::Paused);
        assert!(
            manager.counts().pending() == 0,
            "全部暂停后不该有待调度任务"
        );

        manager.resume(id);
        assert_eq!(manager.tasks()[0].state, DownloadState::Queued);
        // 单条继续 = 用户明确要求它跑起来：全局闸门必须一起打开
        assert!(
            manager
                .tasks()
                .iter()
                .any(|task| task.state == DownloadState::Queued),
            "单条继续没有解除全局暂停"
        );
    }

    /// 轮询等待一个条件成立（有超时上限，避免测试挂死）。
    fn wait_for<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(value) = probe() {
                return Some(value);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
