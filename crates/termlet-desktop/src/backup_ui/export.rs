//! 导出主机：按分组或主机勾选（打开时勾选调用方给的主机，没给时全部），选格式（Termlet 主机文件或 ssh_config）、
//! 是否包含端口转发、是否包含凭据（含凭据时设置两次一致的密码，整个文件用它加密），看过将导出的数量（含一并导出的
//! 跳板机）后选择保存位置。导出后说明没有导出的 Telnet、串口主机（ssh_config 只能描述 SSH 主机）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _,
    VirtualListScrollHandle, WindowExt as _, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task,
    TestSupportExt as _, Window, div, prelude::FluentBuilder as _, px, rems, size,
};
use termlet_core::backup::BackupError;
use termlet_core::hostfile::{self, ExportFormat, ExportRequest, ExportSummary};
use termlet_core::id::{GroupId, HostId};
use termlet_core::model::{Group, Host};
use termlet_core::runtime;
use termlet_core::secret::SecretString;
use termlet_core::store::Store;

use super::{PasswordCheck, PasswordPair, errors};
use crate::app_state::AppState;
use crate::prose::{ProseLabel as _, prose};
use crate::sidebar::rows::{TreeRow, group_members, tree_rows};
use crate::t;
use crate::ui_scale::DialogTitleExt as _;

/// 主机树中一行的高度（rem）。
const ROW_HEIGHT: f32 = 1.75;

pub(super) fn open(hosts: Vec<HostId>, window: &mut Window, cx: &mut App) -> Entity<ExportDialog> {
    let view = cx.new(|cx| ExportDialog::new(hosts, window, cx));
    let rem = window.rem_size();
    let width = rems(36.)
        .to_pixels(rem)
        .min(window.viewport_size().width - rems(4.).to_pixels(rem));
    let dialog_view = view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        // 正在写文件时不能关闭（关掉后不知道导出成了没有）
        let busy = dialog_view.read(cx).busy;
        // 在输入框（导出密码）中按 Enter 与点“导出”相同；导出完成后关闭
        let on_ok = {
            let view = dialog_view.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                view.update(cx, |view, cx| view.on_enter(window, cx))
            }
        };
        dialog
            .titled(t!("backup_ui.export.title"))
            .w(width)
            .overlay_closable(false)
            .close_button(!busy)
            .keyboard(!busy)
            .child(dialog_view.clone())
            .on_ok(on_ok)
    });
    view
}

/// 导出完成后的说明。
struct Done {
    path: PathBuf,
    summary: ExportSummary,
    encrypted: bool,
}

