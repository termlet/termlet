//! `termlet` 命令：通过 Termlet 中保存的主机执行命令、传输文件，供用户与 AI Agent 使用。
//!
//! 它是 Termlet 的薄客户端：凭据始终留在 Termlet 中，命令行只经本机通道发请求（协议见 `termlet-proto`）；
//! Termlet 没有运行时以 headless 模式拉起（设置中关闭了自动启动时提示先打开 Termlet）。
//!
//! 退出码：0 成功，1 用法错误，2 主机不存在，3 主机未允许 CLI 访问，4 命令命中危险命令规则被拦截或未获批准，
//! 5 连接或执行失败，6 凭据已锁定，7 外部 CLI 已关闭或 Termlet 没有运行（不允许自动启动时），8 批量执行中有主机没有成功；
//! `exec` 连接成功后透传远程命令的退出码。
//!
//! 批量执行（`exec / upload / download` 加 `-g`、`-t`、`-H`、`--all`）见 `batch`。
//! 人类可读文本跟随系统语言（见 `i18n`）；JSON 输出的字段名与错误码固定英文。
//!
//! `termlet mcp` 是由 AI 工具启动的 MCP 服务（stdio），工具与上面的命令一一对应，见 `mcp`。

mod app;
mod args;
mod batch;
mod debug;
mod i18n;
mod interrupt;
mod mcp;
mod messages;
mod output;
mod script;
mod skill;
mod transfer;

use std::io::{self, IsTerminal, Read, Write};

use termlet_proto::{
    BatchAction, ExecRequest, ExecResponse, ExitCode, MAX_STDIN_COMMAND, OUTPUT_LIMIT, Request,
    Response, Selector, TransferRequest, TransferResponse,
};

use crate::i18n::Lang;
use crate::output::{Output, expiring_notes, table};

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    // MCP 服务在多个线程中写标准输出，不能在这里先锁住它
    if args.first().is_some_and(|a| a == "mcp") {
        std::process::exit(mcp::main(&args[1..]));
    }
    let stdout = io::stdout();
    let stderr = io::stderr();
    let json = !stdout.is_terminal();
    let mut out_lock = stdout.lock();
    let mut err_lock = stderr.lock();
    let code = {
        let mut out = Output {
            json,
            lang: i18n::current(),
            stdout: &mut out_lock,
            stderr: &mut err_lock,
        };
        run(&mut out, &args, &mut io::stdin())
    };
    let _ = out_lock.flush();
    let _ = err_lock.flush();
    std::process::exit(code);
}

/// 执行一条命令，返回退出码。
fn run(out: &mut Output<'_>, args: &[String], stdin: &mut dyn Read) -> i32 {
    let lang = out.lang;
    let Some(command) = args.first() else {
        let _ = out.stderr.write_all(messages::USAGE.get(lang).as_bytes());
        return ExitCode::Usage.into();
    };
    let rest = &args[1..];
    match command.as_str() {
        "version" | "--version" | "-v" | "-V" => {
            let _ = writeln!(out.stdout, "{}", env!("CARGO_PKG_VERSION"));
            ExitCode::Success.into()
        }
        "help" | "--help" | "-h" => {
            let _ = out.stdout.write_all(messages::USAGE.get(lang).as_bytes());
            ExitCode::Success.into()
        }
        "ls" => match parse_ls(rest, lang) {
            Ok(query) => cmd_ls(out, query),
            Err(e) => out.usage_error(&e),
        },
        "exec" => match exec_plan(rest, stdin, lang) {
            Ok(Plan::Single(request)) => cmd_exec(out, request),
            Ok(plan) => run_batch(out, plan),
            Err(e) => out.usage_error(&e),
        },
        "upload" | "download" => {
            let upload = command == "upload";
            match transfer_plan(rest, upload, lang) {
                Ok(Plan::Transfer(request)) => {
                    if upload && let Some(code) = transfer::check_upload_source(out, &request.local)
                    {
                        return code;
                    }
                    cmd_transfer(out, request, upload)
                }
                Ok(plan) => {
                    if let Plan::Batch(_, BatchAction::Upload { local, .. }, _) = &plan
                        && let Some(code) = transfer::check_upload_source(out, local)
                    {
                        return code;
                    }
                    run_batch(out, plan)
                }
                Err(e) => out.usage_error(&e),
            }
        }
        "skill" => skill::run(out, rest),
        other => out.usage_error(&messages::UNKNOWN_COMMAND.fill(lang, &[&other])),
    }
}

