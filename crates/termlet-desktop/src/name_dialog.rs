//! 输入一个名称的对话框（保存为工作区、重命名、新建文件夹等）。
//!
//! 打开时输入框获得焦点（可以全选原内容）；名称为空时不提交；提交可以是异步的（远程的重命名），期间按钮显示忙碌，
//! 失败时在对话框内显示原因并保持打开。Enter 等于点“确定”。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _, rems,
};

use crate::dialog_token::{DialogToken, close_own};
use crate::prose::prose;
use crate::t;
use crate::ui_scale::DialogTitleExt as _;

/// 提交名称：返回的任务完成时 `Ok` 关闭对话框，`Err` 显示原因。
pub type Submit = Box<dyn Fn(String, &mut Window, &mut App) -> Task<Result<(), SharedString>>>;

/// 对话框的内容。
pub struct NameDialog {
    pub title: SharedString,
    pub value: String,
    pub placeholder: SharedString,
    /// 打开时全选原内容（重命名）
    pub select_all: bool,
    pub ok_label: SharedString,
    /// 输入框的元素 ID（UI 测试用）
    pub input_id: &'static str,
}

/// 打开对话框。
pub fn open(dialog: NameDialog, submit: Submit, window: &mut Window, cx: &mut App) {
    let NameDialog {
        title,
        value,
        placeholder,
        select_all,
        ok_label,
        input_id,
    } = dialog;
    let form = cx.new(|cx| {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(value)
        });
        // 名称为空时“确定”不可用：输入变化时重新渲染
        let subscription = cx.observe(&name, |_, _, cx| cx.notify());
        NameForm {
            name,
            submit,
            ok_label,
            input_id,
            busy: false,
            error: None,
            dialog: None,
            _task: None,
            _subscription: subscription,
        }
    });
    let first = Rc::new(Cell::new(true));
    let width = rems(24.).to_pixels(window.rem_size());
    let owner = form.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            let name = form.read(cx).name.clone();
            name.focus_handle(cx).focus(window, cx);
            if select_all {
                name.update(cx, |state, cx| state.select_all(window, cx));
            }
        }
        // 输入框中按 Enter 由对话框转给 on_ok：提交，完成后由表单自己关闭对话框
        let ok = {
            let form = form.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                form.update(cx, |form, cx| form.submit(window, cx));
                false
            }
        };
        dialog
            .titled(title.clone())
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
            .on_ok(ok)
    });
    let token = DialogToken::take(window, cx);
    owner.update(cx, |form, _| form.dialog = token);
}

struct NameForm {
    name: Entity<InputState>,
    submit: Submit,
    ok_label: SharedString,
    input_id: &'static str,
    busy: bool,
    error: Option<SharedString>,
    /// 这个表单所在的对话框（完成后只关它）
    dialog: Option<DialogToken>,
    _task: Option<Task<()>>,
    _subscription: Subscription,
}

impl NameForm {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let raw = self.name.read(cx).value().to_string();
        let name = raw.trim().to_string();
        if name.is_empty() {
            self.error = Some(t!("name_dialog.required"));
            cx.notify();
            return;
        }
        self.busy = true;
        self.error = None;
        let task = (self.submit)(name, window, cx);
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                this._task = None;
                match result {
                    Ok(()) => {
                        // 正在关闭：不能再提交一次
                        this.busy = true;
                        close_own(this.dialog.as_ref(), window, cx);
                    }
                    Err(error) => {
                        this.error = Some(error);
                        cx.notify();
                    }
                }
            });
        }));
        cx.notify();
    }
}

impl Render for NameForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.name.read(cx).value().trim().is_empty();
        v_flex()
            .gap_3()
            .child(Input::new(&self.name).id(self.input_id))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .when_some(self.error.clone(), |this, error| this.child(prose(error))),
                    )
                    .child(
                        Button::new("name-dialog-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("name-dialog-ok")
                            .primary()
                            .label(self.ok_label.clone())
                            .loading(self.busy)
                            .disabled(self.busy || empty)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
