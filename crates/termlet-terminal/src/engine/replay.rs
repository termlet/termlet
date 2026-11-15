//! 回放录制的会话：引擎不接传输，输出与大小变化由调用方按录制的顺序送进来（[`ReplayFeed`]），在引擎线程上依次
//! 处理。输出走与实时终端相同的解析（shell 集成的扫描、同步更新的缓冲与超时），画面相同；终端的应答（光标位置
//! 报告、颜色查询等）丢弃。
//!
//! - 大小变化与输出在同一个队列里，按顺序生效（全屏程序收到 SIGWINCH 后的重绘落在新的大小上）。
//! - 连续的输出与实时终端一样，一批最多解析 1MB，之后通知界面重绘（合并发送）。
//! - 跳转回到前面时调用方另建一个回放引擎从头送进来，等 [`ReplayFeed::idle`] 后再换上，界面不会先闪一下空白；
//!   丢弃 [`ReplayFeed`] 时引擎线程不再处理队列中剩下的内容（换下的引擎、关闭的 Tab），引擎照常可读。
//! - 关键帧（[`ReplayFeed::checkpoint`]）：在队列中的这个位置把引擎的全部内容与状态写成控制序列
//!   （[`crate::dump::checkpoint`]），往前跳时新的引擎先解析它、再接着送之后的事件，不用从头重放。调用方要在
//!   控制序列的边界上要（不在转义序列中间），同步更新中途要的没有结果。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use termlet_io::{GridSize, TerminalInput};

use super::worker::Source;
use super::{EngineOptions, TerminalEngine};
use crate::dump::ScreenDump;

/// 回放的输入端：把录制中的输出与大小变化依次送给引擎。丢弃时引擎停止处理还没处理的部分。
pub struct ReplayFeed {
    tx: async_channel::Sender<Feed>,
    cancelled: Arc<AtomicBool>,
}

/// 送给回放引擎的一项。
pub(super) enum Feed {
    Output(Vec<u8>),
    Resize(GridSize),
    /// 之前的都处理完时通知
    Idle(async_channel::Sender<()>),
    /// 之前的都处理完时写一份关键帧
    Checkpoint(async_channel::Sender<Option<ScreenDump>>),
}

/// 回放引擎的应答没有去处。
struct Discard;

impl TerminalInput for Discard {
    fn write(&self, _: &[u8]) {}
    fn resize(&self, _: GridSize) {}
}

impl TerminalEngine {
    /// 回放用的引擎：网格开始时是 `options.size`（录制开始时的大小），之后由 [`ReplayFeed`] 送来的输出与大小变化决定，
    /// 不随界面的大小变化（视图要用回放模式，见 [`crate::TerminalView::set_replay`]）。
    pub fn replay(options: EngineOptions) -> (Self, ReplayFeed) {
        let (tx, rx) = async_channel::unbounded();
        let cancelled = Arc::new(AtomicBool::new(false));
        let source = Source::Replay {
            feed: rx,
            cancelled: cancelled.clone(),
        };
        let engine = Self::start("terminal-replay", Arc::new(Discard), &options, source);
        (engine, ReplayFeed { tx, cancelled })
    }
}

impl ReplayFeed {
    /// 送一段输出。
    pub fn output(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.tx.try_send(Feed::Output(bytes));
        }
    }

    /// 终端大小变了（录制中的 `r` 事件）。
    pub fn resize(&self, columns: u16, rows: u16) {
        let _ = self
            .tx
            .try_send(Feed::Resize(GridSize::new(columns.max(1), rows.max(1))));
    }

    /// 之前送进来的都处理完（解析进网格）时完成；引擎已停止时也完成。
    pub fn idle(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let (done, wait) = async_channel::bounded(1);
        let _ = self.tx.try_send(Feed::Idle(done));
        async move {
            let _ = wait.recv().await;
        }
    }

    /// 之前送进来的都处理完后的关键帧：在新的回放引擎（这个大小、同样的回滚上限）中送进它，再接着送之后的输出，结果
    /// 与这个引擎相同。同步更新（DEC 2026）中途、引擎已停止时为 `None`。
    pub fn checkpoint(
        &self,
    ) -> impl std::future::Future<Output = Option<ScreenDump>> + Send + 'static {
        let (done, wait) = async_channel::bounded(1);
        let _ = self.tx.try_send(Feed::Checkpoint(done));
        async move { wait.recv().await.ok().flatten() }
    }

    /// 阻塞到之前送进来的都处理完（测试与基准用；界面用 [`Self::idle`]）。
    pub fn wait_idle(&self) {
        futures::executor::block_on(self.idle());
    }
}

