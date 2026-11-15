//! 编辑自定义配色：名称、前景、背景、光标、选区与 16 色，每种颜色是色块（打开取色器）加十六进制输入框，
//! 右侧实时预览并标出看不清的颜色。内置配色不能改，先“复制为自定义”。
//!
//! 编辑的结果交给打开它的一方（设置中的配色库，那里也是草稿，保存设置时才写入）。输入框中写的不是颜色时标红、
//! “确定”不可用；预览只在输入完整的六位颜色时更新（`#abc` 可能是 `#abcdef` 输入到一半），离开输入框时按
//! 输入框中的写法补全（`#abc`、`rgb(…)` 等导入时也认的写法）。

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::model::ColorScheme;
use termlet_core::schemes::{self, MAX_SCHEME_NAME, is_valid_scheme, unique_name};

use super::{SampleHighlights, SchemeSample, ansi_name, hex_of, hsla, unreadable_names};
use crate::prose::prose;
use crate::t;

/// 打开编辑器要给的东西。
pub struct EditorOptions {
    pub scheme: ColorScheme,
    /// 复制来的新配色：标题为“复制为自定义配色”
    pub is_new: bool,
    /// 其他配色的名称（内置与自定义，不含正在编辑的这套）：同名时与导入一样保存为“名称 (1)”
    pub taken: Vec<String>,
    pub highlights: Rc<SampleHighlights>,
    pub font_family: SharedString,
    /// 按“确定”时调用，参数是编辑好的配色（名称已去重）
    pub on_save: Rc<dyn Fn(ColorScheme, &mut Window, &mut App)>,
}

/// 打开配色编辑器（在设置对话框之上再开一层对话框）。
pub fn open(options: EditorOptions, window: &mut Window, cx: &mut App) {
    let is_new = options.is_new;
    let editor = cx.new(|cx| SchemeEditor::new(options, window, cx));
    let width = rems(52.).to_pixels(window.rem_size());
    let first_focus = std::rc::Rc::new(std::cell::Cell::new(true));
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_focus.replace(false) {
            editor.read(cx).name.focus_handle(cx).focus(window, cx);
        }
        let submit = editor.clone();
        dialog
            .title(if is_new {
                t!("schemes.editor.copy_title")
            } else {
                t!("schemes.editor.edit_title")
            })
            .w(width)
            .overlay_closable(false)
            // 输入框中按 Enter：可以保存时保存并关闭
            .on_ok(move |_, window, cx| submit.update(cx, |editor, cx| editor.submit(window, cx)))
            .child(editor.clone())
    });
}

/// 配色中的一项颜色。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Foreground,
    Background,
    Cursor,
    CursorAccent,
    SelectionBackground,
    SelectionForeground,
    Ansi(usize),
}

impl Slot {
    const BASE: [Slot; 6] = [
        Slot::Foreground,
        Slot::Background,
        Slot::Cursor,
        Slot::CursorAccent,
        Slot::SelectionBackground,
        Slot::SelectionForeground,
    ];

    /// 留空时由终端决定：光标处的文字用背景色，选中的文字保持原来的颜色。
    fn optional(self) -> bool {
        matches!(self, Slot::CursorAccent | Slot::SelectionForeground)
    }

    fn label(self) -> SharedString {
        match self {
            Slot::Foreground => t!("schemes.field.foreground"),
            Slot::Background => t!("schemes.field.background"),
            Slot::Cursor => t!("schemes.field.cursor"),
            Slot::CursorAccent => t!("schemes.field.cursor_accent"),
            Slot::SelectionBackground => t!("schemes.field.selection_background"),
            Slot::SelectionForeground => t!("schemes.field.selection_foreground"),
            Slot::Ansi(i) => ansi_name(i),
        }
    }

