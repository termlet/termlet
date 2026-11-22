//! NyaTerm 的主机：本机的数据库（`nyaterm.redb`），或在 NyaTerm 设置中导出的加密完整备份（`.nya`）。
//!
//! 格式按 NyaTerm（Apache-2.0）提交 290212c 的源码（只看格式、不抄代码），主要是 `crates/nyaterm-store/src/storage/`
//! 与 `crates/nyaterm-core/src/models/`、`portable_snapshot/`。
//!
//! **本机数据库**：安装版在 `~/.nyaterm/nyaterm.redb`（预览版 `~/.nyaterm-preview`，Linux 兼容构建
//! `~/.nyaterm-compat`）；用户改过数据目录时 `location.json` 的 `data_dir` 指向它，环境变量 `NYATERM_DATA_DIR` 也可以
//! 指定；便携版在程序旁的 `data/config`，旁边有 `portable.key`。redb 数据库按实体分表，值是 JSON，键带前缀：
//!
//! ```text
//! groups        groups/<id>                         分组
//! connections   connections/<id>                    连接
//! credentials   credentials/key/<id>                私钥（私钥、证书、私钥密码是密文）
//!               credentials/password/<id>           账号（多台连接共用的用户名与密码）
//!               credentials/connection-password/<连接 id>   连接自己的密码（更早的版本写在连接的 auth.password 中）
//! proxies       proxies/<id>                        代理（新版本的密码是 nyaterm-proxy-secret-v4: 加密文）
//! tunnels       tunnels/<id>                        端口转发
//! otp_accounts  otp_accounts/<id>                   验证码
//! settings      settings/default                    设置（其中 security.master_password 是加密的主密码）
//!               settings/doc/quick-command          快捷命令
//! meta          security/master_key                 主密钥的包装（更早的在 text_docs 表的 master.key）
//! ```
//!
//! NyaTerm 运行时一直开着数据库，Windows 上别的进程连读这个文件都会失败（锁住了整个文件），所以要先退出
//! NyaTerm。整个文件读进内存后再打开（见 [`memory`]），不碰原文件。秘密的解密见 [`crypto`]：没有主密码时用
//! 用户主目录的路径（便携版用 `portable.key`）作材料，有主密码时主密码本身也用它加密保存在设置中，所以在本机上
//! 不需要用户输入；换了电脑或主目录改了名就解不开，主机照常导入、不带秘密。
//!
//! **`.nya` 备份**：外层用导出时输入的密码加密（[`crypto::open_snapshot`]），里面是 zip 中的 `snapshot.redb`：
//! `snapshot_meta` 表的 `meta`（`schema_version` 为 3），`entity_docs` 表是实体名 → JSON：`sessions`
//! （`{groups, connections}`）、`keys`（`{"keys": […]}`）、`passwords`（`{"passwords": […]}`）、`otp`
//! （`{"entries": […]}`）、`proxies`、`tunnels`、`quick_commands`、`master_key_token`（导出时改用文件密码包装）。
//! 连接与代理的密码在备份中是明文（外层已经加密），私钥、账号、验证码仍是主密钥加密的。
//!
//! **转换**：SSH、Telnet、串口连接导入，其余（本地终端、RDP、VNC、FTP、ADB）跳过并计数；分组按 `parent_id` 嵌套；
//! 密码来自连接自己或账号（账号的用户名、密码写进主机）；私钥存进密钥库（证书不导入，提醒）；跳板机、代理（SOCKS5、
//! HTTP、代理命令）、验证码、X11、编码、登录后执行的命令、自动录制、端口转发（导入后不自动启动）、备注、标签；
//! 快捷命令用 [`super::add_snippets`] 转换后随主机导入。agent 转发导入后关闭（提醒）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use redb::{ReadableDatabase, ReadableTable, TableDefinition, TableHandle};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::import::files::{LocalEnv, read_limited};
use crate::import::plan::{
    Endpoint, Hint, JumpRef, KeyAlgorithm, PlanBuilder, PlanNote, PlannedHost, PlannedKey,
    PlannedProfile, PlannedProfileAuth, clip, encoding_by_name,
};
use crate::import::{ImportError, ImportPlan, Source};
use crate::model::{
    Auth, FlowControl, Forward, ForwardKind, Host, Parity, Protocol, Proxy, ProxyKind, StopBits,
};
use crate::secret::SecretString;

mod crypto;
mod memory;
mod zip;

