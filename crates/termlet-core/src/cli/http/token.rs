//! MCP 的 HTTP 令牌：一个，256 位系统随机数，写成 `termlet_mcp_<52 个 base32 字符>`（前缀便于 gitleaks、
//! GitHub 的秘密扫描认出误提交的令牌）。
//!
//! - 校验只用 SHA-256 摘要（本机 `meta` 表的 `cli.http.token.sha256`），与请求中的令牌的摘要做常量时间比较；
//!   保险库锁定时校验照常。
//! - 令牌本身经保险库加密存一份（`cli.http.token`，附加数据 `termlet:secret:v1:mcp-http-token`），只供界面显示与复制。
//! - 两项都在本机的 `meta` 表中：不同步；完整备份删除整张 `meta` 表，恢复备份时保留本机的（在另一台电脑上恢复后
//!   重新生成）；不进诊断包、运行日志与 `Debug` 输出（[`SecretString`]）。放在本机设置中的话，设置对话框保存时整组
//!   写回的旧值会把重新生成的令牌换回去。
//! - 第一次打开 HTTP 时（保险库解锁后）自动生成（[`ensure`]）；[`regenerate`] 立即生效，用旧令牌的 AI 工具随即被拒绝。

use sha2::{Digest, Sha256};

use crate::secret::SecretString;
use crate::store::Store;
use crate::{CoreError, Result, db};

/// 令牌的前缀。
pub const TOKEN_PREFIX: &str = "termlet_mcp_";

