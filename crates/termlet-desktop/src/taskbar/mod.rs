//! 传输进度显示在任务栏上：设置“通用”中的开关，本机设置 `window.taskbar_progress`，默认开。
//!
//! - Windows：每个主窗口的任务栏按钮显示这个窗口中的传输（SFTP、SCP 的上传与下载，见核心的 `sftp::transfer`）的
//!   总体进度：多个传输按字节合计，还不知道总量（目录正在统计）时显示为不确定，都在等待重连时显示为暂停（黄色），
//!   有失败的显示为错误（红色）。全部结束后清除；有失败的保持红色，直到这个窗口被激活（结束时窗口正在前台就不保持），
//!   或者失败的记录被继续、移除。
//! - macOS：程序坞图标只有一个，显示所有窗口的传输合计：标记上显示百分比，不确定时为“…”，有失败时为“!”。
//! - Linux：程序的图标只有一个（与 macOS 相同，所有窗口的传输合计），经 D-Bus 发 Unity 的 LauncherEntry 信号显示
//!   进度条，有失败时请求注意（`unity`）；GNOME 的 Dash to Dock 与 Ubuntu Dock、KDE Plasma 等支持，不支持的桌面、没有
//!   D-Bus 会话时没有效果。
//!
//! 一轮（[`Tracker`]）：从有未结束的传输开始，到全部结束为止；这期间出现过的传输都算在内（已完成的仍计入合计，
//! 进度不会因为完成的被移出列表而倒退），用户取消的不计。终端中的 rz / sz、trzsz 传输不在传输列表中，不计。
//!
//! 任务栏的操作经 [`TaskbarHost`]（Windows 上为 `win32`，macOS 上为 `macos`，Linux 上为 `unity`），测试中换成记录调用的
//! 假实现。

#[cfg(target_os = "macos")]
mod macos;
#[cfg(test)]
mod tests;
#[cfg(any(target_os = "linux", test))]
mod unity;
#[cfg(windows)]
mod win32;

use std::collections::{HashMap, HashSet};

use gpui_kit::{AnyWindowHandle, App, Global, OperatingSystem, Window};
use termlet_core::sftp::{TransferState, TransferView};
use termlet_core::store::StoreEvent;

use crate::app_state::AppState;
use crate::sftp::panel::window_owner;
use crate::workspace::Workspace;

/// 系统是否支持（设置中只在这些系统上显示开关）。
pub(crate) fn supported(os: OperatingSystem) -> bool {
    matches!(
        os,
        OperatingSystem::Windows | OperatingSystem::Mac | OperatingSystem::Linux
    )
}

/// 任务栏上显示的状态。进度为千分比。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Indicator {
    /// 不显示
    None,
    /// 进行中
    Normal(u16),
    /// 进行中，还不知道总量
    Indeterminate,
    /// 未结束的都在等待重连
    Paused(u16),
    /// 有失败的
    Error(u16),
}

/// 任务栏的实现。
pub(crate) trait TaskbarHost {
    /// 每个窗口分别显示（Windows 的任务栏按钮）；否则整个应用一个（macOS 的程序坞图标）。
    fn per_window(&self) -> bool;
    /// 显示 `indicator`：分窗口的给出窗口，整个应用的为 `None`。
    fn show(&mut self, window: Option<&Window>, indicator: Indicator);
}

/// 传输列表中的一项（从核心的 `TransferView` 取出用得到的部分）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Progress {
    pub id: String,
    /// 所属窗口（见 `window_owner`）
    pub owner: String,
    pub done: u64,
    /// 总字节数（目录在统计完之前为 0）
    pub total: u64,
    pub state: TransferState,
}

impl From<&TransferView> for Progress {
    fn from(view: &TransferView) -> Self {
        Self {
            id: view.id.as_str().to_string(),
            owner: view.owner.clone(),
            done: view.done,
            total: view.total,
            state: view.state,
        }
    }
}

/// 一个传输在这一轮中最后一次看到的情况。
#[derive(Clone, Copy, Debug)]
struct Seen {
    done: u64,
    total: u64,
    state: TransferState,
}

/// 一个窗口（或整个应用）的进度：把传输列表的快照算成要显示的状态（见模块说明）。
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    /// 这一轮的传输
    round: HashMap<String, Seen>,
    /// 上一轮以失败结束：保持错误状态，直到窗口被激活或这些记录不再是失败的
    held: Option<HashSet<String>>,
    /// 上一次快照中的传输：两次快照之间就开始并结束了的（很快失败的）据此认出是新的
    known: HashSet<String>,
    /// 看过第一次快照（之前已有的记录不算新的）
    started: bool,
}

