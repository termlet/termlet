//! 「命令行工具」页的 Agent Skill：通用目录、Claude Code、WorkBuddy 中的安装状态，安装、更新、删除
//! （立即执行，结果写在设置对话框底部）；以前装在 Codex 旧位置的可以移到通用目录。“复制 Skill”给其他 AI 工具用。

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, ElementId, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task,
    TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::proto::skill::{self, Agent, Status, Target};

use super::CliEnv;
use crate::notify::{self, Notice};
use crate::prose::prose;
use crate::t;

/// 对一个位置的操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SkillOp {
    /// 安装或更新（已有文件时覆盖）
    Install(Agent),
    Remove(Agent),
    /// 从 Codex 的旧位置移到通用目录：先装到通用目录，再删掉旧文件
    Migrate,
}

pub(super) struct SkillSection {
    env: CliEnv,
    /// 各位置与状态；`None` 为还在检查，`Some(None)` 为找不到用户主目录
    pub(super) targets: Option<Option<Vec<Target>>>,
    /// 正在执行的操作（期间其余按钮不能点）
    busy: Option<SkillOp>,
    _task: Option<Task<()>>,
}

/// 各位置与状态（读几个小文件，在后台执行器上调用）。
fn list_targets(env: &CliEnv) -> Option<Vec<Target>> {
    let home = env.home.as_deref()?;
    Some(if env.system_env {
        skill::targets(home, &skill::system_env)
    } else {
        skill::targets(home, &|_| None)
    })
}

/// 执行一个操作。`dirs` 为各位置的 skills 根目录（来自上一次列出的结果）。
fn run(op: SkillOp, dirs: &[(Agent, PathBuf)]) -> std::io::Result<()> {
    let dir = |agent: Agent| {
        dirs.iter()
            .find(|(a, _)| *a == agent)
            .map(|(_, d)| d.clone())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    };
    match op {
        SkillOp::Install(agent) => skill::install(&dir(agent)?).map(drop),
        SkillOp::Remove(agent) => skill::remove(&dir(agent)?),
        SkillOp::Migrate => {
            skill::install(&dir(Agent::Generic)?)?;
            skill::remove(&dir(Agent::Codex)?)
        }
    }
}

/// 位置的名称。
pub(super) fn agent_name(agent: Agent) -> SharedString {
    match agent {
        Agent::Generic => t!("cli_ui.skill.agent_generic"),
        Agent::ClaudeCode => t!("cli_ui.skill.agent_claude"),
        Agent::WorkBuddy => t!("cli_ui.skill.agent_workbuddy"),
        Agent::Codex => t!("cli_ui.skill.agent_codex"),
    }
}

/// 路径下方的说明：通用目录列出会读取它的工具，旧位置说明原因，其余在没有找到工具时提示仍可安装。
pub(super) fn target_note(target: &Target) -> Option<SharedString> {
    if target.legacy {
        return Some(t!("cli_ui.skill.legacy"));
    }
    match target.agent {
        Agent::Generic if target.readers.is_empty() => Some(t!("cli_ui.skill.readers_none")),
        Agent::Generic => {
            let separator = t!("cli_ui.skill.list_separator");
            Some(t!(
                "cli_ui.skill.readers",
                names = target.readers.join(separator.as_ref())
            ))
        }
        _ if target.detected => None,
        agent => Some(t!("cli_ui.skill.not_detected", tool = agent_name(agent))),
    }
}

/// 操作成功后的通知。
fn done_text(op: SkillOp, was: Option<Status>) -> SharedString {
    match op {
        SkillOp::Install(agent) if was == Some(Status::Outdated) => {
            t!("cli_ui.skill.updated", name = agent_name(agent))
        }
        SkillOp::Install(agent) => t!("cli_ui.skill.installed", name = agent_name(agent)),
        SkillOp::Remove(agent) => t!("cli_ui.skill.removed", name = agent_name(agent)),
        SkillOp::Migrate => t!("cli_ui.skill.migrated"),
    }
}

/// 元素 ID：按位置命名（`cli-skill-install-claude`）。
fn id(name: &str, agent: Agent) -> ElementId {
    ElementId::from(SharedString::from(format!("{name}-{}", agent.as_str())))
}

