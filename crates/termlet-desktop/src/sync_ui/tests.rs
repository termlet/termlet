//! 云同步界面的 UI 集成测试：用本机临时文件夹作为同步位置，经真实的事件路径设置、同步、回答首次同步的选择、停止同步。
//! 同步在核心的运行时上进行，派生密钥（Argon2id）较慢，按条件轮询等待。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::v_flex;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, TestAppContext, Window, WindowBounds, WindowOptions, px,
    size,
};
use termlet_core::cloudsync::{
    CloudSync, FILE_NAME, Pending, Service, SetupInput, SyncError, SyncTarget,
};
use termlet_core::db::Database;
use termlet_core::model::Host;
use termlet_core::secret::SecretString;
use termlet_core::store::{Position, Store};
use termlet_core::vault::{MemoryKeyStore, Vault};

use super::{OpenSyncSettings, SyncIndicator, SyncPage, cloud, describe, install_with};
use crate::app_state::AppState;
use crate::testkit::wait_until;

/// 测试用的同步密码。
const PASSWORD: &str = "termlet-sync-ui-test";

/// 测试用的同步文件夹，结束时删除。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "termlet-sync-ui-{}",
            termlet_core::id::random_token(8)
        ));
        std::fs::create_dir_all(&dir).expect("建立临时文件夹");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 只放同步页与标题栏图标的视图，记下图标发出的“打开设置”。
struct Harness {
    page: Entity<SyncPage>,
    indicator: Entity<SyncIndicator>,
    _open: Subscription,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_4()
            .gap_2()
            .child(self.indicator.clone())
            .child(self.page.clone())
    }
}

fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Rc<Cell<usize>>, Entity<SyncPage>) {
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    // 不开始自动同步、不保存快照
    cx.update(|cx| {
        let cloud = CloudSync::new(AppState::store(cx), None);
        install_with(cloud, cx);
    });
    cx.update(|cx| cx.set_reduce_motion(true));
    let opened = Rc::new(Cell::new(0));
    let counter = opened.clone();
    let (handle, harness) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(900.), px(900.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| {
                    let page = cx.new(|cx| SyncPage::new(window, cx));
                    let indicator = cx.new(SyncIndicator::new);
                    let open = cx.subscribe(&indicator, move |_, _, _: &OpenSyncSettings, _| {
                        counter.set(counter.get() + 1)
                    });
                    Harness {
                        page,
                        indicator,
                        _open: open,
                    }
                })
            },
        )
        .expect("打开测试窗口")
    });
    let page = cx.read(|cx| harness.read(cx).page.clone());
    (handle, opened, page)
}

/// 在表单中填写同步文件夹与同步密码。
fn fill_form(cx: &mut TestAppContext, handle: AnyWindowHandle, dir: &TempDir) {
    let path = dir.0.display().to_string();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("sync-path", cx);
        window.input(&path, cx);
        window.click("sync-password", cx);
        window.input(PASSWORD, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

fn host(name: &str, address: &str) -> Host {
    Host {
        name: name.into(),
        address: address.into(),
        port: 22,
        username: "ops".into(),
        ..Default::default()
    }
}

#[gpui_kit::test]
fn first_device_sets_up_a_folder_syncs_and_stops(cx: &mut TestAppContext) {
    let dir = TempDir::new();
    let (handle, opened, page) = open(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("sync-indicator").is_none(),
            "没有设置同步时不显示同步图标"
        );
        assert!(
            window.try_find("sync-password-confirm").is_none(),
            "还不知道是不是第一台设备时只输入一次"
        );
    })
    .unwrap();
    fill_form(cx, handle, &dir);
    cx.update_window(handle, |_, window, cx| window.click("sync-start", cx))
        .unwrap();
    // 同步位置中还没有同步文件：这是第一台设备，要再次输入同步密码，什么都还没保存
    wait_until(cx, handle, "要求再次输入同步密码", |window, _| {
        window.try_find("sync-password-confirm").is_some()
    });
    assert!(cx.read(|cx| cloud(cx).status().config.is_none()));
    assert!(!dir.0.join(FILE_NAME).exists());

    cx.update_window(handle, |_, window, cx| {
        window.click("sync-password-confirm", cx);
        window.input(PASSWORD, cx);
        window.render_frame(cx);
        window.click("sync-start", cx);
    })
    .unwrap();
    // 核心的状态先变，页面在自己的任务里随后收尾（清掉“进行中”）：收尾之前的点击会被忽略
    wait_until(cx, handle, "设置并同步", |window, cx| {
        let status = cloud(cx).status();
        status.last_sync.is_some()
            && !status.syncing
            && !page.read(cx).is_busy()
            && window.try_find("sync-now").is_some()
    });
    assert!(dir.0.join(FILE_NAME).is_file(), "同步文件写到了文件夹中");
    let status = cx.read(|cx| cloud(cx).status());
    assert_eq!(
        status.config.as_ref().map(|c| &c.target),
        Some(&SyncTarget::Folder {
            path: dir.0.clone()
        })
    );
    assert_eq!(status.devices.len(), 1, "设备列表中有本机");
    assert!(status.last_error.is_none());
    let first = status.last_sync;

    // 标题栏出现同步图标，点击时请工作区打开设置的“同步”页
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("sync-indicator", cx);
    })
    .unwrap();
    assert_eq!(opened.get(), 1);

    cx.update_window(handle, |_, window, cx| window.click("sync-now", cx))
        .unwrap();
    wait_until(cx, handle, "立即同步", |_, cx| {
        let status = cloud(cx).status();
        !status.syncing && status.last_sync > first && !page.read(cx).is_busy()
    });

    // 停止同步：确认后回到设置表单，同步位置中的文件不动
    cx.update_window(handle, |_, window, cx| window.click("sync-disable", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("ok", cx);
    })
    .unwrap();
    wait_until(cx, handle, "停止同步", |window, cx| {
        cloud(cx).status().config.is_none() && window.try_find("sync-start").is_some()
    });
    assert!(dir.0.join(FILE_NAME).is_file());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("sync-indicator").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn first_sync_with_data_on_both_sides_asks_how_to_sync(cx: &mut TestAppContext) {
    let dir = TempDir::new();
    // 另一台电脑先把它的一台主机同步到文件夹中
    let db = Database::open_in_memory().expect("内存数据库");
    let vault = Vault::open(&db, Box::new(MemoryKeyStore::default())).expect("保险库");
    let other = Arc::new(Store::open(db, vault).expect("存储"));
    other
        .create(host("db", "10.0.0.2"), None, Position::Last)
        .unwrap();
    let other_sync = CloudSync::new(other, None);
    termlet_core::runtime::runtime().block_on(async {
        other_sync
            .setup(SetupInput {
                target: SyncTarget::Folder {
                    path: dir.0.clone(),
                },
                credential: SecretString::default(),
                password: SecretString::new(PASSWORD),
                device_name: "other".into(),
            })
            .await
            .expect("另一台电脑设置同步");
        other_sync.sync_now().await.expect("另一台电脑同步");
    });

    let (handle, _, _) = open(cx);
    cx.update(|cx| {
        AppState::store(cx)
            .create(host("web", "10.0.0.1"), None, Position::Last)
            .unwrap()
    });
    fill_form(cx, handle, &dir);
    cx.update_window(handle, |_, window, cx| window.click("sync-start", cx))
        .unwrap();
    // 已有同步文件：输入一次同步密码即可；两边都有数据，同步前请用户选择（默认合并）
    wait_until(cx, handle, "首次同步的选择", |window, _| {
        window.try_find("sync-pending-submit").is_some()
    });
    let status = cx.read(|cx| cloud(cx).status());
    assert!(matches!(
        status.pending,
        Some(Pending::FirstSync { duplicates: 0, .. })
    ));
    assert_eq!(cx.read(|cx| AppState::store(cx).list::<Host>().len()), 1);

    cx.update_window(handle, |_, window, cx| {
        window.click("sync-pending-submit", cx)
    })
    .unwrap();
    wait_until(cx, handle, "合并两边", |window, cx| {
        let status = cloud(cx).status();
        status.pending.is_none()
            && status.last_sync.is_some()
            && !status.syncing
            && window.try_find("sync-pending-submit").is_none()
    });
    let mut names: Vec<String> = cx.read(|cx| {
        AppState::store(cx)
            .list::<Host>()
            .into_iter()
            .map(|r| r.doc.name)
            .collect()
    });
    names.sort();
    assert_eq!(names, ["db", "web"], "合并后两边的主机都在本机");
    let status = cx.read(|cx| cloud(cx).status());
    assert_eq!(status.devices.len(), 2, "设备列表中有两台电脑");
}

