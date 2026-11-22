//! 主机工具面板的 UI 集成测试：在无头窗口中渲染面板，经真实的事件路径操作并检查结果（按元素 ID 查找，与界面语言无关）。
//! 不连服务器的测试让终端处在“重连中”（显示缓存、不读取）；`TERMLET_TEST_SSH=127.0.0.1:2222` 时另连真实的 sshd。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::h_flex;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, ElementId, Entity, EntityId,
    IntoElement, ParentElement as _, Pixels, Render, SharedString, Styled as _, Subscription,
    TestAppContext, WeakEntity, Window, WindowBounds, WindowOptions, div, point, px, size,
};
use termlet_core::hosttools::{
    BuildCacheUsage, CleanupPlan, Conn, ConnList, ConnProcess, ConnSource, Container, DockerInfo,
    Engine, HostTools, Image, LogLevel, LogLine, LogResult, MainExit, PlanContainer, PlanImage,
    PlanNetwork, PlanVolume, Process, ProcessList, PruneKind, Service, ServiceList, Target, Timer,
    Volume,
};
use termlet_core::id::HostId;

use super::model::{ConnChip, Page, ProcSort, conn_keys};
use super::panel::{HostToolsEvent, HostToolsPanel};
use super::processes::ProcView;
use super::shared::row_tooltip;
use super::shared::{Cached, ToolPage as _};
use super::target::{ToolsState, ToolsTarget};
use crate::app_state::AppState;
use crate::menu_tooltip::{is_quiet, menu_open};
use crate::t;
use crate::testkit::{poll_until, test_server, wait_until};

/// 只放面板的视图（面板在右侧，与工作区中的位置相同），记下面板发出的事件。
struct Harness {
    panel: Entity<HostToolsPanel>,
    events: Rc<RefCell<Vec<&'static str>>>,
    _events: Subscription,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .items_stretch()
            .child(div().flex_1())
            .child(self.panel.clone())
    }
}

pub(super) fn open(
    cx: &mut TestAppContext,
) -> (
    AnyWindowHandle,
    Entity<HostToolsPanel>,
    Rc<RefCell<Vec<&'static str>>>,
) {
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    cx.update(|cx| cx.set_reduce_motion(true));
    // 这些测试从进程页开始（概览页另有测试）
    cx.update(|cx| {
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(
                |s| {
                    s.host_tools_open = true;
                    s.host_tools_page = Some("processes".into());
                },
                cx,
            )
        })
    });
    let events = Rc::new(RefCell::new(Vec::new()));
    let recorded = events.clone();
    let (handle, harness) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1024.), px(640.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| {
                    let panel = cx.new(|cx| HostToolsPanel::new(window, cx));
                    let events = recorded.clone();
                    let subscription =
                        cx.subscribe(&panel, move |_, _, event: &HostToolsEvent, _| {
                            events.borrow_mut().push(match event {
                                HostToolsEvent::RunInTerminal { .. } => "run",
                                HostToolsEvent::ForwardLocal { .. } => "forward",
                                HostToolsEvent::OpenInBrowser { .. } => "browser",
                                HostToolsEvent::OpenLogs(_) => "logs",
                                HostToolsEvent::OpenSftp { .. } => "sftp",
                                HostToolsEvent::Close => "close",
                                HostToolsEvent::FocusTerminal => "terminal",
                                HostToolsEvent::QuickConnect => "quick",
                                HostToolsEvent::OpenHost(_) => "host",
                            })
                        });
                    Harness {
                        panel,
                        events: recorded.clone(),
                        _events: subscription,
                    }
                })
            },
        )
        .expect("打开测试窗口")
    });
    let panel = cx.read(|cx| harness.read(cx).panel.clone());
    let events = cx.read(|cx| harness.read(cx).events.clone());
    (handle, panel, events)
}

/// 跟随的终端（测试中没有真的终端面板，用一个实体的 ID 区分终端）。
pub(super) fn target(id: EntityId, state: ToolsState) -> ToolsTarget {
    ToolsTarget {
        terminal: WeakEntity::new_invalid(),
        id,
        name: "web".into(),
        login: Some("termlet@10.0.0.5".into()),
        state,
        tools: HostTools::new("test"),
        ssh_host: Some(HostId::from_raw("h_test")),
    }
}

pub(super) fn set_target(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    panel: &Entity<HostToolsPanel>,
    target: Option<ToolsTarget>,
) {
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |panel, cx| panel.set_target(target, window, cx));
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// 用来区分终端的实体（测试期间保持存活，ID 不会被重新使用）。
fn terminal_ids(cx: &mut TestAppContext, n: usize) -> Vec<Entity<()>> {
    (0..n).map(|_| cx.update(|cx| cx.new(|_| ()))).collect()
}

fn process(pid: u32, ppid: u32, name: &str) -> Process {
    Process {
        pid,
        ppid,
        name: name.into(),
        state: 'S',
        uid: Some(1000),
        user: Some("termlet".into()),
        rss: 4096 * u64::from(pid),
        vsize: 0,
        cpu: Some(f64::from(pid % 7)),
        threads: 1,
        priority: 20,
        nice: 0,
        start: None,
        start_ticks: 0,
        cpu_time: 0.,
        in_terminal: false,
        terminal_shell: false,
        service: None,
        container: None,
        cmdline: None,
    }
}

pub(super) fn processes() -> ProcessList {
    let mut shell = process(400, 1, "bash");
    shell.in_terminal = true;
    shell.terminal_shell = true;
    let mut child = process(401, 400, "vim");
    child.in_terminal = true;
    let list = vec![
        process(1, 0, "init"),
        process(42, 1, "nginx"),
        shell,
        child,
        process(900, 1, "sshd"),
    ];
    ProcessList {
        total: list.len(),
        processes: list,
        truncated: false,
        uid: Some(1000),
        mem_total: 0,
        cores: 1,
        at: 0,
    }
}

pub(super) fn cache(
    panel: &Entity<HostToolsPanel>,
    cx: &mut TestAppContext,
    id: EntityId,
    page: Page,
    data: Cached,
) {
    cx.update(|cx| panel.read(cx).cache().borrow_mut().put((id, page), data));
}

#[gpui_kit::test]
fn panel_notice_follows_the_terminal_state(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let has = |cx: &mut TestAppContext, element: &'static str| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(element).is_some()
        })
        .unwrap()
    };
    assert!(has(cx, "host-tools-notice"), "没有终端时说明");
    assert!(has(cx, "host-tools-title"));

    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Unsupported)),
    );
    assert!(has(cx, "host-tools-notice"), "本地终端没有主机工具");
    assert!(
        !cx.read(|cx| panel.read(cx).has_pages()),
        "不支持的终端不建各页"
    );

    set_target(cx, handle, &panel, Some(target(id, ToolsState::Connecting)));
    assert!(has(cx, "host-tools-notice"), "连接中");
    assert!(cx.read(|cx| panel.read(cx).has_pages()));

    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    assert!(!has(cx, "host-tools-notice"), "重连中照常显示各页");

    set_target(cx, handle, &panel, Some(target(id, ToolsState::Ended)));
    assert!(has(cx, "host-tools-notice"), "已断开");

    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::SingleChannel)),
    );
    assert!(has(cx, "host-tools-notice"), "只用一个通道");
}

#[gpui_kit::test]
fn processes_show_the_cache_and_switching_terminal_resets_the_page(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 2);
    let (first, second) = (ids[0].entity_id(), ids[1].entity_id());
    cache(
        &panel,
        cx,
        first,
        Page::Processes,
        Cached::Processes(Rc::new(processes())),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(first, ToolsState::Reconnecting)),
    );

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(("tools-process-row", 42usize)).is_some(),
            "先显示缓存"
        );
        assert!(
            window.try_find(("tools-process-self", 400usize)).is_some(),
            "标出本终端的 shell"
        );
        // 搜索：名称、PID、用户
        window.click("tools-process-search", cx);
        window.input("ngi", cx);
        window.render_frame(cx);
        assert!(window.try_find(("tools-process-row", 42usize)).is_some());
        assert!(window.try_find(("tools-process-row", 900usize)).is_none());
        // 排序：点名称列为升序
        window.click("tools-sort-name", cx);
    })
    .unwrap();
    let page = cx.read(|cx| panel.read(cx).processes()).unwrap();
    cx.read(|cx| {
        let sort = page.read(cx).sort();
        assert_eq!(sort.key, ProcSort::Name);
        assert!(!sort.desc);
        assert_eq!(page.read(cx).query(cx), "ngi");
    });

    // 换了终端：各页重建，没有缓存的终端显示“正在读取”
    set_target(
        cx,
        handle,
        &panel,
        Some(target(second, ToolsState::Reconnecting)),
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(("tools-process-row", 42usize)).is_none());
    })
    .unwrap();
    // 切回来：先显示上次的结果，搜索与排序都已重置
    set_target(
        cx,
        handle,
        &panel,
        Some(target(first, ToolsState::Reconnecting)),
    );
    let page = cx.read(|cx| panel.read(cx).processes()).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(("tools-process-row", 900usize)).is_some());
    })
    .unwrap();
    cx.read(|cx| {
        assert_eq!(page.read(cx).query(cx), "");
        assert_eq!(page.read(cx).sort().key, ProcSort::Cpu);
    });

    // 终端关闭后清除它的缓存
    cx.update(|cx| panel.update(cx, |panel, _| panel.forget(first)));
    assert!(
        cx.read(|cx| panel
            .read(cx)
            .cache()
            .borrow()
            .get(&(first, Page::Processes)))
            .is_none()
    );
}

#[gpui_kit::test]
fn kill_confirmation_names_the_risk_and_pauses_refresh(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 2);
    let (first, second) = (ids[0].entity_id(), ids[1].entity_id());
    cache(
        &panel,
        cx,
        first,
        Page::Processes,
        Cached::Processes(Rc::new(processes())),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(first, ToolsState::Reconnecting)),
    );
    let page = cx.read(|cx| panel.read(cx).processes()).unwrap();

    // 选中这个终端的 shell：底部显示详情（子进程可以点）
    cx.update_window(handle, |_, window, cx| {
        window.click(("tools-process-row", 400usize), cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-process-detail").is_some());
        assert!(window.try_find(("tools-process-child", 401usize)).is_some());
        window.click("tools-process-detail-kill", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("tools-kill-risk-shell").is_some(),
            "结束本终端的 shell 有风险说明"
        );
        assert!(
            window.try_find("tools-kill-effect-kill").is_some(),
            "强制结束的后果"
        );
        assert!(window.has_active_dialog(cx));
        assert!(window.try_find("tools-confirm-host").is_some(), "写明主机");
        assert!(
            window.try_find("tools-confirm-command").is_some(),
            "写明将执行的命令"
        );
    })
    .unwrap();
    assert!(
        cx.read(|cx| page.read(cx).is_paused()),
        "确认框打开时暂停刷新"
    );

    // Esc 取消：恢复刷新
    cx.update_window(handle, |_, window, cx| window.press("escape", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(!window.has_active_dialog(cx));
    })
    .unwrap();
    assert!(!cx.read(|cx| page.read(cx).is_paused()));

    // 跳到子进程
    cx.update_window(handle, |_, window, cx| {
        window.click(("tools-process-child", 401usize), cx);
    })
    .unwrap();
    assert_eq!(cx.read(|cx| page.read(cx).selected()), Some(401));

    // 鼠标停在列表上时暂停
    cx.update_window(handle, |_, window, cx| {
        window.hover(("tools-process-row", 42usize), cx)
    })
    .unwrap();
    assert!(cx.read(|cx| page.read(cx).is_paused()));
    cx.update_window(handle, |_, window, cx| window.hover("host-tools-title", cx))
        .unwrap();
    assert!(!cx.read(|cx| page.read(cx).is_paused()));

    // 确认框开着时换了终端：关掉它，免得在别的终端上执行
    cx.update_window(handle, |_, window, cx| {
        window.click(("tools-process-row", 1usize), cx);
        window.click("tools-process-detail-term", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("tools-kill-risk-init").is_some(),
            "PID 1 的风险"
        );
    })
    .unwrap();
    set_target(
        cx,
        handle,
        &panel,
        Some(target(second, ToolsState::Reconnecting)),
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(!window.has_active_dialog(cx));
    })
    .unwrap();
}

#[gpui_kit::test]
fn hiding_the_page_clears_the_pause(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    cache(
        &panel,
        cx,
        id,
        Page::Processes,
        Cached::Processes(Rc::new(processes())),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    let page = cx.read(|cx| panel.read(cx).processes()).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.hover(("tools-process-row", 42usize), cx)
    })
    .unwrap();
    assert!(cx.read(|cx| page.read(cx).is_paused()));
    // 鼠标还停在列表上时收起面板：收不到移出，暂停随之复位，再打开时照常刷新
    cx.update(|cx| page.update(cx, |page, cx| page.set_active(false, cx)));
    assert!(!cx.read(|cx| page.read(cx).is_paused()));
}

