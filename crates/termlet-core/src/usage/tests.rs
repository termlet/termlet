//! 使用统计的测试：日期与档位、正文的格式（照接收端 `termlet-api` 的 `src/stats/message.ts` 校验）、环境变量、
//! 发送与重试（本机的假接收端）、关闭与删除、远程配置的暂停。

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use regex::Regex;

use super::*;
use crate::update::signature::remote_config_comment;
use crate::update::test_support::{TestKey, TestServer, closed_port_url};

fn rt() -> &'static tokio::runtime::Runtime {
    crate::runtime::runtime()
}

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

/// 那一天 UTC 的某个时刻（Unix 毫秒）。
fn at(day: NaiveDate, h: u32, min: u32, s: u32) -> i64 {
    day.and_hms_opt(h, min, s)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

fn noon(day: NaiveDate) -> i64 {
    at(day, 12, 0, 0)
}

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

// ---------------------------------------------------------------- 日期、档位、标记

#[test]
fn age_bands_follow_the_plan() {
    let since = date(2026, 10, 1);
    let at_days = |n: i64| age(Some(since), since + chrono::Days::new(n as u64));
    assert_eq!(age(None, since), Age::New, "还没有发送成功过");
    assert_eq!(at_days(0), Age::New);
    assert_eq!(at_days(1), Age::W1);
    assert_eq!(at_days(6), Age::W1);
    assert_eq!(at_days(7), Age::M1);
    assert_eq!(at_days(29), Age::M1);
    assert_eq!(at_days(30), Age::M6);
    assert_eq!(at_days(179), Age::M6);
    assert_eq!(at_days(180), Age::Old);
    assert_eq!(at_days(1000), Age::Old);
    // 第一次的日期比今天晚（时钟往回调过）：算作当天
    assert_eq!(age(Some(since), date(2026, 9, 1)), Age::New);
    let names: Vec<String> = [Age::New, Age::W1, Age::M1, Age::M6, Age::Old]
        .iter()
        .map(|a| {
            serde_json::to_value(a)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(names, ["new", "w1", "m1", "m6", "old"]);
}

#[test]
fn today_is_the_utc_date() {
    let day = date(2026, 10, 29);
    assert_eq!(utc_day(at(day, 0, 0, 0)), day);
    assert_eq!(utc_day(at(day, 23, 59, 59) + 999), day);
    assert_eq!(utc_day(at(day, 23, 59, 59) + 1000), date(2026, 10, 30));
    // 北京时间 10-30 早上 7 点仍是 UTC 的 10-29
    assert_eq!(utc_day(at(day, 23, 0, 0)), day);
}

#[test]
fn first_of_week_and_month() {
    // 2026-10-26 是周一，11-01 是周日，11-02 是周一
    let (mon, sat, sun, next_mon) = (
        date(2026, 10, 26),
        date(2026, 10, 31),
        date(2026, 11, 1),
        date(2026, 11, 2),
    );
    assert!(
        first_of_week(None, mon) && first_of_month(None, mon),
        "没有发送过"
    );
    assert!(!first_of_week(Some(mon), mon) && !first_of_month(Some(mon), mon));
    assert!(!first_of_week(Some(mon), sun), "同一个 ISO 周");
    assert!(first_of_week(Some(sun), next_mon), "周一开始新的一周");
    assert!(!first_of_month(Some(mon), sat));
    assert!(first_of_month(Some(sat), sun), "跨月");
    assert!(!first_of_week(Some(sat), sun), "跨月但同一周");
    // 跨年：2026-12-31（周四）与 2027-01-01（周五）都在 2026 年的第 53 周
    assert!(!first_of_week(Some(date(2026, 12, 31)), date(2027, 1, 3)));
    assert!(first_of_month(Some(date(2026, 12, 31)), date(2027, 1, 1)));
    assert!(first_of_week(Some(date(2027, 1, 3)), date(2027, 1, 4)));
    // 去年同月同周不算同一周、同一月
    assert!(first_of_week(Some(date(2025, 10, 27)), date(2026, 10, 26)));
    assert!(first_of_month(Some(date(2025, 10, 15)), date(2026, 10, 15)));
    // 时钟往回调过（上次发送比今天晚）：两个标记都为真
    assert!(first_of_week(Some(next_mon), sun) && first_of_month(Some(next_mon), sun));
}

// ---------------------------------------------------------------- 正文的格式

/// 接收端 `src/stats/message.ts` 的校验，照搬过来。
struct ReceiverRules {
    app: Regex,
    windows: Regex,
    macos: Regex,
    linux: Regex,
}

impl ReceiverRules {
    fn new() -> Self {
        Self {
            app: Regex::new(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$").unwrap(),
            windows: Regex::new(r"^(10|11)( \d{2}H\d)?$").unwrap(),
            macos: Regex::new(r"^\d{2}$").unwrap(),
            linux: Regex::new(
                r"^(other|(ubuntu|debian|fedora|arch|opensuse|linuxmint)( \d{1,8}(\.\d{1,4}){0,2})?)$",
            )
            .unwrap(),
        }
    }

    fn os_version_ok(&self, os: &str, version: &str) -> bool {
        match os {
            "windows" => self.windows.is_match(version),
            "macos" => self.macos.is_match(version),
            "linux" => self.linux.is_match(version),
            _ => false,
        }
    }

    /// `checkMessage`：9 个字段都在、类型对、取值合格。
    fn check(&self, json: &str) -> Result<(), String> {
        let value: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let s = |k: &str| value[k].as_str().ok_or(format!("{k} 不是字符串"));
        let b = |k: &str| value[k].as_bool().ok_or(format!("{k} 不是布尔值"));
        if value["v"] != 1 {
            return Err("v".into());
        }
        let app = s("app")?;
        if !self.app.is_match(app) || app.len() > 32 {
            return Err(format!("app：{app}"));
        }
        let os = s("os")?;
        if !["windows", "macos", "linux"].contains(&os) {
            return Err(format!("os：{os}"));
        }
        if !self.os_version_ok(os, s("osVersion")?) {
            return Err(format!("osVersion：{}", s("osVersion")?));
        }
        if !["x86_64", "aarch64"].contains(&s("arch")?) {
            return Err("arch".into());
        }
        // 与接收端的 LANG 逐项相同（不引用 LANGS：这里照搬的是接收端，两边各写一份才核对得出来）
        if !["zh-CN", "zh-TW", "en", "ja", "ko"].contains(&s("lang")?) {
            return Err("lang".into());
        }
        if !["new", "w1", "m1", "m6", "old"].contains(&s("age")?) {
            return Err("age".into());
        }
        b("firstOfWeek")?;
        b("firstOfMonth")?;
        Ok(())
    }
}

#[test]
fn body_has_exactly_the_agreed_fields_in_order() {
    // 方案 3.7 的例子
    let message = UsageMessage {
        v: 1,
        app: "0.1.0".into(),
        os: "windows".into(),
        os_version: "11 25H2".into(),
        arch: "x86_64".into(),
        lang: "zh-CN".into(),
        age: Age::M1,
        first_of_week: true,
        first_of_month: false,
    };
    let json = message.to_json();
    assert_eq!(
        json,
        r#"{"v":1,"app":"0.1.0","os":"windows","osVersion":"11 25H2","arch":"x86_64","lang":"zh-CN","age":"m1","firstOfWeek":true,"firstOfMonth":false}"#
    );
    assert_eq!(json.len(), 140, "与方案中的 Content-Length 相同");
    ReceiverRules::new().check(&json).unwrap();
}

#[test]
fn built_messages_pass_the_receiver_checks() {
    let rules = ReceiverRules::new();
    let today = date(2026, 10, 29);
    let state = UsageState {
        since: date(2026, 10, 1),
        last_sent: date(2026, 10, 28),
    };
    let platforms = [
        ("windows", "11 25H2", "x86_64"),
        ("windows", "10 22H2", "aarch64"),
        ("windows", "10", "x86_64"),
        ("macos", "15", "aarch64"),
        ("macos", "26", "aarch64"),
        ("linux", "ubuntu 24.04", "x86_64"),
        ("linux", "arch", "x86_64"),
        ("linux", "opensuse 20241015", "x86_64"),
        ("linux", "other", "aarch64"),
    ];
    for (os, os_version, arch) in platforms {
        let platform = Platform {
            os: os.into(),
            os_version: os_version.into(),
            arch: arch.into(),
        };
        for lang in ["zh-CN", "zh-TW", "en", "ja", "ko", "fr"] {
            for state in [None, Some(&state)] {
                let message = build_message(&platform, lang, state, today);
                rules.check(&message.to_json()).unwrap();
                assert_eq!(message.app, crate::VERSION);
            }
        }
    }
    let platform = Platform {
        os: "windows".into(),
        os_version: "11 25H2".into(),
        arch: "x86_64".into(),
    };
    let message = build_message(&platform, "zh-TW", Some(&state), today);
    assert_eq!(
        (
            message.lang.as_str(),
            message.age,
            message.first_of_week,
            message.first_of_month
        ),
        ("zh-TW", Age::M1, false, false)
    );
    // 日文、韩文界面如实发送；接收端不认的语言按 en
    for (lang, sent) in [
        ("ja", "ja"),
        ("ko", "ko"),
        ("fr", "en"),
        ("zh-HK", "en"),
        ("", "en"),
    ] {
        assert_eq!(
            build_message(&platform, lang, None, today).lang,
            sent,
            "{lang}"
        );
    }
    // 客户端会发的语言接收端都认
    for lang in LANGS {
        rules
            .check(&build_message(&platform, lang, None, today).to_json())
            .unwrap();
    }
    let fresh = build_message(&platform, "en", None, today);
    assert_eq!(
        (fresh.age, fresh.first_of_week, fresh.first_of_month),
        (Age::New, true, true)
    );
}

#[test]
fn this_computer_produces_a_valid_message() {
    let rules = ReceiverRules::new();
    assert!(rules.app.is_match(crate::VERSION) && crate::VERSION.len() <= 32);
    let platform = Platform::current().expect("Windows、macOS、Linux 的 x86_64 与 aarch64 都发");
    assert!(
        rules.os_version_ok(&platform.os, &platform.os_version),
        "{platform:?}"
    );
    let json = build_message(&platform, "zh-CN", None, date(2026, 10, 29)).to_json();
    rules.check(&json).unwrap();
    // 不含构建号、补丁号：Windows 的完整版本号（10.0.26200.6899）不出现在正文中
    let full = &crate::diag::system_info().version;
    if !full.is_empty() && full.contains('.') {
        assert!(!json.contains(full.as_str()), "{json}");
    }
}

#[test]
fn coarse_os_versions() {
    let rules = ReceiverRules::new();
    for (build, display, expected) in [
        (Some(26200), "25H2", "11 25H2"),
        (Some(22631), "23H2", "11 23H2"),
        (Some(22000), "", "11"),
        (Some(19045), "22H2", "10 22H2"),
        (Some(18363), "1909", "10"),
        (Some(17763), " 1809 ", "10"),
        (None, "24H2", "10 24H2"),
        (Some(26100), "24h2", "11"),
    ] {
        let version = crate::diag::windows_coarse(build, display);
        assert_eq!(version, expected, "{build:?} {display}");
        assert!(rules.os_version_ok("windows", &version));
    }
    for (darwin, expected) in [
        ("25.0.0", "26"),
        ("24.6.0", "15"),
        ("20.1.0", "11"),
        ("19.6.0", "10"),
        ("", "10"),
        ("x", "10"),
    ] {
        let version = crate::diag::macos_coarse(darwin);
        assert_eq!(version, expected, "{darwin}");
        assert!(rules.os_version_ok("macos", &version));
    }
    let linux = |text: &str| crate::diag::linux_coarse(Some(text));
    for (text, expected) in [
        (
            "NAME=\"Ubuntu\"\nID=ubuntu\nVERSION_ID=\"24.04\"\n",
            "ubuntu 24.04",
        ),
        ("ID=debian\nVERSION_ID=\"12\"\n", "debian 12"),
        ("ID=fedora\nVERSION_ID=41\n", "fedora 41"),
        ("ID=arch\nBUILD_ID=rolling\n", "arch"),
        (
            "ID=\"opensuse-leap\"\nVERSION_ID=\"15.6\"\n",
            "opensuse 15.6",
        ),
        (
            "ID=\"opensuse-tumbleweed\"\nVERSION_ID=\"20241015\"\n",
            "opensuse 20241015",
        ),
        (
            "ID=linuxmint\nVERSION_ID=\"22.1\"\nID_LIKE=ubuntu\n",
            "linuxmint 22.1",
        ),
        ("ID=debian\nVERSION_ID=\"trixie/sid\"\n", "debian"),
        ("ID=ubuntu\nVERSION_ID=1.2.3.4\n", "ubuntu"),
        ("ID=centos\nVERSION_ID=\"9\"\n", "other"),
        ("ID=pop\nID_LIKE=\"ubuntu debian\"\n", "other"),
        ("", "other"),
    ] {
        let version = linux(text);
        assert_eq!(version, expected, "{text:?}");
        assert!(rules.os_version_ok("linux", &version));
    }
    assert_eq!(crate::diag::linux_coarse(None), "other");
    let current = crate::diag::coarse_version();
    assert!(
        rules.os_version_ok(std::env::consts::OS, current),
        "{current}"
    );
}

// ---------------------------------------------------------------- 环境变量

#[test]
fn environment_variables_turn_it_off_or_redirect_it() {
    let fixed = |url: &str| Route::Fixed(Url::parse(url).unwrap());
    // 发布构建
    assert_eq!(Route::resolve(None, None, false), Route::Default);
    assert_eq!(Route::resolve(Some(""), Some("  "), false), Route::Default);
    for dnt in ["1", "true", "yes"] {
        assert_eq!(
            Route::resolve(Some(dnt), Some("https://a.example.com/s"), false),
            Route::EnvOff(DO_NOT_TRACK_ENV),
            "{dnt}"
        );
    }
    for not_set in ["0", "false", "FALSE"] {
        assert_eq!(Route::resolve(Some(not_set), None, false), Route::Default);
    }
    for off in ["off", "OFF", " off "] {
        assert_eq!(
            Route::resolve(None, Some(off), false),
            Route::EnvOff(USAGE_URL_ENV)
        );
    }
    assert_eq!(
        Route::resolve(None, Some("https://stats.example.com/v1/stats"), false),
        fixed("https://stats.example.com/v1/stats")
    );
    assert_eq!(
        Route::resolve(None, Some("http://127.0.0.1:9/v1/stats"), false),
        fixed("http://127.0.0.1:9/v1/stats")
    );
    for bad in [
        "http://stats.example.com/v1/stats",
        "https://u:p@stats.example.com/",
        "not a url",
    ] {
        assert_eq!(
            Route::resolve(None, Some(bad), false),
            Route::EnvOff(USAGE_URL_ENV),
            "{bad}"
        );
    }
    // 调试构建：只发到本机回环地址
    assert_eq!(Route::resolve(None, None, true), Route::DebugBuild);
    assert_eq!(
        Route::resolve(None, Some("https://stats.example.com/"), true),
        Route::DebugBuild
    );
    assert_eq!(
        Route::resolve(None, Some("http://localhost:8787/v1/stats"), true),
        fixed("http://localhost:8787/v1/stats")
    );
    assert_eq!(
        Route::resolve(Some("1"), Some("http://127.0.0.1:9/"), true),
        Route::EnvOff(DO_NOT_TRACK_ENV)
    );
}

// ---------------------------------------------------------------- usage.json

#[test]
fn state_file_round_trip() {
    let state = UsageState {
        since: date(2026, 10, 29),
        last_sent: date(2026, 11, 2),
    };
    let json = state.to_json();
    assert_eq!(
        json,
        r#"{"v":1,"since":"2026-10-29","lastSent":"2026-11-02"}"#
    );
    assert_eq!(UsageState::parse(&json), Some(state));
    for bad in [
        "",
        "{}",
        "not json",
        r#"{"v":1,"since":"2026-13-01","lastSent":"2026-11-02"}"#,
        r#"{"v":1,"since":"2026-10-29"}"#,
    ] {
        assert_eq!(UsageState::parse(bad), None, "{bad}");
    }
    let today = date(2026, 11, 3);
    assert_eq!(
        UsageState::after_sent(Some(&state), today),
        UsageState {
            since: state.since,
            last_sent: today
        }
    );
    assert_eq!(
        UsageState::after_sent(None, today),
        UsageState {
            since: today,
            last_sent: today
        }
    );
    // 第一次的日期比今天晚（时钟往回调过）：从今天算
    assert_eq!(
        UsageState::after_sent(Some(&state), date(2026, 10, 1)).since,
        date(2026, 10, 1)
    );
}

#[test]
fn activity_is_remembered_per_utc_day() {
    let tracker = ActivityTracker::default();
    let mut changes = tracker.subscribe();
    let day = date(2026, 10, 29);
    assert!(!tracker.is_active(noon(day)));
    assert!(tracker.mark(at(day, 1, 0, 0)), "这一天第一次");
    assert!(!tracker.mark(at(day, 2, 0, 0)), "同一天再记不算新的");
    assert!(tracker.is_active(at(day, 23, 59, 59)));
    assert!(!tracker.is_active(noon(day) + DAY_MS), "第二天还没有用过");
    assert!(changes.has_changed().unwrap());
    assert_eq!(*changes.borrow_and_update(), Some(day));
    assert!(tracker.mark(noon(day) + DAY_MS));
    assert!(changes.has_changed().unwrap());
}

// ---------------------------------------------------------------- 发送

/// 假接收端收到的一个请求：请求行与请求头（小写），正文。
struct Received {
    head: String,
    body: String,
}

/// 假接收端：依次接受 `responses.len()` 个连接，各回答一个状态码；`gate` 不为 `None` 时第一个回答之前先等它。
fn receiver_with(
    responses: Vec<u16>,
    gate: Option<mpsc::Receiver<()>>,
) -> (Url, mpsc::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!(
        "http://{}/v1/stats",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut gate = gate;
        for status in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            let received = loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break None;
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
                    let body = String::from_utf8_lossy(&data[end + 4..end + 4 + len]).into_owned();
                    break Some(Received { head, body });
                }
            };
            if let Some(received) = received {
                let _ = tx.send(received);
            }
            if let Some(gate) = gate.take() {
                let _ = gate.recv_timeout(Duration::from_secs(20));
            }
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    (url, rx)
}

fn receiver(responses: Vec<u16>) -> (Url, mpsc::Receiver<Received>) {
    receiver_with(responses, None)
}

fn reporter(dir: &tempfile::TempDir, builtin: Url) -> UsageReporter {
    UsageReporter::with(ReporterOptions {
        state_file: dir.path().join("usage.json"),
        route: Route::Default,
        remote: None,
        builtin,
    })
}

/// 按默认的本机设置（开着）检查：不需要先经过任何提示。
fn input(now_ms: i64) -> CheckInput {
    CheckInput {
        enabled: crate::model::UsageSettings::default().enabled,
        active: true,
        lang: "zh-CN".into(),
        now_ms,
    }
}

fn state_file(dir: &tempfile::TempDir) -> Option<String> {
    fs::read_to_string(dir.path().join("usage.json")).ok()
}

#[test]
fn sends_at_most_once_a_day_and_records_two_dates() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = receiver(vec![204, 204]);
    let usage = reporter(&dir, url);
    let day = date(2026, 10, 29);

    let preview = usage.preview("zh-CN", noon(day)).unwrap();
    assert_eq!(preview.last_sent, None);
    assert!(!preview.sent_today);
    let outcome = rt().block_on(usage.check(input(noon(day))));
    let Outcome::Sent(body) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(body, preview.json, "查看内容显示的就是发出的正文");
    let request = requests.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(request.body, body, "请求的正文逐字节相同");
    assert!(
        request.head.starts_with("post /v1/stats "),
        "{}",
        request.head
    );
    assert!(request.head.contains("content-type: application/json"));
    assert!(request.head.contains(&format!(
        "user-agent: termlet/{}",
        crate::VERSION.to_ascii_lowercase()
    )));
    for header in ["cookie", "authorization"] {
        assert!(!request.head.contains(header), "{}", request.head);
    }
    ReceiverRules::new().check(&body).unwrap();
    let sent: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (
            &sent["age"],
            &sent["firstOfWeek"],
            &sent["firstOfMonth"],
            &sent["lang"]
        ),
        (&"new".into(), &true.into(), &true.into(), &"zh-CN".into())
    );
    assert_eq!(
        state_file(&dir).as_deref(),
        Some(r#"{"v":1,"since":"2026-10-29","lastSent":"2026-10-29"}"#),
        "只记两个日期"
    );

    // 同一个 UTC 日不再发（到 23:59:59 都算同一天）
    assert_eq!(
        rt().block_on(usage.check(input(at(day, 23, 59, 59)))),
        Outcome::AlreadySent
    );
    let preview = usage.preview("zh-CN", noon(day)).unwrap();
    assert!(preview.sent_today);
    assert_eq!(preview.last_sent, Some(day));

    // 第二天：用了 1 天、同一周、同一月
    let next = date(2026, 10, 30);
    let Outcome::Sent(body) = rt().block_on(usage.check(input(at(next, 0, 0, 1)))) else {
        panic!("第二天应发送");
    };
    let sent: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (&sent["age"], &sent["firstOfWeek"], &sent["firstOfMonth"]),
        (&"w1".into(), &false.into(), &false.into())
    );
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(10)).unwrap().body,
        body
    );
    assert_eq!(
        state_file(&dir).as_deref(),
        Some(r#"{"v":1,"since":"2026-10-29","lastSent":"2026-10-30"}"#)
    );
}