// WebDAV、S3 的地址是 http:// 时显示“允许不安全的 HTTP”，不勾选时测试连接报告不安全；S3、Gist 有各自的字段
#[gpui_kit::test]
fn http_addresses_need_explicit_permission(cx: &mut TestAppContext) {
    let (handle, _, page) = open(cx);
    // 没有在监听的端口：勾选之后连接失败（而不是报告不安全）
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("sync-target").click(1usize, cx);
        window.render_frame(cx);
        assert!(window.try_find("sync-allow-http").is_none());
        window.click("sync-url", cx);
        window.input(&format!("http://127.0.0.1:{closed}/dav/"), cx);
        window.click("sync-password", cx);
        window.input(PASSWORD, cx);
        window.render_frame(cx);
        assert!(
            window.try_find("sync-allow-http").is_some(),
            "http:// 时显示允许不安全的 HTTP 与风险"
        );
        window.click("sync-test", cx);
    })
    .unwrap();
    let insecure = cx.read(|_| {
        describe(
            &SyncError::Invalid {
                field: "url".into(),
                reason: "insecure".into(),
            },
            Service::WebDav,
        )
    });
    wait_until(cx, handle, "报告地址不安全", |_, cx| {
        !page.read(cx).is_busy() && page.read(cx).form_error().as_ref() == Some(&insecure)
    });
    cx.update_window(handle, |_, window, cx| {
        window.click("sync-allow-http", cx);
        window.render_frame(cx);
        window.click("sync-test", cx);
    })
    .unwrap();
    wait_until(cx, handle, "勾选后去连接", |_, cx| {
        let page = page.read(cx);
        !page.is_busy() && page.form_error().is_some_and(|e| e != insecure)
    });
    assert!(
        cx.read(|cx| cloud(cx).status().config.is_none()),
        "什么都没保存"
    );

    cx.update_window(handle, |_, window, cx| {
        window.within("sync-target").click(2usize, cx);
        window.render_frame(cx);
        for id in [
            "sync-s3-endpoint",
            "sync-s3-bucket",
            "sync-s3-region",
            "sync-s3-prefix",
            "sync-s3-access-key",
            "sync-s3-secret-key",
            "sync-s3-path-style",
        ] {
            assert!(window.try_find(id).is_some(), "S3 的字段 {id}");
        }
        window.within("sync-target").click(3usize, cx);
        window.render_frame(cx);
        assert!(window.try_find("sync-gist-token").is_some());
        assert!(window.try_find("sync-gist-id").is_some());
        assert!(window.try_find("sync-s3-endpoint").is_none());
    })
    .unwrap();
}