fn parse_ls(args: &[String], lang: Lang) -> Result<String, String> {
    match args {
        [] => Ok(String::new()),
        [flag, query] if flag == "-q" => Ok(query.clone()),
        _ => Err(messages::USAGE_LS.get(lang).into()),
    }
}

/// 解析出的一次调用：单台的执行、传输，或批量执行、只列出主机。
#[derive(Debug)]
enum Plan {
    Single(ExecRequest),
    Transfer(TransferRequest),
    Batch(Selector, BatchAction, batch::Options),
    List(Selector),
}

/// 解析 `exec`：单台是 `exec [--terminal] <主机> <命令|--stdin>`（`--terminal` 也可以放在主机之后；命令本身是
/// `--terminal` 时原样保留），另可 `--script <文件> [-- 参数…]`、`--input <文件> <命令>`；批量时 `<主机>` 换成
/// `-g`、`-t`、`-H`、`--all` 等选项（见 `args`）。
fn exec_plan(args: &[String], stdin: &mut dyn Read, lang: Lang) -> Result<Plan, String> {
    let parsed = args::parse(args, lang)?;
    parsed.check(true, lang)?;
    let usage = || messages::USAGE_EXEC.get(lang).to_string();
    let positional = parsed.positional.clone();
    if parsed.is_batch() {
        if parsed.list {
            if !positional.is_empty() {
                return Err(usage());
            }
            return Ok(Plan::List(parsed.selector));
        }
        let command = match positional.as_slice() {
            [] => None,
            [command] => Some(command.clone()),
            _ => return Err(usage()),
        };
        let (command, input, script) = exec_input(&parsed, command, stdin, lang)?;
        let action = BatchAction::Exec {
            command,
            stdin: input,
            stdin_script: script,
        };
        return Ok(Plan::Batch(
            parsed.selector.clone(),
            action,
            batch_options(&parsed),
        ));
    }
    let (host, command) = match positional.as_slice() {
        [host] => (host.clone(), None),
        [host, command] => (host.clone(), Some(command.clone())),
        _ => return Err(usage()),
    };
    // 以前的写法：命令写成 --stdin
    let (parsed, command) = if command.as_deref() == Some("--stdin") {
        let mut parsed = parsed;
        parsed.stdin = true;
        parsed.check(true, lang)?;
        (parsed, None)
    } else {
        (parsed, command)
    };
    let (command, input, script) = exec_input(&parsed, command, stdin, lang)?;
    Ok(Plan::Single(ExecRequest {
        host,
        command,
        terminal: parsed.terminal,
        stdin: input,
        stdin_script: script,
        caller: Vec::new(),
    }))
}

