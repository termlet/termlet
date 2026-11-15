//! 被堡垒机等外部程序调起：JumpServer、绿盟 OSMS 等用 Xshell、PuTTY、SecureCRT、MobaXterm、WinSCP、Xftp
//! 的启动参数调起本地客户端，参数由核心的 `launch` 解析。
//!
//! - 按设置（设置 → 连接）每次询问、直接打开或忽略。
//! - 打开的是临时连接：不进主机列表，密码只在内存中；只用一个通道（不复制会话、不在旁边开 SFTP、
//!   不记命令历史）。Tab 菜单中可以"保存到主机列表…"。
//! - 请求先排队，窗口建好、保险库解锁后由工作区取走（锁定期间收到的请求不丢）。
//! - 日志只记来源、协议、地址、端口，不记用户名与密码。

use std::sync::Arc;

use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::{App, Global, SharedString, WeakEntity, Window};
use termlet_core::launch::{LaunchKind, LaunchTarget};
use termlet_core::model::{ExternalLaunch, Host};
use termlet_core::ssh::plan::AuthPlan;
use termlet_core::ssh::{TerminalSpec, connect_options, plan_for_quick};
use termlet_core::store::Store;

use crate::app_state::AppState;
use crate::prose::prose;
use crate::t;
use crate::workspace::Workspace;

/// 还没有处理的调起请求。
#[derive(Default)]
struct PendingLaunches(Vec<LaunchTarget>);

impl Global for PendingLaunches {}

/// 解析本进程的启动参数（不含程序本身与 Termlet 自己的选项，见 [`crate::args`]）；认出时放进队列。
/// 读 .xsh 会话文件（最多 64 KB）是同步的，在打开窗口之前调用。
pub fn queue_from_args(args: &[String], cx: &mut App) {
    if args.is_empty() {
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    match termlet_core::launch::parse(args, &cwd) {
        Some(target) => queue(target, cx),
        None => tracing::info!(count = args.len(), "启动参数中没有认出要打开的连接"),
    }
}

/// 收到一个调起请求（本进程的启动参数，或已有实例转交来的）。
pub fn queue(target: LaunchTarget, cx: &mut App) {
    tracing::info!(target = %target.log_summary(), "外部程序调起");
    cx.default_global::<PendingLaunches>().0.push(target);
}

/// 取走排队的请求（工作区在保险库解锁后调用）。
pub fn take(cx: &mut App) -> Vec<LaunchTarget> {
    if !cx.has_global::<PendingLaunches>() {
        return Vec::new();
    }
    std::mem::take(&mut cx.global_mut::<PendingLaunches>().0)
}

/// 按设置处理一个请求：忽略时提示，询问时确认后打开，允许时直接打开。
pub fn handle(
    target: LaunchTarget,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let setting = AppState::store(cx).settings().connection.external_launch;
    tracing::info!(target = %target.log_summary(), ?setting, "处理外部程序调起的请求");
    match setting {
        ExternalLaunch::Off => {
            crate::notify::warning(
                t!("launch.ignored", target = target.display_target()),
                window,
                cx,
            );
        }
        ExternalLaunch::Allow => open(target, &workspace, window, cx),
        ExternalLaunch::Ask => {
            let kind = match target.kind {
                LaunchKind::Terminal => t!("launch.kind_terminal"),
                LaunchKind::Sftp => t!("launch.kind_sftp"),
            };
            let description = t!(
                "launch.confirm_body",
                target = target.display_target(),
                kind = kind
            );
            let target = Arc::new(target);
            crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
                let (target, workspace) = (target.clone(), workspace.clone());
                alert
                    .confirm()
                    .title(t!("launch.confirm_title"))
                    .description(prose(description.clone()))
                    .ok_text(t!("launch.open"))
                    .ok_variant(ButtonVariant::Primary)
                    .cancel_text(t!("common.cancel"))
                    .on_ok(move |_, window, cx| {
                        tracing::info!(target = %target.log_summary(), "用户确认打开外部程序调起的连接");
                        open((*target).clone(), &workspace, window, cx);
                        true
                    })
            });
        }
    }
}

/// 推迟到这次更新结束后再打开：调用方（`Workspace::take_launches`）本身就在更新工作区。
fn open(
    target: LaunchTarget,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let workspace = workspace.clone();
    window.defer(cx, move |window, cx| {
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.open_launch(target, window, cx)
        });
    });
}

