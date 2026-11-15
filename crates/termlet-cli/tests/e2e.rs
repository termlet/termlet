//! 端到端测试：起一个测试用的命令行服务（内存中的存储，临时数据目录，通道名由它得出，不碰正在运行的 Termlet），
//! 以子进程运行 `termlet`，核对输出（被捕获时为 JSON）与退出码。界面由一个后台线程模拟。
//! 设置 `TERMLET_TEST_SSH=127.0.0.1:2222`（testing/sshd 的测试容器）时另外连真实 sshd。
//! `termlet mcp` 同样以子进程运行，经标准输入输出收发 JSON-RPC 消息。

use std::io::{BufRead as _, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use termlet_core::audit::{AuditAction, AuditLog, AuditQuery, AuditStatus};
use termlet_core::cli::{ApprovalRequest, CliConfig, CliEvent, CliService};
use termlet_core::db::Database;
use termlet_core::model::{CliAccess, Host, SecretField};
use termlet_core::paths::DataDir;
use termlet_core::secret::SecretString;
use termlet_core::store::{Owner, Position, Store};
use termlet_core::termrun::TerminalOutput;
use termlet_core::vault::{MemoryKeyStore, Vault};

/// 界面怎么回答批准。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Approve {
    Absent,
    Yes,
    No,
    /// 不回答（一直等到超时或请求被取消）
    Hold,
}

struct Env {
    _dir: tempfile::TempDir,
    data_dir: PathBuf,
    store: Arc<Store>,
    audit: Arc<AuditLog>,
    service: Option<CliService>,
    approve: Arc<Mutex<Approve>>,
    /// 按 `Approve::Hold` 留着没有回答的批准请求
    held: Arc<Mutex<Vec<ApprovalRequest>>>,
    terminal_calls: Arc<Mutex<Vec<String>>>,
}

impl Env {
    /// 不启动服务（Termlet 没有运行）。
    fn without_service() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        DataDir::at(&data_dir).prepare().unwrap();
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::open(&db, Box::new(MemoryKeyStore::default())).unwrap();
        let store = Arc::new(Store::open(db, vault).unwrap());
        let audit = Arc::new(AuditLog::new(store.clone()).unwrap());
        Self {
            _dir: dir,
            data_dir,
            store,
            audit,
            service: None,
            approve: Arc::new(Mutex::new(Approve::Absent)),
            held: Arc::default(),
            terminal_calls: Arc::default(),
        }
    }

    fn new() -> Self {
        let mut env = Self::without_service();
        let config = CliConfig::new(
            DataDir::at(&env.data_dir),
            env.store.clone(),
            env.audit.clone(),
        );
        let (service, events) = CliService::start(config).unwrap();
        let approve = env.approve.clone();
        let held = env.held.clone();
        let calls = env.terminal_calls.clone();
        // 模拟的界面：批准按设置回答；在终端中执行时输出 "ran <命令>"，退出码 3
        std::thread::spawn(move || {
            while let Ok(event) = events.recv_blocking() {
                match event {
                    CliEvent::Approval(request) => match *approve.lock().unwrap() {
                        Approve::Absent => drop(request),
                        Approve::Yes => request.answer(true),
                        Approve::No => request.answer(false),
                        Approve::Hold => held.lock().unwrap().push(request),
                    },
                    CliEvent::RunInTerminal(request) => {
                        calls.lock().unwrap().push(request.command.clone());
                        let output = format!("ran {}\n", request.command);
                        request.respond(Ok(TerminalOutput {
                            exit_code: 3,
                            output,
                            truncated: false,
                        }));
                    }
                    _ => {}
                }
            }
        });
        env.service = Some(service);
        env
    }

    fn enable(&self) {
        self.store
            .update_local_settings(|s| s.cli.enabled = true)
            .unwrap();
    }

    fn add_host(&self, name: &str, address: &str, port: u16, allowed: bool, password: &str) {
        let id = self
            .store
            .create(
                Host {
                    name: name.into(),
                    address: address.into(),
                    port,
                    username: "termlet".into(),
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
                Some(&SecretString::new(password)),
            )
            .unwrap();
    }

    /// 运行 termlet，返回（退出码，标准输出，标准错误）。
    fn run(&self, args: &[&str], stdin: Option<&str>) -> (i32, String, String) {
        self.run_in(args, stdin, "zh", None)
    }

    fn run_in(
        &self,
        args: &[&str],
        stdin: Option<&str>,
        lang: &str,
        dir: Option<&Path>,
    ) -> (i32, String, String) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_termlet"));
        command
            .args(args)
            .env("TERMLET_DATA_DIR", &self.data_dir)
            // 连不上时不拉起任何程序
            .env("TERMLET_APP", "")
            .env("TERMLET_LANG", lang)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        let mut child = command.spawn().unwrap();
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        let output = child.wait_with_output().unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn json(&self, args: &[&str]) -> (i32, Value) {
        let (code, stdout, stderr) = self.run(args, None);
        let value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("不是 JSON（{e}）：{stdout:?} {stderr:?}"));
        (code, value)
    }
}