/// 令牌的随机部分的字节数。
const TOKEN_BYTES: usize = 32;
/// 摘要（十六进制）。
const META_DIGEST: &str = "cli.http.token.sha256";
/// 经保险库加密的令牌。
const META_SEALED: &str = "cli.http.token";
/// 加密令牌时的附加数据。
const SEALED_AAD: &[u8] = b"termlet:secret:v1:mcp-http-token";

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
fn generate() -> SecretString {
    SecretString::new(format!(
        "{TOKEN_PREFIX}{}",
        crate::id::random_token(TOKEN_BYTES)
    ))
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// 保存的摘要；没有（或损坏）时为 `None`。
fn stored_digest(store: &Store) -> Result<Option<[u8; 32]>> {
    let Some(raw) = store.database().meta(META_DIGEST)? else {
        return Ok(None);
    };
    let decoded = data_encoding::HEXLOWER.decode(&raw).ok();
    Ok(decoded.and_then(|bytes| <[u8; 32]>::try_from(bytes).ok()))
}

/// 请求中的令牌是否正确（与保存的摘要做常量时间比较）。没有保存令牌时一律不对。
pub(super) fn verify(store: &Store, presented: &str) -> bool {
    match stored_digest(store) {
        Ok(Some(expected)) => constant_time_eq(&digest(presented), &expected),
        Ok(None) => false,
        Err(e) => {
            tracing::warn!("读取 MCP 的 HTTP 令牌摘要失败：{e}");
            false
        }
    }
}

/// 保存新令牌：摘要与加密的令牌一起写（同一个事务）。保险库锁定时返回 [`CoreError::VaultLocked`]，什么也不写。
fn save(store: &Store, token: &SecretString) -> Result<()> {
    let sealed = store.vault().seal(SEALED_AAD, token.expose().as_bytes())?;
    let hex = data_encoding::HEXLOWER.encode(&digest(token.expose()));
    store.database().write(|tx| {
        db::set_meta(tx, META_DIGEST, hex.as_bytes())?;
        db::set_meta(tx, META_SEALED, &sealed)?;
        Ok(())
    })
}

/// 还没有令牌时生成一个（保险库锁定时等解锁后再生成）。生成了返回真。
pub(super) fn ensure(store: &Store) -> Result<bool> {
    if stored_digest(store)?.is_some() || !store.vault().is_unlocked() {
        return Ok(false);
    }
    save(store, &generate())?;
    Ok(true)
}

/// 重新生成令牌，立即生效（用旧令牌的请求随即被拒绝），返回新令牌供界面显示。保险库锁定时返回
/// [`CoreError::VaultLocked`]（令牌要加密保存一份才能再看到）。
pub fn regenerate(store: &Store) -> Result<SecretString> {
    let token = generate();
    save(store, &token)?;
    tracing::info!("已重新生成 MCP 的 HTTP 令牌");
    Ok(token)
}

/// 界面显示、复制用的令牌。
pub fn view(store: &Store) -> Result<TokenView> {
    let Some(sealed) = store.database().meta(META_SEALED)? else {
        return Ok(TokenView::Missing);
    };
    if stored_digest(store)?.is_none() {
        return Ok(TokenView::Missing);
    }
    match store.vault().open_sealed(SEALED_AAD, &sealed) {
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

    // 令牌：前缀加 52 个小写 base32 字符；只存摘要与加密的令牌，校验按摘要
    #[test]
    fn generate_store_and_verify() {
        let store = memory_store();
        assert_eq!(view(&store).unwrap(), TokenView::Missing);
        assert!(!verify(&store, "anything"), "没有令牌时一律不对");
        assert!(ensure(&store).unwrap(), "第一次生成");
        assert!(!ensure(&store).unwrap(), "已有时不再生成");
        let TokenView::Ready(token) = view(&store).unwrap() else {
            panic!("应能看到令牌");
        };
        let token = token.expose().to_string();
        let random = token.strip_prefix(TOKEN_PREFIX).unwrap();
        assert_eq!(random.len(), 52);
        assert!(random.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7')));
        assert!(verify(&store, &token));
        assert!(!verify(&store, &format!("{token}x")));
        assert!(!verify(&store, ""));

        // 数据库中没有令牌的明文
        let digest_hex = store.database().meta(META_DIGEST).unwrap().unwrap();
        let sealed = store.database().meta(META_SEALED).unwrap().unwrap();
        for blob in [&digest_hex, &sealed] {
            assert!(
                !blob.windows(random.len()).any(|w| w == random.as_bytes()),
                "不应以明文保存"
            );
        }
        assert_eq!(digest_hex.len(), 64);
    }

    // 重新生成立即生效：旧令牌随即被拒绝
    #[test]
    fn regenerate_replaces_the_token() {
        let store = memory_store();
        ensure(&store).unwrap();
        let TokenView::Ready(old) = view(&store).unwrap() else {
            panic!("应能看到令牌");
        };
        let new = regenerate(&store).unwrap();
        assert_ne!(old, new);
        assert!(!verify(&store, old.expose()));
        assert!(verify(&store, new.expose()));
        assert_eq!(view(&store).unwrap(), TokenView::Ready(new.clone()));
        assert!(
            !format!("{new:?}").contains(TOKEN_PREFIX),
            "Debug 不显示令牌"
        );
    }

    // 保险库没有解锁时校验照常，界面看不到令牌；不能重新生成，也不自动生成
    #[test]
    fn locked_vault_still_verifies() {
        let store = locked_store();
        assert!(!ensure(&store).unwrap(), "锁定时不生成");
        assert_eq!(view(&store).unwrap(), TokenView::Missing);
        assert!(matches!(regenerate(&store), Err(CoreError::VaultLocked)));
        // 解锁时生成过的令牌：摘要照常校验，加密的令牌要解锁后才能看
        let token = generate();
        let hex = data_encoding::HEXLOWER.encode(&digest(token.expose()));
        store
            .database()
            .set_meta(META_DIGEST, hex.as_bytes())
            .unwrap();
        store.database().set_meta(META_SEALED, b"sealed").unwrap();
        assert!(verify(&store, token.expose()));
        assert_eq!(view(&store).unwrap(), TokenView::Locked);
        assert!(!ensure(&store).unwrap());
    }

    // 加密的令牌解不开（保险库重置过）：校验照常，界面提示重新生成
    #[test]
    fn unreadable_token_is_lost() {
        let store = memory_store();
        let token = regenerate(&store).unwrap();
        store.database().set_meta(META_SEALED, b"garbage").unwrap();
        assert_eq!(view(&store).unwrap(), TokenView::Lost);
        assert!(verify(&store, token.expose()));
    }
}
