//! 请求与响应。JSON 字段名是 camelCase；`termlet` 在 JSON 模式下原样输出的结构（主机列表、执行结果、传输结果）
//! 的字段名不能改（AI 与脚本依赖它们）。

use serde::{Deserialize, Serialize};
use zeroize::Zeroize as _;

use crate::ErrorCode;

/// 人类可读文本用的语言：服务端按它写错误说明。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    Zh,
    En,
}

impl Lang {
    /// 按语言取文字。
    pub fn pick<'a>(self, zh: &'a str, en: &'a str) -> &'a str {
        match self {
            Lang::Zh => zh,
            Lang::En => en,
        }
    }
}

/// 连接后的第一条消息。这个结构以后只加带默认值的字段。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientHello {
    /// 客户端支持的最高协议版本
    pub protocol: u32,
    /// 客户端的名称与版本（`termlet 0.1.0`），写进日志
    #[serde(default)]
    pub client: String,
    #[serde(default)]
    pub lang: Lang,
}

/// 握手成功：协商后的协议版本与应用的版本。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Welcome {
    pub protocol: u32,
    #[serde(default)]
    pub app_version: String,
}

/// 请求。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
pub enum Request {
    /// 列出允许命令行访问的主机（`termlet ls [-q 关键词]`）
    ListHosts {
        #[serde(default)]
        query: String,
    },
    /// 执行一条命令（`termlet exec`）
    Exec(ExecRequest),
    /// 上传单个文件（`termlet upload`）
    Upload(TransferRequest),
    /// 下载单个文件（`termlet download`）
    Download(TransferRequest),
    /// 已有实例在运行时，第二个 Termlet 进程把启动参数交给它（打开窗口、`ssh://` 链接等）后退出
    Activate {
        #[serde(default)]
        args: Vec<String>,
        /// 参数中的会话文件（.xsh）的内容：交出参数前读好一起发，堡垒机在调起的进程退出后就删掉这个文件。
        /// 没有时（较早的版本）由已有实例按路径读
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<LaunchFile>,
    },
    /// 版本不同的新 Termlet 进程请已有实例退出（更新后旧实例还在后台运行）：没有打开的窗口、没有进行中的命令行请求时
    /// 退出并回答 `Ok`，否则回答错误（不丢掉用户的会话），新进程改为把启动参数交给它
    Quit,
}

/// 启动参数中的一个会话文件的内容。内容中有密码：丢弃时清零，`Debug` 不显示内容。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchFile {
    /// 参数中原样的路径
    pub path: String,
    pub data: Vec<u8>,
}

impl Drop for LaunchFile {
    fn drop(&mut self) {
        self.data.zeroize();
    }
}

impl std::fmt::Debug for LaunchFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchFile")
            .field("path", &self.path)
            .field("len", &self.data.len())
            .finish()
    }
}

/// 响应。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
pub enum Response {
    /// 握手成功
    Welcome(Welcome),
    Hosts(Vec<HostSummary>),
    Exec(ExecResponse),
    Transfer(TransferResponse),
    /// 成功、没有内容
    Ok,
    Error(ErrorBody),
}

/// 主机列表中的一项，不含任何凭据。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSummary {
    pub id: String,
    pub name: String,
    /// 地址
    pub host: String,
    pub port: u16,
    /// 命令行登录用的用户名（设置了专用凭据时是它的）
    pub username: String,
    /// 协议（目前只有 `SSH`）
    pub protocol: String,
}

/// `exec` 的请求。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecRequest {
    /// 主机的 ID 或名称
    pub host: String,
    pub command: String,
    /// 在 Termlet 中已打开的这台主机的终端里执行（用户看得见），否则新建连接执行
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub terminal: bool,
    /// 调用方：`termlet` 的祖先进程名，由近到远（`bash`、`node`、`Code.exe`），审计时区分是谁调用的
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caller: Vec<String>,
}

/// `exec` 的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResponse {
    /// 远程命令的退出码；被信号结束时为 128 + 信号编号，服务器没有报告时为 255
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// 标准输出或标准错误超过 [`crate::OUTPUT_LIMIT`] 被截断
    pub truncated: bool,
    pub duration_ms: u64,
}

/// `upload` / `download` 的请求。语义与 scp 相同：上传时 `remote` 是已存在的目录（或以 `/` 结尾）则放入该目录，
/// 否则作为目标文件名；下载时 `local` 是已存在的目录则放入该目录，否则作为目标文件名。已存在的同名文件会被覆盖。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferRequest {
    pub host: String,
    /// 本机路径，必须是绝对路径（命令行工具按当前目录解析后发送）；应用与命令行以同一用户运行，直接读写
    pub local: String,
    pub remote: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caller: Vec<String>,
}

/// 传输成功：实际写入的路径与字节数。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferResponse {
    pub local: String,
    pub remote: String,
    pub bytes: u64,
    pub duration_ms: u64,
    /// 用的协议：`sftp`，或服务器没有 SFTP 时改用的 `scp`（没有断点续传；命令行本来也不续传）。
    /// 旧版的应用不发送，为空
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub protocol: String,
}