/// 本机数据库的文件名
const DATABASE_FILE: &str = "nyaterm.redb";
/// 用户改过数据目录时，原来的目录中指向新位置的文件
const LOCATION_FILE: &str = "location.json";
/// 便携版的密钥材料（与数据库在同一目录）
const PORTABLE_KEY: &str = "portable.key";
/// 指定数据目录的环境变量
const DATA_DIR_ENV: &str = "NYATERM_DATA_DIR";
/// 安装版的配置目录（正式版、预览版、Linux 兼容构建）
const CONFIG_DIRS: [&str; 3] = [".nyaterm", ".nyaterm-preview", ".nyaterm-compat"];
/// redb 文件的开头
const REDB_MAGIC: &[u8] = b"redb\x1A\x0A\xA9\x0D\x0A";
/// 文件大小的上限：本机数据库可能有很多命令历史；备份中的快照上限是 50 MiB
const MAX_DATABASE: u64 = 256 << 20;
const MAX_BACKUP: u64 = 128 << 20;
const MAX_SNAPSHOT: usize = 64 << 20;
/// 备份快照的格式版本（3 为完整备份，4 为同步用的快照）；2 是更早的、另一种表结构
const SNAPSHOT_VERSIONS: [u64; 2] = [3, 4];
/// 新版本中代理密码的密文前缀
const PROXY_SECRET_PREFIX: &str = "nyaterm-proxy-secret-v4:";
/// 分组的层数上限
const MAX_GROUP_DEPTH: usize = 32;
/// Windows 的 ERROR_LOCK_VIOLATION：文件被另一个进程锁住
const LOCK_VIOLATION: i32 = 33;

/// 读取 `path`：数据目录、其中的 `nyaterm.redb`，或 `.nya` 备份（要 `password`，没有时返回
/// [`ImportError::PasswordRequired`]）。
pub(in crate::import) fn read(
    path: &Path,
    env: &LocalEnv,
    password: Option<&SecretString>,
) -> Result<ImportPlan, ImportError> {
    let mut path = env.input_path(path);
    if path.is_dir() {
        path = database_in(&path).ok_or(ImportError::NyaTermNotFound)?;
    }
    let nya = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("nya"));
    let limit = if nya { MAX_BACKUP } else { MAX_DATABASE };
    let raw = read_limited(&path, limit).map_err(|e| match e {
        ImportError::Io { source, .. } if source.raw_os_error() == Some(LOCK_VIOLATION) => {
            ImportError::NyaTermInUse
        }
        e => e,
    })?;
    let raw = Zeroizing::new(raw);
    let material = local_material(&path, env);
    let material = material.as_ref().map(|m| m.as_slice());
    let entities = if raw.starts_with(REDB_MAGIC) {
        Entities::from_database(raw, material)?
    } else if !(nya || raw.starts_with(crypto::SNAPSHOT_MAGIC) || zip::is_zip(&raw)) {
        // 更早的没有文件头的备份只能按扩展名认
        return Err(not_data("不是 NyaTerm 的数据库或 .nya 备份"));
    } else {
        Entities::from_backup(&raw, password, material)?
    };
    Ok(entities.into_plan())
}

/// 默认位置：环境变量指定的数据目录，或安装版配置目录（含 `location.json` 指向的位置）中的数据库。
pub(in crate::import) fn default_path(env: &LocalEnv) -> Option<PathBuf> {
    let from_env = std::env::var_os(DATA_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    from_env
        .into_iter()
        .chain(
            CONFIG_DIRS
                .iter()
                .filter_map(|dir| env.home.as_ref().map(|h| h.join(dir))),
        )
        .find_map(|dir| database_in(&dir))
}

/// 目录中的数据库：直接在其中，或 `location.json` 指向的数据目录中。
fn database_in(dir: &Path) -> Option<PathBuf> {
    let direct = dir.join(DATABASE_FILE);
    if direct.is_file() {
        return Some(direct);
    }
    let location = std::fs::read(dir.join(LOCATION_FILE)).ok()?;
    let value: Value = serde_json::from_slice(&location).ok()?;
    let data_dir = value.get("data_dir")?.as_str()?;
    let file = Path::new(data_dir).join(DATABASE_FILE);
    file.is_file().then_some(file)
}

/// 本机数据没设主密码时的密钥材料：数据库旁有 `portable.key`（便携版）时是它的内容，否则是用户主目录的路径
/// （与 NyaTerm 取的 `dirs::home_dir()` 相同）。
fn local_material(path: &Path, env: &LocalEnv) -> Option<Zeroizing<Vec<u8>>> {
    let portable = path.parent().map(|d| d.join(PORTABLE_KEY));
    if let Some(text) = portable.and_then(|p| std::fs::read_to_string(p).ok()) {
        let text = Zeroizing::new(text);
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(Zeroizing::new(trimmed.as_bytes().to_vec()));
        }
    }
    let home = env.home.as_ref()?;
    Some(Zeroizing::new(home.to_string_lossy().as_bytes().to_vec()))
}

fn not_data(reason: impl std::fmt::Display) -> ImportError {
    ImportError::NotNyaTermData(reason.to_string())
}

type Object = Map<String, Value>;

