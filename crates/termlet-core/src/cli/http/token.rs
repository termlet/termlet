//! MCP 的 HTTP 令牌：两个（[`TokenKind`]），各是 256 位系统随机数。本机的写成 `termlet_mcp_<52 个 base32 字符>`，
//! 局域网的写成 `termlet_mcp_lan_<52 个 base32 字符>`（前缀便于 gitleaks、GitHub 的秘密扫描认出误提交的令牌）。
//!
//! - 按对端地址选令牌：回环地址来的连接只认本机的令牌，局域网来的只认局域网的令牌。本机的令牌因此从不在网络上
//!   明文传输；局域网的令牌可以单独重新生成，撤销局域网中的电脑不影响这台电脑上的 AI 工具。
//! - 校验只用 SHA-256 摘要（本机 `meta` 表的 `cli.http.token.sha256`、`cli.http.lan_token.sha256`），与请求中的
//!   令牌的摘要做常量时间比较；保险库锁定时校验照常。
//! - 令牌本身经保险库加密存一份（`cli.http.token`、`cli.http.lan_token`，附加数据
//!   `termlet:secret:v1:mcp-http-token`、`termlet:secret:v1:mcp-lan-token`），只供界面显示与复制。
//! - 都在本机的 `meta` 表中：不同步；完整备份删除整张 `meta` 表，恢复备份时保留本机的（在另一台电脑上恢复后
//!   重新生成）；不进诊断包、运行日志与 `Debug` 输出（[`SecretString`]）。放在本机设置中的话，会跟着设置的草稿保存，
//!   与“重新生成”这类立即执行的操作混在一起。
//! - 本机的令牌在第一次打开 HTTP 时、局域网的在第一次改为监听局域网时（保险库解锁后）自动生成（[`ensure`]）；
//!   [`regenerate`] 立即生效，用旧令牌的 AI 工具随即被拒绝。

use sha2::{Digest, Sha256};

use crate::secret::SecretString;
use crate::store::Store;
use crate::{CoreError, Result, db};

/// 本机的令牌的前缀。
pub const TOKEN_PREFIX: &str = "termlet_mcp_";
/// 局域网的令牌的前缀（gitleaks 的同一条规则认两种）。
pub const LAN_TOKEN_PREFIX: &str = "termlet_mcp_lan_";

/// 令牌的随机部分的字节数。
const TOKEN_BYTES: usize = 32;

/// 哪一个令牌。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// 这台电脑上的 AI 工具（经回环地址连接）
    Local,
    /// 局域网中的其他电脑
    Lan,
}

impl TokenKind {
    pub fn prefix(self) -> &'static str {
        match self {
            TokenKind::Local => TOKEN_PREFIX,
            TokenKind::Lan => LAN_TOKEN_PREFIX,
        }
    }

    /// `meta` 表中的摘要（十六进制）。
    fn meta_digest(self) -> &'static str {
        match self {
            TokenKind::Local => "cli.http.token.sha256",
            TokenKind::Lan => "cli.http.lan_token.sha256",
        }
    }

    /// `meta` 表中经保险库加密的令牌。
    fn meta_sealed(self) -> &'static str {
        match self {
            TokenKind::Local => "cli.http.token",
            TokenKind::Lan => "cli.http.lan_token",
        }
    }

    /// 加密令牌时的附加数据（两种令牌的密文不能互换）。
    fn sealed_aad(self) -> &'static [u8] {
        match self {
            TokenKind::Local => b"termlet:secret:v1:mcp-http-token",
            TokenKind::Lan => b"termlet:secret:v1:mcp-lan-token",
        }
    }
}

/// 界面上能看到的令牌。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenView {
    /// 还没有生成（HTTP 还没有打开过，或打开时保险库没有解锁）
    Missing,
    /// 保险库锁定（主密码模式）：解锁后可以查看；校验照常
    Locked,
    /// 解不开（保险库重置过、数据目录换了电脑）：校验照常，要看令牌只能重新生成
    Lost,
    Ready(SecretString),
}

