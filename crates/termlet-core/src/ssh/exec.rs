//! 在已有的连接上执行一次性命令（检测 tmux、结束 tmux 会话，以后的资源监控、主机工具也用它）。
//!
//! 输出超过上限时截断（不再保存，但继续读完，让命令正常结束）。超时后先给远端命令发 SIGTERM、再关闭通道，
//! 等服务器确认关闭后才返回（最多 [`CLOSE_WAIT`]）：russh 丢弃 `Channel` 时不会关闭通道，不关的话服务器上
//! 卡住的命令与通道会一直占着这条连接的会话数（sshd 默认每条连接 10 个）。调用方不再等待（future 被丢弃）时
//! 同样在后台结束命令并关闭通道。

use std::time::Duration;

use russh::client::Msg;
use russh::{Channel, ChannelMsg, ChannelOpenFailure, Sig};
use tokio::time::Instant;

use crate::ssh::SshError;
use crate::ssh::connection::Connection;

/// 超时后等服务器确认关闭通道的最长时间。
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// 命令的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecResult {
    /// 退出码；被信号结束、或服务器没有报告时为 `None`
    pub exit_code: Option<u32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// 输出超过上限被截断
    pub truncated: bool,
}

/// 执行 `command`，每一路输出最多保留 `limit` 字节，最多等 `timeout`。
pub async fn exec(
    connection: &Connection,
    command: &str,
    limit: usize,
    timeout: Duration,
) -> Result<ExecResult, SshError> {
    exec_with_input(connection, command, None, limit, timeout).await
}

/// 与 [`exec`] 相同；`input` 不为空时写进命令的标准输入，写完后发送 EOF
/// （部署公钥：脚本经标准输入交给 `sh`，不受登录 shell 种类影响）。
pub async fn exec_with_input(
    connection: &Connection,
    command: &str,
    input: Option<&[u8]>,
    limit: usize,
    timeout: Duration,
) -> Result<ExecResult, SshError> {
    exec_until(connection, command, input, limit, Instant::now() + timeout)
        .await
        .map_err(|failure| match failure {
            ExecFailure::Timeout => {
                SshError::Channel(format!("命令在 {} 秒内没有结束", timeout.as_secs()))
            }
            other => other.into_ssh_error(),
        })
}

/// 一次性命令失败的原因：主机工具据此区分超时、服务器拒绝另开通道的几种情况。
#[derive(Debug)]
pub(crate) enum ExecFailure {
    /// 到了时限还没有结束：已给远端命令发 SIGTERM 并关闭通道
    Timeout,
    /// 服务器拒绝打开会话通道（拒绝的原因与说明）
    OpenRefused(ChannelOpenFailure),
    /// 通道打开了，服务器拒绝执行命令
    ExecRefused,
    /// 其他错误（连接断开等）
    Other(SshError),
}

impl ExecFailure {
    fn into_ssh_error(self) -> SshError {
        match self {
            ExecFailure::Timeout => SshError::Channel("命令没有在时限内结束".into()),
            ExecFailure::OpenRefused(reason) => {
                SshError::Channel(russh::Error::ChannelOpenFailure(reason).to_string())
            }
            ExecFailure::ExecRefused => SshError::Channel("服务器拒绝执行命令".into()),
            ExecFailure::Other(error) => error,
        }
    }
}

