//! 任务栏进度：一轮传输的合计与状态（[`Tracker`]），以及按窗口显示、开关、激活后清除（假的任务栏实现记下调用）。

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::{AppContext as _, TestAppContext, Window};
use termlet_core::sftp::TransferState;

use super::{Indicator, Progress, Taskbar, TaskbarHost, Tracker, install_with, sync};
use crate::app_state::AppState;
use crate::sftp::panel::window_owner;

fn item(id: &str, done: u64, total: u64, state: TransferState) -> Progress {
    Progress {
        id: id.into(),
        owner: "w".into(),
        done,
        total,
        state,
    }
}

use TransferState::{Canceled, Done, Failed, Queued, Running, WaitingReconnect};

// 多个传输按字节合计；完成后被移出列表的仍计入（进度不倒退），取消的不计；全部完成后清除
#[test]
fn sums_bytes_across_a_round() {
    let mut t = Tracker::default();
    assert_eq!(t.update(&[], false), Indicator::None);
    let list = vec![item("a", 100, 400, Running), item("b", 0, 600, Queued)];
    assert_eq!(t.update(&list, false), Indicator::Normal(100));
    // a 完成，b 传了一半：(400 + 300) / 1000
    let list = vec![item("a", 400, 400, Done), item("b", 300, 600, Running)];
    assert_eq!(t.update(&list, false), Indicator::Normal(700));
    // a 被自动清除：仍计入
    let list = vec![item("b", 300, 600, Running)];
    assert_eq!(t.update(&list, false), Indicator::Normal(700));
    // 新加入的 c 计入这一轮；取消的 d 不计
    let list = vec![
        item("b", 600, 600, Done),
        item("c", 0, 1000, Queued),
        item("d", 50, 5000, Canceled),
    ];
    assert_eq!(t.update(&list, false), Indicator::Normal(500));
    let list = vec![item("b", 600, 600, Done), item("c", 1000, 1000, Done)];
    assert_eq!(t.update(&list, false), Indicator::None, "全部完成后清除");
    // 下一轮从头算，上一轮完成的不再计入
    let list = vec![item("b", 600, 600, Done), item("e", 10, 100, Running)];
    assert_eq!(t.update(&list, false), Indicator::Normal(100));
}

// 还不知道总量时为不确定；都在等待重连时为暂停；有失败的为错误
#[test]
fn states_follow_the_transfers() {
    let mut t = Tracker::default();
    assert_eq!(
        t.update(&[item("dir", 0, 0, Running)], false),
        Indicator::Indeterminate
    );
    let list = vec![
        item("dir", 200, 1000, WaitingReconnect),
        item("x", 0, 1000, WaitingReconnect),
    ];
    assert_eq!(t.update(&list, false), Indicator::Paused(100));
    let list = vec![item("dir", 200, 1000, Running), item("x", 0, 1000, Failed)];
    assert_eq!(
        t.update(&list, false),
        Indicator::Error(100),
        "进行中有失败的"
    );
    // 传输中的目录还不知道总量：整体为不确定，统计完后按字节算
    let mut t = Tracker::default();
    let list = vec![item("f", 500, 1000, Running), item("dir", 0, 0, Running)];
    assert_eq!(t.update(&list, false), Indicator::Indeterminate);
    let list = vec![item("f", 500, 1000, Running), item("dir", 0, 1000, Running)];
    assert_eq!(t.update(&list, false), Indicator::Normal(250));
}

// 一轮以失败结束：窗口不在前台时保持错误，直到窗口被激活、失败的被移除或继续；在前台时不保持
#[test]
fn failure_is_held_until_seen() {
    let mut t = Tracker::default();
    t.update(&[item("a", 10, 100, Running)], false);
    let failed = vec![item("a", 10, 100, Failed)];
    assert_eq!(t.update(&failed, false), Indicator::Error(1000));
    assert_eq!(t.update(&failed, false), Indicator::Error(1000), "保持");
    assert!(t.acknowledge());
    assert_eq!(t.update(&failed, false), Indicator::None, "激活后清除");
    assert!(!t.acknowledge());

    // 失败的记录被移除
    t.update(&[item("b", 0, 100, Running)], false);
    t.update(&[item("b", 0, 100, Failed)], false);
    assert_eq!(t.update(&[], false), Indicator::None);

    // 继续：开始新的一轮，完成后清除
    t.update(&[item("c", 0, 100, Running)], false);
    t.update(&[item("c", 0, 100, Failed)], false);
    assert_eq!(
        t.update(&[item("c", 0, 100, Running)], false),
        Indicator::Normal(0)
    );
    assert_eq!(
        t.update(&[item("c", 100, 100, Done)], false),
        Indicator::None
    );

    // 结束时窗口在前台：不保持
    let mut t = Tracker::default();
    t.update(&[item("d", 0, 100, Running)], true);
    assert_eq!(
        t.update(&[item("d", 0, 100, Failed)], true),
        Indicator::None
    );
}