#[gpui_kit::test]
fn pages_and_jumps_between_them(cx: &mut TestAppContext) {
    let (handle, panel, events) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let mut web = Conn {
        proto: "tcp".into(),
        state: "LISTEN".into(),
        local_addr: "0.0.0.0".into(),
        local_port: 80,
        peer_addr: "0.0.0.0".into(),
        peer_port: 0,
        recv_q: 0,
        send_q: 0,
        processes: vec![ConnProcess {
            pid: 42,
            name: "nginx".into(),
        }],
        uid: Some(0),
        user: None,
    };
    web.local_port = 80;
    let conns = ConnList {
        conns: vec![web],
        counts: Vec::new(),
        total: 1,
        truncated: false,
        source: ConnSource::Ss,
        uid: Some(0),
        sudo: false,
    };
    let key = conn_keys(&conns.conns)[0].clone();
    cache(
        &panel,
        cx,
        id,
        Page::Processes,
        Cached::Processes(Rc::new(processes())),
    );
    cache(
        &panel,
        cx,
        id,
        Page::Network,
        Cached::Network(Rc::new(conns)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );

    // 切到网络页（记在界面状态中），点进程名跳到进程页并选中
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(4usize, cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-conn-list").is_some());
        assert!(
            window.try_find("tools-conn-summary").is_none(),
            "没有汇总卡片（数量在页签上）"
        );
    })
    .unwrap();
    assert_eq!(cx.read(|cx| panel.read(cx).page()), Page::Network);
    assert_eq!(
        cx.read(|cx| AppState::global(cx)
            .ui
            .read(cx)
            .state()
            .host_tools_page
            .clone()),
        Some("network".to_string())
    );
    cx.update_window(handle, |_, window, cx| {
        window.click(format!("tools-conn-process-{key}"), cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(cx.read(|cx| panel.read(cx).page()), Page::Processes);
    let page = cx.read(|cx| panel.read(cx).processes()).unwrap();
    assert_eq!(cx.read(|cx| page.read(cx).selected()), Some(42));

    // 收起按钮
    cx.update_window(handle, |_, window, cx| window.click("host-tools-close", cx))
        .unwrap();
    assert_eq!(*events.borrow(), ["close"]);
}

#[gpui_kit::test]
fn services_and_docker_confirm_before_acting(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let ssh = Service {
        unit: "ssh.service".into(),
        description: "OpenBSD Secure Shell server".into(),
        load_state: "loaded".into(),
        active_state: "active".into(),
        sub_state: "running".into(),
        unit_file_state: "enabled".into(),
        fragment_path: Some("/lib/systemd/system/ssh.service".into()),
        main_pid: Some(900),
        custom: false,
        ..Default::default()
    };
    let services = ServiceList {
        timers: Vec::new(),
        services: vec![ssh],
        version: "systemd 255".into(),
        system_state: "degraded".into(),
        uid: Some(0),
        zone: None,
    };
    let container = |name: &str, project: Option<&str>, state: &str| Container {
        id: format!("{:0>64}", name.len()),
        name: name.into(),
        image: "nginx".into(),
        state: state.into(),
        status: String::new(),
        ports: String::new(),
        created: String::new(),
        project: project.map(Into::into),
        service: None,
        working_dir: None,
        run: None,
    };
    let image = Image {
        id: "sha256:0123456789abcdef".into(),
        repository: "nginx".into(),
        tag: "latest".into(),
        size: "188MB".into(),
        created: String::new(),
        created_at: None,
        containers: 1,
        in_use: true,
    };
    let docker = DockerInfo {
        engine: Engine::Docker,
        sudo: false,
        version: None,
        compose: None,
        containers: vec![
            container("solo", None, "exited"),
            container("shop-web-1", Some("shop"), "running"),
        ],
        // 同一个镜像的两个标签各占一行
        images: vec![
            image.clone(),
            Image {
                tag: "1.27".into(),
                ..image
            },
        ],
        volumes: Vec::new(),
        networks: Vec::new(),
    };
    cache(
        &panel,
        cx,
        id,
        Page::Services,
        Cached::Services(Rc::new(services)),
    );
    cache(
        &panel,
        cx,
        id,
        Page::Docker,
        Cached::Docker(Rc::new(docker)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );

    // 服务：停止 SSH 服务重点警告
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(2usize, cx);
        window.click("tools-service-ssh.service", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-service-detail").is_some());
        assert!(
            window.try_find("tools-service-detail-start").is_none(),
            "运行中的不能再启动"
        );
        window.click("tools-service-detail-stop", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-service-risk-connection").is_some());
        assert!(window.try_find("tools-confirm-host").is_some());
        assert!(window.try_find("tools-confirm-command").is_some());
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();

    // compose 项目整体停止：写明主机、涉及的容器与将执行的命令
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(3usize, cx);
        window.render_frame(cx);
        window.click("tools-project-stop-shop", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-docker-confirm").is_some());
        assert!(window.try_find("tools-confirm-host").is_some());
        assert!(window.try_find("tools-confirm-command").is_some());
        assert!(
            window.try_find("tools-docker-confirm-effect").is_some(),
            "写明停止的后果"
        );
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();

    // Docker：按 compose 项目分组，单独的容器在后；使用中的镜像不能删除
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(!window.has_active_dialog(cx));
        window.within("host-tools-pages").click(3usize, cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-container-group-shop").is_some());
        assert!(window.try_find("tools-container-group-").is_some());
        window.within("tools-docker-tabs").click(2usize, cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find("tools-item-image-sha256:0123456789abcdef-nginx:1.27")
                .is_some()
        );
        // 删除按钮悬停时才出现
        assert!(
            window
                .try_find("tools-remove-image-sha256:0123456789abcdef-nginx:latest")
                .is_none()
        );
        window.hover("tools-item-image-sha256:0123456789abcdef-nginx:latest", cx);
        window.render_frame(cx);
        window.click(
            "tools-remove-image-sha256:0123456789abcdef-nginx:latest",
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(!window.has_active_dialog(cx), "使用中的镜像点删除没有反应");
    })
    .unwrap();
}

/// 概览页显示跟随的终端自己的采样：CPU、内存、网络、磁盘（按占用排序、带 inode）；默认不列虚拟网卡与没有收发过的
/// 网卡，可以展开全部；每核可以展开。点进程跳到进程页并选中。资源监控关闭、服务器不是 Linux 时说明原因。
#[gpui_kit::test]
fn overview_shows_the_terminal_samples(cx: &mut TestAppContext) {
    use termlet_core::monitor::{Details, Disk, Inodes, NetIface};
    use termlet_core::ssh::{Answer, Prompter};

    use super::model::OverviewData;
    use crate::monitor_bar::Resources;
    use crate::terminal_panel::{Session, TerminalPanel};

    let (handle, panel, _) = open(cx);
    // 整页放得下，不用滚动
    cx.simulate_window_resize(handle, size(px(1024.), px(1800.)));
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
    let iface = |name: &str, total: u64| NetIface {
        name: name.into(),
        rx_total: total,
        tx_total: total,
        rx_rate: Some(2048.),
        tx_rate: Some(512.),
    };
    let disk = |mount: &str, used: u64| Disk {
        mount: mount.into(),
        filesystem: "/dev/vda1".into(),
        total: 100,
        used,
    };
    // 最近 10 分钟，每 3 秒一次
    let samples = (0..=200).map(|i| termlet_core::monitor::Sample {
        per_core: vec![12., 97.],
        cores: 2,
        iowait: Some(1.),
        steal: Some(0.),
        disks: vec![disk("/", 30), disk("/data", 91)],
        net: vec![
            iface("eth0", 10),
            iface("veth0", 10),
            iface("docker0", 10),
            iface("eth1", 0),
        ],
        ..crate::monitor_model::test_sample(i * 3000)
    });
    cx.update(|cx| terminal.update(cx, |t, cx| t.feed_samples(samples, cx)));
    let id = terminal.entity_id();
    let details = Details {
        system: Some("Alpine Linux v3.23".into()),
        kernel: Some("Linux 6.18.33".into()),
        arch: Some("x86_64".into()),
        inodes: vec![Inodes {
            mount: "/data".into(),
            total: 100,
            used: 80,
        }],
    };
    cache(
        &panel,
        cx,
        id,
        Page::Processes,
        Cached::Processes(Rc::new(processes())),
    );
    cache(
        &panel,
        cx,
        id,
        Page::Overview,
        Cached::Overview(Rc::new(OverviewData::new(Some(details), &processes()))),
    );
    let followed = ToolsTarget {
        terminal: terminal.downgrade(),
        ..target(id, ToolsState::Reconnecting)
    };
    set_target(cx, handle, &panel, Some(followed));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(0usize, cx);
        window.render_frame(cx);
        for element in [
            "tools-overview-system",
            "tools-overview-facts",
            "tools-overview-cpu-facts",
            "tools-overview-cpu",
            "tools-overview-memory",
            "tools-overview-net",
            "tools-overview-disk-/data",
            "tools-overview-iface-eth0",
        ] {
            assert!(window.try_find(element).is_some(), "{element}");
        }
        // 磁盘按占用排序
        assert!(
            window.find("tools-overview-disk-/data").bounds().top()
                < window.find("tools-overview-disk-/").bounds().top()
        );
        // CPU 占用最高的进程紧接在 CPU 之后；每核是一排细竖条；曲线的说明在 ⓘ 中
        let top = window.find("tools-overview-top").bounds().top();
        assert!(window.find("tools-overview-cpu").bounds().top() < top);
        assert!(top < window.find("tools-overview-memory").bounds().top());
        assert!(
            window
                .try_find(("tools-overview-core-bar", 1usize))
                .is_some()
        );
        assert!(window.try_find("tools-overview-history-hint").is_some());
        // 鼠标停在曲线上：写出那一刻的时间与读数
        assert!(
            window
                .try_find("tools-overview-cpu-chart-readout")
                .is_none()
        );
        window.hover("tools-overview-cpu-chart", cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find("tools-overview-cpu-chart-readout")
                .is_some()
        );
        window.hover("tools-overview-memory-chart", cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find("tools-overview-cpu-chart-readout")
                .is_none()
        );
        assert!(
            window
                .try_find("tools-overview-memory-chart-readout")
                .is_some()
        );
        for hidden in ["tools-overview-iface-veth0", "tools-overview-iface-eth1"] {
            assert!(window.try_find(hidden).is_none(), "默认不列 {hidden}");
        }
        window.click("tools-overview-ifaces", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-overview-iface-veth0").is_some());
        assert!(window.try_find(("tools-overview-core", 1usize)).is_none());
        window.click("tools-overview-cores", cx);
        window.render_frame(cx);
        assert!(window.try_find(("tools-overview-core", 1usize)).is_some());
        // CPU 最高的是 900（缓存的进程中 CPU 为 PID 除以 7 的余数）
        window.click(("tools-overview-proc", 900usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(cx.read(|cx| panel.read(cx).page()), Page::Processes);
    let page = cx.read(|cx| panel.read(cx).processes()).unwrap();
    assert_eq!(cx.read(|cx| page.read(cx).selected()), Some(900));
    assert!(
        cx.read(|cx| panel.read(cx).overview()).is_some(),
        "概览页随各页一起建好"
    );

    // 设置中关闭了资源监控：说明，可以直接打开
    cx.update(|cx| {
        AppState::store(cx)
            .update_settings(|s| s.terminal.monitor = false)
            .unwrap()
    });
    cx.update(|cx| terminal.update(cx, |t, cx| t.set_resources(Resources::Off, cx)));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(0usize, cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-overview-monitor_off").is_some());
        window.click("tools-overview-monitor-on", cx);
    })
    .unwrap();
    assert!(cx.update(|cx| AppState::store(cx).settings().terminal.monitor));
    // 服务器不是 Linux
    cx.update(|cx| terminal.update(cx, |t, cx| t.set_resources(Resources::Unsupported, cx)));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-overview-unsupported").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn resizer_changes_and_resets_the_width(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let width = |cx: &mut TestAppContext| {
        cx.read(|cx| AppState::global(cx).ui.read(cx).state().host_tools_width)
    };
    // 向左拖 100 变宽 100（窗口宽 1024，默认 400）
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let center = window.find("host-tools-resizer").bounds().center();
        window.drag(center, point(center.x - px(100.), center.y), cx);
    })
    .unwrap();
    let dragged = width(cx).expect("拖动后记下宽度");
    assert!((495. ..=505.).contains(&dragged), "{dragged}");
    // 获得焦点后 ← 变宽一步
    cx.update_window(handle, |_, window, cx| {
        window.click("host-tools-resizer", cx);
        window.press("left", cx);
    })
    .unwrap();
    assert_eq!(width(cx), Some(dragged + super::model::WIDTH_STEP));
    // 不超过窗口留给左侧之后的宽度
    cx.update_window(handle, |_, window, cx| {
        for _ in 0..20 {
            window.press("left", cx);
        }
    })
    .unwrap();
    assert_eq!(width(cx), Some(1024. - super::model::REST_MIN));
    // 双击恢复默认
    cx.update_window(handle, |_, window, cx| {
        window.double_click("host-tools-resizer", cx)
    })
    .unwrap();
    assert_eq!(width(cx), None);
    drop(panel);
}

#[gpui_kit::test]
fn reads_and_acts_on_a_real_host(cx: &mut TestAppContext) {
    use termlet_core::model::{Host, SecretField};
    use termlet_core::secret::SecretString;
    use termlet_core::ssh::{Answer, ConnectOptions, Prompt, Prompter, connect, plan_for_host};
    use termlet_core::store::{Owner, Position};
    let Some((address, port)) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let (handle, panel, _) = open(cx);
    let store = cx.update(|cx| AppState::store(cx));
    let host = store
        .create(
            Host {
                name: "tools".into(),
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
    let plan = plan_for_host(&store, &host).unwrap();
    let prompter: Prompter = Arc::new(|prompt| {
        Box::pin(async move {
            match prompt {
                Prompt::HostKey(_) => Answer::AcceptHostKey,
                _ => Answer::Cancel,
            }
        })
    });
    let connection = termlet_core::runtime::runtime()
        .block_on(connect(
            store.clone(),
            &plan,
            prompter,
            &ConnectOptions::default(),
        ))
        .expect("连接测试用的 sshd");
    // 在服务器上留一个要结束的进程
    let started = termlet_core::runtime::runtime()
        .block_on(termlet_core::ssh::exec(
            &connection,
            "nohup sleep 345 >/dev/null 2>&1 & echo $!",
            4096,
            std::time::Duration::from_secs(10),
        ))
        .expect("启动 sleep");
    let sleep_pid: u32 = String::from_utf8_lossy(&started.stdout)
        .trim()
        .parse()
        .unwrap();

    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let connected = ToolsState::Connected(Target::ssh(connection.clone(), false));
    set_target(cx, handle, &panel, Some(target(id, connected)));
    let row = ("tools-process-row", sleep_pid as usize);
    wait_until(cx, handle, "读到进程列表", |window, _| {
        window.try_find(row).is_some()
    });

    // “在终端中执行”的命令结束后重读发起操作的页：只重读跟着那个终端、正在显示的页
    let processes = cx.read(|cx| panel.read(cx).processes()).unwrap();
    let loading = |cx: &mut TestAppContext| cx.read(|cx| processes.read(cx).is_loading());
    poll_until(cx, "这一次读完", |cx| !loading(cx));
    let others = terminal_ids(cx, 1);
    cx.update(|cx| {
        panel.update(cx, |p, cx| {
            p.refresh_page_of(others[0].entity_id(), Page::Processes, cx);
            p.refresh_page_of(id, Page::Services, cx);
        })
    });
    assert!(!loading(cx), "别的终端、不在显示的页不重读");
    cx.update(|cx| panel.update(cx, |p, cx| p.refresh_page_of(id, Page::Processes, cx)));
    assert!(loading(cx), "重读进程页");
    poll_until(cx, "重读完", |cx| !loading(cx));

    // 结束它：确认后执行，之后重读
    cx.update_window(handle, |_, window, cx| {
        window.click(row, cx);
        window.render_frame(cx);
        window.click("tools-process-detail-term", cx);
    })
    .unwrap();
    wait_until(cx, handle, "确认框", |window, _| {
        window.try_find("tools-kill-risk-normal").is_some()
    });
    let page = cx.read(|cx| panel.read(cx).processes()).unwrap();
    assert_eq!(
        cx.read(|cx| page.read(cx).cmdline_of(sleep_pid)),
        Some("sleep 345".to_string()),
        "列表中读到命令行"
    );
    cx.update_window(handle, |_, window, cx| window.click("ok", cx))
        .unwrap();
    let op = ("tools-process-op", sleep_pid as usize);
    let mut saw_op = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            saw_op |= window.try_find(op).is_some();
        })
        .unwrap();
        // 操作之后的重读在 0.8 秒之后：推进测试的时钟
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        if !cx.read(|cx| page.read(cx).pids()).contains(&sleep_pid) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "等待进程结束");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(saw_op, "行上说明结束进行中、已结束");
    // 成功：这一页还显示着，由它在行上说明，不弹出提示
    cx.update_window(handle, |_, window, cx| {
        assert!(window.notifications(cx).is_empty(), "不弹出成功的提示");
    })
    .unwrap();

    // e2e 的容器是 Alpine：没有 systemd、docker
    cx.update_window(handle, |_, window, cx| {
        window.within("host-tools-pages").click(2usize, cx)
    })
    .unwrap();
    wait_until(cx, handle, "服务页说明没有 systemd", |window, _| {
        window.try_find("tools-notice-no_systemd").is_some()
    });
    cx.update_window(handle, |_, window, cx| {
        window.within("host-tools-pages").click(3usize, cx)
    })
    .unwrap();
    wait_until(cx, handle, "Docker 页说明没有 Docker", |window, _| {
        window.try_find("tools-notice-no_docker").is_some()
    });
    cx.update_window(handle, |_, window, cx| {
        window.within("host-tools-pages").click(4usize, cx)
    })
    .unwrap();
    wait_until(cx, handle, "网络页的列表", |window, _| {
        window.try_find("tools-conn-list").is_some()
    });
    termlet_core::runtime::runtime().block_on(connection.disconnect());
}

// 面板拖到最窄时，顶部各页与各页的筛选页签在每种语言下都放得下：英文的放不下时放宽面板的最小宽度
#[gpui_kit::test]
fn tabs_fit_at_the_minimum_width_in_every_language(cx: &mut TestAppContext) {
    tabs_fit_at_the_minimum_width(cx, termlet_core::ui_scale::DEFAULT);
}

// 界面放大到 150% 时同样放得下：最小宽度随界面一起放大，页签也不被裁掉
#[gpui_kit::test]
fn tabs_fit_at_the_minimum_width_at_150_percent(cx: &mut TestAppContext) {
    tabs_fit_at_the_minimum_width(cx, 150);
}

fn tabs_fit_at_the_minimum_width(cx: &mut TestAppContext, percent: u16) {
    use crate::i18n::{Language, set_language};
    let (handle, panel, _) = open(cx);
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.window.ui_scale = percent)
            .unwrap()
    });
    cx.run_until_parked();
    let scale = f32::from(percent) / 100.;
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    // 各页都有数据（显示着筛选页签）
    let pages = [
        (Page::Processes, Cached::Processes(Rc::new(processes()))),
        (
            Page::Services,
            Cached::Services(Rc::new(ServiceList {
                timers: Vec::new(),
                services: Vec::new(),
                version: "systemd 255".into(),
                system_state: "running".into(),
                uid: Some(0),
                zone: None,
            })),
        ),
        (
            Page::Docker,
            Cached::Docker(Rc::new(DockerInfo {
                engine: Engine::Docker,
                sudo: false,
                version: None,
                compose: None,
                containers: Vec::new(),
                images: Vec::new(),
                volumes: Vec::new(),
                networks: Vec::new(),
            })),
        ),
        (
            Page::Network,
            Cached::Network(Rc::new(ConnList {
                conns: Vec::new(),
                counts: Vec::new(),
                total: 0,
                truncated: false,
                source: ConnSource::Ss,
                uid: Some(0),
                sudo: false,
            })),
        ),
    ];
    for (page, data) in pages {
        cache(&panel, cx, id, page, data);
    }
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    cx.update(|cx| {
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.host_tools_width = Some(super::model::WIDTH_MIN), cx)
        })
    });
    // 英文的页签最宽，放在最后：循环之后检查面板按它放宽了
    let languages = Language::ALL.into_iter().filter(|&l| l != Language::En);
    for language in languages.chain([Language::En]) {
        set_language(language);
        for ix in 0..Page::ALL.len() {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.within("host-tools-pages").click(ix, cx);
                // 第一帧量出页签排开的宽度，下一帧按它放宽
                window.render_frame(cx);
                window.render_frame(cx);
            })
            .unwrap();
            let overflow = cx.read(|cx| panel.read(cx).tabs_overflow()(cx));
            assert!(
                overflow.is_some_and(|o| o <= px(0.5)),
                "{language:?}，第 {ix} 页：页签比可见区域宽出 {overflow:?}"
            );
        }
    }
    let width = cx
        .update_window(handle, |_, window, _| {
            window.find("host-tools").bounds().size.width
        })
        .unwrap();
    assert!(
        width > px(super::model::WIDTH_MIN * scale),
        "英文的页签放不下最小宽度：面板放宽了（{width:?}）"
    );
    set_language(Language::ZhCn);
}

