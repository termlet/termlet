//! 终端视图的 UI 集成测试：无头窗口中渲染真实的视图，经真实的事件路径输入，检查发给传输的字节与网格内容。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, ClipboardItem, Entity, TestAppContext,
    VisualTestContext, WindowBounds, WindowOptions, WindowVisibility, px, size,
};
use termlet_terminal::view::CURSOR_BLINK_INTERVAL;
use termlet_terminal::{
    CloseReason, CommandSuggestions, CursorShape, EngineOptions, GridSize, HighlightColor,
    HighlightRuleSpec, Highlighter, IoEvent, MouseTracking, SearchQuery, ShellEvent,
    TerminalEngine, TerminalInput, TerminalIo, TerminalStyle, TerminalView, TerminalViewEvent,
};

/// 记下终端写给传输的全部字节。
#[derive(Default)]
struct Recorder {
    written: Mutex<Vec<u8>>,
    size: Mutex<Option<GridSize>>,
}

impl TerminalInput for Recorder {
    fn write(&self, data: &[u8]) {
        self.written.lock().unwrap().extend_from_slice(data);
    }
    fn resize(&self, size: GridSize) {
        *self.size.lock().unwrap() = Some(size);
    }
}

impl Recorder {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.written.lock().unwrap())
    }
}

struct Fixture {
    window: AnyWindowHandle,
    view: Entity<TerminalView>,
    recorder: Arc<Recorder>,
    output: async_channel::Sender<IoEvent>,
}

fn open(cx: &mut TestAppContext) -> Fixture {
    open_with(cx, TerminalStyle::default())
}

fn open_with(cx: &mut TestAppContext, style: TerminalStyle) -> Fixture {
    // 引擎线程是真实的系统线程，会在测试调度器运行时唤醒界面的任务
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(termlet_terminal::init);
    let (output, rx) = async_channel::bounded(16);
    let recorder = Arc::new(Recorder::default());
    let io = TerminalIo {
        output: rx,
        input: recorder.clone(),
    };
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(800.), px(480.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let engine = TerminalEngine::new(io, EngineOptions::default());
                cx.new(|cx| TerminalView::new(engine, style, window, cx))
            },
        )
        .expect("打开测试窗口")
    });
    Fixture {
        window,
        view,
        recorder,
        output,
    }
}

#[gpui_kit::test]
fn typing_goes_to_the_transport(cx: &mut TestAppContext) {
    let fixture = open(cx);
    // 第一次渲染时按窗口大小调整网格
    cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        assert_eq!(window.find("terminal").focused(), Some(true));
        fixture.recorder.take();

        window.input("ls -la", cx);
        window.press("enter", cx);
        assert_eq!(fixture.recorder.take(), b"ls -la\r");

        window.press("ctrl-c", cx);
        assert_eq!(fixture.recorder.take(), b"\x03");

        window.press("up", cx);
        assert_eq!(fixture.recorder.take(), b"\x1b[A");

        window.input("中文", cx);
        assert_eq!(fixture.recorder.take(), "中文".as_bytes());
    })
    .unwrap();
    let size = fixture
        .recorder
        .size
        .lock()
        .unwrap()
        .expect("应已按窗口调整大小");
    assert!(size.columns > 40 && size.rows > 10, "网格大小：{size:?}");
}

// gpui-kit 的 Root 把 Tab / Shift+Tab 绑给了切换焦点，快捷键又先于按键监听器分发：焦点在终端上时这两个键
// 要交给终端（shell 补全等）；会话结束后终端不用它们，照旧由 Root 切换焦点
#[gpui_kit::test]
fn tab_goes_to_the_terminal_until_the_session_ends(cx: &mut TestAppContext) {
    let fixture = open(cx);
    // 被快捷键用掉的按键与动作（交给终端的按键不经过快捷键，不会出现在这里）
    let actions = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let _observer = cx.update(|cx| {
        let actions = actions.clone();
        cx.observe_keystrokes(move |event, _, _| {
            if let Some(action) = &event.action {
                actions.lock().unwrap().push(action.name());
            }
        })
    });
    cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        fixture.recorder.take();

        window.press("tab", cx);
        assert_eq!(fixture.recorder.take(), b"\t", "Tab 发给远端");
        window.press("shift-tab", cx);
        assert_eq!(fixture.recorder.take(), b"\x1b[Z", "Shift+Tab 发给远端");
        assert_eq!(window.find("terminal").focused(), Some(true));
    })
    .unwrap();
    assert!(
        actions.lock().unwrap().is_empty(),
        "Tab 被快捷键用掉了：{:?}",
        actions.lock().unwrap()
    );

    fixture
        .output
        .send_blocking(IoEvent::Closed(CloseReason::Exited(Some(0))))
        .unwrap();
    eventually(cx, |cx| {
        fixture
            .view
            .read_with(cx, |view, _| view.closed().is_some())
    });
    cx.update_window(fixture.window, |_, window, cx| {
        window.press("tab", cx);
        window.press("shift-tab", cx);
    })
    .unwrap();
    assert!(fixture.recorder.take().is_empty(), "会话结束后不再写给传输");
    assert_eq!(*actions.lock().unwrap(), ["root::Tab", "root::TabPrev"]);
}

