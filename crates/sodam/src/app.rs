//! 应用根视图：状态定义、初始化、输入处理与顶层渲染。

#[path = "app_actions.rs"]
mod actions;
#[path = "app_download.rs"]
mod download_actions;
#[path = "app_search.rs"]
mod search_actions;

use gpui::prelude::*;
use gpui::{
    div, px, Context, Decorations, EntityInputHandler, FocusHandle, Image, Render, UTF16Selection,
    Window,
};
use sodam_core::{
    library::{RecommendationSource, SceneCard, SearchScope},
    log_outlet::{self, log_event, LogLevel, LogTopic},
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

/// 远程读取类失败的统一说法：读不到歌单、收藏、搜索、推荐都是这一句。
pub(crate) const REMOTE_TEXT: &str = "出现了一点问题，检查网络后重试。";
/// 本地保存/写入类失败的统一说法：这类失败和网络无关。
pub(crate) const SAVE_TEXT: &str = "没能保存，检查磁盘空间后重试。";
/// 既不是网络也不是写入的失败（例如本地编码出错）：不指责任何一方，只说重试。
pub(crate) const RETRY_TEXT: &str = "出现了一点问题，请重试。";

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
use crate::ui::text_field::TextField;
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

impl QueueOrigin {
    /// 是否为推荐 / 听歌模式这类无限流。
    ///
    /// 这类队列的歌由服务端持续追加，播放模式没有意义：单曲循环会让流停住，
    /// 随机也打不乱服务端的推荐节奏。播放器栏因此隐藏模式按钮。
    pub fn is_feed_like(&self) -> bool {
        matches!(
            self,
            QueueOrigin::Feed | QueueOrigin::FeedMode(_) | QueueOrigin::Radio(_)
        )
    }
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

/// 当前接收编辑键的输入框。
///
/// `app_text` 的所有动作都通过 `Root::active_field_mut` 分发到这里选中的那一份，
/// 不再硬编码 `search_field` —— 否则在「本地音乐」的过滤框里打字会写进搜索页。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveInput {
    /// 顶部搜索页的搜索框。
    Search,
    /// 「本地音乐 → 本地音乐」页的过滤框。
    LocalFilter,
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
    /// 自绘标题栏拖拽区的按下锚点：按下只记录，移动超过阈值才交给合成器，
    /// 否则合成器会抢走指针，双击最大化收不到第二次按下（见 ui::titlebar）。
    pub(crate) titlebar_anchor: ui::titlebar::DragAnchor,
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
    /// 全局轻提示：每次 status 更新都递增序号，并记录起始时间。
    pub(crate) status_seq: u64,
    pub(crate) status_since: Option<std::time::Instant>,
    /// 全局 message 队列与计时去重集合。
    pub(crate) messages: Vec<ui::message::Message>,
    pub(crate) next_message_id: u64,
    pub(crate) message_timers: HashSet<u64>,
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
    /// 歌词区自己管滚动（外层是普通滚动容器，不是虚拟列表），
    /// 这样长句折行后行高不一致也能正确定位。
    pub lyrics_scroll: gpui::ScrollHandle,
    /// 用户手动滚过歌词后，暂停自动跟随到这个时刻。
    ///
    /// 官方客户端是「滚轮后 5 秒内不跟着进度跳」，这里沿用同一策略，
    /// 否则用户往回翻歌词会被立刻拽回当前句。
    pub lyrics_user_scroll_until: Option<std::time::Instant>,
    /// 等待下一帧补做的歌词对齐（布局未就绪时记下目标句）。
    pub(crate) pending_lyrics_scroll: Option<usize>,
    /// 防止快速换歌时旧歌词请求回写。
    lyrics_request_seq: u64,
    pub search_field: TextField,
    /// 「本地音乐」页的过滤框。
    ///
    /// 和 `search_field` 共用 [`app_text`] 的编辑动作（通过 [`ActiveInput`] 分发），
    /// 但两者状态独立：在本地音乐里打字不会污染搜索页的查询。
    pub local_filter_field: TextField,
    /// 当前该由哪个输入框接收编辑键。
    ///
    /// 由各自的 `FocusHandle` 在渲染时置位；没有聚焦任何输入框时是 `None`，
    /// 此时编辑动作静默丢弃（不会误改搜索框）。
    pub active_input: Option<ActiveInput>,
    pub local_filter_focus: FocusHandle,
    /// 搜索框文本区的窗口内 bounds：canvas 绘制时回填，鼠标点击定位用。
    pub(crate) search_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    /// 当前鼠标拖拽是否按词扩选（双击起步的拖拽为真；松开鼠标复位）。
    pub(crate) search_drag_by_word: bool,
    /// 撤销合并窗口的单调时钟起点（见 [`Root::search_edit_time`]）。
    search_edit_epoch: std::time::Instant,
    pub search_focus: FocusHandle,
    /// 「按下 Ctrl+F，等搜索框进树之后把焦点给它」的待办标记。
    ///
    /// 只在 `app_text::Root::focus_search` 里置位，在 `Root::render` 开头消费。
    pub(crate) search_focus_pending: bool,
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
    /// 录制时的行内提示（徽标键被拒、按键无法绑定）：下一次按键、取消或离开设置页都会清掉。
    ///
    /// 不走 `status` —— 那个字段现在没有任何地方渲染（见 #40），提示必须长在出问题的行上。
    pub shortcut_note: Option<&'static str>,
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
    pub(crate) preferences_writer: sodam_core::preferences::PreferencesWriter,
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
    /// 播放请求序号：每次 start_track +1。
    ///
    /// 作用：加载是异步的（下载 + 解密），如果用户在 A 加载途中换了 B，
    /// A 完成时必须**作废**，否则会把正在播的 B 顶掉（实测踩过：
    /// B 播一会儿后突然跳回 A，封面也对不上）。
    pub(crate) play_seq: u64,
    /// 播放错误（显示在播放栏：常驻状态条已移除，错误必须有地方可见）。
    pub playback_error: Option<String>,
    /// 连续装载失败次数（>=3 就停下，避免无限跳歌）。
    ///
    /// 清零判据是「最近一次成功装载真的播出去若干秒」，不是「此刻有声」：
    /// 见 `AttemptProgress::recovered` 与下面的 `progress_token`。
    pub(crate) consecutive_failures: u32,
    /// 最近一次**成功装载**交给引擎的序号；任何一次装载失败都把它作废。
    ///
    /// 连败复位只认「这个序号 == 引擎当前快照的 load_seq」，也就是进展必须归属于
    /// 最新那次尝试。没有它，上一首还在放就会把计数每拍清零，封顶永远触发不了。
    pub(crate) progress_token: Option<u64>,
    /// 连败封顶闸门：打开后自动续播（播完切下一首、预取巡检）全部停住，
    /// 直到用户主动播放。只写 `playing = false` 拦不住——引擎仍在出声，
    /// 「已暂停」就成了谎话。
    pub(crate) playback_halted: bool,
    /// 正在预取的曲目 id 集合（多首并行去重；预取结果只写缓存，不改 UI）。
    pub(crate) prefetch_inflight: HashSet<String>,
    /// 预取失败表（曲目 id →（最近失败时刻，可否重试））。
    ///
    /// 冷却类期满可以再排；**永久失败（下架 / 无权益 / 签名服务鉴权）不再自动排**——
    /// 巡检每 5 秒重排一次就是在持续打扰全体用户共享的那台签名服务。
    pub(crate) prefetch_failed:
        HashMap<String, (std::time::Instant, sodam_core::failure::Retryability)>,
    /// 预取巡检节拍：心跳每 200ms 自增，每 25 拍（约 5s）补一次预取。
    pub(crate) prefetch_patrol: u32,
    /// 缓存统计快照（音频字节, 歌曲数, 封面字节, 封面张数）：
    /// 后台扫盘后回填，设置页只读它（渲染路径不做同步 IO）。
    pub(crate) cache_summary: (u64, usize, u64, usize),
    pub(crate) cache_maintenance_running: bool,
    pub(crate) cache_clearing: bool,
    /// 正在删除诊断日志；只用于禁用重复点击，与缓存清理互不影响。
    pub(crate) log_clearing: bool,
    /// 下载完成后交给音频线程的装载序号；避免同一曲目重载/失败时永久 loading。
    pub(crate) pending_engine_load: Option<u64>,
    /// 正在加载（下载/解密）的曲目：非空时播放栏显示 loading，且进度条不可拖。
    pub pending_track: Option<TrackItem>,
    /// 已处理过的完成事件（曲目结束序号 + 跳转序号）。
    pub(crate) last_finished_seq: u64,
    pub(crate) last_seek_seq: u64,
    /// 拖动进度条时的预览位置（0.0~1.0）：拖动中只改它，松手才真跳。
    pub progress_preview: Option<f32>,
    /// 歌词页大封面：切曲后的 `(track_id, 开始时刻)`；管「至少 200ms 加载态」的门。
    pub lyrics_cover_gate: Option<(String, std::time::Instant)>,
    /// 队列快照（含版本号）：避免抽屉每帧深拷贝整条队列。
    pub queue_cache: (u64, Arc<Vec<TrackItem>>),
    /// 已尝试过的封面 URL（成功在 covers 里，失败的也记下来，避免每帧重复排队）。
    pub cover_attempted: Arc<Mutex<HashSet<String>>>,
    /// 封面缓存：URL → 本地文件。**只由后台任务写入**，渲染路径只读，
    /// 绝不在 render 里发网络请求（否则主线程会被下载卡住）。
    pub covers: Arc<HashMap<String, PathBuf>>,
    pub original_covers: HashMap<String, PathBuf>,
    pub original_cover_attempted: Arc<Mutex<HashSet<String>>>,
    /// “其他”里的贡献者图（contrib.rocks 组合 SVG）；独立缓存，不参与歌曲封面统计。
    pub contributors_image: Option<Arc<Image>>,
    pub(crate) contributors_fetching: bool,
    pub(crate) contributors_failed: bool,
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
        let mut settings = Settings::load().merged_with_env();
        let engine = PlaybackEngine::with_volume(settings.volume);
        settings.volume = engine.snapshot().volume;
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
        let mut queue = Queue::new(Vec::<TrackItem>::new());
        // 播放模式跨启动保留（设置页没做这一项，按钮在播放栏）。
        queue.mode = settings.play_mode;
        let engine_progress = engine.snapshot();

        let logged_in = !settings.cookie.trim().is_empty();
        let play_reporter = PlayReporter::new(settings.clone());
        let downloads = DownloadManager::new(settings.clone());
        downloads.set_download_settings(settings.download.clone());
        let mut root = Self {
            titlebar_anchor: std::rc::Rc::new(std::cell::Cell::new(None)),
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
            play_seq: 0,
            playback_error: None,
            consecutive_failures: 0,
            progress_token: None,
            playback_halted: false,
            playing: false,
            recommendation: None,
            loading_recommendation: false,
            switching_queue: None,
            switching_label: language.text("推荐队列").to_string(),
            recommendation_request_seq: 0,
            status,
            status_seq: 0,
            status_since: None,
            messages: Vec::new(),
            next_message_id: 0,
            message_timers: HashSet::new(),
            lyrics: Arc::new(Vec::new()),
            lyrics_track_id: String::new(),
            lyrics_title: String::new(),
            lyrics_artist: String::new(),
            lyrics_cover: String::new(),
            loading_lyrics: false,
            lyrics_error: None,
            lyrics_active: None,
            lyrics_scroll: gpui::ScrollHandle::new(),
            lyrics_user_scroll_until: None,
            pending_lyrics_scroll: None,
            lyrics_request_seq: 0,
            search_field: TextField::default(),
            local_filter_field: TextField::default(),
            active_input: None,
            local_filter_focus: cx.focus_handle(),
            search_drag_by_word: false,
            search_edit_epoch: std::time::Instant::now(),
            search_bounds: std::rc::Rc::new(std::cell::Cell::new(None)),
            search_focus: cx.focus_handle(),
            search_focus_pending: false,
            workspace_focus: cx.focus_handle(),
            shortcut_focus: cx.focus_handle(),
            shortcut_capture: None,
            shortcut_note: None,
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
            preferences_writer: sodam_core::preferences::PreferencesWriter::new(
                Settings::config_path(),
            ),
            progress_track_bounds: Arc::new(Mutex::new(None)),
            volume_track_bounds: Arc::new(Mutex::new(None)),
            prefetch_inflight: HashSet::new(),
            prefetch_failed: HashMap::new(),
            prefetch_patrol: 0,
            cache_summary: (0, 0, 0, 0),
            cache_maintenance_running: false,
            cache_clearing: false,
            log_clearing: false,
            pending_engine_load: None,
            pending_track: None,
            last_finished_seq: engine_progress.finished_seq,
            last_seek_seq: engine_progress.seek_seq,
            progress_preview: None,
            lyrics_cover_gate: None,
            queue_cache: (0, Arc::new(Vec::new())),
            cover_attempted: Arc::new(Mutex::new(HashSet::new())),
            cover_requests: Arc::new(Mutex::new(Vec::new())),
            covers: Arc::new(HashMap::new()),
            original_covers: HashMap::new(),
            original_cover_attempted: Arc::new(Mutex::new(HashSet::new())),
            contributors_image: None,
            contributors_fetching: false,
            contributors_failed: false,
            ambient_colors: HashMap::new(),
            cover_pool: CoverPool::new(),
            login: LoginState::Idle,
            account: None,
            engine,
            play_reporter,
            downloads,
            download_open: false,
            download_tab: DownloadTab::default(),
            download_cache: (u64::MAX, Arc::new(Vec::new())),
            download_counts: DownloadCounts::default(),
            download_settings_fingerprint: 0,
        };
        cx.on_app_quit(|root, _cx| {
            sodam_core::flush_global();
            root.preferences_writer.flush()
        })
        .detach();
        root.maintain_cache(cx);
        if logged_in {
            root.refresh_account(cx);
            root.load_liked_ids(cx);
            root.start_recommendation(false, cx);
        } else {
            root.set_status_text(root.tr("请先在「设置 → 账户」扫码登录"));
        }
        // 本地目录扫描与登录无关（纯本地文件系统），启动就快照一次。
        root.refresh_local(cx);
        // 开发/验证便利：`SODAM_QUERY="周杰伦" cargo run -p sodam` 启动即搜索一次
        if let Ok(query) = std::env::var("SODAM_QUERY") {
            if !query.trim().is_empty() {
                root.search_field.set_text(query);
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
        // 配合 `SODAM_FRAME_LOG=1`（把 app 话题提到 DEBUG）就能量出「滑动是否掉帧」，不用手工滚。
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
                    log_event(
                        LogLevel::Debug,
                        LogTopic::Player,
                        "race_probe_skip",
                        &[("reason", "no_uncached_tracks")],
                    );
                    return;
                };
                let (first, second) = (picks[0].to_string(), picks[1].to_string());
                log_event(
                    LogLevel::Debug,
                    LogTopic::Player,
                    "race_probe_pick",
                    &[("a", first.as_str()), ("b", second.as_str())],
                );
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

/// 搜索框光标的宽度。绘制（`views::search_box`）与滚动判定（`search_fit_caret`、
/// `search_max_scroll`）必须共用这一个值：只按 0 宽度算滚动的话，滚到最右时光标正好
/// 压在 canvas 的裁剪线上，整个被裁掉，看起来就是「末尾没有光标」。
pub(crate) const CARET_WIDTH: gpui::Pixels = gpui::px(2.0);

impl Root {
    /// 按当前 text style 把搜索框内容 shape 成一行。
    ///
    /// 光标位置和点击定位都用**同一条** ShapedLine 量，避免"自己估宽度"和
    /// 实际渲染不一致导致的光标错位（sonora 的 Input 也是这么做的）。
    fn search_shaped_line(&self, window: &Window) -> Option<gpui::ShapedLine> {
        self.search_shaped_line_with(window, theme::text())
    }

    /// 同上，但文字色由调用方给：视图现在要自己 `line.paint` 真实文本（好跟着横向滚动走），
    /// 颜色必须跟 `theme::text()` 一致，不能退回窗口默认色。
    pub(crate) fn search_shaped_line_with(
        &self,
        window: &Window,
        color: gpui::Rgba,
    ) -> Option<gpui::ShapedLine> {
        if self.search_field.is_empty() {
            return None;
        }
        let style = window.text_style();
        let run = gpui::TextRun {
            len: self.search_field.len(),
            font: style.font(),
            color: gpui::Hsla::from(color),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        Some(window.text_system().shape_line(
            self.search_field.text().into(),
            style.font_size.to_pixels(window.rem_size()),
            &[run],
            None,
        ))
    }

    /// 鼠标点击/拖拽位置的窗口内 x → 最近的字符边界（字节下标）。
    ///
    /// 文本是按 `-scroll_x` 绘制的，所以命中测试要先把滚走的那段加回来，否则滚过之后
    /// 点哪儿都会偏。
    pub(crate) fn search_cursor_for_x(&self, x: gpui::Pixels, window: &Window) -> usize {
        let Some(line) = self.search_shaped_line(window) else {
            return 0;
        };
        // canvas 就铺在文本盒上，bounds.left 就是文本起点。
        let origin = self
            .search_bounds
            .get()
            .map(|bounds| bounds.origin.x)
            .unwrap_or_else(|| gpui::px(0.0));
        let local = x - origin + self.search_scroll_x();
        if local <= gpui::px(0.0) {
            return 0;
        }
        line.closest_index_for_x(local).min(self.search_field.len())
    }

    /// 文本坐标系（未计滚动）下某个字节下标的横向位置。
    fn search_text_x(&self, index: usize, window: &Window) -> gpui::Pixels {
        match self.search_shaped_line(window) {
            Some(line) => line.x_for_index(index),
            None => gpui::px(0.0),
        }
    }

    pub(crate) fn search_scroll_x(&self) -> gpui::Pixels {
        gpui::px(self.search_field.scroll_x())
    }

    /// 撤销合并用的时间戳（毫秒，自 `Root` 构造起算）。
    ///
    /// `TextField` 刻意不读时钟：读了就写不出确定性的单测，500ms 合并窗口也没法验证。
    /// 所以时钟留在这里，状态层只接收算好的数。
    pub(crate) fn search_edit_time(&self) -> crate::ui::text_field::Milliseconds {
        self.search_edit_epoch.elapsed().as_millis() as u64
    }

    /// 插入点在输入框内的横向偏移：**已扣掉滚动量**，可以直接当元素的 `left`。
    pub(crate) fn search_caret_x(&self, window: &Window) -> gpui::Pixels {
        let caret = self.search_field.caret();
        self.search_text_x(caret, window) - self.search_scroll_x()
    }

    /// 选区左右两条边（文本坐标系）。无选区时 `None`。
    ///
    /// 单行框的选区必然落在同一行上，所以两端就够，不用像多行编辑器那样逐行切矩形。
    pub(crate) fn search_selection_edges(
        &self,
        window: &Window,
    ) -> Option<(gpui::Pixels, gpui::Pixels)> {
        if !self.search_field.has_selection() {
            return None;
        }
        let range = self.search_field.selection();
        Some((
            self.search_text_x(range.start, window),
            self.search_text_x(range.end, window),
        ))
    }

    /// 文本能横向滚动的最大距离；文本比视口窄时为 0。
    ///
    /// 多留出 `CARET_WIDTH`：插入点在最后一段文字之后时它本身就落在文本宽度上，
    /// 不留这 2px 就永远滚不进视区。
    pub(crate) fn search_max_scroll(&self, window: &Window, width: gpui::Pixels) -> gpui::Pixels {
        let text_width = self
            .search_shaped_line(window)
            .map(|line| line.width())
            .unwrap_or_else(|| gpui::px(0.0));
        (text_width + CARET_WIDTH - width).max(gpui::px(0.0))
    }

    /// 把活动端拉进可视区，只滚到刚好露出（不居中）。返回是否滚了。
    pub(crate) fn search_fit_caret(&mut self, window: &Window, width: gpui::Pixels) -> bool {
        let caret_x = f32::from(self.search_text_x(self.search_field.caret(), window));
        let max = f32::from(self.search_max_scroll(window, width));
        self.search_field
            .ensure_visible(caret_x, f32::from(CARET_WIDTH), f32::from(width), max)
    }

    /// 同上，但宽度取上次绘制记下的文本区宽度（按键路径拿不到布局，只能这样）。
    pub(crate) fn search_fit_caret_in_view(&mut self, window: &Window) {
        let Some(bounds) = self.search_bounds.get() else {
            return;
        };
        self.search_fit_caret(window, bounds.size.width);
    }

    /// 鼠标按下：单击定位、双击选词、三击全选（Chromium 的单行框三击就是 SelectAll）。
    /// 同时记下这次的拖拽粒度，供随后的拖拽沿用。
    pub(crate) fn search_pointer_down(
        &mut self,
        x: gpui::Pixels,
        click_count: usize,
        window: &Window,
    ) -> bool {
        let index = self.search_cursor_for_x(x, window);
        self.search_drag_by_word = click_count >= 2;
        match click_count {
            0 | 1 => self.search_field.set_caret(index),
            2 => self.search_field.select_word_at(index),
            _ => self.search_field.select_all(),
        }
    }

    /// 拖拽扩选：双击起步的拖拽按整词扩（Chromium 会记住 `double_click_word_`，
    /// 否则双击后一拖就退化成字符级），普通拖拽逐字符。
    pub(crate) fn search_pointer_drag(&mut self, x: gpui::Pixels, window: &Window) -> bool {
        let index = self.search_cursor_for_x(x, window);
        if self.search_drag_by_word {
            self.search_field.select_words_through(index)
        } else {
            self.search_field.select_through(index)
        }
    }

    /// 松开鼠标：结束按词拖拽，下一次按下重新判定。
    pub(crate) fn search_pointer_up(&mut self) {
        self.search_drag_by_word = false;
    }

    /// 拖拽选区时按 `delta` 横滚，夹在 `[0, max]` 内。返回是否滚了。
    pub(crate) fn search_scroll_by(
        &mut self,
        delta: gpui::Pixels,
        window: &Window,
        width: gpui::Pixels,
    ) -> bool {
        let max = f32::from(self.search_max_scroll(window, width));
        let desired = self.search_field.scroll_x() + f32::from(delta);
        self.search_field.set_scroll_x(desired, max)
    }
}

/// gpui 的输入协议桥：把平台的 UTF-16 区间换算交给 [`TextField`]，
/// 这里只留「平台说了什么」→「状态怎么变」这一层，外加焦点门禁。
///
/// 两个输入框共用这套实现：每个方法先问「当前聚焦的是哪一个」，
/// 再决定读写哪份 [`TextField`]。焦点门禁也分两路，避免过滤框拿到搜索框的候选窗。
impl Root {
    /// 当前聚焦的输入框；没聚焦任何一个时返回 `None`。
    fn focused_input(&self, window: &Window) -> Option<ActiveInput> {
        if self.search_focus.is_focused(window) {
            Some(ActiveInput::Search)
        } else if self.local_filter_focus.is_focused(window) {
            Some(ActiveInput::LocalFilter)
        } else {
            None
        }
    }

    fn focused_field_mut(&mut self, window: &Window) -> Option<&mut TextField> {
        match self.focused_input(window)? {
            ActiveInput::Search => Some(&mut self.search_field),
            ActiveInput::LocalFilter => Some(&mut self.local_filter_field),
        }
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
        let field = self.focused_field_mut(window)?;
        let range = field.byte_range_from_utf16(range_utf16);
        actual_range.replace(field.utf16_range_from_byte(range.clone()));
        Some(field.slice(range))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let field = self.focused_field_mut(window)?;
        // 组合中上报组合区间，否则上报选区（无选区时是活动端的折叠区间）。
        // `reversed` 要照实给：方向决定 Shift 继续扩选时往哪边长，macOS 的候选窗也读它。
        Some(UTF16Selection {
            range: field.utf16_range_from_byte(field.selection_range()),
            reversed: field.is_reversed(),
        })
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        let input = self.focused_input(window)?;
        let field = match input {
            ActiveInput::Search => &self.search_field,
            ActiveInput::LocalFilter => &self.local_filter_field,
        };
        field
            .marked()
            .map(|range| field.utf16_range_from_byte(range))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(field) = self.focused_field_mut(window) else {
            return;
        };
        if field.clear_marked() {
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
        let Some(input) = self.focused_input(window) else {
            return;
        };
        let now = self.search_edit_time();
        {
            let Some(field) = self.focused_field_mut(window) else {
                return;
            };
            let range = range_utf16.map(|range| field.byte_range_from_utf16(range));
            field.begin_edit();
            field.replace(range, text);
            field.end_edit(now);
        }
        if input == ActiveInput::Search {
            self.search_fit_caret_in_view(window);
        }
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
        let Some(input) = self.focused_input(window) else {
            return;
        };
        let now = self.search_edit_time();
        {
            let Some(field) = self.focused_field_mut(window) else {
                return;
            };
            let range = range_utf16.map(|range| field.byte_range_from_utf16(range));
            field.begin_edit();
            field.replace_and_mark(range, new_text);
            field.end_edit(now);
        }
        if input == ActiveInput::Search {
            self.search_fit_caret_in_view(window);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        element_bounds: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        // 过滤框不做精确排版（见 `local_filter_box`），候选窗直接贴在整框上。
        if self.focused_input(window) == Some(ActiveInput::LocalFilter) {
            return Some(element_bounds);
        }
        self.focused_input(window)?;
        // 候选窗要贴在真正在编辑的那一段下面：整框宽度当锚点会让候选窗一直停在框的左下，
        // 滚过长文本之后更是完全对不上。这里与 `search_cursor_for_x` 共用同一条排版结果。
        let range = self.search_field.byte_range_from_utf16(range_utf16);
        let left = self.search_text_x(range.start, window) - self.search_scroll_x();
        let right = self.search_text_x(range.end, window) - self.search_scroll_x();
        let bounds = self.search_bounds.get().unwrap_or(element_bounds);
        let width = gpui::px(1.0).max(right - left);
        Some(gpui::Bounds {
            origin: gpui::point(bounds.origin.x + left, bounds.origin.y),
            size: gpui::size(width, bounds.size.height.max(gpui::px(1.0))),
        })
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        match self.focused_input(window)? {
            // 过滤框没有排版结果，点击定位交给 `on_mouse_down` 的粗略处理。
            ActiveInput::LocalFilter => None,
            ActiveInput::Search => {
                // 语义是 UTF-16 偏移：按点击位置算出字节下标后再换算。
                let byte = self.search_cursor_for_x(point.x, window);
                Some(self.search_field.byte_to_utf16(byte))
            }
        }
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.focused_input(window) else {
            return;
        };
        let Some(field) = self.focused_field_mut(window) else {
            return;
        };
        let range = field.byte_range_from_utf16(range_utf16);
        // 平台交回的是整段区间，此前只取 `start` 会把选区丢掉；这里按真选区收下，
        // 活动端落在区间末尾（方向由两端的大小关系决定）。
        field.set_position(range.end, range.start);
        if input == ActiveInput::Search {
            self.search_fit_caret_in_view(window);
        }
        cx.notify();
    }

    fn text_length_utf16(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        Some(self.focused_field_mut(window)?.utf16_len())
    }

    fn accepts_text_input(&self, window: &mut Window, _cx: &mut Context<Self>) -> bool {
        self.focused_input(window).is_some()
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
        self.set_status_text(self.localized(key, args));
    }

    /// 普通操作反馈：只进入底部状态行，不触发全局 message。
    pub fn set_status_text(&mut self, text: impl Into<String>) {
        self.status = sodam_core::failure::strip_urls_for_user(&text.into());
        self.show_status();
    }

    /// 技术原文的唯一去处：诊断日志（默认开，写在本机日志目录）。
    /// 界面文案一律不用它——用户只看人话。
    ///
    /// 话题是 `app`：设置保存、登录、搜索、歌单这些非播放链路的失败都归这里。
    /// 播放链路的失败另有 `player` 话题的尝试轨迹，不靠这条。
    /// 进这里的内容不需要自己脱敏：日志出口在写盘前统一切断查询串与凭据。
    pub(crate) fn log_failure(&self, context: &str, error: impl std::fmt::Display) {
        let detail = error.to_string();
        log_event(
            LogLevel::Error,
            LogTopic::App,
            "failure",
            &[("context", context), ("detail", detail.as_str())],
        );
    }

    /// 记录技术原文，并返回给用户看的那一句。
    ///
    /// 界面文案一律走这里，不再复述 `{err}`：普通用户看 `tls connection init failed`
    /// 得不到任何行动信息，而它也不该被藏进折叠区——原文的去处是日志。
    pub(crate) fn failure_text(
        &self,
        context: &str,
        error: impl std::fmt::Display,
        text: &'static str,
    ) -> String {
        self.log_failure(context, error);
        self.tr(text).to_string()
    }

    /// 重要成功结果：状态 + 全局轻提示。
    #[allow(dead_code)] // 公共 API 给不重复的 user-triggered success
    pub fn notify_success(&mut self, text: impl Into<String>) -> u64 {
        self.notify_message(ui::message::MessageKind::Success, text.into(), None)
    }

    /// 同一来源的成功提示只保留一条。
    pub fn notify_success_once(&mut self, key: &str, text: impl Into<String>) -> u64 {
        self.notify_message(ui::message::MessageKind::Success, text.into(), Some(key))
    }

    /// 重要警告：状态 + 全局轻提示。
    #[allow(dead_code)] // 公共 API 给不重复的 user-triggered warning
    pub fn notify_warning(&mut self, text: impl Into<String>) -> u64 {
        self.notify_message(ui::message::MessageKind::Warning, text.into(), None)
    }

    /// 同一来源的警告只保留一条。
    pub fn notify_warning_once(&mut self, key: &str, text: impl Into<String>) -> u64 {
        self.notify_message(ui::message::MessageKind::Warning, text.into(), Some(key))
    }

    /// 需要用户注意的可恢复错误：状态 + 全局轻提示。
    #[allow(dead_code)] // 留给不重复的 user-triggered errors
    pub fn notify_error(&mut self, text: impl Into<String>) -> u64 {
        self.notify_message(ui::message::MessageKind::Error, text.into(), None)
    }

    /// 同一来源的重复错误替换旧提示，避免轮询/重试堆成消息墙。
    pub fn notify_error_once(&mut self, key: &str, text: impl Into<String>) -> u64 {
        self.notify_message(ui::message::MessageKind::Error, text.into(), Some(key))
    }

    fn notify_message(
        &mut self,
        kind: ui::message::MessageKind,
        text: String,
        key: Option<&str>,
    ) -> u64 {
        // 上屏前摘掉接口地址：内部端点对普通用户既无意义又泄露拓扑；带签名 token 的查询串
        // 更早就不该出现。给维护者看的原文留给日志（`PlaybackFailure::detail`）。
        self.status = sodam_core::failure::strip_urls_for_user(&text);
        self.show_status();
        self.push_message_with_key(
            kind,
            self.status.clone(),
            Some(ui::message::DEFAULT_DURATION),
            key,
        )
    }

    /// 记录最新状态。
    pub(crate) fn show_status(&mut self) {
        self.status_seq = self.status_seq.wrapping_add(1);
        self.status_since = Some(std::time::Instant::now());
    }

    pub fn set_language(&mut self, language: Language, cx: &mut Context<Self>) {
        self.language = language;
        self.language_follows_system = false;
        self.settings.language = language.key().to_string();
        let save_result = self.settings.save();
        match save_result {
            Ok(()) => {
                let status = if language.is_zh() {
                    language.text("语言已切换为中文").to_string()
                } else {
                    language.text("Language switched to English").to_string()
                };
                self.set_status_text(status);
            }
            Err(err) => {
                let message = self.failure_text("语言已切换，但保存失败", &err, SAVE_TEXT);
                self.notify_warning_once("language-save-failed", message);
            }
        }
        // macOS 菜单栏的标题也要跟着换语言。
        #[cfg(target_os = "macos")]
        crate::menus::refresh(cx, language);
        cx.notify();
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Windows 自绘标题栏（系统标题栏被 WM_NCCALCSIZE 去掉）；macOS 用系统红绿灯。
        // Linux 交给设置项：默认 auto 跟随桌面协商结果 —— GNOME 这类要求客户端装饰的
        // 桌面需要自绘，Hyprland/sway 这类平铺桌面协商成服务端装饰但不画标题栏，
        // 用户也可以在设置里强制显示或隐藏。
        let own_titlebar = if cfg!(target_os = "windows") {
            true
        } else if cfg!(target_os = "macos") {
            false
        } else {
            sodam_core::config::TitlebarMode::from_key(&self.settings.titlebar).draws_titlebar(
                matches!(window.window_decorations(), Decorations::Client { .. }),
            )
        };
        let titlebar_anchor = self.titlebar_anchor.clone();
        // 没有元素聚焦时把焦点收回根元素：gpui 的上下文栈取自聚焦节点的路径，
        // 空栈会让带谓词的快捷键绑定（播放三键）全部失效。
        if window.focused(cx).is_none() {
            window.focus(&self.workspace_focus, cx);
        }
        // Ctrl+F 的焦点不在动作里落：从别的页过来时搜索框要到这一帧的渲染才进 dispatch
        // tree，早一帧 focus 拿不到 `input` 上下文。这里与上面收回根焦点是同一处时机。
        if self.search_focus_pending {
            self.search_focus_pending = false;
            window.focus(&self.search_focus, cx);
        }
        // 开发期帧耗时探针（`SODAM_FRAME_LOG=1` 把 app 话题提到 DEBUG）：渲染超过 16ms 就打一行。
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
        if log_outlet::enabled(LogLevel::Debug, LogTopic::App) {
            let current = self
                .queue
                .current()
                .map(|track| track.title.clone())
                .unwrap_or_default();
            let url = ambient_url.clone().unwrap_or_default();
            let color = ambient_color
                .map(|rgb| format!("{rgb:?}"))
                .unwrap_or_else(|| "none".to_string());
            log_event(
                LogLevel::Debug,
                LogTopic::App,
                "theme_snapshot",
                &[
                    ("current", current.as_str()),
                    ("url", url.as_str()),
                    ("color", color.as_str()),
                ],
            );
        }
        theme::set_ambient_rgb(ambient_color);
        // 上一帧布局未就绪时记下的歌词对齐，在这里补做一次。
        self.flush_pending_lyrics_scroll();
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
            // 用户刚手动滚过就让位，别把视图拽回去（官方同样是滚轮后 5 秒内不跟随）。
            let user_scrolling = self
                .lyrics_user_scroll_until
                .is_some_and(|until| std::time::Instant::now() < until);
            if let Some(index) = active {
                if !user_scrolling {
                    self.scroll_lyrics_to(index);
                }
            }
            // 切曲后强制大封面显示至少 200ms 的加载态（即便图已在缓存里）。
            if let Some(track) = self.queue.current() {
                let fresh = self
                    .lyrics_cover_gate
                    .as_ref()
                    .map(|(id, _)| id != &track.id)
                    .unwrap_or(true);
                if fresh {
                    self.lyrics_cover_gate = Some((track.id.clone(), std::time::Instant::now()));
                    cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(220))
                            .await;
                        let _ = this.update(cx, |_, cx| cx.notify());
                    })
                    .detach();
                }
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
            // 焦点在窗口内的任何地方都能命中：根元素是全部聚焦节点的共同祖先。
            .on_action(cx.listener(Root::focus_search))
            .bg(theme::ambient_background())
            .text_color(ui::theme::text())
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(ui::sidebar::render(self, cx, own_titlebar))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            // macOS：让红绿灯悬浮在内容之上，内容整体下移 28px。
                            .when(cfg!(target_os = "macos"), |this| this.pt(px(28.0)))
                            // 自绘标题栏：只下移内容，根背景和侧栏仍铺到标题栏顶部。
                            .when(own_titlebar, |this| this.pt(px(ui::titlebar::STRIP_HEIGHT)))
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
                                own_titlebar,
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
                                own_titlebar,
                                cx,
                            ),
                        ))
                    }),
            )
            .child(ui::player_bar::render(self, cx))
            // 标题栏覆盖在各面板背景之上；不再单独占一行而截断侧栏底色。
            // 放在普通内容之后、弹层之前，控制按钮可点击，弹层仍能覆盖标题栏。
            .when(own_titlebar, |this| {
                this.child(ui::titlebar::render(window, &titlebar_anchor))
            })
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
            .when_some(ui::message::render(self, cx), |this, message| {
                this.child(message)
            })
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
        if log_outlet::enabled(LogLevel::Debug, LogTopic::App) {
            let elapsed = frame_started.elapsed().as_millis();
            if elapsed > 16 {
                let rows = match self.nav {
                    Nav::Search => self.results.len(),
                    Nav::Liked => self.liked.len(),
                    Nav::Library => self.playlists.len(),
                    _ => 0,
                };
                let elapsed_ms = elapsed.to_string();
                let nav = format!("{:?}", self.nav);
                let rows = rows.to_string();
                log_event(
                    LogLevel::Debug,
                    LogTopic::App,
                    "frame_slow",
                    &[
                        ("elapsed_ms", elapsed_ms.as_str()),
                        ("nav", nav.as_str()),
                        ("rows", rows.as_str()),
                    ],
                );
            }
        }
        element
    }
}
