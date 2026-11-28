//! 同步记录：本机的数据按"类型/ID"分成一条条记录，记录的内容是规范的 JSON（对象的键按字母排序，内容相同则摘要相同）。
//!
//! | 记录键 | 内容 |
//! |---|---|
//! | `host/h_…`、`group/g_…`、`profile/…`、`key/…`、`snippet/…`、`forward/…`、`workspace/…`、`scheme/…` | `{"created", "doc", "parent", "sort"}`：条目内容、上级、排序键与创建时间 |
//! | `setting/<组名>` | 这组设置的内容，去掉只属于本机的项（见 [`local_only`]） |
//! | `secret/<类型>/<ID>` | 这个条目的全部秘密：字段名 → 明文（整个同步文件再用同步密码加密） |
//! | `detected/host/<ID>` | 主机连接后识别出的系统：`{"os": "<键>"}`（见 [`crate::osprobe`]） |
//!
//! 修改时间不在内容中，在记录的版本里（见 [`version`]）：条目取 `updated_at`，设置组取那一行的 `updated_at`
//! （从没改过的为 0，合并时比任何修改都旧），秘密取各项秘密与条目本身最近的修改时间。
//! 命令历史、审计日志、主机指纹、本机设置（`local_setting`）不同步。
//!
//! 识别出的系统存在主机里（`Host::os`），但不是用户的修改：主机记录中去掉它，另成一条 `detected/` 记录，
//! 写入它不改变主机记录，不会与其他设备上对这台主机的修改冲突。它没有修改时间（为 0，两边不同时以云端的为准），
//! 冲突不列给用户看（[`super::RecordKind::of_key`] 不认识它），也不让被删除的主机留下（[`refs_of`] 不算它引用主机）；
//! 主机不在了时写入它什么都不做，下一次同步时从云端删除。

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::SyncCounts;
use crate::model::{
    Auth, CliAuth, Forward, Group, Host, Jump, KeySource, Kind, Profile, ProfileAuth,
};
use crate::secret::SecretString;
use crate::store::sync::{RawChange, RawItem, RawSecrets, RawSnapshot};

pub(crate) const SETTING: &str = "setting";
pub(crate) const SECRET: &str = "secret";
pub(crate) const DETECTED: &str = "detected";
/// 主机内容（`doc`）中识别出的系统的字段名，主机记录中去掉，另成 `detected/` 记录
const OS_FIELD: &str = "os";

/// 同步的设置组中只属于本机的项（检查更新、ssh-agent 地址、被堡垒机调起、自动记录会话日志、录制输入）。
/// 上传时去掉，写入时保留本机的值。
pub(crate) fn local_only(group: &str) -> &'static [&'static str] {
    match group {
        "general" => &["autoCheckUpdate"],
        "connection" => &["agentPath", "externalLaunch"],
        "terminal" => &["sessionLog", "recordInput"],
        _ => &[],
    }
}

/// 记录的内容（规范的 JSON 文本）。秘密记录的内容是明文，丢弃时清零，`Debug` 不显示内容。
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Data(Zeroizing<String>);

impl Data {
    pub fn new(text: String) -> Self {
        Self(Zeroizing::new(text))
    }

    /// 由 JSON 值生成（规范化）。
    pub fn of(value: &Value) -> Self {
        Self::new(canonical(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn value(&self) -> Option<Value> {
        serde_json::from_str(&self.0).ok()
    }

    pub fn hash(&self) -> String {
        hash_of(&self.0)
    }
}

impl std::fmt::Debug for Data {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Data({} 字节)", self.0.len())
    }
}

/// 本机的一条记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Local {
    pub data: Data,
    pub hash: String,
    /// 本机修改的时间（Unix 毫秒）；0 表示从没改过（默认值）
    pub changed: i64,
}

/// 同步文件中的一条记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Remote {
    /// 版本："修改时间（13 位 Unix 毫秒）-设备 ID-内容摘要前 8 位"，每次修改都不同
    pub v: String,
    pub data: Data,
    pub hash: String,
}

