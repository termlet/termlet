//! .xsh 会话文件：绿盟 OSMS 调起时传来，格式与 Xshell 的会话文件相同（INI）。

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;

use encoding_rs::{GBK, UTF_16LE};
use zeroize::Zeroizing;

use super::{DEFAULT_PORT, LaunchKind, LaunchSource, LaunchTarget, parse_port, trim};
use crate::model::Encoding;
use crate::secret::SecretString;

/// .xsh 会话文件的大小上限（字节）：更大的不是会话文件，不读。
pub const MAX_XSH_SIZE: u64 = 64 * 1024;

/// 解析 .xsh 会话文件：`[CONNECTION]` 的 Host、Port、Protocol，`[CONNECTION:AUTHENTICATION]` 的 UserName
/// （OSMS 写成“用户#验证信息”，原样保留）、Password，`[TERMINAL]` 的 CodePage。节名与键名不分大小写。
///
/// 只认 SSH 会话，必须有地址与用户名。文本先看 BOM（UTF-16LE、UTF-8），再试 UTF-8，最后按 GBK 解码。
/// 结果的来源为 [`LaunchSource::Xsh`]，没有名称。
pub fn parse_xsh(data: &[u8]) -> Option<LaunchTarget> {
    let text = decode_text(data);
    let ini = parse_ini(&text);
    let field = |section: &str, key: &str| ini.get(section).and_then(|s| s.get(key)).copied();

    let protocol = field("CONNECTION", "protocol").map(trim).unwrap_or("");
    if !protocol.is_empty() && protocol.to_uppercase() != "SSH" {
        return None;
    }
    let address = field("CONNECTION", "host")
        .map(trim)
        .filter(|host| !host.is_empty())?;
    let username = field("CONNECTION:AUTHENTICATION", "username")
        .map(trim)
        .filter(|user| !user.is_empty())?;
    let port = match field("CONNECTION", "port")
        .map(trim)
        .filter(|port| !port.is_empty())
    {
        Some(port) => parse_port(port)?,
        None => DEFAULT_PORT,
    };
    Some(LaunchTarget {
        kind: LaunchKind::Terminal,
        address: address.to_string(),
        port,
        username: username.to_string(),
        password: password(field("CONNECTION:AUTHENTICATION", "password")),
        name: None,
        encoding: field("TERMINAL", "codepage").and_then(code_page),
        source: LaunchSource::Xsh,
    })
}

/// 读取会话文件；不存在、读不了或超过 [`MAX_XSH_SIZE`] 时返回 `None`。
pub(super) fn read_file(path: &Path) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_XSH_SIZE {
        return None;
    }
    let mut data = Vec::new();
    file.take(MAX_XSH_SIZE + 1).read_to_end(&mut data).ok()?;
    (data.len() as u64 <= MAX_XSH_SIZE).then_some(data)
}

/// 按 BOM 解码：Xshell 的会话文件是 UTF-16LE，其他程序生成的可能是 UTF-8 或 GBK。
/// 文本中有 Base64 的密码，丢弃时清零。
fn decode_text(data: &[u8]) -> Zeroizing<String> {
    let text = if let Some(rest) = data.strip_prefix(&[0xFF, 0xFE]) {
        UTF_16LE.decode_without_bom_handling(rest).0.into_owned()
    } else if let Some(rest) = data.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(rest).into_owned()
    } else {
        match std::str::from_utf8(data) {
            Ok(text) => text.to_string(),
            Err(_) => GBK.decode_without_bom_handling(data).0.into_owned(),
        }
    };
    Zeroizing::new(text)
}

/// INI：节名转大写，键名转小写；值保留等号之后的原文（行首尾的空白已去掉）。
/// 第一个节之前的行忽略；重复的节以后一个为准，重复的键以后一个为准。
fn parse_ini(text: &str) -> HashMap<String, HashMap<String, &str>> {
    let mut sections: HashMap<String, HashMap<String, &str>> = HashMap::new();
    let mut current: Option<String> = None;
    for raw in text.lines() {
        let line = trim(raw);
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .filter(|name| !name.is_empty())
        {
            let name = name.to_uppercase();
            sections.insert(name.clone(), HashMap::new());
            current = Some(name);
        } else if let Some(eq) = line.find('=').filter(|&eq| eq > 0)
            && let Some(section) = current.as_ref().and_then(|name| sections.get_mut(name))
        {
            section.insert(trim(&line[..eq]).to_lowercase(), &line[eq + 1..]);
        }
    }
    sections
}

/// 与 JavaScript 的 atob 相同：不检查最后一个字符中多余的位。
static BASE64_LENIENT: LazyLock<data_encoding::Encoding> = LazyLock::new(|| {
    let mut spec = data_encoding::BASE64.specification();
    spec.check_trailing_bits = false;
    spec.encoding().expect("base64 规格有效")
});

/// OSMS 的 .xsh 中密码是 Base64；Xshell 自己保存的密码是加密的，解不出可读的文本时不用（连接时询问）。
fn password(raw: Option<&str>) -> Option<SecretString> {
    let value = trim(raw?);
    let body = value.trim_end_matches('=');
    let base64 = !body.is_empty()
        && value.len() - body.len() <= 2
        && value.len().is_multiple_of(4)
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/');
    if !base64 {
        return None;
    }
    let bytes = BASE64_LENIENT.decode(value.as_bytes()).ok()?;
    let text = SecretString::new(String::from_utf8(bytes).ok()?);
    // 有控制字符的不是密码（解出来的是 Xshell 加密后的数据）
    let readable = !text.is_empty()
        && !text
            .expose()
            .chars()
            .any(|c| matches!(c, '\0'..='\x1f' | '\x7f'));
    readable.then_some(text)
}

/// CodePage 对应的编码；不认识的为 `None`（按 UTF-8）。
fn code_page(value: &str) -> Option<Encoding> {
    match trim(value) {
        "936" | "54936" => Some(Encoding::Gb18030),
        "950" => Some(Encoding::Big5),
        "65001" => Some(Encoding::Utf8),
        _ => None,
    }
}
