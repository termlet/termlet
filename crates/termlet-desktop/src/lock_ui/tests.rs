//! 应用锁、设置页与崩溃记录的 UI 集成测试：在无头窗口中渲染生产环境的视图，经真实的事件路径操作并检查结果
//! （按元素 ID 查找，与界面语言无关）。系统功能换成假的（[`FakePlatform`]），更新不访问网络（`FeedSource::Off`）。

use std::cell::Cell;
use std::num::NonZeroIsize;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Context, Entity, FocusHandle,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _, Subscription,
    TestAppContext, Window, WindowBounds, WindowOptions, div, px, size,
};
use termlet_core::applock::{AppLock, LockPlatform, Verification};
use termlet_core::crash::CrashLog;
use termlet_core::model::{LocalSettings, Settings};
use termlet_core::secret::SecretString;
use termlet_core::update::UpdateStatus;
use termlet_core::vault::{KeyStore as _, MemoryKeyStore, Remember};

use super::{AboutExtras, Endpoint, LockScreen, SecurityPage, Setup};
use crate::app_state::AppState;
use crate::testkit::wait_until;
use crate::ui_scale::DialogTitleExt as _;

/// 假的系统功能：Windows Hello 是否可用、验证的结果可以在测试中改。
struct FakePlatform {
    available: bool,
    verdict: Mutex<Verification>,
}

impl FakePlatform {
    fn new(available: bool) -> Arc<Self> {
        Arc::new(Self {
            available,
            verdict: Mutex::new(Verification::Verified),
        })
    }

    fn set_verdict(&self, verdict: Verification) {
        *self.verdict.lock().unwrap() = verdict;
    }
}

impl LockPlatform for FakePlatform {
    fn idle_time(&self) -> Option<Duration> {
        None
    }

    fn session_locked(&self) -> Option<bool> {
        None
    }

    fn system_auth_available(&self) -> bool {
        self.available
    }

    fn system_verify(&self, _: Option<NonZeroIsize>, _: &str) -> Verification {
        *self.verdict.lock().unwrap()
    }
}

/// 测试的临时目录，测试结束时删除。
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 装好应用状态与 [`super::Services`]，返回这个测试的临时目录（崩溃记录在其中的 `logs`）。
fn setup(cx: &mut TestAppContext, platform: Arc<FakePlatform>) -> TempDir {
    // 应用锁的哈希、崩溃记录的读写在核心的运行时上，不受测试调度器管理
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    cx.update(|cx| cx.set_reduce_motion(true));
    let dir = std::env::temp_dir().join(format!(
        "termlet-lock-ui-test-{}",
        termlet_core::id::random_token(8)
    ));
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let updates = dir.join("updates");
    cx.update(|cx| {
        let lock = AppLock::open_with(AppState::store(cx), platform).ok();
        super::install_with(
            Setup {
                lock,
                updater: super::offline_updater(updates),
                logs_dir: logs,
                remote: None,
            },
            cx,
        );
    });
    TempDir(dir)
}

fn open<V: Render>(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut Window, &mut Context<V>) -> V + 'static,
) -> (AnyWindowHandle, Entity<V>) {
    cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1024.), px(720.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| build(window, cx)),
        )
        .expect("打开测试窗口")
    })
}

fn secret(text: &str) -> SecretString {
    SecretString::new(text.to_string())
}

/// 用核心的接口直接设置锁定密码。
fn enable_lock(cx: &mut TestAppContext, password: &str) -> AppLock {
    let lock = cx.update(|cx| super::app_lock(cx)).expect("应用锁");
    termlet_core::runtime::runtime()
        .block_on(lock.set_password(None, &secret(password)))
        .expect("设置锁定密码");
    cx.update(super::sync_lock_model);
    lock
}

/// 模仿主会话：锁定时整个窗口换成锁屏，解锁后把焦点还回去。
struct Harness {
    focus: FocusHandle,
    screen: Option<Entity<LockScreen>>,
    _observe: Subscription,
}

