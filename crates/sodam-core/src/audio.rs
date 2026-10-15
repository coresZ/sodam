//! 播放引擎：用 rodio 播放 libresoda 解密后的本地文件。
//!
//! 设计：`OutputStream` 在 rodio 0.20 里是 `!Send`，所以**音频对象全部留在专属线程**，
//! UI 侧只通过命令通道操作、通过 `Arc<Mutex<PlaybackSnapshot>>` 读状态。
//!
//! 没有可用音频设备时**不 panic**：把错误写进快照，界面照常能跑（用户可能没接耳机）。

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use rodio::Source;

use crate::models::TrackItem;

/// 播放器状态快照（UI 每帧读这个，加锁时间极短）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaybackSnapshot {
    pub track_id: String,
    pub title: String,
    pub playing: bool,
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub volume: f32,
    /// 当前曲目**实际拉到的流**音质（例：`无损 871k`）。
    pub quality: String,
    /// 最近一次错误（设备不可用、解码失败…），UI 直接展示
    pub error: String,
    /// 当前曲目是否已播完（UI 据此自动切下一首）
    pub finished: bool,
    /// 播完序号：每播完一次 +1。UI 用它做「处理过了吗」的判断 ——
    /// 之前用 track_id 比较，遇到循环同一首或 id 重复就会漏切。
    pub finished_seq: u64,
}

impl PlaybackSnapshot {
    /// 播放进度比例（0.0~1.0），时长未知时返回 0。
    pub fn progress_fraction(&self) -> f32 {
        if self.duration_seconds <= 0.0 {
            return 0.0;
        }
        (self.position_seconds / self.duration_seconds).clamp(0.0, 1.0) as f32
    }

    /// 当前播放位置 `mm:ss`。
    pub fn position_label(&self) -> String {
        format_seconds(self.position_seconds)
    }

    /// 总时长 `mm:ss`。
    pub fn duration_label(&self) -> String {
        format_seconds(self.duration_seconds)
    }

    /// `mm:ss / mm:ss` 形式的进度文案。
    pub fn progress_label(&self) -> String {
        format!("{} / {}", self.position_label(), self.duration_label())
    }
}

enum Command {
    Load {
        track: Box<TrackItem>,
        path: PathBuf,
        /// 这次实际拉到的流音质（显示用）
        quality: String,
    },
    Toggle,
    Play,
    Pause,
    Stop,
    SetVolume(f32),
    /// 跳转到指定秒数
    Seek(f64),
}

/// 播放引擎句柄（可以在任意线程调用，内部只有发命令与读快照）。
pub struct PlaybackEngine {
    tx: Sender<Command>,
    snapshot: Arc<Mutex<PlaybackSnapshot>>,
}

impl Default for PlaybackEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackEngine {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        // 测试 / 无人值守挂机：`SODAM_VOLUME=0` 可让实例起来就是静音。
        let initial_volume = std::env::var("SODAM_VOLUME")
            .ok()
            .and_then(|value| value.trim().parse::<f32>().ok())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        let snapshot = Arc::new(Mutex::new(PlaybackSnapshot {
            volume: initial_volume,
            ..Default::default()
        }));
        let state = snapshot.clone();
        let spawned = std::thread::Builder::new()
            .name("sodam-audio".to_string())
            .spawn(move || audio_thread(rx, state));
        if let Err(err) = spawned {
            if let Ok(mut snap) = snapshot.lock() {
                snap.error = format!("音频线程启动失败：{err}");
            }
        }
        Self { tx, snapshot }
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.snapshot
            .lock()
            .map(|snap| snap.clone())
            .unwrap_or_default()
    }

    /// 播放一个**已经下载并解密**到本地的文件。
    /// 装载并播放：`quality` 是这次实际拉到的流音质（显示用）。
    pub fn load(&self, track: TrackItem, path: PathBuf, quality: impl Into<String>) {
        let _ = self.tx.send(Command::Load {
            track: Box::new(track),
            path,
            quality: quality.into(),
        });
    }

    pub fn toggle(&self) {
        let _ = self.tx.send(Command::Toggle);
    }

    pub fn play(&self) {
        let _ = self.tx.send(Command::Play);
    }

    pub fn pause(&self) {
        let _ = self.tx.send(Command::Pause);
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Command::Stop);
    }

    pub fn set_volume(&self, volume: f32) {
        let _ = self.tx.send(Command::SetVolume(volume.clamp(0.0, 1.0)));
    }

    /// 跳转到指定秒数（越界会被引擎裁剪到 [0, 时长]）。
    pub fn seek(&self, seconds: f64) {
        let _ = self.tx.send(Command::Seek(seconds.max(0.0)));
    }
}

