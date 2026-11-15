//! 崩溃记录的测试。panic hook 的测试在子进程中崩溃。

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use super::*;

const CHILD_DIR_ENV: &str = "TERMLET_TEST_CRASH_DIR";

fn env() -> ReportEnv {
    ReportEnv {
        id: "r1".into(),
        time: "2026-10-06T00:00:00Z".into(),
        version: "1.1.0".into(),
        platform: "windows".into(),
        arch: "x86_64".into(),
        os_release: "10.0.26200".into(),
    }
}

fn alice() -> Scrubber {
    Scrubber::new(r"C:\Users\alice", "alice")
}

fn rt() -> &'static tokio::runtime::Runtime {
    crate::runtime::runtime()
}

/// 只包含约定的字段并脱敏（buildReport 的用例）。
#[test]
fn build_report_has_only_agreed_fields() {
    let report = build_report(
        CrashInput {
            process: "desktop".into(),
            reason: "panic".into(),
            exit_code: Some(-1073741819),
            message: Some("at 192.168.1.1".into()),
            ..Default::default()
        },
        env(),
        &alice(),
    );
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::json!({
            "id": "r1",
            "time": "2026-10-06T00:00:00Z",
            "version": "1.1.0",
            "platform": "windows",
            "arch": "x86_64",
            "osRelease": "10.0.26200",
            "process": "desktop",
            "reason": "panic",
            "exitCode": -1073741819,
            "message": "at <ip>",
        })
    );
}

#[test]
fn build_report_scrubs_every_text_field() {
    let report = build_report(
        CrashInput {
            process: "desktop".into(),
            reason: "panic".into(),
            thread: Some("sftp-alice@10.0.0.2".into()),
            location: Some(
                r"C:\Users\alice\.cargo\registry\src\x\tokio-1.47.1\src\lib.rs:3:9".into(),
            ),
            message: Some(
                r#"called `Result::unwrap()` on an `Err` value: "password=hunter2 host 10.1.1.1""#
                    .into(),
            ),
            stack: Some(
                "   0: termlet_core::ssh::connect\n             at C:\\Users\\alice\\src\\ssh.rs:1"
                    .into(),
            ),
            exit_code: None,
            dump: None,
        },
        env(),
        &alice(),
    );
    let json = serde_json::to_string(&report).unwrap();
    for leak in ["alice", "hunter2", "10.0.0.2", "10.1.1.1"] {
        assert!(!json.contains(leak), "不应包含 {leak}：{json}");
    }
    assert_eq!(
        report.location.as_deref(),
        Some(r"~\.cargo\registry\src\x\tokio-1.47.1\src\lib.rs:3:9")
    );
    assert!(report.stack.unwrap().contains("termlet_core::ssh::connect"));
}

/// 线程名、出错位置、系统版本截到接收端的上限之内，是一行；正好在上限上的不动。
#[test]
fn build_report_fits_the_receiver_limits() {
    let report = |thread: String, location: String, os_release: String| {
        build_report(
            CrashInput {
                process: "desktop".into(),
                reason: "panic".into(),
                thread: Some(thread),
                location: Some(location),
                message: Some("m".repeat(5000)),
                stack: Some("s".repeat(50_000)),
                ..Default::default()
            },
            ReportEnv {
                os_release,
                ..env()
            },
            &alice(),
        )
    };
    let long = report(
        format!("worker\n{}", "线".repeat(400)),
        format!("src/a.rs\r\n{}:1:1", "x".repeat(3000)),
        format!("6.1.0-{}", ["1"; 100].join(".")),
    );
    let thread = long.thread.as_deref().unwrap();
    assert_eq!(thread.chars().count(), MAX_THREAD_CHARS);
    assert!(
        thread.starts_with("worker 线") && thread.ends_with('…'),
        "{thread}"
    );
    let location = long.location.as_deref().unwrap();
    assert_eq!(location.chars().count(), MAX_LOCATION_CHARS);
    assert!(location.starts_with("src/a.rs  x") && location.ends_with('…'));
    assert_eq!(long.os_release.chars().count(), MAX_OS_RELEASE_CHARS);
    assert!(long.os_release.ends_with('…'));
    for text in [thread, location, &long.os_release] {
        assert!(!text.chars().any(char::is_control), "{text:?}");
    }
    // 说明与调用栈照旧（都比接收端的 1000、16384 短）
    assert_eq!(long.message.unwrap().chars().count(), 501);
    assert_eq!(long.stack.unwrap().chars().count(), 8002);

    let exact = report(
        "t".repeat(MAX_THREAD_CHARS),
        "l".repeat(MAX_LOCATION_CHARS),
        "1".repeat(MAX_OS_RELEASE_CHARS),
    );
    assert_eq!(exact.thread.unwrap(), "t".repeat(MAX_THREAD_CHARS));
    assert_eq!(exact.location.unwrap(), "l".repeat(MAX_LOCATION_CHARS));
    assert_eq!(exact.os_release, "1".repeat(MAX_OS_RELEASE_CHARS));
}

