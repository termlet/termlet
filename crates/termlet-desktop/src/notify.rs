//! 给用户的提示。
//!
//! - 错误与警告（[`error`]、[`warning`]，以及 [`show`] 中的这两种）：显示的同时写一条日志，排查问题时能从日志中
//!   对上用户当时看到了什么。成功的提示不写日志。提示的文字本来就是给用户看的，不含秘密。
//! - 在对话框中做的操作，结果用 [`show`]：gpui-kit 0.8 的对话框画在延迟绘制的一层（`deferred`，优先级 10 起），
//!   通知列表画在普通的一层，对话框开着时弹出的通知被它盖住（大的对话框盖住右上角通知的左半边，短的提示整条看不见）。
//!   开着时会产生结果的对话框（审计日志、崩溃记录、查看公钥、命令历史、把本机代理借给服务器、Docker 清理、主机与
//!   凭据档案等）在自己里面放一条提示行（[`NoticeLine`]），打开后登记在窗口上（[`attach`]），开着时结果写在其中
//!   一条里（选哪条见 `top_line`）；窗口中没有开着的对话框或没有登记的提示行时照常弹出通知。新加这样的对话框时
//!   照此办理。
//! - 设置 Tab 的提示行也登记在窗口上（[`attach_tab`]）：没有开着的对话框、焦点在设置里时结果写在保存条上，靠近
//!   做操作的地方；焦点在别处时照常弹出通知。
//!   与对话框、设置无关的提示（终端、SFTP 等在后台得到的结果）仍用通知。

use gpui_kit::assets::IconName;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, FocusHandle, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, WeakEntity, Window, div,
    prelude::FluentBuilder as _,
};

use crate::dialog_token::DialogToken;
use crate::prose::prose;

/// 提示的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Success,
    Warning,
    Error,
}

/// 一条提示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub level: Level,
    pub text: SharedString,
}

impl Notice {
    pub fn success(text: impl Into<SharedString>) -> Self {
        Self::new(Level::Success, text)
    }

    pub fn warning(text: impl Into<SharedString>) -> Self {
        Self::new(Level::Warning, text)
    }

    pub fn error(text: impl Into<SharedString>) -> Self {
        Self::new(Level::Error, text)
    }

    fn new(level: Level, text: impl Into<SharedString>) -> Self {
        Self {
            level,
            text: text.into(),
        }
    }

    /// 出错写 warn，警告写 info（多是当前用不了某个操作）；成功的不写。
    fn log(&self) {
        let message = &self.text;
        match self.level {
            Level::Error => tracing::warn!(%message, "提示用户：出错"),
            Level::Warning => tracing::info!(%message, "提示用户：警告"),
            Level::Success => {}
        }
    }

    fn into_notification(self) -> Notification {
        match self.level {
            Level::Success => Notification::success(self.text),
            Level::Warning => Notification::warning(self.text),
            Level::Error => Notification::error(self.text),
        }
    }
}

/// 出错的提示（弹出通知）。
pub fn error(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    toast(Notice::error(message), window, cx);
}

/// 警告的提示（弹出通知）。
pub fn warning(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    toast(Notice::warning(message), window, cx);
}

/// 操作的结果：窗口中开着带提示行的对话框时写在对话框里的提示行中（见模块说明），否则弹出通知。
pub fn show(notice: Notice, window: &mut Window, cx: &mut App) {
    match top_line(window, cx) {
        Some(line) => {
            notice.log();
            line.update(cx, |line, cx| {
                line.notice = Some(notice);
                cx.notify();
            });
        }
        None => toast(notice, window, cx),
    }
}

fn toast(notice: Notice, window: &mut Window, cx: &mut App) {
    notice.log();
    window.push_notification(notice.into_notification(), cx);
}

/// 登记了的提示行与所在的窗口（按登记的先后；对话框的视图丢弃后弱引用失效）。
#[derive(Default)]
struct Lines(Vec<(AnyWindowHandle, WeakEntity<NoticeLine>)>);

impl Global for Lines {}

/// 刚打开的对话框里的提示行：在 `open_dialog` 之后立即调用（与 [`DialogToken::take`] 相同，焦点还在新对话框上）。
pub fn attach(line: &Entity<NoticeLine>, window: &Window, cx: &mut App) {
    let token = DialogToken::take(window, cx);
    line.update(cx, |line, _| line.dialog = token);
    register(line, window, cx);
}

/// 设置 Tab 里的提示行：焦点在 `focus` 里（且没有开着的对话框）时结果写在这里。Tab 移到别的窗口后重新登记。
pub fn attach_tab(line: &Entity<NoticeLine>, focus: FocusHandle, window: &Window, cx: &mut App) {
    line.update(cx, |line, _| line.tab = Some(focus));
    register(line, window, cx);
}

