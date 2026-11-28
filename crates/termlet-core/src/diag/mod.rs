//! 诊断信息：用户反馈问题时附上的一段文本。
//!
//! 内容：版本与运行时长、系统、数据目录与数据库结构版本、保险库状态、各类条目的数量、设置摘要、最近的崩溃记录、
//! 界面与运行中的模块给出的信息，最后是日志的末尾。不含主机地址、凭据、命令内容、审计日志、主机指纹：各部分只取
//! 必要的内容（自定义危险命令规则、关键词高亮只记条数，自定义本地终端只记程序名、参数个数与环境变量名——参数中
//! 常有令牌，环境变量的值常是令牌或带密码的代理地址），生成时再统一脱敏（[`Scrubber`]：用户目录、用户名、网址、
//! IP 地址、三级以上的域名、秘密）。
//!
//! 生成的是文本（依赖树中没有 zip 库，不打压缩包）：
//! 可以直接复制进反馈，也可以存成文件（日志取末尾，见 [`Scope`]）。
//!
//! # 用法（界面）
//!
//! - 设置的“关于”页放三个按钮，旁边是说明（“包含版本、运行环境与日志，
//!   不含主机地址、凭据和命令内容”）：
//!   - “复制诊断信息”：[`Diagnostics::text`] 传 [`Scope::Copy`]（日志只取最后 [`COPY_LOG_BYTES`]），写进剪贴板；
//!   - “导出诊断信息…”：选择保存位置（默认文件名 [`default_file_name`]），[`Diagnostics::export`]
//!     （日志最多 [`EXPORT_LOG_BYTES`]），完成后提示保存的位置；
//!   - “打开日志目录”：用系统的文件管理器打开 `DataDir::logs_dir()`（日志与崩溃记录都在这里）。
//! - [`Diagnostics::new`] 传入存储与数据目录；界面自己的信息用 [`Diagnostics::client`] 加上（gpui-kit 版本、显卡、
//!   窗口数、界面语言与主题等，不含任何用户数据）；运行中的模块用 [`Diagnostics::section`] 加上：终端、SFTP 会话
//!   与传输、运行中的转发的数量，应用锁是否启用，云同步用 [`sync_section`] 生成（不含路径、地址、账号与设备名）。
//! - 方法是异步的，内部在核心的运行时上读取（数据库、日志文件），返回的 future 是 `Send`，可以在 GPUI 的任务里
//!   直接 `await`。生成文本本身是纯函数 [`render`]，各部分的摘要也是纯函数（[`settings_summary`]、[`sync_section`]）。
//! - 运行时长从第一次调用 [`crate::crash::install`]（启动时最先调用）或 [`Diagnostics::new`] 时算起。

mod scrub;
mod system;
#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

pub use scrub::{MAX_MESSAGE_CHARS, MAX_STACK_CHARS, Scrubber};
pub use system::{
    SystemInfo, coarse_version, linux_coarse, macos_coarse, system_info, windows_coarse,
};

use crate::cloudsync::SyncStatus;
use crate::model::{
    ColorScheme, Forward, Group, Host, KeyEntry, LayoutNode, LocalSettings, Profile, Protocol,
    Settings, Snippet, Workspace,
};
use crate::paths::{DATA_DIR_ENV, DataDir};
use crate::store::Store;
use crate::vault::VaultStatus;

/// 复制到剪贴板时日志最多取这么多（字节）。
pub const COPY_LOG_BYTES: usize = 48 << 10;
/// 导出成文件时日志最多取这么多（字节，从最新的文件往前取）。
pub const EXPORT_LOG_BYTES: usize = 8 << 20;
/// 诊断信息中列出的最近的崩溃记录数。
const MAX_CRASHES: usize = 5;

/// 进程开始的时间（近似：第一次用到时）。
pub(crate) static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

/// 诊断信息的用途：决定日志取多少。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// 复制到剪贴板（[`COPY_LOG_BYTES`]）
    Copy,
    /// 导出成文件（[`EXPORT_LOG_BYTES`]）
    Export,
}

impl Scope {
    fn log_bytes(self) -> usize {
        match self {
            Scope::Copy => COPY_LOG_BYTES,
            Scope::Export => EXPORT_LOG_BYTES,
        }
    }
}

