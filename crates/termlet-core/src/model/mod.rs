//! 条目的内容（存在条目表的 `doc` 列中）。
//!
//! 每种条目是一个带默认值的结构体，序列化为 camelCase 的 JSON：
//! - 加选项只加带默认值的字段，不改表；读到缺少字段的旧数据时用默认值。
//! - 不认识的字段（更新版本的 Termlet 同步过来的）原样存在 `extra` 中，保存时写回，
//!   旧版本改过的条目同步回新版本时不会丢掉新版本的字段。
//! - 内容中不含任何秘密：密码、私钥等在 `secret` 表中，按"条目 / 字段"引用（见 [`SecretField`]）。

mod forward;
mod group;
mod highlight;
mod host;
mod key;
mod profile;
mod remote_os;
mod scheme;
mod settings;
mod snippet;
mod workspace;

use serde::Serialize;
use serde::de::DeserializeOwned;

pub use forward::*;
pub use group::*;
pub use highlight::*;
pub use host::*;
pub use key::*;
pub use profile::*;
pub use remote_os::RemoteOs;
pub(crate) use remote_os::valid_key as valid_os_key;
pub use scheme::*;
pub use settings::*;
pub use snippet::*;
pub use workspace::*;

use crate::Invalid;
use crate::id::{ForwardId, GroupId, HostId, KeyId, ProfileId, SchemeId, SnippetId, WorkspaceId};

/// 不认识的字段，原样保存。
pub type Extra = serde_json::Map<String, serde_json::Value>;

/// 条目的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Host,
    Group,
    Profile,
    Key,
    Snippet,
    Forward,
    Workspace,
    Scheme,
}

impl Kind {
    pub const ALL: [Kind; 8] = [
        Kind::Host,
        Kind::Group,
        Kind::Profile,
        Kind::Key,
        Kind::Snippet,
        Kind::Forward,
        Kind::Workspace,
        Kind::Scheme,
    ];

    /// 数据库中的表名。
    pub fn table(self) -> &'static str {
        match self {
            Kind::Host => "host",
            Kind::Group => "host_group",
            Kind::Profile => "profile",
            Kind::Key => "ssh_key",
            Kind::Snippet => "snippet",
            Kind::Forward => "forward",
            Kind::Workspace => "workspace",
            Kind::Scheme => "color_scheme",
        }
    }

    /// 秘密的归属、同步记录中的类型名。
    pub fn name(self) -> &'static str {
        match self {
            Kind::Host => "host",
            Kind::Group => "group",
            Kind::Profile => "profile",
            Kind::Key => "key",
            Kind::Snippet => "snippet",
            Kind::Forward => "forward",
            Kind::Workspace => "workspace",
            Kind::Scheme => "scheme",
        }
    }
}

/// 条目的 ID 类型（[`crate::id`] 中定义的各种 ID）。
pub trait ItemId:
    Clone
    + Eq
    + std::hash::Hash
    + Ord
    + std::fmt::Debug
    + std::fmt::Display
    + Serialize
    + DeserializeOwned
    + Send
    + Sync
    + 'static
{
    fn generate() -> Self;
    fn from_raw(raw: String) -> Self;
    fn as_str(&self) -> &str;
}

macro_rules! impl_item_id {
    ($($id:ty),*) => {$(
        impl ItemId for $id {
            fn generate() -> Self {
                <$id>::new()
            }
            fn from_raw(raw: String) -> Self {
                <$id>::from_raw(raw)
            }
            fn as_str(&self) -> &str {
                <$id>::as_str(self)
            }
        }
    )*};
}

impl_item_id!(
    HostId,
    GroupId,
    ProfileId,
    KeyId,
    SnippetId,
    ForwardId,
    WorkspaceId,
    SchemeId
);

