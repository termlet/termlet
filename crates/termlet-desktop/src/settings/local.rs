//! 设置中的“本地终端”：默认打开哪一个、侧边栏“本机”中显示哪些，以及添加、编辑、删除自定义的 shell。
//!
//! 侧边栏与默认值中的项写作 `local:<shell ID>`，WSL 发行版写作 `wsl:<发行版>`。默认的一项总是显示在侧边栏中。
//! 新添加的自定义 shell 列在侧边栏中。
//!
//! 检测到的 shell 不能删除，可以编辑（名称、启动参数、起始目录、环境变量，按 shell ID 记在本机设置中）与恢复默认；
//! 检测到的 shell 与 WSL 发行版可以隐藏（默认的一项除外）。这次没检测到的改动、隐藏照样保存，重新检测到时照旧生效。
//!
//! Windows 上最后另有资源管理器右键菜单“在此处打开 Termlet”的开关（[`crate::folder_menu`]，立即生效，不经“保存”）。

use std::collections::BTreeMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::form::{Field, field, v_form};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _,
    Subscription, Task, TestSupportExt as _, WeakEntity, Window, div, prelude::FluentBuilder as _,
    rems,
};
use termlet_core::id::random_token;
use termlet_core::localshell::Profile;
use termlet_core::model::{LocalShell, ShellOverride, ShellSettings};

use crate::choice::{Choice, ChoiceState, choice_state};
use crate::folder_menu::FolderMenuSection;
use crate::local_shell::{LocalTarget, shell_key};
use crate::prose::prose;
use crate::{local_shell, t};

