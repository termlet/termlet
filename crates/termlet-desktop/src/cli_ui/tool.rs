//! 「AI 与自动化」页的“termlet 命令”：命令行工具的位置、是否已在当前用户的 PATH 中，以及加入、移出 PATH
//! （立即执行，只在 Windows 上由 Termlet 修改；其他平台给出手动链接的命令）。开发构建中命令行工具在程序旁边时
//! 给出它的路径、不改 PATH，没有时只作说明；
//! 便携版不修改 PATH，只说明用完整路径运行。Linux 上按安装方式说明（[`linux_install`]）：deb、rpm 已装好
//! `/usr/bin/termlet`；AppImage 显示 AppImage 文件本身（链接为 termlet 即为命令行工具），不显示每次都变的挂载点；
//! tar.gz 等给出手动链接的命令。

use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Task, TestSupportExt as _, Window, div,
    prelude::FluentBuilder as _,
};
use termlet_core::userpath;

use super::CliEnv;
use crate::app_state::AppState;
use crate::install_kind::{LinuxInstall, SYSTEM_CLI, linux_install};
use crate::notify::{self, Notice};
use crate::prose::prose;
use crate::t;

/// 示例命令。
const EXAMPLES: &str = "termlet ls
termlet exec web \"df -h && uptime\"
echo 'cd /srv && ls' | termlet exec web --stdin
termlet upload web ./app.tar.gz /tmp/
termlet download web /var/log/nginx/error.log .";

/// PATH 的状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PathState {
    /// 开发构建：应用程序旁边没有命令行工具
    Missing,
    /// 开发构建：命令行工具在应用程序旁边（不改 PATH）
    DevBuild,
    /// 这个平台不能由 Termlet 修改 PATH
    Unsupported,
    /// 便携版：不修改 PATH
    Portable,
    /// Linux 的 deb、rpm：已是系统命令 `/usr/bin/termlet`
    SystemCommand,
    /// Linux 的 AppImage：链接为 termlet 即可
    AppImage,
    Checking,
    InPath(bool),
    /// 读取 PATH 失败（系统的报错）
    Failed(String),
}

pub(super) struct ToolSection {
    /// 命令行工具的完整路径与所在的目录；读不到应用程序的路径时为 `None`
    paths: Option<(PathBuf, PathBuf)>,
    pub(super) state: PathState,
    busy: bool,
    _task: Option<Task<()>>,
}

impl ToolSection {
    pub(super) fn new(env: &CliEnv, cx: &mut Context<Self>) -> Self {
        let paths = env
            .exe
            .as_deref()
            .map(|exe| (userpath::cli_path(exe), userpath::cli_dir(exe)));
        let exists = paths.as_ref().is_some_and(|(path, _)| path.is_file());
        let linux = env.exe.as_deref().and_then(|exe| {
            linux_install(exe, env.system_link.as_deref(), env.appimage.as_deref())
        });
        let mut this = Self {
            paths,
            state: PathState::Missing,
            busy: false,
            _task: None,
        };
        let dev = if exists {
            None
        } else {
            env.exe.as_deref().and_then(super::dev_cli)
        };
        if let Some(dev) = dev {
            let dir = dev.parent().map(Path::to_path_buf).unwrap_or_default();
            this.paths = Some((dev, dir));
            this.state = PathState::DevBuild;
        } else if exists && env.portable {
            this.state = PathState::Portable;
        } else if let Some(linux) = linux {
            // 显示的是用户要用的那个文件：系统命令，或 AppImage 文件本身
            let shown = match linux {
                LinuxInstall::System => {
                    this.state = PathState::SystemCommand;
                    PathBuf::from(SYSTEM_CLI)
                }
                LinuxInstall::AppImage(image) => {
                    this.state = PathState::AppImage;
                    image
                }
            };
            let dir = shown.parent().map(Path::to_path_buf).unwrap_or_default();
            this.paths = Some((shown, dir));
        } else if exists && userpath::supported() {
            this.state = PathState::Checking;
            this.check(cx);
        } else if exists {
            this.state = PathState::Unsupported;
        }
        this
    }

    /// 显示的路径（测试用）。
    #[cfg(test)]
    pub(super) fn shown_path(&self) -> Option<&Path> {
        self.paths.as_ref().map(|(path, _)| path.as_path())
    }

