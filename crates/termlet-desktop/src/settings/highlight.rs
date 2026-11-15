//! 设置中的“关键词高亮”：开关、规则列表（关键词或正则、颜色）与预览。
//!
//! 规则没有 ID，界面上每一行用一个递增的编号区分。关键词为空的规则保存时去掉；有无效的正则时不能保存。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::Select;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    StyledText, Subscription, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::model::{ColorScheme, HighlightRule, HighlightSettings};
use termlet_core::schemes::{self, HighlightPalette};
use termlet_terminal::colors::to_hsla;

use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::prose::prose;
use crate::{t, terminal_colors};

/// 规则最多这么多条。
const MAX_RULES: usize = 50;
/// 预览的默认文字：覆盖内置规则。
const SAMPLE: &str = "12:00:01 ERROR connect 10.0.0.12:5432 refused · 12:00:02 WARN retry 1/3 · 12:00:03 OK connected";
/// 可选的颜色（终端配色中的颜色名）。
const COLORS: [&str; 6] = ["red", "green", "yellow", "blue", "magenta", "cyan"];

struct RuleRow {
    key: u64,
    pattern: Entity<InputState>,
    regex: bool,
    case_sensitive: bool,
    color: Entity<ChoiceState<String>>,
    enabled: bool,
    _subscriptions: [Subscription; 2],
}

pub(super) struct HighlightPage {
    enabled: bool,
    rules: Vec<RuleRow>,
    next_key: u64,
    sample: Entity<InputState>,
    /// 预览用的终端配色：设置中（尚未保存的）当前深浅的配色，由设置对话框在切到这一页时给出
    scheme: Option<ColorScheme>,
    /// 上一次渲染时检查、匹配的结果：规则与示例文字没变时不重新编译正则
    memo: Option<Memo>,
    _sample_subscription: Subscription,
}

/// 规则的检查结果与预览的匹配。
struct Memo {
    enabled: bool,
    rules: Vec<HighlightRule>,
    sample: String,
    /// 与 `rules` 一一对应：无效的规则的错误信息
    errors: Vec<Option<String>>,
    /// 示例文字中的匹配（字节区间与颜色名）
    matches: Vec<(std::ops::Range<usize>, &'static str)>,
}

impl HighlightPage {
    pub(super) fn new(
        settings: &HighlightSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sample = cx.new(|cx| InputState::new(window, cx).default_value(SAMPLE));
        let sample_subscription = cx.observe(&sample, |_, _, cx| cx.notify());
        let mut this = Self {
            enabled: settings.enabled,
            rules: Vec::new(),
            next_key: 0,
            sample,
            scheme: None,
            memo: None,
            _sample_subscription: sample_subscription,
        };
        for rule in &settings.rules {
            this.push_rule(rule, window, cx);
        }
        this
    }

