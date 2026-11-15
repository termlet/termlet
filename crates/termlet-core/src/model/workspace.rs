//! 保存的工作区：分屏布局与各窗格的主机，一键打开其中的全部会话。

use serde::{Deserialize, Serialize};

use super::{Extra, trimmed};
use crate::Invalid;
use crate::id::HostId;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Workspace {
    pub name: String,
    pub layout: LayoutNode,
    #[serde(flatten)]
    pub extra: Extra,
}

/// 布局树：叶子是一个会话，分支按方向排列子节点，`sizes` 是各子节点所占的比例。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "camelCase")]
pub enum LayoutNode {
    #[serde(rename_all = "camelCase")]
    Session { kind: SessionKind, host: HostId },
    #[serde(rename_all = "camelCase")]
    Split {
        axis: Axis,
        children: Vec<LayoutNode>,
        sizes: Vec<f32>,
    },
}

impl Default for LayoutNode {
    fn default() -> Self {
        LayoutNode::Split {
            axis: Axis::Horizontal,
            children: Vec::new(),
            sizes: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    #[default]
    Terminal,
    Sftp,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// 左右排列
    #[default]
    Horizontal,
    /// 上下排列
    Vertical,
}

impl Workspace {
    pub(crate) fn normalize(&mut self) -> Result<(), Invalid> {
        trimmed(&mut self.name, "name", 60)?;
        if self.name.is_empty() {
            return Err(Invalid::new("name", "required"));
        }
        Ok(())
    }

    /// 布局中出现的全部主机（按出现顺序，可能重复）。
    pub fn hosts(&self) -> Vec<&HostId> {
        fn walk<'a>(node: &'a LayoutNode, out: &mut Vec<&'a HostId>) {
            match node {
                LayoutNode::Session { host, .. } => out.push(host),
                LayoutNode::Split { children, .. } => {
                    children.iter().for_each(|child| walk(child, out))
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.layout, &mut out);
        out
    }
}
