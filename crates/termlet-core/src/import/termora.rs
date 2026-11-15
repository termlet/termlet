//! Termora 2.x 的数据目录。
//!
//! Termora 2.x 把主机保存在数据目录下的 `config/termora.db`（SQLite）。数据目录默认是 `~/.termora`，
//! 可由环境变量 `TERMORA_BASE_DATA_DIR` 指定，Windows 绿色版为程序目录下的 `data`。
//!
//! ```text
//! tb_data(id, ownerId, ownerType, type, version, synced, deleted, data, …)  type 为 Host 的行是主机与文件夹
//! tb_unsafe_setting(name, value)                                            明文保存 __DB_PASSWORD、__DB_SALT
//! ```
//!
//! `data` 列是整段 JSON 加密后的 Base64（Exposed 的 AES_256_PBE_GCM，即 Spring 的 AesBytesEncryptor）：
//! 密钥 = PBKDF2-HMAC-SHA1(`__DB_PASSWORD`, hex 解码的 `__DB_SALT`, 1024 次, 32 字节)，AES-256-GCM，
//! 16 字节 nonce 在密文前，没有附加数据。密钥与数据在同一个文件中（Termora 2.x 没有主密码），不需要用户输入。
//!
//! 解密后的主机 JSON（等于默认值的字段不写出）：
//!
//! ```json
//! {"id":"…","name":"web","protocol":"SSH","host":"10.0.0.1","port":22,"username":"root",
//!  "authentication":{"type":"PublicKey","password":"<钥匙管理器中的密钥 ID>"},
//!  "proxy":{"type":"SOCKS5","host":"127.0.0.1","port":1080,"authenticationType":"Password","username":"u","password":"…"},
//!  "options":{"jumpHosts":["<第一跳主机 ID>","<最后一跳主机 ID>"],"tags":["<标签 ID>"]},
//!  "tunnelings":[…],"remark":"…","sort":3,"parentId":"<文件夹 ID>"}
//! ```
//!
//! 密码认证时 `authentication.password` 是密码。钥匙管理器中的密钥是 type 为 KeyPair 的行：
//! `{"id","name","type","privateKey":"<未加密的 PKCS#8 DER 的 Base64>",…}`。
//!
//! 文件夹是 protocol 为 Folder 的同类记录，parentId 为 "0" 或空时在根目录。只导入 SSH 主机；
//! 登录密码、代理密码与钥匙管理器中的私钥一并导入。
//! Termora 1.x 的数据（storage 目录，Xodus 格式）不支持，需要先用 Termora 2.x 打开一次完成迁移。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use aes_gcm::aead::Aead;
use aes_gcm::aead::consts::U16;
use aes_gcm::aes::Aes256;
use aes_gcm::{AesGcm, KeyInit};
use data_encoding::{Encoding, Specification};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Deserializer};
use zeroize::Zeroizing;

use crate::model::{Auth, Host, Proxy, ProxyKind};
use crate::secret::SecretString;

use super::files::{LocalEnv, TempCopy};
use super::plan::{
    Endpoint, Hint, JumpRef, KeyAlgorithm, PlanBuilder, PlannedHost, PlannedKey, clip,
};
use super::{ImportError, ImportPlan, Source};

const DB_FILE: &str = "termora.db";
/// 数据库文件的大小上限（含快捷命令、密钥等，比单个配置文件大）
const MAX_DB_SIZE: u64 = 256 << 20;
/// 根目录"我的主机"的 ID
const ROOT_ID: &str = "0";

/// `path` 可以是数据目录、其中的 config 目录或 termora.db 文件。
pub(super) fn read(path: &Path, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let file = db_path(&env.input_path(path))?;
    // Termora 运行时也在使用这个数据库：复制一份再读，不加锁、不影响它
    let copy = TempCopy::of(&file, MAX_DB_SIZE)?;
    let conn = Connection::open_with_flags(
        copy.file(),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| ImportError::NotTermora(e.to_string()))?;
    let records = read_records(&conn)?;
    drop(conn);
    Ok(plan(records))
}

/// 数据密钥：PBKDF2-HMAC-SHA1(口令, 盐, 1024 次, 32 字节)。
fn derive_key(password: &[u8], salt: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut key = Zeroizing::new([0u8; 32]);
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, salt, 1024, &mut key[..]);
    key
}

/// Termora 的数据目录（含 `config/termora.db` 时）：`TERMORA_BASE_DATA_DIR` 优先，其次 `~/.termora`。
pub(super) fn default_path(env: &LocalEnv) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    dirs.extend(env.termora_dir.clone());
    dirs.extend(env.home.as_ref().map(|h| h.join(".termora")));
    dirs.into_iter()
        .find(|d| d.join("config").join(DB_FILE).is_file())
}

/// 由用户选择的路径得到数据库文件。
fn db_path(p: &Path) -> Result<PathBuf, ImportError> {
    let meta = fs::metadata(p).map_err(|source| ImportError::Io {
        path: p.to_path_buf(),
        source,
    })?;
    if !meta.is_dir() {
        return Ok(p.to_path_buf());
    }
    for c in [p.join("config").join(DB_FILE), p.join(DB_FILE)] {
        if c.is_file() {
            return Ok(c);
        }
    }
    // 1.x 的数据在 storage 目录（2.x 迁移后改名为 storage-old-<时间>）
    if p.join("storage").is_dir() {
        return Err(ImportError::TermoraV1);
    }
    Err(ImportError::TermoraNotFound)
}

/// 解密 `data` 列：Base64(nonce\[16\] ‖ 密文 ‖ tag\[16\])。
struct DataCipher(AesGcm<Aes256, U16>);