    /// 输入框的 ID（UI 测试按它找）。
    fn id(self) -> SharedString {
        match self {
            Slot::Foreground => "scheme-color-foreground".into(),
            Slot::Background => "scheme-color-background".into(),
            Slot::Cursor => "scheme-color-cursor".into(),
            Slot::CursorAccent => "scheme-color-cursor-accent".into(),
            Slot::SelectionBackground => "scheme-color-selection-background".into(),
            Slot::SelectionForeground => "scheme-color-selection-foreground".into(),
            Slot::Ansi(i) => format!("scheme-ansi-{i}").into(),
        }
    }

    fn get(self, scheme: &ColorScheme) -> &str {
        match self {
            Slot::Foreground => &scheme.foreground,
            Slot::Background => &scheme.background,
            Slot::Cursor => &scheme.cursor,
            Slot::CursorAccent => &scheme.cursor_accent,
            Slot::SelectionBackground => &scheme.selection_background,
            Slot::SelectionForeground => &scheme.selection_foreground,
            Slot::Ansi(i) => schemes::ansi_color(scheme, i),
        }
    }

    fn set(self, scheme: &mut ColorScheme, value: String) {
        match self {
            Slot::Foreground => scheme.foreground = value,
            Slot::Background => scheme.background = value,
            Slot::Cursor => scheme.cursor = value,
            Slot::CursorAccent => scheme.cursor_accent = value,
            Slot::SelectionBackground => scheme.selection_background = value,
            Slot::SelectionForeground => scheme.selection_foreground = value,
            Slot::Ansi(i) => {
                if let Some(slot) = scheme.ansi.get_mut(i) {
                    *slot = value;
                }
            }
        }
    }
}

/// 输入框中的写法转成小写的 `#rrggbb`：`#rgb`、`#rrggbb`（可以不带 `#`），以及导入时也认的 `rgb()`、`0x`、
/// 颜色名等；不是颜色时为 `None`。
fn parse(text: &str) -> Option<String> {
    schemes::normalize_hex(text).or_else(|| schemes::parse_color(text))
}

/// 输入到一半也不会误会的写法：完整的六位（可以不带 `#`）。
fn complete_hex(text: &str) -> Option<String> {
    let digits = text.trim().strip_prefix('#').unwrap_or(text.trim());
    (digits.len() == 6)
        .then(|| schemes::normalize_hex(digits))
        .flatten()
}

struct ColorRow {
    slot: Slot,
    input: Entity<InputState>,
    picker: Entity<ColorPickerState>,
}

struct SchemeEditor {
    taken: Vec<String>,
    highlights: Rc<SampleHighlights>,
    font_family: SharedString,
    on_save: Rc<dyn Fn(ColorScheme, &mut Window, &mut App)>,
    name: Entity<InputState>,
    rows: Vec<ColorRow>,
    /// 预览用的配色（名称不在这里更新）
    draft: ColorScheme,
    _subscriptions: Vec<Subscription>,
}