#[test]
fn nothing_is_sent_when_turned_off_inactive_or_not_permitted() {
    let dir = tempfile::tempdir().unwrap();
    // 接收端不接受任何连接：真发了的话结果是网络错误，而不是下面这些
    let (url, requests) = receiver(Vec::new());
    let usage = reporter(&dir, url.clone());
    let now = noon(date(2026, 10, 29));
    let check = |usage: &UsageReporter, edit: fn(&mut CheckInput)| {
        let mut input = input(now);
        edit(&mut input);
        rt().block_on(usage.check(input))
    };
    assert_eq!(check(&usage, |i| i.enabled = false), Outcome::Disabled);
    assert_eq!(check(&usage, |i| i.active = false), Outcome::Inactive);
    let with_route = |route: Route| {
        UsageReporter::with(ReporterOptions {
            state_file: dir.path().join("usage.json"),
            route,
            remote: None,
            builtin: url.clone(),
        })
    };
    let dnt = with_route(Route::EnvOff(DO_NOT_TRACK_ENV));
    assert_eq!(check(&dnt, |_| {}), Outcome::EnvOff(DO_NOT_TRACK_ENV));
    assert_eq!(dnt.env_off(), Some(DO_NOT_TRACK_ENV));
    assert_eq!(dnt.destination(), Destination::EnvOff(DO_NOT_TRACK_ENV));
    let debug = with_route(Route::DebugBuild);
    assert_eq!(check(&debug, |_| {}), Outcome::DebugBuild);
    assert_eq!(debug.destination(), Destination::DebugBuild);
    assert_eq!(debug.env_off(), None);
    assert!(requests.recv_timeout(Duration::from_millis(200)).is_err());
    assert_eq!(state_file(&dir), None);
}

