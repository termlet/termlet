//! 批量执行（`exec / upload / download` 加 `-g`、`-t`、`-H`、`--all`）：请求交给 Termlet 展开、执行，结果按主机流式
//! 返回（协议见 `termlet_proto::batch`）。
//!
//! 输出：
//! - 可读文本：每台结束时在标准错误报一行进度；全部结束后在标准输出按“退出码 + 输出”合并成组（`--no-group` 时逐台），
//!   列出没有成功的主机与原因，最后一行汇总（连不上与命令失败分开计数）。
//! - JSON（被管道捕获时）：结束时一个对象 `{batch, hosts, summary, approvalWaitMs}`；`--json-lines` 时每台结束输出一行，
//!   最后一行是 `{batch, summary, approvalWaitMs}`。
//! - `--output-dir`：每台写 `<主机名>.out`、`<主机名>.err`，另写 `summary.json`（与 JSON 输出相同）与 `retry.txt`
//!   （没有成功的主机的 ID，`-H @retry.txt` 只重跑它们）。
//!
//! 退出码：执行了的主机都成功为 0，否则为 8；整批没有执行（选不出主机、被拦截等）时按错误码。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use termlet_proto::batch::group_outputs;
use termlet_proto::{
    BatchAction, BatchHostResult, BatchReport, BatchRequest, BatchStatus, BatchSummary, ErrorCode,
    ExitCode, Request, Resolution, Response, Selector,
};

use crate::app;
use crate::interrupt::Watch;
use crate::messages;
use crate::output::{Output, table};
use crate::transfer::{format_bytes, format_duration};
use termlet_mcp::text as words;

/// 批量执行的选项（来自命令行参数）。
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub parallel: Option<u32>,
    /// 秒
    pub timeout: Option<u64>,
    pub max_fail: Option<u32>,
    pub no_group: bool,
    pub output_dir: Option<PathBuf>,
    pub json_lines: bool,
}

/// 组的标题中最多列出的主机数。
const SHOWN_NAMES: usize = 8;

/// 连上 Termlet，确认它支持批量执行。
fn connect(out: &mut Output<'_>) -> Result<termlet_proto::client::Client, i32> {
    let lang = out.lang;
    let client = app::connect(lang).map_err(|f| out.fail(f.exit, &f.code, &f.message))?;
    if !client.supports_batch() {
        let version = client.welcome().app_version.clone();
        return Err(out.fail(
            ExitCode::Failed,
            ErrorCode::ProtocolMismatch.as_str(),
            &messages::BATCH_OLD_APP.fill(lang, &[&version]),
        ));
    }
    Ok(client)
}

/// `--list`：列出会选中、会跳过的主机。
pub fn list(out: &mut Output<'_>, selector: Selector) -> i32 {
    let lang = out.lang;
    let mut client = match connect(out) {
        Ok(client) => client,
        Err(code) => return code,
    };
    let resolution = match client.call(&Request::ResolveHosts(selector)) {
        Ok(Response::Resolved(resolution)) => resolution,
        Ok(Response::Error(body)) => return out.api_fail(&body),
        Ok(other) => {
            let message = messages::PROTOCOL.fill(lang, &[&format!("{other:?}")]);
            return out.fail(ExitCode::Failed, "internal", &message);
        }
        Err(e) => {
            let f = app::call_failure(&e, lang);
            return out.fail(f.exit, &f.code, &f.message);
        }
    };
    if out.json {
        return out.print_json(&resolution);
    }
    print_hosts(out, &resolution);
    ExitCode::Success.into()
}