impl Tracker {
    /// 按新的快照（只含这个窗口的传输）算出要显示的状态。`active` 为窗口现在是否在前台：一轮以失败结束时在前台就
    /// 不保持错误状态（用户正看着传输列表）。
    pub(crate) fn update<'a>(
        &mut self,
        transfers: impl IntoIterator<Item = &'a Progress>,
        active: bool,
    ) -> Indicator {
        let mut pending = false;
        let mut present: HashSet<&str> = HashSet::new();
        let mut failed_now: HashSet<&str> = HashSet::new();
        let first = !std::mem::replace(&mut self.started, true);
        for t in transfers {
            present.insert(&t.id);
            let new = !first && !self.known.contains(&t.id);
            if t.state == TransferState::Failed {
                failed_now.insert(&t.id);
            }
            let seen = Seen {
                done: t.done,
                total: t.total,
                state: t.state,
            };
            if t.state.is_pending() || new {
                // 新出现就已经结束的：两次快照之间开始并结束（例如马上失败），同样算进这一轮
                pending |= t.state.is_pending();
                self.round.insert(t.id.clone(), seen);
            } else if let Some(slot) = self.round.get_mut(&t.id) {
                *slot = seen;
            }
        }
        self.known = present.iter().map(|id| id.to_string()).collect();
        // 不在快照中的：完成后被自动清除的照样计入，其余的（移除了）不再算
        self.round
            .retain(|id, seen| present.contains(id.as_str()) || seen.state == TransferState::Done);
        if pending {
            self.held = None;
            return self.round_indicator();
        }
        if !self.round.is_empty() {
            // 这一轮结束了
            let failed: HashSet<String> = self
                .round
                .iter()
                .filter(|(_, seen)| seen.state == TransferState::Failed)
                .map(|(id, _)| id.clone())
                .collect();
            self.round.clear();
            if !failed.is_empty() && !active {
                self.held = Some(failed);
            }
        }
        if let Some(held) = &mut self.held {
            // 继续了（又开始新的一轮时已经放下）、移除了的不再算
            held.retain(|id| failed_now.contains(id.as_str()));
            if held.is_empty() {
                self.held = None;
            }
        }
        match self.held {
            Some(_) => Indicator::Error(1000),
            None => Indicator::None,
        }
    }

    /// 窗口被激活：放下保持着的错误状态，返回是否放下了。
    pub(crate) fn acknowledge(&mut self) -> bool {
        self.held.take().is_some()
    }

    /// 进行中的一轮：按字节合计；有失败的为错误，未结束的都在等待重连时为暂停，还有排队、传输中的不知道总量
    /// （目录还在统计）时为不确定。
    fn round_indicator(&self) -> Indicator {
        let (mut done, mut total) = (0u64, 0u64);
        let (mut failed, mut moving, mut unknown) = (false, false, false);
        for seen in self.round.values() {
            match seen.state {
                TransferState::Canceled => continue,
                TransferState::Failed => failed = true,
                TransferState::Queued | TransferState::Running => {
                    moving = true;
                    unknown |= seen.total == 0;
                }
                TransferState::WaitingReconnect | TransferState::Done => {}
            }
            let size = seen.total.max(seen.done);
            total = total.saturating_add(size);
            done = done.saturating_add(if seen.state == TransferState::Done {
                size
            } else {
                seen.done
            });
        }
        let permille = (total > 0).then(|| (u128::from(done) * 1000 / u128::from(total)) as u16);
        if failed {
            Indicator::Error(permille.unwrap_or(0))
        } else if !moving {
            Indicator::Paused(permille.unwrap_or(0))
        } else if unknown {
            Indicator::Indeterminate
        } else {
            permille.map_or(Indicator::Indeterminate, Indicator::Normal)
        }
    }
}

/// 一个窗口（或整个应用）的进度与已经显示的状态（`None` 为不知道，下次一定重新显示）。
#[derive(Default)]
struct Slot {
    tracker: Tracker,
    shown: Option<Indicator>,
}

/// 任务栏的状态（全局量，[`install`] 建立）。
struct Taskbar {
    host: Box<dyn TaskbarHost>,
    /// 分窗口时按窗口（键为窗口的标识，见 `window_owner`）；整个应用一个时键为空串
    slots: HashMap<String, Slot>,
    /// 最近一次的传输列表
    last: Vec<Progress>,
}

impl Global for Taskbar {}

/// 装上（应用状态之后、打开主窗口之前调用）；不支持的系统上什么也不做。
pub(crate) fn install(cx: &mut App) {
    if !supported(cx.platform_info().os) {
        return;
    }
    match native_host() {
        Some(host) => install_with(host, cx),
        None => tracing::warn!("任务栏进度不可用：建立任务栏的接口失败"),
    }
}

#[cfg(windows)]
fn native_host() -> Option<Box<dyn TaskbarHost>> {
    win32::Win32Taskbar::new().map(|host| Box::new(host) as Box<dyn TaskbarHost>)
}

#[cfg(target_os = "macos")]
fn native_host() -> Option<Box<dyn TaskbarHost>> {
    Some(Box::new(macos::DockBadge))
}