    /// 在后台读取 PATH（Windows 注册表）。
    fn check(&mut self, cx: &mut Context<Self>) {
        let Some((_, dir)) = self.paths.clone() else {
            return;
        };
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { userpath::contains(&dir) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(contains) => PathState::InPath(contains),
                    Err(e) => PathState::Failed(e.to_string()),
                };
                cx.notify();
            });
        }));
    }

    /// 加入或移出 PATH，并记下（或删除）由 Termlet 加入的目录：重装换了位置时据此跟随。
    fn change(&mut self, add: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, dir)) = self.paths.clone() else {
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        tracing::info!(add, dir = %dir.display(), "命令行工具：修改 PATH");
        let record = AppState::global(cx).data_dir.cli_path_record();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    if add {
                        userpath::add(&dir)?;
                    } else {
                        userpath::remove(&dir)?;
                    }
                    if let Err(e) = userpath::record(&record, add.then_some(dir.as_path())) {
                        tracing::warn!("记录 PATH 中的命令行工具目录失败：{e}");
                    }
                    userpath::contains(&dir)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                let notice = match result {
                    Ok(contains) => {
                        tracing::info!(add, in_path = contains, "命令行工具：已修改 PATH");
                        this.state = PathState::InPath(contains);
                        Notice::success(if add {
                            t!("cli_ui.tool.added")
                        } else {
                            t!("cli_ui.tool.removed")
                        })
                    }
                    Err(e) => {
                        tracing::warn!("修改 PATH 失败：{e}");
                        Notice::error(t!("cli_ui.tool.change_failed", error = e.to_string()))
                    }
                };
                notify::show(notice, window, cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn render_path(&self, path: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let copied = path.to_string();
        h_flex()
            .gap_2()
            .child(
                div()
                    .id("cli-tool-path")
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .rounded(theme.radius)
                    .bg(theme.muted)
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .truncate()
                    .child(path.to_string()),
            )
            .child(
                Button::new("cli-tool-copy-path")
                    .ghost()
                    .small()
                    .icon(IconName::Copy)
                    .tooltip(t!("cli_ui.tool.copy_path"))
                    .on_click(move |_, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                        notify::show(Notice::success(t!("cli_ui.tool.path_copied")), window, cx);
                    }),
            )
    }

    fn render_state(&self, path: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mono = theme.mono_font_family.clone();
        let code_bg = theme.muted;
        let radius = theme.radius;
        let danger = theme.danger;
        match &self.state {
            PathState::Missing => div().into_any_element(),
            PathState::Checking => div()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("cli_ui.tool.checking")))
                .into_any_element(),
            PathState::Failed(error) => div()
                .text_xs()
                .text_color(danger)
                .child(prose(t!("cli_ui.tool.check_failed", error = error.clone())))
                .into_any_element(),
            PathState::DevBuild => div()
                .id("cli-tool-dev-found")
                .test_support()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("cli_ui.tool.dev_build_found")))
                .into_any_element(),
            PathState::Portable => div()
                .id("cli-tool-portable")
                .test_support()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("cli_ui.tool.portable")))
                .into_any_element(),
            PathState::SystemCommand => div()
                .id("cli-tool-system")
                .test_support()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("cli_ui.tool.system_command")))
                .into_any_element(),
            PathState::AppImage => v_flex()
                .id("cli-tool-appimage")
                .test_support()
                .gap_1()
                .text_xs()
                .child(
                    div()
                        .text_color(muted)
                        .child(prose(t!("cli_ui.tool.appimage"))),
                )
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded(radius)
                        .bg(code_bg)
                        .font_family(mono)
                        .child(format!("ln -s \"{path}\" ~/.local/bin/termlet")),
                )
                .into_any_element(),
            PathState::Unsupported => v_flex()
                .gap_1()
                .text_xs()
                .child(
                    div()
                        .text_color(muted)
                        .child(prose(t!("cli_ui.tool.manual"))),
                )
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded(radius)
                        .bg(code_bg)
                        .font_family(mono)
                        .child(format!("sudo ln -sf \"{path}\" /usr/local/bin/termlet")),
                )
                .into_any_element(),
            PathState::InPath(contains) => {
                let contains = *contains;
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .id("cli-tool-path-state")
                            .test_support()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(if contains {
                                t!("cli_ui.tool.in_path")
                            } else {
                                t!("cli_ui.tool.not_in_path")
                            })),
                    )
                    .child(
                        Button::new("cli-tool-path-toggle")
                            .outline()
                            .small()
                            .label(if contains {
                                t!("cli_ui.tool.remove")
                            } else {
                                t!("cli_ui.tool.add")
                            })
                            .loading(self.busy)
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.change(!contains, window, cx)
                            })),
                    )
                    .into_any_element()
            }
        }
    }
}

impl Render for ToolSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mono = theme.mono_font_family.clone();
        let code_bg = theme.muted;
        let radius = theme.radius;
        let path: Option<SharedString> = match (&self.state, &self.paths) {
            (PathState::Missing, _) | (_, None) => None,
            (_, Some((path, _))) => Some(path.display().to_string().into()),
        };
        v_flex()
            .gap_2()
            .map(|this| match &path {
                None => this.child(
                    div()
                        .id("cli-tool-dev-build")
                        .test_support()
                        .text_xs()
                        .text_color(muted)
                        .child(prose(t!("cli_ui.tool.dev_build"))),
                ),
                Some(path) => this
                    .child(self.render_path(path, cx))
                    .child(self.render_state(path, cx)),
            })
            .child(
                v_flex()
                    .gap_1()
                    .text_xs()
                    .child(div().text_color(muted).child(t!("cli_ui.tool.examples")))
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded(radius)
                            .bg(code_bg)
                            .font_family(mono)
                            .children(EXAMPLES.lines().map(|line| div().child(line))),
                    ),
            )
    }
}
