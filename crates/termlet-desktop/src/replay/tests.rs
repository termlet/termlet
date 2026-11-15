//! 回放 Tab 的测试：打开、跳转、倍速、跳过长停顿、关闭；在本地终端上录制再回放（开始、停止、关闭 Tab、会话结束时
//! 文件完整）。另有一个发布构建的基准：10 万个事件的录制从头跳到末尾的耗时。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, Entity, TestAppContext};
use termlet_core::recording::cast::{self, Cast, CastHeader, EventData};
use termlet_core::recording::player::{Playhead, Timeline};

use super::*;
use crate::app_state::AppState;
use crate::terminal_panel::TerminalPanel;
use crate::testkit::{open_workspace, poll_until};
use crate::workspace::Workspace;

/// 一段 40×5 的录制：开始的屏幕、两条命令（第二条之前终端改成 50×6）、一段 28.5 秒的停顿。
const SAMPLE: &str = concat!(
    "{\"version\": 2, \"width\": 40, \"height\": 5, \"timestamp\": 1762670000, \"title\": \"web-1\"}\n",
    "[0.0, \"o\", \"\\u001b[H\\u001b[2J$ \"]\n",
    "[0.5, \"o\", \"echo first\\r\\nfirst\\r\\n$ \"]\n",
    "[1.0, \"r\", \"50x6\"]\n",
    "[1.5, \"o\", \"echo second\\r\\nsecond\\r\\n$ \"]\n",
    "[30.0, \"o\", \"done\\r\\n\"]\n",
);

fn workspace_of(cx: &mut TestAppContext, handle: AnyWindowHandle) -> Entity<Workspace> {
    cx.update_window(handle, |_, window, cx| {
        Workspace::in_window(window, cx).unwrap()
    })
    .unwrap()
}

/// 放测试文件的目录：这个测试自己的数据目录下（随它一起删掉）。
fn test_dir(cx: &mut TestAppContext) -> PathBuf {
    let dir = cx.update(|cx| AppState::global(cx).data_dir.root().join("replay-test"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_sample(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, SAMPLE).unwrap();
    path
}

fn open_replay(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    path: &Path,
) -> Entity<ReplayPanel> {
    let before = cx.update(|cx| workspace.read(cx).live_replays().len());
    let path = path.to_path_buf();
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_replay(path, window, cx))
    })
    .unwrap();
    poll_until(cx, "回放 Tab 打开", |cx| {
        cx.update(|cx| workspace.read(cx).live_replays().len() > before)
    });
    cx.update(|cx| workspace.read(cx).live_replays().pop().unwrap())
}

fn screen(cx: &mut TestAppContext, panel: &Entity<ReplayPanel>) -> String {
    cx.update(|cx| panel.read(cx).terminal().read(cx).screen_text())
}

fn grid(cx: &mut TestAppContext, panel: &Entity<ReplayPanel>) -> (u16, u16) {
    cx.update(|cx| {
        let size = panel.read(cx).terminal().read(cx).engine().size();
        (size.columns, size.rows)
    })
}

fn wait_screen(
    cx: &mut TestAppContext,
    panel: &Entity<ReplayPanel>,
    what: &str,
    ready: impl Fn(&str) -> bool,
) {
    poll_until(cx, what, |cx| ready(&screen(cx, panel)));
}

fn act(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    panel: &Entity<ReplayPanel>,
    f: impl FnOnce(&mut ReplayPanel, &mut gpui_kit::Window, &mut gpui_kit::Context<ReplayPanel>),
) {
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |p, cx| f(p, window, cx))
    })
    .unwrap();
}

