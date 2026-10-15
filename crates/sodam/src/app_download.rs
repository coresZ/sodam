//! 下载动作层：入队、面板控制、设置、打开文件夹。
//!
//! 所有网络 / 落盘都在 `sodam-core` 的下载线程里跑；这里只做三件事：
//! 把用户意图翻成管理器命令、把事件翻成界面提示、维护面板的渲染快照。

use gpui::Context;

use sodam_core::download::{
    DownloadEvent, DownloadSettings, DownloadState, DownloadTask, EnqueueReport,
};
use sodam_core::models::TrackItem;

use super::{DownloadTab, Root};

/// 超过这个数量就不逐条报名字，只报总数（批量下载时状态行不会被刷爆）。
const BULK_NOTICE_THRESHOLD: usize = 5;

impl Root {
    /// 把最新的设置推给下载管理器（只在网络相关字段变化时深拷贝一次）。
    pub(crate) fn sync_download_settings(&mut self) {
        let fingerprint = self.settings.fingerprint();
        if fingerprint == self.download_settings_fingerprint {
            return;
        }
        self.download_settings_fingerprint = fingerprint;
        self.downloads.set_settings(self.settings.clone());
    }

    /// 刷新下载快照（心跳里调用；只有版本号变化才重新克隆任务列表）。
    pub(crate) fn sync_downloads(&mut self, cx: &mut Context<Self>) {
        self.sync_download_settings();
        let revision = self.downloads.revision();
        let mut changed = false;
        if self.download_cache.0 != revision {
            self.download_cache = (revision, std::sync::Arc::new(self.downloads.tasks()));
            self.download_counts = self.downloads.counts();
            changed = true;
        }
        for event in self.downloads.drain_events() {
            changed = true;
            match event {
                DownloadEvent::Completed {
                    title,
                    path,
                    open_when_done,
                } => {
                    self.set_status("「{}」下载完成", std::slice::from_ref(&title));
                    // 刚落盘的文件要进「已下载」标记，重扫一次下载目录。
                    self.refresh_local(cx);
                    if open_when_done {
                        if let Some(dir) = path.parent() {
                            self.open_folder(dir, cx);
                        }
                    }
                }
                DownloadEvent::Failed { title, error } => {
                    self.status = self.localized("「{}」下载失败：{err}", &[title, error]);
                }
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// 当前下载任务快照（渲染只读缓存，不直接问管理器）。
    pub fn download_tasks(&self) -> std::sync::Arc<Vec<DownloadTask>> {
        self.download_cache.1.clone()
    }

    /// 下载面板当前分区的任务（最新的排在最上面）。
    pub fn download_tab_tasks(&self, tab: DownloadTab) -> Vec<DownloadTask> {
        self.download_tasks()
            .iter()
            .rev()
            .filter(|task| tab.matches(&task.state))
            .cloned()
            .collect()
    }

    /// 下载单曲；`open_when_done` 为真时完成后自动打开所在文件夹。
    pub fn download_track(
        &mut self,
        track: TrackItem,
        open_when_done: bool,
        cx: &mut Context<Self>,
    ) {
        let report = self
            .downloads
            .enqueue(std::slice::from_ref(&track), "", open_when_done);
        self.report_enqueue(&report, std::slice::from_ref(&track.title));
        self.sync_downloads(cx);
    }

    /// 批量下载（歌单 / 专辑 / 收藏）；来源名会显示在任务行上。
    pub fn download_tracks(
        &mut self,
        tracks: Vec<TrackItem>,
        source: String,
        cx: &mut Context<Self>,
    ) {
        if tracks.is_empty() {
            self.status = self.tr("没有可下载的曲目").to_string();
            cx.notify();
            return;
        }
        let report = self.downloads.enqueue(&tracks, &source, false);
        let names: Vec<String> = tracks.iter().map(|track| track.title.clone()).collect();
        self.report_enqueue(&report, &names);
        self.sync_downloads(cx);
    }

    /// 入队结果 → 状态行文案。
    fn report_enqueue(&mut self, report: &EnqueueReport, titles: &[String]) {
        if report.added == 0 {
            self.status = if report.skipped > 0 {
                self.tr("这些歌曲已在下载队列里").to_string()
            } else {
                self.tr("没有可下载的曲目").to_string()
            };
            return;
        }
        if report.added >= BULK_NOTICE_THRESHOLD || report.skipped > 0 {
            self.status = self.localized(
                "已加入下载队列：{} 首（跳过 {} 首已在队列）",
                &[report.added.to_string(), report.skipped.to_string()],
            );
            return;
        }
        let label = titles
            .first()
            .cloned()
            .unwrap_or_else(|| self.tr("下载").to_string());
        self.status = self.localized("已加入下载队列：{}", std::slice::from_ref(&label));
    }

    /// 打开下载面板（与队列抽屉互斥，两个右侧面板叠在一起会互相遮挡）。
    pub fn open_downloads(&mut self, cx: &mut Context<Self>) {
        self.download_open = true;
        self.queue_open = false;
        self.sync_downloads(cx);
    }

    pub fn toggle_downloads(&mut self, cx: &mut Context<Self>) {
        if self.download_open {
            self.download_open = false;
            cx.notify();
        } else {
            self.open_downloads(cx);
        }
    }

    pub fn set_download_tab(&mut self, tab: DownloadTab, cx: &mut Context<Self>) {
        self.download_tab = tab;
        cx.notify();
    }

    /// 暂停 / 继续 / 重试单条任务（按当前状态决定语义）。
    pub fn toggle_download_task(&mut self, task: &DownloadTask, cx: &mut Context<Self>) {
        match task.state {
            DownloadState::Paused => self.downloads.resume(task.id),
            DownloadState::Failed { .. } | DownloadState::Cancelled => {
                self.downloads.resume(task.id)
            }
            DownloadState::Queued | DownloadState::Running { .. } => self.downloads.pause(task.id),
            DownloadState::Completed { .. } => {}
        }
        self.sync_downloads(cx);
    }

    /// 取消（进行中的）并移除这一行 —— 面板上「×」按钮的语义：
    /// 进行中 / 排队的先中止传输，已完成 / 失败的只删记录。
    pub fn discard_download_task(&mut self, id: u64, cx: &mut Context<Self>) {
        self.downloads.cancel(id);
        self.downloads.remove(id);
        self.sync_downloads(cx);
    }

    pub fn pause_all_downloads(&mut self, cx: &mut Context<Self>) {
        self.downloads.pause_all();
        self.status = self.tr("已暂停全部下载").to_string();
        self.sync_downloads(cx);
    }

    pub fn resume_all_downloads(&mut self, cx: &mut Context<Self>) {
        self.downloads.resume_all();
        self.status = self.tr("已继续全部下载").to_string();
        self.sync_downloads(cx);
    }

    pub fn clear_finished_downloads(&mut self, cx: &mut Context<Self>) {
        self.downloads.clear_finished();
        self.status = self.tr("下载任务已清空").to_string();
        self.sync_downloads(cx);
    }

    /// 打开文件所在目录（跨平台，fire-and-forget）。
    pub fn open_folder(&mut self, dir: &std::path::Path, cx: &mut Context<Self>) {
        match system_open_path(dir) {
            Ok(()) => {
                self.status = self.localized("已打开文件夹：{}", &[dir.display().to_string()]);
            }
            Err(err) => {
                self.status = self.localized("打开文件夹失败：{err}", &[err.to_string()]);
            }
        }
        cx.notify();
    }

    /// 打开下载目录（设置页与面板都走这里）。
    pub fn open_download_dir(&mut self, cx: &mut Context<Self>) {
        let dir = self.settings.download.resolve_dir();
        // 目录可能还没建（用户刚改完路径）：先建再开，否则文件管理器会弹错误。
        if let Err(err) = std::fs::create_dir_all(&dir) {
            self.status = self.localized("打开文件夹失败：{err}", &[err.to_string()]);
            cx.notify();
            return;
        }
        self.open_folder(&dir, cx);
    }

    /// 设置页保存下载设置。
    pub fn apply_download_settings(&mut self, settings: DownloadSettings, cx: &mut Context<Self>) {
        self.settings.download = settings.clone();
        match self.settings.save() {
            Ok(()) => {
                self.downloads.set_download_settings(settings);
                self.status = self.tr("下载设置已保存").to_string();
            }
            Err(err) => {
                self.status = self.localized("保存失败：{err}", &[err.to_string()]);
            }
        }
        // 目录或命名模板可能变了：本地音乐列表与「已下载」快照都要跟着重算。
        self.refresh_local(cx);
        cx.notify();
    }

    /// 下载管理里的「清理记录」：清掉失败 / 已取消的任务，以及清单里已失效的条目。
    ///
    /// **一律不碰磁盘文件** —— 删文件是另一件事（本地音乐页的「删除本地文件」）。
    /// 所以清理之后，「本地音乐」里那些文件仍然在（只是退回成没有元数据的裸文件）。
    pub fn clear_download_records(&mut self, cx: &mut Context<Self>) {
        let mut cleared = 0usize;

        // 失败 / 已取消的任务：逐条从管理器移除（用现成的 remove，不新增核心 API）。
        // 迭代的是快照，所以边遍历边移除是安全的。
        for task in self
            .downloads
            .tasks()
            .iter()
            .filter(|task| task.state.is_failed())
        {
            self.downloads.remove(task.id);
            cleared += 1;
        }

        // 清单里有、磁盘上已经没了的条目：记录也一并清掉（文件本来就不在了）。
        for entry in self.library_missing.as_ref() {
            if let Err(err) =
                sodam_core::download_library::remove(&self.settings.download, &entry.relative)
            {
                if std::env::var("SODAM_DOWNLOAD_LOG").is_ok() {
                    eprintln!("[download] 清理下载记录失败：{err}");
                }
            } else {
                cleared += 1;
            }
        }

        self.status = self.localized("已清理 {} 条记录", &[cleared.to_string()]);
        self.refresh_local(cx);
        cx.notify();
    }
}

/// 用系统文件管理器打开一个目录（fire-and-forget：不等结果）。
fn system_open_path(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    return std::process::Command::new("open")
        .arg(path.as_os_str())
        .spawn()
        .map(|_| ());
    #[cfg(windows)]
    return std::process::Command::new("explorer")
        .arg(path.as_os_str())
        .spawn()
        .map(|_| ());
    #[cfg(all(unix, not(target_os = "macos")))]
    return std::process::Command::new("xdg-open")
        .arg(path.as_os_str())
        .spawn()
        .map(|_| ());
}