impl Harness {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = super::lock_model(cx).expect("锁定状态");
        let observe = cx.observe_in(&model, window, |this, model, window, cx| {
            let locked = model.read(cx).is_locked();
            this.sync(locked, window, cx);
        });
        let locked = model.read(cx).is_locked();
        let mut this = Self {
            focus: cx.focus_handle(),
            screen: None,
            _observe: observe,
        };
        this.sync(locked, window, cx);
        this
    }

    fn sync(&mut self, locked: bool, window: &mut Window, cx: &mut Context<Self>) {
        match (locked, self.screen.take()) {
            (true, None) => self.screen = Some(cx.new(|cx| LockScreen::new(window, cx))),
            (true, Some(screen)) => self.screen = Some(screen),
            (false, Some(screen)) => {
                screen.update(cx, |screen, cx| screen.restore_focus(window, cx));
            }
            (false, None) => {}
        }
        cx.notify();
    }
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        match &self.screen {
            Some(screen) => screen.clone().into_any_element(),
            None => div()
                .id("harness-content")
                .size_full()
                .track_focus(&self.focus)
                .child(Button::new("harness-button").label("content"))
                .into_any_element(),
        }
    }
}

fn screen_state(cx: &mut TestAppContext, harness: &Entity<Harness>) -> Option<(String, bool)> {
    cx.read(|cx| {
        harness.read(cx).screen.as_ref().map(|screen| {
            let (text, busy) = screen.read(cx).snapshot(cx);
            (text.to_string(), busy)
        })
    })
}

#[gpui_kit::test]
fn lock_screen_unlocks_with_the_password_and_clears_a_wrong_one(cx: &mut TestAppContext) {
    let _dir = setup(cx, FakePlatform::new(false));
    assert!(!cx.update(super::lock_now), "没有启用时不锁定");
    let lock = enable_lock(cx, "secret-1");
    let (handle, harness) = open(cx, Harness::new);

    // 锁定前焦点在内容上；锁定后整个窗口换成锁屏，焦点在密码框
    cx.update_window(handle, |_, window, cx| {
        let focus = harness.read(cx).focus.clone();
        window.focus(&focus, cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(cx.update(super::lock_now));
    assert!(cx.update(|cx| super::is_locked(cx)));
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("lock-screen").is_some(), "盖上锁屏");
        assert!(window.try_find("harness-button").is_none(), "内容不渲染");
        assert!(
            window.try_find("lock-screen-system").is_none(),
            "Windows Hello 不可用时不显示"
        );
        window.input("wrong-pass", cx);
        window.press("enter", cx);
    })
    .unwrap();
    wait_until(cx, handle, "提示锁定密码错误", |window, _| {
        window.try_find("lock-screen-error").is_some()
    });
    assert_eq!(
        screen_state(cx, &harness),
        Some((String::new(), false)),
        "输错后清空密码框"
    );
    assert!(lock.is_locked());

    cx.update_window(handle, |_, window, cx| {
        window.input("secret-1", cx);
        window.press("enter", cx);
    })
    .unwrap();
    wait_until(cx, handle, "解锁后去掉锁屏", |window, _| {
        window.try_find("harness-button").is_some()
    });
    assert!(!lock.is_locked());
    cx.update_window(handle, |_, window, cx| {
        assert!(
            harness.read(cx).focus.is_focused(window),
            "焦点回到锁定前的位置"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn lock_screen_covers_open_dialogs_and_keeps_them(cx: &mut TestAppContext) {
    let _dir = setup(cx, FakePlatform::new(false));
    enable_lock(cx, "secret-1");
    let (handle, _) = open(cx, Harness::new);
    let clicked = Rc::new(Cell::new(false));
    cx.update_window(handle, |_, window, cx| {
        let clicked = clicked.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let clicked = clicked.clone();
            dialog.titled("dialog").child(
                Button::new("dialog-button")
                    .label("press")
                    .on_click(move |_, _, _| clicked.set(true)),
            )
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(super::lock_now);
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("lock-screen").is_some());
        // 对话框还在，但被锁屏盖住：点不到，Esc 也关不掉；点了之后焦点仍在密码框
        window.input("secret", cx);
        window.click("dialog-button", cx);
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(window.try_find("dialog-button").is_some(), "对话框原样还在");
        window.input("-1", cx);
        window.press("enter", cx);
    })
    .unwrap();
    assert!(!clicked.get(), "锁定期间点不到对话框");
    wait_until(cx, handle, "解锁", |window, _| {
        window.try_find("lock-screen").is_none()
    });
    cx.update_window(handle, |_, window, cx| {
        window.click("dialog-button", cx);
    })
    .unwrap();
    assert!(clicked.get(), "解锁后对话框照常可用");
}

#[gpui_kit::test]
fn lock_screen_unlocks_with_windows_hello(cx: &mut TestAppContext) {
    let platform = FakePlatform::new(true);
    let _dir = setup(cx, platform.clone());
    let lock = enable_lock(cx, "secret-1");
    let (handle, harness) = open(cx, Harness::new);
    cx.update(super::lock_now);
    wait_until(cx, handle, "检查完 Windows Hello", |window, _| {
        window.try_find("lock-screen-system").is_some()
    });

    // 用户取消：不提示
    platform.set_verdict(Verification::Canceled);
    cx.update_window(handle, |_, window, cx| {
        window.click("lock-screen-system", cx)
    })
    .unwrap();
    wait_until(cx, handle, "取消后不再忙", |_, cx| {
        harness
            .read(cx)
            .screen
            .as_ref()
            .is_some_and(|s| !s.read(cx).snapshot(cx).1)
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("lock-screen-error").is_none(), "取消不提示");
    })
    .unwrap();
    assert!(lock.is_locked());

    // 验证失败：提示
    platform.set_verdict(Verification::Failed);
    cx.update_window(handle, |_, window, cx| {
        window.click("lock-screen-system", cx)
    })
    .unwrap();
    wait_until(cx, handle, "提示系统验证未通过", |window, _| {
        window.try_find("lock-screen-error").is_some()
    });
    assert!(lock.is_locked());

    platform.set_verdict(Verification::Verified);
    cx.update_window(handle, |_, window, cx| {
        window.click("lock-screen-system", cx)
    })
    .unwrap();
    wait_until(cx, handle, "验证通过后解锁", |window, _| {
        window.try_find("harness-button").is_some()
    });
    assert!(!lock.is_locked());
}

/// 只放设置页的视图。
struct PageHarness<V: 'static> {
    page: Entity<V>,
}

impl<V: Render> Render for PageHarness<V> {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.page.clone())
    }
}

