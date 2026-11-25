//! MCP 的工具：定义（名称、说明、输入的 JSON Schema、行为提示）、参数检查、发给 Termlet 的请求与结果的写法。
//!
//! 说明与 Schema 用英文写（给 AI 看）；参数错误、失败的说明与命令行工具一样跟随语言（简体中文或英文），并带上固定的
//! 英文错误码（与 `termlet` 的 JSON 输出中的 `error` 相同）。
//!
//! 一次调用分三步：[`request`] 检查参数、写成发给 Termlet 的请求（与命令行工具的请求相同）；交给 Termlet；
//! [`outcome`] 把回答写成工具的结果。`termlet mcp` 用 [`call`] 经 [`Backend`] 同步地走完三步，应用内的 HTTP 服务
//! 在异步运行时上直接处理请求，只用第一步与第三步。

use std::borrow::Cow;
use std::path::Path;

use serde_json::{Map, Value, json};
use termlet_proto::{
    BatchAction, BatchReport, BatchRequest, BatchStatus, ErrorCode, ExecRequest, ExecResponse,
    HostSummary, Lang, MAX_STDIN_COMMAND, OUTPUT_LIMIT, Request, Response, Selector,
    TransferRequest, TransferResponse,
};

use crate::backend::{AbortSlot, Backend, Failure};
use crate::protocol::Reach;
use crate::text::{self as messages, upload_source_problem};

/// 执行命令时返回给 AI 的输出上限（标准输出与标准错误合计，字节），超出时保留开头与结尾。Termlet 本身每路最多返回
/// 1MB（[`OUTPUT_LIMIT`]）；Claude Code 在工具结果超过约 1 万 token 时提醒，32KB 的文本大致在这以内。
pub const OUTPUT_BUDGET: usize = 32 * 1024;

/// 截断时开头保留的比例（其余留给结尾：错误信息、命令的结果通常在最后）。
const HEAD_SHARE: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    ListHosts,
    RunCommand,
    RunCommandOnHosts,
    UploadFile,
    DownloadFile,
}

