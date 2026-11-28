//! 导出快捷命令：按分组勾选（默认全选，未分组的算一组），选好保存位置后写成 Termlet 的快捷命令文件（格式见
//! `termlet_core::snippetfile`，默认文件名带日期）。文件是明文，对话框中一行说明提醒。导出后可以在文件夹中显示。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, TestSupportExt as _,
    Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::model::{Kind, Snippet};
use termlet_core::runtime;
use termlet_core::snippetfile;
use termlet_core::store::{Store, StoreEvent};

use super::errors;
use crate::app_state::AppState;
use crate::prose::prose;
use crate::t;
use crate::ui_scale::DialogTitleExt as _;

pub(super) fn open(window: &mut Window, cx: &mut App) -> Entity<SnippetExportDialog> {
    let view = cx.new(SnippetExportDialog::new);
    let rem = window.rem_size();
    let width = rems(30.)
        .to_pixels(rem)
        .min(window.viewport_size().width - rems(4.).to_pixels(rem));
    let dialog_view = view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        // 正在写文件时不能关闭（关掉后不知道导出成了没有）
        let busy = dialog_view.read(cx).busy;
        // Enter 与点“导出”相同；导出完成后关闭
        let on_ok = {
            let view = dialog_view.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                view.update(cx, |view, cx| view.on_enter(window, cx))
            }
        };
        dialog
            .titled(t!("backup_ui.snippet_export.title"))
            .w(width)
            .overlay_closable(false)
            .close_button(!busy)
            .keyboard(!busy)
            .child(dialog_view.clone())
            .on_ok(on_ok)
    });
    view
}

/// 导出完成：文件与条数。
struct Done {
    path: PathBuf,
    count: usize,
}