/// 命令与远程的标准输入：`--stdin` 时命令从标准输入读，`--script` 时命令是 `sh -s -- 参数…`、脚本是标准输入，
/// `--input` 时文件是标准输入。返回（命令，标准输入，标准输入是否是脚本）。
fn exec_input(
    parsed: &args::Parsed,
    command: Option<String>,
    stdin: &mut dyn Read,
    lang: Lang,
) -> Result<(String, String, bool), String> {
    let usage = || messages::USAGE_EXEC.get(lang).to_string();
    let (command, input, script) = if parsed.stdin {
        if command.is_some() {
            return Err(usage());
        }
        (read_stdin(stdin, lang)?, String::new(), false)
    } else if let Some(path) = &parsed.script {
        if command.is_some() {
            return Err(usage());
        }
        let text = script::read_text(path, lang)?;
        let command = script::command_for(&text, &parsed.rest, lang)?;
        (command, text, true)
    } else if let Some(path) = &parsed.input {
        let command = command.ok_or_else(usage)?;
        (command, script::read_text(path, lang)?, false)
    } else {
        (command.ok_or_else(usage)?, String::new(), false)
    };
    if command.trim().is_empty() {
        return Err(messages::EMPTY_COMMAND.get(lang).into());
    }
    Ok((command, input, script))
}

/// 从标准输入读命令（最多 1MB，UTF-8；多读 1 字节判断是否超出：截断后的脚本可能只执行了一半，不能照常执行）。
fn read_stdin(stdin: &mut dyn Read, lang: Lang) -> Result<String, String> {
    let mut data = Vec::new();
    stdin
        .take(MAX_STDIN_COMMAND as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| messages::READ_STDIN.fill(lang, &[&e]))?;
    if data.len() > MAX_STDIN_COMMAND {
        return Err(messages::STDIN_TOO_LARGE.get(lang).into());
    }
    let text =
        String::from_utf8(data).map_err(|_| messages::STDIN_NOT_UTF8.get(lang).to_string())?;
    // PowerShell 等可能在开头加上 BOM
    Ok(text
        .strip_prefix('\u{feff}')
        .map(str::to_string)
        .unwrap_or(text))
}

/// 解析 `upload` / `download`：单台照旧（见 `transfer`）；批量时位置参数是（本地文件，远程路径）或（远程文件，本地目录）。
fn transfer_plan(args: &[String], upload: bool, lang: Lang) -> Result<Plan, String> {
    let parsed = args::parse(args, lang)?;
    parsed.check(false, lang)?;
    let usage = || {
        if upload {
            messages::USAGE_UPLOAD
        } else {
            messages::USAGE_DOWNLOAD
        }
        .get(lang)
        .to_string()
    };
    if !parsed.is_batch() {
        let request = if upload {
            transfer::parse_upload(&parsed.positional, lang)?
        } else {
            transfer::parse_download(&parsed.positional, lang)?
        };
        return Ok(Plan::Transfer(request));
    }
    if parsed.list {
        return if parsed.positional.is_empty() {
            Ok(Plan::List(parsed.selector))
        } else {
            Err(usage())
        };
    }
    let [first, second] =
        <[String; 2]>::try_from(parsed.positional.clone()).map_err(|_| usage())?;
    let absolute = |local: &str| {
        if local.is_empty() {
            return Err(messages::EMPTY_PATH.get(lang).to_string());
        }
        std::path::absolute(local)
            .map(|p| p.display().to_string())
            .map_err(|e| messages::BAD_LOCAL_PATH.fill(lang, &[&local, &e]))
    };
    if first.is_empty() || second.is_empty() {
        return Err(messages::EMPTY_PATH.get(lang).into());
    }
    let action = if upload {
        BatchAction::Upload {
            local: absolute(&first)?,
            remote: second,
        }
    } else {
        BatchAction::Download {
            remote: first,
            local: absolute(&second)?,
        }
    };
    Ok(Plan::Batch(
        parsed.selector.clone(),
        action,
        batch_options(&parsed),
    ))
}

fn batch_options(parsed: &args::Parsed) -> batch::Options {
    batch::Options {
        parallel: parsed.parallel,
        timeout: parsed.timeout,
        max_fail: parsed.max_fail,
        no_group: parsed.no_group,
        output_dir: parsed.output_dir.clone(),
        json_lines: parsed.json_lines,
    }
}