/// 进程详情的“父进程”、服务详情的“主进程”：按钮按自己的宽度左对齐，与同一列的其他值对齐（不被拉宽后把文字居中）。
#[gpui_kit::test]
fn detail_links_are_left_aligned(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let service = Service {
        unit: "demo-app.service".into(),
        description: "Demo".into(),
        load_state: "loaded".into(),
        active_state: "active".into(),
        sub_state: "running".into(),
        main_pid: Some(42),
        memory: Some(12 << 20),
        tasks: Some(4),
        ..Default::default()
    };
    cache(
        &panel,
        cx,
        id,
        Page::Processes,
        Cached::Processes(Rc::new(processes())),
    );
    cache(
        &panel,
        cx,
        id,
        Page::Services,
        Cached::Services(Rc::new(ServiceList {
            timers: Vec::new(),
            services: vec![service],
            version: "systemd 257".into(),
            system_state: "running".into(),
            uid: Some(0),
            zone: None,
        })),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    cx.update_window(handle, |_, window, cx| {
        window.click(("tools-process-row", 400usize), cx);
        window.render_frame(cx);
        let parent = window.find(("tools-process-parent", 1usize)).bounds();
        let command = window.find("tools-process-command").bounds();
        assert!(
            (parent.left() - command.left()).abs() < px(1.),
            "与命令行同一列左对齐：{parent:?} / {command:?}"
        );
        assert!(
            parent.size.width < command.size.width / 2.,
            "按钮按自己的宽度，不被拉宽：{parent:?}"
        );

        window.within("host-tools-pages").click(2usize, cx);
        window.click("tools-service-demo-app.service", cx);
        window.render_frame(cx);
        let main_pid = window.find("tools-service-main-pid").bounds();
        let usage = window.find("tools-service-usage").bounds();
        assert!(
            (main_pid.left() - usage.left()).abs() < px(1.),
            "与同一列的值左对齐：{main_pid:?} / {usage:?}"
        );
        assert!(main_pid.size.width < usage.size.width / 2.);
    })
    .unwrap();
}

/// 镜像、卷、网络的行：两行文字都在行内（第二行不被挤到行底、被下一行盖住），删除按钮对着整行居中。
#[gpui_kit::test]
fn docker_item_rows_keep_both_lines_inside(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let volume = |name: &str, containers: usize| Volume {
        name: name.into(),
        driver: "local".into(),
        mountpoint: format!("/var/lib/docker/volumes/{name}/_data"),
        containers,
        in_use: containers > 0,
    };
    cache(
        &panel,
        cx,
        id,
        Page::Docker,
        Cached::Docker(Rc::new(DockerInfo {
            engine: Engine::Docker,
            sudo: false,
            version: None,
            compose: None,
            containers: Vec::new(),
            images: Vec::new(),
            volumes: vec![volume("orphan-vol", 0), volume("shop-data", 1)],
            networks: Vec::new(),
        })),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(3usize, cx);
        window.render_frame(cx);
        window.within("tools-docker-tabs").click(3usize, cx);
        window.render_frame(cx);
        assert!(
            window.try_find("tools-remove-volume-orphan-vol").is_none(),
            "删除按钮悬停时才出现"
        );
        window.hover("tools-item-volume-orphan-vol", cx);
        window.render_frame(cx);
        let first = window.find("tools-item-volume-orphan-vol").bounds();
        let detail = window.find("tools-item-detail-volume-orphan-vol").bounds();
        let second = window.find("tools-item-volume-shop-data").bounds();
        let remove = window.find("tools-remove-volume-orphan-vol").bounds();
        assert!(remove.right() <= first.right() + px(0.5), "浮在行的右端");
        assert!(
            detail.bottom() <= first.bottom() + px(0.5) && detail.top() > first.top(),
            "第二行在自己的行内：{detail:?} / {first:?}"
        );
        assert!(detail.bottom() <= second.top() + px(0.5));
        assert!(
            (remove.center().y - first.center().y).abs() < px(1.),
            "删除按钮对着整行居中：{remove:?} / {first:?}"
        );
    })
    .unwrap();
}

/// 行的右键菜单打开期间，行上的悬停提示不显示（菜单打开时还在等待显示的提示也一样），菜单关闭后恢复：行上的提示
/// 经 `crate::menu_tooltip::quiet` 包过，右键菜单打开期间算作窗口中开着菜单（`quiet` 本身的行为见它的测试）。
#[gpui_kit::test]
fn row_tooltips_stay_hidden_while_the_menu_is_open(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let conn = Conn {
        proto: "tcp".into(),
        state: "LISTEN".into(),
        local_addr: "0.0.0.0".into(),
        local_port: 8080,
        peer_addr: "0.0.0.0".into(),
        peer_port: 0,
        recv_q: 0,
        send_q: 0,
        processes: Vec::new(),
        uid: Some(0),
        user: None,
    };
    let key = conn_keys(std::slice::from_ref(&conn))[0].clone();
    cache(
        &panel,
        cx,
        id,
        Page::Network,
        Cached::Network(Rc::new(ConnList {
            conns: vec![conn],
            counts: Vec::new(),
            total: 1,
            truncated: false,
            source: ConnSource::Ss,
            uid: Some(0),
            sudo: false,
        })),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    let menu_shown = |cx: &mut TestAppContext| {
        cx.update_window(handle, |_, window, cx| menu_open(window, cx))
            .unwrap()
    };
    cx.update_window(handle, |_, window, cx| {
        assert!(is_quiet(row_tooltip("0.0.0.0:8080".into())(window, cx)));
    })
    .unwrap();
    assert!(!menu_shown(cx), "平时照常显示");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(4usize, cx);
        window.render_frame(cx);
        window.hover(format!("tools-conn-local-{key}"), cx);
        window.right_click(format!("tools-conn-local-{key}"), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("popup-menu").is_some(), "右键菜单打开了");
    })
    .unwrap();
    assert!(menu_shown(cx), "菜单打开期间不显示");

    cx.update_window(handle, |_, window, cx| window.press("escape", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("popup-menu").is_none());
    })
    .unwrap();
    assert!(!menu_shown(cx), "菜单关闭后恢复");
}

/// 失败的服务：详情直接显示失败的原因（结果与退出码）与运行信息，另读最近几行日志（终端重连中时说明读不了）；
/// 运行中的服务不读日志。
#[gpui_kit::test]
fn failed_service_shows_why_in_the_detail(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let broken = Service {
        unit: "broken.service".into(),
        description: "Broken worker".into(),
        load_state: "loaded".into(),
        active_state: "failed".into(),
        sub_state: "failed".into(),
        result: "exit-code".into(),
        main_exit: Some(MainExit::Code(3)),
        state_changed: Some(termlet_core::time::now_ms() - 60_000),
        restarts: Some(5),
        cpu_nsec: Some(3_200_000_000),
        ..Default::default()
    };
    let demo = Service {
        unit: "demo-app.service".into(),
        description: "Demo".into(),
        load_state: "loaded".into(),
        active_state: "active".into(),
        sub_state: "running".into(),
        active_since: Some(termlet_core::time::now_ms() - 7_200_000),
        ..Default::default()
    };
    cache(
        &panel,
        cx,
        id,
        Page::Services,
        Cached::Services(Rc::new(ServiceList {
            timers: Vec::new(),
            services: vec![broken, demo],
            version: "systemd 257".into(),
            system_state: "degraded".into(),
            uid: Some(0),
            zone: None,
        })),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(2usize, cx);
        window.click("tools-service-broken.service", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("tools-service-failure").is_some(),
            "失败的原因"
        );
        assert!(window.try_find("tools-service-usage").is_some(), "运行信息");
        assert!(
            window.try_find("tools-service-tail").is_some(),
            "最近的日志"
        );
    })
    .unwrap();

    // 可能因为权限看不全时说明（看不到时代替“没有日志”，看到一部分时写在日志上方）
    let page = cx.read(|cx| panel.read(cx).services()).expect("服务页");
    let logs = |lines: Vec<LogLine>, limited| LogResult {
        lines,
        truncated: false,
        sudo: false,
        limited,
        cursor: None,
    };
    let some = || {
        vec![LogLine {
            at: None,
            text: "broken[1]: starting".into(),
            level: LogLevel::Normal,
        }]
    };
    for (lines, limited) in [(Vec::new(), true), (some(), true), (some(), false)] {
        cx.update(|cx| page.update(cx, |page, cx| page.set_test_tail(logs(lines, limited), cx)));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.try_find("tools-service-tail-limited").is_some(),
                limited
            );
        })
        .unwrap();
    }

    cx.update_window(handle, |_, window, cx| {
        window.click("tools-service-demo-app.service", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-service-failure").is_none());
        assert!(
            window.try_find("tools-service-tail").is_none(),
            "运行中的不读日志"
        );
    })
    .unwrap();
}

/// Docker 页：compose 项目只显示能做的操作；“异常”子页只列非 0 退出、不健康的；行上显示资源占用；选中容器时底部
/// 显示详情（端口可转发到本机，绑定挂载可在 SFTP 中打开，最近日志），“查看日志”打开日志查看器。
#[gpui_kit::test]
fn docker_problems_usage_and_detail(cx: &mut TestAppContext) {
    use termlet_core::hosttools::{ContainerDetail, ContainerStats, MountInfo, PortBinding};
    let (handle, panel, events) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let container =
        |n: u8, name: &str, project: Option<&str>, state: &str, status: &str| Container {
            id: format!("{n:0>64}"),
            name: name.into(),
            image: "nginx:alpine".into(),
            state: state.into(),
            status: status.into(),
            ports: if n == 1 {
                "0.0.0.0:8081->80/tcp, :::8081->80/tcp".into()
            } else {
                String::new()
            },
            created: String::new(),
            project: project.map(Into::into),
            service: None,
            working_dir: None,
            run: None,
        };
    let docker = DockerInfo {
        engine: Engine::Docker,
        sudo: false,
        version: None,
        compose: None,
        containers: vec![
            container(1, "shop-web-1", Some("shop"), "running", "Up 1 hour"),
            container(2, "shop-cache-1", Some("shop"), "running", "Up 1 hour"),
            container(
                3,
                "blog-web-1",
                Some("blog"),
                "exited",
                "Exited (0) 2 hours ago",
            ),
            container(4, "oneshot", None, "exited", "Exited (1) 5 minutes ago"),
            container(5, "sick", None, "running", "Up 3 minutes (unhealthy)"),
        ],
        images: Vec::new(),
        volumes: Vec::new(),
        networks: Vec::new(),
    };
    let web = docker.containers[0].id.clone();
    cache(
        &panel,
        cx,
        id,
        Page::Docker,
        Cached::Docker(Rc::new(docker)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    let row = |n: u8| format!("tools-container-{n:0>64}");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(3usize, cx);
        window.render_frame(cx);
        // 全部运行着的项目不能再启动，全部停着的不能停止、重启
        assert!(window.try_find("tools-project-start-shop").is_none());
        assert!(window.try_find("tools-project-stop-shop").is_some());
        assert!(window.try_find("tools-project-start-blog").is_some());
        assert!(window.try_find("tools-project-stop-blog").is_none());
        assert!(window.try_find("tools-project-restart-blog").is_none());
        // 异常：非 0 退出与不健康的，正常退出的不算
        window.within("tools-docker-tabs").click(1usize, cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find(gpui_kit::SharedString::from(row(4)))
                .is_some()
        );
        assert!(
            window
                .try_find(gpui_kit::SharedString::from(row(5)))
                .is_some()
        );
        assert!(
            window
                .try_find(gpui_kit::SharedString::from(row(3)))
                .is_none()
        );
        assert!(
            window
                .try_find(gpui_kit::SharedString::from(row(1)))
                .is_none()
        );
        window.within("tools-docker-tabs").click(0usize, cx);
        window.render_frame(cx);
    })
    .unwrap();

    // 资源占用（测试中直接放进读数）
    let page = cx.read(|cx| panel.read(cx).docker().unwrap());
    cx.update(|cx| {
        page.update(cx, |page, cx| {
            page.set_test_stats(
                vec![ContainerStats {
                    id: web.clone(),
                    cpu: Some(95.),
                    mem_used: Some(20 << 20),
                    mem_limit: Some(1 << 30),
                    mem_percent: Some(2.),
                }],
                cx,
            )
        })
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .try_find(gpui_kit::SharedString::from(format!(
                    "tools-container-usage-{web}"
                )))
                .is_some()
        );
        window.click(gpui_kit::SharedString::from(row(1)), cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-container-detail").is_some());
    })
    .unwrap();
    let detail = ContainerDetail {
        id: web.clone(),
        image: "nginx:alpine".into(),
        status: "running".into(),
        restart_policy: "unless-stopped".into(),
        ports: vec![PortBinding {
            container_port: 80,
            proto: "tcp".into(),
            host_port: Some(8081),
            host_ip: None,
        }],
        mounts: vec![MountInfo {
            kind: "bind".into(),
            name: None,
            source: "/srv/shop/html".into(),
            destination: "/usr/share/nginx/html".into(),
            read_only: true,
        }],
        ..Default::default()
    };
    let logs = LogResult {
        lines: vec![LogLine {
            at: Some(1_793_756_832_000),
            text: "ERROR boom".into(),
            level: LogLevel::Error,
        }],
        truncated: false,
        sudo: false,
        limited: false,
        cursor: None,
    };
    cx.update(|cx| page.update(cx, |page, cx| page.set_test_detail(detail, logs, cx)));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-container-ports").is_some());
        assert!(window.try_find("tools-container-tail").is_some());
        // 头部：运行中的容器直接显示停止、重启，其余在 ⋯ 中
        assert!(window.try_find("tools-container-detail-stop").is_some());
        assert!(window.try_find("tools-container-detail-restart").is_some());
        assert!(window.try_find("tools-container-detail-start").is_none());
        window.click("tools-container-forward-80-tcp-8081", cx);
        // 发布的 TCP 端口：在浏览器中打开
        window.click("tools-container-browser-80-tcp-8081", cx);
        window.click("tools-container-sftp-0", cx);
    })
    .unwrap();
    cx.run_until_parked();
    click_more(
        cx,
        handle,
        "tools-container-detail-more",
        t!("tools.logs.view"),
    );
    assert_eq!(*events.borrow(), ["forward", "browser", "sftp", "logs"]);
}

/// 点详情头部的 ⋯（元素 ID 为 `more`），在弹出的菜单中点文案为 `label` 的一项。
fn click_more(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    more: &'static str,
    label: gpui_kit::SharedString,
) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(more, cx);
    })
    .unwrap();
    cx.run_until_parked();
    let ix = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let menu = window.within("popup-menu");
            (0..30usize).find(|ix| {
                menu.try_find(*ix)
                    .and_then(|item| item.label().map(str::to_string))
                    .as_deref()
                    == Some(label.as_ref())
            })
        })
        .unwrap()
        .unwrap_or_else(|| panic!("⋯ 中没有“{label}”"));
    cx.update_window(handle, |_, window, cx| {
        window.within("popup-menu").click(ix, cx)
    })
    .unwrap();
    cx.run_until_parked();
}

// ------------------------------------------------------------ 互相跳转与键盘操作

/// 容器 web（发布 8081）的 ID。
const WEB_ID: &str = "c07d8b88b8c6b55cb13f9e92654df607a79449021ceeeb816e058a36b291dc83";

/// 进程页的数据外加容器 web 中的 nginx（500）与 cron 服务的 cron（600）。
fn owned_processes() -> ProcessList {
    let mut list = processes();
    list.processes.push(Process {
        container: Some(WEB_ID.into()),
        ..process(500, 1, "nginx")
    });
    list.processes.push(Process {
        service: Some("cron.service".into()),
        ..process(600, 1, "cron")
    });
    list.total = list.processes.len();
    list
}

fn web_docker() -> DockerInfo {
    DockerInfo {
        engine: Engine::Docker,
        sudo: false,
        version: None,
        compose: None,
        containers: vec![Container {
            id: WEB_ID.into(),
            name: "web".into(),
            image: "nginx:alpine".into(),
            state: "running".into(),
            status: "Up 1 hour".into(),
            ports: "0.0.0.0:8081->80/tcp, :::8081->80/tcp".into(),
            created: String::new(),
            project: None,
            service: None,
            working_dir: None,
            run: None,
        }],
        images: Vec::new(),
        volumes: Vec::new(),
        networks: Vec::new(),
    }
}

fn cron_services() -> ServiceList {
    ServiceList {
        timers: Vec::new(),
        services: vec![Service {
            unit: "cron.service".into(),
            description: "Regular background program processing daemon".into(),
            load_state: "loaded".into(),
            active_state: "active".into(),
            sub_state: "running".into(),
            main_pid: Some(600),
            ..Default::default()
        }],
        version: "systemd 257".into(),
        system_state: "running".into(),
        uid: Some(0),
        zone: None,
    }
}

fn listen(local: &str, port: u16, pid: u32, name: &str) -> Conn {
    Conn {
        proto: "tcp".into(),
        state: "LISTEN".into(),
        local_addr: local.into(),
        local_port: port,
        peer_addr: "0.0.0.0".into(),
        peer_port: 0,
        recv_q: 0,
        send_q: 0,
        processes: vec![ConnProcess {
            pid,
            name: name.into(),
        }],
        uid: Some(0),
        user: None,
    }
}

fn conn_list(conns: Vec<Conn>) -> ConnList {
    ConnList {
        total: conns.len(),
        conns,
        counts: Vec::new(),
        truncated: false,
        source: ConnSource::Ss,
        uid: Some(0),
        sudo: false,
    }
}

/// 一个终端的各页都有缓存：进程、服务、Docker、网络（sshd、cron 与容器 web 发布的 8081）。
fn open_with_everything(
    cx: &mut TestAppContext,
) -> (
    AnyWindowHandle,
    Entity<HostToolsPanel>,
    Rc<RefCell<Vec<&'static str>>>,
    Vec<Entity<()>>,
) {
    let (handle, panel, events) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let mut cron = listen("127.0.0.1", 6000, 600, "cron");
    cron.state = "ESTAB".into();
    cron.peer_addr = "127.0.0.1".into();
    cron.peer_port = 7000;
    let conns = conn_list(vec![
        listen("0.0.0.0", 22, 900, "sshd"),
        listen("::", 22, 900, "sshd"),
        listen("0.0.0.0", 8081, 700, "docker-proxy"),
        listen("::", 8081, 701, "docker-proxy"),
        listen("0.0.0.0", 9090, 702, "docker-proxy"),
        cron,
    ]);
    for (page, data) in [
        (
            Page::Processes,
            Cached::Processes(Rc::new(owned_processes())),
        ),
        (Page::Services, Cached::Services(Rc::new(cron_services()))),
        (Page::Docker, Cached::Docker(Rc::new(web_docker()))),
        (Page::Network, Cached::Network(Rc::new(conns))),
    ] {
        cache(&panel, cx, id, page, data);
    }
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    (handle, panel, events, ids)
}

