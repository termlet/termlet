//! Telnet 客户端（RFC 854）。
//!
//! 同意 ECHO、SGA、BINARY，主动提供 NAWS（窗口大小，RFC 1073）与 TTYPE（终端类型，RFC 1091），拒绝其他选项
//! （含 LINEMODE：由服务器逐字回显，与 PuTTY 的默认行为相同）。
//!
//! - [`Telnet`] 是协商状态与解析器，不做 I/O：收到的字节去掉命令、得出要回复的协商，发送的数据转义。
//! - [`plan_for_host`] / [`plan_for_draft`] 从存储解析网络路径：直连、经代理（与 SSH 主机相同的五种），或经跳板机
//!   （SSH 主机，跳板机的指纹同样核对）；以及自动登录用的用户名、密码。
//! - [`dial`] 建立网络连接，[`test_connection`] 只测网络路径、不登录。
//! - 打开终端见 [`crate::rawterm::open`]：行规程（编码、回车键、本地回显、自动登录）在那里。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use termlet_io::GridSize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use zeroize::Zeroizing;

use crate::id::{GroupId, HostId};
use crate::model::{Host, Jump, Protocol, SecretField};
use crate::rawterm::{Wire, WireEvent};
use crate::secret::SecretString;
use crate::ssh::plan::ProxyPlan;
use crate::ssh::transport::{self, BoxStream};
use crate::ssh::{
    ConnectOptions, ConnectPlan, Connection, Hop, HostDraft, Prompter, SecretEdit, SshError,
};
use crate::store::{Owner, Store};

// 命令（RFC 854）。
const SE: u8 = 240;
const SB: u8 = 250;
const WILL: u8 = 251;
const WONT: u8 = 252;
const DO: u8 = 253;
const DONT: u8 = 254;
const IAC: u8 = 255;

// 选项。
const OPT_BINARY: u8 = 0;
const OPT_ECHO: u8 = 1;
const OPT_SGA: u8 = 3;
const OPT_TTYPE: u8 = 24;
const OPT_NAWS: u8 = 31;

/// 子协商内容的上限：超出的部分丢掉（终端类型的询问只有两个字节）。
const MAX_SUBNEGOTIATION: usize = 512;

/// 读缓冲的大小。
const READ_BUFFER: usize = 32 * 1024;

/// 关闭时最多等多久把写的一端关好（对方不读时也不卡住）。
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

/// 解析状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parse {
    Data,
    /// 收到 IAC
    Iac,
    /// 收到 WILL / WONT / DO / DONT，等选项
    Option(u8),
    /// 子协商中
    Sub,
    /// 子协商中收到 IAC
    SubIac,
}

/// 一条 Telnet 连接的协商状态与解析器。
pub struct Telnet {
    term_type: String,
    /// 我方已启用的选项
    us: [bool; 256],
    /// 对方已启用的选项
    them: [bool; 256],
    /// 我方发出、等待回复的请求（WILL / DO）：对方的回复不再回复，避免协商循环
    asked_us: [bool; 256],
    asked_them: [bool; 256],
    size: (u16, u16),
    /// 在终端中传输文件（ZModem、trzsz）：发送时不在单独的回车后补 NUL
    raw: bool,
    parse: Parse,
    sub: Vec<u8>,
    last_cr: bool,
}

impl Telnet {
    /// 开始协商：返回状态与要先发出的内容（主动提供窗口大小与终端类型，请求服务器回显与抑制 GA）。
    pub fn new(term_type: &str, size: GridSize) -> (Self, Vec<u8>) {
        let mut telnet = Self {
            term_type: term_type.to_string(),
            us: [false; 256],
            them: [false; 256],
            asked_us: [false; 256],
            asked_them: [false; 256],
            size: (size.columns, size.rows),
            raw: false,
            parse: Parse::Data,
            sub: Vec::new(),
            last_cr: false,
        };
        telnet.asked_us[OPT_NAWS as usize] = true;
        telnet.asked_us[OPT_TTYPE as usize] = true;
        telnet.asked_them[OPT_SGA as usize] = true;
        telnet.asked_them[OPT_ECHO as usize] = true;
        let offers = vec![
            IAC, WILL, OPT_NAWS, IAC, WILL, OPT_TTYPE, IAC, DO, OPT_SGA, IAC, DO, OPT_ECHO,
        ];
        (telnet, offers)
    }