pub(super) struct SnippetExportDialog {
    store: Arc<Store>,
    /// 分组（空串为未分组）与条数，按快捷命令页的顺序
    groups: Vec<(String, usize)>,
    /// 不导出的分组：默认全选，打开期间新出现的分组也勾选
    excluded: HashSet<String>,
    busy: bool,
    error: Option<SharedString>,
    done: Option<Done>,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SnippetExportDialog {
    fn new(cx: &mut Context<Self>) -> Self {
        // 打开期间快捷命令有变化（别的窗口、云同步）：重新列出
        let store_model = AppState::global(cx).store.clone();
        let subscription = cx.subscribe(&store_model, |this, _, event: &StoreEvent, cx| {
            if matches!(
                event,
                StoreEvent::Changed(Kind::Snippet) | StoreEvent::SettingsChanged
            ) && this.done.is_none()
            {
                this.reload(cx);
            }
        });
        let store = AppState::store(cx);
        let groups = snippetfile::groups(
            &store.list::<Snippet>(),
            &store.settings().snippets.group_order,
        );
        Self {
            store,
            groups,
            excluded: HashSet::new(),
            busy: false,
            error: None,
            done: None,
            _task: None,
            _subscriptions: vec![subscription],
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.groups = snippetfile::groups(
            &self.store.list::<Snippet>(),
            &self.store.settings().snippets.group_order,
        );
        let existing: HashSet<&str> = self.groups.iter().map(|(g, _)| g.as_str()).collect();
        self.excluded.retain(|g| existing.contains(g.as_str()));
        cx.notify();
    }

    /// 勾选的分组。
    fn chosen(&self) -> Vec<String> {
        self.groups
            .iter()
            .filter(|(g, _)| !self.excluded.contains(g))
            .map(|(g, _)| g.clone())
            .collect()
    }

    /// 将导出的条数。
    fn count(&self) -> usize {
        self.groups
            .iter()
            .filter(|(g, _)| !self.excluded.contains(g))
            .map(|(_, n)| n)
            .sum()
    }

    fn total(&self) -> usize {
        self.groups.iter().map(|(_, n)| n).sum()
    }

    fn set(&mut self, group: &str, on: bool, cx: &mut Context<Self>) {
        if on {
            self.excluded.remove(group);
        } else {
            self.excluded.insert(group.to_string());
        }
        cx.notify();
    }

    fn set_all(&mut self, on: bool, cx: &mut Context<Self>) {
        self.excluded = if on {
            HashSet::new()
        } else {
            self.groups.iter().map(|(g, _)| g.clone()).collect()
        };
        cx.notify();
    }

    fn ready(&self) -> bool {
        !self.busy && self.count() > 0
    }

    /// 对话框收到 Enter；返回是否关闭对话框。
    fn on_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.done.is_some() {
            return true;
        }
        self.submit(window, cx);
        false
    }

    /// 选择保存位置后导出。
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ready() {
            return;
        }
        let groups = self.chosen();
        let name = snippetfile::default_file_name();
        let picked = cx.prompt_for_new_path(&super::default_dir(), Some(&name));
        let store = self.store.clone();
        self.busy = true;
        self.error = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = picked.await else {
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    cx.notify();
                });
                return;
            };
            tracing::info!(path = %path.display(), groups = groups.len(), "导出快捷命令");
            let result = runtime::spawn(snippetfile::export(store, path.clone(), groups)).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(Ok(count)) => this.done = Some(Done { path, count }),
                    Ok(Err(e)) => this.error = Some(errors::describe_backup(&e)),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ------------------------------------------------------------ 渲染

    fn render_groups(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (border, radius, muted, hover) = (
            theme.border,
            theme.radius,
            theme.muted_foreground,
            theme.list_hover,
        );
        let total = self.total();
        let ungrouped = t!("snippets.ungrouped");
        let rows = self.groups.iter().map(|(group, count)| {
            let name: SharedString = if group.is_empty() {
                ungrouped.clone()
            } else {
                group.clone().into()
            };
            let key = group.clone();
            Checkbox::new(super::element_id("snippet-export-group", group))
                .w_full()
                .px_2()
                .py_1()
                .checked(!self.excluded.contains(group))
                .disabled(self.busy)
                .accessibility_label(name.clone())
                .hover(move |style| style.bg(hover))
                .on_change(
                    cx.listener(move |this, checked: &bool, _, cx| this.set(&key, *checked, cx)),
                )
                .child(
                    h_flex()
                        .gap_1p5()
                        .text_sm()
                        .child(
                            Icon::new(IconName::Folder)
                                .xsmall()
                                .text_color(muted)
                                .flex_none(),
                        )
                        .child(div().min_w_0().truncate().child(name))
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(count.to_string()),
                        ),
                )
        });
        let body = if self.groups.is_empty() {
            div()
                .p_3()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("backup_ui.snippet_export.none")))
                .into_any_element()
        } else {
            // gpui-kit 的滚动区域：分组多、对话框放不下时滚轮先滚这里
            v_flex()
                .id("snippet-export-groups")
                .max_h(rems(16.))
                .overflow_y_scrollbar()
                .children(rows)
                .into_any_element()
        };
        v_flex()
            .border_1()
            .border_color(border)
            .rounded(radius)
            .overflow_hidden()
            .child(
                div().px_2().py_1().border_b_1().border_color(border).child(
                    Checkbox::new("snippet-export-all")
                        .label(t!("backup_ui.snippet_export.all", count = total))
                        .checked(total > 0 && self.excluded.is_empty())
                        .disabled(total == 0 || self.busy)
                        .on_change(
                            cx.listener(|this, checked: &bool, _, cx| this.set_all(*checked, cx)),
                        ),
                ),
            )
            .child(body)
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        h_flex()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("snippet-export-error")
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.danger)
                    .when_some(self.error.clone(), |this, error| this.child(prose(error))),
            )
            .child(
                Button::new("snippet-export-cancel")
                    .label(t!("common.cancel"))
                    .disabled(self.busy)
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("snippet-export-submit")
                    .primary()
                    .label(t!("backup_ui.snippet_export.submit", count = self.count()))
                    .loading(self.busy)
                    .disabled(!self.ready())
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            )
    }

    fn render_done(&self, done: &Done, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let path = done.path.clone();
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .text_sm()
                    .child(
                        Icon::new(IconName::CircleCheck)
                            .small()
                            .flex_none()
                            .text_color(theme.success),
                    )
                    .child(
                        div()
                            .id("snippet-export-done")
                            .test_support()
                            .min_w_0()
                            .child(t!(
                                "backup_ui.snippet_export.done",
                                count = done.count,
                                path = done.path.display()
                            )),
                    ),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("snippet-export-reveal")
                            .outline()
                            .label(t!("sftp.show_in_folder"))
                            .on_click(move |_, _, cx| cx.reveal_path(&path)),
                    )
                    .child(
                        Button::new("snippet-export-close")
                            .primary()
                            .label(t!("common.close"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    }
}

impl Render for SnippetExportDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = v_flex().id("snippet-export-dialog").test_support().gap_3();
        if let Some(done) = &self.done {
            return root.child(self.render_done(done, cx));
        }
        let muted = cx.theme().muted_foreground;
        root.child(self.render_groups(cx))
            .child(
                div()
                    .id("snippet-export-plaintext")
                    .test_support()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("backup_ui.snippet_export.plaintext_hint"))),
            )
            .child(self.render_footer(cx))
    }
}

#[cfg(test)]
impl SnippetExportDialog {
    /// 勾选的分组（按列出的顺序）。
    pub(super) fn chosen_groups(&self) -> Vec<String> {
        self.chosen()
    }
}