/// 要执行与跳过的主机（可读文本）。
fn print_hosts(out: &mut Output<'_>, resolution: &Resolution) {
    let lang = out.lang;
    let mut rows = vec![
        messages::HOSTS_HEADER
            .get(lang)
            .split('\t')
            .map(str::to_string)
            .collect::<Vec<_>>(),
    ];
    for h in &resolution.hosts {
        rows.push(vec![
            h.id.clone(),
            h.name.clone(),
            format!("{}:{}", h.host, h.port),
            h.username.clone(),
            h.protocol.clone(),
        ]);
    }
    let _ = out.stdout.write_all(table(&rows).as_bytes());
    if !resolution.skipped.is_empty() {
        let _ = writeln!(
            out.stdout,
            "\n{}",
            messages::BATCH_SKIPPED_TITLE.fill(lang, &[&resolution.skipped.len()])
        );
        for host in &resolution.skipped {
            let _ = writeln!(out.stdout, "{}", words::batch_problem(host, lang));
        }
    }
}

/// 执行一批。
pub fn run(out: &mut Output<'_>, selector: Selector, action: BatchAction, options: Options) -> i32 {
    let lang = out.lang;
    let started = Instant::now();
    let mut client = match connect(out) {
        Ok(client) => client,
        Err(code) => return code,
    };
    if let Some(dir) = &options.output_dir
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        let message = messages::BATCH_DIR.fill(lang, &[&dir.display(), &e]);
        return out.usage_error(&message);
    }
    let transfer = !matches!(action, BatchAction::Exec { .. });
    let upload = matches!(action, BatchAction::Upload { .. });
    let request = Request::Batch(BatchRequest {
        selector,
        action,
        parallel: options.parallel.unwrap_or(0),
        timeout_ms: options.timeout.map(|s| s.saturating_mul(1000)),
        max_fail: options.max_fail,
        stream: true,
        caller: termlet_proto::caller::chain(),
    });
    if let Err(e) = client.send(&request) {
        let f = app::call_failure(&e, lang);
        return out.fail(f.exit, &f.code, &f.message);
    }
    let watch = Watch::start(client.abort_handle().ok(), lang);
    let mut run = Run::new(&options, transfer, upload);
    let report = loop {
        match client.receive() {
            Ok(Response::BatchStart(start)) => run.start(out, start),
            Ok(Response::BatchStarted(id)) => {
                run.running.insert(id);
                watch.set_unfinished(run.unfinished());
            }
            Ok(Response::BatchHost(result)) => {
                run.finished(out, result);
                watch.set_unfinished(run.unfinished());
            }
            Ok(Response::BatchDone(report)) => break Some(report),
            Ok(Response::Error(body)) if run.start.is_none() => return out.api_fail(&body),
            Ok(Response::Error(_)) => break None,
            Ok(other) if run.start.is_none() => {
                let message = messages::PROTOCOL.fill(lang, &[&format!("{other:?}")]);
                return out.fail(ExitCode::Failed, "internal", &message);
            }
            Ok(_) => {}
            // 还没开始就断了：照常报错；开始之后（含按两次 Ctrl+C 取消）用已收到的结果收尾
            Err(e) if run.start.is_none() => {
                let f = app::call_failure(&e, lang);
                return out.fail(f.exit, &f.code, &f.message);
            }
            Err(_) => break None,
        }
    };
    drop(watch);
    run.finish(out, report, started)
}

/// 进行中的一批。
struct Run<'a> {
    options: &'a Options,
    transfer: bool,
    upload: bool,
    start: Option<Resolution>,
    results: HashMap<String, BatchHostResult>,
    running: HashSet<String>,
    /// `--output-dir` 中各台的文件名（不含扩展名）
    files: HashMap<String, String>,
}

impl<'a> Run<'a> {
    fn new(options: &'a Options, transfer: bool, upload: bool) -> Self {
        Self {
            options,
            transfer,
            upload,
            start: None,
            results: HashMap::new(),
            running: HashSet::new(),
            files: HashMap::new(),
        }
    }

    fn total(&self) -> usize {
        self.start.as_ref().map_or(0, |s| s.hosts.len())
    }

    /// 还没有结束的主机的名称。
    fn unfinished(&self) -> Vec<String> {
        let Some(start) = &self.start else {
            return Vec::new();
        };
        start
            .hosts
            .iter()
            .filter(|h| !self.results.contains_key(&h.id))
            .map(|h| h.name.clone())
            .collect()
    }