    /// 解析收到的字节：数据部分（去掉命令）追加到 `data`，要回复给服务器的协商追加到 `reply`。
    pub fn receive(&mut self, input: &[u8], data: &mut Vec<u8>, reply: &mut Vec<u8>) {
        for &b in input {
            match self.parse {
                Parse::Data => {
                    if b == IAC {
                        self.parse = Parse::Iac;
                        continue;
                    }
                    // 非二进制模式下单独的回车以 CR NUL 发送，去掉 NUL
                    if b == 0 && self.last_cr && !self.them[OPT_BINARY as usize] {
                        self.last_cr = false;
                        continue;
                    }
                    self.last_cr = b == b'\r';
                    data.push(b);
                }
                Parse::Iac => match b {
                    IAC => {
                        data.push(IAC);
                        self.last_cr = false;
                        self.parse = Parse::Data;
                    }
                    WILL | WONT | DO | DONT => self.parse = Parse::Option(b),
                    SB => {
                        self.sub.clear();
                        self.parse = Parse::Sub;
                    }
                    // NOP、GA、DM 等：忽略
                    _ => self.parse = Parse::Data,
                },
                Parse::Option(command) => {
                    self.negotiate(command, b, reply);
                    self.parse = Parse::Data;
                }
                Parse::Sub => {
                    if b == IAC {
                        self.parse = Parse::SubIac;
                    } else if self.sub.len() < MAX_SUBNEGOTIATION {
                        self.sub.push(b);
                    }
                }
                Parse::SubIac => match b {
                    SE => {
                        self.subnegotiate(reply);
                        self.parse = Parse::Data;
                    }
                    IAC => {
                        if self.sub.len() < MAX_SUBNEGOTIATION {
                            self.sub.push(IAC);
                        }
                        self.parse = Parse::Sub;
                    }
                    _ => self.parse = Parse::Data,
                },
            }
        }
    }

    /// 处理 WILL、WONT、DO、DONT：只在状态改变时回复，避免协商循环（RFC 1143 的简化）。
    fn negotiate(&mut self, command: u8, option: u8, reply: &mut Vec<u8>) {
        let o = option as usize;
        match command {
            WILL => {
                let asked = std::mem::take(&mut self.asked_them[o]);
                if !want_them(option) {
                    reply.extend([IAC, DONT, option]);
                } else if !self.them[o] {
                    self.them[o] = true;
                    if !asked {
                        reply.extend([IAC, DO, option]);
                    }
                }
            }
            WONT => {
                let asked = std::mem::take(&mut self.asked_them[o]);
                if self.them[o] {
                    self.them[o] = false;
                    if !asked {
                        reply.extend([IAC, DONT, option]);
                    }
                }
            }
            DO => {
                let asked = std::mem::take(&mut self.asked_us[o]);
                if !want_us(option) {
                    reply.extend([IAC, WONT, option]);
                } else if !self.us[o] {
                    self.us[o] = true;
                    if !asked {
                        reply.extend([IAC, WILL, option]);
                    }
                    // 服务器同意了窗口大小：立即告诉它
                    if option == OPT_NAWS {
                        reply.extend(naws_frame(self.size));
                    }
                }
            }
            DONT => {
                let asked = std::mem::take(&mut self.asked_us[o]);
                if self.us[o] {
                    self.us[o] = false;
                    if !asked {
                        reply.extend([IAC, WONT, option]);
                    }
                }
            }
            _ => {}
        }
    }

    /// 子协商：服务器询问终端类型（TTYPE SEND）时回复 TTYPE IS 类型。
    fn subnegotiate(&mut self, reply: &mut Vec<u8>) {
        if self.sub.len() >= 2 && self.sub[0] == OPT_TTYPE && self.sub[1] == 1 {
            reply.extend([IAC, SB, OPT_TTYPE, 0]);
            reply.extend(self.term_type.bytes().filter(|&b| b != IAC));
            reply.extend([IAC, SE]);
        }
    }

