//! Docker 页的清理（工具条上的“清理…”）：对话框先只读地列出各类可以清理的内容与可释放的空间（已停止的容器、未使用的
//! 网络、未使用的卷、悬空镜像、构建缓存，见核心的 `hosttools::prune`），各自可以勾选、展开看名称；卷默认不勾，并醒目
//! 提示会永久删除其中的数据。勾选随时现算：先清理已停止的容器时，只被它们使用的网络、卷与镜像也算在内。
//!
//! 对话框就是确认框：写明主机、将执行的命令（经 sudo 与否与 Docker 页一致）与合计可释放的空间。确认后按
//! `docker system prune` 的先后逐类执行 `docker container prune -f` 等，每一类记一条审计，行上写出进行中与结果（实际删除的
//! 个数与释放的空间，失败时的原因），最后写出合计释放的空间并重读 Docker 页。执行中不能关闭；对话框被关掉（换了终端）
//! 时照常执行完，结束时另有提示。

use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, TestSupportExt as _, WeakEntity, Window,
    div, prelude::FluentBuilder as _, rems,
};
use termlet_core::hosttools::{
    Action, CleanupPlan, PruneKind, PruneOutcome, PrunePreview, ToolsError, parse_prune,
};

use super::docker::DockerPage;
use super::shared::{self, PageContext};
use super::target::HostLabel;
use super::{audit, errors};
use crate::notify::{Notice, NoticeLine};
use crate::prose::prose;
use crate::sftp::list_model::{format_size, visible_name};
use crate::t;

/// 展开时最多列出的名称数。
const MAX_NAMES: usize = 12;
/// 对话框的宽度（rem），窗口窄时缩到窗口内。
const WIDTH: f32 = 32.;

/// 打开清理的对话框（Docker 页的工具条）。返回对话框的视图：调用方据它释放得知对话框已关闭（恢复刷新）。
pub(super) fn open(
    context: PageContext,
    page: WeakEntity<DockerPage>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<CleanupDialog> {
    let host = context.host(cx);
    let dialogs = context.dialogs.clone();
    let view = cx.new(|cx| CleanupDialog::new(context, page, host, cx));
    let rem = window.rem_size();
    let width = rems(WIDTH)
        .to_pixels(rem)
        .min(window.viewport_size().width - rems(4.).to_pixels(rem));
    let dialog_view = view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        // 执行中不能关闭（关掉后不知道清理到了哪一步）
        let busy = dialog_view.read(cx).running;
        dialog
            .title(t!("tools.cleanup.title"))
            .w(width)
            .overlay_closable(false)
            .close_button(!busy)
            .keyboard(!busy)
            .child(dialog_view.clone())
    });
    dialogs.opened(window, cx);
    let notice = view.read(cx).notice.clone();
    crate::notify::attach(&notice, window, cx);
    view
}

/// 清单的读取。
enum Stage {
    Loading,
    Failed(ToolsError),
    Ready(Rc<CleanupPlan>),
}

/// 一类清理的执行情况。
#[derive(Clone, Debug, PartialEq)]
enum StepState {
    Waiting,
    Running,
    Done(Result<PruneOutcome, SharedString>),
}

pub(super) struct CleanupDialog {
    context: PageContext,
    /// 发起的 Docker 页：清理完重读（页已重建时不管）
    page: WeakEntity<DockerPage>,
    /// 确认框写明的主机（审计记录的主机名也取它）
    host: Option<HostLabel>,
    stage: Stage,
    /// 勾选的几类；卷默认不勾
    selected: BTreeSet<PruneKind>,
    /// 展开了名称的几类
    expanded: BTreeSet<PruneKind>,
    /// 执行中与执行完的各类（按执行的先后）；没有开始时为空
    steps: Vec<(PruneKind, StepState)>,
    running: bool,
    /// 执行不了时的说明（如已断开；弹出的通知会被对话框盖住，见 [`crate::notify::show`]）
    notice: Entity<NoticeLine>,
    _load: Option<gpui_kit::Task<()>>,
}

