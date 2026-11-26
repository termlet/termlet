//! 使用统计中的功能名单（正文的 `features`）：上次发送成功以来，这台电脑上的 Termlet 用到过哪些功能。
//!
//! - **只有名称**：固定的清单（[`Feature`]），没有次数、时间与先后；正文中按清单的顺序排列。清单与接收端
//!   （termlet-api 的 `src/stats/features.ts`）一致，名称发布后不改，删掉的项不复用。
//! - **为什么是“上次发送以来”**：每天的统计在当天第一次用到 Termlet 时就发出，那时当天几乎什么都还没用；所以名单留到
//!   下一次发送时带上，发送成功后去掉已发出的（[`FeatureLog::sent`]）。
//! - **怎么记**：各功能的判定点调用 [`super::mark`]（全局的 [`FeatureLog`]），只是在 64 位的位图上置一位；某一项第一次
//!   记下时在后台把名单写进 `{数据目录}/usage-features.json`（[`crate::paths::DataDir::usage_features`]），重启后接着用。
//! - **什么时候记**：启动后、界面读到设置之前先只记在内存中（[`Collecting::Undecided`]）；设置开着、没有被环境变量关掉、
//!   不是调试构建时才写文件（[`Collecting::On`]）；关掉时（[`Collecting::Off`]）清空位图、删除文件，之后不再记。

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use crate::import::Source;

/// 名单中的一项。顺序就是正文中的顺序与位图中的位置（`self as u8`），只能在末尾追加。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Feature {
    // 连接与会话
    /// SSH 终端连上过（保存的主机、快速连接、被堡垒机调起的都算）
    Ssh,
    Telnet,
    Serial,
    /// 本地终端（不含 WSL）
    LocalShell,
    /// WSL 终端或 WSL 的文件管理
    Wsl,
    /// 开着 X11 转发的 SSH 会话连上过
    X11,
    /// 被堡垒机等外部程序调起并打开过连接
    Bastion,
    // 终端
    Split,
    SyncInput,
    /// 撰写栏发送过，或用过“执行并对比”
    Compose,
    /// 发送或写入过快捷命令（选择器、快捷命令栏）
    QuickCommands,
    CommandSuggest,
    SessionLog,
    /// 录制过会话，或打开过回放
    Recording,
    // 外观
    /// Tab 放在左侧（按设置记）
    VerticalTabs,
    HighlightPresets,
    SchemeImport,
    // 文件与转发
    /// 远程的文件管理连上过（SFTP 或 SCP；WSL 的记为 [`Feature::Wsl`]）
    Sftp,
    OpenInEditor,
    PortForward,
    /// 把本机代理借给服务器
    ProxyShare,
    // 主机工具
    HostTools,
    HostToolsPopout,
    /// 在主机工具中执行过写操作（审计来源为“主机工具”）
    HostToolsAction,
    // AI 与命令行（与审计日志的来源一致）
    Cli,
    McpStdio,
    McpHttp,
    McpLan,
    /// 生成正文时这台电脑上装着 Termlet 的 Agent Skill（不记在位图中，见 [`super::ReporterOptions::skill_installed`]）
    AgentSkill,
    ApprovalPrompt,
    // 同步与备份
    Sync,
    SyncS3,
    SyncGist,
    /// 手动创建或恢复过完整备份
    Backup,
    BackupScheduled,
    // 安全
    AppLock,
    MasterPassword,
    // 导入（每个来源一项）
    ImportSshConfig,
    ImportXshell,
    ImportFinalShell,
    ImportMobaXterm,
    ImportTabby,
    ImportTermora,
    ImportWindTerm,
    ImportElecterm,
    ImportNetcatty,
    ImportShellRs,
    /// NyaTerm 的主机（`Source::NyaTermData`）
    ImportNyaTerm,
    /// NyaTerm 导出的快捷命令（`Source::NyaTerm`）
    ImportNyaTermSnippets,
    /// Termlet 主机文件（`.termlet-hosts`）
    ImportTermletFile,
    // 加在最后：位置就是本机记录中的位，发布后不能挪
    /// 批量执行（命令行、MCP 的 `run_command_on_hosts`、界面）
    BatchExec,
}

