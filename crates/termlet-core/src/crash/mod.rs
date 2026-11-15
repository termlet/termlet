//! 崩溃记录与可选的自动上报。
//!
//! - **记录**：[`install`] 设置 panic hook。每次 panic（界面与核心在同一进程，哪个线程都算）在 `{数据目录}/logs/`
//!   写一份 `crash-<UTC 时间>-<随机串>.json`，最多保留 [`MAX_RECORDS`] 份，同时写进
//!   日志；之后照常交给原来的 hook。一次运行最多写 [`MAX_PER_RUN`] 份，反复 panic 时不把以前的记录挤掉。
//! - **内容**（[`CrashReport`]）：版本、系统、线程、出错位置、说明与
//!   调用栈。写入前就已脱敏（[`Scrubber::message`]、[`Scrubber::stack`]：用户目录、用户名、网址、IP 地址、
//!   `user@host`、引号里的内容、`host:port`、秘密），本机看到的就是会发出去的内容；不含内存转储。
//!   系统版本只留版本号与常见的发行版、内核类型标记（[`clean_os_release`]）；线程名、出错位置与系统版本是一行，
//!   长度不超过接收端的上限（[`MAX_THREAD_CHARS`] 等）。
//! - **自动上报**（设置“自动发送崩溃报告”，`LogSettings::crash_report`，默认关闭）：开启期间写入的记录标为待发送，
//!   [`CrashLog::flush`] 发到接收地址（[`report_url`]：签名的远程配置给出的地址，环境变量 [`CRASH_URL_ENV`] 可以
//!   覆盖，`off` 表示不发），成功后标为已发送；关闭后待发送的不再发送（本机的记录保留）。
//!   只发报告本身（不带只在本机有用的 `dump`）。接收端拒收的（4xx，408、429 除外）标为被拒、不再发送，接着发下一份；
//!   网络错误、超时、408、429 与 5xx 留到下次，429 带的 `Retry-After`（最多 [`MAX_RETRY_AFTER`]）之内不再发。
//! - **原生崩溃**（Windows，[`native`]）：访问冲突、栈溢出等不经过 panic 的崩溃也写一份记录（原因为 `native`，
//!   说明是异常代码），并在 `{日志目录}/dumps/` 保存小型内存转储（最多 [`native::MAX_DUMPS`] 份，只在本机，不上报）。
//! - 发布构建去掉了符号，调用栈多半只有 `<unknown>`；出错位置（源文件与行号，panic 自带）是最有用的部分。
//!   快速失败（`__fastfail`，Rust 的 abort 也是它）不经过任何处理程序，没有记录（panic 引起的 abort 已有 panic 的记录）。
//!
//! # 用法（界面）
//!
//! - 启动时尽早调用 [`install`]（初始化日志之后、打开窗口之前），传入 `DataDir::logs_dir()`；按设置调用
//!   [`set_reporting`]，设置中改了“自动发送崩溃报告”时再调用一次并随后 [`CrashLog::flush`]。
//! - 窗口打开后 [`CrashLog::unseen`]：有新的记录时提示“Termlet 上次运行时出错了”（查看 / 忽略），提示过后
//!   [`CrashLog::mark_seen`]。查看：[`CrashLog::list`]（最新的在前），显示与复制用 [`CrashRecord::to_text`]；
//!   删除：[`CrashLog::delete`]、[`CrashLog::clear`]。
//! - 接收地址：`report_url(remote_config.crash_report_endpoint())`（远程配置由更新模块读取并校验签名）。为 `None` 时
//!   设置中不显示“自动发送崩溃报告”（没有地址时开了也发不出去）。有地址且开启时，启动后与设置改变后调用
//!   [`CrashLog::flush`]；暂时发不出去的留到下次。
//! - 方法都是异步的，内部在核心的运行时上执行，返回的 future 是 `Send`，可以在 GPUI 的任务里直接 `await`。

#[cfg(windows)]
pub mod native;
#[cfg(test)]
mod tests;

use std::fs;
use std::io;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::diag::{Scrubber, system_info};

