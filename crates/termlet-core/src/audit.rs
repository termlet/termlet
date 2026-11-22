//! 审计日志：`termlet` 命令行工具的调用（执行命令、在终端中执行、上传、下载）与主机工具面板的写操作
//! （结束进程、启停服务、启停与删除容器等）。
//!
//! - 存在本机数据库的 `audit_log` 表中（不同步；完整备份可选是否包含），不写入运行日志。
//! - 记录的是命令原文与传输的路径；秘密（密码、私钥）从不经过这里。
//! - 单条内容最多保存 [`MAX_DETAIL`]，超出部分不保存并加上截断标记；列表只返回内容的开头（[`SUMMARY_LENGTH`] 个字符），
//!   查看某一条时用 [`AuditLog::get`] 取完整内容。
//! - 保留最近 [`MAX_ENTRIES`] 条、内容合计不超过 [`MAX_BYTES`]，每写入 100 条清理一次更早的。
//! - 用时（`duration_ms`）不含等待用户批准危险命令的时间，等待的时长另记在 `approval_wait_ms`（0.3.2 起；
//!   更早的记录没有这一项，它们的用时含等待的时间）。
//!
//! 方法都是同步的（SQLite），在界面中从后台执行器调用；写入很快（毫秒级）。

use std::io::Write;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use regex::Regex;
use rusqlite::{Connection, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::proto::mcp::{CALLER_HTTP, CALLER_MARKER};
use crate::store::Store;
use crate::{CoreError, Result};

/// 保留的最多记录数，超出后删除最早的记录。
pub const MAX_ENTRIES: usize = 100_000;

/// 全部记录内容（命令原文、主机名、调用方）的总大小上限（字节），超出后删除最早的记录：单条最多 [`MAX_DETAIL`]，
/// 只按条数限制时最坏要占约 6GB。两个上限都要满足，平常的短命令只受条数限制。
pub const MAX_BYTES: i64 = 256 << 20;

/// 保存的内容（命令原文、传输的路径）的上限（字节）：AI 反复用 `--stdin` 发送大脚本时，不截断会让数据库膨胀到 GB 级。
pub const MAX_DETAIL: usize = 64 << 10;

/// 列表中每条内容的上限（字符）：更长的只返回开头（`partial`），完整内容用 [`AuditLog::get`] 取得。
pub const SUMMARY_LENGTH: usize = 500;

/// 主机名（主机不存在时是命令行传来的原文）、调用方的上限（字节）。
const MAX_FIELD: usize = 1 << 10;

/// 表结构。新建的数据库由结构版本 1 建表（`db/schema_v1.sql` 中的同一张表）；这里补上在加入这张表之前创建的
/// 开发用数据库（发布前去掉）。`detail` 以外的列都有默认值。之后加的列见 [`ADDED_COLUMNS`]。
const ENSURE_TABLE: &str = "CREATE TABLE IF NOT EXISTS audit_log (
  id          INTEGER PRIMARY KEY,
  at          INTEGER NOT NULL DEFAULT 0,
  host_id     TEXT NOT NULL DEFAULT '',
  host_name   TEXT NOT NULL DEFAULT '',
  action      TEXT NOT NULL DEFAULT '',
  detail      TEXT NOT NULL,
  status      TEXT NOT NULL DEFAULT '',
  exit_code   INTEGER,
  error_code  TEXT,
  duration_ms INTEGER NOT NULL DEFAULT 0,
  bytes       INTEGER,
  caller      TEXT NOT NULL DEFAULT ''
) STRICT";

/// 结构版本 1 之后加的列（名称与定义）：打开时没有就补上，不升结构版本。只加可以为空的列，旧版本的 Termlet
/// 照常读写（插入、查询都写明列名），备份恢复只复制两边都有的列，云同步不涉及这张表。
const ADDED_COLUMNS: &[(&str, &str)] = &[
    // 0.3.2：等待用户批准危险命令的时长（毫秒）；没有询问过用户的为空
    ("approval_wait_ms", "INTEGER"),
];

/// 内容超过 [`MAX_DETAIL`] 时追加在保存的开头部分之后的标记，数字为原文的字节数。导出的文件中原样保留；
/// [`AuditLog::get`] 时去掉标记，改为设置 `truncated` 与 `detail_size`。
fn truncated_mark(size: usize) -> String {
    format!("\n…[termlet: truncated, {size} bytes in total]")
}

static TRUNCATED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\n…\[termlet: truncated, (\d+) bytes in total\]$").expect("截断标记的正则")
});

/// 动作。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    /// `termlet exec`（新建连接执行）
    #[default]
    Exec,
    /// `termlet exec --terminal`（在已打开的终端中执行）
    Terminal,
    Upload,
    Download,
    /// 主机工具面板的写操作（内容是执行的命令，名称作为注释附在后面，见 `hosttools::audit_detail`）
    Panel,
    /// 主机工具面板把需要 root 的操作放进新的终端执行（sudo 的密码由用户在终端中输入，结果不在这里）
    PanelTerminal,
    /// 本版本不认识的动作（更新版本的 Termlet 写入的）
    #[serde(other)]
    Other,
}

impl AuditAction {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditAction::Exec => "exec",
            AuditAction::Terminal => "terminal",
            AuditAction::Upload => "upload",
            AuditAction::Download => "download",
            AuditAction::Panel => "panel",
            AuditAction::PanelTerminal => "panel_terminal",
            AuditAction::Other => "other",
        }
    }

    fn parse(text: &str) -> Self {
        [
            AuditAction::Exec,
            AuditAction::Terminal,
            AuditAction::Upload,
            AuditAction::Download,
            AuditAction::Panel,
            AuditAction::PanelTerminal,
        ]
        .into_iter()
        .find(|a| a.as_str() == text)
        .unwrap_or(AuditAction::Other)
    }

    fn is_panel(self) -> bool {
        matches!(self, AuditAction::Panel | AuditAction::PanelTerminal)
    }
}

