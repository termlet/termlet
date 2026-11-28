//! 部署公钥到服务器：勾选主机（可搜索），用各主机保存的凭据登录、把公钥追加到 `~/.ssh/authorized_keys`
//! （已有时不重复添加），再只用这把密钥验证一次登录；可以选择验证通过后改用这把密钥登录这些主机。
//! 逐台进行，每台显示结果。需要确认指纹、输入密码时在这个窗口中询问。

use std::collections::{BTreeSet, HashMap};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, rems,
};
use termlet_core::id::{HostId, KeyId};
use termlet_core::keyvault::{self, DeployOutcome, DeployReport, DeployRequest, KeyVaultError};
use termlet_core::model::{Host, KeyEntry, Protocol};
use termlet_core::ssh::{Prompter, SshError};
use termlet_core::store::Record;

use super::errors;
use crate::app_state::AppState;
use crate::prose::{ProseLabel as _, prose};
use crate::ui_scale::DialogTitleExt as _;
use crate::{ssh_errors, t};

pub fn open(id: KeyId, window: &mut Window, cx: &mut App) {
    let Some(record) = AppState::store(cx).get::<KeyEntry>(&id) else {
        return;
    };
    let title = t!("keys.deploy_title", name = record.doc.name.clone());
    let prompter = AppState::prompter_for(window, cx);
    let form = cx.new(|cx| DeployForm::new(id, prompter, window, cx));
    let width = rems(34.).to_pixels(window.rem_size());
    let first = std::rc::Rc::new(std::cell::Cell::new(true));
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            form.read(cx).filter.focus_handle(cx).focus(window, cx);
        }
        dialog
            .titled(title.clone())
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
    });
}

/// 一台主机的部署状态。
#[derive(Clone)]
enum Progress {
    Waiting,
    Running,
    Done(Result<DeployReport, KeyVaultError>),
}

struct DeployForm {
    key: KeyId,
    prompter: Prompter,
    hosts: Vec<Record<Host>>,
    filter: Entity<InputState>,
    selected: BTreeSet<HostId>,
    use_key: bool,
    progress: HashMap<HostId, Progress>,
    running: bool,
    _task: Option<Task<()>>,
    _subscription: Subscription,
}