    /// 转义要发送的数据：值为 255 的字节重复；非二进制模式下单独的回车（后面不是换行）补 NUL（RFC 854）。
    pub fn encode(&self, input: &[u8]) -> Vec<u8> {
        let cr_nul = !self.us[OPT_BINARY as usize] && !self.raw;
        let mut out = Vec::with_capacity(input.len() + 8);
        for (i, &b) in input.iter().enumerate() {
            out.push(b);
            if b == IAC {
                out.push(IAC);
            } else if b == b'\r' && cr_nul && input.get(i + 1) != Some(&b'\n') {
                out.push(0);
            }
        }
        out
    }

    /// 记下终端大小；服务器同意了 NAWS 且大小变了时返回要发送的通知。
    pub fn resize(&mut self, size: GridSize) -> Option<Vec<u8>> {
        let size = (size.columns, size.rows);
        let changed = size != self.size;
        self.size = size;
        (changed && self.us[OPT_NAWS as usize]).then(|| naws_frame(size))
    }

    /// 服务器回显输入（同意了 ECHO）。
    pub fn remote_echo(&self) -> bool {
        self.them[OPT_ECHO as usize]
    }

    /// 在终端中传输文件开始、结束时调用：期间单独的回车不补 NUL。
    pub fn set_raw(&mut self, on: bool) {
        self.raw = on;
    }

    /// 两个方向都协商了二进制模式：服务器不按网络虚拟终端的规则改写回车与 NUL。
    pub fn binary(&self) -> bool {
        self.us[OPT_BINARY as usize] && self.them[OPT_BINARY as usize]
    }
}

/// 我方愿意启用的选项。
fn want_us(option: u8) -> bool {
    matches!(option, OPT_NAWS | OPT_TTYPE | OPT_SGA | OPT_BINARY)
}

/// 同意对方启用的选项。
fn want_them(option: u8) -> bool {
    matches!(option, OPT_ECHO | OPT_SGA | OPT_BINARY)
}

/// 窗口大小的子协商（宽、高各两字节，值为 255 的字节要重复）。
fn naws_frame((columns, rows): (u16, u16)) -> Vec<u8> {
    let mut frame = vec![IAC, SB, OPT_NAWS];
    for b in columns.to_be_bytes().into_iter().chain(rows.to_be_bytes()) {
        frame.push(b);
        if b == IAC {
            frame.push(IAC);
        }
    }
    frame.extend([IAC, SE]);
    frame
}

/// 打开一台 Telnet 主机要用的信息。
#[derive(Clone, Debug)]
pub struct TelnetPlan {
    /// 显示用的名称
    pub label: String,
    pub address: String,
    pub port: u16,
    /// 自动登录的用户名（主机的，没有时沿用分组默认的）；为空时不自动填写用户名。
    /// 也是代理命令中的 `%r`
    pub username: String,
    /// 自动登录的密码（保存了时）
    pub password: Option<SecretString>,
    /// 代理（经跳板机时不使用，与 SSH 相同）
    pub proxy: Option<ProxyPlan>,
    /// 经过的跳板机（SSH 主机，按连接顺序）；为空时直连或经代理
    pub jumps: Vec<Hop>,
    /// 协商的终端类型
    pub term_type: String,
}

/// 保存的 Telnet 主机。
pub fn plan_for_host(store: &Store, host: &HostId) -> Result<TelnetPlan, SshError> {
    let record = store
        .get::<Host>(host)
        .ok_or_else(|| SshError::Config("主机不存在".into()))?;
    let group = record.parent.clone().map(GroupId::from_raw);
    let owner = Owner::host(host);
    build_plan(store, Some(host), group.as_ref(), &record.doc, &|field| {
        read_secret(store, &owner, field)
    })
}

