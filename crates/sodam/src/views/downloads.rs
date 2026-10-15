//! 下载管理面板：右侧抽屉，分「进行中 / 已完成 / 失败」三个分区。
//!
//! 与队列抽屉同构（`uniform_list` 虚拟化 + 顶部工具栏），行高与滚动手感保持一致。
//! 渲染路径只读 `Root::download_cache` 快照，不做任何网络 / 扫盘。

use gpui::prelude::*;
use gpui::{
    div, px, svg, uniform_list, AnyElement, ClickEvent, Context, IntoElement, SharedString,
};

use sodam_core::download::{human_eta, human_speed, DownloadCounts, DownloadState, DownloadTask};

use crate::app::{DownloadTab, Root};
use crate::ui::artwork::cover;
use crate::ui::i18n::Language;
use crate::ui::{icons, theme};

/// 行高（与队列抽屉一致）。
const ROW_H: f32 = theme::size::LIST_ROW;

/// 字节 → 人类可读（与设置页的缓存统计同一套口径）。
pub(crate) fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KB * KB * KB {
        format!("{:.2} GB", value / (KB * KB * KB))
    } else if value >= KB * KB {
        format!("{:.1} MB", value / (KB * KB))
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 行内文案（在渲染闭包外算好，闭包只需要 `Language`，不需要整个 `Root`）。
#[derive(Clone)]
pub(crate) struct RowLabels {
    /// 状态行：百分比 · 速度 · 已下载 / 失败原因 / 完成信息。
    detail: String,
    /// 主操作按钮的图标。
    primary_icon: &'static str,
    /// 主操作按钮的语义。
    primary: PrimaryAction,
    /// 主操作按钮是否要强调色（失败态）。
    primary_accent: bool,
    /// 失败 / 完成的行用不同的文字色。
    failed: bool,
    /// 是否画进度条。
    progress: Option<f32>,
}

/// 行的主操作。
#[derive(Clone)]
enum PrimaryAction {
    Toggle,
    OpenFolder(std::path::PathBuf),
}

/// 把任务状态翻成渲染需要的一组值。
pub(crate) fn row_labels(root: &Root, task: &DownloadTask) -> RowLabels {
    let language = root.language;
    let failed = matches!(
        task.state,
        DownloadState::Failed { .. } | DownloadState::Cancelled
    );
    let done = matches!(task.state, DownloadState::Completed { .. });
    let detail = match &task.state {
        DownloadState::Queued => language.text("等待中").to_string(),
        DownloadState::Paused => language.text("已暂停").to_string(),
        DownloadState::Running {
            downloaded,
            total,
            speed,
        } => {
            let speed_text = human_speed(*speed);
            let mut parts: Vec<String> = Vec::new();
            match total.filter(|total| *total > 0) {
                Some(total) => {
                    let percent = (*downloaded as f64 / total as f64 * 100.0).min(100.0);
                    parts.push(format!("{percent:.0}%"));
                    let eta = human_eta(total.saturating_sub(*downloaded), *speed);
                    if !eta.is_empty() {
                        parts.push(eta);
                    }
                }
                None => parts.push(human_bytes(*downloaded)),
            }
            if !speed_text.is_empty() {
                parts.push(speed_text);
            }
            parts.push(human_bytes(*downloaded));
            parts.join(" · ")
        }
        DownloadState::Completed {
            bytes,
            quality,
            skipped,
            ..
        } => {
            if *skipped {
                root.localized("已存在 · {}", std::slice::from_ref(quality))
            } else {
                format!("{} · {}", human_bytes(*bytes), quality)
            }
        }
        DownloadState::Failed { error, .. } => error.clone(),
        DownloadState::Cancelled => language.text("已取消").to_string(),
    };
    let (primary_icon, primary) = match &task.state {
        DownloadState::Queued | DownloadState::Running { .. } => ("pause", PrimaryAction::Toggle),
        DownloadState::Paused => ("play", PrimaryAction::Toggle),
        DownloadState::Failed { .. } | DownloadState::Cancelled => {
            ("repeat", PrimaryAction::Toggle)
        }
        DownloadState::Completed { path, .. } => {
            let dir = path
                .parent()
                .map(|dir| dir.to_path_buf())
                .unwrap_or_else(|| path.clone());
            ("folder-open", PrimaryAction::OpenFolder(dir))
        }
    };
    RowLabels {
        detail,
        primary_icon,
        primary,
        primary_accent: failed,
        failed,
        progress: if done { None } else { task.state.progress() },
    }
}

/// 任务行。
#[allow(clippy::too_many_arguments)]
pub(crate) fn task_row(
    entity: &gpui::Entity<Root>,
    task: &DownloadTask,
    labels: RowLabels,
    index: usize,
    cover_path: Option<std::path::PathBuf>,
) -> AnyElement {
    let title: SharedString = task.track.title.clone().into();
    let source: SharedString = task.source.clone().into();
    let detail: SharedString = labels.detail.clone().into();
    let detail_color = if labels.failed {
        theme::accent()
    } else if matches!(task.state, DownloadState::Completed { .. }) {
        theme::text_muted()
    } else {
        theme::text_faint()
    };
    let title_color = if labels.failed {
        theme::text_muted()
    } else {
        theme::text()
    };
    let progress = labels.progress;
    let task_id = task.id;
    let primary_task = task.clone();

    div()
        .id(("download-row", index))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .h(px(ROW_H))
        .px(px(theme::space::MD))
        .rounded(px(theme::radius::ROW))
        .hover(|style| style.bg(theme::surface_hover()))
        .child(cover(cover_path, theme::size::ROW_ART, theme::radius::ART))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(2.0))
                .child(
                    div()
                        .truncate()
                        .text_size(theme::Text::Small.size())
                        .text_color(title_color)
                        .child(title),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(theme::Text::Tiny.size())
                        .text_color(detail_color)
                        .child(if source.is_empty() {
                            detail.clone()
                        } else {
                            SharedString::from(format!("{source} · {detail}"))
                        }),
                )
                .when_some(progress, |this, progress| {
                    this.child(
                        div()
                            .mt(px(2.0))
                            .h(px(3.0))
                            .w_full()
                            .rounded(px(2.0))
                            .bg(theme::surface_hover())
                            .child(
                                div()
                                    .h(px(3.0))
                                    .rounded(px(2.0))
                                    .bg(theme::accent())
                                    .w(gpui::relative(progress)),
                            ),
                    )
                }),
        )
        .child({
            // 主操作：进行中 = 暂停，暂停 / 失败 = 继续 / 重试，完成 = 打开文件夹
            let icon = labels.primary_icon;
            let accent = labels.primary_accent;
            let primary = labels.primary.clone();
            let entity = entity.clone();
            div()
                .id(("download-primary", index))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(theme::size::CONTROL_SM))
                .rounded(px(theme::radius::PILL))
                .cursor_pointer()
                .hover(|style| style.bg(theme::surface_selected()))
                .on_click(move |_event: &ClickEvent, _window, cx: &mut gpui::App| {
                    let primary = primary.clone();
                    let task = primary_task.clone();
                    entity.update(cx, |root, cx| match primary {
                        PrimaryAction::Toggle => root.toggle_download_task(&task, cx),
                        PrimaryAction::OpenFolder(dir) => root.open_folder(&dir, cx),
                    });
                })
                .child(
                    svg()
                        .path(icons::path(icon))
                        .size(px(theme::ICON_SM))
                        .text_color(if accent {
                            theme::accent()
                        } else {
                            theme::text_muted()
                        }),
                )
        })
        .child({
            // 「×」= 取消并移除这一行（进行中会立刻中止传输）。
            let entity = entity.clone();
            div()
                .id(("download-remove", index))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(theme::size::CONTROL_SM))
                .rounded(px(theme::radius::PILL))
                .cursor_pointer()
                .hover(|style| style.bg(theme::surface_selected()))
                .on_click(move |_event: &ClickEvent, _window, cx: &mut gpui::App| {
                    entity.update(cx, |root, cx| {
                        root.discard_download_task(task_id, cx);
                    });
                })
                .child(
                    svg()
                        .path(icons::path("x"))
                        .size(px(theme::ICON_SM))
                        .text_color(theme::text_faint()),
                )
        })
        .into_any_element()
}

