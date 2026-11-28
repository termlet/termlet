//! 下拉选择：选项（显示的文字与对应的值）与画下拉的 [`select`]（弹出层按窗口收高度）。对话框里的分组、跳板机、
//! 代理类型、编码等都用它。

use gpui_kit::component::IndexPath;
use gpui_kit::component::searchable_list::{
    SearchableListDelegate, SearchableListItem, SearchableVec,
};
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{Sizable, Size};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, IntoElement, Pixels, RenderOnce,
    SharedString, StyleRefinement, Styled, Window, rems,
};

/// 下拉弹出层最高多少（rem），与 gpui-kit 的默认值相同。
const MENU_MAX_HEIGHT: f32 = 20.;
/// 窗口放不下时弹出层的选项少留的高度（rem）：gpui-kit 的最大高度只管选项，上面还有搜索框（可以搜索的下拉），
/// 弹出层离窗口上下边还各有 8px 与边框。
const MENU_WINDOW_EDGE: f32 = 4.;

/// 一个选项。
#[derive(Clone)]
pub struct Choice<V> {
    pub title: SharedString,
    pub value: V,
}

impl<V> Choice<V> {
    pub fn new(title: impl Into<SharedString>, value: V) -> Self {
        Self {
            title: title.into(),
            value,
        }
    }
}

impl<V: Clone + PartialEq + 'static> SearchableListItem for Choice<V> {
    type Value = V;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &V {
        &self.value
    }
}

pub type ChoiceState<V> = SelectState<SearchableVec<Choice<V>>>;

/// 建一个下拉选择，选中值等于 `selected` 的一项（没有时选第一项）。选项多时（`searchable`）可以搜索。
pub fn choice_state<V: Clone + PartialEq + 'static, T: 'static>(
    choices: Vec<Choice<V>>,
    selected: &V,
    searchable: bool,
    window: &mut Window,
    cx: &mut Context<T>,
) -> Entity<ChoiceState<V>> {
    let index = choices
        .iter()
        .position(|c| &c.value == selected)
        .or(if choices.is_empty() { None } else { Some(0) })
        .map(IndexPath::new);
    cx.new(|cx| {
        SelectState::new(SearchableVec::new(choices), index, window, cx).searchable(searchable)
    })
}

/// 下拉选择（gpui-kit 的 `Select`），弹出层按窗口收高度：gpui-kit 的弹出层固定最高 20rem，放不下时只挪进窗口、不缩短，
/// 窗口矮、界面放大时下面几项伸到窗口外点不到。窗口放得下 20rem 时与默认相同。画的时候才知道窗口多高，所以包一层，
/// 常用的设置照 `Select` 的写法。
#[allow(clippy::disallowed_methods)]
pub fn select<D>(state: &Entity<SelectState<D>>) -> FitSelect<D>
where
    D: SearchableListDelegate + 'static,
    <D::Item as SearchableListItem>::Value: PartialEq + Clone,
{
    FitSelect {
        select: Select::new(state),
    }
}

/// [`select`] 建的下拉选择。
#[derive(IntoElement)]
pub struct FitSelect<D>
where
    D: SearchableListDelegate + 'static,
    <D::Item as SearchableListItem>::Value: PartialEq + Clone,
{
    select: Select<D>,
}

impl<D> FitSelect<D>
where
    D: SearchableListDelegate + 'static,
    <D::Item as SearchableListItem>::Value: PartialEq + Clone,
{
    pub fn id(self, id: impl Into<ElementId>) -> Self {
        Self {
            select: self.select.id(id),
        }
    }

    pub fn placeholder(self, placeholder: impl Into<SharedString>) -> Self {
        Self {
            select: self.select.placeholder(placeholder),
        }
    }

    pub fn search_placeholder(self, placeholder: impl Into<SharedString>) -> Self {
        Self {
            select: self.select.search_placeholder(placeholder),
        }
    }

    pub fn disabled(self, disabled: bool) -> Self {
        Self {
            select: self.select.disabled(disabled),
        }
    }
}

impl<D> Sizable for FitSelect<D>
where
    D: SearchableListDelegate + 'static,
    <D::Item as SearchableListItem>::Value: PartialEq + Clone,
{
    fn with_size(self, size: impl Into<Size>) -> Self {
        Self {
            select: self.select.with_size(size),
        }
    }
}

impl<D> Styled for FitSelect<D>
where
    D: SearchableListDelegate + 'static,
    <D::Item as SearchableListItem>::Value: PartialEq + Clone,
{
    fn style(&mut self) -> &mut StyleRefinement {
        self.select.style()
    }
}

impl<D> RenderOnce for FitSelect<D>
where
    D: SearchableListDelegate + 'static,
    <D::Item as SearchableListItem>::Value: PartialEq + Clone,
{
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        self.select.menu_max_h(menu_max_height(window))
    }
}

/// 下拉弹出层在这个窗口中最高多少：20rem，窗口放不下时缩到窗口内。
fn menu_max_height(window: &Window) -> Pixels {
    let rem = window.rem_size();
    let room = window.viewport_size().height - rems(MENU_WINDOW_EDGE).to_pixels(rem);
    rems(MENU_MAX_HEIGHT).to_pixels(rem).min(room)
}

/// 下拉选择当前选中的值。
pub fn chosen<V: Clone + PartialEq + 'static>(
    state: &Entity<ChoiceState<V>>,
    cx: &gpui_kit::App,
) -> Option<V> {
    state.read(cx).selected_value().cloned()
}

#[cfg(test)]
mod tests {
    use gpui_kit::{
        AppContext as _, Bounds, Empty, TestAppContext, WindowBounds, WindowOptions, px, rems, size,
    };

    use super::menu_max_height;

    // 窗口放得下时与 gpui-kit 的默认值（20rem）相同，矮时缩到窗口内
    #[gpui_kit::test]
    fn menu_height_fits_the_window(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        for (height, short) in [(px(800.), false), (px(300.), true)] {
            let (handle, _) = cx
                .update(|cx| {
                    gpui_kit::open_window(
                        WindowOptions {
                            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                                Default::default(),
                                size(px(600.), height),
                            ))),
                            ..Default::default()
                        },
                        cx,
                        |_, cx| cx.new(|_| Empty),
                    )
                })
                .unwrap();
            cx.update_window(handle, |_, window, _| {
                let max = menu_max_height(window);
                let default = rems(20.).to_pixels(window.rem_size());
                if short {
                    assert!(max < default && max < window.viewport_size().height);
                } else {
                    assert_eq!(max, default);
                }
            })
            .unwrap();
        }
    }
}
