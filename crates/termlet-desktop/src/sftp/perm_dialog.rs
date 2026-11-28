//! 修改权限对话框（右键“权限…”，作用于所选的全部项）。
//!
//! - 所有者 / 组 / 其他人的读、写、执行，以及 SUID、SGID、粘滞位；八进制输入框与复选框双向同步。
//! - 多选时各项不同的位显示为横线（三态），不改动时各项保持原样；提交时按每一项原来的权限分别计算。
//! - 所选项中有目录时可以同时修改其中的文件与子目录；有横线的位时，递归与符号链接都不能提交（无从得知原来的权限）。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, prelude::FluentBuilder as _, rems,
};

use super::perm::{self, BitState, PermChange};
use crate::prose::prose;
use crate::t;
use crate::ui_scale::DialogTitleExt as _;

/// 要修改的一项。
#[derive(Clone, Debug)]
pub struct PermItem {
    pub name: String,
    pub path: String,
    /// 原来的权限位（符号链接为它指向的文件的）
    pub perm: u32,
    pub is_dir: bool,
    pub is_link: bool,
}

/// 修改失败：之前已改好的项数与原因。
pub struct ChmodFailure {
    pub done: usize,
    pub error: SharedString,
}

/// 提交：按组（新权限、路径）逐组修改，`recursive` 为同时修改目录中的内容。
pub type Submit = Box<
    dyn Fn(Vec<(u32, Vec<String>)>, bool, &mut Window, &mut App) -> Task<Result<(), ChmodFailure>>,
>;

/// 打开对话框。
pub fn open(items: Vec<PermItem>, submit: Submit, window: &mut Window, cx: &mut App) {
    if items.is_empty() {
        return;
    }
    let form = cx.new(|cx| PermForm::new(items, submit, window, cx));
    let width = rems(24.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, _, _| {
        let ok = {
            let form = form.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                form.update(cx, |form, cx| form.submit(window, cx));
                false
            }
        };
        dialog
            .titled(t!("sftp.perm.title"))
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
            .on_ok(ok)
    });
}

/// 3×3 的权限位：行为所有者、组、其他人，列为读、写、执行。
const GRID: [(u32, u32, u32); 3] = [
    (0o400, 0o200, 0o100),
    (0o040, 0o020, 0o010),
    (0o004, 0o002, 0o001),
];
const SPECIAL: [u32; 3] = [0o4000, 0o2000, 0o1000];

struct PermForm {
    items: Vec<PermItem>,
    submit: Submit,
    change: PermChange,
    /// 打开时各项不同的位（可以回到“保持原样”）
    mixed_at_start: u32,
    octal: Entity<InputState>,
    recursive: bool,
    busy: bool,
    error: Option<SharedString>,
    _task: Option<Task<()>>,
    _subscription: Subscription,
}

impl PermForm {
    fn new(
        items: Vec<PermItem>,
        submit: Submit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let perms: Vec<u32> = items.iter().map(|i| i.perm).collect();
        let change = PermChange::initial(&perms);
        let octal = cx.new(|cx| InputState::new(window, cx).default_value(change.display()));
        // 输入合法的八进制时立即把全部的位设为确定的值
        let subscription =
            cx.subscribe(&octal, |this: &mut Self, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    if let Some(mode) = perm::parse_octal(&input.read(cx).value()) {
                        this.change = PermChange::exact(mode);
                    }
                    cx.notify();
                }
            });
        Self {
            items,
            submit,
            mixed_at_start: change.mixed(),
            change,
            octal,
            recursive: false,
            busy: false,
            error: None,
            _task: None,
            _subscription: subscription,
        }
    }

    fn toggle(&mut self, bit: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.change = self.change.cycle(bit, self.mixed_at_start);
        let display = self.change.display();
        self.octal
            .update(cx, |input, cx| input.set_value(display, window, cx));
        cx.notify();
    }

    fn has_dir(&self) -> bool {
        self.items.iter().any(|i| i.is_dir && !i.is_link)
    }

    /// 八进制输入框的内容合法，或与复选框同步出来的写法相同（含 `?`）。
    fn octal_valid(&self, cx: &App) -> bool {
        let text = self.octal.read(cx).value();
        let text = text.trim();
        perm::parse_octal(text).is_some() || text == self.change.display()
    }

    /// 不能提交的原因（有横线的位时）。
    fn blocked(&self) -> Option<SharedString> {
        if self.change.mixed() == 0 {
            return None;
        }
        if self.recursive && self.has_dir() {
            Some(t!("sftp.perm.mixed_recursive"))
        } else if self.items.iter().any(|i| i.is_link) {
            Some(t!("sftp.perm.mixed_link"))
        } else {
            None
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || !self.octal_valid(cx) || self.blocked().is_some() {
            return;
        }
        let items: Vec<(&str, u32)> = self
            .items
            .iter()
            .map(|i| (i.path.as_str(), i.perm))
            .collect();
        let plan: Vec<(u32, Vec<String>)> = perm::chmod_plan(&items, self.change)
            .into_iter()
            .map(|(mode, paths)| (mode, paths.into_iter().map(str::to_string).collect()))
            .collect();
        let recursive = self.recursive && self.has_dir();
        let total = self.items.len();
        self.busy = true;
        self.error = None;
        let task = (self.submit)(plan, recursive, window, cx);
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                this._task = None;
                match result {
                    Ok(()) => window.close_dialog(cx),
                    Err(failure) => {
                        this.error = Some(if failure.done > 0 {
                            t!(
                                "sftp.perm.partial",
                                done = failure.done,
                                total = total,
                                error = failure.error
                            )
                        } else {
                            failure.error
                        });
                        cx.notify();
                    }
                }
            });
        }));
        cx.notify();
    }

    /// 一个三态的复选框。
    fn render_bit(
        &self,
        bit: u32,
        label: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let state = self.change.state(bit);
        let filled = state != BitState::Off;
        h_flex()
            .id(SharedString::from(format!("perm-bit-{bit:o}")))
            .justify_center()
            .child(
                div()
                    .size_4()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme.radius / 2.)
                    .border_1()
                    .border_color(if filled { theme.primary } else { theme.input })
                    .when(filled, |this| this.bg(theme.primary))
                    .text_color(theme.primary_foreground)
                    .map(|this| match state {
                        BitState::On => this.child(Icon::new(IconName::Check).xsmall()),
                        BitState::Mixed => this.child(Icon::new(IconName::Minus).xsmall()),
                        BitState::Off => this,
                    }),
            )
            .cursor_pointer()
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(label.clone()).build(window, cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(bit, window, cx)))
    }
}