/// 某个分区标签应显示的数量。
///
/// **必须与 [`DownloadTab::matches`] 的过滤条件一一对应**：标签上的数字和点进去
/// 看到的行数必须同源。曾经这里按 `[active, completed, failed]` 的顺序往数组里填，
/// 而 `DownloadTab::ALL` 的顺序是 `[Active, Failed, Completed]` —— 「下载失败」标签
/// 挂的是已完成数、点进去却是空的（用户实测反馈）。改成按 tab 取值，顺序不再能对错。
fn tab_count(counts: &DownloadCounts, tab: DownloadTab) -> usize {
    match tab {
        DownloadTab::Active => counts.active(),
        DownloadTab::Failed => counts.failed + counts.cancelled,
        DownloadTab::Completed => counts.completed,
    }
}

/// 分区标签（带数量）。
fn tab_button(
    language: Language,
    tab: DownloadTab,
    count: usize,
    selected: bool,
    cx: &mut Context<Root>,
) -> AnyElement {
    div()
        .id(("download-tab", tab as usize))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::XS))
        .px(px(theme::space::MD))
        .py(px(theme::space::XS))
        .rounded(px(theme::radius::PILL))
        .cursor_pointer()
        .when(selected, |this| this.bg(theme::surface_selected()))
        .hover(|style| style.bg(theme::surface_hover()))
        .text_size(theme::Text::Small.size())
        .text_color(if selected {
            theme::text()
        } else {
            theme::text_muted()
        })
        .on_click(cx.listener(move |root, _event: &ClickEvent, _window, cx| {
            root.set_download_tab(tab, cx);
        }))
        .child(tab.label(language).to_string())
        .when(count > 0, |this| {
            this.child(
                div()
                    .text_size(theme::Text::Tiny.size())
                    .text_color(if selected {
                        theme::accent()
                    } else {
                        theme::text_faint()
                    })
                    .child(count.to_string()),
            )
        })
        .into_any_element()
}