/// 只接受连接、立即关闭的端口（连接很快失败）。
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

#[test]
fn version_help_and_usage_errors() {
    let env = Env::without_service();
    let (code, stdout, _) = env.run(&["version"], None);
    assert_eq!((code, stdout.trim()), (0, env!("CARGO_PKG_VERSION")));
    let (code, stdout, _) = env.run_in(&["help"], None, "en", None);
    assert_eq!(code, 0);
    assert!(stdout.contains("Exit codes:"), "{stdout}");
    let (code, value) = env.json(&["exec", "web-1"]);
    assert_eq!(code, 1);
    assert_eq!(value["error"], "usage");
    let (code, value) = env.json(&["upload", "web-1", "no-such-file.txt", "/tmp/"]);
    assert_eq!(code, 1, "上传前检查本机文件");
    assert_eq!(value["error"], "path_not_found");
}

// Termlet 没有运行（不拉起）：退出码 5；在运行但命令行服务没有启动时说明原因
#[test]
fn termlet_not_running() {
    let env = Env::without_service();
    let (code, value) = env.json(&["ls"]);
    assert_eq!(code, 5);
    assert_eq!(value["error"], "connect_failed");
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("没有找到 Termlet 程序"),
        "{value}"
    );

    // 设置中不允许在 Termlet 没有打开时自动启动：提示先打开 Termlet
    std::fs::write(
        env.data_dir.join("cli-launch.json"),
        r#"{"autoStart":false}"#,
    )
    .unwrap();
    let (code, value) = env.json(&["ls"]);
    assert_eq!(code, 7);
    assert_eq!(value["error"], "app_not_running");
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("请先打开 Termlet"),
        "{value}"
    );
    std::fs::remove_file(env.data_dir.join("cli-launch.json")).unwrap();

    std::fs::write(
        env.data_dir.join("run").join("cli.unavailable"),
        termlet_proto::endpoint::unavailable_marker_text(std::process::id(), "path too long"),
    )
    .unwrap();
    let (code, value) = env.json(&["ls"]);
    assert_eq!(code, 5);
    let message = value["message"].as_str().unwrap();
    assert!(
        message.contains("命令行服务没有启动") && message.contains("path too long"),
        "{message}"
    );
}

// 外部 CLI 默认关闭：退出码 7 并说明在哪里开启；开启后可用
#[test]
fn disabled_by_default() {
    let env = Env::new();
    let (code, value) = env.json(&["ls"]);
    assert_eq!(code, 7);
    assert_eq!(value["error"], "cli_disabled");
    assert!(
        value["message"].as_str().unwrap().contains("命令行工具"),
        "{value}"
    );
    let (code, stdout, stderr) = env.run_in(&["ls"], None, "en", None);
    assert_eq!(code, 7);
    assert!(stdout.contains("Settings → Command-line tool"), "{stdout}");
    assert!(stderr.is_empty(), "JSON 模式不写标准错误：{stderr}");

    env.enable();
    let (code, value) = env.json(&["ls"]);
    assert_eq!((code, value), (0, serde_json::json!([])));
}