impl Feature {
    /// 全部，按顺序（`ALL[i] as usize == i`）。
    pub const ALL: [Feature; 51] = [
        Feature::Ssh,
        Feature::Telnet,
        Feature::Serial,
        Feature::LocalShell,
        Feature::Wsl,
        Feature::X11,
        Feature::Bastion,
        Feature::Split,
        Feature::SyncInput,
        Feature::Compose,
        Feature::QuickCommands,
        Feature::CommandSuggest,
        Feature::SessionLog,
        Feature::Recording,
        Feature::VerticalTabs,
        Feature::HighlightPresets,
        Feature::SchemeImport,
        Feature::Sftp,
        Feature::OpenInEditor,
        Feature::PortForward,
        Feature::ProxyShare,
        Feature::HostTools,
        Feature::HostToolsPopout,
        Feature::HostToolsAction,
        Feature::Cli,
        Feature::McpStdio,
        Feature::McpHttp,
        Feature::McpLan,
        Feature::AgentSkill,
        Feature::ApprovalPrompt,
        Feature::Sync,
        Feature::SyncS3,
        Feature::SyncGist,
        Feature::Backup,
        Feature::BackupScheduled,
        Feature::AppLock,
        Feature::MasterPassword,
        Feature::ImportSshConfig,
        Feature::ImportXshell,
        Feature::ImportFinalShell,
        Feature::ImportMobaXterm,
        Feature::ImportTabby,
        Feature::ImportTermora,
        Feature::ImportWindTerm,
        Feature::ImportElecterm,
        Feature::ImportNetcatty,
        Feature::ImportShellRs,
        Feature::ImportNyaTerm,
        Feature::ImportNyaTermSnippets,
        Feature::ImportTermletFile,
        Feature::BatchExec,
    ];

