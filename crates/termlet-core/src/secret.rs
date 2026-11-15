//! 内存中的秘密：`Debug` 只显示 `***`，丢弃时清零。

use zeroize::Zeroizing;

/// 密码、私钥、私钥密码、TOTP 密钥等文本秘密。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// 取出明文。只在真正要用时调用（认证、加密），不要存进别的结构。
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_value() {
        let secret = SecretString::new("hunter2");
        assert_eq!(format!("{secret:?}"), "SecretString(***)");
        assert_eq!(secret.expose(), "hunter2");
    }
}