// 打开后自动播放；跳到末尾：输出与大小变化都生效；跳回前面：从头重放到目标位置后换上（大小也回到当时的）；
// 跳过长停顿缩短时长；同一个文件再打开时切到已打开的；关闭后 Tab 不在
#[gpui_kit::test]
fn replay_tab_seeks_and_closes(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let workspace = workspace_of(cx, handle);
    let tmp = test_dir(cx);
    let path = write_sample(&tmp, "web-1-20261109-120000.cast");
    let panel = open_replay(cx, handle, &workspace, &path);
    assert!(cx.update(|cx| panel.read(cx).is_playing()));
    assert_eq!(
        cx.update(|cx| panel.read(cx).title_text()),
        "回放 · web-1-20261109-120000"
    );
    assert_eq!(cx.update(|cx| panel.read(cx).duration()), 30.0);

    // 快捷键（焦点在回放的终端上）：空格暂停，→ 前进 5 秒，Home、End 到开头、结尾
    let position = |cx: &mut TestAppContext| cx.update(|cx| panel.read(cx).position());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.press("space", cx);
    })
    .unwrap();
    assert!(!cx.update(|cx| panel.read(cx).is_playing()), "空格暂停");
    let paused_at = position(cx);
    cx.update_window(handle, |_, window, cx| window.press("right", cx))
        .unwrap();
    assert_eq!(position(cx), paused_at + 5.);
    cx.update_window(handle, |_, window, cx| window.press("end", cx))
        .unwrap();
    assert_eq!(position(cx), 30.0);
    cx.update_window(handle, |_, window, cx| window.press("home", cx))
        .unwrap();
    assert_eq!(position(cx), 0.0);

    act(cx, handle, &panel, |p, window, cx| p.seek(100., window, cx));
    wait_screen(cx, &panel, "跳到末尾", |s| s.contains("done"));
    assert!(screen(cx, &panel).contains("second"));
    assert_eq!(grid(cx, &panel), (50, 6));

    act(cx, handle, &panel, |p, window, cx| p.seek(0.6, window, cx));
    wait_screen(cx, &panel, "跳回第一条命令之后", |s| {
        s.contains("first") && !s.contains("second")
    });
    assert_eq!(grid(cx, &panel), (40, 5));
    assert_eq!(cx.update(|cx| panel.read(cx).position()), 0.6);

    act(cx, handle, &panel, |p, window, cx| {
        p.set_skip_idle(true, window, cx)
    });
    assert_eq!(cx.update(|cx| panel.read(cx).duration()), 3.5);
    act(cx, handle, &panel, |p, _, cx| p.set_speed(4., cx));

    // 同一个文件再打开：切到已打开的
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_replay(path.clone(), window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| workspace.read(cx).live_replays().len()), 1);

    act(cx, handle, &panel, |p, _, cx| p.close_tab(cx));
    drop(panel);
    cx.run_until_parked();
    assert!(cx.update(|cx| workspace.read(cx).live_replays().is_empty()));
}

// 拖进窗口的文件只打开 .cast；读不了的文件不开 Tab
#[gpui_kit::test]
fn dropped_files_open_only_recordings(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let workspace = workspace_of(cx, handle);
    let tmp = test_dir(cx);
    let text = tmp.join("notes.txt");
    std::fs::write(&text, "hello").unwrap();
    let bad = tmp.join("bad.CAST");
    std::fs::write(&bad, "not a recording").unwrap();
    let opened = cx
        .update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.open_replay_files(&[text.clone(), bad.clone()], window, cx)
            })
        })
        .unwrap();
    assert!(opened, "bad.CAST 按扩展名算录制文件");
    // 读入失败：提示，不开 Tab
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(cx.update(|cx| workspace.read(cx).live_replays().is_empty()));
    let none = cx
        .update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.open_replay_files(std::slice::from_ref(&text), window, cx)
            })
        })
        .unwrap();
    assert!(!none);
}

/// 新开一个本地终端，等它启动。
fn open_local(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
) -> Entity<TerminalPanel> {
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
    })
    .unwrap();
    poll_until(cx, "本地终端启动", |cx| {
        cx.update(|cx| {
            workspace
                .read(cx)
                .live_terminals()
                .last()
                .is_some_and(|t| t.read(cx).is_live(cx))
        })
    });
    cx.update(|cx| workspace.read(cx).live_terminals().pop().unwrap())
}