    /// 正文中的名称（与接收端的清单相同），发布后不能改。
    pub fn name(self) -> &'static str {
        match self {
            Feature::Ssh => "ssh",
            Feature::Telnet => "telnet",
            Feature::Serial => "serial",
            Feature::LocalShell => "local_shell",
            Feature::Wsl => "wsl",
            Feature::X11 => "x11",
            Feature::Bastion => "bastion",
            Feature::Split => "split",
            Feature::SyncInput => "sync_input",
            Feature::Compose => "compose",
            Feature::QuickCommands => "quick_commands",
            Feature::CommandSuggest => "command_suggest",
            Feature::SessionLog => "session_log",
            Feature::Recording => "recording",
            Feature::VerticalTabs => "vertical_tabs",
            Feature::HighlightPresets => "highlight_presets",
            Feature::SchemeImport => "scheme_import",
            Feature::Sftp => "sftp",
            Feature::OpenInEditor => "open_in_editor",
            Feature::PortForward => "port_forward",
            Feature::ProxyShare => "proxy_share",
            Feature::HostTools => "host_tools",
            Feature::HostToolsPopout => "host_tools_popout",
            Feature::HostToolsAction => "host_tools_action",
            Feature::Cli => "cli",
            Feature::McpStdio => "mcp_stdio",
            Feature::McpHttp => "mcp_http",
            Feature::McpLan => "mcp_lan",
            Feature::AgentSkill => "agent_skill",
            Feature::ApprovalPrompt => "approval_prompt",
            Feature::Sync => "sync",
            Feature::SyncS3 => "sync_s3",
            Feature::SyncGist => "sync_gist",
            Feature::Backup => "backup",
            Feature::BackupScheduled => "backup_scheduled",
            Feature::AppLock => "app_lock",
            Feature::MasterPassword => "master_password",
            Feature::ImportSshConfig => "import_ssh_config",
            Feature::ImportXshell => "import_xshell",
            Feature::ImportFinalShell => "import_finalshell",
            Feature::ImportMobaXterm => "import_mobaxterm",
            Feature::ImportTabby => "import_tabby",
            Feature::ImportTermora => "import_termora",
            Feature::ImportWindTerm => "import_windterm",
            Feature::ImportElecterm => "import_electerm",
            Feature::ImportNetcatty => "import_netcatty",
            Feature::ImportShellRs => "import_shellrs",
            Feature::ImportNyaTerm => "import_nyaterm",
            Feature::ImportNyaTermSnippets => "import_nyaterm_snippets",
            Feature::ImportTermletFile => "import_termlet_file",
            Feature::BatchExec => "batch_exec",
        }
    }

    /// 从某个来源导入；还没有对应项的来源为 `None`（不记）。
    pub fn import(source: Source) -> Option<Feature> {
        Some(match source {
            Source::SshConfig => Feature::ImportSshConfig,
            Source::Xshell => Feature::ImportXshell,
            Source::FinalShell => Feature::ImportFinalShell,
            Source::MobaXterm => Feature::ImportMobaXterm,
            Source::Tabby => Feature::ImportTabby,
            Source::Termora => Feature::ImportTermora,
            Source::WindTerm => Feature::ImportWindTerm,
            Source::Electerm => Feature::ImportElecterm,
            Source::Netcatty => Feature::ImportNetcatty,
            Source::ShellRs => Feature::ImportShellRs,
            Source::NyaTermData => Feature::ImportNyaTerm,
            Source::NyaTerm => Feature::ImportNyaTermSnippets,
            // 新的名称要先加进接收端的白名单（未知的名称整份统计都会被拒），之后再加
            Source::TermletSnippets => return None,
        })
    }

    /// 位图中的这一位。
    pub fn bit(self) -> u64 {
        1 << (self as u8)
    }

    fn by_name(name: &str) -> Option<Feature> {
        Feature::ALL.into_iter().find(|f| f.name() == name)
    }
}

/// 位图中的各项，按清单的顺序。
pub fn names(bits: u64) -> Vec<&'static str> {
    Feature::ALL
        .into_iter()
        .filter(|f| bits & f.bit() != 0)
        .map(Feature::name)
        .collect()
}

/// 记不记。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Collecting {
    /// 启动后、界面读到设置之前：只记在内存中，不写文件
    Undecided,
    /// 记下并写文件
    On,
    /// 不记（关掉了使用统计、被环境变量关掉、调试构建）
    Off,
}

/// `usage-features.json` 的格式版本。
const FILE_FORMAT: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Stored {
    v: u32,
    features: Vec<String>,
}

/// 上次发送以来用到过的功能（位图）与本机的名单文件。全局一份（[`global`]），测试另建。
pub struct FeatureLog {
    bits: AtomicU64,
    collecting: AtomicU8,
    file: Mutex<Option<PathBuf>>,
    /// 写文件时持有：每次写的都是写的那一刻的位图，先后不要紧
    writing: Mutex<()>,
}

impl Default for FeatureLog {
    fn default() -> Self {
        Self {
            bits: AtomicU64::new(0),
            collecting: AtomicU8::new(Collecting::Undecided as u8),
            file: Mutex::new(None),
            writing: Mutex::new(()),
        }
    }
}

impl std::fmt::Debug for FeatureLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureLog")
            .field("features", &names(self.pending()))
            .field("collecting", &self.collecting())
            .finish_non_exhaustive()
    }
}

static GLOBAL: LazyLock<Arc<FeatureLog>> = LazyLock::new(Arc::default);

/// 全进程的名单（[`super::mark`] 记在这里）。
pub fn global() -> Arc<FeatureLog> {
    GLOBAL.clone()
}

