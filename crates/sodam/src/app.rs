//! 应用根视图：状态定义、初始化、输入处理与顶层渲染。

#[path = "app_actions.rs"]
mod actions;
#[path = "app_download.rs"]
mod download_actions;
#[path = "app_search.rs"]
mod search_actions;

use gpui::prelude::*;
use gpui::{div, px, Context, EntityInputHandler, FocusHandle, Render, UTF16Selection, Window};
use sodam_core::{
    library::{RecommendationSource, SceneCard, SearchScope},
    login::{LoginStatus, QrMatrix},
    models::{
        AlbumDetail, ArtistDetail, LyricLine, PlaylistItem, SceneItem, SearchResults, TrackItem,
    },
    queue::Queue,
    session::{AccountInfo, Session},
    PlayReporter, PlaybackEngine, Settings,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sodam_core::download::{DownloadCounts, DownloadManager, DownloadTask};

/// 封面请求池：有界并发 + 去重 + 队列上限。
///
/// 直接对整页每张封面发请求（尤其是 300 首的歌单）会把网络和 CDN 打爆，
/// 也会让后台线程池占满。池子只保持 `MAX_CONCURRENCY` 个在飞请求，
/// 其余排队；渲染永远只读缓存。
pub(crate) struct CoverPool {
    queue: VecDeque<String>,
    inflight: usize,
    seen: HashSet<String>,
}

impl CoverPool {
    const MAX_CONCURRENCY: usize = 8;
    const MAX_QUEUE: usize = 400;

    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            inflight: 0,
            seen: HashSet::new(),
        }
    }

    /// 入队（去重 + 队列上限），返回是否新增。
    fn push(&mut self, url: &str) -> bool {
        let url = url.trim();
        if url.is_empty() || self.seen.contains(url) || self.queue.len() >= Self::MAX_QUEUE {
            return false;
        }
        self.seen.insert(url.to_string());
        self.queue.push_back(url.to_string());
        true
    }

    fn next(&mut self) -> Option<String> {
        if self.inflight >= Self::MAX_CONCURRENCY {
            return None;
        }
        let url = self.queue.pop_front()?;
        self.inflight += 1;
        Some(url)
    }

    fn done(&mut self) {
        self.inflight = self.inflight.saturating_sub(1);
    }
}

/// 列表一次渲染的行数上限：GPUI 没有虚拟滚动，一次性布局几百行 + 解码封面
/// 会把主线程顶住；分批渲染 + 「显示更多」是这里最稳的做法。
pub const ROW_BATCH: usize = 40;

/// 把扫码状态翻成中文短标签（用于「等待扫码 / 已扫码」这类提示）。
pub(crate) fn status_label(status: LoginStatus, language: Language) -> &'static str {
    match status {
        LoginStatus::Waiting => language.text("等待扫码"),
        LoginStatus::Scanned => language.text("已扫码"),
        LoginStatus::Success => language.text("登录成功"),
        LoginStatus::Expired => language.text("已过期"),
        LoginStatus::Failed => language.text("失败"),
    }
}

use crate::ui;
use crate::ui::i18n::{scene_mode_text, Language};
use crate::ui::theme::{self, ThemeKind};
use crate::views;

/// 左侧导航项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Nav {
    Home,
    Scenes,
    Search,
    Liked,
    Library,
    Artist,
    Album,
    Settings,
    Lyrics,
    /// 本地音乐：本应用下载到本地的曲目（跨重启持久）。
    Local,
}

impl Root {
    /// 侧栏/页面标题用的导航名；听歌模式跟随当前队列来源动态变化。
    pub fn nav_display_label(&self, nav: Nav) -> String {
        let label = match nav {
            Nav::Home => self.tr("推荐"),
            Nav::Scenes => self.tr("听歌模式"),
            Nav::Search => self.tr("搜索"),
            Nav::Liked => self.tr("我喜欢的音乐"),
            Nav::Library => self.tr("我的歌单"),
            Nav::Artist => self.tr("音乐人"),
            Nav::Album => self.tr("专辑"),
            Nav::Settings => self.tr("设置"),
            Nav::Lyrics => self.tr("正在播放"),
            Nav::Local => self.tr("本地音乐"),
        };
        if nav != Nav::Scenes {
            return label.to_string();
        }
        let active_origin = self
            .switching_queue
            .clone()
            .unwrap_or_else(|| self.queue_origin.clone());
        let QueueOrigin::FeedMode(kind) = active_origin else {
            return label.to_string();
        };
        let scene_text = self
            .recommendation
            .as_ref()
            .and_then(|source| source.scene.as_ref())
            .filter(|scene| scene.sub_queue_type == kind)
            .map(|scene| self.scene_display_text(&scene.sub_queue_type, &scene.text))
            .or_else(|| {
                self.scenes
                    .iter()
                    .find(|scene| scene.sub_queue_type == kind)
                    .map(|scene| self.scene_display_text(&scene.sub_queue_type, &scene.text))
            });
        scene_text.unwrap_or_else(|| label.to_string())
    }
}

/// 登录状态机。
pub enum LoginState {
    /// 还没开始
    Idle,
    /// 正在创建二维码
    Loading,
    /// 二维码已就绪，等待扫码（`status` 是轮询到的最新状态文案）
    Waiting {
        matrix: QrMatrix,
        token: String,
        status: String,
    },
    /// 已登录
    LoggedIn(#[allow(dead_code)] AccountInfo),
    /// 出错（可点重试）
    Failed(String),
}

/// 当前播放队列来源，语义对齐官方 queue metaInfo。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueOrigin {
    None,
    Feed,
    FeedMode(String),
    Radio(String),
    Liked,
    Playlist(String),
    Artist(String),
    Album(String),
    Search,
    /// 本地音乐页（磁盘上的文件，不走网络取流）。
    Local,
}

/// 歌曲右键菜单状态：目标曲目与弹出位置。
pub struct TrackMenu {
    pub track: TrackItem,
    pub position: gpui::Point<gpui::Pixels>,
}

/// 本地音乐页的横向入口。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LocalTab {
    /// 本地音乐：**以磁盘为准** —— 下载目录里真实存在的每一个音频，
    /// 包括用户自己放进来的（那些没有元数据，只显示文件名）。
    #[default]
    Songs,
    /// 下载管理：**以记录为准** —— 本应用的下载任务与下载记录。
    Downloads,
    /// 扫描本地音乐。本次只放占位入口，扫描能力见 `design-plan.md` 的 B 阶段。
    Scan,
}

