//! 只与这台电脑有关的界面状态（`{数据目录}/ui-state.json`）：分组的展开与折叠、侧边栏的宽度与显示等。
//!
//! 不进数据库、不同步、不进备份：丢了也只是界面回到默认样子。读不了（不存在、内容坏了）时用默认值；
//! 不认识的字段原样保留，新旧版本交替运行不会互相抹掉。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 界面状态。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiState {
    /// 侧边栏主机树中展开着的分组（分组 ID）与节（`section:<名称>`）；没有记的折叠着（默认全部折叠，新建的、
    /// 同步过来的分组也是）
    pub expanded_groups: BTreeSet<String>,
    /// 快捷命令页中折叠着的分组（`snippet-group:<名称>`）；没有记的展开着
    pub collapsed_groups: BTreeSet<String>,
    /// 侧边栏的宽度（与界面缩放无关的逻辑像素）；`None` 用默认宽度
    pub sidebar_width: Option<f32>,
    /// 侧边栏已隐藏（Ctrl+Shift+B）
    pub sidebar_hidden: bool,
    /// 竖排的 Tab 列表的宽度（100% 界面大小时的逻辑像素，用时乘上界面的缩放倍数）；`None` 用默认宽度
    pub tab_list_width: Option<f32>,
    /// 竖排的 Tab 列表收成了只显示图标的窄栏
    pub tab_list_collapsed: bool,
    /// 竖排的 Tab 列表按主机分组时折叠着的主机（保存的主机按 ID，其余按连接的目标；与 [`HostPage::host`] 一样按主机
    /// 记，最近折叠的在前，最多 [`TAB_LIST_HOSTS`] 台）；没有记的展开着
    pub collapsed_tab_hosts: Vec<String>,
    /// SFTP：够宽的窗格中也不显示本机文件（只有远程一栏）
    pub sftp_local_hidden: bool,
    /// SFTP：本机栏占的宽度比例（0.2–0.8）；`None` 为一半
    pub sftp_local_ratio: Option<f32>,
    /// SFTP：本机栏上次所在的目录
    pub sftp_local_dir: Option<PathBuf>,
    /// SFTP：上次下载到的目录（没有显示本机栏时选择的）
    pub sftp_download_dir: Option<PathBuf>,
    /// SFTP：远程栏不显示隐藏的文件（以 `.` 开头的）；默认显示
    pub sftp_remote_hide_hidden: bool,
    /// SFTP：本机栏不显示隐藏的文件（Windows 的隐藏属性；其他系统是以 `.` 开头的）；默认显示
    pub sftp_local_hide_hidden: bool,
    /// SFTP：文件列表显示哪些次要的列（远程栏、本机栏共用）；默认都显示，宽度不够时按权限、大小、修改时间的顺序让位
    pub sftp_columns: SftpColumns,
    /// 终端中的文件传输（sz、tsz）上次保存到的目录
    pub terminal_transfer_dir: Option<PathBuf>,
    /// 终端区域下方显示快捷命令栏
    pub snippet_bar_open: bool,
    /// 快捷命令栏显示的分组（空串为未分组）；`None` 为全部
    pub snippet_bar_group: Option<String>,
    /// 主机工具面板打开着
    pub host_tools_open: bool,
    /// 主机工具面板的宽度（逻辑像素）；`None` 用默认宽度
    pub host_tools_width: Option<f32>,
    /// 主机工具面板上次的页（`overview`、`processes`、`services`、`docker`、`network`）：跟随到没有记下的主机时用它
    pub host_tools_page: Option<String>,
    /// 主机工具面板在各台主机上次看的页（最近的在前，最多 [`HOST_TOOLS_HOSTS`] 台）：面板跟随到某台主机时打开它
    pub host_tools_host_pages: Vec<HostPage>,
    /// 主机工具面板中详情区的高度（100% 界面大小时的逻辑像素，用时乘上界面的缩放倍数；各页共用）；`None` 按内容的高度
    pub host_tools_detail_height: Option<f32>,
    /// 最近打开的主机（主机 ID，最近的在前，最多 [`RECENT_HOSTS`] 个）：主机工具面板没有终端时列出
    pub recent_hosts: Vec<String>,
    /// 主窗口上次的位置与大小（最后关闭的那个窗口）；`None` 时按默认大小放在主屏中间
    pub main_window: Option<WindowPlacement>,
    /// 主机概览的小窗（主机工具的概览弹出成的独立窗口）在各台主机上的位置、大小与是否置顶（最近用的在前，最多
    /// [`OVERVIEW_WINDOWS`] 台）
    pub overview_windows: Vec<OverviewWindow>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 记下的最近打开的主机数。
pub const RECENT_HOSTS: usize = 5;