// 两次快照之间就开始并失败的传输同样算作失败；打开窗口（或打开开关）时已有的失败记录不算
#[test]
fn fast_failures_between_snapshots_count() {
    let mut t = Tracker::default();
    let old = item("old", 0, 100, Failed);
    assert_eq!(t.update([&old], false), Indicator::None, "已有的失败不算");
    let quick = item("quick", 0, 100, Failed);
    assert_eq!(
        t.update([&old, &quick], false),
        Indicator::Error(1000),
        "新出现就失败的"
    );
    assert!(t.acknowledge());
    let fast = item("fast", 100, 100, Done);
    assert_eq!(
        t.update([&old, &quick, &fast], false),
        Indicator::None,
        "新出现就完成的不显示"
    );
}

/// 记下调用的任务栏：（窗口的标识，状态）。
struct FakeHost {
    per_window: bool,
    calls: Rc<RefCell<Vec<(Option<String>, Indicator)>>>,
}

impl TaskbarHost for FakeHost {
    fn per_window(&self) -> bool {
        self.per_window
    }

    fn show(&mut self, window: Option<&Window>, indicator: Indicator) {
        self.calls
            .borrow_mut()
            .push((window.map(window_owner), indicator));
    }
}

fn setup(
    cx: &mut TestAppContext,
    per_window: bool,
) -> Rc<RefCell<Vec<(Option<String>, Indicator)>>> {
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    let calls: Rc<RefCell<Vec<(Option<String>, Indicator)>>> = Rc::default();
    let host = FakeHost {
        per_window,
        calls: calls.clone(),
    };
    cx.update(|cx| install_with(Box::new(host), cx));
    calls
}

fn set_transfers(cx: &mut TestAppContext, list: Vec<Progress>) {
    cx.update(|cx| {
        cx.global_mut::<Taskbar>().last = list;
        sync(cx);
    });
}

// 按窗口：每个主窗口显示自己的传输；关掉开关时清除，打开后恢复
#[gpui_kit::test]
fn each_window_shows_its_own_transfers(cx: &mut TestAppContext) {
    let calls = setup(cx, true);
    let open = |cx: &mut TestAppContext| {
        let (handle, _) = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| crate::Workspace::new(window, cx))
                })
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, _| window_owner(window))
            .unwrap()
    };
    let first = open(cx);
    let second = open(cx);
    calls.borrow_mut().clear();

    let progress = |owner: &str, done: u64| Progress {
        id: format!("t-{owner}"),
        owner: owner.into(),
        done,
        total: 1000,
        state: Running,
    };
    set_transfers(cx, vec![progress(&first, 250)]);
    assert_eq!(
        calls.borrow().as_slice(),
        &[(Some(first.clone()), Indicator::Normal(250))],
        "只有发起传输的窗口显示进度"
    );
    calls.borrow_mut().clear();
    set_transfers(cx, vec![progress(&first, 250), progress(&second, 500)]);
    assert_eq!(
        calls.borrow().as_slice(),
        &[(Some(second.clone()), Indicator::Normal(500))],
        "没有变化的窗口不重复设置"
    );

    calls.borrow_mut().clear();
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.window.taskbar_progress = false)
            .unwrap()
    });
    cx.run_until_parked();
    let mut cleared = calls.borrow().clone();
    cleared.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![
        (Some(first.clone()), Indicator::None),
        (Some(second.clone()), Indicator::None),
    ];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(cleared, expected, "关掉开关时清除");

    calls.borrow_mut().clear();
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.window.taskbar_progress = true)
            .unwrap()
    });
    cx.run_until_parked();
    assert_eq!(calls.borrow().len(), 2, "打开后重新显示");
    assert!(
        calls
            .borrow()
            .contains(&(Some(first.clone()), Indicator::Normal(250)))
    );
}

