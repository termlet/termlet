//! Electerm（MIT 许可的开源 SSH 客户端）的书签。
//!
//! 两种来源：
//! - 书签导出：书签列表上导出的 `bookmarks-<时间>.json`，或“导出全部数据”的 `<时间>-electerm-all-data.json`，
//!   都是 `{"bookmarkGroups": [...], "bookmarks": [...]}`（后者另有 `profiles` 等）；也认只有书签的数组与单个书签。
//!   其中的密码、私钥是明文。默认位置是“下载”“桌面”“文档”中最新的这种文件。
//! - 本机的数据目录 `<配置目录>/electerm/users/default_user`（便携版在程序目录下的 `electerm/users/default_user`）：
//!   2.x 起是 SQLite 的 `electerm.db`（每个表两列：`_id` 与 JSON 的 `data`），更早的版本与旧系统的构建是 NeDB
//!   （`electerm.<表>.nedb`，每行一个 JSON，同一 `_id` 以最后一行为准）。3.0 起书签与凭据档案整条用系统的加密
//!   （Electron safeStorage，`enc:v2:safe:` 开头）保存：Termlet 不去解（那是窃取浏览器密码的做法），这样的书签跳过并
//!   计数，全都读不了时返回 [`ImportError::ElectermEncrypted`]，请用户先在 Electerm 中导出书签。系统加密不可用时
//!   Electerm 写成 `enc:` 加明文 JSON，照常读。3.0 以前把登录密码按字符移位保存（`passwordEncrypted`，没有密钥），这里还原。
//!   数据库在 Electerm 运行时也在用：复制一份再读。没有默认位置的数据目录时由用户选择。
//!
//! 映射：
//! - 分组：`bookmarkGroups` 的 `bookmarkIds`、`bookmarkGroupIds` 组成的树；ID 为 `default` 的默认分组是根（不建分组）。
//! - 类型：`type` 缺省为 ssh；导入 ssh、telnet、serial，rdp、vnc、web、ftp、本机终端等跳过并计数。
//! - SSH：名称、地址、端口、用户名、备注（description）；登录方式按 `authType`：密码、私钥（`privateKey` 是私钥内容，
//!   连同 `passphrase` 解开后存进密钥库；只有 `privateKeyPath` 时用私钥文件）、凭据档案（`profiles`，档案在“导出全部数据”
//!   与本机数据中有，书签导出中没有时提醒）；编码（encode）、X11 转发；起始目录（startDirectory）与登录后执行的脚本
//!   （runScripts）成为登录后执行的命令；代理（`socks5://`、`socks4://`、`http://`、`https://` 的 URL，或旧版的对象）、
//!   代理命令（proxyCommand）；端口转发（sshTunnels，导入后不自动启动；远程的 SOCKS 转发不支持）。
//! - 跳板机（connectionHoppings）写在书签里（每一跳有自己的地址与凭据），不引用别的书签：这里按地址、端口、用户名找
//!   同一文件中的主机，找不到时为它新建一台（提醒“为谁的跳板机新建”），多级时新建的跳板机依次相连。`hasHopping` 为假的
//!   旧书签中书签自己的地址才是第一跳，顺序不同，不导入跳板机并提醒。
//! - Telnet：地址、端口、用户名与密码（自动登录用；书签用凭据档案时取档案的 telnet 部分）。串口：端口、波特率、
//!   数据位、停止位、校验、流控（rtscts、xon）。
//! - 没有导入：颜色、图标、终端外观、环境变量、快捷命令、触发器、算法设置等；开着的 agent 转发导入后关闭并提醒。
//!
//! 秘密都要用户明确勾选“导入保存的密码”后才导入（[`Source::secrets_need_consent`]）。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};

use crate::hosttools::sh_quote;
use crate::model::{
    Auth, DEFAULT_BIND_HOST, FlowControl, Forward, ForwardKind, Host, KeySource, Parity, Protocol,
    Proxy, ProxyKind, SERIAL_BAUD_RANGE, SerialOptions, StopBits,
};
use crate::secret::SecretString;

use super::files::{LocalEnv, MAX_CONFIG_SIZE, TempCopy, decode_text, read_limited};
use super::plan::{
    Endpoint, Hint, JumpRef, KeyAlgorithm, PlanBuilder, PlannedHost, PlannedKey, clip,
    encoding_by_name,
};
use super::{ImportError, ImportPlan, Source};