impl DeployForm {
    fn new(key: KeyId, prompter: Prompter, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let hosts = AppState::store(cx)
            .list::<Host>()
            .into_iter()
            .filter(|h| h.doc.protocol == Protocol::Ssh)
            .collect();
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(t!("keys.deploy_filter")));
        let subscription = cx.subscribe(&filter, |_: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            key,
            prompter,
            hosts,
            filter,
            selected: BTreeSet::new(),
            use_key: false,
            progress: HashMap::new(),
            running: false,
            _task: None,
            _subscription: subscription,
        }
    }

    fn visible(&self, cx: &App) -> Vec<Record<Host>> {
        let query = self.filter.read(cx).value().trim().to_lowercase();
        self.hosts
            .iter()
            .filter(|h| {
                query.is_empty()
                    || [&h.doc.name, &h.doc.address, &h.doc.username]
                        .iter()
                        .any(|f| f.to_lowercase().contains(&query))
                    || h.doc.tags.iter().any(|t| t.to_lowercase().contains(&query))
            })
            .cloned()
            .collect()
    }

    /// 按列表的顺序逐台部署。
    fn deploy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running || self.selected.is_empty() {
            return;
        }
        let targets: Vec<HostId> = self
            .hosts
            .iter()
            .filter(|h| self.selected.contains(&h.id))
            .map(|h| h.id.clone())
            .collect();
        self.progress = targets
            .iter()
            .map(|id| (id.clone(), Progress::Waiting))
            .collect();
        self.running = true;
        let (store, key, use_key, prompter) = (
            AppState::store(cx),
            self.key.clone(),
            self.use_key,
            self.prompter.clone(),
        );
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            for host in targets {
                if this
                    .update(cx, |this, cx| {
                        this.progress.insert(host.clone(), Progress::Running);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                let request = DeployRequest {
                    key: key.clone(),
                    host: host.clone(),
                    use_key,
                    prompter: prompter.clone(),
                };
                tracing::info!(key = %key, host = %host, use_key, "部署公钥");
                let job = termlet_core::runtime::spawn(keyvault::deploy(store.clone(), request));
                let result = job
                    .await
                    .unwrap_or_else(|e| Err(KeyVaultError::Store(e.to_string())));
                match &result {
                    Ok(report) => tracing::info!(
                        key = %key,
                        host = %host,
                        outcome = ?report.outcome,
                        verified = report.verify_error.is_none(),
                        "部署公钥完成"
                    ),
                    Err(e) => tracing::warn!(key = %key, host = %host, "部署公钥失败：{e}"),
                }
                if this
                    .update(cx, |this, cx| {
                        this.progress.insert(host.clone(), Progress::Done(result));
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.running = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 一台主机的结果：颜色（成功 / 部分 / 失败）与说明。
    fn describe(&self, progress: &Progress, cx: &App) -> (gpui_kit::Hsla, SharedString) {
        let theme = cx.theme();
        match progress {
            Progress::Waiting => (theme.muted_foreground, t!("keys.deploy_waiting")),
            Progress::Running => (theme.muted_foreground, t!("keys.deploy_running")),
            Progress::Done(Err(error)) => {
                let mut text =
                    t!("keys.deploy_failed", reason = errors::describe(error)).to_string();
                if matches!(
                    error,
                    KeyVaultError::Connect(SshError::HostKeyRejected { .. })
                ) {
                    text.push('\n');
                    text.push_str(&t!("keys.deploy_hostkey_hint"));
                }
                (theme.danger, text.into())
            }
            Progress::Done(Ok(report)) => {
                let separator = " · ";
                let mut parts: Vec<SharedString> = vec![match report.outcome {
                    DeployOutcome::Added => t!("keys.deploy_added"),
                    DeployOutcome::AlreadyPresent => t!("keys.deploy_exists"),
                }];
                match &report.verify_error {
                    None => parts.push(t!("keys.deploy_verified")),
                    Some(error) => parts.push(t!(
                        "keys.deploy_not_verified",
                        reason = ssh_errors::describe(error)
                    )),
                }
                if report.switched {
                    parts.push(t!("keys.deploy_switched"));
                } else if self.use_key {
                    parts.push(t!("keys.deploy_not_switched"));
                }
                let color = if report.verified() {
                    theme.success
                } else {
                    theme.warning
                };
                (color, parts.join(separator).into())
            }
        }
    }
}

impl Render for DeployForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let visible = self.visible(cx);
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let radius = cx.theme().radius;
        let running = self.running;
        // 主机列表最高 16rem，窗口矮时缩短，整个对话框留在窗口内（其余部分与对话框离窗口上下边的距离约 18rem）
        let rem = window.rem_size();
        let list_max = rems(16.)
            .to_pixels(rem)
            .min(window.viewport_size().height - rems(18.).to_pixels(rem))
            .max(rems(4.).to_pixels(rem));
        let mut rows = Vec::with_capacity(visible.len());
        for host in visible {
            let id = host.id.clone();
            let checked = self.selected.contains(&id);
            let address = if host.doc.username.is_empty() {
                format!("{}:{}", host.doc.address, host.doc.port)
            } else {
                format!(
                    "{}@{}:{}",
                    host.doc.username, host.doc.address, host.doc.port
                )
            };
            let status = self.progress.get(&id).map(|p| self.describe(p, cx));
            let toggle = id.clone();
            rows.push(
                v_flex()
                    .id(SharedString::from(format!("deploy-host-{id}")))
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .child(
                        Checkbox::new(SharedString::from(format!("deploy-check-{id}")))
                            .checked(checked)
                            .disabled(running)
                            .label(host.doc.name.clone())
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                if *checked {
                                    this.selected.insert(toggle.clone());
                                } else {
                                    this.selected.remove(&toggle);
                                }
                                cx.notify();
                            })),
                    )
                    .child(div().pl_6().text_xs().text_color(muted).child(address))
                    .when_some(status, |this, (color, text)| {
                        this.child(div().pl_6().text_xs().text_color(color).child(prose(text)))
                    }),
            );
        }
        let count = self.selected.len();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(prose(t!("keys.deploy_hint"))),
            )
            .child(Input::new(&self.filter).id("deploy-filter"))
            .child(
                v_flex()
                    .border_1()
                    .border_color(border)
                    .rounded(radius)
                    .overflow_hidden()
                    .child(
                        v_flex()
                            .max_h(list_max)
                            .overflow_y_scrollbar()
                            .id("deploy-hosts")
                            .when(rows.is_empty(), |this| {
                                this.child(
                                    div()
                                        .p_2()
                                        .text_sm()
                                        .text_color(muted)
                                        .child(t!("keys.deploy_no_hosts")),
                                )
                            })
                            .children(rows),
                    ),
            )
            .child(
                Checkbox::new("deploy-use-key")
                    .prose_label(t!("keys.deploy_use_key"))
                    .checked(self.use_key)
                    .disabled(running)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.use_key = *checked;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("deploy-close")
                            .ghost()
                            .label(t!("common.close"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("deploy-submit")
                            .primary()
                            .label(if running {
                                t!("keys.deploy_running")
                            } else {
                                t!("keys.deploy_submit", count = count)
                            })
                            .disabled(running || count == 0)
                            .on_click(cx.listener(|this, _, window, cx| this.deploy(window, cx))),
                    ),
            )
    }
}