/// 两种来源读出的实体（JSON 对象），转换时不再区分来源。
#[derive(Default)]
struct Entities {
    groups: Vec<Object>,
    connections: Vec<Object>,
    keys: HashMap<String, Object>,
    accounts: HashMap<String, Object>,
    /// 连接 ID → 单独存放的连接密码（密文；更早的版本可能是明文）
    stored_passwords: HashMap<String, String>,
    proxies: HashMap<String, Object>,
    tunnels: Vec<Object>,
    otp: HashMap<String, Object>,
    quick_commands: Option<Value>,
    master: Option<crypto::Key>,
    /// 有加密的秘密却解不开主密钥（数据来自另一台电脑、用户目录改过名）
    locked: bool,
}

impl Entities {
    /// 本机数据库。`material` 是没设主密码时的密钥材料。
    fn from_database(
        raw: Zeroizing<Vec<u8>>,
        material: Option<&[u8]>,
    ) -> Result<Self, ImportError> {
        let db = memory::open(raw).map_err(not_data)?;
        let txn = db.begin_read().map_err(not_data)?;
        let tables: HashSet<String> = txn
            .list_tables()
            .map_err(not_data)?
            .map(|t| t.name().to_string())
            .collect();
        if !tables.contains("connections") {
            return Err(not_data("没有 connections 表"));
        }
        // 表中键以 prefix 开头的 JSON 对象（ID 去掉前缀）
        let docs =
            |table: &'static str, prefix: &str| -> Result<Vec<(String, Object)>, ImportError> {
                if !tables.contains(table) {
                    return Ok(Vec::new());
                }
                let table = txn
                    .open_table(TableDefinition::<&str, &[u8]>::new(table))
                    .map_err(not_data)?;
                let mut out = Vec::new();
                for entry in table.iter().map_err(not_data)? {
                    let (key, value) = entry.map_err(not_data)?;
                    let Some(id) = key.value().strip_prefix(prefix) else {
                        continue;
                    };
                    if let Ok(Value::Object(o)) = serde_json::from_slice(value.value()) {
                        out.push((id.to_string(), o));
                    }
                }
                Ok(out)
            };
        let text = |table: &'static str, key: &str| -> Option<String> {
            if !tables.contains(table) {
                return None;
            }
            let table = txn
                .open_table(TableDefinition::<&str, &str>::new(table))
                .ok()?;
            let value = table.get(key).ok()??.value().to_string();
            Some(value).filter(|v| !v.trim().is_empty())
        };
        let doc = |key: &str| -> Option<Value> {
            if !tables.contains("settings") {
                return None;
            }
            let table = txn
                .open_table(TableDefinition::<&str, &[u8]>::new("settings"))
                .ok()?;
            serde_json::from_slice(table.get(key).ok()??.value()).ok()
        };
        let values = |items: Vec<(String, Object)>| items.into_iter().map(|(_, o)| o).collect();
        let by_id = |items: Vec<(String, Object)>| items.into_iter().collect();

        let mut entities = Entities {
            groups: values(docs("groups", "groups/")?),
            connections: values(docs("connections", "connections/")?),
            keys: by_id(docs("credentials", "credentials/key/")?),
            accounts: by_id(docs("credentials", "credentials/password/")?),
            stored_passwords: docs("credentials", "credentials/connection-password/")?
                .into_iter()
                .filter_map(|(id, o)| Some((id, field(&o, "password")?.to_string())))
                .collect(),
            proxies: by_id(docs("proxies", "proxies/")?),
            tunnels: values(docs("tunnels", "tunnels/")?),
            otp: by_id(docs("otp_accounts", "otp_accounts/")?),
            quick_commands: doc("settings/doc/quick-command"),
            master: None,
            locked: false,
        };
        let token = text("meta", "security/master_key").or_else(|| text("text_docs", "master.key"));
        if let Some(token) = token {
            entities.master = material.and_then(|fallback| {
                // 设了主密码时，主密码用没设主密码时的那把密钥加密保存在设置中
                let password = doc("settings/default")
                    .as_ref()
                    .and_then(|s| {
                        s.get("security")?
                            .get("master_password")?
                            .as_str()
                            .map(String::from)
                    })
                    .filter(|t| !t.trim().is_empty())
                    .and_then(|t| crypto::settings_secret(&t, fallback));
                let material = password.as_ref().map_or(fallback, |p| p.as_bytes());
                crypto::unwrap_master_key(&token, material)
            });
            entities.locked = entities.master.is_none();
        }
        Ok(entities)
    }

    /// `.nya` 备份（或没有加密的快照 zip）。`material` 用于没加密的快照：那时主密钥仍按本机的方式包装。
    fn from_backup(
        raw: &[u8],
        password: Option<&SecretString>,
        material: Option<&[u8]>,
    ) -> Result<Self, ImportError> {
        let (payload, material): (Zeroizing<Vec<u8>>, Option<Zeroizing<Vec<u8>>>) =
            if zip::is_zip(raw) {
                (
                    Zeroizing::new(raw.to_vec()),
                    material.map(|m| Zeroizing::new(m.to_vec())),
                )
            } else {
                let password = password.ok_or(ImportError::PasswordRequired)?;
                let payload =
                    crypto::open_snapshot(password.expose(), raw).map_err(|e| match e {
                        crypto::EnvelopeError::WrongPassword => ImportError::WrongPassword,
                        crypto::EnvelopeError::Unsupported => not_data("不认识的备份格式"),
                    })?;
                let material = Zeroizing::new(password.expose().trim().as_bytes().to_vec());
                (payload, Some(material))
            };
        let snapshot = if zip::is_zip(&payload) {
            zip::extract(&payload, "snapshot.redb", MAX_SNAPSHOT)
                .ok_or_else(|| not_data("备份中没有 snapshot.redb"))?
        } else {
            payload
        };
        if !snapshot.starts_with(REDB_MAGIC) {
            return Err(not_data("备份中不是 redb 数据库"));
        }
        let db = memory::open(snapshot).map_err(not_data)?;
        let txn = db.begin_read().map_err(not_data)?;
        let text = |table: &'static str, key: &str| -> Result<Option<String>, ImportError> {
            let table = txn
                .open_table(TableDefinition::<&str, &str>::new(table))
                .map_err(not_data)?;
            Ok(table
                .get(key)
                .map_err(not_data)?
                .map(|v| v.value().to_string()))
        };
        let meta: Value = text("snapshot_meta", "meta")?
            .and_then(|m| serde_json::from_str(&m).ok())
            .ok_or_else(|| not_data("没有快照的元数据"))?;
        let version = meta.get("schema_version").and_then(Value::as_u64);
        if !version.is_some_and(|v| SNAPSHOT_VERSIONS.contains(&v)) {
            return Err(not_data(format!("不支持的快照版本 {version:?}")));
        }
        let entity = |name: &str| -> Result<Option<Value>, ImportError> {
            Ok(text("entity_docs", name)?.and_then(|t| serde_json::from_str(&t).ok()))
        };
        let objects = |value: Option<&Value>| -> Vec<Object> {
            value
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_object().cloned())
                        .collect()
                })
                .unwrap_or_default()
        };
        let by_id = |items: Vec<Object>| -> HashMap<String, Object> {
            items
                .into_iter()
                .filter_map(|o| Some((field(&o, "id")?.to_string(), o)))
                .collect()
        };
        let wrapped = |name: &str, inner: &str| -> Result<Vec<Object>, ImportError> {
            Ok(objects(entity(name)?.as_ref().and_then(|v| v.get(inner))))
        };
        let sessions = entity("sessions")?.unwrap_or_default();
        let connections = sessions
            .get("connections")
            .or_else(|| sessions.get("sessions"));
        let mut entities = Entities {
            groups: objects(sessions.get("groups")),
            connections: objects(connections),
            keys: by_id(wrapped("keys", "keys")?),
            accounts: by_id(wrapped("passwords", "passwords")?),
            stored_passwords: HashMap::new(),
            proxies: by_id(objects(entity("proxies")?.as_ref())),
            tunnels: objects(entity("tunnels")?.as_ref()),
            otp: by_id(wrapped("otp", "entries")?),
            quick_commands: entity("quick_commands")?,
            master: None,
            locked: false,
        };
        let token = entity("master_key_token")?;
        if let Some(token) = token.as_ref().and_then(Value::as_str) {
            entities.master = material.and_then(|m| crypto::unwrap_master_key(token, &m));
            entities.locked = entities.master.is_none();
        }
        Ok(entities)
    }

    /// 解开主密钥加密的秘密。
    fn decrypt(&self, token: &str) -> Option<SecretString> {
        crypto::decrypt_secret(self.master.as_ref()?, token)
    }

    fn into_plan(self) -> ImportPlan {
        Converter::new(&self).run()
    }
}