impl Remote {
    pub fn new(v: String, data: Data) -> Self {
        let hash = data.hash();
        Self { v, data, hash }
    }
}

pub(crate) type Locals = BTreeMap<String, Local>;
pub(crate) type Remotes = BTreeMap<String, Remote>;

/// 把 JSON 值编码为规范的文本：对象的键按字母排序（serde_json 开了 `preserve_order`，要自己排）。
pub(crate) fn canonical(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = Map::with_capacity(map.len());
                for key in keys {
                    out.insert(key.clone(), sorted(&map[key]));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sorted(value)).expect("JSON 值总能编码")
}

/// 内容的摘要：SHA-256 的前 16 字节（十六进制）。
pub(crate) fn hash_of(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    data_encoding::HEXLOWER.encode(&digest[..16])
}

/// 生成版本：修改时间、设备与内容摘要。比较新旧只看时间部分。
pub(crate) fn version(at: i64, device: &str, hash: &str) -> String {
    format!("{:013}-{device}-{}", at.max(0), &hash[..hash.len().min(8)])
}

/// 版本中的修改时间。
pub(crate) fn version_time(v: &str) -> i64 {
    v.split('-')
        .next()
        .and_then(|t| t.parse().ok())
        .unwrap_or(0)
}

/// 记录键的类型部分（`host`、`setting`、`secret`……）。
pub(crate) fn kind_of(key: &str) -> &str {
    key.split('/').next().unwrap_or("")
}

pub(crate) fn item_kind(name: &str) -> Option<Kind> {
    Kind::ALL.into_iter().find(|k| k.name() == name)
}

pub(crate) fn item_key(kind: Kind, id: &str) -> String {
    format!("{}/{id}", kind.name())
}

pub(crate) fn secret_key(owner: &str) -> String {
    format!("{SECRET}/{owner}")
}

/// 秘密记录的归属（`secret/host/h_1` → `host/h_1`）。
pub(crate) fn owner_of(key: &str) -> Option<&str> {
    key.strip_prefix(SECRET)?.strip_prefix('/')
}

/// 主机识别出的系统的记录键（`host/h_1` → `detected/host/h_1`）；不是主机时为 `None`。
pub(crate) fn detected_key(owner: &str) -> Option<String> {
    (kind_of(owner) == Kind::Host.name()).then(|| format!("{DETECTED}/{owner}"))
}

/// 同步时与条目同去同留的记录：它的秘密，主机还有识别出的系统。
pub(crate) fn companions(owner: &str) -> Vec<String> {
    let mut out = vec![secret_key(owner)];
    out.extend(detected_key(owner));
    out
}

/// 本机能否写入这条记录：认识的条目类型、设置组，以及它们的秘密。其他设备上更新版本的 Termlet 写的、本机不认识的
/// 记录留在云端，不写入本机，也不当作本机删除了它。
pub(crate) fn applicable(key: &str) -> bool {
    match key.split_once('/') {
        Some((SETTING, group)) => SETTING_GROUPS.contains(&group),
        Some((SECRET, owner)) => owner
            .split_once('/')
            .is_some_and(|(k, _)| item_kind(k).is_some()),
        Some((DETECTED, owner)) => owner
            .split_once('/')
            .is_some_and(|(k, _)| k == Kind::Host.name()),
        Some((kind, _)) => item_kind(kind).is_some(),
        None => false,
    }
}

/// 同步的设置组。
pub(crate) const SETTING_GROUPS: &[&str] = &[
    "general",
    "appearance",
    "terminal",
    "highlight",
    "connection",
    "guard",
    "keybindings",
    "snippets",
];

/// 是否计入删除、数量的统计：条目（凭据随主机、档案增删，设置不会删除，都不计入）。
pub(crate) fn countable(key: &str) -> bool {
    item_kind(kind_of(key)).is_some()
}

// ---------------------------------------------------------------- 本机 → 记录