/// 本机数据的 SQLite 文件。
const SQLITE_FILE: &str = "electerm.db";
/// 现行版本只有这一个用户目录（约 1.34 版以前可以有别的）。
const DEFAULT_USER: &str = "default_user";
/// 默认分组的 ID：分组树的根，不建分组。
const DEFAULT_GROUP: &str = "default";
/// 数据库文件的大小上限（含命令历史等，比单个配置文件大）。
const MAX_DB_SIZE: u64 = 256 << 20;
/// 分组的层数、登录后执行的命令的行数与每行长度、端口转发的条数上限。
const MAX_GROUP_DEPTH: usize = 32;
const MAX_STARTUP_LINES: usize = 20;
const MAX_STARTUP_LINE: usize = 1000;
const MAX_FORWARDS: usize = 100;

type Record = Map<String, Value>;

pub(super) fn read(path: &Path, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let path = env.input_path(path);
    let data = match locate(&path)? {
        Location::Export(file) => {
            let raw = read_limited(&file, MAX_CONFIG_SIZE)?;
            parse_export(&decode_text(&raw, encoding_rs::UTF_8))?
        }
        Location::Sqlite(file) => read_sqlite(&file)?,
        Location::Nedb { dir, suffix } => read_nedb(&dir, suffix)?,
    };
    if data.bookmarks.is_empty() && data.encrypted > 0 {
        return Err(ImportError::ElectermEncrypted);
    }
    Ok(plan(data, env))
}

/// 默认位置：“下载”“桌面”“文档”中最新的书签导出；没有时为本机的数据目录（有 electerm.db 或 NeDB 文件时）。
pub(super) fn default_path(env: &LocalEnv) -> Option<PathBuf> {
    newest_export(env).or_else(|| {
        let dir = env
            .config_dir
            .as_ref()?
            .join("electerm")
            .join("users")
            .join(DEFAULT_USER);
        (dir.join(SQLITE_FILE).is_file() || dir.join("electerm.bookmarks.nedb").is_file())
            .then_some(dir)
    })
}

/// 书签导出的文件名：`bookmarks-<时间>.json` 或 `<时间>-electerm-all-data.json`。
fn is_export_name(name: &str) -> bool {
    let name = name.to_lowercase();
    name.ends_with(".json")
        && (name.starts_with("bookmarks-") || name.contains("electerm-all-data"))
}

fn newest_export(env: &LocalEnv) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for dir in [&env.downloads, &env.desktop, &env.documents]
        .into_iter()
        .flatten()
    {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            if !is_export_name(&entry.file_name().to_string_lossy()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if meta.is_file() && best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, entry.path()));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// 用户选择的位置是什么。
#[derive(Debug, PartialEq, Eq)]
enum Location {
    /// 书签导出（JSON）
    Export(PathBuf),
    Sqlite(PathBuf),
    /// NeDB 文件所在的目录；`suffix` 为 `.bak` 时是迁移到 SQLite 后留下的备份
    Nedb {
        dir: PathBuf,
        suffix: &'static str,
    },
}

/// 文件按扩展名区分；目录可以是用户目录（`users/default_user`）、数据目录（含 `users`）或它的上级。
fn locate(p: &Path) -> Result<Location, ImportError> {
    let meta = fs::metadata(p).map_err(|source| ImportError::Io {
        path: p.to_path_buf(),
        source,
    })?;
    if !meta.is_dir() {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let dir = || p.parent().unwrap_or(Path::new(".")).to_path_buf();
        return Ok(if name.ends_with(".db") {
            Location::Sqlite(p.to_path_buf())
        } else if name.ends_with(".nedb") {
            Location::Nedb {
                dir: dir(),
                suffix: "",
            }
        } else if name.ends_with(".nedb.bak") {
            Location::Nedb {
                dir: dir(),
                suffix: ".bak",
            }
        } else {
            Location::Export(p.to_path_buf())
        });
    }
    let mut candidates = vec![
        p.to_path_buf(),
        p.join("users").join(DEFAULT_USER),
        p.join("electerm").join("users").join(DEFAULT_USER),
    ];
    if let Ok(entries) = fs::read_dir(p.join("users")) {
        let mut others: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|d| d.is_dir())
            .collect();
        others.sort();
        candidates.extend(others);
    }
    for dir in candidates {
        if dir.join(SQLITE_FILE).is_file() {
            return Ok(Location::Sqlite(dir.join(SQLITE_FILE)));
        }
        for suffix in ["", ".bak"] {
            if dir
                .join(format!("electerm.bookmarks.nedb{suffix}"))
                .is_file()
            {
                return Ok(Location::Nedb { dir, suffix });
            }
        }
    }
    Err(ImportError::NotElecterm(format!(
        "{} 中没有 {SQLITE_FILE}",
        p.display()
    )))
}