/// 去掉首尾空白后不为空的字符串字段。
fn field<'a>(o: &'a Object, key: &str) -> Option<&'a str> {
    o.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn flag(o: &Object, key: &str) -> bool {
    o.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn number(o: &Object, key: &str) -> Option<u64> {
    o.get(key).and_then(Value::as_u64)
}

/// 把实体转成导入计划。
struct Converter<'a> {
    e: &'a Entities,
    plan: PlanBuilder,
    /// 分组 ID → （名称，上级 ID）
    groups: HashMap<&'a str, (&'a str, Option<&'a str>)>,
    /// 私钥 ID → 密钥库中的下标（解不开时为 `None`）
    keys: HashMap<String, Option<usize>>,
    /// 私钥指纹 → 下标（同一把私钥只存一次）
    fingerprints: HashMap<String, usize>,
    /// 连接 ID → 计划中的下标（端口转发用）
    hosts: HashMap<String, usize>,
    /// 账号 ID → 凭据档案的下标（同一个账号只建一个档案）
    profiles: HashMap<String, usize>,
}

impl<'a> Converter<'a> {
    fn new(e: &'a Entities) -> Self {
        let groups = e
            .groups
            .iter()
            .filter_map(|g| {
                Some((
                    field(g, "id")?,
                    (field(g, "name").unwrap_or_default(), field(g, "parent_id")),
                ))
            })
            .collect();
        Self {
            e,
            plan: PlanBuilder::new(Source::NyaTermData),
            groups,
            keys: HashMap::new(),
            fingerprints: HashMap::new(),
            hosts: HashMap::new(),
            profiles: HashMap::new(),
        }
    }

