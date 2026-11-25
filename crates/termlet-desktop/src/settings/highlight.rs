//! 设置中的“关键词高亮”：开关、规则列表（关键词或正则、颜色）与预览。
//!
//! 规则没有 ID，界面上每一行用一个递增的编号区分。关键词为空的规则保存时去掉；有无效的正则时不能保存。
//! “添加预设规则”列出核心的 [`HIGHLIGHT_PRESETS`]：按预设的先后插入（不是加在最后），已在列表中的打勾、不能再加；
//! 是预设的行在关键词前标出预设的名称。
//!
//! 颜色（[`HighlightColor`]）在下拉菜单中选：配色中自动、深色、亮色三组各六种，或“自定义颜色…”（gpui-kit 的取色器，
//! 存成 `#rrggbb`，之后点色块调整）。色块、菜单中的颜色与预览都是当前配色上实际显示的样子（自定义的颜色对背景
//! 不够清楚时调整过）。不认识的写法（以后的版本写的）按红色显示，没改这一行的颜色时原样保存。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, Subscription, TestSupportExt as _,
    Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::model::{
    ColorScheme, Extra, HIGHLIGHT_PRESETS, HighlightPreset, HighlightRule, HighlightSettings,
    highlight_preset_of, preset_insert_position,
};
use termlet_core::schemes::{self, HighlightColor, HighlightPalette};
use termlet_terminal::colors::to_hsla;

use crate::app_state::AppState;
use crate::prose::prose;
use crate::schemes::{hex_of, hsla};
use crate::{t, terminal_colors};

/// 规则最多这么多条。
const MAX_RULES: usize = 50;
/// 预览的默认文字：覆盖默认规则（时间、数字也能演示对应的预设）。
const SAMPLE: &str = "12:00:01 ERROR connect 10.0.0.12:5432 refused · 12:00:02 WARN retry 1/3 via [2001:db8::1]:22 · 12:00:03 OK connected";
/// 颜色一列的宽度（rem）：色块与下拉按钮。
const COLOR_WIDTH: f32 = 8.5;

struct RuleRow {
    key: u64,
    pattern: Entity<InputState>,
    regex: bool,
    case_sensitive: bool,
    /// 设置中的写法（见 [`HighlightColor`]）；不认识的原样留着，改了颜色才换
    color: String,
    /// 自定义颜色的取色器（颜色是自定义的时候显示在色块的位置）
    picker: Entity<ColorPickerState>,
    enabled: bool,
    /// 规则中以后的版本加的字段，原样保存
    extra: Extra,
    _subscriptions: [Subscription; 2],
}

impl RuleRow {
    /// 显示的颜色：不认识的写法按红色（与终端相同）。
    fn shown_color(&self) -> HighlightColor {
        HighlightColor::from_name(&self.color).unwrap_or(HighlightColor::Red)
    }
}

