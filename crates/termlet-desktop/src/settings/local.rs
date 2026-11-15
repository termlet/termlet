//! 设置中的“本地终端”：默认打开哪一个、侧边栏“本机”中显示哪些，以及添加、编辑、删除自定义的 shell。
//!
//! 侧边栏与默认值中的项写作 `local:<shell ID>`，WSL 发行版写作 `wsl:<发行版>`。默认的一项总是显示在侧边栏中。
//! 新添加的自定义 shell 列在侧边栏中。

use std::collections::BTreeMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _,
    Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::id::random_token;
use termlet_core::localshell::Profile;
use termlet_core::model::{LocalShell, ShellSettings};

use crate::choice::{Choice, ChoiceState, choice_state};
use crate::local_shell::{LocalTarget, shell_key};
use crate::prose::prose;
use crate::{local_shell, t};

pub(super) struct LocalPage {
    /// 检测到的 shell（不含自定义的）；`None` 为还在检测
    detected: Option<Vec<Profile>>,
    /// 检测到的 WSL 发行版（与 shell 一起检测完）
    wsl: Vec<String>,
    custom: Vec<LocalShell>,
    sidebar: Vec<String>,
    default: String,
    default_select: Entity<ChoiceState<String>>,
    _default_subscription: Subscription,
    _detect: Task<()>,
}

impl LocalPage {
    pub(super) fn new(
        settings: &ShellSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let default_select = choice_state(Vec::new(), &String::new(), false, window, cx);
        // 只在用户选了一项时改默认值：列表换了内容（检测完、增删自定义的 shell）时下拉框跟着变，不算用户的选择
        let default_subscription = cx.subscribe(
            &default_select,
            |this, _, event: &SelectEvent<SearchableVec<Choice<String>>>, cx| {
                if let SelectEvent::Confirm(Some(key)) = event {
                    this.default = key.clone();
                    cx.notify();
                }
            },
        );
        let detector = local_shell::detector(cx);
        let distros = local_shell::wsl_distros(cx);
        let detect = cx.spawn_in(window, async move |this, cx| {
            let (found, wsl) = cx
                .background_spawn(async move { (detector.list(false), distros.list()) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.detected = Some(found.into_iter().filter(|p| !p.is_custom()).collect());
                this.wsl = wsl;
                this.refresh_default_select(window, cx);
            });
        });
        Self {
            detected: None,
            wsl: Vec::new(),
            custom: settings.custom.clone(),
            sidebar: settings.sidebar.clone(),
            default: settings.default.clone(),
            default_select,
            _default_subscription: default_subscription,
            _detect: detect,
        }
    }

    /// 全部本地终端：检测到的 shell 在前，然后是自定义的，最后是 WSL 发行版。
    fn targets(&self) -> Vec<LocalTarget> {
        let mut list: Vec<LocalTarget> = self
            .detected
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(LocalTarget::Shell)
            .collect();
        list.extend(
            self.custom
                .iter()
                .map(|s| LocalTarget::Shell(Profile::custom(s))),
        );
        list.extend(self.wsl.iter().cloned().map(LocalTarget::Wsl));
        list
    }

    /// 实际的默认项：设置的那个还在时用它，否则是列表中的第一个。
    fn default_key(&self, targets: &[LocalTarget]) -> Option<String> {
        targets
            .iter()
            .map(LocalTarget::key)
            .find(|k| *k == self.default)
            .or_else(|| targets.first().map(LocalTarget::key))
    }