fn run_batch(out: &mut Output<'_>, plan: Plan) -> i32 {
    match plan {
        Plan::List(selector) => batch::list(out, selector),
        Plan::Batch(selector, action, options) => batch::run(out, selector, action, options),
        // 单台的在调用处处理
        Plan::Single(request) => cmd_exec(out, request),
        Plan::Transfer(_) => ExitCode::Usage.into(),
    }
}

/// 单台的 `exec`（测试用的简写）：主机、命令、`--terminal`。
#[cfg(test)]
fn parse_exec(
    args: &[String],
    stdin: &mut dyn Read,
    lang: Lang,
) -> Result<(String, String, bool), String> {
    match exec_plan(args, stdin, lang)? {
        Plan::Single(request) => Ok((request.host, request.command, request.terminal)),
        other => Err(format!("{other:?}")),
    }
}

/// 连接 Termlet 并发送一个请求；失败时输出错误，返回退出码。
fn call(out: &mut Output<'_>, request: Request) -> Result<Response, i32> {
    let lang = out.lang;
    let mut client = match app::connect(lang) {
        Ok(client) => client,
        Err(f) => {
            debug::debug!("连接 Termlet 失败：{}（{}）", f.message, f.code);
            return Err(out.fail(f.exit, &f.code, &f.message));
        }
    };
    let kind = request_kind(&request);
    let started = std::time::Instant::now();
    debug::debug!("发送请求：{kind}");
    let result = client.call(&request);
    let elapsed = started.elapsed().as_millis();
    match result {
        Ok(Response::Error(body)) => {
            debug::debug!("请求 {kind} 失败：{}（{elapsed} ms）", body.code);
            Err(out.api_fail(&body))
        }
        Ok(response) => {
            debug::debug!("请求 {kind} 完成（{elapsed} ms）");
            Ok(response)
        }
        Err(e) => {
            let f = app::call_failure(&e, lang);
            debug::debug!("请求 {kind} 中断：{}（{elapsed} ms）", f.message);
            Err(out.fail(f.exit, &f.code, &f.message))
        }
    }
}

/// 调试输出中请求的写法（不含命令内容）。
fn request_kind(request: &Request) -> String {
    match request {
        Request::ListHosts { .. } => "ls".into(),
        Request::Exec(r) if r.terminal => format!("exec --terminal（主机 {}）", r.host),
        Request::Exec(r) => format!("exec（主机 {}）", r.host),
        Request::Upload(r) => format!("upload（主机 {}）", r.host),
        Request::Download(r) => format!("download（主机 {}）", r.host),
        Request::Batch(_) => "batch".into(),
        Request::ResolveHosts(_) => "resolve".into(),
        Request::Activate { .. } => "activate".into(),
        Request::Quit => "quit".into(),
    }
}

fn unexpected(out: &mut Output<'_>, response: Response) -> i32 {
    let message = messages::PROTOCOL.fill(out.lang, &[&format!("{response:?}")]);
    out.fail(ExitCode::Failed, "internal", &message)
}

fn cmd_ls(out: &mut Output<'_>, query: String) -> i32 {
    let hosts = match call(out, Request::ListHosts { query }) {
        Ok(Response::Hosts(hosts)) => hosts,
        Ok(other) => return unexpected(out, other),
        Err(code) => return code,
    };
    if out.json {
        return out.print_json(&hosts);
    }
    let lang = out.lang;
    if hosts.is_empty() {
        let _ = writeln!(out.stderr, "{}", messages::NO_HOSTS.get(lang));
        return ExitCode::Success.into();
    }
    let mut rows = vec![
        messages::HOSTS_HEADER
            .get(lang)
            .split('\t')
            .map(str::to_string)
            .collect::<Vec<_>>(),
    ];
    for h in &hosts {
        rows.push(vec![
            h.id.clone(),
            h.name.clone(),
            format!("{}:{}", h.host, h.port),
            h.username.clone(),
            h.protocol.clone(),
        ]);
    }
    let _ = out.stdout.write_all(table(&rows).as_bytes());
    ExitCode::Success.into()
}

