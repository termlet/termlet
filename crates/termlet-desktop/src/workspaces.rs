//! 保存的工作区：把分屏布局（各窗格连接的主机）保存下来，之后一键打开其中的全部会话。
//!
//! - 保存：中间区域的布局，每个窗格取正在显示的那个终端或 SFTP；快速连接的不保存（没有主机记录），
//!   本地终端记作 `local:<shell ID>`。只剩一个子节点的分支由它替代，比例按剩下的重新分配。
//! - 打开：中间区域是空的就按工作区排列，否则排在已有布局的右侧。主机已删除的窗格跳过。
//!   SFTP 跟随同一台主机的第一个终端（共用它的连接）。
//! - 最多 50 个工作区；一个工作区最多 16 个窗格、6 层。

use std::sync::Arc;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::dock::{
    BasePanelView as PanelView, DockArea, DockLayout, PaneNode, PaneRef, PaneTree, PanelId,
};
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, Axis as GpuiAxis, Pixels, Size, Task, Window, px};
use termlet_core::id::{HostId, WorkspaceId};
use termlet_core::model::{Axis, LayoutNode, SessionKind, Workspace};
use termlet_core::store::Position;

use crate::app_state::AppState;
use crate::name_dialog::{self, NameDialog};
use crate::prose::prose;
use crate::{store_errors, t};

pub const MAX_WORKSPACES: usize = 50;
pub const MAX_PANES: usize = 16;
pub const MAX_DEPTH: usize = 6;

/// 本地终端在工作区中的写法。
pub fn local_host(shell_id: &str) -> HostId {
    HostId::from_raw(format!("local:{shell_id}"))
}

/// WSL 终端在工作区、命令历史中的写法：`wsl:<发行版>`，以 root 登录的为 `wsl:<发行版>:root`。
pub fn wsl_key(distro: &str, user: &str) -> String {
    if user.is_empty() {
        format!("wsl:{distro}")
    } else {
        format!("wsl:{distro}:{user}")
    }
}

pub fn wsl_host(distro: &str, user: &str) -> HostId {
    HostId::from_raw(wsl_key(distro, user))
}

/// 工作区中的会话是不是 WSL 终端；是时返回发行版与用户。
pub fn wsl_of(host: &HostId) -> Option<(String, String)> {
    let rest = host.as_str().strip_prefix("wsl:")?;
    let (distro, user) = rest.split_once(':').unwrap_or((rest, ""));
    (!distro.is_empty()).then(|| (distro.to_string(), user.to_string()))
}

/// 工作区中的会话是不是本地终端；是时返回 shell ID。
pub fn local_shell_of(host: &HostId) -> Option<&str> {
    host.as_str()
        .strip_prefix("local:")
        .filter(|id| !id.is_empty())
}

/// 窗格中的面板是什么会话（不能保存的为 `None`）。
pub type Leaf = Option<(SessionKind, HostId)>;

/// 中间区域的布局 → 工作区的布局：`leaf` 给出窗格中正在显示的面板对应的会话。
pub fn capture(tree: &PaneTree, leaf: &impl Fn(PanelId) -> Leaf) -> Option<LayoutNode> {
    capture_node(tree.root(), leaf)
}

fn capture_node(node: &PaneNode, leaf: &impl Fn(PanelId) -> Leaf) -> Option<LayoutNode> {
    match node.kind() {
        PaneRef::Tabs { panels, active_ix } => {
            let panel = panels.get(active_ix).or(panels.first())?;
            leaf(*panel).map(|(kind, host)| LayoutNode::Session { kind, host })
        }
        PaneRef::Split {
            axis,
            children,
            sizes,
        } => branch(
            match axis {
                GpuiAxis::Horizontal => Axis::Horizontal,
                GpuiAxis::Vertical => Axis::Vertical,
            },
            children
                .iter()
                .zip(sizes.iter().chain(std::iter::repeat(&None)))
                .map(|(child, size)| (capture_node(child, leaf), size.map(f32::from)))
                .collect(),
        ),
    }
}

/// 子节点为 `None` 的去掉，比例按剩下的重新分配（有子节点没有大小时平分）；只剩一个时由它替代分支。
fn branch(axis: Axis, items: Vec<(Option<LayoutNode>, Option<f32>)>) -> Option<LayoutNode> {
    let kept: Vec<(LayoutNode, Option<f32>)> = items
        .into_iter()
        .filter_map(|(node, size)| node.map(|n| (n, size)))
        .collect();
    if kept.len() <= 1 {
        return kept.into_iter().next().map(|(node, _)| node);
    }
    let all_sized = kept.iter().all(|(_, s)| s.is_some_and(|s| s > 0.));
    let total: f32 = kept.iter().filter_map(|(_, s)| *s).sum();
    let count = kept.len() as f32;
    let (children, sizes) = kept
        .into_iter()
        .map(|(node, size)| {
            let ratio = match size {
                Some(s) if all_sized && total > 0. => s / total,
                _ => 1. / count,
            };
            (node, ratio)
        })
        .unzip();
    Some(LayoutNode::Split {
        axis,
        children,
        sizes,
    })
}

