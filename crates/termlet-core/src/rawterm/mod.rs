//! Telnet、串口的终端会话：把去掉协议命令后的字节流包装成终端的字节接口（[`TerminalIo`]），
//! 中间是行规程——编码转换（GBK、Big5）、回车键与退格键的映射、本地回显、自动登录（看到登录、密码提示时填写
//! 保存的用户名与密码）、传输文件时原样收发。
//!
//! # 用法（界面）
//!
//! - 按主机的协议分派：SSH 主机用 [`crate::ssh::open_terminal`]，
//!   Telnet、串口主机先 [`RawSpec::for_host`]，在终端中切换过编码时改 `spec.config.encoding`（重连后沿用），
//!   再在核心的运行时上 `open`（`crate::runtime::spawn(rawterm::open(...))`）。串口的打开是阻塞的，`open`
//!   另开阻塞任务，不占运行时的工作线程。
//! - 得到的 [`RawTerminal::io`] 与本地终端、SSH 终端一样交给终端视图（要在终端中传输文件时先经
//!   `termxfer::Interceptor::wrap`）；`set_encoding` 在线切换编码，`set_binary` 由文件传输调用，`close`
//!   关闭连接（Telnet 断开 TCP 与跳板机，串口释放端口）。
//! - 结束原因：Telnet 服务器关闭连接（logout）是正常结束 `Exited(Some(0))`；连接出错、跳板机断开、串口拔掉是
//!   断线 `Lost`（界面自动重连，退避与 SSH 相同：[`crate::ssh::reconnect_delay`]；串口插回即恢复）；
//!   本机关闭是 `Exited(None)`。打开失败的错误见 [`RawError::is_retryable`]。
//! - 测试连接：Telnet 只测网络路径（[`crate::telnet::test_connection`]），串口打开后随即关闭
//!   （[`crate::serial::test_open`]，阻塞）。
//!
//! 线程：Telnet 的读写是运行时上的两个任务，串口的读写是两个系统线程（与本地终端相同）；行规程是运行时上的
//! 一个任务。界面线程上的调用（写入、调整大小、切换编码、关闭）都只是放进队列，不会阻塞。

mod line;
mod login;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use termlet_io::{CloseReason, GridSize, IoEvent, TerminalInput, TerminalIo};
use tokio::sync::mpsc;

use crate::id::HostId;
use crate::model::{Backspace, Encoding, Enter, Host, LocalEcho, Protocol};
use crate::secret::SecretString;
use crate::serial::{SerialConfig, SerialError, SerialWire};
use crate::ssh::{ConnectOptions, Prompter, SshError};
use crate::store::Store;
use crate::telnet::{TelnetPlan, TelnetWire};

use line::Discipline;

/// 传输层送来的事件。
#[derive(Debug)]
pub(crate) enum WireEvent {
    /// 去掉协议命令后的数据
    Data(Vec<u8>),
    /// 对方正常关闭了连接
    Eof,
    /// 断开、出错（原因给日志看）
    Lost(String),
}

/// 传输层：Telnet 连接或串口。调用都不阻塞（发送放进队列）。
pub(crate) trait Wire: Send + Sync + 'static {
    /// 发给对方（去掉协议命令之前的原始数据，由传输层转义）。
    fn send(&self, data: &[u8]);
    /// 终端大小变了（Telnet 的 NAWS；串口没有）。
    fn resize(&self, _size: GridSize) {}
    /// 对方回显输入：本地回显为"自动"时据此决定。
    fn remote_echo(&self) -> bool;
    /// 在终端中传输文件开始、结束（Telnet 期间单独的回车不补 NUL）。
    fn set_binary(&self, _on: bool) {}
    /// 见 [`TerminalInput::mangles_control_bytes`]（非二进制模式的 Telnet）。
    fn mangles_control_bytes(&self) -> bool {
        false
    }
    /// 关闭连接；之后事件通道随之结束。
    fn close(&self);
}