fn security_state(cx: &mut TestAppContext, page: &Entity<SecurityPage>) -> (Option<String>, bool) {
    cx.read(|cx| page.read(cx).snapshot())
}

/// 点一个输入框，换成新的内容。
fn type_into(window: &mut Window, id: &'static str, text: &str, cx: &mut gpui_kit::App) {
    window.click(id, cx);
    window.press("ctrl-a", cx);
    window.input(text, cx);
}

#[gpui_kit::test]
fn security_page_enables_changes_and_disables_the_lock(cx: &mut TestAppContext) {
    let _dir = setup(cx, FakePlatform::new(false));
    let local = cx.update(|cx| AppState::store(cx).local_settings());
    let (handle, harness) = open(cx, move |window, cx| PageHarness {
        page: cx.new(|cx| SecurityPage::new(&local, window, cx)),
    });
    let page = cx.read(|cx| harness.read(cx).page.clone());
    let enabled = |cx: &mut TestAppContext| cx.update(|cx| super::is_enabled(cx));

    // 启用：两次输入不一致、过短时不提交
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("security-idle").is_none(),
            "没启用时不显示三项设置"
        );
        window.click("security-enable", cx);
        window.render_frame(cx);
        assert!(window.try_find("security-current").is_none());
        type_into(window, "security-new", "abc", cx);
        type_into(window, "security-confirm", "abd", cx);
        window.click("security-submit", cx);
    })
    .unwrap();
    assert_eq!(security_state(cx, &page), (Some("Mismatch".into()), false));
    cx.update_window(handle, |_, window, cx| {
        type_into(window, "security-confirm", "abc", cx);
        window.press("enter", cx);
    })
    .unwrap();
    assert_eq!(security_state(cx, &page), (Some("TooShort".into()), false));
    cx.update_window(handle, |_, window, cx| {
        type_into(window, "security-new", "secret-1", cx);
        type_into(window, "security-confirm", "secret-1", cx);
        window.click("security-submit", cx);
    })
    .unwrap();
    // 应用锁的状态先变，页面的结果提示在它自己的任务里随后写上：两者都到了才算完成
    wait_until(cx, handle, "启用应用锁", |window, cx| {
        window.try_find("security-idle").is_some()
            && page.read(cx).snapshot().0.as_deref() == Some("Enabled")
    });
    assert!(enabled(cx));
    assert_eq!(security_state(cx, &page).0.as_deref(), Some("Enabled"));

    // 修改密码：当前密码错误时提示
    cx.update_window(handle, |_, window, cx| {
        window.click("security-change", cx);
        window.render_frame(cx);
        type_into(window, "security-current", "wrong-pass", cx);
        type_into(window, "security-new", "secret-2", cx);
        type_into(window, "security-confirm", "secret-2", cx);
        window.click("security-submit", cx);
    })
    .unwrap();
    wait_until(cx, handle, "提示当前密码错误", |_, cx| {
        !page.read(cx).snapshot().1
    });
    assert_eq!(
        security_state(cx, &page).0.as_deref(),
        Some("WrongPassword")
    );
    cx.update_window(handle, |_, window, cx| {
        type_into(window, "security-current", "secret-1", cx);
        window.click("security-submit", cx);
    })
    .unwrap();
    wait_until(cx, handle, "修改密码", |_, cx| {
        page.read(cx).snapshot().0.as_deref() == Some("Changed")
    });

    // 停用：要新的密码
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("security-disable", cx);
        window.render_frame(cx);
        assert!(window.try_find("security-new").is_none());
        type_into(window, "security-current", "secret-2", cx);
        window.click("security-submit", cx);
    })
    .unwrap();
    wait_until(cx, handle, "停用应用锁", |window, cx| {
        window.try_find("security-enable").is_some()
            && page.read(cx).snapshot().0.as_deref() == Some("Disabled")
    });
    assert!(!enabled(cx));
    assert_eq!(security_state(cx, &page).0.as_deref(), Some("Disabled"));
}

