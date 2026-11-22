//! FinalShell 的数据目录（其中的 `conn`），或在 FinalShell 中右键“导出”得到的目录。
//!
//! FinalShell 闭源，没有格式文档。下面的格式按几个开源客户端的 FinalShell 导入归纳（2026-11-22 查阅，只看格式、
//! 不抄代码）：NyaTerm（Apache-2.0，提交 290212c）的 `crates/nyaterm-core/src/session_import/finalshell.rs`、
//! Hatoba（MIT）的 `apps/desktop/src-tauri/src/import/finalshell.rs`、DengShell（MIT）的
//! `internal/app/finalshell_import.go`。
//!
//! ```text
//! conn/<id>_connect_config.json            顶层的连接
//! conn/<文件夹 ID>/folder.json             文件夹：{"id", "name", "parent_id", "delete_time"}
//! conn/<文件夹 ID>/<id>_connect_config.json
//! ```
//!
//! - 文件夹的目录名是它的 ID，显示名称在 `folder.json` 中；连接与文件夹都以 `parent_id` 指向所在文件夹的 ID
//!   （顶层为 `root`、`0` 或空），按它拼出分组路径。右键“导出”的目录以文件夹的名称命名、没有 `folder.json`，
//!   这时按目录名分组。
//! - 连接文件：`name`、`host`、`port`（有的版本是字符串）、`user_name`、`description`（备注）、`terminal_encoding`。
//!   `conection_type`（原文如此）为 100 的是 SSH，其余（RDP 等）跳过并计数；`delete_time` 非 0 的在 FinalShell 的
//!   回收站中，与已删除的文件夹一样不导入。
//! - `authentication_type` 为 2（公钥）时私钥在 FinalShell 自己的密钥库中；`proxy_id` 指向 FinalShell 自己的代理列表；
//!   `port_forwarding_list` 是端口转发（`remote_port_forwarding` 的结构不清楚，非空时按 1 条计）。这些设置不在连接
//!   文件中、格式也没有公开资料，不导入，在预览中逐台说明。
//! - `authentication_type` 为 1（密码）时的 `password` 是 FinalShell 可逆加密的密码（见 [`password`]），用户勾选
//!   “导入保存的密码”后才导入（[`Source::secrets_need_consent`]）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::model::Host;

use super::files::{LocalEnv, decode_text, dir_segments, read_limited, walk_files};
use super::plan::{Hint, PlanBuilder, PlannedHost, clip, encoding_by_name};
use super::{ImportError, ImportPlan, Source};

mod password;

/// `conection_type`：SSH
const SSH: i64 = 100;
/// `authentication_type`：密码
const AUTH_PASSWORD: i64 = 1;
/// `authentication_type`：公钥
const AUTH_KEY: i64 = 2;
/// 连接文件名的结尾
const CONNECTION_SUFFIX: &str = "_connect_config.json";
/// 文件夹的配置文件
const FOLDER_FILE: &str = "folder.json";
/// 单个文件的大小上限：连接、文件夹的配置只有几百字节
const MAX_FILE: u64 = 1 << 20;
/// 读取的文件总数与总大小的上限，防止误选很大的目录；超出的连接跳过并计数
const MAX_FILES: usize = 5000;
const MAX_TOTAL: u64 = 64 << 20;
/// 文件夹的层数上限
const MAX_DEPTH: usize = 32;

pub(super) fn read(dir: &Path, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let mut dir = env.input_path(dir);
    // 选的是数据目录（conn 的上一级）
    if dir.join("conn").is_dir() {
        dir.push("conn");
    }
    let mut files = Vec::new();
    let mut total = 0;
    let mut dropped = 0;
    for (rel, file) in walk_files(&dir, "json")? {
        let connection = is_connection_file(&rel);
        if !connection && !is_folder_file(&rel) {
            continue;
        }
        if files.len() >= MAX_FILES || total > MAX_TOTAL {
            dropped += usize::from(connection);
            continue;
        }
        // 读不了的连接文件当作格式不对（跳过并计数）
        let text = read_limited(&file, MAX_FILE)
            .map(|raw| {
                total += raw.len() as u64;
                decode_text(&raw, env.ansi)
            })
            .unwrap_or_default();
        files.push((rel, text));
    }
    let mut plan = parse(&files);
    plan.skipped += dropped;
    Ok(plan)
}

/// 解析读到的文件：`rel` 是相对于连接目录的各级名称（最后一个是文件名），`text` 是文件内容。
/// 只认 `folder.json` 与 `*_connect_config.json`，其余文件忽略。
pub(super) fn parse(files: &[(Vec<String>, String)]) -> ImportPlan {
    let mut plan = PlanBuilder::new(Source::FinalShell);
    let mut folders = HashMap::new();
    // 有 folder.json 的目录：其中的连接按 parent_id 分组，不按目录名
    let mut folder_dirs = HashSet::new();
    for (rel, text) in files.iter().filter(|(rel, _)| is_folder_file(rel)) {
        folder_dirs.insert(dir_segments(rel));
        let Ok(Value::Object(o)) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        let id = field(&o, "id");
        if id.is_empty() || deleted(&o) {
            continue;
        }
        folders.insert(
            id,
            Folder {
                name: field(&o, "name"),
                parent: field(&o, "parent_id"),
            },
        );
    }
    for (rel, text) in files.iter().filter(|(rel, _)| is_connection_file(rel)) {
        let Ok(Value::Object(o)) = serde_json::from_str::<Value>(text) else {
            plan.skip(1);
            continue;
        };
        if deleted(&o) {
            continue;
        }
        let path = folder_path(&folders, &field(&o, "parent_id")).unwrap_or_else(|| {
            let dirs = dir_segments(rel);
            if folder_dirs.contains(dirs) {
                Vec::new()
            } else {
                dirs.to_vec()
            }
        });
        match connection(&o, rel.join("/")) {
            Some(mut item) => {
                item.group = plan.group(&path);
                plan.push(item);
            }
            None => plan.skip(1),
        }
    }
    plan.finish()
}