/// 最多保留的崩溃记录数。
pub const MAX_RECORDS: usize = 20;
/// 一次运行最多写这么多份。
pub const MAX_PER_RUN: usize = 10;
/// 原生崩溃的内存转储所在的目录（日志目录下）。
pub const DUMP_DIR: &str = "dumps";
/// 覆盖接收地址的环境变量（测试用）；值为 `off` 时不发送。
pub const CRASH_URL_ENV: &str = "TERMLET_CRASH_URL";
/// 线程名最多的字符数（含截断时加的省略号；与接收端的上限相同，说明与调用栈见 [`Scrubber::message`]、
/// [`Scrubber::stack`]，都比接收端的上限短）。
pub const MAX_THREAD_CHARS: usize = 256;
/// 出错位置最多的字符数（含省略号）。
pub const MAX_LOCATION_CHARS: usize = 1024;
/// 系统版本最多的字符数（含省略号）。
pub const MAX_OS_RELEASE_CHARS: usize = 128;
/// 接收端回 429 时，`Retry-After` 最多按这么久算（一次运行之内有效）。
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// 发送一份报告的时限。
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// 被拒时日志中最多记接收端回答的这么多字节。
const MAX_REJECT_DETAIL: usize = 200;
const FILE_PREFIX: &str = "crash-";
const FILE_SUFFIX: &str = ".json";
/// 记录文件的格式版本。
const FORMAT: u32 = 1;

/// 崩溃报告：本机保存、也是发出去的内容（已脱敏）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashReport {
    pub id: String,
    /// RFC 3339（UTC）
    pub time: String,
    pub version: String,
    /// `windows`、`macos`、`linux`
    pub platform: String,
    pub arch: String,
    pub os_release: String,
    /// 进程：`desktop`（界面与核心在同一进程）
    pub process: String,
    /// 原因：`panic`
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// 出错的线程（`main`、`termlet-io` 等）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// 出错位置：`源文件:行:列`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// 原生崩溃的内存转储（`dumps/` 下的文件名，只在本机，上报时不带）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dump: Option<String>,
}

/// 生成报告的输入（还没有脱敏）。
#[derive(Clone, Debug, Default)]
pub struct CrashInput {
    pub process: String,
    pub reason: String,
    pub exit_code: Option<i64>,
    pub thread: Option<String>,
    pub location: Option<String>,
    pub message: Option<String>,
    pub stack: Option<String>,
    pub dump: Option<String>,
}

/// 生成报告时的环境。
#[derive(Clone, Debug, Default)]
pub struct ReportEnv {
    pub id: String,
    pub time: String,
    pub version: String,
    pub platform: String,
    pub arch: String,
    pub os_release: String,
}

/// 生成报告（纯函数）：只包含约定的字段，说明、调用栈、出错位置与线程名经 `scrub` 脱敏，系统版本经
/// [`clean_os_release`] 处理；线程名、出错位置是一行（控制字符换成空格），超长时截断（见 [`MAX_THREAD_CHARS`] 等）。
pub fn build_report(input: CrashInput, env: ReportEnv, scrub: &Scrubber) -> CrashReport {
    let clean =
        |v: Option<String>, f: &dyn Fn(&str) -> String| v.filter(|s| !s.is_empty()).map(|s| f(&s));
    CrashReport {
        id: env.id,
        time: env.time,
        version: env.version,
        platform: env.platform,
        arch: env.arch,
        os_release: clean_os_release(&env.os_release),
        process: input.process,
        reason: input.reason,
        exit_code: input.exit_code,
        thread: clean(input.thread, &|s| {
            one_line(&scrub.text(s), MAX_THREAD_CHARS)
        }),
        location: clean(input.location, &|s| {
            one_line(&scrub.stack(s), MAX_LOCATION_CHARS)
        }),
        message: clean(input.message, &|s| scrub.message(s)),
        stack: clean(input.stack, &|s| scrub.stack(s)),
        dump: input.dump,
    }
}

/// 一行文字：控制字符（换行等）换成空格；超过 `max` 个字符时截断，以省略号结尾，连同省略号不超过 `max` 个字符。
fn one_line(text: &str, max: usize) -> String {
    let line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if line.chars().count() <= max {
        return line;
    }
    let end = line
        .char_indices()
        .nth(max.saturating_sub(1))
        .map_or(line.len(), |(i, _)| i);
    format!("{}…", &line[..end])
}