    fn start(&mut self, out: &mut Output<'_>, start: Resolution) {
        let lang = out.lang;
        self.files = file_names(start.hosts.iter().map(|h| (h.id.as_str(), h.name.as_str())));
        if self.options.json_lines {
            for host in &start.skipped {
                let _ = serde_json::to_writer(&mut *out.stdout, host);
                let _ = writeln!(out.stdout);
            }
        } else if !out.json {
            let hosts = words::hosts_count(start.hosts.len() as u64, lang);
            let line = if start.skipped.is_empty() {
                messages::BATCH_START.fill(lang, &[&hosts])
            } else {
                messages::BATCH_START_SKIPPED.fill(lang, &[&hosts, &start.skipped.len()])
            };
            let _ = writeln!(out.stderr, "[termlet] {line}");
        }
        self.start = Some(start);
    }

    fn finished(&mut self, out: &mut Output<'_>, result: BatchHostResult) {
        self.running.remove(&result.id);
        if let Some(dir) = &self.options.output_dir
            && !self.transfer
            && result.exit_code.is_some()
        {
            let name = self
                .files
                .get(&result.id)
                .cloned()
                .unwrap_or_else(|| result.id.clone());
            let _ = std::fs::write(dir.join(format!("{name}.out")), &result.stdout);
            if !result.stderr.is_empty() {
                let _ = std::fs::write(dir.join(format!("{name}.err")), &result.stderr);
            }
        }
        if self.options.json_lines {
            let _ = serde_json::to_writer(&mut *out.stdout, &result);
            let _ = writeln!(out.stdout);
        } else if !out.json {
            let done = self.results.len() + 1;
            let line = progress_line(&result, done, self.total(), out.lang);
            let _ = writeln!(out.stderr, "{line}");
        }
        self.results.insert(result.id.clone(), result);
    }

    /// 收尾：输出结果、写结果目录，返回退出码。`report` 是 Termlet 的汇总（连接中途断开时没有）。
    fn finish(self, out: &mut Output<'_>, report: Option<BatchReport>, started: Instant) -> i32 {
        let lang = out.lang;
        let Some(start) = &self.start else {
            return ExitCode::Failed.into();
        };
        let mut hosts: Vec<BatchHostResult> = start
            .hosts
            .iter()
            .map(|h| {
                self.results
                    .get(&h.id)
                    .cloned()
                    .unwrap_or_else(|| BatchHostResult {
                        id: h.id.clone(),
                        name: h.name.clone(),
                        status: if self.running.contains(&h.id) {
                            BatchStatus::Canceled
                        } else {
                            BatchStatus::Skipped
                        },
                        error: Some(termlet_proto::ErrorBody::new(
                            ErrorCode::Canceled,
                            String::new(),
                        )),
                        ..Default::default()
                    })
            })
            .collect();
        hosts.extend(start.skipped.iter().cloned());
        hosts.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        let (summary, approval_wait_ms) = match &report {
            Some(report) => (report.summary.clone(), report.approval_wait_ms),
            None => {
                let mut summary = BatchSummary::of(&hosts);
                summary.duration_ms = started.elapsed().as_millis() as u64;
                (summary, None)
            }
        };
        let batch = report
            .as_ref()
            .map(|r| r.batch.clone())
            .unwrap_or_else(|| start.batch.clone());
        let full = BatchReport {
            batch: batch.clone(),
            hosts: hosts.clone(),
            summary: summary.clone(),
            approval_wait_ms,
        };
        // 因为失败的上限或取消而没开始的算没成功；因为未允许、不是 SSH 而跳过的不算
        let unrun = hosts
            .iter()
            .filter(|h| {
                h.status == BatchStatus::Skipped
                    && h.error
                        .as_ref()
                        .is_some_and(|e| e.code == ErrorCode::Canceled.as_str())
            })
            .count() as u32;
        if let Some(dir) = &self.options.output_dir {
            write_dir(out, dir, &full);
        }
        if self.options.json_lines {
            let tail = serde_json::json!({
                "batch": batch,
                "summary": summary,
                "approvalWaitMs": approval_wait_ms,
            });
            let _ = serde_json::to_writer(&mut *out.stdout, &tail);
            let _ = writeln!(out.stdout);
        } else if out.json {
            out.print_json(&full);
        } else {
            self.print_text(out, &hosts);
            let took = words::batch_took(summary.duration_ms, approval_wait_ms, lang);
            let line =
                messages::BATCH_SUMMARY.fill(lang, &[&words::batch_summary(&summary, lang), &took]);
            let _ = writeln!(out.stdout, "\n{line}");
            if let Some(dir) = &self.options.output_dir {
                let _ = writeln!(
                    out.stderr,
                    "[termlet] {}",
                    messages::BATCH_WROTE.fill(lang, &[&dir.display()])
                );
            }
        }
        if summary.succeeded(unrun) {
            ExitCode::Success.into()
        } else {
            ExitCode::BatchFailed.into()
        }
    }