/// 系统版本只留数字与认识的标记，别的段（可能是自己编的内核加的主机名、用户名）换成 `*`。
#[test]
fn os_release_keeps_only_versions_and_known_markers() {
    for kept in [
        "10.0.26200.6899",
        "24.1.0",
        "6.8.0-45-generic",
        "5.15.153.1-microsoft-standard-WSL2",
        "6.6.31+rpt-rpi-v8",
        "4.18.0-553.el8_10.x86_64",
        "5.4.17-2136.300.7.el8uek.x86_64",
        "6.11.5-300.fc41.x86_64",
        "6.10.10-arch1-1",
        "6.6.47-1-MANJARO",
        "6.11.10-400.asahi.fc41.aarch64+16k",
        "6.4.0-150600.23.25-default",
        "6.1.0-25-amd64",
        "6.10.3-1-pve",
        "5.10.104-linuxkit",
        "6.6.56-0-lts",
        "6.1.0-rc3+",
        "6.6.32-02877-gde0406ce7a1d",
        "",
    ] {
        assert_eq!(clean_os_release(kept), kept);
    }
    assert_eq!(clean_os_release("6.9.0-alice-laptop"), "6.9.0-*-*");
    assert_eq!(clean_os_release("6.9.0-build.corp+"), "6.9.0-*.*+");
    assert_eq!(clean_os_release(" 5.10.0 (root@box)\n"), "5.10.*");
    assert_eq!(clean_os_release("6.1.0-généric"), "6.1.0-*");
    // 处理过的再处理一遍不变
    assert_eq!(clean_os_release("6.9.0-*-*"), "6.9.0-*-*");
}

#[test]
fn retry_after_is_seconds_or_a_date_with_a_limit() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-29T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let parse = |v: &str| parse_retry_after(v, now);
    assert_eq!(parse("120"), Some(Duration::from_secs(120)));
    assert_eq!(parse(" 0 "), Some(Duration::ZERO));
    assert_eq!(parse("99999999999999999999999"), Some(MAX_RETRY_AFTER));
    assert_eq!(parse("172800"), Some(MAX_RETRY_AFTER));
    assert_eq!(
        parse("Thu, 29 Oct 2026 12:00:30 GMT"),
        Some(Duration::from_secs(30))
    );
    assert_eq!(parse("Thu, 29 Oct 2026 11:00:00 GMT"), Some(Duration::ZERO));
    for bad in ["", "soon", "-1", "1.5"] {
        assert_eq!(parse(bad), None, "{bad}");
    }
}