impl Drop for ReplayFeed {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EngineEvent;
    use crate::shell_events::ShellEvent;

    fn replay(columns: u16, rows: u16) -> (TerminalEngine, ReplayFeed) {
        TerminalEngine::replay(EngineOptions {
            scrollback: 100,
            size: GridSize::new(columns, rows),
        })
    }

    // 输出与大小变化按顺序生效：大小变化之后的输出落在新的大小上
    #[test]
    fn output_and_resize_apply_in_order() {
        let (engine, feed) = replay(10, 3);
        feed.output(b"0123456789AB".to_vec());
        feed.resize(20, 3);
        feed.output(b"\x1b[H\x1b[2J0123456789ABCDEFGHIJ".to_vec());
        feed.wait_idle();
        assert_eq!(engine.size(), GridSize::new(20, 3));
        assert_eq!(engine.dimensions().columns, 20);
        assert_eq!(
            engine.screen_text().lines().next(),
            Some("0123456789ABCDEFGHIJ")
        );
        // 界面收到重绘通知
        assert!(matches!(
            engine.events().try_recv(),
            Ok(EngineEvent::Wakeup)
        ));
    }

    // 终端的应答丢弃（没有传输），也不会卡住
    #[test]
    fn replies_are_discarded() {
        let (engine, feed) = replay(10, 3);
        feed.output(b"\x1b[6n\x1b[c\x1b]11;?\x07ok".to_vec());
        feed.wait_idle();
        assert_eq!(engine.screen_text().trim(), "ok");
    }

    // 丢弃输入端后不再处理队列中剩下的内容，线程结束；等待也不会卡住
    #[test]
    fn dropping_the_feed_stops_the_worker() {
        let (engine, feed) = replay(10, 3);
        let idle = feed.idle();
        drop(feed);
        futures::executor::block_on(idle);
        drop(engine);
    }

    // 关键帧：新的回放引擎送进它、再接着送同样的输出，与一直放下来的相同；同步更新中途要的没有结果
    #[test]
    fn checkpoint_restores_into_a_new_engine() {
        let (engine, feed) = replay(12, 4);
        for i in 0..20 {
            feed.output(format!("\x1b[3{}mline {i}\r\n", i % 8).into_bytes());
        }
        feed.output(b"\x1b[2;3r\x1b[1mhalf".to_vec());
        let frame = futures::executor::block_on(feed.checkpoint()).expect("关键帧");
        let (copy, copy_feed) = replay(frame.columns, frame.rows);
        copy_feed.output(frame.text.into_bytes());
        let rest = b"\x1b[3;1H\nscrolled\x1b[r\r\nend".to_vec();
        feed.output(rest.clone());
        copy_feed.output(rest);
        feed.wait_idle();
        copy_feed.wait_idle();
        assert_eq!(copy.screen_text(), engine.screen_text());
        assert_eq!(
            copy.dimensions().history_size,
            engine.dimensions().history_size
        );
        assert_eq!(copy.cursor_point(), engine.cursor_point());

        feed.output(b"\x1b[?2026hpending".to_vec());
        assert_eq!(futures::executor::block_on(feed.checkpoint()), None);
    }