/// 结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    /// 已执行（远程命令可能以非 0 退出码结束）
    #[default]
    Ok,
    /// 未能执行：主机不存在、未允许、连接失败等
    Failed,
    /// 命中危险命令规则被拦截（设置为直接拦截，或没有打开的窗口可以询问）
    Blocked,
    /// 命中危险命令规则，经用户在界面中批准后执行
    Approved,
    /// 命中危险命令规则，用户拒绝或等待批准超时
    Denied,
    /// 本版本不认识的结果
    #[serde(other)]
    Other,
}

impl AuditStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditStatus::Ok => "ok",
            AuditStatus::Failed => "failed",
            AuditStatus::Blocked => "blocked",
            AuditStatus::Approved => "approved",
            AuditStatus::Denied => "denied",
            AuditStatus::Other => "other",
        }
    }

    fn parse(text: &str) -> Self {
        [
            AuditStatus::Ok,
            AuditStatus::Failed,
            AuditStatus::Blocked,
            AuditStatus::Approved,
            AuditStatus::Denied,
        ]
        .into_iter()
        .find(|s| s.as_str() == text)
        .unwrap_or(AuditStatus::Other)
    }
}

/// 来源：由动作与调用方得出，不另存一列（旧记录同样适用）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditSource {
    /// `termlet` 命令行工具
    #[default]
    Cli,
    /// 命令行工具的 MCP 服务（`termlet mcp`）：调用方以 `mcp` 或 `mcp:<客户端名>` 开头
    Mcp,
    /// 主机工具面板
    Panel,
}

impl AuditSource {
    pub fn of(action: AuditAction, caller: &str) -> Self {
        if action.is_panel() {
            AuditSource::Panel
        } else if mcp_client(caller).is_some() {
            AuditSource::Mcp
        } else {
            AuditSource::Cli
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AuditSource::Cli => "cli",
            AuditSource::Mcp => "mcp",
            AuditSource::Panel => "panel",
        }
    }
}

/// MCP 服务的调用：调用方的第一项是 `mcp:<客户端名>`（客户端自报的名字）或 `mcp`（客户端没有报名字），返回
/// 客户端名（没有时为空）；不是 MCP 的调用为 `None`。
pub fn mcp_client(caller: &str) -> Option<&str> {
    let first = caller.split(" < ").next().unwrap_or_default();
    if first == CALLER_MARKER {
        return Some("");
    }
    first.strip_prefix(CALLER_MARKER)?.strip_prefix(':')
}

/// 经 HTTP 的 MCP 调用：调用方的第一项是 MCP 的标记，第二项是 `http`（stdio 的第二项是祖先进程名）。
pub fn mcp_over_http(caller: &str) -> bool {
    let mut items = caller.split(" < ");
    let first = items.next().unwrap_or_default();
    mcp_client(first).is_some() && items.next() == Some(CALLER_HTTP)
}

/// 一条审计记录。写入时 `id`、`source`、`partial`、`truncated`、`detail_size` 不用填。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AuditEntry {
    pub id: i64,
    /// Unix 毫秒；写入时为 0 取当前时间
    pub at: i64,
    pub host_id: String,
    /// 主机名；主机不存在时是命令行传来的原文
    pub host_name: String,
    pub action: AuditAction,
    /// 命令原文或传输的路径（`本机 → 远程`、`远程 → 本机`）
    pub detail: String,
    pub status: AuditStatus,
    /// 远程命令的退出码（执行了的）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// 失败时的错误码（`termlet_proto::ErrorCode` 的写法）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error_code: String,
    /// 用时：不含等待用户批准的时间（0.3.2 之前的记录含）
    pub duration_ms: i64,
    /// 等待用户批准危险命令的时长（询问过用户的才有）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_wait_ms: Option<i64>,
    /// 传输的字节数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<i64>,
    /// 调用方祖先进程名，由近到远，以 ` < ` 连接（来自面板的记录为空）
    pub caller: String,
    pub source: AuditSource,
    /// `detail` 只是开头的 [`SUMMARY_LENGTH`] 个字符（[`AuditLog::list`] 的结果）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// 原文超过 [`MAX_DETAIL`]，只保存了开头部分；`detail_size` 是原文的字节数（[`AuditLog::get`] 的结果）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub detail_size: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl AuditEntry {
    /// 去掉 [`AuditLog::get`] 结果中的截断标记，改为设置 `truncated` 与 `detail_size`。
    /// 只认写入时加上的标记：开头部分接近 [`MAX_DETAIL`] 且原文更长；命令本身以同样的文字结尾时不会被误认。
    fn unclip(&mut self) {
        let Some(caps) = TRUNCATED_RE.captures(&self.detail) else {
            return;
        };
        let whole = caps.get(0).expect("整个匹配");
        if whole.start() < MAX_DETAIL - 4 {
            return;
        }
        let Ok(size) = caps[1].parse::<i64>() else {
            return;
        };
        if size <= MAX_DETAIL as i64 {
            return;
        }
        self.detail.truncate(whole.start());
        self.truncated = true;
        self.detail_size = size;
    }

    /// 与列表中相同的摘要：内容只留开头的 [`SUMMARY_LENGTH`] 个字符（更长时 `partial`）。
    pub fn summary(&self) -> AuditEntry {
        let mut chars = self.detail.char_indices();
        let end = chars.nth(SUMMARY_LENGTH).map(|(i, _)| i);
        AuditEntry {
            detail: self.detail[..end.unwrap_or(self.detail.len())].to_string(),
            partial: end.is_some(),
            host_id: self.host_id.clone(),
            host_name: self.host_name.clone(),
            error_code: self.error_code.clone(),
            caller: self.caller.clone(),
            ..*self
        }
    }
}

