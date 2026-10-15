//! 用户动作层：搜索、推荐、媒体库、播放和封面加载。
//!
//! [`Root`] 的字段定义在 [`super`]，这里按领域拆分实现，避免单文件继续膨胀。

use crate::ui::theme::{self, ThemeKind};
use gpui::prelude::*;
use gpui::{Context, Window};
use sodam_core::library::RecommendationSource;
use sodam_core::models::{PlaylistItem, SceneItem, TrackItem};
use sodam_core::session::Session;
use sodam_core::Settings;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use super::{
    status_label, DownloadedIndex, LocalFile, LoginState, Nav, QueueOrigin, Root, ROW_BATCH,
};
use sodam_core::queue::Queue;

/// 本地音乐页合成的曲目 id 前缀（见 `LocalFile::to_track`）。
///
/// 这类曲目在汽水没有对应条目：歌词、上报、网络取流都不该发起。
const LOCAL_TRACK_PREFIX: &str = "local:";

/// 批量匹配时每首之间的间隔：给搜索接口留口气，也让界面有机会刷新。
const MATCH_BATCH_INTERVAL_MS: u64 = 350;

/// 是不是本地文件合成的曲目（没有汽水 `track_id`）。
pub(crate) fn is_local_track(track: &TrackItem) -> bool {
    track.id.starts_with(LOCAL_TRACK_PREFIX)
}

impl Root {
    /// UI 用：这一行是不是本地文件（本地曲目按定义就在磁盘上）。
    pub(crate) fn is_local_track(&self, track: &TrackItem) -> bool {
        is_local_track(track)
    }
    pub(crate) fn search_input_focused(&self, window: &Window) -> bool {
        self.search_focus.is_focused(window)
    }

    pub(crate) fn search_utf16_to_byte_offset(&self, offset: usize) -> usize {
        let mut units = 0;
        for (byte_offset, character) in self.search_input.char_indices() {
            if units >= offset {
                return byte_offset;
            }
            units += character.len_utf16();
        }
        self.search_input.len()
    }

    pub(crate) fn search_byte_to_utf16_offset(&self, offset: usize) -> usize {
        self.search_input
            .char_indices()
            .take_while(|(byte_offset, _)| *byte_offset < offset)
            .map(|(_, character)| character.len_utf16())
            .sum()
    }

    pub(crate) fn search_byte_range_from_utf16(
        &self,
        range: std::ops::Range<usize>,
    ) -> std::ops::Range<usize> {
        let start = self.search_utf16_to_byte_offset(range.start);
        let end = self.search_utf16_to_byte_offset(range.end);
        start.min(end)..start.max(end)
    }

    pub(crate) fn search_utf16_range_from_byte(
        &self,
        range: std::ops::Range<usize>,
    ) -> std::ops::Range<usize> {
        self.search_byte_to_utf16_offset(range.start)..self.search_byte_to_utf16_offset(range.end)
    }

    pub(crate) fn search_replacement_range(
        &self,
        range_utf16: Option<std::ops::Range<usize>>,
    ) -> std::ops::Range<usize> {
        range_utf16
            .map(|range| self.search_byte_range_from_utf16(range))
            .or_else(|| self.search_marked_range.clone())
            .unwrap_or(self.search_input.len()..self.search_input.len())
    }

    /// 预取队列接下来几首到本地缓存（只落盘，不动界面状态）。
    ///
    /// 保持「拉完再播」的简单架构不变，但把等待时间提前吃掉：
    /// 当前曲目一开播，就在后台把接下来几首下好；切歌时命中缓存直接播。
    ///
    /// * 预取数量：`SODAM_PREFETCH_COUNT`（默认 3）；
    /// * 并发上限：`SODAM_PREFETCH_CONCURRENCY`（默认 2，避免签名服务/网络压力）；
    /// * 已缓存 / 正在预取 / 正在装载的曲目跳过，继续看更后面的。
    ///
    /// 触发点：装载成功、切音质、「下一首播放」，以及心跳每 ~5s 的巡检
    /// （后者自动覆盖 append/remove 等队列变更，不必逐个调用点补）。
    pub(crate) fn spawn_prefetch(&mut self, cx: &mut Context<Self>) {
        let log = std::env::var("SODAM_PREFETCH_LOG").is_ok();
        let ahead = prefetch_ahead_count();
        let max_inflight = prefetch_concurrency().max(1);
        let candidates: Vec<TrackItem> = self
            .queue
            .peek_ahead(ahead)
            .iter()
            .map(|track| (*track).clone())
            .collect();
        if candidates.is_empty() {
            if log {
                eprintln!("[prefetch] 队列没有更前面的曲目，跳过");
            }
            return;
        }
        for next in candidates {
            if self.prefetch_inflight.len() >= max_inflight {
                break;
            }
            // 本地曲目不需要预取：文件已经在磁盘上，`start_track` 会直接装载。
            // （否则会拿合成 id 去打网络，白白消耗签名服务。）
            if self.local_playback_path(&next).is_some() {
                if log {
                    eprintln!("[prefetch] 跳过（本地文件）：{}", next.title);
                }
                continue;
            }
            // 已经在缓存里就不用再排（继续看更后面的：多首预取的意义）
            if let Some(session) = &self.session {
                if session.is_cached(&next.id) {
                    if log {
                        eprintln!("[prefetch] 跳过（已缓存）：{}", next.title);
                    }
                    continue;
                }
            }
            // 正在预取 / 正在装载（播放下载中）的曲目不重复排
            if self.prefetch_inflight.contains(&next.id)
                || self
                    .pending_track
                    .as_ref()
                    .map(|track| track.id == next.id)
                    .unwrap_or(false)
            {
                continue;
            }
            // 近期失败过的曲目进入冷却：巡检不能把永久失败的歌（下架/受限）
            // 每 5 秒无限重试——那会持续打签名服务和网络
            if self
                .prefetch_failed
                .get(&next.id)
                .is_some_and(|failed_at| failed_at.elapsed() < PREFETCH_RETRY_COOLDOWN)
            {
                continue;
            }
            if log {
                eprintln!("[prefetch] 开始预取：{}", next.title);
            }
            self.prefetch_inflight.insert(next.id.clone());
            let settings = self.settings.clone();
            let track = next.clone();
            let work_track = track.clone();
            let result = cx.background_spawn(async move {
                Session::new(settings).download_to_cache(&work_track)
            });
            cx.spawn(async move |this, cx| {
                let outcome = result.await;
                let succeeded = outcome.is_ok();
                if log {
                    match &outcome {
                        Ok(cached) => {
                            eprintln!("[prefetch] {} → {}", track.title, cached.quality)
                        }
                        Err(err) => eprintln!("[prefetch] {} 失败: {err}", track.title),
                    }
                }
                let _ = this.update(cx, |root, _cx| {
                    // 只删自己的：避免早先的完成回执误清后来排的曲目（单槽时代踩过）
                    root.prefetch_inflight.remove(&track.id);
                    // 成功清冷却；失败记时刻，冷却期内巡检不再重排
                    if succeeded {
                        root.prefetch_failed.remove(&track.id);
                    } else {
                        root.prefetch_failed
                            .insert(track.id.clone(), std::time::Instant::now());
                    }
                });
            })
            .detach();
        }
    }