/// 临时连接的连接参数：只用一个通道；带了密码时用它，否则与快速连接相同（先试 ssh-agent，再询问密码）。
pub fn spec(store: &Store, target: &LaunchTarget) -> TerminalSpec {
    let mut plan = plan_for_quick(&target.quick_target());
    if let Some(hop) = plan.hops.first_mut()
        && target.password.is_some()
    {
        hop.auth = AuthPlan::Password {
            saved: target.password.clone(),
        };
    }
    TerminalSpec {
        plan,
        connect: connect_options(&store.settings()),
        encoding: target.encoding.unwrap_or_default(),
        tmux: false,
        agent_forward: false,
        single_channel: true,
        x11: None,
    }
}

/// Tab 标题：有名称时用名称，否则为"用户@地址"。
pub fn title(target: &LaunchTarget) -> SharedString {
    target.title().into()
}

/// "保存到主机列表…"预填的主机。
pub fn host_draft(target: &LaunchTarget) -> Host {
    let mut host = Host {
        name: target.name.clone().unwrap_or_default(),
        address: target.address.clone(),
        port: if target.port == 22 { 0 } else { target.port },
        username: target.username.clone(),
        ..Default::default()
    };
    host.terminal.encoding = target.encoding.unwrap_or_default();
    host
}

/// 快速连接的 Tab "保存到主机列表…"预填的主机（地址、端口、用户名）。
pub fn quick_host_draft(target: &termlet_core::quick::QuickTarget) -> Host {
    Host {
        address: target.host.clone(),
        port: if target.port == 22 { 0 } else { target.port },
        username: target.username.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termlet_core::model::Encoding;

    fn target(args: &[&str]) -> LaunchTarget {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        termlet_core::launch::parse(&args, std::path::Path::new(".")).expect("认得出")
    }

    #[test]
    fn spec_uses_the_password_and_a_single_channel() {
        let db = termlet_core::db::Database::open_in_memory().unwrap();
        let keys = Box::new(termlet_core::vault::MemoryKeyStore::default());
        let vault = termlet_core::vault::Vault::open(&db, keys).unwrap();
        let store = Store::open(db, vault).unwrap();
        let with_password = target(&["-ssh", "ops@10.0.0.5", "-P", "2200", "-pw", "s3cret"]);
        let spec = super::spec(&store, &with_password);
        assert!(spec.single_channel);
        assert!(!spec.tmux);
        let hop = &spec.plan.hops[0];
        assert_eq!((hop.address.as_str(), hop.port), ("10.0.0.5", 2200));
        match &hop.auth {
            AuthPlan::Password {
                saved: Some(password),
            } => assert_eq!(password.expose(), "s3cret"),
            other => panic!("应当用带来的密码：{other:?}"),
        }
        // 兼容老堡垒机的旧算法
        assert!(hop.legacy_algorithms);
        // 没有带密码：先试 ssh-agent，再询问
        let spec = super::spec(&store, &target(&["ssh://ops@10.0.0.5"]));
        assert!(matches!(
            spec.plan.hops[0].auth,
            AuthPlan::AgentThenPassword
        ));
    }

    // 设置为“直接打开”：工作区取走排队的请求时（正在更新工作区）不能再同步更新它
    #[gpui_kit::test]
    fn allowed_launch_opens_a_terminal(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        cx.update(|cx| {
            AppState::store(cx)
                .update_settings(|s| s.connection.external_launch = ExternalLaunch::Allow)
                .unwrap()
        });
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        cx.update(|cx| queue(target(&["ssh://ops@127.0.0.1:1"]), cx));
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.take_launches(window, cx))
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(workspace.read(cx).terminal_count(), 1));
    }

    #[test]
    fn host_draft_prefills_the_connection() {
        let target = target(&["ssh://ops@10.0.0.5:22"]);
        let host = host_draft(&target);
        assert_eq!(host.address, "10.0.0.5");
        assert_eq!(host.port, 0, "22 按默认端口保存");
        assert_eq!(host.username, "ops");
        assert_eq!(host.terminal.encoding, Encoding::Utf8);
    }

    #[test]
    fn quick_host_draft_prefills_the_address() {
        let target = termlet_core::quick::QuickTarget {
            username: "ops".into(),
            host: "10.0.0.5".into(),
            port: 2222,
        };
        let host = quick_host_draft(&target);
        assert_eq!(
            (host.address.as_str(), host.port, host.username.as_str()),
            ("10.0.0.5", 2222, "ops")
        );
    }
}
