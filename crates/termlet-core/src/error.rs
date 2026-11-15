//! 核心的错误类型。
//!
//! 消息是写给日志看的中文；界面按变体（与 [`Invalid`] 的字段、原因）选用自己的文案，不直接显示这里的文字。

use std::path::PathBuf;

/// 核心操作的结果。
pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("数据库错误：{0}")]
    Database(#[from] rusqlite::Error),

    #[error("读写文件失败：{0}")]
    Io(#[from] std::io::Error),

    #[error("数据格式错误：{0}")]
    Json(#[from] serde_json::Error),

    #[error("数据目录不可用：{0}")]
    DataDir(String),

    #[error("数据库 {path} 来自更新版本的 Termlet（结构版本 {found}，本版本最高支持 {supported}）")]
    DatabaseTooNew {
        path: PathBuf,
        found: i64,
        supported: i64,
    },

    /// 保险库尚未解锁（需要主密码，或主密钥丢失后还没有重置）。
    #[error("保险库已锁定")]
    VaultLocked,

    /// 主密钥与数据库中的校验值不符：数据目录被拷到了另一台电脑或另一个账户，或钥匙串中的条目被替换。
    #[error("主密钥与数据不符")]
    VaultKeyMismatch,

    #[error("主密码不正确")]
    WrongPassword,

    #[error("系统钥匙串不可用：{0}")]
    Keyring(String),

    /// 解密失败：数据被篡改、或不是用这把主密钥加密的。
    #[error("解密失败")]
    Decrypt,

    #[error("启动本地终端失败：{0}")]
    Pty(String),

    #[error("没有找到{0}")]
    NotFound(&'static str),

    #[error("内容不合法：{0}")]
    Invalid(Invalid),

    #[error("{0}")]
    Key(crate::sshkey::KeyError),
}

impl From<crate::sshkey::KeyError> for CoreError {
    fn from(error: crate::sshkey::KeyError) -> Self {
        match error {
            crate::sshkey::KeyError::Invalid(invalid) => CoreError::Invalid(invalid),
            other => CoreError::Key(other),
        }
    }
}

/// 校验失败：哪一项、为什么。界面据此提示具体的输入框。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{field}：{reason}")]
pub struct Invalid {
    pub field: &'static str,
    pub reason: &'static str,
}

impl Invalid {
    pub fn new(field: &'static str, reason: &'static str) -> Self {
        Self { field, reason }
    }
}

impl From<Invalid> for CoreError {
    fn from(invalid: Invalid) -> Self {
        CoreError::Invalid(invalid)
    }
}
