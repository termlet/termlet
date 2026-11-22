//! FinalShell 保存的密码（连接文件的 `password`）。
//!
//! FinalShell 用 DES 可逆地加密保存的密码，密钥由密文开头的 8 字节按固定算法派生，不需要用户输入，也不绑定电脑或
//! Windows 账户：拿到文件的人都能还原，实质上是混淆。所以只在用户勾选“导入保存的密码”后才导入
//! （[`crate::import::Source::secrets_need_consent`]），解开后存进 Termlet 的保险库。
//!
//! 格式：Base64（头 8 字节 ‖ DES-ECB 密文，PKCS#5 填充）。头的各字节按有符号数（Java 的 `byte`），密钥：
//!
//! 1. `a` = `new java.util.Random(头[5]).nextInt(127)`；
//! 2. `r` = `new Random(3680984568597093857 / a)`，先丢掉 `头[0]` 个 `nextLong()`（为负时不丢）；
//! 3. `r2` = `new Random(r.nextLong())`；
//! 4. 依次写出 8 个 64 位大端整数：头[4]、`r2.nextLong()`、头[7]、头[3]、`r2.nextLong()`、头[1]、`r.nextLong()`、头[2]；
//! 5. 这 64 字节的 MD5 取前 8 字节作 DES 密钥。
//!
//! 依据两份公开的开源实现（2026-11-22 查阅，两者一致）：DengShell（MIT）的 `internal/app/finalshell_password.go`；
//! sncce/finalshell-decoder-use-python 的 `finalshell解密.py`（逐行注释了 FinalShell 的 Java 原文，没有声明许可，只看不抄）。
//! 测试中的四组密文取自 DengShell 的测试（MIT）。

use des::Des;
use des::cipher::{Block, BlockCipherDecrypt, KeyInit};
use md5::{Digest, Md5};
use zeroize::Zeroizing;

use crate::secret::SecretString;

/// Base64 文本的长度上限：密码不会这么长，防止异常的文件
const MAX_ENCODED: usize = 64 * 1024;

/// 保存的密码格式不对，解不开。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Invalid;

