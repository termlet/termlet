//! 主机概览的小窗（[`super::popout`]）：弹出、再弹出时拿到前面，固定一台主机，让终端照常采样、关掉后恢复，随终端
//! 关闭，记住位置、大小与置顶，紧凑版式按窗口的大小显示多少、150% 界面大小下放得下。用本地终端加上喂进去的采样，
//! 不连服务器（真实的采样、断开与重连见 `workspace/popout_tests.rs`）。

use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Pixels, TestAppContext, WindowVisibility, px,
    size,
};
use termlet_core::id::HostId;
use termlet_core::monitor::{Details, Disk, Sample};
use termlet_core::ssh::{Answer, Prompter};

use super::model::{OverviewData, Page};
use super::overview::Fit;
use super::panel::HostToolsPanel;
use super::popout::{self, DEFAULT_SIZE, OverviewWindow, first_rect};
use super::shared::Cached;
use super::target::{ToolsState, ToolsTarget};
use super::tests::{cache, open, processes, set_target, target};
use crate::app_state::AppState;
use crate::terminal_panel::{Session, TerminalPanel};
use crate::window_state::Rect;

fn disk(mount: &str, used: u64) -> Disk {
    Disk {
        mount: mount.into(),
        filesystem: "/dev/vda1".into(),
        total: 100,
        used,
    }
}

/// 开一个本地终端（不在停靠区中，也就不在前台），当作采到了最近 10 分钟的样本（CPU 为 `cpu`）；主机工具跟随它，
/// 它连的“主机”是 `host`（保存的主机的 ID，小窗按它区分主机）。
fn follow_local(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    panel: &Entity<HostToolsPanel>,
    host: &str,
    cpu: f64,
) -> Entity<TerminalPanel> {
    let terminal = cx
        .update_window(handle, |_, window, cx| {
            let settings = AppState::store(cx).local_settings();
            let profile = crate::local_shell::default_profile(&settings, cx).expect("默认的 shell");
            let prompter: Prompter = Arc::new(|_| Box::pin(async { Answer::Cancel }));
            TerminalPanel::open(
                "local".into(),
                Session::Local { profile },
                prompter,
                window,
                cx,
            )
        })
        .unwrap();
    let samples = (0..=200).map(|i| Sample {
        cpu: Some(cpu),
        per_core: vec![cpu, cpu],
        cores: 2,
        swap_total: 100,
        swap_used: 10,
        disks: vec![disk("/", 30), disk("/data", 91)],
        ..crate::monitor_model::test_sample(i * 3000)
    });
    cx.update(|cx| terminal.update(cx, |t, cx| t.feed_samples(samples, cx)));
    let id = terminal.entity_id();
    let details = Details {
        system: Some("Alpine Linux v3.23".into()),
        kernel: Some("Linux 6.18.33".into()),
        arch: Some("x86_64".into()),
        inodes: Vec::new(),
    };
    cache(
        panel,
        cx,
        id,
        Page::Overview,
        Cached::Overview(Rc::new(OverviewData::new(Some(details), &processes()))),
    );
    let followed = ToolsTarget {
        terminal: terminal.downgrade(),
        ssh_host: Some(HostId::from_raw(host)),
        ..target(id, ToolsState::Reconnecting)
    };
    set_target(cx, handle, panel, Some(followed));
    terminal
}

/// 在主机工具的概览页上点右上角的弹出按钮；返回开着的小窗。
fn click_pop_out(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
) -> Vec<(String, AnyWindowHandle, Entity<OverviewWindow>)> {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(0usize, cx);
        window.render_frame(cx);
        window.click("tools-overview-popout", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.read(popout::open_windows)
}

/// 小窗中显示着的 CPU 最高的进程数（缓存的进程中 CPU 不为 0 的有 5 个）。
fn shown_processes(cx: &mut TestAppContext, handle: AnyWindowHandle) -> usize {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        [900usize, 1, 42, 401, 400]
            .into_iter()
            .filter(|pid| window.try_find(("tools-overview-proc", *pid)).is_some())
            .count()
    })
    .unwrap()
}