/// 执行 `command`，到 `deadline` 还没有结束时结束它并返回 [`ExecFailure::Timeout`]
/// （等服务器确认关闭通道的时间不算在内，最多再等 [`CLOSE_WAIT`]）。命令可以是字节（SCP 模式中按服务器的
/// 文件名编码写的路径）。
pub(crate) async fn exec_until(
    connection: &Connection,
    command: impl AsRef<[u8]>,
    input: Option<&[u8]>,
    limit: usize,
    deadline: Instant,
) -> Result<ExecResult, ExecFailure> {
    // 资源监控等每隔几秒执行一次：成功只记 trace；命令内容不记（可能是用户的命令）
    let started = Instant::now();
    let host = connection.label();
    let opened =
        match tokio::time::timeout_at(deadline, connection.handle().channel_open_session()).await {
            Err(_) => {
                tracing::debug!(host, "打开执行命令的通道超时");
                return Err(ExecFailure::Timeout);
            }
            Ok(opened) => opened,
        };
    let channel = match opened {
        Ok(channel) => channel,
        Err(russh::Error::ChannelOpenFailure(reason)) => {
            tracing::debug!(host, reason = ?reason, "服务器拒绝打开执行命令的通道");
            return Err(ExecFailure::OpenRefused(reason));
        }
        Err(e) => {
            tracing::debug!(host, "打开执行命令的通道失败：{e}");
            return Err(ExecFailure::Other(SshError::Channel(e.to_string())));
        }
    };
    let mut guard = ChannelGuard(Some(channel));
    let outcome = tokio::time::timeout_at(
        deadline,
        run(guard.channel(), command.as_ref(), input, limit),
    )
    .await;
    match outcome {
        Ok(Ok(result)) => {
            // 服务器已关闭通道（或连接已断开），不用再关
            guard.0 = None;
            tracing::trace!(
                host,
                exit_code = ?result.exit_code,
                stdout = result.stdout.len(),
                stderr = result.stderr.len(),
                truncated = result.truncated,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "命令执行完"
            );
            Ok(result)
        }
        // 出错时通道可能还开着：丢弃 guard 时在后台关闭
        Ok(Err(failure)) => {
            if matches!(failure, ExecFailure::ExecRefused) {
                tracing::debug!(host, "服务器拒绝执行命令");
            }
            Err(failure)
        }
        Err(_) => {
            tracing::debug!(
                host,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "命令超时，结束远端命令并关闭通道"
            );
            guard.terminate().await;
            Err(ExecFailure::Timeout)
        }
    }
}

async fn run(
    channel: &mut Channel<Msg>,
    command: &[u8],
    input: Option<&[u8]>,
    limit: usize,
) -> Result<ExecResult, ExecFailure> {
    let channel_error = |e: russh::Error| ExecFailure::Other(SshError::Channel(e.to_string()));
    channel
        .exec(true, command.to_vec())
        .await
        .map_err(channel_error)?;
    if let Some(input) = input {
        channel.data(input).await.map_err(channel_error)?;
        channel.eof().await.map_err(channel_error)?;
    }
    let mut result = ExecResult::default();
    let keep = |buffer: &mut Vec<u8>, data: &[u8], truncated: &mut bool| {
        let room = limit.saturating_sub(buffer.len());
        if data.len() > room {
            *truncated = true;
        }
        buffer.extend_from_slice(&data[..data.len().min(room)]);
    };
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => keep(&mut result.stdout, &data, &mut result.truncated),
            ChannelMsg::ExtendedData { data, .. } => {
                keep(&mut result.stderr, &data, &mut result.truncated)
            }
            ChannelMsg::ExitStatus { exit_status } => result.exit_code = Some(exit_status),
            ChannelMsg::Failure => return Err(ExecFailure::ExecRefused),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(result)
}

/// 打开着的通道：没有正常结束时（超时、出错、调用方不再等待）结束远端命令并关闭通道。
struct ChannelGuard(Option<Channel<Msg>>);

impl ChannelGuard {
    fn channel(&mut self) -> &mut Channel<Msg> {
        self.0.as_mut().expect("通道还没有关闭")
    }

    /// 给远端命令发 SIGTERM、关闭通道，并等服务器确认关闭（最多 [`CLOSE_WAIT`]；
    /// 没等到时丢弃 guard 再在后台关一次）。
    async fn terminate(mut self) {
        let channel = self.channel();
        let closed = tokio::time::timeout(CLOSE_WAIT, async {
            let _ = channel.signal(Sig::TERM).await;
            let _ = channel.close().await;
            while let Some(message) = channel.wait().await {
                if matches!(message, ChannelMsg::Close) {
                    break;
                }
            }
        })
        .await
        .is_ok();
        if closed {
            self.0 = None;
        }
    }
}

impl Drop for ChannelGuard {
    fn drop(&mut self) {
        if let Some(channel) = self.0.take() {
            crate::runtime::spawn(async move {
                let _ = channel.signal(Sig::TERM).await;
                let _ = channel.close().await;
            });
        }
    }
}