#[cfg(target_os = "linux")]
fn native_host() -> Option<Box<dyn TaskbarHost>> {
    Some(Box::new(unity::UnityLauncher::default()))
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn native_host() -> Option<Box<dyn TaskbarHost>> {
    None
}

/// 用给定的实现装上：传输列表变化、开关变化、窗口激活与关闭时更新。
pub(crate) fn install_with(host: Box<dyn TaskbarHost>, cx: &mut App) {
    cx.set_global(Taskbar {
        host,
        slots: HashMap::new(),
        last: Vec::new(),
    });
    let mut changes = AppState::sftp(cx).watch_transfers();
    cx.spawn(async move |cx| {
        loop {
            let snapshot: Vec<Progress> = changes
                .borrow_and_update()
                .iter()
                .map(Progress::from)
                .collect();
            cx.update(|cx| {
                if cx.has_global::<Taskbar>() {
                    cx.global_mut::<Taskbar>().last = snapshot;
                    sync(cx);
                }
            });
            if changes.changed().await.is_err() {
                return;
            }
        }
    })
    .detach();
    cx.observe_new::<Workspace>(|_, window, cx| {
        if let Some(window) = window {
            cx.observe_window_activation(window, |_, window, cx| {
                if window.is_window_active() {
                    activated(window, cx);
                }
            })
            .detach();
        }
        cx.defer(sync);
    })
    .detach();
    cx.on_window_closed(|cx, _| sync(cx)).detach();
    let store = AppState::global(cx).store.clone();
    cx.subscribe(&store, |_, event: &StoreEvent, cx| {
        if matches!(event, StoreEvent::LocalSettingsChanged) {
            sync(cx);
        }
    })
    .detach();
    cx.on_app_quit(|cx| {
        if cx.has_global::<Taskbar>() && !cx.global::<Taskbar>().host.per_window() {
            // 程序坞的标记在退出后不会自己消失（Linux 上发信号的连接随退出断开，桌面一般随之清掉）
            cx.global_mut::<Taskbar>().host.show(None, Indicator::None);
        }
        async {}
    })
    .detach();
    sync(cx);
}

/// 开关开着。
fn enabled(cx: &App) -> bool {
    cx.try_global::<AppState>()
        .is_some_and(|_| AppState::store(cx).local_settings().window.taskbar_progress)
}

/// 窗口被激活：放下它（整个应用一个时为应用）保持着的错误状态，并重新显示一次（任务栏按钮可能重建过：从托盘恢复、
/// 资源管理器重启）。
fn activated(window: &Window, cx: &mut App) {
    if !cx.has_global::<Taskbar>() {
        return;
    }
    let taskbar = cx.global_mut::<Taskbar>();
    let key = if taskbar.host.per_window() {
        window_owner(window)
    } else {
        String::new()
    };
    if let Some(slot) = taskbar.slots.get_mut(&key) {
        if slot.tracker.acknowledge() {
            tracing::debug!("窗口激活，清除任务栏上的传输失败状态");
        }
        slot.shown = None;
    }
    // 激活的回调在窗口的更新中，那时取不到这个窗口：等这次更新结束
    cx.defer(sync);
}

/// 打开着的主窗口：窗口、它的标识、是否在前台。
fn main_windows(cx: &mut App) -> Vec<(AnyWindowHandle, String, bool)> {
    cx.windows()
        .into_iter()
        .filter_map(|handle| {
            handle
                .update(cx, |_, window, cx| {
                    Workspace::in_window(window, cx)
                        .map(|_| (handle, window_owner(window), window.is_window_active()))
                })
                .ok()
                .flatten()
        })
        .collect()
}

/// 按最近的传输列表与开关更新任务栏。
fn sync(cx: &mut App) {
    if !cx.has_global::<Taskbar>() {
        return;
    }
    let enabled = enabled(cx);
    let windows = main_windows(cx);
    let taskbar = cx.global_mut::<Taskbar>();
    let snapshot = taskbar.last.clone();
    if !taskbar.host.per_window() {
        let active = windows.iter().any(|(_, _, active)| *active);
        let slot = taskbar.slots.entry(String::new()).or_default();
        let indicator = if enabled {
            slot.tracker.update(snapshot.iter(), active)
        } else {
            slot.tracker = Tracker::default();
            Indicator::None
        };
        if slot.shown != Some(indicator) {
            slot.shown = Some(indicator);
            taskbar.host.show(None, indicator);
        }
        return;
    }
    taskbar
        .slots
        .retain(|owner, _| windows.iter().any(|(_, o, _)| o == owner));
    let mut changed: Vec<(AnyWindowHandle, Indicator)> = Vec::new();
    for (handle, owner, active) in &windows {
        let slot = taskbar.slots.entry(owner.clone()).or_default();
        let indicator = if enabled {
            slot.tracker
                .update(snapshot.iter().filter(|t| &t.owner == owner), *active)
        } else {
            slot.tracker = Tracker::default();
            Indicator::None
        };
        if slot.shown != Some(indicator) {
            slot.shown = Some(indicator);
            changed.push((*handle, indicator));
        }
    }
    for (handle, indicator) in changed {
        let _ = handle.update(cx, |_, window, cx| {
            cx.global_mut::<Taskbar>()
                .host
                .show(Some(window), indicator);
        });
    }
}