/// 解开 FinalShell 保存的密码：没有保存（空）时为 `Ok(None)`。
pub(super) fn decode(encoded: &str) -> Result<Option<SecretString>, Invalid> {
    // 旧的 Java 编码器每 76 个字符换一行
    let compact: String = encoded
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    if compact.is_empty() {
        return Ok(None);
    }
    if compact.len() > MAX_ENCODED {
        return Err(Invalid);
    }
    let data = Zeroizing::new(
        data_encoding::BASE64
            .decode(compact.as_bytes())
            .map_err(|_| Invalid)?,
    );
    if data.len() < 16 || data.len() % 8 != 0 {
        return Err(Invalid);
    }
    let (head, body) = data.split_at(8);
    let key = des_key(head.try_into().map_err(|_| Invalid)?).ok_or(Invalid)?;
    let cipher = Des::new_from_slice(&key[..]).map_err(|_| Invalid)?;
    let mut plain = Zeroizing::new(body.to_vec());
    let mut block = Block::<Des>::default();
    for chunk in plain.as_chunks_mut::<8>().0 {
        block.copy_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
    block.fill(0);
    // PKCS#5 填充：最后 n 个字节都是 n（1–8）
    let pad = usize::from(*plain.last().ok_or(Invalid)?);
    if !(1..=8).contains(&pad)
        || plain[plain.len() - pad..]
            .iter()
            .any(|&b| usize::from(b) != pad)
    {
        return Err(Invalid);
    }
    let text = std::str::from_utf8(&plain[..plain.len() - pad]).map_err(|_| Invalid)?;
    Ok(Some(SecretString::new(text)))
}

/// 由头 8 字节派生 DES 密钥；`nextInt(127)` 为 0 时 FinalShell 自己也会出错（除以零），不是它写出的密文。
fn des_key(head: &[u8; 8]) -> Option<Zeroizing<[u8; 8]>> {
    let byte = |i: usize| i64::from(head[i] as i8);
    let divisor = i64::from(JavaRandom::new(byte(5)).next_int(127));
    if divisor == 0 {
        return None;
    }
    let mut random = JavaRandom::new(3_680_984_568_597_093_857 / divisor);
    for _ in 0..byte(0).max(0) {
        random.next_long();
    }
    let mut r2 = JavaRandom::new(random.next_long());
    // 与 Java 的数组初始化相同，从左到右求值
    let values = [
        byte(4),
        r2.next_long(),
        byte(7),
        byte(3),
        r2.next_long(),
        byte(1),
        random.next_long(),
        byte(2),
    ];
    let mut material = Zeroizing::new([0u8; 64]);
    for (chunk, value) in material.as_chunks_mut::<8>().0.iter_mut().zip(values) {
        *chunk = value.to_be_bytes();
    }
    let digest = Zeroizing::new(<[u8; 16]>::from(Md5::digest(&material[..])));
    let mut key = Zeroizing::new([0u8; 8]);
    key.copy_from_slice(&digest[..8]);
    Some(key)
}

/// `java.util.Random`：48 位线性同余发生器。
struct JavaRandom(u64);

impl JavaRandom {
    const MULTIPLIER: u64 = 0x5_DEEC_E66D;
    const MASK: u64 = (1 << 48) - 1;

    fn new(seed: i64) -> Self {
        Self((seed as u64 ^ Self::MULTIPLIER) & Self::MASK)
    }

    /// `next(bits)`：高 `bits` 位，按 Java 的 `int` 截断为有符号数。
    fn next(&mut self, bits: u32) -> i32 {
        self.0 = self.0.wrapping_mul(Self::MULTIPLIER).wrapping_add(0xB) & Self::MASK;
        (self.0 >> (48 - bits)) as i32
    }

    fn next_long(&mut self) -> i64 {
        let high = i64::from(self.next(32));
        let low = i64::from(self.next(32));
        (high << 32).wrapping_add(low)
    }

    /// `nextInt(bound)`，`bound` 不是 2 的幂时的算法（FinalShell 只用 127）。
    fn next_int(&mut self, bound: i32) -> i32 {
        loop {
            let bits = self.next(31);
            let value = bits % bound;
            if bits.wrapping_sub(value).wrapping_add(bound - 1) >= 0 {
                return value;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use des::cipher::BlockCipherEncrypt;

    use super::*;

    /// 按 FinalShell 的方式加密（测试中构造密文）。
    fn encode(plain: &str, head: [u8; 8]) -> String {
        let key = des_key(&head).unwrap();
        let cipher = Des::new_from_slice(&key[..]).unwrap();
        let mut data = plain.as_bytes().to_vec();
        let pad = 8 - data.len() % 8;
        data.extend(std::iter::repeat_n(pad as u8, pad));
        let mut block = Block::<Des>::default();
        for chunk in data.as_chunks_mut::<8>().0 {
            block.copy_from_slice(chunk);
            cipher.encrypt_block(&mut block);
            chunk.copy_from_slice(&block);
        }
        let mut out = head.to_vec();
        out.extend(data);
        data_encoding::BASE64.encode(&out)
    }

    fn plain(encoded: &str) -> Option<String> {
        decode(encoded).unwrap().map(|p| p.expose().to_string())
    }

    #[test]
    fn java_random() {
        // java.util.Random 的公认值
        assert_eq!(JavaRandom::new(0).next(32), -1_155_484_576);
        assert_eq!(JavaRandom::new(42).next(32), -1_170_105_035);
        assert_eq!(JavaRandom::new(0).next_long(), -4_962_768_465_676_381_896);
        assert_eq!(JavaRandom::new(42).next_int(10), 0);
    }

    #[test]
    fn reference_vectors() {
        // 取自 DengShell（MIT）internal/app/finalshell_import_test.go 的 TestFinalShellPasswordReferenceVectors
        for (encoded, expected) in [
            (
                "AQIDBAUGBwhSo9sQPy5eFkuGZiawt4poDYA6D2O3v3Y=",
                "DengShell-QA-123!",
            ),
            (
                "//6AgQjIBgvm59KvCX6kcV9BMrqUIZugyUGPDWTploFMGL5nsWmTlw==",
                "简体繁體 Paßwörd 🔑",
            ),
            ("fwECAwQFBgdn/leI7eV6W2BjAAFLHxlS", "eight888"),
            ("gH9+fXx7ennnkpiMwHHB+A==", ""),
        ] {
            assert_eq!(plain(encoded).as_deref(), Some(expected), "{encoded}");
        }
        assert_eq!(plain(""), None, "没有保存密码");
        assert_eq!(plain("  "), None);
    }

    #[test]
    fn round_trip() {
        // 头[0] 为负（不丢）与较大（丢 127 个）、头[5] 为负
        for head in [
            [0x81, 1, 2, 3, 4, 5, 6, 7],
            [0x7F, 0xFE, 0x80, 0x01, 0x10, 0x90, 0x33, 0xEE],
            [0, 0, 0, 0, 0, 0, 0, 0],
        ] {
            if des_key(&head).is_none() {
                continue;
            }
            for text in ["", "p", "12345678", "中文密码 with spaces"] {
                assert_eq!(
                    plain(&encode(text, head)).as_deref(),
                    Some(text),
                    "{head:?}"
                );
            }
        }
    }

    #[test]
    fn line_breaks_in_base64() {
        let encoded = encode("a fairly long password to make the text wrap", [5; 8]);
        let wrapped = format!("{}\r\n{}", &encoded[..20], &encoded[20..]);
        assert_eq!(
            plain(&wrapped).as_deref(),
            Some("a fairly long password to make the text wrap")
        );
    }

    #[test]
    fn invalid() {
        let head = [1, 2, 3, 4, 5, 6, 7, 8];
        let key = des_key(&head).unwrap();
        let cipher = Des::new_from_slice(&key[..]).unwrap();
        let raw = |block: [u8; 8]| {
            let mut b = Block::<Des>::from(block);
            cipher.encrypt_block(&mut b);
            let mut out = head.to_vec();
            out.extend_from_slice(&b);
            data_encoding::BASE64.encode(&out)
        };
        for encoded in [
            "not base64!".to_string(),
            "AAAA".to_string(),
            data_encoding::BASE64.encode(&[0; 17]),
            "A".repeat(MAX_ENCODED + 4),
            // 填充为 0、填充不一致、不是 UTF-8
            raw([1, 2, 3, 4, 5, 6, 7, 0]),
            raw([1, 2, 3, 4, 5, 6, 7, 2]),
            raw([0xFF, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 1]),
        ] {
            assert_eq!(decode(&encoded).err(), Some(Invalid), "{encoded:.40}");
        }
    }

    #[test]
    fn zero_divisor_has_no_key() {
        // 头[5] 使 nextInt(127) 为 0 时没有密钥（FinalShell 不会写出这样的密文）
        for b in i8::MIN..=i8::MAX {
            let mut head = [0u8; 8];
            head[5] = b as u8;
            let zero = JavaRandom::new(i64::from(b)).next_int(127) == 0;
            assert_eq!(des_key(&head).is_none(), zero);
        }
    }
}