/// 行规程的参数（取值见主机设置）。
#[derive(Clone, Debug)]
pub struct RawConfig {
    /// 对方的编码
    pub encoding: Encoding,
    /// 回车键发送的内容
    pub enter: Enter,
    pub local_echo: LocalEcho,
    pub backspace: Backspace,
    /// 对方正常关闭连接（Telnet 中 logout）是会话结束（退出码 0）；为假时按断线处理（串口）
    pub eof_exit: bool,
    /// 自动登录：在输出中看到登录、密码提示时各填写一次（都为空时不自动登录）
    pub username: String,
    pub password: Option<SecretString>,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            encoding: Encoding::Utf8,
            enter: Enter::Crlf,
            local_echo: LocalEcho::Auto,
            backspace: Backspace::Del,
            eof_exit: false,
            username: String::new(),
            password: None,
        }
    }
}

impl RawConfig {
    /// 主机设置中的编码与行规程（按协议取默认值）；不含自动登录。
    pub fn for_host(host: &Host) -> Self {
        Self {
            encoding: host.terminal.encoding,
            enter: host.line.enter_for(host.protocol),
            local_echo: host.line.local_echo_for(host.protocol),
            backspace: host.line.backspace,
            eof_exit: host.protocol == Protocol::Telnet,
            ..Default::default()
        }
    }
}

/// 连到哪里。
#[derive(Clone, Debug)]
pub enum RawTarget {
    Telnet(TelnetPlan),
    Serial(SerialConfig),
}

/// 打开 Telnet、串口终端所需的全部信息（来自主机设置）。
#[derive(Clone, Debug)]
pub struct RawSpec {
    pub target: RawTarget,
    pub config: RawConfig,
}

impl RawSpec {
    /// 保存的 Telnet、串口主机。读取保存的密码（Telnet 自动登录用）与跳板机的凭据。
    pub fn for_host(store: &Store, host: &HostId) -> Result<Self, RawError> {
        let record = store
            .get::<Host>(host)
            .ok_or_else(|| SshError::Config("主机不存在".into()))?;
        let doc = &record.doc;
        let mut config = RawConfig::for_host(doc);
        let target = match doc.protocol {
            Protocol::Telnet => {
                let plan = crate::telnet::plan_for_host(store, host)?;
                config.username = plan.username.clone();
                config.password = plan.password.clone();
                RawTarget::Telnet(plan)
            }
            Protocol::Serial => RawTarget::Serial(SerialConfig::for_host(doc)),
            Protocol::Ssh => return Err(RawError::NotRaw),
        };
        Ok(Self { target, config })
    }
}

