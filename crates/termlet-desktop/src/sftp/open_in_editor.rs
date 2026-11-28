//! 文件管理的“在 VS Code 中打开”“在 Zed 中打开”（远程栏的右键菜单、Tab 的菜单）：只在这台电脑上装了对应的编辑器、
//! 设置（本机设置 `window.remote_editor`）允许时出现。链接的拼法与 `~/.ssh/config` 的读写见核心的 `remote_editor`。
//!
//! - `~/.ssh/config` 中有这台主机的别名、WSL 发行版：直接交给系统打开链接。
//! - 没有别名：先问（[`ConfirmView`]）：编辑器要连的主机、直接打开时可能连不上的原因、可以追加到 `~/.ssh/config`
//!   的条目；可以“直接打开”或“写入配置并打开”（只追加，写之前备份原文件）。
//! - 被堡垒机调起的临时连接、“只用一个通道”的主机（堡垒机）不显示：编辑器要在服务器上运行程序、开多个通道。
//! - 装了哪些编辑器在后台检测：启动时、窗口激活时（至多每 30 秒一次）。

#[cfg(test)]
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, Global, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, TestSupportExt as _,
    Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::model::{Host, RemoteEditorSetting};
use termlet_core::remote_editor::{
    self, Editor, Endpoint, Launch, Obstacle, Remote, SshConfig, detect, ssh_config,
};
use termlet_core::store::Store;

use super::panel::SftpTarget;
use crate::app_state::AppState;
use crate::notify::{Notice, NoticeLine};
use crate::prose::prose;
use crate::ui_scale::DialogTitleExt as _;
use crate::workspace::Workspace;
use crate::{ssh_errors, t};

/// 窗口激活时重新检测的最短间隔。
const RECHECK: Duration = Duration::from_secs(30);

/// 装了的编辑器与打开链接的方式（全局量，[`init`] 建立）。
struct Editors {
    installed: Vec<Editor>,
    checked: Option<Instant>,
    checking: bool,
    /// 测试中固定了装了哪些（不再检测）
    fixed: bool,
    /// 把链接交给系统（测试中换成记录下来）
    opener: Rc<dyn Fn(&str, &mut App)>,
    /// `~/.ssh/config` 的位置（测试中换成临时文件）；取不到主目录时为 `None`
    config_path: Option<PathBuf>,
}

impl Global for Editors {}

/// 建立全局量并开始检测；窗口激活时重新检测（`crate::init` 中调用，UI 测试也调用）。
pub(crate) fn init(cx: &mut App) {
    if cx.has_global::<Editors>() {
        return;
    }
    cx.set_global(Editors {
        installed: Vec::new(),
        checked: None,
        checking: false,
        fixed: false,
        opener: Rc::new(|url, cx| cx.open_url(url)),
        config_path: ssh_config::default_path(),
    });
    refresh(cx);
    cx.observe_new::<Workspace>(|_, window, cx| {
        if let Some(window) = window {
            cx.observe_window_activation(window, |_, window, cx| {
                if window.is_window_active() {
                    refresh(cx);
                }
            })
            .detach();
        }
    })
    .detach();
}

/// 在后台重新检测装了哪些编辑器（距上次不到 30 秒、正在检测、测试中固定了的不检测）。
fn refresh(cx: &mut App) {
    let Some(editors) = cx.try_global::<Editors>() else {
        return;
    };
    let recent = editors.checked.is_some_and(|at| at.elapsed() < RECHECK);
    if editors.fixed || editors.checking || recent {
        return;
    }
    cx.global_mut::<Editors>().checking = true;
    let job = cx.background_spawn(async { detect::installed() });
    cx.spawn(async move |cx| {
        let found = job.await;
        cx.update(|cx| {
            let editors = cx.global_mut::<Editors>();
            editors.checking = false;
            editors.checked = Some(Instant::now());
            if !editors.fixed && editors.installed != found {
                tracing::info!(
                    editors = ?found.iter().map(|e| e.name()).collect::<Vec<_>>(),
                    "检测到装了的编辑器"
                );
                editors.installed = found;
            }
        });
    })
    .detach();
}

/// 装了的编辑器（检测完成前为空）。
pub(crate) fn installed(cx: &App) -> Vec<Editor> {
    cx.try_global::<Editors>()
        .map(|e| e.installed.clone())
        .unwrap_or_default()
}