/// 主密码模式（钥匙串不可用）下“安全”页有“主密码”一节：两次输入不一致、太短、当前主密码不对时提示，
/// 改好后用新的主密码解锁、旧的不行；钥匙串模式下没有这一节。
#[gpui_kit::test]
fn security_page_changes_the_master_password(cx: &mut TestAppContext) {
    // 钥匙串模式：没有主密码可改
    let _dir = setup(cx, FakePlatform::new(false));
    let local = cx.update(|cx| AppState::store(cx).local_settings());
    let (handle, _) = open(cx, move |window, cx| PageHarness {
        page: cx.new(|cx| SecurityPage::new(&local, window, cx)),
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("master-password").is_none());
    })
    .unwrap();

    // 主密码模式
    cx.update(|cx| {
        AppState::install_in_memory_with(
            Box::new(termlet_core::vault::MemoryKeyStore::unavailable()),
            cx,
        );
        AppState::store(cx)
            .set_vault_password(&secret("old master pw"))
            .unwrap();
    });
    let local = cx.update(|cx| AppState::store(cx).local_settings());
    let (handle, harness) = open(cx, move |window, cx| PageHarness {
        page: cx.new(|cx| SecurityPage::new(&local, window, cx)),
    });
    let master = cx.read(|cx| harness.read(cx).page.read(cx).master().clone());
    let state = |cx: &mut TestAppContext| cx.read(|cx| master.read(cx).snapshot());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("master-change", cx);
        window.render_frame(cx);
        type_into(window, "master-current", "old master pw", cx);
        type_into(window, "master-new", "new master pw", cx);
        type_into(window, "master-confirm", "new master px", cx);
        window.click("master-submit", cx);
    })
    .unwrap();
    assert_eq!(state(cx), (Some("Mismatch".into()), false));
    cx.update_window(handle, |_, window, cx| {
        type_into(window, "master-new", "short", cx);
        type_into(window, "master-confirm", "short", cx);
        window.press("enter", cx);
    })
    .unwrap();
    assert_eq!(state(cx), (Some("TooShort".into()), false));
    cx.update_window(handle, |_, window, cx| {
        type_into(window, "master-current", "wrong master pw", cx);
        type_into(window, "master-new", "new master pw", cx);
        type_into(window, "master-confirm", "new master pw", cx);
        window.click("master-submit", cx);
    })
    .unwrap();
    wait_until(cx, handle, "提示当前主密码不对", |_, cx| {
        let (message, busy) = master.read(cx).snapshot();
        !busy && message.is_some()
    });
    assert_eq!(state(cx).0.as_deref(), Some("WrongPassword"));
    cx.update_window(handle, |_, window, cx| {
        type_into(window, "master-current", "old master pw", cx);
        window.click("master-submit", cx);
    })
    .unwrap();
    wait_until(cx, handle, "修改主密码", |window, cx| {
        master.read(cx).snapshot().0.as_deref() == Some("Changed")
            && window.try_find("master-current").is_none()
    });
    cx.update(|cx| {
        let store = AppState::store(cx);
        assert!(store.unlock_vault(&secret("old master pw")).is_err());
        store.unlock_vault(&secret("new master pw")).unwrap();
    });
}

