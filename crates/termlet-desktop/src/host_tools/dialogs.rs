//! 需要 root 时的对话框：说明原因、在终端中实际执行的命令，可以改在终端中执行。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::{
    App, InteractiveElement as _, ParentElement as _, SharedString, Styled as _,
    TestSupportExt as _, WeakEntity, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::hosttools::{Action, SudoState, TerminalRequest, terminal_command};

use super::errors;
use super::model::Page;
use super::panel::HostToolsPanel;
use super::shared::{
    DOCKER_GROUP_COMMAND, OpenDialogs, bullet, confirm_command, confirm_host, copy_command,
};
use super::target::HostLabel;
use crate::prose::prose;
use crate::t;
use crate::terminal_panel::TerminalPanel;

/// 操作需要 root、又没有免密码 sudo：说明原因，可以改在终端中执行。
pub(super) struct RootRequest {
    pub sudo: SudoState,
    pub action: Action,
    /// Docker 的操作：另提示加入 docker 组
    pub docker: bool,
    /// 操作的说明（“结束进程 sshd（1）”）
    pub title: SharedString,
    /// 审计记录中的名称
    pub label: String,
    /// 发起操作的终端：对话框开着时焦点可能换到别的终端，“在终端中执行”只用它
    pub terminal: WeakEntity<TerminalPanel>,
    /// 发起操作的终端的名称（审计记录的主机）
    pub host: String,
    /// 对话框中写明的主机（面板已经换了终端时没有）
    pub host_label: Option<HostLabel>,
    /// 发起操作的页：在终端中执行的命令结束后重读它
    pub page: Page,
}

/// 信息行尾的锁图标点开的说明（不执行任何操作）：这条连接上的用户不是 root、也没有免密码 sudo，`description` 说明
/// 这一页因此不能做什么；之后是 sudo 的情况与取得 root 权限的办法（与“需要 root 权限”的对话框相同）。
pub(super) fn open_root_info(
    sudo: SudoState,
    description: SharedString,
    host: Option<HostLabel>,
    dialogs: OpenDialogs,
    window: &mut Window,
    cx: &mut App,
) {
    let width = rems(30.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, _, cx| {
        let muted = cx.theme().muted_foreground;
        dialog
            .title(t!("tools.root.title"))
            .w(width)
            .child(
                v_flex()
                    .id("tools-root-info")
                    .test_support()
                    .gap_2()
                    .text_sm()
                    .children(host.as_ref().map(|host| confirm_host(host, cx)))
                    .child(prose(description.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(errors::sudo_text(sudo))),
                    )
                    .child(
                        v_flex()
                            .gap_1p5()
                            .text_xs()
                            .text_color(muted)
                            .child(bullet(prose(t!("tools.root.info_terminal"))))
                            .child(bullet(prose(t!("tools.root.login"))))
                            .child(bullet(prose(t!("tools.root.sudoers")))),
                    ),
            )
            .footer(
                DialogFooter::new().child(
                    DialogClose::new()
                        .child(Button::new("tools-root-info-close").label(t!("common.close"))),
                ),
            )
    });
    dialogs.opened(window, cx);
}

/// 打开“需要 root 权限”的对话框。
pub(super) fn open_root(
    request: RootRequest,
    panel: WeakEntity<HostToolsPanel>,
    dialogs: OpenDialogs,
    window: &mut Window,
    cx: &mut App,
) {
    let request = std::rc::Rc::new(request);
    let width = rems(30.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let sudo = request.sudo;
        // 没有 sudo、不允许用 sudo 时只能用 su
        let su_only = matches!(sudo, SudoState::Denied | SudoState::NotInstalled);
        // 在终端中实际执行的命令（与审计记录的写法一致）；sudo 要密码时另可改用 su
        let command = |su: bool| {
            terminal_command(&TerminalRequest::Action {
                action: request.action.clone(),
                root: Some(sudo),
                su,
            })
            .ok()
        };
        let primary = command(su_only);
        let alternative = (sudo == SudoState::Password)
            .then(|| command(true))
            .flatten();
        let run = |su: bool| {
            let request = request.clone();
            let panel = panel.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                let terminal_request = TerminalRequest::Action {
                    action: request.action.clone(),
                    root: Some(request.sudo),
                    su,
                };
                let _ = panel.update(cx, |panel, cx| {
                    panel.run_root(&request, terminal_request, window, cx)
                });
                window.close_dialog(cx);
            }
        };
        dialog
            .title(t!("tools.root.title"))
            .w(width)
            .child(
                v_flex()
                    .id("tools-root-dialog")
                    .test_support()
                    .gap_2()
                    .text_sm()
                    .children(
                        request
                            .host_label
                            .as_ref()
                            .map(|host| confirm_host(host, cx)),
                    )
                    .child(prose(t!(
                        "tools.root.description",
                        action = request.title.clone()
                    )))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(errors::sudo_text(sudo))),
                    )
                    .children(primary.as_deref().map(|command| {
                        confirm_command(
                            "tools-root-command",
                            t!("tools.root.will_run"),
                            command,
                            cx,
                        )
                    }))
                    .children(alternative.as_deref().map(|command| {
                        confirm_command(
                            "tools-root-command-su",
                            t!("tools.root.will_run_su"),
                            command,
                            cx,
                        )
                    }))
                    .child(
                        v_flex()
                            .gap_1p5()
                            .text_xs()
                            .text_color(muted)
                            .child(bullet(if su_only {
                                t!("tools.root.terminal_su")
                            } else {
                                t!("tools.root.terminal")
                            }))
                            .when(sudo == SudoState::Password, |this| {
                                this.child(bullet(t!("tools.root.terminal_su_alt")))
                            })
                            .when(request.docker, |this| {
                                this.child(bullet(
                                    v_flex()
                                        .gap_1()
                                        .child(prose(t!("tools.root.docker_group")))
                                        .child(copy_command(
                                            "tools-root-copy-docker-group",
                                            DOCKER_GROUP_COMMAND,
                                            cx,
                                        )),
                                ))
                            })
                            .child(bullet(t!("tools.root.login")))
                            .child(bullet(t!("tools.root.sudoers"))),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        DialogClose::new()
                            .child(Button::new("tools-root-close").label(t!("common.close"))),
                    )
                    .when(sudo == SudoState::Password, |this| {
                        this.child(
                            Button::new("tools-root-run-su")
                                .outline()
                                .label(t!("tools.root.run_su"))
                                .on_click(run(true)),
                        )
                    })
                    .child(
                        Button::new("tools-root-run")
                            .primary()
                            .icon(IconName::SquareTerminal)
                            .label(t!("tools.root.run"))
                            .on_click(run(su_only)),
                    ),
            )
    });
    dialogs.opened(window, cx);
}