pub(super) struct LocalPage {
    /// 检测到的 shell（不含自定义的，没有加上改动）；`None` 为还在检测
    detected: Option<Vec<Profile>>,
    /// 检测到的 WSL 发行版（与 shell 一起检测完）
    wsl: Vec<String>,
    custom: Vec<LocalShell>,
    /// 对检测到的 shell 的改动（含这次没检测到的）
    overrides: BTreeMap<String, ShellOverride>,
    /// 隐藏的项（含这次没检测到的）
    hidden: Vec<String>,
    sidebar: Vec<String>,
    default: String,
    default_select: Entity<ChoiceState<String>>,
    _default_subscription: Subscription,
    _detect: Task<()>,
    /// 资源管理器右键菜单的开关（只在 Windows 上）
    folder_menu: Option<Entity<FolderMenuSection>>,
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
            overrides: settings.overrides.clone(),
            hidden: settings.hidden.clone(),
            sidebar: settings.sidebar.clone(),
            default: settings.default.clone(),
            default_select,
            _default_subscription: default_subscription,
            _detect: detect,
            folder_menu: termlet_core::folder_menu::supported()
                .then(|| cx.new(FolderMenuSection::new)),
        }
    }

    /// 全部本地终端（含隐藏的）：检测到的 shell（加上改动）在前，然后是自定义的，最后是 WSL 发行版。
    fn targets(&self) -> Vec<LocalTarget> {
        let mut list: Vec<LocalTarget> = self
            .detected
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|p| match self.overrides.get(&p.id) {
                Some(changes) => LocalTarget::Shell(p.with_override(changes)),
                None => LocalTarget::Shell(p),
            })
            .collect();
        list.extend(
            self.custom
                .iter()
                .map(|s| LocalTarget::Shell(Profile::custom(s))),
        );
        list.extend(self.wsl.iter().cloned().map(LocalTarget::Wsl));
        list
    }

    fn is_hidden(&self, key: &str) -> bool {
        self.hidden.iter().any(|k| k == key)
    }

    /// 实际的默认项：设置的那个还在、没有隐藏时用它，否则是没有隐藏的第一个。
    fn default_key(&self, targets: &[LocalTarget]) -> Option<String> {
        let visible: Vec<String> = targets
            .iter()
            .map(LocalTarget::key)
            .filter(|k| !self.is_hidden(k))
            .collect();
        visible
            .iter()
            .find(|k| **k == self.default)
            .or(visible.first())
            .cloned()
    }

    fn refresh_default_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self.targets();
        let mut choices: Vec<Choice<String>> = targets
            .iter()
            .filter(|t| !self.is_hidden(&t.key()))
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
        settings.overrides = self.overrides.clone();
        settings.hidden = self.hidden.clone();
        settings.sidebar = self.sidebar.clone();
        settings.default = self.default.clone();
    }

    fn set_shown(&mut self, key: &str, shown: bool) {
        self.sidebar.retain(|k| k != key);
        if shown {
            self.sidebar.push(key.to_string());
        }
    }

    /// 隐藏或取消隐藏。侧边栏中显示的勾选留着，取消隐藏后照旧。
    fn set_hidden(&mut self, key: &str, hidden: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.hidden.retain(|k| k != key);
        if hidden {
            self.hidden.push(key.to_string());
        }
        self.refresh_default_select(window, cx);
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
    fn edit_custom(
        &mut self,
        shell: Option<LocalShell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = cx.entity().downgrade();
        let editor = cx.new(|cx| ShellEditor::custom(shell, page, window, cx));
        self.open_editor(editor, window, cx);
    }

    /// 编辑检测到的 shell（`id` 为它的 shell ID）。
    fn edit_detected(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.detected.iter().flatten().find(|p| p.id == id).cloned() else {
            return;
        };
        let changes = self.overrides.get(id).cloned().unwrap_or_default();
        let page = cx.entity().downgrade();
        let editor = cx.new(|cx| ShellEditor::detected(profile, &changes, page, window, cx));
        self.open_editor(editor, window, cx);
    }

    fn open_editor(
        &mut self,
        editor: Entity<ShellEditor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let width = rems(30.).to_pixels(window.rem_size());
        let first = std::rc::Rc::new(std::cell::Cell::new(true));
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                editor.read(cx).name.focus_handle(cx).focus(window, cx);
            }
            let title = match &editor.read(cx).editing {
                Editing::Custom(None) => t!("local_shell.new_title"),
                Editing::Custom(Some(_)) | Editing::Detected(_) => t!("local_shell.edit_title"),
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

    /// 编辑器保存了对检测到的 shell 的改动；没有改动（恢复了默认）时不再保存。
    fn on_override_saved(
        &mut self,
        id: String,
        changes: ShellOverride,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if changes.is_empty() {
            self.overrides.remove(&id);
        } else {
            self.overrides.insert(id, changes);
        }
        self.refresh_default_select(window, cx);
    }
}

impl Render for LocalPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hits = super::search::hits(window, cx);
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
                hits.mark(
                    field()
                        .label(t!("local_shell.default_label"))
                        .description(default_hint(cx))
                        .child(Select::new(&self.default_select).id("settings-default-shell")),
                    "settings-default-shell",
                    cx,
                ),
            );
            let last = targets.len().saturating_sub(1);
            let rows = targets.iter().enumerate().map(|(i, target)| {
                let key = target.key();
                let is_default = default_key.as_deref() == Some(key.as_str());
                let hidden = self.is_hidden(&key);
                let shown = !hidden && (is_default || self.sidebar.contains(&key));
                let command = match target {
                    LocalTarget::Shell(profile) if profile.is_custom() => profile.command.clone(),
                    LocalTarget::Shell(profile) => profile.path.display().to_string(),
                    LocalTarget::Wsl(distro) => format!("wsl.exe -d {distro}"),
                };
                let id = |name: &str| ElementId::from(SharedString::from(format!("{name}-{key}")));
                let (custom, detected) = match target {
                    LocalTarget::Shell(profile) if profile.is_custom() => {
                        (Some(profile.id.clone()), None)
                    }
                    LocalTarget::Shell(profile) => (None, Some(profile.id.clone())),
                    LocalTarget::Wsl(_) => (None, None),
                };
                // 自定义的可以删除，检测到的与 WSL 发行版可以隐藏
                let can_hide = custom.is_none();
                // 状态：隐藏的写“已隐藏”，改过的写“已修改”
                let status = if hidden {
                    Some(t!("local_shell.hidden"))
                } else if detected
                    .as_ref()
                    .is_some_and(|id| self.overrides.contains_key(id))
                {
                    Some(t!("local_shell.modified"))
                } else {
                    None
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
                            .disabled(is_default || hidden)
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
                            .when(hidden, |this| this.text_color(muted))
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
                    .when_some(status, |this, status| {
                        this.child(
                            div()
                                .id(id("settings-shell-status"))
                                .test_support()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(status),
                        )
                    })
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
                                    this.edit_custom(shell, window, cx);
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
                    .when_some(detected, |this, shell_id| {
                        this.child(
                            Button::new(id("settings-shell-edit"))
                                .ghost()
                                .small()
                                .icon(IconName::Pencil)
                                .tooltip(t!("local_shell.edit"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit_detected(&shell_id, window, cx)
                                })),
                        )
                    })
                    // 检测到的不能删除，可以隐藏；默认打开的一项不能隐藏（先另选一个默认的）
                    .when(can_hide, |this| {
                        let tooltip = if hidden {
                            t!("local_shell.unhide")
                        } else if is_default {
                            t!("local_shell.hide_default")
                        } else {
                            t!("local_shell.hide")
                        };
                        this.child(
                            Button::new(id("settings-shell-hide"))
                                .ghost()
                                .small()
                                .icon(if hidden {
                                    IconName::Eye
                                } else {
                                    IconName::EyeOff
                                })
                                .tooltip(tooltip)
                                .disabled(is_default && !hidden)
                                .on_click(cx.listener({
                                    let key = key.clone();
                                    move |this, _, window, cx| {
                                        this.set_hidden(&key, !hidden, window, cx)
                                    }
                                })),
                        )
                    })
            });
            form = form.child(
                hits.mark(
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
                    "settings-shells-shown",
                    cx,
                ),
            );
        }
        form.child(
            hits.mark(
                field().child(
                    h_flex().child(
                        Button::new("settings-shell-new")
                            .outline()
                            .small()
                            .icon(IconName::Plus)
                            .label(t!("local_shell.new"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.edit_custom(None, window, cx)
                            })),
                    ),
                ),
                "settings-shell-new",
                cx,
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
        .map(|form| match self.folder_menu.clone() {
            Some(section) => {
                form.child(hits.mark(field().child(section), "settings-folder-menu", cx))
            }
            None => form,
        })
    }
}

