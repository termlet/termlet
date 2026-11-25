//! 设置中的「AI 与自动化」页：外部 CLI 的开关（“允许 AI 工具与脚本访问”）、审计日志入口、命令行工具、Agent Skill 与
//! MCP 服务的接入，以及危险命令规则。
//!
//! 开关、MCP 的 HTTP 接入（开关与端口）与规则是草稿，随设置的“保存”写入（[`CliPage::apply`]，有不能使用的
//! 自定义规则或端口时 [`CliPage::validate`] 返回错误）；安装、PATH、重新生成 HTTP 令牌立即执行。规则测试用草稿中的
//! 规则，输入命令时即时显示命中哪一条。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::form::{Field, field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, TestSupportExt as _,
    Window, div, prelude::FluentBuilder as _,
};
use termlet_core::cli::APPROVAL_TIMEOUT;
use termlet_core::guard::{Guard, MAX_CUSTOM_RULES, Rule, RuleError, builtin_rules};
use termlet_core::model::{Approval, LocalSettings, Settings};

use super::CliEnv;
use super::mcp::McpSection;
use super::rules::{
    custom_error, custom_patterns, draft_settings, error_text, outcome_text, rule_name,
    set_rule_enabled, test_command,
};
use super::skill::SkillSection;
use super::tool::ToolSection;
use crate::prose::{ProseLabel as _, prose};
use crate::t;

/// “命中规则时”的选项，按界面上的顺序。
const APPROVALS: [Approval; 2] = [Approval::Ask, Approval::Block];

/// 一条自定义规则（界面上的一行）。规则没有 ID，用递增的编号区分各行。
pub(super) struct CustomRule {
    key: u64,
    input: Entity<InputState>,
    /// 输入框中的原文（随输入更新，保存时不必读输入框）
    pattern: String,
    /// 正则不能使用的原因（随输入更新）；数量超出上限另算，见 [`CliPage::custom_errors`]
    error: Option<RuleError>,
    _subscription: Subscription,
}

pub(crate) struct CliPage {
    /// 外部 CLI 的开关（本机设置）
    enabled: bool,
    /// Termlet 没有打开时由命令行自动在后台启动（本机设置）
    auto_start: bool,
    approval: Approval,
    /// 关闭的内置规则（设置中不认识的 ID 原样保留）
    disabled_rules: Vec<String>,
    pub(super) custom: Vec<CustomRule>,
    next_key: u64,
    test: Entity<InputState>,
    /// 规则测试的结果：`None` 为没有输入命令，`Some(None)` 为没有命中
    pub(super) test_result: Option<Option<Rule>>,
    /// 按草稿中的规则建好的检查器，规则改了时清掉、测试时再建
    guard: Option<Guard>,
    pub(super) tool: Entity<ToolSection>,
    pub(super) skill: Entity<SkillSection>,
    pub(super) mcp: Entity<McpSection>,
    _test_subscription: Subscription,
}

impl CliPage {
    pub(crate) fn new(
        settings: &Settings,
        local: &LocalSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_env(settings, local, CliEnv::system(cx), window, cx)
    }

