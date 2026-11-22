//! ShellRS 数据目录中的 `shellrs.db`（SQLite，WAL）与 `settings.json`。
//!
//! 结构按 `crates/shellrs-hosts/src/database.rs`（提交 98d471c），版本在 `PRAGMA user_version`：13 是第一个发布的
//! 结构，发布版本中 0.1.16–0.1.17 是 20、0.1.18 是 22、0.1.19 是 24，之后是 25。各版本只在表末尾加列，改约束时重建表
//! 但列名不变，所以先看有哪些列、按列名读，较老版本没有的列取默认值：
//!
//! ```text
//! groups              id, parent_id, name, sort_order
//! credentials         id, name, kind (password | key | agent), username, key_path
//! hosts               id, public_id, group_id, sort_order, name, address, port, auth (password | no-password |
//!                     credential), username（用凭据时为 NULL）, credential_id, route (direct | jump | proxy),
//!                     proxy_kind (http | socks5), proxy_host, proxy_port, proxy_username, notes,
//!                     protocol (ssh | telnet | serial；15、16 起), serial_baud, serial_data_bits,
//!                     serial_parity (none | odd | even), serial_stop_bits (1 | 2), serial_flow (none | rts-cts |
//!                     xon-xoff), telnet_user_prompt, telnet_pass_prompt（16 起）, credential_username（23 起）,
//!                     one_time_code, one_time_code_prompt（24 起）, login_commands（25 起）
//! host_jumps          host_id, position, jump_id（NULL 是已删除的主机）
//! forwards            host_id, name, kind (local | remote | dynamic), bind_host, bind_port, target_host,
//!                     target_port, auto_start, sort_order
//! snippet_categories  id, name, parent_id, sort_order（14 起；后两列 21 起）
//! snippets            category_id, name, command, run_on_click, sort_order（14 起；sort_order 21 起）
//! ```
//!
//! 秘密都不在这里（在系统钥匙串中）。ShellRS 运行时一直开着数据库：复制一份（连同 `-wal`）再读。
//! `settings.json` 的 `sync.location` 是同步位置（`{"kind": "folder", "path": …}`，或 WebDAV、S3），用来提示改选
//! 同步文件夹。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, Row};
use serde_json::Value;

use super::{
    Category, Credential, Data, ForwardRecord, Group, HostRecord, Route, SnippetRecord,
    credential_kind, forward_kind, login, port_of, protocol, proxy_kind, serial_options,
};
use crate::import::ImportError;
use crate::import::files::{TempCopy, read_limited};
use crate::import::plan::PlanNote;
use crate::model::{ForwardKind, Protocol};

/// 数据库的文件名
pub(super) const DATABASE_FILE: &str = "shellrs.db";
/// 设置文件（与数据库在同一目录）
const SETTINGS_FILE: &str = "settings.json";
const MAX_DATABASE: u64 = 256 << 20;
const MAX_SETTINGS: u64 = 4 << 20;

/// 选的路径中的数据库：目录中的 `shellrs.db`，或直接选的文件。
pub(super) fn locate(path: &Path) -> Option<PathBuf> {
    if path.is_dir() {
        let file = path.join(DATABASE_FILE);
        return file.is_file().then_some(file);
    }
    path.is_file().then(|| path.to_path_buf())
}

fn not_shellrs(reason: impl std::fmt::Display) -> ImportError {
    ImportError::NotShellRs(reason.to_string())
}