/// 主机对话框中尚未保存的 Telnet 主机（测试连接）：秘密取对话框中的修改，没改的取已保存的。
pub fn plan_for_draft(store: &Store, draft: &HostDraft<'_>) -> Result<TelnetPlan, SshError> {
    let own = |field: SecretField| -> Result<Option<SecretString>, SshError> {
        match draft.secrets.get(field) {
            SecretEdit::Set(value) => Ok(Some(value.clone())),
            SecretEdit::Clear => Ok(None),
            SecretEdit::Keep => match draft.id {
                Some(id) => read_secret(store, &Owner::host(id), field),
                None => Ok(None),
            },
        }
    };
    build_plan(store, draft.id, draft.group, draft.host, &own)
}

fn read_secret(
    store: &Store,
    owner: &Owner,
    field: SecretField,
) -> Result<Option<SecretString>, SshError> {
    store
        .secret(owner, field)
        .map_err(|e| SshError::Vault(e.to_string()))
}

/// 解析规则与 SSH 主机相同（见 [`crate::ssh::plan`]）：跳板机"沿用分组"时取最近的分组默认值（就是这台主机自己时忽略），
/// 跳板机自己的跳板机继续向上；代理只在直连时生效。用户名没有时沿用分组的默认用户名，再没有时不自动填写
/// （不用本机的用户名：设备上多半没有这个用户）。
fn build_plan(
    store: &Store,
    id: Option<&HostId>,
    group: Option<&GroupId>,
    host: &Host,
    own: &dyn Fn(SecretField) -> Result<Option<SecretString>, SshError>,
) -> Result<TelnetPlan, SshError> {
    if host.protocol != Protocol::Telnet {
        return Err(SshError::Config("不是 Telnet 主机".into()));
    }
    let chain = store.group_chain(group);
    let jump = match &host.jump {
        Jump::Direct => None,
        Jump::Via { host } => Some(host.clone()),
        Jump::Inherit => chain
            .iter()
            .find_map(|g| g.doc.defaults.jump.clone())
            .filter(|jump| Some(jump) != id),
    };
    let jumps = match &jump {
        Some(jump) => {
            let is_ssh = store.get::<Host>(jump).map(|r| r.doc.is_ssh());
            match is_ssh {
                None => return Err(SshError::Config("引用的主机已删除".into())),
                Some(false) => return Err(SshError::Config("跳板机不是 SSH 主机".into())),
                Some(true) => crate::ssh::plan_for_host(store, jump)?.hops,
            }
        }
        None => Vec::new(),
    };
    if let Some(id) = id
        && jumps.iter().any(|hop| hop.host_id.as_ref() == Some(id))
    {
        return Err(SshError::Config("跳板机形成了循环".into()));
    }
    let username = std::iter::once(host.username.clone())
        .chain(chain.iter().map(|g| g.doc.defaults.username.clone()))
        .find(|u| !u.trim().is_empty())
        .unwrap_or_default();
    let proxy = if jumps.is_empty() {
        crate::ssh::plan::proxy_plan(host, &chain, own)?
    } else {
        None
    };
    Ok(TelnetPlan {
        label: host.display_name().to_string(),
        address: host.address.clone(),
        port: host.effective_port(),
        username,
        password: own(SecretField::Password)?,
        proxy,
        jumps,
        term_type: host.telnet.term_type.clone(),
    })
}

/// 建好的网络连接。
pub struct Dialed {
    pub stream: BoxStream,
    /// 经过的跳板机连接：随这条流保持，流结束后断开
    pub jump: Option<Arc<Connection>>,
}