impl Tool {
    /// `tools/list` 中的顺序。
    const ALL: [Tool; 5] = [
        Tool::ListHosts,
        Tool::RunCommand,
        Tool::RunCommandOnHosts,
        Tool::UploadFile,
        Tool::DownloadFile,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Tool::ListHosts => "list_hosts",
            Tool::RunCommand => "run_command",
            Tool::RunCommandOnHosts => "run_command_on_hosts",
            Tool::UploadFile => "upload_file",
            Tool::DownloadFile => "download_file",
        }
    }

    pub fn parse(name: &str) -> Option<Tool> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    /// 读写运行 Termlet 的这台电脑上的文件（本机路径）：局域网中的客户端不能用。
    pub fn local_files(self) -> bool {
        matches!(self, Tool::UploadFile | Tool::DownloadFile)
    }

    fn title(self) -> &'static str {
        match self {
            Tool::ListHosts => "List Termlet hosts",
            Tool::RunCommand => "Run a command on a host",
            Tool::RunCommandOnHosts => "Run a command on several hosts",
            Tool::UploadFile => "Upload a file to a host",
            Tool::DownloadFile => "Download a file from a host",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Tool::ListHosts => {
                "List the servers saved in Termlet that the user allows AI tools to use. Each entry has the host's id, name, address, port, login user and protocol, never a password or key. Pass the id or name as `host` to the other tools. A server that is missing here needs \"Allow AI tools and scripts to use this host\" turned on in Termlet first."
            }
            Tool::RunCommand => {
                "Run a shell command on a server saved in Termlet (same as `termlet exec`) and return its exit code, stdout and stderr. Termlet logs in with the credentials it keeps; you never see them. Each call is a new SSH session, so nothing carries over between calls: put related steps in one command (for example `cd /srv/app && git pull`). Interactive programs and password prompts don't work; use non-interactive options such as `-y`. Commands that match the user's dangerous-command rules wait for the user to approve them in Termlet (up to 60 seconds); on command_denied or command_blocked, don't try to get around the rule, ask the user to run it themselves. Output is capped at 32 KB (the beginning and the end are kept), so redirect long output to a file and read it in parts."
            }
            Tool::RunCommandOnHosts => {
                "Run the same shell command on several servers saved in Termlet at once (same as `termlet exec -g/-t/-H`) and get the results merged: hosts with the same exit code and output are grouped, so many servers that agree take the space of one. Choose hosts with `hosts` (ids or names), `groups` (group names or paths such as `prod/web`, subgroups included), `tags`, or `all`; `exclude` removes hosts. Hosts the user hasn't allowed for AI tools are listed as not run. Each host gets a new SSH session (up to 10 at a time), so put related steps in one command. If the command matches the user's dangerous-command rules, the user approves the whole batch once in Termlet (up to 60 seconds). Use `timeout_seconds` to stop hosts that hang. Output per group is clipped so the whole result stays around 32 KB."
            }
            Tool::UploadFile => {
                "Upload one file from this computer to a server saved in Termlet over SFTP, or SCP when the server has no SFTP (same as `termlet upload`, scp semantics). `local_path` must be an absolute path. If `remote_path` is an existing directory or ends with `/`, the file keeps its name inside it; otherwise `remote_path` is the new file's path. An existing file is overwritten. Directories aren't supported; archive them first (for example with tar)."
            }
            Tool::DownloadFile => {
                "Download one file from a server saved in Termlet to this computer over SFTP, or SCP when the server has no SFTP (same as `termlet download`). `local_path` must be an absolute path; if it is an existing directory, the file keeps its name inside it, otherwise `local_path` is the new file's path. An existing local file is overwritten. Directories aren't supported; archive them on the server first."
            }
        }
    }

    /// 参数名（不认识的参数报错，免得 AI 写错了名字却以为生效了）。
    fn arguments(self) -> &'static [&'static str] {
        match self {
            Tool::ListHosts => &["query"],
            Tool::RunCommand => &["host", "command", "terminal"],
            Tool::RunCommandOnHosts => &[
                "hosts",
                "groups",
                "tags",
                "exclude",
                "all",
                "command",
                "timeout_seconds",
            ],
            Tool::UploadFile => &["host", "local_path", "remote_path"],
            Tool::DownloadFile => &["host", "remote_path", "local_path"],
        }
    }

    fn input_schema(self) -> Value {
        let host = json!({
            "type": "string",
            "description": "ID or name of a host from list_hosts.",
        });
        match self {
            Tool::ListHosts => json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Optional keyword that filters by name, address, user, tag or ID (case-insensitive).",
                    },
                },
                "additionalProperties": false,
            }),
            Tool::RunCommand => json!({
                "type": "object",
                "properties": {
                    "host": host,
                    "command": {
                        "type": "string",
                        "description": "Command line or multi-line script, run by the server's login shell (up to 1 MB).",
                    },
                    "terminal": {
                        "type": "boolean",
                        "default": false,
                        "description": "Run it in the terminal the user already has open for this host in Termlet instead of a new connection: the user sees it, and it inherits that shell's current directory and environment. Use it for servers behind bastion hosts that refuse new connections. Needs bash or sh on the server; stdout and stderr come back together.",
                    },
                },
                "required": ["host", "command"],
                "additionalProperties": false,
            }),
            Tool::RunCommandOnHosts => {
                let names = |what: &str| {
                    json!({
                        "type": "array",
                        "items": { "type": "string" },
                        "description": what,
                    })
                };
                json!({
                    "type": "object",
                    "properties": {
                        "hosts": names("IDs or names of hosts from list_hosts."),
                        "groups": names("Host group names or paths (prod/web); subgroups are included. Combined with tags, a host must be in one of the groups and have one of the tags."),
                        "tags": names("Host tags (case-insensitive)."),
                        "exclude": names("IDs or names of hosts to leave out."),
                        "all": {
                            "type": "boolean",
                            "default": false,
                            "description": "Every host the user allows AI tools to use (instead of groups and tags).",
                        },
                        "command": {
                            "type": "string",
                            "description": "Command line or multi-line script, run by each server's login shell (up to 1 MB).",
                        },
                        "timeout_seconds": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Optional time limit per host (connection not included); a host over it gets SIGTERM and is reported as timed out.",
                        },
                    },
                    "required": ["command"],
                    "additionalProperties": false,
                })
            }
            Tool::UploadFile => json!({
                "type": "object",
                "properties": {
                    "host": host,
                    "local_path": {
                        "type": "string",
                        "description": "Absolute path of the file on this computer.",
                    },
                    "remote_path": {
                        "type": "string",
                        "description": "Destination on the server: an existing directory (or a path ending with /) to keep the file name, or the full path of the new file.",
                    },
                },
                "required": ["host", "local_path", "remote_path"],
                "additionalProperties": false,
            }),
            Tool::DownloadFile => json!({
                "type": "object",
                "properties": {
                    "host": host,
                    "remote_path": {
                        "type": "string",
                        "description": "Path of the file on the server.",
                    },
                    "local_path": {
                        "type": "string",
                        "description": "Absolute destination on this computer: an existing directory to keep the file name, or the full path of the new file.",
                    },
                },
                "required": ["host", "remote_path", "local_path"],
                "additionalProperties": false,
            }),
        }
    }

    /// 行为提示（客户端据此决定是否先问用户；只是提示，真正的检查在 Termlet 中）。
    fn annotations(self) -> Value {
        let (read_only, destructive, idempotent, open_world) = match self {
            Tool::ListHosts => (true, false, true, false),
            Tool::RunCommand | Tool::RunCommandOnHosts => (false, true, false, true),
            // 覆盖已有的文件
            Tool::UploadFile | Tool::DownloadFile => (false, true, true, true),
        };
        json!({
            "title": self.title(),
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": open_world,
        })
    }

    fn definition(self, annotations: bool, title: bool) -> Value {
        let mut tool = json!({
            "name": self.name(),
            "description": self.description(),
            "inputSchema": self.input_schema(),
        });
        if title {
            tool["title"] = json!(self.title());
        }
        if annotations {
            tool["annotations"] = self.annotations();
        }
        tool
    }
}

