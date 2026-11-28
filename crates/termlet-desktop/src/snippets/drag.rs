//! 快捷命令页的拖动排序：快捷命令在组内、跨组拖动（跨组即改分组），分组标题拖动调整分组的先后（未分组固定在最前）。
//! 放在哪里、放下后怎样移动都按行算，写成纯函数；写入见核心的 `Store::move_snippet` 与设置中的分组顺序。

use termlet_core::id::SnippetId;
use termlet_core::store::Position;

/// 正在拖动的东西。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dragged {
    Snippet(SnippetId),
    /// 分组名（未分组不能拖）
    Group(String),
}

/// 能放的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropRow {
    /// 快捷命令与它所在的分组
    Snippet { id: SnippetId, group: String },
    /// 分组标题（空串为未分组）
    Group(String),
    /// 拖动快捷命令、又没有未分组的标题时最下面多出的“放到这里移到未分组”
    Ungrouped,
}

/// 放在目标行的哪里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropPlace {
    Before,
    /// 放进分组（排到最后）
    Into,
    After,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropTarget {
    pub row: DropRow,
    pub place: DropPlace,
}

/// 拖到 `row` 上、指针在行内的相对高度为 `y`（0 在上沿、1 在下沿）时放在哪里；不能放时为 `None`。
///
/// - 快捷命令拖到快捷命令上：上半在前、下半在后（进入那一条所在的分组）；拖到分组标题或“移到未分组”上：放进去。
/// - 分组拖到别的分组标题上：上半在前、下半在后。未分组的标题与快捷命令的行不接受分组。
pub fn drop_target(dragged: &Dragged, row: &DropRow, y: f32) -> Option<DropTarget> {
    let half = if y < 0.5 {
        DropPlace::Before
    } else {
        DropPlace::After
    };
    let place = match (dragged, row) {
        (Dragged::Snippet(moving), DropRow::Snippet { id, .. }) if moving == id => return None,
        (Dragged::Snippet(_), DropRow::Snippet { .. }) => half,
        (Dragged::Snippet(_), DropRow::Group(_) | DropRow::Ungrouped) => DropPlace::Into,
        (Dragged::Group(moving), DropRow::Group(target))
            if !target.is_empty() && target != moving =>
        {
            half
        }
        _ => return None,
    };
    Some(DropTarget {
        row: row.clone(),
        place,
    })
}

/// 放下后的移动。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Move {
    /// 把快捷命令移到分组 `group`（空串为未分组）中的 `position`
    Snippet {
        id: SnippetId,
        group: String,
        position: Position<SnippetId>,
    },
    /// 分组的新顺序（全部有名称的分组）
    Groups(Vec<String>),
}

