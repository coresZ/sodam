//! 本地音乐页：横向三个入口 —— 本地音乐 / 下载管理 / 扫描本地音乐。
//!
//! **两个「列表」的区别是刻意的，不要把它们并成一份数据：**
//!
//! * **本地音乐** 以**磁盘为准**（`Root.local_files`）：下载目录里真实存在的每一个音频，
//!   包括用户自己手动放进来的（那些没有元数据，只显示文件名）。
//! * **下载管理 → 已下载** 以**记录为准**（`Root.library`）：本应用下载留下的记录，
//!   带音质 / 大小，跨重启存在。
//!
//! 下载管理的三个子 tab：
//!
//! * **下载中** —— 内存任务里还在跑的（排队 / 传输中 / 已暂停）
//! * **下载失败** —— 失败 / 已取消的任务 + 清单里已失效的记录（文件被删或移走）
//! * **已下载** —— 下载记录（来自清单，不依赖内存任务）
//!
//! 「清理记录」只清记录、**一律不碰磁盘文件** —— 删文件是另一件事（M2）。
//!
//! 语义提醒：这里的「有」= 磁盘上有这个文件；**不代表播放会走本地文件**
//! （播放用独立的播放缓存，见 `session::cached_track`）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui::prelude::*;
use gpui::{
    div, point, px, svg, uniform_list, AnyElement, ClickEvent, Context, IntoElement, MouseButton,
    MouseDownEvent,
};

use crate::app::{DownloadTab, LocalFile, LocalTab, Root};
use crate::ui::artwork::cover;
use crate::ui::i18n::Language;
use crate::ui::{icons, theme};
use sodam_core::download::{DownloadState, DownloadTask};
use sodam_core::download_library::LibraryEntry;

use super::downloads::human_bytes;
use super::empty_state;
use super::settings::segmented;

/// 行高与 `track_row` 一致，切页时列表手感不跳。
const ROW_H: f32 = theme::size::LIST_ROW + theme::space::XS;
const COL_ARTIST: f32 = 180.0;
const COL_TIME: f32 = 60.0;
const COL_SIZE: f32 = 84.0;
const COL_QUALITY: f32 = 96.0;
/// 进度条宽度（下载中的行）。
const PROGRESS_W: f32 = 160.0;

// ---------------------------------------------------------------------------
// 页面骨架
// ---------------------------------------------------------------------------

/// 本地音乐页。
pub(crate) fn local_music_page(root: &Root, cx: &mut Context<Root>) -> AnyElement {
    let tabs = segmented(
        "local-tab",
        vec![
            (LocalTab::Songs, root.tr("本地音乐").to_string()),
            (LocalTab::Downloads, root.tr("下载管理").to_string()),
            (LocalTab::Scan, root.tr("扫描本地音乐").to_string()),
        ],
        &root.local_tab,
        |tab, root, cx| {
            root.local_tab = tab;
            cx.notify();
        },
        cx,
    );

    let body = match root.local_tab {
        LocalTab::Songs => songs_view(root, cx),
        LocalTab::Downloads => downloads_view(root, cx),
        LocalTab::Scan => scan_placeholder(root),
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .gap(px(theme::space::LG))
        .child(tabs)
        .when(root.local_tab == LocalTab::Songs, |this| {
            this.child(songs_toolbar(root, cx))
        })
        .child(body)
        .into_any_element()
}

/// 「本地音乐」的工具栏：批量「补全信息」+ 进度。
///
/// 只在有**待匹配**文件（没有汽水 id 的）时显示按钮，避免空页面挂个没用的入口。
fn songs_toolbar(root: &Root, cx: &mut Context<Root>) -> AnyElement {
    let pending = root
        .local_files
        .iter()
        .filter(|file| {
            file.entry
                .as_ref()
                .map(|entry| entry.track_id.is_none())
                .unwrap_or(true)
        })
        .count();

    let mut toolbar = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .min_h(px(theme::size::CONTROL_SM));

    if let Some((done, total)) = root.matching {
        // 匹配进行中：显示进度，按钮禁用（避免重复触发）
        toolbar = toolbar.child(
            div()
                .text_size(theme::Text::Small.size())
                .text_color(theme::text_muted())
                .child(root.localized("正在匹配：{}/{}", &[done.to_string(), total.to_string()])),
        );
        return toolbar.into_any_element();
    }

    if pending == 0 {
        return toolbar.into_any_element();
    }

    let can_match = !root.settings.cookie.trim().is_empty();
    let label = root.localized("补全信息（{} 首）", &[pending.to_string()]);
    toolbar
        .child(
            div()
                .id("local-match-all")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(theme::space::SM))
                .h(px(theme::size::CONTROL_SM))
                .px(px(theme::space::LG))
                .rounded(px(theme::radius::PILL))
                .bg(theme::surface_elevated())
                .cursor_pointer()
                .hover(|style| style.bg(theme::surface_hover()))
                .text_size(theme::Text::Small.size())
                .text_color(if can_match {
                    theme::text()
                } else {
                    theme::text_faint()
                })
                .on_click(cx.listener(move |root, _event: &ClickEvent, _window, cx| {
                    if can_match {
                        root.match_all_local_files(cx);
                    }
                }))
                .child(
                    svg()
                        .path(icons::path("search"))
                        .size(px(theme::ICON_SM))
                        .text_color(theme::text_muted()),
                )
                .child(label),
        )
        .into_any_element()
}

