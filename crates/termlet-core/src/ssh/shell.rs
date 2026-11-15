//! 终端会话：在连接上开会话通道、申请 PTY 与 shell，把通道接到终端的字节接口上。
//!
//! 远端的输出经编码转换（GBK、Big5）后送给终端；终端的输入编码回去后发给远端。
//! 输出通道有界：终端解析跟不上时这里等待，SSH 窗口随之不再增长，形成背压。
//!
//! 结束原因：服务器报告了退出码或信号的是正常结束；没有报告就关闭（连接断开、sshd 被结束）的算作断线，可以自动重连。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use russh::client::Msg;
use russh::{Channel, ChannelMsg};
use termlet_io::{CloseReason, GridSize, IoEvent, TerminalInput, TerminalIo};
use tokio::sync::mpsc;

use crate::charset::{InputEncoder, OutputDecoder};
use crate::model::Encoding;
use crate::ssh::SshError;
use crate::ssh::connection::Connection;
use crate::ssh::x11::{X11Options, X11Status};

/// 打开终端的选项。
#[derive(Clone, Debug)]
pub struct ShellOptions {
    pub size: GridSize,
    pub encoding: Encoding,
    /// 终端类型（`TERM`）
    pub term: String,
    /// 额外的环境变量（服务器的 `AcceptEnv` 允许时才生效）
    pub env: Vec<(String, String)>,
    /// 启动的命令（例如接入 tmux 会话）；`None` 时启动登录 shell
    pub command: Option<String>,
    /// 请求转发本机的 ssh-agent
    pub agent_forward: bool,
    /// 请求转发 X11（见 [`crate::ssh::x11`]）
    pub x11: Option<X11Options>,
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self {
            size: GridSize::default(),
            encoding: Encoding::Utf8,
            term: "xterm-256color".into(),
            env: Vec::new(),
            command: None,
            agent_forward: false,
            x11: None,
        }
    }
}

enum Input {
    Data(Vec<u8>),
    Resize(GridSize),
    Close,
}

struct ShellInput {
    queue: mpsc::UnboundedSender<Input>,
    encoder: std::sync::Mutex<InputEncoder>,
    /// 当前的编码：读的一端每收到一批输出时比较，变了就换解码器
    encoding: Arc<std::sync::Mutex<Encoding>>,
    /// 终端中的文件传输期间原样收发，不转换编码（见 [`TerminalInput::set_binary`]）
    binary: Arc<AtomicBool>,
}

impl TerminalInput for ShellInput {
    fn write(&self, data: &[u8]) {
        let data = if self.binary.load(Ordering::Acquire) {
            data.to_vec()
        } else {
            self.encoder.lock().unwrap().encode(data)
        };
        let _ = self.queue.send(Input::Data(data));
    }

    fn set_binary(&self, on: bool) {
        self.binary.store(on, Ordering::Release);
    }

    fn resize(&self, size: GridSize) {
        let _ = self.queue.send(Input::Resize(size));
    }

    fn close(&self) {
        let _ = self.queue.send(Input::Close);
    }

    fn set_encoding(&self, name: &str) {
        let Ok(encoding) =
            serde_json::from_value::<Encoding>(serde_json::Value::String(name.into()))
        else {
            tracing::warn!(name, "不认识的编码");
            return;
        };
        *self.encoding.lock().unwrap() = encoding;
        *self.encoder.lock().unwrap() = InputEncoder::new(encoding);
    }
}

/// 打开一个交互式终端。
pub async fn open_shell(
    connection: Arc<Connection>,
    options: ShellOptions,
) -> Result<TerminalIo, SshError> {
    open_shell_reporting(connection, options)
        .await
        .map(|(io, _)| io)
}