#[test]
fn hosts_errors_and_exit_codes() {
    let env = Env::new();
    env.enable();
    let port = dead_port();
    env.add_host("web-1", "127.0.0.1", port, true, "pw");
    env.add_host("db", "127.0.0.1", port, false, "pw");

    let (code, value) = env.json(&["ls"]);
    assert_eq!(code, 0);
    let hosts = value.as_array().unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0]["name"], "web-1");
    assert_eq!(hosts[0]["port"], port);
    assert_eq!(hosts[0]["protocol"], "SSH");
    assert!(hosts[0]["id"].as_str().unwrap().starts_with("h_"));
    let (_, value) = env.json(&["ls", "-q", "nothing"]);
    assert_eq!(value, serde_json::json!([]));

    let (code, value) = env.json(&["exec", "web-1", "sudo reboot"]);
    assert_eq!(code, 4, "命中危险命令规则（没有窗口可以询问）");
    assert_eq!(value["error"], "command_blocked");
    assert!(
        value["message"].as_str().unwrap().contains("power"),
        "{value}"
    );
    assert_eq!(env.json(&["exec", "db", "uptime"]).0, 3);
    assert_eq!(env.json(&["exec", "nope", "uptime"]).0, 2);
    let (code, value) = env.json(&["exec", "web-1", "uptime"]);
    assert_eq!(code, 5);
    assert_eq!(value["error"], "connect_failed");

    env.service.as_ref().unwrap().set_app_locked(true);
    let (code, value) = env.json(&["exec", "web-1", "uptime"]);
    assert_eq!(code, 6);
    assert_eq!(value["error"], "app_locked");
    env.service.as_ref().unwrap().set_app_locked(false);

    // 审计：调用方是祖先进程（测试程序、cargo）
    let entries = env.audit.list(&AuditQuery::default()).unwrap();
    assert_eq!(entries.len(), 5);
    let blocked = entries
        .iter()
        .find(|e| e.status == AuditStatus::Blocked)
        .unwrap();
    assert_eq!(blocked.detail, "sudo reboot");
    assert!(
        blocked.caller.to_lowercase().contains("cargo"),
        "调用方：{}",
        blocked.caller
    );
}

// exec --terminal 与批准：远程命令的退出码透传；拒绝为 4；--stdin
#[test]
fn terminal_approval_and_stdin() {
    let env = Env::new();
    env.enable();
    env.add_host("web-1", "127.0.0.1", dead_port(), true, "pw");

    let (code, value) = env.json(&["exec", "--terminal", "web-1", "echo hi"]);
    assert_eq!(code, 3, "透传远程命令的退出码");
    assert_eq!(value["exitCode"], 3);
    assert_eq!(value["stdout"], "ran echo hi\n");
    assert_eq!(value["stderr"], "");

    *env.approve.lock().unwrap() = Approve::No;
    let (code, value) = env.json(&["exec", "web-1", "--terminal", "reboot"]);
    assert_eq!(code, 4);
    assert_eq!(value["error"], "command_denied");
    *env.approve.lock().unwrap() = Approve::Yes;
    let (code, value) = env.json(&["exec", "--terminal", "web-1", "reboot"]);
    assert_eq!((code, value["stdout"].as_str()), (3, Some("ran reboot\n")));

    let script = "echo \"$HOME\" | awk '{print $1}' > /dev/null\n";
    let (code, _, _) = env.run(&["exec", "--terminal", "web-1", "--stdin"], Some(script));
    assert_eq!(code, 3);
    assert_eq!(env.terminal_calls.lock().unwrap().last().unwrap(), script);

    let statuses: Vec<(AuditAction, AuditStatus)> = env
        .audit
        .list(&AuditQuery::default())
        .unwrap()
        .iter()
        .rev()
        .map(|e| (e.action, e.status))
        .collect();
    assert_eq!(
        statuses,
        [
            (AuditAction::Terminal, AuditStatus::Ok),
            (AuditAction::Terminal, AuditStatus::Denied),
            (AuditAction::Terminal, AuditStatus::Approved),
            (AuditAction::Terminal, AuditStatus::Ok),
        ]
    );
}