impl CleanupDialog {
    fn new(
        context: PageContext,
        page: WeakEntity<DockerPage>,
        host: Option<HostLabel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut dialog = Self {
            stage: Stage::Loading,
            context,
            page,
            host,
            selected: BTreeSet::new(),
            expanded: BTreeSet::new(),
            steps: Vec::new(),
            running: false,
            notice: NoticeLine::new(cx),
            _load: None,
        };
        dialog.load(cx);
        dialog
    }

    /// 读清单（只读）；没有连接（重连中）时说明。
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.context.target.clone() else {
            self.stage = Stage::Failed(ToolsError::Exec(
                termlet_core::hosttools::ExecError::Disconnected,
            ));
            return;
        };
        self.stage = Stage::Loading;
        let tools = self.context.tools.clone();
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = tools.cleanup_plan(&target).await;
            let _ = this.update(cx, |dialog, cx| {
                match result {
                    Ok(plan) => dialog.set_plan(plan),
                    Err(error) => dialog.stage = Stage::Failed(error),
                }
                cx.notify();
            });
        }));
    }

    /// 读到清单：默认勾选有内容的几类，卷除外。
    fn set_plan(&mut self, plan: CleanupPlan) {
        let plan = Rc::new(plan);
        self.selected = plan
            .kinds()
            .into_iter()
            .filter(|kind| *kind != PruneKind::Volumes)
            .filter(|kind| has_work(&plan.preview(*kind, true)))
            .collect();
        self.stage = Stage::Ready(plan);
    }

    /// 一并清理已停止的容器（勾选了，且有可以清理的）。
    fn with_containers(&self, plan: &CleanupPlan) -> bool {
        self.selected.contains(&PruneKind::Containers)
            && has_work(&plan.preview(PruneKind::Containers, true))
    }

    /// 将执行的几类：勾选了且按现在的勾选有可以清理的，按执行的先后。
    fn planned(&self, plan: &CleanupPlan) -> Vec<(PruneKind, PrunePreview)> {
        let with = self.with_containers(plan);
        plan.kinds()
            .into_iter()
            .filter(|kind| self.selected.contains(kind))
            .map(|kind| (kind, plan.preview(kind, with)))
            .filter(|(_, preview)| has_work(preview))
            .collect()
    }

    fn toggle(&mut self, kind: PruneKind, on: bool, cx: &mut Context<Self>) {
        if self.running || !self.steps.is_empty() {
            return;
        }
        if on {
            self.selected.insert(kind);
        } else {
            self.selected.remove(&kind);
        }
        cx.notify();
    }

    fn toggle_names(&mut self, kind: PruneKind, cx: &mut Context<Self>) {
        if !self.expanded.remove(&kind) {
            self.expanded.insert(kind);
        }
        cx.notify();
    }

    /// 执行：逐类 prune，每类记一条审计；对话框被关掉也照常执行完（结束时提示），之后重读 Docker 页。
    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Stage::Ready(plan) = &self.stage else {
            return;
        };
        if self.running || !self.steps.is_empty() {
            return;
        }
        let kinds: Vec<PruneKind> = self.planned(plan).into_iter().map(|(k, _)| k).collect();
        if kinds.is_empty() {
            return;
        }
        let Some(target) = self.context.target.clone() else {
            crate::notify::show(Notice::error(t!("tools.error.disconnected")), window, cx);
            return;
        };
        let engine = plan.engine;
        self.steps = kinds.iter().map(|k| (*k, StepState::Waiting)).collect();
        self.running = true;
        cx.notify();
        let tools = self.context.tools.clone();
        let host = self
            .host
            .as_ref()
            .map(|h| h.name.to_string())
            .unwrap_or_default();
        let label = t!("tools.cleanup.audit_label").to_string();
        let log = crate::cli_bridge::audit(cx);
        let page = self.page.clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut outcomes: Vec<Result<PruneOutcome, SharedString>> = Vec::new();
            for (i, kind) in kinds.iter().copied().enumerate() {
                let _ = this.update(cx, |dialog, cx| {
                    dialog.steps[i].1 = StepState::Running;
                    cx.notify();
                });
                let action = Action::Prune { engine, kind };
                let command = action.command_line().unwrap_or_default();
                let started = Instant::now();
                let result = tools.act(&target, action).await;
                audit::record(
                    log.clone(),
                    audit::action_entry(&host, &command, &label, &result, started.elapsed()),
                );
                let outcome = match result {
                    Ok(result) if result.ok => Ok(parse_prune(kind, &result.output)),
                    Ok(result) => {
                        let output = shared::brief(&result.output);
                        Err(if output.is_empty() {
                            t!("tools.exit_code", code = result.exit_code)
                        } else {
                            output.into()
                        })
                    }
                    Err(ToolsError::Exec(termlet_core::hosttools::ExecError::Timeout)) => {
                        Err(t!("tools.error.action_timeout"))
                    }
                    Err(error) => Err(errors::describe(&error)),
                };
                outcomes.push(outcome.clone());
                let _ = this.update(cx, |dialog, cx| {
                    dialog.steps[i].1 = StepState::Done(outcome);
                    cx.notify();
                });
            }
            let shown = this
                .update(cx, |dialog, cx| {
                    dialog.running = false;
                    cx.notify();
                })
                .is_ok();
            let _ = page.update(cx, |page, cx| page.reload(cx));
            // 对话框已被关掉（换了终端）：提示结果
            if !shown {
                let _ = cx.update(|window, cx| {
                    let failed = outcomes.iter().any(Result::is_err);
                    let text = t!("tools.cleanup.done_note", result = total_text(&outcomes));
                    let note = if failed {
                        Notification::warning(text)
                    } else {
                        Notification::success(text)
                    };
                    window.push_notification(note, cx);
                });
            }
        })
        .detach();
    }

    // ------------------------------------------------------------ 渲染

    /// 一类：勾选框与名称、个数与可释放的空间（执行中、执行完时是进度与结果），可以展开看名称；卷另有醒目的提示。
    fn render_kind(
        &self,
        plan: &CleanupPlan,
        kind: PruneKind,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let preview = plan.preview(kind, self.with_containers(plan));
        let work = has_work(&preview);
        let started = !self.steps.is_empty();
        let step = self
            .steps
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, s)| s.clone());
        let key = kind_key(kind);
        let checked = self.selected.contains(&kind) && work;
        let status: AnyElement = match &step {
            None => div()
                .id(SharedString::from(format!("tools-cleanup-summary-{key}")))
                .test_support()
                .flex_none()
                .text_color(muted)
                .child(summary_text(kind, &preview))
                .into_any_element(),
            Some(StepState::Waiting) => div()
                .flex_none()
                .text_color(muted)
                .child(t!("tools.cleanup.step_waiting"))
                .into_any_element(),
            Some(StepState::Running) => h_flex()
                .flex_none()
                .gap_1()
                .text_color(muted)
                .child(Spinner::new().xsmall().color(muted))
                .child(t!("tools.cleanup.running"))
                .into_any_element(),
            Some(StepState::Done(Ok(outcome))) => h_flex()
                .id(SharedString::from(format!("tools-cleanup-result-{key}")))
                .test_support()
                .flex_none()
                .gap_1()
                .text_color(theme.success)
                .child(Icon::new(IconName::Check).xsmall())
                .child(outcome_text(outcome))
                .into_any_element(),
            Some(StepState::Done(Err(error))) => div()
                .id(SharedString::from(format!("tools-cleanup-result-{key}")))
                .test_support()
                .flex_1()
                .min_w_0()
                .text_color(danger)
                .child(prose(error.clone()))
                .into_any_element(),
        };
        let toggle = (!preview.items.is_empty()).then(|| {
            let open = self.expanded.contains(&kind);
            Button::new(SharedString::from(format!("tools-cleanup-names-{key}")))
                .ghost()
                .xsmall()
                .icon(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .tooltip(if open {
                    t!("tools.cleanup.hide_items")
                } else {
                    t!("tools.cleanup.show_items")
                })
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_names(kind, cx)))
        });
        let names = (self.expanded.contains(&kind) && !preview.items.is_empty()).then(|| {
            let mut shown: Vec<String> = preview
                .items
                .iter()
                .take(MAX_NAMES)
                .map(|item| visible_name(&item.name))
                .collect();
            if preview.items.len() > MAX_NAMES {
                shown.push(
                    t!(
                        "tools.cleanup.more",
                        count = preview.items.len() - MAX_NAMES
                    )
                    .to_string(),
                );
            }
            div()
                .id(SharedString::from(format!("tools-cleanup-items-{key}")))
                .test_support()
                .pl_6()
                .text_color(muted)
                .font_family(theme.mono_font_family.clone())
                .child(shown.join(&t!("tools.list_separator")))
        });
        // 卷：数据会永久删除（总是写出，勾选时加重）；Docker 23 起命名的卷不在其中
        let volume_notes = (kind == PruneKind::Volumes).then(|| {
            let named = plan.named_unused_volumes(self.with_containers(plan));
            v_flex()
                .pl_6()
                .gap_0p5()
                .child(
                    div()
                        .id("tools-cleanup-volume-warning")
                        .test_support()
                        .text_color(danger)
                        .when(checked, |this| this.font_medium())
                        .child(prose(t!("tools.cleanup.volume_warning"))),
                )
                .when(named > 0, |this| {
                    this.child(
                        div()
                            .id("tools-cleanup-named-volumes")
                            .test_support()
                            .text_color(muted)
                            .child(prose(if named == 1 {
                                t!("tools.cleanup.named_volumes_one")
                            } else {
                                t!("tools.cleanup.named_volumes", count = named)
                            })),
                    )
                })
        });
        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div().flex_1().min_w_0().child(
                            Checkbox::new(SharedString::from(format!("tools-cleanup-check-{key}")))
                                .label(kind_label(kind, plan))
                                .checked(checked)
                                .disabled(!work || started)
                                .on_change(cx.listener(move |this, on: &bool, _, cx| {
                                    this.toggle(kind, *on, cx)
                                })),
                        ),
                    )
                    .child(status)
                    .children(toggle),
            )
            .children(names)
            .children(volume_notes)
            .into_any_element()
    }

    /// 将执行的命令（与审计记录的写法一致；经免密码 sudo 读取时带 `sudo -n`）。
    fn render_commands(&self, plan: &CleanupPlan, cx: &App) -> Option<AnyElement> {
        let lines: Vec<String> = self
            .planned(plan)
            .into_iter()
            .filter_map(|(kind, _)| {
                Action::Prune {
                    engine: plan.engine,
                    kind,
                }
                .command_line_with(plan.sudo)
                .ok()
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        let theme = cx.theme();
        Some(
            h_flex()
                .id("tools-cleanup-commands")
                .test_support()
                .items_start()
                .gap_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(div().flex_none().child(t!("tools.confirm.will_run")))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .font_family(theme.mono_font_family.clone())
                        .children(lines.into_iter().map(|line| div().child(line))),
                )
                .into_any_element(),
        )
    }

    /// 底部：左边是合计（可释放、已释放），右边是按钮。
    fn render_footer(&self, plan: Option<&CleanupPlan>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let finished = !self.steps.is_empty() && !self.running;
        let total: Option<SharedString> = if finished {
            let outcomes: Vec<Result<PruneOutcome, SharedString>> = self
                .steps
                .iter()
                .filter_map(|(_, s)| match s {
                    StepState::Done(outcome) => Some(outcome.clone()),
                    _ => None,
                })
                .collect();
            Some(total_text(&outcomes))
        } else {
            plan.map(|plan| {
                let planned = self.planned(plan);
                if planned.is_empty() {
                    t!("tools.cleanup.nothing_selected")
                } else if planned.iter().any(|(_, p)| p.bytes.is_none()) {
                    t!("tools.cleanup.total_unknown")
                } else {
                    let bytes: u64 = planned.iter().filter_map(|(_, p)| p.bytes).sum();
                    t!("tools.cleanup.total_estimate", size = format_size(bytes))
                }
            })
        };
        let can_run =
            plan.is_some_and(|plan| !self.planned(plan).is_empty()) && self.steps.is_empty();
        let buttons: Vec<AnyElement> = if finished {
            vec![
                Button::new("tools-cleanup-close")
                    .primary()
                    .label(t!("common.close"))
                    .on_click(|_, window, cx| window.close_dialog(cx))
                    .into_any_element(),
            ]
        } else {
            vec![
                Button::new("tools-cleanup-cancel")
                    .label(t!("common.cancel"))
                    .disabled(self.running)
                    .on_click(|_, window, cx| window.close_dialog(cx))
                    .into_any_element(),
                Button::new("tools-cleanup-run")
                    .danger()
                    .icon(IconName::BrushCleaning)
                    .label(t!("tools.cleanup.run"))
                    .loading(self.running)
                    .disabled(!can_run)
                    .on_click(cx.listener(|this, _, window, cx| this.run(window, cx)))
                    .into_any_element(),
            ]
        };
        h_flex()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("tools-cleanup-total")
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .when(finished, |this| this.font_medium())
                    .children(total.map(prose)),
            )
            .children(buttons)
            .into_any_element()
    }
}