struct Folder {
    name: String,
    parent: String,
}

/// 连接文件中的一个连接；不是 SSH、没有地址时为 `None`（跳过并计数）。
fn connection(o: &Map<String, Value>, key: String) -> Option<PlannedHost> {
    if integer(o, "conection_type").is_some_and(|t| t != SSH) {
        return None;
    }
    let address = field(o, "host");
    if address.is_empty() {
        return None;
    }
    let mut username = field(o, "user_name");
    if username.is_empty() {
        username = field(o, "username");
    }
    let mut name = field(o, "name");
    if name.is_empty() {
        name = address.clone();
    }
    let port = integer(o, "port")
        .filter(|p| (1..=65535).contains(p))
        .map_or(22, |p| p as u16);
    let host = Host {
        name,
        address,
        port,
        username,
        note: clip(&field(o, "description"), 2000),
        ..Default::default()
    };
    let mut item = PlannedHost::new(key, host);
    if item.host.username.is_empty() {
        item.hints.push(Hint::MissingUsername);
    }
    let encoding = field(o, "terminal_encoding");
    if !encoding.is_empty() {
        item.set_encoding(encoding_by_name(&encoding), &encoding);
    }

    // 没有 authentication_type 的旧文件：有私钥 ID 的按公钥
    let auth = integer(o, "authentication_type");
    let uses_key =
        auth == Some(AUTH_KEY) || (auth.is_none() && is_reference(&field(o, "secret_key_id")));
    if uses_key {
        item.hints.push(Hint::PrivateKeyNotImported { name: None });
    } else if auth.is_none_or(|a| a == AUTH_PASSWORD) {
        // 公钥认证时文件中可能还留着以前的密码，不导入
        match password::decode(&field(o, "password")) {
            Ok(Some(password)) => item.set_password(password),
            Ok(None) => {}
            Err(password::Invalid) => item.hints.push(Hint::SavedPasswordInvalid),
        }
    }
    if is_reference(&field(o, "proxy_id")) {
        item.hints.push(Hint::ProxyNotImported { name: None });
    }
    let forwards = count(o.get("port_forwarding_list")) + count(o.get("remote_port_forwarding"));
    if forwards > 0 {
        item.hints
            .push(Hint::ForwardsNotImported { count: forwards });
    }
    Some(item)
}

/// 文件夹 `parent` 的分组路径（从最外层起）；顶层或找不到这个文件夹时为 `None`。
/// 向上找时遇到不存在、已删除或成环的上级就停。
fn folder_path(folders: &HashMap<String, Folder>, parent: &str) -> Option<Vec<String>> {
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut current = parent;
    while !is_root(current) && path.len() < MAX_DEPTH && seen.insert(current) {
        let Some(folder) = folders.get(current) else {
            break;
        };
        path.push(folder.name.clone());
        current = &folder.parent;
    }
    if path.is_empty() {
        return None;
    }
    path.reverse();
    Some(path)
}

fn is_connection_file(rel: &[String]) -> bool {
    rel.last()
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(CONNECTION_SUFFIX))
}

fn is_folder_file(rel: &[String]) -> bool {
    rel.last()
        .is_some_and(|name| name.eq_ignore_ascii_case(FOLDER_FILE))
}

/// 字段的文本：字符串去掉首尾空白，数字转为十进制；其余为空。
fn field(o: &Map<String, Value>, key: &str) -> String {
    match o.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// 字段的整数值：数字，或只有数字的字符串（有的版本把端口等写成字符串）。
fn integer(o: &Map<String, Value>, key: &str) -> Option<i64> {
    match o.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// 在 FinalShell 的回收站中（`delete_time` 非 0）。
fn deleted(o: &Map<String, Value>) -> bool {
    integer(o, "delete_time").is_some_and(|t| t != 0)
}

/// 所在文件夹的 ID 表示顶层。
fn is_root(id: &str) -> bool {
    id.is_empty() || id == "root" || id == "0"
}

/// 引用（私钥、代理的 ID）是否有值：空、`0`、`-1` 表示没有。
fn is_reference(id: &str) -> bool {
    !id.is_empty() && id != "0" && id != "-1"
}

/// 端口转发的条数：数组的元素数；结构不清楚的非空对象按 1 条。
fn count(value: Option<&Value>) -> usize {
    match value {
        Some(Value::Array(items)) => items.len(),
        Some(Value::Object(map)) => usize::from(!map.is_empty()),
        _ => 0,
    }
}

/// FinalShell 的数据目录的候选（其中的 `conn`）：注册表中的安装目录，安装程序的默认位置（`%LOCALAPPDATA%`），
/// `%APPDATA%`、系统盘根目录、主目录下的 `finalshell`，以及 macOS、Linux 上的位置。
pub(super) fn default_candidates(
    env: &LocalEnv,
    registry: impl Fn() -> Option<PathBuf>,
) -> Vec<PathBuf> {
    let home = env.home.as_ref();
    [
        registry(),
        home.map(|h| h.join("AppData").join("Local").join("finalshell")),
        env.config_dir.as_ref().map(|d| d.join("finalshell")),
        cfg!(windows).then(|| PathBuf::from(format!("{}:\\finalshell", env.system_drive))),
        home.map(|h| h.join("finalshell")),
        home.map(|h| h.join("Library").join("FinalShell")),
        home.map(|h| h.join(".finalshell")),
    ]
    .into_iter()
    .flatten()
    .map(|base| base.join("conn"))
    .collect()
}

#[cfg(test)]
mod tests;