#[test]
fn trim_backtrace_starts_at_the_failing_code() {
    let text = [
        "   0: std::backtrace_rs::backtrace::win64::trace",
        "             at /rustc/abc/library/std/src/../../backtrace/src/backtrace/win64.rs:85",
        "   1: std::backtrace::Backtrace::force_capture",
        "   2: termlet_core::crash::record_panic",
        "             at .\\crates\\termlet-core\\src\\crash\\mod.rs:300",
        "   3: <alloc::boxed::Box<F,A> as core::ops::function::Fn<Args>>::call",
        "   4: std::panicking::rust_panic_with_hook",
        "   5: std::panicking::begin_panic_handler::closure$0",
        "   6: core::panicking::panic_fmt",
        "   7: core::result::unwrap_failed",
        "   8: termlet_desktop::sidebar::render",
        "             at .\\crates\\termlet-desktop\\src\\sidebar.rs:42",
        "   9: gpui::window::Window::draw",
    ]
    .join("\n");
    let trimmed = trim_backtrace(&text);
    assert!(
        trimmed.starts_with("   8: termlet_desktop::sidebar::render\n"),
        "{trimmed}"
    );
    assert!(trimmed.contains("sidebar.rs:42"));
    assert!(trimmed.contains("9: gpui::window::Window::draw"));
    // 没有符号（发布构建）时原样保留
    let unknown = "   0: <unknown>\n   1: <unknown>\n";
    assert_eq!(trim_backtrace(unknown), unknown);
    // 全是 panic 处理本身的帧时也原样保留
    let only = "   0: std::panicking::begin_panic\n";
    assert_eq!(trim_backtrace(only), only);
}

fn sample_report(id: &str) -> CrashReport {
    CrashReport {
        id: id.into(),
        time: "2026-10-20T00:00:00.000Z".into(),
        version: crate::VERSION.into(),
        platform: "windows".into(),
        arch: "x86_64".into(),
        os_release: "10.0.26200".into(),
        process: "desktop".into(),
        reason: "panic".into(),
        thread: Some("main".into()),
        location: Some("src/main.rs:1:1".into()),
        message: Some("boom".into()),
        ..Default::default()
    }
}

#[test]
fn records_list_view_seen_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let log = CrashLog::new(dir.path());
    let first = write_record(dir.path(), &sample_report("a"), Upload::None, 1_000).unwrap();
    let second = write_record(dir.path(), &sample_report("b"), Upload::Pending, 2_000).unwrap();
    fs::write(dir.path().join("crash-broken.json"), b"not json").unwrap();
    fs::write(dir.path().join("termlet.2026-10-20.log"), b"log").unwrap();

    let list = rt().block_on(log.list());
    let ids: Vec<&str> = list.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        [second.as_str(), first.as_str()],
        "最新的在前，坏文件跳过"
    );
    assert_eq!(list[0].upload, Upload::Pending);
    assert!(list[0].to_text().contains("说明：boom"));
    assert!(list[0].to_text().contains("位置：src/main.rs:1:1"));

    assert_eq!(rt().block_on(log.unseen()).len(), 2);
    assert_eq!(rt().block_on(log.mark_seen()).unwrap(), 2);
    assert!(rt().block_on(log.unseen()).is_empty());
    assert_eq!(rt().block_on(log.mark_seen()).unwrap(), 0);

    rt().block_on(log.delete(&first)).unwrap();
    // 已经没有了也算成功
    rt().block_on(log.delete(&first)).unwrap();
    // 标识不能指向别处
    for bad in ["../data", "crash-../../x", r"crash-a\b", "data", "crash-A"] {
        assert!(rt().block_on(log.delete(bad)).is_err(), "{bad}");
    }
    assert_eq!(rt().block_on(log.list()).len(), 1);
    assert_eq!(rt().block_on(log.clear()).unwrap(), 2, "坏文件一并删除");
    assert!(rt().block_on(log.list()).is_empty());
    assert!(
        dir.path().join("termlet.2026-10-20.log").exists(),
        "日志不动"
    );
}

#[test]
fn prune_keeps_newest() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..(MAX_RECORDS as i64 + 5) {
        write_record(
            dir.path(),
            &sample_report(&i.to_string()),
            Upload::None,
            1_000 * (i + 1),
        )
        .unwrap();
    }
    prune(dir.path(), MAX_RECORDS);
    let list = list_in(dir.path());
    assert_eq!(list.len(), MAX_RECORDS);
    assert_eq!(list[0].report.id, (MAX_RECORDS + 4).to_string());
    assert_eq!(list.last().unwrap().report.id, "5");
}

