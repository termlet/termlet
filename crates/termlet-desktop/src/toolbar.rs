//! 标题栏右侧可自定义的工具栏：有哪些按钮、默认的顺序与显示，按本机设置（`window.toolbar`，不同步）排出顺序与
//! 显示（[`resolve`]），以及窗口窄、界面放大时放不下的按钮收进“更多”菜单（[`Fit`]）。
//!
//! - 按钮各有出现的条件（有终端时、有运行中的转发时、当前是 SSH 终端时、启用了应用锁时），条件不满足时不占位置；
//!   隐藏的按钮对应的功能仍可从命令面板、快捷键使用。
//! - 更新提示、同步状态与设置固定在最右边，不在其中：前两个是需要处理时才出现的状态，设置是找回工具栏设置的入口。
//! - 设置中只存与默认不同的写法（与默认相同时存空），以后的版本加了按钮、改了默认时跟着变；不认识的按钮（更新的
//!   版本加的）保存时原样留着。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, Bounds, Pixels, SharedString, Window};
use termlet_core::model::ToolbarButton;

use crate::app_state::AppState;
use crate::{store_errors, t};

/// 工具栏中的按钮。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolbarItem {
    /// 打开 / 收起主机工具面板
    HostTools,
    /// 同步输入
    MultiExec,
    /// 运行中的端口转发（点击切到侧边栏的“转发”页）
    Forwards,
    /// 为当前终端的主机打开 SFTP
    Sftp,
    /// 快捷命令选择器
    Snippets,
    /// 打开 / 关闭撰写栏
    Compose,
    /// 关键词高亮的设置
    Highlight,
    /// 切换浅色 / 深色
    Theme,
    /// 立即锁定
    Lock,
}

impl ToolbarItem {
    /// 全部按钮，按默认的顺序（与做成可自定义之前的标题栏相同，撰写栏是新加的）。
    pub const ALL: [ToolbarItem; 9] = [
        ToolbarItem::HostTools,
        ToolbarItem::MultiExec,
        ToolbarItem::Forwards,
        ToolbarItem::Sftp,
        ToolbarItem::Snippets,
        ToolbarItem::Compose,
        ToolbarItem::Highlight,
        ToolbarItem::Theme,
        ToolbarItem::Lock,
    ];

    /// 存进设置的 ID（发布后不能改）。
    pub fn id(self) -> &'static str {
        match self {
            ToolbarItem::HostTools => "hostTools",
            ToolbarItem::MultiExec => "multiExec",
            ToolbarItem::Forwards => "forwards",
            ToolbarItem::Sftp => "sftp",
            ToolbarItem::Snippets => "snippets",
            ToolbarItem::Compose => "compose",
            ToolbarItem::Highlight => "highlight",
            ToolbarItem::Theme => "theme",
            ToolbarItem::Lock => "lock",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|item| item.id() == id)
    }

    /// 默认显示：撰写栏的按钮是新加的，默认不显示（撰写栏原来只从快捷键、命令面板打开，默认的标题栏不变）。
    pub fn shown_by_default(self) -> bool {
        self != ToolbarItem::Compose
    }

    /// 设置中、“更多”菜单中的名称。
    pub fn title(self) -> SharedString {
        match self {
            ToolbarItem::HostTools => t!("toolbar.item.host_tools"),
            ToolbarItem::MultiExec => t!("toolbar.item.multi_exec"),
            ToolbarItem::Forwards => t!("toolbar.item.forwards"),
            ToolbarItem::Sftp => t!("toolbar.item.sftp"),
            ToolbarItem::Snippets => t!("toolbar.item.snippets"),
            ToolbarItem::Compose => t!("toolbar.item.compose"),
            ToolbarItem::Highlight => t!("toolbar.item.highlight"),
            ToolbarItem::Theme => t!("toolbar.item.theme"),
            ToolbarItem::Lock => t!("toolbar.item.lock"),
        }
    }

    /// 什么时候出现（设置中名称后面的说明）；总在的为 `None`。
    pub fn condition(self) -> Option<SharedString> {
        match self {
            ToolbarItem::HostTools
            | ToolbarItem::MultiExec
            | ToolbarItem::Snippets
            | ToolbarItem::Compose => Some(t!("toolbar.when.terminals")),
            ToolbarItem::Forwards => Some(t!("toolbar.when.forwards")),
            ToolbarItem::Sftp => Some(t!("toolbar.when.ssh")),
            ToolbarItem::Lock => Some(t!("toolbar.when.app_lock")),
            ToolbarItem::Highlight | ToolbarItem::Theme => None,
        }
    }

    /// 设置中、“更多”菜单中的图标（标题栏上有的随状态变化，见工作区的标题栏）。
    pub fn icon(self) -> IconName {
        match self {
            ToolbarItem::HostTools => IconName::PanelRightOpen,
            ToolbarItem::MultiExec => IconName::LayoutGrid,
            ToolbarItem::Forwards => IconName::ArrowRightLeft,
            ToolbarItem::Sftp => IconName::FolderOpen,
            ToolbarItem::Snippets => IconName::Zap,
            ToolbarItem::Compose => IconName::SquarePen,
            ToolbarItem::Highlight => IconName::Highlighter,
            ToolbarItem::Theme => IconName::Moon,
            ToolbarItem::Lock => IconName::Lock,
        }
    }
}

