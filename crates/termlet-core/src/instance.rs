//! 单实例：每个数据目录同一时间只运行一个 Termlet。
//!
//! 启动时对 `run/instance.lock` 加排他锁，进程退出（包括崩溃）时操作系统自动释放。
//! 拿不到锁说明已有实例在运行，调用方应改为经 CLI 服务把启动参数交给它。

use std::fs::{File, OpenOptions, TryLockError};
use std::io;

use crate::paths::DataDir;

/// 持有期间独占这个数据目录。
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// 尝试获取锁；已有实例在运行时返回 `Ok(None)`。
    pub fn try_acquire(dir: &DataDir) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.run_dir().join("instance.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_fails_until_first_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::at(tmp.path());
        dir.prepare().unwrap();
        let first = InstanceLock::try_acquire(&dir).unwrap();
        assert!(first.is_some());
        assert!(InstanceLock::try_acquire(&dir).unwrap().is_none());
        drop(first);
        assert!(InstanceLock::try_acquire(&dir).unwrap().is_some());
    }
}