impl SchemeEditor {
    fn new(options: EditorOptions, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let EditorOptions {
            scheme,
            is_new: _,
            taken,
            highlights,
            font_family,
            on_save,
        } = options;
        let name = cx.new(|cx| InputState::new(window, cx).default_value(scheme.name.clone()));
        let mut subscriptions = vec![cx.subscribe(&name, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })];
        let slots = Slot::BASE.into_iter().chain((0..16).map(Slot::Ansi));
        let mut rows = Vec::new();
        for slot in slots {
            let value = slot.get(&scheme).to_string();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(if slot.optional() {
                        t!("schemes.editor.auto")
                    } else {
                        SharedString::default()
                    })
                    .default_value(value.clone())
            });
            let picker = cx.new(|cx| {
                let state = ColorPickerState::new(window, cx);
                if value.is_empty() {
                    state
                } else {
                    state.default_value(hsla(&value))
                }
            });
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this, input, event: &InputEvent, window, cx| {
                    this.on_input(slot, input, event, window, cx)
                },
            ));
            subscriptions.push(cx.subscribe_in(
                &picker,
                window,
                move |this, _, event: &ColorPickerEvent, window, cx| {
                    let ColorPickerEvent::Change(Some(color)) = event else {
                        return;
                    };
                    this.set_color(slot, hex_of(*color), true, window, cx);
                },
            ));
            rows.push(ColorRow {
                slot,
                input,
                picker,
            });
        }
        Self {
            taken,
            highlights,
            font_family,
            on_save,
            name,
            rows,
            draft: scheme,
            _subscriptions: subscriptions,
        }
    }

    fn row(&self, slot: Slot) -> Option<&ColorRow> {
        self.rows.iter().find(|r| r.slot == slot)
    }

    /// 输入框改了：完整的颜色才更新预览与色块；离开输入框时按输入框中的写法补全。
    fn on_input(
        &mut self,
        slot: Slot,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = input.read(cx).value().to_string();
        match event {
            InputEvent::Change => {
                if let Some(hex) = complete_hex(&text) {
                    self.set_color(slot, hex, false, window, cx);
                } else if slot.optional() && text.trim().is_empty() {
                    slot.set(&mut self.draft, String::new());
                }
                cx.notify();
            }
            InputEvent::Blur => {
                if let Some(hex) = parse(&text)
                    && hex != text
                {
                    self.set_color(slot, hex, true, window, cx);
                }
            }
            _ => {}
        }
    }

    /// 一项颜色定下来了：预览与色块跟着变，`write_input` 时把规范的写法写回输入框。
    fn set_color(
        &mut self,
        slot: Slot,
        hex: String,
        write_input: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.row(slot) else {
            return;
        };
        let (input, picker) = (row.input.clone(), row.picker.clone());
        if write_input {
            input.update(cx, |state, cx| state.set_value(hex.clone(), window, cx));
        }
        let color = hsla(&hex);
        if picker.read(cx).value().map(hex_of).as_deref() != Some(hex.as_str()) {
            picker.update(cx, |state, cx| state.set_value(color, window, cx));
        }
        slot.set(&mut self.draft, hex);
        cx.notify();
    }

    fn name_text(&self, cx: &App) -> String {
        self.name
            .read(cx)
            .value()
            .trim()
            .chars()
            .take(MAX_SCHEME_NAME)
            .collect()
    }

    /// 输入框中的写法不合法：必填的为空，或写的不是颜色。
    fn is_invalid(row: &ColorRow, cx: &App) -> bool {
        let text = row.input.read(cx).value();
        if text.trim().is_empty() {
            !row.slot.optional()
        } else {
            parse(&text).is_none()
        }
    }

    /// 按输入框中的写法得出要保存的配色；有不合法的输入时为 `None`（“确定”不可用）。
    fn result(&self, cx: &App) -> Option<ColorScheme> {
        let name = self.name_text(cx);
        if name.is_empty() {
            return None;
        }
        let mut out = self.draft.clone();
        out.name = unique_name(&name, self.taken.iter().map(String::as_str));
        for row in &self.rows {
            let text = row.input.read(cx).value();
            let value = if text.trim().is_empty() {
                if !row.slot.optional() {
                    return None;
                }
                String::new()
            } else {
                parse(&text)?
            };
            row.slot.set(&mut out, value);
        }
        is_valid_scheme(&out).then_some(out)
    }

    /// 保存；可以保存时返回 true（关闭对话框）。
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(scheme) = self.result(cx) else {
            cx.notify();
            return false;
        };
        (self.on_save)(scheme, window, cx);
        true
    }

    fn render_color(
        &self,
        row: &ColorRow,
        featured: &[Hsla],
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let label = row.slot.label();
        let invalid = Self::is_invalid(row, cx);
        v_flex()
            .min_w_0()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(label.clone()),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        ColorPicker::new(&row.picker)
                            .small()
                            .featured_colors(featured.to_vec())
                            .accessibility_label(label),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&row.input)
                                .small()
                                .id(row.slot.id())
                                .when(invalid, |this| this.border_color(theme.danger)),
                        ),
                    ),
            )
    }
}