impl DataCipher {
    fn new(key: &[u8; 32]) -> Self {
        Self(AesGcm::new(key.into()))
    }

    fn decrypt(&self, data: &str) -> Option<Zeroizing<Vec<u8>>> {
        let raw = BASE64.decode(data.as_bytes()).ok()?;
        if raw.len() < 32 {
            return None;
        }
        let (nonce, ciphertext) = raw.split_first_chunk::<16>()?;
        self.0
            .decrypt(nonce.into(), ciphertext)
            .ok()
            .map(Zeroizing::new)
    }

    #[cfg(test)]
    fn encrypt(&self, plain: &[u8], nonce: &[u8; 16]) -> String {
        let mut raw = nonce.to_vec();
        raw.extend(self.0.encrypt(nonce.into(), plain).unwrap());
        BASE64.encode(&raw)
    }
}

/// 标准 Base64，忽略换行与空白。
static BASE64: LazyLock<Encoding> = LazyLock::new(|| {
    let mut spec = Specification::new();
    spec.symbols
        .push_str("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/");
    spec.padding = Some('=');
    spec.ignore.push_str(" \t\r\n");
    spec.encoding().expect("Base64 编码表")
});

/// 读取数据库中保存的口令与盐，得到解密 `data` 列的密钥。
fn data_cipher(conn: &Connection) -> Result<DataCipher, ImportError> {
    let not_termora = |e: rusqlite::Error| ImportError::NotTermora(e.to_string());
    let mut stmt = conn
        .prepare(
            "SELECT name, value FROM tb_unsafe_setting WHERE name IN ('__DB_PASSWORD', '__DB_SALT')",
        )
        .map_err(not_termora)?;
    let mut password = Zeroizing::new(String::new());
    let mut salt = String::new();
    let mut rows = stmt.query([]).map_err(not_termora)?;
    while let Some(row) = rows.next().map_err(not_termora)? {
        let name: String = row.get(0).unwrap_or_default();
        let Ok(value) = row.get::<_, String>(1) else {
            continue;
        };
        match name.as_str() {
            "__DB_PASSWORD" => password = Zeroizing::new(value),
            _ => salt = value,
        }
    }
    let salt = data_encoding::HEXLOWER_PERMISSIVE
        .decode(salt.trim().as_bytes())
        .map_err(|_| ImportError::TermoraKeyMissing)?;
    if password.is_empty() {
        return Err(ImportError::TermoraKeyMissing);
    }
    Ok(DataCipher::new(&derive_key(password.as_bytes(), &salt)))
}

/// 解密后的记录。
#[derive(Default)]
struct Records {
    hosts: Vec<TermoraHost>,
    /// 密钥 ID → 密钥
    keys: HashMap<String, TermoraKey>,
    /// 标签 ID → 文字
    tags: HashMap<String, String>,
    /// 无法解密或解析的记录数
    failed: usize,
}

fn read_records(conn: &Connection) -> Result<Records, ImportError> {
    let cipher = data_cipher(conn)?;
    let not_termora = |e: rusqlite::Error| ImportError::NotTermora(e.to_string());
    let mut stmt = conn
        .prepare(
            "SELECT type, data FROM tb_data WHERE deleted = 0 AND type IN ('Host', 'KeyPair', 'Tag')",
        )
        .map_err(not_termora)?;
    let mut rows = stmt.query([]).map_err(not_termora)?;
    let mut records = Records::default();
    while let Some(row) = rows.next().map_err(not_termora)? {
        let (Ok(kind), Ok(data)) = (row.get::<_, String>(0), row.get::<_, String>(1)) else {
            records.failed += 1;
            continue;
        };
        let Some(plain) = cipher.decrypt(&data) else {
            records.failed += 1;
            continue;
        };
        match kind.as_str() {
            "Host" => match serde_json::from_slice::<TermoraHost>(&plain) {
                Ok(h) if !h.id.is_empty() => records.hosts.push(h),
                _ => records.failed += 1,
            },
            "KeyPair" => {
                if let Some((id, key)) = parse_key(&plain) {
                    records.keys.insert(id, key);
                }
            }
            _ => {
                if let Ok(tag) = serde_json::from_slice::<TermoraTag>(&plain) {
                    records.tags.insert(tag.id, tag.text);
                }
            }
        }
    }
    if records.failed > 0 && records.hosts.is_empty() {
        return Err(ImportError::TermoraDecrypt);
    }
    Ok(records)
}