#[test]
fn report_url_rules() {
    let url = |env: Option<&str>, endpoint: Option<&str>| {
        resolve_report_url(env, endpoint).map(|u| u.to_string())
    };
    assert_eq!(
        url(None, Some("https://crash.example.com/r")),
        Some("https://crash.example.com/r".into())
    );
    assert_eq!(url(None, Some("http://crash.example.com/r")), None);
    assert_eq!(url(None, Some("https://u:p@crash.example.com/r")), None);
    assert_eq!(url(None, Some("not a url")), None);
    assert_eq!(url(None, None), None);
    // 环境变量优先：off 表示不发，本机回环地址可以是 http
    assert_eq!(url(Some("off"), Some("https://crash.example.com/r")), None);
    assert_eq!(
        url(
            Some("http://127.0.0.1:9/r"),
            Some("https://crash.example.com/r")
        ),
        Some("http://127.0.0.1:9/r".into())
    );
    assert_eq!(url(Some("http://example.com/r"), None), None);
    assert_eq!(
        url(Some(""), Some("https://a.example.com/")),
        Some("https://a.example.com/".into())
    );
}

/// [`CrashLog::flush`] 同一时间只进行一次（进程内的全局状态）：测试 flush 的用例依次运行。
static FLUSH_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 接收端的一个回答：状态码、另加的响应头（每行以 `\r\n` 结尾）、正文。
type Reply = (u16, &'static str, &'static str);

/// 接收端：收 `responses.len()` 个请求，依次回答，把请求体送出来。
fn receiver(responses: Vec<Reply>) -> (Url, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}/crash", listener.local_addr().unwrap())).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (status, headers, reply) in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            let body = loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break Vec::new();
                }
                data.extend_from_slice(&buf[..n]);
                let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if data.len() >= end + 4 + len {
                    assert!(head.starts_with("post /crash "), "{head}");
                    assert!(head.contains("content-type: application/json"), "{head}");
                    break data[end + 4..end + 4 + len].to_vec();
                }
            };
            let _ = tx.send(body);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (url, rx)
}

fn record_by_id(dir: &Path, id: &str) -> CrashRecord {
    list_in(dir)
        .into_iter()
        .find(|r| r.report.id == id)
        .unwrap()
}

#[test]
fn flush_sends_pending_reports_and_marks_them() {
    let _serial = FLUSH_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let log = CrashLog::new(dir.path());
    let native = CrashReport {
        reason: "native".into(),
        dump: Some("termlet-1.dmp".into()),
        ..sample_report("old")
    };
    write_record(dir.path(), &native, Upload::Pending, 1_000).unwrap();
    write_record(dir.path(), &sample_report("new"), Upload::Pending, 2_000).unwrap();
    write_record(dir.path(), &sample_report("local"), Upload::None, 3_000).unwrap();

    // 没有接收地址：留着
    assert_eq!(rt().block_on(log.flush(None, true)), Flushed::default());

    // 第一份成功、第二份失败：第二份留到下次
    let (url, bodies) = receiver(vec![(200, "", ""), (500, "", "")]);
    let flushed = rt().block_on(log.flush(Some(url), true));
    assert_eq!(flushed.sent, 1);
    assert_eq!(
        flushed.error,
        Some(SendError::Status {
            status: 500,
            retry_after: None
        })
    );
    let raw: serde_json::Value = serde_json::from_slice(&bodies.recv().unwrap()).unwrap();
    assert!(raw.get("dump").is_none(), "不带本机的转储文件名：{raw}");
    let sent: CrashReport = serde_json::from_value(raw).unwrap();
    assert_eq!(
        sent,
        CrashReport {
            dump: None,
            ..native
        },
        "从旧到新发送，只发报告本身"
    );
    let raw: serde_json::Value = serde_json::from_slice(&bodies.recv().unwrap()).unwrap();
    assert!(raw.get("upload").is_none() && raw.get("seen").is_none());
    let by_id = |id: &str| record_by_id(dir.path(), id);
    assert_eq!(by_id("old").upload, Upload::Sent);
    assert!(by_id("old").sent_at.is_some());
    assert_eq!(
        by_id("old").report.dump.as_deref(),
        Some("termlet-1.dmp"),
        "本机的记录照旧有转储文件名"
    );
    assert_eq!(by_id("new").upload, Upload::Pending);
    assert_eq!(by_id("local").upload, Upload::None);

    // 关闭了自动上报：待发送的不再发送，本机的记录保留
    let flushed = rt().block_on(log.flush(None, false));
    assert_eq!(flushed.dropped, 1);
    assert_eq!(by_id("new").upload, Upload::None);
    assert_eq!(list_in(dir.path()).len(), 3);
}