/// `groups` 是现在显示的分组顺序（有未分组时空串在最前）。顺序没有变化时为 `None`。
pub fn plan_move(dragged: &Dragged, target: &DropTarget, groups: &[String]) -> Option<Move> {
    match (dragged, &target.row, target.place) {
        (Dragged::Snippet(id), DropRow::Snippet { id: beside, group }, place) => {
            Some(Move::Snippet {
                id: id.clone(),
                group: group.clone(),
                position: match place {
                    DropPlace::Before => Position::Before(beside.clone()),
                    _ => Position::After(beside.clone()),
                },
            })
        }
        (Dragged::Snippet(id), DropRow::Group(group), _) => Some(Move::Snippet {
            id: id.clone(),
            group: group.clone(),
            position: Position::Last,
        }),
        (Dragged::Snippet(id), DropRow::Ungrouped, _) => Some(Move::Snippet {
            id: id.clone(),
            group: String::new(),
            position: Position::Last,
        }),
        (Dragged::Group(moving), DropRow::Group(beside), place) => {
            let current: Vec<String> = groups.iter().filter(|g| !g.is_empty()).cloned().collect();
            let mut order: Vec<String> = current.iter().filter(|g| *g != moving).cloned().collect();
            let at = order.iter().position(|g| g == beside)?;
            let at = if place == DropPlace::Before {
                at
            } else {
                at + 1
            };
            order.insert(at, moving.clone());
            (order != current).then_some(Move::Groups(order))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: &str) -> SnippetId {
        SnippetId::from_raw(raw)
    }

    fn row(raw: &str, group: &str) -> DropRow {
        DropRow::Snippet {
            id: id(raw),
            group: group.into(),
        }
    }

    fn groups(names: &[&str]) -> Vec<String> {
        names.iter().map(|g| g.to_string()).collect()
    }

    #[test]
    fn snippet_targets() {
        let moving = Dragged::Snippet(id("s_1"));
        let place = |row: &DropRow, y| drop_target(&moving, row, y).map(|t| t.place);
        assert_eq!(place(&row("s_2", "a"), 0.2), Some(DropPlace::Before));
        assert_eq!(place(&row("s_2", "a"), 0.8), Some(DropPlace::After));
        assert_eq!(place(&row("s_1", "a"), 0.2), None, "不能放在自己身上");
        assert_eq!(
            place(&DropRow::Group("b".into()), 0.1),
            Some(DropPlace::Into)
        );
        assert_eq!(
            place(&DropRow::Group(String::new()), 0.9),
            Some(DropPlace::Into)
        );
        assert_eq!(place(&DropRow::Ungrouped, 0.5), Some(DropPlace::Into));
    }

    #[test]
    fn group_targets() {
        let moving = Dragged::Group("a".into());
        let place = |row: &DropRow, y| drop_target(&moving, row, y).map(|t| t.place);
        assert_eq!(
            place(&DropRow::Group("b".into()), 0.3),
            Some(DropPlace::Before)
        );
        assert_eq!(
            place(&DropRow::Group("b".into()), 0.7),
            Some(DropPlace::After)
        );
        assert_eq!(place(&DropRow::Group("a".into()), 0.3), None, "自己");
        assert_eq!(
            place(&DropRow::Group(String::new()), 0.3),
            None,
            "未分组固定在最前"
        );
        assert_eq!(place(&row("s_2", "b"), 0.3), None);
        assert_eq!(place(&DropRow::Ungrouped, 0.3), None);
    }

    // 快捷命令放在别的快捷命令前后进入它的分组，放到标题上排到那一组的最后，放到“移到未分组”上改为未分组
    #[test]
    fn snippet_moves() {
        let moving = Dragged::Snippet(id("s_1"));
        let plan = |row: DropRow, y| {
            let target = drop_target(&moving, &row, y).unwrap();
            plan_move(&moving, &target, &groups(&["", "a", "b"])).unwrap()
        };
        assert_eq!(
            plan(row("s_2", "b"), 0.2),
            Move::Snippet {
                id: id("s_1"),
                group: "b".into(),
                position: Position::Before(id("s_2")),
            }
        );
        assert_eq!(
            plan(row("s_2", "a"), 0.9),
            Move::Snippet {
                id: id("s_1"),
                group: "a".into(),
                position: Position::After(id("s_2")),
            }
        );
        assert_eq!(
            plan(DropRow::Group("b".into()), 0.5),
            Move::Snippet {
                id: id("s_1"),
                group: "b".into(),
                position: Position::Last,
            }
        );
        assert_eq!(
            plan(DropRow::Ungrouped, 0.5),
            Move::Snippet {
                id: id("s_1"),
                group: String::new(),
                position: Position::Last,
            }
        );
    }

    // 分组放在别的分组前后：得到全部有名称的分组的新顺序；没有变化时不写
    #[test]
    fn group_moves() {
        let current = groups(&["", "a", "b", "c"]);
        let plan = |moving: &str, beside: &str, y| {
            let dragged = Dragged::Group(moving.into());
            let target = drop_target(&dragged, &DropRow::Group(beside.into()), y)?;
            plan_move(&dragged, &target, &current)
        };
        assert_eq!(
            plan("c", "a", 0.2),
            Some(Move::Groups(groups(&["c", "a", "b"])))
        );
        assert_eq!(
            plan("a", "c", 0.8),
            Some(Move::Groups(groups(&["b", "c", "a"])))
        );
        assert_eq!(
            plan("a", "b", 0.8),
            Some(Move::Groups(groups(&["b", "a", "c"])))
        );
        assert_eq!(plan("a", "b", 0.2), None, "放回原处");
        assert_eq!(plan("b", "a", 0.8), None, "放回原处");
    }
}
