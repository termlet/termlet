//! 这条连接上能否执行需要 root 的操作：每条连接探测一次（`id -u`；不是 root 时 `sudo -n true`），结果记在
//! [`HostTools`] 中，终端重连（换了连接）后重新探测。不在每次刷新时探测：`sudo -n true` 会在服务器的 auth 日志里
//! 留一条记录（是 root 时不调用 sudo）。
//!
//! 界面据此标出权限（有免密码 sudo 时一个“sudo”标记，没有时一个锁图标，不再常驻“不是 root”的提示）；网络页有免密码
//! sudo 时经 `sudo -n ss` 读出全部进程的进程名（见 [`HostTools::connections`]）。写操作照常先直接执行、权限不足时
//! 再试 `sudo -n`（见 `scripts::PRIV_FUNC`），不依赖这里的结果。

use std::sync::{Arc, Weak};

use super::exec::{Limits, Target, on_runtime, one_line, run, sh_script};
use super::sections::{Sections, uid_of, unreadable};
use super::{HostTools, LIST_LIMIT, LIST_TIMEOUT, SudoState, ToolsError, scripts};
use crate::ssh::{Connection, ExecResult};

/// 需要 root 的操作能否执行。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Privilege {
    /// 登录用户就是 root
    Root,
    /// 普通用户，有免密码 sudo：需要 root 的操作自动经 `sudo -n` 执行
    Sudo,
    /// 普通用户，没有可用的免密码 sudo（附 sudo 的情况）：需要 root 的操作会失败，可以改在终端中执行
    Limited(SudoState),
}

/// 探测是哪一条连接上的结果（终端重连后换成新的连接，要重新探测）。
#[derive(Clone)]
enum TargetKey {
    /// 弱引用让旧连接的内存留着，新的连接不会恰好用同一个地址
    Ssh(Weak<Connection>),
    Wsl {
        distro: String,
        user: String,
    },
}

impl TargetKey {
    fn of(target: &Target) -> Self {
        match target {
            Target::Ssh { connection, .. } => TargetKey::Ssh(Arc::downgrade(connection)),
            Target::Wsl { distro, user } => TargetKey::Wsl {
                distro: distro.clone(),
                user: user.clone(),
            },
        }
    }

    fn matches(&self, target: &Target) -> bool {
        match (self, target) {
            (TargetKey::Ssh(weak), Target::Ssh { connection, .. }) => {
                weak.as_ptr() == Arc::as_ptr(connection)
            }
            (TargetKey::Wsl { distro, user }, Target::Wsl { distro: d, user: u }) => {
                distro == d && user == u
            }
            _ => false,
        }
    }
}

/// 记下的探测结果。探测期间持有锁：几页同时要时只探测一次。
#[derive(Default)]
pub(crate) struct PrivilegeCache(tokio::sync::Mutex<Option<(TargetKey, Privilege)>>);

/// 探测的脚本：是 root 时只输出 UID；否则看有没有 sudo、`sudo -n true` 能否执行（错误输出经 `q` 输出，按它区分
/// 要密码与不允许）。
const PRIVILEGE_BODY: &str = r#"echo @@uid; id -u;
if [ "$(id -u)" -eq 0 ]; then exit 0; fi;
if command -v sudo >/dev/null 2>&1; then e=$(sudo -n true 2>&1); echo "@@sudo $?"; printf '%s\n' "$e" | head -n 3 | q; else echo "@@sudo none"; fi"#;

/// 带开头的探测脚本。
pub(crate) fn privilege_script() -> String {
    format!("{}{PRIVILEGE_BODY}", scripts::PRELUDE)
}

/// 探测的命令。
pub(crate) fn privilege_command() -> String {
    sh_script::<&str>(&one_line(&privilege_script()), &[])
}

/// 解析探测的输出；没有 UID 时为 `None`。
pub(crate) fn parse_privilege(out: &str) -> Option<Privilege> {
    let sections = Sections::parse(out);
    if uid_of(sections.get("uid"))? == 0 {
        return Some(Privilege::Root);
    }
    let Some(sudo) = sections.get("sudo") else {
        return Some(Privilege::Limited(SudoState::NotInstalled));
    };
    Some(match sudo.arg.trim() {
        "0" => Privilege::Sudo,
        "none" => Privilege::Limited(SudoState::NotInstalled),
        _ => {
            let text = sudo.lines.join("\n");
            // 与 `scripts::PRIV_FUNC` 的分法相同；认不出时当作要密码（也可能根本没有 sudo 权限，界面一并说明）
            if text.contains("sudoers")
                || text.contains("not allowed to")
                || text.contains("may not run sudo")
            {
                Privilege::Limited(SudoState::Denied)
            } else {
                Privilege::Limited(SudoState::Password)
            }
        }
    })
}

fn finish_privilege(result: &ExecResult) -> Result<Privilege, ToolsError> {
    parse_privilege(&String::from_utf8_lossy(&result.stdout)).ok_or_else(|| unreadable(result))
}

impl HostTools {
    /// 这条连接上能否执行需要 root 的操作。每条连接只探测一次（之后直接返回记下的结果）；读不到时返回错误，不记下，
    /// 下次再探测。
    pub async fn privilege(&self, target: &Target) -> Result<Privilege, ToolsError> {
        let inner = self.inner.clone();
        let target = target.clone();
        on_runtime(async move {
            let mut cache = inner.privilege.0.lock().await;
            if let Some((key, privilege)) = cache.as_ref()
                && key.matches(&target)
            {
                return Ok(*privilege);
            }
            let limits = Limits {
                timeout: LIST_TIMEOUT,
                output: LIST_LIMIT,
                long: false,
            };
            let result = run(&target, &privilege_command(), limits).await?;
            let privilege = finish_privilege(&result)?;
            tracing::debug!(?privilege, "主机工具：探测到这条连接上的权限");
            *cache = Some((TargetKey::of(&target), privilege));
            Ok(privilege)
        })
        .await
    }
}