/// [`TransferResponse::protocol`] 的取值。
pub const TRANSFER_SFTP: &str = "sftp";
/// 服务器没有 SFTP，改用了 SCP。
pub const TRANSFER_SCP: &str = "scp";

/// 失败的原因。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    /// 错误码（[`ErrorCode::as_str`]）；可能是本版本不认识的
    pub code: String,
    /// 补充说明（命中的规则、路径、服务器的说明等），按握手时的语言书写；可以为空
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().to_string(),
            message: message.into(),
        }
    }

    /// 错误码（不认识的为 `None`）。
    pub fn error_code(&self) -> Option<ErrorCode> {
        ErrorCode::parse(&self.code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_responses_round_trip() {
        let requests = [
            Request::ListHosts {
                query: "web".into(),
            },
            Request::Exec(ExecRequest {
                host: "web-1".into(),
                command: "uptime".into(),
                terminal: true,
                caller: vec!["bash".into(), "node".into()],
            }),
            Request::Upload(TransferRequest {
                host: "web-1".into(),
                local: r"C:\a.txt".into(),
                remote: "/tmp/".into(),
                caller: Vec::new(),
            }),
            Request::Activate {
                args: vec!["ssh://root@h".into()],
                files: Vec::new(),
            },
            Request::Activate {
                args: vec!["-newtab".into(), "a.xsh".into()],
                files: vec![LaunchFile {
                    path: "a.xsh".into(),
                    data: b"[CONNECTION]".to_vec(),
                }],
            },
            Request::Quit,
        ];
        for request in requests {
            let json = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        }
        let responses = [
            Response::Hosts(vec![HostSummary {
                id: "h_1".into(),
                name: "web".into(),
                host: "10.0.0.5".into(),
                port: 22,
                username: "deploy".into(),
                protocol: "SSH".into(),
            }]),
            Response::Ok,
            Response::Error(ErrorBody::new(ErrorCode::CommandBlocked, "power")),
        ];
        for response in responses {
            let json = serde_json::to_string(&response).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
        }
    }

    // 命令行在 JSON 模式下原样输出这些结构：字段名不能改（AI 与脚本依赖它们）
    #[test]
    fn output_field_names_match_previous_version() {
        let exec = serde_json::to_value(ExecResponse {
            exit_code: 3,
            stdout: "hi\n".into(),
            stderr: String::new(),
            truncated: false,
            duration_ms: 12,
        })
        .unwrap();
        assert_eq!(
            exec,
            serde_json::json!({"exitCode": 3, "stdout": "hi\n", "stderr": "", "truncated": false, "durationMs": 12})
        );
        let transfer = serde_json::to_value(TransferResponse {
            local: "D:\\a".into(),
            remote: "/tmp/a".into(),
            bytes: 7,
            duration_ms: 5,
            protocol: TRANSFER_SCP.into(),
        })
        .unwrap();
        assert_eq!(transfer["durationMs"], 5);
        assert_eq!(transfer["bytes"], 7);
        assert_eq!(transfer["protocol"], "scp");
        // 旧版的应用不发送 protocol
        let old: TransferResponse =
            serde_json::from_str(r#"{"local":"a","remote":"b","bytes":1,"durationMs":2}"#).unwrap();
        assert!(old.protocol.is_empty());
    }

    // 不认识的字段、请求类型：字段忽略；类型解析失败（服务端回答"请求无效"）
    #[test]
    fn unknown_fields_are_ignored() {
        let hello: ClientHello = serde_json::from_str(r#"{"protocol":3,"future":true}"#).unwrap();
        assert_eq!(hello.protocol, 3);
        assert_eq!(hello.lang, Lang::Zh);
        let exec: Request = serde_json::from_str(
            r#"{"type":"exec","data":{"host":"h","command":"ls","newOption":1}}"#,
        )
        .unwrap();
        assert!(matches!(
            exec,
            Request::Exec(ExecRequest {
                terminal: false,
                ..
            })
        ));
        assert!(serde_json::from_str::<Request>(r#"{"type":"teleport","data":{}}"#).is_err());
    }

    // 交启动参数：较早的版本不带会话文件的内容，没有内容时也不发这个字段（较早的版本认得）；Debug 不显示内容
    #[test]
    fn activate_files_are_optional_and_hidden_from_debug() {
        let old: Request =
            serde_json::from_str(r#"{"type":"activate","data":{"args":["ssh://h"]}}"#).unwrap();
        assert_eq!(
            old,
            Request::Activate {
                args: vec!["ssh://h".into()],
                files: Vec::new(),
            }
        );
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("files"), "{json}");
        let file = LaunchFile {
            path: "a.xsh".into(),
            data: b"Password=c2VjcmV0".to_vec(),
        };
        let debug = format!("{file:?}");
        assert!(
            debug.contains("a.xsh") && !debug.contains("c2VjcmV0"),
            "{debug}"
        );
    }
}