fn cmd_exec(out: &mut Output<'_>, mut request: ExecRequest) -> i32 {
    request.caller = termlet_proto::caller::chain();
    let request = Request::Exec(request);
    let response = match call(out, request) {
        Ok(Response::Exec(response)) => response,
        Ok(other) => return unexpected(out, other),
        Err(code) => return code,
    };
    print_exec(out, &response);
    response.exit_code
}

/// `exec` 的结果：JSON 原样输出；可读文本模式下远程命令的输出照原样写到标准输出、标准错误，截断与证书快过期的
/// 提醒写在标准错误。
fn print_exec(out: &mut Output<'_>, response: &ExecResponse) {
    if out.json {
        out.print_json(response);
    } else {
        let _ = out.stdout.write_all(response.stdout.as_bytes());
        let _ = out.stderr.write_all(response.stderr.as_bytes());
        if response.truncated {
            let note = messages::TRUNCATED.fill(out.lang, &[&OUTPUT_LIMIT]);
            let _ = out.stderr.write_all(note.as_bytes());
        }
        let notes = expiring_notes(&response.expiring_certificates, out.lang);
        // 远程命令的标准错误没有以换行结尾时，提醒另起一行
        if !notes.is_empty()
            && !response.truncated
            && !response.stderr.is_empty()
            && !response.stderr.ends_with('\n')
        {
            let _ = writeln!(out.stderr);
        }
        for note in notes {
            let _ = writeln!(out.stderr, "[termlet] {note}");
        }
    }
}

fn cmd_transfer(out: &mut Output<'_>, mut request: TransferRequest, upload: bool) -> i32 {
    request.caller = termlet_proto::caller::chain();
    let request = if upload {
        Request::Upload(request)
    } else {
        Request::Download(request)
    };
    let response = match call(out, request) {
        Ok(Response::Transfer(response)) => response,
        Ok(other) => return unexpected(out, other),
        Err(code) => return code,
    };
    print_transfer(out, &response, upload)
}