pub(super) struct HighlightPage {
    enabled: bool,
    rules: Vec<RuleRow>,
    next_key: u64,
    sample: Entity<InputState>,
    /// 预览用的终端配色：设置中（尚未保存的）当前深浅的配色，由设置在切到这一页时给出
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
    /// 示例文字中的匹配（字节区间与颜色）
    matches: Vec<(std::ops::Range<usize>, HighlightColor)>,
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
            this.insert_rule(this.rules.len(), rule, window, cx);
        }
        this
    }

    /// 在第 `index` 行插入一条规则。
    fn insert_rule(
        &mut self,
        index: usize,
        rule: &HighlightRule,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.next_key;
        let pattern = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("settings.highlight.pattern_placeholder"))
                .default_value(rule.pattern.clone())
        });
        let picker = cx.new(|cx| {
            let state = ColorPickerState::new(window, cx);
            match HighlightColor::from_name(&rule.color) {
                Some(HighlightColor::Custom(rgb)) => state.default_value(hsla(&rgb.to_hex())),
                _ => state,
            }
        });
        let subscriptions = [
            cx.subscribe(&pattern, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            // 取色器中选了颜色（拖动滑块时连续地来）：这一行用这个自定义的颜色
            cx.subscribe(&picker, move |this, _, event: &ColorPickerEvent, cx| {
                if let ColorPickerEvent::Change(Some(color)) = event
                    && let Some(row) = this.row_mut(key)
                {
                    row.color = hex_of(*color);
                    cx.notify();
                }
            }),
        ];
        self.rules.insert(
            index.min(self.rules.len()),
            RuleRow {
                key,
                pattern,
                regex: rule.regex,
                case_sensitive: rule.case_sensitive,
                color: rule.color.clone(),
                picker,
                enabled: rule.enabled,
                extra: rule.extra.clone(),
                _subscriptions: subscriptions,
            },
        );
        self.next_key += 1;
    }

    /// 添加一条预设：插在第一条顺序在它后面的预设之前（见 [`preset_insert_position`]）。
    fn add_preset(
        &mut self,
        preset: &HighlightPreset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rules.len() >= MAX_RULES {
            return;
        }
        let rules: Vec<HighlightRule> = self.rules.iter().map(|row| self.rule(row, cx)).collect();
        if rules.iter().any(|rule| preset.matches(rule)) {
            return;
        }
        let index = preset_insert_position(&rules, preset);
        self.insert_rule(index, &preset.rule(), window, cx);
        termlet_core::usage::mark(termlet_core::usage::Feature::HighlightPresets);
        cx.notify();
    }

    fn rule(&self, row: &RuleRow, cx: &App) -> HighlightRule {
        HighlightRule {
            pattern: row.pattern.read(cx).value().to_string(),
            regex: row.regex,
            case_sensitive: row.case_sensitive,
            color: row.color.clone(),
            enabled: row.enabled,
            extra: row.extra.clone(),
        }
    }

    /// 第 `key` 行改用配色中的一种颜色。
    fn choose_color(&mut self, key: u64, color: HighlightColor, cx: &mut Context<Self>) {
        if let Some(row) = self.row_mut(key) {
            row.color = color.name().into_owned();
        }
        cx.notify();
    }

    /// 第 `key` 行选了“自定义颜色…”：还不是自定义的颜色时从现在显示的颜色（`start`，`#rrggbb`）开始，打开取色器，
    /// 焦点放在色值框中（可以直接输入色值、回车）。
    fn choose_custom(
        &mut self,
        key: u64,
        start: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.row_mut(key) else {
            return;
        };
        if !matches!(row.shown_color(), HighlightColor::Custom(_)) {
            row.color = start.to_string();
            row.picker
                .update(cx, |picker, cx| picker.set_value(hsla(start), window, cx));
        }
        row.picker
            .update(cx, |picker, cx| picker.set_open(true, cx));
        // 菜单关上时把焦点还给下拉按钮：等它还完再把焦点放进色值框
        let hex = row.picker.read(cx).hex_input().clone();
        window.defer(cx, move |window, cx| {
            hex.update(cx, |input, cx| input.focus(window, cx));
        });
        cx.notify();
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
        // 规则改过的记下默认规则的版本：之后读取时不再把它当作没改过的旧默认规则升级
        if settings.rules != rules {
            settings.rules_version = Some(HighlightSettings::RULES_VERSION);
        }
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

    /// 一行规则；`preset` 是这一行对应的预设的名称（不是预设时为 `None`），标在关键词前。
    fn render_rule(
        &self,
        row: &RuleRow,
        error: Option<String>,
        preset: Option<SharedString>,
        colors: &HighlightPalette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let key = row.key;
        let muted = cx.theme().muted_foreground;
        let id = |name: &'static str| ElementId::from((name, key as usize));
        let pattern = Input::new(&row.pattern)
            .small()
            .id(id("highlight-pattern"))
            .when_some(preset, |input, name| {
                input.prefix(
                    div()
                        .id(id("highlight-preset-name"))
                        .test_support()
                        .aria_label(name.clone())
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child(name),
                )
            });
        let color = self.render_color(row, colors, cx);
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
                    .child(div().flex_1().min_w_0().child(pattern))
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
                    .child(color)
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

    /// 颜色一列：色块（自定义的颜色是取色器，点它调整）与下拉菜单（配色中自动、深色、亮色三组各六种，分隔线隔开、
    /// 每组标出组名，最后是“自定义颜色…”；色块都是当前配色上实际显示的样子）。
    fn render_color(
        &self,
        row: &RuleRow,
        colors: &HighlightPalette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let key = row.key;
        let shown = row.shown_color();
        let custom = matches!(shown, HighlightColor::Custom(_));
        let current = colors.get(shown);
        let swatches: Vec<(HighlightColor, Hsla)> = HighlightColor::NAMED
            .into_iter()
            .map(|color| (color, hsla(&colors.get(color))))
            .collect();
        let swatch = if custom {
            // 取色器顶上一排（放得下 12 个）：深色、亮色两组
            ColorPicker::new(&row.picker)
                .xsmall()
                .featured_colors(swatches[6..].iter().map(|(_, c)| *c).collect())
                .accessibility_label(t!("settings.highlight.custom"))
                .into_any_element()
        } else {
            div()
                .size_3()
                .rounded_full()
                .bg(hsla(&current))
                .into_any_element()
        };
        let page = cx.entity().downgrade();
        let menu = Button::new(("highlight-color", key as usize))
            .outline()
            .small()
            .w_full()
            .label(color_title(shown))
            .dropdown_caret(true)
            .dropdown_menu(move |menu, window, _| {
                // 一共 25 行：窗口矮时可以滚动
                let max = window.bounds().size.height - window.rem_size() * 6.;
                let menu = menu
                    .scrollable(true)
                    .max_h(max.max(window.rem_size() * 16.));
                let groups = [
                    t!("settings.highlight.group_auto"),
                    t!("settings.highlight.group_dark"),
                    t!("settings.highlight.group_bright"),
                ];
                let named = swatches.chunks(6).zip(groups).enumerate().fold(
                    menu,
                    |menu, (i, (group, title))| {
                        let menu = if i == 0 { menu } else { menu.separator() };
                        group
                            .iter()
                            .fold(menu.label(title), |menu, &(color, swatch)| {
                                let page = page.clone();
                                menu.item(
                                    color_item(color, swatch).checked(color == shown).on_click(
                                        move |_, _, cx| {
                                            let _ = page.update(cx, |page, cx| {
                                                page.choose_color(key, color, cx)
                                            });
                                        },
                                    ),
                                )
                            })
                    },
                );
                let (page, start) = (page.clone(), current.clone());
                named.separator().item(
                    PopupMenuItem::new(t!("settings.highlight.custom_color"))
                        .checked(custom)
                        .on_click(move |_, window, cx| {
                            let _ = page
                                .update(cx, |page, cx| page.choose_custom(key, &start, window, cx));
                        }),
                )
            });
        h_flex()
            .w(rems(COLOR_WIDTH))
            .gap_1p5()
            .child(
                div()
                    .w_4()
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(swatch),
            )
            .child(div().flex_1().min_w_0().child(menu))
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
            .map(|(range, color)| {
                (
                    range.clone(),
                    HighlightStyle {
                        color: Some(hsla(&colors.get(*color))),
                        ..Default::default()
                    },
                )
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
            .child(
                div()
                    .w(rems(COLOR_WIDTH))
                    .child(t!("settings.highlight.color")),
            )
            .child(div().w(rems(1.5)));
        let (errors, presets): (Vec<Option<String>>, Vec<Option<&'static HighlightPreset>>) = self
            .memo
            .as_ref()
            .map(|m| {
                (
                    m.errors.clone(),
                    m.rules.iter().map(highlight_preset_of).collect(),
                )
            })
            .unwrap_or_default();
        let rules = self
            .rules
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let error = errors.get(i).cloned().flatten();
                let preset = presets.get(i).copied().flatten().map(preset_title);
                self.render_rule(row, error, preset, &colors, cx)
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let page = cx.entity().downgrade();
        let add_preset = Button::new("highlight-add-preset")
            .outline()
            .small()
            .icon(IconName::Plus)
            .label(t!("settings.highlight.add_preset"))
            .dropdown_caret(true)
            .disabled(full)
            .dropdown_menu(move |menu, _, _| {
                HIGHLIGHT_PRESETS.iter().fold(menu, |menu, preset| {
                    let added = presets.contains(&Some(preset));
                    let page = page.clone();
                    menu.item(
                        PopupMenuItem::new(preset_title(preset))
                            .checked(added)
                            .disabled(added || full)
                            .on_click(move |_, window, cx| {
                                let _ =
                                    page.update(cx, |page, cx| page.add_preset(preset, window, cx));
                            }),
                    )
                })
            });
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
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("highlight-add")
                                    .outline()
                                    .small()
                                    .icon(IconName::Plus)
                                    .label(t!("settings.highlight.add"))
                                    .disabled(full)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let rule = HighlightRule {
                                            color: HighlightColor::Red.name().into(),
                                            enabled: true,
                                            ..Default::default()
                                        };
                                        this.insert_rule(this.rules.len(), &rule, window, cx);
                                        if let Some(row) = this.rules.last() {
                                            row.pattern.update(cx, |s, cx| s.focus(window, cx));
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(add_preset),
                    ),
            )
            .child(self.render_preview(&scheme, &colors, cx))
    }
}

/// 预设的名称（菜单、规则行前的标注）。
fn preset_title(preset: &HighlightPreset) -> SharedString {
    match preset.id {
        "url" => t!("settings.highlight.preset.url"),
        "errors" => t!("settings.highlight.preset.errors"),
        "warnings" => t!("settings.highlight.preset.warnings"),
        "success" => t!("settings.highlight.preset.success"),
        "ipv6" => t!("settings.highlight.preset.ipv6"),
        "ipv4" => t!("settings.highlight.preset.ipv4"),
        "mac" => t!("settings.highlight.preset.mac"),
        "uuid" => t!("settings.highlight.preset.uuid"),
        "datetime" => t!("settings.highlight.preset.datetime"),
        _ => t!("settings.highlight.preset.number"),
    }
}

/// 颜色菜单中的一项：色块与名称。
fn color_item(color: HighlightColor, swatch: Hsla) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| {
        h_flex()
            .gap_2()
            .child(div().size_3().flex_none().rounded_full().bg(swatch))
            .child(color_title(color))
    })
}

/// 颜色的名称（下拉按钮与菜单）；自定义的颜色写“自定义”（色块的提示中有色值）。
fn color_title(color: HighlightColor) -> SharedString {
    match color {
        HighlightColor::Red => t!("settings.color.red"),
        HighlightColor::Green => t!("settings.color.green"),
        HighlightColor::Yellow => t!("settings.color.yellow"),
        HighlightColor::Blue => t!("settings.color.blue"),
        HighlightColor::Magenta => t!("settings.color.magenta"),
        HighlightColor::Cyan => t!("settings.color.cyan"),
        HighlightColor::DarkRed => t!("settings.color.dark_red"),
        HighlightColor::DarkGreen => t!("settings.color.dark_green"),
        HighlightColor::DarkYellow => t!("settings.color.dark_yellow"),
        HighlightColor::DarkBlue => t!("settings.color.dark_blue"),
        HighlightColor::DarkMagenta => t!("settings.color.dark_magenta"),
        HighlightColor::DarkCyan => t!("settings.color.dark_cyan"),
        HighlightColor::BrightRed => t!("settings.color.bright_red"),
        HighlightColor::BrightGreen => t!("settings.color.bright_green"),
        HighlightColor::BrightYellow => t!("settings.color.bright_yellow"),
        HighlightColor::BrightBlue => t!("settings.color.bright_blue"),
        HighlightColor::BrightMagenta => t!("settings.color.bright_magenta"),
        HighlightColor::BrightCyan => t!("settings.color.bright_cyan"),
        HighlightColor::Custom(_) => t!("settings.highlight.custom"),
    }
}

/// 无效的规则返回正则的错误信息；关键词为空不算错误（保存时去掉）。与终端用同一套规则。
fn rule_error(rule: &HighlightRule) -> Option<String> {
    termlet_terminal::rule_error(&terminal_colors::rule_spec(rule))
}

/// 示例文字中的匹配（字节区间与颜色），与终端中的匹配相同。
fn preview_matches(
    text: &str,
    rules: &[HighlightRule],
) -> Vec<(std::ops::Range<usize>, HighlightColor)> {
    let specs: Vec<_> = rules.iter().map(terminal_colors::rule_spec).collect();
    let (highlighter, _) = termlet_terminal::Highlighter::new(&specs);
    highlighter
        .find_matches(text)
        .into_iter()
        .map(|m| (m.range, terminal_colors::core_color(m.color)))
        .collect()
}

#[cfg(test)]
mod tests;