fn register(line: &Entity<NoticeLine>, window: &Window, cx: &mut App) {
    let lines = &mut cx.default_global::<Lines>().0;
    lines.retain(|(_, existing)| {
        existing
            .upgrade()
            .is_some_and(|existing| existing.entity_id() != line.entity_id())
    });
    lines.push((window.window_handle(), line.downgrade()));
}

/// 结果写在哪条提示行里：
/// - 窗口中有开着的对话框时：焦点在哪个带提示行的对话框里就写在它的里面（上面的刚关掉、还在退出的过程中时，焦点
///   已经回到下面的对话框）；焦点在别的对话框里（确认框、导出等叠在上面的对话框，多半随即关闭）时写在最后登记的
///   那条里。设置 Tab 的提示行被对话框盖着，不算。
/// - 没有开着的对话框时：焦点在设置 Tab 里就写在它的提示行里，否则没有（弹出通知）。
fn top_line(window: &mut Window, cx: &mut App) -> Option<Entity<NoticeLine>> {
    let handle = window.window_handle();
    let lines: Vec<Entity<NoticeLine>> = cx
        .try_global::<Lines>()?
        .0
        .iter()
        .filter(|(owner, _)| *owner == handle)
        .filter_map(|(_, line)| line.upgrade())
        .collect();
    let (tabs, lines): (Vec<_>, Vec<_>) = lines
        .into_iter()
        .partition(|line| line.read(cx).tab.is_some());
    if !window.has_active_dialog(cx) {
        return tabs.into_iter().find(|line| {
            line.read(cx)
                .tab
                .as_ref()
                .is_some_and(|focus| focus.contains_focused(window, cx))
        });
    }
    let focused = lines.iter().rev().find(|line| {
        line.read(cx)
            .dialog
            .clone()
            .is_some_and(|dialog| dialog.contains_focus(window, cx))
    });
    focused.or(lines.last()).cloned()
}

/// 对话框里的提示行：显示最近一条结果（[`show`] 写入）。放在对话框的哪里、什么时候清掉由对话框决定；文字的大小
/// 与颜色随所在的位置，出错的用红色。
pub struct NoticeLine {
    notice: Option<Notice>,
    /// 所在的对话框（[`attach`] 时记下）
    dialog: Option<DialogToken>,
    /// 所在的设置 Tab 的焦点（[`attach_tab`] 时记下）
    tab: Option<FocusHandle>,
}

impl NoticeLine {
    /// 新的提示行（对话框的视图创建时）。打开对话框后用 [`attach`] 登记。
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {
            notice: None,
            dialog: None,
            tab: None,
        })
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.notice.take().is_some() {
            cx.notify();
        }
    }

    /// 正显示着提示：放在有间距的列里时据此决定放不放进去（空的提示行也会占一份间距）。
    pub fn is_shown(&self) -> bool {
        self.notice.is_some()
    }
}

impl Render for NoticeLine {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(notice) = self.notice.clone() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (icon, color) = match notice.level {
            Level::Success => (IconName::CircleCheck, theme.success),
            Level::Warning => (IconName::TriangleAlert, theme.warning),
            Level::Error => (IconName::CircleX, theme.danger),
        };
        h_flex()
            .id("dialog-notice")
            .test_support()
            .role(gpui_kit::Role::Status)
            .aria_label(notice.text.clone())
            .min_w_0()
            .gap_2()
            .items_start()
            .when(notice.level == Level::Error, |this| {
                this.text_color(theme.danger)
            })
            // 图标放在高为一行文字的盒子里垂直居中：行高比图标高，直接顶对齐时图标偏上；单行与多行的第一行都对齐
            .child(
                h_flex()
                    .id("dialog-notice-icon")
                    .test_support()
                    .flex_none()
                    .items_center()
                    .child(Icon::new(icon).small().text_color(color))
                    // 不占宽度的一个字：盒子的高度随所在位置的字号，正好一行
                    .child(div().w_0().overflow_hidden().child("\u{a0}")),
            )
            .child(
                div()
                    .id("dialog-notice-text")
                    .test_support()
                    .min_w_0()
                    .child(prose(notice.text)),
            )
            .into_any_element()
    }
}

