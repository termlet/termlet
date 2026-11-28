//! 滚动区的写法。
//!
//! 能滚动的区域都要看得出能滚：gpui-kit 的滚动区放不下时一直显示滚动条（见 `appearance.rs`）。GPUI 自己的
//! `overflow_y_scroll()` 等只能滚、不画滚动条，`clippy.toml` 不许直接用：
//! - 一般用 gpui-kit 的 `overflow_y_scrollbar()`（`ScrollableElement`）。它把元素放进自己的滚动区，元素原来的 id 与
//!   `test_support()` 随之失效：要按 id 找时，id 写在它返回的 `Scrollable` 上（`.overflow_y_scrollbar().id(…)`，
//!   同一处调用建出多个时也靠它区分），要在测试中找到时在外面或里面另包一层。外边框、圆角放在外面一层（放在它上面会
//!   随内容一起滚走）。
//! - 要由程序滚动（`ScrollHandle` 记着位置、滚到某一项）时用 [`TrackedScroll::tracked_y`]，外面套一层 `relative()`
//!   的容器挂 `.vertical_scrollbar(&handle)` 画滚动条。
//!
//! 滚动区右边一条 [`GUTTER`] 宽的地方归滚动条（放不下时）：内容右端有按钮、勾选框等可点的东西或右对齐的数字，且离右边
//! 不到 1rem 的，用 [`ScrollGutter::scrollbar_gutter`] 让出来；窄的地方（收起的竖排 Tab 栏、分页导航）用 [`thin_scrollbar`]。
//!
//! 对话框里的滚动区也要用 gpui-kit 的：对话框比窗口高时 gpui-kit 只让滚轮滚对话框本身，里面 GPUI 自己的滚动区滚不动；
//! 指针下最里层的 gpui-kit 滚动区则独占滚轮。

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::{App, ElementId, Pixels, ScrollHandle, StatefulInteractiveElement, Styled, px};

/// 滚动条占住的宽度：gpui-base 的滚动条轨道宽 16px（不随界面大小缩放）。滚动条一直显示，内容放不下时这一条接住鼠标
/// 按下（点了是跳着滚动），压在里面的按钮点不到、文字被滑块盖住：右端有按钮或右对齐数字的内容经
/// [`ScrollGutter::scrollbar_gutter`] 留出这么宽。
pub const GUTTER: Pixels = px(16.);

/// 滚动区的内容右边让出 [`GUTTER`]。
pub trait ScrollGutter: Styled {
    /// 右内边距换成 [`GUTTER`]（左边照旧）：用在滚动的内容上（`overflow_y_scrollbar()` 之前的元素、`tracked_y` 的元素、
    /// 虚拟列表本身或它的行；行有表头时表头一起用，列才对得齐），或用在贴着右边浮出的行内按钮上。
    fn scrollbar_gutter(self) -> Self {
        self.pr(GUTTER)
    }
}

impl<E: Styled> ScrollGutter for E {}

/// 位置由 [`ScrollHandle`] 记着的滚动区。
pub trait TrackedScroll: StatefulInteractiveElement {
    /// 纵向滚动、位置记在 `handle` 中；滚动条由外层的 `.vertical_scrollbar(handle)` 画。
    #[allow(clippy::disallowed_methods)]
    fn tracked_y(self, handle: &ScrollHandle) -> Self {
        self.overflow_y_scroll().track_scroll(handle)
    }

    /// 横向滚动、位置记在 `handle` 中的一行（快捷命令栏这类一行按钮）：不画滚动条，放不下的部分露出半截，鼠标滚轮经
    /// `ScrollableMask::vertical_wheel(handle)` 左右滚。
    #[allow(clippy::disallowed_methods)]
    fn tracked_x(self, handle: &ScrollHandle) -> Self {
        self.overflow_x_scroll().track_scroll(handle)
    }
}

impl<E: StatefulInteractiveElement> TrackedScroll for E {}

/// 窄的地方用的细滚动条（收起的竖排 Tab 栏、对话框左侧的分页导航）：默认 16px 的轨道会盖住按钮、接住点击，这里的轨道
/// 只有 0.25rem，落在按钮与右边之间。放在要滚动的区域上面一层：`div().absolute().inset_0().child(thin_scrollbar(…))`。
pub fn thin_scrollbar(id: impl Into<ElementId>, scroll: &ScrollHandle, cx: &App) -> Scrollbar {
    let rem = cx.theme().font_size;
    let (track, thumb, inset) = (rem * 0.25, rem * 0.1875, rem * 0.03125);
    Scrollbar::vertical(scroll)
        .id(id)
        .viewport_from_layout()
        .styles(|styles| {
            styles
                .track(|style| style.width(track))
                .thumb(|style| style.width(thumb).inset(inset))
                .thumb_hover(|style| style.width(thumb).inset(inset))
                .thumb_active(|style| style.width(thumb).inset(inset))
        })
}