impl FeatureLog {
    /// 名单写在 `path`：读出已有的（上次运行记下、还没发出的）并进位图。读不出、格式不对时当作没有。
    pub fn attach(&self, path: PathBuf) {
        if let Ok(text) = fs::read_to_string(&path)
            && let Ok(stored) = serde_json::from_str::<Stored>(&text)
        {
            let loaded = stored
                .features
                .iter()
                .filter_map(|name| Feature::by_name(name))
                .fold(0, |bits, f| bits | f.bit());
            self.bits.fetch_or(loaded, Ordering::SeqCst);
        }
        *self.file.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
    }

    pub fn collecting(&self) -> Collecting {
        match self.collecting.load(Ordering::SeqCst) {
            x if x == Collecting::On as u8 => Collecting::On,
            x if x == Collecting::Off as u8 => Collecting::Off,
            _ => Collecting::Undecided,
        }
    }

    /// 开始或停止收集：打开时把内存中已记的写进文件；关掉时清空位图、删除文件（[`FeatureLog::forget`]）。
    pub fn set_collecting(&self, on: bool) {
        let next = if on { Collecting::On } else { Collecting::Off };
        let before = self.collecting.swap(next as u8, Ordering::SeqCst);
        if on {
            if before != Collecting::On as u8 {
                self.persist();
            }
        } else {
            self.forget();
        }
    }

    /// 记下用到过 `feature`。不收集时什么都不做；这一项第一次记下、并且在收集时，在核心运行时的阻塞线程上把名单写进
    /// 文件（判定点可能在界面线程上）。
    pub fn mark(self: &Arc<Self>, feature: Feature) {
        if self.collecting() == Collecting::Off {
            return;
        }
        let bit = feature.bit();
        let before = self.bits.fetch_or(bit, Ordering::SeqCst);
        if before & bit == 0 && self.collecting() == Collecting::On && self.has_file() {
            let log = self.clone();
            drop(crate::runtime::runtime().spawn_blocking(move || log.persist()));
        }
    }

    /// 记下过 `feature` 没有（测试、界面用）。
    pub fn contains(&self, feature: Feature) -> bool {
        self.bits.load(Ordering::SeqCst) & feature.bit() != 0
    }

    /// 现在的位图（生成正文时取一份快照）。
    pub fn pending(&self) -> u64 {
        self.bits.load(Ordering::SeqCst)
    }

    /// `bits` 已经发送成功：从位图中去掉（发送期间新记的保留），重写文件（空了就删除）。
    pub fn sent(&self, bits: u64) {
        self.bits.fetch_and(!bits, Ordering::SeqCst);
        self.persist();
    }

    /// 清空位图、删除文件（关掉使用统计时）。
    pub fn forget(&self) {
        self.bits.store(0, Ordering::SeqCst);
        self.persist();
    }

    fn has_file(&self) -> bool {
        self.file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// 把现在的位图写进文件：收集中且不空时写，不收集或空了时删除；还没读到设置时不动文件（里面可能是上次运行时
    /// 还没发出的）。写不成只记日志（下一次照样带上内存中的）。
    pub(crate) fn persist(&self) {
        if self.collecting() == Collecting::Undecided {
            return;
        }
        let Some(path) = self.file.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return;
        };
        let _writing = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        let bits = self.bits.load(Ordering::SeqCst);
        let result = if bits == 0 || self.collecting() != Collecting::On {
            match fs::remove_file(&path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        } else {
            let stored = Stored {
                v: FILE_FORMAT,
                features: names(bits).into_iter().map(String::from).collect(),
            };
            let json = serde_json::to_string(&stored).unwrap_or_default();
            path.parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| crate::platform::write_private_file(&path, json.as_bytes()))
        };
        if let Err(e) = result {
            tracing::warn!("写入使用统计的功能名单（usage-features.json）失败：{e}");
        }
    }
}