/// 把内容截断到 [`MAX_DETAIL`] 字节以内（不切断 UTF-8 字符），并加上截断标记。
fn clip(detail: String) -> String {
    if detail.len() <= MAX_DETAIL {
        return detail;
    }
    let size = detail.len();
    let mut kept = cut(&detail, MAX_DETAIL).to_string();
    kept.push_str(&truncated_mark(size));
    kept
}

/// `s` 不超过 `n` 字节、不切断 UTF-8 字符的开头部分。
fn cut(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 查询条件。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditQuery {
    /// 最多返回几条：默认 200，最大 1000
    pub limit: usize,
    /// 翻页：只返回 ID 小于它的记录（0 表示从最新的开始）
    pub before: i64,
    /// 匹配主机名、内容、调用方（ASCII 不区分大小写，`%` `_` 按字面）
    pub q: String,
    pub status: Option<AuditStatus>,
    pub source: Option<AuditSource>,
}

impl AuditQuery {
    fn conditions(&self) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        let mut conds: Vec<String> = Vec::new();
        let mut args: Vec<Value> = Vec::new();
        if self.before > 0 {
            conds.push("id < ?".into());
            args.push(Value::Integer(self.before));
        }
        if let Some(status) = self.status {
            conds.push("status = ?".into());
            args.push(Value::Text(status.as_str().into()));
        }
        let panel = [AuditAction::Panel, AuditAction::PanelTerminal]
            .map(|a| Value::Text(a.as_str().into()));
        // 与 mcp_client 相同：调用方的第一项是 mcp 或 mcp:<客户端名>（标记是 ASCII，substr 按字符计也一样）
        let m = CALLER_MARKER;
        let mcp = format!(
            "(caller = '{m}' OR substr(caller, 1, {}) = '{m}:' OR substr(caller, 1, {}) = '{m} < ')",
            m.len() + 1,
            m.len() + 3
        );
        match self.source {
            Some(AuditSource::Panel) => {
                conds.push("action IN (?, ?)".into());
                args.extend(panel);
            }
            Some(AuditSource::Mcp) => {
                conds.push(format!("action NOT IN (?, ?) AND {mcp}"));
                args.extend(panel);
            }
            Some(AuditSource::Cli) => {
                conds.push(format!("action NOT IN (?, ?) AND NOT {mcp}"));
                args.extend(panel);
            }
            None => {}
        }
        if !self.q.is_empty() {
            let like = format!(
                "%{}%",
                self.q
                    .replace('\\', r"\\")
                    .replace('%', r"\%")
                    .replace('_', r"\_")
            );
            conds.push(
                r"(host_name LIKE ? ESCAPE '\' OR detail LIKE ? ESCAPE '\' OR caller LIKE ? ESCAPE '\')"
                    .into(),
            );
            for _ in 0..3 {
                args.push(Value::Text(like.clone()));
            }
        }
        if conds.is_empty() {
            (String::new(), args)
        } else {
            (format!(" WHERE {}", conds.join(" AND ")), args)
        }
    }
}

/// 导出的格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    /// 带 UTF-8 BOM（Excel 直接打开中文不乱码）
    Csv,
    Json,
}

/// 完整的记录（get、export）与列表中的摘要：内容只取开头 [`SUMMARY_LENGTH`] 个字符，最后一列表示还有更多
/// （SQLite 的 substr、length 对文本按字符计）。
const COLUMNS: &str = "id, at, host_id, host_name, action, detail, status, exit_code, \
                       COALESCE(error_code, ''), duration_ms, bytes, caller, 0, approval_wait_ms";

fn summary_columns() -> String {
    format!(
        "id, at, host_id, host_name, action, substr(detail, 1, {SUMMARY_LENGTH}), status, exit_code, \
         COALESCE(error_code, ''), duration_ms, bytes, caller, length(detail) > {SUMMARY_LENGTH}, \
         approval_wait_ms"
    )
}

fn entry_from_row(row: &Row<'_>) -> rusqlite::Result<AuditEntry> {
    let action = AuditAction::parse(&row.get::<_, String>(4)?);
    let caller: String = row.get(11)?;
    Ok(AuditEntry {
        id: row.get(0)?,
        at: row.get(1)?,
        host_id: row.get(2)?,
        host_name: row.get(3)?,
        action,
        detail: row.get(5)?,
        status: AuditStatus::parse(&row.get::<_, String>(6)?),
        exit_code: row.get(7)?,
        error_code: row.get(8)?,
        duration_ms: row.get(9)?,
        approval_wait_ms: row.get(13)?,
        bytes: row.get(10)?,
        source: AuditSource::of(action, &caller),
        caller,
        partial: row.get(12)?,
        truncated: false,
        detail_size: 0,
    })
}

fn query(
    conn: &Connection,
    sql: &str,
    args: Vec<rusqlite::types::Value>,
) -> rusqlite::Result<Vec<AuditEntry>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params_from_iter(args), entry_from_row)?;
    rows.collect()
}

/// 审计日志。在应用中只有一份（`Arc` 共享），命令行服务与主机工具面板都写这里。
pub struct AuditLog {
    store: Arc<Store>,
    inserts: AtomicU64,
    max_entries: usize,
    max_bytes: i64,
}

impl AuditLog {
    pub fn new(store: Arc<Store>) -> Result<Self> {
        Self::with_limits(store, MAX_ENTRIES, MAX_BYTES)
    }