#[test]
fn server_errors_are_retried_and_rejections_wait_for_the_next_day() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = receiver(vec![500, 429, 204, 400, 204]);
    let usage = reporter(&dir, url);
    let day = date(2026, 10, 29);
    assert_eq!(
        rt().block_on(usage.check(input(noon(day)))),
        Outcome::Failed(SendError::Status(500))
    );
    assert_eq!(state_file(&dir), None, "失败不记日期");
    assert_eq!(
        rt().block_on(usage.check(input(noon(day) + 3_600_000))),
        Outcome::Failed(SendError::Status(429))
    );
    assert!(matches!(
        rt().block_on(usage.check(input(noon(day) + 7_200_000))),
        Outcome::Sent(_)
    ));
    // 第二天被拒绝（格式不对等）：当天不再试，第二天再发
    let next = date(2026, 10, 30);
    assert_eq!(
        rt().block_on(usage.check(input(noon(next)))),
        Outcome::Rejected(400)
    );
    assert_eq!(
        rt().block_on(usage.check(input(noon(next) + 3_600_000))),
        Outcome::GaveUpToday
    );
    let Outcome::Sent(body) = rt().block_on(usage.check(input(noon(date(2026, 10, 31))))) else {
        panic!("第三天应发送");
    };
    // 前一天没发成功的不补发：第三天的这条照常按第三天算（上次发送是第一天）
    let sent: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(sent["age"], "w1");
    assert_eq!(requests.iter().take(5).count(), 5);
}