/// 读出的书签、分组与凭据档案。
#[derive(Debug, Default)]
struct Data {
    bookmarks: Vec<Record>,
    groups: Vec<Record>,
    profiles: Vec<Record>,
    /// 用系统加密保存、读不了的书签数
    encrypted: usize,
}

impl Data {
    /// 加入本机数据中的一条记录：ID 在行上，内容里没有。
    fn add(&mut self, table: Table, id: String, decoded: Decoded) {
        match decoded {
            Decoded::Plain(mut record) => {
                record.insert("id".into(), Value::String(id));
                match table {
                    Table::Bookmarks => self.bookmarks.push(record),
                    Table::Groups => self.groups.push(record),
                    Table::Profiles => self.profiles.push(record),
                }
            }
            // 读不了的凭据档案：用它的书签会提醒
            Decoded::Encrypted if table == Table::Bookmarks => self.encrypted += 1,
            Decoded::Encrypted => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Table {
    Bookmarks,
    Groups,
    Profiles,
}

impl Table {
    const ALL: [Table; 3] = [Table::Bookmarks, Table::Groups, Table::Profiles];

    fn name(self) -> &'static str {
        match self {
            Table::Bookmarks => "bookmarks",
            Table::Groups => "bookmarkGroups",
            Table::Profiles => "profiles",
        }
    }
}

/// 一条记录的内容。
#[derive(Debug)]
enum Decoded {
    Plain(Record),
    /// 用系统加密保存的（`enc:v2:safe:`），读不了
    Encrypted,
}

/// 本机数据中一条记录的内容：JSON、`enc:` 加 JSON（系统加密不可用时），或系统加密的密文；都不是时为 `None`。
fn decode_record(raw: &str) -> Option<Decoded> {
    let raw = raw.trim();
    let json = match raw.strip_prefix("enc:") {
        Some(rest) if rest.trim_start().starts_with('{') => rest,
        Some(_) => return Some(Decoded::Encrypted),
        None => raw,
    };
    match serde_json::from_str::<Value>(json) {
        Ok(Value::Object(record)) => Some(Decoded::Plain(record)),
        _ => None,
    }
}

/// 书签导出：`{bookmarks, bookmarkGroups, profiles?}`、书签的数组，或单个书签。
fn parse_export(text: &str) -> Result<Data, ImportError> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| ImportError::NotElecterm(e.to_string()))?;
    let records = |v: Option<&Value>| -> Vec<Record> {
        v.and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|x| x.as_object().cloned())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut data = Data::default();
    match &value {
        Value::Array(_) => data.bookmarks = records(Some(&value)),
        Value::Object(obj)
            if obj.contains_key("bookmarks") || obj.contains_key("bookmarkGroups") =>
        {
            data.bookmarks = records(obj.get("bookmarks"));
            data.groups = records(obj.get("bookmarkGroups"));
            data.profiles = records(obj.get("profiles"));
        }
        Value::Object(obj) if obj.contains_key("host") || obj.contains_key("path") => {
            data.bookmarks = vec![obj.clone()];
        }
        _ => return Err(ImportError::NotElecterm("文件中没有书签".into())),
    }
    Ok(data)
}

/// SQLite（2.x 起）：每个表两列，`_id` 与 `data`。
fn read_sqlite(file: &Path) -> Result<Data, ImportError> {
    let copy = TempCopy::of(file, MAX_DB_SIZE)?;
    let not_electerm = |e: rusqlite::Error| ImportError::NotElecterm(e.to_string());
    let conn = Connection::open_with_flags(
        copy.file(),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(not_electerm)?;
    let mut data = Data::default();
    for table in Table::ALL {
        let exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table.name()],
                |row| row.get(0),
            )
            .map_err(not_electerm)?;
        if exists == 0 {
            continue;
        }
        let mut stmt = conn
            .prepare(&format!("SELECT _id, data FROM `{}`", table.name()))
            .map_err(not_electerm)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(not_electerm)?;
        for row in rows {
            let (id, raw) = row.map_err(not_electerm)?;
            if let Some(decoded) = raw.as_deref().and_then(decode_record) {
                data.add(table, id, decoded);
            }
        }
    }
    Ok(data)
}

/// NeDB（2.x 以前与旧系统的构建）：`electerm.<表>.nedb<suffix>`。
fn read_nedb(dir: &Path, suffix: &str) -> Result<Data, ImportError> {
    let mut data = Data::default();
    for table in Table::ALL {
        let file = dir.join(format!("electerm.{}.nedb{suffix}", table.name()));
        if !file.is_file() {
            continue;
        }
        let raw = read_limited(&file, MAX_DB_SIZE)?;
        for (id, decoded) in nedb_records(&decode_text(&raw, encoding_rs::UTF_8)) {
            data.add(table, id, decoded);
        }
    }
    Ok(data)
}