    /// 指定保留上限（测试用）；`max_bytes` 为 0 时只按条数。
    pub(crate) fn with_limits(
        store: Arc<Store>,
        max_entries: usize,
        max_bytes: i64,
    ) -> Result<Self> {
        store.database().write(|tx| {
            tx.execute(ENSURE_TABLE, [])?;
            for (name, definition) in ADDED_COLUMNS {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM pragma_table_info('audit_log') WHERE name = ?1)",
                    [name],
                    |r| r.get(0),
                )?;
                if !exists {
                    tx.execute(
                        &format!("ALTER TABLE audit_log ADD COLUMN {name} {definition}"),
                        [],
                    )?;
                    tracing::info!(column = name, "审计日志：补上新加的列");
                }
            }
            Ok(())
        })?;
        Ok(Self {
            store,
            inserts: AtomicU64::new(0),
            max_entries,
            max_bytes,
        })
    }

    /// 写入一条记录，返回它的 ID。内容超过 [`MAX_DETAIL`] 时只保存开头部分；每写入 100 条清理一次超出上限的旧记录。
    pub fn add(&self, mut entry: AuditEntry) -> Result<i64> {
        if entry.at == 0 {
            entry.at = crate::time::now_ms();
        }
        let detail = clip(entry.detail);
        let host_name = cut(&entry.host_name, MAX_FIELD).to_string();
        let caller = cut(&entry.caller, MAX_FIELD).to_string();
        let id = self.store.database().write(|tx| {
            tx.execute(
                "INSERT INTO audit_log (at, host_id, host_name, action, detail, status, exit_code, error_code, duration_ms, bytes, caller, approval_wait_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULLIF(?8, ''), ?9, ?10, ?11, ?12)",
                params![
                    entry.at,
                    entry.host_id,
                    host_name,
                    entry.action.as_str(),
                    detail,
                    entry.status.as_str(),
                    entry.exit_code,
                    entry.error_code,
                    entry.duration_ms,
                    entry.bytes,
                    caller,
                    entry.approval_wait_ms,
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })?;
        // 第 1、101、201……条写入时清理
        if self
            .inserts
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(100)
        {
            self.prune()?;
        }
        Ok(id)
    }

    /// 删除超出条数上限与总大小上限的最早的记录。两次清理之间最多多出 100 条（约 6.4MB）。
    /// 大小按现有的列计算：从新到旧累加（`octet_length` 只读记录头中的长度，内容在溢出页中的长记录不必读出），
    /// 累计超过上限的那一条及更早的删除。逐条累计要读遍整个表，先看数据库实际使用的大小（不含空闲页），
    /// 没有超过上限时审计记录也不会超过，不必累计：平常（短命令）只有一次很快的查询。
    fn prune(&self) -> Result<()> {
        let max_entries = self.max_entries as i64;
        let max_bytes = self.max_bytes;
        self.store.database().write(|tx| {
            tx.execute(
                "DELETE FROM audit_log WHERE id <= (SELECT id FROM audit_log ORDER BY id DESC LIMIT 1 OFFSET ?1)",
                [max_entries],
            )?;
            if max_bytes <= 0 {
                return Ok(());
            }
            let used: i64 = tx.query_row(
                "SELECT (p.page_count - f.freelist_count) * z.page_size
                 FROM pragma_page_count() p, pragma_freelist_count() f, pragma_page_size() z",
                [],
                |r| r.get(0),
            )?;
            if used <= max_bytes {
                return Ok(());
            }
            tx.execute(
                "DELETE FROM audit_log WHERE id <= (
                   SELECT id FROM (
                     SELECT id, SUM(octet_length(detail) + octet_length(host_name) + octet_length(caller))
                       OVER (ORDER BY id DESC) AS total
                     FROM audit_log
                   ) WHERE total > ?1 ORDER BY id DESC LIMIT 1)",
                [max_bytes],
            )?;
            Ok(())
        })
    }

    /// 按时间倒序返回记录。内容只返回开头的 [`SUMMARY_LENGTH`] 个字符（`partial`），完整内容用 [`AuditLog::get`] 取得。
    pub fn list(&self, q: &AuditQuery) -> Result<Vec<AuditEntry>> {
        let limit = if q.limit == 0 || q.limit > 1000 {
            200
        } else {
            q.limit
        };
        let (cond, mut args) = q.conditions();
        args.push(rusqlite::types::Value::Integer(limit as i64));
        let sql = format!(
            "SELECT {} FROM audit_log{cond} ORDER BY id DESC LIMIT ?",
            summary_columns()
        );
        self.store.database().read(|conn| query(conn, &sql, args))
    }

    /// 一条完整的记录；内容在写入时被截断过的，`truncated` 为真、`detail_size` 为原文的字节数。
    pub fn get(&self, id: i64) -> Result<AuditEntry> {
        let sql = format!("SELECT {COLUMNS} FROM audit_log WHERE id = ?");
        let mut list = self
            .store
            .database()
            .read(|conn| query(conn, &sql, vec![rusqlite::types::Value::Integer(id)]))?;
        let mut entry = list.pop().ok_or(CoreError::NotFound("审计记录"))?;
        entry.unclip();
        Ok(entry)
    }

    /// 删除全部记录。
    pub fn clear(&self) -> Result<()> {
        self.store
            .database()
            .write(|tx| Ok(tx.execute("DELETE FROM audit_log", [])?))?;
        Ok(())
    }

    /// 按查询条件（忽略 `limit` 与 `before`）导出全部匹配的记录，返回条数。截断标记原样保留。
    pub fn export(
        &self,
        out: &mut dyn Write,
        format: ExportFormat,
        q: &AuditQuery,
    ) -> Result<usize> {
        let q = AuditQuery {
            limit: 0,
            before: 0,
            ..q.clone()
        };
        let (cond, args) = q.conditions();
        let sql = format!("SELECT {COLUMNS} FROM audit_log{cond} ORDER BY id");
        let list = self.store.database().read(|conn| query(conn, &sql, args))?;
        match format {
            ExportFormat::Json => {
                serde_json::to_writer_pretty(&mut *out, &list)?;
                out.write_all(b"\n")?;
            }
            ExportFormat::Csv => {
                out.write_all("\u{feff}".as_bytes())?;
                write_csv_row(
                    out,
                    &[
                        "time",
                        "host",
                        "action",
                        "status",
                        "exit_code",
                        "error_code",
                        "duration_ms",
                        "bytes",
                        "caller",
                        "detail",
                        "source",
                        // 0.3.2 加的列放在最后，按位置读的脚本不受影响
                        "approval_wait_ms",
                    ],
                )?;
                for e in &list {
                    let time = rfc3339(e.at);
                    let exit = e.exit_code.map(|c| c.to_string()).unwrap_or_default();
                    let bytes = e.bytes.map(|b| b.to_string()).unwrap_or_default();
                    let duration = e.duration_ms.to_string();
                    let waited = e
                        .approval_wait_ms
                        .map(|ms| ms.to_string())
                        .unwrap_or_default();
                    write_csv_row(
                        out,
                        &[
                            &time,
                            &not_formula(&e.host_name),
                            e.action.as_str(),
                            e.status.as_str(),
                            &exit,
                            &not_formula(&e.error_code),
                            &duration,
                            &bytes,
                            &not_formula(&e.caller),
                            &not_formula(&e.detail),
                            e.source.as_str(),
                            &waited,
                        ],
                    )?;
                }
            }
        }
        out.flush()?;
        Ok(list.len())
    }
}