/// 行首封面。`path` 是封面缓存里的本地文件路径；没有时 `cover` 自己会给占位块。
fn row_cover(path: Option<PathBuf>) -> AnyElement {
    cover(path, theme::size::ROW_ART, theme::radius::ART)
}

/// 解析封面 URL → 本地缓存路径。
///
/// 命中缓存直接返回；未命中且 URL 非空时**把它排进请求队列**（虚拟列表只渲染可见行，
/// 所以只有真正渲染出来的行才会发请求）。`covers` 里存的是 URL → 本地文件路径，
/// 拿 URL 本身当路径传给 `cover()` 会永远命中不了。
fn resolve_cover(
    url: Option<&str>,
    covers: &HashMap<String, PathBuf>,
    cover_requests: &Mutex<Vec<String>>,
) -> Option<PathBuf> {
    let url = url?.trim();
    if url.is_empty() {
        return None;
    }
    let cached = covers.get(url).cloned();
    if cached.is_none() {
        if let Ok(mut queue) = cover_requests.lock() {
            if !queue.iter().any(|item| item == url) {
                queue.push(url.to_string());
            }
        }
    }
    cached
}

/// 本地文件行的封面：**旁路优先**，其次清单里的网络封面 URL。
///
/// 旁路图（`<下载目录>/image/<文件名>.png`）就在磁盘上，不需要网络；
/// 用 `file://` 伪 URL 走同一套封面池（`Session::cover_path` 会识别它并跳过下载）。
/// 清单里的 URL 作为回退 —— 迁移过来的库可能没有旁路，但清单里有封面地址。
fn resolve_local_cover(
    file: &LocalFile,
    covers: &HashMap<String, PathBuf>,
    cover_requests: &Mutex<Vec<String>>,
) -> Option<PathBuf> {
    if let Some(path) = file.cover_path.as_ref() {
        let pseudo = format!("file://{}", path.display());
        if let Some(cached) = covers.get(&pseudo).cloned() {
            return Some(cached);
        }
        if let Ok(mut queue) = cover_requests.lock() {
            if !queue.iter().any(|item| item == &pseudo) {
                queue.push(pseudo);
            }
        }
        // 队列消费是异步的（60ms 一轮）；本轮先给占位块，下一帧就有图。
        return None;
    }
    resolve_cover(
        file.entry.as_ref().and_then(|entry| entry.cover.as_deref()),
        covers,
        cover_requests,
    )
}

/// 歌名列。
///
/// 有元数据用主文本色；只有文件名时弱一档 —— 这样「我下载的」和「我自己丢进来的
/// 文件」一眼能分开，而且不需要引入新颜色。
fn title_cell(text: String, known: bool) -> AnyElement {
    div()
        .flex_1()
        .min_w(px(0.0))
        .truncate()
        .text_size(theme::Text::Body.size())
        .text_color(if known {
            theme::text()
        } else {
            theme::text_muted()
        })
        .child(text)
        .into_any_element()
}

/// 定宽、可截断的弱色文字列。
fn text_cell(width: f32, text: String) -> AnyElement {
    div()
        .w(px(width))
        .flex_none()
        .truncate()
        .text_size(theme::Text::Small.size())
        .text_color(theme::text_muted())
        .child(text)
        .into_any_element()
}