/// 本地音乐列表的一行：磁盘上真实存在的一个音频文件。
///
/// 与「下载记录」（`download_library::LibraryEntry`）的区别是刻意的：
/// 这里**以磁盘为准**，记录**以清单为准**。所以用户手动丢进下载目录的文件会出现在
/// 「本地音乐」里、却不在「已下载」里 —— 这两个概念不该混成一份数据。
#[derive(Clone, Default)]
pub struct LocalFile {
    /// 相对下载目录、不含扩展名。
    pub relative: PathBuf,
    /// 实际落盘扩展名（`m4a` / `flac` …）；拼绝对路径要用。
    pub extension: String,
    pub bytes: u64,
    /// 清单里有对应记录时带上元数据（歌名 / 歌手 / 音质…）；没有则为 `None`。
    pub entry: Option<sodam_core::download_library::LibraryEntry>,
    /// 容器头探测出的时长（秒）；清单里的权威值优先。
    pub probed_duration: Option<f64>,
    /// `<下载目录>/image/<文件名>.<png|jpg|jpeg|webp>`（旁路封面），扫描时探得。
    pub cover_path: Option<PathBuf>,
    /// `<下载目录>/lrc/<文件名>.lrc`（旁路歌词），扫描时探得。
    pub lyrics_path: Option<PathBuf>,
}

impl LocalFile {
    /// 磁盘上的真实路径：`<下载目录>/<relative>.<extension>`。
    ///
    /// `relative` 不含扩展名（与清单主键、命名模板同源），这里**字面拼接**扩展名。
    /// 不能用 `with_extension` —— 它会把曲名里本来就有的点号当成扩展名截断
    /// （`A - B 1.5` → `A - B 1.m4a`，实测踩过，见下面的测试）。
    pub fn absolute_path(&self, settings: &sodam_core::download::DownloadSettings) -> PathBuf {
        let parent = self
            .relative
            .parent()
            .unwrap_or_else(|| std::path::Path::new(""));
        let mut name = self
            .relative
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_default();
        name.push(".");
        name.push(&self.extension);
        settings.resolve_dir().join(parent).join(name)
    }

    /// 展示用时长（秒）：清单权威值 → 探测值 → 无。
    pub fn duration_seconds(&self) -> Option<i64> {
        self.entry
            .as_ref()
            .and_then(|entry| entry.duration_seconds)
            .filter(|seconds| *seconds > 0)
            .or_else(|| {
                self.probed_duration
                    .filter(|seconds| *seconds > 0.0)
                    .map(|seconds| seconds.round() as i64)
            })
    }

    /// 交给引擎的 `TrackItem`。
    ///
    /// 有清单记录就用它的元数据；没有（用户手动放进下载目录的文件）就合成一个：
    /// `id` 用 `local:<relative>`（**必须非空** —— `toggle_play` 等处以
    /// `snapshot.track_id.is_empty()` 判断"引擎还没装载"），标题退化成文件名。
    pub fn to_track(&self) -> TrackItem {
        if let Some(entry) = self.entry.as_ref() {
            if let Some(track) = entry.to_track() {
                return track;
            }
        }
        let relative = self.relative.to_string_lossy().replace('\\', "/");
        TrackItem {
            id: format!("local:{relative}"),
            title: self
                .entry
                .as_ref()
                .map(|entry| entry.display_title())
                .unwrap_or_else(|| {
                    self.relative
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| relative.clone())
                }),
            artist: self
                .entry
                .as_ref()
                .map(|entry| entry.display_artist())
                .unwrap_or_default(),
            duration_seconds: self.duration_seconds().unwrap_or(0),
            ..Default::default()
        }
    }
}

/// 「已下载」判定快照。
///
/// 把「已存在的音频相对路径集合」和「算这些路径时用的命名模板」**成对**快照下来：
/// 路径是拿模板算出来的，模板一改旧路径就对不上，两者必须同源同版本。
///
/// 判定语义严格等于「点下载会不会说『已存在』」——用的是 `download::scan_downloaded`
/// 与 `download::render_relative_path`，与下载侧跳过逻辑同一套代码。
///
/// **不代表播放会走本地文件或能离线播放**：播放用的是独立的播放缓存
/// （`session.rs` 的 `cached_track`），完全不读下载目录。
#[derive(Clone, Default)]
pub struct DownloadedIndex {
    paths: Arc<HashSet<PathBuf>>,
    naming: Arc<str>,
}

#[cfg(test)]
mod local_file_tests {
    use super::LocalFile;
    use sodam_core::download::DownloadSettings;
    use std::path::PathBuf;

    /// 没有元数据的文件（用户手动放进下载目录的）也要能交给引擎：
    /// 合成 id 必须非空 —— `toggle_play` 等处以 `track_id.is_empty()`
    /// 判断「引擎还没装载」，空 id 会让播放键永远走「装载当前曲目」分支。
    #[test]
    fn synthetic_track_has_a_non_empty_id_and_falls_back_to_file_name() {
        let file = LocalFile {
            relative: PathBuf::from("某歌手 - 某歌"),
            extension: "m4a".to_string(),
            bytes: 1234,
            ..Default::default()
        };
        let track = file.to_track();
        assert!(!track.id.is_empty(), "合成 id 不能为空：{:?}", track.id);
        assert_eq!(track.id, "local:某歌手 - 某歌");
        assert_eq!(track.title, "某歌手 - 某歌", "标题退化成文件名");
        assert_eq!(track.duration_seconds, 0, "未知时长按 0");
    }

    /// 有清单记录时用记录的元数据（不是文件名）。
    #[test]
    fn entry_metadata_wins_over_the_file_name() {
        let entry = sodam_core::download_library::LibraryEntry {
            relative: PathBuf::from("A - B"),
            extension: "m4a".to_string(),
            source: sodam_core::download_library::EntrySource::Downloaded,
            bytes: 10,
            added_at: 0,
            quality: Some("无损 871k".to_string()),
            downloaded_at: Some(0),
            mtime: None,
            track_id: Some("123".to_string()),
            title: Some("真标题".to_string()),
            artist: Some("真歌手".to_string()),
            album: None,
            artist_id: None,
            album_id: None,
            cover: None,
            duration_seconds: Some(215),
            vip: Some(false),
        };
        let file = LocalFile {
            relative: PathBuf::from("A - B"),
            extension: "m4a".to_string(),
            bytes: 10,
            entry: Some(entry),
            ..Default::default()
        };
        let track = file.to_track();
        assert_eq!(track.id, "123");
        assert_eq!(track.title, "真标题");
        assert_eq!(track.artist, "真歌手");
        assert_eq!(track.duration_seconds, 215);
    }