/// 测试：固定装了的编辑器与 `~/.ssh/config` 的位置，返回打开过的链接。
#[cfg(test)]
pub(crate) fn fake_for_test(
    installed: Vec<Editor>,
    config_path: PathBuf,
    cx: &mut App,
) -> Rc<RefCell<Vec<String>>> {
    init(cx);
    let opened: Rc<RefCell<Vec<String>>> = Rc::default();
    let record = opened.clone();
    let editors = cx.global_mut::<Editors>();
    editors.installed = installed;
    editors.fixed = true;
    editors.config_path = Some(config_path);
    editors.opener = Rc::new(move |url, _| record.borrow_mut().push(url.to_string()));
    opened
}

/// 能不能在编辑器中打开这个连接：SSH 主机（不是“只用一个通道”的）、快速连接、WSL；以 root 打开的按原来的主机。
fn supported(target: &SftpTarget, store: &Store) -> bool {
    match target {
        SftpTarget::Host(id) => store
            .get::<Host>(id)
            .is_some_and(|host| !host.doc.ssh.single_channel),
        SftpTarget::Quick(_) | SftpTarget::Wsl { .. } => true,
        SftpTarget::Launch(_) => false,
        SftpTarget::Sudo(inner) => supported(inner, store),
    }
}

/// 这个连接的菜单中显示哪些编辑器：装了的、设置允许的、能打开这种连接的。
pub(crate) fn editors_for(target: &SftpTarget, store: &Store, cx: &App) -> Vec<Editor> {
    if !supported(target, store) {
        return Vec::new();
    }
    let setting: RemoteEditorSetting = store.local_settings().window.remote_editor;
    let wsl = is_wsl(target);
    installed(cx)
        .into_iter()
        .filter(|e| e.allowed_by(setting) && (!wsl || e.supports_wsl()))
        .collect()
}

fn is_wsl(target: &SftpTarget) -> bool {
    match target {
        SftpTarget::Wsl { .. } => true,
        SftpTarget::Sudo(inner) => is_wsl(inner),
        _ => false,
    }
}

/// 菜单中“在 VS Code 中打开”等项（打开 `path`）；没有可用的编辑器时原样返回。
pub(crate) fn menu_items(
    menu: PopupMenu,
    target: &SftpTarget,
    path: String,
    store: &Store,
    cx: &App,
) -> PopupMenu {
    let mut menu = menu;
    for editor in editors_for(target, store, cx) {
        let (target, path) = (target.clone(), path.clone());
        menu = menu.item(
            PopupMenuItem::new(t!("sftp.editor.open", editor = editor.name())).on_click(
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    open(editor, &target, &path, window, cx)
                },
            ),
        );
    }
    menu
}

/// 编辑器要连的地方：从连接计划中取出地址、用户、端口与登录方式。
fn remote_of(target: &SftpTarget, store: &Store) -> Result<Remote, SharedString> {
    match target {
        SftpTarget::Sudo(inner) => remote_of(inner, store),
        SftpTarget::Wsl { distro, .. } => Ok(Remote::Wsl {
            distro: distro.clone(),
        }),
        SftpTarget::Host(id) => {
            let spec = target.spec(store).map_err(|e| ssh_errors::describe(&e))?;
            let name = store
                .get::<Host>(id)
                .map(|host| host.doc.name)
                .unwrap_or_default();
            Ok(Remote::Ssh(Endpoint::from_plan(&name, &spec.plan)))
        }
        SftpTarget::Quick(quick) => {
            let spec = target.spec(store).map_err(|e| ssh_errors::describe(&e))?;
            Ok(Remote::Ssh(Endpoint::from_plan(&quick.host, &spec.plan)))
        }
        SftpTarget::Launch(_) => Err(t!("sftp.editor.unsupported")),
    }
}

/// 读 `~/.ssh/config`（读不了时当作空的，并记日志）。
fn read_config(path: Option<&PathBuf>) -> SshConfig {
    let Some(path) = path else {
        return SshConfig::default();
    };
    SshConfig::read(path).unwrap_or_else(|e| {
        tracing::warn!("读取 {} 失败：{e}", path.display());
        SshConfig::default()
    })
}

fn config_path(cx: &App) -> Option<PathBuf> {
    cx.try_global::<Editors>()
        .and_then(|e| e.config_path.clone())
}

