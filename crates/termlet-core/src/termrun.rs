//! 在已打开的终端中执行一条命令并取回输出与退出码：`termlet exec --terminal`（堡垒机后面的服务器不能新建连接，
//! 命令写进用户看得见的终端里执行），以后撰写栏的"执行并对比"也用它。
//!
//! 做法：
//!
//! 1. 先输入一行探测（printf 输出带随机串的标记 P 与 `$((1+1))` 的结果），确认 shell 在等待输入、是 POSIX shell；
//!    [`PROBE_WAIT`] 内没有回应时按终端忙处理（正在运行别的程序、或是 fish 等不认这个写法的 shell）。
//! 2. 再输入执行行：前后各输出标记 B、E（E 带退出码），命令经 printf 的八进制转义交给 bash（没有时为 sh）执行，
//!    标准输入为 `/dev/null`（不会卡在交互提示上），标准错误并入输出；两行都以空格开头（bash 的 ignorespace 下不进历史）。
//! 3. 截取 B 与 E 之间的输出，去掉控制序列，换行统一为 `\n`。
//!
//! 标记是终端不显示的私有 OSC 序列（`ESC ] 6973 ; …… BEL`）；回显的命令行中也有标记的文字，但没有 ESC，不会被误认。
//!
//! # 界面怎么用
//!
//! 1. 找到目标终端（这台主机最新打开、已连接的 SSH 终端）。没有时返回 [`TerminalError::NotOpen`]；终端在运行全屏程序
//!    （备用屏幕）、正在传输文件、还在重连、或其中已有一次执行时返回 [`TerminalError::Busy`]。
//! 2. `let run = TermRun::new(limit)`，把 [`TermRun::feed`] 接到终端的输出旁路（解析线程上每批输出调用一次，很轻）。
//! 3. `run.execute(write, command, encoding, canceled, closed).await`：内部在核心的运行时上执行，返回的 future 可以在
//!    GPUI 的任务里直接 `await`。`write` 把字节写进终端的输入（与用户打字相同的路径），返回 `false` 表示写不进去；
//!    `canceled` 在调用方不再等待时完成（命令行工具退出：向终端发送 Ctrl+C），`closed` 在终端关闭时完成。
//! 4. 结束后从输出旁路上去掉它。

use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use regex::Regex;
use tokio::sync::watch;

use crate::charset::InputEncoder;
use crate::model::Encoding;

/// 等待探测标记的最长时间。
pub const PROBE_WAIT: Duration = Duration::from_secs(5);

/// 输入行的最大长度（bash、dash 的行编辑能接受的范围）。
pub const MAX_LINE: usize = 4000;

/// 没有指定输出上限时，B 标记之后最多保留的字节数。
const MAX_RUN_OUTPUT: usize = 16 << 20;

/// 要从输出中去掉的控制序列：CSI、OSC、其他两字节的 ESC 序列。
static ANSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]")
        .expect("控制序列的正则")
});

/// 执行的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalOutput {
    /// 退出码；结束标记中的退出码读不出来时为 -1
    pub exit_code: i32,
    /// 标准输出与标准错误（并在一起），去掉了控制序列，换行为 `\n`
    pub output: String,
    pub truncated: bool,
}

/// 不能在终端中执行的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TerminalError {
    /// 这台主机没有在 Termlet 中打开的终端
    #[error("这台主机没有在 Termlet 中打开的终端")]
    NotOpen,
    /// 终端不在等待输入（正在运行程序、全屏程序或文件传输）
    #[error("终端正忙（正在运行程序或全屏程序），请回到 shell 提示符后重试")]
    Busy,
    /// 执行中终端被关闭
    #[error("终端已关闭")]
    Closed,
    /// 命令转义后超过 [`MAX_LINE`]
    #[error("命令太长，不能在终端中执行（请改用脚本文件）")]
    TooLong,
    /// 调用方不再等待（已向终端发送 Ctrl+C）
    #[error("已取消")]
    Canceled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// 等探测标记
    Probe,
    /// 等 B 标记
    Begin,
    /// 等 E 标记
    Body,
    Done,
}

struct State {
    /// B 标记之后最多保留的字节数，超过的丢弃（之后只找 E 标记）
    keep: usize,
    /// 要在其中找标记的输出：找探测、B 标记时只留末尾可能是标记开头的部分；找到 B 之后是它之后的输出
    buf: Vec<u8>,
    /// buf 中已找过标记的长度：下次从它往前留出标记长度的位置找起，跨段的标记也能找到
    scanned: usize,
    exit: i32,
    out: Vec<u8>,
    truncated: bool,
}