/// 按计划建立到 Telnet 服务器的 TCP 连接：直连、经代理，或经跳板机的 `direct-tcpip` 通道。
/// 经跳板机时可能要确认指纹、输入密码与验证码（经 `prompter`）。
pub async fn dial(
    store: Arc<Store>,
    plan: &TelnetPlan,
    prompter: Prompter,
    options: &ConnectOptions,
) -> Result<Dialed, SshError> {
    tracing::debug!(
        host = %plan.address,
        port = plan.port,
        jumps = plan.jumps.len(),
        proxy = plan.proxy.is_some(),
        "建立 Telnet 的 TCP 连接"
    );
    if plan.jumps.is_empty() {
        if let Some(proxy) = &plan.proxy {
            let tokens = crate::ssh::proxy_command::Tokens {
                host: &plan.address,
                port: plan.port,
                user: &plan.username,
                alias: &proxy.alias,
            };
            crate::ssh::proxy_command::ensure_allowed(
                &store,
                proxy,
                &tokens,
                &plan.label,
                &prompter,
            )
            .await?;
        }
        // Telnet 没有握手，连接失败在之后才知道：不需要查看代理命令的进程
        let (stream, _) = transport::first_hop(
            &plan.address,
            plan.port,
            &plan.username,
            plan.proxy.as_ref(),
        )
        .await?;
        return Ok(Dialed { stream, jump: None });
    }
    let jumps = ConnectPlan {
        hops: plan.jumps.clone(),
    };
    let last = jumps.target().label.clone();
    let connection = crate::ssh::connect(store, &jumps, prompter, options)
        .await
        .map_err(|error| as_jump_error(&last, error))?;
    let channel = connection
        .handle()
        .channel_open_direct_tcpip(plan.address.clone(), plan.port as u32, "127.0.0.1", 0)
        .await
        .map_err(|e| SshError::JumpTarget {
            host: plan.address.clone(),
            port: plan.port,
            reason: e.to_string(),
        })?;
    Ok(Dialed {
        stream: Box::new(channel.into_stream()),
        jump: Some(connection),
    })
}

/// 跳板机（连接计划中的最后一跳）连不上时说明是哪一台；指纹、取消、配置类的错误原样返回。
fn as_jump_error(label: &str, error: SshError) -> SshError {
    match error {
        SshError::Dns { .. }
        | SshError::Refused { .. }
        | SshError::Timeout { .. }
        | SshError::Network { .. }
        | SshError::Proxy(_)
        | SshError::ProxyCommand(_)
        | SshError::ProxyCommandExited { .. }
        | SshError::NoCommonAlgorithm(_)
        | SshError::AuthFailed { .. }
        | SshError::Certificate(_)
        | SshError::Key(_)
        | SshError::Channel(_)
        | SshError::Disconnected
        | SshError::Protocol(_) => SshError::Jump {
            jump: label.to_string(),
            reason: error.to_string(),
        },
        other => other,
    }
}

/// 测试连接：只测网络路径（经跳板机、代理建立 TCP 连接），不登录，随即断开。
/// 返回耗时（不含等待用户回答的时间）。
pub async fn test_connection(
    store: Arc<Store>,
    plan: &TelnetPlan,
    prompter: Prompter,
    options: &ConnectOptions,
) -> Result<Duration, SshError> {
    let waited = Arc::new(Mutex::new(Duration::ZERO));
    let timed: Prompter = {
        let waited = waited.clone();
        Arc::new(move |prompt| {
            let prompter = prompter.clone();
            let waited = waited.clone();
            Box::pin(async move {
                let started = Instant::now();
                let answer = prompter(prompt).await;
                *waited.lock().unwrap() += started.elapsed();
                answer
            })
        })
    };
    let options = ConnectOptions {
        keepalive: None,
        ..options.clone()
    };
    let started = Instant::now();
    let dialed = dial(store, plan, timed, &options).await?;
    let elapsed = started.elapsed().saturating_sub(*waited.lock().unwrap());
    drop(dialed.stream);
    if let Some(jump) = dialed.jump {
        jump.disconnect().await;
    }
    Ok(elapsed.max(Duration::from_millis(1)))
}

/// Telnet 连接作为行规程的传输层：读写各一个任务，在核心的运行时上。
pub(crate) struct TelnetWire {
    state: Arc<Mutex<Telnet>>,
    /// 发给服务器的帧（可能是自动登录填写的密码：丢弃时清零）
    queue: mpsc::UnboundedSender<Zeroizing<Vec<u8>>>,
    closed: watch::Sender<bool>,
}