/// 打开一个交互式终端，并报告 X11 转发的结果。
pub(crate) async fn open_shell_reporting(
    connection: Arc<Connection>,
    options: ShellOptions,
) -> Result<(TerminalIo, X11Status), SshError> {
    let host = connection.label().to_string();
    let mut channel = connection
        .handle()
        .channel_open_session()
        .await
        .map_err(|e| {
            tracing::warn!(host = %host, "打开终端通道失败：{e}");
            SshError::Channel(e.to_string())
        })?;
    let size = options.size;
    tracing::debug!(
        host = %host,
        term = %options.term,
        cols = size.columns,
        rows = size.rows,
        encoding = ?options.encoding,
        env = options.env.len(),
        command = options.command.is_some(),
        agent_forward = options.agent_forward,
        x11 = options.x11.is_some(),
        "请求伪终端与 shell"
    );
    channel
        .request_pty(
            true,
            &options.term,
            size.columns as u32,
            size.rows as u32,
            size.pixel_width() as u32,
            size.pixel_height() as u32,
            &[],
        )
        .await
        .map_err(|e| SshError::Channel(e.to_string()))?;
    for (key, value) in &options.env {
        let _ = channel.set_env(false, key.clone(), value.clone()).await;
    }
    if options.agent_forward {
        connection.enable_agent_forwarding();
        if let Err(e) = channel.agent_forward(false).await {
            tracing::warn!("请求转发 ssh-agent 失败：{e}");
        }
    }
    let x11 = match &options.x11 {
        None => X11Status::Off,
        Some(x11) => request_x11(&connection, &mut channel, x11).await?,
    };
    match &options.command {
        Some(command) => channel.exec(true, command.as_bytes()).await,
        None => channel.request_shell(true).await,
    }
    .map_err(|e| SshError::Channel(e.to_string()))?;

    let (mut read_half, write_half) = channel.split();
    let (output_tx, output) = async_channel::bounded::<IoEvent>(64);
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Input>();
    // 本机关闭了通道：russh 此后不再把服务器的确认转给读的一端，要另行通知它结束
    let closed_locally = Arc::new(tokio::sync::Notify::new());
    let reader_closed = closed_locally.clone();

    // 读：远端输出 → 解码 → 终端
    let reader_connection = connection.clone();
    let encoding = Arc::new(std::sync::Mutex::new(options.encoding));
    let reader_encoding = encoding.clone();
    let mut decoding = options.encoding;
    let mut decoder = OutputDecoder::new(decoding);
    let binary = Arc::new(AtomicBool::new(false));
    let reader_binary = binary.clone();
    let mut in_binary = false;
    crate::runtime::spawn(async move {
        // 服务器报告的退出：退出码，或被信号结束（没有退出码）
        let mut exited: Option<Option<i32>> = None;
        let mut by_us = false;
        loop {
            let message = tokio::select! {
                message = read_half.wait() => message,
                _ = reader_closed.notified() => {
                    by_us = true;
                    break;
                }
            };
            match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    // 文件传输期间原样交出；结束后重新开始解码（传输前剩下的半个字符丢弃）
                    let raw = reader_binary.load(Ordering::Acquire);
                    if in_binary && !raw {
                        decoder = OutputDecoder::new(decoding);
                    }
                    in_binary = raw;
                    // 用户切换了编码：之后的输出按新的编码解码
                    let current = *reader_encoding.lock().unwrap();
                    if current != decoding {
                        decoding = current;
                        decoder = OutputDecoder::new(current);
                    }
                    let bytes = if raw {
                        data.to_vec()
                    } else {
                        decoder.decode(&data)
                    };
                    if output_tx.send(IoEvent::Output(bytes)).await.is_err() {
                        return;
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exited = Some(Some(exit_status as i32))
                }
                Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                    tracing::info!("远端 shell 被信号结束：{signal_name:?}");
                    exited.get_or_insert(None);
                }
                Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            }
        }
        let reason = match exited {
            Some(code) => CloseReason::Exited(code),
            None if by_us => CloseReason::Exited(None),
            None if reader_connection.is_closed() => CloseReason::Lost("连接已断开".into()),
            None => CloseReason::Lost("服务器关闭了终端，没有报告退出码".into()),
        };
        tracing::info!(host = %reader_connection.label(), by_us, reason = ?reason, "SSH 终端通道结束");
        let _ = output_tx.send(IoEvent::Closed(reason)).await;
    });

    // 写：终端输入 → 远端；窗口大小
    crate::runtime::spawn(async move {
        while let Some(input) = input_rx.recv().await {
            let result = match input {
                Input::Data(data) => write_half.data_bytes(data).await,
                Input::Resize(size) => {
                    tracing::trace!(cols = size.columns, rows = size.rows, "终端大小变化");
                    write_half
                        .window_change(
                            size.columns as u32,
                            size.rows as u32,
                            size.pixel_width() as u32,
                            size.pixel_height() as u32,
                        )
                        .await
                }
                Input::Close => break,
            };
            if let Err(e) = result {
                tracing::debug!("向终端通道写入失败：{e}");
                break;
            }
        }
        // 终端不用了（Tab 关闭、重连换上了新的通道）：关闭通道，远端的 shell 随之结束（tmux 会话保留）。
        // 连接与别的终端共用时只关这一个通道
        let _ = write_half.close().await;
        closed_locally.notify_one();
    });

    let io = TerminalIo {
        output,
        input: Arc::new(ShellInput {
            queue: input_tx,
            encoder: std::sync::Mutex::new(InputEncoder::new(options.encoding)),
            encoding,
            binary,
        }),
    };
    Ok((io, x11))
}

/// 等服务器回答请求的最长时间。
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// 请求 X11 转发：本机准备不了（显示写错、不受信任时没有 xauth）时不请求；请求后等服务器的回答
/// （前面的伪终端请求也要回答：第二个回答是 X11 的）。服务器不回答时当作同意。
async fn request_x11(
    connection: &Connection,
    channel: &mut Channel<Msg>,
    options: &X11Options,
) -> Result<X11Status, SshError> {
    let request = match connection.x11().prepare(options).await {
        Ok(request) => request,
        Err(problem) => {
            tracing::info!(host = %connection.label(), "没有请求 X11 转发：{problem}");
            return Ok(X11Status::Unavailable(problem));
        }
    };
    channel
        .request_x11(
            true,
            false,
            request.protocol,
            request.cookie,
            request.screen,
        )
        .await
        .map_err(|e| SshError::Channel(e.to_string()))?;
    let replies = async {
        let mut seen = 0;
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Success | ChannelMsg::Failure => {
                    seen += 1;
                    if seen == 2 {
                        return Some(matches!(message, ChannelMsg::Success));
                    }
                }
                ChannelMsg::Close | ChannelMsg::Eof => return None,
                _ => {}
            }
        }
        None
    };
    let accepted = tokio::time::timeout(REPLY_TIMEOUT, replies)
        .await
        .ok()
        .flatten();
    tracing::info!(host = %connection.label(), ?accepted, "请求 X11 转发");
    Ok(if accepted == Some(false) {
        X11Status::Refused
    } else {
        X11Status::Forwarding {
            display: options.display.clone(),
            trusted: options.trusted,
        }
    })
}