#[test]
fn network_errors_are_retried() {
    let dir = tempfile::tempdir().unwrap();
    let url = Url::parse(&format!("{}/v1/stats", rt().block_on(closed_port_url()))).unwrap();
    let usage = reporter(&dir, url);
    let now = noon(date(2026, 10, 29));
    for _ in 0..2 {
        let outcome = rt().block_on(usage.check(input(now)));
        assert!(
            matches!(&outcome, Outcome::Failed(SendError::Network(_))),
            "{outcome:?}"
        );
    }
    assert_eq!(state_file(&dir), None);
    assert!(SendError::Status(503).retryable() && SendError::Status(408).retryable());
    assert!(!SendError::Status(404).retryable() && !SendError::Status(302).retryable());
}

#[test]
fn turning_it_off_deletes_the_record_and_starts_over() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = receiver(vec![204, 204]);
    let usage = reporter(&dir, url);
    let first = date(2026, 10, 1);
    assert!(matches!(
        rt().block_on(usage.check(input(noon(first)))),
        Outcome::Sent(_)
    ));
    assert!(state_file(&dir).is_some());
    usage.forget();
    assert_eq!(state_file(&dir), None, "本机不再保留统计用的记录");
    usage.forget();
    // 再打开时从头算：第一天、本周与本月第一次
    let later = date(2026, 10, 20);
    let Outcome::Sent(body) = rt().block_on(usage.check(input(noon(later)))) else {
        panic!("重新打开后应发送");
    };
    let sent: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (&sent["age"], &sent["firstOfWeek"], &sent["firstOfMonth"]),
        (&"new".into(), &true.into(), &true.into())
    );
}