#[gpui_kit::test]
fn paste_uses_bracketed_mode_and_strips_controls(cx: &mut TestAppContext) {
    let fixture = open(cx);
    cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        fixture.recorder.take();
        cx.write_to_clipboard(ClipboardItem::new_string("echo a\x1b[201~\nrm".into()));
        window.press("ctrl-shift-v", cx);
        // 没有开启括号粘贴：换行变回车，夹带的转义序列被去掉
        assert_eq!(fixture.recorder.take(), b"echo a[201~\rrm");
    })
    .unwrap();
}

#[gpui_kit::test]
fn right_click_pastes_through_the_paste_action(cx: &mut TestAppContext) {
    let fixture = open(cx);
    let pasted = cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        fixture.recorder.take();
        cx.write_to_clipboard(ClipboardItem::new_string("whoami".into()));
        // 没有选中内容：右键粘贴（与快捷键一样走 Paste 动作，上层可以先拦下检查）
        window.right_click("terminal", cx);
        fixture.recorder.take()
    });
    let pasted = [pasted.unwrap(), fixture.recorder.take()].concat();
    assert_eq!(pasted, b"whoami");
}

/// 推进测试时钟一拍光标闪烁的间隔，返回光标这一拍画不画出。
fn blink_step(cx: &mut TestAppContext, view: &Entity<TerminalView>) -> bool {
    cx.executor().advance_clock(CURSOR_BLINK_INTERVAL);
    cx.run_until_parked();
    view.read_with(cx, |view, _| view.cursor_shown())
}

// 光标闪烁：默认不闪；默认样式改为闪烁后，终端获得焦点、窗口在前台时亮灭交替，按键后常亮一拍再闪；窗口到后台、
// 不可见、终端失焦时停止并常亮，回来后接着闪；程序设置稳定的光标（DECSCUSR）时不闪，`CSI 0 SP q` 后回到默认的
#[gpui_kit::test]
fn cursor_blinks_only_while_the_terminal_is_attended(cx: &mut TestAppContext) {
    let fixture = open(cx);
    let view = fixture.view.clone();
    let blinking = |cx: &mut TestAppContext| view.read_with(cx, |view, _| view.cursor_blinking());
    let shown = |cx: &mut TestAppContext| view.read_with(cx, |view, _| view.cursor_shown());
    cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.activate_window();
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window, |_, window, _| {
        assert!(window.is_window_active())
    })
    .unwrap();
    assert!(!blinking(cx), "默认不闪");
    assert!(blink_step(cx, &view));

    view.update(cx, |view, cx| {
        view.set_style(
            TerminalStyle {
                cursor_shape: CursorShape::Bar,
                cursor_blink: true,
                ..TerminalStyle::default()
            },
            cx,
        )
    });
    assert!(blinking(cx));
    assert!(!blink_step(cx, &view), "灭");
    assert!(blink_step(cx, &view), "亮");
    assert!(!blink_step(cx, &view), "灭");
    // 按键：马上亮，下一拍仍亮，再下一拍才灭
    cx.update_window(fixture.window, |_, window, cx| window.input("a", cx))
        .unwrap();
    assert!(shown(cx));
    assert!(blink_step(cx, &view), "按键后保持一拍");
    assert!(!blink_step(cx, &view));

    // 窗口到后台：停止并常亮；回到前台接着闪
    VisualTestContext::from_window(fixture.window, cx).deactivate_window();
    assert!(!blinking(cx));
    assert!(shown(cx));
    assert!(blink_step(cx, &view));
    cx.update_window(fixture.window, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
    assert!(blinking(cx));

    // 窗口不可见（最小化、隐藏到托盘）：停止
    cx.simulate_window_visibility_change(fixture.window, WindowVisibility::Hidden);
    cx.run_until_parked();
    assert!(!blinking(cx) && shown(cx));
    cx.simulate_window_visibility_change(fixture.window, WindowVisibility::Visible);
    cx.run_until_parked();
    assert!(blinking(cx));

    // 程序设置稳定的方块光标：不闪；`CSI 0 SP q` 回到默认的闪烁竖线
    fixture
        .output
        .send_blocking(IoEvent::Output(b"\x1b[2 q".to_vec()))
        .unwrap();
    eventually(cx, |cx| !blinking(cx));
    fixture
        .output
        .send_blocking(IoEvent::Output(b"\x1b[0 q".to_vec()))
        .unwrap();
    eventually(cx, |cx| blinking(cx));

    // 终端失焦：停止
    cx.update_window(fixture.window, |_, window, cx| window.blur(cx))
        .unwrap();
    cx.run_until_parked();
    assert!(!blinking(cx) && shown(cx));
}

