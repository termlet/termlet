//! 界面大小：按本机设置（[`termlet_core::ui_scale`]：跟随系统的文字大小或固定的档位）设定 gpui-kit 主题的字号。
//!
//! gpui-kit 每帧把各窗口的 rem 设为主题的字号（`Root` 的 `prepare`），界面上按 rem 写的尺寸（文字、间距、按钮、面板、
//! 对话框）随之一起缩放，所有窗口同时生效；终端的网格按终端的字号（像素），不受影响。侧边栏、主机工具面板记下的宽度
//! 按 100% 时的像素记，用时乘上 [`factor`]，换了界面大小后与界面一起缩放。窗口的大小与最小尺寸是物理边界，不缩放。
//!
//! 跟随系统时，启动与窗口激活时读一次系统的文字大小：系统设置中改了之后回到 Termlet 就跟上。
//!
//! gpui-kit 有几处按 100% 时的像素写死，这里随界面大小改掉：标题栏的高度（[`title_bar`]）；通知、Sheet 让开标题栏的
//! 距离与通知的宽度（主题中的设置，[`apply`] 一并改）；警示对话框（确认框、提示框）的宽度（[`open_alert`]）。

use gpui_kit::component::dialog::{AlertDialog, Dialog};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Edges, Theme, TitleBar, WindowExt as _};
use gpui_kit::{
    App, Global, InteractiveElement as _, OperatingSystem, ParentElement as _, SharedString,
    Styled as _, TestSupportExt as _, Window, actions, div, px, rems,
};
use termlet_core::ui_scale;

use crate::app_state::AppState;
use crate::choice::Choice;
use crate::prose::prose;
use crate::{store_errors, t};

actions!(
    ui_scale,
    [
        /// 放大界面：界面大小的下一档
        ZoomInUi,
        /// 缩小界面
        ZoomOutUi,
        /// 界面大小恢复默认（跟随系统）
        ResetUiZoom,
    ]
);

/// 100% 时的 rem：gpui-kit 默认主题的字号。
pub const BASE_REM: f32 = 16.;
/// 100% 时 gpui-kit 主题的等宽字号（gpui-kit 的代码编辑器等用它）。
const BASE_MONO: f32 = 13.;

/// 代替系统读数的文字大小（UI 测试：不受这台电脑的设置影响）。
pub(crate) struct SystemOverride(pub u16);

impl Global for SystemOverride {}

/// 放大、缩小一档或恢复默认。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    In,
    Out,
    Reset,
}

/// 这个系统上“跟随系统”有没有来源：只有 Windows 有全局的文字大小（见 [`termlet_core::ui_scale`]）。没有时设置中
/// 不列“跟随系统”，默认值显示为“100%（默认）”。
pub fn follows_system(os: OperatingSystem) -> bool {
    os == OperatingSystem::Windows
}

/// 系统的文字大小（百分比）。
pub fn system_percent(cx: &App) -> u16 {
    cx.try_global::<SystemOverride>()
        .map_or_else(ui_scale::system_text_scale, |o| o.0)
}

/// 按本机设置实际的界面大小（百分比）。
pub fn current_percent(cx: &App) -> u16 {
    let setting = AppState::store(cx).local_settings().window.ui_scale;
    ui_scale::effective(setting, system_percent(cx))
}