    fn run(mut self) -> ImportPlan {
        let e = self.e;
        // 同一分组中按 NyaTerm 的顺序
        let mut connections: Vec<&Object> = e.connections.iter().collect();
        connections.sort_by_key(|c| {
            (
                c.get("sort_order").and_then(Value::as_i64).unwrap_or(0),
                field(c, "name").unwrap_or_default().to_string(),
            )
        });
        // 跳板机按终点（地址、端口、实际的用户名）对应
        let endpoints: HashMap<&str, (String, Endpoint)> = connections
            .iter()
            .filter(|c| field(c, "type") == Some("ssh"))
            .filter_map(|c| {
                let name = field(c, "name").unwrap_or_default().to_string();
                let endpoint = Endpoint::new(field(c, "host")?, port(c, 22), &self.username(c));
                Some((field(c, "id")?, (name, endpoint)))
            })
            .collect();
        for c in connections {
            match self.host(c, &endpoints) {
                Some(item) => {
                    let id = field(c, "id").map(String::from);
                    let index = self.plan.push(item);
                    if let Some(id) = id {
                        self.hosts.insert(id, index);
                    }
                }
                None => self.plan.skip(1),
            }
        }
        self.forwards();
        if let Some(commands) = e.quick_commands.clone() {
            // 结构不对的快捷命令文档不影响主机的导入
            let _ = super::add_snippets(&mut self.plan, commands);
        }
        self.plan.finish()
    }

