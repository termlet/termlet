//! 端口转发在界面一侧：显示名称与路线、错误文案，以及经核心的转发管理器启动、停止、检测服务器端口。
//!
//! [`ForwardsModel`] 是应用级的（`AppState`）：持有管理器、最近的状态快照（变化时通知观察者，侧边栏的列表与
//! 标题栏的“N 个转发”据此重绘），以及管理器没有记下的启动错误。启动时的询问（确认指纹、输入密码）在启动它的
//! 窗口里弹出。
//!
//! 主机工具的“在浏览器中打开”（[`open_in_browser`]）也经这里：沿用或新建一条本地转发规则，共用终端的连接启动、随连接
//! 结束而停，建好后交给系统的浏览器（测试中换成记录下来，见 [`BrowserOpener`]）。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, AppContext as _, Context, Entity, Global, SharedString, Task, Window};
use termlet_core::forward::{
    ForwardError, ForwardManager, ForwardOptions, ForwardSnapshot, ForwardState, ListeningPort,
    SocksError, StartRequest, TargetError,
};
use termlet_core::id::{ForwardId, HostId};
use termlet_core::model::{Forward, ForwardKind, Host, join_host_port};
use termlet_core::ssh::{self, Prompter};
use termlet_core::store::{Position, Store};

use crate::app_state::AppState;
use crate::sftp::panel::window_owner;
use crate::terminal_panel::TerminalPanel;
use crate::{ssh_errors, t};

/// 转发的应用级状态。
pub struct ForwardsModel {
    manager: ForwardManager,
    snapshot: Arc<ForwardSnapshot>,
    /// 正在启动（等待管理器返回）的规则
    starting: HashSet<ForwardId>,
    /// 启动失败而管理器没有记下状态的原因（例如共用的终端已断开）
    errors: HashMap<ForwardId, SharedString>,
    /// 勾了“自动启动”的规则已经启动过（第一个窗口打开、保险库解锁后只做一次）
    auto_started: bool,
    _watch: Task<()>,
}

impl ForwardsModel {
    pub fn new(store: Arc<Store>, cx: &mut Context<Self>) -> Self {
        let manager = ForwardManager::new(store, ForwardOptions::default());
        let mut changes = manager.watch();
        let watch = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                let snapshot = changes.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| {
                        this.snapshot = snapshot;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            snapshot: Arc::new(manager.snapshot()),
            manager,
            starting: HashSet::new(),
            errors: HashMap::new(),
            auto_started: false,
            _watch: watch,
        }
    }

    pub fn manager(&self) -> &ForwardManager {
        &self.manager
    }
}

fn model(cx: &App) -> Entity<ForwardsModel> {
    AppState::global(cx).forwards.clone()
}

/// 服务器上正在监听的端口（检测的结果）。
pub type Port = ListeningPort;

/// 列表中的路线：本地 `监听:端口 → 目标:端口`，远程“服务器 监听 → 本机 目标”，动态 `SOCKS5 监听:端口`。
pub fn route(rule: &Forward) -> SharedString {
    let bind = join_host_port(&rule.bind_host, rule.bind_port);
    let target = join_host_port(&rule.target_host, rule.target_port);
    match rule.kind {
        ForwardKind::Local => format!("{bind} → {target}").into(),
        ForwardKind::Remote => t!("forward.route_remote", bind = bind, target = target),
        ForwardKind::Dynamic => format!("SOCKS5 {bind}").into(),
    }
}

/// 显示的名称：没有名称时按端口写。
pub fn display_name(rule: &Forward) -> SharedString {
    if !rule.name.is_empty() {
        return rule.name.clone().into();
    }
    let target = join_host_port(&rule.target_host, rule.target_port);
    match rule.kind {
        ForwardKind::Local => format!("{} → {target}", rule.bind_port).into(),
        ForwardKind::Remote => t!(
            "forward.label_remote",
            port = rule.bind_port,
            target = target
        ),
        ForwardKind::Dynamic => format!("SOCKS5 {}", rule.bind_port).into(),
    }
}

/// 和已有的规则监听同一个地址与端口时，说明被哪条规则占用。
pub fn listen_conflict(
    store: &Store,
    rule: &Forward,
    editing: Option<&ForwardId>,
) -> Option<SharedString> {
    let other = termlet_core::forward::find_conflict(store, rule, editing)?;
    let address = join_host_port(&rule.bind_host, rule.bind_port);
    let name = display_name(&other.doc);
    Some(if rule.kind == ForwardKind::Remote {
        t!("forward.conflict_remote", address = address, name = name)
    } else {
        t!("forward.conflict_local", address = address, name = name)
    })
}

