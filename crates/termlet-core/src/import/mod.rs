//! 从其他客户端导入主机：OpenSSH 的 `~/.ssh/config`、Xshell、FinalShell、MobaXterm、Tabby、Termora 2.x、WindTerm、
//! Electerm、Netcatty；以及 NyaTerm 导出的快捷命令（文件中只有快捷命令，没有主机）。
//!
//! 解析结果是与来源无关的导入计划（[`ImportPlan`]）：分组树、主机、主机之间的跳板引用、每台主机的提示、
//! 随主机导入的端口转发（Xshell）、快捷命令（WindTerm、NyaTerm）与转换时丢掉或改动的内容，以及（Termora、WindTerm 有的）登录密码、代理密码、
//! 私钥密码与要存进密钥库的私钥。计划只用自己内部的下标互相引用，写入存储时再换成真正的 ID。
//!
//! 一般只导入主机信息（名称、地址、端口、用户名、分组、私钥路径、代理），**不导入密码**：其他工具的密码
//! 以各自私有的方式加密。例外是 Termora 2.x 与 WindTerm：Termora 的数据只用保存在同一个文件中的密钥加密，
//! WindTerm 用主密码（没设时为空）派生的密钥加密，密码、私钥一并导入。Electerm 的书签导出、Netcatty 的 CSV 导出中
//! 密码是明文（Electerm 本机的旧数据是可逆的移位），只有用户明确同意后才导入（[`Source::secrets_need_consent`]，
//! 不同意时界面用 [`ImportPlan::without_secrets`]）；这两个来源重复导入时只给已有主机补上还没保存的密码，不覆盖。
//! 计划中的凭据只有核心能取出（`pub(crate)`），界面只看得到"带密码 / 带私钥"的标记。
//!
//! 读取文件、目录、注册表的部分与解析分开：各来源的解析函数接受文本（Termora 是解密后的记录），
//! 可以直接用字符串测试。
//!
//! # 用法（界面）
//!
//! 选来源（[`Source::default_path`] 给出默认位置）→ [`read`]（在阻塞线程上解析；WindTerm 设了主密码时返回
//! [`ImportError::WindTermPasswordRequired`]，再用 [`read_with_password`] 读）→ [`preview`] 列出主机并标出本机已有的
//! 相同主机 → 界面用 [`select`] 中的函数搜索、筛选、勾选（默认勾选新主机）→ [`fn@apply`] 导入勾选的主机，返回新增 / 合并 /
//! 跳过的数量与没能导入的项。计划由界面保存到导入为止（其中的凭据界面取不出）。Termlet 自己的主机文件见
//! [`crate::hostfile`]，预览、勾选与结果的类型相同。

mod apply;
// 主机文件（hostfile）的导入预览与客户端导入共用
pub(crate) use apply::{preview_forward, startup_line};
mod electerm;
mod files;
mod finalshell;
mod mobaxterm;
mod netcatty;
mod nyaterm;
mod plan;
pub mod select;
mod ssh_config;
mod sys;
mod tabby;
mod termora;
mod windterm;
mod xshell;

use std::path::{Path, PathBuf};

pub(crate) use apply::{
    ExistingHosts, GroupIndex, effective_username, has_credentials, move_into, one_line,
    proxy_command, set_jump, snapshot_before_import, write_secrets,
};
pub use apply::{
    FailureReason, FailureSubject, HostOutcome, ImportFailure, ImportPreview, ImportReport,
    ImportedHost, OnDuplicate, PreviewForward, PreviewItem, apply, preview,
};
pub use mobaxterm::MOBAXTERM_REGISTRY_PATH;
pub use plan::{
    Endpoint, Hint, ImportPlan, JumpRef, KeyAlgorithm, PlannedGroup, PlannedHost, PlannedKey,
    SnippetNotes,
};

use files::LocalEnv;

/// 系统 ANSI 代码页的编码（只有 Windows；其他系统为 `None`）：解码本机程序的输出（如代理命令的标准错误）用。
pub(crate) fn ansi_encoding() -> Option<&'static encoding_rs::Encoding> {
    sys::ansi_code_page().map(files::encoding_for_code_page)
}

use crate::secret::SecretString;

/// 导入来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// OpenSSH 的 config 文件
    SshConfig,
    /// Xshell 的会话目录（Sessions）
    Xshell,
    /// FinalShell 的连接配置目录（conn）
    FinalShell,
    /// MobaXterm.ini、导出的 `.mxtsessions`，或安装版保存在注册表中的书签
    MobaXterm,
    /// Tabby（原 Terminus）的 config.yaml
    Tabby,
    /// Termora 2.x 的数据目录（`~/.termora`）
    Termora,
    /// WindTerm 的程序目录或数据目录（`.wind`）
    WindTerm,
    /// Electerm 的书签导出（JSON），或本机的数据目录（`users/default_user`，3.0 起书签用系统加密，读不了）
    Electerm,
    /// Netcatty 在主机页导出的 CSV
    Netcatty,
    /// NyaTerm 在快捷命令面板导出的 JSON（`nyaterm-quick-commands.json`）：只有快捷命令
    NyaTerm,
}