    /// 可读文本：执行命令时按输出合并成组（`--no-group` 时逐台），传输时逐台；然后是没有成功的主机。
    fn print_text(&self, out: &mut Output<'_>, hosts: &[BatchHostResult]) {
        let lang = out.lang;
        let mut problems: Vec<&BatchHostResult> = Vec::new();
        if self.transfer {
            for host in hosts {
                if host.status == BatchStatus::Ok {
                    let (done, target) = if self.upload {
                        (messages::UPLOADED, &host.remote)
                    } else {
                        (messages::DOWNLOADED, &host.local)
                    };
                    let text = done.fill(
                        lang,
                        &[
                            &format_bytes(host.bytes.unwrap_or_default()),
                            target,
                            &format_duration(host.duration_ms, lang),
                        ],
                    );
                    let _ = writeln!(out.stdout, "{}: {text}", host.name);
                    if host.protocol == termlet_proto::TRANSFER_SCP {
                        let _ = writeln!(
                            out.stderr,
                            "[termlet] {}: {}",
                            host.name,
                            messages::VIA_SCP.get(lang)
                        );
                    }
                } else {
                    problems.push(host);
                }
            }
        } else if self.options.no_group {
            for host in hosts {
                match host.exit_code {
                    Some(code) => {
                        let title = words::batch_group_title(&[&host.name], Some(code), 1, lang);
                        let _ = writeln!(out.stdout, "── {title}");
                        print_output(out, host);
                    }
                    None if host.status != BatchStatus::Ok => problems.push(host),
                    None => {}
                }
            }
        } else {
            let (groups, others) = group_outputs(hosts);
            for group in &groups {
                let first = &hosts[group.members[0]];
                let names: Vec<&str> = group
                    .members
                    .iter()
                    .map(|&i| hosts[i].name.as_str())
                    .collect();
                let title = words::batch_group_title(&names, first.exit_code, SHOWN_NAMES, lang);
                let _ = writeln!(out.stdout, "── {title}");
                print_output(out, first);
            }
            problems.extend(
                others
                    .iter()
                    .map(|&i| &hosts[i])
                    .filter(|h| h.status != BatchStatus::Ok),
            );
        }
        if !problems.is_empty() {
            let _ = writeln!(
                out.stdout,
                "\n{}",
                words::BATCH_UNSUCCESSFUL.fill(lang, &[&problems.len()])
            );
            for host in problems {
                let _ = writeln!(out.stdout, "{}", words::batch_problem(host, lang));
            }
        }
        for host in hosts {
            if host.truncated {
                let note = messages::TRUNCATED.fill(lang, &[&termlet_proto::OUTPUT_LIMIT]);
                let note = note.trim().trim_start_matches("[termlet]").trim();
                let line = messages::BATCH_HOST_NOTE.fill(lang, &[&host.name, &note]);
                let _ = writeln!(out.stderr, "[termlet] {line}");
            }
            for note in words::expiring_notes(&host.expiring_certificates, lang) {
                let line = messages::BATCH_HOST_NOTE.fill(lang, &[&host.name, &note]);
                let _ = writeln!(out.stderr, "[termlet] {line}");
            }
        }
    }
}

