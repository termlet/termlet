//! 分段页签（gpui-kit 的 `TabBar`）按文案排开放不放得下。页签不收缩，排不下时被横向裁掉（英文的文案比中文长，
//! 侧边栏的 Tunnels 曾被裁成 Tu），所以装它的面板按页签实际排开的宽度放宽自己的最小宽度：`TabBar` 跟踪一个
//! `ScrollHandle`，排版后由 [`overflow`] 量出页签比可见区域宽出多少；面板在排版后用 [`NeededWidth::measure`]
//! 算出所需的宽度，变了时再画一帧，按新的最小宽度排。按实际排版量，与字体、字号无关。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{App, IntoElement, Pixels, ScrollHandle, Styled as _, canvas};

/// 跟踪着 `scroll` 的页签栏中，`tabs` 个页签排开后比可见区域宽出多少：放不下时为正，有富余时为负；
/// 还没排过版时为 `None`。
pub fn overflow(scroll: &ScrollHandle, tabs: usize) -> Option<Pixels> {
    let first = scroll.bounds_for_item(0)?;
    let last = scroll.bounds_for_item(tabs.checked_sub(1)?)?;
    Some(last.right() - first.left() - scroll.bounds().size.width)
}

/// 面板里的页签全部显示所需的面板宽度（逻辑像素；每帧排版后量得，还没量过时为 `None`）。
#[derive(Clone, Default)]
pub struct NeededWidth(Rc<Cell<Option<f32>>>);

impl NeededWidth {
    pub fn get(&self) -> Option<f32> {
        self.0.get()
    }

    /// 放在面板中、页签栏之后的元素（不占布局）：排版完后按这一帧面板的宽度 `width` 加上 `overflow`（面板中各页签栏
    /// 放不下的部分，取最大的）算出所需的宽度；变了时再画一帧。
    pub fn measure(
        &self,
        width: Pixels,
        overflow: impl FnOnce(&App) -> Option<Pixels> + 'static,
    ) -> impl IntoElement {
        let needed = self.0.clone();
        canvas(
            move |_, window, cx| {
                let Some(overflow) = overflow(cx) else {
                    return;
                };
                // 取整到像素；容许一点排版的浮点误差，免得结果在相邻的两个整数之间来回跳、一直重画
                let width = (f32::from(width + overflow) - 0.01).ceil();
                if needed.replace(Some(width)) != Some(width) {
                    window.on_next_frame(|window, _| window.refresh());
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
    }
}