/// `tools/list` 中的工具：局域网中的客户端没有传文件的工具。`annotations`、`title` 按协议版本（较早的版本没有
/// 这些字段）。
pub fn list(reach: Reach, annotations: bool, title: bool) -> Value {
    Value::Array(
        Tool::ALL
            .into_iter()
            .filter(|tool| reach == Reach::Local || !tool.local_files())
            .map(|tool| tool.definition(annotations, title))
            .collect(),
    )
}

/// 局域网中的客户端调用了传文件的工具：工具执行错误，说明小的文本文件可以用 `run_command` 读写。
pub fn lan_unavailable(tool: Tool, lang: Lang) -> Outcome {
    let text = messages::MCP_LAN_UNAVAILABLE.fill(lang, &[&tool.name()]);
    let code = messages::CODE_LAN_UNAVAILABLE;
    Outcome {
        text: format!("[{code}] {text}"),
        structured: Some(json!({ "error": code, "message": text })),
        is_error: true,
    }
}

/// 一次工具调用用到的东西。
pub struct Context<'a> {
    pub backend: &'a dyn Backend,
    pub lang: Lang,
    /// 审计与批准框中的调用方（`mcp:<客户端名>` 与祖先进程）
    pub callers: &'a [String],
    pub slot: &'a AbortSlot,
}

/// 工具的结果。
#[derive(Debug, PartialEq)]
pub struct Outcome {
    text: String,
    /// `structuredContent`（较早的协议版本不发）；执行命令的结果没有：只看它的客户端会看不到输出
    structured: Option<Value>,
    is_error: bool,
}

impl Outcome {
    fn ok(text: String, structured: Option<Value>) -> Self {
        Self {
            text,
            structured,
            is_error: false,
        }
    }

    /// 失败：错误码（固定英文）与按语言写的说明，与 `termlet` 的 JSON 输出中的 `error`、`message` 相同
    /// （提到命令行用法的换成对应的工具）。
    fn error(code: &str, message: &str, lang: Lang) -> Self {
        let text = if ErrorCode::parse(code) == Some(ErrorCode::HostNotFound) {
            let hint = messages::MCP_HOST_NOT_FOUND.get(lang);
            if message.is_empty() {
                hint.to_string()
            } else {
                format!("{hint}: {message}")
            }
        } else {
            messages::human_text(lang, code, message)
        };
        Self {
            text: format!("[{code}] {text}"),
            structured: Some(json!({ "error": code, "message": text })),
            is_error: true,
        }
    }

    /// 参数不对（AI 可以改了重试）。
    fn invalid(message: String, lang: Lang) -> Self {
        Self::error(ErrorCode::BadRequest.as_str(), &message, lang)
    }

    /// 是否是工具执行错误（`isError`）。
    pub fn is_error(&self) -> bool {
        self.is_error
    }

    /// `CallToolResult`（不含 `resultType`、`_meta`）。
    pub fn into_result(self, structured: bool) -> Map<String, Value> {
        let mut result = Map::new();
        result.insert(
            "content".into(),
            json!([{ "type": "text", "text": self.text }]),
        );
        result.insert("isError".into(), json!(self.is_error));
        if structured && let Some(value) = self.structured {
            result.insert("structuredContent".into(), value);
        }
        result
    }
}

/// 调用一个工具（stdio：客户端总在这台电脑上）：检查参数，把请求交给 Termlet，写出结果。
pub fn call(tool: Tool, arguments: &Map<String, Value>, context: &Context<'_>) -> Outcome {
    let lang = context.lang;
    let request = match request(tool, arguments, lang, context.callers, Reach::Local) {
        Ok(request) => request,
        Err(outcome) => return outcome,
    };
    let terminal = in_terminal(&request);
    outcome(
        tool,
        terminal,
        context.backend.call(request, context.slot),
        lang,
        Reach::Local,
    )
}

/// 请求是否在已打开的终端中执行（结果中两路输出合在一起）。
pub fn in_terminal(request: &Request) -> bool {
    matches!(request, Request::Exec(r) if r.terminal)
}

/// Termlet 的回答（或连不上的原因）写成工具的结果。`terminal` 见 [`in_terminal`]；`reach` 是客户端在哪里（局域网中的
/// 客户端没有传文件的工具，说明中不提它们）。
pub fn outcome(
    tool: Tool,
    terminal: bool,
    response: Result<Response, Failure>,
    lang: Lang,
    reach: Reach,
) -> Outcome {
    let response = match response {
        Ok(Response::Error(body)) => return Outcome::error(&body.code, &body.message, lang),
        Ok(response) => response,
        Err(failure) => return Outcome::error(&failure.code, &failure.message, lang),
    };
    match (tool, response) {
        (Tool::ListHosts, Response::Hosts(hosts)) => hosts_outcome(&hosts, lang),
        (Tool::RunCommand, Response::Exec(exec)) => {
            Outcome::ok(exec_text(&exec, terminal, lang, reach), None)
        }
        (Tool::RunCommandOnHosts, Response::BatchDone(report)) => {
            Outcome::ok(batch_text(&report, lang), None)
        }
        (Tool::UploadFile, Response::Transfer(done)) => transfer_outcome(&done, true, lang),
        (Tool::DownloadFile, Response::Transfer(done)) => transfer_outcome(&done, false, lang),
        (_, other) => Outcome::error(
            ErrorCode::Internal.as_str(),
            &messages::PROTOCOL.fill(lang, &[&format!("{other:?}")]),
            lang,
        ),
    }
}