/// 工具栏中的一项：按钮与是否显示。
pub type Entry = (ToolbarItem, bool);

/// 默认的工具栏。
pub fn defaults() -> Vec<Entry> {
    ToolbarItem::ALL
        .into_iter()
        .map(|item| (item, item.shown_by_default()))
        .collect()
}

/// 按设置排出全部按钮的顺序与显示：设置中列出的按设置（不认识的、重复的跳过）；没列出的（新版本加的按钮）按默认
/// 显示与否，放在默认顺序中排在它前面、已经排出的按钮之后（前面的都没有时放在最前）。设置为空时就是默认。
pub fn resolve(saved: &[ToolbarButton]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for button in saved {
        if let Some(item) = ToolbarItem::from_id(&button.id)
            && !entries.iter().any(|(i, _)| *i == item)
        {
            entries.push((item, button.shown));
        }
    }
    for (ix, item) in ToolbarItem::ALL.into_iter().enumerate() {
        if entries.iter().any(|(i, _)| *i == item) {
            continue;
        }
        let at = ToolbarItem::ALL[..ix]
            .iter()
            .rev()
            .find_map(|before| entries.iter().position(|(i, _)| i == before))
            .map_or(0, |pos| pos + 1);
        entries.insert(at, (item, item.shown_by_default()));
    }
    entries
}

/// 设置中不认识的按钮（更新的版本加的）：保存时原样附在后面，不丢。
pub fn unknown(saved: &[ToolbarButton]) -> Vec<ToolbarButton> {
    saved
        .iter()
        .filter(|b| ToolbarItem::from_id(&b.id).is_none())
        .cloned()
        .collect()
}

/// 存进设置的写法：与默认相同（也没有不认识的按钮）时存空，否则逐项写出，再附上不认识的。
pub fn to_saved(entries: &[Entry], unknown: &[ToolbarButton]) -> Vec<ToolbarButton> {
    if unknown.is_empty() && entries == defaults().as_slice() {
        return Vec::new();
    }
    entries
        .iter()
        .map(|(item, shown)| ToolbarButton {
            id: item.id().to_string(),
            shown: *shown,
        })
        .chain(unknown.iter().cloned())
        .collect()
}

/// 把 `item` 上移（`up`）或下移一位；已在头、尾时不动。返回移动了没有。
pub fn move_entry(entries: &mut [Entry], item: ToolbarItem, up: bool) -> bool {
    let Some(ix) = entries.iter().position(|(i, _)| *i == item) else {
        return false;
    };
    let target = if up {
        ix.checked_sub(1)
    } else {
        (ix + 1 < entries.len()).then_some(ix + 1)
    };
    let Some(target) = target else {
        return false;
    };
    entries.swap(ix, target);
    true
}

/// 设置中显示的按钮，按顺序。
pub fn shown(saved: &[ToolbarButton]) -> Vec<ToolbarItem> {
    resolve(saved)
        .into_iter()
        .filter_map(|(item, shown)| shown.then_some(item))
        .collect()
}

/// 立即隐藏一个按钮（标题栏按钮的右键菜单），并说明在哪里恢复；保存失败时提示原因。
pub fn hide(item: ToolbarItem, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    let saved = store.local_settings().window.toolbar;
    let mut entries = resolve(&saved);
    if let Some(entry) = entries.iter_mut().find(|(i, _)| *i == item) {
        entry.1 = false;
    }
    let toolbar = to_saved(&entries, &unknown(&saved));
    match store.update_local_settings(|s| s.window.toolbar = toolbar) {
        Ok(()) => {
            tracing::info!(button = item.id(), "隐藏工具栏的按钮");
            window.push_notification(
                Notification::info(t!("toolbar.hidden", name = item.title())),
                cx,
            );
        }
        Err(e) => {
            tracing::error!("保存工具栏的设置失败：{e}");
            crate::notify::error(
                t!("settings.save_failed", error = store_errors::describe(&e)),
                window,
                cx,
            );
        }
    }
}

// ------------------------------------------------------------ 放不下时