/// 交给系统打开链接（测试中记录下来）。
fn launch_url(launch: &Launch, cx: &mut App) {
    tracing::info!(
        editor = launch.editor.name(),
        alias = launch.alias.is_some(),
        "在编辑器中打开远程目录"
    );
    termlet_core::usage::mark(termlet_core::usage::Feature::OpenInEditor);
    let opener = cx.try_global::<Editors>().map(|e| e.opener.clone());
    match opener {
        Some(opener) => opener(&launch.url, cx),
        None => cx.open_url(&launch.url),
    }
}

/// 在 `editor` 中打开 `path`：配置中有别名的直接打开，没有的先问。
pub(crate) fn open(
    editor: Editor,
    target: &SftpTarget,
    path: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let store = AppState::store(cx);
    let remote = match remote_of(target, &store) {
        Ok(remote) => remote,
        Err(message) => {
            crate::notify::error(message, window, cx);
            return;
        }
    };
    let config_path = config_path(cx);
    let config = read_config(config_path.as_ref());
    let Some(launch) = remote_editor::plan(
        editor,
        &remote,
        path,
        &config,
        &remote_editor::local_username(),
        &remote_editor::stamp_now(),
    ) else {
        return;
    };
    if launch.needs_confirm() {
        confirm(launch, remote, path.to_string(), config_path, window, cx);
    } else {
        launch_url(&launch, cx);
    }
}

/// 没有别名时的询问对话框。
fn confirm(
    launch: Launch,
    remote: Remote,
    path: String,
    config_path: Option<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    let recommend_write =
        config_path.is_some() && launch.obstacles.iter().any(Obstacle::fixed_by_entry);
    let view = cx.new(|cx| ConfirmView {
        launch,
        remote,
        path,
        config_path,
        notice: NoticeLine::new(cx),
    });
    let notice = view.read(cx).notice.clone();
    let width = rems(36.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, _, cx| {
        let (editor, can_write) = {
            let view = view.read(cx);
            (view.launch.editor, view.config_path.is_some())
        };
        let direct = view.clone();
        let write = view.clone();
        let ok = view.clone();
        let direct_button = Button::new("editor-open-direct").label(t!("sftp.editor.open_direct"));
        let write_button =
            Button::new("editor-write-config").label(t!("sftp.editor.write_and_open"));
        // 推荐的一项是主按钮（Enter）；三个按钮一样宽（DialogClose、DialogAction 本身占满所分的宽度）
        let footer = DialogFooter::new().child(
            DialogClose::new().child(
                Button::new("editor-cancel")
                    .w_full()
                    .label(t!("common.cancel")),
            ),
        );
        let footer =
            if recommend_write {
                footer
                    .child(
                        div()
                            .size_full()
                            .child(direct_button.outline().w_full().on_click(
                                move |_, window, cx| {
                                    direct.update(cx, |view, cx| view.open_direct(cx));
                                    window.close_dialog(cx);
                                },
                            )),
                    )
                    .child(DialogAction::new().child(write_button.primary().w_full()))
            } else {
                footer
                    .when(can_write, |footer| {
                        footer.child(
                            div()
                                .size_full()
                                .child(write_button.outline().w_full().on_click(
                                    move |_, window, cx| {
                                        if write
                                            .update(cx, |view, cx| view.write_and_open(window, cx))
                                        {
                                            window.close_dialog(cx);
                                        }
                                    },
                                )),
                        )
                    })
                    .child(DialogAction::new().child(direct_button.primary().w_full()))
            };
        dialog
            .titled(t!("sftp.editor.confirm_title", editor = editor.name()))
            .w(width)
            .child(view.clone())
            .footer(footer)
            .on_ok(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                ok.update(cx, |view, cx| {
                    if recommend_write {
                        view.write_and_open(window, cx)
                    } else {
                        view.open_direct(cx);
                        true
                    }
                })
            })
    });
    crate::notify::attach(&notice, window, cx);
}

/// 询问对话框的内容。
struct ConfirmView {
    launch: Launch,
    remote: Remote,
    path: String,
    config_path: Option<PathBuf>,
    /// 写入 ~/.ssh/config 失败的说明（对话框不关，弹出的通知会被它盖住，见 [`crate::notify::show`]）
    notice: Entity<NoticeLine>,
}

impl ConfirmView {
    fn open_direct(&mut self, cx: &mut Context<Self>) {
        launch_url(&self.launch, cx);
    }