    /// UI 心跳：播放中每 200ms 刷新一次（进度条需要持续重绘），
    /// 曲目播完则自动切下一首。
    pub(crate) fn start_heartbeat(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(200))
                .await;
            let _ = this.update(cx, |root, cx| {
                let snap = root.engine.snapshot();
                // 播放进度 / 时长上报：喂一拍快照（内部异步发请求，不阻塞）。
                // 本地文件合成的曲目（`local:` 前缀）在汽水没有条目，跳过上报。
                if !snap.track_id.starts_with(LOCAL_TRACK_PREFIX) {
                    root.play_reporter.observe(&snap);
                }
                // 加载完成：引擎已切到 pending 曲目 → 清 loading 态
                if let Some(pending) = &root.pending_track {
                    if snap.track_id == pending.id {
                        root.pending_track = None;
                    }
                }
                if snap.playing || root.pending_track.is_some() {
                    cx.notify();
                }
                // 播完自动下一首：按序号判断（循环同一首也不会漏切）。
                // 注意：正在装载下一首（pending）时「播完」属于旧曲目，
                // 只消费事件不推进——否则会把 pending 的那首跳过去（收尾瞬间
                // 手动切歌/插入队列时踩过）。
                if snap.finished && snap.finished_seq != root.last_finished_seq {
                    root.last_finished_seq = snap.finished_seq;
                    if root.pending_track.is_none() {
                        root.next_track(cx);
                    }
                }
                // 预取巡检：每 25 拍（约 5s）补一次，覆盖 append/remove 等
                // 队列变更路径（spawn_prefetch 自身会去重，空转开销极小）
                root.prefetch_patrol = root.prefetch_patrol.wrapping_add(1);
                if root.prefetch_patrol % 25 == 0 {
                    root.spawn_prefetch(cx);
                }
                // 下载：消费完成 / 失败事件 + 刷新面板快照（版本号变了才重克隆）
                root.sync_downloads(cx);
                root.load_more_recommendation(cx);
            });
        })
        .detach();
    }

    /// 打开抽屉时把滚动位置锚到「正在播放」这一行。
    pub fn anchor_queue_scroll(&self) {
        let row = crate::ui::player_bar::now_playing_slot(self);
        self.queue_scroll
            .scroll_to_item(row, gpui::ScrollStrategy::Center);
    }

    /// 从队列移除某一首（右键菜单）。正在播放的那首不允许移除：
    /// 它还挂在引擎上，移除后「队列当前项」与「实际在播」会分叉。
    pub fn remove_from_queue(&mut self, index: usize, cx: &mut Context<Self>) {
        if index == self.queue.index() {
            self.queue_menu = None;
            self.status = self
                .tr("正在播放的曲目不能从队列移除，可直接点「下一首」")
                .to_string();
            cx.notify();
            return;
        }
        if self.queue.remove(index).is_some() {
            self.queue_menu = None;
            self.sync_queue_cache();
            cx.notify();
        }
    }

    /// 队列变化后刷新快照（抽屉渲染只读它，不再每帧 to_vec）。
    pub(crate) fn sync_queue_cache(&mut self) {
        let revision = self.queue.revision();
        if self.queue_cache.0 != revision {
            self.queue_cache = (revision, Arc::new(self.queue.tracks().to_vec()));
        }
    }

    /// 设置音质档位（`auto` = 按账号权益自动）。换档后重新拉当前曲目的流。
    pub fn set_quality(&mut self, quality: &str, cx: &mut Context<Self>) {
        let value = if quality == "auto" { "" } else { quality };
        self.settings.quality = value.to_string();
        let _ = self.settings.save();
        if let Some(session) = self.session.as_mut() {
            let _ = session.apply(self.settings.clone());
        }
        // 切档后按新档位重新预取下一首（正在播的这首不受影响）
        self.spawn_prefetch(cx);
    }

    /// 按比例跳转播放位置（0.0~1.0）。
    pub fn seek_fraction(&mut self, fraction: f32) {
        let snapshot = self.engine.snapshot();
        if snapshot.duration_seconds <= 0.0 {
            return;
        }
        let target = snapshot.duration_seconds * fraction.clamp(0.0, 1.0) as f64;
        self.engine.seek(target);
    }

    /// 设置音量（0.0~1.0）。
    pub fn set_volume(&mut self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        self.engine.set_volume(volume);
        if volume > 0.0 {
            self.volume_before_mute = volume;
        }
    }

    /// 静音 / 恢复。
    pub fn toggle_mute(&mut self) {
        let current = self.engine.snapshot().volume;
        if current > 0.001 {
            self.volume_before_mute = current;
            self.engine.set_volume(0.0);
        } else {
            self.engine.set_volume(if self.volume_before_mute <= 0.001 {
                1.0
            } else {
                self.volume_before_mute
            });
        }
    }

    /// 跳到队列中的某一首并播放。
    pub fn jump_in_queue(&mut self, index: usize, cx: &mut Context<Self>) {
        if std::env::var("SODAM_PLAYER_LOG").is_ok() {
            eprintln!("[player] 队列跳转 → #{index}");
        }
        if self.queue.jump(index).is_some() {
            self.sync_queue_cache();
            self.start_track(cx);
        }
    }

    /// 创建二维码并在后台轮询扫码状态；成功后写入 Cookie。
    pub fn start_login(&mut self, cx: &mut Context<Self>) {
        let language = self.language;
        let settings = self.settings.clone();
        self.login = LoginState::Loading;
        cx.notify();

        let create_settings = settings.clone();
        let create =
            cx.background_spawn(async move { Session::new(create_settings).create_qr_login() });
        cx.spawn(async move |this, cx| {
            let created = match create.await {
                Ok(created) => created,
                Err(err) => {
                    let _ = this.update(cx, |root, cx| {
                        root.login = LoginState::Failed(
                            root.localized("创建二维码失败：{err}", &[err.to_string()]),
                        );
                        root.status = language
                            .text("创建二维码失败，可点「重新获取二维码」重试")
                            .to_string();
                        cx.notify();
                    });
                    return;
                }
            };
            let matrix = match sodam_core::login::qr_matrix(&created.scan_url) {
                Ok(matrix) => matrix,
                Err(err) => {
                    let _ = this.update(cx, |root, cx| {
                        root.login = LoginState::Failed(
                            root.localized("二维码编码失败：{err}", &[err.to_string()]),
                        );
                        cx.notify();
                    });
                    return;
                }
            };
            let token = created.token.clone();
            let _ = this.update(cx, |root, cx| {
                root.login = LoginState::Waiting {
                    matrix,
                    token: token.clone(),
                    status: language.text("等待扫码…").to_string(),
                };
                root.status = language.text("请用汽水音乐 App 扫码并确认").to_string();
                cx.notify();
            });

            // 轮询：libresoda 内部有 2.5s 最小间隔 + 5s 限流冷却，这里 3s 一次足够温和
            // 二次验证窗口只自动拉起一次（误关可重试登录流程）；完成后轮询自动领取结果
            let mut verify_window_opened = false;
            for _ in 0..80 {
                // 6 秒一次：3 秒会触发服务端限流（error_code=7），确认握手会被丢掉
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(6))
                    .await;
                let poll_settings = settings.clone();
                let poll_token = token.clone();
                let poll = cx.background_spawn(async move {
                    Session::new(poll_settings).check_qr_login(&poll_token)
                });
                let result = match poll.await {
                    Ok(result) => result,
                    Err(err) => {
                        // 单次失败（限流 / 网络抖动）不该结束整个登录流程，继续等下一轮
                        let _ = this.update(cx, |root, cx| {
                            if let LoginState::Waiting { status, .. } = &mut root.login {
                                *status = language
                                    .textf("轮询异常（会自动重试）：{err}", &[err.to_string()]);
                            }
                            cx.notify();
                        });
                        continue;
                    }
                };
                let status = if result.message.trim().is_empty() {
                    status_label(result.status, language).to_string()
                } else {
                    result.message.clone()
                };
                let cookie = result.cookie.trim().to_string();
                let finished = result.is_finished();
                if finished {
                    let _ = this.update(cx, |root, cx| {
                        match root.session.as_mut() {
                            Some(session) => match session.apply_login_cookie(&cookie) {
                                Ok(()) => {
                                    root.settings = session.settings().clone();
                                    root.status =
                                        language.text("登录成功，正在读取账号信息…").to_string();
                                }
                                Err(err) => {
                                    root.set_status(
                                        "登录成功但保存失败：{err}",
                                        &[err.to_string()],
                                    );
                                }
                            },
                            None => {
                                root.status = language.text("登录成功（会话未初始化）").to_string()
                            }
                        }
                        root.login = LoginState::Idle;
                        root.login_modal_open = false;
                        cx.notify();
                    });
                    let _ = this.update(cx, |root, cx| {
                        root.refresh_account(cx);
                        root.load_liked_ids(cx);
                    });
                    return;
                }
                if result.rate_limited {
                    // 被限流就再多等一会儿，避免继续加压
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(9))
                        .await;
                }
                let expired = result.is_terminal_failure();
                if result.need_second_verify && !verify_window_opened {
                    verify_window_opened = true;
                    // 自动拉起验证窗口（libresoda 会经签名页桥接官方验证组件）；
                    // 用户在窗口里完成验证后，下一次轮询会拿到登录成功
                    let open_settings = settings.clone();
                    let open_token = token.clone();
                    let open = cx.background_spawn(async move {
                        Session::new(open_settings).open_second_verify(&open_token)
                    });
                    let message = match open.await {
                        Ok(_) => language
                            .text("已打开二次验证窗口，请在其中完成验证")
                            .to_string(),
                        Err(err) => language.textf(
                            "打开二次验证窗口失败：{err}，请改用官方客户端导出 Cookie",
                            &[err.to_string()],
                        ),
                    };
                    let _ = this.update(cx, |root, cx| {
                        root.status = message;
                        cx.notify();
                    });
                }
                let _ = this.update(cx, |root, cx| {
                    if let LoginState::Waiting { status: slot, .. } = &mut root.login {
                        *slot = status;
                    }
                    cx.notify();
                });
                if expired {
                    // 过期就自动换一张新码（会话只有 3 分钟，用户常常来不及扫/确认）
                    let _ = this.update(cx, |root, cx| {
                        root.status = language.text("二维码已过期，正在自动换新码…").to_string();
                        root.start_login(cx);
                    });
                    return;
                }
            }
        })
        .detach();
    }

    /// 读取账号信息，并在用户没有手动指定音质时按 VIP 情况自动选最高档。
    pub fn refresh_account(&mut self, cx: &mut Context<Self>) {
        if self.settings.cookie.trim().is_empty() {
            return;
        }
        let settings = self.settings.clone();
        let work = cx.background_spawn(async move { Session::new(settings).fetch_account() });
        cx.spawn(async move |this, cx| {
            let account = work.await;
            let _ = this.update(cx, |root, cx| {
                match account {
                    Ok(info) => {
                        let vip = info.vip;
                        if !info.avatar_url.trim().is_empty() {
                            root.ensure_covers(std::slice::from_ref(&info.avatar_url.clone()), cx);
                        }
                        root.account = Some(info.clone());
                        root.play_reporter.set_user_id(info.user_id.clone());
                        root.login = LoginState::LoggedIn(info.clone());
                        // 账号请求成功说明网络就绪；若收藏 ids 此前拉取失败，自动重试。
                        if root.liked_ids_failed {
                            root.load_liked_ids(cx);
                        }
                        root.set_status(
                            "已登录：{}（{}）",
                            &[
                                if info.nickname.is_empty() {
                                    root.tr("账号")
                                } else {
                                    info.nickname.as_str()
                                }
                                .to_string(),
                                if vip {
                                    root.tr("VIP")
                                } else {
                                    root.tr("非 VIP")
                                }
                                .to_string(),
                            ],
                        );
                        if let Some(session) = root.session.as_mut() {
                            match session.refresh_quality(vip) {
                                Ok(Some(quality)) => {
                                    root.settings = session.settings().clone();
                                    root.status = root.localized(
                                        "{}；音质已按权益自动设为 {}",
                                        &[root.status.clone(), quality.to_string()],
                                    );
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    root.status = root
                                        .localized("音质偏好保存失败：{err}", &[err.to_string()])
                                }
                            }
                        }
                    }
                    Err(err) => {
                        root.status = root.localized("读取账号信息失败：{err}", &[err.to_string()])
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 设置界面主题并写入配置；一旦手动选择，不再跟随系统。
    pub fn set_ui_theme(&mut self, theme: ThemeKind, cx: &mut Context<Self>) {
        self.theme = theme;
        self.theme_follows_system = false;
        self.settings.theme = theme.key().to_string();
        let save_result = self.settings.save();
        theme::set_theme(theme);
        self.status = match save_result {
            Ok(()) => format!(
                "主题已切换为{}",
                if theme == ThemeKind::Light {
                    "浅色"
                } else {
                    "深色"
                }
            ),
            Err(err) => self.localized("主题已切换，但保存失败：{err}", &[err.to_string()]),
        };
        cx.notify();
    }

    /// 主题恢复为跟随系统：清空配置里的 theme（空串 = auto），渲染时按窗口外观取值。
    pub fn set_ui_theme_system(&mut self, cx: &mut Context<Self>) {
        self.theme_follows_system = true;
        self.theme = theme::theme_from_appearance(cx.window_appearance());
        self.settings.theme = String::new();
        let save_result = self.settings.save();
        theme::set_theme(self.theme);
        self.status = match save_result {
            Ok(()) => self.tr("主题已恢复为跟随系统").to_string(),
            Err(err) => self.localized("已切换为跟随系统，但保存失败：{err}", &[err.to_string()]),
        };
        cx.notify();
    }

    /// 用系统默认应用打开配置文件（macOS `open` / Windows `start` / Linux `xdg-open`）。
    pub fn open_config_file(&mut self, cx: &mut Context<Self>) {
        let path = Settings::config_path();
        self.status = match system_open(&path.display().to_string()) {
            Ok(_) => self.tr("已用系统默认应用打开配置文件").to_string(),
            Err(err) => self.localized("打开配置文件失败：{err}", &[err.to_string()]),
        };
        cx.notify();
    }

    /// 用系统默认浏览器打开项目 GitHub 仓库。
    pub fn open_github_repository(&mut self, cx: &mut Context<Self>) {
        let url = env!("CARGO_PKG_REPOSITORY");
        self.status = match system_open(url) {
            Ok(_) => self.tr("已在浏览器打开 GitHub 仓库").to_string(),
            Err(err) => self.localized("打开 GitHub 仓库失败：{err}", &[err.to_string()]),
        };
        cx.notify();
    }

    /// 打开歌词播放页，并按需加载当前（或正在装载）歌曲的歌词。
    pub fn open_lyrics(&mut self, cx: &mut Context<Self>) {
        let track = self
            .pending_track
            .clone()
            .or_else(|| self.queue.current().cloned());
        let Some(track) = track else {
            self.status = self.tr("还没有正在播放的歌曲").to_string();
            cx.notify();
            return;
        };
        if self.nav != Nav::Lyrics {
            self.lyrics_return_nav = Some(self.nav);
        }
        self.set_nav(Nav::Lyrics, cx);
        // 本地文件没有汽水曲目，歌词接口只会报错。
        if !is_local_track(&track) {
            self.load_lyrics(track, false, cx);
        }
    }

    pub(crate) fn ensure_original_cover(&mut self, url: &str, cx: &mut Context<Self>) {
        let url = url.trim();
        if url.is_empty() || self.original_covers.contains_key(url) {
            return;
        }
        let first_try = match self.original_cover_attempted.lock() {
            Ok(mut attempted) => attempted.insert(url.to_string()),
            Err(_) => false,
        };
        if !first_try {
            return;
        }
        let settings = self.settings.clone();
        let fetch_url = url.to_string();
        let work_url = fetch_url.clone();
        let work = cx
            .background_spawn(async move { Session::new(settings).original_cover_path(&work_url) });
        cx.spawn(async move |this, cx| {
            let path = work.await;
            let _ = this.update(cx, |root, cx| {
                if let Some(path) = path {
                    root.original_covers.insert(fetch_url.clone(), path.clone());
                    // 原图下载成功但缩略图失败时，列表 / 队列也能立刻拿到封面。
                    if !root.covers.contains_key(&fetch_url) {
                        Arc::make_mut(&mut root.covers).insert(fetch_url.clone(), path);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn load_lyrics(&mut self, track: TrackItem, force: bool, cx: &mut Context<Self>) {
        if !force && self.lyrics_track_id == track.id && !self.lyrics.is_empty() {
            return;
        }
        self.lyrics_request_seq = self.lyrics_request_seq.wrapping_add(1);
        let request_seq = self.lyrics_request_seq;
        self.loading_lyrics = true;
        self.lyrics_error = None;
        self.lyrics_title = track.title.clone();
        self.lyrics_artist = track.artist.clone();
        self.lyrics_cover = track.cover.clone();
        self.lyrics_track_id = track.id.clone();
        self.ensure_covers(std::slice::from_ref(&track.cover), cx);
        self.ensure_original_cover(&track.cover, cx);
        self.set_status("正在读取「{}」歌词…", std::slice::from_ref(&track.title));
        cx.notify();

        let settings = self.settings.clone();
        let work_track = track.clone();
        let work = cx.background_spawn(async move { Session::new(settings).lyrics(&work_track) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                if root.lyrics_request_seq != request_seq {
                    return;
                }
                root.loading_lyrics = false;
                match result {
                    Ok(raw) => {
                        root.lyrics = Arc::new(raw);
                        root.lyrics_active = None;
                        root.status = if root.lyrics.is_empty() {
                            root.localized("「{}」没有可用歌词", std::slice::from_ref(&track.title))
                        } else {
                            root.localized("「{}」歌词已加载", std::slice::from_ref(&track.title))
                        };
                    }
                    Err(err) => {
                        root.lyrics = Arc::new(Vec::new());
                        root.lyrics_active = None;
                        root.lyrics_error = Some(err.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn activate_recommended_queue(
        &mut self,
        source: RecommendationSource,
        tracks: Vec<TrackItem>,
        label: &str,
        autoplay: bool,
        cx: &mut Context<Self>,
    ) {
        self.queue.replace(tracks, 0);
        self.played_history.clear();
        self.playback_error = None;
        self.queue_origin = match source.scene.as_ref() {
            Some(scene) => QueueOrigin::FeedMode(scene.sub_queue_type.clone()),
            None => QueueOrigin::Feed,
        };
        self.recommendation = Some(source);
        self.sync_queue_cache();
        let covers: Vec<String> = self
            .queue
            .tracks()
            .iter()
            .map(|track| track.cover.clone())
            .collect();
        self.set_status("已切换到{}队列", &[label.to_string()]);
        self.ensure_covers(&covers, cx);
        self.open_lyrics(cx);
        if autoplay {
            self.start_track(cx);
        } else {
            self.pause_pending(cx);
        }
    }

    pub(crate) fn sync_scenes_list(&mut self) {
        let width = self.scenes_width.lock().map(|width| *width).unwrap_or(0.0);
        // 官方 SceneMode.vue 的自适应公式：floor((width - 48) / 187)。
        let columns = if width <= 48.0 {
            4
        } else {
            ((width - 48.0) / 187.0).floor().clamp(1.0, 8.0) as usize
        };
        let old_columns = (self.scenes_common_columns, self.scenes_card_columns);
        self.scenes_common_columns = columns;
        self.scenes_card_columns = columns;
        let common_rows = self.scenes.len().div_ceil(columns).max(1);
        let card_rows = self.scene_cards.len().div_ceil(columns).max(1);
        let count = common_rows + card_rows + 3;
        let columns_changed = old_columns != (columns, columns);
        if columns_changed {
            self.scenes_list.reset(count);
        } else if count > self.scenes_list_items {
            let old_count = self.scenes_list_items;
            self.scenes_list
                .splice(old_count..old_count, count - old_count);
        } else if count < self.scenes_list_items {
            self.scenes_list.reset(count);
        }
        self.scenes_list_items = count;
    }

    pub fn load_scenes(&mut self, cx: &mut Context<Self>) {
        if self.loading_scenes {
            return;
        }
        let existing_ids: Vec<u64> = self
            .scene_cards
            .iter()
            .filter_map(|card| card.inner_block_id.parse().ok())
            .collect();
        self.loading_scenes = true;
        self.loading_scene_cards = !existing_ids.is_empty();
        if std::env::var("SODAM_SCENE_LOG").is_ok() {
            eprintln!(
                "[scenes] load_more start existing={} loading={}",
                existing_ids.len(),
                self.loading_scene_cards
            );
        }
        self.status = if existing_ids.is_empty() {
            self.tr("正在读取听歌模式…").to_string()
        } else {
            self.tr("正在加载更多探索歌单…").to_string()
        };
        cx.notify();
        let settings = self.settings.clone();
        let work = cx.background_spawn(async move {
            let session = Session::new(settings);
            let mut mode_error = None;
            let mut card_error = None;
            let mut modes = Vec::new();
            let mut cards = Vec::new();

            if existing_ids.is_empty() {
                match sodam_core::library::scene_modes(&session) {
                    Ok(value) => modes = value,
                    Err(err) => mode_error = Some(err.to_string()),
                }
            }

            let result = if existing_ids.is_empty() {
                sodam_core::library::scene_discover_cards(&session).and_then(|mut cards| {
                    if cards.is_empty() {
                        cards = sodam_core::library::scene_discover_cards_more(
                            &session,
                            &existing_ids,
                        )?;
                    }
                    Ok(cards)
                })
            } else {
                sodam_core::library::scene_discover_cards_more(&session, &[])
            };
            match result {
                Ok(value) => cards = value,
                Err(err) => card_error = Some(err.to_string()),
            }
            cards.retain(|card| {
                card.inner_block_id
                    .parse::<u64>()
                    .map(|id| !existing_ids.contains(&id))
                    .unwrap_or(true)
            });
            Ok::<_, anyhow::Error>((modes, cards, mode_error, card_error))
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                root.loading_scenes = false;
                root.loading_scene_cards = false;
                match result {
                    Ok((modes, cards, mode_error, card_error)) => {
                        if std::env::var("SODAM_SCENE_LOG").is_ok() {
                            eprintln!("[scenes] load_more applied cards={} modes={} mode_error={:?} card_error={:?}", cards.len(), modes.len(), mode_error, card_error);
                        }
                        let mut covers: Vec<String> = cards
                            .iter()
                            .map(|card| card.playlist.cover.clone())
                            .collect();
                        covers.extend(
                            modes
                                .iter()
                                .map(|scene| scene.cover.clone())
                                .filter(|cover| !cover.is_empty()),
                        );
                        let added = cards.len();
                        if !modes.is_empty() {
                            root.scenes = Arc::new(modes);
                        }
                        let mut all_cards = root.scene_cards.as_ref().clone();
                        all_cards.extend(cards);
                        root.scene_cards = Arc::new(all_cards);
                        root.sync_scenes_list();
                        root.ensure_covers(&covers, cx);
                        root.status = if let Some(err) = mode_error.or(card_error) {
                            root.localized("读取听歌模式失败：{err}", &[err])
                        } else if added == 0 {
                            root.tr("没有更多探索歌单").to_string()
                        } else {
                            root.localized("探索歌单已追加 {} 张",&[added.to_string()])
                        };
                    }
                    Err(err) => root.status = root.localized("读取听歌模式失败：{err}", &[err.to_string()]),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn start_scene_queue(&mut self, scene: SceneItem, cx: &mut Context<Self>) {
        let origin = QueueOrigin::FeedMode(scene.sub_queue_type.clone());
        if self.queue_origin == origin {
            self.open_playback(cx);
            return;
        }
        let scene_name = self.scene_display_text(&scene.sub_queue_type, &scene.text);
        self.begin_queue_switch(origin, &scene_name, cx);
        self.loading_recommendation = true;
        self.recommendation_request_seq = self.recommendation_request_seq.wrapping_add(1);
        let request_seq = self.recommendation_request_seq;
        self.status = self.localized("正在加载「{}」队列…", &[scene_name]);
        cx.notify();

        let settings = self.settings.clone();
        let played_media = self.play_reporter.played_media();
        let work = cx.background_spawn(async move {
            sodam_core::library::recommended_scene_start(
                &Session::new(settings),
                scene,
                &played_media,
            )
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                if root.recommendation_request_seq != request_seq {
                    return;
                }
                root.loading_recommendation = false;
                root.switching_queue = None;
                match result {
                    Ok(start) => {
                        root.activate_recommended_queue(
                            start.source,
                            start.tracks,
                            root.tr("听歌模式"),
                            true,
                            cx,
                        );
                    }
                    Err(err) => {
                        root.set_status("加载听歌模式队列失败：{err}", &[err.to_string()]);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 装载第一首但不播放，停在第一秒等待用户点播放。
    pub(crate) fn pause_pending(&mut self, cx: &mut Context<Self>) {
        self.playing = true;
        self.play_seq = self.play_seq.wrapping_add(1);
        let request_seq = self.play_seq;
        let Some(track) = self.queue.current().cloned() else {
            return;
        };
        self.pending_track = Some(track.clone());
        self.progress_preview = Some(0.0);
        self.set_status("已准备播放：{}", std::slice::from_ref(&track.title));
        self.ensure_covers(std::slice::from_ref(&track.cover), cx);
        cx.notify();
        let settings = self.settings.clone();
        let work_track = track.clone();
        let work = cx
            .background_spawn(async move { Session::new(settings).download_to_cache(&work_track) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                if root.play_seq == request_seq {
                    root.pending_track = None;
                    root.progress_preview = None;
                }
                if let Err(err) = result {
                    root.playback_error = Some(err.to_string());
                    root.playing = false;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 打开播放页；永远不改变当前队列来源。
    pub fn open_playback(&mut self, cx: &mut Context<Self>) {
        self.set_nav(Nav::Lyrics, cx);
        if self.lyrics_return_nav.is_none() {
            self.lyrics_return_nav = Some(Nav::Home);
        }
        if let Some(track) = self
            .pending_track
            .clone()
            .or_else(|| self.queue.current().cloned())
        {
            self.ensure_playback_assets(track, cx);
        }
        cx.notify();
    }

    /// 点击「推荐」：如果当前已是推荐队列，只打开播放页；否则替换队列。
    pub fn activate_recommendation(&mut self, cx: &mut Context<Self>) {
        if matches!(self.queue_origin, QueueOrigin::Feed) {
            self.set_nav(Nav::Lyrics, cx);
            self.lyrics_return_nav = Some(Nav::Home);
            cx.notify();
            return;
        }
        self.start_recommendation(true, cx);
    }

    /// 进入「推荐」页：**已有推荐队列就直接用，不重新拉网络**。
    ///
    /// 启动时已经 `start_recommendation(false)` 拉过一次（队列 = 推荐内容），
    /// 之前每次点侧边栏都会再拉一遍 —— 用户看到的就是「临时加载」。
    ///
    /// 只有队列**不是**推荐来源、或队列为空时才真的去拉。
    fn enter_recommendation(&mut self, cx: &mut Context<Self>) {
        let has_recommendation_queue =
            matches!(self.queue_origin, QueueOrigin::Feed) && !self.queue.is_empty();
        if has_recommendation_queue {
            self.set_nav(Nav::Lyrics, cx);
            self.lyrics_return_nav = Some(Nav::Home);
            cx.notify();
            return;
        }
        self.start_recommendation(true, cx);
    }

    pub(crate) fn begin_queue_switch(
        &mut self,
        origin: QueueOrigin,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        self.switching_queue = Some(origin.clone());
        self.switching_label = label.to_string();
        self.set_nav(Nav::Lyrics, cx);
        if self.lyrics_return_nav.is_none() {
            self.lyrics_return_nav = Some(Nav::Home);
        }
        self.engine.stop();
        self.playing = false;
        self.pending_track = None;
        self.progress_preview = None;
        self.lyrics = Arc::new(Vec::new());
        self.lyrics_active = None;
        self.lyrics_error = None;
        self.loading_lyrics = false;
        self.playback_error = None;
    }

    /// 清空歌词状态（切到没有歌词的曲目时用）。
    ///
    /// 只清数据不清 `lyrics_return_nav` —— 那是「从哪来回哪去」的导航状态，与曲目无关。
    pub(crate) fn clear_lyrics_state(&mut self) {
        self.lyrics = Arc::new(Vec::new());
        self.lyrics_track_id.clear();
        self.lyrics_active = None;
        self.lyrics_error = None;
        self.loading_lyrics = false;
        // 让在途的旧请求作废（回来时看到 seq 不匹配就丢弃）。
        self.lyrics_request_seq = self.lyrics_request_seq.wrapping_add(1);
    }

    pub(crate) fn ensure_playback_assets(&mut self, track: TrackItem, cx: &mut Context<Self>) {
        // 本地曲目（合成 id）没有对应的汽水曲目：歌词只从旁路 `.lrc` 来。
        if is_local_track(&track) {
            if self.lyrics_track_id != track.id {
                self.load_local_lyrics(&track);
            }
        } else if self.lyrics_track_id != track.id
            || self.lyrics.is_empty()
            || self.lyrics_error.is_some()
        {
            self.load_lyrics(track.clone(), false, cx);
        }
        // 封面来源与 `lyrics_view` 必须**完全一致**：播放页用旁路 `file://` 伪 URL
        // 取图，这里却拿 `track.cover`（网络 URL）去准备，两者对不上 ——
        // 表现就是「有封面却一直显示重试加载」。
        let source = self.playback_cover_source(&track);
        self.ensure_covers(std::slice::from_ref(&source), cx);
        self.ensure_original_cover(&source, cx);
    }

    /// 播放页 / 播放条的封面来源：本地曲目优先旁路图（`file://` 伪 URL）。
    ///
    /// **唯一入口** —— 准备与渲染必须用同一个值，否则会出现
    /// 「已下载原图」判定为假、界面一直显示加载态。
    pub(crate) fn playback_cover_source(&self, track: &TrackItem) -> String {
        if is_local_track(track) {
            if let Some(path) = self
                .local_files
                .iter()
                .find(|file| file.to_track().id == track.id)
                .and_then(|file| file.cover_path.clone())
            {
                return format!("file://{}", path.display());
            }
        }
        track.cover.clone()
    }

    pub fn retry_playback_assets(&mut self, track: TrackItem, cx: &mut Context<Self>) {
        // 来源与渲染 / 准备保持一致（本地曲目是 `file://` 伪 URL）。
        let source = self.playback_cover_source(&track);
        self.original_cover_attempted
            .lock()
            .ok()
            .map(|mut attempted| attempted.remove(&source));
        self.cover_attempted
            .lock()
            .ok()
            .map(|mut attempted| attempted.remove(&source));
        if let Some(session) = self.session.as_mut() {
            session.invalidate_original_cover(&source);
            session.invalidate_cover_path(&source);
        }
        if !is_local_track(&track) {
            self.load_lyrics(track.clone(), true, cx);
        } else {
            // 本地曲目重试：重新读旁路 `.lrc`（文件可能刚被放进来）。
            self.load_local_lyrics(&track);
        }
        self.ensure_covers(std::slice::from_ref(&source), cx);
        self.ensure_original_cover(&source, cx);
        cx.notify();
    }

    /// 点击「推荐」：清空旧队列并切换到官方推荐队列。
    pub fn start_recommendation(&mut self, autoplay: bool, cx: &mut Context<Self>) {
        if self.loading_recommendation || self.settings.cookie.trim().is_empty() {
            return;
        }
        self.loading_recommendation = true;
        self.begin_queue_switch(QueueOrigin::Feed, self.tr("推荐队列"), cx);
        self.recommendation_request_seq = self.recommendation_request_seq.wrapping_add(1);
        let request_seq = self.recommendation_request_seq;
        self.status = self.tr("正在加载推荐队列…").to_string();
        cx.notify();

        let settings = self.settings.clone();
        let played_media = self.play_reporter.played_media();
        let work = cx.background_spawn(async move {
            sodam_core::library::recommended_start(&Session::new(settings), &played_media)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                if root.recommendation_request_seq != request_seq {
                    return;
                }
                root.loading_recommendation = false;
                root.switching_queue = None;
                match result {
                    Ok(start) => {
                        root.activate_recommended_queue(
                            start.source,
                            start.tracks,
                            root.tr("推荐"),
                            autoplay,
                            cx,
                        );
                    }
                    Err(err) => {
                        root.set_status("加载推荐队列失败：{err}", &[err.to_string()]);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 听歌模式探索卡：歌单走歌单曲队，电台走 FeedRadioTracks。
    pub fn start_scene_card(
        &mut self,
        card: sodam_core::library::SceneCard,
        cx: &mut Context<Self>,
    ) {
        match card.kind {
            sodam_core::library::SceneCardKind::Playlist => {
                self.start_scene_playlist(card.playlist, cx);
            }
            sodam_core::library::SceneCardKind::Radio => {
                let Some(radio) = card.radio.clone() else {
                    return;
                };
                self.begin_queue_switch(
                    QueueOrigin::Radio(radio.radio_id.clone()),
                    &card.playlist.title,
                    cx,
                );
                self.loading_playlist = true;
                self.set_status(
                    "正在加载电台「{}」…",
                    std::slice::from_ref(&card.playlist.title),
                );
                cx.notify();

                let settings = self.settings.clone();
                let played_media = self.play_reporter.played_media();
                let work = cx.background_spawn(async move {
                    let session = Session::new(settings);
                    sodam_core::library::radio_tracks(&session, &radio, &played_media)
                });
                cx.spawn(async move |this, cx| {
                    let result = work.await;
                    let _ = this.update(cx, |root, cx| {
                        root.loading_playlist = false;
                        root.switching_queue = None;
                        match result {
                            Ok(tracks) if !tracks.is_empty() => {
                                root.queue.replace(tracks, 0);
                                root.played_history.clear();
                                root.playback_error = None;
                                root.queue_origin = QueueOrigin::Radio(card.playlist.id.clone());
                                root.recommendation = None;
                                root.sync_queue_cache();
                                let covers: Vec<String> = root
                                    .queue
                                    .tracks()
                                    .iter()
                                    .map(|track| track.cover.clone())
                                    .collect();
                                root.ensure_covers(&covers, cx);
                                root.open_lyrics(cx);
                                root.start_track(cx);
                            }
                            Ok(_) => {
                                root.status = root.localized(
                                    "电台「{}」没有可播放曲目",
                                    std::slice::from_ref(&card.playlist.title),
                                );
                            }
                            Err(err) => {
                                root.status =
                                    root.localized("加载电台失败：{err}", &[err.to_string()])
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    /// 听歌模式探索卡：直接播放整张歌单，不进入详情页。
    pub fn start_scene_playlist(&mut self, playlist: PlaylistItem, cx: &mut Context<Self>) {
        if self.loading_playlist {
            return;
        }
        self.begin_queue_switch(
            QueueOrigin::Playlist(playlist.id.clone()),
            &playlist.title,
            cx,
        );
        self.loading_playlist = true;
        self.set_status("正在加载歌单「{}」…", std::slice::from_ref(&playlist.title));
        cx.notify();

        let settings = self.settings.clone();
        let playlist_id = playlist.id.clone();
        let work = cx.background_spawn(async move {
            let session = Session::new(settings);
            sodam_core::library::playlist_tracks(&session, &playlist_id)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                root.loading_playlist = false;
                match result {
                    Ok(tracks) if !tracks.is_empty() => {
                        root.queue.replace(tracks, 0);
                        root.played_history.clear();
                        root.playback_error = None;
                        root.queue_origin = QueueOrigin::Playlist(playlist.id.clone());
                        root.recommendation = None;
                        root.switching_queue = None;
                        root.sync_queue_cache();
                        let covers: Vec<String> = root
                            .queue
                            .tracks()
                            .iter()
                            .map(|track| track.cover.clone())
                            .collect();
                        root.ensure_covers(&covers, cx);
                        root.open_lyrics(cx);
                        root.start_track(cx);
                    }
                    Ok(_) => root.set_status(
                        "歌单「{}」没有可播放曲目",
                        std::slice::from_ref(&playlist.title),
                    ),
                    Err(err) => {
                        root.status = root.localized("加载歌单失败：{err}", &[err.to_string()])
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 官方推荐队列的动态加载：当前歌后方少于 6 首时由心跳触发。
    pub(crate) fn load_more_recommendation(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.recommendation.clone() else {
            return;
        };
        if !source.has_more || self.loading_recommendation || self.settings.cookie.trim().is_empty()
        {
            return;
        }
        let current = self.queue.index();
        let remaining = self.queue.len().saturating_sub(current + 1);
        if remaining >= 6 {
            return;
        }

        self.loading_recommendation = true;
        self.recommendation_request_seq = self.recommendation_request_seq.wrapping_add(1);
        let request_seq = self.recommendation_request_seq;
        self.status = self.tr("正在加载更多推荐…").to_string();
        cx.notify();

        let settings = self.settings.clone();
        let played_media = self.play_reporter.played_media();
        let work = cx.background_spawn(async move {
            sodam_core::library::recommended_append(&Session::new(settings), &source, &played_media)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                if root.recommendation_request_seq != request_seq {
                    return;
                }
                root.loading_recommendation = false;
                match result {
                    Ok(batch) => {
                        let added = root.queue.append(batch.tracks);
                        root.recommendation = Some(batch.source);
                        root.sync_queue_cache();
                        let covers: Vec<String> = root
                            .queue
                            .tracks()
                            .iter()
                            .skip(root.queue.len().saturating_sub(added))
                            .map(|track| track.cover.clone())
                            .collect();
                        root.ensure_covers(&covers, cx);
                        root.set_status("推荐队列已追加 {} 首", &[added.to_string()]);
                    }
                    Err(err) => {
                        root.set_status("加载更多推荐失败：{err}", &[err.to_string()]);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 退出登录：清除 Cookie 并立即回到未登录状态。
    ///
    /// 播放和队列一并停止，避免旧账号的歌单/收藏继续留在界面上；
    /// 封面本地缓存只按 URL 索引，可以复用，不涉及凭据。
    pub fn logout(&mut self, cx: &mut Context<Self>) {
        let mut settings = self.settings.clone();
        settings.cookie.clear();

        self.engine.stop();
        self.playing = false;
        self.pending_track = None;
        self.playback_error = None;
        self.queue = Queue::new(Vec::new());
        self.played_history.clear();
        self.sync_queue_cache();

        self.account = None;
        self.login = LoginState::Idle;
        self.login_modal_open = false;
        self.results = Arc::new(Vec::new());
        self.searching = false;
        self.playlists.clear();
        self.liked = Arc::new(Vec::new());
        self.liked_ids = Arc::new(HashSet::new());
        self.liked_ids_failed = false;
        self.liked_loaded = false;
        self.open_playlist = None;
        self.track_menu = None;
        self.queue_menu = None;
        self.set_nav(Nav::Settings, cx);

        let save_result = if let Some(session) = self.session.as_mut() {
            session.apply(settings.clone())
        } else {
            settings.save()
        };
        self.settings = settings;
        self.status = match save_result {
            Ok(()) => self.tr("已退出登录").to_string(),
            Err(err) => self.localized("已退出，但保存配置失败：{err}", &[err.to_string()]),
        };
        cx.notify();
    }

    /// 详情列表内的加载更多分发。
    pub fn load_more_detail(&mut self, cx: &mut Context<Self>) {
        if self.nav == Nav::Artist {
            self.load_more_artist_tracks(cx);
        }
    }

    /// 统一页面切换：保留返回栈，详情页使用通用返回组件。
    pub(crate) fn set_nav(&mut self, nav: Nav, cx: &mut Context<Self>) {
        if self.nav == nav {
            return;
        }
        // 栈顶与当前页相同就不重复压 —— 详情页（专辑 / 歌手）反复进出时，
        // 栈里会攒下一串同名项，返回要按很多次才出得去。
        if self.nav_history.last() != Some(&self.nav) {
            self.nav_history.push(self.nav);
        }
        if self.nav_history.len() > 50 {
            self.nav_history.remove(0);
        }
        self.nav = nav;
        if matches!(nav, Nav::Settings) {
            self.refresh_cache_stats(cx);
        }
        self.on_nav_changed(cx);
        cx.notify();
    }

    /// 后台刷新缓存统计（同步扫盘不能进渲染路径：设置页每次重绘都扫
    /// 目录会把 UI 卡出顿挫，预取/心跳的高频 notify 会放大这个问题）。
    pub(crate) fn refresh_cache_stats(&mut self, cx: &mut Context<Self>) {
        let work = cx.background_spawn(async { sodam_core::audio::cache_stats() });
        cx.spawn(async move |this, cx| {
            let stats = work.await;
            let _ = this.update(cx, |root, cx| {
                root.cache_summary = stats;
                cx.notify();
            });
        })
        .detach();
    }

    /// 后台重扫本地目录，一次刷新两样东西：「已下载」标记 + 本地音乐库。
    ///
    /// 与 [`Self::refresh_cache_stats`] 同理：扫描是阻塞 I/O，绝不能进渲染路径。
    /// **扫盘只做一次**，同一份结果同时喂给标记与清单校验（大目录省一半 I/O）。
    ///
    /// 另外做了**合并**——已有扫描在跑时只记一个「待重扫」，结束后补扫一次，
    /// 免得连续触发（切页 / 下载完成 / 改设置）堆出一串重复扫描。
    pub(crate) fn refresh_local(&mut self, cx: &mut Context<Self>) {
        if self.local_scanning {
            self.local_rescan = true;
            return;
        }
        self.local_scanning = true;
        let settings = self.settings.download.clone();
        let naming = settings.naming().to_string();
        let work = cx.background_spawn(async move {
            // 清单先读：它提供两样东西 —— 元数据、以及**权威时长**（有就不必读盘探测）。
            let entries = sodam_core::download_library::load(&settings);
            let known_durations: std::collections::HashMap<std::path::PathBuf, f64> = entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .duration_seconds
                        .filter(|seconds| *seconds > 0)
                        .map(|seconds| (entry.relative.clone(), seconds as f64))
                })
                .collect();

            // 一次目录遍历同时喂四样东西：已下载标记、清单校验、本地音乐列表、时长。
            let scanned = sodam_core::download::scan_local_files(&settings, &known_durations);
            let sidecars = sodam_core::download::scan_sidecars(&settings);
            let on_disk: std::collections::HashSet<std::path::PathBuf> =
                scanned.iter().map(|file| file.relative.clone()).collect();
            let index = DownloadedIndex::from_paths(on_disk.clone(), &naming);
            let outcome = sodam_core::download_library::reconcile_with(&on_disk, entries);

            // 本地音乐 = **磁盘真相**：扫到的每个文件一行，清单里有记录就把元数据挂上。
            // 用户手动放进下载目录的文件因此也会出现在这里（只是没有元数据）。
            let mut meta: std::collections::HashMap<_, _> = outcome
                .present
                .iter()
                .map(|entry| (entry.relative.clone(), entry.clone()))
                .collect();
            let files: Vec<LocalFile> = scanned
                .into_iter()
                .map(|file| {
                    // 旁路按**文件名 stem** 索引（旁路不跟随模板分层）。
                    let stem = file
                        .relative
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_default();
                    LocalFile {
                        entry: meta.remove(&file.relative),
                        relative: file.relative,
                        extension: file.extension,
                        bytes: file.bytes,
                        probed_duration: file.duration_seconds,
                        cover_path: sidecars.covers.get(&stem).cloned(),
                        lyrics_path: sidecars.lyrics.get(&stem).cloned(),
                    }
                })
                .collect();
            (index, outcome, files)
        });
        cx.spawn(async move |this, cx| {
            let (index, outcome, files) = work.await;
            let _ = this.update(cx, |root, cx| {
                root.local_scanning = false;
                root.downloaded = index;
                root.library = Arc::new(outcome.present);
                root.library_missing = Arc::new(outcome.missing);
                root.local_files = Arc::new(files);
                cx.notify();
                if root.local_rescan {
                    root.local_rescan = false;
                    root.refresh_local(cx);
                }
            });
        })
        .detach();
    }

    /// 通用返回：同页详情优先关闭，再按真实访问历史回退。
    pub fn go_back(&mut self, cx: &mut Context<Self>) {
        if self.nav == Nav::Search && self.search_playlist.is_some() {
            self.search_playlist = None;
            self.loading_search_playlist = false;
            cx.notify();
            return;
        }
        if self.nav == Nav::Library && self.open_playlist.is_some() {
            self.open_playlist = None;
            self.loading_playlist = false;
            cx.notify();
            return;
        }
        let fallback = if self.settings.cookie.trim().is_empty() {
            Nav::Settings
        } else {
            Nav::Home
        };
        let nav = self.nav_history.pop().unwrap_or(fallback);
        self.nav = nav;
        self.on_nav_changed(cx);
        cx.notify();
    }

    /// 切换导航时的懒加载：进入「我的歌单 / 我喜欢的音乐」才拉数据。
    ///
    /// 未登录时其他页面一律进不去（登录入口在设置 → 账户）。
    pub fn on_nav_changed(&mut self, cx: &mut Context<Self>) {
        if !self.settings.cookie.trim().is_empty() {
            match self.nav {
                Nav::Home => self.enter_recommendation(cx),
                // 听歌模式只负责首屏懒加载；“加载更多”必须由用户显式点击。
                Nav::Scenes if self.scene_cards.is_empty() && !self.loading_scenes => {
                    self.load_scenes(cx)
                }
                Nav::Library => self.load_playlists(cx),
                Nav::Liked => {
                    // 收藏页使用独立列表状态；已加载数据也要重建到页首。
                    self.liked_detail_list.reset(self.liked.len() + 1);
                    self.load_liked(cx);
                }
                _ => {}
            }
        }
        // 切页时顺手重扫一次下载目录（合并机制保证不会堆扫描）。
        self.refresh_local(cx);
    }

    /// 封面本地路径（渲染路径唯一入口：只查缓存，不发请求）。
    pub fn cover_of(&self, url: &str) -> Option<PathBuf> {
        self.covers.get(url).cloned()
    }

    /// 把封面放进请求池（渲染只读缓存，缺图先用占位块）。
    ///
    /// 用 `cover_attempted` 去重：下载失败的 URL 也记下来，
    /// 否则「缺图 → 每帧重新排队」会变成 CPU/网络空转（实测踩过）。
    pub fn ensure_covers(&mut self, urls: &[String], cx: &mut Context<Self>) {
        let mut added = false;
        for url in urls {
            let url = url.trim();
            if url.is_empty() {
                continue;
            }
            let first_try = match self.cover_attempted.lock() {
                Ok(mut attempted) => attempted.insert(url.to_string()),
                Err(_) => false,
            };
            if first_try {
                added |= self.cover_pool.push(url);
            }
        }
        if added {
            self.pump_covers(cx);
        }
    }

    /// 驱动请求池：保持最多 `MAX_CONCURRENCY` 个下载在飞，完成一个补一个。
    pub(crate) fn pump_covers(&mut self, cx: &mut Context<Self>) {
        while let Some(url) = self.cover_pool.next() {
            let settings = self.settings.clone();
            let fetch_url = url.clone();
            let task = cx.background_spawn(async move {
                let session = Session::new(settings);
                let path = session.cover_path(&fetch_url);
                let color = path.as_deref().and_then(sodam_core::color::dominant_color);
                (path, color)
            });
            cx.spawn(async move |this, cx| {
                let (path, color) = task.await;
                let _ = this.update(cx, |root, cx| {
                    root.cover_pool.done();
                    if std::env::var("SODAM_THEME_LOG").is_ok() {
                        eprintln!(
                            "[theme] cover url={:?} path={:?} color={:?}",
                            url,
                            path.as_ref().map(|path| path
                                .file_name()
                                .map(|name| name.to_string_lossy())
                                .unwrap_or_default()),
                            color
                        );
                    }
                    let has_result = path.is_some() || color.is_some();
                    if let Some(path) = path {
                        Arc::make_mut(&mut root.covers).insert(url.clone(), path);
                    }
                    if let Some(color) = color {
                        root.ambient_colors.insert(url.clone(), color);
                    }
                    if has_result {
                        cx.notify();
                    }
                    // 一个下载完成就补下一个，池子始终保持有界并发
                    root.pump_covers(cx);
                });
            })
            .detach();
        }
    }

    /// 拉「我的歌单」（自己创建的 + 收藏的）。
    pub fn load_playlists(&mut self, cx: &mut Context<Self>) {
        if self.loading_library || !self.playlists.is_empty() {
            return;
        }
        if self.session.is_none() {
            self.status = self.tr("需要先登录才能读取歌单").to_string();
            cx.notify();
            return;
        }
        let settings = self.settings.clone();
        self.loading_library = true;
        self.status = self.tr("正在读取歌单…").to_string();
        cx.notify();
        let work = cx.background_spawn(async move {
            let session = Session::new(settings);
            sodam_core::library::user_playlists(&session)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                root.loading_library = false;
                match result {
                    Ok(playlists) => {
                        root.set_status("共 {} 个歌单", &[playlists.len().to_string()]);
                        let covers: Vec<String> = playlists
                            .iter()
                            .take(ROW_BATCH)
                            .map(|item| item.cover.clone())
                            .collect();
                        root.playlists = playlists;
                        root.ensure_covers(&covers, cx);
                    }
                    Err(err) => {
                        root.status = root.localized("读取歌单失败：{err}", &[err.to_string()])
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 只拉「我喜欢的音乐」的 id 集合（列表爱心状态用，比整表轻）。
    pub fn load_liked_ids(&mut self, cx: &mut Context<Self>) {
        if self.liked_loading || self.settings.cookie.trim().is_empty() {
            return;
        }
        self.liked_loading = true;
        let settings = self.settings.clone();
        let work = cx.background_spawn(async move {
            let session = Session::new(settings);
            // 启动时网络/接口可能未就绪：最多尝试 3 次（2s / 4s 退避），
            // 否则一次瞬时失败会让整个会话的爱心状态全部落在「未收藏」。
            let mut attempt = 0;
            loop {
                match sodam_core::library::liked_track_ids(&session) {
                    Ok(ids) => return Ok(ids),
                    Err(_err) if attempt < 2 => {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_secs(2 * attempt));
                    }
                    Err(err) => return Err(err),
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                root.liked_loading = false;
                match result {
                    Ok(ids) => {
                        root.liked_ids = Arc::new(ids);
                        root.liked_ids_failed = false;
                        cx.notify();
                    }
                    Err(_) => {
                        // 未登录/网络异常：保持空集合，不打扰用户；
                        // 但置失败标记，等账号刷新成功（网络就绪信号）后自动重试。
                        root.liked_ids_failed = true;
                    }
                }
            });
        })
        .detach();
    }

    /// 点爱心：收藏 / 取消收藏（乐观更新，失败回滚）。
    pub fn toggle_like(&mut self, track: TrackItem, cx: &mut Context<Self>) {
        let id = track.id.clone();
        let liked = !self.liked_ids.contains(&id);
        {
            let mut ids = self.liked_ids.as_ref().clone();
            if liked {
                ids.insert(id.clone());
            } else {
                ids.remove(&id);
            }
            self.liked_ids = Arc::new(ids);
        }
        // 只有完整加载后才同步本地列表；未加载时后台拉全量，避免覆盖历史收藏。
        if self.liked_loaded {
            let mut list = self.liked.as_ref().clone();
            if liked {
                if !list.iter().any(|item| item.id == id) {
                    list.insert(0, track.clone());
                }
            } else {
                list.retain(|item| item.id != id);
            }
            self.liked = Arc::new(list);
        } else {
            self.load_liked(cx);
        }
        self.status = if liked {
            self.localized("已收藏：{}", std::slice::from_ref(&track.title))
        } else {
            self.localized("已取消收藏：{}", std::slice::from_ref(&track.title))
        };
        cx.notify();

        let settings = self.settings.clone();
        let work_id = id.clone();
        let rollback_track = track.clone();
        let work = cx.background_spawn(async move {
            let session = Session::new(settings);
            sodam_core::library::set_track_liked(&session, &work_id, liked)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                if let Err(err) = result {
                    // 回滚 ids
                    let mut ids = root.liked_ids.as_ref().clone();
                    if liked {
                        ids.remove(&id);
                    } else {
                        ids.insert(id.clone());
                    }
                    root.liked_ids = Arc::new(ids);
                    // 同步回滚本地完整列表，避免 ids 与列表互相矛盾。
                    if root.liked_loaded {
                        let mut list = root.liked.as_ref().clone();
                        if liked {
                            list.retain(|item| item.id != id);
                        } else if !list.iter().any(|item| item.id == id) {
                            list.insert(0, rollback_track);
                        }
                        root.liked = Arc::new(list);
                    }
                    root.localized("收藏失败：{err}", &[err.to_string()]);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 拉「我喜欢的音乐」。
    pub fn load_liked(&mut self, cx: &mut Context<Self>) {
        self.liked_detail_list.reset(self.liked.len() + 1);
        // `liked` 可能被点爱心提前预填；是否完整加载必须单独标记。
        if self.loading_liked || self.liked_loaded {
            return;
        }
        if self.session.is_none() {
            self.status = self.tr("需要先登录才能读取收藏").to_string();
            cx.notify();
            return;
        }
        let settings = self.settings.clone();
        self.loading_liked = true;
        self.status = self.tr("正在读取我喜欢的音乐…").to_string();
        cx.notify();
        let work = cx.background_spawn(async move {
            let session = Session::new(settings);
            sodam_core::library::liked_songs(&session)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                root.loading_liked = false;
                match result {
                    Ok(tracks) => {
                        root.liked_loaded = true;
                        // 全量列表是收藏的权威快照：同步修复全局 ids，
                        // 避免启动时 ids 拉取失败后爱心状态一直错。
                        root.liked_ids =
                            Arc::new(tracks.iter().map(|track| track.id.clone()).collect());
                        root.liked_ids_failed = false;
                        root.liked_detail_list.reset(tracks.len() + 1);
                        root.set_status("我喜欢的音乐：{} 首", &[tracks.len().to_string()]);
                        // 只取头部封面（列表封面由「可见行」按需排队）
                        let head: Vec<String> = tracks
                            .first()
                            .map(|track| vec![track.cover.clone()])
                            .unwrap_or_default();
                        root.liked = Arc::new(tracks);
                        root.ensure_covers(&head, cx);
                    }
                    Err(err) => {
                        root.status = root.localized("读取收藏失败：{err}", &[err.to_string()])
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 点列表行播放（列表以 `Arc` 共享，避免每次点击都深拷贝整张表）。
    pub fn play_from_arc(
        &mut self,
        tracks: std::sync::Arc<Vec<TrackItem>>,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        self.play_from(tracks.as_ref().clone(), index, cx);
    }

    /// 本地曲目 → 磁盘上的真实路径（不是本地曲目则 `None`）。
    ///
    /// 判定依据是 `Root.local_files` 里能否按相对路径找到 —— 这是唯一可信的来源
    /// （合成 id 只是 `local:<relative>` 的字符串，不拿它反解路径）。
    fn local_playback_path(&self, track: &TrackItem) -> Option<PathBuf> {
        let file = self
            .local_files
            .iter()
            .find(|file| file.to_track().id == track.id)?;
        let path = file.absolute_path(&self.settings.download);
        path.is_file().then_some(path)
    }
    /// 本地曲目的音质文案（清单里有就照用，否则 `None` 交给调用方兜底）。
    fn local_quality_for(&self, track: &TrackItem) -> Option<String> {
        self.local_files
            .iter()
            .find(|file| file.to_track().id == track.id)
            .and_then(|file| file.entry.as_ref())
            .and_then(|entry| entry.quality.clone())
    }

    /// 播放「本地音乐」列表里的一行：文件已经在磁盘上，**直接交给引擎，不联网**。
    ///
    /// 与 [`Self::play_from`] 的区别：后者是网络曲目，要走取流 + 解密 + 缓存；
    /// 这里的文件是本应用下载好的（或用户自己放进下载目录的），直接 `engine.load`。
    ///
    /// 队列仍然整表入队（播放完能自动下一首、播放条 / 抽屉能看到列表），
    /// 只是每一首的装载都从本地文件走（`start_track` 会识别本地曲目）。
    pub(crate) fn play_local_file(
        &mut self,
        files: Arc<Vec<LocalFile>>,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = files.get(index) else {
            return;
        };
        let track = file.to_track();
        let path = file.absolute_path(&self.settings.download);
        if !path.is_file() {
            self.status = self.tr("文件已不在").to_string();
            cx.notify();
            return;
        }
        let quality = file
            .entry
            .as_ref()
            .and_then(|entry| entry.quality.clone())
            .unwrap_or_else(|| self.tr("未知音质").to_string());

        // 队列整表入队：自动下一首 / 播放条 / 队列抽屉都依赖它。
        let tracks: Vec<TrackItem> = files.iter().map(|file| file.to_track()).collect();
        self.queue.replace(tracks, index);
        self.played_history.clear();
        self.queue.mode = sodam_core::queue::PlayMode::Sequential;
        self.queue_origin = QueueOrigin::Local;
        // 歌词：有旁路 `.lrc` 就用它（与网络歌词同一套解析器），否则清掉上一首的。
        self.load_local_lyrics(&track);

        // 交给引擎（本地文件，没有取流这一步）。
        self.playing = true;
        self.play_seq = self.play_seq.wrapping_add(1);
        self.pending_track = None;
        self.progress_preview = None;
        self.playback_error = None;
        self.consecutive_failures = 0;
        self.engine.load(track.clone(), path, quality);
        self.set_status("播放中：{}", std::slice::from_ref(&track.title));
        // 封面：本地曲目走旁路图（`file://` 伪 URL），与播放页的取图来源一致。
        let cover_source = self.playback_cover_source(&track);
        self.ensure_covers(std::slice::from_ref(&cover_source), cx);
        self.sync_queue_cache();
        cx.notify();
    }

    /// 本地曲目的歌词：查旁路 `.lrc` → 解析 → 填状态；没有就清空。
    ///
    /// 与网络歌词的区别：**不发起任何请求**，也不进 loading 态（文件读取是同步的
    /// 微秒级操作，且已经在后台扫盘时拿到了路径）。
    pub(crate) fn load_local_lyrics(&mut self, track: &TrackItem) {
        let lyrics = self
            .local_files
            .iter()
            .find(|file| file.to_track().id == track.id)
            .and_then(|file| file.lyrics_path.as_ref())
            .and_then(|path| sodam_core::download::read_sidecar_lyrics(path));
        self.clear_lyrics_state();
        if let Some(lines) = lyrics {
            self.lyrics_track_id = track.id.clone();
            self.lyrics = Arc::new(lines);
        }
    }

    /// 单条「匹配信息」：拿文件名搜汽水，够可信才写回清单。
    ///
    /// 匹配不上（返回 `None`）时**打开候选浮层**让用户自己挑 —— 冷门曲目
    /// 自动分不够，但用户一眼能认出来。
    pub fn match_local_file(&mut self, relative: PathBuf, cx: &mut Context<Self>) {
        if self.settings.cookie.trim().is_empty() {
            self.status = self.tr("登录后才能使用搜索、歌单与播放").to_string();
            cx.notify();
            return;
        }
        let Some(file) = self
            .local_files
            .iter()
            .find(|file| file.relative == relative)
            .cloned()
        else {
            return;
        };
        let stem = file
            .relative
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let duration_hint = file.duration_seconds().map(|seconds| seconds as f64);
        self.status = self.localized("正在匹配：{}", std::slice::from_ref(&stem));
        cx.notify();

        let settings = self.settings.clone();
        let work_stem = stem.clone();
        let work = cx.background_spawn(async move {
            let session = Session::new(settings);
            sodam_core::match_metadata::search_candidates(&session, &work_stem, duration_hint)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| match result {
                Ok(candidates) => {
                    // 首选够阈值就直接应用，省一步点击。
                    let best = candidates
                        .iter()
                        .find(|outcome| {
                            outcome.score >= sodam_core::match_metadata::MATCH_THRESHOLD
                        })
                        .cloned();
                    match best {
                        Some(outcome) => {
                            root.apply_match(&relative, &outcome.track, outcome.score);
                            root.refresh_local(cx);
                        }
                        None if candidates.is_empty() => {
                            root.status =
                                root.localized("未找到匹配：{}", std::slice::from_ref(&stem));
                            cx.notify();
                        }
                        None => {
                            // 有候选但都不够可信 → 让用户自己挑。
                            let count = candidates.len();
                            root.status = root
                                .localized("有 {} 个相似结果，请手动选择", &[count.to_string()]);
                            root.match_picker = Some((relative, candidates));
                            cx.notify();
                        }
                    }
                }
                Err(err) => {
                    root.status = root.localized("匹配失败：{err}", &[err.to_string()]);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 打开「手动选择匹配」浮层：直接拉候选列表（不尝试自动应用）。
    pub fn open_match_picker(&mut self, relative: PathBuf, cx: &mut Context<Self>) {
        if self.settings.cookie.trim().is_empty() {
            self.status = self.tr("登录后才能使用搜索、歌单与播放").to_string();
            cx.notify();
            return;
        }
        let Some(file) = self
            .local_files
            .iter()
            .find(|file| file.relative == relative)
            .cloned()
        else {
            return;
        };
        let stem = file
            .relative
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let duration_hint = file.duration_seconds().map(|seconds| seconds as f64);
        self.status = self.localized("正在搜索：{}", std::slice::from_ref(&stem));
        cx.notify();

        let settings = self.settings.clone();
        let work_stem = stem.clone();
        let work = cx.background_spawn(async move {
            let session = Session::new(settings);
            sodam_core::match_metadata::search_candidates(&session, &work_stem, duration_hint)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| match result {
                Ok(candidates) if candidates.is_empty() => {
                    root.status = root.localized("未找到匹配：{}", std::slice::from_ref(&stem));
                    cx.notify();
                }
                Ok(candidates) => {
                    root.match_picker = Some((relative, candidates));
                    cx.notify();
                }
                Err(err) => {
                    root.status = root.localized("匹配失败：{err}", &[err.to_string()]);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 用户在浮层里选定某条候选 → 写回清单。
    pub fn apply_picked_match(
        &mut self,
        relative: PathBuf,
        track: TrackItem,
        cx: &mut Context<Self>,
    ) {
        self.match_picker = None;
        // 手动选择没有自动分数，用 0 表示"用户指定"。
        self.apply_match(&relative, &track, 0);
        self.refresh_local(cx);
    }

    /// 批量匹配：把所有缺元数据的本地文件逐个匹配，直到全部处理完。
    ///
    /// **串行 + 限速**（每首之间歇一下）：批量匹配会打搜索接口，
    /// 并发或全速会把签名服务打爆（`MODIFICATIONS.md` 里记过同类教训）。
    /// 每完成一首就刷新一次列表 —— 用户能看着条目一个个「长」出元数据。
    pub fn match_all_local_files(&mut self, cx: &mut Context<Self>) {
        if self.matching.is_some() {
            return;
        }
        if self.settings.cookie.trim().is_empty() {
            self.status = self.tr("登录后才能使用搜索、歌单与播放").to_string();
            cx.notify();
            return;
        }
        // 只处理「没有清单记录」或「记录里没有 track_id」的文件 ——
        // 已经有完整元数据的不必再匹配。
        let targets: Vec<(PathBuf, String, Option<f64>)> = self
            .local_files
            .iter()
            .filter(|file| {
                file.entry
                    .as_ref()
                    .map(|entry| entry.track_id.is_none())
                    .unwrap_or(true)
            })
            .map(|file| {
                let stem = file
                    .relative
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default();
                (
                    file.relative.clone(),
                    stem,
                    file.duration_seconds().map(|s| s as f64),
                )
            })
            .collect();
        if targets.is_empty() {
            self.status = self.tr("没有需要匹配的文件").to_string();
            cx.notify();
            return;
        }
        let total = targets.len();
        self.matching = Some((0, total));
        self.status = self.localized("开始匹配 {} 个文件…", &[total.to_string()]);
        cx.notify();
        self.run_match_batch(targets, cx);
    }

    /// 批量匹配的驱动：每次处理一首，完成后等一小会儿再处理下一首。
    fn run_match_batch(
        &mut self,
        mut targets: Vec<(PathBuf, String, Option<f64>)>,
        cx: &mut Context<Self>,
    ) {
        let Some((relative, stem, duration_hint)) = targets.first().cloned() else {
            // 全部处理完
            let (done, total) = self.matching.unwrap_or((0, 0));
            self.matching = None;
            self.status = self.localized(
                "匹配完成：{} 首（共 {} 首）",
                &[done.to_string(), total.to_string()],
            );
            cx.notify();
            self.refresh_local(cx);
            return;
        };
        targets.remove(0);
        let settings = self.settings.clone();
        let work = cx.background_spawn(async move {
            let session = Session::new(settings);
            sodam_core::match_metadata::match_track(&session, &stem, duration_hint)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                match result {
                    Ok(Some(outcome)) => {
                        root.apply_match(&relative, &outcome.track, outcome.score);
                        if let Some((done, total)) = root.matching.as_mut() {
                            *done += 1;
                            let text = format!("{} / {}", *done, *total);
                            root.status = root.localized("匹配中：{}", &[text]);
                        }
                    }
                    Ok(None) => {
                        if let Some((done, total)) = root.matching.as_mut() {
                            *done += 1;
                            let text = format!("{} / {}", *done, *total);
                            root.status = root.localized("匹配中：{}", &[text]);
                        }
                    }
                    Err(err) => {
                        // 网络类失败：中断整批（继续只会连着失败）
                        root.matching = None;
                        root.status = root.localized("匹配失败：{err}", &[err.to_string()]);
                        cx.notify();
                        return;
                    }
                }
                cx.notify();
            });
            // 限速：给搜索接口留口气，也让界面有机会刷新。
            // 注意：`await` 必须在 `update` 闭包**之外** —— 闭包是同步的。
            cx.background_executor()
                .timer(std::time::Duration::from_millis(MATCH_BATCH_INTERVAL_MS))
                .await;
            let _ = this.update(cx, |root, cx| {
                root.run_match_batch(targets, cx);
            });
        })
        .detach();
    }

    /// 把匹配结果写回清单（保留原有音质与入库时间）。
    fn apply_match(&mut self, relative: &PathBuf, track: &TrackItem, score: i32) {
        let settings = self.settings.download.clone();
        let existing = sodam_core::download_library::load(&settings)
            .into_iter()
            .find(|entry| &entry.relative == relative);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|delta| delta.as_secs() as i64)
            .unwrap_or(0);
        let entry = sodam_core::download_library::LibraryEntry {
            relative: relative.clone(),
            extension: existing
                .as_ref()
                .map(|entry| entry.extension.clone())
                .or_else(|| {
                    self.local_files
                        .iter()
                        .find(|file| &file.relative == relative)
                        .map(|file| file.extension.clone())
                })
                .unwrap_or_else(|| "m4a".to_string()),
            source: sodam_core::download_library::EntrySource::Scanned,
            bytes: existing
                .as_ref()
                .map(|entry| entry.bytes)
                .or_else(|| {
                    self.local_files
                        .iter()
                        .find(|file| &file.relative == relative)
                        .map(|file| file.bytes)
                })
                .unwrap_or(0),
            added_at: existing.as_ref().map(|entry| entry.added_at).unwrap_or(now),
            // 音质未知：文件是用户自己放进来 / 迁移来的，不冒充已知音质。
            quality: existing.as_ref().and_then(|entry| entry.quality.clone()),
            downloaded_at: existing.as_ref().and_then(|entry| entry.downloaded_at),
            mtime: existing.as_ref().and_then(|entry| entry.mtime),
            track_id: Some(track.id.clone()),
            title: Some(track.title.clone()),
            artist: Some(track.artist.clone()),
            album: Some(track.album.clone()),
            artist_id: Some(track.artist_id.clone()),
            album_id: Some(track.album_id.clone()),
            cover: Some(track.cover.clone()),
            duration_seconds: Some(track.duration_seconds).filter(|seconds| *seconds > 0),
            vip: Some(track.vip),
        };
        match sodam_core::download_library::upsert(&settings, entry) {
            Ok(_) => {
                self.status = self.localized(
                    "已匹配：{}（{} 分）",
                    &[track.title.clone(), score.to_string()],
                );
            }
            Err(err) => {
                self.status = self.localized("写入清单失败：{err}", &[err.to_string()]);
            }
        }
    }

    /// 随机播放整张歌单：清空当前队列，按洗牌后的顺序从第一首开始。
    pub fn shuffle_play_arc(
        &mut self,
        tracks: std::sync::Arc<Vec<TrackItem>>,
        cx: &mut Context<Self>,
    ) {
        let shuffled = Queue::shuffled(tracks.as_ref().clone());
        self.play_from(shuffled, 0, cx);
    }

    /// 点歌单/收藏列表里的某一行播放（把该列表整体入队）。
    pub fn play_from(&mut self, tracks: Vec<TrackItem>, index: usize, cx: &mut Context<Self>) {
        if tracks.is_empty() {
            return;
        }
        let title = tracks
            .get(index)
            .map(|track| track.title.clone())
            .unwrap_or_default();
        let covers: Vec<String> = tracks.iter().map(|track| track.cover.clone()).collect();
        self.queue.replace(tracks, index);
        // 列表整体换队时清掉旧播放历史，避免抽屉把上一批队列误标成“已播放”。
        self.played_history.clear();
        self.queue.mode = sodam_core::queue::PlayMode::Sequential;
        self.queue_origin = match self.nav {
            Nav::Liked => QueueOrigin::Liked,
            Nav::Library => self
                .open_playlist
                .as_ref()
                .map(|(id, _, _)| QueueOrigin::Playlist(id.clone()))
                .unwrap_or(QueueOrigin::Playlist(String::new())),
            Nav::Artist => self
                .open_artist
                .as_ref()
                .map(|detail| QueueOrigin::Artist(detail.artist.id.clone()))
                .unwrap_or(QueueOrigin::Search),
            Nav::Album => self
                .open_album
                .as_ref()
                .map(|detail| QueueOrigin::Album(detail.album.id.clone()))
                .unwrap_or(QueueOrigin::Search),
            Nav::Search => QueueOrigin::Search,
            Nav::Home => QueueOrigin::Feed,
            _ => self.queue_origin.clone(),
        };
        self.set_status("已入队：{}", std::slice::from_ref(&title));
        self.sync_queue_cache();
        self.ensure_covers(&covers, cx);
        self.start_track(cx);
    }

    /// 打开搜索结果里的歌单：带着已知标题，切换到歌单详情页。
    pub fn open_search_playlist(&mut self, playlist: PlaylistItem, cx: &mut Context<Self>) {
        if self.loading_search_playlist {
            return;
        }
        let playlist_id = playlist.id.clone();
        self.search_playlist = Some((
            playlist_id.clone(),
            playlist.title.clone(),
            Arc::new(Vec::new()),
        ));
        self.loading_search_playlist = true;
        self.set_status("正在读取歌单「{}」…", std::slice::from_ref(&playlist.title));
        cx.notify();

        let settings = self.settings.clone();
        let work_playlist_id = playlist_id.clone();
        let work = cx.background_spawn(async move {
            sodam_core::library::playlist_tracks(&Session::new(settings), &work_playlist_id)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                root.loading_search_playlist = false;
                match result {
                    Ok(tracks) => {
                        let count = tracks.len();
                        root.search_playlist_detail_list.reset(count + 1);
                        let head: Vec<String> = tracks
                            .first()
                            .map(|track| vec![track.cover.clone()])
                            .unwrap_or_default();
                        root.search_playlist =
                            Some((playlist_id, playlist.title, Arc::new(tracks)));
                        root.ensure_covers(&head, cx);
                        root.set_status("搜索歌单共 {} 首", &[count.to_string()]);
                    }
                    Err(err) => {
                        root.search_playlist = None;
                        root.set_status("读取搜索歌单失败：{err}", &[err.to_string()]);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 单曲右键「下一首播放」：插到当前曲目后面，不打断当前歌。
    pub fn play_track_next(&mut self, track: TrackItem, cx: &mut Context<Self>) {
        let title = track.title.clone();
        let cover = track.cover.clone();
        let was_empty = self.queue.insert_next(vec![track]);
        self.track_menu = None;
        self.set_status("下一首将播放：{}", std::slice::from_ref(&title));
        self.sync_queue_cache();
        self.ensure_covers(std::slice::from_ref(&cover), cx);
        if was_empty {
            self.start_track(cx);
        } else {
            self.spawn_prefetch(cx);
        }
        cx.notify();
    }

    /// 打开歌单：拉取曲目并展示（标题来自列表，曲目来自接口）。
    pub fn open_playlist(
        &mut self,
        playlist_id: String,
        known_title: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.loading_playlist {
            return;
        }
        let playlist_title = known_title.unwrap_or_else(|| {
            self.playlists
                .iter()
                .find(|playlist| playlist.id == playlist_id)
                .map(|playlist| playlist.title.clone())
                .unwrap_or_else(|| self.tr("歌单").to_string())
        });

        // 立刻切到详情页（曲目先空着），页面内显示 loading ——
        // 不然大歌单要等好几秒才「进得去」。
        self.open_playlist = Some((
            playlist_id.clone(),
            playlist_title.clone(),
            Arc::new(Vec::new()),
        ));
        self.library_detail_list.reset(1);
        self.loading_playlist = true;
        self.status = self.tr("正在读取歌单曲目…").to_string();
        cx.notify();

        let settings = self.settings.clone();
        let work_id = playlist_id.clone();
        let work = cx.background_spawn(async move {
            let session = Session::new(settings);
            sodam_core::library::playlist_tracks(&session, &work_id)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                root.loading_playlist = false;
                match result {
                    Ok(tracks) => {
                        // 系统歌单「Ta喜欢的音乐」在部分账号上普通详情接口会返回空；
                        // 已有 liked 全量数据时直接兜底，避免详情页空白。
                        let tracks = if tracks.is_empty()
                            && playlist_title.contains("喜欢")
                            && !root.liked.is_empty()
                        {
                            root.liked.as_ref().clone()
                        } else {
                            tracks
                        };
                        root.set_status("歌单共 {} 首", &[tracks.len().to_string()]);
                        let actual = tracks.len() as i64;
                        // 接口给的 track_count 可能是过期值（实测 361 vs 实际 414），
                        // 以实际拉到的曲目数为准回写网格。
                        for playlist in root.playlists.iter_mut() {
                            if playlist.id == playlist_id {
                                playlist.track_count = actual;
                            }
                        }
                        let final_count = tracks.len();
                        root.open_playlist = Some((playlist_id, playlist_title, Arc::new(tracks)));
                        root.library_detail_list.reset(final_count + 1);
                    }
                    Err(err) => {
                        root.set_status("读取歌单失败：{err}", &[err.to_string()]);
                        root.open_playlist = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn toggle_play(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.engine.snapshot();
        if snapshot.track_id.is_empty() {
            if self.queue.is_empty() {
                self.status = self.tr("队列是空的：先去搜索或打开一张歌单").to_string();
                cx.notify();
                return;
            }
            // 队列里有歌但引擎还没装载 → 装载当前曲目
            self.start_track(cx);
            return;
        }
        self.engine.toggle();
        self.playing = !snapshot.playing;
        self.status = if self.playing {
            self.tr("播放中").to_string()
        } else {
            self.tr("已暂停").to_string()
        };
        cx.notify();
    }

    /// 装载并播放「当前队列曲目」：后台下载+解密 → 回 UI 线程交给引擎。
    ///
    /// 本地音乐队列（`QueueOrigin::Local`）是例外：文件已在磁盘上，
    /// 直接交给引擎，不走进度条前面的网络路径（见 [`Self::play_local_file`]）。
    pub fn start_track(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.queue.current().cloned() else {
            self.status = self.tr("队列是空的：先去搜索或打开一张歌单").to_string();
            cx.notify();
            return;
        };
        // 本地曲目：从磁盘直接装载。合成 id（`local:` 前缀）没有对应的汽水曲目，
        // 走网络只会连续失败（自动切下一首时会踩到）。
        if let Some(path) = self.local_playback_path(&track) {
            self.playing = true;
            self.play_seq = self.play_seq.wrapping_add(1);
            self.pending_track = None;
            self.progress_preview = None;
            self.playback_error = None;
            self.consecutive_failures = 0;
            // 歌词：有旁路 `.lrc` 就用它，否则清掉上一首的（本地文件没有汽水曲目）。
            self.load_local_lyrics(&track);
            let quality = self
                .local_quality_for(&track)
                .unwrap_or_else(|| self.tr("未知音质").to_string());
            self.engine.load(track.clone(), path, quality);
            self.set_status("播放中：{}", std::slice::from_ref(&track.title));
            // 封面：与播放页取图来源一致（本地曲目走旁路 `file://` 伪 URL）。
            let cover_source = self.playback_cover_source(&track);
            self.ensure_covers(std::slice::from_ref(&cover_source), cx);
            cx.notify();
            return;
        }
        if std::env::var("SODAM_PLAYER_LOG").is_ok() {
            eprintln!("[player] 开始装载：{}（id={}）", track.title, track.id);
        }
        self.playing = true;
        self.play_seq = self.play_seq.wrapping_add(1);
        let request_seq = self.play_seq;
        self.pending_track = Some(track.clone());
        self.progress_preview = None;
        self.set_status("正在准备播放：{}…", std::slice::from_ref(&track.title));
        self.ensure_covers(std::slice::from_ref(&track.cover), cx);
        if self.nav == Nav::Lyrics && !is_local_track(&track) {
            self.load_lyrics(track.clone(), false, cx);
        }
        cx.notify();

        let settings = self.settings.clone();
        let work_track = track.clone();
        // 拉流失败重试 2 次（共 3 次），退避逐渐拉长
        let work = cx.background_spawn(async move {
            let session = Session::new(settings);
            let mut last_err = None;
            for attempt in 1..=3 {
                match session.download_to_cache(&work_track) {
                    Ok(cached) => return Ok(cached),
                    Err(err) => {
                        if std::env::var("SODAM_PLAYER_LOG").is_ok() {
                            eprintln!("[player] 第 {attempt} 次拉流失败：{err}");
                        }
                        last_err = Some(err);
                        if attempt < 3 {
                            std::thread::sleep(std::time::Duration::from_millis(
                                800 * attempt as u64,
                            ));
                        }
                    }
                }
            }
            Err(last_err.unwrap_or_else(|| anyhow::anyhow!("拉流失败")))
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |root, cx| {
                // 这次加载已经被更新的播放请求取代 → 整个结果作废
                if root.play_seq != request_seq {
                    if std::env::var("SODAM_PLAYER_LOG").is_ok() {
                        eprintln!(
                            "[player] 丢弃过期加载结果（seq={request_seq} 当前={}）：{}",
                            root.play_seq, track.title
                        );
                    }
                    return;
                }
                match result {
                    Ok(cached) => {
                        root.engine.load(track.clone(), cached.path, cached.quality);
                        if std::env::var("SODAM_PLAYER_LOG").is_ok() {
                            eprintln!("[player] 装载完成：{}", track.title);
                        }
                        root.consecutive_failures = 0;
                        root.playback_error = None;
                        // 记录真实播放历史（去重最近一条，避免重复刷屏）
                        if root.played_history.last().map(String::as_str) != Some(track.id.as_str())
                        {
                            root.played_history.push(track.id.clone());
                            let overflow = root.played_history.len().saturating_sub(100);
                            if overflow > 0 {
                                root.played_history.drain(0..overflow);
                            }
                        }
                        root.set_status("播放中：{}", std::slice::from_ref(&track.title));
                        // 开播后立刻预取下一首，切歌时基本是秒开
                        root.spawn_prefetch(cx);
                    }
                    Err(err) => {
                        if std::env::var("SODAM_PLAYER_LOG").is_ok() {
                            eprintln!("[player] 装载失败：{} → {err}", track.title);
                        }
                        root.pending_track = None;
                        root.consecutive_failures += 1;
                        let message = root.localized(
                            "「{}」拉流失败：{err}",
                            &[track.title.to_string(), err.to_string()],
                        );
                        root.status = message.clone();
                        root.playback_error = Some(message);
                        if root.consecutive_failures >= 3 {
                            root.playing = false;
                            root.status = root
                                .tr("连续 3 首拉流失败，已暂停（检查网络或签名服务）")
                                .to_string();
                        } else if !root.queue.is_empty() {
                            root.next_track(cx);
                        } else {
                            root.playing = false;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn next_track(&mut self, cx: &mut Context<Self>) {
        // 切歌时丢掉进度预览，避免「拖到一半换歌」把 seek 用到新歌上。
        // 队列当前项不会被移除（remove 拒绝当前项），所以直接推进即可。
        self.progress_preview = None;
        self.queue.advance();
        self.sync_queue_cache();
        self.start_track(cx);
    }

    pub fn prev_track(&mut self, cx: &mut Context<Self>) {
        self.progress_preview = None;
        self.queue.rewind();
        self.sync_queue_cache();
        self.start_track(cx);
    }

    /// 设置页保存后调用：落盘并重建会话。
    #[allow(dead_code)] // 设置表单（roadmap 第 3 项）接入后由保存按钮调用
    pub fn apply_settings(&mut self, settings: Settings) {
        let reporter_settings = settings.clone();
        let download_settings = settings.download.clone();
        match self
            .session
            .as_mut()
            .map(|session| session.apply(settings.clone()))
        {
            Some(Ok(())) => {
                self.settings = settings;
                self.status = self.tr("设置已保存").to_string();
            }
            Some(Err(err)) => self.status = self.localized("保存失败：{err}", &[err.to_string()]),
            None => {
                self.settings = settings;
                self.session = Some(Session::new(self.settings.clone()));
            }
        }
        self.play_reporter.apply_settings(reporter_settings);
        // 下载管理器持有的是设置快照：换账号 / 改音质后必须同步，
        // 否则队列里新排的任务还会用旧的 Cookie 去取流。
        self.downloads.set_settings(self.settings.clone());
        self.downloads.set_download_settings(download_settings);
        self.download_settings_fingerprint = self.settings.fingerprint();
    }
}

/// 跨平台「用系统默认应用打开」（文件或 URL 均可）：
/// macOS `open`、Windows `cmd /C start ""`（空标题防 URL 被当窗口名）、
/// 其他 `xdg-open`。fire-and-forget：不等待打开结果，UI 动作不阻塞。
fn system_open(target: &str) -> std::io::Result<std::process::Child> {
    #[cfg(target_os = "macos")]
    return std::process::Command::new("open").arg(target).spawn();
    #[cfg(windows)]
    return {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", target])
            .spawn()
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    return std::process::Command::new("xdg-open").arg(target).spawn();
}

/// 预取失败冷却：失败的曲目 60s 内不再重试（巡检会把永久失败的歌无限重试，
/// 持续打签名服务和网络）。
const PREFETCH_RETRY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

/// 预取数量：环境变量 `SODAM_PREFETCH_COUNT`（默认 3，解析失败/为 0 用默认）。
fn prefetch_ahead_count() -> usize {
    std::env::var("SODAM_PREFETCH_COUNT")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
        .unwrap_or(3)
}

/// 预取并发上限：环境变量 `SODAM_PREFETCH_CONCURRENCY`（默认 2）。
fn prefetch_concurrency() -> usize {
    std::env::var("SODAM_PREFETCH_CONCURRENCY")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
        .unwrap_or(2)
}