/// 窗格数与层数（只有一个窗格时为 1 层）。
pub fn measure(node: &LayoutNode) -> (usize, usize) {
    match node {
        LayoutNode::Session { .. } => (1, 1),
        LayoutNode::Split { children, .. } => {
            let (panes, depth) = children
                .iter()
                .map(measure)
                .fold((0, 0), |(p, d), (cp, cd)| (p + cp, d.max(cd)));
            (panes, depth + 1)
        }
    }
}

/// 工作区的布局 → 中间区域的布局：`make` 为每个会话建一个面板（主机已删除等返回 `None`，随之去掉）。
/// `extent` 是放置区域的大小，用来把比例换成像素；为零时各子节点平分。
pub fn build(
    node: &LayoutNode,
    extent: Size<Pixels>,
    make: &mut impl FnMut(SessionKind, &HostId) -> Option<Arc<dyn PanelView>>,
    cx: &App,
) -> Option<DockLayout> {
    match node {
        LayoutNode::Session { kind, host } => {
            make(*kind, host).map(|view| DockLayout::tabs().panel_view(view, cx))
        }
        LayoutNode::Split {
            axis,
            children,
            sizes,
        } => {
            let along = match axis {
                Axis::Horizontal => extent.width,
                Axis::Vertical => extent.height,
            };
            let built: Vec<(DockLayout, f32)> = children
                .iter()
                .enumerate()
                .filter_map(|(i, child)| {
                    let ratio = sizes.get(i).copied().unwrap_or(0.);
                    let child_extent = match axis {
                        Axis::Horizontal => Size {
                            width: along * ratio,
                            height: extent.height,
                        },
                        Axis::Vertical => Size {
                            width: extent.width,
                            height: along * ratio,
                        },
                    };
                    build(child, child_extent, make, cx).map(|layout| (layout, ratio))
                })
                .collect();
            if built.len() <= 1 {
                return built.into_iter().next().map(|(layout, _)| layout);
            }
            let total: f32 = built.iter().map(|(_, r)| *r).sum();
            let mut split = match axis {
                Axis::Horizontal => DockLayout::h_split(),
                Axis::Vertical => DockLayout::v_split(),
            };
            for (layout, ratio) in built {
                let size = (along > px(0.) && total > 0.).then(|| along * (ratio / total));
                split = split.child(layout, size);
            }
            Some(split)
        }
    }
}

/// 已有的中间区域 → 布局描述（打开工作区时把它放在左边，面板原样保留）。
pub fn existing(dock: &DockArea, node: &PaneNode, cx: &App) -> Option<DockLayout> {
    match node.kind() {
        PaneRef::Tabs { panels, active_ix } => {
            let mut tabs = DockLayout::tabs();
            let mut any = false;
            for panel in panels {
                if let Some(view) = dock.panel(*panel) {
                    tabs = tabs.panel_view(view.clone(), cx);
                    any = true;
                }
            }
            any.then(|| tabs.active_index(active_ix))
        }
        PaneRef::Split {
            axis,
            children,
            sizes,
        } => {
            let mut split = match axis {
                GpuiAxis::Horizontal => DockLayout::h_split(),
                GpuiAxis::Vertical => DockLayout::v_split(),
            };
            let mut any = false;
            for (i, child) in children.iter().enumerate() {
                if let Some(layout) = existing(dock, child, cx) {
                    split = split.child(layout, sizes.get(i).copied().flatten());
                    any = true;
                }
            }
            any.then_some(split)
        }
    }
}

// ------------------------------------------------------------------ 对话框