/// 进行中的一次执行：输出旁路把输出交给 [`TermRun::feed`]，找到标记时通知等待的一方。
///
/// 输出可能很多（例如 cat 一个大文件），而 `feed` 在解析线程上同步执行：缓存有上限，每段输出只扫描新增的部分。
pub struct TermRun {
    nonce: String,
    limit: usize,
    state: Mutex<State>,
    phase: watch::Sender<Phase>,
    probe_wait: Duration,
}

impl TermRun {
    /// 新的一次执行；输出最多返回 `limit` 字节（0 表示不限，按 16MB）。
    pub fn new(limit: usize) -> Arc<Self> {
        Self::with_nonce(&crate::id::random_token(6), limit, PROBE_WAIT)
    }

    fn with_nonce(nonce: &str, limit: usize, probe_wait: Duration) -> Arc<Self> {
        // 去掉控制序列之后通常会变短，最后再按 limit 截断
        let keep = if limit == 0 {
            MAX_RUN_OUTPUT
        } else {
            limit.saturating_mul(2)
        };
        Arc::new(Self {
            nonce: nonce.to_string(),
            limit,
            state: Mutex::new(State {
                keep,
                buf: Vec::new(),
                scanned: 0,
                exit: -1,
                out: Vec::new(),
                truncated: false,
            }),
            phase: watch::channel(Phase::Probe).0,
            probe_wait,
        })
    }

    fn mark(&self, kind: &str) -> Vec<u8> {
        format!("\x1b]6973;{kind};{}", self.nonce).into_bytes()
    }

    /// 探测行：先清掉输入行中可能残留的内容（Ctrl+U）。
    fn probe_line(&self) -> String {
        format!(
            "\x15 printf '\\033]6973;P;{};%d\\007' $((1+1))\r",
            self.nonce
        )
    }

    /// 执行行；`command` 是按终端的编码编码后的字节。
    fn exec_line(&self, command: &[u8]) -> Result<String, TerminalError> {
        let nonce = &self.nonce;
        let line = format!(
            " printf '\\033]6973;B;{nonce}\\007'; \"$(command -v bash || command -v sh)\" -c \"$(printf '{}')\" </dev/null 2>&1; printf '\\033]6973;E;{nonce};%d\\007' $?\r",
            shell_quote(command)
        );
        if line.len() > MAX_LINE {
            return Err(TerminalError::TooLong);
        }
        Ok(line)
    }

    /// 终端的一段输出（远端的输出解码成 UTF-8 之后）。在终端的解析线程上调用。
    pub fn feed(&self, chunk: &[u8]) {
        let mut phase = *self.phase.borrow();
        if phase == Phase::Done {
            return;
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.buf.extend_from_slice(chunk);
        if phase == Phase::Probe {
            // 探测标记带 $((1+1)) 的结果：只有 POSIX shell 会输出 ;2
            let mut mark = self.mark("P");
            mark.extend_from_slice(b";2\x07");
            if find(&mut s, &mark).is_none() {
                keep_tail(&mut s, mark.len() - 1);
                return;
            }
            s.buf.clear();
            s.scanned = 0;
            drop(s);
            self.phase.send_replace(Phase::Begin);
            return;
        }
        if phase == Phase::Begin {
            let mut mark = self.mark("B");
            mark.push(0x07);
            let Some(i) = find(&mut s, &mark) else {
                keep_tail(&mut s, mark.len() - 1);
                return;
            };
            let rest = s.buf.split_off(i + mark.len());
            s.buf = rest;
            s.scanned = 0;
            phase = Phase::Body;
            self.phase.send_replace(Phase::Body);
        }
        debug_assert_eq!(phase, Phase::Body);
        let end = self.mark("E");
        let Some(i) = find(&mut s, &end) else {
            // 超过上限的输出丢弃：留下前 keep 字节，加上末尾可能是 E 标记开头的部分
            let tail = end.len() - 1;
            if s.buf.len() > s.keep + tail {
                let keep = s.keep;
                let len = s.buf.len();
                s.buf.drain(keep..len - tail);
                s.scanned = s.buf.len();
                s.truncated = true;
            }
            return;
        };
        let rest = &s.buf[i + end.len()..];
        // ;<退出码> BEL；还没到齐时下次从标记处重新找
        let bel = rest.iter().position(|&b| b == 0x07);
        if bel.is_none() && rest.len() < 16 {
            s.scanned = i;
            return;
        }
        let mut code = -1;
        if let Some(j) = bel
            && j > 1
            && rest[0] == b';'
            && let Some(n) = std::str::from_utf8(&rest[1..j])
                .ok()
                .and_then(|t| t.parse().ok())
        {
            code = n;
        }
        s.exit = code;
        let keep = s.keep;
        s.out = s.buf[..i.min(keep)].to_vec();
        s.truncated = s.truncated || i > keep;
        drop(s);
        self.phase.send_replace(Phase::Done);
    }

    /// 在终端中执行 `command`（见模块说明）。内部在核心的运行时上执行。
    pub fn execute(
        self: &Arc<Self>,
        write: impl Fn(&[u8]) -> bool + Send + 'static,
        command: &str,
        encoding: Encoding,
        canceled: impl Future<Output = ()> + Send + 'static,
        closed: impl Future<Output = ()> + Send + 'static,
    ) -> impl Future<Output = Result<TerminalOutput, TerminalError>> + Send + 'static {
        let run = self.clone();
        let encoded = InputEncoder::new(encoding).encode(command.as_bytes());
        let line = self.exec_line(&encoded);
        let task = crate::runtime::spawn(async move {
            let line = line?;
            run.drive(write, &line, canceled, closed).await
        });
        async move { task.await.unwrap_or(Err(TerminalError::Closed)) }
    }