impl TelnetWire {
    /// 在建好的连接上开始协商，返回传输层与它的事件（数据、结束）。
    pub(crate) fn start(
        dialed: Dialed,
        term_type: &str,
        size: GridSize,
    ) -> (Arc<Self>, mpsc::Receiver<WireEvent>) {
        let (telnet, offers) = Telnet::new(term_type, size);
        let state = Arc::new(Mutex::new(telnet));
        let (queue, mut frames) = mpsc::unbounded_channel::<Zeroizing<Vec<u8>>>();
        let (closed, _) = watch::channel(false);
        let (events_tx, events) = mpsc::channel::<WireEvent>(16);
        let _ = queue.send(Zeroizing::new(offers));
        let (mut reader, mut writer) = tokio::io::split(dialed.stream);

        // 读：服务器 → 去掉命令、回复协商 → 行规程
        let reader_state = state.clone();
        let reader_queue = queue.clone();
        let reader_closed = closed.clone();
        let mut stop = closed.subscribe();
        let jump = dialed.jump.clone();
        crate::runtime::spawn(async move {
            let mut buf = vec![0u8; READ_BUFFER];
            let end = loop {
                let read = tokio::select! {
                    read = reader.read(&mut buf) => read,
                    _ = stop.wait_for(|closed| *closed) => break None,
                };
                let n = match read {
                    Ok(0) => {
                        // 跳板机断开时通道也随之结束：那是断线，不是服务器让我们退出
                        break Some(if jump.as_ref().is_some_and(|j| j.is_closed()) {
                            WireEvent::Lost("跳板机的连接已断开".into())
                        } else {
                            WireEvent::Eof
                        });
                    }
                    Ok(n) => n,
                    Err(e) => break Some(WireEvent::Lost(e.to_string())),
                };
                let (mut data, mut reply) = (Vec::new(), Vec::new());
                reader_state
                    .lock()
                    .unwrap()
                    .receive(&buf[..n], &mut data, &mut reply);
                if !reply.is_empty() {
                    let _ = reader_queue.send(Zeroizing::new(reply));
                }
                if data.is_empty() {
                    continue;
                }
                tokio::select! {
                    sent = events_tx.send(WireEvent::Data(data)) => if sent.is_err() { break None },
                    _ = stop.wait_for(|closed| *closed) => break None,
                }
            };
            if let Some(event) = end {
                let _ = events_tx.send(event).await;
            }
            // 读的一端结束：写的一端随之结束，释放连接
            reader_closed.send_replace(true);
        });

        // 写：协商、终端输入 → 服务器
        let mut stop = closed.subscribe();
        let keep_jump = dialed.jump;
        crate::runtime::spawn(async move {
            let _jump = keep_jump;
            loop {
                let frame = tokio::select! {
                    frame = frames.recv() => frame,
                    _ = stop.wait_for(|closed| *closed) => None,
                };
                let Some(frame) = frame else { break };
                // 对方还活着但不再读（发送缓冲写满）时写入会一直等：关闭时不等它，放下连接与跳板机
                let wrote = tokio::select! {
                    wrote = async {
                        writer.write_all(&frame).await?;
                        writer.flush().await
                    } => wrote.is_ok(),
                    _ = stop.wait_for(|closed| *closed) => false,
                };
                if !wrote {
                    break;
                }
            }
            let _ = tokio::time::timeout(SHUTDOWN_WAIT, writer.shutdown()).await;
        });

        (
            Arc::new(Self {
                state,
                queue,
                closed,
            }),
            events,
        )
    }
}

impl Wire for TelnetWire {
    fn send(&self, data: &[u8]) {
        let frame = self.state.lock().unwrap().encode(data);
        let _ = self.queue.send(Zeroizing::new(frame));
    }

    fn resize(&self, size: GridSize) {
        let frame = self.state.lock().unwrap().resize(size);
        if let Some(frame) = frame {
            let _ = self.queue.send(Zeroizing::new(frame));
        }
    }

    fn remote_echo(&self) -> bool {
        self.state.lock().unwrap().remote_echo()
    }

    fn set_binary(&self, on: bool) {
        self.state.lock().unwrap().set_raw(on);
    }