/// 本地时间的 RFC 3339（到秒）。
fn rfc3339(ms: i64) -> String {
    use chrono::{Local, SecondsFormat, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// 文字字段以 `=` `+` `-` `@`、制表符或回车开头时前面加 `'`：命令可能是 AI 工具产生的（可能被提示注入），
/// 用 Excel 打开导出的 CSV 时不会被当作公式执行。
fn not_formula(text: &str) -> std::borrow::Cow<'_, str> {
    if text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{text}").into()
    } else {
        text.into()
    }
}

/// 写一行 CSV（RFC 4180）：含逗号、引号、换行或以空白开头的字段加引号，引号写两遍。
fn write_csv_row(out: &mut dyn Write, fields: &[&str]) -> std::io::Result<()> {
    let mut line = String::new();
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            line.push(',');
        }
        let quote = field.contains([',', '"', '\r', '\n']) || field.starts_with([' ', '\t']);
        if quote {
            line.push('"');
            line.push_str(&field.replace('"', "\"\""));
            line.push('"');
        } else {
            line.push_str(field);
        }
    }
    line.push('\n');
    out.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::testkit::memory_store;

    fn log() -> AuditLog {
        AuditLog::new(memory_store()).unwrap()
    }

    fn entry(host: &str, action: AuditAction, detail: &str, status: AuditStatus) -> AuditEntry {
        AuditEntry {
            host_id: format!("h_{host}"),
            host_name: host.into(),
            action,
            detail: detail.into(),
            status,
            ..Default::default()
        }
    }

    /// 解析导出的 CSV（够测试用：字段中没有换行）。
    fn parse_csv(text: &str) -> Vec<Vec<String>> {
        text.trim_start_matches('\u{feff}')
            .lines()
            .map(|line| {
                let mut fields = Vec::new();
                let mut field = String::new();
                let mut quoted = false;
                let mut chars = line.chars().peekable();
                while let Some(c) = chars.next() {
                    match (c, quoted) {
                        ('"', true) if chars.peek() == Some(&'"') => {
                            field.push('"');
                            chars.next();
                        }
                        ('"', _) => quoted = !quoted,
                        (',', false) => fields.push(std::mem::take(&mut field)),
                        _ => field.push(c),
                    }
                }
                fields.push(field);
                fields
            })
            .collect()
    }

    #[test]
    fn add_list_export_clear() {
        let log = log();
        let mut uptime = entry("web", AuditAction::Exec, "uptime", AuditStatus::Ok);
        uptime.exit_code = Some(0);
        uptime.caller = "bash < node".into();
        let mut blocked = entry("web", AuditAction::Exec, "rm -rf /", AuditStatus::Blocked);
        blocked.error_code = "command_blocked".into();
        let mut upload = entry(
            "db",
            AuditAction::Upload,
            "C:\\a.txt → /tmp/a.txt",
            AuditStatus::Ok,
        );
        upload.bytes = Some(42);
        let mut grep = entry("db", AuditAction::Exec, "grep 100% x_y", AuditStatus::Ok);
        grep.exit_code = Some(3);
        for e in [uptime, blocked, upload, grep] {
            log.add(e).unwrap();
        }

        let all = log.list(&AuditQuery::default()).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].detail, "grep 100% x_y", "应按时间倒序");
        assert_eq!(all[0].exit_code, Some(3));
        assert_eq!(all[3].caller, "bash < node");
        assert!(all[0].at > 0);
        let blocked = log
            .list(&AuditQuery {
                status: Some(AuditStatus::Blocked),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].error_code, "command_blocked");
        // LIKE 中的 % 与 _ 按字面匹配
        let hits = log
            .list(&AuditQuery {
                q: "100%".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        let hits = log
            .list(&AuditQuery {
                q: "NODE".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1, "应能按调用方搜索（不区分大小写）");
        let page = log
            .list(&AuditQuery {
                limit: 2,
                ..Default::default()
            })
            .unwrap();
        let next = log
            .list(&AuditQuery {
                limit: 2,
                before: page[1].id,
                ..Default::default()
            })
            .unwrap();
        assert_eq!((page.len(), next.len()), (2, 2));
        assert!(next[0].id < page[1].id, "翻页");

        let mut csv = Vec::new();
        let count = log
            .export(
                &mut csv,
                ExportFormat::Csv,
                &AuditQuery {
                    q: "web".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(count, 2);
        let text = String::from_utf8(csv).unwrap();
        assert!(text.starts_with('\u{feff}'), "CSV 应以 UTF-8 BOM 开头");
        let rows = parse_csv(&text);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1][9], "uptime");
        assert_eq!(rows[2][3], "blocked");
        // 公式注入：文字字段不会被表格软件当作公式
        log.add(entry(
            "web",
            AuditAction::Exec,
            "=HYPERLINK(\"http://x\")",
            AuditStatus::Ok,
        ))
        .unwrap();
        let mut csv = Vec::new();
        log.export(&mut csv, ExportFormat::Csv, &AuditQuery::default())
            .unwrap();
        let rows = parse_csv(&String::from_utf8(csv).unwrap());
        assert_eq!(rows.last().unwrap()[9], "'=HYPERLINK(\"http://x\")");
        let mut json = Vec::new();
        log.export(&mut json, ExportFormat::Json, &AuditQuery::default())
            .unwrap();
        assert!(
            String::from_utf8(json)
                .unwrap()
                .contains(r#""detail": "rm -rf /""#)
        );

        log.clear().unwrap();
        assert!(log.list(&AuditQuery::default()).unwrap().is_empty());
    }

    #[test]
    fn retention_by_count() {
        let log = AuditLog::with_limits(memory_store(), 5, MAX_BYTES).unwrap();
        for i in 0..102 {
            let detail = "x".repeat(i % 3 + 1);
            log.add(entry("h", AuditAction::Exec, &detail, AuditStatus::Ok))
                .unwrap();
        }
        // 第 1 条与第 101 条写入时清理，最多保留 5 条 + 之后新写入的
        let got = log
            .list(&AuditQuery {
                limit: 1000,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(got.len(), 6);
    }

    // 内容（按字节，含主机名与调用方）累计超过上限时删除最早的记录
    #[test]
    fn retention_by_size() {
        let log = AuditLog::with_limits(memory_store(), MAX_ENTRIES, 10_000).unwrap();
        // 每条 3000 字节（1000 个汉字）+ 主机名 1 字节
        for i in 0..101 {
            let detail = format!("{i:03}{}", "中".repeat(999));
            log.add(entry("h", AuditAction::Exec, &detail, AuditStatus::Ok))
                .unwrap();
        }
        // 第 101 条写入时清理：从新到旧累计不超过 10000 字节的只有 3 条
        let got = log
            .list(&AuditQuery {
                limit: 1000,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(got.len(), 3);
        assert!(got[0].detail.starts_with("100") && got[2].detail.starts_with("098"));

        // 没有大小上限时只按条数
        let log = AuditLog::with_limits(memory_store(), MAX_ENTRIES, 0).unwrap();
        for _ in 0..103 {
            log.add(entry(
                "h",
                AuditAction::Exec,
                &"中".repeat(1000),
                AuditStatus::Ok,
            ))
            .unwrap();
        }
        let got = log
            .list(&AuditQuery {
                limit: 1000,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(got.len(), 103);
    }

    #[test]
    fn detail_limits() {
        let log = log();
        // 2MB 的脚本，在 MAX_DETAIL 处正好切在一个多字节字符的中间
        let huge = format!(
            "{}中{}",
            "a".repeat(MAX_DETAIL - 1),
            "echo 你好\n".repeat(200_000)
        );
        let long = "x".repeat(SUMMARY_LENGTH + 1);
        let mut big = entry("", AuditAction::Exec, &huge, AuditStatus::Ok);
        big.host_name = "h".repeat(5000);
        big.caller = "c".repeat(5000);
        for e in [
            big,
            entry("web", AuditAction::Exec, &long, AuditStatus::Ok),
            entry("web", AuditAction::Exec, "uptime", AuditStatus::Ok),
        ] {
            log.add(e).unwrap();
        }

        let list = log.list(&AuditQuery::default()).unwrap();
        assert_eq!(list.len(), 3);
        assert!(
            list[0].detail == "uptime" && !list[0].partial,
            "短内容应原样返回"
        );
        assert!(
            list[1].detail == long[..SUMMARY_LENGTH] && list[1].partial,
            "长内容在列表中只返回开头并标记 partial"
        );
        assert!(list[2].partial && list[2].detail.chars().count() == SUMMARY_LENGTH);
        assert!(list[2].host_name.len() <= 1024 && list[2].caller.len() <= 1024);

        let full = log.get(list[2].id).unwrap();
        assert!(full.truncated && !full.partial);
        assert_eq!(full.detail_size, huge.len() as i64);
        assert!(full.detail.len() <= MAX_DETAIL && huge.starts_with(&full.detail));
        let got = log.get(list[1].id).unwrap();
        assert!(
            got.detail == long && !got.truncated,
            "没有截断的记录应原样返回"
        );

        // 命令本身以截断标记的文字结尾时不算截断
        let fake = "echo hi\n…[termlet: truncated, 999999 bytes in total]";
        log.add(entry("web", AuditAction::Exec, fake, AuditStatus::Ok))
            .unwrap();
        let latest = log
            .list(&AuditQuery {
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        let got = log.get(latest[0].id).unwrap();
        assert!(got.detail == fake && !got.truncated);
        assert!(matches!(log.get(123_456), Err(CoreError::NotFound(_))));

        // 导出保留截断标记
        let mut json = Vec::new();
        log.export(&mut json, ExportFormat::Json, &AuditQuery::default())
            .unwrap();
        let text = String::from_utf8(json).unwrap();
        assert!(text.contains(&format!(
            "[termlet: truncated, {} bytes in total]",
            huge.len()
        )));
    }

    // MCP 服务的标记在调用方的第一项：mcp:<客户端名>、没有客户端名时只有 mcp；进程名中间带 mcp 的不算
    #[test]
    fn mcp_client_comes_from_the_first_caller() {
        assert_eq!(
            mcp_client("mcp:claude-code < claude.exe"),
            Some("claude-code")
        );
        assert_eq!(mcp_client("mcp:cursor"), Some("cursor"));
        assert_eq!(mcp_client("mcp < node.exe"), Some(""));
        assert_eq!(mcp_client("mcp"), Some(""));
        for caller in [
            "",
            "bash < node",
            "mcpd < bash",
            "bash < mcp:x",
            "mcp-server",
        ] {
            assert_eq!(mcp_client(caller), None, "{caller}");
        }
        // 经 HTTP 的调用：第二项是 http
        assert!(mcp_over_http("mcp:cursor < http < Cursor.exe"));
        assert!(mcp_over_http("mcp < http"));
        assert_eq!(mcp_client("mcp:cursor < http < Cursor.exe"), Some("cursor"));
        for caller in ["mcp:cursor < Cursor.exe", "bash < http", "http", "mcp:x"] {
            assert!(!mcp_over_http(caller), "{caller}");
        }
        assert_eq!(
            AuditSource::of(AuditAction::Exec, "mcp:codex < http"),
            AuditSource::Mcp
        );
        assert_eq!(
            AuditSource::of(AuditAction::Exec, "mcp:codex"),
            AuditSource::Mcp
        );
        assert_eq!(
            AuditSource::of(AuditAction::Panel, "mcp:codex"),
            AuditSource::Panel
        );
        assert_eq!(
            AuditSource::of(AuditAction::Upload, "bash"),
            AuditSource::Cli
        );
    }

    // 主机工具面板的写操作、MCP 与命令行的记录可以分开筛选、导出（来源由动作与调用方得出，旧记录同样适用）
    #[test]
    fn source_is_derived_from_action() {
        let log = log();
        let mut exec = entry("web", AuditAction::Exec, "uptime", AuditStatus::Ok);
        exec.exit_code = Some(0);
        exec.caller = "bash".into();
        let mut mcp = entry("web", AuditAction::Exec, "df -h", AuditStatus::Ok);
        mcp.caller = "mcp:claude-code < claude.exe".into();
        let mut anonymous = entry("db", AuditAction::Upload, "C:\\a → /tmp/a", AuditStatus::Ok);
        anonymous.caller = "mcp < node.exe".into();
        log.add(mcp).unwrap();
        log.add(anonymous).unwrap();
        let mut panel = entry(
            "web",
            AuditAction::Panel,
            "systemctl restart -- nginx.service",
            AuditStatus::Ok,
        );
        panel.exit_code = Some(0);
        for e in [
            exec,
            panel,
            entry(
                "web",
                AuditAction::PanelTerminal,
                "sudo kill -TERM 1  # systemd",
                AuditStatus::Ok,
            ),
            entry(
                "db",
                AuditAction::Download,
                "/etc/hosts → C:\\hosts",
                AuditStatus::Failed,
            ),
        ] {
            log.add(e).unwrap();
        }
        let all = log.list(&AuditQuery::default()).unwrap();
        for e in &all {
            let want = match e.action {
                AuditAction::Panel | AuditAction::PanelTerminal => AuditSource::Panel,
                _ if e.caller.starts_with("mcp") => AuditSource::Mcp,
                _ => AuditSource::Cli,
            };
            assert_eq!(e.source, want, "{e:?}");
        }
        let mcp = log
            .list(&AuditQuery {
                source: Some(AuditSource::Mcp),
                ..Default::default()
            })
            .unwrap();
        let details: Vec<&str> = mcp.iter().map(|e| e.detail.as_str()).collect();
        assert_eq!(details, ["C:\\a → /tmp/a", "df -h"]);
        let panel = log
            .list(&AuditQuery {
                source: Some(AuditSource::Panel),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(panel.len(), 2);
        assert_eq!(panel[0].action, AuditAction::PanelTerminal);
        let cli = log
            .list(&AuditQuery {
                source: Some(AuditSource::Cli),
                q: "web".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(cli.len() == 1 && cli[0].detail == "uptime");
        assert_eq!(log.get(all[0].id).unwrap().source, AuditSource::Cli);
        assert_eq!(log.get(mcp[1].id).unwrap().source, AuditSource::Mcp);
        let mut csv = Vec::new();
        log.export(
            &mut csv,
            ExportFormat::Csv,
            &AuditQuery {
                source: Some(AuditSource::Mcp),
                ..Default::default()
            },
        )
        .unwrap();
        let rows = parse_csv(&String::from_utf8(csv).unwrap());
        assert_eq!((rows.len(), rows[1][10].as_str()), (3, "mcp"));

        let mut csv = Vec::new();
        let n = log
            .export(
                &mut csv,
                ExportFormat::Csv,
                &AuditQuery {
                    source: Some(AuditSource::Panel),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(n, 2);
        let rows = parse_csv(&String::from_utf8(csv).unwrap());
        assert_eq!(rows[0][10], "source");
        assert_eq!(rows[1][10], "panel");
        assert_eq!(rows[1][9], "systemctl restart -- nginx.service");
    }

    #[test]
    fn summary_keeps_the_beginning() {
        let long = entry("web", AuditAction::Exec, &"名".repeat(600), AuditStatus::Ok);
        let summary = long.summary();
        assert!(summary.partial);
        assert_eq!(summary.detail.chars().count(), SUMMARY_LENGTH);
        assert_eq!(summary.host_name, "web");
        let short = entry("web", AuditAction::Exec, "ls", AuditStatus::Ok).summary();
        assert!(!short.partial && short.detail == "ls");
    }

    // JSON 中的写法（界面与导出用）
    #[test]
    fn json_uses_camel_case_and_snake_case_values() {
        let e = AuditEntry {
            action: AuditAction::PanelTerminal,
            status: AuditStatus::Approved,
            exit_code: Some(0),
            ..Default::default()
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["action"], "panel_terminal");
        assert_eq!(v["status"], "approved");
        assert_eq!(v["exitCode"], 0);
        assert!(v.get("bytes").is_none() && v.get("partial").is_none());
        let back: AuditEntry =
            serde_json::from_value(serde_json::json!({"action": "teleport", "status": "?"}))
                .unwrap();
        assert_eq!(
            (back.action, back.status),
            (AuditAction::Other, AuditStatus::Other)
        );
    }

    // 等待批准的时长单独记，用时不含它；列表、详情、导出都带上，没有询问过用户的为空
    #[test]
    fn approval_wait_is_kept_apart() {
        let log = log();
        let mut approved = entry(
            "web",
            AuditAction::Exec,
            "rm -rf /tmp/x",
            AuditStatus::Approved,
        );
        approved.duration_ms = 40;
        approved.approval_wait_ms = Some(31_000);
        log.add(approved).unwrap();
        log.add(entry("web", AuditAction::Exec, "ls", AuditStatus::Ok))
            .unwrap();
        let list = log.list(&AuditQuery::default()).unwrap();
        assert_eq!(list[0].approval_wait_ms, None);
        assert_eq!(
            (list[1].duration_ms, list[1].approval_wait_ms),
            (40, Some(31_000))
        );
        assert_eq!(log.get(list[1].id).unwrap().approval_wait_ms, Some(31_000));

        let mut csv = Vec::new();
        log.export(&mut csv, ExportFormat::Csv, &AuditQuery::default())
            .unwrap();
        let rows = parse_csv(&String::from_utf8(csv).unwrap());
        assert_eq!(rows[0][6], "duration_ms");
        assert_eq!(rows[0][11], "approval_wait_ms", "加在最后一列");
        assert_eq!((rows[1][6].as_str(), rows[1][11].as_str()), ("40", "31000"));
        assert_eq!(rows[2][11], "");
        let mut json = Vec::new();
        log.export(&mut json, ExportFormat::Json, &AuditQuery::default())
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v[0]["approvalWaitMs"], 31_000);
        assert!(v[1].get("approvalWaitMs").is_none());
    }

    // 0.3.1 及更早的数据库没有 approval_wait_ms：打开时补上，已有的记录照常读出（没有等待的时长）；再打开不重复加
    #[test]
    fn added_columns_are_filled_in_for_old_databases() {
        let store = memory_store();
        store
            .database()
            .write(|tx| {
                tx.execute("DROP TABLE audit_log", [])?;
                tx.execute(ENSURE_TABLE, [])?;
                tx.execute(
                    "INSERT INTO audit_log (host_name, action, detail, status, duration_ms, caller)
                     VALUES ('web', 'exec', 'rm -rf /tmp/x', 'approved', 31393, 'mcp:claude-code')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let log = AuditLog::new(store.clone()).unwrap();
        let old = log.list(&AuditQuery::default()).unwrap();
        assert_eq!(old.len(), 1);
        assert_eq!((old[0].duration_ms, old[0].approval_wait_ms), (31393, None));
        assert_eq!(old[0].source, AuditSource::Mcp);
        let again = AuditLog::new(store).unwrap();
        let mut new = entry("web", AuditAction::Exec, "ls", AuditStatus::Approved);
        new.approval_wait_ms = Some(5);
        again.add(new).unwrap();
        assert_eq!(
            again.list(&AuditQuery::default()).unwrap()[0].approval_wait_ms,
            Some(5)
        );
    }

    // 加入这张表之前创建的数据库（没有 audit_log）：打开时补上；与新数据库打开后的表结构相同（结构版本 1 中的表
    // 加上之后补的列）
    #[test]
    fn table_is_ensured_and_matches_schema() {
        let columns = |db: &Database| {
            db.read(|c| {
                let mut stmt = c.prepare("SELECT name, type, \"notnull\", dflt_value FROM pragma_table_info('audit_log')")?;
                let rows = stmt.query_map([], |r| {
                    Ok(format!(
                        "{} {} {} {:?}",
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?
                    ))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
        };
        let fresh = memory_store();
        assert!(
            !columns(fresh.database()).is_empty(),
            "结构版本 1 应有 audit_log"
        );
        AuditLog::new(fresh.clone()).unwrap();
        let schema = columns(fresh.database());
        assert!(
            schema.iter().any(|c| c.starts_with("approval_wait_ms ")),
            "{schema:?}"
        );
        fresh
            .database()
            .write(|tx| Ok(tx.execute("DROP TABLE audit_log", [])?))
            .unwrap();
        let log = AuditLog::new(fresh.clone()).unwrap();
        assert_eq!(columns(fresh.database()), schema);
        log.add(entry("h", AuditAction::Exec, "ls", AuditStatus::Ok))
            .unwrap();
    }
}