/// 规则所属主机的名称；主机找不到时为 ID。
pub fn host_name(store: &Arc<Store>, host: Option<&HostId>) -> SharedString {
    match host {
        Some(id) => store
            .get::<Host>(id)
            .map(|h| SharedString::from(h.doc.name))
            .unwrap_or_else(|| id.as_str().to_string().into()),
        None => SharedString::default(),
    }
}

pub fn describe_error(error: &ForwardError) -> SharedString {
    match error {
        ForwardError::RuleNotFound => t!("forward.error.rule_not_found"),
        ForwardError::HostNotFound => t!("forward.error.host_not_found"),
        ForwardError::NotSsh => t!("forward.error.not_ssh"),
        ForwardError::PortInUse { address } => t!("forward.error.port_in_use", address = address),
        ForwardError::ListenDenied { address } => {
            t!("forward.error.listen_denied", address = address)
        }
        ForwardError::AddressUnavailable { address } => {
            t!("forward.error.address_unavailable", address = address)
        }
        ForwardError::Listen { address, reason } => {
            t!("forward.error.listen", address = address, reason = reason)
        }
        ForwardError::RemoteListenDenied { address } => {
            t!("forward.error.remote_listen_denied", address = address)
        }
        ForwardError::SingleChannel => t!("forward.error.single_channel"),
        ForwardError::Connect(error) => ssh_errors::describe(error),
        ForwardError::Stopped => t!("forward.error.stopped"),
    }
}

pub fn describe_target(error: &TargetError) -> SharedString {
    match error {
        TargetError::Unreachable { target } => t!("forward.target.unreachable", target = target),
        TargetError::Prohibited { target } => t!("forward.target.prohibited", target = target),
        TargetError::Channel { target, reason } => {
            t!("forward.target.channel", target = target, reason = reason)
        }
        TargetError::LocalRefused { target } => t!("forward.target.local_refused", target = target),
        TargetError::Local { target, reason } => {
            t!("forward.target.local", target = target, reason = reason)
        }
        TargetError::Socks(socks) => match socks {
            SocksError::Read => t!("forward.socks.read"),
            SocksError::Timeout => t!("forward.socks.timeout"),
            SocksError::NotSocks => t!("forward.socks.not_socks"),
            SocksError::AuthRequired => t!("forward.socks.auth_required"),
            SocksError::UnsupportedCommand(command) => {
                t!("forward.socks.unsupported_command", command = command)
            }
            SocksError::UnsupportedAddress(kind) => {
                t!("forward.socks.unsupported_address", kind = kind)
            }
            SocksError::BadDomain => t!("forward.socks.bad_domain"),
            SocksError::Malformed => t!("forward.socks.malformed"),
        },
    }
}

/// 一条规则在列表中显示的状态。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowStatus {
    pub state: RowState,
    /// 当前的连接数、收到与发出的字节数
    pub connections: usize,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// 运行中最近一次连接目标失败的原因
    pub last_error: Option<SharedString>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum RowState {
    #[default]
    Stopped,
    Starting,
    Running,
    /// SSH 连接断开，正在第几次重连
    Reconnecting(u32),
    Error(SharedString),
}

impl RowState {
    /// 运行类的状态（启动中、运行中、重连中）：按钮为“停止”。
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            RowState::Starting | RowState::Running | RowState::Reconnecting(_)
        )
    }
}

/// 规则现在的状态。
pub fn status(id: &ForwardId, cx: &App) -> RowStatus {
    let model = model(cx);
    let model = model.read(cx);
    let status = model.snapshot.status(id);
    let state = match &status.state {
        ForwardState::Running => RowState::Running,
        ForwardState::Starting => RowState::Starting,
        ForwardState::Reconnecting { attempt, .. } => RowState::Reconnecting(*attempt),
        ForwardState::Failed(error) => RowState::Error(describe_error(error)),
        ForwardState::Stopped if model.starting.contains(id) => RowState::Starting,
        ForwardState::Stopped => match model.errors.get(id) {
            Some(message) => RowState::Error(message.clone()),
            None => RowState::Stopped,
        },
    };
    RowStatus {
        state,
        connections: status.connections,
        bytes_in: status.bytes_in,
        bytes_out: status.bytes_out,
        last_error: status.last_error.as_ref().map(describe_target),
    }
}

