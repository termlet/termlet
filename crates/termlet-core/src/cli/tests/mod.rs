//! 命令行服务的测试：真实的本机通道（每个测试一个临时数据目录，通道名各不相同）+ 内存中的存储，
//! 用 `termlet-proto` 的同步客户端发请求；界面由一个后台线程模拟（批准、在终端中执行）。

mod sshd;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use termlet_proto::client::{Client, ConnectError};
use termlet_proto::endpoint::Endpoint;
use termlet_proto::{
    ClientHello, ExecRequest, ExecResponse, HostSummary, Lang, OUTPUT_LIMIT, Request, Response,
    TransferRequest,
};

use super::http::HttpOptions;
use super::*;
use crate::audit::{AuditAction, AuditEntry, AuditLog, AuditQuery, AuditStatus, ExportFormat};
use crate::db::Database;
use crate::guard::Guard;
use crate::id::HostId;
use crate::model::{
    Approval, CliAccess, CliAuth, CliCredential, GuardSettings, Host, Protocol, SecretField,
};
use crate::paths::DataDir;
use crate::secret::SecretString;
use crate::store::{Owner, Position, Store};
use crate::termrun::{TerminalError, TerminalOutput};
use crate::testkit::wait_until;
use crate::vault::{KeyStore, MemoryKeyStore, Vault};

/// 界面对批准请求的回答方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Approve {
    /// 没有窗口：丢弃请求
    Absent,
    Yes,
    No,
    /// 不回答（留着请求，直到超时或被锁定）
    Ignore,
}

type TerminalFn =
    Arc<dyn Fn(&TerminalRequest) -> Result<TerminalOutput, TerminalError> + Send + Sync>;

/// 模拟的界面。
#[derive(Default)]
pub(super) struct Desktop {
    approve: Mutex<Option<Approve>>,
    terminal: Mutex<Option<TerminalFn>>,
    /// 收到的批准请求（规则 ID）
    asked: Mutex<Vec<String>>,
    held: Mutex<Vec<ApprovalRequest>>,
    activated: Mutex<Vec<Vec<String>>>,
    /// 收到的会话文件（路径）
    files: Mutex<Vec<String>>,
    /// 被请求退出时是否同意
    allow_quit: Mutex<bool>,
    audited: Mutex<usize>,
}

impl Desktop {
    pub(super) fn set_approve(&self, approve: Approve) {
        *self.approve.lock().unwrap() = Some(approve);
    }

    fn set_terminal(
        &self,
        f: impl Fn(&TerminalRequest) -> Result<TerminalOutput, TerminalError> + Send + Sync + 'static,
    ) {
        *self.terminal.lock().unwrap() = Some(Arc::new(f));
    }

    pub(super) fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }

    fn handle(&self, event: CliEvent) {
        match event {
            CliEvent::Approval(request) => {
                self.asked
                    .lock()
                    .unwrap()
                    .push(request.rule.id().to_string());
                match self.approve.lock().unwrap().unwrap_or(Approve::Absent) {
                    Approve::Absent => drop(request),
                    Approve::Yes => request.answer(true),
                    Approve::No => request.answer(false),
                    Approve::Ignore => self.held.lock().unwrap().push(request),
                }
            }
            CliEvent::RunInTerminal(request) => {
                let f = self.terminal.lock().unwrap().clone();
                match f {
                    Some(f) => {
                        let result = f(&request);
                        request.respond(result);
                    }
                    None => drop(request),
                }
            }
            CliEvent::Activate { args, files } => {
                self.files
                    .lock()
                    .unwrap()
                    .extend(files.iter().map(|f| f.path.clone()));
                self.activated.lock().unwrap().push(args);
            }
            CliEvent::Quit(reply) => {
                let _ = reply.send(*self.allow_quit.lock().unwrap());
            }
            CliEvent::Audited(_) => *self.audited.lock().unwrap() += 1,
        }
    }
}

pub(super) struct Fixture {
    pub store: Arc<Store>,
    pub audit: Arc<AuditLog>,
    pub service: CliService,
    pub desktop: Arc<Desktop>,
    pub endpoint: Endpoint,
    /// 只接受连接、立即关闭的端口：连接主机必然很快失败（Windows 上连本机没有监听的端口要等约 2 秒）
    pub dead_port: u16,
    _dir: tempfile::TempDir,
}

pub(super) fn store_with(key_store: Box<dyn KeyStore>) -> Arc<Store> {
    let db = Database::open_in_memory().unwrap();
    let vault = Vault::open(&db, key_store).unwrap();
    Arc::new(Store::open(db, vault).unwrap())
}

fn dead_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    port
}

impl Fixture {
    /// 外部命令行已开启。
    pub fn new() -> Self {
        Self::with(
            store_with(Box::new(MemoryKeyStore::default())),
            APPROVAL_TIMEOUT,
        )
    }