#[test]
fn turning_it_off_during_a_send_keeps_no_record() {
    let dir = tempfile::tempdir().unwrap();
    let (gate_tx, gate_rx) = mpsc::channel();
    let (url, requests) = receiver_with(vec![204], Some(gate_rx));
    let usage = reporter(&dir, url);
    let now = noon(date(2026, 10, 29));
    let task = rt().spawn({
        let usage = usage.clone();
        async move { usage.check(input(now)).await }
    });
    // 接收端收到了、还没回答：这时另一次检查不再发，关闭开关
    requests.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(rt().block_on(usage.check(input(now))), Outcome::Busy);
    usage.forget();
    gate_tx.send(()).unwrap();
    let outcome = rt().block_on(task).unwrap();
    assert!(
        matches!(outcome, Outcome::Sent(_)),
        "正在进行的那一次不取消"
    );
    assert_eq!(state_file(&dir), None, "关闭后不再写 usage.json");
}

#[test]
fn clock_moved_back_counts_as_not_sent_today() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = receiver(vec![204]);
    let usage = reporter(&dir, url);
    fs::write(
        dir.path().join("usage.json"),
        r#"{"v":1,"since":"2026-10-20","lastSent":"2026-11-05"}"#,
    )
    .unwrap();
    let today = date(2026, 11, 2);
    let Outcome::Sent(body) = rt().block_on(usage.check(input(noon(today)))) else {
        panic!("上次发送比今天晚时按今天还没发处理");
    };
    let sent: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (&sent["age"], &sent["firstOfWeek"], &sent["firstOfMonth"]),
        (&"m1".into(), &true.into(), &true.into())
    );
    assert_eq!(
        state_file(&dir).as_deref(),
        Some(r#"{"v":1,"since":"2026-10-20","lastSent":"2026-11-02"}"#)
    );
}