    /// 相对路径（不含扩展名）+ 扫盘记下的扩展名 → 磁盘上的真实路径。
    ///
    /// 关键：不能用 `with_extension` 去「猜」扩展名 —— 歌名里的点号
    /// （`A - B 1.5`）会被当成扩展名截断。这里断言含点号的曲名能还原正确。
    #[test]
    fn absolute_path_appends_the_scanned_extension() {
        let settings = DownloadSettings {
            dir: "/tmp/sodam-dl".to_string(),
            ..Default::default()
        };
        let file = LocalFile {
            relative: PathBuf::from("A - B 1.5"),
            extension: "m4a".to_string(),
            bytes: 10,
            ..Default::default()
        };
        assert_eq!(
            file.absolute_path(&settings),
            PathBuf::from("/tmp/sodam-dl/A - B 1.5.m4a")
        );
    }

    /// 命名模板带 `/` 分层时，父目录要保留（不能把整条相对路径拍平）。
    #[test]
    fn absolute_path_keeps_template_subdirectories() {
        let settings = DownloadSettings {
            dir: "/tmp/sodam-dl".to_string(),
            ..Default::default()
        };
        let file = LocalFile {
            relative: PathBuf::from("某歌手/某专辑/歌名"),
            extension: "flac".to_string(),
            bytes: 10,
            ..Default::default()
        };
        assert_eq!(
            file.absolute_path(&settings),
            PathBuf::from("/tmp/sodam-dl/某歌手/某专辑/歌名.flac")
        );
    }

    /// 回归：本地曲目的封面来源必须是 `file://` 伪 URL，而不是空的 `track.cover`。
    ///
    /// 曾经的问题：`ensure_playback_assets` 拿 `track.cover` 准备封面，
    /// 而播放页渲染用 `file://` 取图 —— 两者对不上，「已下载原图」永远为假，
    /// 界面一直显示「重试加载」（用户报的 bug）。
    #[test]
    fn local_cover_source_prefers_the_sidecar() {
        let file = LocalFile {
            relative: PathBuf::from("Yang"),
            extension: "m4a".to_string(),
            bytes: 10,
            cover_path: Some(PathBuf::from("/tmp/sodam-dl/image/Yang.png")),
            ..Default::default()
        };
        // 无元数据的文件：`to_track()` 合成 id，`cover` 为空。
        let track = file.to_track();
        assert!(track.cover.is_empty(), "合成曲目没有网络封面");

        // 复现 `Root::playback_cover_source` 的判定（它需要完整 Root，这里只测规则）。
        let source = file
            .cover_path
            .as_ref()
            .map(|path| format!("file://{}", path.display()))
            .unwrap_or_else(|| track.cover.clone());
        assert_eq!(source, "file:///tmp/sodam-dl/image/Yang.png");
        assert!(!source.is_empty(), "来源不能为空 —— 空的会让封面池直接跳过");
    }
}

impl DownloadedIndex {
    /// 由一份已经扫好的「已存在的音频相对路径集合」构造。
    ///
    /// 扫盘只做一次：`refresh_local` 把同一份集合同时喂给这个标记和本地音乐库
    /// （大目录 / 网络盘上少扫一整遍）。
    pub(crate) fn from_paths(paths: HashSet<PathBuf>, naming: &str) -> Self {
        Self {
            paths: Arc::new(paths),
            naming: Arc::from(naming),
        }
    }

    /// 这首曲目在下载目录里是否已经有音频文件。
    pub fn has(&self, track: &TrackItem) -> bool {
        self.paths
            .contains(&sodam_core::download::render_relative_path(
                &self.naming,
                track,
            ))
    }
}