/// 被拒（4xx，408、429 除外）的标为被拒、接着发下一份；408、5xx 留到下次；429 的 Retry-After 之内不再发。
#[test]
fn rejected_reports_do_not_block_the_rest() {
    let _serial = FLUSH_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let log = CrashLog::new(dir.path());
    for (i, id) in ["a", "b", "c", "d", "e"].into_iter().enumerate() {
        write_record(
            dir.path(),
            &sample_report(id),
            Upload::Pending,
            1_000 * (i as i64 + 1),
        )
        .unwrap();
    }
    let by_id = |id: &str| record_by_id(dir.path(), id).upload;
    let invalid = r#"{"error":"invalid_field","field":"message","problem":"too_long"}"#;
    let (url, bodies) = receiver(vec![
        (422, "Content-Type: application/json\r\n", invalid),
        (413, "", ""),
        (200, "", ""),
        (408, "", ""),
    ]);
    let flushed = rt().block_on(log.flush(Some(url.clone()), true));
    assert_eq!((flushed.sent, flushed.rejected), (1, 2));
    assert_eq!(
        flushed.error,
        Some(SendError::Status {
            status: 408,
            retry_after: None
        })
    );
    let order: Vec<String> = (0..4)
        .map(|_| {
            serde_json::from_slice::<CrashReport>(&bodies.recv().unwrap())
                .unwrap()
                .id
        })
        .collect();
    assert_eq!(order, ["a", "b", "c", "d"]);
    assert_eq!(by_id("a"), Upload::Rejected);
    assert_eq!(by_id("b"), Upload::Rejected);
    assert_eq!(by_id("c"), Upload::Sent);
    assert_eq!(by_id("d"), Upload::Pending);
    assert!(record_by_id(dir.path(), "a").sent_at.is_none());
    // 被拒的回答记进错误说明（一行）
    match rt().block_on(send_report(
        &receiver(vec![(400, "", "{\"error\":\n\"invalid_json\"}")]).0,
        &sample_report("x"),
    )) {
        Err(SendError::Rejected { status, detail }) => {
            assert_eq!(status, 400);
            assert_eq!(detail, r#"{"error": "invalid_json"}"#);
        }
        other => panic!("{other:?}"),
    }

    // 429：留到下次，Retry-After 之内不再发（这里不会连接接收端）
    let (url, bodies) = receiver(vec![
        (429, "Retry-After: 1\r\n", ""),
        (200, "", ""),
        (200, "", ""),
    ]);
    let flushed = rt().block_on(log.flush(Some(url.clone()), true));
    assert_eq!(
        flushed.error,
        Some(SendError::Status {
            status: 429,
            retry_after: Some(Duration::from_secs(1))
        })
    );
    assert_eq!(flushed.sent, 0);
    assert_eq!(by_id("d"), Upload::Pending);
    bodies.recv().unwrap();
    assert_eq!(
        rt().block_on(log.flush(Some(url.clone()), true)),
        Flushed::default()
    );
    assert!(bodies.try_recv().is_err(), "等待期间不发");
    // 过了等待的时间照常发送；被拒的、已发送的不再发
    std::thread::sleep(Duration::from_millis(1100));
    let flushed = rt().block_on(log.flush(Some(url), true));
    assert_eq!((flushed.sent, flushed.error), (2, None));
    for id in ["d", "e"] {
        assert_eq!(
            serde_json::from_slice::<CrashReport>(&bodies.recv().unwrap())
                .unwrap()
                .id,
            id
        );
        assert_eq!(by_id(id), Upload::Sent);
    }
}

/// 子进程：装上 panic hook 后 panic（父进程 [`panic_hook_writes_scrubbed_record`] 检查写下的记录）。
/// 正常跑测试时没有环境变量，直接通过。
#[test]
fn panic_hook_child() {
    let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
        return;
    };
    install(Path::new(&dir));
    set_reporting(true);
    let home = dirs::home_dir().unwrap();
    panic!(
        "测试用的崩溃：password=hunter2 连接 10.20.30.40:22 失败，目录 {}",
        home.join("x").display()
    );
}