impl Source {
    pub const ALL: [Source; 10] = [
        Source::SshConfig,
        Source::Xshell,
        Source::FinalShell,
        Source::MobaXterm,
        Source::Tabby,
        Source::Termora,
        Source::WindTerm,
        Source::Electerm,
        Source::Netcatty,
        Source::NyaTerm,
    ];

    /// 稳定的名称，日志、界面状态中使用。
    pub fn name(self) -> &'static str {
        match self {
            Source::SshConfig => "ssh_config",
            Source::Xshell => "xshell",
            Source::FinalShell => "finalshell",
            Source::MobaXterm => "mobaxterm",
            Source::Tabby => "tabby",
            Source::Termora => "termora",
            Source::WindTerm => "windterm",
            Source::Electerm => "electerm",
            Source::Netcatty => "netcatty",
            Source::NyaTerm => "nyaterm",
        }
    }

    /// 来源中只有快捷命令，没有主机（NyaTerm）。
    pub fn snippets_only(self) -> bool {
        self == Source::NyaTerm
    }

    /// 来源中的密码是明文（或可逆）保存的，导入前要用户明确同意（界面上的“导入保存的密码”，默认不勾选）；
    /// 重复导入时只给已有主机补上还没保存的密码，不覆盖（见 [`fn@apply`]）。
    pub fn secrets_need_consent(self) -> bool {
        matches!(self, Source::Electerm | Source::Netcatty)
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// 来源在本机上的默认位置；找不到时为 `None`，由用户手动选择。
    /// MobaXterm 的书签只在注册表中时返回 [`MOBAXTERM_REGISTRY_PATH`]。
    pub fn default_path(self) -> Option<PathBuf> {
        let env = LocalEnv::detect();
        match self {
            Source::SshConfig => first_existing(ssh_config::default_candidates(&env)),
            Source::Xshell => first_existing(xshell::default_candidates(
                &env,
                sys::xshell_registry_data_path,
            )),
            Source::FinalShell => first_existing(finalshell::default_candidates(&env)),
            Source::MobaXterm => {
                mobaxterm::default_path(&env, sys::mobaxterm_registry_has_bookmarks)
            }
            Source::Tabby => first_existing(tabby::default_candidates(&env)),
            Source::Termora => termora::default_path(&env),
            Source::WindTerm => windterm::default_path(&env),
            Source::Electerm => electerm::default_path(&env),
            Source::Netcatty => netcatty::default_path(&env),
            Source::NyaTerm => nyaterm::default_path(&env),
        }
    }
}

/// 按来源读取 `path`（文件或目录；MobaXterm 还可以是 [`MOBAXTERM_REGISTRY_PATH`]）中的主机。
pub fn parse(source: Source, path: &Path) -> Result<ImportPlan, ImportError> {
    parse_with_password(source, path, None)
}

/// 同 [`parse`]；`password` 是来源的主密码（WindTerm 设了主密码时需要，其他来源不用）。
pub fn parse_with_password(
    source: Source,
    path: &Path,
    password: Option<&SecretString>,
) -> Result<ImportPlan, ImportError> {
    let env = LocalEnv::detect();
    let plan = match source {
        Source::SshConfig => ssh_config::read(path, &env)?,
        Source::Xshell => xshell::read(path, &env)?,
        Source::FinalShell => finalshell::read(path, &env)?,
        Source::MobaXterm => mobaxterm::read(path, &env)?,
        Source::Tabby => tabby::read(path, &env)?,
        Source::Termora => termora::read(path, &env)?,
        Source::WindTerm => windterm::read(path, &env, password)?,
        Source::Electerm => electerm::read(path, &env)?,
        Source::Netcatty => netcatty::read(path, &env)?,
        Source::NyaTerm => nyaterm::read(path, &env)?,
    };
    tracing::info!(
        source = source.name(),
        hosts = plan.hosts.len(),
        groups = plan.groups.len(),
        snippets = plan.snippets.len(),
        skipped = plan.skipped,
        "解析导入来源"
    );
    Ok(plan)
}

/// 与 [`parse`] 相同，在阻塞线程上读取（大文件、目录、Termora 与 WindTerm 的解密耗时）；必须在核心的运行时上执行。
pub async fn read(source: Source, path: PathBuf) -> Result<ImportPlan, ImportError> {
    read_with_password(source, path, None).await
}

