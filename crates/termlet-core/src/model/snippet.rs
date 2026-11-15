//! 快捷命令。内容末尾带换行时发送后直接执行；`{{名称}}`、`{{名称:默认值}}` 是发送前填写的参数。

use serde::{Deserialize, Serialize};

use super::{Extra, trimmed};
use crate::Invalid;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Snippet {
    pub name: String,
    pub content: String,
    /// 分组名；空串表示未分组（分组只是一个名称，不单独建表）
    pub group: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Snippet {
    pub(crate) fn normalize(&mut self) -> Result<(), Invalid> {
        trimmed(&mut self.name, "name", 128)?;
        trimmed(&mut self.group, "group", 64)?;
        if self.content.chars().count() > 64 * 1024 {
            return Err(Invalid::new("content", "too_long"));
        }
        if self.content.trim().is_empty() {
            return Err(Invalid::new("content", "required"));
        }
        if self.name.is_empty() {
            self.name = self
                .content
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or_default()
                .trim()
                .chars()
                .take(60)
                .collect();
        }
        Ok(())
    }
}