#[gpui_kit::test]
fn output_is_rendered_and_copyable(cx: &mut TestAppContext) {
    let fixture = open(cx);
    cx.update_window(fixture.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    fixture
        .output
        .send_blocking(IoEvent::Output(b"hello \x1b[1mworld\x1b[0m".to_vec()))
        .unwrap();
    // 解析在后台线程进行，等它写进网格
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = cx.update(|cx| fixture.view.read(cx).screen_text());
        if text.starts_with("hello world") {
            break;
        }
        assert!(Instant::now() < deadline, "输出没有出现在网格中");
        std::thread::sleep(Duration::from_millis(10));
    }
    cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.press("ctrl-shift-a", cx);
        window.press("ctrl-shift-c", cx);
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap();
        assert!(copied.starts_with("hello world"), "复制到：{copied:?}");
    })
    .unwrap();
}

/// 等到条件成立：解析在真实的后台线程中进行，视图的任务在测试调度器中运行。
fn eventually(cx: &mut TestAppContext, mut condition: impl FnMut(&mut TestAppContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        if condition(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "等待超时");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[gpui_kit::test]
fn shell_integration_events_reach_the_view(cx: &mut TestAppContext) {
    let fixture = open(cx);
    let events = Arc::new(Mutex::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let events = events.clone();
        cx.subscribe(&fixture.view, move |_, event: &TerminalViewEvent, _| {
            if let TerminalViewEvent::Shell(event) = event {
                events.lock().unwrap().push(event.clone());
            }
        })
    });
    cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
    })
    .unwrap();
    let send = |text: &str| {
        fixture
            .output
            .send_blocking(IoEvent::Output(text.as_bytes().to_vec()))
            .unwrap()
    };
    send("\x1b]133;A\x07$ \x1b]133;B\x07");
    // 用户经真实的键盘路径输入（命令历史据此知道"用户输入过"），远端回显
    cx.update_window(fixture.window, |_, window, cx| window.input("ls", cx))
        .unwrap();
    send("ls\r\n\x1b]133;C\x07a.txt\r\n\x1b]133;D;0\x07\x1b]7;file://h/tmp\x07");
    eventually(cx, |_| {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ShellEvent::WorkingDirectory(_)))
    });
    let events = events.lock().unwrap().clone();
    assert_eq!(events[0], ShellEvent::PromptStart);
    assert!(
        events.contains(&ShellEvent::CommandEntered(termlet_terminal::CommandText(
            "ls".into()
        )))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        ShellEvent::CommandFinished {
            exit_code: Some(0),
            ..
        }
    )));
    cx.update(|cx| {
        let view = fixture.view.read(cx);
        assert!(view.has_shell_integration());
        assert_eq!(view.working_directory(), Some("/tmp"));
        assert!(!view.bracketed_paste());
    });
    send("\x1b[?2004h");
    eventually(cx, |cx| {
        fixture.view.read_with(cx, |view, _| view.bracketed_paste())
    });
}