/// 便携版在这台电脑上记住了主密码时，“主密码”一节多一行说明与“忘记”：忘记后删掉记住的主密钥（下次启动重新询问），
/// 这一行随之不见；没记住时没有这一行。
#[gpui_kit::test]
fn security_page_forgets_the_remembered_master_password(cx: &mut TestAppContext) {
    let _dir = setup(cx, FakePlatform::new(false));
    let remembered = Arc::new(MemoryKeyStore::default());
    {
        let remembered = remembered.clone();
        cx.update(move |cx| {
            AppState::install_in_memory_locked("master pw 1", Some(Box::new(remembered)), cx);
            AppState::store(cx)
                .unlock_vault(&secret("master pw 1"))
                .unwrap();
        });
    }
    let local = cx.update(|cx| AppState::store(cx).local_settings());
    let (handle, harness) = open(cx, move |window, cx| PageHarness {
        page: cx.new(|cx| SecurityPage::new(&local, window, cx)),
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("master-password").is_some());
        assert!(window.try_find("master-forget").is_none(), "还没记住");
    })
    .unwrap();

    cx.update(|cx| AppState::store(cx).remember_vault_key().unwrap());
    let master = cx.read(|cx| harness.read(cx).page.read(cx).master().clone());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("master-forget", cx);
    })
    .unwrap();
    wait_until(cx, handle, "忘记记住的主密钥", |window, cx| {
        master.read(cx).snapshot() == (Some("Forgotten".into()), false)
            && window.try_find("master-forget").is_none()
    });
    assert!(remembered.load().unwrap().is_none());
    assert_eq!(
        cx.update(|cx| AppState::store(cx).vault_remember()),
        Remember::Available
    );
    assert!(
        cx.update(|cx| AppState::store(cx).vault().is_unlocked()),
        "这次照常可用"
    );
}

#[gpui_kit::test]
fn security_page_writes_the_three_options_on_save(cx: &mut TestAppContext) {
    let _dir = setup(cx, FakePlatform::new(false));
    enable_lock(cx, "secret-1");
    let local = cx.update(|cx| AppState::store(cx).local_settings());
    let (handle, harness) = open(cx, move |window, cx| PageHarness {
        page: cx.new(|cx| SecurityPage::new(&local, window, cx)),
    });
    let page = cx.read(|cx| harness.read(cx).page.clone());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("security-on-startup", cx);
        window.click("security-on-system-lock", cx);
    })
    .unwrap();
    let mut local = LocalSettings::default();
    cx.read(|cx| page.read(cx).apply(&mut local));
    assert!(!local.app_lock.on_startup);
    assert!(!local.app_lock.on_system_lock);
    assert_eq!(local.app_lock.idle_minutes, 15, "没改的照旧");
}