#[test]
fn skill_install_into_directory() {
    let env = Env::without_service();
    let dir = tempfile::tempdir().unwrap();
    let (code, value) = env.json(&[
        "skill",
        "install",
        "--dir",
        &dir.path().display().to_string(),
    ]);
    assert_eq!(code, 0);
    let path = PathBuf::from(value["path"].as_str().unwrap());
    assert_eq!(path, dir.path().join("termlet").join("SKILL.md"));
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .starts_with("---\nname: termlet")
    );
    let (code, stdout, _) = env.run(&["skill"], None);
    assert_eq!(code, 0);
    assert!(stdout.contains("termlet exec"));
}

/// `TERMLET_TEST_SSH` 指定的测试用 sshd（地址，端口）；没有设置时为 `None`。
fn test_ssh() -> Option<(String, u16)> {
    let value = std::env::var("TERMLET_TEST_SSH").ok()?;
    let (host, port) = value.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}

/// 在"界面"中连一次第一台主机、确认它的指纹（命令行不能询问）。
fn accept_host_key(env: &Env) {
    use termlet_core::ssh::{self, Answer, ConnectOptions, Prompter};
    let store = env.store.clone();
    termlet_core::runtime::runtime().block_on(async move {
        let id = store.list::<Host>()[0].id.clone();
        let plan = ssh::plan_for_host(&store, &id).unwrap();
        let prompter: Prompter = Arc::new(|_| Box::pin(async { Answer::AcceptHostKey }));
        let c = ssh::connect(store, &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        c.disconnect().await;
    });
}

// 连真实 sshd：执行、上传、下载（本机路径按当前目录解析）
#[test]
fn real_server() {
    let Some((address, port)) = test_ssh() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let env = Env::new();
    env.enable();
    env.add_host("e2e", &address, port, true, "termlet-e2e");

    let (code, value) = env.json(&["exec", "e2e", "true"]);
    assert_eq!(
        (code, value["error"].as_str()),
        (5, Some("hostkey_unknown"))
    );
    accept_host_key(&env);

    let (code, value) = env.json(&["exec", "e2e", "echo hi; exit 42"]);
    assert_eq!((code, value["stdout"].as_str()), (42, Some("hi\n")));

    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("up.txt"), "payload").unwrap();
    let remote = format!("/tmp/termlet-e2e-{}", std::process::id());
    assert_eq!(
        env.json(&["exec", "e2e", &format!("mkdir -p {remote}")]).0,
        0
    );
    let (code, stdout, stderr) = env.run_in(
        &["upload", "e2e", "up.txt", &format!("{remote}/")],
        None,
        "zh",
        Some(work.path()),
    );
    assert_eq!(code, 0, "{stdout} {stderr}");
    let up: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(up["remote"], format!("{remote}/up.txt"));
    assert_eq!(up["bytes"], 7);
    std::fs::create_dir(work.path().join("out")).unwrap();
    let (code, stdout, _) = env.run_in(
        &["download", "e2e", &format!("{remote}/up.txt"), "out"],
        None,
        "zh",
        Some(work.path()),
    );
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(
        std::fs::read_to_string(work.path().join("out").join("up.txt")).unwrap(),
        "payload"
    );
    let (code, value) = env.json(&[
        "download",
        "e2e",
        "/nope/x",
        &work.path().display().to_string(),
    ]);
    assert_eq!((code, value["error"].as_str()), (5, Some("path_not_found")));
    assert_eq!(env.json(&["exec", "e2e", &format!("rm -r {remote}")]).0, 0);
}

// ---------------------------------------------------------------- termlet mcp

/// `termlet mcp` 子进程：一行一条 JSON-RPC 消息。
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Value>,
    next_id: i64,
    /// 等别的回答时收到的消息
    stray: Vec<Value>,
}

/// 新版本（2026-07-28）请求的 `_meta`。
fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "e2e-modern", "version": "1" },
    })
}

