//! 命令行连接主机与执行命令：不能询问（指纹、密码、私钥密码、验证码、执行代理命令），第一个被拒绝的问题决定错误码；
//! 每次新建连接，用完即断开。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::{ChannelMsg, Sig};
use termlet_proto::ErrorCode;

use super::error::{CliError, Refused, from_ssh};
use super::service::Cancel;
use crate::id::HostId;
use crate::ssh::{self, Answer, ConnectOptions, Connection, Prompt, Prompter};
use crate::store::Store;

/// 记下第一个被拒绝的询问，一律回答"取消"。
fn refusing_prompter() -> (Prompter, Arc<Mutex<Option<Refused>>>) {
    let refused: Arc<Mutex<Option<Refused>>> = Arc::default();
    let record = refused.clone();
    let prompter: Prompter = Arc::new(move |prompt| {
        let (code, subject) = match &prompt {
            Prompt::HostKey(p) => (ErrorCode::HostKeyUnknown, format!("{}:{}", p.host, p.port)),
            // 第一次询问密码：没有保存；再次询问（retry）：保存的不对
            Prompt::Password {
                host,
                username,
                retry: false,
                ..
            } => (ErrorCode::CredentialMissing, format!("{username}@{host}")),
            Prompt::Password { host, username, .. } => {
                (ErrorCode::AuthFailed, format!("{username}@{host}"))
            }
            Prompt::Passphrase {
                key, retry: false, ..
            } => (ErrorCode::KeyPassphraseRequired, key.clone()),
            Prompt::Passphrase { key, .. } => (ErrorCode::KeyWrongPassphrase, key.clone()),
            Prompt::KeyboardInteractive { host, .. } => {
                (ErrorCode::AuthInteractionRequired, host.clone())
            }
            // 代理命令要在应用里确认（显示完整的命令）；命令行不能替用户允许执行本机程序
            Prompt::ProxyCommand(p) => (ErrorCode::ProxyCommandUnconfirmed, p.host.clone()),
        };
        let mut slot = record.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(Refused { code, subject });
        }
        Box::pin(async { Answer::Cancel })
    });
    (prompter, refused)
}

/// 连接的选项（保活、agent 地址）取自设置。
fn options(store: &Store) -> ConnectOptions {
    let connection = store.settings().connection;
    ConnectOptions {
        keepalive: (connection.keepalive_seconds > 0)
            .then(|| Duration::from_secs(u64::from(connection.keepalive_seconds))),
        agent_path: Some(connection.agent_path).filter(|p| !p.trim().is_empty()),
    }
}

/// 用命令行的凭据连接主机（[`ssh::plan_for_cli`]）。失败时的错误码见 [`from_ssh`]；被拒绝的询问另行返回，
/// 传输出错（连接断开等）时也用得上。
pub(super) async fn connect(
    store: Arc<Store>,
    host: &HostId,
    cancel: &mut Cancel,
) -> Result<(Arc<Connection>, Arc<Mutex<Option<Refused>>>), CliError> {
    let plan = {
        let store = store.clone();
        let host = host.clone();
        tokio::task::spawn_blocking(move || ssh::plan_for_cli(&store, &host))
            .await
            .map_err(|e| CliError::new(ErrorCode::Internal, e.to_string()))?
            .map_err(|e| from_ssh(&e, None))?
    };
    let (prompter, refused) = refusing_prompter();
    let options = options(&store);
    let connected = tokio::select! {
        result = ssh::connect(store, &plan, prompter, &options) => result,
        _ = cancel.wait() => return Err(CliError::code(ErrorCode::Canceled)),
    };
    match connected {
        Ok(connection) => Ok((connection, refused)),
        Err(e) => {
            let refused = refused.lock().unwrap_or_else(|e| e.into_inner()).clone();
            Err(from_ssh(&e, refused.as_ref()))
        }
    }
}

/// `exec` 的结果。
pub(super) struct ExecOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

/// 服务器没有报告退出码时的退出码。
const EXIT_UNKNOWN: i32 = 255;