/// 按本机设置应用界面大小：启动时、本机设置变化后、窗口激活时（跟随系统时跟上系统的变化）；与现在的相同时什么也不做。
///
/// 除了字号，主题中按像素记的通知、Sheet 的位置也一并换算：gpui-kit 按 100% 时标题栏的高度（34 像素）让开标题栏，
/// 界面放大后标题栏更高，通知会压在标题栏上。100% 时与 gpui-kit 的默认值相同。
pub fn apply(cx: &mut App) {
    let percent = current_percent(cx);
    let rem = BASE_REM * f32::from(percent) / 100.;
    let font_size = px(rem);
    if cx.theme().font_size == font_size {
        return;
    }
    tracing::info!(percent, "界面大小");
    Theme::update(cx, |theme| {
        theme.font_size = font_size;
        theme.mono_font_size = px(BASE_MONO * f32::from(percent) / 100.);
        let title_bar = px(TITLE_BAR_HEIGHT * rem);
        let gap = px(NOTIFICATION_GAP * rem);
        theme.notification.margins = Edges {
            top: title_bar + gap,
            right: gap,
            bottom: gap,
            left: gap,
        };
        theme.notification.width = px(NOTIFICATION_WIDTH * rem);
        theme.sheet.margin_top = title_bar;
    });
}

/// 通知与窗口边缘（顶上是与标题栏）的距离（rem）：100% 时是 gpui-kit 默认的 16 像素。
const NOTIFICATION_GAP: f32 = 1.;
/// 通知的宽度（rem）：100% 时是 gpui-kit 默认的 382 像素。
const NOTIFICATION_WIDTH: f32 = 23.875;
/// 警示对话框（确认框、提示框）的宽度（rem）：100% 时是 gpui-kit 默认的 448 像素。
const ALERT_WIDTH: f32 = 28.;

/// 对话框标题右边多留的宽度（rem）：gpui-kit 把右上角的关闭按钮（×，1.5rem 见方的小号按钮）放在离右边 8px 处，
/// 标题右边只留了 16px，长标题（带主机名、文件名的）折行后钻到 × 下面。
const CLOSE_BUTTON_ROOM: f32 = 1.5;

/// 对话框的标题。
pub trait DialogTitleExt {
    /// 设置标题，给右上角的 × 让出位置，放不下时折行（避头尾）：代替 gpui-kit 的 `Dialog::title`（`clippy.toml`
    /// 不许直接用）。警示对话框（确认框、提示框）没有 ×，标题照常用 `AlertDialog::title`。
    fn titled(self, title: impl Into<SharedString>) -> Self;
}

impl DialogTitleExt for Dialog {
    #[allow(clippy::disallowed_methods)]
    fn titled(self, title: impl Into<SharedString>) -> Self {
        self.title(
            div().pr(rems(CLOSE_BUTTON_ROOM)).child(
                div()
                    .id("dialog-title-text")
                    .test_support()
                    .child(prose(title.into())),
            ),
        )
    }
}

/// 打开警示对话框（确认框、提示框）：宽度随界面大小（[`ALERT_WIDTH`]），`build` 中另设了宽度时以它为准。
/// gpui-kit 的 `open_alert_dialog` 默认宽度写死像素，界面放大后显得窄，Termlet 的警示对话框都经这里打开
/// （`clippy.toml` 不许直接调用它）。
pub fn open_alert<F>(window: &mut Window, cx: &mut App, build: F)
where
    F: Fn(AlertDialog, &mut Window, &mut App) -> AlertDialog + 'static,
{
    #[allow(clippy::disallowed_methods)]
    window.open_alert_dialog(cx, move |alert, window, cx| {
        let width = rems(ALERT_WIDTH).to_pixels(window.rem_size());
        build(alert.width(width), window, cx)
    });
}

/// 界面相对 100% 的缩放倍数（窗口的 rem 除以 [`BASE_REM`]）：按 100% 时的像素记的宽度乘上它。
pub fn factor(window: &Window) -> f32 {
    f32::from(window.rem_size()) / BASE_REM
}

/// 标题栏的高度（rem）：100% 时是 gpui-kit 默认的 34 像素。gpui-kit 的标题栏写死了像素，界面放大后上面的按钮放不下。
const TITLE_BAR_HEIGHT: f32 = 2.125;

/// 随界面大小缩放的标题栏。
pub fn title_bar() -> TitleBar {
    TitleBar::new().h(rems(TITLE_BAR_HEIGHT))
}