/// 检查参数，写成发给 Termlet 的请求（与命令行工具的请求相同，调用方换成 MCP 的：`callers`）。参数不对时为
/// 工具执行错误（AI 可以改了重试），不发请求；`reach` 见 [`outcome`]。
pub fn request(
    tool: Tool,
    arguments: &Map<String, Value>,
    lang: Lang,
    callers: &[String],
    reach: Reach,
) -> Result<Request, Outcome> {
    let args = Args {
        map: arguments,
        lang,
    };
    args.only(tool.arguments())?;
    let caller = callers.to_vec();
    Ok(match tool {
        Tool::ListHosts => Request::ListHosts {
            query: args.optional_string("query")?.unwrap_or_default(),
        },
        Tool::RunCommand => {
            let host = args.required("host")?;
            let command = args.required("command")?;
            // 与 `exec --stdin` 相同：过长的脚本不截断执行
            if command.len() > MAX_STDIN_COMMAND {
                let message = match reach {
                    Reach::Local => messages::MCP_COMMAND_TOO_LARGE,
                    Reach::Lan => messages::MCP_COMMAND_TOO_LARGE_LAN,
                };
                return Err(Outcome::invalid(message.get(lang).into(), lang));
            }
            Request::Exec(ExecRequest {
                host,
                command,
                terminal: args.flag("terminal")?,
                caller,
                ..Default::default()
            })
        }
        Tool::RunCommandOnHosts => {
            let command = args.required("command")?;
            if command.len() > MAX_STDIN_COMMAND {
                let message = match reach {
                    Reach::Local => messages::MCP_COMMAND_TOO_LARGE,
                    Reach::Lan => messages::MCP_COMMAND_TOO_LARGE_LAN,
                };
                return Err(Outcome::invalid(message.get(lang).into(), lang));
            }
            let selector = Selector {
                hosts: args.string_list("hosts")?,
                groups: args.string_list("groups")?,
                tags: args.string_list("tags")?,
                exclude: args.string_list("exclude")?,
                all: args.flag("all")?,
            };
            if selector.is_empty() {
                return Err(Outcome::invalid(
                    messages::MCP_NO_HOSTS_SELECTED.get(lang).into(),
                    lang,
                ));
            }
            let timeout = args.optional_seconds("timeout_seconds")?;
            Request::Batch(BatchRequest {
                selector,
                action: BatchAction::Exec {
                    command,
                    stdin: String::new(),
                    stdin_script: false,
                },
                parallel: 0,
                timeout_ms: timeout.map(|s| s.saturating_mul(1000)),
                max_fail: None,
                stream: false,
                caller,
            })
        }
        Tool::UploadFile => {
            let host = args.required("host")?;
            let local = args.absolute_path("local_path")?;
            let remote = args.required("remote_path")?;
            if let Some((code, message)) = upload_source_problem(&local, lang) {
                let code = if code == messages::CODE_USAGE {
                    ErrorCode::BadRequest.as_str()
                } else {
                    code
                };
                return Err(Outcome::error(code, &message, lang));
            }
            Request::Upload(TransferRequest {
                host,
                local,
                remote,
                caller,
            })
        }
        Tool::DownloadFile => Request::Download(TransferRequest {
            host: args.required("host")?,
            remote: args.required("remote_path")?,
            local: args.absolute_path("local_path")?,
            caller,
        }),
    })
}

/// 工具的参数（JSON 对象）。`null` 当作没有给。
struct Args<'a> {
    map: &'a Map<String, Value>,
    lang: Lang,
}

impl Args<'_> {
    fn invalid(&self, message: String) -> Outcome {
        Outcome::invalid(message, self.lang)
    }

    /// 只有这些参数。
    fn only(&self, allowed: &[&str]) -> Result<(), Outcome> {
        match self.map.keys().find(|key| !allowed.contains(&key.as_str())) {
            Some(key) => Err(self
                .invalid(messages::MCP_ARG_UNKNOWN.fill(self.lang, &[key, &allowed.join(", ")]))),
            None => Ok(()),
        }
    }

    fn optional_string(&self, key: &str) -> Result<Option<String>, Outcome> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(self.invalid(messages::MCP_ARG_STRING.fill(self.lang, &[&key]))),
        }
    }

    /// 必需、不为空白的字符串（原样返回，不去掉空白）。
    fn required(&self, key: &str) -> Result<String, Outcome> {
        match self.optional_string(key)? {
            None => Err(self.invalid(messages::MCP_ARG_MISSING.fill(self.lang, &[&key]))),
            Some(value) if value.trim().is_empty() => {
                Err(self.invalid(messages::MCP_ARG_EMPTY.fill(self.lang, &[&key])))
            }
            Some(value) => Ok(value),
        }
    }

    /// 本机路径：必须是绝对路径（MCP 服务的当前目录由 AI 工具决定，相对路径的意思不明确）。
    fn absolute_path(&self, key: &str) -> Result<String, Outcome> {
        let path = self.required(key)?;
        if !Path::new(&path).is_absolute() {
            return Err(self.invalid(messages::MCP_NOT_ABSOLUTE.fill(self.lang, &[&key, &path])));
        }
        Ok(path)
    }

    /// 字符串的列表（也接受单个字符串）；没有给时为空。空白的项去掉。
    fn string_list(&self, key: &str) -> Result<Vec<String>, Outcome> {
        let invalid = || self.invalid(messages::MCP_ARG_STRING_LIST.fill(self.lang, &[&key]));
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::String(one)) => Ok(Some(one.trim().to_string())
                .filter(|s| !s.is_empty())
                .into_iter()
                .collect()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Value::String(text) => Ok(text.trim().to_string()),
                    _ => Err(invalid()),
                })
                .filter(|item| !matches!(item, Ok(text) if text.is_empty()))
                .collect(),
            Some(_) => Err(invalid()),
        }
    }

    /// 正整数的秒数；没有给时为 `None`。
    fn optional_seconds(&self, key: &str) -> Result<Option<u64>, Outcome> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => match n.as_u64().filter(|n| *n > 0) {
                Some(n) => Ok(Some(n)),
                None => Err(self.invalid(messages::MCP_ARG_SECONDS.fill(self.lang, &[&key]))),
            },
            Some(_) => Err(self.invalid(messages::MCP_ARG_SECONDS.fill(self.lang, &[&key]))),
        }
    }

    fn flag(&self, key: &str) -> Result<bool, Outcome> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            Some(_) => Err(self.invalid(messages::MCP_ARG_BOOL.fill(self.lang, &[&key]))),
        }
    }
}

