//! 启动时自动打开（设置“通用”页的“启动时打开”，本机设置 `window.startup`，不同步）：默认的本地终端，或指定的
//! 一台或几台主机（按设置的先后打开）。
//!
//! - 只在启动 Termlet 时生效：启动时登记（[`arm`]），本次运行打开的第一个主窗口取走（[`take`]）；新窗口、macOS 上
//!   关掉最后一个窗口后从程序坞再打开的窗口都不打开。被命令行工具在后台启动的，用户第一次打开窗口时打开。
//! - 保险库、应用锁解锁之后才打开：与外部程序调起的请求在同一处取走（工作区的 `take_launches`），先打开这里的，
//!   再处理调起的请求（调起的连接排在最后、获得焦点）。
//! - Termlet 不恢复上次的 Tab（只恢复窗口的位置、大小与侧边栏宽度等界面状态），启动时本来没有打开的 Tab。已经有
//!   同一台主机的终端（例如先处理了别的请求）时不重复打开；最后切到设置中排在最前、还在的那台主机。
//! - 主机被删掉了：跳过，启动后提示一次，并从设置中去掉（之后不再提示）。

use gpui_kit::{App, Global};
use termlet_core::id::HostId;

/// 启动时登记、还没有被窗口取走。
struct PendingStartup;

impl Global for PendingStartup {}

/// 启动时调用（打开主窗口之前）：本次运行的第一个主窗口解锁后按设置打开。
pub fn arm(cx: &mut App) {
    cx.set_global(PendingStartup);
}

/// 取走启动时要打开的（每次运行只有一次）：返回是否轮到这个窗口打开。
pub fn take(cx: &mut App) -> bool {
    if !cx.has_global::<PendingStartup>() {
        return false;
    }
    cx.remove_global::<PendingStartup>();
    true
}

/// 指定主机时要做的。
#[derive(Debug, Default, PartialEq)]
pub struct HostPlan {
    /// 要新打开的，按设置的先后
    pub open: Vec<HostId>,
    /// 打开后切到的：设置中排在最前、还在的主机
    pub first: Option<HostId>,
    /// 已经删掉的主机
    pub missing: Vec<HostId>,
}

/// 按设置的主机排出要做的：`exists` 为主机还在不在，`opened` 为这台主机是否已有终端。重复的只算一次。
pub fn plan_hosts(
    hosts: &[HostId],
    exists: impl Fn(&HostId) -> bool,
    opened: impl Fn(&HostId) -> bool,
) -> HostPlan {
    let mut plan = HostPlan::default();
    let mut seen: Vec<&HostId> = Vec::new();
    for id in hosts {
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        if !exists(id) {
            plan.missing.push(id.clone());
            continue;
        }
        if plan.first.is_none() {
            plan.first = Some(id.clone());
        }
        if !opened(id) {
            plan.open.push(id.clone());
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(raw: &[&str]) -> Vec<HostId> {
        raw.iter().map(|r| HostId::from_raw(*r)).collect()
    }

    // 按设置的先后打开；已有终端的不重复打开，但仍是切到的那台；删掉的跳过并记下；重复的只算一次
    #[test]
    fn plan_skips_opened_and_missing_hosts() {
        let hosts = ids(&["h_a", "h_gone", "h_b", "h_a", "h_c"]);
        let plan = plan_hosts(
            &hosts,
            |id| id.as_str() != "h_gone",
            |id| id.as_str() == "h_a",
        );
        assert_eq!(plan.open, ids(&["h_b", "h_c"]));
        assert_eq!(plan.first, Some(HostId::from_raw("h_a")));
        assert_eq!(plan.missing, ids(&["h_gone"]));

        let plan = plan_hosts(&ids(&["h_gone"]), |_| false, |_| false);
        assert_eq!(plan.first, None);
        assert!(plan.open.is_empty());
        assert_eq!(plan.missing.len(), 1);
        assert_eq!(plan_hosts(&[], |_| true, |_| false), HostPlan::default());
    }

    // 只有第一次取得到（新窗口不再打开）；没有登记时取不到
    #[gpui_kit::test]
    fn only_the_first_window_takes_it(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            assert!(!take(cx), "没有登记");
            arm(cx);
            assert!(take(cx));
            assert!(!take(cx), "第二个窗口取不到");
        });
    }
}