    fn refresh_default_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self.targets();
        let mut choices: Vec<Choice<String>> = targets
            .iter()
            .map(|t| Choice::new(t.title(), t.key()))
            .collect();
        // 设置的默认项这次没有检测到（WSL 检测超时、shell 卸载了）：照样列出并选中，保存时不被悄悄换掉
        if !self.default.is_empty() && !choices.iter().any(|c| c.value == self.default) {
            choices.insert(
                0,
                Choice::new(
                    t!("local_shell.unavailable", name = key_name(&self.default)),
                    self.default.clone(),
                ),
            );
        }
        // 没有设置默认项时显示第一项（与打开终端时的取法相同）
        let selected = if self.default.is_empty() {
            choices.first().map(|c| c.value.clone()).unwrap_or_default()
        } else {
            self.default.clone()
        };
        self.default_select.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(choices), window, cx);
            state.set_selected_value(&selected, window, cx);
        });
        cx.notify();
    }

    /// 写进设置。
    pub(super) fn apply(&self, settings: &mut ShellSettings) {
        settings.custom = self.custom.clone();
        settings.sidebar = self.sidebar.clone();
        settings.default = self.default.clone();
    }

    fn set_shown(&mut self, key: &str, shown: bool) {
        self.sidebar.retain(|k| k != key);
        if shown {
            self.sidebar.push(key.to_string());
        }
    }

    fn remove_custom(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let key = shell_key(id);
        self.custom.retain(|s| s.id != id);
        self.sidebar.retain(|k| *k != key);
        if self.default == key {
            self.default.clear();
        }
        self.refresh_default_select(window, cx);
    }

    /// 添加（`shell` 为 `None`）或编辑自定义的 shell。
    fn open_editor(
        &mut self,
        shell: Option<LocalShell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = cx.entity().downgrade();
        let editor = cx.new(|cx| ShellEditor::new(shell, page, window, cx));
        let width = rems(30.).to_pixels(window.rem_size());
        let first = std::rc::Rc::new(std::cell::Cell::new(true));
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                editor.read(cx).name.focus_handle(cx).focus(window, cx);
            }
            let title = if editor.read(cx).id.is_some() {
                t!("local_shell.edit_title")
            } else {
                t!("local_shell.new_title")
            };
            // 在输入框中按 Enter 与点“保存”相同（有错误时不关闭）
            let save = {
                let editor = editor.clone();
                move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                    editor.update(cx, |editor, cx| editor.save(window, cx))
                }
            };
            dialog
                .title(title)
                .w(width)
                .overlay_closable(false)
                .child(editor.clone())
                .on_ok(save)
        });
    }

    /// 编辑器保存了一个 shell。
    fn on_saved(&mut self, shell: LocalShell, window: &mut Window, cx: &mut Context<Self>) {
        match self.custom.iter_mut().find(|s| s.id == shell.id) {
            Some(existing) => *existing = shell,
            None => {
                self.sidebar.push(shell_key(&shell.id));
                self.custom.push(shell);
            }
        }
        self.refresh_default_select(window, cx);
    }
}

impl Render for LocalPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let border = theme.border;
        let radius = theme.radius;
        let targets = self.targets();
        let default_key = self.default_key(&targets);
        let mut form = v_form();
        if self.detected.is_none() {
            form = form.child(
                field().child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(prose(t!("local_shell.detecting"))),
                ),
            );
        } else if targets.is_empty() {
            form = form.child(
                field().child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(prose(t!("local_shell.none_detected"))),
                ),
            );
        } else {
            form = form.child(
                field()
                    .label(t!("local_shell.default_label"))
                    .description(default_hint(cx))
                    .child(Select::new(&self.default_select).id("settings-default-shell")),
            );
            let last = targets.len().saturating_sub(1);
            let rows = targets.iter().enumerate().map(|(i, target)| {
                let key = target.key();
                let is_default = default_key.as_deref() == Some(key.as_str());
                let shown = is_default || self.sidebar.contains(&key);
                let command = match target {
                    LocalTarget::Shell(profile) if profile.is_custom() => profile.command.clone(),
                    LocalTarget::Shell(profile) => profile.path.display().to_string(),
                    LocalTarget::Wsl(distro) => format!("wsl.exe -d {distro}"),
                };
                let id = |name: &str| ElementId::from(SharedString::from(format!("{name}-{key}")));
                let custom = match target {
                    LocalTarget::Shell(profile) if profile.is_custom() => Some(profile.id.clone()),
                    _ => None,
                };
                h_flex()
                    .id(id("settings-shell"))
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .text_sm()
                    .when(i < last, |this| this.border_b_1().border_color(border))
                    .child(
                        Checkbox::new(id("settings-shell-show"))
                            .checked(shown)
                            .disabled(is_default)
                            .on_change(cx.listener({
                                let key = key.clone();
                                move |this, checked: &bool, _, cx| {
                                    this.set_shown(&key, *checked);
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        div()
                            .w(rems(10.))
                            .flex_none()
                            .truncate()
                            .child(target.title()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted)
                            .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                            .child(command),
                    )
                    .when_some(custom, |this, shell_id| {
                        let edit_id = shell_id.clone();
                        this.child(
                            Button::new(id("settings-shell-edit"))
                                .ghost()
                                .small()
                                .icon(IconName::Pencil)
                                .tooltip(t!("local_shell.edit"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let shell =
                                        this.custom.iter().find(|s| s.id == edit_id).cloned();
                                    this.open_editor(shell, window, cx);
                                })),
                        )
                        .child(
                            Button::new(id("settings-shell-delete"))
                                .ghost()
                                .small()
                                .icon(IconName::Trash)
                                .tooltip(t!("local_shell.delete"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.remove_custom(&shell_id, window, cx)
                                })),
                        )
                    })
            });
            form = form.child(
                field()
                    .label(t!("local_shell.shown_label"))
                    .description(prose(t!("local_shell.shown_hint")))
                    .child(
                        v_flex()
                            .border_1()
                            .border_color(border)
                            .rounded(radius)
                            .overflow_hidden()
                            .children(rows),
                    ),
            );
        }
        form.child(
            field().child(
                h_flex().child(
                    Button::new("settings-shell-new")
                        .outline()
                        .small()
                        .icon(IconName::Plus)
                        .label(t!("local_shell.new"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_editor(None, window, cx)),
                        ),
                ),
            ),
        )
        .child(
            field().child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("local_shell.settings_hint"))),
            ),
        )
    }
}