fn hosts_outcome(hosts: &[HostSummary], lang: Lang) -> Outcome {
    let text = if hosts.is_empty() {
        messages::NO_HOSTS.get(lang).to_string()
    } else {
        serde_json::to_string(hosts).unwrap_or_default()
    };
    Outcome::ok(text, Some(json!({ "hosts": hosts })))
}

fn transfer_outcome(done: &TransferResponse, upload: bool, lang: Lang) -> Outcome {
    let (message, target) = if upload {
        (messages::UPLOADED, &done.remote)
    } else {
        (messages::DOWNLOADED, &done.local)
    };
    let mut text = message.fill(
        lang,
        &[
            &messages::format_bytes(done.bytes),
            target,
            &messages::format_duration(done.duration_ms, lang),
        ],
    );
    if done.protocol == termlet_proto::TRANSFER_SCP {
        text.push('\n');
        text.push_str(messages::VIA_SCP.get(lang));
    }
    for note in messages::expiring_notes(&done.expiring_certificates, lang) {
        text.push('\n');
        text.push_str(&note);
    }
    Outcome::ok(text, serde_json::to_value(done).ok())
}

/// 执行结果的文本：退出码与用时（等用户批准过的另说明等了多久），标准输出、标准错误（太长时只留开头与结尾），
/// 截断的说明（局域网中的客户端不提 download_file），登录用的证书快过期的提醒。
fn exec_text(exec: &ExecResponse, terminal: bool, lang: Lang, reach: Reach) -> String {
    let (out_limit, err_limit) = budgets(exec.stdout.len(), exec.stderr.len(), OUTPUT_BUDGET);
    let (stdout, out_omitted) = clip(&exec.stdout, out_limit);
    let (stderr, err_omitted) = clip(&exec.stderr, err_limit);
    let took = messages::format_duration(exec.duration_ms, lang);
    let mut text = match exec.approval_wait_ms {
        Some(waited) => messages::MCP_EXIT_APPROVED.fill(
            lang,
            &[
                &exec.exit_code,
                &took,
                &messages::format_duration(waited, lang),
            ],
        ),
        None => messages::MCP_EXIT.fill(lang, &[&exec.exit_code, &took]),
    };
    text.push('\n');
    if stdout.is_empty() && stderr.is_empty() {
        text.push_str(messages::MCP_NO_OUTPUT.get(lang));
        text.push('\n');
    }
    // 在终端中执行时两路输出合在一起
    let stdout_label = if terminal { "output" } else { "stdout" };
    for (label, body) in [(stdout_label, &stdout), ("stderr", &stderr)] {
        if body.is_empty() {
            continue;
        }
        text.push_str(&format!("--- {label} ---\n"));
        text.push_str(body);
        if !body.ends_with('\n') {
            text.push('\n');
        }
    }
    if out_omitted + err_omitted > 0 {
        text.push('\n');
        let clipped = match reach {
            Reach::Local => messages::MCP_CLIPPED,
            Reach::Lan => messages::MCP_CLIPPED_LAN,
        };
        text.push_str(&clipped.fill(lang, &[&(OUTPUT_BUDGET / 1024)]));
        text.push('\n');
    }
    if exec.truncated {
        text.push_str(&messages::MCP_APP_TRUNCATED.fill(lang, &[&OUTPUT_LIMIT]));
        text.push('\n');
    }
    for note in messages::expiring_notes(&exec.expiring_certificates, lang) {
        text.push_str(&format!("[termlet] {note}\n"));
    }
    text
}