/// 空态：一句话 + 图标，不留空白面板。
fn empty_state(text: &str) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(theme::space::MD))
        .flex_1()
        .min_h(px(0.0))
        .py(px(theme::space::XXL))
        .child(
            svg()
                .path(icons::path("download"))
                .size(px(theme::ICON_XL))
                .text_color(theme::text_faint()),
        )
        .child(
            div()
                .text_size(theme::Text::Small.size())
                .text_color(theme::text_muted())
                .child(text.to_string()),
        )
        .into_any_element()
}

/// 底部操作按钮。
fn footer_button(id: &'static str, label: String) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(theme::size::CONTROL_SM))
        .px(px(theme::space::MD))
        .rounded(px(theme::radius::ROW))
        .cursor_pointer()
        .hover(|style| style.bg(theme::surface_hover()))
        .text_size(theme::Text::Tiny.size())
        .text_color(theme::text_muted())
        .child(label)
}

/// 下载面板的设计宽度（下限）。叠加时实际宽度由调用方按视口算好传入
/// （`app.rs::overlay_panel_width`）—— 叠加层不能用样式百分比：绝对定位层
/// 宽度是 auto，`relative(..)` 会按元素自身宽度解析（360px × 0.4 = 144px）。
pub(crate) const PANEL_W: f32 = 360.0;

