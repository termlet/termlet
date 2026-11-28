//! 连接过程中的询问：核心把问题（主机指纹、密码、私钥密码、键盘交互、执行代理命令）送到窗口，这里弹出对话框，
//! 用户回答（或关闭对话框）后把答案交还核心。一次只显示一个问题，按到达的顺序。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::oneshot;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::{
    App, AppContext as _, Entity, Focusable as _, InteractiveElement as _, ParentElement as _,
    SharedString, Styled as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::secret::SecretString;
use termlet_core::ssh::{Answer, Prompt, Prompter};

use crate::prose::prose;
use crate::t;
use crate::ui_scale::DialogTitleExt as _;

/// 一个等待回答的问题。
pub struct PromptRequest {
    pub prompt: Prompt,
    pub reply: oneshot::Sender<Answer>,
}

/// 核心一侧的发问者：把问题送进窗口的队列，等待回答（窗口关了就当作取消）。
pub fn prompter(queue: async_channel::Sender<PromptRequest>) -> Prompter {
    Arc::new(move |prompt| {
        let queue = queue.clone();
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            if queue.send(PromptRequest { prompt, reply }).await.is_err() {
                return Answer::Cancel;
            }
            answer.await.unwrap_or(Answer::Cancel)
        })
    })
}

/// 弹出对话框回答一个问题；返回的接收端在对话框关闭后完成。
pub fn open(request: PromptRequest, window: &mut Window, cx: &mut App) -> oneshot::Receiver<()> {
    let (done_tx, done) = oneshot::channel();
    let reply = Rc::new(RefCell::new(Some(request.reply)));
    let done_tx = Rc::new(RefCell::new(Some(done_tx)));
    let asked = prompt_kind(&request.prompt);
    log_prompt(&request.prompt);
    let respond = {
        let reply = reply.clone();
        move |answer: Answer| {
            if let Some(reply) = reply.borrow_mut().take() {
                // 只记回答的种类，不记输入的内容
                tracing::info!(asked, answer = answer_kind(&answer), "询问已回答");
                let _ = reply.send(answer);
            }
        }
    };

    // 输入框在打开前建好（对话框的内容在每次重绘时重建）
    let inputs: Vec<(Entity<InputState>, SharedString)> = match &request.prompt {
        Prompt::Password { .. } | Prompt::Passphrase { .. } => {
            vec![(
                cx.new(|cx| InputState::new(window, cx).masked(true)),
                SharedString::default(),
            )]
        }
        Prompt::KeyboardInteractive { prompts, .. } => prompts
            .iter()
            .map(|(label, echo)| {
                (
                    cx.new(|cx| InputState::new(window, cx).masked(!echo)),
                    SharedString::from(label.trim().to_string()),
                )
            })
            .collect(),
        Prompt::HostKey(_) | Prompt::ProxyCommand(_) => Vec::new(),
    };
    let first_focus = Rc::new(Cell::new(true));
    let prompt = request.prompt;
    let width = rems(28.75).to_pixels(window.rem_size());

    window.open_dialog(cx, move |dialog, window, cx| {
        if first_focus.replace(false)
            && let Some((input, _)) = inputs.first()
        {
            input.focus_handle(cx).focus(window, cx);
        }
        let (title, description, ok_label) = texts(&prompt);
        let mut body = v_flex().gap_3().w_full().child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(prose(description)),
        );
        if let Prompt::HostKey(host_key) = &prompt {
            body = body.child(
                v_flex()
                    .gap_1()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .text_sm()
                    .child(SharedString::from(host_key.algorithm.clone()))
                    .child(
                        div()
                            .font_family("JetBrains Mono")
                            .child(SharedString::from(host_key.fingerprint.clone())),
                    ),
            );
        }
        if let Prompt::ProxyCommand(proxy) = &prompt {
            let theme = cx.theme();
            body = body
                .child(
                    div()
                        .id("prompt-proxy-command")
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .text_sm()
                        .font_family(theme.mono_font_family.clone())
                        .child(SharedString::from(proxy.command.clone())),
                )
                .when_some(proxy.from_group.clone(), |body, group| {
                    body.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(prose(t!("prompt.proxy_command.from_group", group = group))),
                    )
                });
        }
        for (index, (input, label)) in inputs.iter().enumerate() {
            body = body.child(
                v_flex()
                    .gap_1()
                    .when(!label.is_empty(), |this| {
                        this.child(div().text_sm().child(label.clone()))
                    })
                    .child(Input::new(input).id(("prompt-input", index)).mask_toggle()),
            );
        }

        let ok = {
            let respond = respond.clone();
            let inputs: Vec<Entity<InputState>> = inputs.iter().map(|(i, _)| i.clone()).collect();
            let kind = PromptKind::of(&prompt);
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                let values: Vec<SecretString> = inputs
                    .iter()
                    .map(|input| SecretString::new(input.read(cx).value().to_string()))
                    .collect();
                respond(match kind {
                    PromptKind::HostKey => Answer::AcceptHostKey,
                    PromptKind::ProxyCommand => Answer::AllowProxyCommand,
                    PromptKind::Secret => {
                        Answer::Secret(values.into_iter().next().unwrap_or_default())
                    }
                    PromptKind::Responses => Answer::Responses(values),
                });
                true
            }
        };
        let close = {
            let respond = respond.clone();
            let done_tx = done_tx.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, _: &mut App| {
                respond(Answer::Cancel);
                if let Some(done) = done_tx.borrow_mut().take() {
                    let _ = done.send(());
                }
            }
        };

        dialog
            .titled(title)
            .w(width)
            .overlay_closable(false)
            .child(body)
            .on_ok(ok)
            .on_close(close)
            .footer(
                DialogFooter::new()
                    .child(
                        DialogClose::new()
                            .child(Button::new("prompt-cancel").label(t!("common.cancel"))),
                    )
                    .child(
                        DialogAction::new()
                            .child(Button::new("prompt-ok").primary().label(ok_label)),
                    ),
            )
    });
    done
}

