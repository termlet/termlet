//! 凭据档案：保存一份用户名与认证，多台主机引用，改一处对所有使用它的主机生效。

use serde::{Deserialize, Serialize};

use super::{Extra, KeySource, no_control_chars, trimmed};
use crate::Invalid;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Profile {
    pub name: String,
    pub username: String,
    pub auth: ProfileAuth,
    #[serde(flatten)]
    pub extra: Extra,
}

/// 档案的认证；秘密（密码、粘贴的私钥、私钥密码）属于这个档案。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ProfileAuth {
    #[default]
    Password,
    #[serde(rename_all = "camelCase")]
    Key {
        source: KeySource,
    },
    Agent,
}

impl Profile {
    pub(crate) fn normalize(&mut self) -> Result<(), Invalid> {
        trimmed(&mut self.name, "name", 128)?;
        trimmed(&mut self.username, "username", 128)?;
        no_control_chars(&self.username, "username")?;
        if self.name.is_empty() {
            return Err(Invalid::new("name", "required"));
        }
        Ok(())
    }
}
