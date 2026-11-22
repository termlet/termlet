//! ShellRS 的同步文件夹（格式版本 1，ShellRS 0.1.16 起），按 `crates/shellrs-sync/FORMAT.md`（提交 98d471c）。
//!
//! ```text
//! shellrs-sync/v1/vault.json            数据密钥，用同步密码包裹
//! shellrs-sync/v1/devices/<设备>.srs    每台设备一个加密的快照；设备 ID 是 16 个 [a-z0-9]
//! ```
//!
//! - `vault.json`：`format`（大于 1 的不认）、`vault_id`（16 字节 hex）、`kdf`（argon2id、版本 19、`memory_kib`、
//!   `passes`、`parallelism`、16 字节 hex 的 `salt`）、`keys`（`generation`、24 字节 hex 的 `nonce`、`wrapped`）。
//!   密钥加密密钥是 Argon2id（同步密码的 Unicode NFC，输出 32 字节）；各代数据密钥用 XChaCha20-Poly1305 包裹，
//!   附加数据是 `"shellrs-sync key"` ‖ vault_id ‖ generation（u32 大端）。成本超过 256 MiB、10 次、4 路的不认。
//! - `.srs`：49 字节的头 `"SRSY"` ‖ 版本 1 ‖ vault_id ‖ generation（u32 大端）‖ nonce（24），其后是 XChaCha20-Poly1305
//!   的密文，附加数据是这个头 ‖ 文件名中的设备 ID。明文是原始 DEFLATE 压缩的快照 JSON：`format`、`min_reader`、
//!   `device`、`records`。文件超过 16 MiB、解压后超过 64 MiB 的不读；名字不是恰好 `<设备>.srs` 的（网盘的冲突副本）忽略。
//! - 记录：`{"kind", "id", "v": [毫秒, 计数, 设备], "data": {…}}` 或删除标记 `"deleted": true`；同一条记录在各设备的
//!   文件中取 `v` 最大的一份（先比毫秒、计数，再比设备 ID），最大的是删除标记就当不存在；`min_reader` 大于 1 的记录不用。
//!   不认识的种类、字段忽略；`secret.*` 是秘密，只在加密的文件中是明文。
//!
//! 引用不存在的记录时照 ShellRS 合并后的修补：分组、上级、片段分类不存在的到最外层；凭据不存在的改为密码登录；
//! 跳板主机不存在的那一跳为空（导入后直连并提醒）；转发经由的主机不存在的不导入。

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use super::{
    Category, Credential, Data, ForwardRecord, Group, HostRecord, Route, SnippetRecord,
    credential_kind, endpoint_key, forward_kind, login, port_of, protocol, proxy_kind,
};
use crate::import::ImportError;
use crate::import::files::read_limited;
use crate::import::plan::PlanNote;
use crate::model::{ForwardKind, Protocol};
use crate::secret::SecretString;

/// 同步文件夹中的布局
const STORE_DIR: &str = "shellrs-sync";
const VERSION_DIR: &str = "v1";
const VAULT_FILE: &str = "vault.json";
const DEVICES_DIR: &str = "devices";
/// 这里认得的格式版本
const FORMAT: u64 = 1;
const VAULT_MAX: u64 = 1 << 20;
const DEVICE_MAX: u64 = 16 << 20;
const SNAPSHOT_MAX: u64 = 64 << 20;
/// Argon2 成本的上限（与 ShellRS 的读者相同）
const MAX_MEMORY_KIB: u32 = 256 * 1024;
const MAX_PASSES: u32 = 10;
const MAX_LANES: u32 = 4;
const HEADER_MAGIC: &[u8; 4] = b"SRSY";
const HEADER_LEN: usize = 49;
const KEY_CONTEXT: &[u8] = b"shellrs-sync key";
/// 设备 ID 的长度
const DEVICE_ID_LEN: usize = 16;