fn page_of(cx: &mut TestAppContext, panel: &Entity<HostToolsPanel>) -> Page {
    cx.read(|cx| panel.read(cx).page())
}

/// 进程行标出所在的容器（Docker 页读到的名称）与所属的服务；可以隐藏容器内的进程（筛选条件是可清除的小标签）；
/// 详情中跳到所属的服务、所在的容器与这个进程的连接（网络页按进程筛选，不改搜索框）。
#[gpui_kit::test]
fn processes_show_owners_and_jump_to_them(cx: &mut TestAppContext) {
    let (handle, panel, _, _ids) = open_with_everything(cx);
    let processes = cx.read(|cx| panel.read(cx).processes()).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .try_find(("tools-process-container", 500usize))
                .is_some()
        );
        assert!(
            window
                .try_find(("tools-process-service", 600usize))
                .is_some()
        );
        assert!(
            window
                .try_find(("tools-process-container", 600usize))
                .is_none()
        );
        // 选中着容器内的进程时隐藏它们：取消选中（详情不留着看不到的进程）
        window.click(("tools-process-row", 500usize), cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-process-detail").is_some());
        window.click("tools-process-hide-containers", cx);
        window.render_frame(cx);
        assert!(
            window.try_find(("tools-process-row", 500usize)).is_none(),
            "隐藏容器内的进程"
        );
        assert!(window.try_find(("tools-process-row", 600usize)).is_some());
        assert!(window.try_find("tools-process-chip-hidden").is_some());
        assert!(window.try_find("tools-process-detail").is_none());
        window.click("tools-process-chip-hidden-clear", cx);
        window.render_frame(cx);
        assert!(window.try_find(("tools-process-row", 500usize)).is_some());
        assert!(window.try_find("tools-process-chip-hidden").is_none());
        // 所属服务
        window.click(("tools-process-row", 600usize), cx);
        window.render_frame(cx);
        window.click("tools-process-service-link", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(page_of(cx, &panel), Page::Services);
    let services = cx.read(|cx| panel.read(cx).services()).unwrap();
    assert_eq!(
        cx.read(|cx| services.read(cx).selected_unit()).as_deref(),
        Some("cron.service")
    );

    // 所在容器：跳到 Docker 页并选中它
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(1usize, cx);
        window.render_frame(cx);
        window.click(("tools-process-row", 500usize), cx);
        window.render_frame(cx);
        window.click("tools-process-container-link", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(page_of(cx, &panel), Page::Docker);
    let docker = cx.read(|cx| panel.read(cx).docker()).unwrap();
    assert_eq!(
        cx.read(|cx| docker.read(cx).selected_id()).as_deref(),
        Some(WEB_ID)
    );

    // 查看连接（详情头部的 ⋯ 中）：网络页按进程筛选，筛选条件可以清除
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(1usize, cx);
        window.render_frame(cx);
        window.click(("tools-process-row", 600usize), cx);
    })
    .unwrap();
    click_more(
        cx,
        handle,
        "tools-process-detail-more",
        t!("tools.process.show_connections"),
    );
    assert_eq!(page_of(cx, &panel), Page::Network);
    let network = cx.read(|cx| panel.read(cx).network()).unwrap();
    assert_eq!(
        cx.read(|cx| network.read(cx).chip()),
        Some(ConnChip::Pid {
            pid: 600,
            name: Some("cron".into())
        })
    );
    assert_eq!(cx.read(|cx| network.read(cx).query(cx)), "", "不改搜索框");
    let conns = cx.read(|cx| cx_cached_conns(&panel, cx));
    let keys = conn_keys(&conns.conns);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-conn-chip").is_some());
        assert!(window.try_find(format!("tools-conn-{}", keys[5])).is_some());
        assert!(window.try_find(format!("tools-conn-{}", keys[0])).is_none());
        window.click("tools-conn-chip-clear", cx);
        window.render_frame(cx);
        assert!(window.try_find(format!("tools-conn-{}", keys[0])).is_some());
    })
    .unwrap();
    assert_eq!(cx.read(|cx| network.read(cx).chip()), None);
    assert!(!cx.read(|cx| processes.read(cx).key_menu_open()));
}

/// 测试中网络页缓存的连接。
fn cx_cached_conns(panel: &Entity<HostToolsPanel>, cx: &App) -> Rc<ConnList> {
    let panel = panel.read(cx);
    let id = panel.target_id().unwrap();
    match panel.cache().borrow().get(&(id, Page::Network)) {
        Some(Cached::Network(list)) => list,
        _ => panic!("没有网络页的缓存"),
    }
}

/// 容器详情的“查看进程”只列出这个容器内的进程（筛选条件可以清除），发布的端口“查看连接”按端口筛选；
/// 服务详情中主进程的连接；跳过去的目标不在列表中时说明（可以关掉）。
#[gpui_kit::test]
fn container_and_service_details_jump_and_missing_targets_are_explained(cx: &mut TestAppContext) {
    use termlet_core::hosttools::{ContainerDetail, PortBinding};
    let (handle, panel, _, _ids) = open_with_everything(cx);
    let docker = cx.read(|cx| panel.read(cx).docker()).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(3usize, cx);
        window.render_frame(cx);
        window.click(
            gpui_kit::SharedString::from(format!("tools-container-{WEB_ID}")),
            cx,
        );
    })
    .unwrap();
    let detail = ContainerDetail {
        id: WEB_ID.into(),
        status: "running".into(),
        ports: vec![PortBinding {
            container_port: 80,
            proto: "tcp".into(),
            host_port: Some(8081),
            host_ip: None,
        }],
        ..Default::default()
    };
    let logs = LogResult {
        lines: Vec::new(),
        truncated: false,
        sudo: false,
        limited: false,
        cursor: None,
    };
    cx.update(|cx| docker.update(cx, |page, cx| page.set_test_detail(detail, logs, cx)));
    click_more(
        cx,
        handle,
        "tools-container-detail-more",
        t!("tools.docker.show_processes"),
    );
    assert_eq!(page_of(cx, &panel), Page::Processes);
    let processes = cx.read(|cx| panel.read(cx).processes()).unwrap();
    assert_eq!(
        cx.read(|cx| processes.read(cx).filter().container)
            .as_deref(),
        Some(WEB_ID)
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-process-chip-container").is_some());
        assert!(window.try_find(("tools-process-row", 500usize)).is_some());
        assert!(
            window.try_find(("tools-process-row", 42usize)).is_none(),
            "只列出这个容器内的进程"
        );
        window.click("tools-process-chip-container-clear", cx);
        window.render_frame(cx);
        assert!(window.try_find(("tools-process-row", 42usize)).is_some());
        // 回到 Docker 页：端口的“查看连接”
        window.within("host-tools-pages").click(3usize, cx);
        window.render_frame(cx);
        window.click("tools-container-conns-80-tcp-8081", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(page_of(cx, &panel), Page::Network);
    let network = cx.read(|cx| panel.read(cx).network()).unwrap();
    assert_eq!(
        cx.read(|cx| network.read(cx).chip()),
        Some(ConnChip::Port(8081))
    );

    // 服务详情（头部的 ⋯）：主进程的连接
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(2usize, cx);
        window.render_frame(cx);
        window.click("tools-service-cron.service", cx);
    })
    .unwrap();
    click_more(
        cx,
        handle,
        "tools-service-detail-more",
        t!("tools.service.show_connections"),
    );
    assert_eq!(page_of(cx, &panel), Page::Network);
    assert_eq!(
        cx.read(|cx| network.read(cx).chip()),
        Some(ConnChip::Pid {
            pid: 600,
            name: Some("cron".into())
        }),
        "主进程的名称取自进程页读到的列表"
    );

    // 目标不在列表中：说明，可以关掉
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.show_service("gone.service".into(), window, cx)
        });
        window.render_frame(cx);
        assert!(window.try_find("tools-jump-note").is_some());
    })
    .unwrap();
    let services = cx.read(|cx| panel.read(cx).services()).unwrap();
    assert!(cx.read(|cx| services.read(cx).note()).is_some());
    cx.update_window(handle, |_, window, cx| {
        window.click("tools-jump-note-close", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-jump-note").is_none());
        panel.update(cx, |panel, cx| {
            panel.show_container("f".repeat(64), window, cx)
        });
    })
    .unwrap();
    assert!(cx.read(|cx| docker.read(cx).note()).is_some());
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |panel, cx| panel.show_process(4242, window, cx));
    })
    .unwrap();
    assert!(cx.read(|cx| processes.read(cx).note()).is_some());
}

/// 监听视图：同一端口的 IPv4、IPv6 合成一行，端口单独成列；容器发布的端口标出容器（Docker 页读到的名称，没有读数时
/// 只写“容器”），点了跳到 Docker 页并选中发布它的容器（找不到时说明）。
#[gpui_kit::test]
fn listening_ports_are_merged_and_point_at_containers(cx: &mut TestAppContext) {
    let (handle, panel, _, _ids) = open_with_everything(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(4usize, cx);
        window.render_frame(cx);
        window.within("tools-conn-filter").click(1usize, cx);
        window.render_frame(cx);
        assert_eq!(
            window.find_all("tools-listen-tcp-22").len(),
            1,
            "两种 IP 合成一行"
        );
        assert!(window.try_find("tools-listen-port-tcp-22").is_some());
        assert!(window.try_find("tools-listen-scope-tcp-22").is_some());
        assert!(window.try_find("tools-conn-process-tcp-22").is_some());
        assert!(
            window.try_find("tools-conn-container-tcp-8081").is_some(),
            "容器发布的端口"
        );
        assert!(
            window.try_find("tools-conn-container-tcp-9090").is_some(),
            "docker-proxy 打开的端口也标出是容器的"
        );
        assert!(
            window.try_find("tools-listen-tcp-6000").is_none(),
            "已连接的不在监听视图中"
        );
        // 点一行：当前行移到它，焦点进列表
        window.click("tools-listen-scope-tcp-22", cx);
    })
    .unwrap();
    let network = cx.read(|cx| panel.read(cx).network()).unwrap();
    assert_eq!(
        cx.read(|cx| network.read(cx).cursor()).as_deref(),
        Some("tcp-22")
    );
    // 键盘：↓ 到 8081，Enter 跳到发布它的容器
    for key in ["down", "enter"] {
        cx.update_window(handle, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    }
    assert_eq!(page_of(cx, &panel), Page::Docker);
    let docker = cx.read(|cx| panel.read(cx).docker()).unwrap();
    assert_eq!(
        cx.read(|cx| docker.read(cx).selected_id()).as_deref(),
        Some(WEB_ID)
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(4usize, cx);
        window.render_frame(cx);
        window.click("tools-conn-container-tcp-9090", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(page_of(cx, &panel), Page::Docker);
    assert!(
        cx.read(|cx| docker.read(cx).note()).is_some(),
        "没有容器发布 9090：说明"
    );
}

/// 键盘：列表中 ↑ ↓ Home End 移动并选中，Shift+F10 打开右键菜单（Esc 关掉、焦点回到列表），Delete 先确认，
/// / 到搜索框、搜索框中 Esc 回到列表（保留搜索），Ctrl+PageDown / PageUp 切换页、焦点进那一页的列表，L 查看日志，
/// Esc 把焦点还给终端；概览页的进程 Enter 跳到进程页。
#[gpui_kit::test]
fn keyboard_moves_opens_menus_and_switches_pages(cx: &mut TestAppContext) {
    let (handle, panel, events, _ids) = open_with_everything(cx);
    let processes = cx.read(|cx| panel.read(cx).processes()).unwrap();
    let press = |cx: &mut TestAppContext, key: &str| {
        cx.update_window(handle, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    };
    let selected = |cx: &mut TestAppContext| cx.read(|cx| processes.read(cx).selected());
    let focused = |cx: &mut TestAppContext, focus: gpui_kit::FocusHandle| {
        cx.update_window(handle, |_, window, _| focus.is_focused(window))
            .unwrap()
    };
    // 点一行：焦点进列表，当前行有焦点框
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(("tools-process-row", 42usize), cx);
    })
    .unwrap();
    let list = cx.read(|cx| processes.read(cx).list_focus());
    assert!(focused(cx, list.clone()));
    // 按 CPU 从高到低（相同时按 PID）：600、900、500、401、1、400、42
    press(cx, "home");
    assert_eq!(selected(cx), Some(600));
    press(cx, "down");
    assert_eq!(selected(cx), Some(900));
    press(cx, "end");
    assert_eq!(selected(cx), Some(42));
    press(cx, "up");
    assert_eq!(selected(cx), Some(400));
    press(cx, "pagedown");
    assert_eq!(selected(cx), Some(42), "PageDown 到头停在最后一行");

    // Shift+F10：在当前行下面打开右键菜单，Esc 关掉后焦点回到列表
    press(cx, "shift-f10");
    assert!(cx.read(|cx| processes.read(cx).key_menu_open()));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("popup-menu").is_some());
    })
    .unwrap();
    press(cx, "escape");
    assert!(!cx.read(|cx| processes.read(cx).key_menu_open()));
    assert!(focused(cx, list.clone()), "菜单关掉后焦点回到列表");

    // Delete：先确认
    press(cx, "delete");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.has_active_dialog(cx));
        assert!(window.try_find("tools-confirm-command").is_some());
    })
    .unwrap();
    press(cx, "escape");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(!window.has_active_dialog(cx));
    })
    .unwrap();

    // /：到搜索框；搜索框中 Esc 回到列表，搜索的内容留着
    cx.update_window(handle, |_, window, cx| list.focus(window, cx))
        .unwrap();
    press(cx, "/");
    assert!(!focused(cx, list.clone()), "焦点到了搜索框");
    cx.update_window(handle, |_, window, cx| window.input("ngi", cx))
        .unwrap();
    press(cx, "escape");
    assert!(focused(cx, list.clone()), "Esc 回到列表");
    assert_eq!(cx.read(|cx| processes.read(cx).query(cx)), "ngi");
    // Ctrl+F 也到搜索框，Enter 回到列表
    press(cx, "ctrl-f");
    assert!(!focused(cx, list.clone()));
    press(cx, "enter");
    assert!(focused(cx, list.clone()));

    // 切换页：焦点进那一页的列表
    press(cx, "ctrl-pagedown");
    assert_eq!(page_of(cx, &panel), Page::Services);
    let services = cx.read(|cx| panel.read(cx).services()).unwrap();
    let service_list = cx.read(|cx| services.read(cx).list_focus());
    assert!(focused(cx, service_list));
    press(cx, "down");
    assert_eq!(
        cx.read(|cx| services.read(cx).selected_unit()).as_deref(),
        Some("cron.service")
    );
    press(cx, "l");
    assert_eq!(events.borrow().last().copied(), Some("logs"));
    press(cx, "ctrl-pageup");
    assert_eq!(page_of(cx, &panel), Page::Processes);
    assert!(focused(cx, list.clone()));
    press(cx, "ctrl-pageup");
    press(cx, "ctrl-pageup");
    assert_eq!(
        page_of(cx, &panel),
        Page::Network,
        "循环：概览的上一页是网络"
    );
    press(cx, "ctrl-pagedown");
    assert_eq!(page_of(cx, &panel), Page::Overview);

    // Esc：焦点还给终端
    press(cx, "escape");
    assert_eq!(events.borrow().last().copied(), Some("terminal"));
}

/// 概览页的进程：↑ ↓ 移动，Enter 跳到进程页并选中；“打开主机工具的概览”把焦点放进概览页。
#[gpui_kit::test]
fn overview_processes_open_with_enter(cx: &mut TestAppContext) {
    use super::model::OverviewData;
    let (handle, panel, _, ids) = open_with_everything(cx);
    let id = ids[0].entity_id();
    let list = owned_processes();
    let mut top = vec![list.processes[4].clone(), list.processes[1].clone()];
    top[0].cpu = Some(50.);
    top[1].cpu = Some(10.);
    cache(
        &panel,
        cx,
        id,
        Page::Overview,
        Cached::Overview(Rc::new(OverviewData {
            details: None,
            top,
            cpu_pending: false,
        })),
    );
    // 换个终端再换回来：各页按缓存重建
    let other = terminal_ids(cx, 1);
    set_target(
        cx,
        handle,
        &panel,
        Some(target(other[0].entity_id(), ToolsState::Reconnecting)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.show_page_focused(Page::Overview, window, cx)
        });
        window.render_frame(cx);
    })
    .unwrap();
    let overview = cx.read(|cx| panel.read(cx).overview()).unwrap();
    let focus = cx.read(|cx| overview.read(cx).list_focus());
    assert!(
        cx.update_window(handle, |_, window, _| focus.is_focused(window))
            .unwrap()
    );
    for key in ["down", "down", "up", "enter"] {
        cx.update_window(handle, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    }
    assert_eq!(page_of(cx, &panel), Page::Processes);
    let processes = cx.read(|cx| panel.read(cx).processes()).unwrap();
    assert_eq!(cx.read(|cx| processes.read(cx).selected()), Some(900));
}