/// 系统版本（Windows 为 `10.0.26200.6899`，其他系统为 `uname -r`）中可以发出去的部分。按 `.` `-` `+` `_` `~`
/// 分段：数字与常见的发行版、内核类型、架构标记（`generic`、`microsoft-standard-WSL2`、`el9`、`fc41`、`rpi`、
/// `x86_64` 等）照留，别的段换成 `*`。Linux 上自己编的内核可以在版本后面加任意的字符串（`CONFIG_LOCALVERSION`），
/// 可能是主机名、用户名之类；Windows 与 macOS 的只有数字。最长 [`MAX_OS_RELEASE_CHARS`] 个字符。
pub fn clean_os_release(raw: &str) -> String {
    static KNOWN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)^(?:[0-9]+k?|g[0-9a-f]{7,40}|rc[0-9]+|v[0-9]+l?|el[0-9]+(?:uek)?|fc[0-9]+|amzn[0-9]+",
            r"|mga[0-9]+|deb[0-9]+|arch[0-9]*|zen[0-9]*|hardened[0-9]*|lqx[0-9]*|xanmod[0-9]*|rt[0-9]*",
            r"|wsl[0-9]*|asahi[0-9]*|valve[0-9]*|sunxi[0-9]*|rockchip[0-9]*|meson[0-9]*|bcm[0-9]+",
            r"|android[0-9]*|generic|lowlatency|microsoft|standard|linuxkit|cloud|aws|azure|gcp|gke|oracle",
            r"|kvm|virt|virtual|pve|default|lts|rpi|rpt|raspi|bpo|ubuntu|debian|gentoo|dist|manjaro",
            r"|cachyos|liquorix|mainline|stable|vanilla|current|edge|legacy|native|neptune|surface|tegra",
            r"|qcom|oem|hwe|preempt|smp|pae|lpae|custom|dirty|x86|amd64|arm64|aarch64|i[3-6]86|armhf",
            r"|armv[5-8]l?|riscv64|ppc64le|ppc64|s390x|loongarch64)$",
        ))
        .expect("系统版本标记的正则")
    });
    const SEPARATORS: [char; 5] = ['.', '-', '+', '_', '~'];
    let mut out = String::new();
    for piece in raw.trim().split_inclusive(SEPARATORS) {
        let token = piece.strip_suffix(SEPARATORS).unwrap_or(piece);
        if token.is_empty() || KNOWN.is_match(token) {
            out.push_str(token);
        } else {
            out.push('*');
        }
        out.push_str(&piece[token.len()..]);
    }
    one_line(&out, MAX_OS_RELEASE_CHARS)
}

/// 上报的状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Upload {
    /// 不发送（写入时没有开启自动上报，或之后关闭了）
    #[default]
    None,
    /// 等待发送
    Pending,
    /// 已发送
    Sent,
    /// 接收端拒收（4xx，408、429 除外），不再发送
    Rejected,
}

/// 本机的一条崩溃记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashRecord {
    /// 记录的标识（文件名去掉扩展名），删除时用
    pub id: String,
    /// 写入的时间（Unix 毫秒）
    pub created_at: i64,
    /// 界面已经提示过
    pub seen: bool,
    pub upload: Upload,
    /// 发送的时间（Unix 毫秒）
    pub sent_at: Option<i64>,
    pub report: CrashReport,
}

impl CrashRecord {
    /// 显示与复制用的文本。
    pub fn to_text(&self) -> String {
        let r = &self.report;
        let mut out = format!(
            "时间：{}\n版本：{}（{} {} {}）\n",
            crate::time::format_local(self.created_at),
            r.version,
            r.platform,
            r.os_release,
            r.arch
        );
        if let Some(thread) = &r.thread {
            out.push_str(&format!("线程：{thread}\n"));
        }
        if let Some(location) = &r.location {
            out.push_str(&format!("位置：{location}\n"));
        }
        if let Some(message) = &r.message {
            out.push_str(&format!("说明：{message}\n"));
        }
        if let Some(dump) = &r.dump {
            out.push_str(&format!("内存转储：dumps/{dump}（只在本机）\n"));
        }
        if let Some(stack) = &r.stack {
            out.push_str("调用栈：\n");
            out.push_str(stack);
            if !stack.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

/// 记录文件的内容。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredRecord {
    v: u32,
    created_at: i64,
    #[serde(default)]
    seen: bool,
    #[serde(default)]
    upload: Upload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sent_at: Option<i64>,
    report: CrashReport,
}

// ---------------------------------------------------------------- panic hook

/// panic hook 用到的东西：安装时准备好，panic 时不再做耗时、可能失败的事。
struct Hook {
    dir: PathBuf,
    scrub: Scrubber,
    os_release: String,
    written: AtomicUsize,
}

static HOOK: OnceLock<Hook> = OnceLock::new();
/// 新的记录是否标为待发送（[`set_reporting`]）。
static REPORTING: AtomicBool = AtomicBool::new(false);

/// 设置 panic hook，把崩溃记录写到 `logs_dir`。只有第一次调用生效。
pub fn install(logs_dir: &Path) {
    LazyLock::force(&crate::diag::STARTED);
    let hook = Hook {
        dir: logs_dir.to_path_buf(),
        scrub: Scrubber::current_user(),
        // 也编译好其中的正则
        os_release: clean_os_release(&system_info().version),
        written: AtomicUsize::new(0),
    };
    // 先用一次：编译好正则
    let _ = hook.scrub.message("warm up 10.0.0.1 a@b.c 'x'");
    let _ = hook.scrub.stack("warm up");
    if HOOK.set(hook).is_err() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_panic(info);
        previous(info);
    }));
    #[cfg(windows)]
    native::install(logs_dir, record_native);
}

