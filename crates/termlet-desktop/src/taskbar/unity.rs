//! Linux：经 D-Bus 发 Unity 的 LauncherEntry 信号（`com.canonical.Unity.LauncherEntry.Update`），桌面环境据此在
//! 程序的图标上显示进度：GNOME 的 Dash to Dock 与 Ubuntu Dock、KDE Plasma 的任务管理器、Plank 等都认。整个应用一个
//! （与 macOS 的程序坞相同）；图标按菜单项（`dev.termlet.Termlet.desktop`，与窗口的 app_id 相同）认出是 Termlet。
//!
//! - 发信号在单独的线程上（D-Bus 的连接是阻塞的）：第一次有进度要显示时才连接会话总线，连不上（没有会话总线、
//!   远程登录中运行等）时记一次日志，之后不再尝试；桌面不支持时没有谁收这个信号，同样没有效果。
//! - 连接一直保持到退出：有的桌面在发送方断开时清掉它的进度。
//! - 不确定的进度显示为 0；暂停照常显示进度；有失败的另设 `urgent`（桌面环境按自己的方式提醒，如图标闪动、标红）。

use super::Indicator;

/// LauncherEntry 的属性。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LauncherProps {
    /// 0–1
    pub progress: f64,
    pub progress_visible: bool,
    pub urgent: bool,
}

/// 任务栏的状态换成 LauncherEntry 的属性。
pub(crate) fn launcher_props(indicator: Indicator) -> LauncherProps {
    let ratio = |permille: u16| f64::from(permille.min(1000)) / 1000.;
    let (progress, progress_visible, urgent) = match indicator {
        Indicator::None => (0., false, false),
        Indicator::Normal(p) | Indicator::Paused(p) => (ratio(p), true, false),
        Indicator::Indeterminate => (0., true, false),
        Indicator::Error(p) => (ratio(p), true, true),
    };
    LauncherProps {
        progress,
        progress_visible,
        urgent,
    }
}

#[cfg(target_os = "linux")]
pub(super) use self::dbus::UnityLauncher;

#[cfg(target_os = "linux")]
mod dbus {
    use std::collections::HashMap;
    use std::sync::mpsc::{Receiver, Sender, channel};

    use gpui_kit::Window;
    use zbus::zvariant::Value;

    use super::{LauncherProps, launcher_props};
    use crate::taskbar::{Indicator, TaskbarHost};

    /// 信号所在的对象路径（随意，桌面只看接口与程序的地址）。
    const OBJECT_PATH: &str = "/dev/termlet/Termlet";
    const INTERFACE: &str = "com.canonical.Unity.LauncherEntry";

    /// 程序的地址：菜单项的文件名。
    fn app_uri() -> String {
        format!("application://{}.desktop", crate::APP_ID)
    }

    /// 经 D-Bus 显示进度（见模块说明）。
    #[derive(Default)]
    pub(crate) struct UnityLauncher {
        /// 发信号的线程（第一次要显示时启动）
        sender: Option<Sender<LauncherProps>>,
    }

    impl TaskbarHost for UnityLauncher {
        fn per_window(&self) -> bool {
            false
        }

        fn show(&mut self, _: Option<&Window>, indicator: Indicator) {
            let props = launcher_props(indicator);
            // 还没显示过时不用清除（不为此连接 D-Bus）
            if self.sender.is_none() && !props.progress_visible && !props.urgent {
                return;
            }
            let sender = self.sender.get_or_insert_with(spawn);
            let _ = sender.send(props);
        }
    }

    fn spawn() -> Sender<LauncherProps> {
        let (sender, receiver) = channel();
        if let Err(e) = std::thread::Builder::new()
            .name("taskbar-dbus".into())
            .spawn(move || run(receiver))
        {
            tracing::warn!("任务栏进度不可用：无法启动发 D-Bus 信号的线程：{e}");
        }
        sender
    }

    /// 连上会话总线，之后把收到的属性发出去（积压时只发最新的）。连不上时不再尝试。
    fn run(receiver: Receiver<LauncherProps>) {
        let connection = match zbus::blocking::Connection::session() {
            Ok(connection) => connection,
            Err(e) => {
                tracing::info!("任务栏进度不可用：连不上 D-Bus 会话总线：{e}");
                return;
            }
        };
        let uri = app_uri();
        while let Ok(mut props) = receiver.recv() {
            while let Ok(newer) = receiver.try_recv() {
                props = newer;
            }
            let properties: HashMap<&str, Value> = HashMap::from([
                ("progress", Value::from(props.progress)),
                ("progress-visible", Value::from(props.progress_visible)),
                ("urgent", Value::from(props.urgent)),
            ]);
            if let Err(e) = connection.emit_signal(
                None::<&str>,
                OBJECT_PATH,
                INTERFACE,
                "Update",
                &(uri.as_str(), properties),
            ) {
                tracing::debug!("发任务栏进度的 D-Bus 信号失败：{e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 进度为千分比换成 0–1；不确定的显示为 0；失败的另设 urgent；不显示时都关掉
    #[test]
    fn indicators_map_to_launcher_properties() {
        let props = |progress, progress_visible, urgent| LauncherProps {
            progress,
            progress_visible,
            urgent,
        };
        assert_eq!(launcher_props(Indicator::None), props(0., false, false));
        assert_eq!(
            launcher_props(Indicator::Normal(250)),
            props(0.25, true, false)
        );
        assert_eq!(
            launcher_props(Indicator::Paused(500)),
            props(0.5, true, false)
        );
        assert_eq!(
            launcher_props(Indicator::Indeterminate),
            props(0., true, false)
        );
        assert_eq!(
            launcher_props(Indicator::Error(1000)),
            props(1., true, true)
        );
        assert_eq!(launcher_props(Indicator::Normal(5000)).progress, 1.);
    }
}