/// 本机的全部记录。
pub(crate) fn locals_of(snapshot: &RawSnapshot) -> Locals {
    let mut out = Locals::new();
    for item in &snapshot.items {
        let key = item_key(item.kind, &item.id);
        let mut value = item_value(item);
        // 识别出的系统另成一条记录（见模块说明）
        let os = match value.get_mut("doc") {
            Some(Value::Object(doc)) if item.kind == Kind::Host => doc.remove(OS_FIELD),
            _ => None,
        };
        if let (Some(os @ Value::String(_)), Some(detected)) = (os, detected_key(&key)) {
            let mut map = Map::new();
            map.insert(OS_FIELD.into(), os);
            let data = Data::of(&Value::Object(map));
            let hash = data.hash();
            out.insert(
                detected,
                Local {
                    data,
                    hash,
                    changed: 0,
                },
            );
        }
        let data = Data::of(&value);
        let hash = data.hash();
        out.insert(
            key,
            Local {
                data,
                hash,
                changed: item.updated_at,
            },
        );
    }
    for setting in &snapshot.settings {
        let mut doc = setting.doc.clone();
        if let Value::Object(map) = &mut doc {
            for key in local_only(setting.id) {
                map.remove(*key);
            }
        }
        let data = Data::of(&doc);
        let hash = data.hash();
        out.insert(
            format!("{SETTING}/{}", setting.id),
            Local {
                data,
                hash,
                changed: setting.updated_at,
            },
        );
    }
    for secrets in &snapshot.secrets {
        let data = secrets_data(&secrets.fields);
        let hash = data.hash();
        out.insert(
            secret_key(&item_key(secrets.kind, &secrets.id)),
            Local {
                data,
                hash,
                changed: secrets.updated_at,
            },
        );
    }
    out
}

fn item_value(item: &RawItem) -> Value {
    let mut map = Map::new();
    map.insert("created".into(), Value::from(item.created_at));
    map.insert("doc".into(), item.doc.clone());
    map.insert(
        "parent".into(),
        item.parent
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    map.insert("sort".into(), Value::String(item.sort_key.clone()));
    Value::Object(map)
}

fn secrets_data(fields: &BTreeMap<String, SecretString>) -> Data {
    // 直接拼出规范的文本（键已经排好），不经过 serde_json::Value，少一份明文的副本
    let mut text = Zeroizing::new(String::from("{"));
    for (i, (field, value)) in fields.iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        text.push_str(&serde_json::to_string(field).expect("字符串总能编码"));
        text.push(':');
        let quoted = Zeroizing::new(serde_json::to_string(value.expose()).expect("字符串总能编码"));
        text.push_str(&quoted);
    }
    text.push('}');
    Data::new(std::mem::take(&mut *text))
}

/// 本机记录的摘要（同步期间本机是否被修改，比较这个）。
pub(crate) fn hashes(locals: &Locals) -> HashMap<String, String> {
    locals
        .iter()
        .map(|(k, l)| (k.clone(), l.hash.clone()))
        .collect()
}

/// 本机数据的指纹：全部记录键与摘要（自动同步判断本机修改后是否真的有变化）。
pub(crate) fn fingerprint(locals: &Locals) -> String {
    let mut hasher = Sha256::new();
    for (key, local) in locals {
        hasher.update(key.as_bytes());
        hasher.update([0]);
        hasher.update(local.hash.as_bytes());
        hasher.update([0]);
    }
    data_encoding::HEXLOWER.encode(&hasher.finalize())
}

// ---------------------------------------------------------------- 记录 → 本机