/// 测试用：`window` 中提示会写进的那条提示行现在显示的提示。
#[cfg(test)]
pub(crate) fn shown(window: &mut Window, cx: &mut App) -> Option<Notice> {
    top_line(window, cx).and_then(|line| line.read(cx).notice.clone())
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use gpui_kit::test::TestWindowExt as _;

    use super::*;

    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// 带提示行的对话框的内容。
    struct Host(Entity<NoticeLine>);

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child(self.0.clone())
        }
    }

    fn open_host(window: &mut Window, cx: &mut App) -> Entity<NoticeLine> {
        let line = NoticeLine::new(cx);
        let host = cx.new(|_| Host(line.clone()));
        window.open_dialog(cx, move |dialog, _, _| dialog.child(host.clone()));
        attach(&line, window, cx);
        line
    }

    // 没有对话框时弹出通知；开着带提示行的对话框时写在它里面，不弹通知；上面叠了普通的对话框时仍写在下面的那条里；
    // 叠了两个带提示行的写在上面的里，上面的关掉后（还在退出的过程中）写回下面的；都关掉后又弹出通知
    #[gpui_kit::test]
    fn notices_go_into_the_dialog_on_top(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, _) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| Empty)).unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            show(Notice::success("a"), window, cx);
            assert_eq!(window.notifications(cx).len(), 1);

            let first = open_host(window, cx);
            window.render_frame(cx);
            show(Notice::success("b"), window, cx);
            assert_eq!(window.notifications(cx).len(), 1, "不再弹出通知");
            assert_eq!(first.read(cx).notice.as_ref(), Some(&Notice::success("b")));
            assert_eq!(shown(window, cx), Some(Notice::success("b")));
            window.render_frame(cx);
            assert!(window.try_find("dialog-notice").is_some(), "提示行画出来了");

            window.open_dialog(cx, |dialog, _, _| dialog.title("plain"));
            window.render_frame(cx);
            show(Notice::error("c"), window, cx);
            assert_eq!(first.read(cx).notice.as_ref(), Some(&Notice::error("c")));
            window.close_dialog(cx);
            window.render_frame(cx);

            let second = open_host(window, cx);
            window.render_frame(cx);
            show(Notice::success("d"), window, cx);
            assert_eq!(second.read(cx).notice.as_ref(), Some(&Notice::success("d")));
            assert_eq!(first.read(cx).notice.as_ref(), Some(&Notice::error("c")));

            window.close_dialog(cx);
            show(Notice::warning("e"), window, cx);
            assert_eq!(first.read(cx).notice.as_ref(), Some(&Notice::warning("e")));
            assert_eq!(second.read(cx).notice.as_ref(), Some(&Notice::success("d")));

            window.close_dialog(cx);
            show(Notice::success("f"), window, cx);
            assert_eq!(window.notifications(cx).len(), 2);
            assert_eq!(shown(window, cx), None);
        })
        .unwrap();
    }

    /// 宽度有限的提示行（长的提示换成几行）。
    struct Narrow(Entity<NoticeLine>);

    impl Render for Narrow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(gpui_kit::rems(12.)).text_sm().child(self.0.clone())
        }
    }

    // 图标与第一行文字垂直居中对齐：图标所在的盒子与文字一样从顶上开始、正好一行高（单行时与文字等高）；
    // 多行时仍只有一行高，对着第一行
    #[gpui_kit::test]
    fn icon_lines_up_with_the_first_line(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, line) = cx.update(|cx| {
            let line = NoticeLine::new(cx);
            let view = line.clone();
            let handle =
                gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| Narrow(view)))
                    .unwrap()
                    .0;
            (handle, line)
        });
        let bounds = |cx: &mut TestAppContext, text: &str| {
            let notice = Notice::success(text.to_string());
            cx.update(|cx| {
                line.update(cx, |line, cx| {
                    line.notice = Some(notice);
                    cx.notify();
                })
            });
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                (
                    window.find("dialog-notice-icon").bounds(),
                    window.find("dialog-notice-text").bounds(),
                )
            })
            .unwrap()
        };
        let (icon, text) = bounds(cx, "ok");
        assert_eq!(icon.top(), text.top());
        assert_eq!(icon.size.height, text.size.height, "单行：与文字等高");
        let (icon, text) = bounds(cx, &"long words ".repeat(20));
        assert_eq!(icon.top(), text.top());
        assert!(
            text.size.height >= icon.size.height * 2.,
            "多行：图标的盒子只有一行高：{icon:?} / {text:?}"
        );
    }

    // 提示行只属于登记它的窗口：另一个窗口里开着对话框也照常弹出通知
    #[gpui_kit::test]
    fn lines_belong_to_their_window(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let open = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| Empty))
                    .unwrap()
                    .0
            })
        };
        let (one, two) = (open(cx), open(cx));
        let line = cx
            .update_window(one, |_, window, cx| {
                window.render_frame(cx);
                open_host(window, cx)
            })
            .unwrap();
        cx.update_window(two, |_, window, cx| {
            window.render_frame(cx);
            window.open_dialog(cx, |dialog, _, _| dialog.title("plain"));
            window.render_frame(cx);
            show(Notice::success("x"), window, cx);
            assert_eq!(window.notifications(cx).len(), 1);
        })
        .unwrap();
        cx.update(|cx| assert_eq!(line.read(cx).notice.as_ref(), None));
    }
}