/// 运行类状态的规则数（标题栏）。
pub fn active_count(cx: &App) -> usize {
    model(cx).read(cx).snapshot.active()
}

/// 观察转发的状态变化。
pub fn observe<T: 'static>(
    cx: &mut Context<T>,
    on_change: impl FnMut(&mut T, Entity<ForwardsModel>, &mut Context<T>) + 'static,
) -> gpui_kit::Subscription {
    let model = model(cx);
    cx.observe(&model, on_change)
}

/// 启动成功之后在启动它的窗口里做的事（“在浏览器中打开”：打开链接）。
type OnStarted = Box<dyn FnOnce(&mut Window, &mut App)>;

/// 启动：经管理器启动，`share` 为共用的终端连接组。共用失败（只用一个通道）时改为单独连接（随连接结束而停的不改：
/// 它跟着的就是终端的连接）。`announce` 时结果弹出提示；成功后调用 `on_started`。
#[allow(clippy::too_many_arguments)]
fn launch(
    id: &ForwardId,
    owner: String,
    share: Option<String>,
    prompter: Prompter,
    announce: Option<SharedString>,
    until_disconnect: bool,
    on_started: Option<OnStarted>,
    window: &mut Window,
    cx: &mut App,
) {
    let model = model(cx);
    let manager = model.read(cx).manager.clone();
    model.update(cx, |model, cx| {
        model.starting.insert(id.clone());
        model.errors.remove(id);
        cx.notify();
    });
    let id = id.clone();
    tracing::info!(forward = %id, share = share.is_some(), "启动端口转发");
    let rule = id.clone();
    let job = termlet_core::runtime::spawn(async move {
        let request = |share_group: Option<String>| StartRequest {
            rule: rule.clone(),
            owner: owner.clone(),
            share_group,
            prompter: prompter.clone(),
            until_disconnect,
        };
        match manager.start(request(share.clone())).await {
            Err(ForwardError::SingleChannel) if share.is_some() && !until_disconnect => {
                manager.start(request(None)).await
            }
            other => other,
        }
    });
    // 状态在应用级更新：启动途中关闭了窗口也要清掉“启动中”；只有提示依赖窗口
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let result = match job.await {
            Ok(result) => result,
            Err(e) => Err(ForwardError::Connect(ssh::SshError::Protocol(
                e.to_string(),
            ))),
        };
        match &result {
            Ok(()) => tracing::info!(forward = %id, "端口转发已启动"),
            Err(error) => tracing::warn!(forward = %id, "端口转发启动失败：{error}"),
        }
        cx.update(|cx| {
            model.update(cx, |model, cx| {
                model.starting.remove(&id);
                if let Err(error) = &result {
                    model.errors.insert(id.clone(), describe_error(error));
                }
                cx.notify();
            });
            let _ = handle.update(cx, |_, window, cx| {
                // 成功后还有事要做时（打开浏览器）由它提示
                match (&result, announce) {
                    (Ok(()), Some(name)) if on_started.is_none() => window.push_notification(
                        Notification::success(t!("forward.started", name = name)),
                        cx,
                    ),
                    (Err(error), Some(_)) => {
                        crate::notify::error(describe_error(error), window, cx)
                    }
                    _ => {}
                }
                if result.is_ok()
                    && let Some(on_started) = on_started
                {
                    on_started(window, cx);
                }
            });
        });
    })
    .detach();
}

/// 启动（新连接；首次连接可能要确认指纹、输入密码）。结果显示在列表中。
pub fn start(id: &ForwardId, window: &mut Window, cx: &mut App) {
    let owner = window_owner(window);
    let prompter = AppState::prompter_for(window, cx);
    launch(
        id,
        owner,
        None,
        ssh::remembering(prompter),
        None,
        false,
        None,
        window,
        cx,
    );
}

/// 保存后立即启动，共用这个终端的连接（不再认证）；结果弹出提示。
pub fn start_from_terminal(
    id: &ForwardId,
    terminal: &Entity<TerminalPanel>,
    window: &mut Window,
    cx: &mut App,
) {
    let owner = window_owner(window);
    let (share, prompter) = {
        let panel = terminal.read(cx);
        (panel.share_group(), panel.prompter())
    };
    let name = AppState::store(cx)
        .get::<Forward>(id)
        .map(|r| {
            if r.doc.name.is_empty() {
                r.doc.bind_port.to_string().into()
            } else {
                SharedString::from(r.doc.name)
            }
        })
        .unwrap_or_default();
    launch(
        id,
        owner,
        share,
        prompter,
        Some(name),
        false,
        None,
        window,
        cx,
    );
}