fn window_size(cx: &mut TestAppContext, handle: AnyWindowHandle) -> (f32, f32) {
    cx.update_window(handle, |_, window, _| {
        let size = window.bounds().size;
        (f32::from(size.width), f32::from(size.height))
    })
    .unwrap()
}

fn close(cx: &mut TestAppContext, handle: AnyWindowHandle) {
    cx.update_window(handle, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}

/// 概览页右上角的按钮把这台主机的概览弹出成小窗（标题“主机名 · 概览”，紧凑的版式，没有弹出按钮、进程不能点）；
/// 再点一次不另开，把已有的拿到前面。
#[gpui_kit::test]
fn pop_out_opens_one_window_per_host_and_raises_it_again(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let _terminal = follow_local(cx, handle, &panel, "h_web", 12.);
    let open = click_pop_out(cx, handle);
    assert_eq!(open.len(), 1, "弹出了小窗");
    let (host, popout, view) = open[0].clone();
    assert_eq!(host, "h_web");
    assert_eq!(cx.read(|cx| cx.windows().len()), 2);
    assert_eq!(
        cx.read(|cx| view.read(cx).title()).as_ref(),
        "web · 概览",
        "标题是主机名 · 概览"
    );
    cx.update_window(popout, |_, window, cx| {
        window.render_frame(cx);
        for element in [
            "overview-window-title",
            "tools-overview-cpu",
            "tools-overview-cpu-chart",
            "tools-overview-memory-chart",
            "tools-overview-net-chart",
            "tools-overview-disk-/data",
            "tools-overview-top",
        ] {
            assert!(window.try_find(element).is_some(), "小窗中有 {element}");
        }
        // 紧凑：只有最满的磁盘，没有系统信息、每核、网卡列表，也不能再弹出
        for element in [
            "tools-overview-disk-/",
            "tools-overview-system",
            "tools-overview-core-bars",
            "tools-overview-cores",
            "tools-overview-ifaces",
            "tools-overview-popout",
        ] {
            assert!(window.try_find(element).is_none(), "小窗中没有 {element}");
        }
        // 进程不能点（点了也不跳到面板的进程页）
        window.click(("tools-overview-proc", 900usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(cx.read(|cx| panel.read(cx).page()), Page::Overview);
    assert_eq!(shown_processes(cx, popout), 3, "默认大小列出 3 个进程");

    // 再弹出：不另开，拿到前面
    let again = click_pop_out(cx, handle);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].1, popout);
    assert_eq!(cx.read(|cx| cx.windows().len()), 2);
    assert_eq!(cx.read(|cx| view.read(cx).raised()), 1);
    drop((open, again, view));
    close(cx, popout);
    assert!(cx.read(popout::open_windows).is_empty());
}

/// 小窗固定一台主机：主机工具跟随别的终端后小窗不变；那台主机弹出的是另一个小窗。
#[gpui_kit::test]
fn pop_out_stays_with_its_host(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let web = follow_local(cx, handle, &panel, "h_web", 12.);
    let first = click_pop_out(cx, handle);
    let db = follow_local(cx, handle, &panel, "h_db", 80.);
    let (_, _, view) = first[0].clone();
    assert_eq!(
        cx.read(|cx| view.read(cx).terminal_id()),
        Some(web.entity_id()),
        "面板换了终端，小窗还跟着原来的"
    );
    let both = click_pop_out(cx, handle);
    assert_eq!(both.len(), 2, "另一台主机另开一个");
    let other = both
        .iter()
        .find(|(host, _, _)| host == "h_db")
        .map(|(_, _, view)| view.clone())
        .expect("h_db 的小窗");
    assert_eq!(
        cx.read(|cx| other.read(cx).terminal_id()),
        Some(db.entity_id())
    );
    assert_eq!(cx.read(|cx| view.read(cx).raised()), 0);
    // 两个都没有记下位置：错开放
    let (a, b) = (both[0].1, both[1].1);
    let origin = |cx: &mut TestAppContext, handle: AnyWindowHandle| {
        cx.update_window(handle, |_, window, _| window.bounds().origin)
            .unwrap()
    };
    assert_ne!(origin(cx, a), origin(cx, b));
}

/// 小窗显示着时终端不在前台也照常采样；小窗最小化、关掉后恢复原来的规则（不在前台不采样）。
#[gpui_kit::test]
fn pop_out_keeps_the_terminal_sampling_until_it_closes(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let terminal = follow_local(cx, handle, &panel, "h_web", 12.);
    let samples_now = |cx: &mut TestAppContext| cx.read(|cx| terminal.read(cx).samples_now());
    assert!(!samples_now(cx), "终端不在前台：不采样");
    let open = click_pop_out(cx, handle);
    let (_, popout, view) = open[0].clone();
    assert!(samples_now(cx), "小窗开着：照常采样");
    assert!(cx.read(|cx| view.read(cx).is_sampling()));
    cx.simulate_window_visibility_change(popout, WindowVisibility::Hidden);
    cx.run_until_parked();
    assert!(!samples_now(cx), "小窗最小化：不再让终端采样");
    cx.simulate_window_visibility_change(popout, WindowVisibility::Visible);
    cx.run_until_parked();
    assert!(samples_now(cx));
    drop((open, view));
    close(cx, popout);
    assert!(!samples_now(cx), "小窗关掉后恢复");
}

/// 终端的 Tab 关了（终端释放）：小窗随之关闭。
#[gpui_kit::test]
fn pop_out_closes_with_its_terminal(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let terminal = follow_local(cx, handle, &panel, "h_web", 12.);
    let open = click_pop_out(cx, handle);
    assert_eq!(open.len(), 1);
    let popout = open[0].1;
    drop(open);
    set_target(cx, handle, &panel, None);
    let weak = terminal.downgrade();
    drop(terminal);
    // 释放在这次更新结束时
    cx.update(|_| {});
    cx.run_until_parked();
    assert!(weak.upgrade().is_none(), "终端已释放");
    assert!(
        cx.read(popout::open_windows).is_empty(),
        "终端释放后小窗关闭"
    );
    assert!(!cx.read(|cx| cx.windows().contains(&popout)));
}

/// 位置、大小与是否置顶按主机记下，下次弹出时照旧；图钉切换置顶（不支持置顶的系统上没有图钉）。
#[gpui_kit::test]
fn pop_out_remembers_its_place_size_and_pin(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let _terminal = follow_local(cx, handle, &panel, "h_web", 12.);
    let (_, popout, view) = click_pop_out(cx, handle)[0].clone();
    assert!(cx.read(|cx| view.read(cx).is_pinned()), "默认置顶");
    let pinnable = cx.read(|cx| crate::topmost::supported(cx.platform_info().os));
    cx.update_window(popout, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.try_find("overview-window-pin").is_some(), pinnable);
        if pinnable {
            window.click("overview-window-pin", cx);
        }
    })
    .unwrap();
    cx.run_until_parked();
    let saved = |cx: &mut TestAppContext| {
        cx.read(|cx| {
            AppState::ui(cx)
                .read(cx)
                .state()
                .overview_window("h_web")
                .cloned()
        })
    };
    if pinnable {
        assert!(!cx.read(|cx| view.read(cx).is_pinned()), "取消了置顶");
        assert!(!saved(cx).expect("切换置顶时记下").pinned);
    }
    // 改大小：停下片刻后记下
    cx.simulate_window_resize(popout, size(px(400.), px(520.)));
    cx.executor().advance_clock(crate::window_state::SETTLE * 2);
    cx.run_until_parked();
    let placement = saved(cx).and_then(|s| s.placement).expect("记下了位置");
    assert_eq!((placement.width, placement.height), (400., 520.));
    close(cx, popout);

    // 再弹出：照记下的大小与置顶
    let (_, reopened, view) = click_pop_out(cx, handle)[0].clone();
    assert_ne!(reopened, popout);
    assert_eq!(window_size(cx, reopened), (400., 520.));
    assert_eq!(cx.read(|cx| view.read(cx).is_pinned()), !pinnable);
}

/// 大小按 100% 界面大小记：150% 时拉成的大小记下的是除以 1.5 的值，100% 时再弹出按记下的；开着时换了界面大小，
/// 窗口按比例跟着变。
#[gpui_kit::test]
fn pop_out_size_follows_the_ui_scale(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let _terminal = follow_local(cx, handle, &panel, "h_web", 12.);
    let set_scale = |cx: &mut TestAppContext, percent: u16| {
        cx.update(|cx| {
            AppState::store(cx)
                .update_local_settings(|s| s.window.ui_scale = percent)
                .unwrap();
            crate::ui_scale::apply(cx);
        });
        cx.run_until_parked();
    };
    set_scale(cx, 150);
    let (_, popout, _) = click_pop_out(cx, handle)[0].clone();
    cx.simulate_window_resize(popout, size(px(600.), px(780.)));
    cx.executor().advance_clock(crate::window_state::SETTLE * 2);
    cx.run_until_parked();
    let placement = cx
        .read(|cx| {
            AppState::ui(cx)
                .read(cx)
                .state()
                .overview_window("h_web")
                .cloned()
        })
        .and_then(|s| s.placement)
        .expect("记下了位置");
    assert_eq!((placement.width, placement.height), (400., 520.));
    close(cx, popout);

    set_scale(cx, 100);
    let (_, popout, _) = click_pop_out(cx, handle)[0].clone();
    assert_eq!(window_size(cx, popout), (400., 520.));
    // 开着时放大到 150%：窗口跟着变大
    set_scale(cx, 150);
    cx.update_window(popout, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(window_size(cx, popout), (600., 780.));
}

/// 紧凑版式按窗口的大小显示多少：够高时另有系统信息、每核的竖条与全部进程、磁盘。
#[gpui_kit::test]
fn pop_out_shows_more_when_taller(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let _terminal = follow_local(cx, handle, &panel, "h_web", 12.);
    let (_, popout, _) = click_pop_out(cx, handle)[0].clone();
    let tall = Fit::TALL * crate::ui_scale::BASE_REM + 8.;
    cx.simulate_window_resize(popout, size(px(400.), px(tall)));
    cx.run_until_parked();
    cx.update_window(popout, |_, window, cx| {
        window.render_frame(cx);
        for element in [
            "tools-overview-system",
            "tools-overview-core-bars",
            "tools-overview-cpu-facts",
            "tools-overview-swap",
            "tools-overview-disk-/",
        ] {
            assert!(window.try_find(element).is_some(), "够高时有 {element}");
        }
    })
    .unwrap();
    assert_eq!(shown_processes(cx, popout), 5);
}

/// 第一次弹出时的大小随界面大小：100% 与 150% 下全部内容都在窗口之内（不用滚动，不横向溢出），标题栏放得下。
#[gpui_kit::test]
fn pop_out_fits_its_content_at_100_and_150_percent(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let _terminal = follow_local(cx, handle, &panel, "h_web", 12.);
    for percent in [100u16, 150] {
        cx.update(|cx| {
            AppState::store(cx)
                .update_local_settings(|s| s.window.ui_scale = percent)
                .unwrap();
            crate::ui_scale::apply(cx);
        });
        cx.run_until_parked();
        let (_, popout, _) = click_pop_out(cx, handle)[0].clone();
        let scale = f32::from(percent) / 100.;
        assert_eq!(
            window_size(cx, popout),
            (DEFAULT_SIZE.0 * scale, DEFAULT_SIZE.1 * scale),
            "{percent}%：默认大小按界面大小放大"
        );
        cx.update_window(popout, |_, window, cx| {
            window.render_frame(cx);
            window.render_frame(cx);
            let view: Bounds<Pixels> = window.find("overview-window").bounds();
            for element in [
                "overview-window-title",
                "tools-overview-cpu",
                "tools-overview-memory",
                "tools-overview-net",
                "tools-overview-disk-/data",
                "tools-overview-top",
            ] {
                let bounds = window.find(element).bounds();
                assert!(
                    bounds.bottom() <= view.bottom() && bounds.right() <= view.right(),
                    "{percent}%：{element} {bounds:?} 在窗口 {view:?} 之内"
                );
            }
            assert!(
                window
                    .try_find(("tools-overview-proc", 400usize))
                    .is_some_and(|row| row.bounds().bottom() <= view.bottom()),
                "{percent}%：第 3 个进程也在窗口之内"
            );
        })
        .unwrap();
        close(cx, popout);
        // 下一轮按第一次打开算大小
        cx.update(|cx| {
            AppState::ui(cx).update(cx, |ui, cx| ui.update(|s| s.overview_windows.clear(), cx))
        });
    }
}

/// 主机名很长时标题截断，图钉与窗口按钮（Windows、Linux 上画在标题栏里）仍在窗口之内。
#[gpui_kit::test]
fn pop_out_title_truncates_long_host_names(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let terminal = follow_local(cx, handle, &panel, "h_web", 12.);
    let long = ToolsTarget {
        terminal: terminal.downgrade(),
        name: "deploy@very-long-host-name.internal.example.com:2222".into(),
        ssh_host: Some(HostId::from_raw("h_web")),
        ..target(terminal.entity_id(), ToolsState::Reconnecting)
    };
    set_target(cx, handle, &panel, Some(long));
    let (_, popout, _) = click_pop_out(cx, handle)[0].clone();
    cx.update_window(popout, |_, window, cx| {
        window.render_frame(cx);
        let view = window.find("overview-window").bounds();
        let title = window.find("overview-window-title").bounds();
        assert!(
            title.right() <= view.right(),
            "标题 {title:?} 在窗口 {view:?} 之内"
        );
        if cfg!(windows) {
            assert!(
                window.try_find("close").is_some(),
                "Windows 上标题栏里有关闭按钮"
            );
        }
        for control in ["overview-window-pin", "minimize", "maximize", "close"] {
            if let Some(found) = window.try_find(control) {
                let bounds = found.bounds();
                assert!(
                    bounds.right() <= view.right() && bounds.left() >= title.right(),
                    "{control} {bounds:?} 在标题之后、窗口 {view:?} 之内"
                );
            }
        }
    })
    .unwrap();
}

/// 第一次打开的位置：贴着屏幕可见区域的右上角，按界面大小放大，几个依次错开，放不下时不超出屏幕。
#[test]
fn first_rect_sits_at_the_top_right() {
    let screen = Rect {
        x: 0.,
        y: 0.,
        w: 1920.,
        h: 1040.,
    };
    let r = first_rect(Some(&screen), 1., 0);
    assert_eq!((r.w, r.h), DEFAULT_SIZE);
    assert_eq!(r.x + r.w, 1920. - 16.);
    assert_eq!(r.y, 16.);
    let next = first_rect(Some(&screen), 1., 1);
    assert!(next.x < r.x && next.y > r.y, "第二个往左下错开");
    let big = first_rect(Some(&screen), 1.5, 0);
    assert_eq!((big.w, big.h), (DEFAULT_SIZE.0 * 1.5, DEFAULT_SIZE.1 * 1.5));
    let small = Rect {
        x: 100.,
        y: 50.,
        w: 300.,
        h: 400.,
    };
    let fitted = first_rect(Some(&small), 2., 3);
    assert!(fitted.x >= small.x && fitted.y >= small.y);
    assert!(fitted.w <= small.w && fitted.h <= small.h);
}
