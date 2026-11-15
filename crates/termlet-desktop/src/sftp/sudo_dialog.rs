//! 以 root 身份打开 SFTP 时 sudo 要密码：弹出对话框（与连接时的询问一样，一次一个，回答或关闭后交还核心）。
//! 这台主机保存了登录密码时可以一键使用它（sudo 问的是登录用户自己的密码）；它刚被证明不对时不再提供。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::oneshot;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, v_flex};
use gpui_kit::{
    App, AppContext as _, Entity, Focusable as _, ParentElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, rems,
};
use termlet_core::secret::SecretString;
use termlet_core::sftp::{SudoAsker, SudoPrompt};

use crate::prose::prose;
use crate::t;

/// 一个等待回答的询问。
pub struct SudoRequest {
    pub prompt: SudoPrompt,
    pub reply: oneshot::Sender<Option<SecretString>>,
}

/// 核心一侧的询问者：把询问送进队列（由 [`answer_in_order`] 逐个弹出），等待回答（队列不在了就当作取消）。
pub fn asker(queue: async_channel::Sender<SudoRequest>) -> SudoAsker {
    Arc::new(move |prompt| {
        let queue = queue.clone();
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            if queue.send(SudoRequest { prompt, reply }).await.is_err() {
                return None;
            }
            answer.await.ok().flatten()
        })
    })
}

/// 按到达的顺序逐个弹出询问，直到队列关闭。`saved` 为这台主机保存的登录密码。
pub async fn answer_in_order(
    requests: async_channel::Receiver<SudoRequest>,
    saved: Option<SecretString>,
    cx: &mut gpui_kit::AsyncWindowContext,
) {
    // 上一次回答的是保存的密码：再问（它不对）时不再提供
    let mut saved_used = false;
    while let Ok(request) = requests.recv().await {
        let offer = saved
            .clone()
            .filter(|_| !(request.prompt.retry && saved_used));
        let Ok(done) = cx.update(|window, cx| open(request, offer, window, cx)) else {
            break;
        };
        saved_used = done.await.unwrap_or(false);
    }
}

/// 弹出对话框；返回的接收端在对话框关闭后完成（为真时回答的是保存的密码）。
pub fn open(
    request: SudoRequest,
    saved: Option<SecretString>,
    window: &mut Window,
    cx: &mut App,
) -> oneshot::Receiver<bool> {
    let SudoRequest { prompt, reply } = request;
    let (done_tx, done) = oneshot::channel();
    let reply = Rc::new(RefCell::new(Some(reply)));
    let done_tx = Rc::new(RefCell::new(Some(done_tx)));
    // 只记回答的种类，不记密码
    let respond = {
        let (reply, done_tx) = (reply.clone(), done_tx.clone());
        move |answer: Option<SecretString>, from_saved: bool| {
            if let Some(reply) = reply.borrow_mut().take() {
                tracing::info!(
                    answered = answer.is_some(),
                    saved = from_saved,
                    "sudo 密码的询问已回答"
                );
                let _ = reply.send(answer);
            }
            if let Some(done) = done_tx.borrow_mut().take() {
                let _ = done.send(from_saved);
            }
        }
    };
    let input: Entity<InputState> = cx.new(|cx| InputState::new(window, cx).masked(true));
    let first_focus = Rc::new(Cell::new(true));
    let width = rems(28.).to_pixels(window.rem_size());

    window.open_dialog(cx, move |dialog, window, cx| {
        if first_focus.replace(false) {
            input.focus_handle(cx).focus(window, cx);
        }
        let description = if prompt.retry {
            t!("sftp.sudo.retry")
        } else {
            t!(
                "sftp.sudo.description",
                host = prompt.host,
                user = prompt.username
            )
        };
        let body = v_flex()
            .gap_3()
            .w_full()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(prose(description)),
            )
            .child(Input::new(&input).id("sudo-password").mask_toggle())
            .when_some(saved.clone(), |this, saved| {
                let respond = respond.clone();
                this.child(
                    div().child(
                        Button::new("sudo-use-saved")
                            .ghost()
                            .small()
                            .label(t!("sftp.sudo.use_saved"))
                            .on_click(move |_, window, cx| {
                                respond(Some(saved.clone()), true);
                                window.close_dialog(cx);
                            }),
                    ),
                )
            });
        let ok = {
            let (respond, input) = (respond.clone(), input.clone());
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                let password = SecretString::new(input.read(cx).value().to_string());
                respond(Some(password), false);
                true
            }
        };
        let close = {
            let respond = respond.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, _: &mut App| respond(None, false)
        };
        dialog
            .title(t!("sftp.sudo.title"))
            .w(width)
            .overlay_closable(false)
            .child(body)
            .on_ok(ok)
            .on_close(close)
            .footer(
                DialogFooter::new()
                    .child(
                        DialogClose::new()
                            .child(Button::new("sudo-cancel").label(t!("common.cancel"))),
                    )
                    .child(
                        DialogAction::new()
                            .child(Button::new("sudo-ok").primary().label(t!("sftp.sudo.open"))),
                    ),
            )
    });
    done
}
