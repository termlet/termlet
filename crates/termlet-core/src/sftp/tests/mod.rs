//! SFTP 的测试：不需要服务器的用内存 SFTP 服务端（见 [`memfs`]），
//! 其余对真实 sshd（见 [`sshd`]、以 root 身份打开的 [`sudo`]、文件名编码的 [`names`]，设置 `TERMLET_TEST_SSH=127.0.0.1:2222` 后运行，否则跳过）；
//! SCP 模式见 [`scp`]（另用没有 SFTP 的临时容器）；传输吞吐的基准见 [`bench`]（默认不跑）。

mod autoclear;
mod bench;
mod memfs;
mod names;
mod safety;
mod scp;
mod sshd;
mod sudo;
mod transfer;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use memfs::MemFs;

use super::client::SftpClient;
use super::manager::ManagerOptions;
use super::names::PART_SUFFIX;
use super::session::{Link, Redial};
use super::{SftpManager, SftpSession, TransferId, TransferState, TransferView};
use crate::model::Encoding;
pub(super) use crate::testkit::{block_on, wait_until_async};

/// 测试用的参数：等待时间缩短。
pub(super) fn options() -> ManagerOptions {
    ManagerOptions {
        lost_wait: Duration::from_millis(500),
        ..ManagerOptions::default()
    }
}

pub(super) fn manager() -> SftpManager {
    SftpManager::new(options())
}

/// 经内存管道连到 `fs`，打开一个会话（主机 h1）。
pub(super) async fn open_mem(
    manager: &SftpManager,
    fs: &Arc<MemFs>,
    owner: &str,
) -> Arc<SftpSession> {
    manager
        .open_stream("h1", owner, fs.connect())
        .await
        .unwrap()
}

/// 与 [`open_mem`] 相同，但断线后自动重连到同一个 `fs`（`fail` 时重连总是失败，可以重试）。
pub(super) async fn open_mem_redial(
    manager: &SftpManager,
    fs: &Arc<MemFs>,
    owner: &str,
    fail: bool,
) -> Arc<SftpSession> {
    let client = SftpClient::connect(fs.connect(), Encoding::Utf8)
        .await
        .unwrap();
    let target = fs.clone();
    let redial: Redial = Arc::new(move || {
        let fs = target.clone();
        Box::pin(async move {
            if fail {
                return Err(super::SftpError::Disconnected);
            }
            Ok(Link {
                backend: super::backend::Backend::Sftp(Arc::new(
                    SftpClient::connect(fs.connect(), Encoding::Utf8).await?,
                )),
                connection: None,
                group: None,
            })
        })
    });
    let link = Link {
        backend: super::backend::Backend::Sftp(Arc::new(client)),
        connection: None,
        group: None,
    };
    manager
        .0
        .add("h1".into(), owner.into(), link, Some(redial))
        .await
}