    /// 写入配置并用新的别名打开；返回是否成功（失败时提示，对话框留着）。写之前重新读一次配置：期间别处加了这台
    /// 主机的别名时直接用它，加了同名的别名时换一个名字。
    fn write_and_open(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(path) = self.config_path.clone() else {
            return false;
        };
        let config = read_config(Some(&path));
        let Some(fresh) = remote_editor::plan(
            self.launch.editor,
            &self.remote,
            &self.path,
            &config,
            &remote_editor::local_username(),
            &remote_editor::stamp_now(),
        ) else {
            return false;
        };
        let Some(entry) = fresh.entry.clone() else {
            launch_url(&fresh, cx);
            return true;
        };
        match ssh_config::append_now(&path, &entry.text) {
            Ok(appended) => {
                tracing::info!(
                    alias = %entry.alias,
                    backup = appended.backup.is_some(),
                    "已在 ~/.ssh/config 末尾追加 Host 条目"
                );
                let message = match &appended.backup {
                    Some(backup) => t!(
                        "sftp.editor.written",
                        alias = entry.alias.clone(),
                        path = path.display().to_string(),
                        backup = backup.display().to_string()
                    ),
                    None => t!(
                        "sftp.editor.created",
                        alias = entry.alias.clone(),
                        path = path.display().to_string()
                    ),
                };
                window.push_notification(Notification::success(message), cx);
                let launch = Launch {
                    url: remote_editor::alias_url(fresh.editor, &entry.alias, &self.path),
                    host: entry.alias.clone(),
                    alias: Some(entry.alias),
                    obstacles: Vec::new(),
                    entry: None,
                    ..fresh
                };
                launch_url(&launch, cx);
                true
            }
            Err(e) => {
                crate::notify::show(
                    Notice::error(t!(
                        "sftp.editor.write_failed",
                        path = path.display().to_string(),
                        error = e.to_string()
                    )),
                    window,
                    cx,
                );
                false
            }
        }
    }
}

/// 一条原因的说明。
fn obstacle_text(obstacle: &Obstacle, editor: Editor) -> SharedString {
    match obstacle {
        Obstacle::Jump { hosts } => t!("sftp.editor.obstacle.jump", hosts = hosts.join(" → ")),
        Obstacle::Proxy => t!("sftp.editor.obstacle.proxy"),
        Obstacle::ProxyCommand => t!("sftp.editor.obstacle.proxy_command"),
        Obstacle::StoredKey => t!("sftp.editor.obstacle.stored_key"),
        Obstacle::KeyFile { path } => t!("sftp.editor.obstacle.key_file", path = path.clone()),
        Obstacle::Port { port } => t!(
            "sftp.editor.obstacle.port",
            editor = editor.name(),
            port = port
        ),
    }
}

impl Render for ConfirmView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let launch = &self.launch;
        let editor = launch.editor;
        let config = self
            .config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.ssh/config".into());
        let reasons: Vec<SharedString> = launch
            .obstacles
            .iter()
            .map(|o| obstacle_text(o, editor))
            .collect();
        let muted = theme.muted_foreground;
        v_flex()
            .id("editor-confirm")
            .test_support()
            .gap_3()
            .text_sm()
            .child(div().child(prose(t!(
                "sftp.editor.no_alias",
                editor = editor.name(),
                host = launch.host.clone()
            ))))
            .when(!reasons.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(div().child(prose(t!("sftp.editor.obstacles"))))
                        .children(reasons.into_iter().map(|reason| {
                            div().pl_3().text_color(theme.warning).child(prose(reason))
                        })),
                )
            })
            .when_some(launch.entry.as_ref(), |this, entry| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(div().child(prose(t!("sftp.editor.entry", path = config.clone()))))
                        .child(
                            div()
                                .id("editor-entry")
                                .test_support()
                                .px_2()
                                .py_1p5()
                                .rounded(theme.radius)
                                .bg(theme.muted)
                                .font_family(theme.mono_font_family.clone())
                                .text_xs()
                                .child(entry.text.trim_end().to_string()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(prose(t!("sftp.editor.entry_hint"))),
                        ),
                )
            })
            .when(self.notice.read(cx).is_shown(), |this| {
                this.child(self.notice.clone())
            })
    }
}

#[cfg(test)]
mod tests;