/// 选的路径对应的同步数据目录（`shellrs-sync/v1`）：选的是 ShellRS 中设的同步文件夹、其中的 `shellrs-sync`、
/// `v1`，或直接选了 `vault.json`。不是同步文件夹时为 `None`。
pub(super) fn locate(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        let is_vault = path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case(VAULT_FILE));
        return is_vault
            .then(|| path.parent().map(Path::to_path_buf))
            .flatten();
    }
    [
        path.to_path_buf(),
        path.join(VERSION_DIR),
        path.join(STORE_DIR).join(VERSION_DIR),
    ]
    .into_iter()
    .find(|dir| dir.join(VAULT_FILE).is_file())
}

fn not_shellrs(reason: impl std::fmt::Display) -> ImportError {
    ImportError::NotShellRs(reason.to_string())
}

/// 解开同步数据目录 `dir` 中的全部设备文件，合并成一份数据（含秘密）。
pub(super) fn read(dir: &Path, password: &SecretString) -> Result<Data, ImportError> {
    let raw = read_limited(&dir.join(VAULT_FILE), VAULT_MAX)?;
    let vault: Value =
        serde_json::from_slice(&raw).map_err(|e| not_shellrs(format!("vault.json：{e}")))?;
    let keys = unlock(&vault, password)?;
    let mut records = Records::default();
    let mut skipped = 0;
    let mut too_new = 0;
    let mut opened = 0;
    for (device, file) in device_files(&dir.join(DEVICES_DIR)) {
        match open_device(&file, &device, &keys) {
            Ok(Snapshot::Records(list)) => {
                opened += 1;
                records.merge(list);
            }
            Ok(Snapshot::TooNew) => too_new += 1,
            Err(reason) => {
                tracing::warn!(device = %device, reason, "ShellRS 的设备文件读不了，跳过");
                skipped += 1;
            }
        }
    }
    if opened == 0 {
        return Err(if too_new > 0 {
            ImportError::ShellRsTooNew
        } else {
            not_shellrs("同步文件夹中没有能读的设备数据")
        });
    }
    let mut data = records.into_data();
    data.skipped += skipped + too_new;
    data.notes.push(PlanNote::SyncSnapshot);
    Ok(data)
}

/// 用同步密码解开各代数据密钥（generation → 密钥）。
fn unlock(
    vault: &Value,
    password: &SecretString,
) -> Result<HashMap<u32, Zeroizing<[u8; 32]>>, ImportError> {
    let format = vault.get("format").and_then(Value::as_u64).unwrap_or(0);
    if format > FORMAT {
        return Err(ImportError::ShellRsTooNew);
    }
    let vault_id = hex_field(vault, "vault_id", 16).ok_or_else(|| not_shellrs("vault_id"))?;
    let kdf = vault.get("kdf").ok_or_else(|| not_shellrs("kdf"))?;
    let number = |key: &str| {
        kdf.get(key)
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    };
    let (Some(memory), Some(passes), Some(lanes), Some(version)) = (
        number("memory_kib"),
        number("passes"),
        number("parallelism"),
        number("version"),
    ) else {
        return Err(not_shellrs("kdf 的参数"));
    };
    if kdf.get("algorithm").and_then(Value::as_str) != Some("argon2id") || version != 0x13 {
        return Err(not_shellrs("不认识的密钥派生算法"));
    }
    if memory > MAX_MEMORY_KIB
        || passes > MAX_PASSES
        || lanes > MAX_LANES
        || passes == 0
        || lanes == 0
    {
        return Err(not_shellrs("密钥派生的成本超出范围"));
    }
    let salt = hex_field(kdf, "salt", 16).ok_or_else(|| not_shellrs("salt"))?;
    let normalized = icu_normalizer::ComposingNormalizerBorrowed::new_nfc()
        .normalize(password.expose())
        .into_owned();
    let normalized = Zeroizing::new(normalized);
    let mut kek = Zeroizing::new([0u8; 32]);
    let params = argon2::Params::new(memory, passes, lanes, Some(32))
        .map_err(|_| not_shellrs("密钥派生的参数"))?;
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(normalized.as_bytes(), &salt, &mut kek[..])
        .map_err(|_| not_shellrs("密钥派生失败"))?;
    let cipher = XChaCha20Poly1305::new((&*kek).into());

    let entries = vault
        .get("keys")
        .and_then(Value::as_array)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| not_shellrs("keys"))?;
    let mut keys = HashMap::new();
    for entry in entries {
        let Some(generation) = entry
            .get("generation")
            .and_then(Value::as_u64)
            .and_then(|g| u32::try_from(g).ok())
        else {
            continue;
        };
        let (Some(nonce), Some(wrapped)) = (
            hex_field(entry, "nonce", 24),
            entry.get("wrapped").and_then(Value::as_str).and_then(hex),
        ) else {
            continue;
        };
        let Ok(nonce) = <&[u8; 24]>::try_from(nonce.as_slice()) else {
            continue;
        };
        let aad = [KEY_CONTEXT, &vault_id[..], &generation.to_be_bytes()].concat();
        let payload = Payload {
            msg: &wrapped,
            aad: &aad,
        };
        let Ok(plain) = cipher.decrypt(nonce.into(), payload) else {
            continue;
        };
        let plain = Zeroizing::new(plain);
        if let Ok(key) = <[u8; 32]>::try_from(plain.as_slice()) {
            keys.insert(generation, Zeroizing::new(key));
        }
    }
    if keys.is_empty() {
        return Err(ImportError::WrongPassword);
    }
    Ok(keys)
}