/// 一组的输出：标准输出，再是标准错误（两者都有时标出来）。
fn print_output(out: &mut Output<'_>, host: &BatchHostResult) {
    let both = !host.stdout.is_empty() && !host.stderr.is_empty();
    if host.stdout.is_empty() && host.stderr.is_empty() {
        let _ = writeln!(out.stdout, "{}", words::MCP_NO_OUTPUT.get(out.lang));
        return;
    }
    for (label, body) in [("stdout", &host.stdout), ("stderr", &host.stderr)] {
        if body.is_empty() {
            continue;
        }
        if both {
            let _ = writeln!(out.stdout, "--- {label} ---");
        }
        let _ = out.stdout.write_all(body.as_bytes());
        if !body.ends_with('\n') {
            let _ = writeln!(out.stdout);
        }
    }
}

/// 一台结束时的进度：`[ 3/12] ✓ web-1  退出码 0  1.2 秒`。
fn progress_line(
    result: &BatchHostResult,
    done: usize,
    total: usize,
    lang: termlet_proto::Lang,
) -> String {
    let width = total.to_string().len();
    let mark = match result.status {
        BatchStatus::Ok => "✓",
        BatchStatus::Failed => "✗",
        BatchStatus::Skipped => "-",
        _ => "!",
    };
    let detail = match (result.exit_code, &result.error) {
        (Some(code), None) => words::BATCH_EXIT.fill(lang, &[&code]),
        _ => words::batch_problem(result, lang)
            .split_once([':', '：'])
            .map(|(_, rest)| rest.trim().to_string())
            .unwrap_or_default(),
    };
    let took = format_duration(result.duration_ms, lang);
    format!(
        "[{done:>width$}/{total}] {mark} {}  {detail}  {took}",
        result.name
    )
}

/// `--output-dir` 中各台的文件名：主机名换成能做文件名的写法，重名（不区分大小写）的加上主机 ID。
fn file_names<'b>(hosts: impl Iterator<Item = (&'b str, &'b str)>) -> HashMap<String, String> {
    let list: Vec<(String, String)> = hosts
        .map(|(id, name)| (id.to_string(), safe_file_name(name)))
        .collect();
    list.iter()
        .map(|(id, name)| {
            let same = list
                .iter()
                .filter(|(_, n)| n.eq_ignore_ascii_case(name))
                .count();
            let file = if same > 1 || name.is_empty() {
                safe_file_name(&format!("{name}-{id}"))
            } else {
                name.clone()
            };
            (id.clone(), file)
        })
        .collect()
}

/// 能做文件名的写法：Windows 文件名中不允许的字符与控制字符换成 `_`，去掉末尾的点与空格。
fn safe_file_name(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = mapped.trim_end_matches(['.', ' ']).to_string();
    match trimmed.as_str() {
        "" | "." | ".." => "_".to_string(),
        _ => trimmed,
    }
}