/// 添加、编辑一个自定义的 shell：名称、命令行（程序与参数）、起始目录、环境变量。
struct ShellEditor {
    /// 编辑时为原来的 ID
    id: Option<String>,
    page: WeakEntity<LocalPage>,
    name: Entity<InputState>,
    command: Entity<InputState>,
    dir: Entity<InputState>,
    env: Entity<TextareaState>,
    error: Option<SharedString>,
    _pick: Option<Task<()>>,
}

impl ShellEditor {
    fn new(
        shell: Option<LocalShell>,
        page: WeakEntity<LocalPage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let shell_ref = shell.as_ref();
        let input = |value: String,
                     placeholder: SharedString,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let name = input(
            shell_ref.map(|s| s.name.clone()).unwrap_or_default(),
            SharedString::default(),
            window,
            cx,
        );
        let command = input(
            shell_ref.map(|s| s.command.clone()).unwrap_or_default(),
            SharedString::default(),
            window,
            cx,
        );
        let dir = input(
            shell_ref.map(|s| s.dir.clone()).unwrap_or_default(),
            t!("local_shell.dir_placeholder"),
            window,
            cx,
        );
        let env_text = shell_ref.map(|s| format_env(&s.env)).unwrap_or_default();
        let env = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(4)
                .default_value(env_text)
        });
        Self {
            id: shell.map(|s| s.id),
            page,
            name,
            command,
            dir,
            env,
            error: None,
            _pick: None,
        }
    }

    /// 选择程序：替换命令行的第一项，保留已填的参数；名称还空着时用程序名。
    fn pick_program(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        self._pick = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let program = path.display().to_string();
                let current = this.command.read(cx).value().to_string();
                let next = replace_program(&current, &program);
                this.command
                    .update(cx, |s, cx| s.set_value(next, window, cx));
                if this.name.read(cx).value().trim().is_empty() {
                    let stem = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    this.name.update(cx, |s, cx| s.set_value(stem, window, cx));
                }
            });
        }));
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let name = self.name.read(cx).value().trim().to_string();
        let command = self.command.read(cx).value().trim().to_string();
        let dir = self.dir.read(cx).value().trim().to_string();
        let env = match parse_env(&self.env.read(cx).value()) {
            Ok(env) => env,
            Err(line) => {
                self.error = Some(t!("local_shell.env_bad_line", line = line));
                cx.notify();
                return false;
            }
        };
        let missing = if name.is_empty() {
            Some(t!("local_shell.name_required"))
        } else if command.is_empty() {
            Some(t!("local_shell.command_required"))
        } else {
            None
        };
        if let Some(error) = missing {
            self.error = Some(error);
            cx.notify();
            return false;
        }
        let shell = LocalShell {
            id: self
                .id
                .clone()
                .unwrap_or_else(|| format!("custom-{}", random_token(10))),
            name,
            command,
            dir,
            env,
        };
        let _ = self
            .page
            .update(cx, |page, cx| page.on_saved(shell, window, cx));
        true
    }
}