/// NeDB 文件：每行一个 JSON，只追加；同一 `_id` 以最后一行为准，`$$deleted` 为删除，索引的行忽略。
/// 3.0 起加密的文档内容在 `_encdata` 中（旁边可能残留更新前的明文字段，不用）。
fn nedb_records(text: &str) -> Vec<(String, Decoded)> {
    let mut order: Vec<String> = Vec::new();
    let mut docs: HashMap<String, Option<Decoded>> = HashMap::new();
    for line in text.lines() {
        let Ok(Value::Object(mut doc)) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if doc.contains_key("$$indexCreated") || doc.contains_key("$$indexRemoved") {
            continue;
        }
        let Some(id) = doc.get("_id").map(text_of).filter(|id| !id.is_empty()) else {
            continue;
        };
        let decoded = if doc.get("$$deleted") == Some(&Value::Bool(true)) {
            None
        } else {
            match doc.remove("_encdata") {
                Some(Value::String(enc)) => decode_record(&enc),
                Some(_) => None,
                None => {
                    doc.remove("_id");
                    Some(Decoded::Plain(doc))
                }
            }
        };
        if !docs.contains_key(&id) {
            order.push(id.clone());
        }
        docs.insert(id, decoded);
    }
    order
        .into_iter()
        .filter_map(|id| docs.remove(&id).flatten().map(|d| (id, d)))
        .collect()
}

// ---------------------------------------------------------------- 字段

/// 字符串或数字写成文本（去掉首尾空白）；其他为空。
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn text(r: &Record, key: &str) -> String {
    r.get(key).map(text_of).unwrap_or_default()
}

/// 原样的字符串（密码、私钥不去空白）。
fn raw(r: &Record, key: &str) -> String {
    r.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn flag(r: &Record, key: &str) -> bool {
    r.get(key) == Some(&Value::Bool(true))
}

fn port_of(r: &Record, key: &str) -> Option<u16> {
    text(r, key).parse::<u16>().ok().filter(|&p| p > 0)
}

/// 3.0 以前的登录密码按 UTF-16 码元逐个移位保存：第 i 个（从 0 起）加了 i + 1。
fn unshift(s: &str) -> String {
    let units: Vec<u16> = s
        .encode_utf16()
        .enumerate()
        .map(|(i, u)| u.wrapping_sub((i as u16).wrapping_add(1)))
        .collect();
    String::from_utf16_lossy(&units)
}

/// 登录密码：`passwordEncrypted` 时还原移位。
fn password_of(r: &Record) -> String {
    let password = raw(r, "password");
    if flag(r, "passwordEncrypted") {
        unshift(&password)
    } else {
        password
    }
}

/// 书签（或一跳跳板机）的凭据。
#[derive(Default)]
struct Creds {
    username: String,
    password: String,
    /// 私钥内容
    private_key: String,
    private_key_path: String,
    passphrase: String,
}

impl Creds {
    fn of(r: &Record) -> Self {
        Self {
            username: text(r, "username"),
            password: password_of(r),
            private_key: raw(r, "privateKey"),
            private_key_path: text(r, "privateKeyPath"),
            passphrase: raw(r, "passphrase"),
        }
    }

    /// 用凭据档案的（有值的）覆盖。
    fn overlay(&mut self, profile: &Creds) {
        let take = |mine: &mut String, theirs: &String| {
            if !theirs.is_empty() {
                *mine = theirs.clone();
            }
        };
        take(&mut self.username, &profile.username);
        take(&mut self.password, &profile.password);
        take(&mut self.private_key, &profile.private_key);
        take(&mut self.private_key_path, &profile.private_key_path);
        take(&mut self.passphrase, &profile.passphrase);
    }

    fn has_key(&self) -> bool {
        !self.private_key.trim().is_empty() || !self.private_key_path.is_empty()
    }
}

// ---------------------------------------------------------------- 计划

/// 分组树：书签所在的分组、分组的上级。
struct Groups {
    title: HashMap<String, String>,
    parent: HashMap<String, String>,
    of_bookmark: HashMap<String, String>,
}

impl Groups {
    fn new(groups: &[Record]) -> Self {
        let mut tree = Self {
            title: HashMap::new(),
            parent: HashMap::new(),
            of_bookmark: HashMap::new(),
        };
        for g in groups {
            let id = text(g, "id");
            if id.is_empty() {
                continue;
            }
            tree.title.insert(id.clone(), text(g, "title"));
            let ids = |key: &str| -> Vec<String> {
                g.get(key)
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(text_of).filter(|s| !s.is_empty()).collect())
                    .unwrap_or_default()
            };
            for child in ids("bookmarkGroupIds") {
                tree.parent.entry(child).or_insert_with(|| id.clone());
            }
            for bookmark in ids("bookmarkIds") {
                tree.of_bookmark
                    .entry(bookmark)
                    .or_insert_with(|| id.clone());
            }
        }
        tree
    }

    /// 书签所在分组从根开始的各级名称（默认分组是根，不算；有环时到重复处为止）。
    fn path(&self, bookmark: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        let mut current = self.of_bookmark.get(bookmark);
        while let Some(id) = current {
            if id == DEFAULT_GROUP || !seen.insert(id) || names.len() >= MAX_GROUP_DEPTH {
                break;
            }
            names.push(self.title.get(id).cloned().unwrap_or_default());
            current = self.parent.get(id);
        }
        names.reverse();
        names
    }
}