#[test]
fn remote_config_can_pause_it_and_falls_back_to_the_builtin_address() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = receiver(vec![204]);
    let server = rt().block_on(TestServer::start());
    let key = TestKey::random();
    let source = RemoteConfigSource::with(
        Url::parse(&server.url("/update.json")).unwrap(),
        dir.path().join("remote-config.json"),
        vec![key.public_key()],
    );
    let usage = UsageReporter::with(ReporterOptions {
        state_file: dir.path().join("usage.json"),
        route: Route::Default,
        remote: Some(source),
        builtin: url.clone(),
    });
    assert_eq!(usage.destination(), Destination::Url(url.to_string()));
    let publish = |text: &str, issued: i64| {
        server.put("/update.json", text);
        server.put(
            "/update.json.minisig",
            key.sign(
                text.as_bytes(),
                &remote_config_comment(issued, issued + 90 * DAY_MS),
            ),
        );
    };
    let now = noon(date(2026, 10, 29));
    let issued = crate::time::now_ms() / 1000 * 1000;
    publish("{\"usageUrl\":\"off\"}\n", issued);
    assert_eq!(rt().block_on(usage.check(input(now))), Outcome::Paused);
    assert_eq!(usage.destination(), Destination::Paused);
    assert!(
        requests.recv_timeout(Duration::from_millis(200)).is_err(),
        "暂停时不发"
    );
    // 撤销暂停（没有 usageUrl）：用内置地址
    publish("{\"disabled\":false}\n", issued + 1000);
    assert!(matches!(
        rt().block_on(usage.check(input(now))),
        Outcome::Sent(_)
    ));
    assert!(requests.recv_timeout(Duration::from_secs(10)).is_ok());
    assert_eq!(usage.destination(), Destination::Url(url.to_string()));

    // 远程配置不可用（离线、没有签名）：照样用内置地址发
    let (url, requests) = receiver(vec![204]);
    let offline = UsageReporter::with(ReporterOptions {
        state_file: dir.path().join("usage-2.json"),
        route: Route::Default,
        remote: Some(RemoteConfigSource::with(
            Url::parse(&format!("{}/update.json", rt().block_on(closed_port_url()))).unwrap(),
            dir.path().join("remote-config-2.json"),
            vec![key.public_key()],
        )),
        builtin: url,
    });
    assert!(matches!(
        rt().block_on(offline.check(input(now))),
        Outcome::Sent(_)
    ));
    assert!(requests.recv_timeout(Duration::from_secs(10)).is_ok());
}

#[test]
fn fixed_route_skips_the_remote_config() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = receiver(vec![204]);
    let usage = UsageReporter::with(ReporterOptions {
        state_file: dir.path().join("usage.json"),
        route: Route::Fixed(url.clone()),
        remote: None,
        builtin: Url::parse(USAGE_URL).unwrap(),
    });
    assert_eq!(usage.destination(), Destination::Url(url.to_string()));
    assert!(matches!(
        rt().block_on(usage.check(input(noon(date(2026, 10, 29))))),
        Outcome::Sent(_)
    ));
    assert!(requests.recv_timeout(Duration::from_secs(10)).is_ok());
}