/// `devices` 中的设备文件（按名称排序）：名字不是恰好 `<16 个 [a-z0-9]>.srs` 的不算。
fn device_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let device = name.strip_suffix(".srs")?;
            let valid = device.len() == DEVICE_ID_LEN
                && device
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
            valid.then(|| (device.to_string(), e.path()))
        })
        .collect();
    files.sort();
    files
}

enum Snapshot {
    Records(Vec<Record>),
    /// 更新的版本写的，读不了
    TooNew,
}

/// 解开一个设备文件；失败时返回写给日志的原因。
fn open_device(
    file: &Path,
    device: &str,
    keys: &HashMap<u32, Zeroizing<[u8; 32]>>,
) -> Result<Snapshot, &'static str> {
    let raw = read_limited(file, DEVICE_MAX).map_err(|_| "读不了或超过 16 MiB")?;
    if raw.len() < HEADER_LEN + 16 || &raw[..4] != HEADER_MAGIC {
        return Err("不是设备文件");
    }
    if raw[4] != 1 {
        return Ok(Snapshot::TooNew);
    }
    let generation = u32::from_be_bytes(raw[21..25].try_into().map_err(|_| "头")?);
    let key = keys.get(&generation).ok_or("没有这一代的数据密钥")?;
    let aad = [&raw[..HEADER_LEN], device.as_bytes()].concat();
    let cipher = XChaCha20Poly1305::new((&**key).into());
    let payload = Payload {
        msg: &raw[HEADER_LEN..],
        aad: &aad,
    };
    let nonce: &[u8; 24] = raw[25..HEADER_LEN].try_into().map_err(|_| "头")?;
    let compressed = cipher
        .decrypt(nonce.into(), payload)
        .map_err(|_| "解不开（文件改过名或损坏）")?;
    let compressed = Zeroizing::new(compressed);
    let mut json = Zeroizing::new(Vec::new());
    flate2::read::DeflateDecoder::new(compressed.as_slice())
        .take(SNAPSHOT_MAX + 1)
        .read_to_end(&mut json)
        .map_err(|_| "解压失败")?;
    if json.len() as u64 > SNAPSHOT_MAX {
        return Err("快照超过 64 MiB");
    }
    let snapshot: Value = serde_json::from_slice(&json).map_err(|_| "快照不是 JSON")?;
    let number = |key: &str| snapshot.get(key).and_then(Value::as_u64);
    if number("format").unwrap_or(0) > FORMAT || number("min_reader").unwrap_or(1) > FORMAT {
        return Ok(Snapshot::TooNew);
    }
    let id = snapshot
        .get("device")
        .and_then(|d| d.get("id"))
        .and_then(Value::as_str);
    if id != Some(device) {
        return Err("快照中的设备与文件名不符");
    }
    let records = snapshot
        .get("records")
        .and_then(Value::as_array)
        .ok_or("快照中没有 records")?
        .iter()
        .filter_map(Record::parse)
        .collect();
    Ok(Snapshot::Records(records))
}