// ------------------------------------------------------------ 在浏览器中打开

/// 把链接交给系统的浏览器（全局量；没有时直接 `open_url`）：测试中换成记录下来，不真的打开浏览器。
pub struct BrowserOpener(pub Rc<dyn Fn(&str, &mut App)>);

impl Global for BrowserOpener {}

fn open_url(url: &str, cx: &mut App) {
    match cx.try_global::<BrowserOpener>().map(|o| o.0.clone()) {
        Some(opener) => opener(url, cx),
        None => cx.open_url(url),
    }
}

/// “在浏览器中打开”的请求（主机工具）。
pub struct BrowserRequest {
    /// 经由的主机（保存的 SSH 主机）
    pub host: HostId,
    /// 服务器上的目标地址（监听全部地址时为 127.0.0.1）与端口
    pub target_host: String,
    pub port: u16,
    pub https: bool,
    /// 新建的规则的名称（进程名、容器名，可能为空）
    pub name: String,
}

/// 能拿来“在浏览器中打开”的已有规则：同一台主机、同一个目标的本地转发，监听本机的回环地址（或全部地址）。
fn browser_rule(
    store: &Store,
    request: &BrowserRequest,
) -> Option<termlet_core::store::Record<Forward>> {
    store.list::<Forward>().into_iter().find(|r| {
        r.doc.kind == ForwardKind::Local
            && r.doc.host.as_ref() == Some(&request.host)
            && r.doc.target_host == request.target_host
            && r.doc.target_port == request.port
            && matches!(
                r.doc.bind_host.as_str(),
                "127.0.0.1" | "localhost" | "0.0.0.0" | "::1"
            )
    })
}

/// 本机这个地址现在能不能监听（端口空闲）。
fn can_bind(host: &str, port: u16) -> bool {
    let host = if host == "localhost" {
        "127.0.0.1"
    } else {
        host
    };
    std::net::TcpListener::bind((host, port)).is_ok()
}

/// 本机端口：先试与服务器相同的端口（1024 以上、空闲、不与已有的规则冲突：网页里写死端口的跳转照样能用），不行时由
/// 系统挑一个空闲的。
fn pick_port(store: &Store, rule: &Forward, preferred: u16) -> Option<u16> {
    let fits = |port: u16| {
        let mut probe = rule.clone();
        probe.bind_port = port;
        termlet_core::forward::find_conflict(store, &probe, None).is_none()
            && can_bind(&probe.bind_host, port)
    };
    if preferred >= 1024 && fits(preferred) {
        return Some(preferred);
    }
    (0..8).find_map(|_| {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .ok()?
            .local_addr()
            .ok()?
            .port();
        fits(port).then_some(port)
    })
}

/// 浏览器打开的地址：回环地址或全部地址都用 127.0.0.1，IPv6 回环用 `[::1]`。
pub fn browser_url(https: bool, bind_host: &str, port: u16) -> String {
    let scheme = if https { "https" } else { "http" };
    let host = if bind_host == "::1" {
        "[::1]"
    } else {
        "127.0.0.1"
    };
    format!("{scheme}://{host}:{port}/")
}

/// 为“在浏览器中打开”准备规则：沿用已有的（本机端口被别的程序占了时换一个空闲的），没有时新建一条。返回规则与它是否
/// 已经在运行。
pub fn prepare_browser_rule(
    store: &Store,
    request: &BrowserRequest,
    running: impl Fn(&ForwardId) -> bool,
) -> Result<(ForwardId, Forward, bool), SharedString> {
    if let Some(existing) = browser_rule(store, request) {
        if running(&existing.id) {
            return Ok((existing.id, existing.doc, true));
        }
        if can_bind(&existing.doc.bind_host, existing.doc.bind_port) {
            return Ok((existing.id, existing.doc, false));
        }
        let port = pick_port(store, &existing.doc, request.port)
            .ok_or_else(|| t!("forward.browser_no_port"))?;
        let mut doc = existing.doc.clone();
        doc.bind_port = port;
        store
            .update(&existing.id, doc.clone())
            .map_err(|e| crate::store_errors::describe(&e))?;
        return Ok((existing.id, doc, false));
    }
    let mut doc = Forward {
        kind: ForwardKind::Local,
        host: Some(request.host.clone()),
        name: request.name.clone(),
        bind_host: "127.0.0.1".into(),
        bind_port: 0,
        target_host: request.target_host.clone(),
        target_port: request.port,
        auto_start: false,
        ..Default::default()
    };
    doc.bind_port =
        pick_port(store, &doc, request.port).ok_or_else(|| t!("forward.browser_no_port"))?;
    let id = store
        .create(doc.clone(), None, Position::Last)
        .map_err(|e| crate::store_errors::describe(&e))?;
    Ok((id, doc, false))
}