/// 渲染下载面板（主界面右侧抽屉）；`width` 由调用方按视口算成确定像素。
pub fn panel(root: &Root, width: f32, cx: &mut Context<Root>) -> impl IntoElement {
    let language = root.language;
    let counts = root.download_counts;
    let tab = root.download_tab;
    let tasks = std::sync::Arc::new(root.download_tab_tasks(tab));
    // 文案在渲染前一次算好：渲染闭包里不能再借用 `&Root`（否则要重入实体更新）。
    let labels = std::sync::Arc::new(
        tasks
            .iter()
            .map(|task| row_labels(root, task))
            .collect::<Vec<_>>(),
    );
    let covers = root.covers.clone();
    let cover_requests = root.cover_requests.clone();
    let entity = cx.entity();
    let count = tasks.len();
    let total = counts.total();
    // 「有没有还在跑的」决定底栏是「全部暂停」还是「全部继续」：
    // 全部暂停后 active 仍然 > 0（那些行是 paused），只看它按钮就永远停在「全部暂停」。
    let pending = counts.pending();

    let tabs: Vec<AnyElement> = DownloadTab::ALL
        .iter()
        .map(|tab| {
            tab_button(
                language,
                *tab,
                tab_count(&counts, *tab),
                *tab == root.download_tab,
                cx,
            )
        })
        .collect();

    let entity_for_list = entity.clone();
    let list = uniform_list("download-panel-list", count, move |range, _window, _cx| {
        range
            .map(|index| {
                let Some(task) = tasks.get(index) else {
                    return div().h(px(ROW_H)).into_any_element();
                };
                let Some(row) = labels.get(index) else {
                    return div().h(px(ROW_H)).into_any_element();
                };
                let cover_path = covers.get(&task.track.cover).cloned();
                // 只有渲染出来的行才排队取封面（虚拟列表只渲染可见行）
                if cover_path.is_none() && !task.track.cover.trim().is_empty() {
                    if let Ok(mut queue) = cover_requests.lock() {
                        if !queue.iter().any(|item| item == &task.track.cover) {
                            queue.push(task.track.cover.clone());
                        }
                    }
                }
                task_row(&entity_for_list, task, row.clone(), index, cover_path)
            })
            .collect::<Vec<_>>()
    })
    .flex_1()
    .min_h(px(0.0));
    div()
        .relative()
        .flex()
        .flex_col()
        .h_full()
        // 叠加在内容之上：宽度由调用方按视口算成确定像素，加阴影与边框把层次分开。
        .w(px(width))
        .flex_none()
        .bg(theme::bg())
        .border_l_1()
        .border_color(theme::border())
        .shadow_lg()
        .child(
            // 工具栏：标题 + 关闭
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(theme::space::SM))
                .h(px(theme::TOOLBAR_H))
                .flex_none()
                .px(px(theme::space::LG))
                .border_b_1()
                .border_color(theme::border())
                .child(
                    div()
                        .text_size(theme::Text::Tiny.size())
                        .text_color(theme::text_faint())
                        .child(language.textf("下载管理 · {} 项", &[total.to_string()])),
                )
                .child(
                    crate::ui::artwork::icon_button(
                        "downloads-close",
                        "x",
                        theme::ICON_SM,
                        theme::text_muted(),
                    )
                    .on_click(cx.listener(
                        |root, _event: &ClickEvent, _window, cx| {
                            root.download_open = false;
                            cx.notify();
                        },
                    )),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(theme::space::XS))
                .flex_none()
                .px(px(theme::space::MD))
                .py(px(theme::space::SM))
                .children(tabs),
        )
        .child(if count == 0 {
            empty_state(root.tr("下载队列为空")).into_any_element()
        } else {
            list.into_any_element()
        })
        .child({
            // 底部操作条：全部暂停 / 全部继续 / 清空已完成 / 打开目录
            let finished = counts.completed + counts.failed + counts.cancelled;
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(theme::space::XS))
                .flex_none()
                .px(px(theme::space::MD))
                .py(px(theme::space::SM))
                .border_t_1()
                .border_color(theme::border())
                .child({
                    let label = if pending > 0 {
                        root.tr("全部暂停")
                    } else {
                        root.tr("全部继续")
                    };
                    let id = if pending > 0 {
                        "downloads-pause-all"
                    } else {
                        "downloads-resume-all"
                    };
                    footer_button(id, label.to_string()).on_click(cx.listener(
                        |root, _event: &ClickEvent, _window, cx| {
                            if root.download_counts.pending() > 0 {
                                root.pause_all_downloads(cx);
                            } else {
                                root.resume_all_downloads(cx);
                            }
                        },
                    ))
                })
                .child(
                    footer_button("downloads-clear", root.tr("清空已完成").to_string())
                        .when(finished == 0, |this| this.opacity(0.4))
                        .on_click(cx.listener(|root, _event: &ClickEvent, _window, cx| {
                            root.clear_finished_downloads(cx);
                        })),
                )
                .child(
                    footer_button("downloads-open-dir", root.tr("打开文件夹").to_string())
                        .on_click(cx.listener(|root, _event: &ClickEvent, _window, cx| {
                            root.open_download_dir(cx);
                        })),
                )
        })
}

#[cfg(test)]
mod tests {
    use super::tab_count;
    use crate::app::DownloadTab;
    use sodam_core::download::DownloadCounts;

    /// 回归：分区标签的数量必须与该分区列表的过滤结果同源。
    /// 曾经标签按 `[active, completed, failed]` 填数组，而 `DownloadTab::ALL` 的顺序是
    /// `[Active, Failed, Completed]` —— 「下载失败」挂的是已完成数，点进去却是空的。
    #[test]
    fn tab_counts_match_each_partition() {
        let counts = DownloadCounts {
            queued: 1,
            running: 2,
            paused: 3,
            completed: 10,
            failed: 4,
            cancelled: 5,
        };
        assert_eq!(tab_count(&counts, DownloadTab::Active), 6); // queued+running+paused
        assert_eq!(tab_count(&counts, DownloadTab::Failed), 9); // failed+cancelled
        assert_eq!(tab_count(&counts, DownloadTab::Completed), 10);
    }
}