/// 一个日志文件的末尾。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogTail {
    pub name: String,
    /// 文件的大小（字节）
    pub size: u64,
    /// 修改时间（Unix 毫秒）
    pub modified_ms: i64,
    /// 取出的内容（只取了末尾时从一行的开头算起）
    pub text: String,
    /// 只取了末尾
    pub truncated: bool,
}

/// 生成诊断信息，见模块说明。
#[derive(Clone)]
pub struct Diagnostics {
    store: Arc<Store>,
    data_dir: DataDir,
    client: Map<String, Value>,
    sections: Map<String, Value>,
}

impl Diagnostics {
    pub fn new(store: Arc<Store>, data_dir: DataDir) -> Self {
        LazyLock::force(&STARTED);
        Self {
            store,
            data_dir,
            client: Map::new(),
            sections: Map::new(),
        }
    }

    /// 界面的信息（显卡、窗口数、界面语言等），放在 `client` 一节。
    pub fn client(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.client.insert(key.into(), value.into());
        self
    }

    /// 运行中的模块给出的一节（终端与 SFTP 的数量、云同步的状态等）。
    pub fn section(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.sections.insert(name.into(), value.into());
        self
    }

    /// 生成诊断信息的文本。
    pub async fn text(self, scope: Scope) -> String {
        let task = crate::runtime::runtime().spawn_blocking(move || self.collect(scope));
        match task.await {
            Ok(text) => text,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            // 运行时正在关闭（退出时）
            Err(_) => String::new(),
        }
    }

    /// 生成诊断信息并写到 `path`（只有当前用户能读写，先写临时文件再替换）。
    pub async fn export(self, path: PathBuf) -> io::Result<()> {
        let task = crate::runtime::runtime().spawn_blocking(move || {
            let text = self.collect(Scope::Export);
            crate::platform::write_private_file(&path, text.as_bytes())
        });
        match task.await {
            Ok(result) => result,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => Err(io::Error::other(e)),
        }
    }

    fn collect(&self, scope: Scope) -> String {
        let mut info = Map::new();
        info.insert("generatedAt".into(), json!(now_rfc3339()));
        info.insert(
            "app".into(),
            json!({
                "version": crate::VERSION,
                "build": if cfg!(debug_assertions) { "debug" } else { "release" },
                "uptimeSeconds": STARTED.elapsed().as_secs(),
            }),
        );
        info.insert("system".into(), json!(system_info()));
        info.insert("runtime".into(), self.runtime());
        info.insert("counts".into(), counts(&self.store));
        info.insert(
            "settings".into(),
            settings_summary(&self.store.settings(), &self.store.local_settings()),
        );
        info.insert(
            "crashes".into(),
            crash_section(&crate::crash::list_in(&self.data_dir.logs_dir())),
        );
        for (name, value) in &self.sections {
            info.insert(name.clone(), value.clone());
        }
        info.insert("client".into(), Value::Object(self.client.clone()));
        let logs = read_log_tails(&self.data_dir.logs_dir(), scope.log_bytes());
        render(&info, &logs, &Scrubber::current_user())
    }

    fn runtime(&self) -> Value {
        let db = self.store.database();
        let schema = db
            .read(|c| c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)))
            .ok();
        let vault_mode = db
            .meta("vault.mode")
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice::<String>(&v).ok());
        json!({
            "dataDir": self.data_dir.root().display().to_string(),
            "dataDirFromEnv": std::env::var_os(DATA_DIR_ENV).is_some_and(|v| !v.is_empty()),
            "portable": self.data_dir.is_portable(),
            "schemaVersion": schema,
            "vault": vault_status_name(&self.store.vault_status()),
            "vaultMode": vault_mode,
        })
    }
}