/// 待建立的跳板机（全部书签加入后再处理：跳板机可能是排在后面的书签）。
struct HopWork {
    target: usize,
    session: String,
    group: Option<usize>,
    hops: Vec<Record>,
}

struct Builder<'a> {
    plan: PlanBuilder,
    env: &'a LocalEnv,
    profiles: HashMap<String, &'a Record>,
    /// 已加入的私钥（按指纹）
    keys: HashMap<String, usize>,
}

fn plan(data: Data, env: &LocalEnv) -> ImportPlan {
    let groups = Groups::new(&data.groups);
    let mut b = Builder {
        plan: PlanBuilder::new(Source::Electerm),
        env,
        profiles: data
            .profiles
            .iter()
            .map(|p| (text(p, "id"), p))
            .filter(|(id, _)| !id.is_empty())
            .collect(),
        keys: HashMap::new(),
    };
    b.plan.skip(data.encrypted);
    let mut hops = Vec::new();
    for bookmark in &data.bookmarks {
        if let Some(work) = b.add_bookmark(bookmark, &groups) {
            hops.push(work);
        }
    }
    for work in hops {
        b.link_hops(work);
    }
    b.plan.finish()
}

impl Builder<'_> {
    /// 加入一个书签；有跳板机时返回待处理的跳板机。
    fn add_bookmark(&mut self, r: &Record, groups: &Groups) -> Option<HopWork> {
        let protocol = match text(r, "type").to_ascii_lowercase().as_str() {
            "" | "ssh" => Protocol::Ssh,
            "telnet" => Protocol::Telnet,
            "serial" => Protocol::Serial,
            // rdp、vnc、web、ftp、spice、本机终端
            _ => {
                self.plan.skip(1);
                return None;
            }
        };
        let address = if protocol == Protocol::Serial {
            text(r, "path")
        } else {
            text(r, "host")
        };
        if address.is_empty() {
            self.plan.skip(1);
            return None;
        }
        let title = text(r, "title");
        let name = clip(if title.is_empty() { &address } else { &title }, 128);
        let id = text(r, "id");
        let host = Host {
            name: name.clone(),
            protocol,
            address,
            port: if protocol == Protocol::Serial {
                0
            } else {
                port_of(r, "port").unwrap_or(0)
            },
            note: clip(&text(r, "description"), 2000),
            ..Default::default()
        };
        let mut item = PlannedHost::new(
            if id.is_empty() {
                name.clone()
            } else {
                id.clone()
            },
            host,
        );
        item.group = self.plan.group(&groups.path(&id));
        let mut creds = Creds::of(r);
        match protocol {
            Protocol::Ssh => {
                let auth_type = text(r, "authType");
                if auth_type == "profiles" {
                    match self.profile(r) {
                        Some(profile) => creds.overlay(&Creds::of(profile)),
                        None => item.hints.push(Hint::CredentialProfileMissing),
                    }
                }
                item.host.username = creds.username.clone();
                let use_key =
                    auth_type == "privateKey" || (creds.password.is_empty() && creds.has_key());
                self.set_ssh_credentials(&mut item, &creds, use_key, &name);
                self.set_ssh_options(&mut item, r);
                if item.host.username.is_empty() {
                    item.hints.push(Hint::MissingUsername);
                }
            }
            Protocol::Telnet => {
                // 用凭据档案时取档案的 telnet 部分
                if let Some(telnet) = self
                    .profile(r)
                    .and_then(|p| p.get("telnet"))
                    .and_then(Value::as_object)
                {
                    creds.overlay(&Creds::of(telnet));
                }
                item.host.username = creds.username.clone();
                item.set_password(SecretString::new(creds.password));
            }
            Protocol::Serial => item.host.serial = serial_options(r),
        }
        let hops: Vec<Record> = r
            .get("connectionHoppings")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|h| h.as_object().cloned()).collect())
            .unwrap_or_default();
        let group = item.group;
        let target = self.plan.push(item);
        if protocol != Protocol::Ssh || hops.is_empty() {
            return None;
        }
        // 旧书签（没有 hasHopping）中书签自己的地址才是第一跳，最后一跳是目标
        if !flag(r, "hasHopping") {
            self.plan.host_mut(target).hints.push(Hint::JumpNotImported);
            return None;
        }
        Some(HopWork {
            target,
            session: name,
            group,
            hops,
        })
    }

    /// 书签引用的凭据档案（`profile`）。
    fn profile(&self, r: &Record) -> Option<&Record> {
        self.profiles.get(text(r, "profile").as_str()).copied()
    }

    /// SSH 的登录方式：私钥（内容存进密钥库，或私钥文件）或密码。
    fn set_ssh_credentials(
        &mut self,
        item: &mut PlannedHost,
        creds: &Creds,
        use_key: bool,
        name: &str,
    ) {
        if !use_key {
            item.set_password(SecretString::new(creds.password.clone()));
            return;
        }
        if !creds.private_key.trim().is_empty() {
            // 私钥内容（有密码时用它解开）转成不加密的 OpenSSH 格式，存进密钥库；同一把只存一次
            let passphrase =
                Some(SecretString::new(creds.passphrase.clone())).filter(|p| !p.is_empty());
            let parsed = crate::sshkey::parse(creds.private_key.as_bytes(), passphrase.as_ref())
                .ok()
                .and_then(|key| key.to_openssh().ok().map(|pem| (key, pem)));
            if let Some((key, private_key)) = parsed {
                let fingerprint = key.fingerprint();
                let index = match self.keys.get(&fingerprint) {
                    Some(&index) => index,
                    None => {
                        let algorithm = match key.algorithm() {
                            "ssh-ed25519" => KeyAlgorithm::Ed25519,
                            a if a.starts_with("ecdsa") => KeyAlgorithm::Ecdsa,
                            _ => KeyAlgorithm::Rsa,
                        };
                        let index = self.plan.add_key(PlannedKey::new(
                            name.to_string(),
                            algorithm,
                            private_key,
                        ));
                        self.keys.insert(fingerprint, index);
                        index
                    }
                };
                item.set_key(index);
                return;
            }
            // 解不开（格式不支持、密码不对）时有私钥文件就用文件
            if creds.private_key_path.is_empty() {
                item.hints.push(Hint::LibraryKeyUnusable {
                    name: name.to_string(),
                });
                return;
            }
        }
        if creds.private_key_path.is_empty() {
            item.hints.push(Hint::PrivateKeyFileMissing);
            return;
        }
        item.host.auth = Auth::Key {
            source: KeySource::file(self.env.expand_home(&creds.private_key_path)),
        };
        item.set_passphrase(SecretString::new(creds.passphrase.clone()));
    }

    /// SSH 的其余设置：编码、X11、agent 转发、登录后执行的命令、代理、端口转发。
    fn set_ssh_options(&self, item: &mut PlannedHost, r: &Record) {
        let encode = text(r, "encode");
        if !encode.is_empty() {
            item.set_encoding(encoding_by_name(&encode), &encode);
        }
        item.host.ssh.x11_forward = flag(r, "x11");
        if flag(r, "agentForward") {
            item.hints.push(Hint::AgentForwardDisabled);
        }
        item.host.terminal.startup = startup_lines(r).join("\n");
        add_proxy(item, r);
        add_forwards(item, r);
    }

    /// 为跳板机找到或新建主机，设置目标的跳板机（见模块说明）。
    fn link_hops(&mut self, work: HopWork) {
        let mut previous: Option<(String, Endpoint)> = None;
        let mut chain = Vec::new();
        let mut last_created = false;
        for hop in &work.hops {
            let mut creds = Creds::of(hop);
            if let Some(profile) = self.profile(hop) {
                creds.overlay(&Creds::of(profile));
            }
            let address = text(hop, "host");
            let port = port_of(hop, "port").unwrap_or(22);
            let label = match port {
                22 => format!("{}@{address}", creds.username),
                port => format!("{}@{address}:{port}", creds.username),
            };
            let target = self.plan.host_mut(work.target);
            if address.is_empty() {
                target.hints.push(Hint::JumpNotImported);
                return;
            }
            if creds.username.is_empty() {
                target
                    .hints
                    .push(Hint::JumpMissingUsername { name: address });
                return;
            }
            let endpoint = Endpoint::new(&address, port, &creds.username);
            chain.push(label.clone());
            let known = self
                .plan
                .hosts()
                .iter()
                .any(|h| h.host.protocol == Protocol::Ssh && h.endpoint() == endpoint);
            last_created = !known;
            if !known {
                let host = Host {
                    name: clip(&address, 128),
                    address,
                    port,
                    username: creds.username.clone(),
                    ..Default::default()
                };
                let mut item = PlannedHost::new(format!("hop:{label}"), host);
                item.group = work.group;
                item.hints.push(Hint::JumpHostFor {
                    session: work.session.clone(),
                });
                let use_key = creds.password.is_empty() && creds.has_key();
                self.set_ssh_credentials(&mut item, &creds, use_key, &label);
                if let Some((name, endpoint)) = &previous {
                    item.jump = Some(JumpRef::new(name.clone(), endpoint.clone()));
                }
                self.plan.push(item);
            }
            previous = Some((label, endpoint));
        }
        let Some((name, endpoint)) = previous else {
            return;
        };
        let target = self.plan.host_mut(work.target);
        target.jump = Some(JumpRef::new(name, endpoint));
        // 最后一跳是已有的书签：它自己的跳板设置可能与这里的不同
        if chain.len() > 1 && !last_created {
            target.hints.push(Hint::MultiHopJump { chain });
        }
    }
}