// ------------------------------------------------------------ UI 评审的改进（第一批）

/// 选中的行被子页、筛选、搜索藏起来时取消选中、不显示详情：Docker 的“异常”子页、进程与服务的搜索、服务的筛选；
/// 网络页的当前行被搜索藏起来时去掉。
#[gpui_kit::test]
fn hidden_selections_are_dropped(cx: &mut TestAppContext) {
    let (handle, panel, _, _ids) = open_with_everything(cx);
    // 进程：选中 nginx（42），搜索 sshd 后它不在列表中
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(("tools-process-row", 42usize), cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-process-detail").is_some());
        window.click("tools-process-search", cx);
        window.input("sshd", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let processes = cx.read(|cx| panel.read(cx).processes()).unwrap();
    assert_eq!(cx.read(|cx| processes.read(cx).selected()), None);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-process-detail").is_none());
    })
    .unwrap();

    // Docker：选中运行中的 web，切到“异常”子页（web 不在其中）
    cx.update_window(handle, |_, window, cx| {
        window.within("host-tools-pages").click(3usize, cx);
        window.render_frame(cx);
        window.click(
            gpui_kit::SharedString::from(format!("tools-container-{WEB_ID}")),
            cx,
        );
        window.render_frame(cx);
        assert!(window.try_find("tools-container-detail").is_some());
        window.within("tools-docker-tabs").click(1usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    let docker = cx.read(|cx| panel.read(cx).docker()).unwrap();
    assert_eq!(cx.read(|cx| docker.read(cx).selected_id()), None);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-container-detail").is_none());
    })
    .unwrap();

    // 服务：选中运行中的 cron，切到“失败”
    cx.update_window(handle, |_, window, cx| {
        window.within("host-tools-pages").click(2usize, cx);
        window.render_frame(cx);
        window.click("tools-service-cron.service", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-service-detail").is_some());
        window.within("tools-service-filter").click(3usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    let services = cx.read(|cx| panel.read(cx).services()).unwrap();
    assert_eq!(cx.read(|cx| services.read(cx).selected_unit()), None);

    // 网络：当前行（22）被搜索藏起来
    cx.update_window(handle, |_, window, cx| {
        window.within("host-tools-pages").click(4usize, cx);
        window.render_frame(cx);
        window.within("tools-conn-filter").click(1usize, cx);
        window.render_frame(cx);
        window.click("tools-listen-scope-tcp-22", cx);
        window.click("tools-conn-search", cx);
        window.input("8081", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let network = cx.read(|cx| panel.read(cx).network()).unwrap();
    assert_eq!(cx.read(|cx| network.read(cx).cursor()), None);
}

/// 信息行尾的权限标记：有免密码 sudo 时“sudo”，没有时锁图标（点开是说明，不执行任何操作），是 root 时没有；网络页
/// 经 sudo 读到全部进程名时才标“sudo”。
#[gpui_kit::test]
fn privilege_is_marked_at_the_end_of_the_info_line(cx: &mut TestAppContext) {
    use termlet_core::hosttools::{Privilege, SudoState};
    let (handle, panel, _, ids) = open_with_everything(cx);
    let id = ids[0].entity_id();
    let processes = cx.read(|cx| panel.read(cx).processes()).unwrap();
    let set = |cx: &mut TestAppContext, privilege: Option<Privilege>| {
        cx.update(|cx| {
            processes.update(cx, |page, cx| {
                page.feed().privilege = privilege;
                cx.notify();
            })
        });
    };
    let has = |cx: &mut TestAppContext, element: &'static str| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(element).is_some()
        })
        .unwrap()
    };
    set(cx, Some(Privilege::Sudo));
    assert!(has(cx, "tools-sudo-processes"));
    assert!(!has(cx, "tools-locked-processes"));
    set(cx, Some(Privilege::Root));
    assert!(!has(cx, "tools-sudo-processes") && !has(cx, "tools-locked-processes"));
    set(cx, Some(Privilege::Limited(SudoState::Password)));
    assert!(has(cx, "tools-locked-processes"));
    // 点开锁图标：只是说明（写明主机），没有执行的按钮
    cx.update_window(handle, |_, window, cx| {
        window.click("tools-locked-processes", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.has_active_dialog(cx));
        assert!(window.try_find("tools-root-info").is_some());
        assert!(window.try_find("tools-confirm-host").is_some());
        assert!(window.try_find("tools-root-run").is_none());
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();

    // 服务：普通用户，没有免密码 sudo
    let services = ServiceList {
        uid: Some(1000),
        ..cron_services()
    };
    cache(
        &panel,
        cx,
        id,
        Page::Services,
        Cached::Services(Rc::new(services)),
    );
    // 网络：经 sudo 读到的
    let mut conns = conn_list(vec![listen("0.0.0.0", 22, 900, "sshd")]);
    conns.uid = Some(1000);
    conns.sudo = true;
    cache(
        &panel,
        cx,
        id,
        Page::Network,
        Cached::Network(Rc::new(conns)),
    );
    // 换个终端再换回来：按新的缓存重建各页
    let other = terminal_ids(cx, 1);
    set_target(
        cx,
        handle,
        &panel,
        Some(target(other[0].entity_id(), ToolsState::Reconnecting)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    let services = cx.read(|cx| panel.read(cx).services()).unwrap();
    let network = cx.read(|cx| panel.read(cx).network()).unwrap();
    cx.update(|cx| {
        services.update(cx, |page, cx| {
            page.feed().privilege = Some(Privilege::Limited(SudoState::NotInstalled));
            cx.notify();
        });
        network.update(cx, |page, cx| {
            page.feed().privilege = Some(Privilege::Sudo);
            cx.notify();
        });
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(2usize, cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-locked-services").is_some());
        window.within("host-tools-pages").click(4usize, cx);
        window.render_frame(cx);
        assert!(
            window.try_find("tools-sudo-network").is_some(),
            "经 sudo 读到全部进程名"
        );
    })
    .unwrap();
    cx.update(|cx| {
        network.update(cx, |page, cx| {
            page.feed().privilege = Some(Privilege::Limited(SudoState::Password));
            cx.notify();
        })
    });
    assert!(has(cx, "tools-locked-network"));
}

/// 网络页没有汇总卡片：数量写在筛选页签上（监听按合并 IPv4 / IPv6 后的行数，与列表一致）；有 TIME-WAIT 时信息行写出
/// 数量，没有时不写。
#[gpui_kit::test]
fn network_counts_live_in_the_filter_tabs(cx: &mut TestAppContext) {
    use termlet_core::hosttools::ConnCount;
    let (handle, panel, _, ids) = open_with_everything(cx);
    let id = ids[0].entity_id();
    let has_time_wait = |cx: &mut TestAppContext| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.within("host-tools-pages").click(4usize, cx);
            window.render_frame(cx);
            window.try_find("tools-conn-time-wait").is_some()
        })
        .unwrap()
    };
    assert!(!has_time_wait(cx), "没有 TIME-WAIT 时不写");
    let network = cx.read(|cx| panel.read(cx).network()).unwrap();
    // 22、8081 各有 IPv4 与 IPv6 两个，9090 一个：监听 3 行
    assert_eq!(
        cx.update(|cx| network.update(cx, |page, _| page.counts()))[1],
        3
    );

    let count = |proto: &str, state: &str, count: usize| ConnCount {
        proto: proto.into(),
        state: state.into(),
        count,
    };
    let mut conns = (*cx.read(|cx| cx_cached_conns(&panel, cx))).clone();
    conns.counts = vec![
        count("tcp", "ESTAB", 1),
        count("tcp", "LISTEN", 5),
        count("tcp", "TIME-WAIT", 2),
        count("udp", "UNCONN", 1),
    ];
    conns.total = 9;
    cache(
        &panel,
        cx,
        id,
        Page::Network,
        Cached::Network(Rc::new(conns)),
    );
    let other = terminal_ids(cx, 1);
    set_target(
        cx,
        handle,
        &panel,
        Some(target(other[0].entity_id(), ToolsState::Reconnecting)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    assert!(has_time_wait(cx));
    let network = cx.read(|cx| panel.read(cx).network()).unwrap();
    assert_eq!(
        cx.update(|cx| network.update(cx, |page, _| page.counts())),
        [9, 3, 1, 8, 1],
        "全部、已连接、TCP、UDP 取各状态的总数，监听数合并后的行"
    );
    // 按进程筛选时数列出的连接
    cx.update_window(handle, |_, window, cx| {
        network.update(cx, |page, cx| {
            page.set_chip(
                ConnChip::Pid {
                    pid: 900,
                    name: None,
                },
                window,
                cx,
            )
        })
    })
    .unwrap();
    assert_eq!(
        cx.update(|cx| network.update(cx, |page, _| page.counts())),
        [2, 1, 0, 2, 0]
    );
}

/// 进程页：本终端的 shell 是行首的小图标（不再是会被截断的徽标）；列表窄时归属标签只留图标，不挤掉进程名。
#[gpui_kit::test]
fn process_rows_keep_names_and_mark_the_terminal_with_an_icon(cx: &mut TestAppContext) {
    let (handle, _panel, _, _ids) = open_with_everything(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let icon = window.find(("tools-process-self", 400usize)).bounds();
        let row = window.find(("tools-process-row", 400usize)).bounds();
        assert!(
            icon.left() - row.left() < px(24.),
            "在行首：{icon:?} / {row:?}"
        );
        // 默认宽度（400）下标签只留图标
        let tag = window.find(("tools-process-container", 500usize)).bounds();
        assert!(tag.size.width < px(24.), "只留图标：{tag:?}");
    })
    .unwrap();
}

/// 默认宽度下进程列表（卡片里）照样显示用户一列，名称一列不被明显挤窄：放进卡片之前约 5.9rem，现在约 5.8rem。
#[gpui_kit::test]
fn default_width_keeps_the_user_column(cx: &mut TestAppContext) {
    default_width_keeps_the_user_column_at(cx, termlet_core::ui_scale::DEFAULT);
}

// 界面放大到 125% 时同样（面板的默认宽度随界面一起放大）
#[gpui_kit::test]
fn default_width_keeps_the_user_column_at_125_percent(cx: &mut TestAppContext) {
    default_width_keeps_the_user_column_at(cx, 125);
}

fn default_width_keeps_the_user_column_at(cx: &mut TestAppContext, percent: u16) {
    let (handle, _panel, _, _ids) = open_with_everything(cx);
    // 窗口够宽：面板是默认宽度，不因为给终端留位置而变窄
    cx.simulate_window_resize(handle, size(px(1600.), px(800.)));
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.window.ui_scale = percent)
            .unwrap()
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        // 列表的宽度上一帧量出
        window.render_frame(cx);
        window.render_frame(cx);
        let rem = window.rem_size();
        let name = window.find("tools-sort-name").bounds();
        assert!(window.try_find("tools-sort-user").is_some(), "用户一列");
        assert!(name.size.width >= rem * 5.75, "名称一列 {name:?}");
    })
    .unwrap();
}

/// Docker：健康状态是状态前面的小标签；鼠标停在列表上时暂停自动刷新。
#[gpui_kit::test]
fn docker_rows_mark_health_and_pause_on_hover(cx: &mut TestAppContext) {
    use termlet_core::hosttools::{HealthState, RunState};
    let (handle, panel, _, ids) = open_with_everything(cx);
    let id = ids[0].entity_id();
    let mut docker = web_docker();
    docker.containers[0].run = Some(RunState {
        status: "running".into(),
        started_at: Some(termlet_core::time::now_ms() - 15 * 60_000),
        health: Some(HealthState::Unhealthy),
        ..Default::default()
    });
    cache(
        &panel,
        cx,
        id,
        Page::Docker,
        Cached::Docker(Rc::new(docker)),
    );
    let other = terminal_ids(cx, 1);
    set_target(
        cx,
        handle,
        &panel,
        Some(target(other[0].entity_id(), ToolsState::Reconnecting)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    let docker = cx.read(|cx| panel.read(cx).docker()).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(3usize, cx);
        window.render_frame(cx);
        let badge = window
            .find(gpui_kit::SharedString::from(format!(
                "tools-container-health-{WEB_ID}"
            )))
            .bounds();
        let status = window
            .find(gpui_kit::SharedString::from(format!(
                "tools-container-status-{WEB_ID}"
            )))
            .bounds();
        assert!(badge.right() <= status.left(), "标签在状态前面");
        window.hover(
            gpui_kit::SharedString::from(format!("tools-container-{WEB_ID}")),
            cx,
        );
    })
    .unwrap();
    assert!(cx.read(|cx| docker.read(cx).is_paused()), "鼠标停在列表上");
    cx.update_window(handle, |_, window, cx| window.hover("host-tools-title", cx))
        .unwrap();
    assert!(!cx.read(|cx| docker.read(cx).is_paused()));
}

/// 详情区：头部一行（名称 ● 状态 · 主要操作 · ⋯），与列表之间的分隔条可拖动（各页共用，记在界面状态中），双击恢复
/// 按内容的高度。
#[gpui_kit::test]
fn detail_splitter_resizes_and_resets(cx: &mut TestAppContext) {
    let (handle, _panel, _, _ids) = open_with_everything(cx);
    let height = |cx: &mut TestAppContext| {
        cx.read(|cx| {
            AppState::global(cx)
                .ui
                .read(cx)
                .state()
                .host_tools_detail_height
        })
    };
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(("tools-process-row", 42usize), cx);
        window.render_frame(cx);
        for element in [
            "tools-process-detail-term",
            "tools-process-detail-kill",
            "tools-process-detail-more",
        ] {
            assert!(window.try_find(element).is_some(), "{element}");
        }
        let splitter = window
            .find("tools-process-detail-splitter")
            .bounds()
            .center();
        window.drag(splitter, point(splitter.x, splitter.y - px(120.)), cx);
    })
    .unwrap();
    let dragged = height(cx).expect("拖动后记下高度");
    assert!(dragged > 150., "{dragged}");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let detail = window.find("tools-process-detail").bounds();
        assert!(detail.size.height > px(100.), "{detail:?}");
        window.double_click("tools-process-detail-splitter", cx);
    })
    .unwrap();
    assert_eq!(height(cx), None, "双击恢复按内容的高度");
}

/// 面板不可用时用中间区域那一套空状态：没有终端时“快速连接…”与最近打开的主机，已断开时“重新连接”；页签置灰、
/// 点了不切换。
#[gpui_kit::test]
fn unavailable_panel_uses_the_empty_states(cx: &mut TestAppContext) {
    use termlet_core::model::Host;
    use termlet_core::store::Position;
    let (handle, panel, events) = open(cx);
    let host = cx.update(|cx| {
        let host = AppState::store(cx)
            .create(
                Host {
                    name: "web".into(),
                    address: "10.0.0.5".into(),
                    username: "termlet".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.note_recent_host(host.as_str()), cx)
        });
        host
    });
    let recent = format!("host-tools-recent-{}", host.as_str());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("host-tools-empty-no_terminal").is_some());
        assert!(window.try_find(recent.clone()).is_some(), "最近打开的主机");
        window.click("host-tools-quick-connect", cx);
        window.click(recent.clone(), cx);
        // 页签置灰：点了不切换
        window.within("host-tools-pages").click(4usize, cx);
    })
    .unwrap();
    assert_eq!(*events.borrow(), ["quick", "host"]);
    assert_eq!(cx.read(|cx| panel.read(cx).page()), Page::Processes);

    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    for (state, element) in [
        (ToolsState::Connecting, "host-tools-empty-connecting"),
        (ToolsState::Unsupported, "host-tools-empty-unsupported"),
        (ToolsState::SingleChannel, "host-tools-empty-single_channel"),
        (ToolsState::Ended, "host-tools-empty-ended"),
    ] {
        set_target(cx, handle, &panel, Some(target(id, state)));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(element).is_some(), "{element}");
        })
        .unwrap();
    }
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("host-tools-reconnect").is_some());
        assert!(window.try_find("host-tools-quick-connect").is_none());
    })
    .unwrap();
}

/// 列表中完整显示着的最靠下的一行。
fn lowest_row(window: &Window, list: &'static str, rows: &[ElementId]) -> ElementId {
    let bottom = window.find(list).bounds().bottom();
    rows.iter()
        .filter_map(|row| Some((row, window.try_find(row.clone())?.bounds().bottom())))
        .filter(|(_, b)| *b <= bottom)
        .max_by(|a, b| f32::from(a.1).total_cmp(&f32::from(b.1)))
        .map(|(row, _)| row.clone())
        .expect("有显示着的行")
}

/// 这一行显示着、完整地在列表中（没有被挤到详情下面）。
fn assert_row_inside(window: &Window, list: &'static str, row: &ElementId) {
    let list = window.find(list).bounds();
    let row = window
        .try_find(row.clone())
        .unwrap_or_else(|| panic!("{row:?} 不在可见范围"))
        .bounds();
    assert!(
        row.top() >= list.top() && row.bottom() <= list.bottom() + px(0.5),
        "{row:?} 不在 {list:?} 中"
    );
}