/// 上传、下载的结果：JSON 原样输出；可读文本模式下标准输出只有结果，改用 SCP 与证书快过期的说明写在标准错误。
fn print_transfer(out: &mut Output<'_>, response: &TransferResponse, upload: bool) -> i32 {
    if out.json {
        return out.print_json(response);
    }
    let lang = out.lang;
    let (done, target) = if upload {
        (messages::UPLOADED, &response.remote)
    } else {
        (messages::DOWNLOADED, &response.local)
    };
    let text = done.fill(
        lang,
        &[
            &transfer::format_bytes(response.bytes),
            target,
            &transfer::format_duration(response.duration_ms, lang),
        ],
    );
    let _ = writeln!(out.stdout, "{text}");
    // 服务器没有 SFTP、改用了 SCP：说明写在标准错误，标准输出照旧只有结果
    if response.protocol == termlet_proto::TRANSFER_SCP {
        let _ = writeln!(out.stderr, "[termlet] {}", messages::VIA_SCP.get(lang));
    }
    // 登录用的证书快过期：同样写在标准错误
    for note in expiring_notes(&response.expiring_certificates, lang) {
        let _ = writeln!(out.stderr, "[termlet] {note}");
    }
    ExitCode::Success.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_ls() {
        assert_eq!(parse_ls(&[], Lang::Zh), Ok(String::new()));
        assert_eq!(parse_ls(&args(&["-q", "web"]), Lang::Zh), Ok("web".into()));
        for bad in [&["-q"][..], &["web"], &["-x", "web"], &["-q", "a", "b"]] {
            assert!(parse_ls(&args(bad), Lang::Zh).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parses_exec() {
        let none = &mut io::empty();
        assert_eq!(
            parse_exec(&args(&["web-1", "uptime"]), none, Lang::Zh),
            Ok(("web-1".into(), "uptime".into(), false))
        );
        let script = "echo \"$HOME\"\n";
        assert_eq!(
            parse_exec(
                &args(&["web-1", "--stdin"]),
                &mut script.as_bytes(),
                Lang::Zh
            )
            .unwrap()
            .1,
            script
        );
        assert!(
            parse_exec(
                &args(&["web-1", "--stdin"]),
                &mut "  \n".as_bytes(),
                Lang::Zh
            )
            .is_err(),
            "标准输入为空白时应报错"
        );
        // 标准输入最多 1MB：正好 1MB 照常执行，超出时报错而不是截断后执行
        let full = "x".repeat(MAX_STDIN_COMMAND);
        assert_eq!(
            parse_exec(&args(&["web-1", "--stdin"]), &mut full.as_bytes(), Lang::Zh)
                .unwrap()
                .1
                .len(),
            MAX_STDIN_COMMAND
        );
        let over = format!("{full}\nreboot\n");
        let e =
            parse_exec(&args(&["web-1", "--stdin"]), &mut over.as_bytes(), Lang::Zh).unwrap_err();
        assert!(e.contains("1MB"), "{e}");
        // BOM 去掉；不是 UTF-8 时报错
        assert_eq!(
            parse_exec(
                &args(&["h", "--stdin"]),
                &mut "\u{feff}ls".as_bytes(),
                Lang::Zh
            )
            .unwrap()
            .1,
            "ls"
        );
        assert!(parse_exec(&args(&["h", "--stdin"]), &mut &b"\xff\xfe"[..], Lang::Zh).is_err());
        assert!(
            parse_exec(&args(&["web-1", " "]), none, Lang::Zh).is_err(),
            "空命令"
        );
        assert!(
            parse_exec(&args(&["web-1"]), none, Lang::Zh).is_err(),
            "缺少命令"
        );
        assert!(
            parse_exec(&args(&["web-1", "ls", "-l"]), none, Lang::Zh).is_err(),
            "多余参数应报错（命令需整体加引号）"
        );

        // --terminal 可以放在主机之前或之后；命令本身是 --terminal 时原样保留
        for list in [
            &["--terminal", "web-1", "uptime"][..],
            &["web-1", "--terminal", "uptime"],
        ] {
            assert_eq!(
                parse_exec(&args(list), none, Lang::Zh),
                Ok(("web-1".into(), "uptime".into(), true)),
                "{list:?}"
            );
        }
        assert_eq!(
            parse_exec(
                &args(&["--terminal", "web-1", "--terminal"]),
                none,
                Lang::Zh
            ),
            Ok(("web-1".into(), "--terminal".into(), true))
        );
        assert!(parse_exec(&args(&["--terminal", "web-1"]), none, Lang::Zh).is_err());
    }

    #[test]
    fn version_help_and_unknown_commands() {
        let mut c = output::tests::Captured::new();
        assert_eq!(
            run(
                &mut c.output(false, Lang::Zh),
                &args(&["version"]),
                &mut io::empty()
            ),
            0
        );
        assert_eq!(c.stdout().trim(), env!("CARGO_PKG_VERSION"));
        let mut c = output::tests::Captured::new();
        assert_eq!(
            run(
                &mut c.output(false, Lang::En),
                &args(&["help"]),
                &mut io::empty()
            ),
            0
        );
        assert!(c.stdout().starts_with("termlet — run remote commands"));
        let mut c = output::tests::Captured::new();
        assert_eq!(
            run(&mut c.output(false, Lang::Zh), &[], &mut io::empty()),
            1
        );
        assert!(c.stderr().contains("用法"));
        let mut c = output::tests::Captured::new();
        assert_eq!(
            run(
                &mut c.output(true, Lang::Zh),
                &args(&["teleport"]),
                &mut io::empty()
            ),
            1
        );
        let body: serde_json::Value = serde_json::from_slice(&c.stdout).unwrap();
        assert_eq!(body["error"], "usage");
        assert!(body["message"].as_str().unwrap().contains("teleport"));
    }

    fn expiring() -> Vec<termlet_proto::ExpiringCertificate> {
        vec![
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
        ]
    }

    // 登录用的证书快过期：可读文本模式下在标准错误另起一行提醒（跳板机的写明哪一台），标准输出照旧；
    // JSON 中多一个 expiringCertificates，没有时不输出
    #[test]
    fn exec_mentions_expiring_certificates() {
        let response = ExecResponse {
            stdout: "hi\n".into(),
            stderr: "warn".into(),
            expiring_certificates: expiring(),
            ..Default::default()
        };
        let mut c = output::tests::Captured::new();
        print_exec(&mut c.output(false, Lang::Zh), &response);
        assert_eq!(c.stdout(), "hi\n");
        assert_eq!(
            c.stderr(),
            "warn\n[termlet] 跳板机 bastion 登录用的证书将于 2026-11-19 20:30 过期（还剩约 25 分钟），过期后无法再经它连接，请及时重新签发。\n[termlet] 登录用的证书将于 2026-11-19 20:40 过期（还剩约 35 分钟），过期后无法再连接这台主机，请及时重新签发。\n"
        );
        let mut c = output::tests::Captured::new();
        print_exec(&mut c.output(true, Lang::En), &response);
        let body: serde_json::Value = serde_json::from_slice(&c.stdout).unwrap();
        assert_eq!(
            body["expiringCertificates"],
            serde_json::json!([
                {"jump": "bastion", "expiresAt": "2026-11-19T20:30:00+08:00", "minutesLeft": 25},
                {"expiresAt": "2026-11-19T20:40:00+08:00", "minutesLeft": 35},
            ])
        );
        assert!(c.stderr.is_empty());
        let mut c = output::tests::Captured::new();
        print_exec(&mut c.output(true, Lang::En), &ExecResponse::default());
        assert!(!c.stdout().contains("expiringCertificates"), "没有时不输出");
    }

    #[test]
    fn transfer_mentions_expiring_certificates() {
        let response = TransferResponse {
            local: "D:\\a.txt".into(),
            remote: "/tmp/a.txt".into(),
            bytes: 2048,
            duration_ms: 1500,
            protocol: termlet_proto::TRANSFER_SFTP.into(),
            expiring_certificates: expiring()[1..].to_vec(),
        };
        let mut c = output::tests::Captured::new();
        assert_eq!(
            print_transfer(&mut c.output(false, Lang::En), &response, true),
            0
        );
        assert_eq!(c.stdout(), "Uploaded 2.0 KB → /tmp/a.txt (1.5s)\n");
        assert_eq!(
            c.stderr(),
            "[termlet] The certificate used to log in expires at 2026-11-19 20:40 (about 35 min left). After that you won't be able to connect to this host, so get a new one issued in time.\n"
        );
    }

    // 命令行工具是薄客户端，不链接核心（SQLite、SSH、钥匙串、异步运行时）
    #[test]
    fn does_not_link_core() {
        let output = std::process::Command::new(env!("CARGO"))
            .args([
                "tree",
                "-p",
                "termlet-cli",
                "-e",
                "normal",
                "--prefix",
                "none",
                "--offline",
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            other => {
                eprintln!("cargo tree 不可用，跳过：{other:?}");
                return;
            }
        };
        let tree = String::from_utf8_lossy(&output.stdout);
        let names: Vec<&str> = tree
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        assert!(names.contains(&"termlet-proto"), "{tree}");
        for forbidden in [
            "termlet-core",
            "rusqlite",
            "russh",
            "keyring",
            "tokio",
            "ring",
            "gpui-kit",
            "regex",
        ] {
            assert!(!names.contains(&forbidden), "termlet 不应依赖 {forbidden}");
        }
    }
}