fn set_error(state: &Arc<Mutex<PlaybackSnapshot>>, message: String) {
    if let Ok(mut snap) = state.lock() {
        snap.error = message;
        snap.playing = false;
    }
}

fn sync_progress(state: &Arc<Mutex<PlaybackSnapshot>>, player: &rodio::Player) {
    if let Ok(mut snap) = state.lock() {
        snap.position_seconds = player.get_pos().as_secs_f64();
        snap.playing = !player.is_paused() && !player.empty();
    }
}

fn audio_thread(rx: Receiver<Command>, state: Arc<Mutex<PlaybackSnapshot>>) {
    // 打不开输出设备就把错误记在快照里，后续命令照常消费（界面不崩）
    let output = match rodio::DeviceSinkBuilder::open_default_sink() {
        Ok(device) => Some(device),
        Err(err) => {
            set_error(&state, format!("音频设备不可用：{err}"));
            None
        }
    };

    let mut player: Option<rodio::Player> = None;
    // 当前曲目是否已经上报过「播完」；换曲时重置
    let mut finished_reported = false;
    loop {
        // 为什么要超时 recv：进度必须**持续**更新（UI 的进度条依赖它）。
        // 之前只在 play/pause 时同步一次，所以进度条永远停在 0。
        let command = match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(command) => Some(command),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let Some(command) = command else {
            if let Some(current) = &player {
                sync_progress(&state, current);
                // 播完：置 finished，UI 会据此自动切下一首
                // 注意：sync_progress 会把 playing 置 false，所以这里不能用 playing 判断，
                // 必须用本地的 finished_reported 标志（否则自动下一首永远不触发）。
                if current.empty() && !finished_reported {
                    finished_reported = true;
                    if let Ok(mut snap) = state.lock() {
                        snap.playing = false;
                        snap.finished = true;
                        snap.finished_seq = snap.finished_seq.wrapping_add(1);
                    }
                }
            }
            continue;
        };
        match command {
            Command::Load {
                track,
                path,
                quality,
            } => {
                if let Some(previous) = player.take() {
                    previous.stop();
                }
                let Some(device) = &output else {
                    set_error(&state, "音频设备不可用，无法播放".to_string());
                    continue;
                };
                let file = match std::fs::File::open(&path) {
                    Ok(file) => file,
                    Err(err) => {
                        set_error(&state, format!("打开音频文件失败：{err}"));
                        continue;
                    }
                };
                let decoder = match rodio::Decoder::new(std::io::BufReader::new(file)) {
                    Ok(decoder) => decoder,
                    Err(err) => {
                        set_error(&state, format!("解码失败（文件可能未解密）：{err}"));
                        continue;
                    }
                };
                // 时长：优先用**容器头探测**的真实值。
                //
                // 下载目录里的文件（尤其用户手动放进去的）`track.duration_seconds`
                // 往往是 0，之前直接把它写进快照 → 播放页显示 `--:--`、进度条分母是 0。
                // `Decoder` 的 `total_duration()` 只读容器头（MP4 的 mvhd / FLAC 的
                // STREAMINFO / MP3 的 Xing 等），不解码音频，开销可忽略。
                let probed_duration = decoder
                    .total_duration()
                    .map(|duration| duration.as_secs_f64())
                    .filter(|seconds| *seconds > 0.0);
                let new_player = rodio::Player::connect_new(device.mixer());
                let volume = state.lock().map(|snap| snap.volume).unwrap_or(1.0);
                new_player.set_volume(volume);
                new_player.append(decoder);
                new_player.play();
                if let Ok(mut snap) = state.lock() {
                    snap.track_id = track.id.clone();
                    snap.title = track.title.clone();
                    snap.playing = true;
                    snap.position_seconds = 0.0;
                    // 探测值优先；取不到（个别流没有时长头）才回退到元数据里的值。
                    snap.duration_seconds =
                        probed_duration.unwrap_or_else(|| track.duration_seconds.max(0) as f64);
                    snap.quality = quality.clone();
                    snap.error.clear();
                    snap.finished = false;
                    finished_reported = false;
                }
                player = Some(new_player);
            }
            Command::Toggle => {
                if let Some(current) = &player {
                    if current.is_paused() {
                        current.play();
                    } else {
                        current.pause();
                    }
                    sync_progress(&state, current);
                } else {
                    set_error(&state, "还没有可播放的曲目".to_string());
                }
            }
            Command::Play => {
                if let Some(current) = &player {
                    current.play();
                    sync_progress(&state, current);
                }
            }
            Command::Pause => {
                if let Some(current) = &player {
                    current.pause();
                    sync_progress(&state, current);
                }
            }
            Command::Stop => {
                if let Some(current) = player.take() {
                    current.stop();
                }
                if let Ok(mut snap) = state.lock() {
                    snap.playing = false;
                    snap.position_seconds = 0.0;
                }
            }
            Command::Seek(seconds) => {
                if let Some(current) = &player {
                    if current
                        .try_seek(std::time::Duration::from_secs_f64(seconds))
                        .is_ok()
                    {
                        // 跳转后进度要立刻反映出来（避免界面回跳）
                        if let Ok(mut snap) = state.lock() {
                            snap.position_seconds = seconds;
                        }
                    }
                }
            }
            Command::SetVolume(volume) => {
                if let Some(current) = &player {
                    current.set_volume(volume);
                }
                if let Ok(mut snap) = state.lock() {
                    snap.volume = volume;
                }
            }
        }
        if let Some(current) = &player {
            sync_progress(&state, current);
        }
    }
}

