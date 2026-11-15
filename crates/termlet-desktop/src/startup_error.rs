//! 启动失败（数据目录不可用、数据库来自更新版本的 Termlet、已有实例收不下启动参数等）时显示原因的小窗口。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, TitleBar, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Window, WindowBounds, WindowOptions,
    div, px, size,
};
use termlet_core::CoreError;

use crate::prose::prose;
use crate::t;

struct StartupError {
    message: SharedString,
}

pub fn open(error: CoreError, cx: &mut App) {
    open_message(message(&error), cx);
}

/// 显示一段已经写好的原因。
pub fn open_message(message: SharedString, cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(560.), px(320.)),
            cx,
        ))),
        is_resizable: false,
        app_id: Some(crate::APP_ID.into()),
        ..TitleBar::window_options()
    };
    gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| StartupError { message }))
        .expect("无法打开窗口");
}

fn message(error: &CoreError) -> SharedString {
    match error {
        CoreError::DatabaseTooNew { .. } => t!("startup.error.database_too_new"),
        CoreError::DataDir(reason) => t!("startup.error.data_dir", reason = reason),
        other => t!("startup.error.other", error = other.to_string()),
    }
}

impl Render for StartupError {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(crate::ui_scale::title_bar().child(div().text_sm().child(t!("app.name"))))
            .child(
                v_flex()
                    .flex_1()
                    .p_6()
                    .gap_4()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::CircleX).text_color(cx.theme().danger))
                            .child(div().text_lg().child(t!("startup.error.title"))),
                    )
                    // 原因较长（例如便携版放在了不能写的位置）时在这里滚动，按钮总在下面
                    .child(
                        div()
                            .id("startup-error-message")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scrollbar()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(prose(self.message.clone())),
                    )
                    .child(
                        h_flex().justify_end().child(
                            Button::new("startup-error-quit")
                                .primary()
                                .label(t!("vault.quit"))
                                .on_click(|_, _, cx| cx.quit()),
                        ),
                    ),
            )
    }
}