/// 输入名称后保存为工作区。
pub fn prompt_save(layout: LayoutNode, default_name: String, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    if store.list::<Workspace>().len() >= MAX_WORKSPACES {
        crate::notify::error(t!("workspace.too_many", max = MAX_WORKSPACES), window, cx);
        return;
    }
    name_dialog::open(
        NameDialog {
            title: t!("workspace.save_title"),
            value: default_name,
            placeholder: t!("workspace.name_placeholder"),
            select_all: true,
            ok_label: t!("common.save"),
            input_id: "workspace-name",
        },
        Box::new(move |name, window, cx| {
            let result = store
                .create(
                    Workspace {
                        name: name.clone(),
                        layout: layout.clone(),
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .map_err(|e| store_errors::describe(&e));
            if let Ok(id) = &result {
                tracing::info!(name = %name, "已保存工作区");
                window.push_notification(
                    Notification::success(t!("workspace.saved", name = name)),
                    cx,
                );
                // “工作区”一节默认折叠：展开并选中新保存的
                crate::workspace::Workspace::reveal_in_sidebar(
                    crate::sidebar::rows::RowKey::Workspace(id.clone()),
                    window,
                    cx,
                );
            }
            Task::ready(result.map(|_| ()))
        }),
        window,
        cx,
    );
}

/// 重命名工作区。
pub fn prompt_rename(id: WorkspaceId, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    let Some(record) = store.get::<Workspace>(&id) else {
        return;
    };
    let doc = record.doc;
    name_dialog::open(
        NameDialog {
            title: t!("workspace.rename_title"),
            value: doc.name.clone(),
            placeholder: t!("workspace.name_placeholder"),
            select_all: true,
            ok_label: t!("common.save"),
            input_id: "workspace-name",
        },
        Box::new(move |name, _, _| {
            let result = store
                .update(
                    &id,
                    Workspace {
                        name,
                        ..doc.clone()
                    },
                )
                .map_err(|e| store_errors::describe(&e));
            Task::ready(result)
        }),
        window,
        cx,
    );
}

/// 确认后删除工作区（不影响其中的主机）。
pub fn confirm_delete(id: WorkspaceId, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    let Some(record) = store.get::<Workspace>(&id) else {
        return;
    };
    let name = record.doc.name;
    crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
        let store = store.clone();
        let id = id.clone();
        alert
            .confirm()
            .title(t!("workspace.delete_title", name = name.clone()))
            .description(prose(t!("workspace.delete_description")))
            .ok_text(t!("common.delete"))
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(t!("common.cancel"))
            .on_ok(move |_, window, cx| {
                tracing::info!(id = %id, "删除工作区");
                if let Err(e) = store.delete::<Workspace>(&id) {
                    crate::notify::error(store_errors::describe(&e), window, cx);
                }
                true
            })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(host: &str) -> LayoutNode {
        LayoutNode::Session {
            kind: SessionKind::Terminal,
            host: HostId::from_raw(host),
        }
    }

    #[test]
    fn branch_drops_missing_children_and_renormalizes() {
        let node = branch(
            Axis::Horizontal,
            vec![
                (Some(leaf("a")), Some(300.)),
                (None, Some(300.)),
                (Some(leaf("b")), Some(100.)),
            ],
        )
        .unwrap();
        let LayoutNode::Split {
            sizes, children, ..
        } = node
        else {
            panic!("应为分支");
        };
        assert_eq!(children.len(), 2);
        assert!((sizes[0] - 0.75).abs() < 1e-6 && (sizes[1] - 0.25).abs() < 1e-6);
        // 只剩一个时由它替代分支
        assert_eq!(
            branch(Axis::Vertical, vec![(Some(leaf("a")), None), (None, None)]),
            Some(leaf("a"))
        );
        assert_eq!(branch(Axis::Vertical, vec![(None, None)]), None);
        // 有子节点没有大小时平分
        let LayoutNode::Split { sizes, .. } = branch(
            Axis::Vertical,
            vec![(Some(leaf("a")), Some(10.)), (Some(leaf("b")), None)],
        )
        .unwrap() else {
            panic!("应为分支");
        };
        assert_eq!(sizes, [0.5, 0.5]);
    }

    #[test]
    fn measure_counts_panes_and_depth() {
        assert_eq!(measure(&leaf("a")), (1, 1));
        let nested = LayoutNode::Split {
            axis: Axis::Horizontal,
            children: vec![
                leaf("a"),
                LayoutNode::Split {
                    axis: Axis::Vertical,
                    children: vec![leaf("b"), leaf("c")],
                    sizes: vec![0.5, 0.5],
                },
            ],
            sizes: vec![0.5, 0.5],
        };
        assert_eq!(measure(&nested), (3, 3));
    }

    #[test]
    fn local_hosts_round_trip() {
        assert_eq!(local_shell_of(&local_host("pwsh")), Some("pwsh"));
        assert_eq!(local_shell_of(&HostId::from_raw("h_abc")), None);
        assert_eq!(local_shell_of(&HostId::from_raw("local:")), None);
    }
}