/// 批量执行的结果：汇总一行，再按“退出码 + 输出”分组列出（每组的输出平分 [`OUTPUT_BUDGET`]，太长时只留开头与结尾），
/// 最后是没有成功的主机与原因。
fn batch_text(report: &BatchReport, lang: Lang) -> String {
    /// 列出的组最多这么多（其余只说还有几组）。
    const MAX_GROUPS: usize = 20;
    let mut text = messages::batch_summary(&report.summary, lang);
    text.push_str(match lang {
        Lang::Zh => "（",
        Lang::En => " (",
    });
    text.push_str(&messages::batch_took(
        report.summary.duration_ms,
        report.approval_wait_ms,
        lang,
    ));
    text.push_str(match lang {
        Lang::Zh => "）\n",
        Lang::En => ")\n",
    });
    let hosts = &report.hosts;
    let (groups, others) = termlet_proto::batch::group_outputs(hosts);
    let shown = groups.len().min(MAX_GROUPS);
    let budget = (OUTPUT_BUDGET / shown.max(1)).max(1024);
    let mut clipped_any = false;
    for group in groups.iter().take(MAX_GROUPS) {
        let first = &hosts[group.members[0]];
        let names: Vec<&str> = group
            .members
            .iter()
            .map(|&i| hosts[i].name.as_str())
            .collect();
        text.push_str(&format!(
            "\n── {}\n",
            messages::batch_group_title(&names, first.exit_code, 8, lang)
        ));
        let (out_limit, err_limit) = budgets(first.stdout.len(), first.stderr.len(), budget);
        let (stdout, out_omitted) = clip(&first.stdout, out_limit);
        let (stderr, err_omitted) = clip(&first.stderr, err_limit);
        clipped_any |= out_omitted + err_omitted > 0 || first.truncated;
        if stdout.is_empty() && stderr.is_empty() {
            text.push_str(messages::MCP_NO_OUTPUT.get(lang));
            text.push('\n');
        }
        for (label, body) in [("stdout", &stdout), ("stderr", &stderr)] {
            if body.is_empty() {
                continue;
            }
            if !stderr.is_empty() {
                text.push_str(&format!("--- {label} ---\n"));
            }
            text.push_str(body);
            if !body.ends_with('\n') {
                text.push('\n');
            }
        }
    }
    if groups.len() > MAX_GROUPS {
        text.push_str(&format!(
            "\n{}\n",
            messages::MCP_MORE_GROUPS.fill(lang, &[&(groups.len() - MAX_GROUPS)])
        ));
    }
    let problems: Vec<&termlet_proto::BatchHostResult> = others
        .iter()
        .map(|&i| &hosts[i])
        .filter(|h| h.status != BatchStatus::Ok)
        .collect();
    if !problems.is_empty() {
        text.push_str(&format!(
            "\n── {}\n",
            messages::BATCH_UNSUCCESSFUL.fill(lang, &[&problems.len()])
        ));
        for host in problems {
            text.push_str(&messages::batch_problem(host, lang));
            text.push('\n');
        }
    }
    if clipped_any {
        text.push_str(&format!("\n{}\n", messages::MCP_BATCH_CLIPPED.get(lang)));
    }
    let mut notes: Vec<String> = Vec::new();
    for host in hosts {
        for note in messages::expiring_notes(&host.expiring_certificates, lang) {
            notes.push(format!("[termlet] {}: {note}", host.name));
        }
    }
    for note in notes {
        text.push_str(&note);
        text.push('\n');
    }
    text
}

/// 两路输出各自的上限：合计不超过 `total` 时都完整返回；否则短的一路完整返回（不超过一半时），其余给另一路。
fn budgets(stdout: usize, stderr: usize, total: usize) -> (usize, usize) {
    let half = total / 2;
    if stdout + stderr <= total {
        (stdout, stderr)
    } else if stdout <= half {
        (stdout, total - stdout)
    } else if stderr <= half {
        (total - stderr, stderr)
    } else {
        (half, total - half)
    }
}

