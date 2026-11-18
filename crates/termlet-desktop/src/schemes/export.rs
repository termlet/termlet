//! 导出终端配色：勾选的配色导出为 Windows Terminal 的 JSON（`schemes` 数组，一个文件可以放多套，最通用）
//! 或 iTerm2 的 `.itermcolors`（每个文件一套），在系统的保存对话框中选择位置。

use std::collections::HashSet;
use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Task, Window, div,
    prelude::FluentBuilder as _, rems,
};
use termlet_core::schemes::{to_iterm_colors, to_windows_terminal_json};

use super::{SchemeEntry, SchemeSwatch, mode_label};
use crate::prose::prose;
use crate::t;

/// 打开导出对话框：`entries` 是可以导出的配色（内置与自定义），打开时勾选 `initial` 中的。
pub fn open(entries: Vec<SchemeEntry>, initial: Vec<String>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|_| SchemeExport {
        entries,
        selected: initial.into_iter().collect(),
        format: Format::WindowsTerminal,
        busy: false,
        error: None,
        _save: None,
    });
    let width = rems(32.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, _, _| {
        // Enter 与点“导出”相同；写完文件后由 save 关闭对话框，这里总是返回 false
        let on_ok = {
            let view = view.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                view.update(cx, |view, cx| view.save(window, cx));
                false
            }
        };
        dialog
            .title(t!("schemes.export.title"))
            .w(width)
            .child(view.clone())
            .on_ok(on_ok)
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    WindowsTerminal,
    Iterm2,
}

struct SchemeExport {
    entries: Vec<SchemeEntry>,
    /// 勾选的配色 ID
    selected: HashSet<String>,
    format: Format,
    busy: bool,
    /// 写文件失败的原因（系统的错误信息）
    error: Option<String>,
    _save: Option<Task<()>>,
}

impl SchemeExport {
    fn chosen(&self) -> Vec<&SchemeEntry> {
        self.entries
            .iter()
            .filter(|e| self.selected.contains(&e.id))
            .collect()
    }

    /// 不能导出：没有勾选，或 iTerm2 格式勾选了多套。
    fn invalid(&self) -> bool {
        let count = self.chosen().len();
        count == 0 || (self.format == Format::Iterm2 && count > 1)
    }

    /// 选择保存位置并写入文件；成功后提示并关闭对话框。
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.invalid() {
            return;
        }
        let chosen = self.chosen();
        let count = chosen.len();
        let Some(first) = chosen.first() else {
            return;
        };
        let (name, text) = match self.format {
            Format::Iterm2 => (
                format!("{}.itermcolors", safe_name(&first.scheme.name)),
                to_iterm_colors(&first.scheme),
            ),
            Format::WindowsTerminal => (
                if count == 1 {
                    format!("{}.json", safe_name(&first.scheme.name))
                } else {
                    "termlet-color-schemes.json".to_string()
                },
                to_windows_terminal_json(chosen.iter().map(|e| &e.scheme)),
            ),
        };
        let dir = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let path = cx.prompt_for_new_path(&dir, Some(&name));
        self.busy = true;
        self.error = None;
        cx.notify();
        self._save = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = path.await else {
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    cx.notify();
                });
                return;
            };
            let target = path.clone();
            let written = cx
                .background_spawn(async move { std::fs::write(&target, text) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match written {
                Ok(()) => {
                    tracing::info!(count, path = %path.display(), "已导出终端配色");
                    crate::notify::show(
                        crate::notify::Notice::success(t!(
                            "schemes.export.done",
                            count = count,
                            path = path.display().to_string()
                        )),
                        window,
                        cx,
                    );
                    window.close_dialog(cx);
                }
                Err(e) => {
                    tracing::warn!("导出终端配色失败：{e}");
                    this.error = Some(e.to_string());
                    this.busy = false;
                    cx.notify();
                }
            });
        }));
    }
}

impl Render for SchemeExport {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let count = self.chosen().len();
        let hint = match self.format {
            Format::WindowsTerminal => t!("schemes.export.hint_windows_terminal"),
            Format::Iterm2 => t!("schemes.export.hint_iterm2"),
        };
        let rows = self.entries.iter().enumerate().map(|(ix, entry)| {
            let id = entry.id.clone();
            let checked = self.selected.contains(&id);
            h_flex()
                .gap_2()
                .px_2p5()
                .py_1p5()
                .text_xs()
                // 行之间的分隔线（第一行上面是列表的边框）
                .when(ix > 0, |this| this.border_t_1().border_color(theme.border))
                .child(
                    Checkbox::new(ElementId::Name(format!("scheme-export-{id}").into()))
                        .checked(checked)
                        .accessibility_label(entry.scheme.name.clone())
                        .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                            if *checked {
                                this.selected.insert(id.clone());
                            } else {
                                this.selected.remove(&id);
                            }
                            cx.notify();
                        })),
                )
                .child(SchemeSwatch::new(&entry.scheme))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(entry.scheme.name.clone()),
                )
                .child(div().text_color(muted).child(mode_label(entry.is_dark())))
        });
        let error = if self.format == Format::Iterm2 && count > 1 {
            Some(t!("schemes.export.one_only"))
        } else {
            self.error
                .clone()
                .map(|e| t!("schemes.export.failed", error = e))
        };
        v_flex()
            .id("scheme-export")
            .gap_3()
            .child(div().text_sm().text_color(muted).child(prose(hint)))
            .child(
                RadioGroup::horizontal("scheme-export-format")
                    .selected_index(Some(match self.format {
                        Format::WindowsTerminal => 0,
                        Format::Iterm2 => 1,
                    }))
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.format = if *ix == 1 {
                            Format::Iterm2
                        } else {
                            Format::WindowsTerminal
                        };
                        cx.notify();
                    }))
                    .child(
                        Radio::new("scheme-export-windows-terminal")
                            .label(t!("schemes.format.windows_terminal")),
                    )
                    .child(Radio::new("scheme-export-iterm2").label(t!("schemes.format.iterm2"))),
            )
            .child(
                v_flex()
                    .id("scheme-export-list")
                    .max_h(rems(20.))
                    .overflow_y_scroll()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .children(rows),
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
                        Button::new("scheme-export-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("scheme-export-save")
                            .primary()
                            .label(t!("schemes.export.save", count = count))
                            .loading(self.busy)
                            .disabled(self.busy || self.invalid())
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
    }
}

/// 文件名中不能出现的字符（含控制字符）换成下划线；空的为 `scheme`。
fn safe_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "\\/:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        "scheme".to_string()
    } else {
        clean.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_made_safe() {
        assert_eq!(safe_name("One Half: Dark"), "One Half_ Dark");
        assert_eq!(safe_name("a/b\\c"), "a_b_c");
        assert_eq!(safe_name("  "), "scheme");
    }
}