impl Render for ShellEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .child(
                v_form()
                    .child(
                        field()
                            .label(t!("local_shell.name"))
                            .child(Input::new(&self.name).id("shell-name")),
                    )
                    .child(
                        field()
                            .label(t!("local_shell.command"))
                            .description(prose(t!("local_shell.command_hint")))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .child(Input::new(&self.command).id("shell-command")),
                                    )
                                    .child(
                                        Button::new("shell-pick-program")
                                            .outline()
                                            .icon(IconName::FolderOpen)
                                            .tooltip(t!("local_shell.pick_program"))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.pick_program(window, cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        field()
                            .label(t!("local_shell.dir"))
                            .child(Input::new(&self.dir).id("shell-dir")),
                    )
                    .child(
                        field()
                            .label(t!("local_shell.env"))
                            .child(Textarea::new(&self.env)),
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
                            .text_sm()
                            .text_color(theme.danger)
                            .when_some(self.error.clone(), |this, e| this.child(e)),
                    )
                    .child(
                        Button::new("shell-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("shell-save")
                            .primary()
                            .label(t!("common.save"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.save(window, cx) {
                                    window.close_dialog(cx);
                                }
                            })),
                    ),
            )
    }
}

/// “默认打开”的说明：带上（已保存的）打开默认本地终端的快捷键。
fn default_hint(cx: &App) -> SharedString {
    let settings = crate::app_state::AppState::store(cx).settings();
    let os = crate::keymap::current_os(cx);
    let keymap = termlet_core::keymap::effective(&settings.keybindings, os);
    match keymap
        .combos(termlet_core::keymap::KeyAction::NewLocal)
        .first()
    {
        Some(combo) => t!("local_shell.default_hint", key = combo.label(os)),
        None => t!("local_shell.default_hint_no_key"),
    }
}

/// 命令行的第一项换成 `program`（含空格时加引号），保留后面的参数。
fn replace_program(command: &str, program: &str) -> String {
    let trimmed = command.trim_start();
    let rest = if let Some(after) = trimmed.strip_prefix('"') {
        after.find('"').map_or("", |end| &after[end + 1..])
    } else {
        trimmed
            .find(char::is_whitespace)
            .map_or("", |end| &trimmed[end..])
    };
    let program = if program.contains(char::is_whitespace) {
        format!("\"{program}\"")
    } else {
        program.to_string()
    };
    format!("{program}{rest}")
}

/// 环境变量的文字：每行一个 `名称=值`。
fn format_env(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 解析环境变量的文字；格式不对时返回出错的行号（从 1 开始）。空行跳过。
fn parse_env(text: &str) -> Result<BTreeMap<String, String>, usize> {
    let mut env = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once('=') {
            Some((name, value)) if !name.trim().is_empty() => {
                env.insert(name.trim().to_string(), value.to_string());
            }
            _ => return Err(i + 1),
        }
    }
    Ok(env)
}

/// 设置中的写法（`local:<ID>`、`wsl:<发行版>`）对应的名称：没检测到时只能显示它。
fn key_name(key: &str) -> String {
    key.strip_prefix("wsl:")
        .or_else(|| key.strip_prefix("local:"))
        .unwrap_or(key)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_is_replaced_and_arguments_kept() {
        assert_eq!(
            replace_program("bash -l", r"C:\msys\bash.exe"),
            r"C:\msys\bash.exe -l"
        );
        assert_eq!(
            replace_program(r#""C:\Program Files\x.exe" --a"#, r"C:\Program Files\y.exe"),
            r#""C:\Program Files\y.exe" --a"#
        );
        assert_eq!(replace_program("", "nu"), "nu");
    }

    #[test]
    fn env_lines_parse_and_report_bad_line() {
        let env = parse_env("A=1\n\n B = x=y \n").unwrap();
        assert_eq!(env.get("A").map(String::as_str), Some("1"));
        assert_eq!(env.get("B").map(String::as_str), Some(" x=y"));
        assert_eq!(parse_env("A=1\nnovalue"), Err(2));
        assert_eq!(parse_env("=x"), Err(1));
        assert_eq!(format_env(&env), "A=1\nB= x=y");
    }
}