/// 主机工具的“在浏览器中打开”：经由主机把服务器上的端口转发到本机（沿用或新建规则，在侧边栏的“转发”中可见、可停），
/// 共用终端的连接（`share_group`，不再认证；`prompter` 是终端的询问）、随连接结束而停，建好后用系统的浏览器打开
/// `http(s)://127.0.0.1:本机端口/`，并提示转发到了哪个本机端口。
pub fn open_in_browser(
    request: BrowserRequest,
    share_group: Option<String>,
    prompter: Prompter,
    window: &mut Window,
    cx: &mut App,
) {
    let store = AppState::store(cx);
    let prepared = prepare_browser_rule(&store, &request, |id| {
        status(id, cx).state == RowState::Running
    });
    let (id, rule, running) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            crate::notify::error(message, window, cx);
            return;
        }
    };
    let url = browser_url(request.https, &rule.bind_host, rule.bind_port);
    tracing::info!(forward = %id, running, https = request.https, "主机工具：在浏览器中打开转发的端口");
    let address = join_host_port(&rule.bind_host, rule.bind_port);
    let opened = move |window: &mut Window, cx: &mut App| {
        window.push_notification(
            Notification::success(t!("forward.browser_opened", address = address)),
            cx,
        );
        open_url(&url, cx);
    };
    if running {
        opened(window, cx);
        return;
    }
    let owner = window_owner(window);
    launch(
        &id,
        owner,
        share_group,
        prompter,
        Some(display_name(&rule)),
        true,
        Some(Box::new(opened)),
        window,
        cx,
    );
}

pub fn stop(id: &ForwardId, window: &mut Window, cx: &mut App) {
    let model = model(cx);
    let manager = model.read(cx).manager.clone();
    model.update(cx, |model, cx| {
        model.errors.remove(id);
        cx.notify();
    });
    let id = id.clone();
    tracing::info!(forward = %id, "停止端口转发");
    let job = termlet_core::runtime::spawn(async move { manager.stop(&id).await });
    window
        .spawn(cx, async move |cx| {
            if let Ok(Err(error)) = job.await
                && !matches!(error, ForwardError::RuleNotFound)
            {
                let _ = cx
                    .update(|window, cx| crate::notify::error(describe_error(&error), window, cx));
            }
        })
        .detach();
}

/// 改了运行中的规则：按新的配置重新启动（管理器发现规则变了会先停掉旧的）；原来没在运行的不启动。
pub fn restart_if_running(id: &ForwardId, was_active: bool, window: &mut Window, cx: &mut App) {
    if was_active {
        tracing::info!(forward = %id, "规则改了，重新启动端口转发");
        start(id, window, cx);
    }
}

/// 勾了“自动启动”的规则：第一个窗口打开、保险库解锁后逐条启动（只做一次）。
pub fn auto_start(window: &mut Window, cx: &mut App) {
    let model = model(cx);
    if model.read(cx).auto_started {
        return;
    }
    model.update(cx, |model, _| model.auto_started = true);
    let rules = model.read(cx).manager.auto_start_rules();
    tracing::info!(count = rules.len(), "自动启动端口转发");
    for id in rules {
        start(&id, window, cx);
    }
}

/// 检测服务器上正在监听的端口（经终端已有的连接）；没有连接时为 `None`。
pub fn detect_ports(
    terminal: &Entity<TerminalPanel>,
    cx: &mut App,
) -> Option<Task<Result<Vec<Port>, SharedString>>> {
    let connection = terminal.read(cx).connection()?;
    tracing::debug!(session = %terminal.read(cx).name(), "检测服务器上监听的端口");
    let job = termlet_core::runtime::spawn(async move {
        termlet_core::forward::listening_ports(&connection).await
    });
    Some(cx.background_spawn(async move {
        match job.await {
            Ok(Ok(ports)) => Ok(ports),
            Ok(Err(error)) => Err(ssh_errors::describe(&error)),
            Err(e) => Err(SharedString::from(e.to_string())),
        }
    }))
}