/// 编辑的是哪一个本地终端。
enum Editing {
    /// 添加（`None`）或编辑（原来的 ID）自定义的 shell
    Custom(Option<String>),
    /// 检测到的 shell：检测结果，即各项的默认值
    Detected(Profile),
}

/// 添加、编辑一个本地终端：名称、命令行（自定义的是程序与参数，检测到的只有启动参数）、起始目录、环境变量。
struct ShellEditor {
    editing: Editing,
    page: WeakEntity<LocalPage>,
    name: Entity<InputState>,
    /// 自定义的 shell 是命令行，检测到的是启动参数
    command: Entity<InputState>,
    dir: Entity<InputState>,
    env: Entity<TextareaState>,
    error: Option<SharedString>,
    _pick: Option<Task<()>>,
}

impl ShellEditor {
    /// 添加（`shell` 为 `None`）或编辑自定义的 shell。
    fn custom(
        shell: Option<LocalShell>,
        page: WeakEntity<LocalPage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let shell = shell.unwrap_or_default();
        let id = (!shell.id.is_empty()).then(|| shell.id.clone());
        Self::new(
            Editing::Custom(id),
            [
                (shell.name, SharedString::default()),
                (shell.command, SharedString::default()),
            ],
            &shell.dir,
            &shell.env,
            page,
            window,
            cx,
        )
    }

    /// 编辑检测到的 shell：`profile` 是检测结果（没有加上改动），`changes` 是现在的改动。名称与启动参数填上
    /// 现在的值，名称的占位文字是检测到的名称。
    fn detected(
        profile: Profile,
        changes: &ShellOverride,
        page: WeakEntity<LocalPage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let default_name = local_shell::detected_name(&profile);
        let name = match changes.name.trim() {
            "" => default_name.to_string(),
            name => name.to_string(),
        };
        let args = changes.args.clone().unwrap_or_else(|| profile.args_text());
        Self::new(
            Editing::Detected(profile),
            [(name, default_name), (args, SharedString::default())],
            &changes.dir,
            &changes.env,
            page,
            window,
            cx,
        )
    }