impl SkillSection {
    pub(super) fn new(env: CliEnv, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            env,
            targets: None,
            busy: None,
            _task: None,
        };
        this.reload(cx);
        this
    }

    /// 重新检查各位置的安装状态。
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        let env = self.env.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let targets = cx.background_spawn(async move { list_targets(&env) }).await;
            let _ = this.update(cx, |this, cx| {
                this.targets = Some(targets);
                cx.notify();
            });
        }));
    }

    /// 执行操作，完成后重新列出（删除 Codex 旧位置的文件后，这一项不再列出）并说明结果（`notify::show`）。
    pub(super) fn apply(&mut self, op: SkillOp, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Some(targets)) = &self.targets else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        let was = match op {
            SkillOp::Install(agent) => targets.iter().find(|t| t.agent == agent).map(|t| t.status),
            _ => None,
        };
        let dirs: Vec<(Agent, PathBuf)> =
            targets.iter().map(|t| (t.agent, t.dir.clone())).collect();
        let env = self.env.clone();
        tracing::info!(op = ?op, "Agent Skill：执行操作");
        self.busy = Some(op);
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let (result, targets) = cx
                .background_spawn(async move { (run(op, &dirs), list_targets(&env)) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                this.targets = Some(targets);
                let notice = match result {
                    Ok(()) => {
                        tracing::info!(op = ?op, "Agent Skill：操作完成");
                        Notice::success(done_text(op, was))
                    }
                    Err(e) => {
                        tracing::warn!("修改 Agent Skill 失败：{e}");
                        Notice::error(t!("cli_ui.skill.failed", error = e.to_string()))
                    }
                };
                notify::show(notice, window, cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn status_text(status: Status, cx: &Context<Self>) -> (SharedString, Hsla) {
        let theme = cx.theme();
        match status {
            Status::Installed => (t!("cli_ui.skill.status_installed"), theme.success),
            Status::Outdated => (t!("cli_ui.skill.status_outdated"), theme.warning),
            Status::Missing => (t!("cli_ui.skill.status_missing"), theme.muted_foreground),
        }
    }

    fn render_target(
        &self,
        target: &Target,
        last: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let mono = theme.mono_font_family.clone();
        let agent = target.agent;
        let busy = self.busy.is_some();
        let (status, status_color) = Self::status_text(target.status, cx);
        let path: SharedString = target.path.display().to_string().into();
        let tooltip = path.clone();
        let note = target_note(target);
        let install = (!target.legacy && target.status != Status::Installed).then(|| {
            let label = if target.status == Status::Outdated {
                t!("cli_ui.skill.update")
            } else {
                t!("cli_ui.skill.install")
            };
            Button::new(id("cli-skill-install", agent))
                .outline()
                .small()
                .label(label)
                .loading(self.busy == Some(SkillOp::Install(agent)))
                .disabled(busy)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.apply(SkillOp::Install(agent), window, cx)
                }))
        });
        let migrate = target.legacy.then(|| {
            Button::new(id("cli-skill-migrate", agent))
                .outline()
                .small()
                .label(t!("cli_ui.skill.migrate"))
                .loading(self.busy == Some(SkillOp::Migrate))
                .disabled(busy)
                .on_click(
                    cx.listener(|this, _, window, cx| this.apply(SkillOp::Migrate, window, cx)),
                )
        });
        let remove = (target.status != Status::Missing).then(|| {
            Button::new(id("cli-skill-remove", agent))
                .ghost()
                .small()
                .label(t!("cli_ui.skill.remove"))
                .loading(self.busy == Some(SkillOp::Remove(agent)))
                .disabled(busy)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.apply(SkillOp::Remove(agent), window, cx)
                }))
        });
        h_flex()
            .id(id("cli-skill", agent))
            .gap_3()
            .px_3()
            .py_2()
            .when(!last, |this| this.border_b_1().border_color(border))
            .child(
                div()
                    .w(rems(7.))
                    .flex_none()
                    .text_sm()
                    .child(agent_name(agent)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .id(id("cli-skill-path", agent))
                            .truncate()
                            .text_xs()
                            .text_color(muted)
                            .font_family(mono)
                            .child(path)
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip.clone()).build(window, cx)
                            }),
                    )
                    .when_some(note, |this, note| {
                        this.child(div().text_xs().text_color(muted).child(prose(note)))
                    }),
            )
            .child(
                div()
                    .id(id("cli-skill-status", agent))
                    .test_support()
                    .flex_none()
                    .text_xs()
                    .text_color(status_color)
                    .child(status),
            )
            .children(install)
            .children(migrate)
            .children(remove)
    }
}