/// 原生崩溃（Windows）：在写转储的线程上写一份记录。
#[cfg(windows)]
fn record_native(code: u32, dump: Option<&str>) {
    let Some(hook) = HOOK.get() else {
        return;
    };
    let input = CrashInput {
        process: "desktop".into(),
        reason: "native".into(),
        exit_code: Some(i64::from(code)),
        message: Some(native::describe(code)),
        dump: dump.map(str::to_string),
        ..Default::default()
    };
    let now = crate::time::now_ms();
    let report = build_report(input, report_env(now, &hook.os_release), &hook.scrub);
    tracing::error!(
        dump = report.dump.as_deref().unwrap_or(""),
        "程序崩溃（原生）：{}",
        report.message.as_deref().unwrap_or("")
    );
    let upload = if REPORTING.load(Ordering::Relaxed) {
        Upload::Pending
    } else {
        Upload::None
    };
    match write_record(&hook.dir, &report, upload, now) {
        Ok(_) => prune(&hook.dir, MAX_RECORDS),
        Err(e) => tracing::warn!("保存崩溃记录失败：{e}"),
    }
}

/// 开启或关闭自动上报：之后写入的记录是否标为待发送。
pub fn set_reporting(enabled: bool) {
    REPORTING.store(enabled, Ordering::Relaxed);
}

fn record_panic(info: &PanicHookInfo<'_>) {
    let Some(hook) = HOOK.get() else {
        return;
    };
    if hook.written.fetch_add(1, Ordering::Relaxed) >= MAX_PER_RUN {
        return;
    }
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "Box<dyn Any>".into());
    let input = CrashInput {
        process: "desktop".into(),
        reason: "panic".into(),
        exit_code: None,
        thread: std::thread::current().name().map(str::to_string),
        location: info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())),
        message: Some(message),
        stack: Some(trim_backtrace(
            &std::backtrace::Backtrace::force_capture().to_string(),
        )),
        dump: None,
    };
    let now = crate::time::now_ms();
    let report = build_report(input, report_env(now, &hook.os_release), &hook.scrub);
    tracing::error!(
        thread = report.thread.as_deref().unwrap_or(""),
        location = report.location.as_deref().unwrap_or(""),
        "程序出错（panic）：{}",
        report.message.as_deref().unwrap_or("")
    );
    let upload = if REPORTING.load(Ordering::Relaxed) {
        Upload::Pending
    } else {
        Upload::None
    };
    match write_record(&hook.dir, &report, upload, now) {
        Ok(_) => prune(&hook.dir, MAX_RECORDS),
        Err(e) => tracing::warn!("保存崩溃记录失败：{e}"),
    }
}

fn report_env(now: i64, os_release: &str) -> ReportEnv {
    ReportEnv {
        id: random_suffix(16),
        time: rfc3339_utc(now),
        version: crate::VERSION.into(),
        platform: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        os_release: os_release.into(),
    }
}

