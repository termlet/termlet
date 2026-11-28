//! 单元测试共用的辅助：在核心的运行时上执行、等待条件成立、内存中的存储与钥匙串，
//! 以及真实 sshd 的测试容器（testing/sshd）的地址、账号与连接。
//!
//! 模拟一台电脑上的 Termlet（各自的数据目录与保险库主密钥）的 `Machine` 见 `backup::testkit`。

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::Result;
use crate::db::Database;
use crate::id::HostId;
use crate::model::{Host, SecretField};
use crate::secret::SecretString;
use crate::ssh::{Answer, ConnectOptions, Connection, Prompt, Prompter, connect, plan_for_host};
use crate::store::{Owner, Position, Store};
use crate::vault::{KEY_LEN, KeyStore, MemoryKeyStore, Vault};

/// 在核心的运行时上执行（连接、传输、转发等的后台任务都在那里）。
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    crate::runtime::runtime().block_on(future)
}

/// 等到条件成立（后台线程、运行时上的任务在做），最多 5 秒。
pub(crate) fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ok() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 在运行时上等到条件成立，最多 10 秒。
pub(crate) async fn wait_until_async(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(tokio::time::Instant::now() < deadline, "等待超时：{what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// 多次打开同一个数据库文件时共用的内存钥匙串。
pub(crate) type SharedKeys = Arc<MemoryKeyStore>;

/// 内存数据库上的存储（主密钥在内存钥匙串中）。
pub(crate) fn memory_store() -> Arc<Store> {
    let db = Database::open_in_memory().unwrap();
    let vault = Vault::open(&db, Box::new(MemoryKeyStore::default())).unwrap();
    Arc::new(Store::open(db, vault).unwrap())
}

/// 保险库没有解锁的存储（钥匙串不可用、还没有设主密码）：秘密读写不了，其余照常。
pub(crate) fn locked_store() -> Arc<Store> {
    struct NoKeyring;
    impl KeyStore for NoKeyring {
        fn load(&self) -> Result<Option<Zeroizing<[u8; KEY_LEN]>>> {
            Ok(None)
        }
        fn store(&self, _key: &[u8; KEY_LEN]) -> Result<()> {
            Err(crate::CoreError::Keyring("测试中没有钥匙串".into()))
        }
        fn delete(&self) -> Result<()> {
            Ok(())
        }
    }
    let db = Database::open_in_memory().unwrap();
    let vault = Vault::open(&db, Box::new(NoKeyring)).unwrap();
    assert!(!vault.is_unlocked());
    Arc::new(Store::open(db, vault).unwrap())
}

// ---------------------------------------------------------------- 真实 sshd 的测试容器

/// 测试容器的账号（另有以 root 打开 SFTP 用的 sudo-* 账号，密码相同）。
pub(crate) const SSH_USER: &str = "termlet";
/// 测试容器中各账号的密码。
pub(crate) const SSH_PASSWORD: &str = "termlet-e2e";

/// 环境变量中的 `主机:端口`；没有设置、写得不对时为 `None`。
fn address_from_env(name: &str) -> Option<(String, u16)> {
    let value = std::env::var(name).ok()?;
    let (host, port) = value.rsplit_once(':')?;
    // IPv6 写成 [::1]:2222
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    Some((host.to_string(), port.parse().ok()?))
}

/// 测试容器的 sshd（`TERMLET_TEST_SSH=127.0.0.1:2222`）；没有设置时为 `None`，测试跳过。
pub(crate) fn test_server() -> Option<(String, u16)> {
    address_from_env("TERMLET_TEST_SSH")
}

/// 没有 SFTP 的测试容器（testing/nosftp，`TERMLET_TEST_NOSFTP=127.0.0.1:2225`）；没有设置时为 `None`，测试跳过。
pub(crate) fn nosftp_server() -> Option<(String, u16)> {
    address_from_env("TERMLET_TEST_NOSFTP")
}

/// 同一个容器里的 telnetd（`TERMLET_TEST_TELNET=127.0.0.1:2323`）；没有设置时为 `None`，测试跳过。
pub(crate) fn test_telnet() -> Option<(String, u16)> {
    address_from_env("TERMLET_TEST_TELNET")
}

/// 接受指纹，其余一律取消（测试中不应询问密码）。
pub(crate) fn accept_host_key() -> Prompter {
    Arc::new(|prompt| {
        Box::pin(async move {
            match prompt {
                Prompt::HostKey(_) => Answer::AcceptHostKey,
                _ => Answer::Cancel,
            }
        })
    })
}

/// 在存储中加一台以 `user` 登录测试容器的主机，保存容器的统一密码（[`SSH_PASSWORD`]）。
fn add_server_host(store: &Store, name: &str, address: &str, port: u16, user: &str) -> HostId {
    let id = store
        .create(
            Host {
                name: name.into(),
                address: address.into(),
                port,
                username: user.into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    store
        .set_secret(
            &Owner::host(&id),
            SecretField::Password,
            Some(&SecretString::new(SSH_PASSWORD)),
        )
        .unwrap();
    id
}

/// 内存中的存储，加一台以 `user` 登录测试容器的主机（[`add_server_host`]）；没有设置 `TERMLET_TEST_SSH` 时为 `None`。
pub(crate) fn server_store(name: &str, user: &str) -> Option<(Arc<Store>, HostId)> {
    let (address, port) = test_server()?;
    let store = memory_store();
    let host = add_server_host(&store, name, &address, port, user);
    Some((store, host))
}

/// 与 [`server_store`] 相同，连没有 SFTP 的测试容器（testing/nosftp，`TERMLET_TEST_NOSFTP=127.0.0.1:2225`）；
/// 没有设置时为 `None`，测试跳过。
pub(crate) fn nosftp_store(name: &str, user: &str) -> Option<(Arc<Store>, HostId)> {
    let (address, port) = nosftp_server()?;
    let store = memory_store();
    let host = add_server_host(&store, name, &address, port, user);
    Some((store, host))
}

/// 以 [`SSH_USER`] 连上测试容器（接受指纹，用保存的密码）。
pub(crate) async fn connect_test(host: &str, port: u16) -> Arc<Connection> {
    let store = memory_store();
    let id = add_server_host(&store, &format!("{host}:{port}"), host, port, SSH_USER);
    let plan = plan_for_host(&store, &id).unwrap();
    connect(store, &plan, accept_host_key(), &ConnectOptions::default())
        .await
        .unwrap()
}