pub(super) fn read(file: &Path) -> Result<Data, ImportError> {
    let copy = TempCopy::of(file, MAX_DATABASE)?;
    let conn = Connection::open_with_flags(
        copy.file(),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(not_shellrs)?;
    let mut data = Reader::new(&conn)?.read().map_err(not_shellrs)?;
    data.notes.push(PlanNote::SecretsInKeychain);
    if let Some(note) = file
        .parent()
        .and_then(|dir| sync_note(&dir.join(SETTINGS_FILE)))
    {
        data.notes.push(note);
    }
    Ok(data)
}

/// 一行主机，原样读出（取值的含义在 [`RawHost::into_record`] 中判断）。
struct RawHost {
    id: i64,
    public_id: Option<String>,
    group: Option<i64>,
    sort_order: Option<i64>,
    name: Option<String>,
    address: Option<String>,
    port: Option<i64>,
    protocol: Option<String>,
    auth: Option<String>,
    username: Option<String>,
    credential: Option<i64>,
    credential_username: Option<String>,
    route: Option<String>,
    proxy_kind: Option<String>,
    proxy_host: Option<String>,
    proxy_port: Option<i64>,
    proxy_username: Option<String>,
    notes: Option<String>,
    serial: [Option<String>; 5],
    telnet_prompts: [Option<String>; 2],
    one_time_code: Option<i64>,
    one_time_code_prompt: Option<String>,
    login_commands: Option<String>,
}

/// 按位置读的主机列（[`RawHost`] 的顺序）。
const HOST_COLUMNS: [&str; 28] = [
    "id",
    "public_id",
    "group_id",
    "sort_order",
    "name",
    "address",
    "port",
    "protocol",
    "auth",
    "username",
    "credential_id",
    "credential_username",
    "route",
    "proxy_kind",
    "proxy_host",
    "proxy_port",
    "proxy_username",
    "notes",
    "serial_baud",
    "serial_data_bits",
    "serial_parity",
    "serial_stop_bits",
    "serial_flow",
    "telnet_user_prompt",
    "telnet_pass_prompt",
    "one_time_code",
    "one_time_code_prompt",
    "login_commands",
];

impl RawHost {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        // 串口的数字列按文本读出来再解析，免得某一版改了类型就整行读不了
        let any = |i: usize| -> rusqlite::Result<Option<String>> {
            Ok(match r.get_ref(i)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Integer(v) => Some(v.to_string()),
                rusqlite::types::ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
                _ => None,
            })
        };
        Ok(Self {
            id: r.get(0)?,
            public_id: r.get(1)?,
            group: r.get(2)?,
            sort_order: r.get(3)?,
            name: r.get(4)?,
            address: r.get(5)?,
            port: r.get(6)?,
            protocol: r.get(7)?,
            auth: r.get(8)?,
            username: r.get(9)?,
            credential: r.get(10)?,
            credential_username: r.get(11)?,
            route: r.get(12)?,
            proxy_kind: r.get(13)?,
            proxy_host: r.get(14)?,
            proxy_port: r.get(15)?,
            proxy_username: r.get(16)?,
            notes: r.get(17)?,
            serial: [any(18)?, any(19)?, any(20)?, any(21)?, any(22)?],
            telnet_prompts: [r.get(23)?, r.get(24)?],
            one_time_code: r.get(25)?,
            one_time_code_prompt: r.get(26)?,
            login_commands: r.get(27)?,
        })
    }

    /// 转成主机记录（跳板另外填）；不认识的协议、认证、连接方式（更新的版本加的）与缺地址的为 `None`。
    fn into_record(self) -> Option<HostRecord> {
        let protocol = protocol(self.protocol.as_deref().unwrap_or("ssh"))?;
        let serial = match protocol {
            Protocol::Serial => {
                let [baud, bits, parity, stop, flow] = &self.serial;
                let number = |v: &Option<String>| v.as_deref()?.trim().parse::<i64>().ok();
                Some(serial_options(
                    number(baud)?,
                    number(bits)?,
                    parity.as_deref()?,
                    number(stop)?,
                    flow.as_deref()?,
                )?)
            }
            _ => None,
        };
        let route = match self.route.as_deref().unwrap_or("direct") {
            "direct" => Route::Direct,
            "jump" => Route::Jump(Vec::new()),
            "proxy" => Route::Proxy {
                kind: proxy_kind(self.proxy_kind.as_deref()?)?,
                host: self.proxy_host?,
                port: port_of(self.proxy_port?)?,
                username: self.proxy_username.unwrap_or_default(),
            },
            _ => return None,
        };
        let id = self.id.to_string();
        Some(HostRecord {
            key: self
                .public_id
                .filter(|p| !p.trim().is_empty())
                .unwrap_or_else(|| format!("host-{id}")),
            group: self.group.map(|g| g.to_string()),
            sort_order: self.sort_order.unwrap_or_default(),
            name: self.name.unwrap_or_default(),
            address: self.address?,
            port: match protocol {
                Protocol::Serial => 0,
                _ => port_of(self.port?)?,
            },
            protocol,
            auth: login(
                self.auth.as_deref()?,
                self.credential.map(|c| c.to_string()),
            )?,
            username: self.username.unwrap_or_default(),
            credential_username: self.credential_username.filter(|u| !u.trim().is_empty()),
            route,
            notes: self.notes.unwrap_or_default(),
            telnet_prompts: protocol == Protocol::Telnet
                && self
                    .telnet_prompts
                    .iter()
                    .any(|p| p.as_deref().is_some_and(|p| !p.trim().is_empty())),
            totp: (protocol == Protocol::Ssh && self.one_time_code == Some(1))
                .then(|| self.one_time_code_prompt.unwrap_or_default()),
            login_commands: self.login_commands.unwrap_or_default(),
            serial,
            id,
        })
    }
}