/// 去掉调用栈开头属于 panic 处理本身的帧（取调用栈、panic hook、`core::panicking` 等），从出错的代码开始。
/// 没有符号（发布构建）时认不出，原样返回。
pub fn trim_backtrace(text: &str) -> String {
    const MACHINERY: &[&str] = &[
        "std::backtrace",
        "std::sys::backtrace",
        "std::panicking::",
        "core::panicking::",
        "rust_begin_unwind",
        "__rustc::",
        "termlet_core::crash::",
        "core::ops::function::Fn",
        "alloc::boxed::impl$",
        "<alloc::boxed::Box<F,A> as core::ops::function::Fn",
        "core::result::unwrap_failed",
        "core::option::unwrap_failed",
        "core::option::expect_failed",
        "core::result::Result<T,E>::unwrap",
        "core::result::Result<T,E>::expect",
        "core::option::Option<T>::unwrap",
        "core::option::Option<T>::expect",
    ];
    // 按帧切开：帧以"序号: 函数名"开头，之后的"at 文件:行"属于同一帧
    let mut frames: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let is_frame = trimmed
            .split_once(": ")
            .is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if is_frame || frames.is_empty() {
            let name = trimmed
                .split_once(": ")
                .map(|(_, f)| f.to_string())
                .unwrap_or_default();
            frames.push((name, format!("{line}\n")));
        } else if let Some(last) = frames.last_mut() {
            last.1.push_str(line);
            last.1.push('\n');
        }
    }
    let skip = frames
        .iter()
        .take(64)
        .rposition(|(name, _)| MACHINERY.iter().any(|m| name.contains(m)))
        .map_or(0, |i| i + 1);
    if skip >= frames.len() {
        return text.to_string();
    }
    frames[skip..]
        .iter()
        .map(|(_, body)| body.as_str())
        .collect()
}

// ---------------------------------------------------------------- 本机的记录

/// 写一份记录，返回记录的标识。
fn write_record(dir: &Path, report: &CrashReport, upload: Upload, now: i64) -> io::Result<String> {
    fs::create_dir_all(dir)?;
    let id = format!("{FILE_PREFIX}{}-{}", file_stamp(now), random_suffix(4));
    let stored = StoredRecord {
        v: FORMAT,
        created_at: now,
        seen: false,
        upload,
        sent_at: None,
        report: report.clone(),
    };
    let data = serde_json::to_vec_pretty(&stored).map_err(io::Error::other)?;
    crate::platform::write_private_file(&dir.join(format!("{id}{FILE_SUFFIX}")), &data)?;
    Ok(id)
}

/// 更新一份记录的状态（读出、修改、写回）。
fn update_record(dir: &Path, id: &str, edit: impl FnOnce(&mut StoredRecord)) -> io::Result<()> {
    let path = record_path(dir, id)?;
    let mut stored: StoredRecord =
        serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?;
    edit(&mut stored);
    let data = serde_json::to_vec_pretty(&stored).map_err(io::Error::other)?;
    crate::platform::write_private_file(&path, &data)
}

/// 记录文件的路径；标识不合法（含路径分隔符等）时返回错误。
fn record_path(dir: &Path, id: &str) -> io::Result<PathBuf> {
    let valid = id.starts_with(FILE_PREFIX)
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "崩溃记录的标识不合法",
        ));
    }
    Ok(dir.join(format!("{id}{FILE_SUFFIX}")))
}

/// 目录中的记录文件的标识。
fn record_ids(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(FILE_SUFFIX)?;
            (id.starts_with(FILE_PREFIX) && e.file_type().is_ok_and(|t| t.is_file()))
                .then(|| id.to_string())
        })
        .collect()
}