/// 把一条记录（`data` 为 `None` 表示删除）转成写入本机的变更；内容不是本机能理解的格式时返回 `None`。
/// `updated_at` 是写入后本机的修改时间（云端版本中的时间）。
pub(crate) fn change_of(key: &str, data: Option<&Data>, updated_at: i64) -> Option<RawChange> {
    let (kind, rest) = key.split_once('/')?;
    match kind {
        SETTING => {
            let id = SETTING_GROUPS.iter().find(|g| **g == rest)?;
            // 设置组不会被删除：云端没有这一组时保留本机的
            let doc = data?.value()?;
            Some(RawChange::PutSetting {
                id: (*id).to_string(),
                doc,
                updated_at,
                keep_local: local_only(id),
            })
        }
        SECRET => {
            let (owner_kind, id) = rest.split_once('/')?;
            let owner_kind = item_kind(owner_kind)?;
            let Some(data) = data else {
                return Some(RawChange::DeleteSecrets {
                    kind: owner_kind,
                    id: id.to_string(),
                });
            };
            let map: BTreeMap<String, String> = serde_json::from_str(data.as_str()).ok()?;
            let fields = map
                .into_iter()
                .map(|(k, v)| (k, SecretString::new(v)))
                .collect();
            Some(RawChange::PutSecrets(RawSecrets {
                kind: owner_kind,
                id: id.to_string(),
                fields,
                updated_at,
            }))
        }
        DETECTED => {
            let id = rest.strip_prefix(Kind::Host.name())?.strip_prefix('/')?;
            let os = match data {
                None => None,
                Some(data) => {
                    let Value::Object(map) = data.value()? else {
                        return None;
                    };
                    let key = map.get(OS_FIELD)?.as_str()?;
                    if !crate::model::valid_os_key(key) {
                        return None;
                    }
                    Some(key.to_string())
                }
            };
            Some(RawChange::SetHostOs {
                id: id.to_string(),
                os,
            })
        }
        other => {
            let kind = item_kind(other)?;
            let Some(data) = data else {
                return Some(RawChange::DeleteItem {
                    kind,
                    id: rest.to_string(),
                });
            };
            let Value::Object(mut map) = data.value()? else {
                return None;
            };
            let doc = map.remove("doc")?;
            let parent = match map.remove("parent") {
                Some(Value::String(p)) => Some(p),
                _ => None,
            };
            let sort_key = match map.remove("sort") {
                Some(Value::String(s)) => s,
                _ => String::new(),
            };
            let created_at = map
                .remove("created")
                .and_then(|v| v.as_i64())
                .unwrap_or(updated_at);
            Some(RawChange::PutItem(RawItem {
                kind,
                id: rest.to_string(),
                parent,
                sort_key,
                created_at,
                updated_at,
                doc,
            }))
        }
    }
}

/// 本机认得类型、认不出内容的条目记录：交给写入一方记为失败，引用它的条目与它的秘密也不写入（见 [`RawChange::Unreadable`]）。
pub(crate) fn unreadable_of(key: &str) -> Option<RawChange> {
    let (kind, id) = key.split_once('/')?;
    Some(RawChange::Unreadable {
        kind: item_kind(kind)?,
        id: id.to_string(),
    })
}

// ---------------------------------------------------------------- 内容的解读

/// 条目记录中的内容（`doc`）。
fn doc_of(data: &Data) -> Option<Value> {
    match data.value()? {
        Value::Object(mut map) => map.remove("doc"),
        _ => None,
    }
}

fn parent_of(data: &Data) -> Option<String> {
    match data.value()? {
        Value::Object(map) => map.get("parent")?.as_str().map(str::to_string),
        _ => None,
    }
}