impl Env {
    fn mcp(&self, lang: &str) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_termlet"))
            .arg("mcp")
            .env("TERMLET_DATA_DIR", &self.data_dir)
            .env("TERMLET_APP", "")
            .env("TERMLET_LANG", lang)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let message: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("标准输出中只能有 JSON-RPC 消息（{e}）：{line}"));
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        Mcp {
            stdin: child.stdin.take(),
            child,
            lines,
            next_id: 1,
            stray: Vec::new(),
        }
    }
}

impl Mcp {
    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    /// 发一个请求，返回它的 ID（不等回答）。
    fn start(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    /// 等这个 ID 的回答（最多 60 秒），其间收到的别的消息留在 `stray` 中。
    fn wait(&mut self, id: i64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|e| panic!("等不到 ID {id} 的回答：{e}"));
            if message["id"] == id {
                return message;
            }
            self.stray.push(message);
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.start(method, params);
        self.wait(id)
    }

    /// 旧版本的握手。
    fn initialize(&mut self) {
        let reply = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "e2e-test", "version": "1" },
            }),
        );
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25", "{reply}");
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    }

    /// 调用工具，返回（是否出错，文本，structuredContent）。
    fn call(&mut self, name: &str, arguments: Value) -> (bool, String, Value) {
        let reply = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let result = &reply["result"];
        assert!(result.is_object(), "{reply}");
        (
            result["isError"] == true,
            result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            result["structuredContent"].clone(),
        )
    }
}

impl Drop for Mcp {
    // 关闭标准输入：服务应当立即退出
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !std::thread::panicking() {
            panic!("关闭标准输入后 termlet mcp 没有退出");
        }
    }
}

/// 轮询直到条件成立（最多 10 秒）。
fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// 新版本（每个请求带 _meta）与旧版本（initialize 握手）都能用；列工具不需要 Termlet；关闭标准输入后退出
#[test]
fn mcp_handshake_and_tools() {
    let env = Env::without_service();
    let mut mcp = env.mcp("en");
    let discover = mcp.request("server/discover", json!({ "_meta": modern_meta() }));
    assert_eq!(
        discover["result"]["supportedVersions"],
        json!(["2026-07-28"]),
        "{discover}"
    );
    let list = mcp.request("tools/list", json!({ "_meta": modern_meta() }));
    assert_eq!(list["result"]["resultType"], "complete");
    mcp.initialize();
    let list = mcp.request("tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["list_hosts", "run_command", "upload_file", "download_file"]
    );
    let unknown = mcp.request("tools/call", json!({ "name": "nope", "arguments": {} }));
    assert_eq!(unknown["error"]["code"], -32602);
    assert!(mcp.stray.is_empty(), "{:?}", mcp.stray);
}

// Termlet 没有运行（不拉起）：工具结果说明原因；设置中不允许自动启动时提示先打开 Termlet
#[test]
fn mcp_when_termlet_is_not_running() {
    let env = Env::without_service();
    let mut mcp = env.mcp("zh");
    mcp.initialize();
    let (error, text, structured) = mcp.call("list_hosts", json!({}));
    assert!(error);
    assert_eq!(structured["error"], "connect_failed");
    assert!(text.contains("没有找到 Termlet 程序"), "{text}");
    std::fs::write(
        env.data_dir.join("cli-launch.json"),
        r#"{"autoStart":false}"#,
    )
    .unwrap();
    let (error, text, structured) =
        mcp.call("run_command", json!({ "host": "web", "command": "uptime" }));
    assert!(error);
    assert_eq!(structured["error"], "app_not_running");
    assert!(text.contains("请先打开 Termlet"), "{text}");
}

