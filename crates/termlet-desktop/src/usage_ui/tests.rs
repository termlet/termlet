//! 使用统计的 UI 集成测试：在无头窗口中渲染生产环境的视图，经真实的事件路径操作，统计发到本机的假接收端
//! （按元素 ID 查找，与界面语言无关）。

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _,
    Render, Styled as _, TestAppContext, Window, WindowBounds, WindowOptions, div, px, size,
};
use termlet_core::usage::{Collecting, Feature, FeatureLog, ReporterOptions, Route, UsageReporter};

use super::Setup;
use crate::app_state::AppState;
use crate::lock_ui::AboutExtras;
use crate::testkit::poll_until;

/// 本机的假接收端：每个请求回答 204，把正文送出来。返回发到它的去向。
fn receiver() -> (Route, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let route = Route::fixed(&format!(
        "http://{}/v1/stats",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            let body = loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                data.extend_from_slice(&buf[..n]);
                let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if data.len() >= end + 4 + len {
                    break Some(
                        String::from_utf8_lossy(&data[end + 4..end + 4 + len]).into_owned(),
                    );
                }
            };
            if let Some(body) = body
                && tx.send(body).is_err()
            {
                return;
            }
            let _ = write!(
                stream,
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    (route, rx)
}

/// 装好应用状态（本机设置是默认的），使用统计发到 `route`，返回 `usage.json` 的位置。
fn setup(cx: &mut TestAppContext, route: Route) -> PathBuf {
    setup_with_features(cx, route).0
}

/// 同 [`setup`]，另返回这次用的功能名单（记在数据目录的 `usage-features.json` 中，不是全局的那一份）。
fn setup_with_features(cx: &mut TestAppContext, route: Route) -> (PathBuf, Arc<FeatureLog>) {
    // 发送在核心的运行时上，不受测试调度器管理
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    cx.update(|cx| cx.set_reduce_motion(true));
    cx.update(|cx| {
        let data_dir = &AppState::global(cx).data_dir;
        let state_file = data_dir.usage_state();
        let features = Arc::new(FeatureLog::default());
        features.attach(data_dir.usage_features());
        let reporter = UsageReporter::with(ReporterOptions {
            state_file: state_file.clone(),
            route,
            remote: None,
            builtin: termlet_core::usage::USAGE_URL.parse().unwrap(),
            features: features.clone(),
            skill_installed: || false,
        });
        super::install_with(
            Setup {
                reporter,
                follow_activity: false,
            },
            cx,
        );
        (state_file, features)
    })
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

struct EmptyView;

impl Render for EmptyView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}

fn usage_settings(cx: &mut TestAppContext) -> termlet_core::model::UsageSettings {
    cx.update(|cx| AppState::store(cx).local_settings().usage)
}

/// 打开“查看内容”对话框，等它读出正文。
fn open_view(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
) -> (Entity<super::view::UsageView>, String) {
    let view = cx
        .update_window(handle, |_, window, cx| super::open_view(window, cx))
        .unwrap()
        .expect("对话框");
    poll_until(cx, "读出将发送的正文", |cx| {
        cx.read(|cx| view.read(cx).json().is_some())
    });
    let json = cx.read(|cx| view.read(cx).json().unwrap().to_string());
    (view, json)
}

// 默认开，不经任何提示：主窗口显示时检查一次就发送，发出的正文与“查看内容”中显示的逐字节相同
#[gpui_kit::test]
fn sends_by_default_without_any_prompt(cx: &mut TestAppContext) {
    let (route, bodies) = receiver();
    let state_file = setup(cx, route);
    assert!(usage_settings(cx).enabled, "默认开");
    let (handle, _) = open(cx, |_, _| EmptyView);

    // 查看内容：显示的就是将发送的原文，今天还没有发送
    let (view, json) = open_view(cx, handle);
    cx.read(|cx| {
        assert_eq!(
            view.read(cx).text(cx).as_ref(),
            json.as_str(),
            "文本框中是原文"
        );
        assert_eq!(
            view.read(cx).state_line(cx).unwrap().as_ref(),
            crate::t!("usage.view.not_sent_today").as_ref()
        );
    });
    assert!(json.starts_with(r#"{"v":1,"app":""#), "{json}");
    assert!(json.contains(r#""lang":"zh-CN""#), "{json}");
    assert!(
        json.contains(&format!(r#""app":"{}""#, termlet_core::VERSION)),
        "{json}"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("usage-view-json").is_some());
        window.click("usage-view-close", cx);
    })
    .unwrap();
    cx.run_until_parked();

    // 主窗口显示（没有锁着）：今天用过，检查一次就发送
    termlet_core::usage::mark_active();
    cx.update(super::check);
    let sent = bodies
        .recv_timeout(Duration::from_secs(20))
        .expect("默认开着，照常发送");
    assert_eq!(sent, json);
    poll_until(cx, "记下发送的日期", |_| state_file.exists());
    // 同一天不再发
    cx.update(super::check);
    assert!(bodies.recv_timeout(Duration::from_millis(500)).is_err());
    // 对话框的状态改为今天已发送
    let (view, _) = open_view(cx, handle);
    cx.read(|cx| {
        assert_eq!(
            view.read(cx).state_line(cx).unwrap().as_ref(),
            crate::t!("usage.view.sent_today").as_ref()
        );
    });
}

#[gpui_kit::test]
fn nothing_is_sent_while_the_app_is_locked(cx: &mut TestAppContext) {
    let (route, bodies) = receiver();
    setup(cx, route);
    let lock = cx
        .update(|cx| crate::lock_ui::app_lock(cx))
        .expect("应用锁");
    let password = termlet_core::secret::SecretString::new("lock-pass".to_string());
    let runtime = termlet_core::runtime::runtime();
    runtime
        .block_on(lock.set_password(None, &password))
        .unwrap();
    lock.lock_now().unwrap();
    termlet_core::usage::mark_active();
    cx.update(super::check);
    assert!(
        bodies.recv_timeout(Duration::from_millis(500)).is_err(),
        "锁着时不检查"
    );
    runtime.block_on(lock.unlock(&password)).unwrap();
    cx.update(super::check);
    assert!(
        bodies.recv_timeout(Duration::from_secs(20)).is_ok(),
        "解锁后发送"
    );
}

/// 设置“关于”页（不经设置对话框）。
struct PageHarness {
    page: Entity<AboutExtras>,
}

impl Render for PageHarness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().overflow_hidden().child(self.page.clone())
    }
}

fn open_about(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<AboutExtras>) {
    let (settings, local) = cx.update(|cx| {
        let store = AppState::store(cx);
        (store.settings(), store.local_settings())
    });
    let (handle, harness) = open(cx, move |window, cx| PageHarness {
        page: cx.new(|cx| AboutExtras::new(&settings, &local, window, cx)),
    });
    let page = cx.read(|cx| harness.read(cx).page.clone());
    (handle, page)
}

#[gpui_kit::test]
fn settings_switch_takes_effect_after_saving(cx: &mut TestAppContext) {
    let (route, bodies) = receiver();
    let state_file = setup(cx, route);
    let (handle, page) = open_about(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let usage = window.find("about-usage");
        assert_eq!(usage.checked(), Some(true), "默认勾选");
        assert_ne!(usage.disabled(), Some(true));
        assert!(window.try_find("about-usage-env-off").is_none());
        assert!(window.try_find("about-usage-view").is_some());
        assert!(window.try_find("about-privacy-policy").is_some());
        window.click("about-usage", cx);
    })
    .unwrap();
    // 草稿：保存（写进本机设置）后才生效
    let mut local = cx.update(|cx| AppState::store(cx).local_settings());
    let mut settings = cx.update(|cx| AppState::store(cx).settings());
    cx.read(|cx| page.read(cx).apply(&mut settings, &mut local, cx));
    assert!(!local.usage.enabled);
    assert!(usage_settings(cx).enabled, "还没有保存");

    // 先发过一次，再关掉：删除 usage.json，之后不再发
    termlet_core::usage::mark_active();
    cx.update(super::check);
    bodies
        .recv_timeout(Duration::from_secs(20))
        .expect("开着时发送");
    poll_until(cx, "记下发送的日期", |_| state_file.exists());
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.usage.enabled = false)
            .unwrap()
    });
    poll_until(cx, "关闭后删除 usage.json", |_| !state_file.exists());
    cx.update(super::check);
    assert!(bodies.recv_timeout(Duration::from_millis(500)).is_err());
    let (view, _) = open_view(cx, handle);
    cx.read(|cx| {
        assert_eq!(
            view.read(cx).state_line(cx).unwrap().as_ref(),
            crate::t!("usage.view.disabled").as_ref()
        );
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("usage-view-close", cx);
    })
    .unwrap();
    cx.run_until_parked();
    // 再打开：从头算，马上检查一次
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.usage.enabled = true)
            .unwrap()
    });
    // 设置变化的事件经界面的执行器转过来
    let mut body = None;
    poll_until(cx, "重新打开后发送", |_| {
        body = bodies.try_recv().ok();
        body.is_some()
    });
    let body = body.unwrap();
    assert!(body.contains(r#""age":"new""#), "从头算：{body}");
}

#[gpui_kit::test]
fn environment_variable_turns_the_switch_off(cx: &mut TestAppContext) {
    setup(cx, Route::EnvOff(termlet_core::usage::DO_NOT_TRACK_ENV));
    let (handle, page) = open_about(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let usage = window.find("about-usage");
        assert_eq!(usage.checked(), Some(false), "显示为关");
        assert_eq!(usage.disabled(), Some(true), "不能改");
        assert!(window.try_find("about-usage-env-off").is_some());
        window.click("about-usage", cx);
    })
    .unwrap();
    let (mut settings, mut local) = cx.update(|cx| {
        let store = AppState::store(cx);
        (store.settings(), store.local_settings())
    });
    cx.read(|cx| page.read(cx).apply(&mut settings, &mut local, cx));
    assert!(local.usage.enabled, "设置中存的值不变");
    let (view, _) = open_view(cx, handle);
    cx.read(|cx| {
        assert_eq!(
            view.read(cx).state_line(cx).unwrap().as_ref(),
            crate::t!(
                "usage.view.env_off",
                name = termlet_core::usage::DO_NOT_TRACK_ENV
            )
            .as_ref()
        );
    });
}