pub(super) struct ExportDialog {
    store: Arc<Store>,
    format: ExportFormat,
    rows: Vec<TreeRow>,
    members: HashMap<GroupId, Vec<HostId>>,
    /// 全部主机（按树中的顺序）
    order: Vec<HostId>,
    selected: HashSet<HostId>,
    forwards: bool,
    secrets: bool,
    password: PasswordPair,
    /// 将导出的数量；没有勾选时为 `None`
    summary: Option<Result<ExportSummary, BackupError>>,
    busy: bool,
    error: Option<SharedString>,
    done: Option<Done>,
    scroll: VirtualListScrollHandle,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ExportDialog {
    pub(super) fn new(hosts: Vec<HostId>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (password, mut subscriptions) = PasswordPair::new(window, cx);
        // 打开期间主机、分组有变化（别的窗口、云同步）：重新列出
        let store_model = AppState::global(cx).store.clone();
        subscriptions.push(cx.observe(&store_model, |this, _, cx| this.reload(cx)));
        let mut this = Self {
            store: AppState::store(cx),
            format: ExportFormat::Termlet,
            rows: Vec::new(),
            members: HashMap::new(),
            order: Vec::new(),
            selected: HashSet::new(),
            forwards: true,
            secrets: false,
            password,
            summary: None,
            busy: false,
            error: None,
            done: None,
            scroll: VirtualListScrollHandle::new(),
            _task: None,
            _subscriptions: subscriptions,
        };
        this.load_rows();
        this.selected = if hosts.is_empty() {
            this.order.iter().cloned().collect()
        } else {
            let wanted: HashSet<HostId> = hosts.into_iter().collect();
            this.order
                .iter()
                .filter(|id| wanted.contains(*id))
                .cloned()
                .collect()
        };
        this.refresh_summary();
        this
    }

    fn load_rows(&mut self) {
        let groups = self.store.list::<Group>();
        let hosts = self.store.list::<Host>();
        self.rows = tree_rows(&groups, &hosts, "");
        self.members = group_members(&self.rows);
        self.order = self
            .rows
            .iter()
            .filter_map(|row| match row {
                TreeRow::Host { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.done.is_some() {
            return;
        }
        self.load_rows();
        let existing: HashSet<&HostId> = self.order.iter().collect();
        self.selected.retain(|id| existing.contains(id));
        self.refresh_summary();
        cx.notify();
    }

    /// 勾选的主机（按树中的顺序）。
    fn chosen(&self) -> Vec<HostId> {
        self.order
            .iter()
            .filter(|id| self.selected.contains(*id))
            .cloned()
            .collect()
    }

    fn request(&self, password: Option<SecretString>) -> ExportRequest {
        ExportRequest {
            hosts: self.chosen(),
            format: self.format,
            forwards: self.forwards,
            password,
        }
    }

    /// 重新计算将导出的数量（只读内存）。
    fn refresh_summary(&mut self) {
        self.summary = (!self.selected.is_empty())
            .then(|| hostfile::summarize(&self.store, &self.request(None)));
    }

    fn set_many(&mut self, ids: &[HostId], on: bool, cx: &mut Context<Self>) {
        for id in ids {
            if on {
                self.selected.insert(id.clone());
            } else {
                self.selected.remove(id);
            }
        }
        self.refresh_summary();
        cx.notify();
    }

    fn with_secrets(&self) -> bool {
        self.format == ExportFormat::Termlet && self.secrets
    }

    fn ready(&self, cx: &App) -> bool {
        !self.busy
            && !self.selected.is_empty()
            && !matches!(self.summary, Some(Err(_)))
            && (!self.with_secrets() || self.password.check(cx) == PasswordCheck::Ready)
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
        if !self.ready(cx) {
            return;
        }
        let password = if self.with_secrets() {
            let Some(password) = self.password.secret(cx) else {
                return;
            };
            Some(password)
        } else {
            None
        };
        let encrypted = password.is_some();
        let request = self.request(password);
        let name = match self.format {
            ExportFormat::Termlet => {
                format!("termlet-hosts-{}.termlet-hosts", super::file_stamp())
            }
            ExportFormat::SshConfig => "config".to_string(),
        };
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
            tracing::info!(path = %path.display(), "导出主机");
            let job = runtime::spawn(hostfile::export(store, path.clone(), request));
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(Ok(summary)) => {
                        this.done = Some(Done {
                            path,
                            summary,
                            encrypted,
                        })
                    }
                    Ok(Err(e)) => this.error = Some(errors::describe_backup(&e)),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ------------------------------------------------------------ 渲染

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, hover) = (theme.muted_foreground, theme.list_hover);
        let indent = |depth: usize| rems(0.5 + depth as f32);
        match &self.rows[ix] {
            TreeRow::Group { id, name, depth } => {
                let ids = self.members.get(id).cloned().unwrap_or_default();
                let picked = ids.iter().filter(|h| self.selected.contains(*h)).count();
                let total = ids.len();
                Checkbox::new(super::element_id("export-group", id.as_str()))
                    .size_full()
                    .pl(indent(*depth))
                    .pr_2()
                    .py_1()
                    .checked(total > 0 && picked == total)
                    .disabled(total == 0)
                    .accessibility_label(name.clone())
                    .hover(move |style| style.bg(hover))
                    .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_many(&ids, *checked, cx)
                    }))
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
                            .child(div().min_w_0().truncate().font_medium().child(name.clone()))
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(total.to_string()),
                            ),
                    )
                    .into_any_element()
            }
            TreeRow::Host {
                id,
                name,
                address,
                depth,
                ..
            } => {
                let host = id.clone();
                Checkbox::new(super::element_id("export-host", id.as_str()))
                    .size_full()
                    .pl(indent(*depth))
                    .pr_2()
                    .py_1()
                    .checked(self.selected.contains(id))
                    .accessibility_label(name.clone())
                    .hover(move |style| style.bg(hover))
                    .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_many(std::slice::from_ref(&host), *checked, cx)
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(div().min_w_0().truncate().child(name.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(address.clone()),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_tree(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (border, radius, muted) = (theme.border, theme.radius, theme.muted_foreground);
        let total = self.order.len();
        let all_on = total > 0 && self.selected.len() == total;
        let body = if self.rows.is_empty() {
            div()
                .p_3()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("backup_ui.export.no_hosts")))
                .into_any_element()
        } else {
            let row_height = rems(ROW_HEIGHT).to_pixels(window.rem_size());
            let sizes = Rc::new(vec![size(px(0.), row_height); self.rows.len()]);
            v_virtual_list(cx.entity(), "export-rows", sizes, |this, range, _, cx| {
                range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
            })
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        };
        let all = self.order.clone();
        v_flex()
            .flex_1()
            .min_h_0()
            .border_1()
            .border_color(border)
            .rounded(radius)
            .overflow_hidden()
            .child(
                div().px_2().py_1().border_b_1().border_color(border).child(
                    Checkbox::new("export-select-all")
                        .label(t!("backup_ui.export.all", count = total))
                        .checked(all_on)
                        .disabled(total == 0)
                        .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                            this.set_many(&all, *checked, cx)
                        })),
                ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    .vertical_scrollbar(&self.scroll),
            )
    }

    fn render_options(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        let termlet = self.format == ExportFormat::Termlet;
        v_flex()
            .gap_2()
            .child(
                Checkbox::new("export-forwards")
                    .label(t!("backup_ui.export.forwards"))
                    .checked(self.forwards)
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.forwards = *checked;
                        this.refresh_summary();
                        cx.notify();
                    })),
            )
            .when(termlet, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Checkbox::new("export-secrets")
                                .prose_label(t!("backup_ui.export.secrets"))
                                .checked(self.secrets)
                                .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                    this.secrets = *checked;
                                    cx.notify();
                                })),
                        )
                        .child(div().pl_6().text_xs().text_color(muted).child(prose(
                            if self.secrets {
                                t!("backup_ui.export.secrets_on")
                            } else {
                                t!("backup_ui.export.secrets_off")
                            },
                        ))),
                )
                .when(self.secrets, |this| {
                    this.child(div().pl_6().child(self.password.render("export", cx)))
                })
            })
    }

    fn render_summary(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let mut lines: Vec<(SharedString, bool)> = Vec::new();
        match &self.summary {
            None => lines.push((t!("backup_ui.error.nothing_selected"), false)),
            Some(Err(e)) => lines.push((errors::describe_backup(e), true)),
            Some(Ok(s)) => {
                lines.push((
                    match self.format {
                        ExportFormat::Termlet => t!(
                            "backup_ui.export.summary.termlet",
                            hosts = s.hosts,
                            groups = s.groups,
                            profiles = s.profiles,
                            keys = s.keys,
                            forwards = s.forwards
                        ),
                        ExportFormat::SshConfig => t!(
                            "backup_ui.export.summary.ssh_config",
                            hosts = s.hosts,
                            forwards = s.forwards
                        ),
                    },
                    false,
                ));
                if s.added > 0 {
                    lines.push((t!("backup_ui.export.summary.added", count = s.added), false));
                }
                if s.skipped > 0 {
                    lines.push((
                        t!("backup_ui.export.summary.skipped", count = s.skipped),
                        false,
                    ));
                }
            }
        }
        v_flex()
            .id("export-summary")
            .test_support()
            .gap_0p5()
            .text_xs()
            .children(lines.into_iter().map(|(text, error)| {
                div()
                    .text_color(if error {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .child(prose(text))
            }))
    }

    fn render_done(&self, done: &Done, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let summary = &done.summary;
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
                    .child(div().id("export-done").test_support().min_w_0().child(t!(
                        "backup_ui.export.done",
                        count = summary.hosts,
                        path = done.path.display()
                    ))),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when(summary.added > 0, |this| {
                        this.child(prose(t!(
                            "backup_ui.export.done_added",
                            count = summary.added
                        )))
                    })
                    .when(summary.skipped > 0, |this| {
                        this.child(prose(t!(
                            "backup_ui.export.done_skipped",
                            count = summary.skipped
                        )))
                    })
                    .when(done.encrypted, |this| {
                        this.child(prose(t!("backup_ui.export.done_encrypted")))
                    }),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("export-reveal")
                            .outline()
                            .label(t!("sftp.show_in_folder"))
                            .on_click(move |_, _, cx| cx.reveal_path(&path)),
                    )
                    .child(
                        Button::new("export-close")
                            .primary()
                            .label(t!("common.close"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let count = self.selected.len();
        h_flex()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("export-error")
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.danger)
                    .when_some(self.error.clone(), |this, error| this.child(prose(error))),
            )
            .child(
                Button::new("export-cancel")
                    .label(t!("common.cancel"))
                    .disabled(self.busy)
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("export-submit")
                    .primary()
                    .label(t!("backup_ui.export.submit", count = count))
                    .loading(self.busy)
                    .disabled(!self.ready(cx))
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            )
    }
}

impl Render for ExportDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(done) = &self.done {
            return v_flex()
                .id("export-dialog")
                .child(self.render_done(done, cx))
                .into_any_element();
        }
        let rem = window.rem_size();
        // 高 36rem，窗口矮时缩到窗口内，主机树在里面滚动
        let height = rems(36.)
            .to_pixels(rem)
            .min(window.viewport_size().height - rems(8.).to_pixels(rem));
        const FORMATS: [ExportFormat; 2] = [ExportFormat::Termlet, ExportFormat::SshConfig];
        let muted = cx.theme().muted_foreground;
        let hint = match self.format {
            ExportFormat::Termlet => t!("backup_ui.export.hint.termlet"),
            ExportFormat::SshConfig => t!("backup_ui.export.hint.ssh_config"),
        };
        v_flex()
            .id("export-dialog")
            .h(height)
            .gap_3()
            .child(div().text_xs().text_color(muted).child(prose(hint)))
            .child(
                RadioGroup::horizontal("export-format")
                    .children([
                        t!("backup_ui.export.format.termlet"),
                        t!("backup_ui.export.format.ssh_config"),
                    ])
                    .selected_index(FORMATS.iter().position(|f| *f == self.format))
                    .disabled(self.busy)
                    .on_change(cx.listener(|this, ix: &usize, _, cx| {
                        if let Some(format) = FORMATS.get(*ix) {
                            this.format = *format;
                            this.refresh_summary();
                            cx.notify();
                        }
                    })),
            )
            .child(self.render_tree(window, cx))
            .child(self.render_options(cx))
            .child(self.render_summary(cx))
            .child(self.render_footer(cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use termlet_core::db::Database;
    use termlet_core::store::Position;
    use termlet_core::vault::{MemoryKeyStore, Vault};

    use super::*;

    fn memory_store() -> Store {
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::open(&db, Box::new(MemoryKeyStore::default())).unwrap();
        Store::open(db, vault).unwrap()
    }

    fn host(name: &str, favorite: bool) -> Host {
        Host {
            name: name.into(),
            address: format!("{name}.example"),
            username: "root".into(),
            favorite,
            ..Default::default()
        }
    }

    fn group(name: &str) -> Group {
        Group {
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn tree_lists_hosts_once_and_groups_own_nested_hosts() {
        let store = memory_store();
        let prod = store.create(group("生产"), None, Position::Last).unwrap();
        let web = store
            .create(group("web"), Some(prod.as_str().into()), Position::Last)
            .unwrap();
        let a = store
            .create(host("a", false), Some(prod.as_str().into()), Position::Last)
            .unwrap();
        let b = store
            .create(host("b", true), Some(web.as_str().into()), Position::Last)
            .unwrap();
        let c = store
            .create(host("c", false), None, Position::Last)
            .unwrap();
        store.create(group("空"), None, Position::Last).unwrap();

        let rows = tree_rows(&store.list::<Group>(), &store.list::<Host>(), "");
        let hosts: Vec<&HostId> = rows
            .iter()
            .filter_map(|r| match r {
                TreeRow::Host { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        // 收藏的 b 只在它的分组中列出一次；子分组在本组的主机之前
        assert_eq!(hosts, [&b, &a, &c]);
        match rows.last() {
            Some(TreeRow::Host { id, depth, .. }) => {
                assert_eq!((id, *depth), (&c, 0), "不在分组中的主机在最外层、分组之后")
            }
            other => panic!("最后一行应是不在分组中的主机：{other:?}"),
        }
        let members = group_members(&rows);
        assert_eq!(members[&prod], [b.clone(), a.clone()]);
        assert_eq!(members[&web], std::slice::from_ref(&b));
        assert_eq!(members.values().filter(|m| m.is_empty()).count(), 1);
        match &rows[0] {
            TreeRow::Group { name, depth, .. } => assert_eq!((name.as_str(), *depth), ("生产", 0)),
            other => panic!("第一行应是分组：{other:?}"),
        }
    }
}
