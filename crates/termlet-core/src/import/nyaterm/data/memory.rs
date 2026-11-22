//! 在内存中打开 redb 数据库：把整个文件读进来再交给 redb，不打开原文件，也就不碰 NyaTerm 对它加的锁，
//! redb 打开时的修复、写入都只发生在这份内存中。内容可能有解密后的秘密（`.nya` 中的连接密码），用完清零。

use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use redb::{BackendError, Database, DatabaseError, StorageBackend};
use zeroize::Zeroizing;

/// 内存中数据库的大小上限（打开时的修复可能让它略微变大）
const MAX_LEN: usize = 512 << 20;

#[derive(Clone, Default)]
pub(super) struct Memory(Arc<Mutex<Zeroizing<Vec<u8>>>>);

impl Memory {
    pub fn from_bytes(bytes: Zeroizing<Vec<u8>>) -> Self {
        Self(Arc::new(Mutex::new(bytes)))
    }

    /// 测试用：当前的内容（构造样例时取出写好的数据库）。
    #[cfg(test)]
    pub fn bytes(&self) -> Vec<u8> {
        self.lock().to_vec()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Zeroizing<Vec<u8>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 在内存中打开 `bytes` 中的数据库。
pub(super) fn open(bytes: Zeroizing<Vec<u8>>) -> Result<Database, DatabaseError> {
    Database::builder().create_with_backend(Memory::from_bytes(bytes))
}

impl fmt::Debug for Memory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Memory").finish_non_exhaustive()
    }
}

fn out_of_range() -> BackendError {
    BackendError::Io(io::Error::new(
        io::ErrorKind::InvalidData,
        "超出内存中数据库的范围",
    ))
}

fn range(offset: u64, len: usize) -> Result<std::ops::Range<usize>, BackendError> {
    let start = usize::try_from(offset).map_err(|_| out_of_range())?;
    let end = start.checked_add(len).ok_or_else(out_of_range)?;
    Ok(start..end)
}

impl StorageBackend for Memory {
    fn len(&self) -> Result<u64, BackendError> {
        Ok(self.lock().len() as u64)
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), BackendError> {
        let bytes = self.lock();
        let source = bytes
            .get(range(offset, out.len())?)
            .ok_or_else(out_of_range)?;
        out.copy_from_slice(source);
        Ok(())
    }

    fn set_len(&self, len: u64) -> Result<(), BackendError> {
        let len = usize::try_from(len).map_err(|_| out_of_range())?;
        if len > MAX_LEN {
            return Err(out_of_range());
        }
        self.lock().resize(len, 0);
        Ok(())
    }

    fn sync_data(&self) -> Result<(), BackendError> {
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), BackendError> {
        let mut bytes = self.lock();
        let target = bytes
            .get_mut(range(offset, data.len())?)
            .ok_or_else(out_of_range)?;
        target.copy_from_slice(data);
        Ok(())
    }
}