/// 秒 → `mm:ss`。
fn format_seconds(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// 音频缓存目录。
pub fn cache_dir() -> std::path::PathBuf {
    crate::paths::cache_dir().join("audio")
}

/// 探测音频文件的时长（秒）。取不到返回 `None`。
///
/// 两条路径，按开销从低到高：
///
/// 1. **MP4 系（m4a / mp4）手写 mvhd 解析**：只读文件头部若干 KB，微秒级。
///    下载目录里绝大多数是这个容器（服务端无损就是 FLAC-in-MP4）。
/// 2. **其它容器交给 rodio**：`Decoder::total_duration()` 通常也只读头部，
///    但 MP3 没有 Xing 头时会退化成全文件扫描 —— 所以调用方要按需使用，
///    不要在扫描热路径上对未知格式无脑调用。
///
/// 纯文件 I/O，**必须在后台线程调用**（见 `AGENTS.md` 第 2 条）。
pub fn probe_duration(path: &std::path::Path) -> Option<f64> {
    if let Some(seconds) = probe_mp4_duration(path) {
        return Some(seconds);
    }
    let file = std::fs::File::open(path).ok()?;
    let decoder = rodio::Decoder::new(std::io::BufReader::new(file)).ok()?;
    let duration = decoder.total_duration()?.as_secs_f64();
    (duration > 0.0).then_some(duration)
}

/// MP4 系容器的时长：定位 `moov/mvhd`，按 `duration / timescale` 算。
///
/// 手写而不是走 rodio，是因为下载目录里这是**绝大多数**文件的格式，
/// 而扫描要对每个文件都探测一次 —— 省掉一次解码器构造有意义。
///
/// 只读文件头部（`moov` 在流式 MP4 里通常前置）；找不到就返回 `None`
/// 让上层回退到 rodio。
fn probe_mp4_duration(path: &std::path::Path) -> Option<f64> {
    use std::io::Read;

    const SCAN_LIMIT: usize = 2 * 1024 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; SCAN_LIMIT];
    let read = file.read(&mut buffer).ok()?;
    buffer.truncate(read);
    if buffer.len() < 32 || &buffer[4..8] != b"ftyp" {
        return None;
    }
    // 在头部窗口里找 `mvhd`（box 嵌套，直接扫签名比走 box 树简单且够用）。
    let signature = b"mvhd";
    let position = buffer.windows(4).position(|window| window == signature)?;
    let body = buffer.get(position + 4..)?;
    let version = *body.first()?;
    // `body` 从 `mvhd` 四字节类型之后开始，即 body[0] = version。
    // v0: version(1) flags(3) creation(4) modification(4) timescale(4) duration(4)
    // v1: version(1) flags(3) creation(8) modification(8) timescale(4) duration(8)
    let (timescale, duration) = if version == 1 {
        let timescale = u32::from_be_bytes(body.get(20..24)?.try_into().ok()?);
        let duration = u64::from_be_bytes(body.get(24..32)?.try_into().ok()?);
        (timescale, duration)
    } else {
        let timescale = u32::from_be_bytes(body.get(12..16)?.try_into().ok()?);
        let duration = u32::from_be_bytes(body.get(16..20)?.try_into().ok()?) as u64;
        (timescale, duration)
    };
    if timescale == 0 || duration == 0 {
        return None;
    }
    Some(duration as f64 / timescale as f64)
}