#[cfg(test)]
impl CleanupDialog {
    /// 测试中直接放进清单（不连服务器）。
    pub(super) fn set_test_plan(&mut self, plan: CleanupPlan, cx: &mut Context<Self>) {
        self._load = None;
        self.set_plan(plan);
        cx.notify();
    }

    pub(super) fn selected_kinds(&self) -> Vec<PruneKind> {
        self.selected.iter().copied().collect()
    }
}

impl Render for CleanupDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let host = self
            .host
            .as_ref()
            .map(|host| shared::confirm_host(host, cx).into_any_element());
        let body: AnyElement = match &self.stage {
            Stage::Loading => h_flex()
                .id("tools-cleanup-loading")
                .test_support()
                .gap_2()
                .text_sm()
                .text_color(muted)
                .child(Spinner::new().small())
                .child(prose(t!("tools.cleanup.loading")))
                .into_any_element(),
            Stage::Failed(error) => {
                let error = error.clone();
                v_flex()
                    .id("tools-cleanup-failed")
                    .test_support()
                    .gap_2()
                    .text_sm()
                    .child(div().text_color(theme.danger).child(prose(t!(
                        "tools.cleanup.load_failed",
                        detail = errors::describe(&error)
                    ))))
                    .child(
                        h_flex().child(
                            Button::new("tools-cleanup-retry")
                                .outline()
                                .xsmall()
                                .icon(IconName::RefreshCw)
                                .label(t!("tools.refresh"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.load(cx);
                                    cx.notify();
                                })),
                        ),
                    )
                    .into_any_element()
            }
            Stage::Ready(plan) => {
                let plan = plan.clone();
                v_flex()
                    .gap_2p5()
                    .text_sm()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(t!("tools.cleanup.intro"))),
                    )
                    .children(
                        plan.kinds()
                            .into_iter()
                            .map(|kind| self.render_kind(&plan, kind, cx)),
                    )
                    .children(
                        (self.steps.is_empty())
                            .then(|| self.render_commands(&plan, cx))
                            .flatten(),
                    )
                    .into_any_element()
            }
        };
        let plan = match &self.stage {
            Stage::Ready(plan) => Some(plan.clone()),
            _ => None,
        };
        v_flex()
            .id("tools-cleanup-dialog")
            .test_support()
            .gap_3()
            .children(host)
            .child(body)
            .when(self.notice.read(cx).is_shown(), |this| {
                this.child(div().text_sm().child(self.notice.clone()))
            })
            .child(self.render_footer(plan.as_deref(), cx))
    }
}