/// 超过 `limit` 字节时只留开头（约四分之一）与结尾，中间换成说明省略了多少字节；在字符边界上切。返回（文本，省略的字节数）。
fn clip(text: &str, limit: usize) -> (Cow<'_, str>, usize) {
    if text.len() <= limit {
        return (Cow::Borrowed(text), 0);
    }
    let head = text.floor_char_boundary(limit / HEAD_SHARE);
    let tail = text.ceil_char_boundary(text.len() - (limit - limit / HEAD_SHARE));
    let omitted = tail - head;
    (
        Cow::Owned(format!(
            "{}\n[… {omitted} bytes omitted …]\n{}",
            &text[..head],
            &text[tail..]
        )),
        omitted,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_share_the_limit() {
        assert_eq!(budgets(10, 20, 100), (10, 20), "放得下时都完整");
        assert_eq!(
            budgets(10, 500, 100),
            (10, 90),
            "短的一路完整，其余给另一路"
        );
        assert_eq!(budgets(500, 10, 100), (90, 10));
        assert_eq!(budgets(500, 500, 100), (50, 50));
    }

    #[test]
    fn clip_keeps_head_and_tail_on_char_boundaries() {
        assert_eq!(clip("short", 10), (Cow::Borrowed("short"), 0));
        let text = format!("{}{}", "a".repeat(100), "z".repeat(100));
        let (clipped, omitted) = clip(&text, 40);
        assert!(clipped.starts_with(&"a".repeat(10)), "{clipped}");
        assert!(clipped.ends_with(&"z".repeat(30)), "{clipped}");
        assert_eq!(omitted, 160);
        assert!(clipped.contains("[… 160 bytes omitted …]"), "{clipped}");
        // 中文每个字 3 字节：不在字中间切开
        let text = "中".repeat(100);
        let (clipped, omitted) = clip(&text, 40);
        assert!(omitted > 0);
        let kept = clipped.replace(&format!("\n[… {omitted} bytes omitted …]\n"), "");
        assert!(kept.chars().all(|c| c == '中'), "{clipped}");
        assert_eq!(kept.len() + omitted, text.len());
    }

    #[test]
    fn exec_text_layout() {
        let exec = ExecResponse {
            exit_code: 2,
            stdout: "hi\n".into(),
            stderr: "warn".into(),
            truncated: false,
            duration_ms: 1500,
            approval_wait_ms: None,
            expiring_certificates: Vec::new(),
        };
        assert_eq!(
            exec_text(&exec, false, Lang::En, Reach::Local),
            "exit code 2, took 1.5s\n--- stdout ---\nhi\n--- stderr ---\nwarn\n"
        );
        assert_eq!(
            exec_text(&exec, true, Lang::Zh, Reach::Local),
            "退出码 2，用时 1.5 秒\n--- output ---\nhi\n--- stderr ---\nwarn\n"
        );
        let empty = ExecResponse::default();
        assert!(exec_text(&empty, false, Lang::En, Reach::Local).contains("(no output)"));
        // 等用户批准过：用时只算执行，等待的时长另外说明
        let approved = ExecResponse {
            duration_ms: 40,
            approval_wait_ms: Some(31_350),
            ..Default::default()
        };
        assert!(
            exec_text(&approved, false, Lang::Zh, Reach::Local)
                .starts_with("退出码 0，用时 0.0 秒（另等待用户批准 31.4 秒）\n")
        );
        assert!(
            exec_text(&approved, false, Lang::En, Reach::Local).starts_with(
                "exit code 0, took 0.0s (plus 31.4s waiting for the user's approval)\n"
            )
        );
    }

    // 超出上限：截断并说明；Termlet 已截断（每路 1MB）时另外说明
    #[test]
    fn exec_text_is_clipped_with_a_note() {
        let exec = ExecResponse {
            exit_code: 0,
            stdout: format!("first line\n{}\nlast line\n", "x".repeat(200_000)),
            stderr: String::new(),
            truncated: true,
            duration_ms: 10,
            approval_wait_ms: None,
            expiring_certificates: Vec::new(),
        };
        let text = exec_text(&exec, false, Lang::En, Reach::Local);
        assert!(text.len() < OUTPUT_BUDGET + 1024, "{}", text.len());
        assert!(text.contains("first line") && text.contains("last line"));
        assert!(text.contains("bytes omitted"));
        assert!(
            text.contains("only its beginning and end are shown (about 32 KB"),
            "{text}"
        );
        assert!(text.contains("exceeded 1048576 bytes"), "{text}");
        let text = exec_text(&exec, false, Lang::Zh, Reach::Local);
        assert!(text.contains("只返回了开头与结尾"), "{text}");
        assert!(
            text.contains("download_file"),
            "本机的客户端可以下载：{text}"
        );
        // 局域网中的客户端：不提 download_file，改为在命令中取需要的部分
        for lang in [Lang::En, Lang::Zh] {
            let text = exec_text(&exec, false, lang, Reach::Lan);
            assert!(!text.contains("download_file"), "{text}");
            assert!(text.contains("head") && text.contains("grep"), "{text}");
        }
    }

    // 命令超过 1MB：本机的客户端提示先用 upload_file 上传，局域网中的客户端提示用 heredoc 分几次写到服务器上
    #[test]
    fn oversized_command_hint_follows_the_reach() {
        let mut arguments = Map::new();
        arguments.insert("host".into(), json!("web"));
        arguments.insert("command".into(), json!("x".repeat(MAX_STDIN_COMMAND + 1)));
        let text = |lang, reach| {
            let Err(outcome) = request(Tool::RunCommand, &arguments, lang, &[], reach) else {
                panic!("过长的命令不应发出");
            };
            assert!(outcome.is_error());
            outcome.text
        };
        assert!(text(Lang::En, Reach::Local).contains("upload_file"));
        for lang in [Lang::En, Lang::Zh] {
            let lan = text(lang, Reach::Lan);
            assert!(
                !lan.contains("upload_file") && lan.contains("heredoc"),
                "{lan}"
            );
        }
    }

    // 服务器没有 SFTP、改用了 SCP 时在结果中说明；旧版的应用不发 protocol，按 SFTP
    #[test]
    fn transfer_text_mentions_scp() {
        let mut done = TransferResponse {
            local: "D:\\a.txt".into(),
            remote: "/tmp/a.txt".into(),
            bytes: 2048,
            duration_ms: 1500,
            protocol: termlet_proto::TRANSFER_SFTP.into(),
            expiring_certificates: Vec::new(),
        };
        let text = transfer_outcome(&done, true, Lang::En).text;
        assert_eq!(text, "Uploaded 2.0 KB → /tmp/a.txt (1.5s)");
        done.protocol.clear();
        assert!(
            !transfer_outcome(&done, false, Lang::Zh)
                .text
                .contains("SCP")
        );
        done.protocol = termlet_proto::TRANSFER_SCP.into();
        let outcome = transfer_outcome(&done, false, Lang::Zh);
        assert!(outcome.text.starts_with("已下载 2.0 KB → D:\\a.txt"));
        assert!(outcome.text.contains("改用 SCP"), "{}", outcome.text);
        assert_eq!(outcome.structured.unwrap()["protocol"], "scp");
    }

    // 登录用的证书快过期：执行与传输的结果末尾提醒，跳板机的写明哪一台；传输的结构化结果中也有
    #[test]
    fn results_mention_expiring_certificates() {
        let expiring = vec![
            termlet_proto::ExpiringCertificate {
                jump: Some("bastion".into()),
                expires_at: "2026-11-19T20:30:00+08:00".into(),
                minutes_left: 25,
            },
            termlet_proto::ExpiringCertificate {
                jump: None,
                expires_at: "2026-11-19T20:40:00+08:00".into(),
                minutes_left: 35,
            },
        ];
        let exec = ExecResponse {
            stdout: "hi\n".into(),
            expiring_certificates: expiring.clone(),
            ..Default::default()
        };
        let text = exec_text(&exec, false, Lang::Zh, Reach::Local);
        assert!(
            text.ends_with(
                "hi\n[termlet] 跳板机 bastion 登录用的证书将于 2026-11-19 20:30 过期（还剩约 25 分钟），过期后无法再经它连接，请及时重新签发。\n[termlet] 登录用的证书将于 2026-11-19 20:40 过期（还剩约 35 分钟），过期后无法再连接这台主机，请及时重新签发。\n"
            ),
            "{text}"
        );
        let done = TransferResponse {
            local: "D:\\a.txt".into(),
            remote: "/tmp/a.txt".into(),
            bytes: 2048,
            duration_ms: 1500,
            protocol: termlet_proto::TRANSFER_SFTP.into(),
            expiring_certificates: expiring,
        };
        let outcome = transfer_outcome(&done, true, Lang::En);
        assert!(
            outcome
                .text
                .starts_with("Uploaded 2.0 KB → /tmp/a.txt (1.5s)\nThe certificate used to log in to the jump host bastion"),
            "{}",
            outcome.text
        );
        let structured = outcome.structured.unwrap();
        assert_eq!(structured["expiringCertificates"][0]["jump"], "bastion");
        assert_eq!(structured["expiringCertificates"][1]["minutesLeft"], 35);
    }

    #[test]
    fn definitions_follow_the_protocol_version() {
        let tools = list(Reach::Local, true, true);
        let tools = tools.as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "list_hosts",
                "run_command",
                "run_command_on_hosts",
                "upload_file",
                "download_file"
            ]
        );
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "工具名只用字母、数字与下划线：{name}"
            );
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object", "{name}");
            assert_eq!(schema["additionalProperties"], false, "{name}");
            // Schema 中的参数与检查时认的参数一致
            let mut properties: Vec<&str> = schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            let mut allowed = Tool::parse(name).unwrap().arguments().to_vec();
            properties.sort_unstable();
            allowed.sort_unstable();
            assert_eq!(properties, allowed, "{name}");
            for required in schema["required"].as_array().into_iter().flatten() {
                assert!(allowed.contains(&required.as_str().unwrap()), "{name}");
            }
            assert!(tool["description"].as_str().unwrap().len() > 80, "{name}");
            assert!(tool["annotations"].is_object() && tool["title"].is_string());
        }
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
        assert_eq!(tools[1]["annotations"]["destructiveHint"], true);
        // 较早的版本没有 annotations、title
        for tool in list(Reach::Local, false, false).as_array().unwrap() {
            assert!(tool.get("annotations").is_none() && tool.get("title").is_none());
        }
    }

    // 局域网中的客户端：没有传文件的工具；直接调用时是工具执行错误，说明怎样用 run_command 读写小文件
    #[test]
    fn lan_clients_get_no_file_tools() {
        let tools = list(Reach::Lan, true, true);
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["list_hosts", "run_command", "run_command_on_hosts"]);
        assert!(Tool::UploadFile.local_files() && Tool::DownloadFile.local_files());
        assert!(!Tool::ListHosts.local_files() && !Tool::RunCommand.local_files());

        let outcome = lan_unavailable(Tool::DownloadFile, Lang::En);
        assert!(outcome.is_error());
        assert!(
            outcome.text.starts_with(
                "[lan_unavailable] download_file isn't available over the local network"
            ),
            "{}",
            outcome.text
        );
        assert!(outcome.text.contains("cat") && outcome.text.contains("heredoc"));
        let result = outcome.into_result(true);
        assert_eq!(result["structuredContent"]["error"], "lan_unavailable");
        let zh = lan_unavailable(Tool::UploadFile, Lang::Zh);
        assert!(
            zh.text.contains("通过局域网连接时不能使用 upload_file"),
            "{}",
            zh.text
        );
    }
}
