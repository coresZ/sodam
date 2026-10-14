//! 搜索一批曲目并打印各自的取流信息，用于挑一首「能下到整曲」的歌验证下载链路。
//!
//! ```text
//! cargo run -p sodam-core --example stream_probe -- "关键词"
//! ```

use sodam_core::{Session, Settings};

fn main() {
    let query = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "周杰伦".to_string());
    let settings = Settings::load().merged_with_env();
    let session = Session::new(settings.clone());
    let tracks = match session.search(&query) {
        Ok(tracks) => tracks,
        Err(err) => {
            println!("搜索失败：{err}");
            std::process::exit(1);
        }
    };
    println!("搜索「{query}」共 {} 首", tracks.len());
    for track in tracks.iter().take(10) {
        let song = libresoda::Song {
            id: track.id.clone(),
            name: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            duration: track.duration_seconds,
            source: libresoda::model::SOURCE_SODA.to_string(),
            link: format!("https://www.qishui.com/track/{}", track.id),
            extra: std::collections::BTreeMap::from([("track_id".to_string(), track.id.clone())]),
            ..Default::default()
        };
        match session.soda().get_download_info(&song) {
            Ok(info) => println!(
                "{} | {} | vip={} | q={} fmt={} br={} size={} preview={}",
                track.id,
                track.title,
                track.vip,
                info.quality,
                info.format,
                info.bitrate,
                info.size,
                info.is_preview
            ),
            Err(err) => println!("{} | {} | 取流失败：{err}", track.id, track.title),
        }
    }
}
