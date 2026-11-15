//! 条目的全局唯一 ID：类型前缀加 80 位随机数（16 个小写 base32 字符），例如 `h_3k9x2m4p8q1r7s5t`。
//!
//! ID 在新建时生成，改名、改地址、同步到另一台电脑都不变；CLI、导出、同步都用它。
//! 反序列化时不校验格式：新版本同步过来的条目也能原样保存。

use data_encoding::{Encoding, Specification};
use std::sync::LazyLock;

static BASE32_LOWER: LazyLock<Encoding> = LazyLock::new(|| {
    let mut spec = Specification::new();
    spec.symbols.push_str("abcdefghijklmnopqrstuvwxyz234567");
    spec.encoding().expect("base32 编码表")
});

/// 生成 `n` 字节的随机数，以小写 base32 编码（不补等号）。
pub fn random_token(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("系统随机数不可用");
    BASE32_LOWER.encode(&bytes)
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// ID 的类型前缀。
            pub const PREFIX: &'static str = $prefix;

            /// 生成一个新的随机 ID。
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(format!("{}{}", $prefix, random_token(10)))
            }

            /// 按原样包装已有的 ID（来自数据库、同步或命令行），不校验格式。
            pub fn from_raw(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

define_id!(
    /// 主机
    HostId, "h_"
);
define_id!(
    /// 主机分组
    GroupId, "g_"
);
define_id!(
    /// 凭据档案（多台主机共用的用户名与认证）
    ProfileId, "p_"
);
define_id!(
    /// 密钥库中的密钥
    KeyId, "k_"
);
define_id!(
    /// 快捷命令
    SnippetId, "s_"
);
define_id!(
    /// 端口转发规则
    ForwardId, "f_"
);
define_id!(
    /// 保存的工作区（分屏布局）
    WorkspaceId, "w_"
);
define_id!(
    /// 自定义的终端配色
    SchemeId, "c_"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_prefix_and_fixed_length() {
        let id = HostId::new();
        assert!(id.as_str().starts_with("h_"));
        assert_eq!(id.as_str().len(), 2 + 16);
        assert!(
            id.as_str()[2..]
                .chars()
                .all(|c| matches!(c, 'a'..='z' | '2'..='7'))
        );
        assert_ne!(HostId::new(), HostId::new());
    }

    #[test]
    fn ids_serialize_as_plain_strings() {
        let id = GroupId::from_raw("g_abc");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"g_abc\"");
        let back: GroupId = serde_json::from_str("\"g_abc\"").unwrap();
        assert_eq!(back, id);
    }
}