    fn push_rule(&mut self, rule: &HighlightRule, window: &mut Window, cx: &mut Context<Self>) {
        let pattern = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("settings.highlight.pattern_placeholder"))
                .default_value(rule.pattern.clone())
        });
        let colors = COLORS
            .iter()
            .map(|name| Choice::new(color_title(name), name.to_string()))
            .collect();
        let color = choice_state(colors, &rule.color, false, window, cx);
        let subscriptions = [
            cx.subscribe(&pattern, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.observe(&color, |_, _, cx| cx.notify()),
        ];
        self.rules.push(RuleRow {
            key: self.next_key,
            pattern,
            regex: rule.regex,
            case_sensitive: rule.case_sensitive,
            color,
            enabled: rule.enabled,
            _subscriptions: subscriptions,
        });
        self.next_key += 1;
    }

    fn rule(&self, row: &RuleRow, cx: &App) -> HighlightRule {
        HighlightRule {
            pattern: row.pattern.read(cx).value().to_string(),
            regex: row.regex,
            case_sensitive: row.case_sensitive,
            color: chosen(&row.color, cx).unwrap_or_else(|| "red".into()),
            enabled: row.enabled,
        }
    }

    /// 预览改用这套配色（设置中尚未保存的当前配色）。
    pub(super) fn set_scheme(&mut self, scheme: ColorScheme, cx: &mut Context<Self>) {
        if self.scheme.as_ref() != Some(&scheme) {
            self.scheme = Some(scheme);
            cx.notify();
        }
    }

    /// 写进设置；有无效的正则、超过条数上限时返回错误。
    pub(super) fn apply(
        &self,
        settings: &mut HighlightSettings,
        cx: &App,
    ) -> Result<(), SharedString> {
        let rules: Vec<HighlightRule> = self
            .rules
            .iter()
            .map(|row| self.rule(row, cx))
            .filter(|rule| !rule.pattern.trim().is_empty())
            .collect();
        if rules.iter().any(|rule| rule_error(rule).is_some()) {
            return Err(t!("settings.highlight.fix"));
        }
        // 终端只用前 MAX_RULES 条（同步来的可能更多）：多出的保存了也不生效
        if rules.len() > MAX_RULES {
            return Err(t!("settings.highlight.too_many", max = MAX_RULES));
        }
        settings.enabled = self.enabled;
        settings.rules = rules;
        Ok(())
    }

    fn row_mut(&mut self, key: u64) -> Option<&mut RuleRow> {
        self.rules.iter_mut().find(|r| r.key == key)
    }

    /// 检查规则、匹配示例文字；规则与示例文字没变时沿用上一次的结果。
    fn refresh_memo(&mut self, cx: &App) {
        let rules: Vec<HighlightRule> = self.rules.iter().map(|row| self.rule(row, cx)).collect();
        let sample = self.sample.read(cx).value().to_string();
        if let Some(memo) = &self.memo
            && memo.enabled == self.enabled
            && memo.rules == rules
            && memo.sample == sample
        {
            return;
        }
        let errors = rules.iter().map(rule_error).collect();
        let matches = if self.enabled {
            preview_matches(&sample, &rules)
        } else {
            Vec::new()
        };
        self.memo = Some(Memo {
            enabled: self.enabled,
            rules,
            sample,
            errors,
            matches,
        });
    }

    fn render_rule(
        &self,
        row: &RuleRow,
        error: Option<String>,
        colors: &HighlightPalette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let key = row.key;
        let color = chosen(&row.color, cx).unwrap_or_else(|| "red".into());
        let swatch = highlight_color(colors, &color).unwrap_or(cx.theme().muted_foreground);
        let id = |name: &'static str| ElementId::from((name, key as usize));
        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Checkbox::new(id("highlight-enabled"))
                            .checked(row.enabled)
                            .tooltip(t!("settings.highlight.rule_enabled"))
                            .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                                if let Some(row) = this.row_mut(key) {
                                    row.enabled = *checked;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&row.pattern).small().id(id("highlight-pattern"))),
                    )
                    .child(
                        div().w(rems(3.5)).flex().justify_center().child(
                            Checkbox::new(id("highlight-regex"))
                                .checked(row.regex)
                                .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                                    if let Some(row) = this.row_mut(key) {
                                        row.regex = *checked;
                                    }
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        div().w(rems(4.5)).flex().justify_center().child(
                            Checkbox::new(id("highlight-case"))
                                .checked(row.case_sensitive)
                                .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                                    if let Some(row) = this.row_mut(key) {
                                        row.case_sensitive = *checked;
                                    }
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        h_flex()
                            .w(rems(7.))
                            .gap_1p5()
                            .child(div().size_3().flex_none().rounded_full().bg(swatch))
                            .child(
                                div().flex_1().child(
                                    Select::new(&row.color).small().id(id("highlight-color")),
                                ),
                            ),
                    )
                    .child(
                        Button::new(id("highlight-delete"))
                            .ghost()
                            .small()
                            .icon(IconName::Trash)
                            .tooltip(t!("settings.highlight.delete"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.rules.retain(|r| r.key != key);
                                cx.notify();
                            })),
                    ),
            )
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .pl_8()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(prose(t!("settings.highlight.invalid", message = error))),
                )
            })
    }

    /// 预览：按当前（未保存）的规则给示例文字着色，背景与颜色取设置中当前深浅的终端配色，
    /// 高亮色与终端中相同（对背景对比度不够时调整过的）。
    fn render_preview(
        &self,
        scheme: &ColorScheme,
        colors: &HighlightPalette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let palette = terminal_colors::palette_from(scheme);
        let (text, matches) = match &self.memo {
            Some(memo) => (SharedString::from(memo.sample.clone()), &memo.matches[..]),
            None => (SharedString::default(), &[][..]),
        };
        let highlights = matches
            .iter()
            .filter_map(|(range, color)| {
                Some((
                    range.clone(),
                    HighlightStyle {
                        color: Some(highlight_color(colors, color)?),
                        ..Default::default()
                    },
                ))
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .child(prose(t!("settings.highlight.preview"))),
            )
            .child(Input::new(&self.sample).small())
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(cx.theme().radius)
                    .bg(to_hsla(palette.background))
                    .text_color(to_hsla(palette.foreground))
                    .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                    .text_sm()
                    .child(StyledText::new(text).with_highlights(highlights)),
            )
    }
}

