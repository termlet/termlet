//! kitty 键盘协议经真实伪终端的集成测试：在伪终端中运行一个 Python 小程序，它先不开启协议读一次按键，再开启
//! 消歧义（`CSI > 1 u`）读一次，把收到的字节以十六进制打印出来，最后出栈。终端引擎解析它的输出，按引擎中的标志
//! 编码 Shift+Enter 写回伪终端。
//!
//! 需要 PATH 中有 Python 3；Windows 上还需要会把程序输出的序列转给终端的 ConPTY（Windows 11 24H2 起），
//! 旧的 ConPTY 会吞掉 `CSI > 1 u`。因此默认不运行：
//! `cargo test -p termlet-terminal --test kitty_pty -- --ignored`

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::Keystroke;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use termlet_terminal::keys::{AltKey, KeyEventKind, encode_key};
use termlet_terminal::{
    CloseReason, EngineOptions, GridSize, IoEvent, KeyboardFlags, TerminalEngine, TerminalInput,
    TerminalIo,
};

const SCRIPT: &str = r#"
import os, sys
if os.name == "nt":
    import ctypes
    k32 = ctypes.windll.kernel32
    # 输入按 VT 序列读取（不要行输入、回显与 Ctrl+C 处理），输出按 VT 序列处理
    k32.SetConsoleMode(k32.GetStdHandle(-10), 0x0200)
    handle = k32.GetStdHandle(-11)
    mode = ctypes.c_uint32()
    k32.GetConsoleMode(handle, ctypes.byref(mode))
    k32.SetConsoleMode(handle, mode.value | 0x0004 | 0x0008)
else:
    import tty
    tty.setraw(0)
out = sys.stdout.buffer
def say(data):
    out.write(data)
    out.flush()
def read_until(done):
    data = b""
    while not done(data) and len(data) < 256:
        chunk = os.read(0, 64)
        if not chunk:
            break
        data += chunk
    return data
say(b"READY1\r\n")
data = read_until(lambda d: b"\r" in d)
say(b"GOT1 " + data.hex().encode() + b"\r\n")
say(b"\x1b[>1u")
say(b"READY2\r\n")
data = read_until(lambda d: d.endswith(b"u"))
say(b"GOT2 " + data.hex().encode() + b"\r\n")
say(b"\x1b[<u")
say(b"DONE\r\n")
"#;

/// 写给伪终端。
struct PtyInput(Mutex<Box<dyn Write + Send>>);

impl TerminalInput for PtyInput {
    fn write(&self, data: &[u8]) {
        let mut writer = self.0.lock().unwrap();
        let _ = writer.write_all(data);
        let _ = writer.flush();
    }
    fn resize(&self, _: GridSize) {}
}

fn find_python() -> Option<&'static str> {
    ["python", "python3"].into_iter().find(|name| {
        std::process::Command::new(name)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    })
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 输出中 `label` 之后的十六进制数字（程序打印的收到的字节）。
fn hex_after(output: &[u8], label: &str) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let start = text.find(label)? + label.len();
    let hex: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    text[start + hex.len()..].starts_with('\r').then_some(hex)
}

#[test]
#[ignore = "需要 Python 3 与会转发序列的伪终端（见模块说明）"]
fn shift_enter_reaches_a_program_through_a_real_pty() {
    let Some(python) = find_python() else {
        eprintln!("PATH 中没有 Python 3，跳过");
        return;
    };
    let script = std::env::temp_dir().join(format!("termlet-kitty-pty-{}.py", std::process::id()));
    std::fs::write(&script, SCRIPT).unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(python);
    command.arg(&script);
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let (tx, rx) = async_channel::bounded(64);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0
                || tx
                    .send_blocking(IoEvent::Output(buf[..n].to_vec()))
                    .is_err()
            {
                break;
            }
        }
        let _ = tx.send_blocking(IoEvent::Closed(CloseReason::Exited(None)));
    });
    let engine = TerminalEngine::new(
        TerminalIo {
            output: rx,
            input: Arc::new(PtyInput(Mutex::new(writer))),
        },
        EngineOptions::default(),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    engine.set_output_observer(Some(Arc::new(move |bytes: &[u8]| {
        observed.lock().unwrap().extend_from_slice(bytes)
    })));
    let output = || seen.lock().unwrap().clone();
    let contains = |label: &str| String::from_utf8_lossy(&output()).contains(label);
    let shift_enter = Keystroke::parse("shift-enter").unwrap();
    let press = || {
        let mode = engine.modes();
        let key = encode_key(
            &shift_enter,
            mode,
            engine.keyboard_flags(),
            KeyEventKind::Press,
            AltKey::Meta,
        )
        .expect("Shift+Enter 应当编码");
        engine.write(&key.bytes);
    };

    // 没有开启协议：与回车相同
    wait_until("READY1", || contains("READY1"));
    assert_eq!(engine.keyboard_flags(), KeyboardFlags::NONE);
    press();
    let mut got = None;
    wait_until("GOT1", || {
        got = hex_after(&output(), "GOT1 ");
        got.is_some()
    });
    let got = got.unwrap();
    assert!(got.ends_with("0d"), "没有开启协议时收到 {got}");

    // 程序开启消歧义：Shift+Enter 为 CSI 13;2u
    wait_until("READY2 与开启协议", || {
        contains("READY2") && engine.keyboard_flags() == KeyboardFlags::DISAMBIGUATE
    });
    press();
    let mut got = None;
    wait_until("GOT2", || {
        got = hex_after(&output(), "GOT2 ");
        got.is_some()
    });
    let got = got.unwrap();
    assert!(
        got.ends_with("1b5b31333b3275"),
        "开启协议后收到 {got}，应以 ESC [13;2u 结尾"
    );

    // 程序出栈：恢复原来的编码
    wait_until("DONE 与关闭协议", || {
        contains("DONE") && engine.keyboard_flags().is_empty()
    });
    let _ = child.kill();
    let _ = std::fs::remove_file(&script);
}