/// 工具栏中按钮之间的间距（rem，与标题栏中的 `gap_1` 相同）。
pub const GAP: f32 = 0.25;
/// 还没量过“更多”按钮时按这个宽度算（rem，小号的图标按钮）。
const MORE_DEFAULT: f32 = 1.5;
/// 排版的浮点误差。
const EPSILON: f32 = 0.01;

/// 工具栏放不放得下：每帧排版后记下各按钮、“更多”按钮的宽度与工具栏可用的宽度（rem，与界面大小无关），按它们算出
/// 前几个按钮放得下，其余收进“更多”菜单；算出的个数与这一帧画的不同时再画一帧。
///
/// 按钮的宽度与可用的宽度都不随放进几个按钮而变（可用的宽度是左右两组之间余下的空间），不会来回跳。收进菜单的按钮
/// 沿用上次量到的宽度；从没量过的（刚出现的）按 0 算，先排出来，下一帧量到再算。
#[derive(Clone, Default)]
pub struct Fit(Rc<RefCell<FitState>>);

#[derive(Default)]
struct FitState {
    widths: HashMap<ToolbarItem, f32>,
    more: Option<f32>,
    available: Option<f32>,
}

impl Fit {
    /// `present`（这一帧要显示的按钮，按顺序）中前几个放得下。还没量过可用的宽度时全部排出。
    pub fn visible(&self, present: &[ToolbarItem]) -> usize {
        let state = self.0.borrow();
        let Some(available) = state.available else {
            return present.len();
        };
        let widths: Vec<Option<f32>> = present
            .iter()
            .map(|item| state.widths.get(item).copied())
            .collect();
        fit(&widths, available, state.more.unwrap_or(MORE_DEFAULT))
    }

    /// 记下这一帧排出的按钮（`shown`，按顺序）与“更多”按钮（`more`，有时排在最后）的宽度；`bounds` 为工具栏中
    /// 各子元素的位置，`rem` 为这一帧的 rem 大小。
    pub fn record_items(
        &self,
        shown: &[ToolbarItem],
        more: bool,
        bounds: &[Bounds<Pixels>],
        rem: Pixels,
    ) {
        let rem = f32::from(rem).max(1.);
        let mut state = self.0.borrow_mut();
        for (item, bounds) in shown.iter().zip(bounds) {
            state
                .widths
                .insert(*item, f32::from(bounds.size.width) / rem);
        }
        if more && let Some(bounds) = bounds.get(shown.len()) {
            state.more = Some(f32::from(bounds.size.width) / rem);
        }
    }

    /// 记下工具栏可用的宽度。
    pub fn record_available(&self, width: Pixels, rem: Pixels) {
        self.0.borrow_mut().available = Some(f32::from(width) / f32::from(rem).max(1.));
    }
}

