//! 验证下载管线（不播放声音）：取流 → 落盘 → 解密 → 校验 → 改名 →（可选）封面 / 歌词旁路。
//!
//! ```text
//! SODA_COOKIE=... QISHUI_SIGNER_URL=... QISHUI_SIGNER_TOKEN=... \
//!   cargo run -p sodam-core --example download_probe -- <track_id> [输出目录]
//! ```
//!
//! 不带 Cookie 也能跑：免费曲目走明文流，VIP 曲目会得到明确的「仅可试听」失败。
//! 传 `--search <关键词>` 会先搜索取真实元数据（歌名 / 歌手 / 封面），
//! 这样能一并验证命名模板与封面 / 歌词旁路。

use std::sync::atomic::AtomicBool;

use sodam_core::download::{download_track, DownloadSettings};
use sodam_core::models::TrackItem;
use sodam_core::Settings;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let base = Settings::load().merged_with_env();
    let session = sodam_core::Session::new(base.clone());

    // 取曲目元数据：`--search <关键词>` 用真实数据，否则用占位元数据。
    let (track, dir) = match args.first().map(String::as_str) {
        Some("--search") => {
            let query = args.get(1).cloned().unwrap_or_else(|| "纯音乐".to_string());
            let tracks = match session.search(&query) {
                Ok(tracks) if !tracks.is_empty() => tracks,
                Ok(_) => {
                    println!("搜索「{query}」没有结果");
                    std::process::exit(1);
                }
                Err(err) => {
                    println!("搜索失败：{err}");
                    std::process::exit(1);
                }
            };
            let track = tracks[0].clone();
            println!(
                "使用搜索结果：{} - {}（id={}，vip={}，封面={}）",
                track.artist,
                track.title,
                track.id,
                track.vip,
                if track.cover.trim().is_empty() {
                    "(无)"
                } else {
                    "(有)"
                }
            );
            let dir = args.get(2).cloned();
            (track, dir)
        }
        _ => {
            let track_id = args
                .first()
                .cloned()
                .unwrap_or_else(|| "7304719759323923497".to_string());
            let track = TrackItem {
                id: track_id.clone(),
                title: format!("probe-{track_id}"),
                artist: "probe".to_string(),
                album: "probe".to_string(),
                ..Default::default()
            };
            (track, args.get(1).cloned())
        }
    };

    let dir = dir.unwrap_or_else(|| {
        std::env::temp_dir()
            .join("sodam-download-probe")
            .display()
            .to_string()
    });

    println!(
        "signer={} cookie={} 播放音质={}",
        if base.signer_url.trim().is_empty() {
            "(空)"
        } else {
            "(已配置)"
        },
        if base.cookie.trim().is_empty() {
            "(空)"
        } else {
            "(已配置)"
        },
        if base.quality.trim().is_empty() {
            "auto"
        } else {
            base.quality.trim()
        }
    );

    let download = DownloadSettings {
        dir: dir.clone(),
        write_cover: true,
        write_lyrics: true,
        overwrite: true,
        ..Default::default()
    };
    println!(
        "下载目录={} 音质={} 命名={}",
        download.resolve_dir().display(),
        download.quality_for(&base),
        download.naming()
    );
    println!(
        "目标相对路径={}",
        sodam_core::download::render_relative_path(download.naming(), &track).display()
    );

    let stop = AtomicBool::new(false);
    let mut last_report = 0u64;
    let mut progress = |written: u64, total: Option<u64>| {
        // 每 1MB 打一行，避免刷屏
        if written / (1024 * 1024) != last_report / (1024 * 1024) {
            last_report = written;
            println!(
                "  进度 {:.1} MB{}",
                written as f64 / (1024.0 * 1024.0),
                total
                    .map(|total| format!(" / {:.1} MB", total as f64 / (1024.0 * 1024.0)))
                    .unwrap_or_default()
            );
        }
    };

    match download_track(&base, &download, &track, &stop, &mut progress) {
        Ok(outcome) => {
            println!(
                "✅ 下载完成：{} ({} 字节, 音质={}{})",
                outcome.path.display(),
                outcome.bytes,
                outcome.quality,
                if outcome.skipped {
                    ", 已存在跳过"
                } else {
                    ""
                }
            );
            println!(
                "   封面：{}",
                outcome
                    .cover
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "(无)".to_string())
            );
            println!(
                "   歌词：{}",
                outcome
                    .lyrics
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "(无)".to_string())
            );
            // 目录清单：根（音频）+ image/ + lrc/ 三个位置，一并确认没有 .part 残留
            let root = download.resolve_dir();
            for sub in ["", "image", "lrc"] {
                let label = if sub.is_empty() { "根" } else { sub };
                let dir = if sub.is_empty() {
                    root.clone()
                } else {
                    root.join(sub)
                };
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    println!("   {label}/：不存在");
                    continue;
                };
                let mut names: Vec<String> = entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect();
                names.sort();
                println!("   {label}/：{names:?}");
                assert!(
                    !names.iter().any(|name| name.ends_with(".part")),
                    "下载完成后不应残留 .part 临时文件"
                );
            }
        }
        Err(failure) => {
            println!(
                "❌ 下载失败（可重试={}）：{}",
                failure.retryable, failure.message
            );
            if failure.stopped {
                println!("   （用户主动中止）");
            }
            std::process::exit(1);
        }
    }
}