/// 被信号结束时的退出码：128 + 信号编号（与 shell 相同；不认识的信号为 128）。
fn signal_exit_code(signal: &Sig) -> i32 {
    128 + match signal {
        Sig::HUP => 1,
        Sig::INT => 2,
        Sig::QUIT => 3,
        Sig::ILL => 4,
        Sig::ABRT => 6,
        Sig::FPE => 8,
        Sig::KILL => 9,
        Sig::USR1 => 10,
        Sig::SEGV => 11,
        Sig::PIPE => 13,
        Sig::ALRM => 14,
        Sig::TERM => 15,
        Sig::Custom(name) if name == "USR2" => 12,
        Sig::Custom(_) => 0,
    }
}

/// 执行一条命令，每一路输出最多保留 `limit` 字节（超出的不再保存，但继续读完，让命令正常结束）。
/// 没有时限（等命令结束）；命令行工具退出时向命令发送 SIGTERM 并关闭通道。
pub(super) async fn exec(
    connection: &Connection,
    command: &str,
    limit: usize,
    cancel: &mut Cancel,
) -> Result<ExecOutput, CliError> {
    let failed = |e: russh::Error| CliError::new(ErrorCode::ConnectFailed, e.to_string());
    let mut channel = connection
        .handle()
        .channel_open_session()
        .await
        .map_err(failed)?;
    channel.exec(true, command).await.map_err(failed)?;
    let mut output = ExecOutput {
        exit_code: EXIT_UNKNOWN,
        stdout: Vec::new(),
        stderr: Vec::new(),
        truncated: false,
    };
    let keep = |buffer: &mut Vec<u8>, data: &[u8], truncated: &mut bool| {
        let room = limit.saturating_sub(buffer.len());
        if data.len() > room {
            *truncated = true;
        }
        buffer.extend_from_slice(&data[..data.len().min(room)]);
    };
    let mut status = None;
    let mut signal = None;
    loop {
        let message = tokio::select! {
            message = channel.wait() => message,
            _ = cancel.wait() => {
                let _ = channel.signal(Sig::TERM).await;
                let _ = channel.close().await;
                return Err(CliError::code(ErrorCode::Canceled));
            }
        };
        match message {
            None | Some(ChannelMsg::Close) => break,
            Some(ChannelMsg::Data { data }) => {
                keep(&mut output.stdout, &data, &mut output.truncated)
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                keep(&mut output.stderr, &data, &mut output.truncated)
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => status = Some(exit_status),
            Some(ChannelMsg::ExitSignal { signal_name, .. }) => signal = Some(signal_name),
            Some(ChannelMsg::Failure) => {
                // 通道打开了，但服务器不允许执行命令（只允许交互式 shell 的堡垒机等）
                return Err(CliError::new(
                    ErrorCode::ConnectFailed,
                    "服务器拒绝执行命令（只允许交互式登录的堡垒机可以改用 --terminal）",
                ));
            }
            Some(_) => {}
        }
    }
    output.exit_code = match (status, signal) {
        (Some(code), _) => code as i32,
        (None, Some(signal)) => signal_exit_code(&signal),
        (None, None) => EXIT_UNKNOWN,
    };
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_map_to_shell_exit_codes() {
        assert_eq!(signal_exit_code(&Sig::TERM), 143);
        assert_eq!(signal_exit_code(&Sig::KILL), 137);
        assert_eq!(signal_exit_code(&Sig::Custom("USR2".into())), 140);
        assert_eq!(signal_exit_code(&Sig::Custom("WINCH".into())), 128);
    }

    #[tokio::test]
    async fn prompter_records_the_first_refusal() {
        let (prompter, refused) = refusing_prompter();
        let answer = prompter(Prompt::Password {
            host: "web".into(),
            username: "ops".into(),
            retry: true,
            endpoint: "10.0.0.1:22".into(),
        })
        .await;
        assert!(matches!(answer, Answer::Cancel));
        prompter(Prompt::Passphrase {
            key: "id".into(),
            retry: false,
            id: "0".into(),
        })
        .await;
        assert_eq!(
            refused.lock().unwrap().clone(),
            Some(Refused {
                code: ErrorCode::AuthFailed,
                subject: "ops@web".into()
            })
        );
    }
}
