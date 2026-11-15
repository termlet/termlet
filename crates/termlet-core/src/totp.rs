//! 基于时间的一次性验证码（TOTP，RFC 6238）：从验证器 App 的密钥（Base32 或 `otpauth://` 链接）算出当前的验证码。

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};

use crate::Invalid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
}

/// 解析好的 TOTP 配置。`Debug` 不输出密钥。
#[derive(Clone, PartialEq, Eq)]
pub struct Totp {
    secret: Vec<u8>,
    /// 6（默认）或 8
    pub digits: u32,
    /// 秒，默认 30
    pub period: u64,
    pub algorithm: Algorithm,
}

impl std::fmt::Debug for Totp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Totp")
            .field("digits", &self.digits)
            .field("period", &self.period)
            .field("algorithm", &self.algorithm)
            .finish_non_exhaustive()
    }
}

impl Totp {
    /// 解析手动输入的 Base32 密钥（可含空格、连字符、小写、补位符）或二维码中的 `otpauth://totp/` 链接。
    pub fn parse(input: &str) -> Result<Self, Invalid> {
        let input = input.trim();
        let mut digits = 6;
        let mut period = 30;
        let mut algorithm = Algorithm::Sha1;
        let mut secret = input.to_string();
        if input.to_ascii_lowercase().starts_with("otpauth://") {
            let url = url::Url::parse(input).map_err(|_| Invalid::new("totp", "bad_link"))?;
            if !url
                .host_str()
                .is_some_and(|h| h.eq_ignore_ascii_case("totp"))
            {
                return Err(Invalid::new("totp", "not_time_based"));
            }
            secret.clear();
            for (key, value) in url.query_pairs() {
                match key.as_ref() {
                    "secret" => secret = value.into_owned(),
                    "digits" => {
                        digits = value
                            .parse()
                            .map_err(|_| Invalid::new("totp", "bad_digits"))?
                    }
                    "period" => {
                        period = value
                            .parse()
                            .map_err(|_| Invalid::new("totp", "bad_period"))?
                    }
                    "algorithm" => {
                        algorithm = match value.to_ascii_uppercase().as_str() {
                            "SHA1" => Algorithm::Sha1,
                            "SHA256" => Algorithm::Sha256,
                            "SHA512" => Algorithm::Sha512,
                            _ => return Err(Invalid::new("totp", "bad_algorithm")),
                        }
                    }
                    _ => {}
                }
            }
        }
        if digits != 6 && digits != 8 {
            return Err(Invalid::new("totp", "bad_digits"));
        }
        if !(1..=300).contains(&period) {
            return Err(Invalid::new("totp", "bad_period"));
        }
        let normalized: String = secret
            .chars()
            .filter(|c| !matches!(c, ' ' | '-' | '='))
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if normalized.is_empty() {
            return Err(Invalid::new("totp", "missing_secret"));
        }
        let key = data_encoding::BASE32_NOPAD
            .decode(normalized.as_bytes())
            .map_err(|_| Invalid::new("totp", "bad_base32"))?;
        if key.len() < 10 {
            return Err(Invalid::new("totp", "too_short"));
        }
        Ok(Self {
            secret: key,
            digits,
            period,
            algorithm,
        })
    }

    /// 某个 Unix 时间（秒）的验证码。
    pub fn code_at(&self, unix: u64) -> String {
        let counter = (unix / self.period).to_be_bytes();
        let digest = match self.algorithm {
            Algorithm::Sha1 => hmac_digest::<Hmac<sha1::Sha1>>(&self.secret, &counter),
            Algorithm::Sha256 => hmac_digest::<Hmac<sha2::Sha256>>(&self.secret, &counter),
            Algorithm::Sha512 => hmac_digest::<Hmac<sha2::Sha512>>(&self.secret, &counter),
        };
        let offset = (digest[digest.len() - 1] & 0x0f) as usize;
        let value = u32::from_be_bytes([
            digest[offset],
            digest[offset + 1],
            digest[offset + 2],
            digest[offset + 3],
        ]) & 0x7fff_ffff;
        let modulus = 10u32.pow(self.digits);
        format!("{:0width$}", value % modulus, width = self.digits as usize)
    }

    /// 现在的验证码。
    pub fn code_now(&self) -> String {
        self.code_at(unix_now())
    }

    /// 当前验证码还剩几秒失效。
    pub fn remaining_at(&self, unix: u64) -> u64 {
        self.period - unix % self.period
    }

    /// 现在的验证码还剩几秒失效。
    pub fn remaining_now(&self) -> u64 {
        self.remaining_at(unix_now())
    }
}

fn hmac_digest<M: Mac + KeyInit>(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = <M as KeyInit>::new_from_slice(key).expect("HMAC 接受任意长度的密钥");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(secret: &[u8], algorithm: Algorithm) -> Totp {
        Totp {
            secret: secret.to_vec(),
            digits: 8,
            period: 30,
            algorithm,
        }
    }

    #[test]
    fn rfc6238_vectors() {
        let sha1 = config(b"12345678901234567890", Algorithm::Sha1);
        let sha256 = config(b"12345678901234567890123456789012", Algorithm::Sha256);
        let sha512 = config(
            b"1234567890123456789012345678901234567890123456789012345678901234",
            Algorithm::Sha512,
        );
        let cases = [
            (59, "94287082", "46119246", "90693936"),
            (1111111109, "07081804", "68084774", "25091201"),
            (1111111111, "14050471", "67062674", "99943326"),
            (1234567890, "89005924", "91819424", "93441116"),
            (2000000000, "69279037", "90698825", "38618901"),
            (20000000000, "65353130", "77737706", "47863826"),
        ];
        for (unix, a, b, c) in cases {
            assert_eq!(sha1.code_at(unix), a, "SHA1 @ {unix}");
            assert_eq!(sha256.code_at(unix), b, "SHA256 @ {unix}");
            assert_eq!(sha512.code_at(unix), c, "SHA512 @ {unix}");
        }
    }

    #[test]
    fn parse_manual_secret() {
        let totp = Totp::parse(" gezd gnbv gy3t qojq gezd gnbv gy3t qojq== ").unwrap();
        assert_eq!(
            (totp.digits, totp.period, totp.algorithm),
            (6, 30, Algorithm::Sha1)
        );
        assert_eq!(totp.code_at(59), "287082");
        assert_eq!(totp.remaining_at(59), 1);
        assert_eq!(totp.remaining_at(60), 30);
        assert!(!format!("{totp:?}").contains("12345"));
    }

    #[test]
    fn parse_otpauth_link() {
        let totp = Totp::parse(
            "otpauth://totp/Corp:alice?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=Corp&digits=8&algorithm=SHA1",
        )
        .unwrap();
        assert_eq!(totp.digits, 8);
        assert_eq!(totp.code_at(59), "94287082");
        assert!(Totp::parse("otpauth://hotp/x?secret=GEZDGNBVGY3TQOJQ").is_err()); // gitleaks:allow RFC 6238 的公开测试向量
    }

    #[test]
    fn bad_input_is_rejected() {
        for bad in [
            "",
            "!!!!",
            "GEZDG",
            "otpauth://totp/x?secret=GEZDGNBVGY3TQOJQGEZD&digits=7",
        ] {
            assert!(Totp::parse(bad).is_err(), "{bad}");
        }
    }
}
