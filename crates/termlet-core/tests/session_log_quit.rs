//! 退出前停止全部会话日志（`session_log::stop_all`）。它停止的是整个进程里的日志，放在单独的测试进程中，
//! 不影响库里并行运行的其他会话日志测试。

use std::path::Path;
use std::time::Duration;

use termlet_core::session_log::{SessionLog, stop_all};

/// 日志的各行，去掉行首的时间（`[YYYY-MM-DD HH:MM:SS] `）。
fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| {
            assert!(line.starts_with('['), "每行都应带时间：{line}");
            line.split_once("] ")
                .map_or(line, |(_, rest)| rest)
                .to_string()
        })
        .collect()
}

// 还开着的终端的日志、窗口关闭时已经丢弃（后台线程可能还在写）的日志，退出前都写完最后的输出与“停止记录”
#[test]
fn stop_all_writes_out_every_log_before_quitting() {
    let tmp = tempfile::tempdir().unwrap();
    let open = SessionLog::start(tmp.path(), "web-1", |_| panic!("不应停下")).unwrap();
    open.write(b"last output\r\n");
    open.write(b"no newline yet");
    let dropped = SessionLog::start(tmp.path(), "web-2", |_| panic!("不应停下")).unwrap();
    // 2 MB：丢弃时后台线程多半还没写完
    let line = format!("{}\r\n", "x".repeat(1000));
    dropped.write(line.repeat(2000).as_bytes());
    dropped.write(b"written while closing\r\n");
    let dropped_path = dropped.path().to_path_buf();
    drop(dropped);

    assert!(stop_all(Duration::from_secs(10)), "都应在时限内写完");
    assert!(open.is_closed(), "之后的写入不再生效");
    assert_eq!(
        lines(open.path()),
        [
            "[Termlet] 开始记录 web-1",
            "last output",
            "no newline yet",
            "[Termlet] 停止记录",
        ]
    );
    let dropped = lines(&dropped_path);
    assert_eq!(dropped.len(), 2000 + 3, "开始、2000 行输出、最后一行与停止");
    assert_eq!(
        &dropped[dropped.len() - 2..],
        ["written while closing", "[Termlet] 停止记录"]
    );

    // 停止之后的写入不生效；再调用时没有要等的日志，立即返回
    open.write(b"late\r\n");
    assert!(stop_all(Duration::ZERO));
    assert_eq!(lines(open.path()).last().unwrap(), "[Termlet] 停止记录");
}