/// 与 [`parse_with_password`] 相同，在阻塞线程上读取；必须在核心的运行时上执行。
pub async fn read_with_password(
    source: Source,
    path: PathBuf,
    password: Option<SecretString>,
) -> Result<ImportPlan, ImportError> {
    let at = path.clone();
    let parsed =
        tokio::task::spawn_blocking(move || parse_with_password(source, &path, password.as_ref()));
    match parsed.await {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(ImportError::Io {
            path: at,
            source: std::io::Error::other(e.to_string()),
        }),
    }
}

fn first_existing(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|c| c.exists())
}

/// 导入失败的原因。消息是写给日志看的中文，界面按变体选用自己的文案。
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("读取 {path} 失败：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} 超过 {limit_mb} MB，不像是配置文件")]
    TooLarge { path: PathBuf, limit_mb: u64 },

    /// 配置文件有语法错误（Tabby 的 YAML）
    #[error("解析配置失败：{0}")]
    Syntax(String),

    /// Tabby 开启了配置加密（Vault），配置文件中只有密文：需要先在 Tabby 中关闭
    #[error("配置文件已被 Tabby 加密，需要先在 Tabby 设置中关闭配置加密")]
    TabbyEncrypted,

    /// 读取 MobaXterm 保存在注册表中的书签失败；非 Windows 系统上没有注册表
    #[error("读取 MobaXterm 的注册表失败：{0}")]
    Registry(String),

    /// 选择的是 Termora 1.x 的数据，需要先用 Termora 2.x 打开一次完成迁移
    #[error("这是 Termora 1.x 的数据，需要先用 Termora 2.x 打开一次完成数据迁移")]
    TermoraV1,

    /// 目录中没有 Termora 的数据库（`config/termora.db`）
    #[error("没有找到 Termora 的数据库（config/termora.db）")]
    TermoraNotFound,

    /// 文件不是 Termora 2.x 的数据库
    #[error("不是 Termora 2.x 的数据库：{0}")]
    NotTermora(String),

    /// 数据库中缺少解密所需的口令与盐，可能是不支持的 Termora 版本
    #[error("Termora 数据库中缺少解密所需的信息")]
    TermoraKeyMissing,

    /// 全部记录都无法解密，可能是不支持的 Termora 版本
    #[error("无法解密 Termora 的数据")]
    TermoraDecrypt,

    /// 没有找到 WindTerm 的会话文件（`profiles/default.v10/terminal/user.sessions`）
    #[error("没有找到 WindTerm 的会话文件（profiles/default.v10/terminal/user.sessions）")]
    WindTermNotFound,

    /// WindTerm 设了主密码：需要输入主密码才能解开保存的密码
    #[error("WindTerm 设了主密码，需要输入主密码")]
    WindTermPasswordRequired,

    /// 输入的主密码解不开任何一条保存的密码
    #[error("WindTerm 的主密码不对")]
    WindTermWrongPassword,

    /// 不是 Electerm 的书签导出（JSON），目录中也没有它的数据（electerm.db 或 *.nedb）
    #[error("不是 Electerm 的书签导出或数据目录：{0}")]
    NotElecterm(String),

    /// Electerm 3.0 起用系统的加密（Electron safeStorage）保存书签，读不了：需要在 Electerm 中导出书签
    #[error("Electerm 的书签用系统加密保存，需要先在 Electerm 中导出书签")]
    ElectermEncrypted,

    /// 不是 Netcatty 导出的主机 CSV（没有 Hostname/IP 列）
    #[error("不是 Netcatty 导出的主机 CSV")]
    NotNetcatty,

    /// 不是 NyaTerm 导出的快捷命令（不是 JSON，或没有 `commands`）
    #[error("不是 NyaTerm 导出的快捷命令：{0}")]
    NotNyaTerm(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_names_round_trip() {
        for source in Source::ALL {
            assert_eq!(Source::from_name(source.name()), Some(source));
        }
        assert_eq!(Source::from_name("putty"), None, "不支持的来源");
    }

    #[test]
    fn parse_dispatches_by_source() {
        let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/import/testdata");
        let tabby = parse(Source::Tabby, &testdata.join("tabby")).unwrap();
        assert_eq!((tabby.source, tabby.hosts.len()), (Source::Tabby, 4));
        let moba = parse(Source::MobaXterm, &testdata.join("mobaxterm")).unwrap();
        assert_eq!(
            (moba.source, moba.hosts.len(), moba.skipped),
            (Source::MobaXterm, 9, 4)
        );
        assert!(matches!(
            parse(Source::SshConfig, &testdata.join("missing")),
            Err(ImportError::Io { .. })
        ));
    }
}