/// 有可以清理的（构建缓存看记录数与大小）。
fn has_work(preview: &PrunePreview) -> bool {
    preview.count > 0 || (preview.kind == PruneKind::BuildCache && preview.bytes.unwrap_or(0) > 0)
}

/// 元素 ID 用的名称。
fn kind_key(kind: PruneKind) -> &'static str {
    match kind {
        PruneKind::Containers => "containers",
        PruneKind::Networks => "networks",
        PruneKind::Volumes => "volumes",
        PruneKind::Images => "images",
        PruneKind::BuildCache => "build-cache",
    }
}

/// 一类的名称（Docker 23 起只清理匿名卷时写明）。
fn kind_label(kind: PruneKind, plan: &CleanupPlan) -> SharedString {
    match kind {
        PruneKind::Containers => t!("tools.cleanup.kind_containers"),
        PruneKind::Networks => t!("tools.cleanup.kind_networks"),
        PruneKind::Volumes if plan.anonymous_volumes_only => {
            t!("tools.cleanup.kind_anonymous_volumes")
        }
        PruneKind::Volumes => t!("tools.cleanup.kind_volumes"),
        PruneKind::Images => t!("tools.cleanup.kind_images"),
        PruneKind::BuildCache => t!("tools.cleanup.kind_build_cache"),
    }
}