/// 读出全部记录，最新的在前；读不了、格式不对的跳过（记一条警告）。
pub(crate) fn list_in(dir: &Path) -> Vec<CrashRecord> {
    let mut out: Vec<CrashRecord> = record_ids(dir)
        .into_iter()
        .filter_map(|id| {
            let path = dir.join(format!("{id}{FILE_SUFFIX}"));
            let parsed = fs::read(&path).map_err(|e| e.to_string()).and_then(|data| {
                serde_json::from_slice::<StoredRecord>(&data).map_err(|e| e.to_string())
            });
            match parsed {
                Ok(stored) => Some(CrashRecord {
                    id,
                    created_at: stored.created_at,
                    seen: stored.seen,
                    upload: stored.upload,
                    sent_at: stored.sent_at,
                    report: stored.report,
                }),
                Err(e) => {
                    tracing::warn!(record = %id, "崩溃记录无法读取：{e}");
                    None
                }
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    out
}

/// 只保留最新的 `keep` 份（按文件名：以时间开头）。
fn prune(dir: &Path, keep: usize) {
    let mut ids = record_ids(dir);
    ids.sort_unstable_by(|a, b| b.cmp(a));
    for id in ids.into_iter().skip(keep) {
        let _ = fs::remove_file(dir.join(format!("{id}{FILE_SUFFIX}")));
    }
}

/// 本机的崩溃记录，见模块说明。
#[derive(Clone, Debug)]
pub struct CrashLog {
    dir: PathBuf,
}

/// 一次 [`CrashLog::flush`] 的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flushed {
    /// 发送成功的份数
    pub sent: usize,
    /// 接收端拒收、不再发送的份数
    pub rejected: usize,
    /// 关闭了自动上报、不再发送的份数
    pub dropped: usize,
    /// 暂时发不出去的原因（这一份与之后的留到下次）
    pub error: Option<SendError>,
}

impl CrashLog {
    /// `logs_dir` 为 `DataDir::logs_dir()`。
    pub fn new(logs_dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: logs_dir.into(),
        }
    }

    /// 记录对应的内存转储（原生崩溃）；没有或已被删掉时为 `None`。文件名不合法（含路径分隔符等）时也是 `None`。
    pub fn dump_path(&self, report: &CrashReport) -> Option<PathBuf> {
        let name = report.dump.as_deref()?;
        let valid =
            name.ends_with(".dmp") && !name.contains(['/', '\\', ':']) && !name.starts_with('.');
        let path = self.dir.join(DUMP_DIR).join(name);
        (valid && path.is_file()).then_some(path)
    }

    /// 全部记录，最新的在前。
    pub async fn list(&self) -> Vec<CrashRecord> {
        let dir = self.dir.clone();
        blocking(move || list_in(&dir)).await.unwrap_or_default()
    }

    /// 界面还没有提示过的记录，最新的在前。
    pub async fn unseen(&self) -> Vec<CrashRecord> {
        let mut list = self.list().await;
        list.retain(|r| !r.seen);
        list
    }

    /// 把全部记录标为已提示，返回改了几份。
    pub async fn mark_seen(&self) -> io::Result<usize> {
        let dir = self.dir.clone();
        blocking(move || {
            let mut changed = 0;
            for record in list_in(&dir).into_iter().filter(|r| !r.seen) {
                update_record(&dir, &record.id, |s| s.seen = true)?;
                changed += 1;
            }
            Ok(changed)
        })
        .await?
    }

    /// 删除一份记录（已经没有了也算成功）。
    pub async fn delete(&self, id: &str) -> io::Result<()> {
        let path = record_path(&self.dir, id)?;
        let log = self.clone();
        blocking(move || {
            // 原生崩溃的转储随记录一起删
            let dump = fs::read(&path)
                .ok()
                .and_then(|data| serde_json::from_slice::<StoredRecord>(&data).ok())
                .and_then(|stored| log.dump_path(&stored.report));
            if let Some(dump) = dump {
                let _ = fs::remove_file(dump);
            }
            match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        })
        .await?
    }

    /// 删除全部记录，返回删了几份。
    pub async fn clear(&self) -> io::Result<usize> {
        let dir = self.dir.clone();
        blocking(move || {
            let mut removed = 0;
            for id in record_ids(&dir) {
                match fs::remove_file(dir.join(format!("{id}{FILE_SUFFIX}"))) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            // 原生崩溃的转储也全部删掉
            if let Ok(entries) = fs::read_dir(dir.join(DUMP_DIR)) {
                for entry in entries.filter_map(Result::ok) {
                    if entry.path().extension().is_some_and(|x| x == "dmp") {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
            Ok(removed)
        })
        .await?
    }

    /// 按设置处理待发送的记录：`enabled` 为 false 时不再发送（标为不发送）；为 true 且有接收地址时从旧到新逐份发送，
    /// 成功的标为已发送，被拒的标为被拒、接着发下一份，暂时发不出去时停下、留到下次（接收端要求等待时，等待期间
    /// 直接返回）。同一时间只进行一次，另一次正在进行时直接返回。
    pub async fn flush(&self, url: Option<Url>, enabled: bool) -> Flushed {
        static FLUSHING: AtomicBool = AtomicBool::new(false);
        if FLUSHING.swap(true, Ordering::AcqRel) {
            return Flushed::default();
        }
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                FLUSHING.store(false, Ordering::Release);
            }
        }
        let _reset = Reset;
        let dir = self.dir.clone();
        let mut pending: Vec<CrashRecord> = {
            let dir = dir.clone();
            blocking(move || list_in(&dir)).await.unwrap_or_default()
        };
        pending.retain(|r| r.upload == Upload::Pending);
        pending.reverse();
        let mut out = Flushed::default();
        if !enabled {
            let ids: Vec<String> = pending.into_iter().map(|r| r.id).collect();
            out.dropped = blocking(move || {
                ids.iter()
                    .filter(|id| update_record(&dir, id, |s| s.upload = Upload::None).is_ok())
                    .count()
            })
            .await
            .unwrap_or(0);
            return out;
        }
        let Some(url) = url else {
            return out;
        };
        if pending.is_empty() {
            return out;
        }
        if let Some(wait) = retry_wait(&url) {
            tracing::debug!(
                seconds = wait.as_secs(),
                "接收端要求稍后再发崩溃报告，这次不发"
            );
            return out;
        }
        for record in pending {
            let upload = match send_report(&url, &record.report).await {
                Ok(()) => Upload::Sent,
                Err(SendError::Rejected { status, detail }) => {
                    tracing::warn!(record = %record.id, status, detail, "接收端拒收这份崩溃报告，不再发送");
                    Upload::Rejected
                }
                Err(e) => {
                    tracing::warn!("发送崩溃报告失败：{e}");
                    if let SendError::Status {
                        retry_after: Some(wait),
                        ..
                    } = &e
                    {
                        set_retry_wait(&url, *wait);
                    }
                    out.error = Some(e);
                    break;
                }
            };
            let dir = dir.clone();
            let id = record.id.clone();
            let now = crate::time::now_ms();
            let saved = blocking(move || {
                update_record(&dir, &id, |s| {
                    s.upload = upload;
                    if upload == Upload::Sent {
                        s.sent_at = Some(now);
                    }
                })
            })
            .await;
            if let Ok(Err(e)) | Err(e) = saved {
                tracing::warn!("记下崩溃报告的发送结果失败：{e}");
            }
            if upload == Upload::Sent {
                out.sent += 1;
            } else {
                out.rejected += 1;
            }
        }
        if out.sent > 0 {
            tracing::info!(sent = out.sent, "已发送崩溃报告");
        }
        out
    }
}

/// 接收端要求的等待（429 的 `Retry-After`）：按接收地址记在内存中，一次运行之内有效。
static RETRY_AFTER: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());

/// 还要等多久才能再发到 `url`；不用等时为 `None`。
fn retry_wait(url: &Url) -> Option<Duration> {
    let mut waits = RETRY_AFTER.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    waits.retain(|(_, until)| *until > now);
    waits
        .iter()
        .find(|(u, _)| u == url.as_str())
        .map(|(_, until)| *until - now)
}

fn set_retry_wait(url: &Url, wait: Duration) {
    let mut waits = RETRY_AFTER.lock().unwrap_or_else(|e| e.into_inner());
    waits.retain(|(u, _)| u != url.as_str());
    waits.push((url.to_string(), Instant::now() + wait.min(MAX_RETRY_AFTER)));
}

/// 在核心的运行时上执行阻塞的文件操作。
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> io::Result<T> {
    match crate::runtime::runtime().spawn_blocking(f).await {
        Ok(value) => Ok(value),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(io::Error::other(e)),
    }
}

// ---------------------------------------------------------------- 上报

/// 发送崩溃报告失败的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// 网络错误、超时（留到下次）
    #[error("网络错误：{0}")]
    Network(String),
    /// 接收地址暂时不收（408、429、5xx 等，留到下次）；`retry_after` 为 429 的 `Retry-After`
    #[error("接收地址返回 HTTP {status}")]
    Status {
        status: u16,
        retry_after: Option<Duration>,
    },
    /// 接收端拒收这份报告（4xx，408、429 除外），重发也不会收；`detail` 是它的回答（一行，截短）
    #[error("接收地址拒收（HTTP {status}）：{detail}")]
    Rejected { status: u16, detail: String },
}

/// 接收地址：环境变量 [`CRASH_URL_ENV`] 优先（`off` 表示不发送），否则用远程配置给出的地址（传
/// `RemoteConfig::crash_report_endpoint()`）。只接受 https、不带账号的地址；环境变量指向本机回环地址时也可以是
/// http（测试用）。
pub fn report_url(endpoint: Option<&str>) -> Option<Url> {
    let env = std::env::var(CRASH_URL_ENV).ok();
    resolve_report_url(env.as_deref(), endpoint)
}

fn resolve_report_url(env: Option<&str>, endpoint: Option<&str>) -> Option<Url> {
    match env.map(str::trim).filter(|v| !v.is_empty()) {
        Some("off") => None,
        Some(value) => acceptable_url(value, true),
        None => acceptable_url(endpoint?, false),
    }
}

fn acceptable_url(value: &str, allow_loopback_http: bool) -> Option<Url> {
    let url = Url::parse(value.trim()).ok()?;
    let no_account = url.username().is_empty() && url.password().is_none();
    let has_host = url.host_str().is_some_and(|h| !h.is_empty());
    let scheme_ok = url.scheme() == "https"
        || (allow_loopback_http && url.scheme() == "http" && is_loopback(&url));
    (no_account && has_host && scheme_ok).then_some(url)
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// 构造上报请求：`POST` JSON（只有报告本身，不带只在本机有用的 `dump`），10 秒时限。
pub fn report_request(
    client: &reqwest::Client,
    url: &Url,
    report: &CrashReport,
) -> reqwest::RequestBuilder {
    let report = CrashReport {
        dump: None,
        ..report.clone()
    };
    client
        .post(url.clone())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&report).unwrap_or_default())
        .timeout(SEND_TIMEOUT)
}