/// 右对齐的数字列。
fn number_cell(width: f32, text: String) -> AnyElement {
    div()
        .w(px(width))
        .flex_none()
        .text_right()
        .text_size(theme::Text::Small.size())
        .text_color(theme::text_faint())
        .child(text)
        .into_any_element()
}

/// 一行（非可点）的外壳：统一高、内边距与圆角。
///
/// 返回 `Stateful<Div>`（`div().id()` 的类型），这样调用方还能继续挂 `.on_click`
/// —— 普通 `Div` 上没有那些交互方法。
fn row_shell(id: (&'static str, usize)) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .h(px(ROW_H))
        .px(px(theme::space::MD))
        .rounded(px(theme::radius::ROW))
}

/// 秒 → `mm:ss`。
fn label_from_seconds(seconds: i64) -> String {
    let total = seconds.max(0);
    format!("{:02}:{:02}", total / 60, total % 60)
}

// ---------------------------------------------------------------------------
// 本地音乐（磁盘真相）
// ---------------------------------------------------------------------------

fn songs_view(root: &Root, cx: &mut Context<Root>) -> AnyElement {
    let files = root.local_files.clone();
    if files.is_empty() {
        return empty_state("disc-3", root.tr("下载目录里还没有音频文件"));
    }

    let entity = cx.entity();
    let covers = root.covers.clone();
    let cover_requests = root.cover_requests.clone();
    let count = files.len();

    let list = uniform_list("local-songs-list", count, move |range, _window, _cx| {
        range
            .map(|index| {
                let file = &files[index];
                let entry = file.entry.as_ref();
                let known = entry.is_some();
                let title = entry
                    .map(|entry| entry.display_title())
                    .unwrap_or_else(|| display_stem(&file.relative));
                let artist = entry
                    .map(|entry| entry.display_artist())
                    .unwrap_or_default();
                // 时长：清单权威值 → 容器头探测值（用户手动放入的文件靠它）。
                let duration = file
                    .duration_seconds()
                    .map(label_from_seconds)
                    .unwrap_or_default();
                let quality = entry
                    .and_then(|entry| entry.quality.clone())
                    .unwrap_or_default();
                let cover_path = resolve_local_cover(file, &covers, &cover_requests);

                // 磁盘上有文件就能播（有元数据走清单、没有就用文件名合成），
                // 所以每一行都可点 —— 不再有"点了没反应"的行。
                let entity = entity.clone();
                let files = files.clone();
                let relative = file.relative.clone();
                let click_entity = entity.clone();
                let row = row_shell(("local-file", index))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(move |_event: &ClickEvent, _window, cx: &mut gpui::App| {
                        let files = files.clone();
                        click_entity.update(cx, |root, cx| {
                            root.play_local_file(files, index, cx);
                        });
                    })
                    .on_mouse_down(MouseButton::Right, {
                        let entity = entity.clone();
                        move |event: &MouseDownEvent, _window, cx: &mut gpui::App| {
                            let position = event.position;
                            let relative = relative.clone();
                            entity.update(cx, |root, cx| {
                                root.local_menu = Some((relative, position));
                                cx.notify();
                            });
                        }
                    });

                row.child(row_cover(cover_path))
                    .child(title_cell(title, known))
                    .child(text_cell(COL_ARTIST, artist))
                    .child(number_cell(COL_TIME, duration))
                    .child(number_cell(COL_SIZE, human_bytes(file.bytes)))
                    .child(text_cell(COL_QUALITY, quality))
                    .into_any_element()
            })
            .collect::<Vec<_>>()
    })
    .flex_1()
    .min_h(px(0.0));

    list.into_any_element()
}

/// 没有元数据时，用相对路径的文件名当标题。
fn display_stem(relative: &std::path::Path) -> String {
    relative
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| relative.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// 下载管理（记录为准）
// ---------------------------------------------------------------------------

fn downloads_view(root: &Root, cx: &mut Context<Root>) -> AnyElement {
    let tabs = segmented(
        "download-tab",
        DownloadTab::ALL
            .iter()
            .map(|tab| (*tab, tab.label(root.language).to_string()))
            .collect(),
        &root.download_tab,
        |tab, root, cx| {
            root.download_tab = tab;
            cx.notify();
        },
        cx,
    );

    let toolbar = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .child(tabs)
        .child(div().flex_1())
        .child(clear_records_button(root, cx));

    let body = match root.download_tab {
        DownloadTab::Active => tasks_view(root),
        DownloadTab::Failed => failed_view(root),
        DownloadTab::Completed => records_view(root, cx),
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .gap(px(theme::space::MD))
        .child(toolbar)
        .child(body)
        .into_any_element()
}

/// 「清理记录」：只清记录，不删磁盘文件（删文件是另一件事）。
fn clear_records_button(root: &Root, cx: &mut Context<Root>) -> AnyElement {
    let entity = cx.entity();
    div()
        .id("clear-download-records")
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .h(px(theme::size::CONTROL_SM))
        .px(px(theme::space::LG))
        .rounded(px(theme::radius::PILL))
        .bg(theme::surface_elevated())
        .cursor_pointer()
        .hover(|style| style.bg(theme::surface_hover()))
        .text_size(theme::Text::Small.size())
        .text_color(theme::text_muted())
        .child(root.tr("清理记录"))
        .on_click(move |_event: &ClickEvent, _window, cx: &mut gpui::App| {
            entity.update(cx, |root, cx| root.clear_download_records(cx));
        })
        .into_any_element()
}

/// 下载中。
fn tasks_view(root: &Root) -> AnyElement {
    let tasks: Vec<DownloadTask> = root
        .download_tasks()
        .iter()
        .filter(|task| task.state.is_active())
        .cloned()
        .collect();
    if tasks.is_empty() {
        return empty_state("download", root.tr("没有正在进行的下载"));
    }
    let language = root.language;
    let count = tasks.len();

    uniform_list("download-tasks-list", count, move |range, _window, _cx| {
        range
            .map(|index| {
                let task = &tasks[index];
                let detail = task_detail(task, language);
                let mut row = row_shell(("download-task", index));
                if let Some(progress) = task.state.progress() {
                    row = row.child(
                        div()
                            .w(px(PROGRESS_W))
                            .flex_none()
                            .h(px(4.0))
                            .rounded(px(theme::radius::PILL))
                            .bg(theme::surface_hover())
                            .child(
                                div()
                                    .h_full()
                                    .rounded(px(theme::radius::PILL))
                                    // 暂停中的任务不该看起来还在跑。
                                    .bg(if matches!(task.state, DownloadState::Paused) {
                                        theme::text_faint()
                                    } else {
                                        theme::accent()
                                    })
                                    .w(gpui::relative(progress)),
                            ),
                    );
                }
                row.child(title_cell(task.track.title.clone(), true))
                    .child(
                        div()
                            .flex_none()
                            .text_size(theme::Text::Small.size())
                            .text_color(theme::text_faint())
                            .child(detail),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>()
    })
    .flex_1()
    .min_h(px(0.0))
    .into_any_element()
}

/// 下载失败：失败 / 已取消的任务 + 清单里已失效的记录。
fn failed_view(root: &Root) -> AnyElement {
    enum FailedRow {
        Task(DownloadTask),
        Missing(LibraryEntry),
    }

    let mut rows: Vec<FailedRow> = root
        .download_tasks()
        .iter()
        .filter(|task| task.state.is_failed())
        .cloned()
        .map(FailedRow::Task)
        .collect();
    rows.extend(root.library_missing.iter().cloned().map(FailedRow::Missing));
    if rows.is_empty() {
        return empty_state("check", root.tr("没有失败的下载"));
    }
    let language = root.language;
    let count = rows.len();

    uniform_list("download-failed-list", count, move |range, _window, _cx| {
        range
            .map(|index| match &rows[index] {
                FailedRow::Task(task) => row_shell(("download-failed", index))
                    .child(title_cell(task.track.title.clone(), true))
                    .child(
                        div()
                            .flex_none()
                            .text_size(theme::Text::Small.size())
                            .text_color(theme::accent())
                            .child(task_detail(task, language)),
                    )
                    .into_any_element(),
                FailedRow::Missing(entry) => row_shell(("download-failed", index))
                    // 被动状态，不用主色：原因 muted、路径 faint。
                    .child(title_cell(entry.display_title(), true))
                    .child(
                        div()
                            .flex_none()
                            .text_size(theme::Text::Small.size())
                            .text_color(theme::text_muted())
                            .child(language.text("文件已不在")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_right()
                            .text_size(theme::Text::Small.size())
                            .text_color(theme::text_faint())
                            .child(entry.relative.to_string_lossy().to_string()),
                    )
                    .into_any_element(),
            })
            .collect::<Vec<_>>()
    })
    .flex_1()
    .min_h(px(0.0))
    .into_any_element()
}

/// 已下载：下载记录（来自清单，跨重启存在）。
fn records_view(root: &Root, cx: &mut Context<Root>) -> AnyElement {
    let records = root.library.clone();
    if records.is_empty() {
        return empty_state("download", root.tr("还没有下载记录"));
    }

    let entity = cx.entity();
    let covers = root.covers.clone();
    let cover_requests = root.cover_requests.clone();
    let count = records.len();
    // 记录能直接还原成 `LocalFile`（相对路径 + 扩展名 + 元数据都在），
    // 这样「已下载」与「本地音乐」两个列表走同一条本地播放路径。
    let files: Arc<Vec<crate::app::LocalFile>> = Arc::new(
        records
            .iter()
            .map(|record| crate::app::LocalFile {
                relative: record.relative.clone(),
                extension: record.extension.clone(),
                bytes: record.bytes,
                entry: Some(record.clone()),
                ..Default::default()
            })
            .collect(),
    );

    let list = uniform_list(
        "download-records-list",
        count,
        move |range, _window, _cx| {
            range
                .map(|index| {
                    let record = &records[index];
                    let duration = record
                        .duration_seconds
                        .filter(|seconds| *seconds > 0)
                        .map(label_from_seconds)
                        .unwrap_or_default();
                    let quality = record.quality.clone().unwrap_or_default();
                    let cover_path =
                        resolve_cover(record.cover.as_deref(), &covers, &cover_requests);

                    // 磁盘上有文件就能播（清单记录都有元数据），每行都可点。
                    let entity = entity.clone();
                    let files = files.clone();
                    let row = row_shell(("download-record", index))
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::surface_hover()))
                        .on_click(move |_event: &ClickEvent, _window, cx: &mut gpui::App| {
                            let files = files.clone();
                            entity.update(cx, |root, cx| {
                                root.play_local_file(files, index, cx);
                            });
                        });

                    row.child(row_cover(cover_path))
                        .child(title_cell(record.display_title(), true))
                        .child(text_cell(COL_ARTIST, record.display_artist()))
                        .child(number_cell(COL_TIME, duration))
                        .child(number_cell(COL_SIZE, human_bytes(record.bytes)))
                        .child(text_cell(COL_QUALITY, quality))
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        },
    )
    .flex_1()
    .min_h(px(0.0));

    list.into_any_element()
}

/// 任务的一行摘要：进行中给百分比，失败给原因，其余给状态词。
fn task_detail(task: &DownloadTask, language: Language) -> String {
    match &task.state {
        DownloadState::Queued => language.text("等待中").to_string(),
        DownloadState::Paused => language.text("已暂停").to_string(),
        DownloadState::Running {
            downloaded, total, ..
        } => match total {
            Some(total) if *total > 0 => {
                let percent = (*downloaded as f64 / *total as f64) * 100.0;
                format!("{percent:.0}%")
            }
            _ => human_bytes(*downloaded),
        },
        DownloadState::Failed { error, .. } => error.clone(),
        DownloadState::Cancelled => language.text("已取消").to_string(),
        DownloadState::Completed { .. } => language.text("已下载").to_string(),
    }
}

// ---------------------------------------------------------------------------
// 扫描本地音乐（占位）
// ---------------------------------------------------------------------------

/// 占位入口：扫描任意本地文件夹需要音频标签解析能力（见 `plan.md` 的 B 阶段）。
fn scan_placeholder(root: &Root) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .items_center()
        .justify_center()
        .gap(px(theme::space::MD))
        .child(empty_state("search", root.tr("扫描本地音乐即将支持")))
        .child(
            div()
                .max_w(px(420.0))
                .text_size(theme::Text::Small.size())
                .text_color(theme::text_faint())
                .child(
                    root.tr("之后可以在这里添加本地文件夹，把你自己的音乐一并纳入「本地音乐」。"),
                ),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 行右键菜单
// ---------------------------------------------------------------------------

/// 本地音乐行的右键菜单：目前只有「匹配信息」。
///
/// 与歌曲行菜单（`views::track_menu`）同构：`deferred` + `anchored` 锚在鼠标位置。
pub(crate) fn local_menu(root: &Root, cx: &mut Context<Root>) -> AnyElement {
    let Some((relative, position)) = root.local_menu.clone() else {
        return div().into_any_element();
    };
    let file = root
        .local_files
        .iter()
        .find(|file| file.relative == relative)
        .cloned();
    let label = file
        .as_ref()
        .map(|file| file.to_track().title)
        .unwrap_or_else(|| {
            relative
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        });
    // 已经有汽水 id 的文件不必再匹配（重复匹配只会浪费接口）。
    let already_matched = file
        .as_ref()
        .and_then(|file| file.entry.as_ref())
        .and_then(|entry| entry.track_id.as_ref())
        .is_some();
    let can_match = !already_matched && !root.settings.cookie.trim().is_empty();

    gpui::deferred(
        gpui::anchored()
            .snap_to_window()
            .position(point(position.x, position.y))
            .child(
                div()
                    .id("local-menu")
                    .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                        cx.stop_propagation();
                    })
                    .flex()
                    .flex_col()
                    .w(px(180.0))
                    .p(px(theme::space::XS))
                    .rounded(px(theme::radius::CARD))
                    .bg(theme::surface_elevated())
                    .border_1()
                    .border_color(theme::border())
                    .shadow_lg()
                    .child(
                        div()
                            .px(px(theme::space::SM))
                            .py(px(theme::space::XS))
                            .truncate()
                            .text_size(theme::Text::Tiny.size())
                            .text_color(theme::text_faint())
                            .child(label),
                    )
                    .child(
                        div()
                            .id("local-match")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(theme::space::SM))
                            .px(px(theme::space::SM))
                            .py(px(theme::space::XS))
                            .rounded(px(theme::radius::ROW))
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::surface_hover()))
                            .text_size(theme::Text::Small.size())
                            .text_color(if can_match {
                                theme::text()
                            } else {
                                theme::text_faint()
                            })
                            .on_click(cx.listener({
                                let relative = relative.clone();
                                move |root, _event: &ClickEvent, _window, cx| {
                                    root.local_menu = None;
                                    if can_match {
                                        root.match_local_file(relative.clone(), cx);
                                    } else {
                                        cx.notify();
                                    }
                                }
                            }))
                            .child(
                                svg()
                                    .path(icons::path("search"))
                                    .size(px(theme::ICON_SM))
                                    .text_color(theme::text_muted()),
                            )
                            .child(root.tr("匹配信息")),
                    )
                    .child(
                        // 直接打开候选列表：自动匹配够不到阈值时，用户能自己挑。
                        div()
                            .id("local-match-pick")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(theme::space::SM))
                            .px(px(theme::space::SM))
                            .py(px(theme::space::XS))
                            .rounded(px(theme::radius::ROW))
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::surface_hover()))
                            .text_size(theme::Text::Small.size())
                            .text_color(if can_match {
                                theme::text()
                            } else {
                                theme::text_faint()
                            })
                            .on_click(cx.listener({
                                let relative = relative.clone();
                                move |root, _event: &ClickEvent, _window, cx| {
                                    root.local_menu = None;
                                    if can_match {
                                        root.open_match_picker(relative.clone(), cx);
                                    } else {
                                        cx.notify();
                                    }
                                }
                            }))
                            .child(
                                svg()
                                    .path(icons::path("list-music"))
                                    .size(px(theme::ICON_SM))
                                    .text_color(theme::text_muted()),
                            )
                            .child(root.tr("手动选择匹配")),
                    ),
            ),
    )
    .with_priority(2)
    .into_any_element()
}