/// 根视图状态。
pub struct Root {
    pub nav: Nav,
    /// 页面返回栈；详情页统一使用返回组件。
    pub(crate) nav_history: Vec<Nav>,
    pub lyrics_return_nav: Option<Nav>,
    pub theme: ThemeKind,
    pub theme_follows_system: bool,
    pub language: Language,
    pub language_follows_system: bool,
    pub settings: Settings,
    pub session: Option<Session>,
    pub queue: Queue,
    pub queue_origin: QueueOrigin,
    pub playing: bool,
    pub status: String,
    /// 官方推荐队列分页状态。
    pub recommendation: Option<RecommendationSource>,
    pub loading_recommendation: bool,
    /// 点推荐后的整页切换 loading；后台 load-more 不置位。
    pub switching_queue: Option<QueueOrigin>,
    pub switching_label: String,
    pub(crate) recommendation_request_seq: u64,
    /// 搜索框内容与焦点。
    pub lyrics: Arc<Vec<LyricLine>>,
    pub lyrics_track_id: String,
    pub lyrics_title: String,
    pub lyrics_artist: String,
    pub lyrics_cover: String,
    pub loading_lyrics: bool,
    pub lyrics_error: Option<String>,
    pub lyrics_active: Option<usize>,
    pub lyrics_scroll: gpui::UniformListScrollHandle,
    /// 防止快速换歌时旧歌词请求回写。
    lyrics_request_seq: u64,
    pub search_input: String,
    pub search_focus: FocusHandle,
    /// 窗口根元素的焦点。
    ///
    /// 必须有：gpui 的上下文栈取自「聚焦节点到根的路径」，没有聚焦元素时栈是空的，
    /// 带上下文谓词的快捷键绑定（播放三键绑在 `workspace && !input` 上）永远不会匹配。
    /// 所以根元素自己持焦点，点搜索框等元素时焦点自然让出去。
    pub workspace_focus: FocusHandle,
    /// 设置页里的快捷键卡片：录制新组合键时持有焦点，并让播放绑定让路。
    pub shortcut_focus: FocusHandle,
    /// 正在录制的动作（None = 没在录制）。
    pub shortcut_capture: Option<crate::shortcuts::Slot>,
    /// 快捷键配置有改动，需要重新注册 keymap（由 main 的托盘循环统一应用）。
    pub shortcuts_dirty: bool,
    /// 搜索结果与加载状态。
    pub results: Arc<Vec<TrackItem>>,
    pub search_results: SearchResults,
    pub search_tab: SearchScope,
    pub searching: bool,
    /// 最近一次真正提交搜索的关键词；用于区分“没搜过”和“搜了没结果”。
    pub search_keyword: String,
    /// 听歌模式页：常用模式与探索歌单。
    pub scenes: Arc<Vec<SceneItem>>,
    pub loading_scenes: bool,
    pub scene_cards: Arc<Vec<SceneCard>>,
    pub loading_scene_cards: bool,
    /// 听歌模式页的变高虚拟列表状态；只渲染可视行，避免几十张封面同时布局。
    pub scenes_list: gpui::ListState,
    /// 详情页使用独立列表状态，避免页面切换时共享状态导致内容/滚动错乱。
    pub liked_detail_list: gpui::ListState,
    pub library_detail_list: gpui::ListState,
    pub search_playlist_detail_list: gpui::ListState,
    pub artist_detail_list: gpui::ListState,
    pub album_detail_list: gpui::ListState,
    scenes_list_items: usize,
    /// 听歌模式页可用宽度；用于动态计算虚拟网格列数。
    pub scenes_width: Arc<Mutex<f32>>,
    pub scenes_common_columns: usize,
    pub scenes_card_columns: usize,
    /// 我的歌单 / 我喜欢的音乐（进入对应页面时懒加载）。
    pub playlists: Vec<PlaylistItem>,
    pub liked: Arc<Vec<TrackItem>>,
    /// 「我喜欢的音乐」的曲目 id 集合（列表里的爱心状态）。
    pub liked_ids: Arc<HashSet<String>>,
    liked_loading: bool,
    /// 收藏 ids 拉取失败（网络等）：置位后等网络就绪信号自动重试。
    pub liked_ids_failed: bool,
    /// 「已下载」标记快照：后台扫一次下载目录，渲染只读这份快照。
    pub downloaded: DownloadedIndex,
    /// 本地音乐库快照（清单 ∩ 扫盘校验通过）；跨重启存在。
    pub library: Arc<Vec<sodam_core::download_library::LibraryEntry>>,
    /// 「本地音乐」列表：磁盘真相（比 `library` 多出用户手动放进来的文件）。
    pub local_files: Arc<Vec<LocalFile>>,
    /// 元数据匹配进行中：`(已完成, 总数)`；`None` = 没在跑。
    ///
    /// 批量匹配是串行 + 限速的后台任务，界面据此显示进度。
    pub matching: Option<(usize, usize)>,
    /// 本地音乐行的右键菜单：`(相对路径, 弹窗位置)`。
    pub local_menu: Option<(PathBuf, gpui::Point<gpui::Pixels>)>,
    /// 「手动选择匹配」浮层：`(目标文件相对路径, 候选列表)`。
    ///
    /// 自动匹配失败（或用户想自己挑）时打开；候选已按分数降序、最多 5 条。
    pub match_picker: Option<(PathBuf, Vec<sodam_core::match_metadata::MatchOutcome>)>,
    /// 本地音乐页当前横向入口。
    pub local_tab: LocalTab,
    /// 清单里有、磁盘上已不在的条目（页面的「待重下」分区）。
    pub library_missing: Arc<Vec<sodam_core::download_library::LibraryEntry>>,
    /// 已有本地库扫描在跑：只记一个「待重扫」，避免连续触发堆出一串扫描。
    pub(crate) local_scanning: bool,
    /// 扫描期间又被请求刷新：结束后补扫一次。
    pub(crate) local_rescan: bool,
    pub loading_library: bool,
    pub loading_liked: bool,
    pub liked_loaded: bool,
    /// 列表滚动句柄：交给 `uniform_list` 之后，UI 滚动位置由 GPUI 维护，
    /// 同时可用于开发期滚动压测（`SODAM_SCROLL_TEST=1`）。
    pub liked_scroll: gpui::UniformListScrollHandle,
    /// 列表可用宽度（由画布测量）：决定折叠哪些列。
    pub list_width: Arc<Mutex<f32>>,
    /// 队列抽屉的滚动句柄（打开时锚到「正在播放」）。
    pub queue_scroll: gpui::UniformListScrollHandle,
    /// 队列抽屉是否展开。
    pub queue_open: bool,
    pub volume_before_mute: f32,
    /// 播放栏弹层锚点：触发元素（音质徽章/音量/队列）在窗口里的位置，
    /// 由画布测量，弹层据此精确贴住入口。
    /// 进度条轨道在窗口里的位置（画布测量，鼠标事件据此换算 seek 位置）。
    pub progress_track_bounds: Arc<Mutex<Option<gpui::Bounds<gpui::Pixels>>>>,
    /// 音量轨道在窗口里的位置（画布测量，鼠标事件据此换算音量）。
    pub volume_track_bounds: Arc<Mutex<Option<gpui::Bounds<gpui::Pixels>>>>,
    /// 打开中的歌单（点卡片后展示曲目）。
    pub open_playlist: Option<(String, String, Arc<Vec<TrackItem>>)>,
    pub loading_playlist: bool,
    /// 搜索结果里的歌单详情；保留在 Search 上下文，不切去「我的歌单」。
    pub search_playlist: Option<(String, String, Arc<Vec<TrackItem>>)>,
    pub loading_search_playlist: bool,
    /// 音乐人 / 专辑详情页。
    pub open_artist: Option<ArtistDetail>,
    pub loading_artist: bool,
    pub loading_artist_tracks: bool,
    pub open_album: Option<AlbumDetail>,
    pub loading_album: bool,
    /// 渲染期收集到的封面需求（虚拟列表只渲染可见行，所以这里排的就是
    /// 「当前真正看得见」的那些封面）。由后台循环定期消费。
    pub cover_requests: Arc<Mutex<Vec<String>>>,
    /// 真实播放过的曲目 id（最近在后）。用于队列抽屉的「已播放」分区；
    /// 不能用「当前下标之前」来推 —— 那会把跳过的歌也算进去（实测踩过）。
    pub played_history: Vec<String>,
    /// 设置页里的登录 modal 是否展开。
    pub login_modal_open: bool,
    /// 队列右键菜单：被点中的行下标与鼠标位置。
    pub queue_menu: Option<(usize, gpui::Point<gpui::Pixels>)>,
    /// 搜索/歌单/我喜欢等歌曲行右键菜单。
    pub track_menu: Option<TrackMenu>,
    /// Fcitx/IBus 等 IME 的搜索框预编辑范围（UTF-8 byte range）。
    pub(crate) search_marked_range: Option<std::ops::Range<usize>>,
    /// 播放请求序号：每次 start_track +1。
    ///
    /// 作用：加载是异步的（下载 + 解密），如果用户在 A 加载途中换了 B，
    /// A 完成时必须**作废**，否则会把正在播的 B 顶掉（实测踩过：
    /// B 播一会儿后突然跳回 A，封面也对不上）。
    pub(crate) play_seq: u64,
    /// 播放错误（显示在播放栏：常驻状态条已移除，错误必须有地方可见）。
    pub playback_error: Option<String>,
    /// 连续拉流失败次数（>=3 就停下，避免无限跳歌）。
    pub(crate) consecutive_failures: u32,
    /// 正在预取的曲目 id 集合（多首并行去重；预取结果只写缓存，不改 UI）。
    pub(crate) prefetch_inflight: HashSet<String>,
    /// 预取失败冷却表（曲目 id → 最近失败时刻）：巡检不能把永久失败的歌
    /// （下架/地区受限）每 5 秒无限重试，60s 内不再排。
    pub(crate) prefetch_failed: HashMap<String, std::time::Instant>,
    /// 预取巡检节拍：心跳每 200ms 自增，每 25 拍（约 5s）补一次预取。
    pub(crate) prefetch_patrol: u32,
    /// 缓存统计快照（音频字节, 歌曲数, 封面字节, 封面张数）：
    /// 后台扫盘后回填，设置页只读它（渲染路径不做同步 IO）。
    pub(crate) cache_summary: (u64, usize, u64, usize),
    /// 正在加载（下载/解密）的曲目：非空时播放栏显示 loading，且进度条不可拖。
    pub pending_track: Option<TrackItem>,
    /// 已处理过的「播完」序号（配合引擎的 finished_seq 自动切歌）。
    pub(crate) last_finished_seq: u64,
    /// 拖动进度条时的预览位置（0.0~1.0）：拖动中只改它，松手才真跳。
    pub progress_preview: Option<f32>,
    /// 队列快照（含版本号）：避免抽屉每帧深拷贝整条队列。
    pub queue_cache: (u64, Arc<Vec<TrackItem>>),
    /// 已尝试过的封面 URL（成功在 covers 里，失败的也记下来，避免每帧重复排队）。
    pub cover_attempted: Arc<Mutex<HashSet<String>>>,
    /// 封面缓存：URL → 本地文件。**只由后台任务写入**，渲染路径只读，
    /// 绝不在 render 里发网络请求（否则主线程会被下载卡住）。
    pub covers: Arc<HashMap<String, PathBuf>>,
    pub original_covers: HashMap<String, PathBuf>,
    /// 封面图片缓存：有上界，淘汰时把图从 gpui 的图集里释放（不设这个就会无界增长）。
    pub cover_cache: gpui::Entity<ui::cover_cache::CoverImageCache>,
    pub original_cover_attempted: Arc<Mutex<HashSet<String>>>,
    pub ambient_colors: HashMap<String, (u8, u8, u8)>,
    pub(crate) cover_pool: CoverPool,
    /// 各列表当前渲染的行数（点「显示更多」时递增）。
    /// 扫码登录状态与轮询中的 token。
    pub login: LoginState,
    pub account: Option<AccountInfo>,
    /// 音频播放引擎（rodio，跑在专属线程上）
    pub engine: PlaybackEngine,
    /// 播放进度 / 时长上报（喂引擎快照，内部异步发请求）。
    pub play_reporter: PlayReporter,
    /// 下载管理器（队列 / 并发 / 状态机全在 sodam-core）。
    pub downloads: DownloadManager,
    /// 下载面板是否展开（与队列抽屉互斥）。
    pub download_open: bool,
    /// 下载面板当前分区。
    pub download_tab: DownloadTab,
    /// 任务快照缓存：`(revision, tasks)`，渲染只读它，避免每帧深拷贝整个队列。
    pub download_cache: (u64, Arc<Vec<DownloadTask>>),
    /// 任务统计快照（侧栏徽章 / 分区标签）。
    pub download_counts: DownloadCounts,
    /// 上次推给下载管理器的设置指纹（避免每帧深拷贝 Settings）。
    pub(crate) download_settings_fingerprint: u64,
}