/// 封面缓存目录。
pub fn cover_cache_dir() -> std::path::PathBuf {
    crate::paths::cache_dir().join("covers")
}

/// 目录占用：**递归**统计（字节数，文件数）。
///
/// 封面缩略图在 `covers/`，原图在 `covers/original/` 子目录 ——
/// 只看顶层会把整个子目录漏掉（实测踩过：字节/张数都偏小）。
fn dir_usage(dir: &std::path::Path) -> (u64, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut bytes = 0u64;
    let mut files = 0usize;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file() {
            bytes += meta.len();
            files += 1;
        } else if meta.is_dir() {
            let (sub_bytes, sub_files) = dir_usage(&entry.path());
            bytes += sub_bytes;
            files += sub_files;
        }
    }
    (bytes, files)
}

/// 顶层目录里指定扩展名的文件数（用于「歌曲数」：只数 `.m4a`）。
fn count_extension(dir: &std::path::Path, extension: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.path().extension().map(|ext| ext == extension) == Some(true))
        .count()
}

/// 缓存统计：(音频字节, 歌曲数, 封面字节, 封面张数)。
///
/// * 字节数含**全部**文件（`.m4a` / `.quality` 边车 / 下载中的 `.part` /
///   封面原图子目录）；
/// * 「歌曲数」只数 `.m4a`：边车和临时文件不是歌，全算会把数量翻倍（踩过）；
/// * 「封面张数」数全部图片缓存文件（含原图）。
///
/// 注意：这是同步扫盘，别在渲染路径里每帧调 —— 调用方应后台执行并缓存结果。
pub fn cache_stats() -> (u64, usize, u64, usize) {
    let (audio_bytes, _) = dir_usage(&cache_dir());
    let songs = count_extension(&cache_dir(), "m4a");
    let (cover_bytes, cover_files) = dir_usage(&cover_cache_dir());
    (audio_bytes, songs, cover_bytes, cover_files)
}

/// 清空缓存（音频 + 封面），返回删除的文件数。
///
/// **递归删除**：封面原图在 `covers/original/` 子目录里，只删顶层会把它们漏掉
/// —— 而 [`cache_stats`] 是递归统计的，清完占用数字不变就是这里对不上。
/// 单个文件删不掉（被占用 / 权限）不影响其它文件，静默跳过。
pub fn clear_cache() -> usize {
    let mut removed = 0usize;
    for dir in [cache_dir(), cover_cache_dir()] {
        removed += remove_files_recursive(&dir);
    }
    removed
}