// 与命令行工具走同一套：外部 CLI 的开关、按主机授权、危险命令的拦截与批准、在终端中执行、审计
// （调用方前面是 mcp:<客户端名>）
#[test]
fn mcp_goes_through_the_cli_service() {
    let env = Env::new();
    env.add_host("web-1", "127.0.0.1", dead_port(), true, "pw");
    env.add_host("db", "127.0.0.1", dead_port(), false, "pw");
    let mut mcp = env.mcp("zh");
    mcp.initialize();

    let (error, text, structured) = mcp.call("list_hosts", json!({}));
    assert!(error, "外部 CLI 默认关闭");
    assert_eq!(structured["error"], "cli_disabled");
    assert!(text.contains("「命令行工具」"), "{text}");
    env.enable();
    let (error, _, structured) = mcp.call("list_hosts", json!({}));
    assert!(!error);
    let hosts = structured["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 1, "只列出允许命令行访问的主机");
    assert_eq!(hosts[0]["name"], "web-1");

    let (error, _, structured) = mcp.call("run_command", json!({ "host": "db", "command": "ls" }));
    assert!(error);
    assert_eq!(structured["error"], "host_not_allowed");
    // 没有窗口可以询问：拦截
    let (error, text, structured) = mcp.call(
        "run_command",
        json!({ "host": "web-1", "command": "sudo reboot" }),
    );
    assert!(error);
    assert_eq!(structured["error"], "command_blocked");
    assert!(text.contains("power"), "{text}");
    *env.approve.lock().unwrap() = Approve::No;
    let (error, text, structured) = mcp.call(
        "run_command",
        json!({ "host": "web-1", "command": "reboot", "terminal": true }),
    );
    assert!(error);
    assert_eq!(structured["error"], "command_denied");
    assert!(text.contains("不要尝试绕过"), "{text}");
    *env.approve.lock().unwrap() = Approve::Yes;
    let (error, text, _) = mcp.call(
        "run_command",
        json!({ "host": "web-1", "command": "reboot", "terminal": true }),
    );
    assert!(!error, "{text}");
    assert!(
        text.starts_with("退出码 3，") && text.contains("--- output ---\nran reboot\n"),
        "{text}"
    );
    assert_eq!(env.terminal_calls.lock().unwrap().as_slice(), ["reboot"]);
    let (error, _, structured) = mcp.call(
        "run_command",
        json!({ "host": "web-1", "command": "uptime" }),
    );
    assert!(error);
    assert_eq!(structured["error"], "connect_failed");
    // 新版本的请求同样经过 Termlet 的检查
    *env.approve.lock().unwrap() = Approve::Absent;
    let reply = mcp.request(
        "tools/call",
        json!({
            "name": "run_command",
            "arguments": { "host": "web-1", "command": "shutdown -h now" },
            "_meta": modern_meta(),
        }),
    );
    assert_eq!(reply["result"]["resultType"], "complete");
    assert_eq!(
        reply["result"]["structuredContent"]["error"],
        "command_blocked"
    );

    let entries = env.audit.list(&AuditQuery::default()).unwrap();
    let statuses: Vec<(AuditAction, AuditStatus)> =
        entries.iter().rev().map(|e| (e.action, e.status)).collect();
    assert_eq!(
        statuses,
        [
            (AuditAction::Exec, AuditStatus::Failed),
            (AuditAction::Exec, AuditStatus::Blocked),
            (AuditAction::Terminal, AuditStatus::Denied),
            (AuditAction::Terminal, AuditStatus::Approved),
            (AuditAction::Exec, AuditStatus::Failed),
            (AuditAction::Exec, AuditStatus::Blocked),
        ]
    );
    // 最新的一条是新版本的请求（客户端名在 _meta 中）
    assert!(
        entries[0].caller.starts_with("mcp:e2e-modern < "),
        "{}",
        entries[0].caller
    );
    for entry in &entries[1..] {
        assert!(
            entry.caller.starts_with("mcp:e2e-test < "),
            "调用方：{}",
            entry.caller
        );
        assert!(
            entry.caller.to_lowercase().contains("cargo"),
            "{}",
            entry.caller
        );
    }
}

// 取消：等待批准时 AI 工具取消了调用——连接关闭，Termlet 不再等批准、审计记为已取消，被取消的调用不再回答
#[test]
fn mcp_cancel_closes_the_request() {
    let env = Env::new();
    env.enable();
    env.add_host("web-1", "127.0.0.1", dead_port(), true, "pw");
    *env.approve.lock().unwrap() = Approve::Hold;
    let mut mcp = env.mcp("en");
    mcp.initialize();
    let id = mcp.start(
        "tools/call",
        json!({ "name": "run_command", "arguments": { "host": "web-1", "command": "reboot" } }),
    );
    wait_for("批准请求到达界面", || {
        env.held.lock().unwrap().len() == 1
    });
    mcp.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": id, "reason": "user pressed Esc" },
    }));
    wait_for("审计记为已取消", || {
        env.audit
            .list(&AuditQuery::default())
            .unwrap()
            .iter()
            .any(|e| e.error_code == "canceled")
    });
    let pong = mcp.request("ping", json!({}));
    assert_eq!(pong["result"], json!({}));
    assert!(
        mcp.stray.iter().all(|m| m["id"] != id),
        "被取消的调用不再回答：{:?}",
        mcp.stray
    );
}