/// 「手动选择匹配」浮层：候选按相似度排名，点击即应用。
///
/// 显示**排名而不是分数** —— 分数（0~100 的内部权重）对用户没有意义，
/// 而且会暴露实现细节。
pub(crate) fn match_picker(root: &Root, cx: &mut Context<Root>) -> AnyElement {
    let Some((relative, candidates)) = root.match_picker.clone() else {
        return div().into_any_element();
    };
    let file_label = root
        .local_files
        .iter()
        .find(|file| file.relative == relative)
        .map(|file| file.to_track().title)
        .unwrap_or_else(|| {
            relative
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        });

    let rows: Vec<AnyElement> = candidates
        .iter()
        .enumerate()
        .map(|(index, outcome)| {
            let track = outcome.track.clone();
            let cover_path = root.cover_of(&track.cover);
            div()
                .id(("match-candidate", index))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(theme::space::MD))
                .h(px(theme::size::LIST_ROW + theme::space::XS))
                .px(px(theme::space::MD))
                .rounded(px(theme::radius::ROW))
                .cursor_pointer()
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(cx.listener({
                    let relative = relative.clone();
                    let track = track.clone();
                    move |root, _event: &ClickEvent, _window, cx| {
                        root.apply_picked_match(relative.clone(), track.clone(), cx);
                    }
                }))
                .child(
                    // 排名徽章：第 1 名更醒目
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(22.0))
                        .text_size(theme::Text::Small.size())
                        .text_color(if index == 0 {
                            theme::accent()
                        } else {
                            theme::text_faint()
                        })
                        .child(format!("{}", index + 1)),
                )
                .child(cover(cover_path, theme::size::ROW_ART, theme::radius::ART))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(theme::Text::Body.size())
                                .text_color(theme::text())
                                .child(track.title.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(theme::Text::Small.size())
                                .text_color(theme::text_muted())
                                .child(track.artist.clone()),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(theme::Text::Small.size())
                        .text_color(theme::text_faint())
                        .child(track.duration_label()),
                )
                .into_any_element()
        })
        .collect();

    gpui::deferred(
        div()
            .id("match-picker-scrim")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x0000_0000_0066))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|root, _event, _window, cx| {
                    if root.match_picker.is_some() {
                        root.match_picker = None;
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .id("match-picker")
                    .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                        cx.stop_propagation();
                    })
                    .flex()
                    .flex_col()
                    .w(px(460.0))
                    .max_h(px(520.0))
                    .p(px(theme::space::LG))
                    .gap(px(theme::space::MD))
                    .rounded(px(theme::radius::CARD))
                    .bg(theme::surface_elevated())
                    .border_1()
                    .border_color(theme::border())
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(theme::space::XS))
                            .child(
                                div()
                                    .text_size(theme::Text::Body.size())
                                    .text_color(theme::text())
                                    .child(root.tr("选择匹配")),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme::Text::Small.size())
                                    .text_color(theme::text_faint())
                                    .child(file_label),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(theme::space::XS))
                            .children(rows),
                    ),
            ),
    )
    .with_priority(3)
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::resolve_cover;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn covers_with(url: &str, path: &str) -> HashMap<String, PathBuf> {
        let mut covers = HashMap::new();
        covers.insert(url.to_string(), PathBuf::from(path));
        covers
    }

    /// 回归：封面必须返回**缓存里的本地文件路径**，不能把 URL 本身当路径返回。
    /// （本地音乐页曾经返回 URL，`cover()` 的 `path.exists()` 永远为假 —— 列表只有占位块。）
    #[test]
    fn cached_cover_returns_the_local_path_not_the_url() {
        let url = "https://img.example/cover.jpg";
        let covers = covers_with(url, "/tmp/cache/cover.jpg");
        let queue = Mutex::new(Vec::new());

        let resolved = resolve_cover(Some(url), &covers, &queue).expect("命中缓存");
        assert_eq!(resolved, PathBuf::from("/tmp/cache/cover.jpg"));
        assert!(queue.lock().expect("lock").is_empty(), "命中缓存不应排队");
    }

    /// 未命中缓存时把 URL 排进请求队列，且不重复排队。
    #[test]
    fn missing_cover_queues_the_url_once() {
        let covers = HashMap::new();
        let queue = Mutex::new(Vec::new());

        assert!(resolve_cover(Some("https://img.example/a.jpg"), &covers, &queue).is_none());
        assert!(resolve_cover(Some("https://img.example/a.jpg"), &covers, &queue).is_none());

        let queued = queue.lock().expect("lock");
        assert_eq!(queued.len(), 1, "同一个 URL 只应排队一次：{queued:?}");
        assert_eq!(queued[0], "https://img.example/a.jpg");
    }

    /// 空 / 缺失 URL：既不排队也不返回路径。
    #[test]
    fn blank_cover_is_ignored() {
        let covers = HashMap::new();
        let queue = Mutex::new(Vec::new());

        assert!(resolve_cover(None, &covers, &queue).is_none());
        assert!(resolve_cover(Some(""), &covers, &queue).is_none());
        assert!(resolve_cover(Some("   "), &covers, &queue).is_none());
        assert!(queue.lock().expect("lock").is_empty());
    }
}