/// 这个窗口的录制目录中的录制文件。
fn recordings(cx: &mut TestAppContext) -> Vec<PathBuf> {
    let root = cx.update(|cx| {
        let local = AppState::store(cx).local_settings();
        termlet_core::recording::recordings_root(&local.logs, &AppState::global(cx).data_dir)
    });
    let mut files: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|dir| dir.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    files
}

/// 等录制文件写到包含 `text` 的输出、能完整读入。
fn wait_cast(cx: &mut TestAppContext, path: &Path, text: &str) -> Cast {
    let mut last = None;
    poll_until(cx, "录制文件写完", |_| {
        let Ok(cast) = cast::read_file(path) else {
            return false;
        };
        let ok = cast.skipped == 0 && outputs(&cast).contains(text);
        last = Some(cast);
        ok
    });
    last.unwrap()
}

fn outputs(cast: &Cast) -> String {
    cast.events
        .iter()
        .filter_map(|e| match &e.data {
            EventData::Output(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// 录制目录中新出现的那个文件。
fn new_file(cx: &mut TestAppContext, before: &[PathBuf]) -> PathBuf {
    let files: Vec<PathBuf> = recordings(cx)
        .into_iter()
        .filter(|f| !before.contains(f))
        .collect();
    assert_eq!(files.len(), 1, "应只多了一个录制文件：{files:?}");
    files[0].clone()
}

/// 开始录制，等它接上（文件建好、取了屏幕）。
fn start_recording(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    terminal: &Entity<TerminalPanel>,
) {
    cx.update_window(handle, |_, window, cx| {
        terminal.update(cx, |t, cx| t.start_recording(window, cx))
    })
    .unwrap();
    poll_until(cx, "开始录制", |cx| {
        cx.update(|cx| terminal.read(cx).recording_path().is_some())
    });
}

/// 在终端里执行 `echo <word>`，等输出出现。
fn echo(cx: &mut TestAppContext, terminal: &Entity<TerminalPanel>, word: &str) {
    let command = format!("echo {word}\r");
    cx.update(|cx| terminal.update(cx, |t, cx| t.send_text(&command, cx)));
    poll_until(cx, word, |cx| {
        cx.update(|cx| terminal.read(cx).screen_text(cx).matches(word).count() >= 2)
    });
}

// 在本地终端上录制：开始时写入当时的屏幕，停止后文件完整、默认不录输入，回放出同样的内容；关闭 Tab、会话结束时
// 录制随之停止，文件同样完整
#[gpui_kit::test]
fn record_local_terminal_then_replay(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let workspace = workspace_of(cx, handle);
    let terminal = open_local(cx, handle, &workspace);
    echo(cx, &terminal, "before-rec-1");

    // 开始、停止
    start_recording(cx, handle, &terminal);
    echo(cx, &terminal, "during-rec-2");
    cx.update_window(handle, |_, window, cx| {
        terminal.update(cx, |t, cx| t.stop_recording(None, window, cx))
    })
    .unwrap();
    assert!(!cx.update(|cx| terminal.read(cx).is_recording()));
    let first = new_file(cx, &[]);
    let cast = wait_cast(cx, &first, "during-rec-2");
    // 开始时的屏幕里已有录制前的输出；没有开启录制输入
    match &cast.events[0].data {
        EventData::Output(text) => assert!(text.contains("before-rec-1"), "{text:?}"),
        other => panic!("第一个事件应是开始时的屏幕：{other:?}"),
    }
    assert!(
        !cast
            .events
            .iter()
            .any(|e| matches!(e.data, EventData::Input(_)))
    );

    // 回放出同样的内容
    let panel = open_replay(cx, handle, &workspace, &first);
    act(cx, handle, &panel, |p, window, cx| {
        p.pause(cx);
        let end = p.duration();
        p.seek(end, window, cx)
    });
    wait_screen(cx, &panel, "回放出录制的内容", |s| {
        s.contains("before-rec-1") && s.matches("during-rec-2").count() >= 2
    });

    // 关闭 Tab：录制停止，文件完整
    let before = recordings(cx);
    start_recording(cx, handle, &terminal);
    echo(cx, &terminal, "closing-3");
    cx.update(|cx| terminal.update(cx, |t, cx| t.close_tab(cx)));
    drop(terminal);
    cx.run_until_parked();
    let second = new_file(cx, &before);
    wait_cast(cx, &second, "closing-3");

    // 会话结束（shell 退出）：录制随之停止，文件完整
    let terminal = open_local(cx, handle, &workspace);
    let before = recordings(cx);
    start_recording(cx, handle, &terminal);
    echo(cx, &terminal, "exiting-4");
    cx.update(|cx| terminal.update(cx, |t, cx| t.send_text("exit\r", cx)));
    poll_until(cx, "会话结束后录制停止", |cx| {
        cx.update(|cx| !terminal.read(cx).is_recording())
    });
    let third = new_file(cx, &before);
    wait_cast(cx, &third, "exiting-4");
}

// 设置开启“同时录制输入”时，用户的键入记成输入事件；代填等应用写入的内容不经过它（见终端视图的测试）
#[gpui_kit::test]
fn input_is_recorded_when_enabled(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let workspace = workspace_of(cx, handle);
    cx.update(|cx| {
        AppState::store(cx)
            .update_settings(|s| s.terminal.record_input = true)
            .unwrap()
    });
    let terminal = open_local(cx, handle, &workspace);
    start_recording(cx, handle, &terminal);
    echo(cx, &terminal, "typed-5");
    cx.update_window(handle, |_, window, cx| {
        terminal.update(cx, |t, cx| t.stop_recording(None, window, cx))
    })
    .unwrap();
    let file = new_file(cx, &[]);
    let cast = wait_cast(cx, &file, "typed-5");
    let input: String = cast
        .events
        .iter()
        .filter_map(|e| match &e.data {
            EventData::Input(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(input, "echo typed-5\r");
}

/// 基准：10 万个事件（每个约 100 字节的带颜色日志行，共约 10MB）的录制，读入、从头跳到末尾（送进回放引擎并解析完，
/// 途中照常存关键帧）的耗时。`cargo test -p termlet-desktop --release --lib seek_to_end_benchmark -- --ignored --nocapture`
#[test]
#[ignore = "基准，手动运行"]
fn seek_to_end_benchmark() {
    const EVENTS: usize = 100_000;
    let mut text = cast::header_line(&CastHeader {
        width: 160,
        height: 48,
        ..CastHeader::default()
    });
    for i in 0..EVENTS {
        let line = format!(
            "\x1b[32m2026-11-09 12:{:02}:{:02}\x1b[0m INFO  服务请求 id={i:08} path=/api/v1/hosts status=200\r\n",
            i / 60 % 60,
            i % 60
        );
        text.push('\n');
        cast::push_event(&mut text, i as f64 * 0.01, &EventData::Output(line));
    }
    text.push('\n');
    let bytes = text.len();

    let started = Instant::now();
    let parsed = cast::parse(text.as_bytes()).unwrap();
    let parse = started.elapsed();
    assert_eq!(parsed.events.len(), EVENTS);

    let started = Instant::now();
    let mut head = Playhead::new(Timeline::new(&parsed, None));
    let seek = head.seek(f64::MAX);
    let Seek::Forward(range) = seek else {
        panic!("从头往后跳：{seek:?}");
    };
    let (engine, feed) = TerminalEngine::replay(EngineOptions {
        scrollback: 5000,
        size: GridSize::new(160, 48),
    });
    let mut keyframes = Keyframes::default();
    feed_events(&feed, &parsed, range, &mut keyframes, |_, _| {});
    let queued = started.elapsed();
    feed.wait_idle();
    let total = started.elapsed();
    assert!(
        engine
            .screen_text()
            .contains(&format!("id={:08}", EVENTS - 1))
    );
    println!(
        "{EVENTS} 个事件，文件 {:.1} MB，输出 {:.1} MB：读入 {} ms，送进引擎 {} ms，跳到末尾共 {} ms",
        bytes as f64 / 1048576.,
        parsed.output_bytes() as f64 / 1048576.,
        parse.as_millis(),
        queued.as_millis(),
        total.as_millis()
    );
}

/// 一行带颜色的日志（约 100 字节）。
fn log_line(i: usize) -> String {
    format!(
        "\x1b[32m2026-11-09 12:{:02}:{:02}\x1b[0m \x1b[1;34mINFO\x1b[0m  服务请求 id={i:08} path=/api/v1/hosts status=200\r\n",
        i / 60 % 60,
        i % 60
    )
}

/// 基准：大文件往前跳。50 万个事件（约 50MB 带颜色的日志）一路放到末尾（途中存关键帧），再跳回 90% 与 50% 处：
/// 从头重放（没有关键帧时的做法）与从最近的关键帧重放的耗时，以及关键帧占的内存。
/// `cargo test -p termlet-desktop --release --lib seek_back_benchmark -- --ignored --nocapture`
#[test]
#[ignore = "基准，手动运行"]
fn seek_back_benchmark() {
    const EVENTS: usize = 500_000;
    let cast = Cast {
        version: 2,
        header: CastHeader {
            width: 160,
            height: 48,
            ..CastHeader::default()
        },
        events: (0..EVENTS)
            .map(|i| cast::CastEvent {
                time: i as f64 * 0.01,
                data: EventData::Output(log_line(i)),
            })
            .collect(),
        skipped: 0,
    };
    let options = |size| EngineOptions {
        scrollback: 5000,
        size,
    };
    let start = GridSize::new(160, 48);

    // 一路放到末尾，途中存关键帧
    let started = Instant::now();
    let (_engine, feed) = TerminalEngine::replay(options(start));
    let mut keyframes = Keyframes::default();
    let mut waits = Vec::new();
    feed_events(&feed, &cast, 0..EVENTS, &mut keyframes, |index, wait| {
        waits.push((index, wait))
    });
    feed.wait_idle();
    let to_end = started.elapsed();
    for (index, wait) in waits {
        keyframes.finish(index, futures::executor::block_on(wait));
    }
    println!(
        "{EVENTS} 个事件，输出 {:.1} MB：放到末尾（存关键帧）{} ms；关键帧 {} 份，共 {:.1} MB",
        cast.output_bytes() as f64 / 1048576.,
        to_end.as_millis(),
        keyframes.len(),
        keyframes.bytes() as f64 / 1048576.
    );

    for percent in [90, 50] {
        let target = EVENTS * percent / 100;
        let started = Instant::now();
        let (scratch, feed) = TerminalEngine::replay(options(start));
        feed_events(
            &feed,
            &cast,
            0..target,
            &mut Keyframes::new(usize::MAX, 0),
            |_, _| {},
        );
        feed.wait_idle();
        let from_start = started.elapsed();

        let started = Instant::now();
        let keyframe = keyframes.before(target).expect("有关键帧");
        let (restored, feed) = TerminalEngine::replay(options(GridSize::new(
            keyframe.dump.columns,
            keyframe.dump.rows,
        )));
        feed.output(keyframe.dump.text.clone().into_bytes());
        feed_events(
            &feed,
            &cast,
            keyframe.index..target,
            &mut Keyframes::new(usize::MAX, 0),
            |_, _| {},
        );
        feed.wait_idle();
        let from_keyframe = started.elapsed();
        assert_eq!(restored.screen_text(), scratch.screen_text());
        println!(
            "跳回 {percent}%：从头重放 {} ms，从关键帧（第 {} 个事件）重放 {} ms",
            from_start.as_millis(),
            keyframe.index,
            from_keyframe.as_millis()
        );
    }
}

/// 不经界面的回放：跳回前面时新的引擎从头重放，屏幕与一路放过去的相同。
#[test]
fn restart_matches_continuous_playback() {
    let cast = cast::parse(SAMPLE.as_bytes()).unwrap();
    let options = || EngineOptions {
        scrollback: 100,
        size: GridSize::new(cast.header.width, cast.header.height),
    };
    // 一路放到 1.6 秒
    let (continuous, feed) = TerminalEngine::replay(options());
    let mut head = Playhead::new(Timeline::new(&cast, None));
    head.play();
    let mut keyframes = Keyframes::default();
    feed_events(
        &feed,
        &cast,
        head.advance(Duration::from_millis(1600)),
        &mut keyframes,
        |_, _| {},
    );
    feed.wait_idle();
    // 先跳到末尾，再跳回 1.6 秒：从头重放
    let mut other = Playhead::new(Timeline::new(&cast, None));
    assert!(matches!(other.seek(100.), Seek::Forward(_)));
    let Seek::Restart(range) = other.seek(1.6) else {
        panic!("往前跳应从头重放");
    };
    let (restarted, feed) = TerminalEngine::replay(options());
    feed_events(&feed, &cast, range, &mut keyframes, |_, _| {});
    feed.wait_idle();
    assert_eq!(restarted.screen_text(), continuous.screen_text());
    assert_eq!(restarted.size(), GridSize::new(50, 6));
}

/// 文件头里带配色的录制（8 色）。
const THEMED: &str = concat!(
    "{\"version\": 2, \"width\": 40, \"height\": 5, \"theme\": {\"fg\": \"#112233\", \"bg\": \"#fafafa\", ",
    "\"palette\": \"#000000:#aa0000:#00aa00:#aaaa00:#0000aa:#aa00aa:#00aaaa:#aaaaaa\"}}\n",
    "[0.0, \"o\", \"hello\"]\n",
);

fn palette_of(cx: &mut TestAppContext, panel: &Entity<ReplayPanel>) -> Palette {
    cx.update(|cx| panel.read(cx).terminal().read(cx).style().palette.clone())
}

// 文件头里的配色：8 色时亮色与普通色相同，颜色写错、少于 8 个时不用
#[test]
fn recorded_palette_from_header() {
    let theme = CastTheme {
        foreground: "#112233".into(),
        background: "#FAFAFA".into(),
        palette: "#000000:#aa0000:#00aa00:#aaaa00:#0000aa:#aa00aa:#00aaaa:#aaaaaa"
            .split(':')
            .map(str::to_string)
            .collect(),
    };
    let palette = recorded_palette(&theme).unwrap();
    let rgb = |r, g, b| Rgb { r, g, b };
    assert_eq!(palette.foreground, rgb(0x11, 0x22, 0x33));
    assert_eq!(palette.background, rgb(0xfa, 0xfa, 0xfa));
    assert_eq!(palette.cursor, palette.foreground);
    assert_eq!(palette.ansi[1], rgb(0xaa, 0, 0));
    assert_eq!(palette.ansi[9], palette.ansi[1]);
    let mut sixteen = theme.clone();
    sixteen
        .palette
        .extend(std::iter::repeat_n("#ffffff".to_string(), 8));
    assert_eq!(
        recorded_palette(&sixteen).unwrap().ansi[9],
        rgb(0xff, 0xff, 0xff)
    );
    let mut short = theme.clone();
    short.palette.truncate(7);
    assert!(recorded_palette(&short).is_none());
    let mut bad = theme;
    bad.background = "white".into();
    assert!(recorded_palette(&bad).is_none());
}

// 打开带配色的录制：默认用录制时的配色，可切换成当前的终端配色、再切回；没有配色的录制照旧用当前的
#[gpui_kit::test]
fn replay_uses_recorded_colors(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let workspace = workspace_of(cx, handle);
    let tmp = test_dir(cx);
    let path = tmp.join("themed.cast");
    std::fs::write(&path, THEMED).unwrap();
    let panel = open_replay(cx, handle, &workspace, &path);
    let recorded = Rgb {
        r: 0xfa,
        g: 0xfa,
        b: 0xfa,
    };
    assert!(cx.update(|cx| panel.read(cx).uses_recorded_colors()));
    assert_eq!(palette_of(cx, &panel).background, recorded);
    act(cx, handle, &panel, |p, _, cx| {
        p.set_recorded_colors(false, cx)
    });
    assert!(!cx.update(|cx| panel.read(cx).uses_recorded_colors()));
    assert_ne!(palette_of(cx, &panel).background, recorded);
    // 设置、外观变了时照样按选的配色
    act(cx, handle, &panel, |p, _, cx| p.restyle(cx));
    assert_ne!(palette_of(cx, &panel).background, recorded);
    act(cx, handle, &panel, |p, _, cx| {
        p.set_recorded_colors(true, cx)
    });
    assert_eq!(palette_of(cx, &panel).background, recorded);

    let plain = open_replay(cx, handle, &workspace, &write_sample(&tmp, "plain.cast"));
    assert!(!cx.update(|cx| plain.read(cx).has_recorded_colors()));
    assert_ne!(palette_of(cx, &plain).background, recorded);
}

// 一路放过去时存下关键帧；跳回前面时从不晚于目标的关键帧重放，画面与大小与从头重放的相同
#[gpui_kit::test]
fn seeking_back_restores_from_keyframes(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let workspace = workspace_of(cx, handle);
    let tmp = test_dir(cx);
    let path = write_sample(&tmp, "keyframes.cast");
    let panel = open_replay(cx, handle, &workspace, &path);
    act(cx, handle, &panel, |p, _, cx| {
        p.pause(cx);
        p.set_keyframe_spacing(1);
    });
    act(cx, handle, &panel, |p, window, cx| p.seek(100., window, cx));
    wait_screen(cx, &panel, "跳到末尾", |s| s.contains("done"));
    poll_until(cx, "存下关键帧", |cx| {
        cx.update(|cx| panel.read(cx).keyframe_positions().len() >= 3)
    });
    // 大小变化之后没有输出，不另存
    let positions = cx.update(|cx| panel.read(cx).keyframe_positions());
    assert_eq!(positions, [2, 4, 5]);
    let rebuilt = |cx: &mut TestAppContext| {
        poll_until(cx, "重放好", |cx| {
            cx.update(|cx| !panel.read(cx).is_rebuilding())
        });
        cx.update(|cx| panel.read(cx).rebuilt_from())
    };

    // 跳到大小变化之后、第二条命令之前：从第一条命令之后的那一份重放，再送大小变化，大小是 50×6
    act(cx, handle, &panel, |p, window, cx| p.seek(1.2, window, cx));
    assert_eq!(rebuilt(cx), Some(2));
    let s = screen(cx, &panel);
    assert!(s.contains("first") && !s.contains("second"), "{s}");
    assert_eq!(grid(cx, &panel), (50, 6));

    // 跳到第一条命令之前：之前没有关键帧，从头重放，途中存下这里的一份；再跳回来时从它重放
    for from in [0, 1] {
        act(cx, handle, &panel, |p, window, cx| p.seek(1.2, window, cx));
        act(cx, handle, &panel, |p, window, cx| p.seek(0.2, window, cx));
        assert_eq!(rebuilt(cx), Some(from));
        let s = screen(cx, &panel);
        assert!(s.contains('$') && !s.contains("first"), "{s}");
        assert_eq!(grid(cx, &panel), (40, 5));
        poll_until(cx, "存下开头的关键帧", |cx| {
            cx.update(|cx| panel.read(cx).keyframe_positions().contains(&1))
        });
    }
}