#[gpui_kit::test]
fn kitty_keyboard_protocol_changes_key_encoding_only_while_enabled(cx: &mut TestAppContext) {
    let fixture = open(cx);
    let press = |cx: &mut TestAppContext, key: &str| {
        cx.update_window(fixture.window, |_, window, cx| window.press(key, cx))
            .unwrap();
        fixture.recorder.take()
    };
    cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
    })
    .unwrap();
    fixture.recorder.take();
    // 没有开启时与原来相同
    assert_eq!(press(cx, "shift-enter"), b"\r");
    assert_eq!(press(cx, "ctrl-c"), b"\x03");
    assert_eq!(press(cx, "escape"), b"\x1b");

    // 程序开启消歧义、报告事件类型与替代键（Codex 用的 crossterm 是 0b111），并查询
    let send = |bytes: &[u8]| {
        fixture
            .output
            .send_blocking(IoEvent::Output(bytes.to_vec()))
            .unwrap()
    };
    send(b"\x1b[>7u\x1b[?u");
    eventually(cx, |_| {
        fixture
            .recorder
            .written
            .lock()
            .unwrap()
            .ends_with(b"\x1b[?7u")
    });
    fixture.recorder.take();
    // 回车、Tab、退格不报告松开；其余用转义码报告的键报告松开
    assert_eq!(press(cx, "shift-enter"), b"\x1b[13;2u");
    assert_eq!(press(cx, "enter"), b"\r");
    assert_eq!(press(cx, "shift-tab"), b"\x1b[9;2u");
    assert_eq!(press(cx, "tab"), b"\t");
    assert_eq!(press(cx, "ctrl-c"), b"\x1b[99;5u\x1b[99;5:3u");
    assert_eq!(press(cx, "escape"), b"\x1b[27u\x1b[27;1:3u");
    assert_eq!(press(cx, "up"), b"\x1b[A\x1b[1;1:3A");
    cx.update_window(fixture.window, |_, window, cx| window.input("a", cx))
        .unwrap();
    assert_eq!(fixture.recorder.take(), b"a");

    // 程序退出前出栈：恢复原来的编码
    send(b"\x1b[<u");
    eventually(cx, |cx| {
        fixture
            .view
            .read_with(cx, |view, _| view.engine().keyboard_flags().is_empty())
    });
    assert_eq!(press(cx, "shift-enter"), b"\r");
    assert_eq!(press(cx, "ctrl-c"), b"\x03");
    assert_eq!(press(cx, "up"), b"\x1b[A");
}

#[gpui_kit::test]
fn mouse_motion_is_reported_once_per_cell_and_only_when_hovered(cx: &mut TestAppContext) {
    let fixture = open(cx);
    cx.update_window(fixture.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    // 程序开启所有移动的上报（1003）与 SGR 编码
    fixture
        .output
        .send_blocking(IoEvent::Output(b"\x1b[?1003h\x1b[?1006h".to_vec()))
        .unwrap();
    eventually(cx, |cx| {
        fixture.view.read_with(cx, |view, _| {
            view.engine().modes().mouse_tracking == MouseTracking::Motion
        })
    });
    fixture.recorder.take();
    let reports = |bytes: &[u8]| String::from_utf8_lossy(bytes).matches("\x1b[<35;").count();
    cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.simulate_mouse_move(gpui_kit::point(px(100.), px(100.)), cx);
        // 同一格内的移动不再上报
        window.simulate_mouse_move(gpui_kit::point(px(101.), px(101.)), cx);
        assert_eq!(reports(&fixture.recorder.take()), 1);
        window.simulate_mouse_move(gpui_kit::point(px(200.), px(200.)), cx);
        assert_eq!(reports(&fixture.recorder.take()), 1);
        // 指针不在这个终端上（例如在另一个分屏、侧边栏上）时不上报
        window.simulate_mouse_move(gpui_kit::point(px(2000.), px(2000.)), cx);
        assert_eq!(fixture.recorder.take(), b"");
        // 回到终端上，即使是同一格也重新上报
        window.simulate_mouse_move(gpui_kit::point(px(200.), px(200.)), cx);
        assert_eq!(reports(&fixture.recorder.take()), 1);
    })
    .unwrap();
}