impl Render for HighlightPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_memo(cx);
        let scheme = match &self.scheme {
            Some(scheme) => scheme.clone(),
            None => {
                let store = AppState::store(cx);
                terminal_colors::global_scheme(&store, &store.settings(), cx.theme().is_dark())
            }
        };
        let colors = schemes::highlight_palette(&scheme);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let full = self.rules.len() >= MAX_RULES;
        let header = h_flex()
            .gap_2()
            .text_xs()
            .text_color(muted)
            .child(div().w(rems(1.)))
            .child(div().flex_1().child(t!("settings.highlight.pattern")))
            .child(
                div()
                    .w(rems(3.5))
                    .text_center()
                    .child(t!("settings.highlight.regex")),
            )
            .child(
                div()
                    .w(rems(4.5))
                    .text_center()
                    .child(t!("settings.highlight.case")),
            )
            .child(div().w(rems(7.)).child(t!("settings.highlight.color")))
            .child(div().w(rems(1.5)));
        let errors = self
            .memo
            .as_ref()
            .map(|m| m.errors.clone())
            .unwrap_or_default();
        let rules = self
            .rules
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let error = errors.get(i).cloned().flatten();
                self.render_rule(row, error, &colors, cx).into_any_element()
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Checkbox::new("settings-highlight")
                            .label(t!("settings.highlight.enable"))
                            .checked(self.enabled)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                this.enabled = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .pl_6()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(t!("settings.highlight.hint"))),
                    ),
            )
            .child(
                v_flex()
                    .id("highlight-rules")
                    .gap_1p5()
                    .when(!self.enabled, |this| this.opacity(0.5))
                    .child(header)
                    .children(rules)
                    .child(
                        h_flex().child(
                            Button::new("highlight-add")
                                .outline()
                                .small()
                                .icon(IconName::Plus)
                                .label(t!("settings.highlight.add"))
                                .disabled(full)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let rule = HighlightRule {
                                        color: "red".into(),
                                        enabled: true,
                                        ..Default::default()
                                    };
                                    this.push_rule(&rule, window, cx);
                                    if let Some(row) = this.rules.last() {
                                        row.pattern.update(cx, |s, cx| s.focus(window, cx));
                                    }
                                    cx.notify();
                                })),
                        ),
                    ),
            )
            .child(self.render_preview(&scheme, &colors, cx))
    }
}

fn color_title(name: &str) -> SharedString {
    match name {
        "red" => t!("settings.color.red"),
        "green" => t!("settings.color.green"),
        "yellow" => t!("settings.color.yellow"),
        "blue" => t!("settings.color.blue"),
        "magenta" => t!("settings.color.magenta"),
        _ => t!("settings.color.cyan"),
    }
}

/// 无效的规则返回正则的错误信息；关键词为空不算错误（保存时去掉）。与终端用同一套规则。
fn rule_error(rule: &HighlightRule) -> Option<String> {
    termlet_terminal::rule_error(&terminal_colors::rule_spec(rule))
}

/// 示例文字中的匹配（字节区间与颜色名），与终端中的匹配相同。
fn preview_matches(
    text: &str,
    rules: &[HighlightRule],
) -> Vec<(std::ops::Range<usize>, &'static str)> {
    let specs: Vec<_> = rules.iter().map(terminal_colors::rule_spec).collect();
    let (highlighter, _) = termlet_terminal::Highlighter::new(&specs);
    highlighter
        .find_matches(text)
        .into_iter()
        .map(|m| (m.range, m.color.name()))
        .collect()
}

/// 高亮色（颜色名不认识时为 `None`）。
fn highlight_color(colors: &HighlightPalette, name: &str) -> Option<Hsla> {
    let color = schemes::HighlightColor::from_name(name)?;
    let rgb = schemes::Rgb::parse(colors.get(color))?;
    Some(to_hsla(termlet_terminal::Rgb {
        r: rgb.r,
        g: rgb.g,
        b: rgb.b,
    }))
}