/// 详情出现（记下的高度很大，按列表的最小高度收）把列表挤矮后，点中的行滚在可见范围内；右键不改变选中。
#[gpui_kit::test]
fn selected_rows_stay_above_the_detail(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    cx.update(|cx| {
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.host_tools_detail_height = Some(2000.), cx)
        })
    });
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let mut list = processes();
    list.processes = (1..=40)
        .map(|pid| process(pid, 0, &format!("p{pid}")))
        .collect();
    list.total = list.processes.len();
    let services = ServiceList {
        timers: Vec::new(),
        services: (1..=40)
            .map(|n| Service {
                unit: format!("s{n}.service"),
                load_state: "loaded".into(),
                active_state: "active".into(),
                sub_state: "running".into(),
                ..Default::default()
            })
            .collect(),
        ..cron_services()
    };
    cache(
        &panel,
        cx,
        id,
        Page::Processes,
        Cached::Processes(Rc::new(list)),
    );
    cache(
        &panel,
        cx,
        id,
        Page::Services,
        Cached::Services(Rc::new(services)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );

    let page = cx.read(|cx| panel.read(cx).processes()).expect("进程页");
    let processes: Vec<ElementId> = (1..=40usize)
        .map(|pid| ("tools-process-row", pid).into())
        .collect();
    let row = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let row = lowest_row(window, "tools-process-list", &processes);
            // 右键不改变选中：详情不出现、列表不变矮，这一行留在按下的位置上画出菜单
            window.right_click(row.clone(), cx);
            window.render_frame(cx);
            assert!(window.try_find("tools-process-detail").is_none());
            assert_row_inside(window, "tools-process-list", &row);
            row
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        cx.update_window(handle, |_, window, cx| menu_open(window, cx))
            .unwrap()
    );
    assert_eq!(cx.read(|cx| page.read(cx).selected()), None);
    cx.update_window(handle, |_, window, cx| {
        window.press("escape", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        !cx.update_window(handle, |_, window, cx| menu_open(window, cx))
            .unwrap()
    );
    cx.update_window(handle, |_, window, cx| {
        window.click(row.clone(), cx);
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-process-detail").is_some());
        assert_row_inside(window, "tools-process-list", &row);
    })
    .unwrap();

    cx.update(|cx| panel.update(cx, |panel, cx| panel.show_page(Page::Services, cx)));
    let services: Vec<ElementId> = (1..=40)
        .map(|n| SharedString::from(format!("tools-service-s{n}.service")).into())
        .collect();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let row = lowest_row(window, "tools-service-list", &services);
        window.click(row.clone(), cx);
        window.render_frame(cx);
        window.render_frame(cx);
        assert_row_inside(window, "tools-service-list", &row);
    })
    .unwrap();
}

/// 鼠标停在列表上（暂停刷新）时补读到 CPU：就地填进显示着的行，行的顺序不变（默认按 CPU 排序也不重排）、不增删行。
#[gpui_kit::test]
fn paused_cpu_retry_fills_rows_in_place(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let without_cpu = |pids: &[u32]| {
        let mut list = processes();
        list.processes = pids
            .iter()
            .map(|&pid| Process {
                cpu: None,
                ..process(pid, 0, &format!("p{pid}"))
            })
            .collect();
        list.total = list.processes.len();
        list
    };
    cache(
        &panel,
        cx,
        id,
        Page::Processes,
        Cached::Processes(Rc::new(without_cpu(&[1, 2, 3, 4]))),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    let page = cx.read(|cx| panel.read(cx).processes()).expect("进程页");
    let order = |window: &Window| {
        let mut rows: Vec<(Pixels, u32)> = (1..=5u32)
            .filter_map(|pid| {
                let row = window.try_find(("tools-process-row", pid as usize))?;
                Some((row.bounds().top(), pid))
            })
            .collect();
        rows.sort_by(|a, b| f32::from(a.0).total_cmp(&f32::from(b.0)));
        rows.into_iter().map(|(_, pid)| pid).collect::<Vec<_>>()
    };
    let before = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.hover(("tools-process-row", 2usize), cx);
            window.render_frame(cx);
            order(window)
        })
        .unwrap();
    assert_eq!(before.len(), 4);
    let mut fresh = without_cpu(&[1, 2, 3, 4, 5]);
    for p in &mut fresh.processes {
        p.cpu = Some(f64::from(p.pid) * 10.);
    }
    cx.update(|cx| page.update(cx, |page, cx| page.arrive_for_test(fresh, cx)));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(order(window), before, "行的顺序不变、不增删行");
    })
    .unwrap();
    let cpus = cx.read(|cx| page.read(cx).cpus());
    assert!(
        cpus.iter()
            .all(|(pid, cpu)| *cpu == Some(f64::from(*pid) * 10.)),
        "{cpus:?}"
    );
}

/// 把面板放宽到 `width`（窗口跟着放宽，给左侧留出位置），各页按宽面板显示更多的列。
fn widen(cx: &mut TestAppContext, handle: AnyWindowHandle, width: f32) {
    cx.simulate_window_resize(handle, size(px(width + 560.), px(800.)));
    cx.update(|cx| {
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.host_tools_width = Some(width), cx)
        })
    });
    // 列表的宽度上一帧量出：跨过界限后下一帧才按新的宽度显示
    for _ in 0..2 {
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
    }
}

/// 行右端的操作：鼠标停在行上、键盘焦点在这一行时浮出主要操作与 ⋯，盖在行的右端、不改变行高；在 ⋯ 上按下不选中
/// 这一行（选中会让详情出现、把列表挤矮），⋯ 打开的是与右键菜单相同的菜单，打开期间操作一直显示。进程、服务、容器、
/// 镜像各页一致。
#[gpui_kit::test]
fn rows_show_actions_on_hover_and_keyboard_focus(cx: &mut TestAppContext) {
    let (handle, panel, _, _ids) = open_with_everything(cx);
    let page = cx.read(|cx| panel.read(cx).processes()).unwrap();
    let row = ("tools-process-row", 42usize);
    let actions = ("tools-process-actions", 42usize);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(actions).is_none(), "平时不显示");
        let before = window.find(row).bounds();
        window.hover(row, cx);
        window.render_frame(cx);
        let overlay = window.find(actions).bounds();
        let after = window.find(row).bounds();
        assert_eq!(before.size, after.size, "不改变行高与布局");
        assert!(overlay.right() <= after.right() + px(0.5) && overlay.top() >= after.top());
        assert!(
            window
                .try_find(("tools-process-row-term", 42usize))
                .is_some()
        );
        window.click(("tools-process-row-more", 42usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(menu_open(window, cx), "⋯ 打开右键菜单");
        window.hover("host-tools-title", cx);
        window.render_frame(cx);
        assert!(window.try_find(actions).is_some(), "菜单打开期间照样显示");
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| page.read(cx).selected()),
        None,
        "点 ⋯ 不选中这一行"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(actions).is_none(), "移开之后收起");
        // 键盘：当前行同样浮出
        window.click(row, cx);
        window.render_frame(cx);
        window.press("down", cx);
        window.render_frame(cx);
    })
    .unwrap();
    let selected = cx.read(|cx| page.read(cx).selected()).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.hover("host-tools-title", cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find(("tools-process-actions", selected as usize))
                .is_some()
        );
    })
    .unwrap();

    // 服务：运行中的是重启；容器：运行中的是重启
    cx.update_window(handle, |_, window, cx| {
        window.within("host-tools-pages").click(2usize, cx);
        window.render_frame(cx);
        window.hover("tools-service-cron.service", cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find("tools-service-row-restart-cron.service")
                .is_some()
        );
        assert!(
            window
                .try_find("tools-service-row-more-cron.service")
                .is_some()
        );
        window.within("host-tools-pages").click(3usize, cx);
        window.render_frame(cx);
        let container = format!("tools-container-{WEB_ID}");
        window.hover(container, cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find(format!("tools-container-row-restart-{WEB_ID}"))
                .is_some()
        );
        assert!(
            window
                .try_find(format!("tools-container-row-more-{WEB_ID}"))
                .is_some()
        );
    })
    .unwrap();
}

/// 操作进行中：行上状态的位置转圈并写“正在…”，这一行与详情中的操作按钮不能点；结束后恢复（这里没有连接，操作
/// 失败）。
#[gpui_kit::test]
fn rows_show_operations_in_progress(cx: &mut TestAppContext) {
    let (handle, _panel, _, _ids) = open_with_everything(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(2usize, cx);
        window.render_frame(cx);
        window.click("tools-service-cron.service", cx);
        window.render_frame(cx);
        window.click("tools-service-detail-restart", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("ok", cx);
        // 结果还没回来：行上转圈，操作不能点
        window.render_frame(cx);
        assert!(window.try_find("tools-service-op-cron.service").is_some());
        assert_eq!(
            window.find("tools-service-detail-restart").disabled(),
            Some(true)
        );
    })
    .unwrap();
    cx.run_until_parked();
    // 失败：行上写“重启失败”，原因写在信息行下（这一页显示着，不弹提示、不盖住面板的标题），操作又能点了
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-service-op-cron.service").is_some());
        assert!(
            window.try_find("tools-failure").is_some(),
            "信息行下说明原因"
        );
        assert!(
            window.try_find("tools-failure-toggle").is_none(),
            "原因就是全部时不能展开"
        );
        assert_ne!(
            window.find("tools-service-detail-restart").disabled(),
            Some(true)
        );
        assert!(window.notifications(cx).is_empty(), "不弹提示");
    })
    .unwrap();
    // 一会儿之后行上恢复原样，原因留到关掉为止
    cx.executor().advance_clock(super::shared::FAILED_SHOWN);
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-service-op-cron.service").is_none());
        assert!(window.try_find("tools-failure").is_some());
        window.click("tools-failure-dismiss", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-failure").is_none());
    })
    .unwrap();
}

/// 操作失败时那一页不在显示（换到了别的页）：弹出提示，放在窗口右下角（不盖住面板的标题）；回到那一页时信息行下照样
/// 写着原因，可以展开看完整的输出。
#[gpui_kit::test]
fn failures_elsewhere_notify_at_the_bottom(cx: &mut TestAppContext) {
    let (handle, panel, _, _ids) = open_with_everything(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(2usize, cx);
        window.render_frame(cx);
        window.click("tools-service-cron.service", cx);
        window.render_frame(cx);
        window.click("tools-service-detail-restart", cx);
    })
    .unwrap();
    cx.run_until_parked();
    // 确认后立即换到进程页：没有连接，结果马上回来，这时服务页已不在显示
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        panel.update(cx, |panel, cx| panel.show_page(Page::Processes, cx));
        window.click("ok", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.notifications(cx).len(), 1, "另弹提示");
    })
    .unwrap();
    let services = cx.read(|cx| panel.read(cx).services()).unwrap();
    cx.update(|cx| {
        services.update(cx, |page, cx| {
            page.set_test_failure(
                "重启服务 cron",
                "Job for cron.service failed.",
                "Job for cron.service failed.\nSee \"systemctl status cron.service\" for details.",
                cx,
            )
        })
    });
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |panel, cx| panel.show_page(Page::Services, cx));
        window.render_frame(cx);
        assert!(window.try_find("tools-failure-reason").is_some());
        window.click("tools-failure-toggle", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-failure-detail").is_some(), "展开");
        assert!(window.try_find("tools-failure-reason").is_none());
    })
    .unwrap();
}

/// 面板够宽时：进程页另有命令行一列，服务一行一个，容器是一行一个的表格（带表头）。
#[gpui_kit::test]
fn wide_panels_show_more_columns(cx: &mut TestAppContext) {
    let (handle, panel, _, ids) = open_with_everything(cx);
    let id = ids[0].entity_id();
    let mut list = owned_processes();
    list.processes[1].cmdline = Some("nginx: master process /usr/sbin/nginx".into());
    cache(
        &panel,
        cx,
        id,
        Page::Processes,
        Cached::Processes(Rc::new(list)),
    );
    let cmd = ("tools-process-command", 42usize);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(cmd).is_none(), "窄的时候没有命令行一列");
    })
    .unwrap();
    // 换个终端再换回来：重新读缓存
    let other = terminal_ids(cx, 1);
    set_target(
        cx,
        handle,
        &panel,
        Some(target(other[0].entity_id(), ToolsState::Reconnecting)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    widen(cx, handle, 720.);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(cmd).is_some(), "够宽时列出命令行");
        window.within("host-tools-pages").click(2usize, cx);
    })
    .unwrap();
    widen(cx, handle, 720.);
    cx.update_window(handle, |_, window, cx| {
        let row = window.find("tools-service-cron.service").bounds();
        let name = window.find("tools-service-state-cron.service").bounds();
        let description = window
            .find("tools-service-description-cron.service")
            .bounds();
        assert!(
            (name.center().y - description.center().y).abs() < px(1.),
            "描述与状态在同一行"
        );
        assert!(row.size.height < px(2.5 * 16.) - px(1.), "行高变矮");
        window.within("host-tools-pages").click(3usize, cx);
    })
    .unwrap();
    widen(cx, handle, 720.);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("tools-container-table-header").is_some());
        assert!(
            window
                .try_find(format!("tools-container-image-{WEB_ID}"))
                .is_some()
        );
        assert!(
            window
                .try_find(format!("tools-container-ports-{WEB_ID}"))
                .is_some()
        );
        assert!(
            window
                .try_find(format!("tools-container-detail-{WEB_ID}"))
                .is_none(),
            "表格中没有第二行"
        );
    })
    .unwrap();
}

/// 合并同名进程：同名的合成一行（写出个数），点了展开、收起；键盘移到这一行时 Enter 展开；跳到其中的进程时自动展开。
#[gpui_kit::test]
fn same_name_processes_can_be_merged(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let mut list = processes();
    list.processes.push(process(43, 42, "nginx"));
    list.processes.push(process(44, 42, "nginx"));
    list.total = list.processes.len();
    cache(
        &panel,
        cx,
        id,
        Page::Processes,
        Cached::Processes(Rc::new(list)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    let page = cx.read(|cx| panel.read(cx).processes()).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("tools-process-group-nginx").is_none(),
            "默认不合并"
        );
        window.click("tools-process-merge", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-process-group-nginx").is_some());
        assert!(window.try_find("tools-process-group-count-nginx").is_some());
        assert!(
            window.try_find(("tools-process-row", 43usize)).is_none(),
            "收起着"
        );
        assert!(
            window.try_find(("tools-process-row", 400usize)).is_some(),
            "只有一个的照常一行"
        );
        window.click("tools-process-group-nginx", cx);
        window.render_frame(cx);
        assert!(
            window.try_find(("tools-process-row", 43usize)).is_some(),
            "点了展开"
        );
        // Enter 在合并的一行上收起
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(window.try_find(("tools-process-row", 43usize)).is_none());
    })
    .unwrap();
    // 从别的页跳到其中的进程：展开那一组并选中
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |panel, cx| panel.show_process(44, window, cx));
        window.render_frame(cx);
        assert!(window.try_find(("tools-process-row", 44usize)).is_some());
    })
    .unwrap();
    assert_eq!(cx.read(|cx| page.read(cx).selected()), Some(44));
}

