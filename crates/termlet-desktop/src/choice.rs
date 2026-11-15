//! 下拉选择的选项：显示的文字与对应的值。对话框里的分组、跳板机、代理类型、编码等都用它。

use gpui_kit::component::IndexPath;
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::SelectState;
use gpui_kit::{AppContext as _, Context, Entity, SharedString, Window};

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

/// 下拉选择当前选中的值。
pub fn chosen<V: Clone + PartialEq + 'static>(
    state: &Entity<ChoiceState<V>>,
    cx: &gpui_kit::App,
) -> Option<V> {
    state.read(cx).selected_value().cloned()
}
