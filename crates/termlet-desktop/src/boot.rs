//! 启动：在打开窗口之前准备数据目录、日志、单实例锁、数据库与保险库。

use std::path::PathBuf;
use std::sync::Arc;

use termlet_core::db::Database;
use termlet_core::instance::InstanceLock;
use termlet_core::logging::{self, LogGuard};
use termlet_core::paths::DataDir;
use termlet_core::store::Store;
use termlet_core::vault::{self, Vault};
use termlet_core::{CoreError, VERSION};

/// 启动成功后应用持有的资源。
pub struct Booted {
    pub data_dir: DataDir,
    pub store: Arc<Store>,
    pub instance: InstanceLock,
    pub log: Option<LogGuard>,
}

/// 启动失败的原因。
pub enum BootError {
    /// 同一个数据目录已有 Termlet 在运行
    AlreadyRunning,
    /// 便携版的数据在程序旁边的 `dir`，但那里建不了、写不进（程序放在了 Program Files 等不能写的位置）
    PortableNotWritable { dir: PathBuf, reason: String },
    /// 无法继续运行（数据目录不可用、数据库来自更新版本等）
    Fatal(CoreError),
}

/// `log_override`：启动时指定的日志详细程度（环境变量或启动参数），没有时按设置。
pub fn start(log_override: Option<logging::LogOverride>) -> Result<Booted, BootError> {
    let data_dir = DataDir::resolve().map_err(BootError::Fatal)?;
    let warning = prepare(&data_dir)?;
    let instance = match InstanceLock::try_acquire(&data_dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Err(BootError::AlreadyRunning),
        Err(e) => return Err(BootError::Fatal(e.into())),
    };
    // 拿到锁之后再写日志：已有实例在运行时，日志文件归它
    let log = logging::init(&data_dir.logs_dir(), log_override);
    tracing::info!(
        version = VERSION,
        data_dir = %data_dir.root().display(),
        portable = data_dir.is_portable(),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "Termlet 启动"
    );
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    }
    let store = open_store(&data_dir).map_err(|e| {
        tracing::error!("打开数据失败：{e}");
        BootError::Fatal(e)
    })?;
    // 设置中的日志详细程度（启动时指定了的不变）
    logging::apply_setting(store.local_settings().logs.level);
    data_dir.secure_in_background();
    Ok(Booted {
        data_dir,
        store: Arc::new(store),
        instance,
        log,
    })
}

/// 创建数据目录。便携版的数据在程序旁边：建不了、写不进时说明原因（[`BootError::PortableNotWritable`]），
/// 不在别处另建一份数据。
fn prepare(data_dir: &DataDir) -> Result<Option<String>, BootError> {
    // 原因只写系统的报错（拒绝访问等），位置由说明给出
    if data_dir.is_portable()
        && let Err(e) =
            std::fs::create_dir_all(data_dir.run_dir()).and_then(|()| data_dir.probe_writable())
    {
        return Err(BootError::PortableNotWritable {
            dir: data_dir.root().to_path_buf(),
            reason: e.to_string(),
        });
    }
    data_dir.prepare().map_err(BootError::Fatal)
}

fn open_store(data_dir: &DataDir) -> termlet_core::Result<Store> {
    let db = Database::open(&data_dir.database(), &data_dir.backups_dir())?;
    let id = vault::data_dir_id(&db)?;
    let key_store = vault::key_store_for(data_dir, &id);
    // 便携版第一次使用时先让用户选主密码还是这台电脑的凭据管理器（数据可能被带到别的电脑上）
    let vault = if data_dir.is_portable() {
        Vault::open_portable(&db, key_store)?
    } else {
        Vault::open(&db, key_store)?
    };
    tracing::info!(status = ?vault.status(), "保险库");
    Store::open(db, vault)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 便携版的数据目录建不了、写不进时说明是便携版的问题（程序放错了位置），而不是笼统的“数据目录不可用”；
    // 安装版照旧；能写时照常启动
    #[test]
    fn portable_data_dir_must_be_writable() {
        let root = std::env::temp_dir().join(format!(
            "termlet-boot-{}",
            termlet_core::id::random_token(8)
        ));
        std::fs::create_dir_all(&root).unwrap();
        // 程序所在的“文件夹”其实是个文件：data 建不了
        let blocked = root.join("Termlet");
        std::fs::write(&blocked, b"").unwrap();
        let portable = DataDir::at(blocked.join("data")).into_portable();
        assert!(matches!(
            prepare(&portable),
            Err(BootError::PortableNotWritable { dir, .. }) if dir == blocked.join("data")
        ));
        assert!(matches!(
            prepare(&DataDir::at(blocked.join("data"))),
            Err(BootError::Fatal(_))
        ));
        let writable = DataDir::at(root.join("ok").join("data")).into_portable();
        assert!(prepare(&writable).is_ok());
        assert!(writable.run_dir().is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }
}
