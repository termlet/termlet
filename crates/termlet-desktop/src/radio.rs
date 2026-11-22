//! 竖排的单选组。
//!
//! gpui-kit 的 `Radio`（0.7.3、0.8.1 相同）把文字放在宽 100%（`w_full`）的容器里、排在圆点之后，文字容器比
//! 单选项宽出圆点与间距，长到换行的文字在右侧被裁掉一截（例如“AI 与自动化”页的“命中规则时”）。这里在组的右边
//! 留出这段宽度，超出的部分落在组内，文字完整显示。横排的单选组每项按内容排开，不会换行，不用它。

use gpui_kit::component::radio::RadioGroup;
use gpui_kit::{ElementId, Styled as _, rems};

/// 默认大小的圆点（1rem）加上它与文字的间距（0.5rem）。
const INDICATOR_AND_GAP: f32 = 1.5;

/// 竖排的单选组（选项文字较长、可能换行时用它）。
pub fn vertical(id: impl Into<ElementId>) -> RadioGroup {
    RadioGroup::vertical(id).pr(rems(INDICATOR_AND_GAP))
}