#[gpui_kit::test]
fn about_extras_show_update_state_and_write_drafts(cx: &mut TestAppContext) {
    let _dir = setup(cx, FakePlatform::new(false));
    let (settings, local) = cx.update(|cx| {
        let store = AppState::store(cx);
        (store.settings(), store.local_settings())
    });
    let (handle, harness) = open(cx, move |window, cx| PageHarness {
        page: cx.new(|cx| AboutExtras::new(&settings, &local, window, cx)),
    });
    let page = cx.read(|cx| harness.read(cx).page.clone());
    let model = cx.update(|cx| super::update_model(cx)).unwrap();

    // 启动后的检查：更新源关闭，不访问网络
    wait_until(cx, handle, "检查完更新", |_, cx| {
        model.read(cx).state().status == UpdateStatus::Disabled
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("update-status").is_some());
        assert!(window.try_find("update-restart").is_none());
        window.click("update-check", cx);
        window.click("about-auto-update", cx);
    })
    .unwrap();
    assert_eq!(
        cx.read(|cx| model.read(cx).crash_endpoint().clone()),
        Endpoint::Loaded(None)
    );

    // 下载中：进度条旁边写已下载 / 总大小
    let before = cx.read(|cx| model.read(cx).state().clone());
    cx.update(|cx| {
        model.update(cx, |m, cx| {
            let state = termlet_core::update::UpdateState {
                status: UpdateStatus::Downloading {
                    version: "9.9.9".into(),
                    percent: 45,
                    downloaded: 450 * 1024,
                    total: Some(1000 * 1024),
                },
                ..before.clone()
            };
            m.set_state(state, std::time::Instant::now(), cx)
        })
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("update-progress").is_some());
        assert!(window.try_find("update-progress-text").is_some());
    })
    .unwrap();
    cx.update(|cx| {
        model.update(cx, |m, cx| {
            m.set_state(before.clone(), std::time::Instant::now(), cx)
        })
    });

    // “隐私与诊断”页：没有接收地址时不显示自动发送崩溃报告
    cx.update(|cx| page.update(cx, |p, cx| p.set_part(super::AboutPart::Privacy, cx)));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("update-status").is_none(), "更新在“关于”页");
        assert!(
            window.try_find("about-crash-report").is_none(),
            "没有接收地址时不显示自动发送崩溃报告"
        );
    })
    .unwrap();

    // 有接收地址：显示开关
    cx.update(|cx| {
        model.update(cx, |m, cx| {
            m.set_endpoint(
                Endpoint::Loaded(Some("https://crash.example.com/report".into())),
                cx,
            )
        })
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("about-crash-report", cx);
    })
    .unwrap();
    // 日志的详细程度：选“详细”
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("diag-log-level", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("down", cx);
        window.press("enter", cx);
        window.render_frame(cx);
    })
    .unwrap();
    let (mut settings, mut local) = (Settings::default(), LocalSettings::default());
    cx.read(|cx| page.read(cx).apply(&mut settings, &mut local, cx));
    assert!(!settings.general.auto_check_update);
    assert!(local.logs.crash_report);
    assert_eq!(local.logs.level, termlet_core::model::LogLevel::Detailed);
}

/// 写一份崩溃记录（与核心的文件格式相同）。
fn write_crash(logs: &Path, id: &str, created_at: i64) {
    let json = format!(
        r#"{{"v":1,"createdAt":{created_at},"seen":false,"upload":"none","report":{{"id":"{id}","time":"2026-10-20T05:10:00.000Z","version":"1.0.0","platform":"windows","arch":"x86_64","osRelease":"10.0.26200","process":"desktop","reason":"panic","thread":"main","location":"src/main.rs:1:1","message":"boom","stack":"0: main\n"}}}}"#
    );
    std::fs::write(logs.join(format!("{id}.json")), json).unwrap();
}