    pub fn with(store: Arc<Store>, approval_timeout: Duration) -> Self {
        Self::with_http(store, approval_timeout, HttpOptions::default())
    }

    /// 另给 MCP 的 HTTP 接入的参数（HTTP 的开关仍在本机设置中，默认关）。
    pub fn with_http(store: Arc<Store>, approval_timeout: Duration, http: HttpOptions) -> Self {
        store
            .update_local_settings(|s| s.cli.enabled = true)
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let data_dir = DataDir::at(dir.path().join("data"));
        data_dir.prepare().unwrap();
        let audit = Arc::new(AuditLog::new(store.clone()).unwrap());
        let mut config = CliConfig::new(data_dir, store.clone(), audit.clone());
        config.approval_timeout = approval_timeout;
        config.http = http;
        let (service, events) = CliService::start(config).unwrap();
        let desktop = Arc::new(Desktop::default());
        let handler = desktop.clone();
        std::thread::spawn(move || {
            while let Ok(event) = events.recv_blocking() {
                handler.handle(event);
            }
        });
        let endpoint = service.endpoint().clone();
        Self {
            store,
            audit,
            service,
            desktop,
            endpoint,
            dead_port: dead_port(),
            _dir: dir,
        }
    }

    pub fn client(&self, lang: Lang) -> Client {
        Client::connect(
            &self.endpoint,
            &ClientHello {
                protocol: termlet_proto::PROTOCOL_VERSION,
                client: "test".into(),
                lang,
            },
        )
        .unwrap()
    }

    pub fn call(&self, request: Request) -> Response {
        self.client(Lang::Zh).call(&request).unwrap()
    }

    pub fn exec(&self, host: &str, command: &str) -> Response {
        self.call(Request::Exec(ExecRequest {
            host: host.into(),
            command: command.into(),
            ..Default::default()
        }))
    }

    pub fn exec_in_terminal(&self, host: &str, command: &str) -> Response {
        self.call(Request::Exec(ExecRequest {
            host: host.into(),
            command: command.into(),
            terminal: true,
            ..Default::default()
        }))
    }