/// 下载面板的分区。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DownloadTab {
    #[default]
    Active,
    Completed,
    Failed,
}

impl DownloadTab {
    pub const ALL: [DownloadTab; 3] = [
        DownloadTab::Active,
        DownloadTab::Failed,
        DownloadTab::Completed,
    ];

    pub fn label(self, language: Language) -> &'static str {
        match self {
            DownloadTab::Active => language.text("下载中"),
            DownloadTab::Failed => language.text("下载失败"),
            DownloadTab::Completed => language.text("已下载"),
        }
    }

    pub fn matches(self, state: &sodam_core::download::DownloadState) -> bool {
        match self {
            DownloadTab::Active => state.is_active(),
            DownloadTab::Completed => state.is_finished(),
            DownloadTab::Failed => state.is_failed(),
        }
    }
}

impl Root {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let settings = Settings::load().merged_with_env();
        // 首次运行就把配置落盘：用户可以直接编辑 ~/.config/sodam/config.json
        // 换签名服务 / 钉死设备指纹（默认值已内置，见 sodam_core::config）
        if !Settings::config_path().exists() {
            let _ = settings.save();
        }
        let theme_follows_system = ThemeKind::from_key(&settings.theme).is_none();
        let theme = ThemeKind::from_key(&settings.theme)
            .unwrap_or_else(|| theme::theme_from_appearance(cx.window_appearance()));
        theme::set_theme(theme);
        let (language, language_follows_system) = match Language::from_key(&settings.language) {
            Some(Language::System) | None => (Language::system_locale(), true),
            Some(language) => (language, false),
        };
        let session = Session::new(settings.clone());
        let status = if settings.is_ready_for_vip() {
            if language.is_zh() {
                language
                    .text("已配置登录与签名服务，可以去「搜索」或「推荐」了")
                    .to_string()
            } else {
                "Signed in and signer ready. Try Discover or Search.".to_string()
            }
        } else {
            if language.is_zh() {
                language.text("还没配好：请在「设置」里填 Cookie 与签名服务地址（或设 SODA_COOKIE / QISHUI_SIGNER_URL）").to_string()
            } else {
                "Not ready: set Cookie and signer URL in Settings (or use SODA_COOKIE / QISHUI_SIGNER_URL)".to_string()
            }
        };
        let queue = Queue::new(Vec::<TrackItem>::new());