impl Render for PermForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let warning = cx.theme().warning;
        let danger = cx.theme().danger;
        let subject: SharedString = match self.items.as_slice() {
            [one] => super::list_model::visible_name(&one.name).into(),
            items => t!("sftp.perm.many", count = items.len()),
        };
        let all_names: SharedString = self
            .items
            .iter()
            .map(|i| super::list_model::visible_name(&i.name))
            .collect::<Vec<_>>()
            .join("\n")
            .into();
        let cell = || div().w(rems(4.)).flex().justify_center();
        let rows = [
            t!("sftp.perm.owner"),
            t!("sftp.perm.group"),
            t!("sftp.perm.other"),
        ];
        let columns = [
            t!("sftp.perm.read"),
            t!("sftp.perm.write"),
            t!("sftp.perm.execute"),
        ];
        let mut grid = v_flex().gap_1().child(
            h_flex()
                .text_xs()
                .text_color(muted)
                .child(div().w(rems(5.)))
                .children(columns.iter().map(|c| cell().child(c.clone()))),
        );
        for (who, (r, w, x)) in rows.iter().zip(GRID) {
            let bits = [(r, &columns[0]), (w, &columns[1]), (x, &columns[2])];
            let mut line = h_flex()
                .h(rems(1.75))
                .text_sm()
                .child(div().w(rems(5.)).child(who.clone()));
            for (bit, column) in bits {
                let label: SharedString = format!("{who} {column}").into();
                line = line.child(cell().child(self.render_bit(bit, label, cx)));
            }
            grid = grid.child(line);
        }
        let specials = [
            t!("sftp.perm.setuid"),
            t!("sftp.perm.setgid"),
            t!("sftp.perm.sticky"),
        ];
        let mut special = h_flex().gap_4().text_sm();
        for (bit, label) in SPECIAL.into_iter().zip(specials) {
            special = special.child(
                h_flex()
                    .gap_1p5()
                    .child(self.render_bit(bit, label.clone(), cx))
                    .child(label),
            );
        }
        let mixed = self.change.mixed() != 0;
        let has_dir = self.has_dir();
        let blocked = self.blocked();
        let valid = self.octal_valid(cx);
        v_flex()
            .gap_3()
            .child(
                div()
                    .id("perm-subject")
                    .text_sm()
                    .text_color(muted)
                    .truncate()
                    .child(subject)
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(all_names.clone())
                            .build(window, cx)
                    }),
            )
            .child(grid)
            .child(special)
            .child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(t!("sftp.perm.octal"))
                    .child(
                        div().w(rems(5.)).child(
                            Input::new(&self.octal)
                                .small()
                                .id("perm-octal")
                                .font_family(cx.theme().mono_font_family.clone()),
                        ),
                    ),
            )
            .when(mixed, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(prose(t!("sftp.perm.mixed_hint"))),
                )
            })
            .when(has_dir, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Checkbox::new("perm-recursive")
                                .label(t!("sftp.perm.recursive"))
                                .checked(self.recursive)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.recursive = *checked;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .pl_6()
                                .text_xs()
                                .text_color(muted)
                                .child(prose(t!("sftp.perm.recursive_hint"))),
                        ),
                )
            })
            .when_some(blocked.clone(), |this, reason| {
                this.child(div().text_xs().text_color(warning).child(prose(reason)))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_xs().text_color(danger).child(prose(error)))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("perm-cancel")
                            .ghost()
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("perm-ok")
                            .primary()
                            .label(t!("common.ok"))
                            .loading(self.busy)
                            .disabled(self.busy || !valid || blocked.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