/// 一条记录（某台设备文件中的一份）。
struct Record {
    kind: String,
    id: String,
    /// （毫秒，计数，设备）：大的较新
    version: (u64, u64, String),
    /// 删除标记时为 `None`
    data: Option<Map<String, Value>>,
}

impl Record {
    fn parse(value: &Value) -> Option<Record> {
        let kind = value.get("kind")?.as_str()?.to_string();
        let id = value.get("id")?.as_str()?.to_string();
        let v = value.get("v")?.as_array()?;
        let version = (
            v.first()?.as_u64()?,
            v.get(1)?.as_u64()?,
            v.get(2)?.as_str()?.to_string(),
        );
        // 只有更新的版本才认得的记录：不用
        if value.get("min_reader").and_then(Value::as_u64).unwrap_or(1) > FORMAT {
            return None;
        }
        let data = if value.get("deleted").and_then(Value::as_bool) == Some(true) {
            None
        } else {
            Some(value.get("data")?.as_object()?.clone())
        };
        Some(Record {
            kind,
            id,
            version,
            data,
        })
    }
}

/// 合并各设备的记录：同一条取版本最大的一份。
#[derive(Default)]
struct Records {
    latest: HashMap<(String, String), Record>,
}

impl Records {
    fn merge(&mut self, list: Vec<Record>) {
        for record in list {
            let key = (record.kind.clone(), record.id.clone());
            match self.latest.get(&key) {
                Some(old) if old.version >= record.version => {}
                _ => {
                    self.latest.insert(key, record);
                }
            }
        }
    }

    /// 转成与数据库相同的数据（删除标记、缺字段的记录不要；秘密收进 [`super::Secrets`]）。
    fn into_data(self) -> Data {
        let mut data = Data::default();
        let mut live: Vec<(String, String, Map<String, Value>)> = self
            .latest
            .into_values()
            .filter_map(|r| Some((r.kind, r.id, r.data?)))
            .collect();
        // 顺序与结果无关；排一下让测试与日志稳定
        live.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        for (kind, id, d) in live {
            let ok = match kind.as_str() {
                "group" => group(&mut data, id, &d),
                "credential" => credential(&mut data, id, &d),
                "host" => host(&mut data, id, &d),
                "forward" => forward(&mut data, &d),
                "snippet_category" => category(&mut data, id, &d),
                "snippet" => snippet(&mut data, &d),
                kind if kind.starts_with("secret") => secret(&mut data, kind, id, &d),
                // 书签与不认识的种类
                _ => true,
            };
            if !ok {
                data.skipped += usize::from(kind == "host");
            }
        }
        data
    }
}

fn text<'a>(d: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    d.get(key)?.as_str()
}

fn int(d: &Map<String, Value>, key: &str) -> Option<i64> {
    d.get(key)?.as_i64()
}

fn group(data: &mut Data, id: String, d: &Map<String, Value>) -> bool {
    let Some(name) = text(d, "name").filter(|n| !n.trim().is_empty()) else {
        return false;
    };
    data.groups.insert(
        id,
        Group {
            name: name.to_string(),
            parent: text(d, "parent").map(String::from),
            sort_order: int(d, "sort_order").unwrap_or_default(),
        },
    );
    true
}

fn credential(data: &mut Data, id: String, d: &Map<String, Value>) -> bool {
    let Some(kind) = text(d, "kind").and_then(credential_kind) else {
        return false;
    };
    data.credentials.insert(
        id,
        Credential {
            name: text(d, "name").unwrap_or_default().to_string(),
            kind,
            username: text(d, "username").unwrap_or_default().to_string(),
            key_path: None,
        },
    );
    true
}