        let logged_in = !settings.cookie.trim().is_empty();
        let play_reporter = PlayReporter::new(settings.clone());
        let downloads = DownloadManager::new(settings.clone());
        downloads.set_download_settings(settings.download.clone());
        let mut root = Self {
            // 未登录只能待在设置页（登录入口在设置 → 账户）
            nav: if logged_in {
                Nav::Lyrics
            } else {
                Nav::Settings
            },
            nav_history: Vec::new(),
            lyrics_return_nav: if logged_in { Some(Nav::Home) } else { None },
            theme,
            theme_follows_system,
            language,
            language_follows_system,
            settings,
            session: Some(session),
            queue,
            queue_origin: QueueOrigin::None,
            played_history: Vec::new(),
            login_modal_open: false,
            queue_menu: None,
            track_menu: None,
            search_marked_range: None,
            play_seq: 0,
            playback_error: None,
            consecutive_failures: 0,
            playing: false,
            recommendation: None,
            loading_recommendation: false,
            switching_queue: None,
            switching_label: language.text("推荐队列").to_string(),
            recommendation_request_seq: 0,
            status,
            lyrics: Arc::new(Vec::new()),
            lyrics_track_id: String::new(),
            lyrics_title: String::new(),
            lyrics_artist: String::new(),
            lyrics_cover: String::new(),
            loading_lyrics: false,
            lyrics_error: None,
            lyrics_active: None,
            lyrics_scroll: gpui::UniformListScrollHandle::new(),
            lyrics_request_seq: 0,
            search_input: String::new(),
            search_focus: cx.focus_handle(),
            workspace_focus: cx.focus_handle(),
            shortcut_focus: cx.focus_handle(),
            shortcut_capture: None,
            shortcuts_dirty: false,
            results: Arc::new(Vec::new()),
            search_results: SearchResults::default(),
            search_tab: SearchScope::All,
            searching: false,
            search_keyword: String::new(),
            scenes: Arc::new(Vec::new()),
            loading_scenes: false,
            scene_cards: Arc::new(Vec::new()),
            loading_scene_cards: false,
            scenes_list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(theme::space::XL)),
            liked_detail_list: gpui::ListState::new(
                1,
                gpui::ListAlignment::Top,
                px(theme::space::XXL),
            ),
            library_detail_list: gpui::ListState::new(
                1,
                gpui::ListAlignment::Top,
                px(theme::space::XXL),
            ),
            search_playlist_detail_list: gpui::ListState::new(
                1,
                gpui::ListAlignment::Top,
                px(theme::space::XXL),
            ),
            artist_detail_list: gpui::ListState::new(
                1,
                gpui::ListAlignment::Top,
                px(theme::space::XXL),
            ),
            album_detail_list: gpui::ListState::new(
                1,
                gpui::ListAlignment::Top,
                px(theme::space::XXL),
            ),
            scenes_list_items: 0,
            scenes_width: Arc::new(Mutex::new(0.0)),
            scenes_common_columns: 4,
            scenes_card_columns: 4,
            playlists: Vec::new(),
            liked: Arc::new(Vec::new()),
            liked_ids: Arc::new(HashSet::new()),
            liked_loading: false,
            liked_ids_failed: false,
            downloaded: DownloadedIndex::default(),
            library: Arc::new(Vec::new()),
            local_files: Arc::new(Vec::new()),
            matching: None,
            local_menu: None,
            match_picker: None,
            local_tab: LocalTab::default(),
            library_missing: Arc::new(Vec::new()),
            local_scanning: false,
            local_rescan: false,
            loading_library: false,
            loading_liked: false,
            liked_loaded: false,
            liked_scroll: gpui::UniformListScrollHandle::new(),
            list_width: Arc::new(Mutex::new(1200.0)),
            queue_scroll: gpui::UniformListScrollHandle::new(),
            open_playlist: None,
            loading_playlist: false,
            search_playlist: None,
            loading_search_playlist: false,
            open_artist: None,
            loading_artist: false,
            loading_artist_tracks: false,
            open_album: None,
            loading_album: false,
            queue_open: false,
            volume_before_mute: 1.0,
            progress_track_bounds: Arc::new(Mutex::new(None)),
            volume_track_bounds: Arc::new(Mutex::new(None)),
            prefetch_inflight: HashSet::new(),
            prefetch_failed: HashMap::new(),
            prefetch_patrol: 0,
            cache_summary: (0, 0, 0, 0),
            pending_track: None,
            last_finished_seq: 0,
            progress_preview: None,
            queue_cache: (0, Arc::new(Vec::new())),
            cover_attempted: Arc::new(Mutex::new(HashSet::new())),
            cover_requests: Arc::new(Mutex::new(Vec::new())),
            covers: Arc::new(HashMap::new()),
            cover_cache: ui::cover_cache::CoverImageCache::new(cx),
            original_covers: HashMap::new(),
            original_cover_attempted: Arc::new(Mutex::new(HashSet::new())),
            ambient_colors: HashMap::new(),
            cover_pool: CoverPool::new(),
            login: LoginState::Idle,
            account: None,
            engine: PlaybackEngine::new(),
            play_reporter,
            downloads,
            download_open: false,
            download_tab: DownloadTab::default(),
            download_cache: (u64::MAX, Arc::new(Vec::new())),
            download_counts: DownloadCounts::default(),
            download_settings_fingerprint: 0,
        };
        if logged_in {
            root.refresh_account(cx);
            root.load_liked_ids(cx);
            root.start_recommendation(false, cx);
        } else {
            root.status = root.tr("请先在「设置 → 账户」扫码登录").to_string();
        }
        // 本地目录扫描与登录无关（纯本地文件系统），启动就快照一次。
        root.refresh_local(cx);
        // 开发/验证便利：`SODAM_QUERY="周杰伦" cargo run -p sodam` 启动即搜索一次
        if let Ok(query) = std::env::var("SODAM_QUERY") {
            if !query.trim().is_empty() {
                root.search_input = query;
                root.nav = Nav::Search;
                root.run_search(cx);
            }
        }
        // 开发/验证便利：`SODAM_NAV=library|l liked|search|scenes|home` 直接落到某个页面
        if let Ok(nav) = std::env::var("SODAM_NAV") {
            root.nav = match nav.trim().to_lowercase().as_str() {
                "home" => Nav::Home,
                "scenes" => Nav::Scenes,
                "search" => Nav::Search,
                "liked" => Nav::Liked,
                "library" => Nav::Library,
                "artist" => Nav::Artist,
                "album" => Nav::Album,
                "settings" => Nav::Settings,
                "local" => Nav::Local,
                _ => root.nav,
            };
            root.on_nav_changed(cx);
            cx.notify();
        }
        // 封面需求消费循环：渲染只往队列里塞 URL，这里统一喂给请求池
        //（最多 8 并发 + 去重），避免渲染过程中做任何网络/状态更新。
        {
            let queue = root.cover_requests.clone();
            cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(60))
                    .await;
                let pending: Vec<String> = {
                    let mut queue = match queue.lock() {
                        Ok(queue) => queue,
                        Err(_) => continue,
                    };
                    if queue.is_empty() {
                        continue;
                    }
                    std::mem::take(&mut *queue)
                };
                let _ = this.update(cx, |root, cx| root.ensure_covers(&pending, cx));
            })
            .detach();
        }

        // 开发期滚动压测：`SODAM_SCROLL_TEST=1` 时按行推进滚动，
        // 配合 `SODAM_FRAME_LOG=1` 就能量出「滑动是否掉帧」，不用手工滚。
        if std::env::var("SODAM_SCROLL_TEST").is_ok() {
            let handle = root.liked_scroll.clone();
            cx.spawn(async move |_this, cx| {
                let mut item = 0usize;
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(32))
                        .await;
                    item = if item > 380 { 0 } else { item + 2 };
                    handle.scroll_to_item(item, gpui::ScrollStrategy::Center);
                    cx.refresh();
                }
            })
            .detach();
        }
        Self::start_heartbeat(cx);

        // 开发验证用：`SODAM_AUTOPLAY=1` 进收藏页并自动播放第一首；
        // `SODAM_POPUP=queue|downloads` 启动即打开对应叠加面板（便于截图核对）。
        if std::env::var("SODAM_AUTOLOGIN").is_ok() && !root.settings.cookie.trim().is_empty() {
            root.login_modal_open = true;
            root.start_login(cx);
        }
        if std::env::var("SODAM_AUTOPLAY").is_ok() {
            let this = cx.entity();
            cx.spawn(async move |_this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(2500))
                    .await;
                this.update(cx, |root, cx| {
                    if !root.liked.is_empty() {
                        root.play_from_arc(root.liked.clone(), 0, cx);
                    }
                });
            })
            .detach();
        }
        // 竞态复现：先播 A，300ms 后立刻换 B（验证 A 的加载结果会被丢弃）
        if std::env::var("SODAM_RACE_TEST").is_ok() {
            let this = cx.entity();
            cx.spawn(async move |_this, cx| {
                for _ in 0..60 {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(500))
                        .await;
                    let ready = this.update(cx, |root, _cx| root.liked.len() > 8);
                    if ready {
                        break;
                    }
                }
                // 挑两首「不在缓存里」的歌，才能真正制造「A 还在下载时就切 B」
                let picks = this.update(cx, |root, _cx| {
                    let session = root.session.as_ref()?;
                    let tracks = root.liked.as_ref();
                    let mut picks = Vec::new();
                    for (index, track) in tracks.iter().enumerate() {
                        if !session.is_cached(&track.id) {
                            picks.push(index);
                            if picks.len() == 2 {
                                break;
                            }
                        }
                    }
                    (picks.len() == 2).then_some(picks)
                });
                let Some(picks) = picks else {
                    if std::env::var("SODAM_PLAYER_LOG").is_ok() {
                        eprintln!("[player] 竞态测试：缓存里找不到两首未下载的歌，跳过");
                    }
                    return;
                };
                if std::env::var("SODAM_PLAYER_LOG").is_ok() {
                    eprintln!("[player] 竞态测试：A=#{}, B=#{}", picks[0], picks[1]);
                }
                this.update(cx, |root, cx| {
                    root.play_from(root.liked.as_ref().clone(), picks[0], cx);
                });
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(200))
                    .await;
                this.update(cx, |root, cx| {
                    root.play_from(root.liked.as_ref().clone(), picks[1], cx);
                });
            })
            .detach();
        }
        if let Ok(index) = std::env::var("SODAM_JUMP_TEST") {
            let index: usize = index.trim().parse().unwrap_or(0);
            let this = cx.entity();
            cx.spawn(async move |_this, cx| {
                // 等收藏列表加载完（队列非空）再模拟点击
                for _ in 0..60 {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(500))
                        .await;
                    let ready = this.update(cx, |root, _cx| root.queue.len() > index);

                    if ready {
                        break;
                    }
                }
                this.update(cx, |root, cx| {
                    root.jump_in_queue(index, cx);
                    cx.notify();
                });
            })
            .detach();
        }
        // 开发验证用：`SODAM_POPUP=queue|downloads` 启动即打开对应叠加面板（便于截图核对）。
        if let Ok(popup) = std::env::var("SODAM_POPUP") {
            match popup.trim() {
                "queue" => root.queue_open = true,
                "downloads" | "download" => root.download_open = true,
                _ => {}
            }
        }
        root
    }
}