/// 主机工具面板记下各自上次的页的主机数。
pub const HOST_TOOLS_HOSTS: usize = 100;

/// 记下主机概览小窗的主机数。
pub const OVERVIEW_WINDOWS: usize = 50;

/// 记下竖排 Tab 列表中折叠着的主机数。
pub const TAB_LIST_HOSTS: usize = 100;

/// 一台主机的概览小窗：位置与大小（没记下时按默认的放），是否置顶（默认置顶）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OverviewWindow {
    /// 主机：与 [`HostPage::host`] 相同的键
    pub host: String,
    pub placement: Option<WindowPlacement>,
    /// 始终在其他窗口之上
    pub pinned: bool,
}

impl Default for OverviewWindow {
    fn default() -> Self {
        Self {
            host: String::new(),
            placement: None,
            pinned: true,
        }
    }
}

/// 一台主机上次在主机工具面板中看的页。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HostPage {
    /// 主机：保存的主机的 ID，没有保存的为 `用户@地址` 等（见主机工具面板）
    pub host: String,
    pub page: String,
}

impl UiState {
    /// 记下刚打开的主机：移到最前面，超过 [`RECENT_HOSTS`] 个时丢掉最久的。
    pub fn note_recent_host(&mut self, id: &str) {
        self.recent_hosts.retain(|h| h != id);
        self.recent_hosts.insert(0, id.to_string());
        self.recent_hosts.truncate(RECENT_HOSTS);
    }

    /// 主机工具面板在 `host` 上次看的页；没有记下时为 `None`。
    pub fn host_tools_page_of(&self, host: &str) -> Option<&str> {
        self.host_tools_host_pages
            .iter()
            .find(|p| p.host == host)
            .map(|p| p.page.as_str())
    }

    /// 记下主机工具面板在 `host` 上看的页：移到最前面，超过 [`HOST_TOOLS_HOSTS`] 台时丢掉最久的。
    pub fn note_host_tools_page(&mut self, host: &str, page: &str) {
        self.host_tools_host_pages.retain(|p| p.host != host);
        self.host_tools_host_pages.insert(
            0,
            HostPage {
                host: host.to_string(),
                page: page.to_string(),
            },
        );
        self.host_tools_host_pages.truncate(HOST_TOOLS_HOSTS);
    }

    /// `host` 的概览小窗记下的位置与置顶；没有记下时为 `None`。
    pub fn overview_window(&self, host: &str) -> Option<&OverviewWindow> {
        self.overview_windows.iter().find(|w| w.host == host)
    }

    /// 记下一台主机的概览小窗：移到最前面，超过 [`OVERVIEW_WINDOWS`] 台时丢掉最久的。
    pub fn note_overview_window(&mut self, window: OverviewWindow) {
        self.overview_windows.retain(|w| w.host != window.host);
        self.overview_windows.insert(0, window);
        self.overview_windows.truncate(OVERVIEW_WINDOWS);
    }

    /// 竖排的 Tab 列表中 `host` 的分组折叠着。
    pub fn tab_host_collapsed(&self, host: &str) -> bool {
        self.collapsed_tab_hosts.iter().any(|h| h == host)
    }

    /// 折叠（移到最前面，超过 [`TAB_LIST_HOSTS`] 台时丢掉最久的）或展开 `host` 的分组。
    pub fn set_tab_host_collapsed(&mut self, host: &str, collapsed: bool) {
        self.collapsed_tab_hosts.retain(|h| h != host);
        if collapsed {
            self.collapsed_tab_hosts.insert(0, host.to_string());
            self.collapsed_tab_hosts.truncate(TAB_LIST_HOSTS);
        }
    }
}

/// SFTP 文件列表的次要列是否显示（用户在表头的菜单中选）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SftpColumns {
    pub size: bool,
    pub modified: bool,
    /// 权限（只有远程栏与 Linux、macOS 的本机栏有）
    pub mode: bool,
}

impl Default for SftpColumns {
    fn default() -> Self {
        Self {
            size: true,
            modified: true,
            mode: true,
        }
    }
}

/// 窗口的位置与大小：逻辑像素，坐标是所在显示器的（Windows 上各显示器按自己的缩放换算）。最大化（全屏也按最大化
/// 记）时是还原后的位置与大小。字段缺了或不对时恢复的一方按默认值处理，不影响别的界面状态。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WindowPlacement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
    /// 所在的显示器（GPUI 的显示器 UUID）；找不到这块屏幕时放到主屏
    pub display: Option<String>,
}

/// 界面状态文件。
#[derive(Clone, Debug)]
pub struct UiStateFile {
    path: PathBuf,
}