/// 打开 Telnet、串口会话失败的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RawError {
    /// Telnet 的网络路径（直连、代理、跳板机）连不上，或读取主机配置、凭据失败：分类与 SSH 相同，
    /// 界面沿用 SSH 错误的文案
    #[error("{0}")]
    Connect(#[from] SshError),
    /// 串口打不开
    #[error("{0}")]
    Serial(#[from] SerialError),
    /// 主机是 SSH 主机
    #[error("不是 Telnet 或串口主机")]
    NotRaw,
}

impl RawError {
    /// 自动重连是否值得一试。
    pub fn is_retryable(&self) -> bool {
        match self {
            RawError::Connect(error) => error.is_retryable(),
            RawError::Serial(error) => error.is_retryable(),
            RawError::NotRaw => false,
        }
    }
}

/// 打开的会话。
pub struct RawTerminal {
    pub io: TerminalIo,
    /// 实际打开的串口（按 USB 标识找回时与主机设置中的不同）；Telnet 为 `None`
    pub serial_port: Option<String>,
}

/// 打开会话（在核心的运行时上调用）。Telnet 经跳板机时可能要确认指纹、输入密码与验证码（经 `prompter`）；
/// `options` 是跳板机连接的保活等（[`crate::ssh::connect_options`]）。
pub async fn open(
    store: Arc<Store>,
    spec: RawSpec,
    size: GridSize,
    prompter: Prompter,
    options: &ConnectOptions,
) -> Result<RawTerminal, RawError> {
    match spec.target {
        RawTarget::Telnet(plan) => {
            let dialed = crate::telnet::dial(store, &plan, prompter, options)
                .await
                .inspect_err(|e| {
                    tracing::warn!(host = %plan.address, port = plan.port, "Telnet 连接失败：{e}")
                })?;
            tracing::info!(host = %plan.address, port = plan.port, "Telnet 已连接");
            let (wire, events) = TelnetWire::start(dialed, &plan.term_type, size);
            Ok(start(wire, events, &spec.config, None))
        }
        RawTarget::Serial(serial) => {
            tracing::debug!(port = %serial.port, baud = serial.baud, "打开串口");
            let opened = tokio::task::spawn_blocking(move || SerialWire::start(&serial))
                .await
                .map_err(|e| SerialError::Io {
                    port: String::new(),
                    reason: e.to_string(),
                })?
                .inspect_err(|e| tracing::warn!("打开串口失败：{e}"))?;
            let (wire, events, port) = opened;
            Ok(start(wire, events, &spec.config, Some(port)))
        }
    }
}

/// 在传输层上开始行规程，返回会话。
pub(crate) fn start(
    wire: Arc<dyn Wire>,
    mut events: mpsc::Receiver<WireEvent>,
    config: &RawConfig,
    serial_port: Option<String>,
) -> RawTerminal {
    let discipline = Arc::new(Mutex::new(Discipline::new(config, Instant::now())));
    let (output_tx, output) = async_channel::bounded::<IoEvent>(64);
    let (echo_tx, mut echo_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let closed = Arc::new(AtomicBool::new(false));
    let eof_exit = config.eof_exit;

    let driver_wire = wire.clone();
    let driver_discipline = discipline.clone();
    let driver_closed = closed.clone();
    crate::runtime::spawn(async move {
        let by_us = || driver_closed.load(Ordering::SeqCst);
        let reason = loop {
            let output = tokio::select! {
                Some(echo) = echo_rx.recv() => echo,
                event = events.recv() => match event {
                    Some(WireEvent::Data(data)) => {
                        let (out, reply) = driver_discipline
                            .lock()
                            .unwrap()
                            .receive(&data, Instant::now());
                        // 自动登录填写的内容（可能是密码）：发出后清零
                        if let Some(reply) = reply {
                            driver_wire.send(&reply);
                        }
                        out
                    }
                    Some(WireEvent::Eof) if by_us() => break CloseReason::Exited(None),
                    Some(WireEvent::Eof) if eof_exit => break CloseReason::Exited(Some(0)),
                    Some(WireEvent::Eof) => break CloseReason::Lost("对方关闭了连接".into()),
                    Some(WireEvent::Lost(_)) | None if by_us() => break CloseReason::Exited(None),
                    Some(WireEvent::Lost(reason)) => break CloseReason::Lost(reason),
                    None => break CloseReason::Lost("连接已断开".into()),
                },
            };
            if !output.is_empty() && output_tx.send(IoEvent::Output(output)).await.is_err() {
                // 终端已不再读取
                driver_wire.close();
                return;
            }
        };
        tracing::info!(?reason, "Telnet / 串口会话已结束");
        driver_wire.close();
        let _ = output_tx.send(IoEvent::Closed(reason)).await;
    });

    let input = Arc::new(RawInput {
        wire,
        discipline,
        echo: echo_tx,
        closed,
    });
    RawTerminal {
        io: TerminalIo { output, input },
        serial_port,
    }
}

struct RawInput {
    wire: Arc<dyn Wire>,
    discipline: Arc<Mutex<Discipline>>,
    /// 本地回显经行规程的任务送给终端，与对方的输出走同一个出口
    echo: mpsc::UnboundedSender<Vec<u8>>,
    closed: Arc<AtomicBool>,
}

impl TerminalInput for RawInput {
    fn write(&self, data: &[u8]) {
        let remote_echo = self.wire.remote_echo();
        let (echo, out) = self.discipline.lock().unwrap().send(data, remote_echo);
        if !echo.is_empty() {
            let _ = self.echo.send(echo);
        }
        if !out.is_empty() {
            self.wire.send(&out);
        }
    }

    fn resize(&self, size: GridSize) {
        self.wire.resize(size);
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.wire.close();
    }

    fn set_encoding(&self, name: &str) {
        let Ok(encoding) =
            serde_json::from_value::<Encoding>(serde_json::Value::String(name.into()))
        else {
            tracing::warn!(name, "不认识的编码");
            return;
        };
        self.discipline.lock().unwrap().set_encoding(encoding);
    }

    /// 终端中的文件传输（ZModem、trzsz）期间数据原样收发：不转换编码、不映射回车、不回显、不检查登录提示，
    /// Telnet 单独的回车不补 NUL。
    fn set_binary(&self, on: bool) {
        self.discipline.lock().unwrap().set_binary(on);
        self.wire.set_binary(on);
    }

    fn mangles_control_bytes(&self) -> bool {
        self.wire.mangles_control_bytes()
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        self.close();
    }
}