impl EntityInputHandler for Root {
    fn text_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        actual_range: &mut Option<std::ops::Range<usize>>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        if !self.search_input_focused(window) {
            return None;
        }
        let range = self.search_byte_range_from_utf16(range_utf16);
        actual_range.replace(self.search_utf16_range_from_byte(range.clone()));
        Some(self.search_input[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if !self.search_input_focused(window) {
            return None;
        }
        let range = self
            .search_marked_range
            .clone()
            .unwrap_or(self.search_input.len()..self.search_input.len());
        Some(UTF16Selection {
            range: self.search_utf16_range_from_byte(range),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        if !self.search_input_focused(window) {
            return None;
        }
        self.search_marked_range
            .as_ref()
            .map(|range| self.search_utf16_range_from_byte(range.clone()))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.search_marked_range.take().is_some() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<std::ops::Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.search_input_focused(window) {
            return;
        }
        let range = self.search_replacement_range(range_utf16);
        self.search_input.replace_range(range, text);
        self.search_marked_range = None;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range_utf16: Option<std::ops::Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.search_input_focused(window) {
            return;
        }
        let range = self.search_replacement_range(range_utf16);
        let start = range.start;
        self.search_input.replace_range(range, new_text);
        let end = start + new_text.len();
        self.search_marked_range = Some(start..end);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        element_bounds: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.search_input_focused(window).then_some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        if !self.search_input_focused(window) {
            return None;
        }
        Some(self.search_input.chars().map(char::len_utf16).sum())
    }

    fn text_length_utf16(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        if !self.search_input_focused(window) {
            return None;
        }
        Some(self.search_input.chars().map(char::len_utf16).sum())
    }

    fn accepts_text_input(&self, window: &mut Window, _cx: &mut Context<Self>) -> bool {
        self.search_input_focused(window)
    }
}

impl Root {
    pub fn tr(&self, text: &'static str) -> &'static str {
        self.language.text(text)
    }

    /// 听歌模式展示名：中文环境用服务端名，英文环境用固定映射。
    pub fn scene_display_text(&self, sub_queue_type: &str, fallback: &str) -> String {
        scene_mode_text(self.language, sub_queue_type, fallback).to_string()
    }

    pub fn localized(&self, key: &'static str, args: &[String]) -> String {
        self.language.textf(key, args)
    }

    pub fn set_status(&mut self, key: &'static str, args: &[String]) {
        self.status = self.localized(key, args);
    }

    pub fn set_language(&mut self, language: Language, cx: &mut Context<Self>) {
        self.language = language;
        self.language_follows_system = false;
        self.settings.language = language.key().to_string();
        let save_result = self.settings.save();
        self.status = match save_result {
            Ok(()) => {
                if language.is_zh() {
                    language.text("语言已切换为中文").to_string()
                } else {
                    language.text("Language switched to English").to_string()
                }
            }
            Err(err) => {
                if self.language.is_zh() {
                    language.textf("语言已切换，但保存失败：{err}", &[err.to_string()])
                } else {
                    format!("Language switched, but saving failed: {err}")
                }
            }
        };
        // macOS 菜单栏的标题也要跟着换语言。
        #[cfg(target_os = "macos")]
        crate::menus::refresh(cx, language);
        cx.notify();
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 没有元素聚焦时把焦点收回根元素：gpui 的上下文栈取自聚焦节点的路径，
        // 空栈会让带谓词的快捷键绑定（播放三键）全部失效。
        if window.focused(cx).is_none() {
            window.focus(&self.workspace_focus, cx);
        }
        // 开发期帧耗时探针（`SODAM_FRAME_LOG=1`）：渲染超过 16ms 就打一行。
        // 排查「UI 卡死」先看这里，而不是靠感觉。
        let frame_started = std::time::Instant::now();
        if self.theme_follows_system {
            self.theme = theme::theme_from_appearance(window.appearance());
        }
        theme::set_theme(self.theme);
        if self.nav == Nav::Scenes {
            self.sync_scenes_list();
        }
        let ambient_url = self.queue.current().map(|track| track.cover.clone());
        let ambient_color = ambient_url
            .as_deref()
            .and_then(|url| self.ambient_colors.get(url).copied());
        if std::env::var("SODAM_THEME_LOG").is_ok() {
            eprintln!(
                "[theme] current={:?} url={:?} color={:?}",
                self.queue.current().map(|track| track.title.clone()),
                ambient_url,
                ambient_color
            );
        }
        theme::set_ambient_rgb(ambient_color);
        // 叠加面板的宽度在 Rust 侧算（不能交给样式系统的百分比 —— 见
        // `theme::overlay_panel_width`）。
        let viewport_width = f32::from(window.viewport_size().width);
        if self.nav == Nav::Lyrics {
            let position = self.engine.snapshot().position_seconds;
            let active = self
                .lyrics
                .iter()
                .rposition(|line| position + 0.25 >= line.start_seconds)
                .or_else(|| (!self.lyrics.is_empty()).then_some(0));
            self.lyrics_active = active;
            if let Some(index) = active {
                self.lyrics_scroll
                    .scroll_to_item_strict(index, gpui::ScrollStrategy::Center);
            }
        }
        let element = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            // 根元素持焦点 + 挂基础上下文：带谓词的快捷键绑定要有非空上下文栈才生效。
            .track_focus(&self.workspace_focus)
            .key_context(crate::shortcuts::BASE_CONTEXT)
            // macOS 隐藏系统标题栏，顶部留出红绿灯的高度；
            // Windows 隐藏系统标题栏后由应用自绘拖拽区与控制按钮。
            .when(cfg!(target_os = "macos"), |this| this.pt(px(28.0)))
            .when(cfg!(target_os = "windows"), |this| {
                this.child(ui::titlebar::render())
            })
            .bg(theme::ambient_background())
            .text_color(ui::theme::text())
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(ui::sidebar::render(self, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(views::render(self, window, cx)),
                    )
                    // 右侧面板**叠加**在内容之上，而不是挤压内容区。
                    //
                    // 之前是 flex 兄弟节点（`w(320) flex_none`）：打开抽屉会把
                    // 列表压窄 —— 表格列宽是按容器算的，一压就错位（`mod.rs`
                    // 的 `columns_for` 依赖可用宽度）。播放页不受影响：它是
                    // 封面 + 歌词的自适应布局，本来就有缩放兜底。
                    //
                    // 宽度在 Rust 侧按视口算成确定像素（`theme::overlay_panel_width`）：
                    // 叠加层是 `width: auto` 的绝对定位盒子，交给样式系统的
                    // `relative(..)` 会按元素自身宽度解析（320 × 0.4 = 128px）。
                    .when(self.queue_open && self.nav != Nav::Lyrics, |this| {
                        this.child(div().absolute().top(px(0.0)).right(px(0.0)).h_full().child(
                            ui::player_bar::queue_drawer(
                                self,
                                ui::theme::overlay_panel_width(
                                    viewport_width,
                                    ui::player_bar::DRAWER_W,
                                ),
                                cx,
                            ),
                        ))
                    })
                    .when(self.download_open, |this| {
                        this.child(div().absolute().top(px(0.0)).right(px(0.0)).h_full().child(
                            views::downloads::panel(
                                self,
                                ui::theme::overlay_panel_width(
                                    viewport_width,
                                    views::downloads::PANEL_W,
                                ),
                                cx,
                            ),
                        ))
                    }),
            )
            .child(ui::player_bar::render(self, cx))
            .when(self.track_menu.is_some(), |this| {
                this.child(crate::views::track_menu(self, cx))
            })
            .when(self.local_menu.is_some(), |this| {
                this.child(crate::views::local_music::local_menu(self, cx))
            })
            .when(self.match_picker.is_some(), |this| {
                this.child(crate::views::local_music::match_picker(self, cx))
            })
            .when(
                self.login_modal_open && self.settings.cookie.trim().is_empty(),
                |this| this.child(crate::views::login_modal(self, cx)),
            )
            .when(
                self.queue_menu.is_some() || self.track_menu.is_some() || self.local_menu.is_some(),
                |this| {
                    // 透明遮罩：点任何地方都关掉右键菜单；
                    // 菜单本身层级更高（priority 2 > 1），点菜单不会走到这里。
                    this.child(
                        gpui::deferred(
                            div()
                                .id("menu-scrim")
                                .absolute()
                                .top(px(0.0))
                                .left(px(0.0))
                                .size_full()
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(|root, _event, _window, cx| {
                                        if root.queue_menu.is_some()
                                            || root.track_menu.is_some()
                                            || root.local_menu.is_some()
                                        {
                                            root.queue_menu = None;
                                            root.track_menu = None;
                                            root.local_menu = None;
                                            cx.notify();
                                        }
                                    }),
                                ),
                        )
                        .with_priority(1),
                    )
                },
            );
        if std::env::var("SODAM_FRAME_LOG").is_ok() {
            let elapsed = frame_started.elapsed().as_millis();
            if elapsed > 16 {
                let rows = match self.nav {
                    Nav::Search => self.results.len(),
                    Nav::Liked => self.liked.len(),
                    Nav::Library => self.playlists.len(),
                    _ => 0,
                };
                eprintln!("[frame] {elapsed}ms nav={:?} rows={rows}", self.nav);
            }
        }
        // 给整棵子树挂上有上界的封面图片缓存：超预算的封面会被释放，
        // 不挂的话每张见过的封面都会永远留在显存里（见 ui/cover_cache.rs）。
        gpui::image_cache(self.cover_cache.clone())
            .size_full()
            .child(element)
    }
}