#[test]
fn panic_hook_writes_scrubbed_record() {
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["crash::tests::panic_hook_child", "--exact"])
        .env(CHILD_DIR_ENV, dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success(), "子进程应以失败结束");
    let records = list_in(dir.path());
    assert_eq!(
        records.len(),
        1,
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let record = &records[0];
    assert_eq!(record.upload, Upload::Pending);
    assert!(!record.seen);
    let report = &record.report;
    assert_eq!(report.reason, "panic");
    assert_eq!(report.version, crate::VERSION);
    assert_eq!(
        report.thread.as_deref(),
        Some("crash::tests::panic_hook_child")
    );
    let location = report.location.as_deref().unwrap();
    assert!(location.contains("tests.rs"), "{location}");
    let message = report.message.as_deref().unwrap();
    assert!(message.starts_with("测试用的崩溃"), "{message}");
    assert!(message.contains("password=<redacted>"), "{message}");
    assert!(
        message.contains("<host>:<port>") || message.contains("<ip>:22"),
        "{message}"
    );
    assert!(message.contains("~"), "{message}");
    let json = fs::read_to_string(dir.path().join(format!("{}.json", record.id))).unwrap();
    for leak in ["hunter2", "10.20.30.40"] {
        assert!(!json.contains(leak), "记录中不应有 {leak}：{json}");
    }
    // 用户目录的最后一段是用户名（太短的用户名不替换）
    let user = dirs::home_dir()
        .and_then(|h| h.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if user.chars().count() >= 3 {
        assert!(
            !json.to_lowercase().contains(&user.to_lowercase()),
            "{json}"
        );
    }
    // 原来的 hook 照常输出（libtest 捕获了输出时在标准输出中）
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(printed.contains("panicked at"), "{printed}");
}

/// 原生崩溃（Windows）：子进程中访问空指针，应留下内存转储和原因为 `native` 的记录。
#[cfg(windows)]
#[test]
fn native_crash_leaves_a_dump_and_a_record() {
    const CHILD: &str = "TERMLET_TEST_NATIVE_CRASH";
    if let Some(dir) = std::env::var_os(CHILD) {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
        };
        // 不弹出“程序已停止工作”
        unsafe { SetErrorMode(SEM_NOGPFAULTERRORBOX | SEM_FAILCRITICALERRORS) };
        install(std::path::Path::new(&dir));
        unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u8>(), 1) };
        unreachable!("应在上一行崩溃");
    }
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "crash::tests::native_crash_leaves_a_dump_and_a_record",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD, dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "子进程崩溃退出");
    let dumps: Vec<_> = fs::read_dir(dir.path().join(native::DUMP_DIR))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(dumps.len(), 1, "留下一份转储");
    assert!(dumps[0].metadata().unwrap().len() > 1024);
    let list = rt().block_on(CrashLog::new(dir.path()).list());
    assert_eq!(list.len(), 1);
    let report = &list[0].report;
    assert_eq!(report.reason, "native");
    assert_eq!(report.exit_code, Some(0xC000_0005));
    assert!(report.message.as_deref().unwrap().contains("访问冲突"));
    assert_eq!(
        report.dump.as_deref(),
        dumps[0].file_name().to_str(),
        "记录中写着转储的文件名"
    );
    assert!(list[0].to_text().contains("内存转储：dumps/"));
}

#[cfg(windows)]
#[test]
fn old_dumps_are_pruned() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..(native::MAX_DUMPS + 3) {
        fs::write(dir.path().join(format!("termlet-{i}.dmp")), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(15));
    }
    fs::write(dir.path().join("keep.txt"), b"x").unwrap();
    native::prune(dir.path(), native::MAX_DUMPS);
    let mut left: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    left.sort();
    assert_eq!(left.len(), native::MAX_DUMPS + 1, "{left:?}");
    assert!(left.contains(&"keep.txt".to_string()), "不是转储的不删");
    assert!(!left.contains(&"termlet-0.dmp".to_string()), "删最旧的");
}