fn host(data: &mut Data, id: String, d: &Map<String, Value>) -> bool {
    let record = (|| {
        let protocol = protocol(text(d, "protocol").unwrap_or("ssh"))?;
        // 串口主机不同步；更新的版本即使加了，这里也没有它的参数
        if protocol == Protocol::Serial {
            return None;
        }
        let route = match text(d, "route").unwrap_or("direct") {
            "direct" => Route::Direct,
            "jump" => Route::Jump(
                d.get("jumps")?
                    .as_array()?
                    .iter()
                    .map(|j| j.as_str().map(String::from))
                    .collect(),
            ),
            "proxy" => {
                let p = d.get("proxy")?.as_object()?;
                Route::Proxy {
                    kind: proxy_kind(text(p, "kind")?)?,
                    host: text(p, "host")?.to_string(),
                    port: port_of(int(p, "port")?)?,
                    username: text(p, "username").unwrap_or_default().to_string(),
                }
            }
            _ => return None,
        };
        let telnet_prompts = d.get("telnet").and_then(Value::as_object).is_some_and(|t| {
            ["user_prompt", "pass_prompt"]
                .iter()
                .any(|k| text(t, k).is_some_and(|p| !p.trim().is_empty()))
        });
        let totp = d
            .get("totp")
            .and_then(Value::as_object)
            .map(|t| text(t, "prompt").unwrap_or_default().to_string());
        Some(HostRecord {
            key: id.clone(),
            name: text(d, "name").unwrap_or_default().to_string(),
            address: text(d, "address")?.to_string(),
            port: port_of(int(d, "port")?)?,
            group: text(d, "group").map(String::from),
            sort_order: int(d, "sort_order").unwrap_or_default(),
            protocol,
            auth: login(text(d, "auth")?, text(d, "credential").map(String::from))?,
            username: text(d, "username").unwrap_or_default().to_string(),
            credential_username: text(d, "credential_username")
                .filter(|u| !u.trim().is_empty())
                .map(String::from),
            route,
            notes: text(d, "notes").unwrap_or_default().to_string(),
            telnet_prompts: protocol == Protocol::Telnet && telnet_prompts,
            totp: totp.filter(|_| protocol == Protocol::Ssh),
            login_commands: text(d, "login_commands").unwrap_or_default().to_string(),
            serial: None,
            id: id.clone(),
        })
    })();
    match record {
        Some(record) => {
            data.hosts.push(record);
            true
        }
        None => false,
    }
}

fn forward(data: &mut Data, d: &Map<String, Value>) -> bool {
    let record = (|| {
        let kind = forward_kind(text(d, "kind")?)?;
        let (target_host, target_port) = match kind {
            ForwardKind::Dynamic => (String::new(), 0),
            _ => (
                text(d, "target_host")?.to_string(),
                port_of(int(d, "target_port")?)?,
            ),
        };
        Some(ForwardRecord {
            host: text(d, "host")?.to_string(),
            name: text(d, "name").unwrap_or_default().to_string(),
            kind,
            bind_host: text(d, "bind_host").unwrap_or_default().to_string(),
            bind_port: port_of(int(d, "bind_port")?)?,
            target_host,
            target_port,
            // 随 ShellRS 启动开启不同步
            auto_start: false,
            sort_order: int(d, "sort_order").unwrap_or_default(),
        })
    })();
    record.map(|r| data.forwards.push(r)).is_some()
}

fn category(data: &mut Data, id: String, d: &Map<String, Value>) -> bool {
    let Some(name) = text(d, "name").filter(|n| !n.trim().is_empty()) else {
        return false;
    };
    data.categories.insert(
        id,
        Category {
            name: name.to_string(),
            parent: text(d, "parent").map(String::from),
            sort_order: int(d, "sort_order").unwrap_or_default(),
        },
    );
    true
}

fn snippet(data: &mut Data, d: &Map<String, Value>) -> bool {
    let Some(command) = text(d, "command").filter(|c| !c.trim().is_empty()) else {
        return false;
    };
    data.snippets.push(SnippetRecord {
        category: text(d, "category").map(String::from),
        name: text(d, "name").unwrap_or_default().to_string(),
        command: command.to_string(),
        run_on_click: d
            .get("run_on_click")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        sort_order: int(d, "sort_order").unwrap_or_default(),
    });
    true
}