/// 服务页：默认不列出 not-found 的单元与已停止的静态单元，小标签写着藏了几个，点 × 显示全部；有自定义的服务时分成
/// 两组；系统状态 degraded 是可点的警告标签，点了看失败的服务。
#[gpui_kit::test]
fn services_hide_rare_units_group_custom_ones_and_link_degraded(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let service = |unit: &str, active: &str, boot: &str| Service {
        unit: unit.into(),
        description: format!("{unit} desc"),
        load_state: "loaded".into(),
        active_state: active.into(),
        unit_file_state: boot.into(),
        ..Default::default()
    };
    let services = ServiceList {
        timers: Vec::new(),
        services: vec![
            Service {
                custom: true,
                ..service("app.service", "failed", "enabled")
            },
            service("cron.service", "active", "enabled"),
            service("apt-daily.service", "inactive", "static"),
            Service {
                load_state: "not-found".into(),
                ..service("gone.service", "inactive", "")
            },
        ],
        version: "systemd 257 (257.9-1~deb13u1)".into(),
        system_state: "degraded".into(),
        uid: Some(0),
        zone: None,
    };
    cache(
        &panel,
        cx,
        id,
        Page::Services,
        Cached::Services(Rc::new(services)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(2usize, cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-service-apt-daily.service").is_none());
        assert!(window.try_find("tools-service-gone.service").is_none());
        assert!(window.try_find("tools-service-chip-hidden").is_some());
        assert!(window.try_find("tools-service-group-custom").is_some());
        assert!(window.try_find("tools-service-group-system").is_some());
        assert!(
            window.find("tools-service-group-custom").bounds().top()
                < window.find("tools-service-app.service").bounds().top()
        );
        window.click("tools-service-chip-hidden-clear", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-service-apt-daily.service").is_some());
        assert!(window.try_find("tools-service-gone.service").is_some());
        assert!(window.try_find("tools-service-chip-hidden").is_none());
        // 系统状态 degraded：点了只看失败的
        window.click("tools-service-degraded", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-service-cron.service").is_none());
        assert!(window.try_find("tools-service-app.service").is_some());
        // 只列常用的；从进程页跳到默认不列出的单元时先列出全部
        window.click("tools-service-show-all", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-service-chip-hidden").is_some());
        panel.update(cx, |panel, cx| {
            panel.show_service("apt-daily.service".into(), window, cx)
        });
        window.render_frame(cx);
        assert!(window.try_find("tools-service-apt-daily.service").is_some());
        assert!(window.try_find("tools-service-chip-hidden").is_none());
    })
    .unwrap();
    let page = cx.read(|cx| panel.read(cx).services()).unwrap();
    assert_eq!(
        cx.read(|cx| page.read(cx).selected_unit()),
        Some("apt-daily.service".to_string())
    );
}

/// 各台主机记下各自上次看的页：跟随到记下的主机时打开它，没有记下的用全局的上次页。
#[gpui_kit::test]
fn the_page_is_remembered_per_host(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 3);
    let on_host = |ix: usize, host: &str| ToolsTarget {
        ssh_host: Some(HostId::from_raw(host)),
        ..target(ids[ix].entity_id(), ToolsState::Reconnecting)
    };
    set_target(cx, handle, &panel, Some(on_host(0, "h_a")));
    cx.update_window(handle, |_, window, cx| {
        window.within("host-tools-pages").click(3usize, cx)
    })
    .unwrap();
    set_target(cx, handle, &panel, Some(on_host(1, "h_b")));
    assert_eq!(
        page_of(cx, &panel),
        Page::Docker,
        "没有记下的用全局的上次页"
    );
    cx.update_window(handle, |_, window, cx| {
        window.within("host-tools-pages").click(2usize, cx)
    })
    .unwrap();
    set_target(cx, handle, &panel, Some(on_host(0, "h_a")));
    assert_eq!(page_of(cx, &panel), Page::Docker, "回到 A：A 上次看的页");
    set_target(cx, handle, &panel, Some(on_host(1, "h_b")));
    assert_eq!(page_of(cx, &panel), Page::Services);
    set_target(cx, handle, &panel, Some(on_host(2, "h_c")));
    assert_eq!(page_of(cx, &panel), Page::Services, "全局的上次页");
}

/// 网络页“全部”：已连接的行标出入站、出站；当前行有选中的底色；列表够宽时本地端口单独成列。
#[gpui_kit::test]
fn network_rows_mark_direction_and_keep_ports(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let established = |local_port: u16, peer_port: u16, state: &str| Conn {
        state: state.into(),
        local_addr: "172.17.0.3".into(),
        local_port,
        peer_addr: "172.17.0.1".into(),
        peer_port,
        ..listen("0.0.0.0", 0, 900, "sshd")
    };
    let list = vec![
        listen("0.0.0.0", 22, 900, "sshd"),
        established(22, 51000, "ESTAB"),
        established(40000, 443, "ESTAB"),
        established(40001, 80, "CLOSE-WAIT"),
    ];
    let keys = conn_keys(&list);
    cache(
        &panel,
        cx,
        id,
        Page::Network,
        Cached::Network(Rc::new(conn_list(list))),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    widen(cx, handle, 340.);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(4usize, cx);
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find(format!("tools-conn-in-{}", keys[1]))
                .is_some()
        );
        assert!(
            window
                .try_find(format!("tools-conn-out-{}", keys[2]))
                .is_some()
        );
        assert!(
            window
                .try_find(format!("tools-conn-in-{}", keys[3]))
                .is_none(),
            "只标已连接的"
        );
        assert!(
            window
                .try_find(format!("tools-conn-port-{}", keys[1]))
                .is_none(),
            "窄的时候端口接在地址后面"
        );
        window.click(format!("tools-conn-{}", keys[2]), cx);
        window.render_frame(cx);
    })
    .unwrap();
    let page = cx.read(|cx| panel.read(cx).network()).unwrap();
    assert_eq!(cx.read(|cx| page.read(cx).cursor()), Some(keys[2].clone()));
    widen(cx, handle, 520.);
    cx.update_window(handle, |_, window, _| {
        assert!(
            window
                .try_find(format!("tools-conn-port-{}", keys[1]))
                .is_some()
        );
    })
    .unwrap();
}

// ------------------------------------------------------------ 进程树、按用户筛选、定时器、Docker 清理

/// 在下拉菜单（`popup-menu`）中点文案为 `label` 的一项。
fn click_menu_item(cx: &mut TestAppContext, handle: AnyWindowHandle, label: &str) {
    let ix = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let menu = window.within("popup-menu");
            (0..40usize).find(|ix| {
                menu.try_find(*ix)
                    .and_then(|item| item.label().map(str::to_string))
                    .as_deref()
                    == Some(label)
            })
        })
        .unwrap()
        .unwrap_or_else(|| panic!("菜单中没有“{label}”"));
    cx.update_window(handle, |_, window, cx| {
        window.within("popup-menu").click(ix, cx)
    })
    .unwrap();
    cx.run_until_parked();
}

/// 树形：工具条上的开关；点 ▾ 收起（不选中）；键盘 → 展开、再 → 到第一个子进程，← 到父进程、再 ← 收起；搜索命中收起的
/// 分支中的进程时自动展开，父进程只是为了看出位置；与合并同名进程只能选一个。
#[gpui_kit::test]
fn processes_show_as_a_tree_and_fold(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    cache(
        &panel,
        cx,
        id,
        Page::Processes,
        Cached::Processes(Rc::new(processes())),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    let page = cx.read(|cx| panel.read(cx).processes()).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(("tools-process-fold", 1usize)).is_none(),
            "默认是列表"
        );
        window.click("tools-process-tree", cx);
        window.render_frame(cx);
        assert!(window.try_find(("tools-process-fold", 1usize)).is_some());
        assert!(
            window.try_find(("tools-process-row", 401usize)).is_some(),
            "默认全部展开"
        );
        window.click(("tools-process-fold", 400usize), cx);
        window.render_frame(cx);
        assert!(
            window.try_find(("tools-process-row", 401usize)).is_none(),
            "点 ▾ 收起"
        );
    })
    .unwrap();
    assert_eq!(cx.read(|cx| page.read(cx).view()), ProcView::Tree);
    assert_eq!(
        cx.read(|cx| page.read(cx).selected()),
        None,
        "点 ▸ / ▾ 不选中"
    );
    cx.update_window(handle, |_, window, cx| {
        window.click(("tools-process-row", 400usize), cx);
        window.press("right", cx);
        window.render_frame(cx);
        assert!(
            window.try_find(("tools-process-row", 401usize)).is_some(),
            "→ 展开"
        );
        window.press("right", cx);
    })
    .unwrap();
    assert_eq!(
        cx.read(|cx| page.read(cx).selected()),
        Some(401),
        "已展开时 → 到第一个子进程"
    );
    cx.update_window(handle, |_, window, cx| window.press("left", cx))
        .unwrap();
    assert_eq!(
        cx.read(|cx| page.read(cx).selected()),
        Some(400),
        "没有子进程时 ← 到父进程"
    );
    cx.update_window(handle, |_, window, cx| {
        window.press("left", cx);
        window.render_frame(cx);
        assert!(
            window.try_find(("tools-process-row", 401usize)).is_none(),
            "← 收起"
        );
        window.click("tools-process-search", cx);
        window.input("vim", cx);
    })
    .unwrap();
    // 搜索框的变化事件在这次更新之后送达
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(("tools-process-row", 401usize)).is_some(),
            "命中的进程所在的分支自动展开"
        );
    })
    .unwrap();
    cx.update(|cx| {
        page.update(cx, |page, cx| {
            assert_eq!(page.tree_context(1, cx), Some(true), "父进程只为看出位置");
            assert_eq!(page.tree_context(401, cx), Some(false));
            assert_eq!(page.tree_context(42, cx), None, "没有命中的不列出");
        })
    });
    cx.update_window(handle, |_, window, cx| {
        window.click("tools-process-merge", cx);
        window.render_frame(cx);
        assert!(
            window.try_find(("tools-process-fold", 1usize)).is_none(),
            "打开合并时树形关掉"
        );
    })
    .unwrap();
    assert_eq!(cx.read(|cx| page.read(cx).view()), ProcView::Merged);
}