impl Render for SkillSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, radius) = (theme.muted_foreground, theme.border, theme.radius);
        let mono = theme.mono_font_family.clone();
        let code_bg = theme.muted;
        let list = match &self.targets {
            None => div()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("cli_ui.skill.loading")))
                .into_any_element(),
            Some(None) => div()
                .text_xs()
                .text_color(theme.danger)
                .child(prose(t!("cli_ui.skill.no_home")))
                .into_any_element(),
            Some(Some(targets)) => {
                let last = targets.len().saturating_sub(1);
                let rows: Vec<_> = targets
                    .iter()
                    .enumerate()
                    .map(|(i, target)| self.render_target(target, i == last, cx).into_any_element())
                    .collect();
                v_flex()
                    .border_1()
                    .border_color(border)
                    .rounded(radius)
                    .overflow_hidden()
                    .children(rows)
                    .into_any_element()
            }
        };
        v_flex().gap_2().child(list).child(
            h_flex()
                .gap_2()
                .text_xs()
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .flex_wrap()
                        .gap_1()
                        .text_color(muted)
                        .child(prose(t!("cli_ui.skill.cli")))
                        .child(
                            div()
                                .px_1()
                                .rounded(radius)
                                .bg(code_bg)
                                .font_family(mono)
                                .child("termlet skill install --agent agents"),
                        ),
                )
                .child(
                    Button::new("cli-skill-copy")
                        .outline()
                        .small()
                        .icon(IconName::Copy)
                        .label(t!("cli_ui.skill.copy"))
                        .on_click(|_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                skill::CONTENT.to_string(),
                            ));
                            notify::show(Notice::success(t!("cli_ui.skill.copied")), window, cx);
                        }),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::i18n::{self, Language};

    fn target(agent: Agent, detected: bool, readers: &[&str], legacy: bool) -> Target {
        Target {
            agent,
            dir: Path::new("h").join("skills"),
            path: Path::new("h")
                .join("skills")
                .join("termlet")
                .join("SKILL.md"),
            status: Status::Missing,
            detected,
            readers: readers.iter().map(|s| s.to_string()).collect(),
            legacy,
        }
    }

    #[test]
    fn notes_explain_each_location() {
        let original = i18n::language();
        i18n::set_language(Language::ZhCn);
        assert_eq!(
            target_note(&target(Agent::Generic, true, &["Codex", "Cursor"], false)).as_deref(),
            Some("检测到会读取这里的：Codex、Cursor")
        );
        assert!(
            target_note(&target(Agent::Generic, false, &[], false))
                .unwrap()
                .contains("Gemini CLI")
        );
        assert_eq!(
            target_note(&target(Agent::ClaudeCode, true, &[], false)),
            None
        );
        assert_eq!(
            target_note(&target(Agent::WorkBuddy, false, &[], false)).as_deref(),
            Some("这台电脑上还没有 WorkBuddy，也可以先安装")
        );
        assert!(
            target_note(&target(Agent::Codex, true, &[], true))
                .unwrap()
                .starts_with("Codex 已改为读取通用目录")
        );
        i18n::set_language(Language::En);
        assert_eq!(
            target_note(&target(Agent::Generic, true, &["Codex", "Cursor"], false)).as_deref(),
            Some("Found tools that read it: Codex, Cursor")
        );
        assert_eq!(
            done_text(SkillOp::Install(Agent::ClaudeCode), Some(Status::Outdated)),
            "Skill updated: Claude Code"
        );
        assert_eq!(
            done_text(SkillOp::Install(Agent::Generic), Some(Status::Missing)),
            "Skill installed: Shared folder"
        );
        i18n::set_language(original);
    }

    #[test]
    fn operations_install_remove_and_migrate() {
        let root = std::env::temp_dir().join(format!(
            "termlet-cli-ui-skill-{}",
            termlet_core::id::random_token(8)
        ));
        let generic = root.join(".agents").join("skills");
        let codex = root.join(".codex").join("skills");
        let dirs = [
            (Agent::Generic, generic.clone()),
            (Agent::Codex, codex.clone()),
        ];
        run(SkillOp::Install(Agent::Codex), &dirs).unwrap();
        assert_eq!(skill::status_of(&skill::file(&codex)), Status::Installed);
        run(SkillOp::Migrate, &dirs).unwrap();
        assert_eq!(skill::status_of(&skill::file(&generic)), Status::Installed);
        assert_eq!(
            skill::status_of(&skill::file(&codex)),
            Status::Missing,
            "移到通用目录后旧位置的文件删除"
        );
        run(SkillOp::Remove(Agent::Generic), &dirs).unwrap();
        assert_eq!(skill::status_of(&skill::file(&generic)), Status::Missing);
        assert!(
            run(SkillOp::Install(Agent::ClaudeCode), &dirs).is_err(),
            "没有列出的位置不能安装"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