/// 等到任务结束（完成、失败或取消），返回它的状态。
pub(super) async fn wait_finished(manager: &SftpManager, id: &TransferId) -> TransferView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = manager
            .transfers(None)
            .into_iter()
            .find(|v| v.id == *id && v.state.is_finished())
        {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "等待传输结束超时：{:?}",
            manager.transfers(None)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// 等到任务的状态满足条件。
pub(super) async fn wait_view(
    manager: &SftpManager,
    id: &TransferId,
    pred: impl Fn(&TransferView) -> bool,
) -> TransferView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = manager
            .transfers(None)
            .into_iter()
            .find(|v| v.id == *id && pred(v))
        {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "等待传输状态超时：{:?}",
            manager.transfers(None)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// 全部记录的状态（按创建顺序），不经窗口读取（不会安排自动清除）。
pub(super) fn states_of(manager: &SftpManager) -> Vec<TransferState> {
    manager
        .transfers(None)
        .into_iter()
        .map(|v| v.state)
        .collect()
}

/// 等到全部记录的状态为 `want`。
pub(super) async fn wait_states(manager: &SftpManager, want: &[TransferState]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while states_of(manager) != want {
        assert!(
            tokio::time::Instant::now() < deadline,
            "记录应为 {want:?}，实际为 {:?}",
            states_of(manager)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub(super) fn ids_of(manager: &SftpManager) -> Vec<TransferId> {
    manager.transfers(None).into_iter().map(|v| v.id).collect()
}

pub(super) fn random_bytes(n: usize) -> Vec<u8> {
    let mut data = vec![0u8; n];
    getrandom::fill(&mut data).unwrap();
    data
}

/// 本机目录中的名称（排序）。
pub(super) fn local_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// 本机目录中的临时文件。
pub(super) fn local_parts(dir: &Path) -> Vec<String> {
    local_names(dir)
        .into_iter()
        .filter(|n| n.ends_with(PART_SUFFIX))
        .collect()
}

/// 内存服务端目录中的临时文件。
pub(super) fn remote_parts(fs: &MemFs, dir: &str) -> Vec<String> {
    fs.names(dir)
        .into_iter()
        .filter(|n| n.ends_with(PART_SUFFIX))
        .collect()
}

/// 新建指向目录 `target` 的链接：Windows 上用目录联接（不需要管理员权限或开发者模式），其他系统用符号链接。
pub(super) fn link_dir(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(out.status.success(), "新建目录联接失败：{out:?}");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// 让当前用户读不了本机的 `path`（目录不能列出内容、文件不能打开），丢弃时恢复。
/// Windows 上用 icacls 加一条拒绝读取的权限；其他系统改权限为 000。不受权限限制时（Unix 的 root，启用了备份特权的
/// Windows 管理员，例如 GitHub 的运行器上列目录不看拒绝的权限）返回 `None`，跳过测试。
pub(super) struct Denied(std::path::PathBuf);

impl Denied {
    pub(super) fn new(path: &Path) -> Option<Self> {
        #[cfg(windows)]
        {
            let user = std::env::var("USERNAME").ok()?;
            let out = std::process::Command::new("icacls")
                .arg(path)
                .args(["/deny", &format!("{user}:(RD)")])
                .output()
                .unwrap();
            assert!(out.status.success(), "icacls 拒绝读取失败：{out:?}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let denied = Self(path.to_path_buf());
        let readable = if path.is_dir() {
            std::fs::read_dir(path).is_ok()
        } else {
            std::fs::File::open(path).is_ok()
        };
        if readable {
            // 丢弃 denied 时恢复原样
            println!(
                "{} 改了权限仍然读得到（以 root 或有备份特权的管理员运行），跳过",
                path.display()
            );
            return None;
        }
        Some(denied)
    }
}

impl Drop for Denied {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Ok(user) = std::env::var("USERNAME") {
            let _ = std::process::Command::new("icacls")
                .arg(&self.0)
                .args(["/remove:d", &user])
                .output();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }
}

/// 界面在核心的运行时上执行这些操作：返回的 future 必须能跨线程（编译时检查）。
#[allow(dead_code)]
fn futures_are_send(
    manager: SftpManager,
    session: Arc<SftpSession>,
    connection: Arc<crate::ssh::Connection>,
    target: super::SshTarget,
) {
    fn send<T: Send>(_: T) {}
    send(manager.open_ssh(target));
    send(manager.open_stream("h", "w", tokio::io::duplex(1).0));
    send(manager.upload(&session, Default::default()));
    send(manager.download(&session, Default::default()));
    send(manager.clear(Default::default()));
    send(manager.close_owner(None));
    send(session.list(""));
    send(session.stat(""));
    send(session.mkdir(""));
    send(session.rename("", ""));
    send(session.remove(""));
    send(session.chmod(&Default::default()));
    send(session.read_text("", None));
    send(session.write_text(&Default::default()));
    send(super::list_local(Path::new("")));
    send(super::local_places());
    send(super::make_local_dir(Path::new(""), ""));
    send(super::rename_local(Path::new(""), "", ""));
    send(super::trash_local(Path::new(""), &[]));
    send(super::put_file(
        &connection,
        Path::new(""),
        "",
        false,
        std::future::pending(),
    ));
    send(super::get_file(
        &connection,
        "",
        Path::new(""),
        false,
        std::future::pending(),
    ));
    let cwd = super::TerminalCwd::new(&Default::default(), crate::ssh::TmuxStatus::Off, false);
    send(cwd.read(&connection));
}