    /// 允许命令行访问的 SSH 主机，保存了密码。
    pub fn add_host(&self, name: &str, address: &str, port: u16, allowed: bool) -> HostId {
        let id = self
            .store
            .create(
                Host {
                    name: name.into(),
                    address: address.into(),
                    port,
                    username: "u".into(),
                    cli: CliAccess {
                        allowed,
                        credential: None,
                    },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        self.store
            .set_secret(
                &Owner::host(&id),
                SecretField::Password,
                Some(&SecretString::new("pw")),
            )
            .unwrap();
        id
    }

    /// 连不上的主机（端口立即关闭连接）。
    pub fn add_dead_host(&self, name: &str, allowed: bool) -> HostId {
        self.add_host(name, "127.0.0.1", self.dead_port, allowed)
    }

    /// 审计记录，最新的在前。
    pub fn entries(&self) -> Vec<AuditEntry> {
        self.audit.list(&AuditQuery::default()).unwrap()
    }
}

/// 错误码与说明；不是错误时 panic。
pub(super) fn err(response: Response) -> (String, String) {
    match response {
        Response::Error(body) => (body.code, body.message),
        other => panic!("应返回错误：{other:?}"),
    }
}

pub(super) fn code_of(response: Response) -> String {
    err(response).0
}

pub(super) fn exec_ok(response: Response) -> ExecResponse {
    match response {
        Response::Exec(r) => r,
        other => panic!("应执行成功：{other:?}"),
    }
}

fn terminal_ok(output: &str, exit_code: i32) -> Result<TerminalOutput, TerminalError> {
    Ok(TerminalOutput {
        exit_code,
        output: output.into(),
        truncated: false,
    })
}

// 外部命令行关闭时列主机、执行、传输都返回 cli_disabled，不连接服务器；执行与传输记入审计日志
#[test]
fn disabled_cli_rejects_everything() {
    let f = Fixture::new();
    f.store
        .update_local_settings(|s| s.cli.enabled = false)
        .unwrap();
    f.add_dead_host("web", true);

    assert_eq!(
        code_of(f.call(Request::ListHosts {
            query: String::new()
        })),
        "cli_disabled"
    );
    assert_eq!(code_of(f.exec("web", "uptime")), "cli_disabled");
    let local = std::env::temp_dir().display().to_string();
    assert_eq!(
        code_of(f.call(Request::Download(TransferRequest {
            host: "web".into(),
            local,
            remote: "/etc/hosts".into(),
            caller: Vec::new(),
        }))),
        "cli_disabled"
    );
    let entries = f.entries();
    assert_eq!(entries.len(), 2, "执行与传输应记入审计日志");
    assert_eq!(entries[0].error_code, "cli_disabled");
    assert_eq!(entries[0].action, AuditAction::Download);

    // 开启后恢复（随后连接失败）
    f.store
        .update_local_settings(|s| s.cli.enabled = true)
        .unwrap();
    assert_eq!(code_of(f.exec("web", "uptime")), "connect_failed");
}

// 拦截与拒绝不连接服务器，但都记录审计日志
#[test]
fn blocked_and_rejected_calls_are_audited() {
    let f = Fixture::new();
    let web = f.add_dead_host("web", true);
    f.add_dead_host("db", false);

    let (code, message) = err(f.call(Request::Exec(ExecRequest {
        host: "web".into(),
        command: "sudo rm -rf /".into(),
        terminal: false,
        caller: vec!["bash".into(), "claude".into()],
    })));
    assert_eq!(code, "command_blocked", "没有窗口可以询问：拦截");
    assert!(message.contains("rm-root"), "{message}");
    assert_eq!(code_of(f.exec("db", "uptime")), "host_not_allowed");
    assert_eq!(code_of(f.exec("nope", "uptime")), "host_not_found");
    assert_eq!(
        code_of(f.call(Request::Upload(TransferRequest {
            host: "web".into(),
            local: "relative.txt".into(),
            remote: "/tmp/".into(),
            caller: Vec::new(),
        }))),
        "bad_request",
        "相对路径应被拒绝"
    );

    let list = f.entries();
    assert_eq!(list.len(), 4);
    let blocked = &list[3];
    assert_eq!(blocked.status, AuditStatus::Blocked);
    assert_eq!(blocked.host_id, web.as_str());
    assert_eq!(blocked.host_name, "web");
    assert_eq!(blocked.detail, "sudo rm -rf /");
    assert_eq!(blocked.caller, "bash < claude");
    assert_eq!(list[2].error_code, "host_not_allowed");
    assert_eq!(list[1].host_name, "nope");
    assert_eq!(list[0].action, AuditAction::Upload);
    assert_eq!(list[0].error_code, "bad_request");

    // 关闭内置规则后不再拦截（此时会尝试连接，连接失败）；自定义规则拦截
    f.store
        .update_settings(|s| {
            s.guard.disabled_rules = vec!["rm-root".into()];
            s.guard.custom_rules = vec![r"(?i)\bdrop\s+table\b".into()];
        })
        .unwrap();
    assert_eq!(code_of(f.exec("web", "rm -rf /")), "connect_failed");
    let (code, message) = err(f.exec("web", "psql -c 'DROP TABLE x'"));
    assert_eq!(code, "command_blocked");
    assert!(message.contains("custom-1"), "{message}");

    // 界面的规则测试：用当前设置或未保存的规则
    let settings = f.store.settings().guard;
    assert_eq!(
        Guard::from_settings(&settings)
            .0
            .check("reboot")
            .map(|r| r.id().to_string()),
        Some("power".into())
    );
    let unsaved = GuardSettings {
        disabled_rules: vec!["power".into()],
        ..settings
    };
    assert!(Guard::from_settings(&unsaved).0.check("reboot").is_none());

    // 审计查询与导出
    let blocked = f
        .audit
        .list(&AuditQuery {
            status: Some(AuditStatus::Blocked),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(blocked.len(), 2);
    let mut csv = Vec::new();
    let n = f
        .audit
        .export(&mut csv, ExportFormat::Csv, &AuditQuery::default())
        .unwrap();
    assert_eq!(n, 6);
    assert!(String::from_utf8(csv).unwrap().contains("sudo rm -rf /"));
}

// 请求超过上限时明确说明请求过大，不执行、不连接；连接仍可继续使用
#[test]
fn oversized_requests_are_rejected() {
    let f = Fixture::new();
    f.add_dead_host("web", true);
    let mut client = f.client(Lang::Zh);
    let (code, message) = err(client
        .call(&Request::Exec(ExecRequest {
            host: "web".into(),
            command: "echo hello\n".repeat(300 << 10),
            ..Default::default()
        }))
        .unwrap());
    assert_eq!(code, "bad_request");
    assert!(message.contains("请求过大"), "{message}");
    let (code, message) = err(client
        .call(&Request::Exec(ExecRequest {
            host: "web".into(),
            ..Default::default()
        }))
        .unwrap());
    assert_eq!(code, "bad_request");
    assert!(message.contains("需要 host 和 command"), "{message}");
    assert!(f.entries().is_empty(), "缺少参数、过大的请求不记审计");
}

// 大脚本只保存开头 64KB 并标记截断；检查的是完整命令
#[test]
fn large_scripts_are_checked_whole_and_audited_partially() {
    let f = Fixture::new();
    f.add_dead_host("web", true);
    let script = format!("{}reboot\n", "echo hello\n".repeat(100 << 10));
    assert_eq!(
        code_of(f.exec("web", &script)),
        "command_blocked",
        "脚本末尾的 reboot 应被拦截"
    );
    let list = f.entries();
    assert_eq!(list.len(), 1);
    assert!(list[0].partial);
    assert_eq!(list[0].detail.len(), crate::audit::SUMMARY_LENGTH);
    let full = f.audit.get(list[0].id).unwrap();
    assert!(full.truncated);
    assert_eq!(full.detail_size, script.len() as i64);
    assert_eq!(full.detail.len(), crate::audit::MAX_DETAIL);
    assert!(script.starts_with(&full.detail));
}

// 交给已打开的终端执行，不新建连接；同样经过危险命令规则，审计的动作为 terminal
#[test]
fn exec_in_terminal() {
    let f = Fixture::new();
    let web = f.add_dead_host("web", true);
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = calls.clone();
    f.desktop.set_terminal(move |request| {
        log.lock()
            .unwrap()
            .push(format!("{} {}", request.host_id, request.command));
        assert_eq!(request.limit, OUTPUT_LIMIT);
        if request.command == "busy" {
            return Err(TerminalError::Busy);
        }
        terminal_ok("hi\n", 3)
    });

    let r = exec_ok(f.exec_in_terminal("web", "echo hi; exit 3"));
    assert_eq!(
        (r.exit_code, r.stdout.as_str(), r.stderr.as_str()),
        (3, "hi\n", "")
    );
    assert_eq!(code_of(f.exec_in_terminal("web", "busy")), "terminal_busy");
    assert_eq!(
        code_of(f.exec_in_terminal("web", "rm -rf /")),
        "command_blocked"
    );
    assert_eq!(
        *calls.lock().unwrap(),
        [format!("{web} echo hi; exit 3"), format!("{web} busy")],
        "被拦截的命令不应执行"
    );
    let entries = f.entries();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2].action, AuditAction::Terminal);
    assert_eq!(entries[2].status, AuditStatus::Ok);
    assert_eq!(entries[2].exit_code, Some(3));
    assert_eq!(entries[1].error_code, "terminal_busy");

    // 设置了 CLI 专用凭据的主机：拒绝 --terminal（不执行、不询问），普通 exec 照常以专用凭据连接
    let asked = f.desktop.asked().len();
    let db = f.add_dead_host("db", true);
    let mut doc = f.store.get::<Host>(&db).unwrap().doc;
    doc.cli.credential = Some(CliCredential {
        username: "readonly".into(),
        auth: CliAuth::Password,
    });
    f.store.update(&db, doc).unwrap();
    for command in ["uptime", "sudo reboot"] {
        let (code, message) = err(f.exec_in_terminal("db", command));
        assert_eq!(code, "bad_request");
        assert!(
            message.contains("CLI 专用凭据") && message.contains("--terminal"),
            "{message}"
        );
    }
    assert_eq!(calls.lock().unwrap().len(), 2, "不应在终端中执行");
    assert_eq!(f.desktop.asked().len(), asked, "不应询问");
    let e = &f.entries()[0];
    assert_eq!(
        (
            e.action,
            e.status,
            e.error_code.as_str(),
            e.host_name.as_str()
        ),
        (
            AuditAction::Terminal,
            AuditStatus::Failed,
            "bad_request",
            "db"
        )
    );
    assert_eq!(code_of(f.exec("db", "uptime")), "connect_failed");

    // 没有打开的终端（界面丢弃了请求）
    *f.desktop.terminal.lock().unwrap() = None;
    assert_eq!(
        code_of(f.exec_in_terminal("web", "ls")),
        "terminal_not_open"
    );
}

// 命中规则时请界面批准：批准后执行（状态为 approved），拒绝、超时为 command_denied，没有窗口为 command_blocked；
// 设为"拦截"时不询问
#[test]
fn approval_flow() {
    let f = Fixture::with(
        store_with(Box::new(MemoryKeyStore::default())),
        Duration::from_millis(300),
    );
    f.add_dead_host("web", true);
    f.desktop
        .set_terminal(|request| terminal_ok(&format!("ran {}\n", request.command), 0));

    f.desktop.set_approve(Approve::Yes);
    let r = exec_ok(f.exec_in_terminal("web", "sudo reboot"));
    assert_eq!(r.stdout, "ran sudo reboot\n");
    assert_eq!(f.entries()[0].status, AuditStatus::Approved);

    f.desktop.set_approve(Approve::No);
    let (code, message) = err(f.exec_in_terminal("web", "reboot"));
    assert_eq!(code, "command_denied");
    assert!(
        message.contains("用户拒绝执行") && message.contains("power"),
        "{message}"
    );
    assert_eq!(f.entries()[0].status, AuditStatus::Denied);

    f.desktop.set_approve(Approve::Ignore);
    let (code, message) = err(f.exec_in_terminal("web", "reboot"));
    assert_eq!(code, "command_denied");
    assert!(message.contains("等待批准超时"), "{message}");
    let held = f.desktop.held.lock().unwrap().pop().unwrap();
    assert!(held.is_done(), "超时后对话框应关闭");
    held.answer(true);

    f.desktop.set_approve(Approve::Absent);
    assert_eq!(
        code_of(f.exec_in_terminal("web", "reboot")),
        "command_blocked"
    );
    assert_eq!(f.entries()[0].status, AuditStatus::Blocked);

    // 英文
    f.desktop.set_approve(Approve::No);
    let mut client = f.client(Lang::En);
    let (_, message) = err(client
        .call(&Request::Exec(ExecRequest {
            host: "web".into(),
            command: "reboot".into(),
            terminal: true,
            caller: Vec::new(),
        }))
        .unwrap());
    assert!(message.starts_with("denied by the user ("), "{message}");
    assert!(message.contains("(power)"), "{message}");

    // 直接拦截：不询问
    let asked = f.desktop.asked().len();
    f.store
        .update_settings(|s| s.guard.approval = Approval::Block)
        .unwrap();
    assert_eq!(
        code_of(f.exec_in_terminal("web", "reboot")),
        "command_blocked"
    );
    assert_eq!(f.desktop.asked().len(), asked);
}

// 等用户批准的时间不算进用时：结果与审计记录中的用时只算执行，等待的时长另外给出；拒绝的也记下等了多久；
// 没有询问用户的（没有命中规则、没有窗口可以询问）没有等待的时长
#[test]
fn approval_wait_is_not_counted_as_run_time() {
    const WAIT: Duration = Duration::from_millis(400);
    let f = Fixture::new();
    f.add_dead_host("web", true);
    f.desktop.set_terminal(|_| terminal_ok("done\n", 0));
    f.desktop.set_approve(Approve::Ignore);
    let answer_later = |approved: bool| {
        let endpoint = f.endpoint.clone();
        let client = std::thread::spawn(move || {
            Client::connect(
                &endpoint,
                &ClientHello {
                    protocol: termlet_proto::PROTOCOL_VERSION,
                    client: "test".into(),
                    lang: Lang::Zh,
                },
            )
            .unwrap()
            .call(&Request::Exec(ExecRequest {
                host: "web".into(),
                command: "reboot".into(),
                terminal: true,
                caller: Vec::new(),
            }))
            .unwrap()
        });
        wait_until("界面收到批准请求", || {
            !f.desktop.held.lock().unwrap().is_empty()
        });
        std::thread::sleep(WAIT);
        let request = f.desktop.held.lock().unwrap().pop().unwrap();
        request.answer(approved);
        client.join().unwrap()
    };

    let r = exec_ok(answer_later(true));
    let waited = r.approval_wait_ms.expect("等用户批准过");
    assert!(waited >= WAIT.as_millis() as u64, "{waited}");
    assert!(r.duration_ms < WAIT.as_millis() as u64, "{}", r.duration_ms);
    let entry = &f.entries()[0];
    assert_eq!(entry.status, AuditStatus::Approved);
    assert!(
        entry
            .approval_wait_ms
            .is_some_and(|ms| ms >= WAIT.as_millis() as i64)
    );
    assert!(entry.duration_ms < WAIT.as_millis() as i64, "{entry:?}");

    assert_eq!(code_of(answer_later(false)), "command_denied");
    let entry = &f.entries()[0];
    assert_eq!(entry.status, AuditStatus::Denied);
    assert!(
        entry
            .approval_wait_ms
            .is_some_and(|ms| ms >= WAIT.as_millis() as i64)
    );
    assert!(entry.duration_ms < WAIT.as_millis() as i64, "{entry:?}");

    let r = exec_ok(f.exec_in_terminal("web", "uptime"));
    assert_eq!(r.approval_wait_ms, None);
    assert_eq!(f.entries()[0].approval_wait_ms, None);
    f.desktop.set_approve(Approve::Absent);
    assert_eq!(
        code_of(f.exec_in_terminal("web", "reboot")),
        "command_blocked"
    );
    assert_eq!(f.entries()[0].approval_wait_ms, None);
}

// 锁定：一律返回 app_locked；等待批准期间被锁定时立即返回，对话框关闭
#[test]
fn locked_app() {
    let f = Fixture::new();
    f.add_dead_host("web", true);
    f.desktop.set_terminal(|_| terminal_ok("", 0));

    f.desktop.set_approve(Approve::Ignore);
    let started = std::time::Instant::now();
    let client = std::thread::spawn({
        let endpoint = f.endpoint.clone();
        move || {
            let mut client = Client::connect(
                &endpoint,
                &ClientHello {
                    protocol: termlet_proto::PROTOCOL_VERSION,
                    client: "test".into(),
                    lang: Lang::Zh,
                },
            )
            .unwrap();
            client
                .call(&Request::Exec(ExecRequest {
                    host: "web".into(),
                    command: "reboot".into(),
                    terminal: true,
                    caller: Vec::new(),
                }))
                .unwrap()
        }
    });
    wait_until("界面收到批准请求", || !f.desktop.asked().is_empty());
    assert_eq!(f.service.in_flight(), 1, "等待批准的请求算作处理中");
    f.service.set_app_locked(true);
    assert_eq!(code_of(client.join().unwrap()), "app_locked");
    assert_eq!(f.service.in_flight(), 0);
    assert!(started.elapsed() < Duration::from_secs(10), "应立即返回");
    assert!(f.desktop.held.lock().unwrap()[0].is_done());

    assert_eq!(
        code_of(f.call(Request::ListHosts {
            query: String::new()
        })),
        "app_locked"
    );
    assert_eq!(code_of(f.exec_in_terminal("web", "ls")), "app_locked");
    assert_eq!(f.entries()[0].error_code, "app_locked");
    f.service.set_app_locked(false);
    assert!(matches!(
        f.call(Request::ListHosts {
            query: String::new()
        }),
        Response::Hosts(_)
    ));
}

// 启动时已锁定（应用锁“启动时锁定”）：开始监听起就返回 app_locked，没有放行的空档
#[test]
fn starts_locked() {
    let store = store_with(Box::new(MemoryKeyStore::default()));
    store
        .update_local_settings(|s| s.cli.enabled = true)
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let data_dir = DataDir::at(dir.path().join("data"));
    data_dir.prepare().unwrap();
    let audit = Arc::new(AuditLog::new(store.clone()).unwrap());
    let mut config = CliConfig::new(data_dir, store, audit);
    config.locked = true;
    let (service, _events) = CliService::start(config).unwrap();
    let mut client = Client::connect(
        service.endpoint(),
        &ClientHello {
            protocol: termlet_proto::PROTOCOL_VERSION,
            client: "test".into(),
            lang: Lang::Zh,
        },
    )
    .unwrap();
    let response = client
        .call(&Request::ListHosts {
            query: String::new(),
        })
        .unwrap();
    assert_eq!(code_of(response), "app_locked");
    service.set_app_locked(false);
    assert!(matches!(
        client
            .call(&Request::ListHosts {
                query: String::new()
            })
            .unwrap(),
        Response::Hosts(_)
    ));
}

// 保险库没有解锁：需要凭据的请求返回 vault_*（退出码 6）；列主机、在终端中执行不需要凭据
#[test]
fn locked_vault() {
    let f = Fixture::with(
        store_with(Box::new(MemoryKeyStore::unavailable())),
        APPROVAL_TIMEOUT,
    );
    f.store
        .create(
            Host {
                name: "web".into(),
                address: "127.0.0.1".into(),
                port: f.dead_port,
                cli: CliAccess {
                    allowed: true,
                    credential: None,
                },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    f.desktop.set_terminal(|_| terminal_ok("ok\n", 0));
    let (code, _) = err(f.exec("web", "uptime"));
    assert_eq!(code, "vault_needs_setup");
    assert_eq!(
        termlet_proto::ExitCode::for_error(&code),
        termlet_proto::ExitCode::Locked
    );
    assert!(
        matches!(f.call(Request::ListHosts { query: String::new() }), Response::Hosts(h) if h.len() == 1)
    );
    assert_eq!(exec_ok(f.exec_in_terminal("web", "uptime")).stdout, "ok\n");
}

// 列出主机：只有允许命令行访问的 SSH 主机；按名称、地址、标签、ID 筛选；用户名取命令行实际登录用的；按名称排序
#[test]
fn list_hosts() {
    let f = Fixture::new();
    let web = f.add_host("web-1", "10.0.0.5", 2222, true);
    f.add_host("Alpha", "10.0.0.6", 0, true);
    f.add_host("hidden", "10.0.0.7", 0, false);
    f.store
        .create(
            Host {
                name: "switch".into(),
                protocol: Protocol::Telnet,
                address: "10.0.0.8".into(),
                cli: CliAccess {
                    allowed: true,
                    credential: None,
                },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let mut doc = f.store.get::<Host>(&web).unwrap().doc;
    doc.tags = vec!["prod".into()];
    doc.cli.credential = Some(CliCredential {
        username: "readonly".into(),
        auth: CliAuth::Agent,
    });
    f.store.update(&web, doc).unwrap();

    let list = |query: &str| match f.call(Request::ListHosts {
        query: query.into(),
    }) {
        Response::Hosts(hosts) => hosts,
        other => panic!("{other:?}"),
    };
    let all = list("");
    let names: Vec<&str> = all.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, ["Alpha", "web-1"], "只有允许的 SSH 主机，按名称排序");
    assert_eq!(
        all[1],
        HostSummary {
            id: web.to_string(),
            name: "web-1".into(),
            host: "10.0.0.5".into(),
            port: 2222,
            username: "readonly".into(),
            protocol: "SSH".into(),
        }
    );
    assert_eq!(all[0].port, 22);
    assert_eq!(all[0].username, "u");
    assert_eq!(list("PROD").len(), 1);
    assert_eq!(list("10.0.0.6").len(), 1);
    assert_eq!(list(web.as_str()).len(), 1, "按 ID 筛选");
    assert!(list("nothing").is_empty());
}

// 按名称找主机：重名时只有一台允许就用它，有多台允许时报错并列出 ID
#[test]
fn hosts_are_found_by_id_or_unique_name() {
    let f = Fixture::new();
    f.desktop
        .set_terminal(|request| terminal_ok(request.host_id.as_str(), 0));
    let a = f.add_dead_host("same", false);
    let b = f.add_dead_host("same", true);
    assert_eq!(exec_ok(f.exec_in_terminal("same", "ls")).stdout, b.as_str());
    assert_eq!(
        code_of(f.exec_in_terminal(a.as_str(), "ls")),
        "host_not_allowed"
    );
    let c = f.add_dead_host("same", true);
    let (code, message) = err(f.exec_in_terminal("same", "ls"));
    assert_eq!(code, "bad_request");
    assert!(
        message.contains(b.as_str()) && message.contains(c.as_str()),
        "{message}"
    );
}

// 协议：版本不兼容、请求无效、一条连接上多个请求、启动参数
#[test]
fn protocol_handshake_and_requests() {
    let f = Fixture::new();
    let old = Client::connect(
        &f.endpoint,
        &ClientHello {
            protocol: 0,
            client: "old".into(),
            lang: Lang::Zh,
        },
    );
    match old {
        Err(ConnectError::Rejected(body)) => {
            assert_eq!(body.code, "protocol_mismatch");
            assert!(body.message.contains("不兼容"), "{}", body.message);
        }
        other => panic!("{other:?}"),
    }

    let mut client = f.client(Lang::Zh);
    assert_eq!(client.welcome().protocol, termlet_proto::PROTOCOL_VERSION);
    assert_eq!(client.welcome().app_version, crate::VERSION);
    client
        .write_raw(&termlet_proto::frame::encode(&serde_json::json!({"type": "teleport"})).unwrap())
        .unwrap();
    let (code, message) = err(client.read_response().unwrap());
    assert_eq!(code, "bad_request");
    assert!(message.starts_with("请求无效"), "{message}");
    // 同一条连接上继续
    for _ in 0..3 {
        assert!(matches!(
            client
                .call(&Request::ListHosts {
                    query: String::new()
                })
                .unwrap(),
            Response::Hosts(_)
        ));
    }
    assert_eq!(
        client
            .call(&Request::Activate {
                args: vec!["ssh://root@10.0.0.1".into()],
                files: vec![termlet_proto::LaunchFile {
                    path: "a.xsh".into(),
                    data: b"[CONNECTION]".to_vec(),
                }],
            })
            .unwrap(),
        Response::Ok
    );
    wait_until("界面收到启动参数", || {
        !f.desktop.activated.lock().unwrap().is_empty()
    });
    assert_eq!(
        f.desktop.activated.lock().unwrap()[0],
        ["ssh://root@10.0.0.1"]
    );
    assert_eq!(*f.desktop.files.lock().unwrap(), ["a.xsh"]);
    // 外部命令行关闭时启动参数照常（不是外部调用）
    f.store
        .update_local_settings(|s| s.cli.enabled = false)
        .unwrap();
    assert_eq!(
        f.call(Request::Activate {
            args: Vec::new(),
            files: Vec::new()
        }),
        Response::Ok
    );
    // 版本不同的新进程请它退出：界面不同意时回答错误，同意时回答 Ok
    assert_eq!(code_of(f.call(Request::Quit)), "internal");
    *f.desktop.allow_quit.lock().unwrap() = true;
    assert_eq!(f.call(Request::Quit), Response::Ok);
}

// 命令行工具在请求进行中退出：取消请求（等待中的批准结束），审计记为 canceled
#[test]
fn client_disconnect_cancels_the_request() {
    let f = Fixture::new();
    f.add_dead_host("web", true);
    f.desktop.set_approve(Approve::Ignore);
    let mut client = f.client(Lang::Zh);
    client
        .write_raw(
            &termlet_proto::frame::encode(&Request::Exec(ExecRequest {
                host: "web".into(),
                command: "reboot".into(),
                terminal: true,
                caller: Vec::new(),
            }))
            .unwrap(),
        )
        .unwrap();
    wait_until("界面收到批准请求", || !f.desktop.asked().is_empty());
    drop(client);
    wait_until("审计记下取消", || {
        f.entries()
            .first()
            .is_some_and(|e| e.error_code == "canceled")
    });
    assert!(f.desktop.held.lock().unwrap()[0].is_done());
}

// 通道：属主是当前用户，DACL 只有当前用户与 SYSTEM；同一个数据目录的第二个服务启动失败，写下原因；停止后可以重新启动
#[test]
fn listener_security_and_unavailable_marker() {
    let f = Fixture::new();
    #[cfg(windows)]
    {
        let security = super::listen::pipe_security(f.endpoint.pipe_name()).unwrap();
        let (owner, aces) = (&security.owner, &security.aces);
        assert!(security.owner_is_current_user, "属主应是当前用户：{owner}");
        assert_eq!(aces.len(), 2, "{aces:?}");
        assert!(
            aces.iter().any(|a| a.ends_with(&format!(";;;{owner}"))),
            "{aces:?}"
        );
        assert!(aces.iter().any(|a| a.ends_with(";;;SY")), "{aces:?}");
    }

    let data_dir = DataDir::at(f.endpoint.data_dir());
    let second = CliService::start(CliConfig::new(
        data_dir.clone(),
        f.store.clone(),
        f.audit.clone(),
    ));
    assert!(second.is_err(), "通道名被占用时应启动失败");
    let marker = std::fs::read_to_string(f.endpoint.unavailable_marker()).unwrap();
    let (pid, reason) = termlet_proto::endpoint::parse_unavailable_marker(&marker).unwrap();
    assert_eq!(pid, std::process::id());
    assert!(!reason.is_empty());

    // 停止服务，留着临时目录
    let Fixture {
        service,
        store,
        audit,
        endpoint,
        _dir,
        ..
    } = f;
    drop(service);
    let mut restarted = None;
    for _ in 0..100 {
        if let Ok(service) = CliService::start(CliConfig::new(
            data_dir.clone(),
            store.clone(),
            audit.clone(),
        )) {
            restarted = Some(service);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(restarted.is_some(), "停止后应能重新启动");
    assert!(
        !endpoint.unavailable_marker().exists(),
        "启动成功时删除标记"
    );
}

// 命令行工具与核心的数据目录规则相同（通道名由它得出）：都按 Termlet 程序所在的文件夹判断便携版
#[test]
fn data_dir_rule_matches_proto() {
    let core = DataDir::resolve().unwrap();
    let exe = std::env::current_exe().unwrap();
    let proto = termlet_proto::endpoint::data_dir_for(exe.parent()).unwrap();
    assert_eq!(core.root(), proto.path);
    assert_eq!(core.is_portable(), proto.portable);
}

// 命令行的连接计划：专用凭据（用户名、密码）、目标主机不用 TOTP；跳板机用自己的
#[test]
fn cli_plan_uses_dedicated_credential_without_totp() {
    use crate::model::Jump;
    use crate::ssh::plan::AuthPlan;
    let store = store_with(Box::new(MemoryKeyStore::default()));
    let secret = |id: &HostId, field, value: &str| {
        store
            .set_secret(&Owner::host(id), field, Some(&SecretString::new(value)))
            .unwrap()
    };
    let bastion = store
        .create(
            Host {
                name: "bastion".into(),
                address: "b".into(),
                username: "jump".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    secret(&bastion, SecretField::Totp, "JBSWY3DPEHPK3PXP");
    let web = store
        .create(
            Host {
                name: "web".into(),
                address: "w".into(),
                username: "admin".into(),
                jump: Jump::Via {
                    host: bastion.clone(),
                },
                cli: CliAccess {
                    allowed: true,
                    credential: None,
                },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    secret(&web, SecretField::Password, "admin-pw");
    secret(&web, SecretField::Totp, "JBSWY3DPEHPK3PXP");
    secret(&web, SecretField::CliPassword, "ro-pw");

    let password = |auth: &AuthPlan| match auth {
        AuthPlan::Password { saved } => saved.as_ref().map(|s| s.expose().to_string()),
        other => panic!("{other:?}"),
    };
    let plan = crate::ssh::plan_for_cli(&store, &web).unwrap();
    assert_eq!(plan.hops.len(), 2);
    assert!(plan.hops[0].totp.is_some(), "跳板机照常用 TOTP");
    assert!(plan.target().totp.is_none(), "目标主机不用 TOTP");
    assert_eq!(plan.target().username, "admin");
    assert_eq!(password(&plan.target().auth).as_deref(), Some("admin-pw"));

    let mut doc = store.get::<Host>(&web).unwrap().doc;
    doc.cli.credential = Some(CliCredential {
        username: "readonly".into(),
        auth: CliAuth::Password,
    });
    store.update(&web, doc.clone()).unwrap();
    let plan = crate::ssh::plan_for_cli(&store, &web).unwrap();
    assert_eq!(plan.target().username, "readonly");
    assert_eq!(password(&plan.target().auth).as_deref(), Some("ro-pw"));
    assert_eq!(plan.hops[0].username, "jump", "跳板机用自己的凭据");

    // 专用凭据的用户名留空：用主机的
    doc.cli.credential = Some(CliCredential {
        username: String::new(),
        auth: CliAuth::Agent,
    });
    store.update(&web, doc).unwrap();
    let plan = crate::ssh::plan_for_cli(&store, &web).unwrap();
    assert_eq!(plan.target().username, "admin");
    assert!(matches!(plan.target().auth, AuthPlan::Agent));
}