// 整个应用一个（macOS 的程序坞）：所有窗口的传输合计；失败后保持，窗口激活后清除
#[gpui_kit::test]
fn app_wide_badge_sums_all_windows(cx: &mut TestAppContext) {
    let calls = setup(cx, false);
    let (handle, _) = cx
        .update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| crate::Workspace::new(window, cx))
            })
        })
        .unwrap();
    cx.run_until_parked();
    calls.borrow_mut().clear();
    let a = Progress {
        id: "a".into(),
        owner: "window-1".into(),
        done: 100,
        total: 400,
        state: Running,
    };
    let b = Progress {
        id: "b".into(),
        owner: "window-2".into(),
        done: 0,
        total: 600,
        state: Running,
    };
    set_transfers(cx, vec![a.clone(), b.clone()]);
    assert_eq!(calls.borrow().last(), Some(&(None, Indicator::Normal(100))));
    let failed = Progress {
        state: Failed,
        ..b.clone()
    };
    let done = Progress {
        state: Done,
        done: 400,
        ..a.clone()
    };
    // 测试中的窗口不在前台：失败后保持
    set_transfers(cx, vec![done, failed]);
    assert_eq!(calls.borrow().last(), Some(&(None, Indicator::Error(1000))));
    cx.update_window(handle, |_, window, cx| super::activated(window, cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        calls.borrow().last(),
        Some(&(None, Indicator::None)),
        "激活后清除"
    );
}

// 真实的传输（测试容器的 sshd）：窗口中有上传进行时任务栏显示进度，完成后清除
#[gpui_kit::test]
fn real_upload_shows_and_clears_progress(cx: &mut TestAppContext) {
    use termlet_core::model::{Encoding, Host, SecretField};
    use termlet_core::secret::SecretString;
    use termlet_core::sftp::{SshTarget, UploadRequest};
    use termlet_core::ssh::{Answer, Prompt, Prompter, TerminalSpec};
    use termlet_core::store::{Owner, Position};

    let Some((address, port)) = crate::testkit::test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let calls = setup(cx, true);
    let (handle, _) = cx
        .update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| crate::Workspace::new(window, cx))
            })
        })
        .unwrap();
    cx.run_until_parked();
    let owner = cx
        .update_window(handle, |_, window, _| window_owner(window))
        .unwrap();
    let store = cx.update(|cx| AppState::store(cx));
    let host = store
        .create(
            Host {
                name: "taskbar".into(),
                address,
                port,
                username: "termlet".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    store
        .set_secret(
            &Owner::host(&host),
            SecretField::Password,
            Some(&SecretString::new("termlet-e2e")),
        )
        .unwrap();
    let prompter: Prompter = std::sync::Arc::new(|prompt| {
        Box::pin(async move {
            match prompt {
                Prompt::HostKey(_) => Answer::AcceptHostKey,
                _ => Answer::Cancel,
            }
        })
    });
    let manager = cx.update(|cx| AppState::sftp(cx));
    let target = SshTarget {
        store: store.clone(),
        host: host.as_str().to_string(),
        spec: TerminalSpec::for_host(&store, &host).unwrap(),
        share_group: None,
        prompter,
        owner: owner.clone(),
        sudo: None,
        name_encoding: Encoding::Utf8,
    };
    let session = termlet_core::runtime::runtime()
        .block_on(manager.open_ssh(target))
        .expect("打开测试容器的 SFTP");
    // 够大的文件：传输中至少发布一次进度
    let local = std::env::temp_dir().join(format!(
        "termlet-taskbar-{}.bin",
        termlet_core::id::random_token(8)
    ));
    std::fs::write(&local, vec![7u8; 48 << 20]).unwrap();
    let remote_dir = "/tmp";
    let request = UploadRequest {
        local_paths: vec![local.clone()],
        remote_dir: remote_dir.into(),
        overwrite: true,
    };
    {
        let (manager, session) = (manager.clone(), session.clone());
        termlet_core::runtime::runtime()
            .block_on(async move { manager.upload(&session, request).await })
            .expect("开始上传");
    }
    let shown = |calls: &Rc<RefCell<Vec<(Option<String>, Indicator)>>>| {
        calls.borrow().iter().any(|(window, indicator)| {
            window.as_deref() == Some(owner.as_str())
                && matches!(indicator, Indicator::Normal(_) | Indicator::Indeterminate)
        })
    };
    crate::testkit::wait_until(cx, handle, "任务栏显示上传进度", |_, _| {
        shown(&calls)
    });
    crate::testkit::wait_until(cx, handle, "上传完成后清除", |_, _| {
        calls.borrow().last() == Some(&(Some(owner.clone()), Indicator::None))
    });
    let name = local.file_name().unwrap().to_string_lossy().to_string();
    let remote = termlet_core::sftp::path::join(remote_dir, &name);
    let _ = termlet_core::runtime::runtime().block_on(session.remove(&remote));
    let _ = std::fs::remove_file(&local);
}

// 设置中的开关的文案都在 locales 中
#[test]
fn texts_exist() {
    for key in [
        "settings.taskbar_progress",
        "settings.taskbar_progress_hint",
        "settings.taskbar_progress_mac",
        "settings.taskbar_progress_mac_hint",
    ] {
        assert_ne!(crate::t!(key).as_ref(), key, "缺少文案 {key}");
    }
}