    fn mangles_control_bytes(&self) -> bool {
        !self.state.lock().unwrap().binary()
    }

    fn close(&self) {
        self.closed.send_replace(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> Telnet {
        let (telnet, offers) = Telnet::new("xterm-256color", GridSize::new(80, 24));
        // 客户端先主动协商
        assert_eq!(
            offers,
            [
                IAC, WILL, OPT_NAWS, IAC, WILL, OPT_TTYPE, IAC, DO, OPT_SGA, IAC, DO, OPT_ECHO
            ]
        );
        telnet
    }

    fn receive(telnet: &mut Telnet, input: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let (mut data, mut reply) = (Vec::new(), Vec::new());
        telnet.receive(input, &mut data, &mut reply);
        (data, reply)
    }

    #[test]
    fn negotiation() {
        let mut telnet = connected();
        let (data, reply) = receive(
            &mut telnet,
            &[
                // 同意我方的 NAWS、TTYPE（不需要回复），之后发送窗口大小
                IAC, DO, OPT_NAWS, IAC, DO, OPT_TTYPE,
                // 同意回显、抑制 GA（回复已经发过）
                IAC, WILL, OPT_ECHO, IAC, WILL, OPT_SGA,
                // 请求 LINEMODE：拒绝；询问终端类型
                IAC, DO, 34, IAC, SB, OPT_TTYPE, 1, IAC, SE,
                // 数据：IAC IAC 是 255，CR NUL 去掉 NUL
                b'o', b'k', IAC, IAC, b'\r', 0, b'\n',
            ],
        );
        let mut want = vec![
            IAC, SB, OPT_NAWS, 0, 80, 0, 24, IAC, SE, IAC, WONT, 34, IAC, SB, OPT_TTYPE, 0,
        ];
        want.extend(b"xterm-256color");
        want.extend([IAC, SE]);
        assert_eq!(reply, want);
        assert_eq!(data, [b'o', b'k', 255, b'\r', b'\n']);
        assert!(telnet.remote_echo(), "服务器同意了 ECHO");

        // 窗口大小变化（含值为 255 的字节）；大小不变时不发
        assert_eq!(
            telnet.resize(GridSize::new(255, 50)).unwrap(),
            [IAC, SB, OPT_NAWS, 0, 255, 255, 0, 50, IAC, SE]
        );
        assert_eq!(telnet.resize(GridSize::new(255, 50)), None);

        // 发送：255 重复，单独的回车补 NUL，CR LF 不变
        assert_eq!(
            telnet.encode(&[b'a', 255, b'\r', b'b', b'\r', b'\n']),
            [b'a', 255, 255, b'\r', 0, b'b', b'\r', b'\n']
        );
        // 末尾单独的回车同样补 NUL
        assert_eq!(telnet.encode(b"x\r"), [b'x', b'\r', 0]);

        // 服务器不再回显：确认（我方曾同意，需要回复 DONT）；重复的 WONT 不再回复
        let (data, reply) = receive(
            &mut telnet,
            &[IAC, WONT, OPT_ECHO, IAC, WONT, OPT_ECHO, b'x'],
        );
        assert_eq!(reply, [IAC, DONT, OPT_ECHO]);
        assert_eq!(data, b"x");
        assert!(!telnet.remote_echo(), "服务器已不再回显");
    }

    #[test]
    fn binary_option() {
        let mut telnet = connected();
        // 服务器请求双向二进制：同意
        let (data, reply) = receive(
            &mut telnet,
            &[IAC, DO, OPT_BINARY, IAC, WILL, OPT_BINARY, b'\r', 0],
        );
        assert_eq!(reply, [IAC, WILL, OPT_BINARY, IAC, DO, OPT_BINARY]);
        // 二进制模式下 CR NUL 原样保留
        assert_eq!(data, [b'\r', 0]);
        // 发送单独的回车不补 NUL
        assert_eq!(telnet.encode(b"\r"), b"\r");
    }

    #[test]
    fn commands_split_across_reads_and_unknown_ones() {
        let mut telnet = connected();
        // 命令被拆在几次读取之间
        let (data, reply) = receive(&mut telnet, &[b'a', IAC]);
        assert_eq!((data.as_slice(), reply.as_slice()), (&b"a"[..], &[][..]));
        let (data, reply) = receive(&mut telnet, &[DO]);
        assert!(data.is_empty() && reply.is_empty());
        let (data, reply) = receive(&mut telnet, &[OPT_SGA, b'b']);
        assert_eq!(reply, [IAC, WILL, OPT_SGA], "同意抑制 GA");
        assert_eq!(data, b"b");
        // 重复的 DO 不再回复；不认识的 WILL 拒绝；NOP（241）、GA（249）忽略
        let (data, reply) = receive(&mut telnet, &[IAC, DO, OPT_SGA, IAC, WILL, 5, IAC, 241]);
        assert_eq!(reply, [IAC, DONT, 5]);
        assert!(data.is_empty());
        let (data, _) = receive(&mut telnet, &[IAC, 249, b'c']);
        assert_eq!(data, b"c");
        // 子协商中的 IAC IAC 与过长的内容
        let mut long = vec![IAC, SB, 99];
        long.extend(std::iter::repeat_n(7, 2000));
        long.extend([IAC, IAC, IAC, SE, b'd']);
        let (data, reply) = receive(&mut telnet, &long);
        assert_eq!(data, b"d");
        assert!(reply.is_empty());
        // 单独的回车后面不是 NUL：照常保留
        let (data, _) = receive(&mut telnet, b"\rx");
        assert_eq!(data, b"\rx");
    }

    #[test]
    fn raw_mode_keeps_lone_carriage_returns() {
        let mut telnet = connected();
        telnet.set_raw(true);
        assert_eq!(telnet.encode(&[b'\r', 255]), [b'\r', 255, 255]);
        telnet.set_raw(false);
        assert_eq!(telnet.encode(b"\r"), [b'\r', 0]);
    }

    #[test]
    fn naws_before_agreement_is_not_sent() {
        let (mut telnet, _) = Telnet::new("vt100", GridSize::new(80, 24));
        assert_eq!(telnet.resize(GridSize::new(100, 30)), None);
        // 同意后发的是最新的大小
        let (_, reply) = receive(&mut telnet, &[IAC, DO, OPT_NAWS]);
        assert_eq!(reply, [IAC, SB, OPT_NAWS, 0, 100, 0, 30, IAC, SE]);
        // 拒绝后不再发
        let (_, reply) = receive(&mut telnet, &[IAC, DONT, OPT_NAWS]);
        assert_eq!(reply, [IAC, WONT, OPT_NAWS]);
        assert_eq!(telnet.resize(GridSize::new(120, 40)), None);
    }

    // 对方还活着但不再读（发送缓冲满了）：关闭时写的一端不再等，连接随之释放
    #[test]
    fn close_does_not_wait_for_a_stalled_peer() {
        let rt = crate::runtime::runtime();
        let (ours, mut theirs) = tokio::io::duplex(64);
        let (wire, _events) = rt.block_on(async {
            TelnetWire::start(
                Dialed {
                    stream: Box::new(ours),
                    jump: None,
                },
                "xterm",
                GridSize::new(80, 24),
            )
        });
        wire.send(&vec![b'x'; 1 << 20]);
        std::thread::sleep(Duration::from_millis(200));
        wire.close();
        std::thread::sleep(Duration::from_millis(200));
        // 现在才开始读：只有关闭前已写进缓冲的，之后是连接结束；没有放下的话会把整个 1 MB 写完
        let total = rt.block_on(async {
            let mut total = 0;
            let mut buf = [0u8; 4096];
            loop {
                match tokio::time::timeout(Duration::from_secs(5), theirs.read(&mut buf)).await {
                    Ok(Ok(0)) => return total,
                    Ok(Ok(n)) => total += n,
                    other => panic!("连接没有结束：{other:?}"),
                }
            }
        });
        assert!(total < 4096, "关闭后仍在写：{total} 字节");
    }
}