/// 按列名读各表。
struct Reader<'c> {
    conn: &'c Connection,
    /// 表名 → 列名
    columns: HashMap<String, HashSet<String>>,
}

impl<'c> Reader<'c> {
    fn new(conn: &'c Connection) -> Result<Self, ImportError> {
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .and_then(|mut s| s.query_map([], |r| r.get(0))?.collect())
            .map_err(not_shellrs)?;
        let mut columns = HashMap::new();
        for table in tables {
            let sql = format!("PRAGMA table_info(\"{}\")", table.replace('"', "\"\""));
            let names: HashSet<String> = conn
                .prepare(&sql)
                .and_then(|mut s| s.query_map([], |r| r.get(1))?.collect())
                .map_err(not_shellrs)?;
            columns.insert(table, names);
        }
        let reader = Self { conn, columns };
        let required: [(&str, &[&str]); 2] = [
            ("groups", &["id", "parent_id", "name"]),
            (
                "hosts",
                &["id", "group_id", "name", "address", "port", "auth"],
            ),
        ];
        for (table, names) in required {
            if let Some(column) = names.iter().find(|c| !reader.has(table, c)) {
                return Err(not_shellrs(format!("缺少 {table}.{column}")));
            }
        }
        Ok(reader)
    }

    fn has(&self, table: &str, column: &str) -> bool {
        self.columns.get(table).is_some_and(|c| c.contains(column))
    }