    /// 指定本机环境（应用程序的路径、用户主目录），测试中用临时目录。
    pub(super) fn with_env(
        settings: &Settings,
        local: &LocalSettings,
        env: CliEnv,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let test = cx
            .new(|cx| InputState::new(window, cx).placeholder(t!("cli_ui.rules.test_placeholder")));
        let test_subscription = cx.subscribe(&test, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.refresh_test(cx);
                cx.notify();
            }
        });
        let tool = {
            let env = env.clone();
            cx.new(|cx| ToolSection::new(&env, cx))
        };
        let mcp = cx.new(|cx| McpSection::new(&env, local, window, cx));
        let skill = cx.new(|cx| SkillSection::new(env, cx));
        let guard = &settings.guard;
        let mut this = Self {
            enabled: local.cli.enabled,
            auto_start: local.cli.auto_start,
            approval: guard.approval,
            disabled_rules: guard.disabled_rules.clone(),
            custom: Vec::new(),
            next_key: 0,
            test,
            test_result: None,
            guard: None,
            tool,
            skill,
            mcp,
            _test_subscription: test_subscription,
        };
        for pattern in &guard.custom_rules {
            this.push_custom(pattern, window, cx);
        }
        this
    }

    /// 写进设置：外部 CLI 与 MCP 的 HTTP 接入的开关、端口写本机设置，规则写同步的设置（其余字段不动）。
    pub(crate) fn apply(&self, settings: &mut Settings, local: &mut LocalSettings, cx: &App) {
        local.cli.enabled = self.enabled;
        local.cli.auto_start = self.auto_start;
        self.mcp.read(cx).http.read(cx).apply(&mut local.cli.http);
        let guard = &mut settings.guard;
        guard.disabled_rules = self.disabled_rules.clone();
        guard.custom_rules = custom_patterns(self.custom.iter().map(|r| r.pattern.as_str()));
        guard.approval = self.approval;
    }

    /// 立即执行的操作（重新生成令牌、安装 Skill、修改 PATH）正在进行（这时设置不重新载入这一页）。
    pub(crate) fn is_busy(&self, cx: &App) -> bool {
        self.mcp.read(cx).http.read(cx).is_busy()
            || self.skill.read(cx).is_busy()
            || self.tool.read(cx).is_busy()
    }

    /// 有不能使用的自定义规则（不合法的正则、会匹配空命令的、超出数量上限的）时不能保存。
    pub(crate) fn validate(&self, cx: &App) -> Result<(), SharedString> {
        if self.custom_errors().iter().any(Option::is_some) {
            return Err(t!("cli_ui.rules.fix_custom"));
        }
        self.mcp.read(cx).http.read(cx).validate()
    }

    /// 各行自定义规则的错误：正则本身的错误，以及非空的规则中排在上限之后的（保存后会被跳过）。
    fn custom_errors(&self) -> Vec<Option<RuleError>> {
        let mut filled = 0;
        self.custom
            .iter()
            .map(|row| {
                if row.pattern.trim().is_empty() {
                    return None;
                }
                filled += 1;
                if filled > MAX_CUSTOM_RULES {
                    Some(RuleError::TooMany)
                } else {
                    row.error.clone()
                }
            })
            .collect()
    }

    fn push_custom(&mut self, pattern: &str, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.next_key;
        self.next_key += 1;
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("cli_ui.rules.custom_placeholder"))
                .default_value(pattern.to_string())
        });
        let subscription = cx.subscribe(&input, move |this, input, event: &InputEvent, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let value = input.read(cx).value().to_string();
            if let Some(row) = this.custom.iter_mut().find(|r| r.key == key) {
                row.error = custom_error(&value);
                row.pattern = value;
            }
            this.rules_changed(cx);
        });
        self.custom.push(CustomRule {
            key,
            input,
            pattern: pattern.to_string(),
            error: custom_error(pattern),
            _subscription: subscription,
        });
    }

    /// 草稿中的规则改了：规则测试按新的规则重算。
    fn rules_changed(&mut self, cx: &mut Context<Self>) {
        self.guard = None;
        self.refresh_test(cx);
        cx.notify();
    }

    fn refresh_test(&mut self, cx: &App) {
        let command = self.test.read(cx).value();
        if command.trim().is_empty() {
            self.test_result = None;
            return;
        }
        if self.guard.is_none() {
            let settings = draft_settings(
                &self.disabled_rules,
                custom_patterns(self.custom.iter().map(|r| r.pattern.as_str())),
                self.approval,
            );
            self.guard = Some(Guard::from_settings(&settings).0);
        }
        self.test_result = self
            .guard
            .as_ref()
            .and_then(|guard| test_command(guard, &command));
    }

    // ------------------------------------------------------------ 渲染

    /// 分区的标题（与上一部分之间有分隔线）。
    fn section(title: SharedString, cx: &App) -> Field {
        field().child(
            div()
                .pt_3()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_sm()
                .font_medium()
                .child(title),
        )
    }

    /// 一段说明文字。
    fn hint(text: SharedString, cx: &App) -> Field {
        field().child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(prose(text)),
        )
    }

    fn render_enabled(&self, cx: &mut Context<Self>) -> Field {
        field().child(
            v_flex()
                .gap_1()
                .child(
                    Checkbox::new("cli-enabled")
                        .label(t!("cli_ui.enabled"))
                        .checked(self.enabled)
                        .on_change(cx.listener(|this, checked: &bool, _, cx| {
                            this.enabled = *checked;
                            // HTTP 的状态行据此说明要先打开这一项
                            let http = this.mcp.read(cx).http.clone();
                            http.update(cx, |http, cx| http.set_cli_enabled(*checked, cx));
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .pl_6()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(prose(t!("cli_ui.enabled_hint"))),
                )
                .child(
                    Checkbox::new("cli-auto-start")
                        .prose_label(t!("cli_ui.auto_start"))
                        .checked(self.auto_start)
                        .on_change(cx.listener(|this, checked: &bool, _, cx| {
                            this.auto_start = *checked;
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .pl_6()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(prose(t!("cli_ui.auto_start_hint"))),
                ),
        )
    }

    fn render_approval(&self, cx: &mut Context<Self>) -> Field {
        field().label(t!("cli_ui.rules.approval")).child(
            crate::radio::vertical("cli-approval")
                .children([
                    t!(
                        "cli_ui.rules.approval_ask",
                        seconds = APPROVAL_TIMEOUT.as_secs()
                    ),
                    t!("cli_ui.rules.approval_block"),
                ])
                .selected_index(APPROVALS.iter().position(|a| *a == self.approval))
                .on_change(cx.listener(|this, ix: &usize, _, cx| {
                    if let Some(approval) = APPROVALS.get(*ix) {
                        this.approval = *approval;
                        cx.notify();
                    }
                })),
        )
    }

    fn render_builtin(&self, cx: &mut Context<Self>) -> Field {
        let theme = cx.theme();
        let (muted, border, radius) = (theme.muted_foreground, theme.border, theme.radius);
        let mono = theme.mono_font_family.clone();
        let last = builtin_rules().len().saturating_sub(1);
        let rows = builtin_rules().enumerate().map(|(i, rule)| {
            let id = rule.id();
            let enabled = !self.disabled_rules.iter().any(|d| d == id);
            v_flex()
                .gap_0p5()
                .px_3()
                .py_1p5()
                .when(i < last, |this| this.border_b_1().border_color(border))
                .child(
                    Checkbox::new(SharedString::from(format!("cli-rule-{id}")))
                        .label(rule_name(rule))
                        .checked(enabled)
                        .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                            set_rule_enabled(&mut this.disabled_rules, id, *checked);
                            this.rules_changed(cx);
                        })),
                )
                .child(
                    div()
                        .pl_6()
                        .text_xs()
                        .text_color(muted)
                        .font_family(mono.clone())
                        .child(id),
                )
        });
        field()
            .label(t!("cli_ui.rules.builtin"))
            .description(prose(t!("cli_ui.rules.builtin_hint")))
            .child(
                v_flex()
                    .border_1()
                    .border_color(border)
                    .rounded(radius)
                    .overflow_hidden()
                    .children(rows),
            )
    }

    fn render_custom(&self, cx: &mut Context<Self>) -> Field {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let mono = theme.mono_font_family.clone();
        let errors = self.custom_errors();
        let full = self.custom.len() >= MAX_CUSTOM_RULES;
        let rows = self.custom.iter().zip(errors).map(|(row, error)| {
            let key = row.key;
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&row.input)
                                    .small()
                                    .font_family(mono.clone())
                                    .id(("cli-custom-rule", key)),
                            ),
                        )
                        .child(
                            Button::new(("cli-custom-rule-delete", key))
                                .ghost()
                                .small()
                                .icon(IconName::Trash)
                                .tooltip(t!("cli_ui.rules.delete"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.custom.retain(|r| r.key != key);
                                    this.rules_changed(cx);
                                })),
                        ),
                )
                .when_some(error, |this, error| {
                    this.child(
                        div()
                            .id(("cli-custom-rule-error", key))
                            .test_support()
                            .text_xs()
                            .text_color(danger)
                            .child(error_text(&error)),
                    )
                })
        });
        field()
            .label(t!("cli_ui.rules.custom"))
            .description(prose(t!("cli_ui.rules.custom_hint")))
            .child(
                v_flex().gap_1p5().children(rows).child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("cli-custom-rule-add")
                                .outline()
                                .small()
                                .icon(IconName::Plus)
                                .label(t!("cli_ui.rules.add"))
                                .disabled(full)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.push_custom("", window, cx);
                                    if let Some(row) = this.custom.last() {
                                        row.input.update(cx, |s, cx| s.focus(window, cx));
                                    }
                                    cx.notify();
                                })),
                        )
                        .when(full, |this| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(t!("cli_ui.rules.too_many", max = MAX_CUSTOM_RULES)),
                            )
                        }),
                ),
            )
    }

    fn render_test(&self, cx: &mut Context<Self>) -> Field {
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let result = self.test_result.as_ref().map(|hit| {
            let color = match (hit, self.approval) {
                (None, _) => theme.success,
                (Some(_), Approval::Ask) => theme.warning,
                (Some(_), Approval::Block) => theme.danger,
            };
            div()
                .id("cli-rule-test-result")
                .test_support()
                .text_xs()
                .text_color(color)
                .child(outcome_text(hit.as_ref(), self.approval))
        });
        field()
            .label(t!("cli_ui.rules.test"))
            .description(prose(t!("cli_ui.rules.test_hint")))
            .child(
                v_flex()
                    .gap_1()
                    // 试一试的命令按 Enter（设置把它转成 Confirm）什么也不做
                    .on_action(|_: &gpui_kit::base::actions::Confirm, _, _| {})
                    .child(
                        Input::new(&self.test)
                            .small()
                            .font_family(mono)
                            .id("cli-rule-test"),
                    )
                    .children(result),
            )
    }
}

