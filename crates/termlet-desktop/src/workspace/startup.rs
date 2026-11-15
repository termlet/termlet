//! 启动时自动打开设置中的本地终端或主机（规则见 [`crate::startup_open`]）。

use gpui_kit::{Context, Window};
use termlet_core::model::{Host, StartupOpen};

use super::Workspace;
use crate::app_state::AppState;
use crate::startup_open;
use crate::t;

impl Workspace {
    /// 本次运行的第一个主窗口在保险库、应用锁解锁后调用（与外部程序调起的请求同一处，在它们之前）：按设置打开。
    pub(super) fn open_startup_items(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vault_gate.is_some() || crate::lock_ui::is_locked(cx) {
            return;
        }
        if !startup_open::take(cx) {
            return;
        }
        let store = AppState::store(cx);
        let startup = store.local_settings().window.startup;
        match startup.open {
            StartupOpen::Nothing => {}
            StartupOpen::Local => {
                let existing = self
                    .live_terminals()
                    .into_iter()
                    .find(|p| p.read(cx).is_local_session());
                match existing {
                    Some(panel) => {
                        tracing::info!("启动时打开：已有本地终端，切过去");
                        self.show_terminal(&panel, window, cx);
                    }
                    None => {
                        tracing::info!("启动时打开本地终端");
                        self.open_local_terminal(window, cx);
                    }
                }
            }
            StartupOpen::Hosts => {
                let plan = startup_open::plan_hosts(
                    &startup.hosts,
                    |id| store.get::<Host>(id).is_some(),
                    |id| {
                        self.live_terminals()
                            .iter()
                            .any(|p| p.read(cx).host() == Some(id))
                    },
                );
                tracing::info!(
                    open = plan.open.len(),
                    missing = plan.missing.len(),
                    "启动时打开主机"
                );
                for id in &plan.open {
                    self.open_host(id, window, cx);
                }
                let first = plan.first.as_ref().and_then(|id| {
                    self.live_terminals()
                        .into_iter()
                        .find(|p| p.read(cx).host() == Some(id))
                });
                if let Some(panel) = first {
                    self.show_terminal(&panel, window, cx);
                }
                if !plan.missing.is_empty() {
                    // 提示一次，并从设置中去掉（之后启动不再提示）
                    let missing = plan.missing;
                    if let Err(e) = store.update_local_settings(|s| {
                        s.window.startup.hosts.retain(|h| !missing.contains(h))
                    }) {
                        tracing::warn!("从启动时打开的主机中去掉已删除的失败：{e}");
                    }
                    crate::notify::warning(
                        t!("startup_open.hosts_missing", count = missing.len()),
                        window,
                        cx,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{
        AnyWindowHandle, App, AppContext as _, Bounds, Entity, TestAppContext, WindowBounds,
        WindowOptions, px, size,
    };
    use termlet_core::id::HostId;
    use termlet_core::model::{Host, StartupOpen, StartupSettings};
    use termlet_core::store::Position;
    use termlet_core::vault::MemoryKeyStore;

    use super::Workspace;
    use crate::app_state::AppState;
    use crate::startup_open;

    fn prepare(cx: &mut TestAppContext, install: impl FnOnce(&mut App)) {
        // 连接在核心的运行时上进行，本地终端的伪终端是真实的系统线程
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(install);
        cx.update(|cx| cx.set_reduce_motion(true));
    }

    /// 打开一个主窗口（不经 testkit：要在打开之前登记启动时打开、改好设置）。
    fn open_window(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
        let opened = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            Default::default(),
                            size(px(1024.), px(640.)),
                        ))),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Workspace::new(window, cx)),
                )
            })
            .expect("打开测试窗口");
        cx.run_until_parked();
        opened
    }

    /// 连不上的主机（本机的 1 号端口）：打开后很快失败，不用真的 sshd。
    fn add_host(cx: &mut App, name: &str) -> HostId {
        AppState::store(cx)
            .create(
                Host {
                    name: name.into(),
                    address: "127.0.0.1".into(),
                    port: 1,
                    username: "ops".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap()
    }

    fn set_startup(cx: &mut App, open: StartupOpen, hosts: Vec<HostId>) {
        AppState::store(cx)
            .update_local_settings(|s| {
                s.window.startup = StartupSettings {
                    open,
                    hosts,
                    ..Default::default()
                }
            })
            .unwrap();
    }

    /// 窗口中终端连的主机，按打开的先后（本地终端为 `None`）。
    fn terminal_hosts(
        cx: &mut TestAppContext,
        workspace: &Entity<Workspace>,
    ) -> Vec<Option<HostId>> {
        cx.update(|cx| {
            workspace
                .read(cx)
                .live_terminals()
                .iter()
                .map(|p| p.read(cx).host().cloned())
                .collect()
        })
    }

    // 启动时按设置的先后打开主机，切到排在最前的那台；删掉的跳过并从设置中去掉；之后打开的新窗口不再打开
    #[gpui_kit::test]
    fn startup_opens_the_hosts_once(cx: &mut TestAppContext) {
        prepare(cx, AppState::install_in_memory);
        let gone = HostId::from_raw("h_gone");
        let (web, db) = cx.update(|cx| {
            let web = add_host(cx, "web");
            let db = add_host(cx, "db");
            set_startup(
                cx,
                StartupOpen::Hosts,
                vec![db.clone(), gone.clone(), web.clone()],
            );
            startup_open::arm(cx);
            (web, db)
        });
        let (_, workspace) = open_window(cx);
        assert_eq!(
            terminal_hosts(cx, &workspace),
            [Some(db.clone()), Some(web.clone())]
        );
        cx.update(|cx| {
            let terminals = workspace.read(cx).live_terminals();
            assert!(terminals[0].read(cx).is_shown(), "切到排在最前的 db");
            assert!(!terminals[1].read(cx).is_shown());
            assert_eq!(
                AppState::store(cx).local_settings().window.startup.hosts,
                [db.clone(), web.clone()],
                "删掉的主机从设置中去掉了"
            );
        });
        let (_, second) = open_window(cx);
        assert!(terminal_hosts(cx, &second).is_empty(), "新窗口不打开");
    }

    // 已经有同一台主机的终端时不重复打开，切到它
    #[gpui_kit::test]
    fn already_open_hosts_are_not_opened_again(cx: &mut TestAppContext) {
        prepare(cx, AppState::install_in_memory);
        let (web, db) = cx.update(|cx| (add_host(cx, "web"), add_host(cx, "db")));
        let (handle, workspace) = open_window(cx);
        assert!(
            terminal_hosts(cx, &workspace).is_empty(),
            "没有登记时不打开"
        );
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_host(&web, window, cx));
        })
        .unwrap();
        cx.update(|cx| {
            set_startup(cx, StartupOpen::Hosts, vec![web.clone(), db.clone()]);
            startup_open::arm(cx);
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.take_launches(window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            terminal_hosts(cx, &workspace),
            [Some(web.clone()), Some(db.clone())]
        );
        cx.update(|cx| {
            let terminals = workspace.read(cx).live_terminals();
            assert!(terminals[0].read(cx).is_shown(), "切到已经打开的 web");
        });
    }

    // 保险库没有解锁时不打开，解锁后再打开
    #[gpui_kit::test]
    fn startup_waits_for_the_vault(cx: &mut TestAppContext) {
        prepare(cx, |cx| {
            AppState::install_in_memory_with(Box::new(MemoryKeyStore::unavailable()), cx)
        });
        let web = cx.update(|cx| {
            let web = add_host(cx, "web");
            set_startup(cx, StartupOpen::Hosts, vec![web.clone()]);
            startup_open::arm(cx);
            web
        });
        let (_, workspace) = open_window(cx);
        assert!(!cx.update(|cx| AppState::store(cx).vault().is_unlocked()));
        assert!(terminal_hosts(cx, &workspace).is_empty(), "锁着时不打开");
        cx.update(|cx| {
            AppState::store(cx)
                .set_vault_password(&"correct horse".into())
                .unwrap()
        });
        cx.run_until_parked();
        assert_eq!(terminal_hosts(cx, &workspace), [Some(web)]);
    }

    // 选了本地终端：打开默认的本地终端；不额外打开时什么也不开
    #[gpui_kit::test]
    fn startup_opens_the_local_terminal(cx: &mut TestAppContext) {
        prepare(cx, AppState::install_in_memory);
        cx.update(|cx| {
            set_startup(cx, StartupOpen::Local, Vec::new());
            startup_open::arm(cx);
        });
        let (_, workspace) = open_window(cx);
        assert_eq!(terminal_hosts(cx, &workspace), [None]);
        assert!(cx.update(|cx| {
            workspace.read(cx).live_terminals()[0]
                .read(cx)
                .is_local_session()
        }));
    }
}