    async fn drive(
        &self,
        write: impl Fn(&[u8]) -> bool,
        line: &str,
        canceled: impl Future<Output = ()>,
        closed: impl Future<Output = ()>,
    ) -> Result<TerminalOutput, TerminalError> {
        tokio::pin!(canceled, closed);
        let mut phase = self.phase.subscribe();
        // 输入写不进去（远端长时间不接收输入）时按终端忙处理
        if !write(self.probe_line().as_bytes()) {
            return Err(TerminalError::Busy);
        }
        tokio::select! {
            probed = tokio::time::timeout(self.probe_wait, phase.wait_for(|p| *p != Phase::Probe)) => {
                if !matches!(probed, Ok(Ok(_))) {
                    return Err(TerminalError::Busy);
                }
            }
            _ = &mut closed => return Err(TerminalError::Closed),
            _ = &mut canceled => return Err(TerminalError::Canceled),
        }
        if !write(line.as_bytes()) {
            return Err(TerminalError::Busy);
        }
        tokio::select! {
            done = phase.wait_for(|p| *p == Phase::Done) => {
                if done.is_err() {
                    return Err(TerminalError::Closed);
                }
            }
            _ = &mut closed => return Err(TerminalError::Closed),
            _ = &mut canceled => {
                // 调用方不再等待：中断终端中的命令
                write(&[0x03]);
                return Err(TerminalError::Canceled);
            }
        }
        Ok(self.result())
    }

    /// 去掉控制序列、统一换行，按上限截断。
    fn result(&self) -> TerminalOutput {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let text = String::from_utf8_lossy(&s.out);
        let text = ANSI.replace_all(&text, "");
        let text = text.replace("\r\n", "\n");
        let mut output = text.strip_prefix('\n').unwrap_or(&text).to_string();
        let mut truncated = s.truncated;
        if self.limit > 0 && output.len() > self.limit {
            let mut end = self.limit;
            while !output.is_char_boundary(end) {
                end -= 1;
            }
            output.truncate(end);
            truncated = true;
        }
        TerminalOutput {
            exit_code: s.exit,
            output,
            truncated,
        }
    }
}

/// 在 buf 还没找过的部分中找 mark；找不到时记下已找过的位置。
fn find(s: &mut State, mark: &[u8]) -> Option<usize> {
    let from = s.scanned.saturating_sub(mark.len() - 1);
    if let Some(i) = s.buf[from..].windows(mark.len()).position(|w| w == mark) {
        return Some(from + i);
    }
    s.scanned = s.buf.len();
    None
}

/// 只留 buf 末尾的 n 个字节（可能是标记的开头）。
fn keep_tail(s: &mut State, n: usize) {
    if s.buf.len() > n {
        let len = s.buf.len();
        s.buf.drain(..len - n);
        s.scanned = s.buf.len();
    }
}