#[gpui_kit::test]
fn crashes_are_announced_once_and_can_be_deleted(cx: &mut TestAppContext) {
    let dir = setup(cx, FakePlatform::new(false));
    let logs = dir.0.join("logs");
    write_crash(&logs, "crash-20261020-051000123-aaaa", 1_760_937_000_123);
    write_crash(&logs, "crash-20261020-061000123-bbbb", 1_760_940_600_123);
    let (handle, _) = open(cx, |_, _| EmptyView);
    cx.update_window(handle, |_, window, cx| super::check_crashes(window, cx))
        .unwrap();
    wait_until(cx, handle, "提示上次运行时出错了", |window, _| {
        window.try_find("crash-notice-view").is_some()
    });
    let crash = CrashLog::new(logs.clone());
    let runtime = termlet_core::runtime::runtime();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !runtime.block_on(crash.unseen()).is_empty() {
        assert!(std::time::Instant::now() < deadline, "等待超时：标为已提示");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(30));
    }
    // 再调用一次（另一个窗口）不再提示
    cx.update_window(handle, |_, window, cx| super::check_crashes(window, cx))
        .unwrap();

    cx.update_window(handle, |_, window, cx| {
        window.click("crash-notice-view", cx)
    })
    .unwrap();
    wait_until(cx, handle, "列出崩溃记录", |window, _| {
        window
            .try_find("crash-record-crash-20261020-061000123-bbbb")
            .is_some()
            && window
                .try_find("crash-record-crash-20261020-051000123-aaaa")
                .is_some()
    });
    cx.update_window(handle, |_, window, cx| {
        window.click("crash-delete-crash-20261020-061000123-bbbb", cx)
    })
    .unwrap();
    wait_until(cx, handle, "删除一份", |window, _| {
        window
            .try_find("crash-record-crash-20261020-061000123-bbbb")
            .is_none()
    });
    assert_eq!(runtime.block_on(crash.list()).len(), 1);
    // 复制的提示：删除后清掉（说的可能是已删除的记录）
    cx.update_window(handle, |_, window, cx| {
        window.click("crash-copy-crash-20261020-051000123-aaaa", cx);
        assert_eq!(
            crate::notify::shown(window, cx),
            Some(crate::notify::Notice::success(crate::t!(
                "lock_ui.crash.copied"
            )))
        );
    })
    .unwrap();
    // 全部删除：先确认，取消时什么也不删
    cx.update_window(handle, |_, window, cx| window.click("crash-delete-all", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("cancel", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(runtime.block_on(crash.list()).len(), 1, "取消后不删除");
    cx.update_window(handle, |_, window, cx| window.click("crash-delete-all", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("ok", cx);
    })
    .unwrap();
    wait_until(cx, handle, "全部删除", |window, _| {
        window.try_find("crash-empty").is_some()
    });
    assert!(runtime.block_on(crash.list()).is_empty());
    cx.update_window(handle, |_, window, cx| {
        assert_eq!(
            crate::notify::shown(window, cx),
            None,
            "删除后清掉复制的提示"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn native_crash_shows_its_dump_and_deleting_removes_it(cx: &mut TestAppContext) {
    let dir = setup(cx, FakePlatform::new(false));
    let logs = dir.0.join("logs");
    let id = "crash-20261027-010000000-cccc";
    let dump = "termlet-20261027-010000000-4242.dmp";
    std::fs::create_dir_all(logs.join(termlet_core::crash::DUMP_DIR)).unwrap();
    let dump_path = logs.join(termlet_core::crash::DUMP_DIR).join(dump);
    std::fs::write(&dump_path, b"MDMP").unwrap();
    let json = format!(
        r#"{{"v":1,"createdAt":1761526800000,"seen":true,"upload":"none","report":{{"id":"{id}","time":"2026-10-27T01:00:00.000Z","version":"1.0.0","platform":"windows","arch":"x86_64","osRelease":"10.0.26200","process":"desktop","reason":"native","exitCode":3221225477,"message":"0xC0000005 访问冲突（ACCESS_VIOLATION）","dump":"{dump}"}}}}"#
    );
    std::fs::write(logs.join(format!("{id}.json")), json).unwrap();
    let (handle, _) = open(cx, |_, _| EmptyView);
    cx.update_window(handle, |_, window, cx| super::open_crash_log(window, cx))
        .unwrap();
    let reveal = gpui_kit::SharedString::from(format!("crash-reveal-dump-{id}"));
    wait_until(
        cx,
        handle,
        "列出原生崩溃与它的转储",
        |window, _| window.try_find(reveal.clone()).is_some(),
    );
    cx.update_window(handle, |_, window, cx| {
        window.click(
            gpui_kit::SharedString::from(format!("crash-delete-{id}")),
            cx,
        )
    })
    .unwrap();
    wait_until(cx, handle, "删除记录", |window, _| {
        window.try_find("crash-empty").is_some()
    });
    assert!(!dump_path.exists(), "转储随记录一起删除");
}

/// 复制的版本信息：版本（便携版注明），系统的名称、版本与架构（名称、版本读不到时用系统的代号）。
#[test]
fn version_info_names_the_system() {
    use termlet_core::diag::SystemInfo;
    let windows = SystemInfo {
        os: "windows".into(),
        arch: "x86_64".into(),
        version: "10.0.26200.6899".into(),
        name: "Windows 11 Pro 25H2".into(),
        cpus: 8,
    };
    assert_eq!(
        super::about::version_info("0.1.2", false, &windows),
        "Termlet 0.1.2\nWindows 11 Pro 25H2 10.0.26200.6899 (windows, x86_64)"
    );
    assert_eq!(
        super::about::version_info("0.1.2", true, &windows),
        "Termlet 0.1.2 (portable)\nWindows 11 Pro 25H2 10.0.26200.6899 (windows, x86_64)"
    );
    let unknown = SystemInfo {
        os: "linux".into(),
        arch: "aarch64".into(),
        ..Default::default()
    };
    assert_eq!(
        super::about::version_info("0.1.2", false, &unknown),
        "Termlet 0.1.2\nlinux (linux, aarch64)"
    );
    let here = super::about::version_info(
        termlet_core::VERSION,
        false,
        termlet_core::diag::system_info(),
    );
    assert!(here.starts_with(&format!("Termlet {}\n", termlet_core::VERSION)));
    assert!(here.contains(std::env::consts::ARCH));
}

/// 第三方许可的目录：Windows、Linux 在程序旁边，macOS 在应用包的 Contents/Resources 下；都没有（开发版）时不显示。
#[test]
fn third_party_licenses_next_to_the_program() {
    let root = std::env::temp_dir().join(format!(
        "termlet-licenses-{}",
        termlet_core::id::random_token(8)
    ));
    let flat = root.join("flat");
    std::fs::create_dir_all(flat.join("licenses")).unwrap();
    assert_eq!(
        super::about::licenses_dir(&flat.join("Termlet.exe")),
        Some(flat.join("licenses"))
    );
    let app = root.join("Termlet.app").join("Contents");
    std::fs::create_dir_all(app.join("MacOS")).unwrap();
    std::fs::create_dir_all(app.join("Resources").join("licenses")).unwrap();
    let found = super::about::licenses_dir(&app.join("MacOS").join("Termlet")).unwrap();
    assert_eq!(
        found.canonicalize().unwrap(),
        app.join("Resources")
            .join("licenses")
            .canonicalize()
            .unwrap()
    );
    let bare = root.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(super::about::licenses_dir(&bare.join("Termlet.exe")), None);
    let _ = std::fs::remove_dir_all(&root);
}

struct EmptyView;

impl Render for EmptyView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}

// 锁定时清空设置里输入到一半的密码：修改锁定密码的表单收起，解锁后要重新输入
#[gpui_kit::test]
fn locking_clears_passwords_typed_in_settings(cx: &mut TestAppContext) {
    let _dir = setup(cx, FakePlatform::new(false));
    enable_lock(cx, "secret-1");
    let (handle, _form) = open(cx, |window, cx| {
        crate::settings::SettingsForm::new(crate::settings::Page::Security, window, cx)
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("security-change", cx);
        window.render_frame(cx);
        window.click("security-current", cx);
        window.input("secret-1", cx);
        window.render_frame(cx);
        assert!(window.try_find("security-current").is_some());
    })
    .unwrap();
    assert!(cx.update(super::lock_now));
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("security-current").is_none(),
            "锁定后清掉输入到一半的密码"
        );
    })
    .unwrap();
}