/// 一种条目的内容。
pub trait Item:
    Clone + Default + PartialEq + std::fmt::Debug + Serialize + DeserializeOwned + Send + Sync + 'static
{
    type Id: ItemId;
    const KIND: Kind;

    /// 保存前补全默认值、去掉首尾空白并校验。
    fn normalize(&mut self) -> Result<(), Invalid> {
        Ok(())
    }

    /// 编辑时保留 `stored`（存储中现在的内容）里不由用户编辑的字段（主机识别出的系统）：编辑对话框打开期间
    /// 写入的值不会被对话框里的旧值覆盖，同步写入条目时也不会被云端的条目记录（不含这些字段）清掉。
    fn keep_observed(&mut self, _stored: &Self) {}
}

impl Item for Host {
    type Id = HostId;
    const KIND: Kind = Kind::Host;
    fn normalize(&mut self) -> Result<(), Invalid> {
        Host::normalize(self)
    }
    fn keep_observed(&mut self, stored: &Self) {
        self.os = stored.os.clone();
    }
}

impl Item for Group {
    type Id = GroupId;
    const KIND: Kind = Kind::Group;
    fn normalize(&mut self) -> Result<(), Invalid> {
        Group::normalize(self)
    }
}

impl Item for Profile {
    type Id = ProfileId;
    const KIND: Kind = Kind::Profile;
    fn normalize(&mut self) -> Result<(), Invalid> {
        Profile::normalize(self)
    }
}

impl Item for KeyEntry {
    type Id = KeyId;
    const KIND: Kind = Kind::Key;
    fn normalize(&mut self) -> Result<(), Invalid> {
        KeyEntry::normalize(self)
    }
}

impl Item for Snippet {
    type Id = SnippetId;
    const KIND: Kind = Kind::Snippet;
    fn normalize(&mut self) -> Result<(), Invalid> {
        Snippet::normalize(self)
    }
}

impl Item for Forward {
    type Id = ForwardId;
    const KIND: Kind = Kind::Forward;
    fn normalize(&mut self) -> Result<(), Invalid> {
        Forward::normalize(self)
    }
}

impl Item for Workspace {
    type Id = WorkspaceId;
    const KIND: Kind = Kind::Workspace;
    fn normalize(&mut self) -> Result<(), Invalid> {
        Workspace::normalize(self)
    }
}

impl Item for ColorScheme {
    type Id = SchemeId;
    const KIND: Kind = Kind::Scheme;
    fn normalize(&mut self) -> Result<(), Invalid> {
        ColorScheme::normalize(self)
    }
}

/// 属于某个条目的一项秘密。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SecretField {
    /// 登录密码
    Password,
    /// 粘贴的私钥（主机、凭据档案），或密钥库中的私钥（统一转为不加密的 OpenSSH 格式）
    PrivateKey,
    /// 私钥文件或粘贴的私钥的密码
    Passphrase,
    /// TOTP 密钥（Base32 或 `otpauth://` 链接）
    Totp,
    /// 代理的密码
    ProxyPassword,
    /// CLI 专用凭据的密码
    CliPassword,
    /// CLI 专用凭据粘贴的私钥
    CliPrivateKey,
    /// CLI 专用凭据的私钥密码
    CliPassphrase,
}

impl SecretField {
    pub fn name(self) -> &'static str {
        match self {
            SecretField::Password => "password",
            SecretField::PrivateKey => "private_key",
            SecretField::Passphrase => "passphrase",
            SecretField::Totp => "totp",
            SecretField::ProxyPassword => "proxy_password",
            SecretField::CliPassword => "cli_password",
            SecretField::CliPrivateKey => "cli_private_key",
            SecretField::CliPassphrase => "cli_passphrase",
        }
    }
}

/// 去掉首尾空白；超过 `max` 个字符时报错。
pub(crate) fn trimmed(value: &mut String, field: &'static str, max: usize) -> Result<(), Invalid> {
    let t = value.trim();
    if t.len() != value.len() {
        *value = t.to_string();
    }
    if value.chars().count() > max {
        return Err(Invalid::new(field, "too_long"));
    }
    Ok(())
}

/// 不含控制字符（制表符除外）。
pub(crate) fn no_control_chars(value: &str, field: &'static str) -> Result<(), Invalid> {
    if value
        .chars()
        .any(|c| (c < ' ' && c != '\t') || c == '\u{7f}')
    {
        return Err(Invalid::new(field, "control_characters"));
    }
    Ok(())
}