impl Render for CliPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_form()
            .child(self.render_enabled(cx))
            .child(Self::hint(t!("cli_ui.intro"), cx))
            .child(
                field()
                    .label(t!("cli_ui.audit.label"))
                    .description(prose(t!("cli_ui.audit.hint")))
                    .child(
                        h_flex().child(
                            Button::new("cli-open-audit")
                                .outline()
                                .small()
                                .icon(IconName::FileText)
                                .label(t!("cli_ui.audit.open"))
                                .on_click(|_, window, cx| super::open_audit(window, cx)),
                        ),
                    ),
            )
            .child(Self::section(t!("cli_ui.section.tool"), cx))
            .child(
                field()
                    .label(t!("cli_ui.tool.label"))
                    .child(self.tool.clone()),
            )
            .child(
                field()
                    .label(t!("cli_ui.skill.label"))
                    .description(prose(t!("cli_ui.skill.hint")))
                    .child(self.skill.clone()),
            )
            .child(
                field()
                    .label(t!("cli_ui.mcp.label"))
                    .description(prose(t!("cli_ui.mcp.hint")))
                    .child(self.mcp.clone()),
            )
            .child(Self::section(t!("cli_ui.section.rules"), cx))
            .child(Self::hint(t!("cli_ui.rules.hint"), cx))
            .child(self.render_approval(cx))
            .child(self.render_builtin(cx))
            .child(self.render_custom(cx))
            .child(self.render_test(cx))
    }
}
