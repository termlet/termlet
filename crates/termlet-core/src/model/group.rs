//! 主机分组（可嵌套；上级分组在条目行的 `parent_id` 上）。

use serde::{Deserialize, Serialize};

use super::{Extra, no_control_chars, trimmed};
use crate::Invalid;
use crate::id::{HostId, ProfileId};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Group {
    pub name: String,
    pub defaults: GroupDefaults,
    #[serde(flatten)]
    pub extra: Extra,
}

/// 分组的默认值：分组及其子分组中没有设置这些项的主机逐级向上沿用，主机自己的设置优先。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GroupDefaults {
    pub username: String,
    pub profile: Option<ProfileId>,
    pub jump: Option<HostId>,
    /// 终端配色（配色 ID）
    pub color_scheme: String,
    /// 代理命令（与主机的代理命令相同，见 [`super::Proxy::command`]）：没有自己的代理、也没有选"不使用代理"的
    /// 主机沿用；经跳板机连接的主机不使用
    #[serde(skip_serializing_if = "String::is_empty")]
    pub proxy_command: String,
}

impl Group {
    pub(crate) fn normalize(&mut self) -> Result<(), Invalid> {
        trimmed(&mut self.name, "name", 128)?;
        if self.name.is_empty() {
            return Err(Invalid::new("name", "required"));
        }
        trimmed(&mut self.defaults.username, "defaults.username", 128)?;
        let command = &mut self.defaults.proxy_command;
        trimmed(
            command,
            "defaults.proxyCommand",
            super::host::MAX_PROXY_COMMAND,
        )?;
        no_control_chars(command, "defaults.proxyCommand")?;
        Ok(())
    }
}