/// 一屏带关键词的日志。
fn log_screen(rows: usize) -> String {
    (0..rows)
        .map(|i| match i % 3 {
            0 => format!("12:00:{i:02} ERROR 连接 10.0.0.{i} 被拒绝 (connection refused)\r\n"),
            1 => format!("12:00:{i:02} WARN deprecated option, request ok\r\n"),
            _ => format!("12:00:{i:02} INFO 服务启动完成 path=/api/v1/hosts/{i}\r\n"),
        })
        .collect()
}

fn builtin_highlighter() -> Arc<Highlighter> {
    let rule = |pattern: &str, color| HighlightRuleSpec {
        pattern: pattern.into(),
        regex: true,
        case_sensitive: false,
        color,
        enabled: true,
    };
    let (highlighter, errors) = Highlighter::new(&[
        rule(r"\b(error|fail|failed|refused)\b", HighlightColor::Red),
        rule(r"\b(warn|deprecated)\b", HighlightColor::Yellow),
        rule(r"\b(ok|success)\b", HighlightColor::Green),
        rule(r"\b(?:\d{1,3}\.){3}\d{1,3}\b", HighlightColor::Cyan),
    ]);
    assert!(errors.is_empty());
    Arc::new(highlighter)
}

#[gpui_kit::test]
fn keyword_highlights_render(cx: &mut TestAppContext) {
    let style = TerminalStyle {
        highlighter: Some(builtin_highlighter()),
        ..TerminalStyle::default()
    };
    let fixture = open_with(cx, style);
    cx.update_window(fixture.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    fixture
        .output
        .send_blocking(IoEvent::Output(log_screen(40).into_bytes()))
        .unwrap();
    eventually(cx, |cx| {
        cx.update(|cx| {
            let view = fixture.view.read(cx);
            view.engine().dimensions().history_size > 0
        })
    });
    // 渲染（含高亮）与切换样式都不出错
    cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update(|cx| {
        fixture.view.update(cx, |view, cx| {
            let style = TerminalStyle {
                highlighter: None,
                ..view.style().clone()
            };
            view.set_style(style, cx);
        })
    });
    cx.update_window(fixture.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

// 设置中改小了回滚的行数：截掉的历史中的查找结果随之去掉，当前那一处不变
#[gpui_kit::test]
fn shrinking_scrollback_drops_search_matches_in_removed_history(cx: &mut TestAppContext) {
    let fixture = open(cx);
    cx.update_window(fixture.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    let lines: String = (0..300).map(|i| format!("match {i}\r\n")).collect();
    fixture
        .output
        .send_blocking(IoEvent::Output(lines.into_bytes()))
        .unwrap();
    // 回滚的行数与屏幕的行数
    let grid = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let dimensions = fixture.view.read(cx).engine().dimensions();
            (dimensions.history_size, dimensions.screen_lines)
        })
    };
    // 最后一行是光标所在的空行
    eventually(cx, |cx| {
        let (history, rows) = grid(cx);
        history + rows - 1 == 300
    });
    let query = SearchQuery {
        text: "match".into(),
        ..SearchQuery::default()
    };
    let summary = cx
        .update(|cx| {
            fixture
                .view
                .update(cx, |view, cx| view.set_search(Some(query), cx))
        })
        .unwrap();
    assert_eq!((summary.current, summary.total), (300, 300));

    cx.update(|cx| {
        fixture
            .view
            .update(cx, |view, cx| view.set_scrollback(100, cx))
    });
    let (history, rows) = grid(cx);
    assert_eq!(history, 100);
    let summary = cx.update(|cx| fixture.view.read(cx).search_summary());
    assert_eq!(
        summary.total,
        100 + rows - 1,
        "只剩回滚中的 100 行与屏幕上的"
    );
    assert_eq!(summary.current, summary.total, "当前仍是最近的那一处");
    cx.update_window(fixture.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// 满屏的彩色文字：256 色的前景与背景，粗体、斜体、下划线，中英文、emoji（含 ZWJ 组合的）混排，长行自动换行。
/// 最后一行是 `END`。
fn colorful_screen(rows: usize) -> String {
    let words = [
        "status=200",
        "服务启动完成",
        "😀",
        "path=/api/v1",
        "中文宽字符",
        "👨‍👩‍👧",
        "ok",
        "✅done",
    ];
    let mut text: String = (0..rows)
        .map(|i| {
            let mut line = String::new();
            for j in 0..24 {
                let n = (i * 24 + j) % 256;
                let attr = match j % 4 {
                    0 => "1",
                    1 => "3",
                    2 => "4",
                    _ => "22;23;24",
                };
                let word = words[(i + j) % words.len()];
                line.push_str(&format!(
                    "\x1b[{attr};38;5;{n};48;5;{}m{word} ",
                    (n + 128) % 256
                ));
            }
            line.push_str("\x1b[0m\r\n");
            line
        })
        .collect();
    text.push_str("END");
    text
}

/// 渲染的测量：`cargo test -p termlet-terminal --release --test view render_frame_time -- --ignored --nocapture`
///
/// 每帧包括取可见区的快照、关键词高亮、排版与绘制（测试平台的文字系统不真正排字形）。
#[gpui_kit::test]
#[ignore = "耗时测量，手动运行"]
fn render_frame_time(cx: &mut TestAppContext) {
    let mut results = Vec::new();
    let screens = [
        ("日志", format!("{}END", log_screen(60))),
        ("彩色宽字符 emoji", colorful_screen(60)),
    ];
    for (name, screen) in &screens {
        for highlighter in [None, Some(builtin_highlighter())] {
            let enabled = highlighter.is_some();
            let style = TerminalStyle {
                highlighter,
                ..TerminalStyle::default()
            };
            let fixture = open_with(cx, style);
            cx.update_window(fixture.window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            fixture
                .output
                .send_blocking(IoEvent::Output(screen.clone().into_bytes()))
                .unwrap();
            eventually(cx, |cx| {
                fixture
                    .view
                    .read_with(cx, |view, _| view.screen_text().ends_with("END"))
            });
            let frames = 300;
            let started = Instant::now();
            cx.update_window(fixture.window, |_, window, cx| {
                for _ in 0..frames {
                    window.render_frame(cx);
                }
            })
            .unwrap();
            let per_frame = started.elapsed().as_secs_f64() * 1000.0 / frames as f64;
            results.push((name, enabled, per_frame));
        }
    }
    for (name, enabled, ms) in results {
        println!(
            "{name}，关键词高亮{}：每帧 {ms:.3} ms",
            if enabled { "开" } else { "关" }
        );
    }
}

/// 命令补全提示的开销：满屏日志、最底下是提示符与正在输入的命令，1000 条历史，提示关闭与正在提示时每帧的耗时。
/// `cargo test -p termlet-terminal --release --test view render_frame_time_with_suggestion -- --ignored --nocapture`
#[gpui_kit::test]
#[ignore = "耗时测量，手动运行"]
fn render_frame_time_with_suggestion(cx: &mut TestAppContext) {
    let history = CommandSuggestions::new(
        (0..1000)
            .map(|i| format!("tail -f /var/log/app-{i}.log"))
            .chain(["git status --short".to_string()]),
    );
    let history = Arc::new(history);
    let mut results = Vec::new();
    for enabled in [false, true] {
        let fixture = open(cx);
        if enabled {
            let history = history.clone();
            cx.update(|cx| {
                fixture.view.update(cx, |view, cx| {
                    view.set_command_suggestions(Some(history), cx)
                })
            });
        }
        cx.update_window(fixture.window, |_, window, cx| {
            window.render_frame(cx);
            window.click("terminal", cx);
        })
        .unwrap();
        echo(
            &fixture,
            &format!("{}\x1b]133;A\x07$ \x1b]133;B\x07", log_screen(60)),
        );
        eventually(cx, |cx| {
            fixture
                .view
                .read_with(cx, |view, _| view.has_shell_integration())
        });
        cx.update_window(fixture.window, |_, window, cx| window.input("git s", cx))
            .unwrap();
        echo(&fixture, "git s");
        eventually(cx, |cx| {
            let screen = fixture.view.read_with(cx, |view, _| view.screen_text());
            screen.lines().any(|l| l.ends_with("$ git s"))
        });
        if enabled {
            assert_eq!(suggestion(cx, &fixture).as_deref(), Some("tatus --short"));
        }
        let frames = 300;
        let started = Instant::now();
        cx.update_window(fixture.window, |_, window, cx| {
            for _ in 0..frames {
                window.render_frame(cx);
            }
        })
        .unwrap();
        results.push((
            enabled,
            started.elapsed().as_secs_f64() * 1000.0 / frames as f64,
        ));
    }
    for (enabled, ms) in results {
        println!(
            "日志与提示符，命令补全提示{}：每帧 {ms:.3} ms",
            if enabled { "正在提示" } else { "关" }
        );
    }
}

// 用户的键入交给输入观察者（会话录制同时录制输入时），终端自己的应答不经过它
#[gpui_kit::test]
fn input_observer_sees_typing_but_not_replies(cx: &mut TestAppContext) {
    let fixture = open(cx);
    let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = seen.clone();
    cx.update(|cx| {
        fixture.view.update(cx, |view, _| {
            view.set_input_observer(Some(Arc::new(move |bytes: &[u8]| {
                sink.lock().unwrap().extend_from_slice(bytes)
            })))
        })
    });
    cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.input("pwd", cx);
        window.press("enter", cx);
    })
    .unwrap();
    // 光标位置报告：终端的应答直接写给传输
    fixture
        .output
        .send_blocking(IoEvent::Output(b"\x1b[6n".to_vec()))
        .unwrap();
    eventually(cx, |_| {
        String::from_utf8_lossy(&fixture.recorder.written.lock().unwrap()).contains('R')
    });
    assert_eq!(*seen.lock().unwrap(), b"pwd\r");
}

/// 远端输出一段文字（shell 的回显、提示符）；解析在后台线程进行，之后用 `eventually` 等结果。
fn echo(fixture: &Fixture, text: &str) {
    fixture
        .output
        .send_blocking(IoEvent::Output(text.as_bytes().to_vec()))
        .unwrap();
}

/// 现在给出的命令补全提示。
fn suggestion(cx: &mut TestAppContext, fixture: &Fixture) -> Option<String> {
    fixture
        .view
        .read_with(cx, |view, _| view.command_suggestion())
}

// 命令补全提示：在 shell 提示符后输入时按历史给出；→ / End 接受整条、Ctrl+→（macOS ⌥→）接受一个词，作为键入发给
// 远端；回显到达之前连按不会重复发送；没有提示时这些键照常发给远端；Esc 不拦截；关掉后没有
#[gpui_kit::test]
fn command_suggestions_are_accepted_as_typing(cx: &mut TestAppContext) {
    let fixture = open(cx);
    let history = CommandSuggestions::new(
        ["git status --short", "git stash", "echo 中文测试"].map(String::from),
    );
    cx.update(|cx| {
        fixture.view.update(cx, |view, cx| {
            view.set_command_suggestions(Some(Arc::new(history)), cx)
        })
    });
    let os = cx.update(|cx| cx.platform_info().os);
    let word = termlet_terminal::suggest::accept_word_combo(os);
    // 没有提示时这个键照常编码（Ctrl+→ 为 CSI 1;5 C，⌥→ 为 CSI 1;3 C）
    let word_key: &[u8] = if word.starts_with("alt") {
        b"\x1b[1;3C"
    } else {
        b"\x1b[1;5C"
    };
    cx.update_window(fixture.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
    })
    .unwrap();
    echo(&fixture, "\x1b]133;A\x07$ \x1b]133;B\x07");
    eventually(cx, |cx| {
        fixture
            .view
            .read_with(cx, |view, _| view.has_shell_integration())
    });
    fixture.recorder.take();

    // 键入 g、远端回显：提示最近的一条
    let press = |cx: &mut TestAppContext, key: &str| {
        cx.update_window(fixture.window, |_, window, cx| {
            window.press(key, cx);
            window.render_frame(cx);
        })
        .unwrap();
    };
    cx.update_window(fixture.window, |_, window, cx| window.input("g", cx))
        .unwrap();
    assert_eq!(fixture.recorder.take(), b"g");
    echo(&fixture, "g");
    eventually(cx, |cx| {
        suggestion(cx, &fixture).as_deref() == Some("it status --short")
    });
    // 画出来（含光标下的第一个字）
    cx.update_window(fixture.window, |_, window, cx| window.render_frame(cx))
        .unwrap();

    // 接受一个词；回显到达之前再按一次，接着下一个词，不会把 it 发两次
    press(cx, word);
    assert_eq!(fixture.recorder.take(), b"it");
    press(cx, word);
    assert_eq!(fixture.recorder.take(), b" status");
    echo(&fixture, "it status");
    eventually(cx, |cx| {
        suggestion(cx, &fixture).as_deref() == Some(" --short")
    });
    // Esc 交给终端，提示不变
    press(cx, "escape");
    assert_eq!(fixture.recorder.take(), b"\x1b");
    // End 接受整条
    press(cx, "end");
    assert_eq!(fixture.recorder.take(), b" --short");
    echo(&fixture, " --short");
    eventually(cx, |cx| {
        cx.run_until_parked();
        fixture.view.read_with(cx, |view, _| {
            view.screen_text().contains("git status --short")
        })
    });
    assert_eq!(suggestion(cx, &fixture), None, "已经完整");
    // 没有提示：→、End 与按词的键照常发给远端
    press(cx, "right");
    assert_eq!(fixture.recorder.take(), b"\x1b[C");
    press(cx, "end");
    assert_eq!(fixture.recorder.take(), b"\x1b[F");
    press(cx, word);
    assert_eq!(fixture.recorder.take(), word_key);

    // 新的提示符：→ 接受整条；宽字符照常
    press(cx, "enter");
    echo(
        &fixture,
        "\r\n\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07",
    );
    // 等新的提示符解析完（B 之后才算在这个提示符后输入）
    eventually(cx, |cx| {
        fixture.view.read_with(cx, |view, _| {
            view.screen_text()
                .lines()
                .filter(|line| line.starts_with('$'))
                .count()
                == 2
        })
    });
    fixture.recorder.take();
    cx.update_window(fixture.window, |_, window, cx| window.input("echo 中", cx))
        .unwrap();
    echo(&fixture, "echo 中");
    eventually(cx, |cx| {
        suggestion(cx, &fixture).as_deref() == Some("文测试")
    });
    cx.update_window(fixture.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    fixture.recorder.take();
    press(cx, "right");
    assert_eq!(fixture.recorder.take(), "文测试".as_bytes());

    // 关掉之后没有提示，→ 照常发给远端
    echo(&fixture, "文测试");
    cx.update(|cx| {
        fixture
            .view
            .update(cx, |view, cx| view.set_command_suggestions(None, cx))
    });
    cx.update_window(fixture.window, |_, window, cx| window.input(" ", cx))
        .unwrap();
    echo(&fixture, " ");
    fixture.recorder.take();
    assert_eq!(suggestion(cx, &fixture), None);
    press(cx, "right");
    assert_eq!(fixture.recorder.take(), b"\x1b[C");
}

// 回放模式：网格保持录制时的大小（不随窗口变），按键不处理，内容照常显示、可以复制
#[gpui_kit::test]
fn replay_view_keeps_the_recorded_grid(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(termlet_terminal::init);
    let (engine, feed) = TerminalEngine::replay(EngineOptions {
        scrollback: 100,
        size: GridSize::new(200, 60),
    });
    let typed = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = typed.clone();
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(800.), px(480.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| {
                    let mut view = TerminalView::new(engine, TerminalStyle::default(), window, cx);
                    view.set_replay(true, cx);
                    view.set_input_observer(Some(Arc::new(move |bytes: &[u8]| {
                        sink.lock().unwrap().extend_from_slice(bytes)
                    })));
                    view
                })
            },
        )
        .expect("打开测试窗口")
    });
    feed.output(b"recorded \x1b[32mtext\x1b[0m".to_vec());
    feed.wait_idle();
    eventually(cx, |cx| {
        view.read_with(cx, |view, _| {
            view.screen_text().starts_with("recorded text")
        })
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.input("ls", cx);
        window.press("enter", cx);
        window.press("ctrl-shift-a", cx);
        window.press("ctrl-shift-c", cx);
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap();
        assert!(copied.starts_with("recorded text"), "复制到：{copied:?}");
    })
    .unwrap();
    assert!(typed.lock().unwrap().is_empty(), "回放中的按键不算输入");
    let grid = view.read_with(cx, |view, _| view.engine().size());
    assert_eq!((grid.columns, grid.rows), (200, 60));
}