fn secret(data: &mut Data, kind: &str, id: String, d: &Map<String, Value>) -> bool {
    let Some(value) = text(d, "value").filter(|v| !v.is_empty()) else {
        return false;
    };
    let value = SecretString::new(value);
    let secrets = &mut data.secrets;
    let endpoint = || {
        Some(endpoint_key(
            text(d, "user").unwrap_or_default(),
            text(d, "host")?,
            port_of(int(d, "port")?)?,
        ))
    };
    match kind {
        "secret.password" => endpoint()
            .map(|k| secrets.passwords.insert(k, value))
            .is_some(),
        "secret.proxy" => endpoint()
            .map(|k| secrets.proxies.insert(k, value))
            .is_some(),
        "secret.credential" => {
            secrets.credential_passwords.insert(id, value);
            true
        }
        "secret.passphrase" => {
            secrets.passphrases.insert(id, value);
            true
        }
        "secret.private_key" => {
            secrets.private_keys.insert(id, value);
            true
        }
        "secret.totp" => {
            secrets.totp.insert(id, value);
            true
        }
        // 更新的版本加的秘密
        _ => true,
    }
}

fn hex_field(value: &Value, key: &str, len: usize) -> Option<Vec<u8>> {
    value
        .get(key)?
        .as_str()
        .and_then(hex)
        .filter(|b| b.len() == len)
}

/// 十六进制（大小写都认）。
fn hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digit = |c: u8| (c as char).to_digit(16);
            Some((digit(pair[0])? * 16 + digit(pair[1])?) as u8)
        })
        .collect()
}

/// 测试用：按格式写一个同步文件夹（成本取最小，数据密钥只有一代）。
#[cfg(test)]
pub(super) mod writer {
    use super::*;

    pub struct Vault {
        pub vault_id: [u8; 16],
        pub key: [u8; 32],
    }

    /// 在 `root/shellrs-sync/v1` 写 `vault.json`，返回数据密钥。
    pub fn vault(root: &Path, password: &str) -> Vault {
        let dir = root.join(STORE_DIR).join(VERSION_DIR);
        std::fs::create_dir_all(dir.join(DEVICES_DIR)).unwrap();
        let vault_id = [7u8; 16];
        let salt = [9u8; 16];
        let key = [42u8; 32];
        let params = argon2::Params::new(8, 1, 1, Some(32)).unwrap();
        let mut kek = [0u8; 32];
        argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
            .hash_password_into(password.as_bytes(), &salt, &mut kek)
            .unwrap();
        let nonce = [3u8; 24];
        let aad = [KEY_CONTEXT, &vault_id[..], &1u32.to_be_bytes()].concat();
        let wrapped = XChaCha20Poly1305::new((&kek).into())
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: &key,
                    aad: &aad,
                },
            )
            .unwrap();
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let json = serde_json::json!({
            "format": 1,
            "vault_id": hex(&vault_id),
            "kdf": {"algorithm": "argon2id", "version": 19, "memory_kib": 8, "passes": 1,
                    "parallelism": 1, "salt": hex(&salt)},
            "current": 1,
            "keys": [{"generation": 1, "nonce": hex(&nonce), "wrapped": hex(&wrapped)}],
        });
        std::fs::write(dir.join(VAULT_FILE), json.to_string()).unwrap();
        Vault { vault_id, key }
    }

    /// 写一台设备的快照（`records` 是记录的 JSON 数组）。
    pub fn device(root: &Path, vault: &Vault, device: &str, snapshot: &Value) {
        use std::io::Write as _;
        let mut deflate =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        deflate.write_all(snapshot.to_string().as_bytes()).unwrap();
        let compressed = deflate.finish().unwrap();
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(HEADER_MAGIC);
        header.push(1);
        header.extend_from_slice(&vault.vault_id);
        header.extend_from_slice(&1u32.to_be_bytes());
        header.extend_from_slice(&[5u8; 24]);
        let aad = [&header[..], device.as_bytes()].concat();
        let nonce: &[u8; 24] = header[25..HEADER_LEN].try_into().unwrap();
        let sealed = XChaCha20Poly1305::new((&vault.key).into())
            .encrypt(
                nonce.into(),
                Payload {
                    msg: &compressed,
                    aad: &aad,
                },
            )
            .unwrap();
        let path = root
            .join(STORE_DIR)
            .join(VERSION_DIR)
            .join(DEVICES_DIR)
            .join(format!("{device}.srs"));
        std::fs::write(path, [header, sealed].concat()).unwrap();
    }
}