/// 发送一份报告（在核心的运行时上执行）。
pub async fn send_report(url: &Url, report: &CrashReport) -> Result<(), SendError> {
    let url = url.clone();
    let report = report.clone();
    let task = crate::runtime::spawn(async move {
        let client =
            client(&url).ok_or_else(|| SendError::Network("无法创建 HTTP 客户端".into()))?;
        let mut response = report_request(client, &url, &report)
            .send()
            .await
            .map_err(|e| SendError::Network(e.without_url().to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let code = status.as_u16();
        if status.is_client_error() && !matches!(code, 408 | 429) {
            // 回答只用来写日志：读一小段，读不到也照样算被拒
            let mut body = Vec::new();
            while body.len() < MAX_REJECT_DETAIL
                && let Ok(Some(chunk)) = response.chunk().await
            {
                body.extend_from_slice(&chunk);
            }
            body.truncate(MAX_REJECT_DETAIL);
            let detail = one_line(String::from_utf8_lossy(&body).trim(), MAX_REJECT_DETAIL);
            return Err(SendError::Rejected {
                status: code,
                detail,
            });
        }
        let retry_after = (code == 429)
            .then(|| response.headers().get(reqwest::header::RETRY_AFTER))
            .flatten()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| parse_retry_after(v, chrono::Utc::now()));
        Err(SendError::Status {
            status: code,
            retry_after,
        })
    });
    match task.await {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(SendError::Network("程序正在退出".into())),
    }
}

/// `Retry-After` 的值（秒数或 HTTP 日期）距 `now` 还有多久，最多 [`MAX_RETRY_AFTER`]；已经过去的为零，写错的为 `None`。
fn parse_retry_after(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let value = value.trim();
    let wait = if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        // 很长的数字按上限算
        Duration::from_secs(value.parse().unwrap_or(u64::MAX))
    } else {
        let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
        (at.with_timezone(&chrono::Utc) - now)
            .to_std()
            .unwrap_or_default()
    };
    Some(wait.min(MAX_RETRY_AFTER))
}