/// JSON 中的 null 按默认值处理（与缺少该字段相同）。
fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// 记录中的凭据：读出后直接放进 [`SecretString`]。
#[derive(Default)]
struct Secret(SecretString);

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(SecretString::new(
            Option::<String>::deserialize(d)?.unwrap_or_default(),
        )))
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TermoraHost {
    #[serde(deserialize_with = "nullable")]
    id: String,
    #[serde(deserialize_with = "nullable")]
    name: String,
    #[serde(deserialize_with = "nullable")]
    protocol: String,
    #[serde(deserialize_with = "nullable")]
    host: String,
    #[serde(deserialize_with = "nullable")]
    port: i64,
    #[serde(deserialize_with = "nullable")]
    username: String,
    #[serde(deserialize_with = "nullable")]
    remark: String,
    #[serde(deserialize_with = "nullable")]
    sort: i64,
    #[serde(deserialize_with = "nullable")]
    parent_id: String,
    #[serde(deserialize_with = "nullable")]
    authentication: Authentication,
    proxy: Option<TermoraProxy>,
    #[serde(deserialize_with = "nullable")]
    options: HostOptions,
    #[serde(deserialize_with = "nullable")]
    tunnelings: Vec<serde::de::IgnoredAny>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Authentication {
    #[serde(rename = "type", deserialize_with = "nullable")]
    kind: String,
    /// 公钥认证时是钥匙管理器中的密钥 ID，密码认证时是密码
    password: Secret,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TermoraProxy {
    #[serde(rename = "type", deserialize_with = "nullable")]
    kind: String,
    #[serde(deserialize_with = "nullable")]
    host: String,
    #[serde(deserialize_with = "nullable")]
    port: i64,
    #[serde(deserialize_with = "nullable")]
    authentication_type: String,
    #[serde(deserialize_with = "nullable")]
    username: String,
    password: Secret,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct HostOptions {
    #[serde(deserialize_with = "nullable")]
    jump_hosts: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    tags: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TermoraKeyPair {
    #[serde(deserialize_with = "nullable")]
    id: String,
    #[serde(deserialize_with = "nullable")]
    name: String,
    private_key: Secret,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct TermoraTag {
    #[serde(deserialize_with = "nullable")]
    id: String,
    #[serde(deserialize_with = "nullable")]
    text: String,
}

/// 钥匙管理器中的密钥：私钥转为 PEM，并在解析时就检查能否导入。
enum TermoraKey {
    Usable {
        name: String,
        algorithm: KeyAlgorithm,
        pem: SecretString,
    },
    Unusable {
        name: String,
    },
}

fn parse_key(plain: &[u8]) -> Option<(String, TermoraKey)> {
    let k: TermoraKeyPair = serde_json::from_slice(plain).ok()?;
    if k.id.is_empty() {
        return None;
    }
    let der = BASE64
        .decode(k.private_key.0.expose().as_bytes())
        .ok()
        .map(Zeroizing::new);
    let key = match der
        .as_ref()
        .and_then(|d| pkcs8_algorithm(d).map(|a| (a, d)))
    {
        Some((algorithm, der)) => TermoraKey::Usable {
            name: k.name,
            algorithm,
            pem: pem(der),
        },
        None => {
            tracing::debug!(key = %k.name, "Termora 的密钥不是支持的 PKCS#8 私钥");
            TermoraKey::Unusable { name: k.name }
        }
    };
    Some((k.id, key))
}

/// PKCS#8 DER 转为 PEM（标签为 PRIVATE KEY，每行 64 个字符）。
fn pem(der: &[u8]) -> SecretString {
    // 标签分开写，免得源码被密钥扫描当成私钥
    const BEGIN: &str = concat!("-----BEGIN ", "PRIVATE KEY", "-----\n");
    const END: &str = concat!("-----END ", "PRIVATE KEY", "-----\n");
    let b64 = Zeroizing::new(BASE64.encode(der));
    // 一次分配够，追加时不会重新分配而留下未清零的副本
    let mut out = String::with_capacity(BEGIN.len() + b64.len() + b64.len() / 64 + 1 + END.len());
    out.push_str(BEGIN);
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(END);
    SecretString::new(out)
}

/// 检查 PKCS#8 PrivateKeyInfo 的结构并识别算法；不是支持的私钥（RSA、ECDSA P-256/384/521、Ed25519）时为 `None`。
/// 只看结构，不验证密钥本身；转为 OpenSSH 格式在存进密钥库时进行。
fn pkcs8_algorithm(der: &[u8]) -> Option<KeyAlgorithm> {
    const RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
    const EC: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
    const ED25519: &[u8] = &[0x2B, 0x65, 0x70];
    const CURVES: [&[u8]; 3] = [
        &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07], // P-256
        &[0x2B, 0x81, 0x04, 0x00, 0x22],                   // P-384
        &[0x2B, 0x81, 0x04, 0x00, 0x23],                   // P-521
    ];
    let (info, rest) = der_tlv(der, 0x30)?;
    if !rest.is_empty() {
        return None;
    }
    let (version, info) = der_tlv(info, 0x02)?;
    if version != [0] && version != [1] {
        return None;
    }
    let (algorithm, info) = der_tlv(info, 0x30)?;
    let (oid, params) = der_tlv(algorithm, 0x06)?;
    let (key, _) = der_tlv(info, 0x04)?;
    match oid {
        RSA => der_tlv(key, 0x30).map(|_| KeyAlgorithm::Rsa),
        EC => {
            let (curve, _) = der_tlv(params, 0x06)?;
            (CURVES.contains(&curve) && der_tlv(key, 0x30).is_some()).then_some(KeyAlgorithm::Ecdsa)
        }
        ED25519 => {
            let (seed, _) = der_tlv(key, 0x04)?;
            (seed.len() == 32).then_some(KeyAlgorithm::Ed25519)
        }
        _ => None,
    }
}

/// 读出一个标签为 `tag` 的 DER 元素（定长编码），返回内容与其后的数据。
fn der_tlv(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (&t, rest) = input.split_first()?;
    if t != tag {
        return None;
    }
    let (&first, mut rest) = rest.split_first()?;
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let n = usize::from(first & 0x7F);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |acc, &b| acc << 8 | usize::from(b));
        rest = &rest[n..];
        len
    };
    (rest.len() >= len).then(|| rest.split_at(len))
}

/// 把解密后的记录转为导入计划。
fn plan(records: Records) -> ImportPlan {
    let Records {
        mut hosts,
        keys,
        tags,
        failed,
    } = records;
    for h in &mut hosts {
        if h.parent_id.is_empty() {
            h.parent_id = ROOT_ID.into();
        }
        if h.port == 0 {
            h.port = 22;
        }
    }
    let by_id: HashMap<&str, &TermoraHost> = hosts.iter().map(|h| (h.id.as_str(), h)).collect();
    let is_ssh = |h: &TermoraHost| h.protocol.eq_ignore_ascii_case("SSH");

    struct Entry<'a> {
        group: Vec<String>,
        sort: i64,
        item: PlannedHost,
        /// 使用的钥匙管理器中的密钥
        key: Option<&'a str>,
    }
    let mut skipped = failed;
    let mut list: Vec<Entry> = Vec::new();
    for h in &hosts {
        if h.protocol.eq_ignore_ascii_case("Folder") {
            continue;
        }
        // 本地终端、Telnet、RDP、串口、各种存储等不是 SSH 的会话跳过
        if !is_ssh(h) || h.host.trim().is_empty() {
            skipped += 1;
            continue;
        }
        let name = [&h.name, &h.host]
            .into_iter()
            .map(|s| s.trim())
            .find(|s| !s.is_empty())
            .unwrap_or_default();
        let host = Host {
            name: clip(name, 128),
            address: h.host.trim().to_string(),
            port: port_of(h.port, 22),
            username: h.username.trim().to_string(),
            note: clip(h.remark.trim(), 2000),
            ..Default::default()
        };
        let mut item = PlannedHost::new(h.id.clone(), host);
        let mut key = None;
        let auth = &h.authentication;
        match auth.kind.as_str() {
            "Password" => item.set_password(auth.password.0.clone()),
            "PublicKey" => match keys.get(auth.password.0.expose()) {
                None => item.hints.push(Hint::LibraryKeyMissing),
                Some(TermoraKey::Unusable { name }) => item
                    .hints
                    .push(Hint::LibraryKeyUnusable { name: name.clone() }),
                Some(TermoraKey::Usable { .. }) => key = Some(auth.password.0.expose()),
            },
            "SSHAgent" => item.host.auth = Auth::Agent,
            _ => {}
        }
        if let Some(p) = &h.proxy {
            let kind = match p.kind.to_ascii_uppercase().as_str() {
                "HTTP" => Some(ProxyKind::Http),
                "SOCKS5" => Some(ProxyKind::Socks5),
                _ => None,
            };
            if let Some(kind) = kind {
                let mut proxy = Proxy {
                    kind,
                    address: p.host.trim().to_string(),
                    // Termora 的默认值，等于默认值时不写出
                    port: port_of(p.port, 7890),
                    ..Default::default()
                };
                if p.authentication_type == "Password" {
                    proxy.username = p.username.trim().to_string();
                    item.set_proxy_password(p.password.0.clone());
                }
                item.host.proxy = Some(proxy);
            }
        }
        let chain = &h.options.jump_hosts;
        if let Some(last_id) = chain.last() {
            match by_id.get(last_id.as_str()).filter(|j| is_ssh(j)) {
                None => item.hints.push(Hint::JumpNotFound),
                Some(last) => {
                    item.jump = Some(JumpRef::new(
                        last.name.clone(),
                        Endpoint::new(
                            last.host.trim(),
                            port_of(last.port, 22),
                            last.username.trim(),
                        ),
                    ));
                    // 每台主机只有一个跳板机：多级时取最后一跳，它自己的跳板机设置与这条链不同时提醒
                    let n = chain.len();
                    if n > 1 && last.options.jump_hosts[..] != chain[..n - 1] {
                        item.hints.push(Hint::MultiHopJump {
                            chain: chain
                                .iter()
                                .map(|id| {
                                    by_id
                                        .get(id.as_str())
                                        .map_or_else(|| "?".to_string(), |j| j.name.clone())
                                })
                                .collect(),
                        });
                    }
                }
            }
        }
        if !h.tunnelings.is_empty() {
            item.hints.push(Hint::ForwardsNotImported {
                count: h.tunnelings.len(),
            });
        }
        for id in &h.options.tags {
            let tag = tags.get(id).map_or("", |t| t.trim());
            if !tag.is_empty() && item.host.tags.len() < 20 {
                item.host.tags.push(clip(tag, 32));
            }
        }
        list.push(Entry {
            group: group_path(&by_id, &h.parent_id),
            sort: h.sort,
            item,
            key,
        });
    }
    // 与 Termora 中的顺序一致：按分组，同一分组内按 sort（相同时按名称）
    list.sort_by(|a, b| {
        a.group
            .join("/")
            .cmp(&b.group.join("/"))
            .then(a.sort.cmp(&b.sort))
            .then_with(|| a.item.host.name.cmp(&b.item.host.name))
    });

    let mut plan = PlanBuilder::new(Source::Termora);
    let mut key_index: HashMap<&str, usize> = HashMap::new();
    for Entry {
        group,
        mut item,
        key,
        ..
    } in list
    {
        item.group = plan.group(&group);
        if let Some(id) = key
            && let Some(TermoraKey::Usable {
                name,
                algorithm,
                pem,
            }) = keys.get(id)
        {
            let index = *key_index.entry(id).or_insert_with(|| {
                plan.add_key(PlannedKey::new(name.clone(), *algorithm, pem.clone()))
            });
            item.set_key(index);
        }
        plan.push(item);
    }
    plan.skip(skipped);
    plan.finish()
}

fn port_of(port: i64, default: u16) -> u16 {
    u16::try_from(port)
        .ok()
        .filter(|&p| p > 0)
        .unwrap_or(default)
}

/// 所在文件夹的各级名称；文件夹不存在（例如团队数据的根）时到此为止，归到根目录。
fn group_path(by_id: &HashMap<&str, &TermoraHost>, parent: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut id = parent;
    while id != ROOT_ID && !id.is_empty() && seen.insert(id) {
        let Some(folder) = by_id
            .get(id)
            .filter(|f| f.protocol.eq_ignore_ascii_case("Folder"))
        else {
            break;
        };
        let name = folder.name.trim();
        if !name.is_empty() {
            names.push(name.to_string());
        }
        id = &folder.parent_id;
    }
    names.reverse();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "0123456789abcdef"; // gitleaks:allow 测试用的 Termora 数据库口令
    const SALT: &str = "a1b2c3d4e5f6";
    /// PBKDF2-HMAC-SHA1(PASSWORD, SALT, 1024, 32)，用 Python 的 hashlib.pbkdf2_hmac 算出
    const DERIVED_KEY: &str = "500adcca701a1365791584c7ab7e9ce1745967d970d65ab54e86c73bf1f43e84"; // gitleaks:allow 测试用
    /// 用 Termora 所用的 i2p eddsa 0.3.0 生成的测试用 ED25519 密钥（PKCS#8 / X.509 DER 的 Base64），只用于本测试
    const KEY_PRIVATE: &str = "MC4CAQAwBQYDK2VwBCIEIGrP29i5KNfnT78z2w27ww3IfBDfHnwLY49ZBrlM364h"; // gitleaks:allow 一次性测试密钥
    const KEY_PUBLIC: &str = "MCowBQYDK2VwAyEAK4sBws/HJLE+YQYzirf9OnGqjj18GJL3NNbo3xs2ajs="; // gitleaks:allow 一次性测试密钥

    fn salt() -> Vec<u8> {
        data_encoding::HEXLOWER.decode(SALT.as_bytes()).unwrap()
    }

    fn read(path: &Path) -> Result<ImportPlan, ImportError> {
        super::read(path, &crate::import::files::tests::env())
    }

    struct Row<'a> {
        id: &'a str,
        kind: &'a str,
        data: &'a str,
        deleted: bool,
    }

    fn row<'a>(id: &'a str, kind: &'a str, data: &'a str) -> Row<'a> {
        Row {
            id,
            kind,
            data,
            deleted: false,
        }
    }

    /// 按 Termora 2.x 的表结构在 `dir/config/termora.db` 中写入数据。数据库中保存的口令总是 `PASSWORD`，
    /// 数据用 `password` 派生的密钥加密：传入别的口令可以模拟密钥对不上的数据。
    fn write_db(dir: &Path, password: &str, rows: &[Row]) -> PathBuf {
        let file = dir.join("config").join(DB_FILE);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let conn = Connection::open(&file).unwrap();
        conn.execute_batch(
            r#"CREATE TABLE tb_data (id CHAR(32) NOT NULL PRIMARY KEY, ownerId CHAR(32) NOT NULL, ownerType VARCHAR(32) NOT NULL,
                "type" VARCHAR(32) NOT NULL, version BIGINT NOT NULL, synced BOOLEAN NOT NULL, deleted BOOLEAN NOT NULL,
                "data" TEXT NOT NULL, extra1 TEXT NOT NULL, extra2 TEXT NOT NULL, extra3 TEXT NOT NULL, extra4 TEXT NOT NULL,
                extra5 TEXT NOT NULL);
               CREATE TABLE tb_unsafe_setting (id CHAR(32) NOT NULL PRIMARY KEY, "name" VARCHAR(128) NOT NULL,
                "value" TEXT NOT NULL, extra1 TEXT NOT NULL, extra2 TEXT NOT NULL, extra3 TEXT NOT NULL);"#,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tb_unsafe_setting VALUES ('p', '__DB_PASSWORD', ?1, '', '', ''), ('s', '__DB_SALT', ?2, '', '', '')",
            [PASSWORD, SALT],
        )
        .unwrap();
        let cipher = DataCipher::new(&derive_key(password.as_bytes(), &salt()));
        for (i, r) in rows.iter().enumerate() {
            // 删除的记录 data 被置空后再加密
            let data = if r.deleted { "" } else { r.data };
            let mut nonce = [0u8; 16];
            nonce[..8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
            conn.execute(
                "INSERT INTO tb_data VALUES (?1, '0', 'User', ?2, 1, 0, ?3, ?4, '', '', '', '', '')",
                rusqlite::params![r.id, r.kind, r.deleted, cipher.encrypt(data.as_bytes(), &nonce)],
            )
            .unwrap();
        }
        file
    }

    /// 由 Termora 所用的 spring-security-crypto 7.0.0（AesBytesEncryptor，GCM）加密得到的密文，口令与盐同上。
    #[test]
    fn decrypt_known_answer() {
        let cipher = DataCipher::new(&derive_key(PASSWORD.as_bytes(), &salt()));
        let known = "gKPz46eXgb4JBKfsgi1/a8VhmLoepkWACptkkZ21RjhlQmeOh5wWRMiH+WQaqEtnDlsfgVSSLjJxd/EVWjrpc8Gq24AlcTL28V5BhK/N0+kmlajmSmHhwcLfCBzfMXiPa7qxswznUsGHqdOpVjMOTxzVWpaM9Bk001tnIgzUevZn439S3Q3st3CkiCFw3I9GqBHGFiwKazZnhXN7";
        let plain = cipher.decrypt(known).unwrap();
        assert_eq!(
            std::str::from_utf8(&plain).unwrap(),
            r#"{"id":"0199aa00000070008000000000000001","name":"web-01","protocol":"SSH","host":"10.0.0.1","port":2222,"username":"deploy"}"#
        );
        // 反过来：用同样的 nonce（密文的前 16 字节）加密得到逐字节相同的密文
        let nonce = |text: &str| -> [u8; 16] {
            BASE64.decode(text.as_bytes()).unwrap()[..16]
                .try_into()
                .unwrap()
        };
        assert_eq!(cipher.encrypt(&plain, &nonce(known)), known);
        // 空串（删除的记录）
        let known_empty = "o6PrbtAJHFGe81Kb0C1P9ape9DowBrfURffsh73hgEw=";
        let empty = cipher.decrypt(known_empty).unwrap();
        assert!(empty.is_empty());
        assert_eq!(cipher.encrypt(b"", &nonce(known_empty)), known_empty);
        assert!(cipher.decrypt("AAAA").is_none(), "过短的密文");
        assert!(cipher.decrypt("not base64!").is_none());
    }

    #[test]
    fn derive_key_known_answer() {
        let key = derive_key(PASSWORD.as_bytes(), &salt());
        assert_eq!(data_encoding::HEXLOWER.encode(&key[..]), DERIVED_KEY);
    }

    #[test]
    fn hosts_folders_and_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let long_tag = format!(r#"{{"id":"t2","text":"{}"}}"#, "长".repeat(40));
        let key1 = format!(
            r#"{{"id":"k1","name":"deploy-key","type":"ED25519","privateKey":"{KEY_PRIVATE}","publicKey":"{KEY_PUBLIC}"}}"#
        );
        write_db(
            dir.path(),
            PASSWORD,
            &[
                // 文件夹：生产 / 数据库；孤儿文件夹（父文件夹不存在）归到根目录
                row(
                    "f1",
                    "Host",
                    r#"{"id":"f1","name":"生产","protocol":"Folder"}"#,
                ),
                row(
                    "f2",
                    "Host",
                    r#"{"id":"f2","name":"数据库","protocol":"Folder","parentId":"f1"}"#,
                ),
                row(
                    "f3",
                    "Host",
                    r#"{"id":"f3","name":"孤儿","protocol":"Folder","parentId":"team-1"}"#,
                ),
                // 公钥认证（钥匙管理器中的密钥）、默认端口、备注、标签、端口转发
                row(
                    "h1",
                    "Host",
                    r#"{"id":"h1","name":"db-01","protocol":"SSH","host":" 10.0.0.5 ","username":"root",
                    "authentication":{"type":"PublicKey","password":"k1"},"remark":"主库","parentId":"f2","sort":2,
                    "options":{"tags":["t1","t2","t-missing"]},"tunnelings":[{"sourcePort":5432},{"type":"Dynamic"}]}"#,
                ),
                // 密码认证、HTTP 代理（默认端口不写出，没有认证时不带用户名与密码）
                row(
                    "h2",
                    "Host",
                    r#"{"id":"h2","name":"web-01","protocol":"ssh","host":"10.0.0.6","port":2222,
                    "username":"deploy","authentication":{"type":"Password","password":"secret"},"parentId":"f1","sort":1,
                    "proxy":{"type":"HTTP","host":"proxy","username":"pu","password":"pp"}}"#,
                ),
                // ssh-agent、SOCKS5 代理（有认证）、经 h2 的单级跳板机
                row(
                    "h3",
                    "Host",
                    r#"{"id":"h3","name":"bastion-app","protocol":"SSH","host":"10.0.0.7","port":22,
                    "username":"ops","authentication":{"type":"SSHAgent"},"parentId":"f3",
                    "proxy":{"type":"SOCKS5","host":"127.0.0.1","port":1080,"authenticationType":"Password","username":"su","password":"sp"},
                    "options":{"jumpHosts":["h2"]}}"#,
                ),
                // 多级跳板机 h2 → h3：h3 自己经 h2 连接，与这条链一致，不提醒
                row(
                    "h4",
                    "Host",
                    r#"{"id":"h4","name":"deep","protocol":"SSH","host":"10.0.0.8","username":"u",
                    "options":{"jumpHosts":["h2","h3"]}}"#,
                ),
                // 多级跳板机 h3 → h2：h2 自己直连，与这条链不同，提醒
                row(
                    "h5",
                    "Host",
                    r#"{"id":"h5","name":"odd","protocol":"SSH","host":"10.0.0.9","username":"u",
                    "options":{"jumpHosts":["h3","h2"]}}"#,
                ),
                // 跳板机已删除
                row(
                    "h6",
                    "Host",
                    r#"{"id":"h6","name":"lost-jump","protocol":"SSH","host":"10.0.0.10","username":"u",
                    "options":{"jumpHosts":["gone"]}}"#,
                ),
                // 密钥已不存在、密钥无法解析
                row(
                    "h7",
                    "Host",
                    r#"{"id":"h7","name":"no-key","protocol":"SSH","host":"10.0.0.12","username":"u",
                    "authentication":{"type":"PublicKey","password":"gone"}}"#,
                ),
                row(
                    "h8",
                    "Host",
                    r#"{"id":"h8","name":"bad-key","protocol":"SSH","host":"10.0.0.13","username":"u",
                    "authentication":{"type":"PublicKey","password":"k2"}}"#,
                ),
                // 与 h1 共用同一把密钥：密钥库中只存一次
                row(
                    "h9",
                    "Host",
                    r#"{"id":"h9","name":"db-02","protocol":"SSH","host":"10.0.0.14","username":"root",
                    "authentication":{"type":"PublicKey","password":"k1"},"parentId":"f2","sort":3,"proxy":null,"options":null}"#,
                ),
                // 跳过：本地终端、RDP、缺少地址；删除的主机不计数
                row(
                    "x1",
                    "Host",
                    r#"{"id":"x1","name":"本地","protocol":"local"}"#,
                ),
                row(
                    "x2",
                    "Host",
                    r#"{"id":"x2","name":"win","protocol":"RDP","host":"10.0.0.11"}"#,
                ),
                row(
                    "x3",
                    "Host",
                    r#"{"id":"x3","name":"empty","protocol":"SSH"}"#,
                ),
                Row {
                    id: "x4",
                    kind: "Host",
                    data: "",
                    deleted: true,
                },
                row("k1", "KeyPair", &key1),
                row(
                    "k2",
                    "KeyPair",
                    r#"{"id":"k2","name":"broken","type":"RSA","privateKey":"AAAA"}"#,
                ),
                row("t1", "Tag", r#"{"id":"t1","text":"mysql"}"#),
                row("t2", "Tag", &long_tag),
                row("s1", "Snippet", r#"{"id":"s1"}"#),
            ],
        );

        let plan = read(dir.path()).unwrap();
        assert_eq!(plan.skipped, 3, "应跳过本地终端、RDP、缺少地址的 3 个");
        let names: Vec<&str> = plan.hosts.iter().map(|h| h.host.name.as_str()).collect();
        // 按分组排序，同一分组内按 sort、名称
        assert_eq!(
            names,
            [
                "bad-key",
                "deep",
                "lost-jump",
                "no-key",
                "odd",
                "bastion-app",
                "web-01",
                "db-01",
                "db-02"
            ]
        );
        let get = |name: &str| plan.hosts.iter().find(|h| h.host.name == name).unwrap();
        let group = |h: &PlannedHost| {
            h.group
                .map_or_else(String::new, |g| plan.group_path(g).join("/"))
        };

        let db = get("db-01");
        assert_eq!(
            (
                db.host.address.as_str(),
                db.host.port,
                group(db),
                db.key.as_str()
            ),
            ("10.0.0.5", 22, "生产/数据库".to_string(), "h1")
        );
        assert_eq!(
            db.host.tags,
            ["mysql".to_string(), "长".repeat(32)],
            "标签应按 ID 取文字并截断到 32 个字符"
        );
        assert_eq!(db.host.note, "主库");
        assert_eq!(db.hints, [Hint::ForwardsNotImported { count: 2 }]);
        // 钥匙管理器中的私钥转为 PEM，与 Termora 记录的公钥一致
        assert!(!db.has_password());
        let key = &plan.keys[db.key().unwrap()];
        assert_eq!(
            (key.name.as_str(), key.algorithm),
            ("deploy-key", KeyAlgorithm::Ed25519)
        );
        let pem = key.private_key().expose();
        assert!(
            pem.starts_with("-----BEGIN PRIVATE KEY-----\n")
                && pem.ends_with("-----END PRIVATE KEY-----\n")
        );
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        let der = BASE64.decode(body.as_bytes()).unwrap();
        let seed: [u8; 32] = der[der.len() - 32..].try_into().unwrap();
        let public = ssh_key::private::Ed25519Keypair::from_seed(&seed).public;
        let spki = BASE64.decode(KEY_PUBLIC.as_bytes()).unwrap();
        assert_eq!(
            public.0[..],
            spki[spki.len() - 32..],
            "转换后的私钥与 Termora 记录的公钥不一致"
        );
        assert_eq!(get("db-02").key(), db.key(), "同一把密钥只存一次");
        assert_eq!(plan.keys.len(), 1);

        let web = get("web-01");
        assert_eq!((web.host.port, group(web)), (2222, "生产".to_string()));
        assert!(web.hints.is_empty());
        assert_eq!(web.host.auth, Auth::Password);
        let proxy = web.host.proxy.as_ref().unwrap();
        assert_eq!(
            (
                proxy.kind,
                proxy.address.as_str(),
                proxy.port,
                proxy.username.as_str()
            ),
            (ProxyKind::Http, "proxy", 7890, ""),
            "协议名不区分大小写；代理没有认证时不带用户名"
        );
        assert_eq!(web.password().map(SecretString::expose), Some("secret"));
        assert!(!web.has_proxy_password() && web.key().is_none());

        let app = get("bastion-app");
        assert!(!app.has_password() && app.key().is_none());
        assert_eq!(app.proxy_password().map(SecretString::expose), Some("sp"));
        assert_eq!(app.host.auth, Auth::Agent);
        assert_eq!(
            group(app),
            "孤儿",
            "所在文件夹的上级不存在时，该文件夹在根目录"
        );
        let proxy = app.host.proxy.as_ref().unwrap();
        assert_eq!(
            (
                proxy.kind,
                proxy.address.as_str(),
                proxy.port,
                proxy.username.as_str()
            ),
            (ProxyKind::Socks5, "127.0.0.1", 1080, "su")
        );
        let web_index = plan.hosts.iter().position(|h| h.key == "h2");
        assert_eq!(
            app.jump,
            Some(JumpRef {
                name: "web-01".into(),
                endpoint: Endpoint::new("10.0.0.6", 2222, "deploy"),
                host: web_index,
            })
        );

        let deep = get("deep");
        assert_eq!(
            deep.jump.as_ref().map(|j| j.endpoint.address()),
            Some("10.0.0.7")
        );
        assert!(
            deep.hints.is_empty(),
            "与跳板机自身设置一致的多级跳板机不需要提醒"
        );
        let odd = get("odd");
        assert_eq!(
            odd.jump.as_ref().map(|j| j.endpoint.address()),
            Some("10.0.0.6")
        );
        assert_eq!(
            odd.hints,
            [Hint::MultiHopJump {
                chain: vec!["bastion-app".into(), "web-01".into()]
            }]
        );
        let lost = get("lost-jump");
        assert!(lost.jump.is_none());
        assert_eq!(lost.hints, [Hint::JumpNotFound]);
        let no_key = get("no-key");
        assert!(no_key.key().is_none());
        assert_eq!(no_key.hints, [Hint::LibraryKeyMissing]);
        let bad = get("bad-key");
        assert!(bad.key().is_none());
        assert_eq!(
            bad.hints,
            [Hint::LibraryKeyUnusable {
                name: "broken".into()
            }]
        );

        // 凭据不出现在 Debug 输出中
        let debug = format!("{plan:?}");
        for s in ["secret", "\"sp\"", "PRIVATE KEY", &KEY_PRIVATE[..16]] {
            assert!(!debug.contains(s), "Debug 输出中不应有凭据 {s}");
        }

        // 选择 config 目录、数据库文件同样可以
        for p in [
            dir.path().join("config"),
            dir.path().join("config").join(DB_FILE),
        ] {
            assert_eq!(read(&p).unwrap().hosts.len(), plan.hosts.len());
        }
        // 临时副本用完即删
        let copy = TempCopy::of(&dir.path().join("config").join(DB_FILE), MAX_DB_SIZE).unwrap();
        let copied = copy.dir.clone();
        assert!(copy.file().is_file());
        drop(copy);
        assert!(!copied.exists());
    }

    #[test]
    fn errors() {
        // 1.x 的数据目录
        let v1 = tempfile::tempdir().unwrap();
        fs::create_dir(v1.path().join("storage")).unwrap();
        assert!(matches!(db_path(v1.path()), Err(ImportError::TermoraV1)));
        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(
            db_path(empty.path()),
            Err(ImportError::TermoraNotFound)
        ));
        assert!(matches!(
            db_path(&empty.path().join("missing")),
            Err(ImportError::Io { .. })
        ));

        // 不是 Termora 的 SQLite 数据库
        let other = empty.path().join("other.db");
        Connection::open(&other)
            .unwrap()
            .execute_batch("CREATE TABLE t (x)")
            .unwrap();
        assert!(matches!(read(&other), Err(ImportError::NotTermora(_))));
        // 不是 SQLite 文件
        let text = empty.path().join("text.db");
        fs::write(&text, b"hello, this is not sqlite at all").unwrap();
        assert!(matches!(read(&text), Err(ImportError::NotTermora(_))));

        // 密钥对不上：全部无法解密
        let wrong = tempfile::tempdir().unwrap();
        write_db(
            wrong.path(),
            "ffffffffffffffff",
            &[row(
                "h1",
                "Host",
                r#"{"id":"h1","name":"a","protocol":"SSH","host":"h","username":"u"}"#,
            )],
        );
        assert!(matches!(
            read(wrong.path()),
            Err(ImportError::TermoraDecrypt)
        ));
    }

    #[test]
    fn default_path_prefers_env() {
        let home = tempfile::tempdir().unwrap();
        let mut env = crate::import::files::tests::env();
        env.home = Some(home.path().to_path_buf());
        assert_eq!(default_path(&env), None, "没有数据时应为空");
        write_db(&home.path().join(".termora"), PASSWORD, &[]);
        assert_eq!(default_path(&env), Some(home.path().join(".termora")));
        // 环境变量指定的数据目录优先
        let custom = tempfile::tempdir().unwrap();
        write_db(custom.path(), PASSWORD, &[]);
        env.termora_dir = Some(custom.path().to_path_buf());
        assert_eq!(default_path(&env), Some(custom.path().to_path_buf()));
    }

    #[test]
    fn pkcs8_structure() {
        let ed = BASE64.decode(KEY_PRIVATE.as_bytes()).unwrap();
        assert_eq!(pkcs8_algorithm(&ed), Some(KeyAlgorithm::Ed25519));
        assert_eq!(pkcs8_algorithm(&ed[..ed.len() - 1]), None, "截断");
        assert_eq!(pkcs8_algorithm(&[0, 0, 0]), None);
        assert_eq!(pkcs8_algorithm(&[]), None);
        // RSA：PrivateKeyInfo { 0, {rsaEncryption, NULL}, OCTET STRING { SEQUENCE {} } }
        let rsa = [
            0x30, 0x16, 0x02, 0x01, 0x00, 0x30, 0x0D, 0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7,
            0x0D, 0x01, 0x01, 0x01, 0x05, 0x00, 0x04, 0x02, 0x30, 0x00,
        ];
        assert_eq!(pkcs8_algorithm(&rsa), Some(KeyAlgorithm::Rsa));
        // ECDSA P-256
        let ec = [
            0x30, 0x18, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D,
            0x02, 0x01, 0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07, 0x04, 0x02,
            0x30, 0x00,
        ];
        assert_eq!(pkcs8_algorithm(&ec[..]), None, "长度不符");
        let mut ec = ec.to_vec();
        ec[1] = 0x1C;
        assert_eq!(pkcs8_algorithm(&ec), Some(KeyAlgorithm::Ecdsa));
        // DSA 不支持
        let dsa = [
            0x30, 0x10, 0x02, 0x01, 0x00, 0x30, 0x09, 0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x38,
            0x04, 0x01, 0x04, 0x00,
        ];
        assert_eq!(pkcs8_algorithm(&dsa), None);
    }
}