    /// `fields` 是名称与命令行（或启动参数）的值与占位文字。
    fn new(
        editing: Editing,
        fields: [(String, SharedString); 2],
        dir: &str,
        env: &BTreeMap<String, String>,
        page: WeakEntity<LocalPage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
        let [(name, name_placeholder), (command, command_placeholder)] = fields;
        let name = input(name, name_placeholder, window, cx);
        let command = input(command, command_placeholder, window, cx);
        let dir = input(
            dir.to_string(),
            t!("local_shell.dir_placeholder"),
            window,
            cx,
        );
        let env_text = format_env(env);
        let env = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(4)
                .default_value(env_text)
        });
        Self {
            editing,
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

    /// 检测到的 shell：各项换回检测到的（点“保存”后才生效，取消则不变）。
    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Editing::Detected(profile) = &self.editing else {
            return;
        };
        let name = local_shell::detected_name(profile).to_string();
        let args = profile.args_text();
        self.name.update(cx, |s, cx| s.set_value(name, window, cx));
        self.command
            .update(cx, |s, cx| s.set_value(args, window, cx));
        self.dir.update(cx, |s, cx| s.set_value("", window, cx));
        self.env.update(cx, |s, cx| s.set_value("", window, cx));
        self.error = None;
        cx.notify();
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
        let id = match &self.editing {
            Editing::Custom(id) => id.clone(),
            Editing::Detected(profile) => {
                // 与检测到的相同的项不算改动（名称留空也是用检测到的）
                let changes = ShellOverride {
                    name: if name == local_shell::detected_name(profile).as_ref() {
                        String::new()
                    } else {
                        name
                    },
                    args: (command != profile.args_text()).then_some(command),
                    dir,
                    env,
                };
                let id = profile.id.clone();
                let _ = self.page.update(cx, |page, cx| {
                    page.on_override_saved(id, changes, window, cx)
                });
                return true;
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
            id: id.unwrap_or_else(|| format!("custom-{}", random_token(10))),
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

    /// 自定义的 shell：命令行与选择程序的按钮。
    fn command_field(&self, cx: &mut Context<Self>) -> Field {
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
                            .on_click(
                                cx.listener(|this, _, window, cx| this.pick_program(window, cx)),
                            ),
                    ),
            )
    }
}

impl Render for ShellEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, danger, muted) = (theme.border, theme.danger, theme.muted_foreground);
        let detected = match &self.editing {
            Editing::Detected(profile) => Some(profile.path.display().to_string()),
            Editing::Custom(_) => None,
        };
        let is_detected = detected.is_some();
        let form = v_form().child(
            field()
                .label(t!("local_shell.name"))
                .child(Input::new(&self.name).id("shell-name")),
        );
        // 检测到的 shell 不能换程序，只列出它；命令行的位置换成启动参数
        let form = match detected {
            Some(program) => form
                .child(
                    field().label(t!("local_shell.program")).child(
                        div()
                            .id("shell-program")
                            .test_support()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted)
                            .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                            .child(program),
                    ),
                )
                .child(
                    field()
                        .label(t!("local_shell.args"))
                        .child(Input::new(&self.command).id("shell-args")),
                ),
            None => form.child(self.command_field(cx)),
        };
        v_flex()
            .gap_3()
            .child(
                form.child(
                    field()
                        .label(t!("local_shell.dir"))
                        .child(Input::new(&self.dir).id("shell-dir")),
                )
                .child(
                    field().label(t!("local_shell.env")).child(
                        div()
                            .id("shell-env")
                            .test_support()
                            .child(Textarea::new(&self.env)),
                    ),
                ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(border)
                    .when(is_detected, |this| {
                        this.child(
                            Button::new("shell-reset")
                                .ghost()
                                .icon(IconName::RotateCcw)
                                .label(t!("local_shell.reset"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.reset(window, cx)),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(danger)
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
mod ui_tests;

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