/// 登录后执行的命令：起始目录（`cd`），再是登录脚本（runScripts，旧版的 loginScript）。
fn startup_lines(r: &Record) -> Vec<String> {
    let mut lines = Vec::new();
    let mut dir = text(r, "startDirectory");
    if dir.is_empty() {
        dir = text(r, "startDirectoryRemote");
    }
    if !dir.is_empty() {
        lines.push(format!("cd {}", sh_quote(&dir)));
    }
    let mut scripts: Vec<String> = r
        .get("runScripts")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_object)
                .map(|s| raw(s, "script"))
                .collect()
        })
        .unwrap_or_default();
    scripts.push(raw(r, "loginScript"));
    for script in scripts {
        lines.extend(
            script
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| clip(l, MAX_STARTUP_LINE)),
        );
    }
    lines.truncate(MAX_STARTUP_LINES);
    lines
}

/// 代理：URL（`socks5://user:pass@host:port` 等，用户名、密码 Electerm 原样使用）或旧版的对象；没有时看代理命令。
fn add_proxy(item: &mut PlannedHost, r: &Record) {
    let parsed = match r.get("proxy") {
        Some(Value::String(url)) if !url.trim().is_empty() => proxy_url(url.trim()),
        Some(Value::Object(old)) => legacy_proxy(old),
        _ => None,
    };
    match parsed {
        Some(Ok((proxy, password, hint))) => {
            if !proxy.username.is_empty() {
                item.set_proxy_password(SecretString::new(password));
            }
            item.host.proxy = Some(proxy);
            item.hints.extend(hint);
        }
        Some(Err(())) => item.hints.push(Hint::UnsupportedProxy),
        None => {
            let command = text(r, "proxyCommand");
            if !command.is_empty() {
                item.host.proxy = Some(Proxy {
                    kind: ProxyKind::Command,
                    command,
                    ..Default::default()
                });
                item.hints.push(Hint::ProxyCommandImported);
            }
        }
    }
}