/// 个数与可释放的空间：“4 个 · 2.0 MB”，网络只写个数，构建缓存写记录数与大约的空间；没有可以清理的写“没有”。
fn summary_text(kind: PruneKind, preview: &PrunePreview) -> SharedString {
    if !has_work(preview) {
        return t!("tools.cleanup.none");
    }
    let count = match (kind, preview.count) {
        (PruneKind::BuildCache, 1) => t!("tools.cleanup.entries_one"),
        (PruneKind::BuildCache, n) => t!("tools.cleanup.entries", count = n),
        (_, 1) => t!("tools.cleanup.items_one"),
        (_, n) => t!("tools.cleanup.items", count = n),
    };
    match (kind, preview.bytes) {
        (PruneKind::Networks, _) | (_, None) => count,
        (PruneKind::BuildCache, Some(bytes)) => format!(
            "{count} · {}",
            t!("tools.cleanup.about", size = format_size(bytes))
        )
        .into(),
        (_, Some(bytes)) => format!("{count} · {}", format_size(bytes)).into(),
    }
}

/// 一类清理的结果：“已删除 3 个 · 释放 12.3 MB”（看不出的不写）。
fn outcome_text(outcome: &PruneOutcome) -> SharedString {
    let removed = outcome.deleted.map(|n| {
        if n == 1 {
            t!("tools.cleanup.removed_one")
        } else {
            t!("tools.cleanup.removed", count = n)
        }
    });
    let freed = outcome
        .reclaimed
        .map(|bytes| t!("tools.cleanup.freed", size = format_size(bytes)));
    match (removed, freed) {
        (Some(removed), Some(freed)) => format!("{removed} · {freed}").into(),
        (Some(text), None) | (None, Some(text)) => text,
        (None, None) => t!("tools.cleanup.step_done"),
    }
}