/// 记录引用的其他记录（保持引用完整时保留它们）：上级分组、凭据档案、密钥库中的密钥、跳板机、端口转发的主机，
/// 分组默认值中的凭据档案与跳板机，以及秘密的归属。工作区引用的主机、配色不算（缺了界面照常处理）。
pub(crate) fn refs_of(key: &str, data: Option<&Data>) -> Vec<String> {
    let Some(data) = data else {
        return Vec::new();
    };
    let kind = kind_of(key);
    if kind == SECRET {
        return owner_of(key).map(str::to_string).into_iter().collect();
    }
    let mut out = Vec::new();
    let library = |source: &KeySource, out: &mut Vec<String>| {
        if let KeySource::Library { key } = source {
            out.push(item_key(Kind::Key, key.as_str()));
        }
    };
    match item_kind(kind) {
        Some(Kind::Host) => {
            if let Some(parent) = parent_of(data) {
                out.push(item_key(Kind::Group, &parent));
            }
            let Some(host) = doc_of(data).and_then(|d| serde_json::from_value::<Host>(d).ok())
            else {
                return out;
            };
            match &host.auth {
                Auth::Profile { profile } => out.push(item_key(Kind::Profile, profile.as_str())),
                Auth::Key { source } => library(source, &mut out),
                _ => {}
            }
            if let Some(credential) = &host.cli.credential
                && let CliAuth::Key { source } = &credential.auth
            {
                library(source, &mut out);
            }
            if let Jump::Via { host } = &host.jump {
                out.push(item_key(Kind::Host, host.as_str()));
            }
        }
        Some(Kind::Profile) => {
            if let Some(profile) =
                doc_of(data).and_then(|d| serde_json::from_value::<Profile>(d).ok())
                && let ProfileAuth::Key { source } = &profile.auth
            {
                library(source, &mut out);
            }
        }
        Some(Kind::Group) => {
            if let Some(parent) = parent_of(data) {
                out.push(item_key(Kind::Group, &parent));
            }
            if let Some(group) = doc_of(data).and_then(|d| serde_json::from_value::<Group>(d).ok())
            {
                if let Some(profile) = &group.defaults.profile {
                    out.push(item_key(Kind::Profile, profile.as_str()));
                }
                if let Some(jump) = &group.defaults.jump {
                    out.push(item_key(Kind::Host, jump.as_str()));
                }
            }
        }
        Some(Kind::Forward) => {
            if let Some(host) = doc_of(data)
                .and_then(|d| serde_json::from_value::<Forward>(d).ok())
                .and_then(|f| f.host)
            {
                out.push(item_key(Kind::Host, host.as_str()));
            }
        }
        _ => {}
    }
    out
}

/// 记录在冲突列表中显示的名称：条目的名称，设置为组名；没有名称时为 ID。
pub(crate) fn name_of(key: &str, data: Option<&Data>) -> String {
    let (kind, id) = key.split_once('/').unwrap_or((key, ""));
    if kind == SETTING {
        return id.to_string();
    }
    data.and_then(doc_of)
        .and_then(|doc| doc.get("name")?.as_str().map(str::to_string))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| id.to_string())
}

/// 各类数据的条数。
pub(crate) fn counts<'a>(keys: impl Iterator<Item = &'a String>) -> SyncCounts {
    let mut c = SyncCounts::default();
    for key in keys {
        match item_kind(kind_of(key)) {
            Some(Kind::Host) => c.hosts += 1,
            Some(Kind::Group) => c.groups += 1,
            Some(Kind::Profile) => c.profiles += 1,
            Some(Kind::Key) => c.keys += 1,
            Some(Kind::Snippet) => c.snippets += 1,
            Some(Kind::Forward) => c.forwards += 1,
            Some(Kind::Workspace) => c.workspaces += 1,
            Some(Kind::Scheme) => c.schemes += 1,
            None => {}
        }
    }
    c
}

/// 本机与云端 ID 不同、而协议、地址、端口、用户名都相同的主机数（两台设备各自添加的同一台主机，合并后会有两份）。
pub(crate) fn duplicate_hosts(locals: &Locals, remote: &Remotes) -> usize {
    let ident = |data: &Data| {
        let host: Host = serde_json::from_value(doc_of(data)?).ok()?;
        Some((
            format!("{:?}", host.protocol),
            host.address.to_lowercase(),
            host.effective_port(),
            host.username.clone(),
        ))
    };
    let host_prefix = format!("{}/", Kind::Host.name());
    let seen: std::collections::HashSet<_> = remote
        .iter()
        .filter(|(k, _)| k.starts_with(&host_prefix))
        .filter_map(|(_, r)| ident(&r.data))
        .collect();
    locals
        .iter()
        .filter(|(k, l)| {
            k.starts_with(&host_prefix)
                && !remote.contains_key(*k)
                && ident(&l.data).is_some_and(|i| seen.contains(&i))
        })
        .count()
}