    /// 读表中的各行：表中没有的列按 `NULL` 读，按位置照样对得上；没有这张表时为空。
    fn rows<T>(
        &self,
        table: &str,
        columns: &[&str],
        order: &str,
        f: impl FnMut(&Row) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<Vec<T>> {
        if !self.columns.contains_key(table) {
            return Ok(Vec::new());
        }
        let list: Vec<&str> = columns
            .iter()
            .map(|c| if self.has(table, c) { *c } else { "NULL" })
            .collect();
        let sql = format!("SELECT {} FROM \"{table}\" {order}", list.join(", "));
        let mut statement = self.conn.prepare(&sql)?;
        statement.query_map([], f)?.collect()
    }

    fn read(&self) -> rusqlite::Result<Data> {
        let mut data = Data::default();

        for (id, group) in self.rows(
            "groups",
            &["id", "parent_id", "name", "sort_order"],
            "",
            |r| {
                Ok((
                    r.get::<_, i64>(0)?.to_string(),
                    Group {
                        parent: r.get::<_, Option<i64>>(1)?.map(|p| p.to_string()),
                        name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        sort_order: r.get::<_, Option<i64>>(3)?.unwrap_or_default(),
                    },
                ))
            },
        )? {
            data.groups.insert(id, group);
        }

        let credentials = self.rows(
            "credentials",
            &["id", "name", "kind", "username", "key_path"],
            "",
            |r| {
                let kind: Option<String> = r.get(2)?;
                Ok((
                    r.get::<_, i64>(0)?.to_string(),
                    kind.as_deref()
                        .and_then(credential_kind)
                        .map(|kind| Credential {
                            name: r
                                .get::<_, Option<String>>(1)
                                .ok()
                                .flatten()
                                .unwrap_or_default(),
                            kind,
                            username: r
                                .get::<_, Option<String>>(3)
                                .ok()
                                .flatten()
                                .unwrap_or_default(),
                            key_path: r.get::<_, Option<String>>(4).ok().flatten(),
                        }),
                ))
            },
        )?;
        for (id, credential) in credentials {
            if let Some(credential) = credential {
                data.credentials.insert(id, credential);
            }
        }

        // 每台主机的跳板，第一跳在前
        let mut jumps: HashMap<String, Vec<Option<String>>> = HashMap::new();
        for (host, jump) in self.rows(
            "host_jumps",
            &["host_id", "jump_id"],
            "ORDER BY host_id, position",
            |r| {
                Ok((
                    r.get::<_, i64>(0)?.to_string(),
                    r.get::<_, Option<i64>>(1)?.map(|j| j.to_string()),
                ))
            },
        )? {
            jumps.entry(host).or_default().push(jump);
        }

        for raw in self.rows("hosts", &HOST_COLUMNS, "ORDER BY id", RawHost::from_row)? {
            match raw.into_record() {
                Some(mut host) => {
                    if host.route == Route::Jump(Vec::new()) {
                        host.route = Route::Jump(jumps.remove(&host.id).unwrap_or_default());
                    }
                    data.hosts.push(host);
                }
                None => data.skipped += 1,
            }
        }

        let forwards = self.rows(
            "forwards",
            &[
                "host_id",
                "name",
                "kind",
                "bind_host",
                "bind_port",
                "target_host",
                "target_port",
                "auto_start",
                "sort_order",
            ],
            "ORDER BY id",
            |r| {
                let kind: Option<String> = r.get(2)?;
                let Some(kind) = kind.as_deref().and_then(forward_kind) else {
                    return Ok(None);
                };
                let Some(bind_port) = r.get::<_, Option<i64>>(4)?.and_then(port_of) else {
                    return Ok(None);
                };
                let target_port = match kind {
                    ForwardKind::Dynamic => 0,
                    _ => match r.get::<_, Option<i64>>(6)?.and_then(port_of) {
                        Some(port) => port,
                        None => return Ok(None),
                    },
                };
                Ok(Some(ForwardRecord {
                    host: r.get::<_, i64>(0)?.to_string(),
                    name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    kind,
                    bind_host: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    bind_port,
                    target_host: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    target_port,
                    auto_start: r.get::<_, Option<i64>>(7)? == Some(1),
                    sort_order: r.get::<_, Option<i64>>(8)?.unwrap_or_default(),
                }))
            },
        )?;
        data.forwards = forwards.into_iter().flatten().collect();

        for (id, category) in self.rows(
            "snippet_categories",
            &["id", "name", "parent_id", "sort_order"],
            "",
            |r| {
                Ok((
                    r.get::<_, i64>(0)?.to_string(),
                    Category {
                        name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        parent: r.get::<_, Option<i64>>(2)?.map(|p| p.to_string()),
                        sort_order: r.get::<_, Option<i64>>(3)?.unwrap_or_default(),
                    },
                ))
            },
        )? {
            data.categories.insert(id, category);
        }

        data.snippets = self.rows(
            "snippets",
            &[
                "category_id",
                "name",
                "command",
                "run_on_click",
                "sort_order",
            ],
            "ORDER BY id",
            |r| {
                Ok(SnippetRecord {
                    category: r.get::<_, Option<i64>>(0)?.map(|c| c.to_string()),
                    name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    command: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    run_on_click: r.get::<_, Option<i64>>(3)? == Some(1),
                    sort_order: r.get::<_, Option<i64>>(4)?.unwrap_or_default(),
                })
            },
        )?;
        Ok(data)
    }
}

/// `settings.json` 中的同步位置：本机文件夹且其中有同步数据时提示可以改选它，WebDAV、S3 时提示先下载到本机。
fn sync_note(settings: &Path) -> Option<PlanNote> {
    let raw = read_limited(settings, MAX_SETTINGS).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let sync = value.get("sync")?;
    if sync.get("enabled").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let location = sync.get("location")?;
    match location.get("kind")?.as_str()? {
        "folder" => {
            let path = PathBuf::from(location.get("path")?.as_str()?);
            super::sync::locate(&path).map(|_| PlanNote::SyncFolderAvailable { path })
        }
        "webdav" | "s3" => Some(PlanNote::SyncFolderRemote),
        _ => None,
    }
}