/// 日志中询问的种类。
fn prompt_kind(prompt: &Prompt) -> &'static str {
    match prompt {
        Prompt::HostKey(p) if !p.known_algorithms.is_empty() => "host_key_other_type",
        Prompt::HostKey(_) => "host_key",
        Prompt::Password { .. } => "password",
        Prompt::Passphrase { .. } => "passphrase",
        Prompt::KeyboardInteractive { .. } => "keyboard_interactive",
        Prompt::ProxyCommand(_) => "proxy_command",
    }
}

/// 弹出询问时写一条日志：问的是哪台主机的什么（不含任何输入）。
fn log_prompt(prompt: &Prompt) {
    match prompt {
        Prompt::HostKey(p) => tracing::info!(
            host = %p.host,
            port = p.port,
            algorithm = %p.algorithm,
            fingerprint = %p.fingerprint,
            known_algorithms = ?p.known_algorithms,
            "询问：确认主机指纹"
        ),
        Prompt::Password {
            host,
            username,
            retry,
            ..
        } => tracing::info!(host = %host, user = %username, retry, "询问：密码"),
        Prompt::Passphrase { key, retry, .. } => {
            tracing::info!(key = %key, retry, "询问：私钥密码")
        }
        Prompt::KeyboardInteractive { host, prompts, .. } => tracing::info!(
            host = %host,
            prompts = prompts.len(),
            "询问：键盘交互认证"
        ),
        // 只记程序名：命令的参数中可能带令牌
        Prompt::ProxyCommand(p) => tracing::info!(
            host = %p.host,
            program = %termlet_core::ssh::proxy_command::split_program(&p.command).0,
            from_group = p.from_group.is_some(),
            "询问：执行代理命令"
        ),
    }
}

/// 日志中回答的种类。
fn answer_kind(answer: &Answer) -> &'static str {
    match answer {
        Answer::AcceptHostKey => "accept_host_key",
        Answer::AllowProxyCommand => "allow_proxy_command",
        Answer::Cancel => "cancel",
        Answer::Secret(_) => "secret",
        Answer::Responses(_) => "responses",
    }
}

#[derive(Clone, Copy)]
enum PromptKind {
    HostKey,
    ProxyCommand,
    Secret,
    Responses,
}

impl PromptKind {
    fn of(prompt: &Prompt) -> Self {
        match prompt {
            Prompt::HostKey(_) => PromptKind::HostKey,
            Prompt::Password { .. } | Prompt::Passphrase { .. } => PromptKind::Secret,
            Prompt::KeyboardInteractive { .. } => PromptKind::Responses,
            Prompt::ProxyCommand(_) => PromptKind::ProxyCommand,
        }
    }
}

/// 标题、说明、确认按钮的文字。
fn texts(prompt: &Prompt) -> (SharedString, SharedString, SharedString) {
    match prompt {
        Prompt::HostKey(p) if !p.known_algorithms.is_empty() => (
            t!(
                "prompt.host_key.other_type_title",
                host = host_label(&p.host, p.port)
            ),
            t!(
                "prompt.host_key.other_type_description",
                known = p.known_algorithms.join(", "),
                algorithm = p.algorithm
            ),
            t!("prompt.host_key.accept_other_type"),
        ),
        Prompt::HostKey(p) => (
            t!("prompt.host_key.title", host = host_label(&p.host, p.port)),
            t!("prompt.host_key.description"),
            t!("prompt.host_key.accept"),
        ),
        Prompt::Password {
            host,
            username,
            retry,
            ..
        } => (
            t!("prompt.password.title", user = username, host = host),
            if *retry {
                t!("prompt.password.retry")
            } else {
                t!("prompt.password.description")
            },
            t!("prompt.connect"),
        ),
        Prompt::Passphrase { key, retry, .. } => (
            t!("prompt.passphrase.title"),
            if *retry {
                t!("prompt.passphrase.retry")
            } else if key.is_empty() {
                t!("prompt.passphrase.description_unnamed")
            } else {
                t!("prompt.passphrase.description", key = key)
            },
            t!("prompt.connect"),
        ),
        Prompt::KeyboardInteractive {
            host,
            name,
            instructions,
            ..
        } => (
            if name.trim().is_empty() {
                t!("prompt.interactive.title", host = host)
            } else {
                SharedString::from(name.trim().to_string())
            },
            if instructions.trim().is_empty() {
                t!("prompt.interactive.description")
            } else {
                SharedString::from(instructions.trim().to_string())
            },
            t!("prompt.continue"),
        ),
        Prompt::ProxyCommand(p) => (
            t!("prompt.proxy_command.title", host = p.host),
            t!("prompt.proxy_command.description"),
            t!("prompt.proxy_command.allow"),
        ),
    }
}

/// 默认端口只写主机，否则 `主机:端口`（IPv6 加方括号，`::1:2222` 分不清哪段是端口）。
fn host_label(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        termlet_core::model::join_host_port(host, port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_label_brackets_ipv6_with_a_port() {
        assert_eq!(host_label("::1", 22), "::1");
        assert_eq!(host_label("::1", 2222), "[::1]:2222");
        assert_eq!(host_label("db", 2222), "db:2222");
    }
}