/// 生成一个新令牌（不保存）。
fn generate(kind: TokenKind) -> SecretString {
    SecretString::new(format!(
        "{}{}",
        kind.prefix(),
        crate::id::random_token(TOKEN_BYTES)
    ))
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// 保存的摘要；没有（或损坏）时为 `None`。
fn stored_digest(store: &Store, kind: TokenKind) -> Result<Option<[u8; 32]>> {
    let Some(raw) = store.database().meta(kind.meta_digest())? else {
        return Ok(None);
    };
    let decoded = data_encoding::HEXLOWER.decode(&raw).ok();
    Ok(decoded.and_then(|bytes| <[u8; 32]>::try_from(bytes).ok()))
}

/// 请求中的令牌是否是 `kind` 那一个（与保存的摘要做常量时间比较）。没有保存令牌时一律不对。
pub(super) fn verify(store: &Store, kind: TokenKind, presented: &str) -> bool {
    match stored_digest(store, kind) {
        Ok(Some(expected)) => constant_time_eq(&digest(presented), &expected),
        Ok(None) => false,
        Err(e) => {
            tracing::warn!("读取 MCP 的 HTTP 令牌摘要失败：{e}");
            false
        }
    }
}

/// 保存新令牌：摘要与加密的令牌一起写（同一个事务）。保险库锁定时返回 [`CoreError::VaultLocked`]，什么也不写。
fn save(store: &Store, kind: TokenKind, token: &SecretString) -> Result<()> {
    let sealed = store
        .vault()
        .seal(kind.sealed_aad(), token.expose().as_bytes())?;
    let hex = data_encoding::HEXLOWER.encode(&digest(token.expose()));
    store.database().write(|tx| {
        db::set_meta(tx, kind.meta_digest(), hex.as_bytes())?;
        db::set_meta(tx, kind.meta_sealed(), &sealed)?;
        Ok(())
    })
}

/// 还没有 `kind` 这个令牌时生成一个（保险库锁定时等解锁后再生成）。生成了返回真。
pub(super) fn ensure(store: &Store, kind: TokenKind) -> Result<bool> {
    if stored_digest(store, kind)?.is_some() || !store.vault().is_unlocked() {
        return Ok(false);
    }
    save(store, kind, &generate(kind))?;
    Ok(true)
}

/// 重新生成 `kind` 这个令牌，立即生效（用旧令牌的请求随即被拒绝；另一个令牌不变），返回新令牌供界面显示。保险库
/// 锁定时返回 [`CoreError::VaultLocked`]（令牌要加密保存一份才能再看到）。
pub fn regenerate(store: &Store, kind: TokenKind) -> Result<SecretString> {
    let token = generate(kind);
    save(store, kind, &token)?;
    match kind {
        TokenKind::Local => tracing::info!("已重新生成 MCP 的 HTTP 令牌"),
        TokenKind::Lan => tracing::info!("已重新生成 MCP 的局域网令牌"),
    }
    Ok(token)
}

/// 界面显示、复制用的令牌。
pub fn view(store: &Store, kind: TokenKind) -> Result<TokenView> {
    let Some(sealed) = store.database().meta(kind.meta_sealed())? else {
        return Ok(TokenView::Missing);
    };
    if stored_digest(store, kind)?.is_none() {
        return Ok(TokenView::Missing);
    }
    match store.vault().open_sealed(kind.sealed_aad(), &sealed) {
        Ok(plain) => match std::str::from_utf8(&plain) {
            Ok(text) => Ok(TokenView::Ready(SecretString::new(text))),
            Err(_) => Ok(TokenView::Lost),
        },
        Err(CoreError::VaultLocked) => Ok(TokenView::Locked),
        Err(CoreError::Decrypt) => Ok(TokenView::Lost),
        Err(e) => Err(e),
    }
}

/// 逐字节比较，耗时与内容无关。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{locked_store, memory_store};

    const LOCAL: TokenKind = TokenKind::Local;

    // 令牌：前缀加 52 个小写 base32 字符；只存摘要与加密的令牌，校验按摘要
    #[test]
    fn generate_store_and_verify() {
        let store = memory_store();
        assert_eq!(view(&store, LOCAL).unwrap(), TokenView::Missing);
        assert!(!verify(&store, LOCAL, "anything"), "没有令牌时一律不对");
        assert!(ensure(&store, LOCAL).unwrap(), "第一次生成");
        assert!(!ensure(&store, LOCAL).unwrap(), "已有时不再生成");
        let TokenView::Ready(token) = view(&store, LOCAL).unwrap() else {
            panic!("应能看到令牌");
        };
        let token = token.expose().to_string();
        let random = token.strip_prefix(TOKEN_PREFIX).unwrap();
        assert_eq!(random.len(), 52);
        assert!(random.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7')));
        assert!(verify(&store, LOCAL, &token));
        assert!(!verify(&store, LOCAL, &format!("{token}x")));
        assert!(!verify(&store, LOCAL, ""));

        // 数据库中没有令牌的明文
        let digest_hex = store.database().meta(LOCAL.meta_digest()).unwrap().unwrap();
        let sealed = store.database().meta(LOCAL.meta_sealed()).unwrap().unwrap();
        for blob in [&digest_hex, &sealed] {
            assert!(
                !blob.windows(random.len()).any(|w| w == random.as_bytes()),
                "不应以明文保存"
            );
        }
        assert_eq!(digest_hex.len(), 64);
    }

    // 两个令牌各自生成、各自校验：本机的令牌不能用在局域网，反过来也不行；重新生成一个不影响另一个
    #[test]
    fn local_and_lan_tokens_are_separate() {
        let store = memory_store();
        ensure(&store, LOCAL).unwrap();
        assert_eq!(view(&store, TokenKind::Lan).unwrap(), TokenView::Missing);
        assert!(ensure(&store, TokenKind::Lan).unwrap());
        let ready = |kind| match view(&store, kind).unwrap() {
            TokenView::Ready(token) => token.expose().to_string(),
            other => panic!("应能看到令牌：{other:?}"),
        };
        let (local, lan) = (ready(LOCAL), ready(TokenKind::Lan));
        let random = lan.strip_prefix(LAN_TOKEN_PREFIX).unwrap();
        assert_eq!(random.len(), 52);
        assert!(random.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7')));
        assert!(local.starts_with(TOKEN_PREFIX) && !local.starts_with(LAN_TOKEN_PREFIX));
        assert!(verify(&store, LOCAL, &local) && !verify(&store, LOCAL, &lan));
        assert!(verify(&store, TokenKind::Lan, &lan) && !verify(&store, TokenKind::Lan, &local));

        let new_lan = regenerate(&store, TokenKind::Lan).unwrap();
        assert!(!verify(&store, TokenKind::Lan, &lan));
        assert!(verify(&store, TokenKind::Lan, new_lan.expose()));
        assert_eq!(ready(LOCAL), local, "本机的令牌不变");
        assert!(verify(&store, LOCAL, &local));

        // 两种的密文不能互换（附加数据不同）
        let sealed_lan = store
            .database()
            .meta(TokenKind::Lan.meta_sealed())
            .unwrap()
            .unwrap();
        store
            .database()
            .set_meta(LOCAL.meta_sealed(), &sealed_lan)
            .unwrap();
        assert_eq!(view(&store, LOCAL).unwrap(), TokenView::Lost);
    }

    // 重新生成立即生效：旧令牌随即被拒绝
    #[test]
    fn regenerate_replaces_the_token() {
        let store = memory_store();
        ensure(&store, LOCAL).unwrap();
        let TokenView::Ready(old) = view(&store, LOCAL).unwrap() else {
            panic!("应能看到令牌");
        };
        let new = regenerate(&store, LOCAL).unwrap();
        assert_ne!(old, new);
        assert!(!verify(&store, LOCAL, old.expose()));
        assert!(verify(&store, LOCAL, new.expose()));
        assert_eq!(view(&store, LOCAL).unwrap(), TokenView::Ready(new.clone()));
        assert!(
            !format!("{new:?}").contains(TOKEN_PREFIX),
            "Debug 不显示令牌"
        );
    }

    // 保险库没有解锁时校验照常，界面看不到令牌；不能重新生成，也不自动生成
    #[test]
    fn locked_vault_still_verifies() {
        let store = locked_store();
        assert!(!ensure(&store, LOCAL).unwrap(), "锁定时不生成");
        assert!(!ensure(&store, TokenKind::Lan).unwrap(), "锁定时不生成");
        assert_eq!(view(&store, LOCAL).unwrap(), TokenView::Missing);
        assert!(matches!(
            regenerate(&store, LOCAL),
            Err(CoreError::VaultLocked)
        ));
        // 解锁时生成过的令牌：摘要照常校验，加密的令牌要解锁后才能看
        let token = generate(LOCAL);
        let hex = data_encoding::HEXLOWER.encode(&digest(token.expose()));
        store
            .database()
            .set_meta(LOCAL.meta_digest(), hex.as_bytes())
            .unwrap();
        store
            .database()
            .set_meta(LOCAL.meta_sealed(), b"sealed")
            .unwrap();
        assert!(verify(&store, LOCAL, token.expose()));
        assert_eq!(view(&store, LOCAL).unwrap(), TokenView::Locked);
        assert!(!ensure(&store, LOCAL).unwrap());
    }

    // 加密的令牌解不开（保险库重置过）：校验照常，界面提示重新生成
    #[test]
    fn unreadable_token_is_lost() {
        let store = memory_store();
        let token = regenerate(&store, LOCAL).unwrap();
        store
            .database()
            .set_meta(LOCAL.meta_sealed(), b"garbage")
            .unwrap();
        assert_eq!(view(&store, LOCAL).unwrap(), TokenView::Lost);
        assert!(verify(&store, LOCAL, token.expose()));
    }
}