/// 前几个按钮放得下：`widths` 为各按钮的宽度（没量过的为 `None`，按 0 算），`available` 为可用的宽度，按钮之间隔
/// [`GAP`]。全部放得下时为全部；否则还要留出“更多”按钮（`more` 宽）的位置。
pub fn fit(widths: &[Option<f32>], available: f32, more: f32) -> usize {
    let total = |n: usize| -> f32 {
        widths[..n].iter().map(|w| w.unwrap_or(0.)).sum::<f32>() + GAP * n.saturating_sub(1) as f32
    };
    if total(widths.len()) <= available + EPSILON {
        return widths.len();
    }
    (0..widths.len())
        .rev()
        .find(|&n| {
            let gap = if n > 0 { GAP } else { 0. };
            total(n) + gap + more <= available + EPSILON
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button(id: &str, shown: bool) -> ToolbarButton {
        ToolbarButton {
            id: id.into(),
            shown,
        }
    }

    // 设置为空时是默认：与原来的标题栏相同的顺序，撰写栏默认不显示；ID 各不相同、能认回来
    #[test]
    fn empty_setting_is_the_default() {
        let entries = resolve(&[]);
        assert_eq!(entries, defaults());
        assert_eq!(entries.len(), ToolbarItem::ALL.len());
        assert_eq!(
            entries.iter().filter(|(_, shown)| !shown).count(),
            1,
            "只有撰写栏默认不显示"
        );
        assert_eq!(shown(&[]).first(), Some(&ToolbarItem::HostTools));
        for item in ToolbarItem::ALL {
            assert_eq!(ToolbarItem::from_id(item.id()), Some(item));
        }
        assert_eq!(to_saved(&entries, &[]), Vec::<ToolbarButton>::new());
    }

    // 排序、隐藏后存下再读回不变；不认识的（新版本的）按钮保存时原样附在后面，重复的只算第一个
    #[test]
    fn order_and_visibility_round_trip() {
        let mut entries = defaults();
        assert!(move_entry(&mut entries, ToolbarItem::Theme, true));
        assert!(
            !move_entry(&mut entries, ToolbarItem::HostTools, true),
            "已在最前"
        );
        assert!(
            !move_entry(&mut entries, ToolbarItem::Lock, false),
            "已在最后"
        );
        entries.iter_mut().for_each(|e| {
            if e.0 == ToolbarItem::HostTools || e.0 == ToolbarItem::Compose {
                e.1 = !e.1;
            }
        });
        let future = vec![button("aiChat", true)];
        let saved = to_saved(&entries, &future);
        assert_eq!(saved.last(), Some(&button("aiChat", true)));
        assert_eq!(resolve(&saved), entries);
        assert_eq!(unknown(&saved), future);
        let shown = shown(&saved);
        assert!(!shown.contains(&ToolbarItem::HostTools));
        assert!(shown.contains(&ToolbarItem::Compose));
        let theme = shown.iter().position(|i| *i == ToolbarItem::Theme);
        let highlight = shown.iter().position(|i| *i == ToolbarItem::Highlight);
        assert!(theme < highlight, "主题移到了关键词高亮前面");

        let twice = vec![button("theme", false), button("theme", true)];
        assert!(!resolve(&twice).contains(&(ToolbarItem::Theme, true)));
    }

    // 恢复默认：改回默认的顺序与显示后存空
    #[test]
    fn back_to_default_saves_nothing() {
        let mut entries = defaults();
        move_entry(&mut entries, ToolbarItem::Lock, true);
        assert!(!to_saved(&entries, &[]).is_empty());
        move_entry(&mut entries, ToolbarItem::Lock, false);
        assert!(to_saved(&entries, &[]).is_empty());
    }

    // 设置中没列出的按钮（保存之后的版本才加的）按默认显示，放在默认顺序中它前面的按钮之后
    #[test]
    fn missing_buttons_take_their_default_place() {
        let saved = vec![
            button("theme", true),
            button("hostTools", false),
            button("snippets", true),
        ];
        let entries = resolve(&saved);
        let order: Vec<ToolbarItem> = entries.iter().map(|(i, _)| *i).collect();
        use ToolbarItem::*;
        assert_eq!(
            order,
            [
                Theme, Lock, HostTools, MultiExec, Forwards, Sftp, Snippets, Compose, Highlight
            ]
        );
        assert!(entries.contains(&(Compose, false)), "撰写栏默认不显示");
        assert!(entries.contains(&(HostTools, false)));
        assert!(entries.contains(&(Lock, true)));
    }

    // 放得下就全部排出；放不下时留出“更多”按钮，后面的收进菜单；没量过的按 0 算
    #[test]
    fn fit_keeps_the_leading_buttons() {
        let widths = [Some(1.5), Some(1.5), Some(4.), Some(1.5)];
        // 1.5 + 1.5 + 4 + 1.5 + 3 × 0.25 = 9.25
        assert_eq!(fit(&widths, 9.25, 1.5), 4);
        assert_eq!(fit(&widths, 20., 1.5), 4);
        // 前两个 3.25，加间距与“更多” 1.75 = 5
        assert_eq!(fit(&widths, 9., 1.5), 2);
        assert_eq!(fit(&widths, 5., 1.5), 2);
        assert_eq!(fit(&widths, 4.9, 1.5), 1);
        assert_eq!(fit(&widths, 1., 1.5), 0, "连“更多”都放不下时也只剩“更多”");
        assert_eq!(fit(&[None, None], 1., 1.5), 2, "没量过的先排出来");
        assert_eq!(fit(&[], 0., 1.5), 0);
    }

    // 量过一帧后按量到的宽度算：按钮放不下时收起后面的，“更多”的宽度也计入
    #[test]
    fn fit_uses_the_measured_widths() {
        use gpui_kit::{point, px, size};
        let fit = Fit::default();
        let present = [
            ToolbarItem::HostTools,
            ToolbarItem::Highlight,
            ToolbarItem::Theme,
        ];
        assert_eq!(fit.visible(&present), 3, "还没量过");
        let rem = px(24.);
        let bounds: Vec<Bounds<Pixels>> = (0..3)
            .map(|i| Bounds::new(point(px(i as f32 * 42.), px(0.)), size(px(36.), px(36.))))
            .collect();
        fit.record_items(&present, false, &bounds, rem);
        // 每个 1.5rem：三个要 5rem，两个加“更多”也要 5rem，一个加“更多”要 3.25rem
        fit.record_available(px(120.), rem);
        assert_eq!(fit.visible(&present), 3);
        fit.record_available(px(119.), rem);
        assert_eq!(fit.visible(&present), 1);
        fit.record_available(px(48.), rem);
        assert_eq!(fit.visible(&present), 0);
    }
}