    /// 分组从根开始的各级名称；上级不存在、成环或超过层数时停在那里。
    fn group_path(&self, id: Option<&str>) -> Vec<&'a str> {
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        let mut current = id;
        while let Some(id) = current {
            if !seen.insert(id) || names.len() >= MAX_GROUP_DEPTH {
                break;
            }
            let Some(&(name, parent)) = self.groups.get(id) else {
                break;
            };
            names.push(name);
            current = parent;
        }
        names.reverse();
        names
    }

    /// 连接用的账号（`account_id`，旧版本的 `password_id`）。
    fn account(&self, c: &Object) -> Option<&'a Object> {
        let e = self.e;
        let auth = c.get("auth")?.as_object()?;
        let id = field(auth, "account_id").or_else(|| field(auth, "password_id"))?;
        e.accounts.get(id)
    }

    /// 实际登录用的用户名：跟随账号时（没有写 `username_source: connection`）取账号的用户名（不为空时）。
    fn username(&self, c: &Object) -> String {
        let own = field(c, "username").unwrap_or_default();
        let follows_account = c
            .get("auth")
            .and_then(Value::as_object)
            .is_none_or(|a| field(a, "username_source") != Some("connection"));
        match self.account(c).and_then(|a| field(a, "username")) {
            Some(account) if follows_account => account.to_string(),
            _ => own.to_string(),
        }
    }

    /// 密码用账号的时为那个账号（ID 与内容）：写明 `password_source: account`，或没写而连接自己没有密码时
    /// （与 NyaTerm 相同；连接自己的密码可能单独存放）。
    fn password_account(&self, c: &Object) -> Option<(&'a str, &'a Object)> {
        let e = self.e;
        let auth = c.get("auth")?.as_object()?;
        let own = field(auth, "password").is_some()
            || field(c, "id").is_some_and(|id| e.stored_passwords.contains_key(id));
        let uses_account = match field(auth, "password_source") {
            Some("account") => true,
            Some("connection") => false,
            _ => !own,
        };
        if !uses_account {
            return None;
        }
        let id = field(auth, "account_id").or_else(|| field(auth, "password_id"))?;
        e.accounts
            .get_key_value(id)
            .map(|(id, account)| (id.as_str(), account))
    }

    /// 连接的登录密码（用账号的时为账号的密码）；有密文却解不开时为 `Err`。
    fn password(&self, c: &Object) -> Result<Option<SecretString>, ()> {
        let Some(auth) = c.get("auth").and_then(Value::as_object) else {
            return Ok(None);
        };
        if let Some((_, account)) = self.password_account(c) {
            return match field(account, "password") {
                Some(token) => self.e.decrypt(token).map(Some).ok_or(()),
                None => Ok(None),
            };
        }
        let inline = field(auth, "password");
        if let Some(value) = inline {
            // has_password 为真时是主密钥加密的密文；为假时是明文（备份中解开过的、或更早的版本）
            return if flag(auth, "has_password") {
                self.e.decrypt(value).map(Some).ok_or(())
            } else {
                Ok(Some(SecretString::new(value)))
            };
        }
        let stored = field(c, "id").and_then(|id| self.e.stored_passwords.get(id));
        match stored {
            Some(value) => match self.e.decrypt(value) {
                Some(password) => Ok(Some(password)),
                None if !crypto::looks_encrypted(value) => {
                    Ok(Some(SecretString::new(value.as_str())))
                }
                None => Err(()),
            },
            None => Ok(None),
        }
    }

    /// 一个连接；不支持的类型、没有地址时为 `None`（跳过并计数）。
    fn host(
        &mut self,
        c: &Object,
        endpoints: &HashMap<&str, (String, Endpoint)>,
    ) -> Option<PlannedHost> {
        let protocol = match field(c, "type")? {
            "ssh" => Protocol::Ssh,
            "telnet" => Protocol::Telnet,
            "serial" => Protocol::Serial,
            _ => return None,
        };
        let address = match protocol {
            Protocol::Serial => field(c, "port_name")?,
            _ => field(c, "host")?,
        }
        .to_string();
        let tags = c
            .get("tags")
            .and_then(Value::as_array)
            .map(|t| {
                t.iter()
                    .filter_map(Value::as_str)
                    .map(|s| clip(s.trim(), 64))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let mut host = Host {
            name: field(c, "name").unwrap_or(&address).to_string(),
            protocol,
            port: match protocol {
                Protocol::Ssh => port(c, 22),
                Protocol::Telnet => port(c, 23),
                _ => 0,
            },
            address,
            note: clip(field(c, "description").unwrap_or_default(), 2000),
            tags,
            ..Default::default()
        };
        if protocol != Protocol::Serial {
            host.username = self.username(c);
        }
        let key = field(c, "id").unwrap_or(&host.name).to_string();
        let mut item = PlannedHost::new(key, host);
        let path = self.group_path(field(c, "group_id"));
        item.group = self.plan.group(&path);
        if let Some(encoding) = field(c, "encoding") {
            item.set_encoding(encoding_by_name(encoding), encoding);
        }
        match protocol {
            Protocol::Ssh => self.ssh(c, &mut item, endpoints),
            Protocol::Telnet => self.telnet(c, &mut item),
            _ => serial(c, &mut item),
        }
        Some(item)
    }

    fn ssh(
        &mut self,
        c: &Object,
        item: &mut PlannedHost,
        endpoints: &HashMap<&str, (String, Endpoint)>,
    ) {
        let auth = c.get("auth").and_then(Value::as_object);
        let mode = auth.and_then(|a| field(a, "mode")).unwrap_or("password");
        match mode {
            "key" => self.key(auth.and_then(|a| field(a, "key_id")), item),
            "agent" => item.host.auth = Auth::Agent,
            "password" => match self.password_account(c) {
                // 账号导入成凭据档案，多台主机共用；跟随账号的用户名时主机自己的用户名留空
                Some((id, account)) => {
                    let index = self.account_profile(id, account, item);
                    let account_user = field(account, "username");
                    let follows =
                        auth.is_none_or(|a| field(a, "username_source") != Some("connection"));
                    if follows && account_user.is_some() {
                        item.host.username.clear();
                    }
                    item.set_profile(index, account_user.unwrap_or_default());
                }
                None => match self.password(c) {
                    Ok(Some(password)) => item.set_password(password),
                    Ok(None) => {}
                    Err(()) => self.unreadable(item),
                },
            },
            // none：不保存，连接时询问
            _ => {}
        }
        if let Some(otp) = auth.and_then(|a| field(a, "otp_id")) {
            self.totp(otp, item);
        }
        item.host.ssh.x11_forward = flag(c, "x11_forwarding");
        let agent_forward = c
            .get("agent_forwarding_config")
            .and_then(Value::as_object)
            .is_some_and(|a| flag(a, "enabled"))
            || flag(c, "agent_forwarding");
        if agent_forward {
            item.hints.push(Hint::AgentForwardDisabled);
        }
        if let Some(post) = c.get("post_login").and_then(Value::as_object)
            && flag(post, "enabled")
            && let Some(command) = field(post, "command")
        {
            item.host.terminal.startup = command.to_string();
        }
        if let Some(recording) = c.get("recording").and_then(Value::as_object) {
            item.host.terminal.record = flag(recording, "auto_start");
        }
        let network = c.get("network").and_then(Value::as_object);
        if let Some(jump) = network.and_then(|n| field(n, "proxy_jump_id")) {
            match endpoints.get(jump) {
                Some((name, endpoint)) => {
                    item.jump = Some(JumpRef::new(name.clone(), endpoint.clone()));
                }
                None => item.hints.push(Hint::JumpNotFound),
            }
        } else if let Some(proxy) = network.and_then(|n| field(n, "proxy_id")) {
            self.proxy(proxy, item);
        }
    }

    fn telnet(&mut self, c: &Object, item: &mut PlannedHost) {
        // 关了自动登录时 Termlet 不应自动填写用户名
        let auto_login = c
            .get("auto_login")
            .and_then(Value::as_object)
            .is_none_or(|a| a.get("enabled").and_then(Value::as_bool).unwrap_or(true));
        if !auto_login {
            item.host.username.clear();
            return;
        }
        // Termlet 的 Telnet 只用主机自己的用户名与密码：账号的写进主机
        match self.password(c) {
            Ok(Some(password)) => item.set_password(password),
            Ok(None) => {}
            Err(()) => self.unreadable(item),
        }
    }

    /// 账号对应的凭据档案：名称、用户名取账号的，密码解开后一并导入。
    fn account_profile(&mut self, id: &str, account: &Object, item: &mut PlannedHost) -> usize {
        if let Some(&index) = self.profiles.get(id) {
            return index;
        }
        let username = field(account, "username").unwrap_or_default();
        let name = field(account, "name").unwrap_or(username);
        let mut profile = PlannedProfile::new(name, username, PlannedProfileAuth::Password);
        if let Some(token) = field(account, "password") {
            match self.e.decrypt(token) {
                Some(password) => profile.set_password(password),
                None => self.unreadable(item),
            }
        }
        let index = self.plan.add_profile(profile);
        self.profiles.insert(id.to_string(), index);
        index
    }

    /// 保存的密码解不开：整个数据都解不开时在计划中说明一次，否则提醒这台主机。
    fn unreadable(&mut self, item: &mut PlannedHost) {
        if self.e.locked {
            self.plan.note(PlanNote::SecretsUnreadable);
        } else {
            item.hints.push(Hint::SavedPasswordUnreadable);
        }
    }

    /// 私钥认证：解开密钥库中的私钥（连同私钥密码），转成不加密的 OpenSSH 格式存进 Termlet 的密钥库。
    fn key(&mut self, id: Option<&str>, item: &mut PlannedHost) {
        let e = self.e;
        let Some((id, key)) = id.and_then(|id| Some((id, e.keys.get(id)?))) else {
            item.hints.push(Hint::LibraryKeyMissing);
            return;
        };
        let name = field(key, "name").unwrap_or(id).to_string();
        if field(key, "cert").is_some() {
            item.hints
                .push(Hint::CertificateNotImported { name: name.clone() });
        }
        let index = match self.keys.get(id) {
            Some(&index) => index,
            None => {
                let index = self.library_key(key, &name);
                self.keys.insert(id.to_string(), index);
                index
            }
        };
        match index {
            Some(index) => item.set_key(index),
            None => item.hints.push(Hint::LibraryKeyUnusable { name }),
        }
    }

    fn library_key(&mut self, key: &Object, name: &str) -> Option<usize> {
        let private_key = self.e.decrypt(field(key, "key")?)?;
        let passphrase = field(key, "passphrase").and_then(|p| self.e.decrypt(p));
        let parsed =
            crate::sshkey::parse(private_key.expose().as_bytes(), passphrase.as_ref()).ok()?;
        let pem = parsed.to_openssh().ok()?;
        let fingerprint = parsed.fingerprint();
        if let Some(&index) = self.fingerprints.get(&fingerprint) {
            return Some(index);
        }
        let algorithm = match parsed.algorithm() {
            "ssh-ed25519" => KeyAlgorithm::Ed25519,
            a if a.starts_with("ecdsa") => KeyAlgorithm::Ecdsa,
            _ => KeyAlgorithm::Rsa,
        };
        let index = self
            .plan
            .add_key(PlannedKey::new(name.to_string(), algorithm, pem));
        self.fingerprints.insert(fingerprint, index);
        Some(index)
    }

    /// 验证码：TOTP 的密钥（参数不是默认值时写成 `otpauth://` 链接）存为主机的验证码密钥。
    fn totp(&self, id: &str, item: &mut PlannedHost) {
        let secret = self.e.otp.get(id).and_then(|otp| {
            let kind = field(otp, "otp_type").unwrap_or("totp");
            if !kind.eq_ignore_ascii_case("totp") {
                return None;
            }
            let secret = self.e.decrypt(field(otp, "secret")?)?;
            let algorithm = field(otp, "algorithm").unwrap_or("SHA1").to_ascii_uppercase();
            let digits = number(otp, "digits").unwrap_or(6);
            let period = number(otp, "period").unwrap_or(30);
            let value = if algorithm == "SHA1" && digits == 6 && period == 30 {
                secret
            } else {
                SecretString::new(format!(
                    "otpauth://totp/NyaTerm?secret={}&algorithm={algorithm}&digits={digits}&period={period}",
                    secret.expose().trim()
                ))
            };
            crate::totp::Totp::parse(value.expose()).ok()?;
            Some(value)
        });
        match secret {
            Some(secret) => item.set_totp(secret),
            None => item.hints.push(Hint::TotpNotImported),
        }
    }

    fn proxy(&self, id: &str, item: &mut PlannedHost) {
        let Some(p) = self.e.proxies.get(id) else {
            return;
        };
        let kind = match field(p, "protocol").unwrap_or("socks5") {
            "socks5" => ProxyKind::Socks5,
            "http" => ProxyKind::Http,
            "proxycommand" => ProxyKind::Command,
            _ => {
                item.hints.push(Hint::UnsupportedProxy);
                return;
            }
        };
        let mut proxy = Proxy {
            kind,
            ..Default::default()
        };
        if kind == ProxyKind::Command {
            let Some(command) = field(p, "command") else {
                item.hints.push(Hint::UnsupportedProxy);
                return;
            };
            proxy.command = command.to_string();
            item.hints.push(Hint::ProxyCommandImported);
        } else {
            let (Some(address), Some(port)) = (field(p, "host"), number(p, "port")) else {
                item.hints.push(Hint::UnsupportedProxy);
                return;
            };
            proxy.address = address.to_string();
            proxy.port = u16::try_from(port).unwrap_or(1080);
            proxy.username = field(p, "username").unwrap_or_default().to_string();
            // 密码：明文、加了前缀的密文，或引用的账号
            let password = match field(p, "password") {
                Some(value) => match value.strip_prefix(PROXY_SECRET_PREFIX) {
                    Some(token) => self.e.decrypt(token),
                    None => Some(SecretString::new(value)),
                },
                None => field(p, "password_id")
                    .and_then(|a| self.e.accounts.get(a))
                    .and_then(|a| field(a, "password"))
                    .and_then(|t| self.e.decrypt(t)),
            };
            if !proxy.username.is_empty() {
                match password {
                    Some(password) => item.set_proxy_password(password),
                    None => item.hints.push(Hint::ProxyPasswordRequired),
                }
            }
        }
        item.host.proxy = Some(proxy);
    }

    /// 端口转发：加到经由的主机上，导入后不自动启动。
    fn forwards(&mut self) {
        for t in &self.e.tunnels {
            let Some(&index) = field(t, "connection_id").and_then(|id| self.hosts.get(id)) else {
                continue;
            };
            let kind = match field(t, "tunnel_type").unwrap_or("local") {
                "local" => ForwardKind::Local,
                "remote" => ForwardKind::Remote,
                "dynamic" => ForwardKind::Dynamic,
                _ => continue,
            };
            let Some(bind_port) = number(t, "listen_port")
                .and_then(|p| u16::try_from(p).ok())
                .filter(|&p| p > 0)
            else {
                continue;
            };
            let (target_host, target_port) = if kind == ForwardKind::Dynamic {
                (String::new(), 0)
            } else {
                let host = field(t, "target_host").unwrap_or("localhost").to_string();
                let Some(port) = number(t, "target_port")
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|&p| p > 0)
                else {
                    continue;
                };
                (host, port)
            };
            let local_only = t
                .get("bind_localhost")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let item = self.plan.host_mut(index);
            if item.host.protocol != Protocol::Ssh {
                continue;
            }
            item.forwards.push(Forward {
                kind,
                name: clip(field(t, "name").unwrap_or_default(), 64),
                bind_host: if local_only { "127.0.0.1" } else { "0.0.0.0" }.into(),
                bind_port,
                target_host,
                target_port,
                ..Default::default()
            });
        }
    }
}