// 连真实 sshd：执行（退出码、两路输出、长输出截断）、上传、下载
#[test]
fn mcp_real_server() {
    let Some((address, port)) = test_ssh() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let env = Env::new();
    env.enable();
    env.add_host("e2e", &address, port, true, "termlet-e2e");
    let mut mcp = env.mcp("en");
    mcp.initialize();
    let (error, _, structured) =
        mcp.call("run_command", json!({ "host": "e2e", "command": "true" }));
    assert!(error);
    assert_eq!(structured["error"], "hostkey_unknown");
    accept_host_key(&env);

    let (error, text, _) = mcp.call(
        "run_command",
        json!({ "host": "e2e", "command": "echo hi; echo oops >&2; exit 42" }),
    );
    assert!(!error, "退出码不为 0 不算工具出错：{text}");
    assert!(text.starts_with("exit code 42, took "), "{text}");
    assert!(
        text.contains("--- stdout ---\nhi\n--- stderr ---\noops\n"),
        "{text}"
    );
    // 约 590KB 的输出：只返回开头与结尾，并说明怎么看完整的输出
    let (error, text, _) = mcp.call(
        "run_command",
        json!({ "host": "e2e", "command": "seq 1 100000" }),
    );
    assert!(!error);
    assert!(text.len() < 34 * 1024, "{}", text.len());
    assert!(
        text.contains("\n1\n2\n3\n") && text.contains("\n100000\n"),
        "保留开头与结尾"
    );
    assert!(text.contains("bytes omitted") && text.contains("redirect it to a file"));

    let work = tempfile::tempdir().unwrap();
    let local = work.path().join("up.txt");
    std::fs::write(&local, "payload").unwrap();
    let remote = format!("/tmp/termlet-mcp-e2e-{}", std::process::id());
    let (error, _, _) = mcp.call(
        "run_command",
        json!({ "host": "e2e", "command": format!("mkdir -p {remote}") }),
    );
    assert!(!error);
    let (error, text, structured) = mcp.call(
        "upload_file",
        json!({
            "host": "e2e",
            "local_path": local.display().to_string(),
            "remote_path": format!("{remote}/"),
        }),
    );
    assert!(!error, "{text}");
    assert_eq!(structured["remote"], format!("{remote}/up.txt"));
    assert_eq!(structured["bytes"], 7);
    assert!(text.starts_with("Uploaded 7 B"), "{text}");
    let out = work.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let (error, text, structured) = mcp.call(
        "download_file",
        json!({
            "host": "e2e",
            "remote_path": format!("{remote}/up.txt"),
            "local_path": out.display().to_string(),
        }),
    );
    assert!(!error, "{text}");
    assert_eq!(structured["bytes"], 7);
    assert_eq!(
        std::fs::read_to_string(out.join("up.txt")).unwrap(),
        "payload"
    );
    let (error, _, structured) = mcp.call(
        "download_file",
        json!({ "host": "e2e", "remote_path": "/nope/x", "local_path": out.display().to_string() }),
    );
    assert!(error);
    assert_eq!(structured["error"], "path_not_found");
    let (error, _, _) = mcp.call(
        "run_command",
        json!({ "host": "e2e", "command": format!("rm -r {remote}") }),
    );
    assert!(!error);
}