/// 生成诊断信息的文本（纯函数）：`info` 的各节（固定的几节在前）写成缩进的 JSON，之后是各日志的末尾；
/// 全部经过 `scrub` 脱敏。
pub fn render(info: &Map<String, Value>, logs: &[LogTail], scrub: &Scrubber) -> String {
    const FIRST: &[&str] = &[
        "generatedAt",
        "app",
        "system",
        "runtime",
        "counts",
        "settings",
        "crashes",
    ];
    let mut out = String::new();
    out.push_str("Termlet 诊断信息\n");
    out.push_str(
        "（已去掉用户目录、用户名、网址、IP 地址、三级以上的域名与密码等秘密；不含主机地址、凭据与命令内容）\n",
    );
    let mut rest: Vec<&str> = info
        .keys()
        .map(String::as_str)
        .filter(|k| !FIRST.contains(k) && *k != "client")
        .collect();
    rest.sort_unstable();
    let order: Vec<&str> = FIRST
        .iter()
        .copied()
        .chain(rest)
        .chain(std::iter::once("client"))
        .collect();
    for key in order {
        let Some(value) = info.get(key) else {
            continue;
        };
        let mut value = value.clone();
        scrub.value(&mut value);
        let text = serde_json::to_string_pretty(&value).unwrap_or_default();
        let _ = write!(out, "\n## {}\n{text}\n", scrub.text(key));
    }
    for log in logs {
        let size = format_bytes(log.size);
        let note = if log.truncated {
            format!(
                "共 {size}，以下是最后 {}",
                format_bytes(log.text.len() as u64)
            )
        } else {
            format!("共 {size}")
        };
        let _ = write!(
            out,
            "\n## 日志：{}（{note}，修改于 {}）\n",
            log.name,
            crate::time::format_local(log.modified_ms)
        );
        out.push_str(&scrub_log(&log.text, scrub));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// 日志脱敏：JSON 行只处理其中的字符串（不弄坏 JSON），其余的行连成一块按文本处理（Termlet 的日志是文本行，
/// 整块处理比逐行快得多）；跨行的私钥块先整体去掉。
fn scrub_log(text: &str, scrub: &Scrubber) -> String {
    let text = scrub.private_keys(text);
    let mut out = String::with_capacity(text.len());
    let mut plain_start = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = body.trim_start();
        let is_json = (trimmed.starts_with('{') || trimmed.starts_with('['))
            && serde_json::from_str::<serde::de::IgnoredAny>(body).is_ok();
        if is_json {
            out.push_str(&scrub.text(&text[plain_start..offset]));
            out.push_str(&scrub.json(body));
            out.push_str(&line[body.len()..]);
            plain_start = offset + line.len();
        }
        offset += line.len();
    }
    out.push_str(&scrub.text(&text[plain_start..]));
    out
}

/// 读出日志目录中各日志文件的末尾：名称含 `.log` 的文件，从最新的往前取，合计不超过 `budget` 字节；
/// 返回时按时间从旧到新排列。读不了的文件跳过。
pub fn read_log_tails(dir: &Path, budget: usize) -> Vec<LogTail> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, u64, SystemTime)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata().ok()?;
            (meta.is_file() && name.contains(".log")).then(|| {
                (
                    name,
                    meta.len(),
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                )
            })
        })
        .collect();
    files.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| b.0.cmp(&a.0)));
    let mut remaining = budget as u64;
    let mut out = Vec::new();
    for (name, size, modified) in files {
        if remaining == 0 {
            break;
        }
        let Ok(text) = read_tail(&dir.join(&name), size, remaining) else {
            continue;
        };
        remaining = remaining.saturating_sub(size.min(remaining));
        out.push(LogTail {
            name,
            size,
            modified_ms: modified
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            truncated: size > text.len() as u64,
            text,
        });
    }
    out.reverse();
    out
}