/// 合计：“共释放 1.2 GB”（读不到释放的空间时只说完成；有失败的另说明）。
fn total_text(outcomes: &[Result<PruneOutcome, SharedString>]) -> SharedString {
    let freed: Option<u64> = outcomes
        .iter()
        .filter_map(|o| o.as_ref().ok())
        .filter_map(|o| o.reclaimed)
        .reduce(|a, b| a + b);
    let failed = outcomes.iter().filter(|o| o.is_err()).count();
    let total = match freed {
        Some(bytes) => t!("tools.cleanup.total_freed", size = format_size(bytes)),
        None => t!("tools.cleanup.done"),
    };
    match failed {
        0 => total,
        1 => format!("{total} · {}", t!("tools.cleanup.failed_one")).into(),
        n => format!("{total} · {}", t!("tools.cleanup.failed", count = n)).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 结果与合计的写法：看不出的不写；英文的个数区分单复数。
    #[test]
    fn outcome_and_total_texts() {
        use crate::i18n::{Language, set_language};
        set_language(Language::En);
        let both = PruneOutcome {
            deleted: Some(3),
            reclaimed: Some(2048),
        };
        assert_eq!(outcome_text(&both), "Removed 3 · Freed 2.0 KB");
        let one = PruneOutcome {
            deleted: Some(1),
            reclaimed: None,
        };
        assert_eq!(outcome_text(&one), "Removed 1");
        assert_eq!(
            total_text(&[Ok(both), Ok(one), Err("x".into())]),
            "Freed 2.0 KB in total · 1 failed"
        );
        set_language(Language::ZhCn);
        assert_eq!(
            outcome_text(&PruneOutcome::default()),
            t!("tools.cleanup.step_done")
        );
    }
}