fn port(c: &Object, default: u16) -> u16 {
    number(c, "port")
        .and_then(|p| u16::try_from(p).ok())
        .filter(|&p| p > 0)
        .unwrap_or(default)
}

fn serial(c: &Object, item: &mut PlannedHost) {
    let serial = &mut item.host.serial;
    if let Some(baud) = number(c, "baud_rate").and_then(|b| u32::try_from(b).ok()) {
        serial.baud = baud;
    }
    if let Some(bits) = number(c, "data_bits").and_then(|b| u8::try_from(b).ok()) {
        serial.data_bits = bits;
    }
    serial.parity = match field(c, "parity").unwrap_or("none") {
        "odd" => Parity::Odd,
        "even" => Parity::Even,
        "mark" => Parity::Mark,
        "space" => Parity::Space,
        _ => Parity::None,
    };
    serial.stop_bits = match field(c, "stop_bits").unwrap_or("1") {
        "1.5" => StopBits::OnePointFive,
        "2" => StopBits::Two,
        _ => StopBits::One,
    };
    serial.flow_control = match field(c, "flow_control").unwrap_or("none") {
        "hardware" => FlowControl::RtsCts,
        "software" => FlowControl::XonXoff,
        _ => FlowControl::None,
    };
}

#[cfg(test)]
mod tests;