/// 把命令写成 printf 的格式串（放在单引号中）：字母、数字与少数安全的字符原样，其余字节写成 `\ooo`，
/// 避免引号、反斜杠、`%` 与非 ASCII 字符在交互式 shell 与 printf 中被解释。
fn shell_quote(command: &[u8]) -> String {
    let mut out = String::with_capacity(command.len());
    for &c in command {
        if c.is_ascii_alphanumeric() || b" ./-_=:,+@#".contains(&c) {
            out.push(c as char);
        } else {
            out.push_str(&format!("\\{c:03o}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    /// 模拟终端中的 shell：看到探测行时输出 P 标记，看到执行行时回显后输出 B、output、E 标记。
    struct FakeShell {
        input: Arc<StdMutex<Vec<u8>>>,
    }

    impl FakeShell {
        fn new() -> Self {
            Self {
                input: Arc::default(),
            }
        }

        fn writer(&self) -> impl Fn(&[u8]) -> bool + Send + 'static {
            let input = self.input.clone();
            move |bytes| {
                input.lock().unwrap().extend_from_slice(bytes);
                true
            }
        }

        fn input(&self) -> String {
            String::from_utf8_lossy(&self.input.lock().unwrap()).into_owned()
        }

        /// 在后台回应：`respond_probe` 为假时不回应探测（终端忙）。
        fn answer(&self, run: &Arc<TermRun>, respond_probe: bool, output: &'static str, code: i32) {
            let input = self.input.clone();
            let run = run.clone();
            let nonce = run.nonce.clone();
            std::thread::spawn(move || {
                let mut probed = false;
                for _ in 0..500 {
                    let text = String::from_utf8_lossy(&input.lock().unwrap()).into_owned();
                    if !probed && respond_probe && text.contains(&format!("6973;P;{nonce};%d")) {
                        probed = true;
                        run.feed(format!("printf ...\r\n\x1b]6973;P;{nonce};2\x07$ ").as_bytes());
                    } else if probed && text.contains(&format!("6973;B;{nonce}")) {
                        // 回显的命令行中也有标记的文字（没有 ESC），不应被当成标记
                        run.feed(
                            format!(" printf '\\033]6973;B;{nonce}\\007'; ...\r\n").as_bytes(),
                        );
                        run.feed(format!("\x1b]6973;B;{nonce}\x07{output}").as_bytes());
                        run.feed(format!("\x1b]6973;E;{nonce};{code}\x07$ ").as_bytes());
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
        }
    }

    fn pending() -> impl Future<Output = ()> + Send + 'static {
        std::future::pending()
    }

    // 先探测再执行，截取标记之间的输出（去掉控制序列、换行统一），带退出码；命令经八进制转义
    #[tokio::test]
    async fn runs_command_between_markers() {
        let shell = FakeShell::new();
        let run = TermRun::new(0);
        shell.answer(&run, true, "\x1b[31mred\x1b[0m\r\n中文\r\n", 3);
        let result = run
            .execute(
                shell.writer(),
                r#"echo "it's 100%""#,
                Encoding::Utf8,
                pending(),
                pending(),
            )
            .await
            .unwrap();
        assert_eq!(
            result,
            TerminalOutput {
                exit_code: 3,
                output: "red\n中文\n".into(),
                truncated: false
            }
        );
        let input = shell.input();
        assert!(input.starts_with("\x15 printf"), "{input:?}");
        assert!(input.contains(r"echo \042it\047s 100\045\042"), "{input:?}");

        // 输出截断
        let shell = FakeShell::new();
        let run = TermRun::new(4);
        shell.answer(&run, true, "0123456789", 0);
        let result = run
            .execute(
                shell.writer(),
                "seq 10",
                Encoding::Utf8,
                pending(),
                pending(),
            )
            .await
            .unwrap();
        assert_eq!((result.output.as_str(), result.truncated), ("0123", true));
    }

    // 终端的编码不是 UTF-8 时，命令先编码再转义（GBK 的"中"是 D6 D0）
    #[test]
    fn command_is_encoded_for_the_terminal() {
        let run = TermRun::with_nonce("abc", 0, PROBE_WAIT);
        let encoded = InputEncoder::new(Encoding::Gb18030).encode("echo 中".as_bytes());
        assert!(run.exec_line(&encoded).unwrap().contains(r"echo \326\320"));
        let long = vec![b'"'; MAX_LINE];
        assert_eq!(run.exec_line(&long), Err(TerminalError::TooLong));
    }

    // 没有回应探测时报终端忙；执行中被取消时向终端发送 Ctrl+C
    #[tokio::test]
    async fn busy_and_canceled() {
        let shell = FakeShell::new();
        let run = TermRun::with_nonce("n1", 0, Duration::from_millis(100));
        shell.answer(&run, false, "", 0);
        let result = run
            .execute(shell.writer(), "ls", Encoding::Utf8, pending(), pending())
            .await;
        assert_eq!(result, Err(TerminalError::Busy));

        // 执行中被取消：发送 Ctrl+C
        let shell = FakeShell::new();
        let run = TermRun::with_nonce("n2", 0, Duration::from_secs(5));
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let input = shell.input.clone();
        let feeder = run.clone();
        std::thread::spawn(move || {
            for _ in 0..500 {
                let text = String::from_utf8_lossy(&input.lock().unwrap()).into_owned();
                if text.contains("6973;B;n2") {
                    let _ = cancel_tx.send(());
                    return;
                }
                if text.contains("6973;P;n2") {
                    feeder.feed(b"\x1b]6973;P;n2;2\x07");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let result = run
            .execute(
                shell.writer(),
                "sleep 100",
                Encoding::Utf8,
                async {
                    let _ = cancel_rx.await;
                },
                pending(),
            )
            .await;
        assert_eq!(result, Err(TerminalError::Canceled));
        assert!(shell.input().ends_with('\x03'), "取消时应发送 Ctrl+C");

        // 终端关闭
        let shell = FakeShell::new();
        let run = TermRun::new(0);
        let result = run
            .execute(shell.writer(), "ls", Encoding::Utf8, pending(), async {})
            .await;
        assert_eq!(result, Err(TerminalError::Closed));

        // 写不进去
        let run = TermRun::new(0);
        let result = run
            .execute(|_: &[u8]| false, "ls", Encoding::Utf8, pending(), pending())
            .await;
        assert_eq!(result, Err(TerminalError::Busy));
    }

    // 转义的结果经 printf 还原为原来的命令（本机有 bash 时用它验证）
    #[test]
    fn shell_quote_round_trips_through_printf() {
        let Some(bash) = find_bash() else {
            eprintln!("没有 bash，跳过");
            return;
        };
        for cmd in [
            r#"echo "it's" $HOME 100% \n"#,
            "多行\n命令\t`x`",
            "a;b|c&d>e<f(g){h}[i]*?~!^",
        ] {
            let out = std::process::Command::new(&bash)
                .arg("-c")
                .arg(format!("printf '{}'", shell_quote(cmd.as_bytes())))
                .output()
                .unwrap();
            assert!(out.status.success());
            assert_eq!(String::from_utf8_lossy(&out.stdout), cmd);
        }
    }

    fn find_bash() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .flat_map(|dir| [dir.join("bash.exe"), dir.join("bash")])
            .filter(|p| p.is_file())
            // System32 的 bash.exe 是 WSL 的入口，不一定装了发行版
            .find(|p| {
                !p.to_string_lossy()
                    .to_lowercase()
                    .contains("windows\\system32")
            })
    }

    // 输出很多时缓存有上限；每段输出只扫描新增的部分，跨段的标记照样找到
    #[test]
    fn bounded_output_and_split_marks() {
        let run = TermRun::with_nonce("abc123", 1000, PROBE_WAIT);
        let feed_bytes = |text: &[u8]| {
            // 拆成单字节送入：每个标记都跨段
            for b in text {
                run.feed(std::slice::from_ref(b));
            }
        };
        feed_bytes(b"noise \x1b]6973;P;abc123;2\x07");
        assert_eq!(*run.phase.borrow(), Phase::Begin, "应找到探测标记");
        feed_bytes(b"echo ...\r\n\x1b]6973;B;abc123\x07");
        let chunk = b"0123456789".repeat(100);
        for _ in 0..5000 {
            run.feed(&chunk);
            let len = run.state.lock().unwrap().buf.len();
            assert!(
                len <= 2 * 1000 + chunk.len() + 64,
                "缓存应有上限，实际 {len}"
            );
        }
        feed_bytes(b"tail\x1b]6973;E;abc123;7\x07$ ");
        assert_eq!(*run.phase.borrow(), Phase::Done, "应找到结束标记");
        {
            let s = run.state.lock().unwrap();
            assert_eq!(s.exit, 7);
            assert_eq!(s.out.len(), 2000);
            assert!(s.truncated && s.out.starts_with(&chunk));
        }
        assert_eq!(run.result().output.len(), 1000);

        // 没有超过上限：完整保留
        let run = TermRun::with_nonce("abc123", 1000, PROBE_WAIT);
        run.feed(b"\x1b]6973;P;abc123;2\x07");
        run.feed(b"\x1b]6973;B;abc123\x07short\x1b]6973;E;abc123;0\x07");
        let result = run.result();
        assert_eq!(
            (result.output.as_str(), result.truncated, result.exit_code),
            ("short", false, 0)
        );
    }
}
