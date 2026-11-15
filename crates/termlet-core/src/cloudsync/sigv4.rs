//! AWS Signature Version 4（请求头方式），S3 兼容存储用。
//!
//! 规范请求：方法、规范 URI（路径按段解码后再按 AWS 的规则编码一次：只保留 `A-Za-z0-9-._~` 与 `/`，S3 不二次编码）、
//! 规范查询串（键值编码后排序）、规范头（名称小写、值去掉首尾空白并合并连续空白，按名称排序；总是含 `host`）、
//! 签名的头名称、内容的 SHA-256。待签字符串为 `AWS4-HMAC-SHA256`、时间、范围（`日期/区域/服务/aws4_request`）、
//! 规范请求的 SHA-256；签名密钥由秘密访问密钥按日期、区域、服务逐级 HMAC-SHA256 派生。
//!
//! 秘密访问密钥只在派生签名密钥时用到，不出现在请求中，也不写进日志。

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use url::Url;

use crate::secret::SecretString;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// 空内容的 SHA-256（GET、HEAD、DELETE 用）。
pub(crate) const EMPTY_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// 签名用的凭据与范围。
pub(crate) struct Signer {
    pub access_key: String,
    pub secret: SecretString,
    pub region: String,
    /// 服务名：S3 为 `s3`
    pub service: String,
}

/// 要签名的请求。
pub(crate) struct Request<'a> {
    pub method: &'a str,
    pub url: &'a Url,
    /// 除 `host` 以外要签名的头（名称不分大小写）；`x-amz-date` 与 `x-amz-content-sha256` 由调用方给出
    pub headers: &'a [(&'a str, &'a str)],
    /// 请求内容的 SHA-256（十六进制小写）
    pub payload_sha256: &'a str,
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(data))
}

fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC 接受任意长度的密钥");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// 按 AWS 的规则编码：`A-Za-z0-9-._~` 原样，其余字节写成 `%XX`（大写）；`keep_slash` 时 `/` 也原样。
fn aws_encode(raw: &[u8], keep_slash: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    for &b in raw {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 解开 `%XX`（地址中已编码的路径、查询串）；不是合法的 `%XX` 时原样保留。
fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `Host` 头的值：主机名，不是协议的默认端口时带上端口。
pub(crate) fn host_header(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

/// 规范 URI：路径按段解码后再编码。
fn canonical_uri(url: &Url) -> String {
    let path = url.path();
    if path.is_empty() {
        return "/".into();
    }
    path.split('/')
        .map(|segment| aws_encode(&percent_decode(segment), false))
        .collect::<Vec<_>>()
        .join("/")
}

/// 规范查询串：各项解码后再编码，按键、值排序。
fn canonical_query(url: &Url) -> String {
    let Some(query) = url.query().filter(|q| !q.is_empty()) else {
        return String::new();
    };
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            // 查询串中的 + 表示空格（application/x-www-form-urlencoded）
            let k = percent_decode(&k.replace('+', " "));
            let v = percent_decode(&v.replace('+', " "));
            (aws_encode(&k, false), aws_encode(&v, false))
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// 头的值：去掉首尾空白，连续的空白合为一个空格。
fn trim_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 规范请求与签名的头名称（`;` 分隔）。
pub(crate) fn canonical_request(request: &Request<'_>) -> (String, String) {
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("host"))
        .map(|(name, value)| (name.to_ascii_lowercase(), trim_value(value)))
        .collect();
    headers.push(("host".into(), host_header(request.url)));
    headers.sort();
    let signed = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let mut canonical = String::new();
    canonical.push_str(request.method);
    canonical.push('\n');
    canonical.push_str(&canonical_uri(request.url));
    canonical.push('\n');
    canonical.push_str(&canonical_query(request.url));
    canonical.push('\n');
    for (name, value) in &headers {
        canonical.push_str(name);
        canonical.push(':');
        canonical.push_str(value);
        canonical.push('\n');
    }
    canonical.push('\n');
    canonical.push_str(&signed);
    canonical.push('\n');
    canonical.push_str(request.payload_sha256);
    (canonical, signed)
}

impl Signer {
    /// `Authorization` 头的值。`amz_date` 是 `x-amz-date` 头的值（`20130524T000000Z`，UTC），须同时在签名的头中。
    pub fn authorization(&self, request: &Request<'_>, amz_date: &str) -> String {
        let date = &amz_date[..amz_date.len().min(8)];
        let scope = format!("{date}/{}/{}/aws4_request", self.region, self.service);
        let (canonical, signed) = canonical_request(request);
        let string_to_sign = format!(
            "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical.as_bytes())
        );
        let mut key = format!("AWS4{}", self.secret.expose()).into_bytes();
        for part in [
            date,
            self.region.as_str(),
            self.service.as_str(),
            "aws4_request",
        ] {
            let next = hmac(&key, part.as_bytes());
            zeroize::Zeroize::zeroize(&mut key);
            key = next;
        }
        let signature = data_encoding::HEXLOWER.encode(&hmac(&key, string_to_sign.as_bytes()));
        zeroize::Zeroize::zeroize(&mut key);
        format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            self.access_key
        )
    }
}

/// 现在的 `x-amz-date`（UTC）。
pub(crate) fn amz_date_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS 文档（S3 API 参考“Signature Calculations for the Authorization Header”）中的示例凭据。
    fn s3_example() -> Signer {
        Signer {
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret: SecretString::new("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            region: "us-east-1".into(),
            service: "s3".into(),
        }
    }

    fn signature(auth: &str) -> &str {
        auth.rsplit("Signature=").next().unwrap()
    }

    const DATE: &str = "20130524T000000Z";

    // GET Object：带 Range 头
    #[test]
    fn s3_get_object_example() {
        let url = Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").unwrap();
        let headers = [
            ("Range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", DATE),
        ];
        let request = Request {
            method: "GET",
            url: &url,
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
        };
        let auth = s3_example().authorization(&request, DATE);
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    // PUT Object：路径中的 $ 要编码，签名的头中有 Date 与 x-amz-storage-class
    #[test]
    fn s3_put_object_example() {
        let url = Url::parse("https://examplebucket.s3.amazonaws.com/test$file.text").unwrap();
        let body = b"Welcome to Amazon S3.";
        let hash = sha256_hex(body);
        let headers = [
            ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
            ("x-amz-date", DATE),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ("x-amz-content-sha256", hash.as_str()),
        ];
        let request = Request {
            method: "PUT",
            url: &url,
            headers: &headers,
            payload_sha256: &hash,
        };
        let (canonical, signed) = canonical_request(&request);
        assert!(
            canonical.starts_with("PUT\n/test%24file.text\n\n"),
            "{canonical}"
        );
        assert_eq!(
            signed,
            "date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class"
        );
        assert_eq!(
            signature(&s3_example().authorization(&request, DATE)),
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        );
    }

    // 列出对象：查询串排序（prefix=J&max-keys=2 → max-keys=2&prefix=J）
    #[test]
    fn s3_list_objects_example() {
        let url =
            Url::parse("https://examplebucket.s3.amazonaws.com/?prefix=J&max-keys=2").unwrap();
        let headers = [("x-amz-content-sha256", EMPTY_SHA256), ("x-amz-date", DATE)];
        let request = Request {
            method: "GET",
            url: &url,
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
        };
        let (canonical, _) = canonical_request(&request);
        assert!(
            canonical.starts_with("GET\n/\nmax-keys=2&prefix=J\n"),
            "{canonical}"
        );
        assert_eq!(
            signature(&s3_example().authorization(&request, DATE)),
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
    }

    // AWS SigV4 测试集的 get-vanilla（服务名为 service）
    #[test]
    fn sigv4_suite_get_vanilla() {
        let signer = Signer {
            access_key: "AKIDEXAMPLE".into(),
            secret: SecretString::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            region: "us-east-1".into(),
            service: "service".into(),
        };
        let url = Url::parse("https://example.amazonaws.com/").unwrap();
        let date = "20150830T123600Z";
        let headers = [("X-Amz-Date", date)];
        let request = Request {
            method: "GET",
            url: &url,
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
        };
        assert_eq!(
            signer.authorization(&request, date),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn canonical_parts() {
        // 已编码的路径先解码再按 AWS 的规则编码；非默认端口写进 Host
        let url = Url::parse("http://127.0.0.1:9000/bucket/a%20b/c+d!.tlsync").unwrap();
        assert_eq!(canonical_uri(&url), "/bucket/a%20b/c%2Bd%21.tlsync");
        assert_eq!(host_header(&url), "127.0.0.1:9000");
        assert_eq!(
            host_header(&Url::parse("https://s3.example.com:443/x").unwrap()),
            "s3.example.com"
        );
        assert_eq!(trim_value("  a   b  "), "a b");
        assert_eq!(percent_decode("%zz%41"), b"%zzA");
        assert!(amz_date_now().ends_with('Z') && amz_date_now().len() == 16);
    }

    #[test]
    fn signer_debug_free() {
        // 签名结果中没有秘密访问密钥
        let url = Url::parse("https://b.s3.amazonaws.com/k").unwrap();
        let headers = [("x-amz-date", DATE)];
        let request = Request {
            method: "GET",
            url: &url,
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
        };
        let auth = s3_example().authorization(&request, DATE);
        assert!(!auth.contains("wJalrXUtnFEMI"));
    }
}