    /// 取走引擎送出的 shell 集成事件（命令开始时读出的命令行、结束时的退出码等；耗时不比）。
    fn shell_events(engine: &TerminalEngine) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(event) = engine.events().try_recv() {
            if let EngineEvent::Shell(event) = event {
                seen.push(match event {
                    ShellEvent::CommandFinished { exit_code, .. } => format!("D {exit_code:?}"),
                    other => format!("{other:?}"),
                });
            }
        }
        seen
    }

    // 关键帧还原超链接、下划线颜色与 shell 集成记下的命令位置：从关键帧接着送与一直放下来的内容、状态相同，之后的
    // 命令照样读得出来（输入到一半、输入开始处滚进了回滚、命令在执行、633;E 给出的命令行、提示符开始之后 B 之前、
    // 在备用屏幕中时主屏幕上的输入开始处）
    #[test]
    fn checkpoint_keeps_links_and_command_marks() {
        let long = format!("\x1b]133;A\x07$ \x1b]133;B\x07{}", "x".repeat(40));
        let cases: [(&str, &str); 7] = [
            (
                "\x1b]133;A\x07$ \x1b]133;B\x07ls -l",
                "a\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07",
            ),
            (&long, "y\x1b]133;C\x07\r\n"),
            (
                "\x1b]133;A\x07$ \x1b]133;B\x07sleep 1\x1b]133;C\x07\r\n",
                "done\r\n\x1b]133;D;3\x07\x1b]133;A\x07$ ",
            ),
            (
                "\x1b]633;A\x07$ \x1b]633;B\x07echo a;b\x1b]633;E;echo a\\x3bb\x07",
                "\x1b]633;C\x07\r\na\r\n",
            ),
            (
                "\x1b]133;A\x07$ \x1b]133;B\x07x\x1b]133;A\x07$ ",
                "\x1b]133;B\x07pwd\x1b]133;C\x07\r\n",
            ),
            (
                "\x1b]133;A\x07$ \x1b]133;B\x07vim\x1b[?1049h\x1b[2;2Hediting",
                "\x1b[?1049l\x1b]133;C\x07\r\n",
            ),
            (
                "\x1b]8;;https://x.example\x07link\x1b]8;;\x07 \x1b[4;58;5;9mu\x1b]8;id=k;https://y\x07open",
                "more\x1b]8;;\x07\x1b[59m end",
            ),
        ];
        for (before, after) in cases {
            let (engine, feed) = replay(10, 3);
            feed.output(before.as_bytes().to_vec());
            let frame = futures::executor::block_on(feed.checkpoint()).expect("关键帧");
            let (copy, copy_feed) = replay(frame.columns, frame.rows);
            copy_feed.output(frame.text.clone().into_bytes());
            copy_feed.wait_idle();
            shell_events(&engine);
            shell_events(&copy);
            let label = format!("{before:?} 的关键帧 {:?}", frame.text);
            assert_eq!(copy.shell_state(), engine.shell_state(), "{label}");
            assert_eq!(
                copy.cells_with_extras(),
                engine.cells_with_extras(),
                "{label}"
            );

            feed.output(after.as_bytes().to_vec());
            copy_feed.output(after.as_bytes().to_vec());
            feed.wait_idle();
            copy_feed.wait_idle();
            let events = shell_events(&engine);
            assert_eq!(shell_events(&copy), events, "{label}");
            assert_eq!(copy.shell_state(), engine.shell_state(), "{label}");
            assert_eq!(
                copy.cells_with_extras(),
                engine.cells_with_extras(),
                "{label}"
            );
            assert_eq!(copy.screen_text(), engine.screen_text(), "{label}");
        }
    }

    // 回放中的屏幕快照：与实时终端的写法相同，回放从录制开始时的画面接上
    #[test]
    fn screen_dump_of_a_replay() {
        let (engine, feed) = replay(10, 3);
        feed.output(b"\x1b[31mhi\x1b[0m\r\nthere".to_vec());
        feed.wait_idle();
        let dump = engine.screen_dump_then(|dump| dump);
        assert_eq!((dump.columns, dump.rows), (10, 3));
        let (copy, copy_feed) = replay(10, 3);
        copy_feed.output(dump.text.into_bytes());
        copy_feed.wait_idle();
        assert_eq!(copy.screen_text(), engine.screen_text());
    }
}