/// 写 `summary.json` 与 `retry.txt`。
fn write_dir(out: &mut Output<'_>, dir: &Path, report: &BatchReport) {
    let lang = out.lang;
    let mut failed = |e: std::io::Error| {
        let message = messages::BATCH_DIR.fill(lang, &[&dir.display(), &e]);
        let _ = writeln!(out.stderr, "termlet: {message}");
    };
    match serde_json::to_vec_pretty(report) {
        Ok(json) => {
            if let Err(e) = std::fs::write(dir.join("summary.json"), json) {
                failed(e);
            }
        }
        Err(e) => failed(std::io::Error::other(e)),
    }
    let mut retry = String::new();
    for host in &report.hosts {
        let not_run_by_choice = host.status == BatchStatus::Skipped
            && host
                .error
                .as_ref()
                .is_some_and(|e| e.code != ErrorCode::Canceled.as_str());
        if host.status != BatchStatus::Ok && !not_run_by_choice {
            retry.push_str(&format!("{}  # {}\n", host.id, host.name));
        }
    }
    if let Err(e) = std::fs::write(dir.join("retry.txt"), retry) {
        failed(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use crate::output::tests::Captured;

    fn host(id: &str, name: &str) -> termlet_proto::HostSummary {
        termlet_proto::HostSummary {
            id: id.into(),
            name: name.into(),
            host: "10.0.0.1".into(),
            port: 22,
            username: "ops".into(),
            protocol: "SSH".into(),
        }
    }

    fn ran(id: &str, name: &str, code: i32, out: &str) -> BatchHostResult {
        BatchHostResult {
            id: id.into(),
            name: name.into(),
            status: if code == 0 {
                BatchStatus::Ok
            } else {
                BatchStatus::Failed
            },
            exit_code: Some(code),
            stdout: out.into(),
            duration_ms: 1200,
            ..Default::default()
        }
    }

    fn start() -> Resolution {
        Resolution {
            batch: "b_1".into(),
            hosts: vec![
                host("h_1", "web-1"),
                host("h_2", "web-2"),
                host("h_3", "web-3"),
                host("h_9", "web-9"),
            ],
            skipped: vec![BatchHostResult {
                id: "h_7".into(),
                name: "db-1".into(),
                status: BatchStatus::Skipped,
                error: Some(termlet_proto::ErrorBody::new(
                    ErrorCode::HostNotAllowed,
                    String::new(),
                )),
                ..Default::default()
            }],
        }
    }

    fn unreachable() -> BatchHostResult {
        BatchHostResult {
            id: "h_9".into(),
            name: "web-9".into(),
            status: BatchStatus::Unreachable,
            error: Some(termlet_proto::ErrorBody::new(
                ErrorCode::ConnectFailed,
                "Connection refused",
            )),
            ..Default::default()
        }
    }

    fn feed(run: &mut Run<'_>, out: &mut Output<'_>) {
        run.start(out, start());
        run.finished(out, ran("h_1", "web-1", 0, "active\n"));
        run.finished(out, ran("h_3", "web-3", 3, "inactive\n"));
        run.finished(out, unreachable());
        run.finished(out, ran("h_2", "web-2", 0, "active\n"));
    }

    fn report() -> BatchReport {
        let mut summary = BatchSummary::default();
        for status in [
            BatchStatus::Ok,
            BatchStatus::Ok,
            BatchStatus::Failed,
            BatchStatus::Unreachable,
            BatchStatus::Skipped,
        ] {
            summary.add(status);
        }
        summary.duration_ms = 3200;
        BatchReport {
            batch: "b_1".into(),
            hosts: Vec::new(),
            summary,
            approval_wait_ms: None,
        }
    }

    #[test]
    fn text_groups_outputs_and_lists_problems() {
        let options = Options::default();
        let mut c = Captured::new();
        let code = {
            let mut out = c.output(false, Lang::Zh);
            let mut run = Run::new(&options, false, false);
            feed(&mut run, &mut out);
            run.finish(&mut out, Some(report()), Instant::now())
        };
        assert_eq!(code, 8, "有主机没成功");
        let stdout = c.stdout();
        assert!(
            stdout.starts_with("── web-1, web-2（2 台主机，退出码 0）\nactive\n── web-3（1 台主机，退出码 3）\ninactive\n"),
            "{stdout}"
        );
        assert!(stdout.contains("没有成功（2 台）\n"), "{stdout}");
        assert!(stdout.contains("web-9：连不上（"), "{stdout}");
        assert!(stdout.contains("db-1：未执行（"), "{stdout}");
        assert!(
            stdout.contains("汇总：5 台主机：成功 2，失败 1，连不上 1，未执行 1（用时 3.2 秒）"),
            "{stdout}"
        );
        let stderr = c.stderr();
        assert!(
            stderr.contains("[termlet] 开始：4 台主机（跳过 1 台）"),
            "{stderr}"
        );
        assert!(
            stderr.contains("[1/4] ✓ web-1  退出码 0  1.2 秒"),
            "{stderr}"
        );
        assert!(stderr.contains("[3/4] ! web-9  连不上"), "{stderr}");
    }

    #[test]
    fn json_and_json_lines() {
        let options = Options::default();
        let mut c = Captured::new();
        {
            let mut out = c.output(true, Lang::En);
            let mut run = Run::new(&options, false, false);
            feed(&mut run, &mut out);
            run.finish(&mut out, Some(report()), Instant::now());
        }
        let body: serde_json::Value = serde_json::from_slice(&c.stdout).unwrap();
        let names: Vec<&str> = body["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["db-1", "web-1", "web-2", "web-3", "web-9"]);
        assert_eq!(body["hosts"][4]["status"], "unreachable");
        assert_eq!(body["summary"]["unreachable"], 1);
        assert_eq!(body["batch"], "b_1");
        assert!(c.stderr.is_empty(), "JSON 模式不写进度");

        let options = Options {
            json_lines: true,
            ..Default::default()
        };
        let mut c = Captured::new();
        {
            let mut out = c.output(true, Lang::En);
            let mut run = Run::new(&options, false, false);
            feed(&mut run, &mut out);
            run.finish(&mut out, Some(report()), Instant::now());
        }
        let lines: Vec<serde_json::Value> = c
            .stdout()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // 先是跳过的，再按完成的先后，最后一行是汇总
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0]["name"], "db-1");
        assert_eq!(lines[2]["name"], "web-3");
        assert_eq!(lines[5]["summary"]["total"], 5);
    }

    #[test]
    fn output_dir_has_files_summary_and_retry_list() {
        let dir = tempfile::tempdir().unwrap();
        let options = Options {
            output_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let mut c = Captured::new();
        {
            let mut out = c.output(false, Lang::Zh);
            let mut run = Run::new(&options, false, false);
            feed(&mut run, &mut out);
            run.finish(&mut out, Some(report()), Instant::now());
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("web-3.out")).unwrap(),
            "inactive\n"
        );
        assert!(
            !dir.path().join("web-9.out").exists(),
            "连不上的没有输出文件"
        );
        let retry = std::fs::read_to_string(dir.path().join("retry.txt")).unwrap();
        assert_eq!(retry, "h_3  # web-3\nh_9  # web-9\n", "未允许的不重跑");
        let summary: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("summary.json")).unwrap())
                .unwrap();
        assert_eq!(summary["hosts"].as_array().unwrap().len(), 5);
    }

    // 连接中途断开（按两次 Ctrl+C）：已收到的照常输出，执行中的记为已取消，还没开始的为未执行
    #[test]
    fn finishing_without_a_report() {
        let options = Options::default();
        let mut c = Captured::new();
        let code = {
            let mut out = c.output(true, Lang::Zh);
            let mut run = Run::new(&options, false, false);
            run.start(&mut out, start());
            run.running.insert("h_1".into());
            run.finished(&mut out, ran("h_2", "web-2", 0, "x"));
            run.running.insert("h_3".into());
            run.finish(&mut out, None, Instant::now())
        };
        assert_eq!(code, 8);
        let body: serde_json::Value = serde_json::from_slice(&c.stdout).unwrap();
        let status = |name: &str| {
            body["hosts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["name"] == name)
                .unwrap()["status"]
                .clone()
        };
        assert_eq!(status("web-1"), "canceled");
        assert_eq!(status("web-2"), "ok");
        assert_eq!(status("web-9"), "skipped");
    }

    #[test]
    fn file_names_are_safe_and_unique() {
        let names = file_names(
            [
                ("h_1", "web:1"),
                ("h_2", "Web"),
                ("h_3", "web"),
                ("h_4", ".."),
            ]
            .into_iter(),
        );
        assert_eq!(names["h_1"], "web_1");
        assert_eq!(names["h_2"], "Web-h_2");
        assert_eq!(names["h_3"], "web-h_3");
        assert_eq!(names["h_4"], "_");
    }
}