// 功能名单：开着时开始记、随统计发出并从名单中去掉；关掉开关时删除名单、不再记；“查看内容”中说明这一项
#[gpui_kit::test]
fn feature_list_follows_the_switch(cx: &mut TestAppContext) {
    let (route, bodies) = receiver();
    let (state_file, features) = setup_with_features(cx, route);
    let file = cx.update(|cx| AppState::global(cx).data_dir.usage_features());
    poll_until(cx, "开着时开始记功能名单", |_| {
        features.collecting() == Collecting::On
    });
    features.mark(Feature::Split);
    features.mark(Feature::Compose);
    poll_until(cx, "写进 usage-features.json", |_| file.exists());

    // 查看内容：名单在正文中，并有这一项的说明
    let (handle, _) = open(cx, |_, _| EmptyView);
    let (_, json) = open_view(cx, handle);
    assert!(
        json.ends_with(r#","features":["split","compose"]}"#),
        "{json}"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("usage-view-close", cx);
    })
    .unwrap();
    cx.run_until_parked();

    // 发出后从名单中去掉，文件删除
    termlet_core::usage::mark_active();
    cx.update(super::check);
    let sent = bodies.recv_timeout(Duration::from_secs(20)).expect("发送");
    assert_eq!(sent, json);
    poll_until(cx, "发出的从名单中去掉", |_| {
        features.pending() == 0 && !file.exists()
    });
    poll_until(cx, "记下发送的日期", |_| state_file.exists());

    // 关掉：删除名单，不再记
    features.mark(Feature::Sftp);
    poll_until(cx, "再记一项", |_| file.exists());
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.usage.enabled = false)
            .unwrap()
    });
    poll_until(cx, "关掉后删除名单", |_| {
        features.collecting() == Collecting::Off && !file.exists() && !state_file.exists()
    });
    assert_eq!(features.pending(), 0);
    features.mark(Feature::Ssh);
    assert_eq!(features.pending(), 0, "关着时不记");

    // 再打开：从空开始记
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.usage.enabled = true)
            .unwrap()
    });
    poll_until(cx, "重新打开后开始记", |_| {
        features.collecting() == Collecting::On
    });
    features.mark(Feature::Ssh);
    assert_eq!(features.pending(), Feature::Ssh.bit());
}

// 被环境变量关掉时不记功能名单
#[gpui_kit::test]
fn feature_list_is_not_kept_when_turned_off_by_the_environment(cx: &mut TestAppContext) {
    let (_, features) =
        setup_with_features(cx, Route::EnvOff(termlet_core::usage::DO_NOT_TRACK_ENV));
    poll_until(cx, "不收集", |_| {
        features.collecting() == Collecting::Off
    });
    features.mark(Feature::Ssh);
    assert_eq!(features.pending(), 0);
}