/// HTTP 客户端：走系统与环境变量中的代理，本机回环地址不走代理。
fn client(url: &Url) -> Option<&'static reqwest::Client> {
    fn build(direct: bool) -> Option<reqwest::Client> {
        let mut builder =
            reqwest::Client::builder().user_agent(format!("Termlet/{}", crate::VERSION));
        if direct {
            builder = builder.no_proxy();
        }
        builder
            .build()
            .inspect_err(|e| tracing::warn!("无法创建 HTTP 客户端：{e}"))
            .ok()
    }
    static PROXIED: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| build(false));
    static DIRECT: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| build(true));
    if is_loopback(url) {
        DIRECT.as_ref()
    } else {
        PROXIED.as_ref()
    }
}

// ---------------------------------------------------------------- 小工具

/// 文件名中的时间（UTC，到毫秒）：`20261020-051000123`。
pub(crate) fn file_stamp(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y%m%d-%H%M%S%3f").to_string())
        .unwrap_or_else(|| ms.to_string())
}

fn rfc3339_utc(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// `n` 字节的随机数（小写 base32）；系统随机数不可用时（panic 中不能再 panic）改用时间。
fn random_suffix(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (nanos >> ((i % 16) * 8)) as u8;
        }
    }
    data_encoding::BASE32_NOPAD
        .encode(&bytes)
        .to_ascii_lowercase()
}