impl Render for SchemeEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let name = self.name_text(cx);
        let final_name = unique_name(&name, self.taken.iter().map(String::as_str));
        let renamed = !name.is_empty() && final_name != name;
        let result = self.result(cx);
        let error = if result.is_some() {
            None
        } else if name.is_empty() {
            Some(t!("schemes.editor.name_required"))
        } else {
            Some(t!("schemes.editor.invalid_colors"))
        };
        // 取色器中常用的颜色：这套配色自己的前景、背景与 8 种普通色
        let featured: Vec<Hsla> = [&self.draft.foreground, &self.draft.background]
            .into_iter()
            .chain(self.draft.ansi.iter().take(8))
            .filter(|c| !c.is_empty())
            .map(|c| hsla(c))
            .collect();
        let base = Slot::BASE
            .iter()
            .filter_map(|slot| self.row(*slot))
            .map(|row| self.render_color(row, &featured, cx).into_any_element())
            .collect::<Vec<_>>();
        let ansi = (0..16)
            .filter_map(|i| self.row(Slot::Ansi(i)))
            .map(|row| self.render_color(row, &featured, cx).into_any_element())
            .collect::<Vec<_>>();
        let unreadable = unreadable_names(&self.draft)
            .map(|colors| t!("schemes.editor.unreadable", colors = colors));
        // 最高 36rem，窗口矮时缩到窗口内，表单在里面滚动
        let height = rems(36.)
            .to_pixels(window.rem_size())
            .min(window.viewport_size().height - rems(10.).to_pixels(window.rem_size()));
        v_flex()
            .id("scheme-editor")
            .max_h(height)
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(prose(t!("schemes.editor.hint"))),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .gap_4()
                    .child(
                        v_flex()
                            .id("scheme-editor-fields")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .pr_1()
                            .gap_3()
                            .child(
                                v_form().child(
                                    field()
                                        .label(t!("schemes.editor.name"))
                                        .when(renamed, |this| {
                                            this.description(prose(t!(
                                                "schemes.editor.name_taken",
                                                name = final_name
                                            )))
                                        })
                                        .child(Input::new(&self.name).id("scheme-name")),
                                ),
                            )
                            .child(div().grid().grid_cols(3).gap_2().children(base))
                            .child(div().grid().grid_cols(4).gap_2().children(ansi)),
                    )
                    .child(
                        v_flex()
                            .w(rems(19.))
                            .flex_none()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(t!("schemes.editor.preview")),
                            )
                            .child(
                                div()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(theme.border)
                                    .overflow_hidden()
                                    .child(SchemeSample::new(
                                        &self.draft,
                                        &self.highlights,
                                        &self.font_family,
                                    )),
                            )
                            .when_some(unreadable, |this, text| {
                                this.child(
                                    div()
                                        .id("scheme-editor-unreadable")
                                        .text_xs()
                                        .text_color(theme.warning)
                                        .child(prose(text)),
                                )
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(theme.danger)
                            .when_some(error, |this, error| this.child(prose(error))),
                    )
                    .child(
                        Button::new("scheme-editor-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("scheme-editor-save")
                            .primary()
                            .label(t!("schemes.editor.done"))
                            .disabled(result.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.submit(window, cx) {
                                    window.close_dialog(cx);
                                }
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_typed_halfway_are_not_previewed() {
        assert_eq!(complete_hex("#ABCDEF").as_deref(), Some("#abcdef"));
        assert_eq!(complete_hex("abcdef").as_deref(), Some("#abcdef"));
        assert_eq!(complete_hex("#abc"), None);
        // 离开输入框时认更多写法
        assert_eq!(parse("#abc").as_deref(), Some("#aabbcc"));
        assert_eq!(parse("rgb(255, 0, 0)").as_deref(), Some("#ff0000"));
        assert_eq!(parse("not a color"), None);
    }
}