/// 按用户筛选：下拉菜单按进程数列出用户，选中后筛选条件是可清除的小标签；右键菜单的“只看这个用户的进程”同样。
#[gpui_kit::test]
fn processes_filter_by_user_with_a_chip(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let mut list = processes();
    for p in &mut list.processes {
        if p.pid == 1 || p.pid == 900 {
            p.user = Some("root".into());
            p.uid = Some(0);
        }
    }
    cache(
        &panel,
        cx,
        id,
        Page::Processes,
        Cached::Processes(Rc::new(list)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-process-chip-user").is_none());
        window.click("tools-process-users", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let root = t!("tools.process.user_item", user = "root", count = 2);
    click_menu_item(cx, handle, &root);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-process-chip-user").is_some());
        assert!(window.try_find(("tools-process-row", 900usize)).is_some());
        assert!(
            window.try_find(("tools-process-row", 42usize)).is_none(),
            "别的用户的进程不列出"
        );
        window.click("tools-process-chip-user-clear", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-process-chip-user").is_none());
        assert!(window.try_find(("tools-process-row", 42usize)).is_some());
        // 右键菜单：只看这个用户的进程
        window.right_click(("tools-process-row", 42usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let only = t!("tools.process.only_user", user = "termlet");
    click_menu_item(cx, handle, &only);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-process-chip-user").is_some());
        assert!(window.try_find(("tools-process-row", 900usize)).is_none());
    })
    .unwrap();
}

/// 服务页的“定时器”子页：列出定时器（触发的服务失败着时标出），详情写出计划与时间，触发的服务可以点过去（静态单元也
/// 列出来），服务详情中触发它的定时器可以点回来；停止定时器先确认，写明主机与命令。
#[gpui_kit::test]
fn timers_tab_lists_timers_and_jumps_to_the_service(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let now = termlet_core::time::now_ms();
    let services = ServiceList {
        services: vec![
            Service {
                unit: "backup.service".into(),
                description: "Back up the data".into(),
                load_state: "loaded".into(),
                active_state: "inactive".into(),
                sub_state: "dead".into(),
                unit_file_state: "static".into(),
                ..Default::default()
            },
            Service {
                unit: "send-report.service".into(),
                description: "Send the report".into(),
                load_state: "loaded".into(),
                active_state: "failed".into(),
                sub_state: "failed".into(),
                unit_file_state: "static".into(),
                result: "exit-code".into(),
                main_exit: Some(MainExit::Code(1)),
                ..Default::default()
            },
        ],
        timers: vec![
            Timer {
                unit: "backup.timer".into(),
                description: "Nightly backup".into(),
                load_state: "loaded".into(),
                active_state: "active".into(),
                sub_state: "waiting".into(),
                unit_file_state: "enabled".into(),
                activates: "backup.service".into(),
                next: Some(now + 3_600_000),
                calendar: vec!["*-*-* 03:00:00".into()],
                persistent: true,
                custom: true,
                fragment_path: Some("/etc/systemd/system/backup.timer".into()),
                ..Default::default()
            },
            Timer {
                unit: "report.timer".into(),
                description: "Weekly report".into(),
                load_state: "loaded".into(),
                active_state: "active".into(),
                sub_state: "waiting".into(),
                unit_file_state: "enabled".into(),
                activates: "send-report.service".into(),
                last: Some(now - 7_200_000),
                monotonic: vec!["OnBootSec=15min".into()],
                ..Default::default()
            },
        ],
        version: "systemd 257".into(),
        system_state: "degraded".into(),
        uid: Some(0),
        zone: None,
    };
    cache(
        &panel,
        cx,
        id,
        Page::Services,
        Cached::Services(Rc::new(services)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(2usize, cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-timer-backup.timer").is_none());
        window.within("tools-service-filter").click(4usize, cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-timer-backup.timer").is_some());
        assert!(
            window
                .try_find("tools-timer-schedule-backup.timer")
                .is_some()
        );
        assert!(
            window.try_find("tools-timer-failed-report.timer").is_some(),
            "触发的服务失败着"
        );
        assert!(window.try_find("tools-timer-failed-backup.timer").is_none());
        assert!(
            window.try_find("tools-service-backup.service").is_none(),
            "定时器子页不列服务"
        );
        window.click("tools-timer-backup.timer", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-timer-detail").is_some());
        assert!(window.try_find("tools-timer-plan").is_some());
        assert!(window.try_find("tools-timer-next").is_some());
        assert!(
            window.try_find("tools-timer-detail-start").is_none(),
            "在计时的不能再启动"
        );
        window.click("tools-timer-service-link", cx);
        window.render_frame(cx);
    })
    .unwrap();
    let services = cx.read(|cx| panel.read(cx).services()).unwrap();
    assert_eq!(
        cx.read(|cx| services.read(cx).selected_unit()),
        Some("backup.service".into()),
        "静态的服务默认不列出，跳过来时列出全部"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-service-backup.service").is_some());
        window.click("tools-service-timer-backup.timer", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("tools-timer-backup.timer").is_some(),
            "服务详情中的定时器点回来"
        );
        window.click("tools-timer-detail-stop", cx);
    })
    .unwrap();
    assert_eq!(
        cx.read(|cx| services.read(cx).selected_unit()),
        Some("backup.timer".into())
    );
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.has_active_dialog(cx));
        assert!(window.try_find("tools-service-risk-confirm").is_some());
        assert!(window.try_find("tools-confirm-host").is_some());
        assert!(window.try_find("tools-confirm-command").is_some());
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// Docker 清理：工具条上的“清理…”打开对话框（读清单期间、打开时暂停刷新）；卷默认不勾并醒目提示会丢数据，Docker 23
/// 起写明命名的卷不在其中；勾选随时可改，写出将执行的命令；展开看名称；关掉后恢复刷新。
#[gpui_kit::test]
fn docker_cleanup_lists_kinds_and_leaves_volumes_unchecked(cx: &mut TestAppContext) {
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    cache(
        &panel,
        cx,
        id,
        Page::Docker,
        Cached::Docker(Rc::new(web_docker())),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(3usize, cx);
        window.render_frame(cx);
        window.click("tools-docker-cleanup", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let docker = cx.read(|cx| panel.read(cx).docker()).unwrap();
    let dialog = cx
        .read(|cx| docker.read(cx).cleanup_dialog())
        .expect("打开了清理的对话框");
    assert!(cx.read(|cx| docker.read(cx).is_paused()), "打开时暂停刷新");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("tools-cleanup-failed").is_some(),
            "重连中：说明读不到"
        );
    })
    .unwrap();
    let container = |id: char, name: &str, state: &str, volumes: &[&str]| PlanContainer {
        id: id.to_string().repeat(64),
        name: name.into(),
        state: state.into(),
        image: "1".repeat(64),
        volumes: volumes.iter().map(|v| v.to_string()).collect(),
        networks: vec!["bridge".into()],
        size: Some(1500),
    };
    let plan = CleanupPlan {
        engine: Engine::Docker,
        sudo: true,
        anonymous_volumes_only: true,
        sizes: true,
        containers_known: true,
        containers: vec![
            container('a', "old-app", "exited", &["anon1"]),
            container('b', "web", "running", &["web-data"]),
        ],
        dangling: vec![PlanImage {
            id: "2".repeat(64),
            size: Some(120_000_000),
            containers: Some(0),
        }],
        volumes: vec![
            PlanVolume {
                name: "anon1".into(),
                anonymous: true,
                size: Some(10_000_000),
                links: Some(1),
            },
            PlanVolume {
                name: "old-data".into(),
                anonymous: false,
                size: Some(5_000_000),
                links: Some(0),
            },
            PlanVolume {
                name: "web-data".into(),
                anonymous: false,
                size: None,
                links: Some(1),
            },
        ],
        networks: vec![PlanNetwork {
            id: "3".repeat(64),
            name: "old-net".into(),
            endpoints: Some(Vec::new()),
        }],
        build_cache: Some(BuildCacheUsage {
            entries: 2,
            reclaimable: 44,
        }),
    };
    cx.update(|cx| dialog.update(cx, |dialog, cx| dialog.set_test_plan(plan, cx)));
    let selected = |cx: &mut TestAppContext| cx.read(|cx| dialog.read(cx).selected_kinds());
    assert_eq!(
        selected(cx),
        [
            PruneKind::Containers,
            PruneKind::Networks,
            PruneKind::Images,
            PruneKind::BuildCache
        ],
        "卷默认不勾"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-cleanup-check-volumes").is_some());
        assert!(window.try_find("tools-cleanup-volume-warning").is_some());
        assert!(
            window.try_find("tools-cleanup-named-volumes").is_some(),
            "命名的卷不在其中"
        );
        assert!(window.try_find("tools-cleanup-commands").is_some());
        assert!(
            window
                .try_find("tools-cleanup-summary-containers")
                .is_some()
        );
        assert!(window.try_find("tools-cleanup-items-containers").is_none());
        window.click("tools-cleanup-names-containers", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("tools-cleanup-items-containers").is_some(),
            "展开看名称"
        );
        window.click("tools-cleanup-check-volumes", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(selected(cx).contains(&PruneKind::Volumes), "勾上卷");
    // 对话框关掉后视图被释放（测试不再拿着它）
    let weak = dialog.downgrade();
    drop(dialog);
    cx.update_window(handle, |_, window, cx| {
        window.click("tools-cleanup-cancel", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    // 对话框的层在下一帧移除，再下一帧视图才不在画面中
    for _ in 0..3 {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(!window.has_active_dialog(cx));
        })
        .unwrap();
        cx.run_until_parked();
    }
    assert!(weak.upgrade().is_none(), "对话框的视图已释放");
    assert!(
        !cx.read(|cx| docker.read(cx).cleanup_open()),
        "关掉后不再因它暂停刷新"
    );
}

/// 没有免密码 sudo 时，需要 root 的操作在确认框中以“在终端中执行”为主按钮（写出在终端中执行的命令），“仍然尝试”照常
/// 在后台执行；自己的进程、有免密码 sudo 时照旧。
#[gpui_kit::test]
fn without_sudo_root_actions_run_in_the_terminal(cx: &mut TestAppContext) {
    use termlet_core::hosttools::{Privilege, SudoState};
    let (handle, panel, _, ids) = open_with_everything(cx);
    let id = ids[0].entity_id();
    // 进程：init 是 root 的；服务：普通用户
    let mut list = owned_processes();
    list.processes[0].uid = Some(0);
    list.processes[0].user = Some("root".into());
    cache(
        &panel,
        cx,
        id,
        Page::Processes,
        Cached::Processes(Rc::new(list)),
    );
    cache(
        &panel,
        cx,
        id,
        Page::Services,
        Cached::Services(Rc::new(ServiceList {
            uid: Some(1000),
            ..cron_services()
        })),
    );
    // 换个终端再换回来：按新的缓存重建各页
    let other = terminal_ids(cx, 1);
    set_target(
        cx,
        handle,
        &panel,
        Some(target(other[0].entity_id(), ToolsState::Reconnecting)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    let processes = cx.read(|cx| panel.read(cx).processes()).unwrap();
    let services = cx.read(|cx| panel.read(cx).services()).unwrap();
    cx.update(|cx| {
        processes.update(cx, |page, cx| {
            page.feed().privilege = Some(Privilege::Limited(SudoState::Password));
            cx.notify();
        });
        services.update(cx, |page, cx| {
            page.feed().privilege = Some(Privilege::Limited(SudoState::Password));
            cx.notify();
        });
    });

    // 自己的进程：照旧确认、在后台执行
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(("tools-process-row", 42usize), cx);
        window.render_frame(cx);
        window.click("tools-process-detail-term", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.has_active_dialog(cx));
        assert!(window.try_find("tools-confirm-root-first").is_none());
        assert!(window.try_find("tools-confirm-terminal").is_none());
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();

    // root 的进程：主按钮是“在终端中执行”（这里没有真的终端：说明发起的终端已不在，不执行），不在后台尝试
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(("tools-process-row", 1usize), cx);
        window.render_frame(cx);
        window.click("tools-process-detail-term", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("tools-kill-risk-init").is_some(),
            "风险照写"
        );
        assert!(window.try_find("tools-confirm-root-first").is_some());
        assert!(window.try_find("tools-confirm-try").is_some());
        assert!(window.try_find("ok").is_none(), "没有原来的确认按钮");
        window.click("tools-confirm-terminal", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(!window.has_active_dialog(cx));
        assert_eq!(window.notifications(cx).len(), 1, "发起的终端已不在");
        assert!(
            window.try_find(("tools-process-op", 1usize)).is_none(),
            "没有在后台执行"
        );
        window.hover("host-tools-title", cx);
    })
    .unwrap();
    assert!(
        !cx.read(|cx| processes.read(cx).is_paused()),
        "确认框关掉后恢复刷新"
    );

    // 服务：“仍然尝试”照常在后台执行（没有连接：失败的原因写在信息行下）
    cx.update_window(handle, |_, window, cx| {
        window.within("host-tools-pages").click(2usize, cx);
        window.render_frame(cx);
        window.click("tools-service-cron.service", cx);
        window.render_frame(cx);
        window.click("tools-service-detail-stop", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-confirm-root-first").is_some());
        window.click("tools-confirm-try", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(!window.has_active_dialog(cx));
        assert!(window.try_find("tools-failure").is_some(), "在后台试过了");
    })
    .unwrap();

    // 有免密码 sudo：照旧
    cx.update(|cx| {
        services.update(cx, |page, cx| {
            page.feed().privilege = Some(Privilege::Sudo);
            cx.notify();
        })
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("tools-service-detail-stop", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-confirm-root-first").is_none());
        assert!(window.try_find("ok").is_some());
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// 监听着常见网页端口的行上有“在浏览器中打开”（交给工作区转发并打开），别的端口没有，右键菜单中监听着的 TCP 端口都有
/// （Docker 详情中发布的端口见 `docker_problems_usage_and_detail`）。
#[gpui_kit::test]
fn web_ports_open_in_the_browser(cx: &mut TestAppContext) {
    let (handle, panel, events, _ids) = open_with_everything(cx);
    let keys = conn_keys(&cx.read(|cx| cx_cached_conns(&panel, cx)).conns);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(4usize, cx);
        window.render_frame(cx);
        // 全部连接的视图：监听着网页端口的行（9090）有，SSH（22）没有
        assert!(
            window
                .try_find(format!("tools-conn-browser-{}", keys[4]))
                .is_some(),
            "全部连接的视图中也有"
        );
        assert!(
            window
                .try_find(format!("tools-conn-browser-{}", keys[0]))
                .is_none()
        );
        // 监听的视图
        window.within("tools-conn-filter").click(1usize, cx);
        window.render_frame(cx);
        assert!(
            window.try_find("tools-listen-browser-tcp-22").is_none(),
            "SSH 不是网页"
        );
        assert!(window.try_find("tools-listen-browser-tcp-9090").is_some());
        window.click("tools-listen-browser-tcp-8081", cx);
    })
    .unwrap();
    assert_eq!(*events.borrow(), ["browser"]);
    // 右键菜单：SSH 的端口也可以在浏览器中打开（不确定是不是网页的端口也提供）
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click("tools-listen-tcp-22", cx);
    })
    .unwrap();
    cx.run_until_parked();
    click_menu_item(cx, handle, &t!("tools.network.open_browser"));
    assert_eq!(*events.borrow(), ["browser", "browser"]);
}

/// “在浏览器中打开”的规则：同一个目标沿用已有的规则；本机端口先试与服务器相同的（1024 以上、空闲），被占用、小于 1024
/// 时由系统挑空闲的；沿用的规则的端口被别的程序占了时换一个；链接按 http / https 与回环地址写。
#[gpui_kit::test]
fn browser_rules_are_reused_and_take_free_ports(cx: &mut TestAppContext) {
    use crate::forwards::{BrowserRequest, browser_url, prepare_browser_rule};
    use termlet_core::model::Host;
    use termlet_core::store::Position;
    let (_handle, _panel, _) = open(cx);
    let store = cx.update(|cx| AppState::store(cx));
    let host = store
        .create(
            Host {
                name: "web".into(),
                address: "10.0.0.5".into(),
                username: "termlet".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let request = |port: u16| BrowserRequest {
        host: host.clone(),
        target_host: "127.0.0.1".into(),
        port,
        https: false,
        name: "nginx".into(),
    };
    let not_running = |_: &termlet_core::id::ForwardId| false;
    // 服务器上的端口在本机空闲：用同一个端口
    let free = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (id, rule, running) = prepare_browser_rule(&store, &request(free), not_running).unwrap();
    assert!(!running);
    assert_eq!(rule.bind_port, free);
    assert_eq!(rule.name, "nginx");
    // 同一个目标：沿用
    let (again, _, _) = prepare_browser_rule(&store, &request(free), not_running).unwrap();
    assert_eq!(again, id);
    // 运行着的直接用
    let (_, _, running) = prepare_browser_rule(&store, &request(free), |r| *r == id).unwrap();
    assert!(running);
    // 本机端口被别的程序占了：换一个空闲的，记进规则
    let busy = std::net::TcpListener::bind(("127.0.0.1", free)).unwrap();
    let (moved, rule, _) = prepare_browser_rule(&store, &request(free), not_running).unwrap();
    assert_eq!(moved, id);
    assert_ne!(rule.bind_port, free);
    assert_eq!(
        store
            .get::<termlet_core::model::Forward>(&id)
            .unwrap()
            .doc
            .bind_port,
        rule.bind_port
    );
    drop(busy);
    // 别的目标、1024 以下的端口：新的规则，系统挑的端口
    let (low, rule, _) = prepare_browser_rule(&store, &request(80), not_running).unwrap();
    assert_ne!(low, id);
    assert!(rule.bind_port >= 1024 && rule.bind_port != 80);
    assert_eq!(
        browser_url(false, "127.0.0.1", 8080),
        "http://127.0.0.1:8080/"
    );
    assert_eq!(
        browser_url(true, "0.0.0.0", 8443),
        "https://127.0.0.1:8443/"
    );
    assert_eq!(browser_url(false, "::1", 3000), "http://[::1]:3000/");
    // :: 只监听 IPv6：浏览器连 IPv6 回环
    assert_eq!(browser_url(false, "::", 3000), "http://[::1]:3000/");
}

/// 连真实的 sshd：“在浏览器中打开”转发到本机（共用终端的连接），交给浏览器的是本机的链接（测试中记录下来，不真的打开），
/// 经它能连到服务器上的端口；连接结束后转发随之停止、本机端口释放。
#[gpui_kit::test]
fn browser_forward_follows_a_real_connection(cx: &mut TestAppContext) {
    use termlet_core::model::{Host, SecretField};
    use termlet_core::secret::SecretString;
    use termlet_core::ssh::{
        Answer, ConnectOptions, ConnectionPool, Prompt, Prompter, connect, plan_for_host,
    };
    use termlet_core::store::{Owner, Position};
    let Some((address, port)) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let (handle, _panel, _) = open(cx);
    let opened: Rc<RefCell<Vec<String>>> = Rc::default();
    let recorded = opened.clone();
    cx.update(|cx| {
        cx.set_global(crate::forwards::BrowserOpener(Rc::new(move |url, _| {
            recorded.borrow_mut().push(url.to_string())
        })))
    });
    let store = cx.update(|cx| AppState::store(cx));
    let host = store
        .create(
            Host {
                name: "tools".into(),
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
    let prompter: Prompter = Arc::new(|prompt| {
        Box::pin(async move {
            match prompt {
                Prompt::HostKey(_) => Answer::AcceptHostKey,
                _ => Answer::Cancel,
            }
        })
    });
    // 模拟终端已经持有一条连接
    let plan = plan_for_host(&store, &host).unwrap();
    let group = ConnectionPool::new_group();
    let terminal = termlet_core::runtime::runtime()
        .block_on(async {
            let options = ConnectOptions::default();
            ConnectionPool::global()
                .acquire(&group, || {
                    connect(store.clone(), &plan, prompter.clone(), &options)
                })
                .await
        })
        .expect("连接测试用的 sshd");
    // 服务器上的 sshd 自己（22）：经转发读到它的版本行
    let request = crate::forwards::BrowserRequest {
        host: host.clone(),
        target_host: "127.0.0.1".into(),
        port: 22,
        https: false,
        name: "sshd".into(),
    };
    cx.update_window(handle, |_, window, cx| {
        crate::forwards::open_in_browser(request, Some(group.clone()), prompter.clone(), window, cx)
    })
    .unwrap();
    wait_until(cx, handle, "交给浏览器", |_, _| {
        !opened.borrow().is_empty()
    });
    let url = opened.borrow()[0].clone();
    let local: u16 = url
        .trim_start_matches("http://127.0.0.1:")
        .trim_end_matches('/')
        .parse()
        .unwrap_or_else(|_| panic!("{url}"));
    assert!(local >= 1024, "22 小于 1024：系统挑的端口");
    let banner = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", local)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut buf = [0u8; 8];
        stream.read_exact(&mut buf).unwrap();
        String::from_utf8_lossy(&buf).to_string()
    })
    .join()
    .unwrap();
    assert_eq!(banner, "SSH-2.0-", "经转发连到服务器上的端口");
    let id = store
        .list::<termlet_core::model::Forward>()
        .into_iter()
        .find(|r| r.doc.bind_port == local)
        .expect("规则在转发列表中")
        .id;
    // 连接结束：转发随之停止，不重连
    termlet_core::runtime::runtime().block_on(terminal.disconnect());
    drop(terminal);
    wait_until(cx, handle, "转发随连接结束而停", |_, cx| {
        crate::forwards::status(&id, cx).state == crate::forwards::RowState::Stopped
    });
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", local)).is_ok(),
        "本机端口已释放"
    );
}

/// 定时器的时间按服务器的时区：服务器与本机的时区不同时，信息行写出服务器的时区与本机的偏移，行上的时间带上服务器的
/// 偏移，详情写明服务器时区、日历表达式按它解释，并另写本机的时间。
#[gpui_kit::test]
fn timers_show_server_times(cx: &mut TestAppContext) {
    use termlet_core::hosttools::ServerZone;
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    let now = termlet_core::time::now_ms();
    let local = termlet_core::time::local_offset(now).unwrap();
    // 与本机差 5 小时的服务器
    let server = if local > 0 {
        local - 5 * 3600
    } else {
        local + 5 * 3600
    };
    let services = ServiceList {
        timers: vec![Timer {
            unit: "backup.timer".into(),
            description: "Nightly backup".into(),
            load_state: "loaded".into(),
            active_state: "active".into(),
            sub_state: "waiting".into(),
            unit_file_state: "enabled".into(),
            activates: "backup.service".into(),
            next: Some(now + 3_600_000),
            next_offset: Some(server),
            last: Some(now - 3_600_000),
            last_offset: Some(server),
            calendar: vec!["*-*-* 03:00:00".into()],
            ..Default::default()
        }],
        version: "systemd 257".into(),
        system_state: "running".into(),
        uid: Some(0),
        zone: Some(ServerZone {
            name: "Etc/Test".into(),
            abbr: "TST".into(),
            offset: server,
        }),
        ..Default::default()
    };
    cache(
        &panel,
        cx,
        id,
        Page::Services,
        Cached::Services(Rc::new(services)),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("host-tools-pages").click(2usize, cx);
        window.render_frame(cx);
        assert!(
            window.try_find("tools-timer-zone-info").is_none(),
            "服务子页写系统状态"
        );
        window.within("tools-service-filter").click(4usize, cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-timer-zone-info").is_some());
        window.click("tools-timer-backup.timer", cx);
        window.render_frame(cx);
        assert!(window.try_find("tools-timer-zone").is_some());
        assert!(window.try_find("tools-timer-zone-differs").is_some());
        assert!(window.try_find("tools-timer-calendar-zone").is_some());
    })
    .unwrap();
}

/// 服务页的信息行：系统状态在前、版本在后；繁体最窄的面板下 degraded 时版本放不下就整个不显示，不截成半截，状态的
/// 标签完整地在面板中。
#[gpui_kit::test]
fn degraded_state_keeps_the_version_whole(cx: &mut TestAppContext) {
    use crate::i18n::{Language, set_language};
    let (handle, panel, _) = open(cx);
    let ids = terminal_ids(cx, 1);
    let id = ids[0].entity_id();
    cache(
        &panel,
        cx,
        id,
        Page::Services,
        Cached::Services(Rc::new(ServiceList {
            version: "systemd 257 (257.9-1~deb13u1)".into(),
            system_state: "degraded".into(),
            ..cron_services()
        })),
    );
    set_target(
        cx,
        handle,
        &panel,
        Some(target(id, ToolsState::Reconnecting)),
    );
    for (language, width) in [
        (Language::ZhTw, super::model::WIDTH_MIN),
        (Language::En, super::model::WIDTH_MIN),
        (Language::Ja, super::model::WIDTH_MIN),
        (Language::ZhTw, 520.),
    ] {
        set_language(language);
        widen(cx, handle, width);
        cx.update_window(handle, |_, window, cx| {
            window.within("host-tools-pages").click(2usize, cx);
            window.render_frame(cx);
            window.render_frame(cx);
            let panel_box = window.find("host-tools").bounds();
            let state = window.find("tools-service-degraded").bounds();
            let version = window.find("tools-service-version").bounds();
            assert!(
                state.right() <= panel_box.right(),
                "{language:?} {width}：状态标签完整"
            );
            let wrapped = version.top() >= state.bottom();
            assert!(
                wrapped || version.left() >= state.right(),
                "{language:?} {width}：版本在状态后面，或者整个换到看不见的第二行"
            );
            if width > 500. {
                assert!(!wrapped, "{language:?}：宽的面板写出版本");
            }
        })
        .unwrap();
    }
    set_language(Language::ZhCn);
}

/// 详情区的高度按 100% 界面大小记：拖出的高度在 150% 时记成除以 1.5 的值，用时乘回去；换了界面大小后跟着缩放。
#[gpui_kit::test]
fn detail_height_scales_with_the_ui(cx: &mut TestAppContext) {
    let (handle, _panel, _, _ids) = open_with_everything(cx);
    let height = |cx: &mut TestAppContext| {
        cx.read(|cx| {
            AppState::global(cx)
                .ui
                .read(cx)
                .state()
                .host_tools_detail_height
        })
    };
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
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(("tools-process-row", 42usize), cx);
        window.render_frame(cx);
        let splitter = window
            .find("tools-process-detail-splitter")
            .bounds()
            .center();
        window.drag(splitter, point(splitter.x, splitter.y - px(90.)), cx);
        window.render_frame(cx);
    })
    .unwrap();
    let saved = height(cx).expect("拖动后记下高度");
    // 详情卡片显示的高度：分隔条在两张卡片之间空隙的中间，卡片的下边离面板的下边一个空隙
    let shown = |window: &mut Window| {
        let gaps = gpui_kit::rems(1.5 * super::shared::CARD_GAP).to_pixels(window.rem_size());
        f32::from(window.find("host-tools").bounds().bottom())
            - f32::from(
                window
                    .find("tools-process-detail-splitter")
                    .bounds()
                    .center()
                    .y,
            )
            - f32::from(gaps)
    };
    let detail_150 = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            shown(window)
        })
        .unwrap();
    assert!(
        (detail_150 / 1.5 - saved).abs() < 3.,
        "记下的是 100% 时的高度：{saved}，显示 {detail_150}"
    );
    // 回到 100%：显示的高度就是记下的
    set_scale(cx, 100);
    let detail_100 = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.render_frame(cx);
            shown(window)
        })
        .unwrap();
    assert!(
        (detail_100 - saved).abs() < 3.,
        "100% 时显示记下的高度：{saved}，显示 {detail_100}"
    );
    assert_eq!(height(cx), Some(saved), "换界面大小不改记下的高度");
}