/// 放大、缩小一档或恢复默认（命令面板、快捷键）：改本机设置并立即应用，提示现在的大小；保存失败时提示原因。
pub fn zoom(step: Step, window: &mut Window, cx: &mut App) {
    let current = current_percent(cx);
    let setting = match step {
        Step::In => ui_scale::larger(current),
        Step::Out => ui_scale::smaller(current),
        Step::Reset => ui_scale::SYSTEM,
    };
    tracing::debug!(?step, current, setting, "调整界面大小");
    if let Err(e) = AppState::store(cx).update_local_settings(|s| s.window.ui_scale = setting) {
        tracing::error!("保存界面大小失败：{e}");
        crate::notify::error(
            t!("settings.save_failed", error = store_errors::describe(&e)),
            window,
            cx,
        );
        return;
    }
    apply(cx);
    window.push_notification(
        Notification::info(t!(
            "settings.ui_scale_changed",
            percent = current_percent(cx)
        ))
        .id::<UiScaleNotice>(),
        cx,
    );
}

/// 调整界面大小的提示（连续调整时替换上一条）。
struct UiScaleNotice;

/// 设置中“界面大小”的选项与选中的值：`os` 上能跟随系统时第一项是“跟随系统（按系统现在的文字大小实际的大小）”，
/// 接着是各档位；不能时“100%”一项就是默认值（存为跟随系统，存着 100 时也选它）。`setting` 超出范围时选限制后的
/// 档位；在范围内但不是档位（手改过）时另列一项。
pub fn choices(os: OperatingSystem, system: u16, setting: u16) -> (Vec<Choice<u16>>, u16) {
    let follows = follows_system(os);
    let mut choices = Vec::new();
    if follows {
        choices.push(Choice::new(
            t!(
                "settings.ui_scale.system",
                percent = ui_scale::effective(ui_scale::SYSTEM, system)
            ),
            ui_scale::SYSTEM,
        ));
    }
    for percent in ui_scale::PRESETS {
        choices.push(if !follows && percent == ui_scale::DEFAULT {
            Choice::new(t!("settings.ui_scale.default"), ui_scale::SYSTEM)
        } else {
            Choice::new(t!("settings.ui_scale.percent", percent = percent), percent)
        });
    }
    let selected = match setting {
        ui_scale::SYSTEM => ui_scale::SYSTEM,
        _ => match ui_scale::effective(setting, system) {
            ui_scale::DEFAULT if !follows => ui_scale::SYSTEM,
            percent => percent,
        },
    };
    if !choices.iter().any(|c| c.value == selected) {
        choices.push(Choice::new(
            t!("settings.ui_scale.percent", percent = selected),
            selected,
        ));
    }
    (choices, selected)
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Pixels, TestAppContext, WindowBounds,
        WindowOptions, size,
    };
    use termlet_core::ui_scale::{DEFAULT, SYSTEM};

    use super::*;
    use crate::testkit::open_workspace;

    fn values(choices: &[Choice<u16>]) -> Vec<u16> {
        choices.iter().map(|c| c.value).collect()
    }

    fn rem(cx: &mut TestAppContext, handle: AnyWindowHandle) -> Pixels {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.rem_size()
        })
        .unwrap()
    }

    fn set_scale(cx: &mut TestAppContext, percent: u16) {
        cx.update(|cx| {
            AppState::store(cx)
                .update_local_settings(|s| s.window.ui_scale = percent)
                .unwrap()
        });
        // 本机设置的变更事件经异步任务转发
        cx.run_until_parked();
    }

    // Windows 上第一项是跟随系统（写上系统现在的大小），接着是各档位；macOS、Linux 上没有跟随系统，“100%”就是默认值，
    // 存着 100 时也选它；手改过、不在档位上的值另列一项
    #[test]
    fn choices_follow_the_system_only_on_windows() {
        let (list, selected) = choices(OperatingSystem::Windows, 125, SYSTEM);
        assert_eq!(values(&list), [SYSTEM, 90, 100, 110, 125, 150]);
        assert_eq!(list[0].title, "跟随系统（125%）");
        assert_eq!(list[2].title, "100%");
        assert_eq!(selected, SYSTEM);
        assert_eq!(choices(OperatingSystem::Windows, 100, 150).1, 150);
        assert_eq!(choices(OperatingSystem::Windows, 100, DEFAULT).1, DEFAULT);
        for os in [OperatingSystem::Mac, OperatingSystem::Linux] {
            let (list, selected) = choices(os, 100, SYSTEM);
            assert_eq!(values(&list), [90, SYSTEM, 110, 125, 150], "{os:?}");
            assert_eq!(list[1].title, "100%（默认）");
            assert_eq!(selected, SYSTEM);
            assert_eq!(choices(os, 100, DEFAULT).1, SYSTEM, "{os:?}：存着 100");
            assert_eq!(choices(os, 100, 125).1, 125);
        }
        assert_eq!(
            choices(OperatingSystem::Windows, 225, SYSTEM).0[0].title,
            "跟随系统（150%）",
            "按限制后的大小"
        );
        assert_eq!(
            choices(OperatingSystem::Windows, 100, 175).1,
            150,
            "超出范围"
        );
        assert_eq!(choices(OperatingSystem::Windows, 100, 10).1, 90);
        let (list, selected) = choices(OperatingSystem::Windows, 100, 133);
        assert_eq!(selected, 133);
        assert_eq!(
            list.last().map(|c| (c.title.as_ref(), c.value)),
            Some(("133%", 133))
        );
    }

    // 改了界面大小，所有窗口的 rem 一起变；跟随系统时窗口激活就按系统现在的文字大小
    #[gpui_kit::test]
    fn scale_applies_to_every_window_and_follows_the_system(cx: &mut TestAppContext) {
        let first = open_workspace(cx);
        let second = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            Default::default(),
                            size(px(800.), px(600.)),
                        ))),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| crate::Workspace::new(window, cx)),
                )
            })
            .unwrap()
            .0;
        cx.run_until_parked();
        assert_eq!(rem(cx, first), px(BASE_REM), "测试中系统的文字大小按 100%");
        assert_eq!(rem(cx, second), px(BASE_REM));

        set_scale(cx, 125);
        assert_eq!(rem(cx, first), px(20.));
        assert_eq!(rem(cx, second), px(20.));
        assert_eq!(cx.update(|cx| cx.theme().mono_font_size), px(16.25));

        // 跟随系统：系统改成 110% 后，窗口激活时跟上
        set_scale(cx, SYSTEM);
        assert_eq!(rem(cx, first), px(BASE_REM));
        cx.update(|cx| cx.set_global(SystemOverride(110)));
        assert_eq!(rem(cx, first), px(BASE_REM), "还没重读");
        cx.update_window(second, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        assert_eq!(rem(cx, first), px(17.6));
        assert_eq!(rem(cx, second), px(17.6));
        // 系统的 225% 限制在最大的档位
        cx.update(|cx| cx.set_global(SystemOverride(225)));
        cx.update_window(first, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            rem(cx, first),
            px(BASE_REM * f32::from(ui_scale::MAX) / 100.)
        );
    }

    // gpui-kit 按像素写死的几处随界面大小：通知让开标题栏的距离与宽度、Sheet 的上边距、警示对话框的宽度（`build`
    // 另设的宽度优先）；100% 时与 gpui-kit 的默认值相同
    #[gpui_kit::test]
    fn alerts_notifications_and_sheets_follow_the_scale(cx: &mut TestAppContext) {
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::component::notification::NotificationSettings;
        use gpui_kit::component::sheet::SheetSettings;

        let handle = open_workspace(cx);
        let alert_width = |cx: &mut TestAppContext, width: Option<Pixels>| {
            cx.update_window(handle, |_, window, cx| {
                open_alert(window, cx, move |alert, _, _| {
                    let alert = alert.title("确认");
                    match width {
                        Some(width) => alert.width(width),
                        None => alert,
                    }
                });
            })
            .unwrap();
            cx.run_until_parked();
            let width = cx
                .update_window(handle, |_, window, cx| {
                    window.render_frame(cx);
                    let width = window.find("surface").bounds().size.width;
                    window.close_dialog(cx);
                    width
                })
                .unwrap();
            cx.run_until_parked();
            width
        };

        set_scale(cx, 125);
        cx.update(|cx| {
            let theme = cx.theme();
            // 标题栏 2.125rem = 42.5 像素，再隔 1rem
            assert_eq!(theme.notification.margins.top, px(62.5));
            assert_eq!(theme.notification.margins.right, px(20.));
            assert_eq!(theme.notification.width, px(477.5));
            assert_eq!(theme.sheet.margin_top, px(42.5));
        });
        assert_eq!(alert_width(cx, None), px(560.), "28rem");
        assert_eq!(alert_width(cx, Some(px(300.))), px(300.), "另设的宽度优先");

        set_scale(cx, DEFAULT);
        cx.update(|cx| {
            let theme = cx.theme();
            let defaults = NotificationSettings::default();
            assert_eq!(theme.notification.margins, defaults.margins);
            assert_eq!(theme.notification.width, defaults.width);
            assert_eq!(theme.sheet.margin_top, SheetSettings::default().margin_top);
        });
        assert_eq!(alert_width(cx, None), px(448.));
    }

    // 放大、缩小按档位走并存进本机设置，到头了不再变；恢复默认是跟随系统
    #[gpui_kit::test]
    fn zoom_steps_through_the_presets(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        let setting = |cx: &mut TestAppContext| {
            cx.update(|cx| AppState::store(cx).local_settings().window.ui_scale)
        };
        let step = |cx: &mut TestAppContext, action: Box<dyn gpui_kit::Action>| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.dispatch_action(action, cx);
            })
            .unwrap();
            cx.run_until_parked();
        };
        step(cx, Box::new(ZoomInUi));
        assert_eq!(setting(cx), 110);
        assert_eq!(rem(cx, handle), px(17.6));
        step(cx, Box::new(ZoomInUi));
        assert_eq!(setting(cx), 125);
        for _ in 0..5 {
            step(cx, Box::new(ZoomInUi));
        }
        assert_eq!(setting(cx), ui_scale::MAX);
        step(cx, Box::new(ZoomOutUi));
        assert_eq!(setting(cx), 125);
        step(cx, Box::new(ResetUiZoom));
        assert_eq!(setting(cx), SYSTEM);
        assert_eq!(rem(cx, handle), px(BASE_REM));
        for _ in 0..3 {
            step(cx, Box::new(ZoomOutUi));
        }
        assert_eq!(setting(cx), ui_scale::MIN);
    }

    // 长标题（带主机名的代理命令确认等）折行，不钻到右上角的 × 下面；界面 150% 时也是
    #[gpui_kit::test]
    fn long_dialog_titles_clear_the_close_button(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        for percent in [100, 150] {
            set_scale(cx, percent);
            cx.update_window(handle, |_, window, cx| {
                window.open_dialog(cx, |dialog, window, _| {
                    let width = rems(28.75).to_pixels(window.rem_size());
                    dialog.w(width).titled(
                        "在这台电脑上执行“华东生产环境-订单服务-数据库主节点-只读副本-01-通过堡垒机”的代理命令？",
                    )
                })
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                let title = window.find("dialog-title-text").bounds();
                let close = window.within("dialog-close").find("close").bounds();
                assert!(
                    title.size.height > close.size.height,
                    "{percent}%：标题折了行"
                );
                assert!(
                    title.right() <= close.left(),
                    "{percent}%：标题 {title:?} 在 × {close:?} 左边"
                );
                window.close_dialog(cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
    }
}