/// 文件的最后 `max` 字节；只取了末尾时从下一行的开头算起（不留半行）。
fn read_tail(path: &Path, size: u64, max: u64) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let take = size.min(max);
    file.seek(SeekFrom::Start(size - take))?;
    let mut bytes = Vec::with_capacity(take as usize);
    file.take(take).read_to_end(&mut bytes)?;
    if take < size {
        match bytes.iter().position(|&b| b == b'\n') {
            Some(i) => {
                bytes.drain(..=i);
            }
            None => bytes.clear(),
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 设置的摘要（纯函数）：不认识的字段不带；自定义危险命令规则、关键词高亮只记条数，ssh-agent 的地址、会话日志的
/// 目录只记是否设置，自定义本地终端只记程序名（不含目录）、参数个数与环境变量名。
pub fn settings_summary(settings: &Settings, local: &LocalSettings) -> Value {
    fn plain<T: serde::Serialize>(section: T) -> Value {
        serde_json::to_value(section).unwrap_or(Value::Null)
    }
    let mut general = settings.general.clone();
    general.extra.clear();
    let mut appearance = settings.appearance.clone();
    appearance.extra.clear();
    let mut terminal = settings.terminal.clone();
    terminal.extra.clear();
    let highlight = &settings.highlight;
    let connection = &settings.connection;
    let guard = &settings.guard;
    let mut app_lock = local.app_lock.clone();
    app_lock.extra.clear();
    let mut backup = local.backup.clone();
    backup.extra.clear();
    let shells: Vec<Value> = local
        .shells
        .custom
        .iter()
        .map(|shell| {
            let (program, args) = command_shape(&shell.command);
            json!({
                "id": shell.id,
                "name": shell.name,
                "program": program,
                "args": args,
                "dirSet": !shell.dir.is_empty(),
                "envNames": shell.env.keys().collect::<Vec<_>>(),
            })
        })
        .collect();
    // 对检测到的 shell 的改动：只记改了哪些项、参数的个数与环境变量的名称
    let overrides: Vec<Value> = local
        .shells
        .overrides
        .iter()
        .map(|(id, changes)| {
            json!({
                "id": id,
                "nameSet": !changes.name.trim().is_empty(),
                "args": changes.args.as_deref().map(|args| split_command(args).len()),
                "dirSet": !changes.dir.is_empty(),
                "envNames": changes.env.keys().collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "general": plain(general),
        "appearance": plain(appearance),
        "terminal": plain(terminal),
        "highlight": {
            "enabled": highlight.enabled,
            "rules": highlight.rules.len(),
            "enabledRules": highlight.rules.iter().filter(|r| r.enabled).count(),
            "regexRules": highlight.rules.iter().filter(|r| r.regex).count(),
        },
        "connection": {
            "keepaliveSeconds": connection.keepalive_seconds,
            "autoReconnect": connection.auto_reconnect,
            "agentPathSet": !connection.agent_path.is_empty(),
            "externalLaunch": connection.external_launch,
        },
        "guard": {
            "disabledRules": guard.disabled_rules,
            "customRules": guard.custom_rules.len(),
            "approval": guard.approval,
        },
        "keybindings": settings.keybindings.bindings,
        "macKeybindings": settings.keybindings.mac_bindings,
        "local": {
            // MCP 的 HTTP 接入只记开关、端口与监听范围（令牌不在设置中，也不进诊断信息）
            "cli": {
                "enabled": local.cli.enabled,
                "http": {
                    "enabled": local.cli.http.enabled,
                    "port": local.cli.http.port,
                    "scope": local.cli.http.scope,
                },
            },
            "appLock": plain(app_lock),
            "backup": plain(backup),
            "shells": {
                "custom": shells,
                "overrides": overrides,
                "hidden": local.shells.hidden,
                "sidebar": local.shells.sidebar.len(),
                "default": local.shells.default,
            },
            "logs": {
                "sessionLogDirSet": !local.logs.session_log_dir.is_empty(),
                "recordingDirSet": !local.logs.recording_dir.is_empty(),
                "crashReport": local.logs.crash_report,
                "level": local.logs.level,
                // 正在使用的过滤规则与启动时的指定（环境变量或启动参数）
                "filter": crate::logging::current_filter(),
                "overridden": crate::logging::overridden().map(|o| o.describe()),
            },
            "usage": { "enabled": local.usage.enabled },
        },
    })
}

/// 云同步的一节（纯函数）：是否设置、位置的类型、自动同步与间隔、设备数、上次同步的时间与错误码；
/// 不含路径、地址、账号与设备名。
pub fn sync_section(status: &SyncStatus) -> Value {
    let config = status.config.as_ref().map(|c| {
        json!({
            "type": c.target.kind_name(),
            "auto": c.auto,
            "intervalMinutes": c.interval_minutes,
        })
    });
    json!({
        "configured": status.config.is_some(),
        "config": config,
        "syncing": status.syncing,
        "devices": status.devices.len(),
        "lastSync": status.last_sync,
        "lastErrorCode": status.last_error.as_ref().map(|e| e.code()),
        "pending": status.pending.is_some(),
        "conflicts": status.conflicts.len(),
    })
}

/// 导出时的默认文件名：`termlet-diagnostics-20261020-051000.txt`（本地时间）。
pub fn default_file_name(now_ms: i64) -> String {
    use chrono::TimeZone as _;
    let stamp = chrono::Local
        .timestamp_millis_opt(now_ms)
        .single()
        .map(|t| t.format("%Y%m%d-%H%M%S").to_string())
        .unwrap_or_else(|| now_ms.to_string());
    format!("termlet-diagnostics-{stamp}.txt")
}

/// 各类条目的数量（不含内容）；工作区只记各自的分屏数（名称是用户起的）。
fn counts(store: &Store) -> Value {
    let hosts = store.list::<Host>();
    let by = |p: Protocol| hosts.iter().filter(|h| h.doc.protocol == p).count();
    let panes: Vec<usize> = store
        .list::<Workspace>()
        .iter()
        .map(|w| count_panes(&w.doc.layout))
        .collect();
    json!({
        "hosts": hosts.len(),
        "hostsSsh": by(Protocol::Ssh),
        "hostsTelnet": by(Protocol::Telnet),
        "hostsSerial": by(Protocol::Serial),
        "groups": store.list::<Group>().len(),
        "profiles": store.list::<Profile>().len(),
        "keys": store.list::<KeyEntry>().len(),
        "snippets": store.list::<Snippet>().len(),
        "forwards": store.list::<Forward>().len(),
        "workspaces": panes.len(),
        "workspacePanes": panes,
        "colorSchemes": store.list::<ColorScheme>().len(),
    })
}

/// 最近的崩溃记录（崩溃记录写入时已脱敏）。
fn crash_section(records: &[crate::crash::CrashRecord]) -> Value {
    let recent: Vec<Value> = records
        .iter()
        .take(MAX_CRASHES)
        .map(|r| {
            json!({
                "time": r.report.time,
                "version": r.report.version,
                "thread": r.report.thread,
                "location": r.report.location,
                "message": r.report.message,
            })
        })
        .collect();
    json!({ "total": records.len(), "recent": recent })
}

/// 布局中的分屏（叶子）数。
fn count_panes(node: &LayoutNode) -> usize {
    match node {
        LayoutNode::Session { .. } => 1,
        LayoutNode::Split { children, .. } => children.iter().map(count_panes).sum(),
    }
}

/// 命令行的程序名（不含目录）与参数个数。程序路径可以加引号（含空格时），参数按空白分隔，引号中的空白不分隔；
/// 只用于诊断，不必与 shell 的解析完全一致。
fn command_shape(command: &str) -> (String, usize) {
    let fields = split_command(command.trim());
    let Some(first) = fields.first() else {
        return (String::new(), 0);
    };
    let program = match first.rfind(['\\', '/']) {
        Some(i) => &first[i + 1..],
        None => first,
    };
    (program.to_string(), fields.len() - 1)
}

/// 按空白分隔命令行，单双引号中的空白不分隔（引号保留在结果中，第一项去掉两端的引号）。
fn split_command(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                current.push(c);
            }
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                current.push(c);
            }
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if let Some(first) = out.first_mut() {
        *first = first.trim_matches(['"', '\'']).to_string();
    }
    out
}

fn vault_status_name(status: &VaultStatus) -> &'static str {
    match status {
        VaultStatus::Unlocked => "unlocked",
        VaultStatus::NeedsPassword => "needsPassword",
        VaultStatus::NeedsNewPassword { .. } => "needsNewPassword",
        VaultStatus::ChooseKeyStore => "chooseKeyStore",
        VaultStatus::KeyMissing => "keyMissing",
        VaultStatus::KeyMismatch => "keyMismatch",
        VaultStatus::KeyringError(_) => "keyringError",
    }
}

/// 现在的本地时间，RFC 3339 格式。
fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