type ParsedProxy = Result<(Proxy, String, Option<Hint>), ()>;

/// 代理的类型与没写端口时的默认端口。
fn proxy_kind(scheme: &str) -> Option<(ProxyKind, u16)> {
    match scheme.to_ascii_lowercase().as_str() {
        "socks5" | "socks5h" | "socks" => Some((ProxyKind::Socks5, 1080)),
        "socks4" | "socks4a" => Some((ProxyKind::Socks4, 1080)),
        "http" => Some((ProxyKind::Http, 8080)),
        "https" => Some((ProxyKind::Https, 443)),
        _ => None,
    }
}

fn proxy_url(url: &str) -> Option<ParsedProxy> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Some(Err(()));
    };
    let Some((kind, default_port)) = proxy_kind(scheme) else {
        return Some(Err(()));
    };
    let rest = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (user_info, host_port) = match rest.rsplit_once('@') {
        Some((user_info, host_port)) => (user_info, host_port),
        None => ("", rest),
    };
    let (address, port) = match super::ssh_config::split_host_port(host_port) {
        Some((host, port)) => (host, super::ssh_config::parse_port(port)),
        None => (
            host_port
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host_port),
            None,
        ),
    };
    if address.is_empty() {
        return Some(Err(()));
    }
    let (username, password) = user_info.split_once(':').unwrap_or((user_info, ""));
    Some(Ok(proxy_with(
        kind,
        address,
        port,
        default_port,
        username,
        password,
    )))
}