/// 删除目录（含子目录）里的**文件**，返回删除成功数；目录本身保留。
///
/// 用 `DirEntry::file_type()` 判定（不跟进符号链接）：指向目录的符号链接不会被
/// 递归进去，而是当成普通文件删掉链接本身。
fn remove_files_recursive(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            removed += remove_files_recursive(&path);
        } else if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_label_formats() {
        let snap = PlaybackSnapshot {
            position_seconds: 65.4,
            duration_seconds: 236.6,
            ..Default::default()
        };
        assert_eq!(snap.progress_label(), "01:05 / 03:56");
    }

    #[test]
    fn engine_starts_without_audio_device_and_reports_state() {
        // 没有音频设备时也应该能创建引擎（错误写进快照，不 panic）
        let engine = PlaybackEngine::new();
        let snap = engine.snapshot();
        assert_eq!(snap.volume, 1.0);
        assert!(!snap.playing);
    }

    #[test]
    fn dir_usage_and_count_extension_classify_files_correctly() {
        let dir = std::env::temp_dir().join(format!("sodam-cache-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("original")).expect("mkdir");
        std::fs::write(dir.join("a.m4a"), [0u8; 100]).expect("m4a");
        std::fs::write(dir.join("a.quality"), b"lossless\t100").expect("sidecar");
        std::fs::write(dir.join("b.part"), [0u8; 50]).expect("part");
        std::fs::write(dir.join("original/full.img"), [0u8; 25]).expect("cover");

        // 字节：递归含子目录，全部文件都算（100 + 12 + 50 + 25）
        let (bytes, files) = dir_usage(&dir);
        assert_eq!(bytes, 187, "应含子目录与边车/part");
        assert_eq!(files, 4);
        // 歌曲数：只数 .m4a（边车/part 不算歌）
        assert_eq!(count_extension(&dir, "m4a"), 1);
        assert_eq!(count_extension(&dir, "img"), 0, "只看顶层");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 构造一个最小 MP4 头：`ftyp` + `moov` 里一个 `mvhd`。
    ///
    /// 字节布局对齐真实下载文件（实测 `Yang.m4a` 的头部）：
    /// `mvhd` 之后依次是 version(1) flags(3) creation(4) modification(4)
    /// timescale(4) duration(4)。
    fn fake_mp4(timescale: u32, duration: u32, version: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0, 0, 0, 0x1c]); // ftyp box size
        bytes.extend_from_slice(b"ftypisom");
        bytes.extend_from_slice(&[0, 0, 0, 0]); // 补足 ftyp（真实文件更长，够用即可）
        bytes.extend_from_slice(&[0, 0, 0, 0x6c]); // moov box size
        bytes.extend_from_slice(b"moov");
        bytes.extend_from_slice(&[0, 0, 0, 0x6c]); // mvhd box size
        bytes.extend_from_slice(b"mvhd");
        bytes.push(version);
        bytes.extend_from_slice(&[0, 0, 0]); // flags
        if version == 1 {
            bytes.extend_from_slice(&[0u8; 8]); // creation_time
            bytes.extend_from_slice(&[0u8; 8]); // modification_time
            bytes.extend_from_slice(&timescale.to_be_bytes());
            bytes.extend_from_slice(&(duration as u64).to_be_bytes());
        } else {
            bytes.extend_from_slice(&[0u8; 4]); // creation_time
            bytes.extend_from_slice(&[0u8; 4]); // modification_time
            bytes.extend_from_slice(&timescale.to_be_bytes());
            bytes.extend_from_slice(&duration.to_be_bytes());
        }
        bytes
    }

    #[test]
    fn probe_mp4_duration_reads_the_mvhd() {
        let dir = std::env::temp_dir().join(format!("sodam-probe-mp4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("sample.m4a");
        // 与实测文件同构：timescale=1000、duration=193493 → 193.493 秒。
        std::fs::write(&path, fake_mp4(1000, 193493, 0)).expect("write");

        let seconds = probe_mp4_duration(&path).expect("应解析出时长");
        assert!(
            (seconds - 193.493).abs() < 0.01,
            "期望 193.493 秒，实际 {seconds}"
        );

        // v1 头（64 位时长）也要认。
        let path_v1 = dir.join("sample-v1.m4a");
        std::fs::write(&path_v1, fake_mp4(1000, 193493, 1)).expect("write");
        let seconds_v1 = probe_mp4_duration(&path_v1).expect("v1 应解析出时长");
        assert!((seconds_v1 - 193.493).abs() < 0.01, "实际 {seconds_v1}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_mp4_duration_rejects_non_mp4() {
        let dir = std::env::temp_dir().join(format!("sodam-probe-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("not-mp4.bin");
        std::fs::write(&path, b"RIFF____WAVEfmt ").expect("write");
        assert!(probe_mp4_duration(&path).is_none(), "非 MP4 不该给结果");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