impl UiStateFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取；文件不存在或内容不对时返回默认值（内容不对时记一条警告）。
    pub fn load(&self) -> UiState {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %self.path.display(), "界面状态文件内容不对，使用默认值：{e}");
                UiState::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => UiState::default(),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), "读取界面状态失败，使用默认值：{e}");
                UiState::default()
            }
        }
    }

    /// 保存（先写临时文件再替换，不会留下写了一半的文件）。
    pub fn save(&self, state: &UiState) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
        crate::platform::write_private_file(&self.path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let file = UiStateFile::new(dir.path().join("ui-state.json"));
        assert_eq!(file.load(), UiState::default(), "没有文件时用默认值");

        std::fs::write(
            file.path(),
            br#"{"expandedGroups":["g_1"],"futureField":{"a":1}}"#,
        )
        .unwrap();
        let mut state = file.load();
        assert!(state.expanded_groups.contains("g_1"));
        state.sidebar_width = Some(280.0);
        file.save(&state).unwrap();

        let again = file.load();
        assert_eq!(again.sidebar_width, Some(280.0));
        assert!(again.expanded_groups.contains("g_1"));
        assert_eq!(
            again.extra.get("futureField"),
            Some(&serde_json::json!({"a": 1}))
        );
    }

    // 最近打开的主机：最近的在前，重复的移到最前面，最多 5 个；以前没有这一项的文件照样读
    #[test]
    fn recent_hosts_keep_the_latest_five() {
        let mut state: UiState = serde_json::from_str(r#"{"sidebarWidth":300.0}"#).unwrap();
        assert!(state.recent_hosts.is_empty());
        for id in ["a", "b", "c", "d", "e", "f", "c"] {
            state.note_recent_host(id);
        }
        assert_eq!(state.recent_hosts, ["c", "f", "e", "d", "b"]);
        let text = serde_json::to_string(&state).unwrap();
        assert!(text.contains("\"recentHosts\""), "{text}");
    }

    // 主窗口的位置：camelCase；以前没有这一项的文件照样读；缺字段的按默认值，不连累别的界面状态
    #[test]
    fn main_window_placement_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = UiStateFile::new(dir.path().join("ui-state.json"));
        std::fs::write(file.path(), br#"{"sidebarWidth":300.0}"#).unwrap();
        let mut state = file.load();
        assert_eq!(state.main_window, None, "以前的文件没有这一项");
        assert_eq!(state.sidebar_width, Some(300.0));

        state.main_window = Some(WindowPlacement {
            x: -1200.0,
            y: 40.5,
            width: 1100.0,
            height: 700.0,
            maximized: true,
            display: Some("8d1f0b3c-0000-4000-8000-000000000001".into()),
        });
        file.save(&state).unwrap();
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.contains("\"mainWindow\"") && text.contains("\"maximized\": true"));
        assert_eq!(file.load().main_window, state.main_window);

        std::fs::write(
            file.path(),
            br#"{"sidebarWidth":300.0,"mainWindow":{"x":10,"width":"wide"}}"#,
        )
        .unwrap();
        let broken = file.load();
        assert_eq!(
            broken,
            UiState::default(),
            "类型不对时整个文件按默认值（现有的读取规则）"
        );
        std::fs::write(
            file.path(),
            br#"{"sidebarWidth":300.0,"mainWindow":{"x":10}}"#,
        )
        .unwrap();
        let partial = file.load();
        assert_eq!(partial.sidebar_width, Some(300.0));
        assert_eq!(
            partial.main_window,
            Some(WindowPlacement {
                x: 10.0,
                ..Default::default()
            }),
            "缺的字段按默认值（宽高为 0，恢复时当作没有记录）"
        );
    }

    // SFTP 的列：默认都显示；只记了一部分的按默认补齐；camelCase
    #[test]
    fn sftp_columns_default_to_all_and_round_trip() {
        assert_eq!(
            UiState::default().sftp_columns,
            SftpColumns {
                size: true,
                modified: true,
                mode: true
            }
        );
        let dir = tempfile::tempdir().unwrap();
        let file = UiStateFile::new(dir.path().join("ui-state.json"));
        std::fs::write(file.path(), br#"{"sftpColumns":{"mode":false}}"#).unwrap();
        let mut state = file.load();
        assert_eq!(
            state.sftp_columns,
            SftpColumns {
                size: true,
                modified: true,
                mode: false
            }
        );
        state.sftp_columns.size = false;
        file.save(&state).unwrap();
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.contains("\"sftpColumns\""), "{text}");
        assert_eq!(file.load().sftp_columns, state.sftp_columns);
    }

    /// 主机工具在各台主机上次看的页：再记一次移到最前面，超过上限时丢掉最久的；能存下、读回。
    #[test]
    fn host_tools_pages_per_host() {
        let mut state = UiState::default();
        assert_eq!(state.host_tools_page_of("h_1"), None);
        state.note_host_tools_page("h_1", "docker");
        state.note_host_tools_page("h_2", "services");
        state.note_host_tools_page("h_1", "network");
        assert_eq!(state.host_tools_page_of("h_1"), Some("network"));
        assert_eq!(state.host_tools_host_pages[0].host, "h_1");
        assert_eq!(state.host_tools_host_pages.len(), 2);
        for i in 0..HOST_TOOLS_HOSTS {
            state.note_host_tools_page(&format!("x{i}"), "overview");
        }
        assert_eq!(state.host_tools_host_pages.len(), HOST_TOOLS_HOSTS);
        assert_eq!(state.host_tools_page_of("h_2"), None, "最久的丢掉");

        let dir = tempfile::tempdir().unwrap();
        let file = UiStateFile::new(dir.path().join("ui-state.json"));
        file.save(&state).unwrap();
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.contains("\"hostToolsHostPages\""), "{text}");
        assert_eq!(
            file.load().host_tools_host_pages,
            state.host_tools_host_pages
        );
    }

    /// 概览小窗按主机记下位置与置顶：再记一次移到最前面，超过上限时丢掉最久的；没写置顶的按置顶读；能存下、读回。
    #[test]
    fn overview_windows_per_host() {
        let mut state = UiState::default();
        assert_eq!(state.overview_window("h_1"), None);
        let placed = |host: &str, pinned: bool| OverviewWindow {
            host: host.into(),
            placement: Some(WindowPlacement {
                x: 10.,
                y: 20.,
                width: 300.,
                height: 400.,
                ..Default::default()
            }),
            pinned,
        };
        state.note_overview_window(placed("h_1", true));
        state.note_overview_window(placed("h_2", true));
        state.note_overview_window(placed("h_1", false));
        assert!(!state.overview_window("h_1").unwrap().pinned);
        assert_eq!(state.overview_windows[0].host, "h_1");
        assert_eq!(state.overview_windows.len(), 2);
        for i in 0..OVERVIEW_WINDOWS {
            state.note_overview_window(placed(&format!("x{i}"), true));
        }
        assert_eq!(state.overview_windows.len(), OVERVIEW_WINDOWS);
        assert_eq!(state.overview_window("h_2"), None, "最久的丢掉");

        let dir = tempfile::tempdir().unwrap();
        let file = UiStateFile::new(dir.path().join("ui-state.json"));
        file.save(&state).unwrap();
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.contains("\"overviewWindows\""), "{text}");
        assert_eq!(file.load().overview_windows, state.overview_windows);

        std::fs::write(
            file.path(),
            br#"{"overviewWindows":[{"host":"h_9","placement":{"width":320,"height":420}}]}"#,
        )
        .unwrap();
        let read = file.load();
        let window = read.overview_window("h_9").unwrap();
        assert!(window.pinned, "没写置顶时默认置顶");
        assert_eq!(window.placement.as_ref().map(|p| p.width), Some(320.));
    }

    /// 竖排 Tab 列表中折叠的主机：折叠、展开，最近折叠的在前，超过上限时丢掉最久的；能存下、读回，以前的文件照样读。
    #[test]
    fn collapsed_tab_hosts() {
        let mut state: UiState = serde_json::from_str(r#"{"tabListCollapsed":true}"#).unwrap();
        assert!(!state.tab_host_collapsed("h_1"));
        state.set_tab_host_collapsed("h_1", true);
        state.set_tab_host_collapsed("quick:ops@10.0.0.2:22", true);
        state.set_tab_host_collapsed("h_1", true);
        assert_eq!(state.collapsed_tab_hosts, ["h_1", "quick:ops@10.0.0.2:22"]);
        state.set_tab_host_collapsed("h_1", false);
        assert!(!state.tab_host_collapsed("h_1"));
        assert!(state.tab_host_collapsed("quick:ops@10.0.0.2:22"));
        for i in 0..TAB_LIST_HOSTS {
            state.set_tab_host_collapsed(&format!("x{i}"), true);
        }
        assert_eq!(state.collapsed_tab_hosts.len(), TAB_LIST_HOSTS);
        assert!(
            !state.tab_host_collapsed("quick:ops@10.0.0.2:22"),
            "最久的丢掉"
        );

        let dir = tempfile::tempdir().unwrap();
        let file = UiStateFile::new(dir.path().join("ui-state.json"));
        file.save(&state).unwrap();
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.contains("\"collapsedTabHosts\""), "{text}");
        assert_eq!(file.load().collapsed_tab_hosts, state.collapsed_tab_hosts);
    }

    #[test]
    fn corrupt_file_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let file = UiStateFile::new(dir.path().join("ui-state.json"));
        std::fs::write(file.path(), b"{not json").unwrap();
        assert_eq!(file.load(), UiState::default());
    }
}