/// 旧版的代理对象：`proxyType` 5 / 4 为 SOCKS，0 为 HTTP，1 为 HTTPS。
fn legacy_proxy(old: &Record) -> Option<ParsedProxy> {
    let address = text(old, "proxyIp");
    if address.is_empty() {
        return None;
    }
    let scheme = match text(old, "proxyType").as_str() {
        "5" => "socks5",
        "4" => "socks4",
        "0" => "http",
        "1" => "https",
        _ => return Some(Err(())),
    };
    let (kind, default_port) = proxy_kind(scheme)?;
    Some(Ok(proxy_with(
        kind,
        &address,
        port_of(old, "proxyPort"),
        default_port,
        &text(old, "proxyUsername"),
        &raw(old, "proxyPassword"),
    )))
}

fn proxy_with(
    kind: ProxyKind,
    address: &str,
    port: Option<u16>,
    default_port: u16,
    username: &str,
    password: &str,
) -> (Proxy, String, Option<Hint>) {
    let hint = port
        .is_none()
        .then_some(Hint::ProxyPortDefaulted { port: default_port });
    let proxy = Proxy {
        kind,
        address: address.to_string(),
        port: port.unwrap_or(default_port),
        username: username.to_string(),
        ..Default::default()
    };
    (proxy, password.to_string(), hint)
}

/// 端口转发（sshTunnels）：本地（-L）、远程（-R）、本机的 SOCKS（-D）；服务器上的 SOCKS（remoteDynamicForward）
/// 与缺少端口的不导入，记为提醒。地址缺省为 127.0.0.1。
fn add_forwards(item: &mut PlannedHost, r: &Record) {
    let Some(tunnels) = r.get("sshTunnels").and_then(Value::as_array) else {
        return;
    };
    let mut skipped = 0;
    for t in tunnels
        .iter()
        .filter_map(Value::as_object)
        .take(MAX_FORWARDS)
    {
        let address = |key: &str| match text(t, key) {
            a if a.is_empty() => DEFAULT_BIND_HOST.to_string(),
            a => a,
        };
        let local = (
            address("sshTunnelLocalHost"),
            port_of(t, "sshTunnelLocalPort"),
        );
        let remote = (
            address("sshTunnelRemoteHost"),
            port_of(t, "sshTunnelRemotePort"),
        );
        let (kind, (bind_host, bind_port), (target_host, target_port)) =
            match text(t, "sshTunnel").as_str() {
                "forwardLocalToRemote" => (ForwardKind::Local, local, remote),
                "forwardRemoteToLocal" => (ForwardKind::Remote, remote, local),
                "dynamicForward" => (ForwardKind::Dynamic, local, (String::new(), Some(0))),
                _ => {
                    skipped += 1;
                    continue;
                }
            };
        let (Some(bind_port), Some(target_port)) = (bind_port, target_port) else {
            skipped += 1;
            continue;
        };
        item.forwards.push(Forward {
            kind,
            name: clip(&text(t, "name"), 64),
            bind_host,
            bind_port,
            target_host,
            target_port,
            ..Default::default()
        });
    }
    if skipped > 0 {
        item.hints
            .push(Hint::ForwardsNotImported { count: skipped });
    }
}

/// 串口参数：波特率、数据位、停止位、校验，流控按 rtscts（硬件）与 xon（软件）。
fn serial_options(r: &Record) -> SerialOptions {
    let defaults = SerialOptions::default();
    let number = |key: &str| text(r, key).parse::<f64>().ok();
    SerialOptions {
        baud: number("baudRate")
            .map(|b| b as u32)
            .filter(|b| SERIAL_BAUD_RANGE.contains(b))
            .unwrap_or(defaults.baud),
        data_bits: number("dataBits")
            .map(|b| b as u8)
            .filter(|b| (5..=8).contains(b))
            .unwrap_or(defaults.data_bits),
        stop_bits: match text(r, "stopBits").as_str() {
            "1.5" => StopBits::OnePointFive,
            "2" => StopBits::Two,
            _ => StopBits::One,
        },
        parity: match text(r, "parity").to_ascii_lowercase().as_str() {
            "odd" => Parity::Odd,
            "even" => Parity::Even,
            "mark" => Parity::Mark,
            "space" => Parity::Space,
            _ => Parity::None,
        },
        flow_control: if flag(r, "rtscts") {
            FlowControl::RtsCts
        } else if flag(r, "xon") {
            FlowControl::XonXoff
        } else {
            FlowControl::None
        },
        ..defaults
    }
}

#[cfg(test)]
mod tests;
